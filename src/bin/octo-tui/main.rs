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
use std::time::{Duration, Instant};

use crossterm::event::Event;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

use octo_dl::{SessionState, SessionStatus};

use crate::api::DEFAULT_API_PORT;
use crate::app::App;
use crate::download::{handle_download_event, start_login};
use crate::draw::draw;
use crate::event::DownloadEvent;
use crate::input::{handle_input, handle_paste};

#[tokio::main]
async fn main() -> io::Result<()> {
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
        if let Err(e) = api::run_api_server(api_tx, api_port).await {
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
    let mut last_cpu_ticks: u64 = 0;
    let mut last_sample_time = Instant::now();

    loop {
        terminal.draw(|f| draw(f, &app))?;

        // Sample CPU/memory every 10 ticks (~1s)
        tick_count += 1;
        if tick_count.is_multiple_of(10)
            && let Some((cpu_ticks, rss_bytes)) = sample_proc_stats()
        {
            let now = Instant::now();
            let delta_wall = now.duration_since(last_sample_time).as_secs_f64();
            if delta_wall > 0.0 && last_cpu_ticks > 0 {
                let delta_ticks = cpu_ticks.saturating_sub(last_cpu_ticks);
                #[allow(clippy::cast_precision_loss)]
                let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
                #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                {
                    app.cpu_usage =
                        (delta_ticks as f64 / (delta_wall * clk_tck) * 100.0) as f32;
                }
            }
            last_cpu_ticks = cpu_ticks;
            last_sample_time = now;
            app.memory_rss = rss_bytes;
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

/// Reads `/proc/self/stat` and `/proc/self/statm` to get CPU ticks and RSS bytes.
fn sample_proc_stats() -> Option<(u64, u64)> {
    // CPU ticks: fields 14 (utime) + 15 (stime) in /proc/self/stat (1-indexed)
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let fields: Vec<&str> = stat.split_whitespace().collect();
    let utime: u64 = fields.get(13)?.parse().ok()?;
    let stime: u64 = fields.get(14)?.parse().ok()?;
    let cpu_ticks = utime + stime;

    // RSS: field 2 in /proc/self/statm (0-indexed) is RSS in pages
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let statm_fields: Vec<&str> = statm.split_whitespace().collect();
    let rss_pages: u64 = statm_fields.get(1)?.parse().ok()?;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    #[allow(clippy::cast_sign_loss)]
    let rss_bytes = rss_pages * (page_size as u64);

    Some((cpu_ticks, rss_bytes))
}
