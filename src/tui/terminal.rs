use std::io;
use std::panic;
use std::time::Duration;

use crossterm::event::Event;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use sysinfo::System;
use tokio::sync::{mpsc, watch};

use super::app::{App, UiAction};
use super::draw::draw;
use super::event::DownloadEvent;
use super::input::{handle_input, handle_paste};
use super::terminal_support::{TerminalGuard, TerminalPanicHookGuard, terminal_input_channel};

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
    state_sync_enabled: bool,
) -> io::Result<()> {
    let panic_hook_guard = TerminalPanicHookGuard::install();
    let result =
        run_interactive_tui_loop(app, download_rx, action_rx, state_tx, state_sync_enabled).await;
    drop(panic_hook_guard);
    result
}

async fn run_interactive_tui_loop(
    app: &mut App,
    download_rx: &mut mpsc::UnboundedReceiver<DownloadEvent>,
    action_rx: &mut mpsc::UnboundedReceiver<UiAction>,
    state_tx: &watch::Sender<String>,
    state_sync_enabled: bool,
) -> io::Result<()> {
    let _terminal_guard = TerminalGuard::new()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut tick_count: u32 = 0;
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.tick().await;
    let mut input_rx = terminal_input_channel();
    let mut sys = System::new();
    let pid = sysinfo::get_current_pid().ok();
    let mut needs_draw = true;
    let mut download_state_dirty = false;

    loop {
        if needs_draw {
            terminal.draw(|f| draw(f, app))?;
            needs_draw = false;
        }

        tokio::select! {
            Some(event) = input_rx.recv() => {
                match event {
                    Event::Key(key) => handle_input(app, key),
                    Event::Paste(text) => handle_paste(app, &text),
                    _ => {}
                }
                needs_draw = true;
            }
            Some(event) = download_rx.recv() => {
                app.handle_download_event(event);
                let _ = app.drain_download_events(download_rx);
                download_state_dirty = true;
            }
            Some(action) = action_rx.recv() => {
                app.handle_ui_action(action);
                let _ = app.drain_ui_actions(action_rx);
                needs_draw = true;
            }
            _ = tick.tick() => {
                tick_count = tick_count.saturating_add(1);
                app.handle_terminal_tick(download_rx, action_rx, tick_count, &mut sys, pid);
                needs_draw = true;
                download_state_dirty = false;
            }
        }

        if state_sync_enabled {
            let _ = app.publish_snapshot_if_observed(state_tx);
        }

        if download_state_dirty {
            continue;
        }

        if app.should_quit {
            app.sync_session_for_shutdown();
            break;
        }
    }

    terminal.show_cursor()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::terminal_support::TerminalPanicHookGuard;
    use parking_lot::Mutex;
    use std::sync::Arc;

    static TEST_PANIC_HOOK_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn terminal_panic_hook_runs_cleanup_before_previous_hook() {
        let _lock = TEST_PANIC_HOOK_LOCK.lock();
        let original_hook = panic::take_hook();
        let events = Arc::new(Mutex::new(Vec::new()));
        let cleanup_events = Arc::clone(&events);
        let previous_events = Arc::clone(&events);

        panic::set_hook(Box::new(move |_| {
            previous_events.lock().push("previous");
        }));

        let hook_guard = TerminalPanicHookGuard::install_with_cleanup(Arc::new(move || {
            cleanup_events.lock().push("cleanup");
        }));

        let _ = panic::catch_unwind(|| panic!("boom"));

        drop(hook_guard);
        panic::set_hook(original_hook);

        let events = events.lock();
        assert_eq!(events.as_slice(), ["cleanup", "previous"]);
    }

    #[test]
    fn terminal_panic_hook_restores_previous_hook_after_drop() {
        let _lock = TEST_PANIC_HOOK_LOCK.lock();
        let original_hook = panic::take_hook();
        let events = Arc::new(Mutex::new(Vec::new()));
        let cleanup_events = Arc::clone(&events);
        let previous_events = Arc::clone(&events);

        panic::set_hook(Box::new(move |_| {
            previous_events.lock().push("previous");
        }));

        let hook_guard = TerminalPanicHookGuard::install_with_cleanup(Arc::new(move || {
            cleanup_events.lock().push("cleanup");
        }));
        drop(hook_guard);

        let _ = panic::catch_unwind(|| panic!("boom"));

        panic::set_hook(original_hook);

        let events = events.lock();
        assert_eq!(events.as_slice(), ["previous"]);
    }
}
