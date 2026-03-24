//! octo-tui - Interactive TUI for downloading MEGA files.

mod api;
mod app;
mod download;
mod draw;
mod event;
mod input;
mod terminal;
mod terminal_server;
pub mod web;

use std::env;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::Event;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

use crate::{ServiceConfig, SessionState, SessionStatus, UrlStatus, format_bytes};
use app::FileStatus;
use sysinfo::System;

use self::api::DEFAULT_API_PORT;
use self::app::{App, UiAction};
use self::download::{handle_download_event, start_login};
use self::draw::draw;
use self::event::DownloadEvent;

/// Options for the web UI server.
///
/// Passed to [`api::run_api_server`] when `--web` is enabled so that the
/// bookmarklet page and PWA manifest use the correct publicly-reachable host
/// (which may differ from the bind address when behind a reverse proxy).
#[derive(Clone)]
pub struct WebOptions {
    pub public_host: String,
}

/// RAII guard that ensures terminal cleanup on drop.
/// Restores terminal to normal mode even if a panic occurs.
struct TerminalGuard;

impl TerminalGuard {
    fn new() -> io::Result<Self> {
        enable_raw_mode()?;
        crossterm::execute!(
            io::stdout(),
            EnterAlternateScreen,
            crossterm::event::EnableBracketedPaste
        )?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = crossterm::execute!(
            io::stdout(),
            crossterm::event::DisableBracketedPaste,
            LeaveAlternateScreen
        );
    }
}
use self::input::{handle_input, handle_paste};

// ---------------------------------------------------------------------------
// Helpers — small, focused functions extracted from the three run variants
// ---------------------------------------------------------------------------

/// Loads the latest session (if any), pre-filling credentials and URLs.
///
/// Previously-fetched URLs are reset to pending so the download pipeline
/// re-evaluates them (files already on disk are skipped automatically).
pub(crate) fn resume_session(app: &mut App) {
    let Some(mut session) = SessionState::latest() else {
        return;
    };
    log::info!("Resuming session {}", session.id);

    if let Some((email, password, mfa)) = session.credentials.decrypt() {
        app.login
            .set_credentials(email, password, mfa.unwrap_or_default());
    }

    for entry in &mut session.urls {
        if entry.status == UrlStatus::Fetched {
            entry.status = UrlStatus::Pending;
        }
    }
    let resumed_urls: Vec<String> = session.urls.iter().map(|u| u.url.clone()).collect();
    app.urls = resumed_urls.clone();
    for url in resumed_urls {
        let _ = app.url_tx.send(url);
    }
    app.session = Some(session);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DownloadConfig, SavedCredentials, UrlEntry};
    use std::env;
    use std::path::Path;
    use tempfile::tempdir;

    struct StateDirectoryGuard;

    impl StateDirectoryGuard {
        fn set(path: &Path) -> Self {
            unsafe { env::set_var("STATE_DIRECTORY", path) };
            Self
        }
    }

    impl Drop for StateDirectoryGuard {
        fn drop(&mut self) {
            unsafe { env::remove_var("STATE_DIRECTORY") };
        }
    }

    #[test]
    fn resume_session_requeues_urls() {
        let dir = tempdir().unwrap();
        let _guard = StateDirectoryGuard::set(dir.path());

        let urls = vec![
            UrlEntry {
                url: "https://mega.nz/file/first".to_string(),
                status: UrlStatus::Pending,
            },
            UrlEntry {
                url: "https://mega.nz/file/second".to_string(),
                status: UrlStatus::Fetched,
            },
        ];
        let session = SessionState::new(
            SavedCredentials::encrypt("test@example.com", "hunter2", None),
            DownloadConfig::default(),
            urls,
        );
        session.save().unwrap();

        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(0, event_tx, true);

        resume_session(&mut app);

        let expected_urls = vec![
            "https://mega.nz/file/first".to_string(),
            "https://mega.nz/file/second".to_string(),
        ];
        assert_eq!(app.urls, expected_urls);

        let mut url_rx = app.url_rx.take().expect("url_rx should exist");
        assert_eq!(url_rx.try_recv().unwrap(), expected_urls[0]);
        assert_eq!(url_rx.try_recv().unwrap(), expected_urls[1]);
        assert!(url_rx.try_recv().is_err());

        let session_state = app.session.as_ref().expect("session should be present");
        assert!(
            session_state
                .urls
                .iter()
                .all(|entry| matches!(entry.status, UrlStatus::Pending))
        );
    }
}

/// Syncs the session file list with visible UI files before shutdown.
///
/// Removes files the user deleted, marks the session completed if nothing
/// remains, or paused otherwise.
pub(crate) fn sync_session_on_shutdown(app: &mut App) {
    let Some(ref mut session) = app.session else {
        return;
    };
    if session.status == SessionStatus::Completed {
        return;
    }

    let visible: std::collections::HashSet<&str> = app
        .files
        .iter()
        .filter(|f| {
            matches!(
                f.status,
                FileStatus::Queued | FileStatus::Downloading | FileStatus::Error(_)
            )
        })
        .map(|f| f.name.as_str())
        .collect();

    session.files.retain(|f| visible.contains(f.path.as_str()));

    if session.files.is_empty() {
        let _ = session.mark_completed();
    } else {
        log::info!("Marking session as paused for later resume");
        let _ = session.mark_paused();
    }
}

/// Drains all pending download events from the channel (non-blocking).
pub(crate) fn drain_download_events(
    app: &mut App,
    download_rx: &mut mpsc::UnboundedReceiver<DownloadEvent>,
) {
    while let Ok(event) = download_rx.try_recv() {
        handle_download_event(app, event);
    }
}

/// Drains all pending token messages (non-blocking).
pub(crate) fn drain_token_messages(app: &mut App) {
    while let Ok(msg) = app.token_rx.try_recv() {
        app.cancellation_tokens.insert(msg.file_path, msg.token);
    }
}

/// Logs a periodic progress summary when downloads are active.
fn log_progress(app: &mut App) {
    app.update_speeds();
    if app.files_total == 0 {
        return;
    }
    let pct = if app.total_size > 0 {
        app.total_downloaded * 100 / app.total_size
    } else {
        0
    };
    if pct > 0 && pct < 100 {
        log::info!(
            "[progress] {}/{} files, {} / {} ({}%), {}/s",
            app.files_completed,
            app.files_total,
            format_bytes(app.total_downloaded),
            format_bytes(app.total_size),
            pct,
            format_bytes(app.current_speed),
        );
    }
}

#[cfg(test)]
mod progress_tests {
    use super::app::{App, FileEntry, FileStatus};
    use super::event::DownloadEvent;
    use super::log_progress;
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc;

    fn build_test_app() -> App {
        let (tx, _rx) = mpsc::unbounded_channel::<DownloadEvent>();
        App::new(9723, tx, true)
    }

    #[test]
    fn log_progress_skips_logging_but_still_updates_speeds() {
        let mut app = build_test_app();
        app.files.push(FileEntry {
            name: "test".to_string(),
            size: 1024,
            downloaded: 0,
            speed: 0,
            speed_accum: 42,
            status: FileStatus::Downloading,
        });
        app.last_tick = Instant::now() - Duration::from_secs(1);

        log_progress(&mut app);

        assert!(app.current_speed > 0);
        assert_eq!(app.files[0].speed_accum, 0);
    }

    #[test]
    fn log_progress_updates_speeds_when_active() {
        let mut app = build_test_app();
        app.files.push(FileEntry {
            name: "busy".to_string(),
            size: 2048,
            downloaded: 512,
            speed: 0,
            speed_accum: 128,
            status: FileStatus::Downloading,
        });
        app.files_total = 1;
        app.total_downloaded = 512;
        app.total_size = 2048;
        app.last_tick = Instant::now() - Duration::from_secs(1);

        log_progress(&mut app);

        assert!(app.current_speed > 0);
        assert!(app.files.iter().all(|f| f.speed_accum == 0));
    }
}

/// Initiates login if credentials are available.
///
/// When no credentials are found, `fallback` determines what happens:
/// - [`NoCredentialsFallback::ShowPopup`] opens the login form (TUI / web).
/// - [`NoCredentialsFallback::Silent`] does nothing (headless API).
///
/// Returns `true` when login was initiated.
pub(crate) fn auto_login(app: &mut App, fallback: app::NoCredentialsFallback) -> bool {
    if app.login.has_credentials() {
        app.login.logging_in = true;
        app.status = "Logging in...".to_string();
        start_login(app);
        true
    } else {
        if fallback == app::NoCredentialsFallback::ShowPopup {
            app.popup = app::Popup::Login;
        }
        false
    }
}

/// Fills in credentials from environment variables if not already set.
///
/// Call this after `apply_service_config` / `resume_session` so that
/// env vars act as a fallback, never silently overriding explicit sources.
fn load_credentials_from_env(app: &mut App) {
    let email = env::var("MEGA_EMAIL").unwrap_or_default();
    let password = env::var("MEGA_PASSWORD").unwrap_or_default();
    let mfa = env::var("MEGA_MFA").unwrap_or_default();
    if !email.is_empty() || !password.is_empty() {
        log::info!("Using MEGA credentials from environment variables");
    }
    app.login
        .set_credentials_if_missing(&email, &password, &mfa);
}

/// Loads and validates a `ServiceConfig` from disk, applying download
/// directory, credentials, and download settings to the `App`.
///
/// Returns the validated `(api_host, api_port)` from the config.  If
/// credentials are stored in plaintext they are encrypted in-place and
/// the file is re-saved.
fn apply_service_config(app: &mut App, config_path: &Path) -> io::Result<(String, u16)> {
    let mut service_config = ServiceConfig::load_or_create(config_path)?;
    log::info!("Loaded config from {}", config_path.display());

    // Set working directory to [download] path
    if let Some(ref dl_path) = service_config.download.path {
        let download_dir = Path::new(dl_path);
        if !download_dir.exists() {
            std::fs::create_dir_all(download_dir)?;
        }
        std::env::set_current_dir(download_dir)?;
        log::info!("Download directory: {dl_path}");
    }

    app.config.config = service_config.download.clone();

    // Try to load credentials from config file first
    let mut credentials_from_config = false;
    if service_config.credentials.has_credentials() {
        if let Some((email, password, mfa)) = service_config.credentials.decrypt_if_needed() {
            log::info!("Loaded credentials from config file");
            credentials_from_config = app.login.set_credentials(email, password, mfa);

            // Encrypt plaintext credentials in-place
            if !service_config.credentials.encrypted {
                log::info!("Encrypting plaintext credentials in config file");
                service_config.credentials.encrypt_in_place();
                service_config.save(config_path)?;
            }
        } else {
            log::warn!(
                "Failed to decrypt credentials from config (machine key mismatch?). Falling back to environment variables."
            );
        }
    }

    // Fall back to environment variables if config credentials unavailable
    if !credentials_from_config {
        if let (Ok(email), Ok(password)) = (env::var("MEGA_EMAIL"), env::var("MEGA_PASSWORD")) {
            log::info!("Using credentials from MEGA_EMAIL and MEGA_PASSWORD environment variables");
            app.login
                .set_credentials(email, password, env::var("MEGA_MFA").unwrap_or_default());
        } else if service_config.credentials.has_credentials() {
            // Config had credentials but decryption failed, and no env vars provided
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Failed to decrypt credentials from config file. Set MEGA_EMAIL and MEGA_PASSWORD environment variables, or re-create the config file as the current user.",
            ));
        }
    }

    Ok((service_config.api.host, service_config.api.port))
}

fn build_runtime_app(
    download_tx: mpsc::UnboundedSender<DownloadEvent>,
    config_path: Option<&Path>,
    quit_enabled: bool,
) -> io::Result<(App, String, u16)> {
    if let Some(path) = config_path {
        let mut app = App::new(0, download_tx, quit_enabled);
        let (host, port) = apply_service_config(&mut app, path)?;
        app.api_port = port;
        Ok((app, host, port))
    } else {
        let port = env::var("OCTO_API_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(DEFAULT_API_PORT);
        let app = App::new(port, download_tx, quit_enabled);
        Ok((app, "127.0.0.1".to_string(), port))
    }
}

fn spawn_api_server(
    api_tx: mpsc::UnboundedSender<DownloadEvent>,
    host: String,
    port: u16,
    web_opts: Option<WebOptions>,
    label: &'static str,
) {
    tokio::spawn(async move {
        log::info!("Starting {label} on {host}:{port}");
        if let Err(e) = api::run_api_server(api_tx, &host, port, web_opts.as_ref(), None).await {
            log::error!("{label} error: {e}");
        }
    });
}

// ---------------------------------------------------------------------------
// Headless event loop (shared by --api and --web modes)
// ---------------------------------------------------------------------------

/// Async event loop for headless modes (`--api` and `--web`).
///
/// Processes download events and logs periodic progress summaries.
/// Runs until SIGINT or SIGTERM.
///
/// # Panics
/// Panics if SIGTERM signal handler registration fails on Unix platforms.
async fn run_headless_loop(
    app: &mut App,
    download_rx: &mut mpsc::UnboundedReceiver<DownloadEvent>,
) {
    let mut progress_interval = tokio::time::interval(Duration::from_secs(30));
    progress_interval.tick().await; // consume the immediate first tick

    let shutdown = async {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to register SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => log::info!("Received SIGINT"),
            _ = sigterm.recv() => log::info!("Received SIGTERM"),
        }
    };
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            () = &mut shutdown => break,
            event = download_rx.recv() => {
                if let Some(evt) = event {
                    handle_download_event(app, evt);
                } else {
                    log::warn!("Event channel closed");
                    break;
                }
            }
            _ = progress_interval.tick() => {
                log_progress(app);
            }
        }

        drain_download_events(app, download_rx);
        drain_token_messages(app);
    }
}

/// Event loop for `--web` mode.  The event loop is the **single owner** of
/// `App` — no `Arc<Mutex>`.  API handlers communicate through:
///
/// * `action_rx` — inbound [`UiAction`] commands (login, pause, etc.)
/// * `state_tx`  — outbound JSON snapshot pushed via [`tokio::sync::watch`]
///
/// Serialisation happens only when state is dirty **and** at least one API
/// client holds a receiver (`receiver_count() > 1`, since the
/// [`SharedAppState`] template receiver always counts as one).
#[allow(dead_code)]
async fn run_web_loop(
    app: &mut App,
    download_rx: &mut mpsc::UnboundedReceiver<DownloadEvent>,
    action_rx: &mut mpsc::UnboundedReceiver<UiAction>,
    state_tx: &tokio::sync::watch::Sender<String>,
) {
    let mut progress_interval = tokio::time::interval(Duration::from_secs(30));
    progress_interval.tick().await;

    let mut fast_tick = tokio::time::interval(Duration::from_millis(100));
    fast_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // CPU/RAM sampling every 5 seconds
    let mut resource_tick = tokio::time::interval(Duration::from_secs(5));
    resource_tick.tick().await;
    let mut sys = System::new();
    let pid = sysinfo::get_current_pid().ok();

    let mut dirty = true; // publish initial state

    let shutdown = async {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to register SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => log::info!("Received SIGINT"),
            _ = sigterm.recv() => log::info!("Received SIGTERM"),
        }
    };
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            () = &mut shutdown => break,
            event = download_rx.recv() => {
                if let Some(evt) = event {
                    handle_download_event(app, evt);
                    drain_download_events(app, download_rx);
                    drain_token_messages(app);
                    dirty = true;
                } else {
                    log::warn!("Event channel closed");
                    break;
                }
            }
            Some(action) = action_rx.recv() => {
                handle_ui_action(app, action);
                dirty = true;
            }
            _ = fast_tick.tick() => {
                drain_download_events(app, download_rx);
                app.update_speeds();
                drain_token_messages(app);
            }
            _ = resource_tick.tick() => {
                if let Some(pid) = pid {
                    use sysinfo::ProcessesToUpdate;
                    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), false);
                    if let Some(proc) = sys.process(pid) {
                        app.cpu_usage = proc.cpu_usage();
                        app.memory_rss = proc.memory();
                    }
                }
                dirty = true;
            }
            _ = progress_interval.tick() => {
                log_progress(app);
            }
        }

        // Publish state only when dirty and someone is listening.
        // receiver_count() > 1 because SharedAppState's template rx is always 1.
        if dirty && state_tx.receiver_count() > 1 {
            state_tx.send_replace(app.to_json());
            dirty = false;
        }
    }
}

/// Translates a [`UiAction`] from an API handler into mutations on `App`.
#[allow(dead_code)]
fn handle_ui_action(app: &mut App, action: UiAction) {
    match action {
        UiAction::AddUrls(urls) => {
            for url in &urls {
                let _ = app.url_tx.send(url.clone());
            }
            app.urls.extend(urls.iter().cloned());
            let _ = app.event_tx.send(DownloadEvent::UrlsReceived { urls });
        }
        UiAction::Login {
            email,
            password,
            mfa,
        } => {
            if app.login.set_credentials(email, password, mfa) {
                start_login(app);
            }
        }
        UiAction::TogglePause => {
            app.paused = !app.paused;
        }
        UiAction::DeleteFile(name) => {
            if let Some(token) = app.cancellation_tokens.remove(&name) {
                token.cancel();
            }
            app.deleted_files.insert(name.clone());
            app.files.retain(|f| f.name != name);
            app.recompute_totals();
        }
        UiAction::RetryFile(name) => {
            if let Some(f) = app.files.iter_mut().find(|f| f.name == name) {
                f.status = FileStatus::Queued;
                f.downloaded = 0;
                f.speed = 0;
                f.speed_accum = 0;
            }
            // Re-send URL so the download pipeline picks it up
            if let Some(url) = app.urls.iter().find(|u| u.contains(&name)).cloned() {
                let _ = app.url_tx.send(url);
            }
        }
        UiAction::UpdateConfig {
            chunks_per_file,
            concurrent_files,
            force_overwrite,
            cleanup_on_error,
        } => {
            if let Some(v) = chunks_per_file {
                app.config.config.chunks_per_file = v.max(1);
            }
            if let Some(v) = concurrent_files {
                app.config.config.concurrent_files = v.max(1);
            }
            if let Some(v) = force_overwrite {
                app.config.config.force_overwrite = v;
            }
            if let Some(v) = cleanup_on_error {
                app.config.config.cleanup_on_error = v;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Run the interactive terminal TUI.
///
/// If `api_host` is `Some`, the HTTP API server is started. When the inner
/// `Option` is `None`, the host from config (or default) is used. When `Some(host)`,
/// that explicit host is used.
/// When `web` is true the web UI is served alongside the API.
/// When `config_path` is provided, credentials and download settings are
/// loaded from the config file.
///
/// # Errors
/// Returns an error if terminal setup fails or TUI operations encounter I/O errors.
#[allow(clippy::too_many_lines, clippy::unused_async)]
pub async fn run(
    api_host: Option<Option<String>>,
    web: bool,
    config_path: Option<&Path>,
    quit_enabled: bool,
) -> io::Result<()> {
    // Initialize terminal with RAII guard for automatic cleanup
    let _terminal_guard = TerminalGuard::new()?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let (download_tx, mut download_rx) = mpsc::unbounded_channel::<DownloadEvent>();

    let (mut app, api_bind_host, api_port) =
        build_runtime_app(download_tx, config_path, quit_enabled)?;

    // Start the API server (if enabled)
    if let Some(explicit_host) = api_host {
        let host = explicit_host.unwrap_or(api_bind_host);
        let web_opts = web.then(|| WebOptions {
            public_host: host.clone(),
        });
        spawn_api_server(app.event_tx.clone(), host, api_port, web_opts, "API server");
    }

    resume_session(&mut app);
    load_credentials_from_env(&mut app);
    auto_login(&mut app, app::NoCredentialsFallback::ShowPopup);

    let mut tick_count: u32 = 0;
    let mut sys = System::new();
    let pid = sysinfo::get_current_pid().ok();

    loop {
        terminal.draw(|f| draw(f, &mut app))?;

        // Sample CPU/memory every 50 ticks (~5 s)
        tick_count += 1;
        if tick_count.is_multiple_of(50)
            && let Some(pid) = pid
        {
            use sysinfo::ProcessesToUpdate;
            sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), false);
            if let Some(proc) = sys.process(pid) {
                app.cpu_usage = proc.cpu_usage();
                app.memory_rss = proc.memory();
            }
        }

        // Poll for terminal events (100 ms timeout)
        if crossterm::event::poll(Duration::from_millis(100))? {
            match crossterm::event::read()? {
                Event::Key(key) => handle_input(&mut app, key),
                Event::Paste(text) => handle_paste(&mut app, &text),
                _ => {}
            }
        }

        drain_download_events(&mut app, &mut download_rx);
        app.update_speeds();
        if tick_count.is_multiple_of(50) {
            log_progress(&mut app);
        }
        drain_token_messages(&mut app);

        if app.should_quit {
            sync_session_on_shutdown(&mut app);
            break;
        }
    }

    // Show cursor before exit (terminal cleanup handled by RAII guard)
    terminal.show_cursor()?;

    Ok(())
}

/// Run the API server in headless mode (no TUI, no CLI).
///
/// Loads configuration from `config_path`, encrypts plaintext credentials
/// in-place, starts the API server, auto-logs in, and runs an event loop
/// that processes download events until SIGTERM/SIGINT.
///
/// # Errors
/// Returns an error if configuration loading fails, server startup fails, or I/O operations fail.
///
/// # Panics
/// Panics if SIGTERM signal handler registration fails on Unix platforms.
pub async fn run_api_only(config_path: &Path) -> io::Result<()> {
    let (download_tx, mut download_rx) = mpsc::unbounded_channel::<DownloadEvent>();
    let (mut app, api_host, api_port) = build_runtime_app(download_tx, Some(config_path), true)?;
    load_credentials_from_env(&mut app);

    if app.login.has_credentials() {
        // credentials loaded — good
    } else {
        log::error!(
            "No credentials configured. Edit {} and set email/password under [credentials], then restart.",
            config_path.display()
        );
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("No credentials in {}", config_path.display()),
        ));
    }

    app.api_port = api_port;
    resume_session(&mut app);
    auto_login(&mut app, app::NoCredentialsFallback::Silent);

    // Start the API server (headless — no web UI)
    spawn_api_server(app.event_tx.clone(), api_host.clone(), api_port, None, "API server");

    log::info!("Entering headless event loop");
    run_headless_loop(&mut app, &mut download_rx).await;

    sync_session_on_shutdown(&mut app);
    log::info!("Shutdown complete");
    Ok(())
}

/// Run the web TUI as the primary interface (no terminal TUI).
///
/// Starts the API + web UI server and an event loop that processes
/// download events and web UI actions until SIGTERM/SIGINT.  The user
/// logs in and manages downloads through the browser.
///
/// When `config_path` is provided, credentials and download settings
/// are loaded from the config file (same as `--api` mode).  Otherwise
/// the user logs in via the web UI.
///
/// # Errors
/// Returns an error if server startup fails or I/O operations fail.
///
/// # Panics
/// Panics if SIGTERM signal handler registration fails on Unix platforms.
pub async fn run_web(api_host: &str, config_path: Option<&Path>) -> io::Result<()> {
    let (host, port) = if let Some(path) = config_path {
        let config = ServiceConfig::load_or_create(path)?;
        (config.api.host.clone(), config.api.port)
    } else {
        let port = env::var("OCTO_API_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(DEFAULT_API_PORT);
        (api_host.to_string(), port)
    };

    let (bridge, child, mut log_reader) = terminal::spawn_tui_process(config_path, false)?;
    let log_forwarder = tokio::task::spawn_blocking(move || -> io::Result<()> {
        let stderr = std::io::stderr();
        let mut buf = [0u8; 4096];
        loop {
            match log_reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let mut handle = stderr.lock();
                    handle.write_all(&buf[..n])?;
                    let _ = handle.flush();
                }
                Err(ref err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        Ok(())
    });
    let bridge = Arc::new(bridge);

    log::debug!("Starting terminal web UI on {host}:{port}");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_handle = {
        let host = host.clone();
        let bridge = bridge.clone();
        tokio::spawn(async move {
            if let Err(e) =
                terminal_server::run_terminal_server(&host, port, bridge, shutdown_rx).await
            {
                log::error!("Terminal server error: {e}");
            }
        })
    };

    let child_handle = tokio::task::spawn_blocking(move || {
        let mut child = child;
        let status = child.wait();
        let _ = shutdown_tx.send(());
        status
    });

    match child_handle.await {
        Ok(Ok(status)) => {
            log::info!("Terminal UI exited with status {status}");
        }
        Ok(Err(e)) => {
            log::error!("Terminal UI wait failed: {e}");
        }
        Err(e) => {
            log::error!("Terminal UI join error: {e}");
        }
    }

    let _ = server_handle.await;
    let _ = log_forwarder.await;
    Ok(())
}
