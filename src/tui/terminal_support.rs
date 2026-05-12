use std::io;
use std::panic;
use std::sync::Arc;

use crossterm::event::Event;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use parking_lot::Mutex;
use tokio::sync::mpsc;

static TERMINAL_PANIC_HOOK_LOCK: Mutex<()> = Mutex::new(());

/// RAII guard that ensures terminal cleanup on drop.
/// Restores terminal to normal mode even if a panic occurs.
pub(crate) struct TerminalGuard;

pub(crate) fn restore_terminal_state() {
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(
        io::stdout(),
        crossterm::event::DisableBracketedPaste,
        LeaveAlternateScreen
    );
}

impl TerminalGuard {
    pub(crate) fn new() -> io::Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = crossterm::execute!(
            io::stdout(),
            EnterAlternateScreen,
            crossterm::event::EnableBracketedPaste
        ) {
            restore_terminal_state();
            return Err(error);
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal_state();
    }
}

pub(crate) struct TerminalPanicHookGuard {
    _lock: parking_lot::MutexGuard<'static, ()>,
    previous_hook: Arc<dyn Fn(&panic::PanicHookInfo<'_>) + Send + Sync + 'static>,
}

impl TerminalPanicHookGuard {
    pub(crate) fn install() -> Self {
        Self::install_with_cleanup(Arc::new(restore_terminal_state))
    }

    pub(crate) fn install_with_cleanup(cleanup: Arc<dyn Fn() + Send + Sync + 'static>) -> Self {
        let lock = TERMINAL_PANIC_HOOK_LOCK.lock();
        let previous_hook = Arc::new(panic::take_hook());
        let previous_for_hook = Arc::clone(&previous_hook);
        panic::set_hook(Box::new(move |info| {
            cleanup();
            previous_for_hook(info);
        }));
        Self {
            _lock: lock,
            previous_hook,
        }
    }
}

impl Drop for TerminalPanicHookGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        let previous_hook = Arc::clone(&self.previous_hook);
        panic::set_hook(Box::new(move |info| previous_hook(info)));
    }
}

pub(crate) fn terminal_input_channel() -> mpsc::UnboundedReceiver<Event> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        loop {
            match crossterm::event::read() {
                Ok(event) => {
                    if tx.send(event).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    log::error!("Terminal input error: {error}");
                    break;
                }
            }
        }
    });
    rx
}
