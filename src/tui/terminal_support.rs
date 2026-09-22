use std::io;
use std::panic;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossterm::event::{DisableMouseCapture, EnableMouseCapture, Event};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use parking_lot::Mutex;
use thiserror::Error;
use tokio::sync::mpsc;

static TERMINAL_PANIC_HOOK_LOCK: Mutex<()> = Mutex::new(());
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Error)]
pub enum TerminalCleanupError {
    #[error("failed to disable raw terminal mode: {0}")]
    RawMode(#[source] io::Error),
    #[error("failed to restore the terminal screen: {0}")]
    Screen(#[source] io::Error),
    #[error(
        "failed to disable raw terminal mode ({raw_mode}) and restore the terminal screen ({screen})"
    )]
    Both {
        raw_mode: io::Error,
        screen: io::Error,
    },
}

#[derive(Debug, Error)]
pub enum TerminalSetupError {
    #[error("failed to enable raw terminal mode: {0}")]
    RawMode(#[source] io::Error),
    #[error("failed to enter the alternate screen: {0}")]
    Screen(#[source] io::Error),
    #[error(
        "failed to enter the alternate screen ({error}) and terminal cleanup also failed ({cleanup})"
    )]
    ScreenAndCleanup {
        error: io::Error,
        cleanup: TerminalCleanupError,
    },
}

#[derive(Debug, Error)]
pub enum TerminalLifecycleError {
    #[error("terminal operation failed: {0}")]
    Operation(#[source] io::Error),
    #[error("terminal restoration failed: {0}")]
    Restore(#[source] TerminalCleanupError),
    #[error("terminal operation failed ({operation}) and restoration also failed ({restore})")]
    OperationAndRestore {
        operation: io::Error,
        restore: TerminalCleanupError,
    },
}

impl From<TerminalSetupError> for io::Error {
    fn from(error: TerminalSetupError) -> Self {
        Self::other(error)
    }
}

fn finish_terminal_lifecycle_result(
    operation: io::Result<()>,
    restore: Result<(), TerminalCleanupError>,
) -> io::Result<()> {
    match (operation, restore) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(operation), Ok(())) => Err(io::Error::other(TerminalLifecycleError::Operation(
            operation,
        ))),
        (Ok(()), Err(restore)) => Err(io::Error::other(TerminalLifecycleError::Restore(restore))),
        (Err(operation), Err(restore)) => Err(io::Error::other(
            TerminalLifecycleError::OperationAndRestore { operation, restore },
        )),
    }
}

#[derive(Debug, Error)]
pub enum TerminalInputError {
    #[error("terminal input read failed: {0}")]
    Read(#[source] io::Error),
    #[error("terminal input worker closed unexpectedly")]
    Closed,
}

impl TerminalInputError {
    #[cfg(test)]
    pub(crate) fn kind(&self) -> io::ErrorKind {
        match self {
            Self::Read(error) => error.kind(),
            Self::Closed => io::ErrorKind::UnexpectedEof,
        }
    }

    pub(crate) fn into_io_error(self) -> io::Error {
        match self {
            Self::Read(error) => error,
            Self::Closed => io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "terminal input worker closed unexpectedly",
            ),
        }
    }
}

/// RAII guard that ensures terminal cleanup on drop.
/// Restores terminal to normal mode even if a panic occurs.
pub struct TerminalGuard {
    restored: bool,
}

pub fn finish_terminal_lifecycle(
    operation: io::Result<()>,
    guard: TerminalGuard,
) -> io::Result<()> {
    finish_terminal_lifecycle_result(operation, guard.restore())
}

pub fn restore_terminal_state() -> Result<(), TerminalCleanupError> {
    restore_terminal_state_with(disable_raw_mode, || {
        crossterm::execute!(
            io::stdout(),
            crossterm::event::DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        )
    })
}

fn restore_terminal_state_with<Disable, Screen>(
    disable: Disable,
    screen: Screen,
) -> Result<(), TerminalCleanupError>
where
    Disable: FnOnce() -> io::Result<()>,
    Screen: FnOnce() -> io::Result<()>,
{
    let raw_mode = disable().err();
    let screen = screen().err();
    match (raw_mode, screen) {
        (None, None) => Ok(()),
        (Some(error), None) => Err(TerminalCleanupError::RawMode(error)),
        (None, Some(error)) => Err(TerminalCleanupError::Screen(error)),
        (Some(raw_mode), Some(screen)) => Err(TerminalCleanupError::Both { raw_mode, screen }),
    }
}

impl TerminalGuard {
    pub(crate) fn new() -> Result<Self, TerminalSetupError> {
        enable_raw_mode().map_err(TerminalSetupError::RawMode)?;
        if let Err(error) = crossterm::execute!(
            io::stdout(),
            EnterAlternateScreen,
            crossterm::event::EnableBracketedPaste,
            EnableMouseCapture
        ) {
            return match restore_terminal_state() {
                Ok(()) => Err(TerminalSetupError::Screen(error)),
                Err(cleanup) => Err(TerminalSetupError::ScreenAndCleanup { error, cleanup }),
            };
        }
        Ok(Self { restored: false })
    }

    pub(crate) fn restore(mut self) -> Result<(), TerminalCleanupError> {
        self.restored = true;
        restore_terminal_state()
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if !self.restored
            && let Err(error) = restore_terminal_state()
        {
            log::error!("Failed to restore terminal state during drop: {error}");
        }
    }
}

pub struct TerminalPanicHookGuard {
    _lock: parking_lot::MutexGuard<'static, ()>,
    previous_hook: Option<Arc<PanicHook>>,
    active: Arc<AtomicBool>,
}

struct PanicHook(Box<dyn Fn(&panic::PanicHookInfo<'_>) + Send + Sync + 'static>);

impl PanicHook {
    fn call(&self, info: &panic::PanicHookInfo<'_>) {
        (self.0)(info);
    }
}

impl TerminalPanicHookGuard {
    pub(crate) fn install() -> Self {
        Self::install_with_cleanup(Arc::new(|| {
            if let Err(error) = restore_terminal_state() {
                log::error!("Failed to restore terminal state during panic: {error}");
            }
        }))
    }

    pub(crate) fn install_with_cleanup(cleanup: Arc<dyn Fn() + Send + Sync + 'static>) -> Self {
        let lock = TERMINAL_PANIC_HOOK_LOCK.lock();
        let previous_hook = Arc::new(PanicHook(panic::take_hook()));
        let previous_for_hook = Arc::clone(&previous_hook);
        let active = Arc::new(AtomicBool::new(true));
        let active_for_hook = Arc::clone(&active);
        panic::set_hook(Box::new(move |info| {
            if active_for_hook.load(Ordering::Acquire) {
                cleanup();
            }
            previous_for_hook.call(info);
        }));
        Self {
            _lock: lock,
            previous_hook: Some(previous_hook),
            active,
        }
    }
}

impl Drop for TerminalPanicHookGuard {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
        if std::thread::panicking() {
            // Rust forbids installing a panic hook while unwinding. Drop our
            // owned previous hook so a nested panic cannot retain stale hook
            // state; the process is already on an unrecoverable unwind path.
            self.previous_hook.take();
            return;
        }
        drop(panic::take_hook());
        if let Some(previous_hook) = self.previous_hook.take().and_then(Arc::into_inner) {
            panic::set_hook(previous_hook.0);
        }
    }
}

pub struct TerminalInput {
    receiver: mpsc::UnboundedReceiver<Result<Event, TerminalInputError>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl TerminalInput {
    pub(crate) async fn recv_result(&mut self) -> Result<Event, TerminalInputError> {
        match self.receiver.recv().await {
            Some(Ok(event)) => Ok(event),
            Some(Err(error)) => Err(error),
            None => Err(TerminalInputError::Closed),
        }
    }
}

impl Drop for TerminalInput {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take()
            && worker.thread().id() != thread::current().id()
            && let Err(error) = worker.join()
        {
            log::error!("Terminal input worker panicked: {error:?}");
        }
    }
}

fn spawn_terminal_input_worker<F>(stop: Arc<AtomicBool>, mut read: F) -> TerminalInput
where
    F: FnMut() -> io::Result<Option<Event>> + Send + 'static,
{
    let (tx, receiver) = mpsc::unbounded_channel();
    let worker_stop = Arc::clone(&stop);
    let worker = thread::Builder::new()
        .name("octo-tui-input".to_string())
        .spawn(move || {
            while !worker_stop.load(Ordering::Acquire) {
                match read() {
                    Ok(Some(event)) => {
                        if tx.send(Ok(event)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => thread::yield_now(),
                    Err(error) => {
                        let _ = tx.send(Err(TerminalInputError::Read(error)));
                        break;
                    }
                }
            }
        })
        .expect("terminal input worker should spawn");

    TerminalInput {
        receiver,
        stop,
        worker: Some(worker),
    }
}

pub fn terminal_input_channel() -> TerminalInput {
    let stop = Arc::new(AtomicBool::new(false));
    spawn_terminal_input_worker(stop, || {
        if crossterm::event::poll(INPUT_POLL_INTERVAL)? {
            crossterm::event::read().map(Some)
        } else {
            Ok(None)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn terminal_input_worker_reports_read_errors() {
        let stop = Arc::new(AtomicBool::new(false));
        let mut input = spawn_terminal_input_worker(Arc::clone(&stop), || {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "reader failed"))
        });

        let error = input
            .recv_result()
            .await
            .expect_err("the worker error should reach the owner");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn dropping_terminal_input_cancels_and_joins_worker() {
        let stop = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicBool::new(false));
        let exited = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker_started = Arc::clone(&started);
        let worker_exited = Arc::clone(&exited);
        let input = spawn_terminal_input_worker(stop, move || {
            worker_started.store(true, Ordering::Release);
            while !worker_stop.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            worker_exited.store(true, Ordering::Release);
            Ok(None)
        });

        while !started.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        drop(input);

        assert!(exited.load(Ordering::Acquire));
    }

    #[test]
    fn terminal_restore_reports_both_cleanup_failures() {
        let error = restore_terminal_state_with(
            || Err(io::Error::new(io::ErrorKind::PermissionDenied, "raw mode")),
            || Err(io::Error::new(io::ErrorKind::BrokenPipe, "screen")),
        )
        .expect_err("both cleanup failures should be reported");

        match error {
            TerminalCleanupError::Both { raw_mode, screen } => {
                assert_eq!(raw_mode.kind(), io::ErrorKind::PermissionDenied);
                assert_eq!(screen.kind(), io::ErrorKind::BrokenPipe);
            }
            other => panic!("expected both cleanup failures, got {other:?}"),
        }
    }

    #[test]
    fn terminal_lifecycle_surfaces_restore_failure_after_successful_operation() {
        let error = finish_terminal_lifecycle_result(
            Ok(()),
            Err(TerminalCleanupError::Screen(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "screen",
            ))),
        )
        .expect_err("restore failure should be returned");

        assert!(error.to_string().contains("terminal restoration failed"));
        assert!(error.to_string().contains("screen"));
    }
}
