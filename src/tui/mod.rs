//! octo-tui - Interactive TUI for downloading MEGA files.

pub(crate) mod ansi_input;
mod api;
mod app;
mod download;
mod draw;
mod event;
mod input;

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

#[allow(dead_code)]
fn parse_resize_message(data: &[u8]) -> Option<(u16, u16)> {
    if data.first() != Some(&b'{') {
        return None;
    }

    let msg: serde_json::Value = serde_json::from_slice(data).ok()?;
    if msg.get("type").and_then(|value| value.as_str()) != Some("resize") {
        return None;
    }

    let cols = msg.get("cols").and_then(|value| value.as_u64()).unwrap_or(80) as u16;
    let rows = msg.get("rows").and_then(|value| value.as_u64()).unwrap_or(24) as u16;
    Some((cols, rows))
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
            if let Err(e) = api::run_api_server(api_tx, &host, api_port, None, None).await {
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
        if let Err(e) = api::run_api_server(api_tx, &api_host_owned, api_port, None, None).await {
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

/// Run the TUI in web mode — ratatui renders server-side into an in-memory
/// buffer and streams ANSI output over a WebSocket to xterm.js in the browser.
/// Keyboard input travels back over the same WebSocket.
///
/// # Errors
/// Returns an error if server startup or rendering fails.
///
/// # Panics
/// Panics if SIGTERM signal handler registration fails on Unix.
#[allow(clippy::too_many_lines, clippy::missing_panics_doc)]
pub async fn run_web(api_host: Option<String>) -> io::Result<()> {
    use ratatui::layout::Rect;
    use ratatui::TerminalOptions;
    use ratatui::Viewport;

    let api_port = env::var("OCTO_API_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_API_PORT);

    let host = api_host.unwrap_or_else(|| "127.0.0.1".to_string());

    let (download_tx, mut download_rx) = mpsc::unbounded_channel::<DownloadEvent>();

    // Channels for WebSocket ↔ event loop communication
    let (ws_input_tx, mut ws_input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (ws_frame_tx, ws_frame_rx) = tokio::sync::watch::channel::<Vec<u8>>(Vec::new());

    // Start the API + WebSocket server
    let api_tx = download_tx.clone();
    let api_host_owned = host.clone();
    tokio::spawn(async move {
        log::info!("Starting web UI server on {api_host_owned}:{api_port}");
        if let Err(e) =
            api::run_api_server(api_tx, &api_host_owned, api_port, Some(ws_input_tx), Some(ws_frame_rx))
                .await
        {
            log::error!("API server error: {e}");
        }
    });

    // In-memory terminal — ratatui draws here instead of stdout
    let backend = CrosstermBackend::new(BufWriter::default());
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, 80, 24)),
        },
    )?;

    let mut app = App::new(api_port, download_tx);

    // Check for resumable session
    if let Some(mut session) = SessionState::latest() {
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

    // Auto-login if credentials are present
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

    log::info!("Web UI ready at http://{host}:{api_port}/");

    // Shutdown future
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

    let mut tick_interval = tokio::time::interval(Duration::from_millis(100));

    loop {
        tokio::select! {
            () = &mut shutdown => {
                app.should_quit = true;
            }
            _ = tick_interval.tick() => {}
        }

        if app.should_quit {
            break;
        }

        // Render frame into BufWriter
        terminal.draw(|f| draw(f, &mut app))?;
        let frame_bytes = terminal.backend_mut().writer_mut().drain();
        if !frame_bytes.is_empty() {
            let _ = ws_frame_tx.send(frame_bytes);
        }

        // Sample CPU/memory periodically
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

        // Drain WebSocket keyboard input
        while let Ok(data) = ws_input_rx.try_recv() {
            // Resize messages are JSON: {"type":"resize","cols":80,"rows":24}
            if data.first() == Some(&b'{') {
                if let Ok(msg) = serde_json::from_slice::<serde_json::Value>(&data) {
                    if msg.get("type").and_then(|t| t.as_str()) == Some("resize") {
                        let cols = msg.get("cols").and_then(|v| v.as_u64()).unwrap_or(80) as u16;
                        let rows = msg.get("rows").and_then(|v| v.as_u64()).unwrap_or(24) as u16;
                        let rect = Rect::new(0, 0, cols, rows);
                        terminal.resize(rect)?;
                        log::debug!("Terminal resized to {cols}x{rows}");
                        continue;
                    }
                }
            }

            // Raw keyboard bytes → crossterm events
            for event in ansi_input::parse_xterm_input(&data) {
                match event {
                    Event::Key(key) => handle_input(&mut app, key),
                    Event::Paste(text) => handle_paste(&mut app, &text),
                    _ => {}
                }
            }
        }

        // Drain download events
        while let Ok(event) = download_rx.try_recv() {
            handle_download_event(&mut app, event);
        }

        app.update_speeds();

        // Drain token messages
        if let Some(ref mut token_rx) = app.token_rx {
            while let Ok(msg) = token_rx.try_recv() {
                app.cancellation_tokens.insert(msg.file_path, msg.token);
            }
        }
    }

    // Session cleanup (same as other modes)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // -----------------------------------------------------------------------
    // BufWriter
    // -----------------------------------------------------------------------

    #[test]
    fn bufwriter_starts_empty() {
        let w = BufWriter::default();
        assert!(w.buf.is_empty());
    }

    #[test]
    fn bufwriter_accumulates_writes() {
        let mut w = BufWriter::default();
        w.write_all(b"hello").unwrap();
        w.write_all(b" world").unwrap();
        assert_eq!(&w.buf, b"hello world");
    }

    #[test]
    fn bufwriter_drain_returns_bytes_and_clears() {
        let mut w = BufWriter::default();
        w.write_all(b"data").unwrap();
        let drained = w.drain();
        assert_eq!(&drained, b"data");
        assert!(w.buf.is_empty(), "buf should be empty after drain");
    }

    #[test]
    fn bufwriter_drain_empty_is_empty_vec() {
        let mut w = BufWriter::default();
        let drained = w.drain();
        assert!(drained.is_empty());
    }

    #[test]
    fn bufwriter_write_returns_correct_len() {
        let mut w = BufWriter::default();
        let n = w.write(b"abc").unwrap();
        assert_eq!(n, 3);
    }

    #[test]
    fn bufwriter_flush_is_noop() {
        let mut w = BufWriter::default();
        w.write_all(b"test").unwrap();
        w.flush().unwrap();
        assert_eq!(&w.buf, b"test"); // flush doesn't clear
    }

    // -----------------------------------------------------------------------
    // BufWriter + ratatui integration
    // -----------------------------------------------------------------------

    #[test]
    fn bufwriter_produces_ansi_via_ratatui() {
        use ratatui::layout::Rect;
        use ratatui::{Terminal, TerminalOptions, Viewport};

        let backend = CrosstermBackend::new(BufWriter::default());
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 80, 24)),
            },
        )
        .unwrap();

        terminal.draw(|_frame| {}).unwrap();
        let bytes = terminal.backend_mut().writer_mut().drain();
        // ratatui should have emitted *something* (cursor hide, clears, etc.)
        assert!(!bytes.is_empty(), "expected ANSI output from ratatui draw");
    }

    #[test]
    fn bufwriter_drain_between_frames_isolates_output() {
        use ratatui::layout::Rect;
        use ratatui::{Terminal, TerminalOptions, Viewport};

        let backend = CrosstermBackend::new(BufWriter::default());
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 40, 10)),
            },
        )
        .unwrap();

        // Frame 1
        terminal.draw(|_| {}).unwrap();
        let frame1 = terminal.backend_mut().writer_mut().drain();

        // Frame 2
        terminal.draw(|_| {}).unwrap();
        let frame2 = terminal.backend_mut().writer_mut().drain();

        // Both should have content, and draining between them means frame2
        // does NOT contain frame1's bytes.
        assert!(!frame1.is_empty());
        assert!(!frame2.is_empty());
        // Frame 1 includes initial setup (cursor hide, full clear) so it's
        // typically larger than a no-op redraw.
        assert!(
            frame1.len() >= frame2.len(),
            "first frame should be >= second (initial setup overhead)"
        );
    }

    // -----------------------------------------------------------------------
    // Resize message parsing (extracted logic from run_web event loop)
    // -----------------------------------------------------------------------

    #[test]
    fn resize_message_parsed_correctly() {
        let data = br#"{"type":"resize","cols":120,"rows":40}"#;
        assert_eq!(parse_resize_message(data), Some((120, 40)));
    }

    #[test]
    fn resize_message_defaults_on_missing_dims() {
        let data = br#"{"type":"resize"}"#;
        assert_eq!(parse_resize_message(data), Some((80, 24)));
    }

    #[test]
    fn resize_ignores_non_resize_json() {
        let data = br#"{"type":"ping"}"#;
        assert_eq!(parse_resize_message(data), None);
    }

    #[test]
    fn resize_ignores_non_json() {
        assert_eq!(parse_resize_message(b"hello"), None);
    }

    #[test]
    fn resize_ignores_empty() {
        assert_eq!(parse_resize_message(b""), None);
    }

    #[test]
    fn resize_ignores_raw_keyboard_bytes() {
        // Arrow key sequence should NOT be parsed as resize
        assert_eq!(parse_resize_message(b"\x1b[A"), None);
    }
}