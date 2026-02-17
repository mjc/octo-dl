//! octo-tui - Interactive TUI for downloading MEGA files.

pub(crate) mod ansi_input;
mod api;
mod app;
mod download;
mod draw;
mod event;
mod input;
pub mod web;

// ---------------------------------------------------------------------------
// In-memory writer for server-side ratatui rendering
// ---------------------------------------------------------------------------

/// A [`std::io::Write`] target that buffers ANSI output in memory.
///
/// Used with [`ratatui::backend::CrosstermBackend<BufWriter>`] so that
/// `Terminal::draw()` writes ANSI escape sequences into a `Vec<u8>`
/// instead of stdout.  Call [`drain()`](BufWriter::drain) after each
/// frame to extract the bytes and send them over WebSocket.
#[derive(Default)]
pub(crate) struct BufWriter {
    buf: Vec<u8>,
}

impl BufWriter {
    /// Takes all buffered bytes, leaving the internal buffer empty.
    pub fn drain(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.buf)
    }
}

impl std::io::Write for BufWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

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
use tokio::sync::mpsc;

use crate::{ServiceConfig, SessionState, SessionStatus, UrlStatus, format_bytes};
use app::FileStatus;
use sysinfo::System;

use self::api::DEFAULT_API_PORT;
use self::app::App;
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
        app.login.set_credentials(email, password, mfa.unwrap_or_default());
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
    app.login.set_credentials_if_missing(&email, &password, &mfa);
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
            log::warn!("Failed to decrypt credentials from config (machine key mismatch?). Falling back to environment variables.");
        }
    }

    // Fall back to environment variables if config credentials unavailable
    if !credentials_from_config {
        if let (Ok(email), Ok(password)) = (env::var("MEGA_EMAIL"), env::var("MEGA_PASSWORD")) {
            log::info!("Using credentials from MEGA_EMAIL and MEGA_PASSWORD environment variables");
            app.login.set_credentials(email, password, env::var("MEGA_MFA").unwrap_or_default());
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
    }
}

/// Variant of the headless event loop for shared-App mode (`--web`).
///
/// Same as [`run_headless_loop`] but locks the `Arc<Mutex<App>>` around
/// each batch of work so WebSocket handlers can interleave rendering and
/// input processing.
async fn run_headless_loop_shared(
    app: &Arc<tokio::sync::Mutex<App>>,
    download_rx: &mut mpsc::UnboundedReceiver<DownloadEvent>,
) {
    let mut progress_interval = tokio::time::interval(Duration::from_secs(30));
    progress_interval.tick().await;

    // Also tick at 100ms to keep download state fresh for WS renderers
    let mut fast_tick = tokio::time::interval(Duration::from_millis(100));
    fast_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // CPU/RAM sampling every 5 seconds
    let mut resource_tick = tokio::time::interval(Duration::from_secs(5));
    resource_tick.tick().await;
    let mut sys = System::new();
    let pid = sysinfo::get_current_pid().ok();

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
                    let mut app = app.lock().await;
                    handle_download_event(&mut app, evt);
                    drain_download_events(&mut app, download_rx);
                    drain_token_messages(&mut app);
                } else {
                    log::warn!("Event channel closed");
                    break;
                }
            }
            _ = fast_tick.tick() => {
                let mut app = app.lock().await;
                drain_download_events(&mut app, download_rx);
                app.update_speeds();
                drain_token_messages(&mut app);
            }
            _ = resource_tick.tick() => {
                if let Some(pid) = pid {
                    use sysinfo::ProcessesToUpdate;
                    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), false);
                    if let Some(proc) = sys.process(pid) {
                        let mut app = app.lock().await;
                        app.cpu_usage = proc.cpu_usage();
                        app.memory_rss = proc.memory();
                    }
                }
            }
            _ = progress_interval.tick() => {
                let mut app = app.lock().await;
                log_progress(&mut app);
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
) -> io::Result<()> {
    // Initialize terminal with RAII guard for automatic cleanup
    let _terminal_guard = TerminalGuard::new()?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let (download_tx, mut download_rx) = mpsc::unbounded_channel::<DownloadEvent>();

    // Load service config if provided (credentials, download settings, api bind)
    let api_port;
    let api_bind_host;
    let mut app;
    if let Some(path) = config_path {
        app = App::new(0, download_tx);
        let (host, port) = apply_service_config(&mut app, path)?;
        api_port = port;
        api_bind_host = host;
        app.api_port = api_port;
    } else {
        api_port = env::var("OCTO_API_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(DEFAULT_API_PORT);
        api_bind_host = "127.0.0.1".to_string();
        app = App::new(api_port, download_tx);
    }

    // Start the API server (if enabled)
    if let Some(explicit_host) = api_host {
        let host = explicit_host.unwrap_or(api_bind_host);
        let api_tx = app.event_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = api::run_api_server(api_tx, &host, api_port, web, None).await {
                log::error!("API server error: {e}");
            }
        });
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
    let mut app = App::new(0, download_tx);

    let (api_host, api_port) = apply_service_config(&mut app, config_path)?;
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
    let api_tx = app.event_tx.clone();
    let api_host_owned = api_host.clone();
    tokio::spawn(async move {
        log::info!("Starting API server on {api_host_owned}:{api_port}");
        if let Err(e) = api::run_api_server(api_tx, &api_host_owned, api_port, false, None).await {
            log::error!("API server error: {e}");
        }
    });

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
pub async fn run_web(
    api_host: &str,
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

    // Wrap the App in Arc<Mutex> for sharing with WebSocket handlers
    let shared_app = Arc::new(tokio::sync::Mutex::new(app));

    // Start the API + web server
    let api_tx = {
        let app = shared_app.lock().await;
        app.event_tx.clone()
    };
    let api_host_owned = api_host.clone();
    let shared_app_clone = shared_app.clone();
    tokio::spawn(async move {
        log::info!("Starting web TUI on {api_host_owned}:{api_port}");
        if let Err(e) =
            api::run_api_server(api_tx, &api_host_owned, api_port, true, Some(shared_app_clone))
                .await
        {
            log::error!("API server error: {e}");
        }
    });

    {
        let mut app = shared_app.lock().await;
        resume_session(&mut app);
        load_credentials_from_env(&mut app);
        auto_login(&mut app, app::NoCredentialsFallback::ShowPopup);
    }

    eprintln!("octo-dl web TUI running at http://{api_host}:{api_port}");
    log::info!("Entering web TUI event loop");

    run_headless_loop_shared(&shared_app, &mut download_rx).await;

    {
        let mut app = shared_app.lock().await;
        sync_session_on_shutdown(&mut app);
    }
    log::info!("Shutdown complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    #[test]
    fn buf_writer_drain() {
        let mut w = BufWriter::default();
        std::io::Write::write_all(&mut w, b"hello").unwrap();
        let data = w.drain();
        assert_eq!(data, b"hello");
        assert!(w.drain().is_empty(), "drain should leave buffer empty");
    }

    #[test]
    fn buf_writer_with_crossterm_backend() {
        let buf = BufWriter::default();
        let backend = CrosstermBackend::new(buf);
        let mut terminal = Terminal::with_options(
            backend,
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(Rect::new(0, 0, 80, 24)),
            },
        )
        .unwrap();

        terminal.draw(|_frame| {}).unwrap();
        let data = terminal.backend_mut().writer_mut().drain();
        // First frame should produce ANSI output (cursor movement, clear, etc.)
        assert!(!data.is_empty(), "first draw should produce ANSI output");
    }
}
