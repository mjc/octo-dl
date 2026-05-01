use std::env;
use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::Event;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use parking_lot::Mutex;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use sysinfo::System;
use tokio::sync::{mpsc, watch};

use crate::ServiceConfig;

use super::app::{App, UiAction};
use super::draw::draw;
use super::event::DownloadEvent;
use super::input::{handle_input, handle_paste};
use super::terminal_server;

/// Connection to the pseudo-terminal master that allows writing keystrokes.
#[derive(Clone)]
pub struct TerminalBridge {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl TerminalBridge {
    pub(crate) fn new(master: Box<dyn MasterPty + Send>, writer: Box<dyn Write + Send>) -> Self {
        Self {
            master: Arc::new(Mutex::new(master)),
            writer: Arc::new(Mutex::new(writer)),
        }
    }

    pub(crate) fn try_clone_reader(&self) -> io::Result<Box<dyn Read + Send>> {
        self.master
            .lock()
            .try_clone_reader()
            .map_err(io::Error::other)
    }

    /// Writes raw bytes to the terminal master.
    pub fn write(&self, data: &[u8]) -> io::Result<()> {
        let mut guard = self.writer.lock();
        guard.write_all(data)?;
        guard.flush()?;
        drop(guard);
        Ok(())
    }

    /// Writes a line (text + CR) to the terminal.
    pub fn write_line(&self, text: &str) -> io::Result<()> {
        self.write(text.as_bytes())?;
        self.write(b"\r")?;
        Ok(())
    }

    pub(crate) fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        self.master
            .lock()
            .resize(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io::Error::other)
    }
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

/// Spawns the terminal UI inside a pseudo-terminal and returns a bridge to the master plus the child handle.
pub fn spawn_tui_process(
    config_path: Option<&Path>,
    log_addr: Option<String>,
    api_port: Option<u16>,
) -> io::Result<(TerminalBridge, Box<dyn portable_pty::Child + Send + Sync>)> {
    fn map_err<E: std::fmt::Display>(err: E) -> io::Error {
        io::Error::other(err.to_string())
    }

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(map_err)?;

    let current_exe = env::current_exe()?;
    let mut cmd = CommandBuilder::new(current_exe);
    cmd.arg("--tui");
    if let Some(port) = api_port {
        cmd.arg("--api");
        cmd.arg("--web");
        cmd.arg("--host");
        cmd.arg("127.0.0.1");
        cmd.env("OCTO_API_PORT", port.to_string());
    }
    if let Some(config) = config_path {
        cmd.arg("--config");
        cmd.arg(config);
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    if let Some(addr) = log_addr {
        cmd.env("OCTO_TUI_LOG_ADDR", addr);
    }

    let child = pair.slave.spawn_command(cmd).map_err(map_err)?;
    let writer = pair.master.take_writer().map_err(map_err)?;
    let bridge = TerminalBridge::new(pair.master, writer);

    Ok((bridge, child))
}

pub async fn run_terminal_web_bridge(
    host: &str,
    port: u16,
    config_path: Option<&Path>,
    api_port: Option<u16>,
) -> io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let log_addr = listener.local_addr()?;
    let log_addr_string = log_addr.to_string();
    let log_forwarder = tokio::task::spawn_blocking(move || -> io::Result<()> {
        let (mut stream, _) = listener.accept()?;
        let stderr = std::io::stderr();
        let mut buf = [0u8; 4096];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let mut handle = stderr.lock();
                    handle.write_all(&buf[..n])?;
                    let _ = handle.flush();
                }
                Err(ref err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        Ok(())
    });

    let (bridge, child) = spawn_tui_process(config_path, Some(log_addr_string), api_port)?;
    let bridge = Arc::new(bridge);

    log::debug!("Starting terminal web UI on {host}:{port}");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_handle = {
        let host = host.to_string();
        let bridge = bridge.clone();
        tokio::spawn(async move {
            if let Err(e) =
                terminal_server::run_terminal_server(&host, port, bridge, shutdown_rx, api_port)
                    .await
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

pub async fn run_terminal_web_mode(
    api_host: &str,
    config_path: Option<&Path>,
    default_api_port: u16,
) -> io::Result<()> {
    let (host, port, child_config, api_port) = if let Some(path) = config_path {
        let mut config = ServiceConfig::load_or_create(path)?;
        let host = config.api.host.clone();
        let port = config.api.port;
        let api_port = local_free_port()?;
        config.api.host = "127.0.0.1".to_string();
        config.api.port = api_port;
        let child_config = std::env::temp_dir().join(format!(
            "octo-dl-terminal-web-api-{}-{api_port}.toml",
            std::process::id()
        ));
        config.save(&child_config)?;
        (host, port, Some(child_config), Some(api_port))
    } else {
        let port = env::var("OCTO_API_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(default_api_port);
        let api_port = local_free_port()?;
        (api_host.to_string(), port, None, Some(api_port))
    };

    let result = run_terminal_web_bridge(&host, port, child_config.as_deref(), api_port).await;
    if let Some(path) = child_config {
        let _ = std::fs::remove_file(path);
    }
    result
}

fn local_free_port() -> io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

pub async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        let sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
        match sigterm {
            Ok(mut sigterm) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => log::info!("Received SIGINT"),
                    _ = sigterm.recv() => log::info!("Received SIGTERM"),
                }
            }
            Err(e) => {
                log::warn!("Failed to register SIGTERM handler: {e}");
                let _ = tokio::signal::ctrl_c().await;
                log::info!("Received SIGINT");
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        log::info!("Received Ctrl-C");
    }
}

pub async fn run_interactive_tui(
    app: &mut App,
    download_rx: &mut mpsc::UnboundedReceiver<DownloadEvent>,
    action_rx: &mut mpsc::UnboundedReceiver<UiAction>,
    state_tx: &watch::Sender<String>,
    web: bool,
) -> io::Result<()> {
    let _terminal_guard = TerminalGuard::new()?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut tick_count: u32 = 0;
    let mut sys = System::new();
    let pid = sysinfo::get_current_pid().ok();

    loop {
        terminal.draw(|f| draw(f, app))?;

        tick_count += 1;

        if crossterm::event::poll(Duration::from_millis(100))? {
            match crossterm::event::read()? {
                Event::Key(key) => handle_input(app, key),
                Event::Paste(text) => handle_paste(app, &text),
                _ => {}
            }
        }

        app.handle_terminal_tick(download_rx, action_rx, tick_count, &mut sys, pid);
        if web {
            let _ = app.publish_snapshot_if_observed(state_tx);
        }

        if app.should_quit {
            app.sync_session_for_shutdown();
            break;
        }
    }

    terminal.show_cursor()?;
    Ok(())
}
