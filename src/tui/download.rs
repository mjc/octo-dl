//! Download task management and transport-side event emission.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::{DlcKeyCache, DownloadConfig, DownloadProgress, core::ProgressDelta, is_dlc_path};
#[cfg(test)]
use crate::{SavedCredentials, SessionState, UrlEntry, UrlStatus};
use dirs;

#[cfg(test)]
use super::app::{FileEntry, FileStatus};
use super::event::{
    DownloadChannels, DownloadEvent, FileOrigin, QueuedFile, TokenMessage, TuiProgress,
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

pub(crate) fn schedule_download_artifact_delete(path: String) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            if let Err(e) = crate::delete_download_artifacts(&path).await {
                log::warn!("Failed to delete download artifacts for {path}: {e}");
            }
        });
    } else {
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("Failed to delete output artifact {path}: {e}");
        }
        schedule_resume_artifact_delete(path);
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
}

impl ResolvedUrl {
    fn direct(url: &str) -> Self {
        Self {
            source_url: url.to_string(),
            submitted_url: url.to_string(),
        }
    }

    fn from_source(source_url: String, submitted_url: &str) -> Self {
        Self {
            source_url,
            submitted_url: submitted_url.to_string(),
        }
    }

    fn file_origin(&self) -> FileOrigin {
        FileOrigin {
            source_url: self.source_url.clone(),
            submitted_url: self.submitted_url.clone(),
        }
    }
}

struct FetchedNodeSet {
    resolved: ResolvedUrl,
    nodes: mega::Nodes,
}

struct QueuedDownload {
    resolved: ResolvedUrl,
    item: crate::OwnedDownloadItem,
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
            name: self.item.path.clone(),
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
    http: &'a Arc<reqwest::Client>,
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
    result: crate::Result<crate::FileStats>,
}

struct FileProgress {
    tx: mpsc::UnboundedSender<DownloadEvent>,
    id: String,
}

impl DownloadProgress for FileProgress {
    fn on_file_start(&self, _name: &str, size: u64) {
        let _ = self.tx.send(DownloadEvent::FileStart {
            id: self.id.clone(),
            name: self.id.clone(),
            size,
        });
    }

    fn on_progress(&self, _name: &str, delta: ProgressDelta) {
        let _ = self.tx.send(DownloadEvent::Progress {
            id: Arc::<str>::from(self.id.as_str()),
            delta,
        });
    }

    fn on_resume_reused(&self, _name: &str, chunks: usize, bytes: u64) {
        let _ = self.tx.send(DownloadEvent::ResumeReused {
            id: self.id.clone(),
            chunks,
            bytes,
        });
    }

    fn on_file_complete(&self, _name: &str, _stats: &crate::FileStats) {
        let _ = self.tx.send(DownloadEvent::FileComplete {
            id: self.id.clone(),
            name: self.id.clone(),
        });
    }

    fn on_error(&self, _name: &str, error: &str) {
        let _ = self.tx.send(DownloadEvent::Error {
            id: Some(self.id.clone()),
            name: self.id.clone(),
            error: error.to_string(),
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
        let _ = tx.send(DownloadEvent::Error {
            id: None,
            name: "setup".to_string(),
            error: "No client channel available".to_string(),
        });
        return;
    };
    let Ok((mega_client, http)) = rx.await else {
        let _ = tx.send(DownloadEvent::Error {
            id: None,
            name: "setup".to_string(),
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
            url_opt = url_rx.recv() => {
                let Some(first_url) = url_opt else { break };
                let batch = collect_url_batch(first_url, &mut url_rx);
                queue_url_batch(&batch, &tx);
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

fn collect_url_batch(
    first_url: String,
    url_rx: &mut mpsc::UnboundedReceiver<String>,
) -> Vec<String> {
    let mut batch = vec![first_url];
    while let Ok(url) = url_rx.try_recv() {
        batch.push(url);
    }
    batch
}

fn queue_url_batch(batch: &[String], tx: &mpsc::UnboundedSender<DownloadEvent>) {
    for url in batch {
        let _ = tx.send(DownloadEvent::UrlQueued { url: url.clone() });
    }

    let _ = tx.send(DownloadEvent::StatusMessage(format!(
        "Processing {} URL(s)...",
        batch.len()
    )));
}

fn spawn_download_batch(
    join_set: &mut tokio::task::JoinSet<()>,
    batch: Vec<String>,
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
        let resolved = resolve_urls(&batch, &http, &dlc_cache, &event_tx).await;
        download_batch(
            &resolved,
            pause_rx,
            &skipped_paths,
            BatchContext {
                http: &http,
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

/// Resolves raw URL strings (including DLC files) into MEGA URLs.
async fn resolve_urls(
    urls: &[String],
    http: &Arc<reqwest::Client>,
    dlc_cache: &Arc<DlcKeyCache>,
    tx: &mpsc::UnboundedSender<DownloadEvent>,
) -> Vec<ResolvedUrl> {
    let mut resolved = Vec::new();
    for url in urls {
        resolved.extend(resolve_submitted_url(url, http, dlc_cache, tx).await);
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
            let _ = tx.send(DownloadEvent::Error {
                id: None,
                name: url.to_string(),
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
            let _ = tx.send(DownloadEvent::Error {
                id: None,
                name: url.to_string(),
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
        let _ = tx.send(DownloadEvent::Error {
            id: None,
            name: "download".to_string(),
            error: format!("Batch task panicked: {e}"),
        });
    }
}

/// Fetches nodes from URLs, collects files, and downloads them.
///
/// The semaphore is shared across all batches to enforce a global concurrency
/// limit for file downloads.
#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::super::app::App;
    use super::super::event::DownloadEvent;
    use super::*;
    use crate::{FileEntry as SessionFileEntry, FileEntryStatus, SessionStatus};
    use std::env;
    use std::path::Path;
    use tempfile::tempdir;
    use tokio::sync::mpsc;

    struct StateDirectoryGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        previous: Option<std::ffi::OsString>,
    }

    impl StateDirectoryGuard {
        fn set(path: &Path) -> Self {
            let lock = crate::state::STATE_DIRECTORY_TEST_LOCK.lock().unwrap();
            let previous = env::var_os("STATE_DIRECTORY");
            unsafe { env::set_var("STATE_DIRECTORY", path) };
            Self {
                _lock: lock,
                previous,
            }
        }
    }

    impl Drop for StateDirectoryGuard {
        fn drop(&mut self) {
            if let Some(ref value) = self.previous {
                unsafe { env::set_var("STATE_DIRECTORY", value) };
            } else {
                unsafe { env::remove_var("STATE_DIRECTORY") };
            }
        }
    }

    fn test_app() -> App {
        let (tx, _rx) = mpsc::unbounded_channel();
        App::new(9723, tx, true)
    }

    fn session_with_file(path: &str, size: u64) -> SessionState {
        let mut session = SessionState::new(
            SavedCredentials::encrypt("test@example.com", "hunter2", None),
            DownloadConfig::default(),
            vec![UrlEntry {
                url: "https://mega.nz/folder/root".to_string(),
                status: UrlStatus::Fetched,
            }],
        );
        session.files.push(SessionFileEntry {
            key: None,
            url_index: 0,
            path: path.to_string(),
            size,
            status: FileEntryStatus::Pending,
        });
        session
    }

    #[test]
    fn describe_panic_handles_known_and_unknown_payloads() {
        let static_msg: &(dyn std::any::Any + Send) = &"static boom";
        let string_msg: &(dyn std::any::Any + Send) = &String::from("owned boom");
        let unknown_msg: &(dyn std::any::Any + Send) = &123_u32;

        assert_eq!(describe_panic(static_msg), "static boom");
        assert_eq!(describe_panic(string_msg), "owned boom");
        assert_eq!(describe_panic(unknown_msg), "unknown panic payload");
    }

    #[test]
    fn handle_file_complete_marks_session_file_complete() {
        let dir = tempdir().unwrap();
        let _guard = StateDirectoryGuard::set(dir.path());
        let mut app = test_app();
        app.files.push(FileEntry {
            id: "first.bin".to_string(),
            name: "first.bin".to_string(),
            size: 64,
            downloaded: 16,
            source_url: Some("https://mega.nz/file/first".to_string()),
            status: FileStatus::Downloading,
        });
        app.recompute_totals();
        let session = session_with_file("first.bin", 64);
        let session_path = session.state_path();
        app.session = Some(session);

        app.mark_visible_file_complete("first.bin", "renamed.bin");

        let file = app
            .files
            .iter()
            .find(|file| file.id == "first.bin")
            .expect("file should remain visible");
        assert_eq!(file.name, "renamed.bin");
        assert_eq!(file.status, FileStatus::Complete);
        assert_eq!(file.downloaded, 64);
        assert_eq!(app.status, "All downloads complete");
        assert!(session_path.exists());

        let session = app.session.as_ref().expect("session should remain");
        assert_eq!(session.files.len(), 1);
        assert_eq!(session.files[0].status, FileEntryStatus::Completed);
        assert_eq!(session.status, SessionStatus::Completed);
    }

    #[test]
    fn file_queued_clears_stale_error_state() {
        let mut app = test_app();
        app.files.push(FileEntry {
            id: "file-id".to_string(),
            name: "old-name.mkv".to_string(),
            size: 64,
            downloaded: 17,
            source_url: Some("https://mega.nz/file/old".to_string()),
            status: FileStatus::Error("stale error".to_string()),
        });

        app.handle_download_event(DownloadEvent::FileQueued(QueuedFile {
            id: "file-id".to_string(),
            size: 128,
            count_toward_progress: true,
            origin: FileOrigin {
                source_url: "https://mega.nz/file/new".to_string(),
                submitted_url: "https://mega.nz/folder/root".to_string(),
            },
        }));

        let file = app.files.iter().find(|file| file.id == "file-id").unwrap();
        assert_eq!(file.name, "file-id");
        assert_eq!(file.size, 128);
        assert_eq!(file.source_url.as_deref(), Some("https://mega.nz/file/new"));
        assert_eq!(file.status, FileStatus::Queued);
        assert_eq!(file.downloaded, 0);
        assert_eq!(app.file_speed("file-id"), 0);
    }

    #[test]
    fn file_queued_does_not_restore_session_skipped_file() {
        let dir = tempdir().unwrap();
        let _guard = StateDirectoryGuard::set(dir.path());
        let mut app = test_app();
        let mut session = SessionState::new(
            SavedCredentials::encrypt("test@example.com", "hunter2", None),
            DownloadConfig::default(),
            vec![UrlEntry {
                url: "https://mega.nz/file/root".to_string(),
                status: UrlStatus::Fetched,
            }],
        );
        session.files.push(SessionFileEntry {
            key: Some("0:episode.mkv".to_string()),
            url_index: 0,
            path: "episode.mkv".to_string(),
            size: 128,
            status: FileEntryStatus::Skipped,
        });
        app.session = Some(session);

        app.handle_download_event(DownloadEvent::FileQueued(QueuedFile {
            id: "episode.mkv".to_string(),
            size: 128,
            count_toward_progress: true,
            origin: FileOrigin {
                source_url: "https://mega.nz/file/root".to_string(),
                submitted_url: "https://mega.nz/file/root".to_string(),
            },
        }));

        assert!(app.files.is_empty());
        let session = app.session.as_ref().expect("session should remain");
        assert_eq!(session.files.len(), 1);
        assert_eq!(session.files[0].status, FileEntryStatus::Skipped);
    }

    #[test]
    fn url_placeholder_lives_in_overlay_until_resolved() {
        let mut app = test_app();
        let url = "https://mega.nz/folder/root".to_string();

        app.handle_download_event(DownloadEvent::UrlQueued { url: url.clone() });
        assert!(app.overlay_files.contains_key(&url));
        assert!(app.files.iter().any(|file| file.id == url));

        app.handle_download_event(DownloadEvent::UrlResolved { url: url.clone() });
        assert!(!app.overlay_files.contains_key(&url));
        assert!(!app.files.iter().any(|file| file.id == url));
    }

    #[test]
    fn url_level_error_replaces_placeholder_in_overlay() {
        let dir = tempdir().unwrap();
        let _guard = StateDirectoryGuard::set(dir.path());
        let mut app = test_app();
        let url = "https://mega.nz/folder/root".to_string();
        let session = SessionState::new(
            SavedCredentials::encrypt("test@example.com", "hunter2", None),
            DownloadConfig::default(),
            vec![UrlEntry {
                url: url.clone(),
                status: UrlStatus::Pending,
            }],
        );
        app.session = Some(session);

        app.handle_download_event(DownloadEvent::UrlQueued { url: url.clone() });
        app.handle_download_event(DownloadEvent::Error {
            id: None,
            name: url.clone(),
            error: "bad folder".to_string(),
        });

        let overlay = app
            .overlay_files
            .get(&url)
            .expect("url-level errors should remain in overlay");
        assert!(matches!(overlay.file.status, FileStatus::Error(ref msg) if msg == "bad folder"));
        let session = app.session.as_ref().expect("session should remain");
        assert!(matches!(
            session.urls[0].status,
            UrlStatus::Error(ref msg) if msg == "bad folder"
        ));
    }

    #[test]
    fn handle_file_complete_is_idempotent_for_visible_complete_rows() {
        let mut app = test_app();
        app.files.push(FileEntry {
            id: "file-id".to_string(),
            name: "file.mkv".to_string(),
            size: 128,
            downloaded: 128,
            source_url: Some("https://mega.nz/file/root".to_string()),
            status: FileStatus::Complete,
        });
        app.recompute_totals();
        assert_eq!(app.files_completed, 1);

        app.mark_visible_file_complete("file-id", "file.mkv");

        assert_eq!(app.files_completed, 1);
        let file = app.files.iter().find(|file| file.id == "file-id").unwrap();
        assert_eq!(file.status, FileStatus::Complete);
        assert_eq!(file.downloaded, 128);
    }

    #[test]
    fn completed_file_cannot_be_duplicated_by_startup_queue_events() {
        let mut app = test_app();
        app.upsert_overlay_file(
            FileEntry {
                id: "episode.mkv".to_string(),
                name: "episode.mkv".to_string(),
                size: 128,
                downloaded: 128,
                source_url: Some("https://mega.nz/file/root".to_string()),
                status: FileStatus::Complete,
            },
            false,
        );
        app.recompute_totals();

        app.handle_download_event(DownloadEvent::FileQueued(QueuedFile {
            id: "episode.mkv".to_string(),
            size: 128,
            count_toward_progress: false,
            origin: FileOrigin {
                source_url: "https://mega.nz/file/root".to_string(),
                submitted_url: "https://mega.nz/file/root".to_string(),
            },
        }));
        app.handle_download_event(DownloadEvent::FileComplete {
            id: "episode.mkv".to_string(),
            name: "episode.mkv".to_string(),
        });

        assert_eq!(app.files.len(), 1);
        let file = app
            .files
            .iter()
            .find(|file| file.id == "episode.mkv")
            .unwrap();
        assert_eq!(file.status, FileStatus::Complete);
        assert_eq!(file.downloaded, 128);
        assert_eq!(app.files_completed, 0);
        assert_eq!(app.files_total, 0);
    }

    #[test]
    fn successful_submitted_urls_deduplicates_only_fetched_submissions() {
        let resolved = vec![
            ResolvedUrl {
                source_url: "https://mega.nz/file/one".to_string(),
                submitted_url: "bundle.dlc".to_string(),
            },
            ResolvedUrl {
                source_url: "https://mega.nz/file/two".to_string(),
                submitted_url: "bundle.dlc".to_string(),
            },
            ResolvedUrl {
                source_url: "https://mega.nz/file/three".to_string(),
                submitted_url: "https://mega.nz/folder/direct".to_string(),
            },
        ];

        let urls = successful_submitted_urls(resolved.iter());

        assert_eq!(
            urls,
            vec![
                "bundle.dlc".to_string(),
                "https://mega.nz/folder/direct".to_string()
            ]
        );
    }

    #[test]
    fn resolved_url_direct_uses_same_source_and_submission() {
        let resolved = ResolvedUrl::direct("https://mega.nz/file/test");

        assert_eq!(resolved.source_url, "https://mega.nz/file/test");
        assert_eq!(resolved.submitted_url, "https://mega.nz/file/test");
    }

    #[test]
    fn expand_dlc_path_leaves_non_filesystem_inputs_unchanged() {
        assert_eq!(
            expand_dlc_path("bundle.dlc").unwrap(),
            "bundle.dlc".to_string()
        );
        assert_eq!(
            expand_dlc_path("/tmp/archive.dlc").unwrap(),
            "/tmp/archive.dlc".to_string()
        );
    }

    /// Regression test: the mega library reports *cumulative* bytes downloaded,
    /// but `on_progress` must send true deltas.  If cumulative values leak
    /// through as deltas, `downloaded` will vastly exceed `size`.
    #[test]
    fn progress_deltas_do_not_exceed_file_size() {
        let mut app = test_app();
        let file_size: u64 = 1_000_000;

        // Simulate FileStart
        app.handle_download_event(DownloadEvent::FileStart {
            id: "test.bin".to_string(),
            name: "test.bin".to_string(),
            size: file_size,
        });

        // Simulate a sequence of correct *delta* progress events
        // (as they should arrive after the cumulative→delta fix in download.rs).
        let deltas = [100_000u64, 250_000, 350_000, 200_000, 100_000]; // sum = 1_000_000
        for d in deltas {
            app.handle_download_event(DownloadEvent::Progress {
                id: std::sync::Arc::<str>::from("test.bin"),
                delta: ProgressDelta {
                    total_bytes_delta: d,
                    network_bytes_delta: d,
                },
            });
        }

        let file = app.files.iter().find(|f| f.id == "test.bin").unwrap();
        assert_eq!(
            file.downloaded, file_size,
            "downloaded should equal sum of deltas"
        );
        assert!(
            file.downloaded <= file.size,
            "downloaded ({}) must not exceed size ({})",
            file.downloaded,
            file.size,
        );
        assert_eq!(app.total_downloaded, file_size);
    }

    /// Verify that even if buggy cumulative values were sent as deltas,
    /// visible progress is capped at the known file size.
    #[test]
    fn cumulative_values_as_deltas_are_capped_at_file_size() {
        let mut app = test_app();
        let file_size: u64 = 1_000_000;

        app.handle_download_event(DownloadEvent::FileStart {
            id: "test.bin".to_string(),
            name: "test.bin".to_string(),
            size: file_size,
        });

        // Simulate the OLD bug: cumulative totals sent as bytes_delta
        let cumulatives = [100_000u64, 350_000, 700_000, 900_000, 1_000_000];
        for c in cumulatives {
            app.handle_download_event(DownloadEvent::Progress {
                id: std::sync::Arc::<str>::from("test.bin"),
                delta: ProgressDelta {
                    total_bytes_delta: c, // wrong! these are cumulative
                    network_bytes_delta: c,
                },
            });
        }

        let file = app.files.iter().find(|f| f.id == "test.bin").unwrap();
        assert_eq!(file.downloaded, file_size);
        assert_eq!(app.total_downloaded, file_size);
    }
}

async fn download_batch(
    urls: &[ResolvedUrl],
    mut pause_rx: watch::Receiver<bool>,
    skipped_session_paths: &HashMap<String, HashSet<String>>,
    ctx: BatchContext<'_>,
) {
    let node_sets = fetch_node_sets(urls, ctx.http, ctx.event_tx).await;
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
        let progress = file_progress(&file_id, &event_tx);
        let result = downloader
            .download_file(
                &item.item.node,
                &item.item.path,
                &progress,
                Some(cancel_token),
            )
            .await;
        emit_pause_cancellation_if_needed(&file_id, &result, &pause_rx, &event_tx);
        DownloadTaskResult {
            id: item.item.path,
            result,
        }
    });
}

fn file_progress(
    file_id: &str,
    event_tx: &mpsc::UnboundedSender<DownloadEvent>,
) -> Arc<dyn DownloadProgress> {
    Arc::new(FileProgress {
        tx: event_tx.clone(),
        id: file_id.to_string(),
    })
}

fn emit_pause_cancellation_if_needed(
    file_id: &str,
    result: &crate::Result<crate::FileStats>,
    pause_rx: &watch::Receiver<bool>,
    event_tx: &mpsc::UnboundedSender<DownloadEvent>,
) {
    if matches!(result, Err(crate::Error::Cancelled)) && *pause_rx.borrow() {
        let _ = event_tx.send(DownloadEvent::FileCancelled {
            id: file_id.to_string(),
            name: file_id.to_string(),
        });
    }
}

async fn fetch_node_sets(
    urls: &[ResolvedUrl],
    http: &Arc<reqwest::Client>,
    event_tx: &mpsc::UnboundedSender<DownloadEvent>,
) -> Vec<FetchedNodeSet> {
    let mut node_sets = Vec::new();
    for resolved in urls {
        let _ = event_tx.send(DownloadEvent::StatusMessage(format!(
            "Fetching: {}",
            resolved.source_url
        )));
        match fetch_node_set(resolved, http).await {
            Ok(nodes) => {
                node_sets.push(FetchedNodeSet {
                    resolved: resolved.clone(),
                    nodes,
                });
            }
            Err(error) => {
                let _ = event_tx.send(DownloadEvent::Error {
                    id: None,
                    name: resolved.source_url.clone(),
                    error,
                });
            }
        }
    }
    node_sets
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
    let mut skipped_count = 0;
    let mut partial_count = 0;
    let successful_submitted_urls =
        successful_submitted_urls(node_sets.iter().map(|node_set| &node_set.resolved));

    for node_set in node_sets {
        let collected = collect_node_set(node_set, downloader, progress, skipped_paths).await;
        skipped_count += collected.skipped_count;
        partial_count += collected.partial_count;
        queued_items.extend(collected.queued_items);
        completed_items.extend(collected.completed_items);
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
    let skipped_for_url = skipped_paths.get(&node_set.resolved.submitted_url);
    let collected = downloader.collect_files(&node_set.nodes, progress).await;
    let mut skipped_count = collected.skipped;
    let partial_count = collected.partial;
    let (to_download, completed) = collected.into_owned_parts();

    let queued_items = visible_downloads(
        to_download,
        &node_set.resolved,
        skipped_for_url,
        &mut skipped_count,
    );
    let completed_items = visible_downloads(
        completed,
        &node_set.resolved,
        skipped_for_url,
        &mut skipped_count,
    );

    CollectedNodeSet {
        queued_items,
        completed_items,
        skipped_count,
        partial_count,
    }
}

fn successful_submitted_urls<'a>(
    resolved_urls: impl IntoIterator<Item = &'a ResolvedUrl>,
) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut urls = Vec::new();

    for resolved in resolved_urls {
        if seen.insert(resolved.submitted_url.clone()) {
            urls.push(resolved.submitted_url.clone());
        }
    }

    urls
}

fn visible_downloads(
    items: Vec<crate::OwnedDownloadItem>,
    resolved: &ResolvedUrl,
    skipped_paths: Option<&HashSet<String>>,
    skipped_count: &mut usize,
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
        Ok(DownloadTaskResult { id, result: Err(e) }) => {
            let _ = event_tx.send(DownloadEvent::Error {
                id: Some(id.clone()),
                name: id.clone(),
                error: format!("Download failed: {e}"),
            });
        }
        Err(e) => {
            let _ = event_tx.send(DownloadEvent::Error {
                id: None,
                name: "download".to_string(),
                error: format!("Task panicked: {e}"),
            });
        }
    }
}
