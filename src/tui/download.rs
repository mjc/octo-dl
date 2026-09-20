//! Download task management and transport-side event emission.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

use indexmap::{IndexMap, IndexSet};
use rustc_hash::FxBuildHasher;
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
    DownloadChannels, DownloadEvent, DownloadEventSender, DownloadRequest, FileOrigin, QueuedFile,
    TokenMessage, TuiProgress, VerificationOperationId,
};

const PACKAGE_REVERIFY_CONCURRENCY: usize = 4;
const VERIFICATION_PROGRESS_EVENT_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct FileIdPtrKey {
    ptr: usize,
    len: usize,
}

fn file_id_ptr_key(file_id: &FileId) -> FileIdPtrKey {
    let id = file_id.as_str().as_bytes();
    FileIdPtrKey {
        ptr: id.as_ptr() as usize,
        len: id.len(),
    }
}

pub(crate) fn schedule_resume_artifact_delete(path: String) {
    for artifact in resume_artifact_paths(&path) {
        if let Err(error) = std::fs::remove_file(&artifact)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!(
                "Failed to delete resume artifact {}: {error}",
                artifact.display()
            );
        }
    }
}

pub(crate) fn schedule_output_artifact_delete(path: String) {
    if let Err(error) = std::fs::remove_file(&path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!("Failed to delete output artifact {path}: {error}");
    }
}

fn resume_artifact_paths(path: &str) -> [std::path::PathBuf; 4] {
    [
        crate::download::part_path(path),
        crate::download::sidecar_path(path),
        crate::download::legacy_binary_sidecar_path(path),
        crate::download::legacy_json_sidecar_path(path),
    ]
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
    submission_attempt_id: u64,
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
            attempt_id: self.attempt_id,
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

    fn emit_events(&self, event_tx: &DownloadEventSender) {
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

    fn emit_file_queue_events(&self, event_tx: &DownloadEventSender) {
        for item in &self.queued_items {
            let _ = event_tx.send(DownloadEvent::FileQueued(
                item.queued_event(FileAccounting::CurrentRun),
            ));
        }
    }

    fn emit_completed_file_events(&self, event_tx: &DownloadEventSender) {
        for item in &self.completed_items {
            let _ = event_tx.send(DownloadEvent::FileQueued(
                item.queued_event(FileAccounting::Preexisting),
            ));
            let _ = event_tx.send(item.complete_event());
        }
    }

    fn emit_url_resolved_events(&self, event_tx: &DownloadEventSender) {
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
    concurrent_files: usize,
    submission_attempts: Mutex<HashMap<String, u64>>,
}

impl DownloadRuntime {
    fn next_submission_attempt(&self, url: &str) -> u64 {
        let mut attempts = self.submission_attempts.lock().unwrap();
        let attempt = attempts.entry(url.to_string()).or_default();
        let current = *attempt;
        *attempt = attempt.saturating_add(1);
        current
    }
}

struct DownloadTaskResult {
    id: FileId,
    attempt_id: u64,
    result: crate::Result<crate::FileStats>,
}

struct SchedulerState {
    desired_pending_order: Vec<FileId>,
    desired_pending_set: HashSet<FileId, FxBuildHasher>,
    pending_queue: VecDeque<FileId>,
    resume_priority_set: HashSet<FileId, FxBuildHasher>,
    available_downloads: HashMap<FileId, QueuedDownload, FxBuildHasher>,
    available_download_ptrs: HashSet<FileIdPtrKey>,
    active_downloads: HashSet<FileId, FxBuildHasher>,
    active_download_ptrs: HashSet<FileIdPtrKey>,
    join_set: tokio::task::JoinSet<DownloadTaskResult>,
}

impl SchedulerState {
    fn new() -> Self {
        Self {
            desired_pending_order: Vec::new(),
            desired_pending_set: HashSet::with_hasher(FxBuildHasher::default()),
            pending_queue: VecDeque::new(),
            resume_priority_set: HashSet::with_hasher(FxBuildHasher::default()),
            available_downloads: HashMap::with_hasher(FxBuildHasher::default()),
            available_download_ptrs: HashSet::new(),
            active_downloads: HashSet::with_hasher(FxBuildHasher::default()),
            active_download_ptrs: HashSet::new(),
            join_set: tokio::task::JoinSet::new(),
        }
    }

    fn has_available_download(&self, file_id: &FileId) -> bool {
        self.available_download_ptrs
            .contains(&file_id_ptr_key(file_id))
            || self.available_downloads.contains_key(file_id)
    }

    fn has_active_download(&self, file_id: &FileId) -> bool {
        self.active_download_ptrs
            .contains(&file_id_ptr_key(file_id))
            || self.active_downloads.contains(file_id)
    }

    fn sync_pending_order(&mut self, file_ids: Vec<FileId>) {
        if file_ids == self.desired_pending_order {
            return;
        }
        if file_ids.len() >= self.desired_pending_order.len()
            && file_ids[..self.desired_pending_order.len()] == self.desired_pending_order
        {
            for file_id in &file_ids[self.desired_pending_order.len()..] {
                if self.has_available_download(file_id) && !self.has_active_download(file_id) {
                    self.pending_queue.push_back(file_id.clone());
                }
            }
            self.desired_pending_order = file_ids;
            self.rebuild_desired_pending_set();
            return;
        }
        self.desired_pending_order = file_ids;
        self.rebuild_desired_pending_set();
        self.rebuild_pending_queue();
    }

    fn register_resolved_batch(&mut self, batch: CollectedBatch) -> CollectedBatch {
        for item in &batch.queued_items {
            let file_id = FileId::from(item.item.path.as_str());
            self.available_download_ptrs
                .insert(file_id_ptr_key(&file_id));
            self.available_downloads.insert(file_id, item.clone());
        }
        batch
    }

    fn finish_download(&mut self, file_id: &FileId, result: &crate::Result<crate::FileStats>) {
        self.active_downloads.remove(file_id);
        self.active_download_ptrs.remove(&file_id_ptr_key(file_id));
        if matches!(result, Err(crate::Error::Cancelled)) {
            self.rebuild_pending_queue();
            return;
        }
        self.available_downloads.remove(file_id);
        self.available_download_ptrs
            .remove(&file_id_ptr_key(file_id));
    }

    fn pause_file_ids(&mut self, file_ids: &[FileId]) -> Vec<QueuedDownload> {
        let mut paused = Vec::new();
        let removed_file_ids = file_ids
            .iter()
            .map(|file_id| file_id.as_str())
            .collect::<HashSet<_, FxBuildHasher>>();
        for file_id in file_ids {
            let active = self.has_active_download(file_id);
            if let Some(download) = self.available_downloads.remove(file_id) {
                self.available_download_ptrs
                    .remove(&file_id_ptr_key(file_id));
                paused.push(download);
            }
            self.resume_priority_set.remove(file_id);
            if !active {
                self.desired_pending_set.remove(file_id);
            }
        }
        let active_file_ids = self.active_downloads.clone();
        self.desired_pending_order.retain(|file_id| {
            !removed_file_ids.contains(file_id.as_str()) || active_file_ids.contains(file_id)
        });
        self.pending_queue.retain(|file_id| {
            !removed_file_ids.contains(file_id.as_str()) || active_file_ids.contains(file_id)
        });
        paused
    }

    fn unpause_downloads(&mut self, downloads: impl IntoIterator<Item = QueuedDownload>) {
        for download in downloads {
            let file_id = FileId::from(download.item.path.as_str());
            self.available_download_ptrs
                .insert(file_id_ptr_key(&file_id));
            self.available_downloads.insert(file_id.clone(), download);
            if self.desired_pending_set.insert(file_id.clone()) {
                self.desired_pending_order.push(file_id.clone());
            }
            if !self.has_active_download(&file_id) {
                self.pending_queue.push_back(file_id.clone());
            }
            self.resume_priority_set.insert(file_id);
        }
    }

    fn mark_resume_priority_file_ids(&mut self, file_ids: &[FileId]) {
        self.resume_priority_set.extend(file_ids.iter().cloned());
    }

    fn clear_resume_priority_file_ids(&mut self, file_ids: impl IntoIterator<Item = FileId>) {
        for file_id in file_ids {
            self.resume_priority_set.remove(&file_id);
        }
    }

    fn rebuild_pending_queue(&mut self) {
        let desired_pending_order = &self.desired_pending_order;
        let available_download_ptrs = &self.available_download_ptrs;
        let available_downloads = &self.available_downloads;
        let active_download_ptrs = &self.active_download_ptrs;
        let active_downloads = &self.active_downloads;
        self.pending_queue.clear();
        for file_id in desired_pending_order {
            let key = file_id_ptr_key(file_id);
            let is_available =
                available_download_ptrs.contains(&key) || available_downloads.contains_key(file_id);
            let is_active =
                active_download_ptrs.contains(&key) || active_downloads.contains(file_id);
            if is_available && !is_active {
                self.pending_queue.push_back(file_id.clone());
            }
        }
    }

    fn rebuild_desired_pending_set(&mut self) {
        self.desired_pending_set.clear();
        self.desired_pending_set
            .extend(self.desired_pending_order.iter().cloned());
    }
}

#[cfg(test)]
fn contains_file_id_map_key<V>(
    ptrs: &HashSet<FileIdPtrKey>,
    ids: &HashMap<FileId, V>,
    file_id: &FileId,
) -> bool {
    ptrs.contains(&file_id_ptr_key(file_id)) || ids.contains_key(file_id)
}

#[cfg(test)]
fn select_startable_file_ids(
    pending_queue: &VecDeque<FileId>,
    resume_priority_set: &HashSet<FileId>,
    available_file_ids: &HashSet<FileId>,
    active_downloads: &HashSet<FileId>,
    capacity: usize,
) -> Vec<FileId> {
    if capacity == 0 {
        return Vec::new();
    }

    let resume_priority = pending_queue
        .iter()
        .filter(|file_id| {
            resume_priority_set.contains(*file_id)
                && available_file_ids.contains(*file_id)
                && !active_downloads.contains(*file_id)
        })
        .take(capacity)
        .cloned()
        .collect::<Vec<_>>();
    if !resume_priority.is_empty() {
        return resume_priority;
    }
    if resume_priority_set
        .iter()
        .any(|file_id| !active_downloads.contains(file_id))
    {
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
    tx: DownloadEventSender,
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
        let _ = self
            .tx
            .send(DownloadEvent::VerificationProgressForOperation {
                id: self.id.clone(),
                operation_id: VerificationOperationId::new(self.attempt_id),
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
    tx: DownloadEventSender,
    id: FileId,
    operation_id: Option<VerificationOperationId>,
    pending_bytes: AtomicU64,
}

impl VerificationProgress {
    fn new<E>(tx: E, id: FileId) -> Self
    where
        E: Into<DownloadEventSender>,
    {
        Self::with_operation(tx.into(), id, None)
    }

    fn with_operation(
        tx: DownloadEventSender,
        id: FileId,
        operation_id: Option<VerificationOperationId>,
    ) -> Self {
        Self {
            tx,
            id,
            operation_id,
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
        let event = self.operation_id.map_or_else(
            || DownloadEvent::VerificationProgress {
                id: self.id.clone(),
                bytes_delta,
            },
            |operation_id| DownloadEvent::VerificationProgressForOperation {
                id: self.id.clone(),
                operation_id,
                bytes_delta,
            },
        );
        let _ = self.tx.send(event);
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
        concurrent_files: config.concurrent_files.max(1),
        submission_attempts: Mutex::new(HashMap::new()),
    };
    let mut scheduler = SchedulerState::new();
    let mut pause_rx = pause_rx;

    loop {
        tokio::select! {
            request_opt = url_rx.recv() => {
                let Some(request) = request_opt else { break };
                if !handle_download_request_batch(
                    request,
                    &mut url_rx,
                    &runtime,
                    &mut scheduler,
                    &tx,
                    &token_tx,
                )
                .await
                {
                    break;
                }
                if !start_pending_downloads(&runtime, &mut scheduler, &tx, &token_tx, &pause_rx)
                    .await
                {
                    break;
                }
            }
            Some(result) = scheduler.join_set.join_next(), if !scheduler.active_downloads.is_empty() => {
                handle_download_join_result(result, &mut scheduler, &tx);
                if !flush_ready_download_requests(
                    &runtime,
                    &mut scheduler,
                    &tx,
                    &token_tx,
                    &mut url_rx,
                ).await {
                    break;
                }
                if !start_pending_downloads(&runtime, &mut scheduler, &tx, &token_tx, &pause_rx)
                    .await
                {
                    break;
                }
            }
            changed = pause_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                if !*pause_rx.borrow() {
                    if !flush_ready_download_requests(
                        &runtime,
                        &mut scheduler,
                        &tx,
                        &token_tx,
                        &mut url_rx,
                    ).await {
                        break;
                    }
                    if !start_pending_downloads(
                        &runtime,
                        &mut scheduler,
                        &tx,
                        &token_tx,
                        &pause_rx,
                    )
                    .await
                    {
                        break;
                    }
                }
            }
        }
    }

    while let Some(result) = scheduler.join_set.join_next().await {
        handle_download_join_result(result, &mut scheduler, &tx);
    }
}

fn drain_ready_requests(
    pending: &mut VecDeque<DownloadRequest>,
    url_rx: &mut mpsc::Receiver<DownloadRequest>,
) {
    while let Ok(request) = url_rx.try_recv() {
        pending.push_back(request);
    }
}

async fn handle_download_request_batch(
    first_request: DownloadRequest,
    url_rx: &mut mpsc::Receiver<DownloadRequest>,
    runtime: &DownloadRuntime,
    scheduler: &mut SchedulerState,
    tx: &DownloadEventSender,
    token_tx: &mpsc::Sender<TokenMessage>,
) -> bool {
    let mut pending = VecDeque::from([first_request]);
    loop {
        drain_ready_requests(&mut pending, url_rx);
        let Some(request) = pending.pop_front() else {
            return true;
        };
        if !handle_download_request(request, runtime, scheduler, tx, token_tx).await {
            return false;
        }
    }
}

async fn flush_ready_download_requests(
    runtime: &DownloadRuntime,
    scheduler: &mut SchedulerState,
    tx: &DownloadEventSender,
    token_tx: &mpsc::Sender<TokenMessage>,
    url_rx: &mut mpsc::Receiver<DownloadRequest>,
) -> bool {
    while let Ok(request) = url_rx.try_recv() {
        if !handle_download_request_batch(request, url_rx, runtime, scheduler, tx, token_tx).await {
            return false;
        }
    }
    true
}

fn queue_download_request_events(request: &DownloadRequest, tx: &DownloadEventSender) {
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
        DownloadRequest::ReverifyFileIds { file_ids, .. }
        | DownloadRequest::ReverifyFileIdsWithOperations { file_ids, .. } => {
            let _ = tx.send(DownloadEvent::StatusMessage(format!(
                "Reverifying {} file(s)...",
                file_ids.len()
            )));
        }
        DownloadRequest::VerifyCompletedFileIds { file_ids, .. }
        | DownloadRequest::VerifyCompletedFileIdsWithOperations { file_ids, .. } => {
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
    tx: &DownloadEventSender,
    token_tx: &mpsc::Sender<TokenMessage>,
) -> bool {
    match request {
        DownloadRequest::SubmitUrl { .. } | DownloadRequest::ResumeFileIds { .. } => {
            queue_download_request_events(&request, tx);
            let submission_attempt_id = match &request {
                DownloadRequest::SubmitUrl { url } => runtime.next_submission_attempt(url),
                DownloadRequest::ResumeFileIds { .. } => 0,
                _ => unreachable!(),
            };
            let batch = vec![request];
            let resolved = resolve_download_requests(
                &batch,
                submission_attempt_id,
                &runtime.http,
                &runtime.dlc_cache,
                tx,
            )
            .await;
            let progress = collection_progress(tx, &resolved);
            let collected = collect_batch(&resolved, &runtime.downloader, &progress).await;
            let collected = scheduler.register_resolved_batch(collected);
            collected.emit_events(tx);
            let _ = token_tx;
            true
        }
        DownloadRequest::ReverifyFileIds {
            source_url,
            file_ids,
        } => {
            handle_reverify_request(source_url, file_ids, HashMap::new(), runtime, scheduler, tx)
                .await;
            true
        }
        DownloadRequest::ReverifyFileIdsWithOperations {
            source_url,
            file_ids,
            operation_ids,
        } => {
            handle_reverify_request(source_url, file_ids, operation_ids, runtime, scheduler, tx)
                .await;
            true
        }
        DownloadRequest::VerifyCompletedFileIds {
            source_url,
            file_ids,
        } => {
            verify_completed_files(source_url, file_ids, HashMap::new(), runtime, tx).await;
            true
        }
        DownloadRequest::VerifyCompletedFileIdsWithOperations {
            source_url,
            file_ids,
            operation_ids,
        } => {
            verify_completed_files(source_url, file_ids, operation_ids, runtime, tx).await;
            true
        }
        DownloadRequest::SyncPendingOrder { file_ids } => {
            scheduler.sync_pending_order(file_ids);
            true
        }
    }
}

async fn handle_reverify_request(
    source_url: String,
    file_ids: Vec<FileId>,
    operation_ids: HashMap<FileId, VerificationOperationId>,
    runtime: &DownloadRuntime,
    scheduler: &mut SchedulerState,
    tx: &DownloadEventSender,
) {
    let paused = scheduler.pause_file_ids(&file_ids);
    let mut paused = paused;
    for download in &mut paused {
        let id = FileId::from(download.item.path.as_str());
        if let Some(operation_id) = operation_ids.get(&id) {
            download.attempt_id = operation_id.raw();
        }
    }
    for (id, operation_id) in &operation_ids {
        if let Some(download) = scheduler.available_downloads.get_mut(id) {
            download.attempt_id = operation_id.raw();
        }
    }
    let paused_ids = paused
        .iter()
        .map(|download| FileId::from(download.item.path.as_str()))
        .collect::<Vec<_>>();
    scheduler.mark_resume_priority_file_ids(&paused_ids);
    queue_download_request_events(
        &DownloadRequest::ReverifyFileIds {
            source_url: source_url.clone(),
            file_ids: file_ids.clone(),
        },
        tx,
    );
    let reverified =
        reverify_resume_files(source_url, file_ids.clone(), operation_ids, runtime, tx).await;
    let reverified_ids = reverified.keys().cloned().collect::<HashSet<_>>();
    scheduler.clear_resume_priority_file_ids(
        paused
            .iter()
            .map(|download| FileId::from(download.item.path.as_str()))
            .filter(|file_id| !reverified_ids.contains(file_id)),
    );
    scheduler.unpause_downloads(paused);
    scheduler.clear_resume_priority_file_ids(
        file_ids
            .iter()
            .filter(|file_id| !reverified_ids.contains(*file_id))
            .cloned(),
    );
}

/// Resolves download requests (including DLC files) into MEGA URLs.
async fn resolve_download_requests(
    requests: &[DownloadRequest],
    submission_attempt_id: u64,
    http: &Arc<reqwest::Client>,
    dlc_cache: &Arc<DlcKeyCache>,
    tx: &DownloadEventSender,
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
            | DownloadRequest::ReverifyFileIdsWithOperations { .. }
            | DownloadRequest::VerifyCompletedFileIds { .. }
            | DownloadRequest::VerifyCompletedFileIdsWithOperations { .. }
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
                submission_attempt_id,
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
    tx: &DownloadEventSender,
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
    tx: &DownloadEventSender,
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
    operation_ids: HashMap<FileId, VerificationOperationId>,
    runtime: &DownloadRuntime,
    tx: &DownloadEventSender,
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

    let operation_ids = Arc::new(operation_ids);
    let operation_ids_for_items = Arc::clone(&operation_ids);
    let downloader = Arc::clone(&runtime.downloader);
    let tx_for_items = tx.clone();
    for_each_verification_item(items, PACKAGE_REVERIFY_CONCURRENCY, move |item| {
        let downloader = Arc::clone(&downloader);
        let tx = tx_for_items.clone();
        let operation_ids = Arc::clone(&operation_ids_for_items);
        async move {
            let id = FileId::from(item.path.as_str());
            let operation_id = operation_ids.get(&id).copied();
            let progress =
                VerificationProgress::with_operation(tx.clone(), id.clone(), operation_id);
            match downloader
                .verify_completed_file_with_progress(&item.node, &item.path, Some(&progress))
                .await
            {
                Ok(result) => {
                    progress.flush_pending();
                    if let Some(operation_id) = operation_id {
                        let _ = tx.send(DownloadEvent::CompletedFileVerifiedForOperation {
                            id,
                            operation_id,
                            bytes: result.bytes,
                        });
                    } else {
                        let _ = tx.send(DownloadEvent::CompletedFileVerified {
                            id,
                            bytes: result.bytes,
                        });
                    }
                    let mut message = String::with_capacity(item.path.len().saturating_add(40));
                    let _ = write!(message, "Verified {}: ", item.path);
                    crate::format::push_formatted_bytes(&mut message, result.bytes);
                    message.push_str(" final file OK");
                    let _ = tx.send(DownloadEvent::StatusMessage(message));
                }
                Err(error) => {
                    progress.flush_pending();
                    if let Some(operation_id) = operation_id {
                        let _ = tx.send(DownloadEvent::VerificationFailed {
                            id,
                            operation_id,
                            error: format!("Final verification failed: {error}"),
                        });
                    } else {
                        let _ = tx.send(DownloadEvent::ScopeError {
                            scope: item.path,
                            error: format!("Final verification failed: {error}"),
                        });
                    }
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
        if let Some(operation_id) = operation_ids.get(&id).copied() {
            let _ = tx.send(DownloadEvent::VerificationFailed {
                id,
                operation_id,
                error: "Completed file was not found during verification".to_string(),
            });
        } else {
            let _ = tx.send(DownloadEvent::VerificationSkipped {
                id,
                completed: true,
            });
        }
    }
}

async fn reverify_resume_files(
    source_url: String,
    file_ids: Vec<FileId>,
    operation_ids: HashMap<FileId, VerificationOperationId>,
    runtime: &DownloadRuntime,
    tx: &DownloadEventSender,
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

    let operation_ids = Arc::new(operation_ids);
    let operation_ids_for_items = Arc::clone(&operation_ids);
    let downloader = Arc::clone(&runtime.downloader);
    let tx_for_items = tx.clone();
    let reverified_for_items = Arc::clone(&reverified);
    for_each_verification_item(items, PACKAGE_REVERIFY_CONCURRENCY, move |item| {
        let downloader = Arc::clone(&downloader);
        let tx = tx_for_items.clone();
        let operation_ids = Arc::clone(&operation_ids_for_items);
        let reverified = Arc::clone(&reverified_for_items);
        async move {
            let id = FileId::from(item.path.as_str());
            let operation_id = operation_ids.get(&id).copied();
            let progress =
                VerificationProgress::with_operation(tx.clone(), id.clone(), operation_id);
            match downloader
                .reverify_resume_file_with_progress(&item.node, &item.path, Some(&progress))
                .await
            {
                Ok(result) if result.sidecar_loaded => {
                    progress.flush_pending();
                    if let Some(operation_id) = operation_id {
                        let _ = tx.send(DownloadEvent::ResumeReverifiedForOperation {
                            id: id.clone(),
                            operation_id,
                            chunks: result.chunks,
                            bytes: result.bytes,
                        });
                    } else {
                        let _ = tx.send(DownloadEvent::ResumeReverified {
                            id: id.clone(),
                            chunks: result.chunks,
                            bytes: result.bytes,
                        });
                    }
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
                    if let Some(operation_id) = operation_id {
                        let _ = tx.send(DownloadEvent::ResumeReverifiedForOperation {
                            id: id.clone(),
                            operation_id,
                            chunks: 0,
                            bytes: 0,
                        });
                    } else {
                        let _ = tx.send(DownloadEvent::ResumeReverified {
                            id: id.clone(),
                            chunks: 0,
                            bytes: 0,
                        });
                    }
                    reverified.lock().await.insert(id, result);
                    let _ = tx.send(DownloadEvent::StatusMessage(format!(
                        "Reverified {}: no resume sidecar",
                        item.path
                    )));
                }
                Err(error) => {
                    progress.flush_pending();
                    if let Some(operation_id) = operation_id {
                        let _ = tx.send(DownloadEvent::VerificationFailed {
                            id,
                            operation_id,
                            error: format!("Reverify failed: {error}"),
                        });
                    } else {
                        let _ = tx.send(DownloadEvent::ScopeError {
                            scope: item.path,
                            error: format!("Reverify failed: {error}"),
                        });
                    }
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
        if let Some(operation_id) = operation_ids.get(&id).copied() {
            let _ = tx.send(DownloadEvent::VerificationFailed {
                id,
                operation_id,
                error: "File was not found during reverify".to_string(),
            });
        } else {
            let _ = tx.send(DownloadEvent::VerificationSkipped {
                id,
                completed: false,
            });
        }
    }

    match Arc::try_unwrap(reverified) {
        Ok(mutex) => mutex.into_inner(),
        Err(shared) => shared.lock().await.clone(),
    }
}

fn handle_download_join_result(
    result: Result<DownloadTaskResult, tokio::task::JoinError>,
    scheduler: &mut SchedulerState,
    tx: &DownloadEventSender,
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

async fn start_pending_downloads(
    runtime: &DownloadRuntime,
    scheduler: &mut SchedulerState,
    event_tx: &DownloadEventSender,
    token_tx: &mpsc::Sender<TokenMessage>,
    pause_rx: &watch::Receiver<bool>,
) -> bool {
    if *pause_rx.borrow() {
        return true;
    }

    let capacity = runtime
        .concurrent_files
        .saturating_sub(scheduler.active_downloads.len());
    let startable = if capacity == 0 {
        Vec::new()
    } else {
        let resume_priority = scheduler
            .pending_queue
            .iter()
            .filter(|file_id| {
                scheduler.resume_priority_set.contains(*file_id)
                    && scheduler.available_downloads.contains_key(*file_id)
                    && !scheduler.active_downloads.contains(*file_id)
            })
            .take(capacity)
            .cloned()
            .collect::<Vec<_>>();
        if !resume_priority.is_empty() {
            resume_priority
        } else if scheduler
            .resume_priority_set
            .iter()
            .any(|file_id| !scheduler.active_downloads.contains(file_id))
        {
            Vec::new()
        } else {
            scheduler
                .pending_queue
                .iter()
                .filter(|file_id| {
                    scheduler.available_downloads.contains_key(*file_id)
                        && !scheduler.active_downloads.contains(*file_id)
                })
                .take(capacity)
                .cloned()
                .collect::<Vec<_>>()
        }
    };

    for file_id in startable {
        let Some(item) = scheduler.available_downloads.get(&file_id).cloned() else {
            continue;
        };
        scheduler
            .pending_queue
            .retain(|pending| pending != &file_id);
        scheduler.resume_priority_set.remove(&file_id);
        if !scheduler.active_downloads.insert(file_id.clone()) {
            continue;
        }
        let cancel_token =
            match register_download_token(item.item.path.clone().into(), token_tx).await {
                Ok(cancel_token) => cancel_token,
                Err(error) => {
                    scheduler.active_downloads.remove(&file_id);
                    scheduler.pending_queue.push_front(file_id);
                    log::warn!("Unable to register download cancellation token: {error}");
                    return false;
                }
            };
        spawn_file_download(
            &mut scheduler.join_set,
            item,
            Arc::clone(&runtime.downloader),
            event_tx.clone(),
            pause_rx.clone(),
            cancel_token,
        );
    }
    true
}

async fn register_download_token(
    file_id: FileId,
    token_tx: &mpsc::Sender<TokenMessage>,
) -> Result<CancellationToken, mpsc::error::SendError<TokenMessage>> {
    let cancel_token = CancellationToken::new();
    token_tx
        .send(TokenMessage {
            file_id,
            token: cancel_token.clone(),
        })
        .await
        .map(|()| cancel_token)
}

fn spawn_file_download(
    join_set: &mut tokio::task::JoinSet<DownloadTaskResult>,
    item: QueuedDownload,
    downloader: Arc<crate::Downloader>,
    event_tx: DownloadEventSender,
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
    event_tx: &DownloadEventSender,
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
    event_tx: &DownloadEventSender,
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

fn collection_progress(
    event_tx: &DownloadEventSender,
    node_sets: &[FetchedNodeSet],
) -> Arc<dyn DownloadProgress> {
    let default_attempt_id = node_sets
        .first()
        .map_or(0, |node_set| node_set.submission_attempt_id);
    let attempt_ids = node_sets
        .iter()
        .flat_map(|node_set| node_set.requested_attempt_ids.iter())
        .map(|(id, attempt_id)| (id.clone(), *attempt_id))
        .collect();
    Arc::new(TuiProgress::with_attempt_ids(
        event_tx.clone(),
        default_attempt_id,
        attempt_ids,
    ))
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
    let skipped_count = completed.len();

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
        node_set.submission_attempt_id,
    );
    let completed_items = visible_downloads(
        completed,
        &resolved,
        &node_set.requested_files,
        &node_set.requested_attempt_ids,
        node_set.submission_attempt_id,
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
    submission_attempt_id: u64,
) -> Vec<QueuedDownload> {
    items
        .into_iter()
        .map(|item| QueuedDownload {
            resolved: resolved.clone(),
            attempt_id: requested_attempt_ids
                .get(item.path.as_str())
                .copied()
                .unwrap_or(submission_attempt_id),
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
