//! octo-tui - Interactive TUI for downloading MEGA files.

mod api;
mod app;
mod download;
mod draw;
mod event;
mod input;
pub mod web;

use std::env;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::Event;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::{RwLock, broadcast, mpsc};

use crate::{ServiceConfig, SessionState, SessionStatus, UrlStatus, format_bytes};
use app::FileStatus;
use sysinfo::System;

use self::api::DEFAULT_API_PORT;
use self::app::{App, AppSnapshot, SharedAppState, UiAction};
use self::download::{handle_download_event, start_login};
use self::draw::draw;
use self::event::DownloadEvent;

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

/// Options for the web UI frontend.
#[derive(Debug, Clone)]
pub struct WebOptions {
    /// Public hostname for the PWA manifest and share target.
    pub public_host: String,
}

// ---------------------------------------------------------------------------
// Helpers — small, focused functions extracted from the three run variants
// ---------------------------------------------------------------------------

/// Loads the latest session (if any), pre-filling credentials and URLs.
///
/// Previously-fetched URLs are reset to pending so the download pipeline
/// re-evaluates them (files already on disk are skipped automatically).
fn resume_session(app: &mut App) {
    let Some(mut session) = SessionState::latest() else {
        return;
    };
    log::info!("Resuming session {}", session.id);

    if let Some((email, password, mfa)) = session.credentials.decrypt() {
        app.login.email = email;
        app.login.password = password;
        app.login.mfa = mfa.unwrap_or_default();
    }

    app.urls = session.urls.iter().map(|u| u.url.clone()).collect();
    for entry in &mut session.urls {
        if entry.status == UrlStatus::Fetched {
            entry.status = UrlStatus::Pending;
        }
    }
    app.session = Some(session);
}

/// Syncs the session file list with visible UI files before shutdown.
///
/// Removes files the user deleted, marks the session completed if nothing
/// remains, or paused otherwise.
fn sync_session_on_shutdown(app: &mut App) {
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
fn drain_download_events(
    app: &mut App,
    download_rx: &mut mpsc::UnboundedReceiver<DownloadEvent>,
) {
    while let Ok(event) = download_rx.try_recv() {
        handle_download_event(app, event);
    }
}

/// Drains all pending token messages (non-blocking).
fn drain_token_messages(app: &mut App) {
    if let Some(ref mut token_rx) = app.token_rx {
        while let Ok(msg) = token_rx.try_recv() {
            app.cancellation_tokens.insert(msg.file_path, msg.token);
        }
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

/// Initiates login if credentials are available.
///
/// When `show_popup` is true and no credentials are found, the login
/// popup is shown (used by the terminal TUI).  Returns `true` when
/// login was initiated.
fn auto_login(app: &mut App, show_popup: bool) -> bool {
    let has_credentials = !app.login.email.is_empty() && !app.login.password.is_empty();
    if has_credentials {
        app.login.logging_in = true;
        app.status = "Logging in...".to_string();
        start_login(app);
    } else if show_popup {
        app.popup = app::Popup::Login;
    }
    has_credentials
}

/// Loads and validates a `ServiceConfig` from disk, applying download
/// directory, credentials, and download settings to the `App`.
///
/// Returns the validated `(api_host, api_port)` from the config.  If
/// credentials are stored in plaintext they are encrypted in-place and
/// the file is re-saved.
fn apply_service_config(
    app: &mut App,
    config_path: &Path,
) -> io::Result<(String, u16)> {
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

    // Decrypt / encrypt credentials if present
    if service_config.credentials.has_credentials() {
        let (email, password, mfa) =
            service_config
                .credentials
                .decrypt_if_needed()
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "Failed to decrypt credentials")
                })?;

        if !service_config.credentials.encrypted {
            log::info!("Encrypting plaintext credentials in config file");
            service_config.credentials.encrypt_in_place();
            service_config.save(config_path)?;
        }

        // Config credentials override anything from the session
        app.login.email = email;
        app.login.password = password;
        app.login.mfa = mfa;
    }

    Ok((service_config.api.host, service_config.api.port))
}

// ---------------------------------------------------------------------------
// Web UI shared state bundle
// ---------------------------------------------------------------------------

/// Bundles the channels and shared state needed for web UI broadcasting.
struct WebState {
    shared: SharedAppState,
    action_rx: mpsc::UnboundedReceiver<UiAction>,
    broadcast_tx: broadcast::Sender<AppSnapshot>,
}

impl WebState {
    /// Creates a new `WebState` with fresh channels.
    fn new() -> Self {
        let (action_tx, action_rx) = mpsc::unbounded_channel::<UiAction>();
        let (broadcast_tx, _) = broadcast::channel::<AppSnapshot>(16);
        let shared = SharedAppState {
            snapshot: Arc::new(RwLock::new(AppSnapshot::default())),
            broadcast_tx: broadcast_tx.clone(),
            action_tx,
        };
        Self {
            shared,
            action_rx,
            broadcast_tx,
        }
    }

    /// Drains pending web UI actions and broadcasts the latest snapshot.
    async fn process_tick(&mut self, app: &mut App) {
        while let Ok(action) = self.action_rx.try_recv() {
            process_ui_action(app, action);
        }
        let snap = app.snapshot();
        let _ = self.broadcast_tx.send(snap.clone());
        *self.shared.snapshot.write().await = snap;
    }
}

// ---------------------------------------------------------------------------
// Headless event loop (shared by --api and --web modes)
// ---------------------------------------------------------------------------

/// Async event loop for headless modes (`--api` and `--web`).
///
/// Processes download events, optionally processes web UI actions and
/// broadcasts state, and logs periodic progress summaries.  Runs until
/// SIGINT or SIGTERM.
///
/// # Panics
/// Panics if SIGTERM signal handler registration fails on Unix platforms.
async fn run_headless_loop(
    app: &mut App,
    download_rx: &mut mpsc::UnboundedReceiver<DownloadEvent>,
    web: &mut Option<WebState>,
) {
    let mut progress_interval = tokio::time::interval(Duration::from_secs(30));
    progress_interval.tick().await; // consume the immediate first tick

    let shutdown = async {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
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

        if let Some(ws) = web {
            ws.process_tick(app).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Run the interactive terminal TUI.
///
/// If `api_host` is `Some`, the HTTP API server is started on that address.
/// If `web_opts` is `Some`, the web UI frontend is served alongside the API.
///
/// # Errors
/// Returns an error if terminal setup fails or TUI operations encounter I/O errors.
#[allow(clippy::too_many_lines, clippy::unused_async)]
pub async fn run(api_host: Option<String>, web_opts: Option<WebOptions>) -> io::Result<()> {
    // Initialize terminal with RAII guard for automatic cleanup
    let _terminal_guard = TerminalGuard::new()?;
    
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let (download_tx, mut download_rx) = mpsc::unbounded_channel::<DownloadEvent>();

    let api_port = env::var("OCTO_API_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_API_PORT);

    // Set up shared state for web UI (if enabled)
    let mut web_state = web_opts.as_ref().map(|_| WebState::new());

    // Start the API server (if enabled)
    if let Some(host) = api_host {
        let api_tx = download_tx.clone();
        let shared = web_state.as_ref().map(|w| w.shared.clone());
        let web = web_opts.clone();
        tokio::spawn(async move {
            if let Err(e) =
                api::run_api_server(api_tx, &host, api_port, web.as_ref(), shared).await
            {
                log::error!("API server error: {e}");
            }
        });
    }

    let mut app = App::new(api_port, download_tx);
    resume_session(&mut app);
    auto_login(&mut app, true);

    let mut tick_count: u32 = 0;
    let mut sys = System::new_all();
    let pid = sysinfo::get_current_pid().ok();

    loop {
        terminal.draw(|f| draw(f, &mut app))?;

        // Sample CPU/memory every 50 ticks (~5 s)
        tick_count += 1;
        if tick_count.is_multiple_of(50)
            && let Some(pid) = pid
        {
            use sysinfo::ProcessesToUpdate;
            sys.refresh_processes(ProcessesToUpdate::All);
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
        drain_token_messages(&mut app);

        if let Some(ref mut ws) = web_state {
            ws.process_tick(&mut app).await;
        }

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
    let mut app = App::new(0, download_tx);

    let (api_host, api_port) = apply_service_config(&mut app, config_path)?;

    if !app.login.email.is_empty() || !app.login.password.is_empty() {
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
    auto_login(&mut app, false);

    // Start the API server (headless — no web UI)
    let api_tx = app.event_tx.clone();
    let api_host_owned = api_host.clone();
    tokio::spawn(async move {
        log::info!("Starting API server on {api_host_owned}:{api_port}");
        if let Err(e) = api::run_api_server(api_tx, &api_host_owned, api_port, None, None).await {
            log::error!("API server error: {e}");
        }
    });

    log::info!("Entering headless event loop");
    run_headless_loop(&mut app, &mut download_rx, &mut None).await;

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
pub async fn run_web(
    api_host: &str,
    web_opts: WebOptions,
    config_path: Option<&Path>,
) -> io::Result<()> {
    let (download_tx, mut download_rx) = mpsc::unbounded_channel::<DownloadEvent>();
    let mut app = App::new(0, download_tx);

    // Load service config if provided (credentials, download settings, api bind)
    let (api_host, api_port) = if let Some(path) = config_path {
        let (host, port) = apply_service_config(&mut app, path)?;
        (host, port)
    } else {
        let port = env::var("OCTO_API_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(DEFAULT_API_PORT);
        (api_host.to_string(), port)
    };

    app.api_port = api_port;

    let web_state = WebState::new();

    // Start the API + web server
    let api_tx = app.event_tx.clone();
    let api_host_owned = api_host.clone();
    let shared = web_state.shared.clone();
    let web = web_opts.clone();
    tokio::spawn(async move {
        log::info!("Starting web TUI on {api_host_owned}:{api_port}");
        if let Err(e) =
            api::run_api_server(api_tx, &api_host_owned, api_port, Some(&web), Some(shared)).await
        {
            log::error!("API server error: {e}");
        }
    });

    resume_session(&mut app);
    auto_login(&mut app, false);

    eprintln!("octo-dl web TUI running at http://{api_host}:{api_port}");
    log::info!("Entering web TUI event loop");

    run_headless_loop(&mut app, &mut download_rx, &mut Some(web_state)).await;

    sync_session_on_shutdown(&mut app);
    log::info!("Shutdown complete");
    Ok(())
}

// ---------------------------------------------------------------------------
// Web UI action processing
// ---------------------------------------------------------------------------

/// Applies a `UiAction` from the web UI to the application state.
fn process_ui_action(app: &mut App, action: UiAction) {
    match action {
        UiAction::Login {
            email,
            password,
            mfa,
        } => {
            app.login.email = email;
            app.login.password = password;
            app.login.mfa = mfa;
            app.login.error = None;
            app.login.logging_in = true;
            app.status = "Logging in...".to_string();
            start_login(app);
        }
        UiAction::AddUrls(urls) => {
            for url in urls {
                input::add_url(app, url);
            }
        }
        UiAction::TogglePause => {
            app.paused = !app.paused;
        }
        UiAction::DeleteFile(name) => {
            if let Some(idx) = app.files.iter().position(|f| f.name == name) {
                let file = &app.files[idx];
                let can_remove = matches!(
                    file.status,
                    FileStatus::Queued | FileStatus::Error(_) | FileStatus::Downloading
                );
                if can_remove {
                    if matches!(file.status, FileStatus::Downloading) {
                        if let Some(token) = app.cancellation_tokens.remove(&name) {
                            token.cancel();
                        }
                    }
                    app.deleted_files.insert(name.clone());
                    app.files.remove(idx);
                    app.recompute_totals();
                    if let Some(ref mut session) = app.session {
                        let _ = session.remove_file(&name);
                    }
                }
            }
        }
        UiAction::RetryFile(name) => {
            if let Some(file) = app.files.iter_mut().find(|f| f.name == name) {
                if matches!(file.status, FileStatus::Error(_)) {
                    file.status = FileStatus::Queued;
                    file.downloaded = 0;
                    file.speed = 0;
                }
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
