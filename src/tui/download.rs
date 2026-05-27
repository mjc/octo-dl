//! Download task management and transport-side event emission.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::future::Future;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

use indexmap::{IndexMap, IndexSet};
#[cfg(test)]
#[path = "download_tests.rs"]
mod tests;

use futures_util::{FutureExt, StreamExt, stream};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::core::{FileAccounting, FileId, PackageId};
use crate::{
    DlcKeyCache, DownloadConfig, DownloadProgress, core::ProgressDelta, format_bytes, is_dlc_path,
};
use dirs;

use super::event::{
    DownloadChannels, DownloadEvent, DownloadRequest, FileOrigin, QueuedFile, TokenMessage,
    TuiProgress,
};

const PACKAGE_REVERIFY_CONCURRENCY: usize = 4;
const VERIFICATION_PROGRESS_EVENT_BYTES: u64 = 8 * 1024 * 1024;

pub(crate) fn schedule_resume_artifact_delete(path: String) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            if let Err(e) = crate::delete_resume_artifacts(&path).await {
                log::warn!("Failed to delete resume artifacts for {path}: {e}");
            }
        });
    } else {
        let part = crate::download::part_path(&path);
        let sidecar = crate::download::sidecar_path(&path);
        let legacy_json_sidecar = crate::download::legacy_json_sidecar_path(&path);
        if let Err(e) = std::fs::remove_file(&part)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("Failed to delete resume artifact {}: {e}", part.display());
        }
        if let Err(e) = std::fs::remove_file(&sidecar)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!(
                "Failed to delete resume artifact {}: {e}",
                sidecar.display()
            );
        }
        if let Err(e) = std::fs::remove_file(&legacy_json_sidecar)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!(
                "Failed to delete resume artifact {}: {e}",
                legacy_json_sidecar.display()
            );
        }
    }
}

pub(crate) fn schedule_output_artifact_delete(path: String) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            if let Err(e) = tokio::fs::remove_file(&path).await
                && e.kind() != std::io::ErrorKind::NotFound
            {
                log::warn!("Failed to delete output artifact {path}: {e}");
            }
        });
    } else {
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("Failed to delete output artifact {path}: {e}");
        }
    }
}

pub(super) fn build_http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .pool_idle_timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(8)
        .tcp_keepalive(Duration::from_secs(30))
        .build()
}

fn describe_panic(panic: &(dyn std::any::Any + Send)) -> String {
    panic.downcast_ref::<&str>().map_or_else(
        || {
            panic.downcast_ref::<String>().map_or_else(
                || "unknown panic payload".to_string(),
                std::clone::Clone::clone,
            )
        },
        |msg| (*msg).to_string(),
    )
}

#[derive(Clone)]
struct ResolvedUrl {
    source_url: String,
    submitted_url: String,
    package_id: Option<PackageId>,
    package_display_name: Option<String>,
}

impl ResolvedUrl {
    fn direct(url: &str) -> Self {
        Self {
            source_url: url.to_string(),
            submitted_url: url.to_string(),
            package_id: None,
            package_display_name: None,
        }
    }

    fn from_source(source_url: String, submitted_url: &str) -> Self {
        Self {
            source_url,
            submitted_url: submitted_url.to_string(),
            package_id: None,
            package_display_name: None,
        }
    }

    fn file_origin(&self) -> FileOrigin {
        FileOrigin {
            package_id: self.package_id.clone(),
            package_display_name: self.package_display_name.clone(),
            source_url: self.source_url.clone(),
            submitted_url: self.submitted_url.clone(),
        }
    }
}

struct FetchedNodeSet {
    resolved: ResolvedUrl,
    nodes: Option<mega::Nodes>,
    requested_files: RequestedFiles,
    requested_attempt_ids: HashMap<FileId, u64>,
    emit_url_resolved: bool,
}

#[derive(Clone)]
enum RequestedFiles {
    All,
    Only(IndexSet<FileId>),
}

#[derive(Clone)]
struct QueuedDownload {
    resolved: ResolvedUrl,
    item: crate::OwnedDownloadItem,
    attempt_id: u64,
    trust_resume_state: bool,
}

impl QueuedDownload {
    fn queued_event(&self, accounting: FileAccounting) -> QueuedFile {
        QueuedFile {
            id: self.item.path.clone().into(),
            size: self.item.node.size(),
            accounting,
            origin: self.resolved.file_origin(),
        }
    }

    fn complete_event(&self) -> DownloadEvent {
        DownloadEvent::FileComplete {
            id: self.item.path.clone().into(),
            attempt_id: self.attempt_id,
        }
    }
}

struct CollectedBatch {
    queued_items: Vec<QueuedDownload>,
    completed_items: Vec<QueuedDownload>,
    skipped_count: usize,
    partial_count: usize,
    successful_submitted_urls: Vec<String>,
}

impl CollectedBatch {
    fn total_bytes(&self) -> u64 {
        self.queued_items
            .iter()
            .chain(self.completed_items.iter())
            .map(|item| item.item.node.size())
            .sum()
    }

    fn file_total(&self) -> usize {
        self.queued_items.len() + self.completed_items.len()
    }

    fn emit_events(&self, event_tx: &mpsc::UnboundedSender<DownloadEvent>) {
        let _ = event_tx.send(DownloadEvent::FilesCollected {
            total: self.file_total(),
            skipped: self.skipped_count,
            partial: self.partial_count,
            total_bytes: self.total_bytes(),
        });

        self.emit_file_queue_events(event_tx);
        self.emit_completed_file_events(event_tx);
        self.emit_url_resolved_events(event_tx);
    }

    fn emit_file_queue_events(&self, event_tx: &mpsc::UnboundedSender<DownloadEvent>) {
        for item in &self.queued_items {
            let _ = event_tx.send(DownloadEvent::FileQueued(
                item.queued_event(FileAccounting::CurrentRun),
            ));
        }
    }

    fn emit_completed_file_events(&self, event_tx: &mpsc::UnboundedSender<DownloadEvent>) {
        for item in &self.completed_items {
            let _ = event_tx.send(DownloadEvent::FileQueued(
                item.queued_event(FileAccounting::Preexisting),
            ));
            let _ = event_tx.send(item.complete_event());
        }
    }

    fn emit_url_resolved_events(&self, event_tx: &mpsc::UnboundedSender<DownloadEvent>) {
        for url in &self.successful_submitted_urls {
            let _ = event_tx.send(DownloadEvent::UrlResolved { url: url.clone() });
        }
    }
}

struct CollectedNodeSet {
    queued_items: Vec<QueuedDownload>,
    completed_items: Vec<QueuedDownload>,
    skipped_count: usize,
    partial_count: usize,
}

struct DownloadRuntime {
    downloader: Arc<crate::Downloader>,
    http: Arc<reqwest::Client>,
    dlc_cache: Arc<DlcKeyCache>,
    progress: Arc<dyn DownloadProgress>,
    concurrent_files: usize,
}

struct DownloadTaskResult {
    id: FileId,
    attempt_id: u64,
    result: crate::Result<crate::FileStats>,
}

struct SchedulerState {
    desired_pending_order: Vec<FileId>,
    pending_queue: VecDeque<FileId>,
    available_downloads: HashMap<FileId, QueuedDownload>,
    active_downloads: HashSet<FileId>,
    exclusive_resume_target: Option<FileId>,
    join_set: tokio::task::JoinSet<DownloadTaskResult>,
}

impl SchedulerState {
    fn new() -> Self {
        Self {
            desired_pending_order: Vec::new(),
            pending_queue: VecDeque::new(),
            available_downloads: HashMap::new(),
            active_downloads: HashSet::new(),
            exclusive_resume_target: None,
            join_set: tokio::task::JoinSet::new(),
        }
    }

    fn sync_pending_order(&mut self, file_ids: Vec<FileId>) {
        if file_ids == self.desired_pending_order {
            return;
        }
        if file_ids.len() >= self.desired_pending_order.len()
            && file_ids[..self.desired_pending_order.len()] == self.desired_pending_order
        {
            for file_id in &file_ids[self.desired_pending_order.len()..] {
                if self.available_downloads.contains_key(file_id)
                    && !self.active_downloads.contains(file_id)
                {
                    self.pending_queue.push_back(file_id.clone());
                }
            }
            self.desired_pending_order = file_ids;
            return;
        }
        self.desired_pending_order = file_ids;
        self.rebuild_pending_queue();
    }

    fn register_resolved_batch(&mut self, batch: CollectedBatch) -> CollectedBatch {
        for item in &batch.queued_items {
            self.available_downloads
                .insert(item.item.path.clone().into(), item.clone());
        }
        batch
    }

    fn finish_download(&mut self, file_id: &FileId, result: &crate::Result<crate::FileStats>) {
        self.active_downloads.remove(file_id);
        if matches!(result, Err(crate::Error::Cancelled)) {
            self.rebuild_pending_queue();
            return;
        }
        self.available_downloads.remove(file_id);
    }

    fn pause_file_ids(&mut self, file_ids: &[FileId]) -> Vec<QueuedDownload> {
        let mut paused = Vec::new();
        for file_id in file_ids {
            if let Some(download) = self.available_downloads.remove(file_id) {
                paused.push(download);
            }
        }
        self.desired_pending_order
            .retain(|file_id| !file_ids.contains(file_id));
        self.pending_queue
            .retain(|file_id| !file_ids.contains(file_id));
        paused
    }

    fn unpause_downloads(&mut self, downloads: impl IntoIterator<Item = QueuedDownload>) {
        for download in downloads {
            let file_id = FileId::from(download.item.path.as_str());
            self.available_downloads.insert(file_id.clone(), download);
            if !self.desired_pending_order.contains(&file_id) {
                self.desired_pending_order.push(file_id.clone());
            }
            if !self.active_downloads.contains(&file_id) {
                self.pending_queue.push_back(file_id.clone());
            }
            self.exclusive_resume_target = Some(file_id);
        }
    }

    fn clear_exclusive_resume_target(&mut self, file_id: &FileId) {
        if self
            .exclusive_resume_target
            .as_ref()
            .is_some_and(|target| target == file_id)
        {
            self.exclusive_resume_target = None;
        }
    }

    fn rebuild_pending_queue(&mut self) {
        self.pending_queue.clear();
        self.pending_queue.extend(
            self.desired_pending_order
                .iter()
                .filter(|file_id| {
                    self.available_downloads.contains_key(*file_id)
                        && !self.active_downloads.contains(*file_id)
                })
                .cloned(),
        );
    }
}

#[cfg(test)]
fn select_startable_file_ids(
    pending_queue: &VecDeque<FileId>,
    available_file_ids: &HashSet<FileId>,
    active_downloads: &HashSet<FileId>,
    exclusive_resume_target: &Option<FileId>,
    capacity: usize,
) -> Vec<FileId> {
    if capacity == 0 {
        return Vec::new();
    }
    if let Some(target) = exclusive_resume_target {
        if available_file_ids.contains(target) && !active_downloads.contains(target) {
            return vec![target.clone()];
        }
        return Vec::new();
    }

    pending_queue
        .iter()
        .filter(|file_id| {
            available_file_ids.contains(*file_id) && !active_downloads.contains(*file_id)
        })
        .take(capacity)
        .cloned()
        .collect()
}

struct FileProgress {
    tx: mpsc::UnboundedSender<DownloadEvent>,
    id: FileId,
    attempt_id: u64,
}

impl DownloadProgress for FileProgress {
    fn on_file_start(&self, _name: &str, size: u64) {
        let _ = self.tx.send(DownloadEvent::FileStart {
            id: self.id.clone(),
            size,
            attempt_id: self.attempt_id,
        });
    }

    fn on_resume_validation_start(&self, _name: &str) {
        let _ = self.tx.send(DownloadEvent::ResumeValidationStarted {
            id: self.id.clone(),
            attempt_id: self.attempt_id,
        });
    }

    fn on_resume_validation_chunk(&self, _name: &str, bytes_delta: u64) {
        let _ = self.tx.send(DownloadEvent::VerificationProgress {
            id: self.id.clone(),
            bytes_delta,
        });
    }

    fn on_progress(&self, _name: &str, delta: ProgressDelta) {
        let _ = self.tx.send(DownloadEvent::Progress {
            id: self.id.clone(),
            delta,
            attempt_id: self.attempt_id,
        });
    }

    fn on_resume_reused(&self, _name: &str, chunks: usize, bytes: u64) {
        let _ = self.tx.send(DownloadEvent::ResumeReused {
            id: self.id.clone(),
            chunks,
            bytes,
            attempt_id: self.attempt_id,
        });
    }

    fn on_file_complete(&self, _name: &str, _stats: &crate::FileStats) {
        let _ = self.tx.send(DownloadEvent::FileComplete {
            id: self.id.clone(),
            attempt_id: self.attempt_id,
        });
    }

    fn on_error(&self, _name: &str, error: &str) {
        let _ = self.tx.send(DownloadEvent::FileError {
            id: self.id.clone(),
            error: error.to_string(),
            attempt_id: self.attempt_id,
        });
    }
}

struct VerificationProgress {
    tx: mpsc::UnboundedSender<DownloadEvent>,
    id: FileId,
    pending_bytes: AtomicU64,
}

impl VerificationProgress {
    fn new(tx: mpsc::UnboundedSender<DownloadEvent>, id: FileId) -> Self {
        Self {
            tx,
            id,
            pending_bytes: AtomicU64::new(0),
        }
    }

    fn flush_pending(&self) {
        let bytes_delta = self.pending_bytes.swap(0, Ordering::AcqRel);
        self.send_progress(bytes_delta);
    }

    fn send_progress(&self, bytes_delta: u64) {
        if bytes_delta == 0 {
            return;
        }
        let _ = self.tx.send(DownloadEvent::VerificationProgress {
            id: self.id.clone(),
            bytes_delta,
        });
    }
}

impl DownloadProgress for VerificationProgress {
    fn on_progress(&self, _name: &str, delta: ProgressDelta) {
        let previous = self
            .pending_bytes
            .fetch_add(delta.total_bytes_delta, Ordering::AcqRel);
        if previous.saturating_add(delta.total_bytes_delta) < VERIFICATION_PROGRESS_EVENT_BYTES {
            return;
        }
        let bytes_delta = self.pending_bytes.swap(0, Ordering::AcqRel);
        self.send_progress(bytes_delta);
    }
}

async fn for_each_verification_item<T, F, Fut>(items: Vec<T>, limit: usize, f: F)
where
    T: Send + 'static,
    F: Fn(T) -> Fut + Clone,
    Fut: Future<Output = ()> + Send,
{
    stream::iter(items)
        .for_each_concurrent(limit.max(1), move |item| {
            let f = f.clone();
            async move { f(item).await }
        })
        .await;
}

#[allow(clippy::too_many_lines)]
pub(super) async fn run_download(channels: DownloadChannels, config: DownloadConfig) {
    let DownloadChannels {
        client_rx,
        event_tx: tx,
        mut url_rx,
        token_tx,
        pause_rx,
    } = channels;

    let progress: Arc<dyn DownloadProgress> = Arc::new(TuiProgress::new(tx.clone()));

    // Receive the pre-authenticated client from the login task
    let Some(rx) = client_rx else {
        let _ = tx.send(DownloadEvent::ScopeError {
            scope: "setup".to_string(),
            error: "No client channel available".to_string(),
        });
        return;
    };
    let Ok((mega_client, http)) = rx.await else {
        let _ = tx.send(DownloadEvent::ScopeError {
            scope: "setup".to_string(),
            error: "Login task dropped before sending client".to_string(),
        });
        return;
    };

    let dlc_cache = DlcKeyCache::new();

    let _ = tx.send(DownloadEvent::StatusMessage("Ready".to_string()));

    let runtime = DownloadRuntime {
        downloader: Arc::new(crate::Downloader::new(mega_client, config.clone())),
        http: Arc::new(http),
        dlc_cache: Arc::new(dlc_cache),
        progress,
        concurrent_files: config.concurrent_files.max(1),
    };
    let mut scheduler = SchedulerState::new();
    let mut pause_rx = pause_rx;

    loop {
        tokio::select! {
            request_opt = url_rx.recv() => {
                let Some(request) = request_opt else { break };
                if !handle_download_request(
                    request,
                    &runtime,
                    &mut scheduler,
                    &tx,
                    &token_tx,
                ).await {
                    break;
                }
                start_pending_downloads(&runtime, &mut scheduler, &tx, &token_tx, &pause_rx);
            }
            Some(result) = scheduler.join_set.join_next(), if !scheduler.active_downloads.is_empty() => {
                handle_download_join_result(result, &mut scheduler, &tx);
                start_pending_downloads(&runtime, &mut scheduler, &tx, &token_tx, &pause_rx);
            }
            changed = pause_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                if !*pause_rx.borrow() {
                    start_pending_downloads(&runtime, &mut scheduler, &tx, &token_tx, &pause_rx);
                }
            }
        }
    }

    while let Some(result) = scheduler.join_set.join_next().await {
        handle_download_join_result(result, &mut scheduler, &tx);
    }
}

fn queue_download_request_events(
    request: &DownloadRequest,
    tx: &mpsc::UnboundedSender<DownloadEvent>,
) {
    match request {
        DownloadRequest::SubmitUrl { url } => {
            let _ = tx.send(DownloadEvent::UrlQueued { url: url.clone() });
            let _ = tx.send(DownloadEvent::StatusMessage(
                "Processing 1 URL(s)...".to_string(),
            ));
        }
        DownloadRequest::ResumeFileIds { file_ids, .. } => {
            let _ = tx.send(DownloadEvent::StatusMessage(format!(
                "Refreshing {} queued file(s)...",
                file_ids.len()
            )));
        }
        DownloadRequest::ReverifyFileIds { file_ids, .. } => {
            let _ = tx.send(DownloadEvent::StatusMessage(format!(
                "Reverifying {} file(s)...",
                file_ids.len()
            )));
        }
        DownloadRequest::VerifyCompletedFileIds { file_ids, .. } => {
            let _ = tx.send(DownloadEvent::StatusMessage(format!(
                "Verifying {} completed file(s)...",
                file_ids.len()
            )));
        }
        DownloadRequest::SyncPendingOrder { .. } => {}
    }
}

async fn handle_download_request(
    request: DownloadRequest,
    runtime: &DownloadRuntime,
    scheduler: &mut SchedulerState,
    tx: &mpsc::UnboundedSender<DownloadEvent>,
    token_tx: &mpsc::UnboundedSender<TokenMessage>,
) -> bool {
    match request {
        DownloadRequest::SubmitUrl { .. } | DownloadRequest::ResumeFileIds { .. } => {
            queue_download_request_events(&request, tx);
            let batch = vec![request];
            let resolved =
                resolve_download_requests(&batch, &runtime.http, &runtime.dlc_cache, tx).await;
            let collected = collect_batch(&resolved, &runtime.downloader, &runtime.progress).await;
            let collected = scheduler.register_resolved_batch(collected);
            collected.emit_events(tx);
            let _ = token_tx;
            true
        }
        DownloadRequest::ReverifyFileIds {
            source_url,
            file_ids,
        } => {
            let paused = scheduler.pause_file_ids(&file_ids);
            queue_download_request_events(
                &DownloadRequest::ReverifyFileIds {
                    source_url: source_url.clone(),
                    file_ids: file_ids.clone(),
                },
                tx,
            );
            let reverified = reverify_resume_files(source_url, file_ids, runtime, tx).await;
            scheduler.unpause_downloads(paused.into_iter().filter(|download| {
                reverified.contains_key(&FileId::from(download.item.path.as_str()))
            }));
            true
        }
        DownloadRequest::VerifyCompletedFileIds {
            source_url,
            file_ids,
        } => {
            queue_download_request_events(
                &DownloadRequest::VerifyCompletedFileIds {
                    source_url: source_url.clone(),
                    file_ids: file_ids.clone(),
                },
                tx,
            );
            verify_completed_files(source_url, file_ids, runtime, tx).await;
            true
        }
        DownloadRequest::SyncPendingOrder { file_ids } => {
            scheduler.sync_pending_order(file_ids);
            true
        }
    }
}

/// Resolves download requests (including DLC files) into MEGA URLs.
async fn resolve_download_requests(
    requests: &[DownloadRequest],
    http: &Arc<reqwest::Client>,
    dlc_cache: &Arc<DlcKeyCache>,
    tx: &mpsc::UnboundedSender<DownloadEvent>,
) -> Vec<FetchedNodeSet> {
    let mut by_source: IndexMap<String, (RequestedFiles, HashMap<FileId, u64>, bool)> =
        IndexMap::new();

    for request in requests {
        match request {
            DownloadRequest::SubmitUrl { url } => {
                by_source
                    .entry(url.clone())
                    .and_modify(|entry| {
                        entry.0 = RequestedFiles::All;
                        entry.1.clear();
                        entry.2 = true;
                    })
                    .or_insert_with(|| (RequestedFiles::All, HashMap::new(), true));
            }
            DownloadRequest::ResumeFileIds {
                source_url,
                file_ids,
                attempt_ids,
            } => {
                let file_ids = file_ids.iter().cloned().collect::<IndexSet<_>>();
                let entry = by_source.entry(source_url.clone()).or_insert_with(|| {
                    (RequestedFiles::Only(IndexSet::new()), HashMap::new(), false)
                });
                match &mut entry.0 {
                    RequestedFiles::Only(existing) => {
                        existing.extend(file_ids);
                    }
                    RequestedFiles::All => {
                        // A submit request for this URL takes precedence and should force all
                        // files to be resolved.
                    }
                }
                entry.1.extend(attempt_ids.clone());
            }
            DownloadRequest::ReverifyFileIds { .. }
            | DownloadRequest::VerifyCompletedFileIds { .. }
            | DownloadRequest::SyncPendingOrder { .. } => {}
        }
    }

    let mut resolved = Vec::new();
    for (submitted_url, (file_ids, attempt_ids, emit_url_resolved)) in by_source {
        let sources = resolve_submitted_url(&submitted_url, http, dlc_cache, tx).await;
        for source in sources {
            let requested_files = file_ids.clone();
            let requested_attempt_ids = attempt_ids.clone();
            let nodes = match fetch_node_set(&source, http).await {
                Ok(nodes) => Some(nodes),
                Err(error) => {
                    let _ = tx.send(DownloadEvent::ScopeError {
                        scope: source.source_url.clone(),
                        error,
                    });
                    None
                }
            };
            resolved.push(FetchedNodeSet {
                resolved: source,
                nodes,
                requested_files,
                requested_attempt_ids,
                emit_url_resolved,
            });
        }
    }

    resolved
}

async fn resolve_submitted_url(
    url: &str,
    http: &Arc<reqwest::Client>,
    dlc_cache: &Arc<DlcKeyCache>,
    tx: &mpsc::UnboundedSender<DownloadEvent>,
) -> Vec<ResolvedUrl> {
    if is_dlc_path(url) {
        return resolve_dlc_urls(url, http, dlc_cache, tx).await;
    }

    vec![ResolvedUrl::direct(url)]
}

async fn resolve_dlc_urls(
    url: &str,
    http: &Arc<reqwest::Client>,
    dlc_cache: &Arc<DlcKeyCache>,
    tx: &mpsc::UnboundedSender<DownloadEvent>,
) -> Vec<ResolvedUrl> {
    let _ = tx.send(DownloadEvent::StatusMessage(format!(
        "Processing DLC: {url}"
    )));

    let dlc_path = match expand_dlc_path(url) {
        Ok(path) => path,
        Err(error) => {
            let _ = tx.send(DownloadEvent::ScopeError {
                scope: url.to_string(),
                error,
            });
            return Vec::new();
        }
    };

    match crate::parse_dlc_file(&dlc_path, http, dlc_cache).await {
        Ok(dlc_urls) => {
            let _ = tx.send(DownloadEvent::StatusMessage(format!(
                "DLC {url}: {} MEGA link(s)",
                dlc_urls.len()
            )));
            dlc_urls
                .into_iter()
                .map(|resolved_url| ResolvedUrl::from_source(resolved_url, url))
                .collect()
        }
        Err(e) => {
            let _ = tx.send(DownloadEvent::ScopeError {
                scope: url.to_string(),
                error: format!("DLC parse error: {e}"),
            });
            Vec::new()
        }
    }
}

fn expand_dlc_path(url: &str) -> Result<String, String> {
    if !url.starts_with('~') && !url.starts_with('/') {
        return Ok(url.to_string());
    }

    if !url.starts_with('~') {
        return Ok(url.to_string());
    }

    let Some(home) = dirs::home_dir() else {
        return Err("Could not determine home directory".to_string());
    };

    Ok(url.replacen('~', home.to_string_lossy().as_ref(), 1))
}

async fn verify_completed_files(
    source_url: String,
    file_ids: Vec<FileId>,
    runtime: &DownloadRuntime,
    tx: &mpsc::UnboundedSender<DownloadEvent>,
) {
    let mut requested = file_ids.into_iter().collect::<HashSet<_>>();
    let sources = resolve_submitted_url(&source_url, &runtime.http, &runtime.dlc_cache, tx).await;
    let progress: Arc<dyn DownloadProgress> = Arc::new(crate::NoProgress);
    let mut matched = 0usize;
    let mut items = Vec::new();

    for source in sources {
        let nodes = match fetch_node_set(&source, &runtime.http).await {
            Ok(nodes) => nodes,
            Err(error) => {
                let _ = tx.send(DownloadEvent::ScopeError {
                    scope: source.source_url,
                    error,
                });
                continue;
            }
        };
        let collected = runtime.downloader.collect_files(&nodes, &progress).await;
        for item in collected.completed {
            let id = FileId::from(item.path.as_str());
            if !requested.contains(&id) {
                continue;
            }
            requested.remove(&id);
            matched = matched.saturating_add(1);
            items.push(crate::OwnedDownloadItem {
                path: item.path,
                node: item.node.clone(),
                was_partial: item.was_partial,
            });
        }
    }

    let downloader = Arc::clone(&runtime.downloader);
    let tx_for_items = tx.clone();
    for_each_verification_item(items, PACKAGE_REVERIFY_CONCURRENCY, move |item| {
        let downloader = Arc::clone(&downloader);
        let tx = tx_for_items.clone();
        async move {
            let id = FileId::from(item.path.as_str());
            let progress = VerificationProgress::new(tx.clone(), id.clone());
            match downloader
                .verify_completed_file_with_progress(&item.node, &item.path, Some(&progress))
                .await
            {
                Ok(result) => {
                    progress.flush_pending();
                    let _ = tx.send(DownloadEvent::CompletedFileVerified {
                        id,
                        bytes: result.bytes,
                    });
                    let mut message = String::with_capacity(item.path.len().saturating_add(40));
                    let _ = write!(message, "Verified {}: ", item.path);
                    crate::format::push_formatted_bytes(&mut message, result.bytes);
                    message.push_str(" final file OK");
                    let _ = tx.send(DownloadEvent::StatusMessage(message));
                }
                Err(error) => {
                    progress.flush_pending();
                    let _ = tx.send(DownloadEvent::ScopeError {
                        scope: item.path,
                        error: format!("Final verification failed: {error}"),
                    });
                }
            }
        }
    })
    .await;

    if matched == 0 {
        let _ = tx.send(DownloadEvent::StatusMessage(format!(
            "No completed file(s) found to verify for {source_url}"
        )));
    }
    for id in requested {
        let _ = tx.send(DownloadEvent::VerificationSkipped {
            id,
            completed: true,
        });
    }
}

async fn reverify_resume_files(
    source_url: String,
    file_ids: Vec<FileId>,
    runtime: &DownloadRuntime,
    tx: &mpsc::UnboundedSender<DownloadEvent>,
) -> HashMap<FileId, crate::ResumeReverify> {
    let mut requested = file_ids.into_iter().collect::<HashSet<_>>();
    let reverified = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let sources = resolve_submitted_url(&source_url, &runtime.http, &runtime.dlc_cache, tx).await;
    let progress: Arc<dyn DownloadProgress> = Arc::new(crate::NoProgress);
    let mut matched = 0usize;
    let mut items = Vec::new();

    for source in sources {
        let nodes = match fetch_node_set(&source, &runtime.http).await {
            Ok(nodes) => nodes,
            Err(error) => {
                let _ = tx.send(DownloadEvent::ScopeError {
                    scope: source.source_url,
                    error,
                });
                continue;
            }
        };
        let collected = runtime.downloader.collect_files(&nodes, &progress).await;
        for item in collected
            .to_download
            .into_iter()
            .chain(collected.completed.into_iter())
        {
            let id = FileId::from(item.path.as_str());
            if !requested.contains(&id) {
                continue;
            }
            requested.remove(&id);
            matched = matched.saturating_add(1);
            items.push(crate::OwnedDownloadItem {
                path: item.path,
                node: item.node.clone(),
                was_partial: item.was_partial,
            });
        }
    }

    let downloader = Arc::clone(&runtime.downloader);
    let tx_for_items = tx.clone();
    let reverified_for_items = Arc::clone(&reverified);
    for_each_verification_item(items, PACKAGE_REVERIFY_CONCURRENCY, move |item| {
        let downloader = Arc::clone(&downloader);
        let tx = tx_for_items.clone();
        let reverified = Arc::clone(&reverified_for_items);
        async move {
            let id = FileId::from(item.path.as_str());
            let progress = VerificationProgress::new(tx.clone(), id.clone());
            match downloader
                .reverify_resume_file_with_progress(&item.node, &item.path, Some(&progress))
                .await
            {
                Ok(result) if result.sidecar_loaded => {
                    progress.flush_pending();
                    let _ = tx.send(DownloadEvent::ResumeReverified {
                        id: id.clone(),
                        chunks: result.chunks,
                        bytes: result.bytes,
                    });
                    reverified.lock().await.insert(id, result);
                    let _ = tx.send(DownloadEvent::StatusMessage(format!(
                        "Reverified {}: {} chunk(s), {} reusable",
                        item.path,
                        result.chunks,
                        format_bytes(result.bytes)
                    )));
                }
                Ok(result) => {
                    progress.flush_pending();
                    let _ = tx.send(DownloadEvent::ResumeReverified {
                        id: id.clone(),
                        chunks: 0,
                        bytes: 0,
                    });
                    reverified.lock().await.insert(id, result);
                    let _ = tx.send(DownloadEvent::StatusMessage(format!(
                        "Reverified {}: no resume sidecar",
                        item.path
                    )));
                }
                Err(error) => {
                    progress.flush_pending();
                    let _ = tx.send(DownloadEvent::ScopeError {
                        scope: item.path,
                        error: format!("Reverify failed: {error}"),
                    });
                }
            }
        }
    })
    .await;

    if matched == 0 {
        let _ = tx.send(DownloadEvent::StatusMessage(format!(
            "No matching file(s) found to reverify for {source_url}"
        )));
    }
    for id in requested {
        let _ = tx.send(DownloadEvent::VerificationSkipped {
            id,
            completed: false,
        });
    }

    match Arc::try_unwrap(reverified) {
        Ok(mutex) => mutex.into_inner(),
        Err(shared) => shared.lock().await.clone(),
    }
}

fn handle_download_join_result(
    result: Result<DownloadTaskResult, tokio::task::JoinError>,
    scheduler: &mut SchedulerState,
    tx: &mpsc::UnboundedSender<DownloadEvent>,
) {
    match result {
        Ok(task) => {
            scheduler.finish_download(&task.id, &task.result);
            if let Err(error) = task.result
                && !matches!(error, crate::Error::Cancelled)
            {
                let _ = tx.send(DownloadEvent::FileError {
                    id: task.id,
                    error: format!("Download failed: {error}"),
                    attempt_id: task.attempt_id,
                });
            }
        }
        Err(error) => {
            let _ = tx.send(DownloadEvent::ScopeError {
                scope: "download".to_string(),
                error: format!("Download task panicked: {error}"),
            });
        }
    }
}

fn start_pending_downloads(
    runtime: &DownloadRuntime,
    scheduler: &mut SchedulerState,
    event_tx: &mpsc::UnboundedSender<DownloadEvent>,
    token_tx: &mpsc::UnboundedSender<TokenMessage>,
    pause_rx: &watch::Receiver<bool>,
) {
    if *pause_rx.borrow() {
        return;
    }

    let capacity = runtime
        .concurrent_files
        .saturating_sub(scheduler.active_downloads.len());
    let startable = if capacity == 0 {
        Vec::new()
    } else if let Some(target) = scheduler.exclusive_resume_target.as_ref() {
        if scheduler.available_downloads.contains_key(target)
            && !scheduler.active_downloads.contains(target)
        {
            vec![target.clone()]
        } else {
            Vec::new()
        }
    } else {
        scheduler
            .pending_queue
            .iter()
            .take(capacity)
            .cloned()
            .collect::<Vec<_>>()
    };

    for file_id in startable {
        let Some(item) = scheduler.available_downloads.get(&file_id).cloned() else {
            continue;
        };
        scheduler
            .pending_queue
            .retain(|pending| pending != &file_id);
        if !scheduler.active_downloads.insert(file_id.clone()) {
            continue;
        }
        scheduler.clear_exclusive_resume_target(&file_id);
        let cancel_token = register_download_token(&item, token_tx);
        spawn_file_download(
            &mut scheduler.join_set,
            item,
            Arc::clone(&runtime.downloader),
            event_tx.clone(),
            pause_rx.clone(),
            cancel_token,
        );
    }
}

fn register_download_token(
    item: &QueuedDownload,
    token_tx: &mpsc::UnboundedSender<TokenMessage>,
) -> CancellationToken {
    let cancel_token = CancellationToken::new();
    let _ = token_tx.send(TokenMessage {
        file_id: item.item.path.clone().into(),
        token: cancel_token.clone(),
    });
    cancel_token
}

fn spawn_file_download(
    join_set: &mut tokio::task::JoinSet<DownloadTaskResult>,
    item: QueuedDownload,
    downloader: Arc<crate::Downloader>,
    event_tx: mpsc::UnboundedSender<DownloadEvent>,
    pause_rx: watch::Receiver<bool>,
    cancel_token: CancellationToken,
) {
    join_set.spawn(async move {
        let file_id: FileId = item.item.path.clone().into();
        let attempt_id = item.attempt_id;
        let progress = file_progress(&file_id, attempt_id, &event_tx);
        let result = downloader
            .download_file(
                &item.item.node,
                &item.item.path,
                &progress,
                item.trust_resume_state,
                Some(cancel_token),
            )
            .await;
        emit_pause_cancellation_if_needed(&file_id, attempt_id, &result, &pause_rx, &event_tx);
        DownloadTaskResult {
            id: item.item.path.into(),
            attempt_id,
            result,
        }
    });
}

fn file_progress(
    file_id: &FileId,
    attempt_id: u64,
    event_tx: &mpsc::UnboundedSender<DownloadEvent>,
) -> Arc<dyn DownloadProgress> {
    Arc::new(FileProgress {
        tx: event_tx.clone(),
        id: file_id.clone(),
        attempt_id,
    })
}

fn emit_pause_cancellation_if_needed(
    file_id: &FileId,
    attempt_id: u64,
    result: &crate::Result<crate::FileStats>,
    pause_rx: &watch::Receiver<bool>,
    event_tx: &mpsc::UnboundedSender<DownloadEvent>,
) {
    if matches!(result, Err(crate::Error::Cancelled)) && *pause_rx.borrow() {
        let _ = event_tx.send(DownloadEvent::FileCancelled {
            id: file_id.clone(),
            attempt_id,
        });
    }
}

async fn fetch_node_set(
    resolved: &ResolvedUrl,
    http: &Arc<reqwest::Client>,
) -> Result<mega::Nodes, String> {
    let fetch_result =
        std::panic::AssertUnwindSafe(crate::fetch_public_nodes(http, &resolved.source_url))
            .catch_unwind()
            .await;

    match fetch_result {
        Ok(Ok(nodes)) => Ok(nodes),
        Ok(Err(e)) => Err(format!("Fetch failed: {e}")),
        Err(panic) => Err(format!("Fetch panicked: {}", describe_panic(&*panic))),
    }
}

async fn collect_batch(
    node_sets: &[FetchedNodeSet],
    downloader: &Arc<crate::Downloader>,
    progress: &Arc<dyn DownloadProgress>,
) -> CollectedBatch {
    let mut queued_items = Vec::new();
    let mut completed_items = Vec::new();
    let mut duplicate_resolver = BatchDuplicateResolver::default();
    let mut skipped_count = 0;
    let mut partial_count = 0;
    let successful_submitted_urls = successful_submitted_urls(node_sets.iter());

    for node_set in node_sets {
        let collected = collect_node_set(node_set, downloader, progress).await;
        skipped_count += collected.skipped_count;
        partial_count += collected.partial_count;
        duplicate_resolver.extend_queued(
            &mut queued_items,
            &mut completed_items,
            collected.queued_items,
        );
        duplicate_resolver.extend_completed(
            &mut queued_items,
            &mut completed_items,
            collected.completed_items,
        );
    }
    CollectedBatch {
        queued_items,
        completed_items,
        skipped_count,
        partial_count,
        successful_submitted_urls,
    }
}

async fn collect_node_set(
    node_set: &FetchedNodeSet,
    downloader: &Arc<crate::Downloader>,
    progress: &Arc<dyn DownloadProgress>,
) -> CollectedNodeSet {
    let Some(nodes) = node_set.nodes.as_ref() else {
        return CollectedNodeSet {
            queued_items: Vec::new(),
            completed_items: Vec::new(),
            skipped_count: 0,
            partial_count: 0,
        };
    };

    let collected = downloader.collect_files(nodes, progress).await;
    let mut resolved = node_set.resolved.clone();
    let (package_id, package_display_name) = package_identity_for_nodes(nodes, &collected);
    resolved.package_id = Some(package_id);
    resolved.package_display_name = Some(package_display_name);
    let skipped_count = 0;
    let keep_file = |path: &str| -> bool {
        match &node_set.requested_files {
            RequestedFiles::All => true,
            RequestedFiles::Only(ids) => ids.contains(path),
        }
    };

    let to_download = collected
        .to_download
        .into_iter()
        .filter(|item| keep_file(&item.path))
        .collect::<Vec<_>>();
    let completed = collected
        .completed
        .into_iter()
        .filter(|item| keep_file(&item.path))
        .collect::<Vec<_>>();

    let mut partial_count: usize = 0;
    for item in &to_download {
        if item.was_partial {
            partial_count = partial_count.saturating_add(1);
        }
    }

    let to_download = to_download
        .into_iter()
        .map(|item| crate::OwnedDownloadItem {
            path: item.path.to_string(),
            node: item.node.clone(),
            was_partial: item.was_partial,
        })
        .collect::<Vec<_>>();

    let completed = completed
        .into_iter()
        .map(|item| crate::OwnedDownloadItem {
            path: item.path.to_string(),
            node: item.node.clone(),
            was_partial: item.was_partial,
        })
        .collect::<Vec<_>>();

    let to_download = order_items_by_request(to_download, &node_set.requested_files);
    let completed = order_items_by_request(completed, &node_set.requested_files);

    let queued_items = visible_downloads(
        to_download,
        &resolved,
        &node_set.requested_files,
        &node_set.requested_attempt_ids,
    );
    let completed_items = visible_downloads(
        completed,
        &resolved,
        &node_set.requested_files,
        &node_set.requested_attempt_ids,
    );

    CollectedNodeSet {
        queued_items,
        completed_items,
        skipped_count,
        partial_count,
    }
}

#[derive(Clone, Debug)]
struct BatchItemSnapshot {
    size: u64,
    modified_at: Option<i64>,
    sparse_checksum: Option<[u8; 16]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum BatchDestination {
    Queued,
    Completed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct BatchItemRef {
    destination: BatchDestination,
    index: usize,
}

#[derive(Default)]
struct BatchDuplicateResolver {
    item_paths: HashMap<(String, String), BatchItemRef>,
    used_paths: HashSet<(String, String)>,
}

impl BatchDuplicateResolver {
    fn extend_queued(
        &mut self,
        queued_items: &mut Vec<QueuedDownload>,
        completed_items: &mut Vec<QueuedDownload>,
        items: Vec<QueuedDownload>,
    ) {
        for item in items {
            self.insert(
                queued_items,
                completed_items,
                item,
                BatchDestination::Queued,
            );
        }
    }

    fn extend_completed(
        &mut self,
        queued_items: &mut Vec<QueuedDownload>,
        completed_items: &mut Vec<QueuedDownload>,
        items: Vec<QueuedDownload>,
    ) {
        for item in items {
            self.insert(
                queued_items,
                completed_items,
                item,
                BatchDestination::Completed,
            );
        }
    }

    fn insert(
        &mut self,
        queued_items: &mut Vec<QueuedDownload>,
        completed_items: &mut Vec<QueuedDownload>,
        mut item: QueuedDownload,
        destination: BatchDestination,
    ) {
        let package_id = batch_item_package_id(&item);
        let original_path = item.item.path.clone();
        let snapshot = batch_item_snapshot(&item);

        if let Some(existing_ref) = self
            .item_paths
            .get(&(package_id.clone(), original_path.clone()))
            .copied()
        {
            let existing_snapshot = self.snapshot_for(queued_items, completed_items, existing_ref);
            if remote_files_match(&existing_snapshot, &snapshot) {
                return;
            }

            if snapshot.size > existing_snapshot.size {
                let renamed_existing = next_available_duplicate_path(
                    &package_id,
                    &original_path,
                    &mut self.used_paths,
                );
                self.rename_item(
                    queued_items,
                    completed_items,
                    existing_ref,
                    &package_id,
                    &original_path,
                    &renamed_existing,
                );
            } else {
                let renamed_incoming = next_available_duplicate_path(
                    &package_id,
                    &original_path,
                    &mut self.used_paths,
                );
                item.item.path = renamed_incoming;
            }
        }

        let final_path = item.item.path.clone();
        let item_ref = self.push_item(queued_items, completed_items, item, destination);
        self.used_paths
            .insert((package_id.clone(), final_path.clone()));
        self.item_paths.insert((package_id, final_path), item_ref);
    }

    fn snapshot_for(
        &self,
        queued_items: &[QueuedDownload],
        completed_items: &[QueuedDownload],
        item_ref: BatchItemRef,
    ) -> BatchItemSnapshot {
        let item = match item_ref.destination {
            BatchDestination::Queued => &queued_items[item_ref.index],
            BatchDestination::Completed => &completed_items[item_ref.index],
        };
        batch_item_snapshot(item)
    }

    fn rename_item(
        &mut self,
        queued_items: &mut [QueuedDownload],
        completed_items: &mut [QueuedDownload],
        item_ref: BatchItemRef,
        package_id: &str,
        old_path: &str,
        new_path: &str,
    ) {
        let item = match item_ref.destination {
            BatchDestination::Queued => &mut queued_items[item_ref.index],
            BatchDestination::Completed => &mut completed_items[item_ref.index],
        };
        item.item.path = new_path.to_string();
        self.item_paths
            .remove(&(package_id.to_string(), old_path.to_string()));
        self.item_paths
            .insert((package_id.to_string(), new_path.to_string()), item_ref);
    }

    fn push_item(
        &self,
        queued_items: &mut Vec<QueuedDownload>,
        completed_items: &mut Vec<QueuedDownload>,
        item: QueuedDownload,
        destination: BatchDestination,
    ) -> BatchItemRef {
        match destination {
            BatchDestination::Queued => {
                queued_items.push(item);
                BatchItemRef {
                    destination,
                    index: queued_items.len() - 1,
                }
            }
            BatchDestination::Completed => {
                completed_items.push(item);
                BatchItemRef {
                    destination,
                    index: completed_items.len() - 1,
                }
            }
        }
    }
}

fn batch_item_package_id(item: &QueuedDownload) -> String {
    item.resolved
        .package_id
        .map(|package_id| package_id.to_string())
        .unwrap_or_else(|| item.resolved.source_url.clone())
}

fn package_identity_for_nodes(
    nodes: &mega::Nodes,
    collected: &crate::CollectedFiles<'_>,
) -> (PackageId, String) {
    let display_name = crate::download::infer_package_display_name(nodes, collected);
    let package_id = crate::download::infer_package_id(nodes, collected);
    (package_id, display_name)
}

fn batch_item_snapshot(item: &QueuedDownload) -> BatchItemSnapshot {
    BatchItemSnapshot {
        size: item.item.node.size(),
        modified_at: item.item.node.modified_at().map(|date| date.timestamp()),
        sparse_checksum: item.item.node.sparse_checksum().copied(),
    }
}

fn remote_files_match(left: &BatchItemSnapshot, right: &BatchItemSnapshot) -> bool {
    if let (Some(left_checksum), Some(right_checksum)) =
        (left.sparse_checksum, right.sparse_checksum)
    {
        return left_checksum == right_checksum;
    }
    left.size == right.size && left.modified_at.is_some() && left.modified_at == right.modified_at
}

fn next_available_duplicate_path(
    package_id: &str,
    path: &str,
    used_paths: &mut HashSet<(String, String)>,
) -> String {
    for ordinal in 2.. {
        let candidate = duplicate_path(path, ordinal);
        if used_paths.insert((package_id.to_string(), candidate.clone())) {
            return candidate;
        }
    }
    unreachable!("unbounded duplicate suffix search should always find a path")
}

fn duplicate_path(path: &str, ordinal: usize) -> String {
    let (parent, file_name) = path
        .rsplit_once('/')
        .map_or(("", path), |(parent, file)| (parent, file));
    let (stem, extension) = file_name
        .rsplit_once('.')
        .filter(|(stem, _)| !stem.is_empty())
        .map_or((file_name, ""), |(stem, extension)| (stem, extension));
    let renamed = if extension.is_empty() {
        format!("{stem} ({ordinal})")
    } else {
        format!("{stem} ({ordinal}).{extension}")
    };
    if parent.is_empty() {
        renamed
    } else {
        format!("{parent}/{renamed}")
    }
}

fn successful_submitted_urls<'a>(
    resolved_urls: impl IntoIterator<Item = &'a FetchedNodeSet>,
) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut urls = Vec::new();

    for resolved in resolved_urls {
        if !resolved.emit_url_resolved {
            continue;
        }
        if seen.insert(resolved.resolved.submitted_url.clone()) {
            urls.push(resolved.resolved.submitted_url.clone());
        }
    }

    urls
}

fn visible_downloads(
    items: Vec<crate::OwnedDownloadItem>,
    resolved: &ResolvedUrl,
    requested_files: &RequestedFiles,
    requested_attempt_ids: &HashMap<FileId, u64>,
) -> Vec<QueuedDownload> {
    items
        .into_iter()
        .map(|item| QueuedDownload {
            resolved: resolved.clone(),
            attempt_id: requested_attempt_ids
                .get(item.path.as_str())
                .copied()
                .unwrap_or(0),
            trust_resume_state: matches!(
                requested_files,
                RequestedFiles::Only(file_ids) if file_ids.contains(item.path.as_str())
            ),
            item,
        })
        .collect()
}

fn order_items_by_request(
    items: Vec<crate::OwnedDownloadItem>,
    requested_files: &RequestedFiles,
) -> Vec<crate::OwnedDownloadItem> {
    let RequestedFiles::Only(requested) = requested_files else {
        return items;
    };

    let mut by_id = items
        .into_iter()
        .map(|item| (FileId::from(item.path.clone()), item))
        .collect::<HashMap<_, _>>();
    requested
        .iter()
        .filter_map(|file_id| by_id.remove(file_id))
        .collect()
}
