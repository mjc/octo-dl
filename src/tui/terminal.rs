use std::env;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Arc;

use parking_lot::Mutex;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

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
}

/// Spawns the terminal UI inside a pseudo-terminal and returns a bridge to the master plus the child handle.
pub fn spawn_tui_process(
    config_path: Option<&Path>,
    log_addr: Option<String>,
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
