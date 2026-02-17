//! octo-tui - Interactive TUI for downloading MEGA files.

mod api;
mod app;
mod download;
mod draw;
mod event;
mod input;

use std::env;
use std::io;
use std::path::Path;
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

/// Run the interactive TUI.
///
/// If `api_host` is `Some`, the HTTP API server is started on that address.
/// If `None`, no API server is spawned.
///
/// # Errors
/// Returns an error if terminal setup fails or TUI operations encounter I/O errors.
#[allow(clippy::too_many_lines, clippy::unused_async)]
pub async fn run(api_host: Option<String>) -> io::Result<()> {
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

    // Start the web API server for bookmarklet URL injection (if enabled)
    if let Some(host) = api_host {
        let api_tx = download_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = api::run_api_server(api_tx, &host, api_port).await {
                log::error!("API server error: {e}");
            }
        });
    }

    let mut app = App::new(api_port, download_tx);

    // Check for resumable session
    if let Some(mut session) = SessionState::latest() {
        // Pre-fill from session
        if let Some((email, password, mfa)) = session.credentials.decrypt() {
            app.login.email = email;
            app.login.password = password;
            app.login.mfa = mfa.unwrap_or_default();
        }

        // Pre-fill URLs
        app.urls = session.urls.iter().map(|u| u.url.clone()).collect();
        // Reset URL statuses so they get re-sent through the download pipeline.
        // The downloader will skip files already complete on disk.
        for entry in &mut session.urls {
            if entry.status == UrlStatus::Fetched {
                entry.status = UrlStatus::Pending;
            }
        }
        app.session = Some(session);
    }

    // Auto-login if credentials are present, otherwise show login popup
    let has_credentials = !app.login.email.is_empty() && !app.login.password.is_empty();
    if has_credentials {
        app.login.logging_in = true;
        app.status = "Logging in...".to_string();
        start_login(&mut app);
    } else {
        app.popup = app::Popup::Login;
    }

    let mut tick_count: u32 = 0;
    let mut sys = System::new_all();
    let pid = sysinfo::get_current_pid().ok();

    loop {
        terminal.draw(|f| draw(f, &mut app))?;

        // Sample CPU/memory every 50 ticks (~5s) to reduce /proc scanning overhead
        tick_count += 1;
        if tick_count.is_multiple_of(50)
            && let Some(pid) = pid
        {
            use sysinfo::ProcessesToUpdate;
            sys.refresh_processes(ProcessesToUpdate::All);
            if let Some(proc) = sys.process(pid) {
                app.cpu_usage = proc.cpu_usage();
                app.memory_rss = proc.memory(); // sysinfo returns bytes
            }
        }

        // Poll for events with 100ms timeout
        if crossterm::event::poll(Duration::from_millis(100))? {
            match crossterm::event::read()? {
                Event::Key(key) => handle_input(&mut app, key),
                Event::Paste(text) => handle_paste(&mut app, &text),
                _ => {}
            }
        }

        // Drain download events (non-blocking)
        while let Ok(event) = download_rx.try_recv() {
            handle_download_event(&mut app, event);
        }

        // Compute instantaneous speeds from bytes accumulated this tick
        app.update_speeds();

        // Drain token messages (non-blocking)
        if let Some(ref mut token_rx) = app.token_rx {
            while let Ok(msg) = token_rx.try_recv() {
                app.cancellation_tokens.insert(msg.file_path, msg.token);
            }
        }

        if app.should_quit {
            // Sync session files with what the user actually sees — the
            // download pipeline may have added entries the user already
            // deleted from the visible list.
            if let Some(ref mut session) = app.session
                && session.status != SessionStatus::Completed
            {
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
                    let _ = session.mark_paused();
                }
            }
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
#[allow(clippy::too_many_lines, clippy::missing_panics_doc)]
pub async fn run_api_only(config_path: &Path) -> io::Result<()> {
    // Load service config (creates template if missing)
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

    // Check credentials are present
    if !service_config.credentials.has_credentials() {
        log::error!(
            "No credentials configured. Edit {} and set email/password under [credentials], then restart.",
            config_path.display()
        );
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("No credentials in {}", config_path.display()),
        ));
    }

    // Decrypt credentials (encrypt in-place if still plaintext)
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

    let api_host = &service_config.api.host;
    let api_port = service_config.api.port;

    let (download_tx, mut download_rx) = mpsc::unbounded_channel::<DownloadEvent>();

    // Start the API server
    let api_tx = download_tx.clone();
    let api_host_owned = api_host.clone();
    tokio::spawn(async move {
        log::info!("Starting API server on {api_host_owned}:{api_port}");
        if let Err(e) = api::run_api_server(api_tx, &api_host_owned, api_port).await {
            log::error!("API server error: {e}");
        }
    });

    // Build App with download config from service config
    let mut app = App::new(api_port, download_tx);
    app.config.config = service_config.download;

    // Check for resumable session
    if let Some(mut session) = SessionState::latest() {
        log::info!("Resuming session {}", session.id);
        app.urls = session.urls.iter().map(|u| u.url.clone()).collect();
        // Reset URL statuses so they get re-sent through the download pipeline.
        // The downloader will skip files already complete on disk.
        for entry in &mut session.urls {
            if entry.status == UrlStatus::Fetched {
                entry.status = UrlStatus::Pending;
            }
        }
        app.session = Some(session);
    }

    // Set credentials and auto-login
    app.login.email = email;
    app.login.password = password;
    app.login.mfa = mfa;
    app.login.logging_in = true;
    app.status = "Logging in...".to_string();
    start_login(&mut app);

    log::info!("Entering headless event loop");

    // Periodic progress summary (every 30s)
    let mut progress_interval = tokio::time::interval(Duration::from_secs(30));
    progress_interval.tick().await; // consume the immediate first tick

    // Shutdown future: resolves on SIGINT or SIGTERM (systemd sends SIGTERM)
    #[cfg(unix)]
    let shutdown = async {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to register SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => log::info!("Received SIGINT"),
            _ = sigterm.recv() => log::info!("Received SIGTERM"),
        }
    };
    
    #[cfg(not(unix))]
    let shutdown = async {
        tokio::signal::ctrl_c().await.ok();
        log::info!("Received SIGINT");
    };
    
    tokio::pin!(shutdown);

    // Headless event loop — process download events until signal
    loop {
        tokio::select! {
            () = &mut shutdown => {
                break;
            }
            event = download_rx.recv() => {
                if let Some(evt) = event {
                    handle_download_event(&mut app, evt);
                } else {
                    log::warn!("Event channel closed");
                    break;
                }
            }
            _ = progress_interval.tick() => {
                app.update_speeds();
                if app.files_total > 0 {
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
            }
        }

        // Drain any remaining buffered events
        while let Ok(event) = download_rx.try_recv() {
            handle_download_event(&mut app, event);
        }

        // Drain token messages
        if let Some(ref mut token_rx) = app.token_rx {
            while let Ok(msg) = token_rx.try_recv() {
                app.cancellation_tokens.insert(msg.file_path, msg.token);
            }
        }
    }

    // Sync session files with what was visible, then save
    if let Some(ref mut session) = app.session
        && session.status != SessionStatus::Completed
    {
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

    log::info!("Shutdown complete");
    Ok(())
}
