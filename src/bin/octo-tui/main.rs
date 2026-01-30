//! octo-tui - Interactive TUI for downloading MEGA files.

#![warn(clippy::pedantic)]
#![warn(clippy::nursery)]

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

use octo_dl::{SessionState, SessionStatus};
use sysinfo::System;

use crate::api::DEFAULT_API_PORT;
use crate::app::App;
use crate::download::{handle_download_event, start_login};
use crate::draw::draw;
use crate::event::DownloadEvent;
use crate::input::{handle_input, handle_paste};

#[tokio::main]
async fn main() -> io::Result<()> {
    // Parse command-line arguments
    let mut api_host = "127.0.0.1".to_string();
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--api-host" => {
                if let Some(host) = args.next() {
                    api_host = host;
                } else {
                    eprintln!("Error: --api-host requires a value");
                    std::process::exit(1);
                }
            }
            "--help" => {
                println!("octo-tui - Interactive TUI for downloading MEGA files");
                println!("\nOptions:");
                println!("  --api-host <HOST>  API server host (default: 127.0.0.1)");
                println!("  --help              Show this help");
                std::process::exit(0);
            }
            _ => {
                eprintln!("Error: unknown option '{}'", arg);
                eprintln!("Use --help for usage information");
                std::process::exit(1);
            }
        }
    }

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

    // Start the web API server for bookmarklet URL injection
    let api_tx = download_tx.clone();
    let api_port = env::var("OCTO_API_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_API_PORT);
    tokio::spawn(async move {
        if let Err(e) = api::run_api_server(api_tx, &api_host, api_port).await {
            log::error!("API server error: {e}");
        }
    });

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
