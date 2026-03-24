use std::env;
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::path::Path;
use std::sync::Arc;

use parking_lot::Mutex;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

#[cfg(unix)]
const TUI_LOG_FD: RawFd = 100;
#[cfg(unix)]
const TUI_CONTROL_FD: RawFd = 101;

/// Connection to the pseudo-terminal master that allows writing keystrokes.
#[derive(Clone)]
pub struct TerminalBridge {
    master: Arc<Box<dyn MasterPty + Send>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
}

// MasterPty is not Sync upstream, but we only use its safe handles.
unsafe impl Send for TerminalBridge {}
unsafe impl Sync for TerminalBridge {}

impl TerminalBridge {
    pub(crate) fn new(master: Box<dyn MasterPty + Send>, writer: Box<dyn Write + Send>) -> Self {
        Self {
            master: Arc::new(master),
            writer: Arc::new(Mutex::new(writer)),
        }
    }

    pub(crate) fn try_clone_reader(&self) -> io::Result<Box<dyn Read + Send>> {
        self.master
            .try_clone_reader()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }

    /// Writes raw bytes to the terminal master.
    pub fn write(&self, data: &[u8]) -> io::Result<()> {
        let mut guard = self.writer.lock();
        guard.write_all(data)?;
        guard.flush()?;
        Ok(())
    }

    /// Writes a line (text + CR) to the terminal.
    pub fn write_line(&self, text: &str) -> io::Result<()> {
        self.write(text.as_bytes())?;
        self.write(b"\r")?;
        Ok(())
    }
}

#[cfg(unix)]
fn install_inherited_fd(fd: BorrowedFd<'_>, target: RawFd) -> io::Result<()> {
    let rc = unsafe { nix::libc::dup2(fd.as_raw_fd(), target) };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }

    let flags = unsafe { nix::libc::fcntl(target, nix::libc::F_GETFD) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }

    let rc = unsafe { nix::libc::fcntl(target, nix::libc::F_SETFD, flags & !nix::libc::FD_CLOEXEC) };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

#[cfg(unix)]
fn close_inherited_fd(fd: RawFd) {
    let _ = unsafe { nix::libc::close(fd) };
}

/// Spawns the terminal UI inside a pseudo-terminal and returns a bridge to the master,
/// the child handle, and a log reader for the child process.
#[cfg(unix)]
pub fn spawn_tui_process(
    config_path: Option<&Path>,
    quit_enabled: bool,
) -> io::Result<(
    TerminalBridge,
    Box<dyn portable_pty::Child + Send + Sync>,
    os_pipe::PipeReader,
)> {
    fn map_err<E: std::fmt::Display>(err: E) -> io::Error {
        io::Error::new(io::ErrorKind::Other, err.to_string())
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

    let (log_reader, log_writer) = os_pipe::pipe()?;
    let (control_reader, mut control_writer) = os_pipe::pipe()?;
    install_inherited_fd(log_writer.as_fd(), TUI_LOG_FD)?;
    install_inherited_fd(control_reader.as_fd(), TUI_CONTROL_FD)?;

    let current_exe = env::current_exe()?;
    let mut cmd = CommandBuilder::new(current_exe);
    cmd.arg("--tui");
    if let Some(config) = config_path {
        cmd.arg("--config");
        cmd.arg(config);
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");

    let child = pair.slave.spawn_command(cmd).map_err(map_err)?;
    close_inherited_fd(TUI_LOG_FD);
    close_inherited_fd(TUI_CONTROL_FD);
    control_writer.write_all(&[u8::from(quit_enabled)])?;
    let writer = pair.master.take_writer().map_err(map_err)?;
    let bridge = TerminalBridge::new(pair.master, writer);

    Ok((bridge, child, log_reader))
}

#[cfg(not(unix))]
pub fn spawn_tui_process(
    _config_path: Option<&Path>,
    _quit_enabled: bool,
) -> io::Result<(
    TerminalBridge,
    Box<dyn portable_pty::Child + Send + Sync>,
    os_pipe::PipeReader,
)> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "web terminal spawning is only implemented on unix",
    ))
}
