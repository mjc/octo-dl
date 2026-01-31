//! octo-tui - Interactive TUI for downloading MEGA files.

mod api;
mod app;
mod download;
mod draw;
mod event;
mod input;

use std::env;
use std::io;
use std::time::Duration;

use crossterm::event::Event;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

use crate::{SessionState, SessionStatus};
use sysinfo::System;

use self::api::DEFAULT_API_PORT;
use self::app::App;
use self::download::{handle_download_event, start_login};
use self::draw::draw;
use self::event::DownloadEvent;
use self::input::{handle_input, handle_paste};

/// Run the interactive TUI.
///
/// If `api_host` is `Some`, the HTTP API server is started on that address.
/// If `None`, no API server is spawned.
pub async fn run(api_host: Option<String>) -> io::Result<()> {
    // Initialize terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(
        stdout,
        EnterAlternateScreen,
        crossterm::event::EnableBracketedPaste
    )?;
    let backend = CrosstermBackend::new(stdout);
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
    if let Some(session) = SessionState::latest() {
        // Pre-fill from session
        if let Some((email, password, mfa)) = session.credentials.decrypt() {
            app.login.email = email;
            app.login.password = password;
            app.login.mfa = mfa.unwrap_or_default();
        }

        // Pre-fill URLs
        app.urls = session.urls.iter().map(|u| u.url.clone()).collect();
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
        terminal.draw(|f| draw(f, &app))?;

        // Sample CPU/memory every 50 ticks (~5s) to reduce /proc scanning overhead
        tick_count += 1;
        if tick_count.is_multiple_of(50) {
            if let Some(pid) = pid {
                use sysinfo::ProcessesToUpdate;
                sys.refresh_processes(ProcessesToUpdate::All);
                if let Some(proc) = sys.process(pid) {
                    app.cpu_usage = proc.cpu_usage();
                    app.memory_rss = proc.memory(); // sysinfo returns bytes
                }
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
            // Save session state on quit
            if let Some(ref mut session) = app.session
                && session.status != SessionStatus::Completed
            {
                if session.files.is_empty() {
                    let _ = session.mark_completed();
                } else {
                    let _ = session.mark_paused();
                }
            }
            break;
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::event::DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;

    Ok(())
}

/// Run the API server in headless mode (no TUI, no CLI).
///
/// Blocks until the server shuts down.
pub async fn run_api_only(api_host: String) -> io::Result<()> {
    let api_port = env::var("OCTO_API_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_API_PORT);

    let (tx, _rx) = mpsc::unbounded_channel::<DownloadEvent>();

    eprintln!("Starting API server on {api_host}:{api_port}");
    api::run_api_server(tx, &api_host, api_port)
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
}
