//! Download task management and transport-side event emission.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

#[cfg(test)]
#[path = "download_tests.rs"]
mod tests;

use futures_util::FutureExt;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::download::part_path;
use crate::{DlcKeyCache, DownloadConfig, DownloadProgress, core::ProgressDelta, is_dlc_path};
use dirs;

use super::event::{
    DownloadChannels, DownloadEvent, DownloadRequest, FileOrigin, QueuedFile, TokenMessage,
    TuiProgress,
};

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

pub(crate) fn schedule_download_artifact_delete(path: String) {
    schedule_output_artifact_delete(path.clone());
    schedule_resume_artifact_delete(path);
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
    package_id: Option<String>,
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
    requested_file_ids: Option<HashSet<String>>,
    requested_attempt_ids: HashMap<String, u64>,
    emit_url_resolved: bool,
}

struct QueuedDownload {
    resolved: ResolvedUrl,
    item: crate::OwnedDownloadItem,
    attempt_id: u64,
    trust_resume_state: bool,
}

impl QueuedDownload {
    fn queued_event(&self, count_toward_progress: bool) -> QueuedFile {
        QueuedFile {
            id: self.item.path.clone(),
            size: self.item.node.size(),
            count_toward_progress,
            origin: self.resolved.file_origin(),
        }
    }

    fn complete_event(&self) -> DownloadEvent {
        DownloadEvent::FileComplete {
            id: self.item.path.clone(),
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
            let _ = event_tx.send(DownloadEvent::FileQueued(item.queued_event(true)));
        }
    }

    fn emit_completed_file_events(&self, event_tx: &mpsc::UnboundedSender<DownloadEvent>) {
        for item in &self.completed_items {
            let _ = event_tx.send(DownloadEvent::FileQueued(item.queued_event(false)));
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

struct BatchContext<'a> {
    downloader: &'a Arc<crate::Downloader>,
    progress: &'a Arc<dyn DownloadProgress>,
    semaphore: &'a Arc<tokio::sync::Semaphore>,
    event_tx: &'a mpsc::UnboundedSender<DownloadEvent>,
    token_tx: &'a mpsc::UnboundedSender<TokenMessage>,
}

struct DownloadRuntime {
    downloader: Arc<crate::Downloader>,
    http: Arc<reqwest::Client>,
    dlc_cache: Arc<DlcKeyCache>,
    progress: Arc<dyn DownloadProgress>,
    semaphore: Arc<tokio::sync::Semaphore>,
}

struct DownloadTaskResult {
    id: String,
    attempt_id: u64,
    result: crate::Result<crate::FileStats>,
}

struct FileProgress {
    tx: mpsc::UnboundedSender<DownloadEvent>,
    id: String,
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

    fn on_progress(&self, _name: &str, delta: ProgressDelta) {
        let _ = self.tx.send(DownloadEvent::Progress {
            id: Arc::<str>::from(self.id.as_str()),
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

#[allow(clippy::too_many_lines)]
pub(super) async fn run_download(channels: DownloadChannels, config: DownloadConfig) {
    let DownloadChannels {
        client_rx,
        event_tx: tx,
        mut url_rx,
        token_tx,
        pause_rx,
        skipped_session_paths,
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
        // Shared semaphore across all batches so concurrent_files is a global limit
        semaphore: Arc::new(tokio::sync::Semaphore::new(config.concurrent_files)),
    };
    let mut join_set = tokio::task::JoinSet::new();

    loop {
        tokio::select! {
            request_opt = url_rx.recv() => {
                let Some(first_request) = request_opt else { break };
                let batch = collect_download_requests(first_request, &mut url_rx);
                queue_download_batch_events(&batch, &tx);
                spawn_download_batch(
                    &mut join_set,
                    batch,
                    &runtime,
                    &tx,
                    &token_tx,
                    &pause_rx,
                    &skipped_session_paths,
                );
            }
            Some(result) = join_set.join_next() => {
                handle_batch_join_result(result, &tx);
            }
        }
    }

    // Drain remaining batch tasks
    while let Some(result) = join_set.join_next().await {
        handle_batch_join_result(result, &tx);
    }
}

fn collect_download_requests(
    first_request: DownloadRequest,
    url_rx: &mut mpsc::UnboundedReceiver<DownloadRequest>,
) -> Vec<DownloadRequest> {
    let mut batch = vec![first_request];
    while let Ok(request) = url_rx.try_recv() {
        batch.push(request);
    }
    batch
}

fn queue_download_batch_events(
    batch: &[DownloadRequest],
    tx: &mpsc::UnboundedSender<DownloadEvent>,
) {
    for url in batch {
        if let DownloadRequest::SubmitUrl { url } = url {
            let _ = tx.send(DownloadEvent::UrlQueued { url: url.clone() });
        }
    }

    let _ = tx.send(DownloadEvent::StatusMessage(format!(
        "Processing {} URL(s)...",
        batch.iter().count()
    )));
}

fn spawn_download_batch(
    join_set: &mut tokio::task::JoinSet<()>,
    batch: Vec<DownloadRequest>,
    runtime: &DownloadRuntime,
    tx: &mpsc::UnboundedSender<DownloadEvent>,
    token_tx: &mpsc::UnboundedSender<TokenMessage>,
    pause_rx: &watch::Receiver<bool>,
    skipped_session_paths: &HashMap<String, HashSet<String>>,
) {
    let downloader = Arc::clone(&runtime.downloader);
    let progress = Arc::clone(&runtime.progress);
    let http = Arc::clone(&runtime.http);
    let dlc_cache = Arc::clone(&runtime.dlc_cache);
    let semaphore = Arc::clone(&runtime.semaphore);
    let event_tx = tx.clone();
    let token_tx = token_tx.clone();
    let pause_rx = pause_rx.clone();
    let skipped_paths = skipped_session_paths.clone();

    join_set.spawn(async move {
        let resolved = resolve_download_requests(&batch, &http, &dlc_cache, &event_tx).await;
        download_batch(
            &resolved,
            pause_rx,
            &skipped_paths,
            BatchContext {
                downloader: &downloader,
                progress: &progress,
                semaphore: &semaphore,
                event_tx: &event_tx,
                token_tx: &token_tx,
            },
        )
        .await;
    });
}

/// Resolves download requests (including DLC files) into MEGA URLs.
async fn resolve_download_requests(
    requests: &[DownloadRequest],
    http: &Arc<reqwest::Client>,
    dlc_cache: &Arc<DlcKeyCache>,
    tx: &mpsc::UnboundedSender<DownloadEvent>,
) -> Vec<FetchedNodeSet> {
    let mut by_source: HashMap<String, (Option<HashSet<String>>, HashMap<String, u64>, bool)> =
        HashMap::new();

    for request in requests {
        match request {
            DownloadRequest::SubmitUrl { url } => {
                by_source
                    .entry(url.clone())
                    .and_modify(|entry| {
                        entry.0 = None;
                        entry.1.clear();
                        entry.2 = true;
                    })
                    .or_insert_with(|| (None, HashMap::new(), true));
            }
            DownloadRequest::ResumeFileIds {
                source_url,
                file_ids,
                attempt_ids,
            } => {
                let file_ids = file_ids.iter().cloned().collect::<HashSet<_>>();
                let entry = by_source
                    .entry(source_url.clone())
                    .or_insert_with(|| (None, HashMap::new(), false));
                match entry.0.as_mut() {
                    Some(existing) => {
                        existing.extend(file_ids);
                    }
                    None => {
                        // A submit request for this URL takes precedence and should force all
                        // files to be resolved.
                    }
                }
                entry.1.extend(attempt_ids.clone());
            }
        }
    }

    let mut resolved = Vec::new();
    for (submitted_url, (file_ids, attempt_ids, emit_url_resolved)) in by_source {
        let sources = resolve_submitted_url(&submitted_url, http, dlc_cache, tx).await;
        for source in sources {
            let requested_file_ids = file_ids.clone();
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
                requested_file_ids,
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

fn handle_batch_join_result(
    result: Result<(), tokio::task::JoinError>,
    tx: &mpsc::UnboundedSender<DownloadEvent>,
) {
    if let Err(e) = result {
        let _ = tx.send(DownloadEvent::ScopeError {
            scope: "download".to_string(),
            error: format!("Batch task panicked: {e}"),
        });
    }
}

/// Fetches nodes from URLs, collects files, and downloads them.
///
/// The semaphore is shared across all batches to enforce a global concurrency
/// limit for file downloads.

async fn download_batch(
    node_sets: &[FetchedNodeSet],
    mut pause_rx: watch::Receiver<bool>,
    skipped_session_paths: &HashMap<String, HashSet<String>>,
    ctx: BatchContext<'_>,
) {
    let collected = collect_batch(
        &node_sets,
        ctx.downloader,
        ctx.progress,
        skipped_session_paths,
    )
    .await;

    collected.emit_events(ctx.event_tx);

    let mut join_set = tokio::task::JoinSet::new();
    for item in collected.queued_items {
        let cancel_token = register_download_token(&item, ctx.token_tx);
        if wait_until_resumed(&mut pause_rx).await.is_err() {
            return;
        }
        let permit = acquire_download_permit(ctx.semaphore).await;
        spawn_file_download(
            &mut join_set,
            item,
            permit,
            Arc::clone(ctx.downloader),
            ctx.event_tx.clone(),
            pause_rx.clone(),
            cancel_token,
        );
    }

    drain_download_join_set(join_set, ctx.event_tx).await;
}

fn register_download_token(
    item: &QueuedDownload,
    token_tx: &mpsc::UnboundedSender<TokenMessage>,
) -> CancellationToken {
    let cancel_token = CancellationToken::new();
    let _ = token_tx.send(TokenMessage {
        file_id: item.item.path.clone(),
        token: cancel_token.clone(),
    });
    cancel_token
}

async fn wait_until_resumed(pause_rx: &mut watch::Receiver<bool>) -> Result<(), ()> {
    while *pause_rx.borrow() {
        if pause_rx.changed().await.is_err() {
            return Err(());
        }
    }
    Ok(())
}

async fn acquire_download_permit(
    semaphore: &Arc<tokio::sync::Semaphore>,
) -> tokio::sync::OwnedSemaphorePermit {
    Arc::clone(semaphore)
        .acquire_owned()
        .await
        .expect("semaphore not closed")
}

fn spawn_file_download(
    join_set: &mut tokio::task::JoinSet<DownloadTaskResult>,
    item: QueuedDownload,
    permit: tokio::sync::OwnedSemaphorePermit,
    downloader: Arc<crate::Downloader>,
    event_tx: mpsc::UnboundedSender<DownloadEvent>,
    pause_rx: watch::Receiver<bool>,
    cancel_token: CancellationToken,
) {
    join_set.spawn(async move {
        let _permit = permit;
        let file_id = item.item.path.clone();
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
            id: item.item.path,
            attempt_id,
            result,
        }
    });
}

fn file_progress(
    file_id: &str,
    attempt_id: u64,
    event_tx: &mpsc::UnboundedSender<DownloadEvent>,
) -> Arc<dyn DownloadProgress> {
    Arc::new(FileProgress {
        tx: event_tx.clone(),
        id: file_id.to_string(),
        attempt_id,
    })
}

fn emit_pause_cancellation_if_needed(
    file_id: &str,
    attempt_id: u64,
    result: &crate::Result<crate::FileStats>,
    pause_rx: &watch::Receiver<bool>,
    event_tx: &mpsc::UnboundedSender<DownloadEvent>,
) {
    if matches!(result, Err(crate::Error::Cancelled)) && *pause_rx.borrow() {
        let _ = event_tx.send(DownloadEvent::FileCancelled {
            id: file_id.to_string(),
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
    skipped_paths: &HashMap<String, HashSet<String>>,
) -> CollectedBatch {
    let mut queued_items = Vec::new();
    let mut completed_items = Vec::new();
    let mut duplicate_resolver = BatchDuplicateResolver::default();
    let mut skipped_count = 0;
    let mut partial_count = 0;
    let successful_submitted_urls = successful_submitted_urls(node_sets.iter());

    for node_set in node_sets {
        let collected = collect_node_set(node_set, downloader, progress, skipped_paths).await;
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
    skipped_paths: &HashMap<String, HashSet<String>>,
) -> CollectedNodeSet {
    let Some(nodes) = node_set.nodes.as_ref() else {
        return CollectedNodeSet {
            queued_items: Vec::new(),
            completed_items: Vec::new(),
            skipped_count: 0,
            partial_count: 0,
        };
    };

    let skipped_for_url = skipped_paths.get(&node_set.resolved.submitted_url);
    let collected = downloader.collect_files(nodes, progress).await;
    let mut skipped_count = 0;
    let keep_file = |path: &str| -> bool {
        node_set
            .requested_file_ids
            .as_ref()
            .is_none_or(|ids| ids.contains(path))
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
        if downloader.config().force_overwrite {
            continue;
        }
        if tokio::fs::metadata(&item.path)
            .await
            .is_ok_and(|metadata| metadata.len() == item.node.size())
        {
            continue;
        }
        if tokio::fs::metadata(part_path(&item.path)).await.is_ok() {
            partial_count = partial_count.saturating_add(1);
        }
    }

    let to_download = to_download
        .into_iter()
        .map(|item| crate::OwnedDownloadItem {
            path: item.path.to_string(),
            node: item.node.clone(),
        })
        .collect::<Vec<_>>();

    let completed = completed
        .into_iter()
        .map(|item| crate::OwnedDownloadItem {
            path: item.path.to_string(),
            node: item.node.clone(),
        })
        .collect::<Vec<_>>();

    let queued_items = visible_downloads(
        to_download,
        &node_set.resolved,
        skipped_for_url,
        &mut skipped_count,
        node_set.requested_file_ids.as_ref(),
        &node_set.requested_attempt_ids,
    );
    let completed_items = visible_downloads(
        completed,
        &node_set.resolved,
        skipped_for_url,
        &mut skipped_count,
        node_set.requested_file_ids.as_ref(),
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
    package_id: String,
    path: String,
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
            self.insert(queued_items, completed_items, item, BatchDestination::Queued);
        }
    }

    fn extend_completed(
        &mut self,
        queued_items: &mut Vec<QueuedDownload>,
        completed_items: &mut Vec<QueuedDownload>,
        items: Vec<QueuedDownload>,
    ) {
        for item in items {
            self.insert(queued_items, completed_items, item, BatchDestination::Completed);
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
                let renamed_existing =
                    next_available_duplicate_path(&package_id, &original_path, &mut self.used_paths);
                self.rename_item(
                    queued_items,
                    completed_items,
                    existing_ref,
                    &package_id,
                    &original_path,
                    &renamed_existing,
                );
            } else {
                let renamed_incoming =
                    next_available_duplicate_path(&package_id, &original_path, &mut self.used_paths);
                item.item.path = renamed_incoming;
            }
        }

        let final_path = item.item.path.clone();
        let item_ref = self.push_item(queued_items, completed_items, item, destination);
        self.used_paths.insert((package_id.clone(), final_path.clone()));
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
        self.item_paths.remove(&(package_id.to_string(), old_path.to_string()));
        self.item_paths.insert(
            (package_id.to_string(), new_path.to_string()),
            item_ref,
        );
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
        .clone()
        .unwrap_or_else(|| item.resolved.source_url.clone())
}

fn batch_item_snapshot(item: &QueuedDownload) -> BatchItemSnapshot {
    BatchItemSnapshot {
        package_id: batch_item_package_id(item),
        path: item.item.path.clone(),
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
    skipped_paths: Option<&HashSet<String>>,
    skipped_count: &mut usize,
    requested_file_ids: Option<&HashSet<String>>,
    requested_attempt_ids: &HashMap<String, u64>,
) -> Vec<QueuedDownload> {
    items
        .into_iter()
        .filter_map(|item| {
            if skipped_paths.is_some_and(|paths| paths.contains(&item.path)) {
                *skipped_count += 1;
                return None;
            }
            Some(QueuedDownload {
                resolved: resolved.clone(),
                attempt_id: requested_attempt_ids.get(&item.path).copied().unwrap_or(0),
                trust_resume_state: requested_file_ids
                    .is_some_and(|file_ids| file_ids.contains(&item.path)),
                item,
            })
        })
        .collect()
}

async fn drain_download_join_set(
    mut join_set: tokio::task::JoinSet<DownloadTaskResult>,
    event_tx: &mpsc::UnboundedSender<DownloadEvent>,
) {
    while let Some(result) = join_set.join_next().await {
        handle_download_join_result(result, event_tx);
    }
}

fn handle_download_join_result(
    result: Result<DownloadTaskResult, tokio::task::JoinError>,
    event_tx: &mpsc::UnboundedSender<DownloadEvent>,
) {
    match result {
        Ok(DownloadTaskResult {
            result: Ok(_stats), ..
        }) => {}
        Ok(DownloadTaskResult {
            result: Err(crate::Error::Cancelled),
            ..
        }) => {} // user cancelled
        Ok(DownloadTaskResult {
            id,
            attempt_id,
            result: Err(e),
        }) => {
            let _ = event_tx.send(DownloadEvent::FileError {
                id: id.clone(),
                error: format!("Download failed: {e}"),
                attempt_id,
            });
        }
        Err(e) => {
            let _ = event_tx.send(DownloadEvent::ScopeError {
                scope: "download".to_string(),
                error: format!("Task panicked: {e}"),
            });
        }
    }
}
