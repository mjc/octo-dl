//! Download task management and event handling.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use octo_dl::{
    DlcKeyCache, DownloadConfig, DownloadProgress, SavedCredentials, SessionState, UrlEntry,
    UrlStatus, is_dlc_path,
};

use crate::app::{App, FileEntry, FileStatus, Popup};
use crate::event::{DownloadChannels, DownloadEvent, TokenMessage, TuiProgress};
use crate::input::add_url;

fn build_http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .pool_idle_timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(8)
        .tcp_keepalive(Duration::from_secs(30))
        .build()
}

/// Spawns the login task which sends back `LoginResult`.
///
/// On success, the authenticated `mega::Client` and `reqwest::Client` are sent
/// via the oneshot channel in `app.client_rx` so the download task can reuse
/// them without logging in a second time.
pub fn start_login(app: &mut App) {
    let tx = app.event_tx.clone();
    let email = app.login.email.clone();
    let password = app.login.password.clone();
    let mfa = if app.login.mfa.is_empty() {
        None
    } else {
        Some(app.login.mfa.clone())
    };

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

        // Start the download task now that we're authenticated
        start_download_task(app);

        // Send queued URLs — skip already-fetched URLs on resume
        for url in &app.urls {
            let already_fetched = app.session.as_ref().is_some_and(|s| {
                s.urls
                    .iter()
                    .any(|u| u.url == *url && u.status == UrlStatus::Fetched)
            });
            if !already_fetched {
                if let Some(ref url_tx) = app.url_tx {
                    let _ = url_tx.send(url.clone());
                }
            }
        }
    } else {
        app.login.error = error;
        app.popup = Popup::Login;
    }
}

pub fn handle_file_complete(app: &mut App, name: &str) {
    app.cancellation_tokens.remove(name);
    if let Some(fp) = app.files.iter_mut().find(|f| f.name == name) {
        fp.status = FileStatus::Complete;
        fp.downloaded = fp.size;
        fp.speed = 0;
    }
    app.files_completed += 1;

    if let Some(ref mut session) = app.session {
        let _ = session.remove_file(name);
    }

    if app.files_completed == app.files_total && app.files_total > 0 {
        app.status = "All downloads complete".to_string();
    } else {
        app.status = format!("Downloading ({}/{})", app.files_completed, app.files_total);
    }
}

pub fn handle_file_error(app: &mut App, name: &str, error: &str) {
    app.cancellation_tokens.remove(name);
    if let Some(fp) = app.files.iter_mut().find(|f| f.name == name) {
        fp.status = FileStatus::Error(error.to_string());
        fp.speed = 0;
    } else {
        app.files.push(FileEntry {
            name: name.to_string(),
            size: 0,
            downloaded: 0,
            speed: 0,
            status: FileStatus::Error(error.to_string()),
        });
    }

    if let Some(ref mut session) = app.session {
        let _ = session.mark_file_error(name, error);
    }
}

#[allow(clippy::too_many_lines)]
pub fn handle_download_event(app: &mut App, event: DownloadEvent) {
    match event {
        DownloadEvent::LoginResult { success, error } => handle_login_result(app, success, error),
        DownloadEvent::FilesCollected {
            total,
            skipped,
            partial,
            total_bytes,
        } => {
            app.files_total += total;
            app.total_size += total_bytes;
            app.status = format!("Found {total} files ({skipped} skipped, {partial} partial)");
        }
        DownloadEvent::FileStart { name, size } => {
            if app.deleted_files.contains(&name) {
                return;
            }
            if let Some(fp) = app.files.iter_mut().find(|f| f.name == name) {
                fp.status = FileStatus::Downloading;
                fp.size = size;
            } else {
                app.files.push(FileEntry {
                    name,
                    size,
                    downloaded: 0,
                    speed: 0,
                    status: FileStatus::Downloading,
                });
            }
        }
        DownloadEvent::Progress {
            name,
            bytes_delta,
            speed,
        } => {
            if app.deleted_files.contains(&name) {
                return;
            }
            if let Some(fp) = app.files.iter_mut().find(|f| f.name == name) {
                fp.downloaded = fp.downloaded.saturating_add(bytes_delta);
                fp.speed = speed;
            }
            app.total_downloaded = app.total_downloaded.saturating_add(bytes_delta);
            // Aggregate speed = sum of all currently-downloading files' speeds
            app.current_speed = app
                .files
                .iter()
                .filter(|f| matches!(f.status, FileStatus::Downloading))
                .map(|f| f.speed)
                .sum();
        }
        DownloadEvent::FileComplete { name } => {
            if app.deleted_files.remove(&name) {
                app.cancellation_tokens.remove(&name);
                if let Some(ref mut session) = app.session {
                    let _ = session.remove_file(&name);
                }
                return;
            }
            handle_file_complete(app, &name);
        }
        DownloadEvent::Error { name, error } => {
            if app.deleted_files.remove(&name) {
                app.cancellation_tokens.remove(&name);
                if let Some(ref mut session) = app.session {
                    let _ = session.remove_file(&name);
                }
                return;
            }
            handle_file_error(app, &name, &error);
        }
        DownloadEvent::UrlQueued { url } => {
            if app.deleted_files.contains(&url) {
                return;
            }
            // Add a placeholder entry showing the URL while we fetch file info
            if !app.files.iter().any(|f| f.name == url) {
                app.files.push(FileEntry {
                    name: url,
                    size: 0,
                    downloaded: 0,
                    speed: 0,
                    status: FileStatus::Queued,
                });
            }
        }
        DownloadEvent::FileQueued { name, size } => {
            if app.deleted_files.contains(&name) {
                return;
            }
            // Add a real file entry with name and size
            if let Some(fp) = app.files.iter_mut().find(|f| f.name == name) {
                fp.size = size;
            } else {
                app.files.push(FileEntry {
                    name: name.clone(),
                    size,
                    downloaded: 0,
                    speed: 0,
                    status: FileStatus::Queued,
                });
            }

            // Track file in session for resume support
            if let Some(ref mut session) = app.session
                && !session.files.iter().any(|f| f.path == name)
            {
                session.files.push(octo_dl::FileEntry {
                    url_index: 0,
                    path: name,
                    size,
                    status: octo_dl::FileEntryStatus::Pending,
                });
                let _ = session.save();
            }
        }
        DownloadEvent::UrlResolved { url } => {
            // Remove the URL placeholder now that real file entries exist
            app.files.retain(|f| f.name != url);
            // Mark URL as fetched in session so it's not re-sent on resume
            if let Some(ref mut session) = app.session {
                if let Some(entry) = session.urls.iter_mut().find(|u| u.url == url) {
                    entry.status = UrlStatus::Fetched;
                }
                let _ = session.save();
            }
        }
        DownloadEvent::StatusMessage(msg) => {
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

    let (url_tx, url_rx) = mpsc::unbounded_channel::<String>();
    app.url_tx = Some(url_tx);
    let (token_tx, token_rx) = mpsc::unbounded_channel::<TokenMessage>();
    app.token_rx = Some(token_rx);

    // Reuse existing session on resume, or create a new one
    if app.session.is_none() {
        let email = app.login.email.clone();
        let password = app.login.password.clone();
        let mfa = if app.login.mfa.is_empty() {
            None
        } else {
            Some(app.login.mfa.clone())
        };
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
    } = channels;

    let progress: Arc<dyn DownloadProgress> = Arc::new(TuiProgress { tx: tx.clone() });

    // Receive the pre-authenticated client from the login task
    let Some(rx) = client_rx else {
        let _ = tx.send(DownloadEvent::Error {
            name: "setup".to_string(),
            error: "No client channel available".to_string(),
        });
        return;
    };
    let Ok((mega_client, http)) = rx.await else {
        let _ = tx.send(DownloadEvent::Error {
            name: "setup".to_string(),
            error: "Login task dropped before sending client".to_string(),
        });
        return;
    };

    let dlc_cache = DlcKeyCache::new();

    let _ = tx.send(DownloadEvent::StatusMessage("Ready".to_string()));

    let downloader = Arc::new(octo_dl::Downloader::new(mega_client, config.clone()));
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
                let sem = Arc::clone(&semaphore);
                let tx2 = tx.clone();
                let token_tx2 = token_tx.clone();
                join_set.spawn(async move {
                    download_batch(&resolved, &dl, &prog, &sem, &tx2, &token_tx2, &batch).await;
                });
            }
            Some(result) = join_set.join_next() => {
                if let Err(e) = result {
                    let _ = tx.send(DownloadEvent::Error {
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
) -> Vec<String> {
    let mut resolved = Vec::new();
    for url in urls {
        if is_dlc_path(url) {
            let _ = tx.send(DownloadEvent::StatusMessage(format!(
                "Processing DLC: {url}"
            )));
            match octo_dl::parse_dlc_file(url, http, dlc_cache).await {
                Ok(dlc_urls) => {
                    let _ = tx.send(DownloadEvent::StatusMessage(format!(
                        "DLC {url}: {} MEGA link(s)",
                        dlc_urls.len()
                    )));
                    resolved.extend(dlc_urls);
                }
                Err(e) => {
                    let _ = tx.send(DownloadEvent::Error {
                        name: url.clone(),
                        error: format!("DLC parse error: {e}"),
                    });
                }
            }
        } else {
            resolved.push(url.clone());
        }
    }
    resolved
}

/// Fetches nodes from URLs, collects files, and downloads them.
///
/// The semaphore is shared across all batches to enforce a global concurrency
/// limit for file downloads.
async fn download_batch(
    urls: &[String],
    downloader: &Arc<octo_dl::Downloader>,
    progress: &Arc<dyn DownloadProgress>,
    semaphore: &Arc<tokio::sync::Semaphore>,
    event_tx: &mpsc::UnboundedSender<DownloadEvent>,
    token_tx: &mpsc::UnboundedSender<TokenMessage>,
    source_urls: &[String],
) {
    let mut node_sets: Vec<mega::Nodes> = Vec::new();
    for url in urls {
        let _ = event_tx.send(DownloadEvent::StatusMessage(format!("Fetching: {url}")));
        match downloader.client().fetch_public_nodes(url).await {
            Ok(nodes) => {
                node_sets.push(nodes);
            }
            Err(e) => {
                let _ = event_tx.send(DownloadEvent::Error {
                    name: url.clone(),
                    error: format!("Fetch failed: {e}"),
                });
            }
        }
    }

    let mut all_owned_items = Vec::new();
    let mut actual_skipped = 0;
    let mut actual_partial = 0;

    for nodes in &node_sets {
        let collected = downloader.collect_files(nodes, progress).await;
        actual_skipped += collected.skipped;
        actual_partial += collected.partial;
        all_owned_items.extend(collected.into_owned());
    }

    let total_bytes: u64 = all_owned_items.iter().map(|i| i.node.size()).sum();
    let total_files = all_owned_items.len();

    let _ = event_tx.send(DownloadEvent::FilesCollected {
        total: total_files,
        skipped: actual_skipped,
        partial: actual_partial,
        total_bytes,
    });

    // Queue all files so they appear in the list immediately
    for item in &all_owned_items {
        let _ = event_tx.send(DownloadEvent::FileQueued {
            name: item.node.name().to_string(),
            size: item.node.size(),
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

    for item in all_owned_items {
        // Create a cancellation token for this download
        let cancel_token = CancellationToken::new();
        let token_msg = TokenMessage {
            file_path: item.node.name().to_string(),
            token: cancel_token.clone(),
        };
        let _ = token_tx.send(token_msg);

        // Wait for a permit before spawning — this ensures files start in order.
        let permit = Arc::clone(semaphore)
            .acquire_owned()
            .await
            .expect("semaphore not closed");
        let dl = Arc::clone(downloader);
        let prog = Arc::clone(progress);
        join_set.spawn(async move {
            let _permit = permit; // held until download completes
            dl.download_file(&item.node, &item.path, &prog, Some(cancel_token))
                .await
        });
    }

    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(Ok(_stats)) => {}
            Ok(Err(octo_dl::Error::Cancelled)) => {} // user cancelled
            Ok(Err(e)) => {
                let _ = event_tx.send(DownloadEvent::Error {
                    name: "download".to_string(),
                    error: format!("Download failed: {e}"),
                });
            }
            Err(e) => {
                let _ = event_tx.send(DownloadEvent::Error {
                    name: "download".to_string(),
                    error: format!("Task panicked: {e}"),
                });
            }
        }
    }
}
