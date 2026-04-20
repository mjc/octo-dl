//! Download task management and event handling.
//!
//! # TODO: Package-based download grouping
//!
//! Currently, downloads are tracked as a flat list of files keyed by filename.
//! This causes collisions when different MEGA folders contain files with the
//! same name. Instead, downloads should be grouped into **packages** (similar
//! to JDownloader2), where each MEGA URL/folder submission becomes a package
//! that contains its files. This would:
//!
//! - Eliminate filename collisions across different sources
//! - Allow per-package progress, pause/resume, and cancellation
//! - Enable collapsible package groups in the TUI file list
//! - Make session persistence more robust (key by package+path, not filename)

use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::{
    DlcKeyCache, DownloadConfig, DownloadProgress, SavedCredentials, SessionState, UrlEntry,
    UrlStatus, format_bytes, is_dlc_path,
};
use dirs;

use super::app::{App, FileEntry, FileStatus, Popup};
use super::event::{DownloadChannels, DownloadEvent, TokenMessage, TuiProgress};
use super::input::add_url;

fn build_http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .pool_idle_timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(8)
        .tcp_keepalive(Duration::from_secs(30))
        .build()
}

fn describe_panic(panic: Box<dyn std::any::Any + Send>) -> String {
    if let Some(msg) = panic.downcast_ref::<&str>() {
        (*msg).to_string()
    } else if let Some(msg) = panic.downcast_ref::<String>() {
        msg.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

#[derive(Clone)]
struct ResolvedUrl {
    url: String,
    session_url: String,
}

struct QueuedDownload {
    id: String,
    name: String,
    source_url: String,
    session_url: String,
    item: crate::OwnedDownloadItem,
}

/// Spawns the login task which sends back `LoginResult`.
///
/// On success, the authenticated `mega::Client` and `reqwest::Client` are sent
/// via the oneshot channel in `app.client_rx` so the download task can reuse
/// them without logging in a second time.
pub fn start_login(app: &mut App) {
    let tx = app.event_tx.clone();
    let email = app.login.email().to_owned();
    let password = app.login.password().to_owned();
    let mfa = app.login.mfa_option().map(str::to_owned);

    let (client_tx, client_rx) = tokio::sync::oneshot::channel();
    app.client_rx = Some(client_rx);

    tokio::spawn(async move {
        let _ = tx.send(DownloadEvent::StatusMessage("Logging in...".to_string()));

        let http = match build_http_client() {
            Ok(http) => http,
            Err(e) => {
                let _ = tx.send(DownloadEvent::LoginResult {
                    success: false,
                    error: Some(format!("Failed to build HTTP client: {e}")),
                });
                return;
            }
        };

        let mut mega_client = match mega::Client::builder().build(http.clone()) {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(DownloadEvent::LoginResult {
                    success: false,
                    error: Some(format!("Failed to create MEGA client: {e}")),
                });
                return;
            }
        };

        if let Err(e) = mega_client.login(&email, &password, mfa.as_deref()).await {
            let _ = tx.send(DownloadEvent::LoginResult {
                success: false,
                error: Some(format!("Login failed: {e}")),
            });
            return;
        }

        let _ = client_tx.send((mega_client, http));
        let _ = tx.send(DownloadEvent::LoginResult {
            success: true,
            error: None,
        });
    });
}

pub fn handle_login_result(app: &mut App, success: bool, error: Option<String>) {
    app.login.logging_in = false;
    if success {
        app.authenticated = true;
        app.popup = Popup::None;
        app.status = "Login successful".to_string();

        // Start the download task — it takes the url_rx and immediately
        // receives any URLs that were already buffered in the channel.
        start_download_task(app);
    } else {
        app.login.error = error;
        app.popup = Popup::Login;
    }
}

pub fn handle_file_complete(app: &mut App, id: &str, name: &str) {
    app.cancellation_tokens.remove(id);
    if let Some(fp) = app.find_file_mut(id) {
        fp.status = FileStatus::Complete;
        fp.downloaded = fp.size;
        fp.speed = 0;
        fp.speed_accum = 0;
        fp.name = name.to_string();
    }
    app.files_completed += 1;

    if let Some(ref mut session) = app.session {
        let _ = session.remove_file(id);
    }

    if app.files_completed == app.files_total && app.files_total > 0 {
        if let Some(ref mut session) = app.session {
            let _ = session.mark_completed();
        }
        app.status = "All downloads complete".to_string();
    } else {
        app.status = format!("Downloading ({}/{})", app.files_completed, app.files_total);
    }
}

pub fn handle_file_error(app: &mut App, id: &str, name: &str, error: &str) {
    app.cancellation_tokens.remove(id);
    if let Some(fp) = app.find_file_mut(id) {
        fp.status = FileStatus::Error(error.to_string());
        fp.speed = 0;
        fp.speed_accum = 0;
        fp.name = name.to_string();
    } else {
        app.files.push(FileEntry {
            id: id.to_string(),
            name: name.to_string(),
            size: 0,
            downloaded: 0,
            speed: 0,
            speed_accum: 0,
            source_url: None,
            status: FileStatus::Error(error.to_string()),
        });
    }

    if let Some(ref mut session) = app.session {
        let _ = session.mark_file_error(id, error);
    }
}

/// Show error in UI without persisting to session.
/// Used for URL-level errors that should never be retried.
pub fn show_error_ui_only(app: &mut App, name: &str, error: &str) {
    app.cancellation_tokens.remove(name);
    if let Some(fp) = app.find_file_mut(name) {
        fp.status = FileStatus::Error(error.to_string());
        fp.speed = 0;
        fp.speed_accum = 0;
    } else {
        app.files.push(FileEntry {
            id: name.to_string(),
            name: name.to_string(),
            size: 0,
            downloaded: 0,
            speed: 0,
            speed_accum: 0,
            source_url: None,
            status: FileStatus::Error(error.to_string()),
        });
    }
}

#[allow(clippy::too_many_lines)]
pub fn handle_download_event(app: &mut App, event: DownloadEvent) {
    match event {
        DownloadEvent::LoginResult { success, error } => {
            if success {
                log::info!("Login successful");
            } else {
                log::error!("Login failed: {}", error.as_deref().unwrap_or("unknown"));
            }
            handle_login_result(app, success, error);
        }
        DownloadEvent::FilesCollected {
            total,
            skipped,
            partial,
            total_bytes,
        } => {
            log::info!(
                "Files collected: {total} total, {skipped} skipped, {partial} partial, {}",
                format_bytes(total_bytes)
            );
            app.status = format!("Found {total} files ({skipped} skipped, {partial} partial)");
        }
        DownloadEvent::FileStart { id, name, size } => {
            log::info!("Download started: {name} ({})", format_bytes(size));
            if app.deleted_files.contains(&id) {
                return;
            }
            if let Some(fp) = app.find_file_mut(&id) {
                fp.status = FileStatus::Downloading;
                fp.name = name;
                fp.size = size;
            } else {
                app.files.push(FileEntry {
                    id,
                    name,
                    size,
                    downloaded: 0,
                    speed: 0,
                    speed_accum: 0,
                    source_url: None,
                    status: FileStatus::Downloading,
                });
            }
            app.recompute_totals();
        }
        DownloadEvent::Progress {
            id,
            bytes_delta,
            speed,
        } => {
            if app.deleted_files.contains(id.as_ref()) {
                return;
            }
            let _ = speed; // lifetime average from library — ignored, we compute our own
            if let Some(fp) = app.find_file_mut(id.as_ref()) {
                fp.downloaded = fp.downloaded.saturating_add(bytes_delta);
                fp.speed_accum = fp.speed_accum.saturating_add(bytes_delta);
            }
            app.total_downloaded = app.total_downloaded.saturating_add(bytes_delta);
        }
        DownloadEvent::FileComplete { id, name } => {
            log::info!("Download complete: {name}");
            if app.deleted_files.remove(&id) {
                app.cancellation_tokens.remove(&id);
                if let Some(ref mut session) = app.session {
                    let _ = session.remove_file(&id);
                }
                return;
            }
            handle_file_complete(app, &id, &name);
            app.recompute_totals();
        }
        DownloadEvent::FileCancelled { id, name } => {
            log::info!("Download cancelled: {name}");
            if app.deleted_files.remove(&id) {
                app.cancellation_tokens.remove(&id);
                if let Some(ref mut session) = app.session {
                    let _ = session.remove_file(&id);
                }
                return;
            }
            app.cancellation_tokens.remove(&id);
            if let Some(fp) = app.find_file_mut(&id) {
                fp.status = FileStatus::Queued;
                fp.speed = 0;
                fp.speed_accum = 0;
            }
            if app.paused {
                app.status = "Paused".to_string();
            }
            app.recompute_totals();
        }
        DownloadEvent::Error { id, name, error } => {
            log::error!("Download error: {name}: {error}");
            if let Some(id) = id.as_ref()
                && app.deleted_files.remove(id)
            {
                app.cancellation_tokens.remove(id);
                if let Some(ref mut session) = app.session {
                    let _ = session.remove_file(id);
                }
                return;
            }
            // URL-level fetch/parse errors are terminal for resume purposes.
            if app
                .session
                .as_ref()
                .is_some_and(|session| session.urls.iter().any(|u| u.url == name))
            {
                if let Some(ref mut session) = app.session {
                    if let Some(url_entry) = session.urls.iter_mut().find(|u| u.url == name) {
                        url_entry.status = UrlStatus::Error(error.clone());
                    }
                    let _ = session.save();
                }
                // Remove URL placeholder from UI and show error without persisting as file
                app.files.retain(|f| f.id != name);
                show_error_ui_only(app, &name, &error);
            } else if let Some(id) = id {
                // For actual file download errors, mark as error and keep in session for retry
                handle_file_error(app, &id, &name, &error);
            } else {
                show_error_ui_only(app, &name, &error);
            }
            app.recompute_totals();
        }
        DownloadEvent::UrlQueued { url } => {
            if app.deleted_files.contains(&url) {
                return;
            }
            // Add a placeholder entry showing the URL while we fetch file info
            if !app.files.iter().any(|f| f.id == url) {
                app.files.push(FileEntry {
                    id: url.clone(),
                    name: url,
                    size: 0,
                    downloaded: 0,
                    speed: 0,
                    speed_accum: 0,
                    source_url: None,
                    status: FileStatus::Queued,
                });
            }
            app.recompute_totals();
        }
        DownloadEvent::FileQueued {
            id,
            name,
            size,
            source_url,
            session_url,
        } => {
            if app.deleted_files.contains(&id) {
                return;
            }
            // Add a real file entry with stable identity and size.
            if let Some(fp) = app.find_file_mut(&id) {
                fp.name = name.clone();
                fp.size = size;
                fp.source_url = Some(source_url.clone());
            } else {
                app.files.push(FileEntry {
                    id: id.clone(),
                    name: name.clone(),
                    size,
                    downloaded: 0,
                    speed: 0,
                    speed_accum: 0,
                    source_url: Some(source_url.clone()),
                    status: FileStatus::Queued,
                });
            }

            // Track file in session for resume support
            if let Some(ref mut session) = app.session {
                let url_index = session
                    .urls
                    .iter()
                    .position(|u| u.url == session_url)
                    .unwrap_or(0);

                if !session.files.iter().any(|f| f.path == id) {
                    session.files.push(crate::FileEntry {
                        url_index,
                        path: id,
                        size,
                        status: crate::FileEntryStatus::Pending,
                    });
                    let _ = session.save();
                }
            }
            app.recompute_totals();
        }
        DownloadEvent::UrlResolved { url } => {
            // Remove the URL placeholder now that real file entries exist
            app.files.retain(|f| f.id != url);
            // Mark URL as fetched in session so it's not re-sent on resume
            if let Some(ref mut session) = app.session {
                if let Some(entry) = session.urls.iter_mut().find(|u| u.url == url) {
                    entry.status = UrlStatus::Fetched;
                }
                let _ = session.save();
            }
            app.recompute_totals();
        }
        DownloadEvent::StatusMessage(msg) => {
            log::info!("Status: {msg}");
            app.status = msg;
        }
        DownloadEvent::UrlsReceived { urls } => {
            let count = urls.len();
            for url in urls {
                add_url(app, url);
            }
            app.status = format!("Received {count} URL(s) from bookmarklet");
        }
    }
}

/// Starts the persistent download task. Called once after login succeeds.
///
/// Expects `app.client_rx` to contain the oneshot receiver from `start_login`.
fn start_download_task(app: &mut App) {
    let tx = app.event_tx.clone();
    let config = app.config.config.clone();

    let url_rx = app.url_rx.take().expect("start_download_task called twice");
    let pause_rx = app
        .pause_rx
        .take()
        .expect("start_download_task called twice");
    let token_tx = app
        .token_tx
        .take()
        .expect("start_download_task called twice");

    // Reuse existing session on resume, or create a new one
    if app.session.is_none() {
        let email = app.login.email().to_owned();
        let password = app.login.password().to_owned();
        let mfa = app.login.mfa_option().map(str::to_owned);
        let url_entries: Vec<UrlEntry> = app
            .urls
            .iter()
            .map(|url| UrlEntry {
                url: url.clone(),
                status: UrlStatus::Pending,
            })
            .collect();

        let session = SessionState::new(
            SavedCredentials::encrypt(&email, &password, mfa.as_deref()),
            config.clone(),
            url_entries,
        );
        let _ = session.save();
        app.session = Some(session);
    }

    // Take the oneshot receiver with the pre-authenticated client
    let client_rx = app.client_rx.take();

    let channels = DownloadChannels {
        client_rx,
        event_tx: tx,
        url_rx,
        token_tx,
        pause_rx,
    };

    tokio::spawn(async move {
        run_download(channels, config).await;
    });
}

#[allow(clippy::too_many_lines)]
async fn run_download(channels: DownloadChannels, config: DownloadConfig) {
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

    let downloader = Arc::new(crate::Downloader::new(mega_client, config.clone()));
    let http = Arc::new(http);
    let dlc_cache = Arc::new(dlc_cache);

    // Shared semaphore across all batches so concurrent_files is a global limit
    let semaphore = Arc::new(tokio::sync::Semaphore::new(config.concurrent_files));
    let mut join_set = tokio::task::JoinSet::new();

    loop {
        tokio::select! {
            url_opt = url_rx.recv() => {
                let Some(first_url) = url_opt else { break };
                let mut batch = vec![first_url];
                while let Ok(url) = url_rx.try_recv() {
                    batch.push(url);
                }

                for url in &batch {
                    let _ = tx.send(DownloadEvent::UrlQueued { url: url.clone() });
                }

                let _ = tx.send(DownloadEvent::StatusMessage(format!(
                    "Processing {} URL(s)...",
                    batch.len()
                )));

                // Resolve URLs inline (fast, just URL/DLC parsing)
                let resolved = resolve_urls(&batch, &http, &dlc_cache, &tx).await;

                // Spawn the download work so we can receive new URLs immediately
                let dl = Arc::clone(&downloader);
                let prog = Arc::clone(&progress);
                let http2 = Arc::clone(&http);
                let sem = Arc::clone(&semaphore);
                let tx2 = tx.clone();
                let token_tx2 = token_tx.clone();
                let pause_rx2 = pause_rx.clone();
                join_set.spawn(async move {
                    download_batch(
                        &resolved,
                        &http2,
                        &dl,
                        &prog,
                        &sem,
                        &tx2,
                        &token_tx2,
                        pause_rx2,
                        &batch,
                    )
                    .await;
                });
            }
            Some(result) = join_set.join_next() => {
                if let Err(e) = result {
                    let _ = tx.send(DownloadEvent::Error {
                        id: None,
                        name: "download".to_string(),
                        error: format!("Batch task panicked: {e}"),
                    });
                }
            }
        }
    }

    // Drain remaining batch tasks
    while let Some(result) = join_set.join_next().await {
        if let Err(e) = result {
            let _ = tx.send(DownloadEvent::Error {
                id: None,
                name: "download".to_string(),
                error: format!("Batch task panicked: {e}"),
            });
        }
    }
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
        if is_dlc_path(url) {
            let _ = tx.send(DownloadEvent::StatusMessage(format!(
                "Processing DLC: {url}"
            )));
            // For local filesystem paths (starting with ~ or /), expand ~ to home directory
            let dlc_path = if url.starts_with('~') || url.starts_with('/') {
                if url.starts_with('~') {
                    if let Some(home) = dirs::home_dir() {
                        url.replacen('~', home.to_string_lossy().as_ref(), 1)
                    } else {
                        let _ = tx.send(DownloadEvent::Error {
                            id: None,
                            name: url.clone(),
                            error: "Could not determine home directory".to_string(),
                        });
                        continue;
                    }
                } else {
                    url.clone()
                }
            } else {
                url.clone()
            };
            match crate::parse_dlc_file(&dlc_path, http, dlc_cache).await {
                Ok(dlc_urls) => {
                    let _ = tx.send(DownloadEvent::StatusMessage(format!(
                        "DLC {url}: {} MEGA link(s)",
                        dlc_urls.len()
                    )));
                    resolved.extend(dlc_urls.into_iter().map(|resolved_url| ResolvedUrl {
                        url: resolved_url,
                        session_url: url.clone(),
                    }));
                }
                Err(e) => {
                    let _ = tx.send(DownloadEvent::Error {
                        id: None,
                        name: url.clone(),
                        error: format!("DLC parse error: {e}"),
                    });
                }
            }
        } else {
            resolved.push(ResolvedUrl {
                url: url.clone(),
                session_url: url.clone(),
            });
        }
    }
    resolved
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
    use tokio::sync::mpsc;

    fn test_app() -> App {
        let (tx, _rx) = mpsc::unbounded_channel();
        App::new(9723, tx, true)
    }

    /// Regression test: the mega library reports *cumulative* bytes downloaded,
    /// but `on_progress` must send true deltas.  If cumulative values leak
    /// through as deltas, `downloaded` will vastly exceed `size`.
    #[test]
    fn progress_deltas_do_not_exceed_file_size() {
        let mut app = test_app();
        let file_size: u64 = 1_000_000;

        // Simulate FileStart
        handle_download_event(
            &mut app,
            DownloadEvent::FileStart {
                id: "test.bin".to_string(),
                name: "test.bin".to_string(),
                size: file_size,
            },
        );

        // Simulate a sequence of correct *delta* progress events
        // (as they should arrive after the cumulative→delta fix in download.rs).
        let deltas = [100_000u64, 250_000, 350_000, 200_000, 100_000]; // sum = 1_000_000
        for d in deltas {
            handle_download_event(
                &mut app,
                DownloadEvent::Progress {
                    id: std::sync::Arc::<str>::from("test.bin"),
                    bytes_delta: d,
                    speed: 0,
                },
            );
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

    /// Verify that if buggy cumulative values were sent as deltas,
    /// downloaded would blow past the file size (the pre-fix behaviour).
    #[test]
    fn cumulative_values_as_deltas_would_overshoot() {
        let mut app = test_app();
        let file_size: u64 = 1_000_000;

        handle_download_event(
            &mut app,
            DownloadEvent::FileStart {
                id: "test.bin".to_string(),
                name: "test.bin".to_string(),
                size: file_size,
            },
        );

        // Simulate the OLD bug: cumulative totals sent as bytes_delta
        let cumulatives = [100_000u64, 350_000, 700_000, 900_000, 1_000_000];
        for c in cumulatives {
            handle_download_event(
                &mut app,
                DownloadEvent::Progress {
                    id: std::sync::Arc::<str>::from("test.bin"),
                    bytes_delta: c, // wrong! these are cumulative
                    speed: 0,
                },
            );
        }

        let file = app.files.iter().find(|f| f.id == "test.bin").unwrap();
        // Sum of cumulatives = 3_050_000, which is 3x the file size
        assert_eq!(file.downloaded, 3_050_000);
        assert!(
            file.downloaded > file.size,
            "this demonstrates the bug: {} > {}",
            file.downloaded,
            file.size,
        );
    }
}

async fn download_batch(
    urls: &[ResolvedUrl],
    http: &Arc<reqwest::Client>,
    downloader: &Arc<crate::Downloader>,
    progress: &Arc<dyn DownloadProgress>,
    semaphore: &Arc<tokio::sync::Semaphore>,
    event_tx: &mpsc::UnboundedSender<DownloadEvent>,
    token_tx: &mpsc::UnboundedSender<TokenMessage>,
    mut pause_rx: watch::Receiver<bool>,
    source_urls: &[String],
) {
    let mut node_sets: Vec<(ResolvedUrl, mega::Nodes)> = Vec::new();
    for resolved in urls {
        let _ = event_tx.send(DownloadEvent::StatusMessage(format!(
            "Fetching: {}",
            resolved.url
        )));
        let fetch_result =
            std::panic::AssertUnwindSafe(crate::fetch_public_nodes(http, &resolved.url))
                .catch_unwind()
                .await;

        match fetch_result {
            Ok(Ok(nodes)) => {
                node_sets.push((resolved.clone(), nodes));
            }
            Ok(Err(e)) => {
                let _ = event_tx.send(DownloadEvent::Error {
                    id: None,
                    name: resolved.url.clone(),
                    error: format!("Fetch failed: {e}"),
                });
            }
            Err(panic) => {
                let _ = event_tx.send(DownloadEvent::Error {
                    id: None,
                    name: resolved.url.clone(),
                    error: format!("Fetch panicked: {}", describe_panic(panic)),
                });
            }
        }
    }

    let mut all_queued_items = Vec::new();
    let mut actual_skipped = 0;
    let mut actual_partial = 0;

    for (resolved, nodes) in &node_sets {
        let collected = downloader.collect_files(nodes, progress).await;
        actual_skipped += collected.skipped;
        actual_partial += collected.partial;
        all_queued_items.extend(
            collected
                .into_owned()
                .into_iter()
                .map(|item| QueuedDownload {
                    id: item.path.clone(),
                    name: item.path.clone(),
                    source_url: resolved.url.clone(),
                    session_url: resolved.session_url.clone(),
                    item,
                }),
        );
    }

    let total_bytes: u64 = all_queued_items.iter().map(|i| i.item.node.size()).sum();
    let total_files = all_queued_items.len();

    let _ = event_tx.send(DownloadEvent::FilesCollected {
        total: total_files,
        skipped: actual_skipped,
        partial: actual_partial,
        total_bytes,
    });

    // Queue all files so they appear in the list immediately
    for item in &all_queued_items {
        let _ = event_tx.send(DownloadEvent::FileQueued {
            id: item.id.clone(),
            name: item.name.clone(),
            size: item.item.node.size(),
            source_url: item.source_url.clone(),
            session_url: item.session_url.clone(),
        });
    }

    // Remove URL placeholders now that real file entries exist
    for source_url in source_urls {
        let _ = event_tx.send(DownloadEvent::UrlResolved {
            url: source_url.clone(),
        });
    }

    // Download concurrently using JoinSet + shared Semaphore.
    // Permits are acquired BEFORE spawning so files start in queue order.
    let mut join_set = tokio::task::JoinSet::new();

    for item in all_queued_items {
        // Create a cancellation token for this download
        let cancel_token = CancellationToken::new();
        let token_msg = TokenMessage {
            file_id: item.id.clone(),
            token: cancel_token.clone(),
        };
        let _ = token_tx.send(token_msg);

        // Wait for a permit before spawning — this ensures files start in order.
        while *pause_rx.borrow() {
            if pause_rx.changed().await.is_err() {
                return;
            }
        }
        let permit = Arc::clone(semaphore)
            .acquire_owned()
            .await
            .expect("semaphore not closed");
        let dl = Arc::clone(downloader);
        let prog = Arc::clone(progress);
        let event_tx = event_tx.clone();
        let pause_rx_for_task = pause_rx.clone();
        join_set.spawn(async move {
            let _permit = permit; // held until download completes
            let result = dl
                .download_file(&item.item.node, &item.item.path, &prog, Some(cancel_token))
                .await;
            if matches!(result, Err(crate::Error::Cancelled)) && *pause_rx_for_task.borrow() {
                let _ = event_tx.send(DownloadEvent::FileCancelled {
                    id: item.id.clone(),
                    name: item.name.clone(),
                });
            }
            (item.id, item.name, result)
        });
    }

    while let Some(result) = join_set.join_next().await {
        match result {
            Ok((_id, _name, Ok(_stats))) => {}
            Ok((_id, _name, Err(crate::Error::Cancelled))) => {} // user cancelled
            Ok((id, name, Err(e))) => {
                let _ = event_tx.send(DownloadEvent::Error {
                    id: Some(id),
                    name,
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
}
