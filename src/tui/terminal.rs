use std::io;
use std::panic;
use std::time::Duration;

use crossterm::event::Event;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use sysinfo::System;
use tokio::sync::{mpsc, watch};

use super::app::{App, NoCredentialsFallback, UiAction};
use super::draw::draw;
use super::event::DownloadEvent;
use super::input::{handle_input, handle_paste, request_quit};
use super::terminal_support::{TerminalGuard, TerminalPanicHookGuard, terminal_input_channel};

fn should_draw_after_tick(app: &App, dashboard_dirty: bool) -> bool {
    dashboard_dirty || app.has_active_dashboard_transfer()
}

fn finish_interactive_shutdown_if_ready(app: &mut App, shutting_down: &mut bool) -> bool {
    if app.should_quit && !*shutting_down {
        *shutting_down = true;
        app.drain_token_messages();
        app.skip_all_shutdown_verifications();
        if !app.begin_shutdown() {
            app.sync_session_for_shutdown();
            return true;
        }
    }

    if *shutting_down {
        app.drain_token_messages();
    }

    if *shutting_down && app.shutdown_pending_files.is_empty() {
        app.sync_session_for_shutdown();
        return true;
    }

    false
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
    state_tx: &watch::Sender<bytes::Bytes>,
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
    state_tx: &watch::Sender<bytes::Bytes>,
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
    let shutdown = wait_for_shutdown_signal();
    tokio::pin!(shutdown);
    let mut sys = System::new();
    let pid = sysinfo::get_current_pid().ok();
    let mut needs_draw = true;
    let mut download_state_dirty = false;
    let mut dashboard_dirty = state_sync_enabled;
    let mut auto_login_after_first_draw = true;
    let mut shutting_down = false;

    loop {
        let mut publish_dashboard_now = false;
        if needs_draw {
            terminal.draw(|f| draw(f, app))?;
            needs_draw = false;
            if auto_login_after_first_draw {
                auto_login_after_first_draw = false;
                app.schedule_auto_login(NoCredentialsFallback::ShowPopup);
            }
        }

        tokio::select! {
            () = &mut shutdown => {
                request_quit(app);
                needs_draw = true;
                dashboard_dirty = true;
                publish_dashboard_now = true;
            }
            Some(event) = input_rx.recv() => {
                app.note_user_activity();
                match event {
                    Event::Key(key) => handle_input(app, key),
                    Event::Paste(text) => handle_paste(app, &text),
                    _ => {}
                }
                needs_draw = true;
                dashboard_dirty = true;
                publish_dashboard_now = true;
            }
            Some(event) = download_rx.recv() => {
                app.handle_download_event(event);
                let _ = app.drain_download_events(download_rx);
                download_state_dirty = true;
                dashboard_dirty = true;
            }
            Some(action) = action_rx.recv() => {
                app.note_user_activity();
                app.handle_ui_action(action);
                let _ = app.drain_ui_actions(action_rx);
                needs_draw = true;
                dashboard_dirty = true;
                publish_dashboard_now = true;
            }
            _ = tick.tick() => {
                tick_count = tick_count.saturating_add(1);
                let publish_active_transfer_ticks = state_tx.receiver_count() > 1;
                dashboard_dirty |= app.handle_terminal_tick(
                    download_rx,
                    action_rx,
                    tick_count,
                    &mut sys,
                    pid,
                    publish_active_transfer_ticks,
                );
                needs_draw |= should_draw_after_tick(app, dashboard_dirty);
                download_state_dirty = false;
                publish_dashboard_now |= dashboard_dirty;
            }
        }

        if state_sync_enabled && dashboard_dirty && publish_dashboard_now {
            app.mark_dashboard_dirty();
            let _ = app.publish_snapshot_if_observed(state_tx);
            dashboard_dirty = false;
        }

        if finish_interactive_shutdown_if_ready(app, &mut shutting_down) {
            break;
        }

        if download_state_dirty {
            continue;
        }
    }

    terminal.show_cursor()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{CoreEvent, FileId, FileLifecycle, PackageKey, ResolvedFile, ResolvedPackage};
    use crate::tui::app::{FileEntry, FileStatus, VerificationTarget};
    use crate::tui::terminal_support::TerminalPanicHookGuard;
    use parking_lot::Mutex;
    use std::sync::Arc;
    use tempfile::tempdir;

    use crate::test_support::{StateDirectoryGuard, package_id};

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

    #[test]
    fn tick_draw_stays_idle_without_dirty_state_or_active_transfer() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let app = App::new(9723, tx, true);
        assert!(!should_draw_after_tick(&app, false));
    }

    #[test]
    fn tick_draw_continues_for_active_transfer_without_dashboard_publish() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, tx, true);
        app.files.push(FileEntry {
            id: "file.bin".into(),
            name: "file.bin".to_string(),
            size: 100,
            downloaded: 1,
            status: FileStatus::Downloading,
        });
        assert!(should_draw_after_tick(&app, false));
    }

    #[test]
    fn interactive_quit_waits_for_cancellation_before_syncing_session() {
        let dir = tempdir().expect("temp dir should exist");
        let _guard = StateDirectoryGuard::set(dir.path());
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, event_tx, true);
        let file_id = FileId::from("episode.bin");

        app.ensure_session_for_pending_urls();
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id("pkg", "https://mega.nz/folder/root"),
                source_url: "https://mega.nz/folder/root".to_string(),
                key: PackageKey::new("https://mega.nz/folder/root".to_string()),
                display_name: "Package".to_string(),
                files: vec![ResolvedFile {
                    file_id: file_id.clone(),
                    path: "episode.bin".to_string(),
                    size: 128,
                }],
                collision: None,
            },
        });
        app.apply_core_event(CoreEvent::FileStarted {
            file_id: file_id.clone(),
            size: 128,
        });
        app.handle_file_progress_event(
            file_id.clone(),
            crate::core::ProgressDelta {
                total_bytes_delta: 64,
                network_bytes_delta: 64,
            },
            0,
        );
        app.flush_session_persistence();

        let latest_before_quit =
            crate::core::SessionSnapshot::latest().expect("session should exist before quit");
        assert_eq!(
            latest_before_quit
                .find_file("episode.bin")
                .expect("file should exist before quit")
                .progress
                .visible_completed_bytes,
            0
        );

        let token = tokio_util::sync::CancellationToken::new();
        app.cancellation_tokens.insert(file_id.clone(), token.clone());
        app.should_quit = true;
        let mut shutting_down = false;

        assert!(!finish_interactive_shutdown_if_ready(
            &mut app,
            &mut shutting_down
        ));
        assert!(shutting_down);
        assert!(app.paused);
        assert!(token.is_cancelled());

        let latest_during_shutdown =
            crate::core::SessionSnapshot::latest().expect("session should still exist");
        assert_eq!(
            latest_during_shutdown
                .find_file("episode.bin")
                .expect("file should still exist during shutdown")
                .progress
                .visible_completed_bytes,
            0
        );

        app.handle_file_cancelled_event(file_id.clone(), 0);
        assert!(finish_interactive_shutdown_if_ready(
            &mut app,
            &mut shutting_down
        ));

        let latest = crate::core::SessionSnapshot::latest().expect("session should be saved");
        let file = latest
            .find_file("episode.bin")
            .expect("file should exist after shutdown");
        assert_eq!(file.progress.visible_completed_bytes, 64);
        assert_eq!(file.progress.downloaded_network_bytes, 64);
        assert_eq!(file.lifecycle, FileLifecycle::Queued);
    }

    #[test]
    fn interactive_quit_waits_for_late_download_token_registration_before_syncing_session() {
        let dir = tempdir().expect("temp dir should exist");
        let _guard = StateDirectoryGuard::set(dir.path());
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, event_tx, true);
        let file_id = FileId::from("episode.bin");

        app.ensure_session_for_pending_urls();
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id("pkg", "https://mega.nz/folder/root"),
                source_url: "https://mega.nz/folder/root".to_string(),
                key: PackageKey::new("https://mega.nz/folder/root".to_string()),
                display_name: "Package".to_string(),
                files: vec![ResolvedFile {
                    file_id: file_id.clone(),
                    path: "episode.bin".to_string(),
                    size: 128,
                }],
                collision: None,
            },
        });
        app.apply_core_event(CoreEvent::FileStarted {
            file_id: file_id.clone(),
            size: 128,
        });
        app.handle_file_progress_event(
            file_id.clone(),
            crate::core::ProgressDelta {
                total_bytes_delta: 64,
                network_bytes_delta: 64,
            },
            0,
        );
        app.flush_session_persistence();

        let latest_before_quit =
            crate::core::SessionSnapshot::latest().expect("session should exist before quit");
        assert_eq!(
            latest_before_quit
                .find_file("episode.bin")
                .expect("file should exist before quit")
                .progress
                .visible_completed_bytes,
            0
        );

        app.should_quit = true;
        let mut shutting_down = false;

        assert!(!finish_interactive_shutdown_if_ready(
            &mut app,
            &mut shutting_down
        ));
        assert!(shutting_down);
        assert!(app.paused);
        assert!(app.cancellation_tokens.is_empty());

        let latest_during_shutdown =
            crate::core::SessionSnapshot::latest().expect("session should still exist");
        assert_eq!(
            latest_during_shutdown
                .find_file("episode.bin")
                .expect("file should still exist during shutdown")
                .progress
                .visible_completed_bytes,
            0
        );
    }

    #[test]
    fn interactive_quit_skips_late_resume_validation_without_waiting() {
        let dir = tempdir().expect("temp dir should exist");
        let _guard = StateDirectoryGuard::set(dir.path());
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, event_tx, true);
        let file_id = FileId::from("episode.bin");

        app.ensure_session_for_pending_urls();
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id("pkg", "https://mega.nz/folder/root"),
                source_url: "https://mega.nz/folder/root".to_string(),
                key: PackageKey::new("https://mega.nz/folder/root".to_string()),
                display_name: "Package".to_string(),
                files: vec![ResolvedFile {
                    file_id: file_id.clone(),
                    path: "episode.bin".to_string(),
                    size: 128,
                }],
                collision: None,
            },
        });
        app.handle_resume_validation_started_event(file_id.clone(), 0);
        app.handle_verification_progress_event(file_id.clone(), 64);
        app.flush_session_persistence();

        let latest_before_quit =
            crate::core::SessionSnapshot::latest().expect("session should exist before quit");
        assert_eq!(
            latest_before_quit
                .find_file("episode.bin")
                .expect("file should exist before quit")
                .progress
                .visible_completed_bytes,
            0
        );

        app.should_quit = true;
        let mut shutting_down = false;

        assert!(finish_interactive_shutdown_if_ready(
            &mut app,
            &mut shutting_down
        ));
        assert!(shutting_down);
        assert!(app.shutdown_pending_files.is_empty());
        assert!(!app.verifying_files.contains(&file_id));
        assert!(!app.verification_inflight_files.contains(&file_id));
        assert!(!app.shutdown_blocking_verifications.contains(&file_id));

        let latest_during_shutdown =
            crate::core::SessionSnapshot::latest().expect("session should be saved");
        assert_eq!(
            latest_during_shutdown
                .find_file("episode.bin")
                .expect("file should still exist after shutdown")
                .progress
                .visible_completed_bytes,
            64
        );
    }

    #[test]
    fn interactive_quit_after_pause_does_not_wait_on_stale_tokens() {
        let dir = tempdir().expect("temp dir should exist");
        let _guard = StateDirectoryGuard::set(dir.path());
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, event_tx, true);
        let file_id = FileId::from("episode.bin");

        app.ensure_session_for_pending_urls();
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id("pkg", "https://mega.nz/folder/root"),
                source_url: "https://mega.nz/folder/root".to_string(),
                key: PackageKey::new("https://mega.nz/folder/root".to_string()),
                display_name: "Package".to_string(),
                files: vec![ResolvedFile {
                    file_id: file_id.clone(),
                    path: "episode.bin".to_string(),
                    size: 128,
                }],
                collision: None,
            },
        });
        app.apply_core_event(CoreEvent::FileStarted {
            file_id: file_id.clone(),
            size: 128,
        });
        let token = tokio_util::sync::CancellationToken::new();
        app.cancellation_tokens.insert(file_id.clone(), token.clone());

        app.pause_downloads();
        assert!(token.is_cancelled());
        assert!(app.cancellation_tokens.is_empty());

        app.should_quit = true;
        let mut shutting_down = false;

        assert!(finish_interactive_shutdown_if_ready(
            &mut app,
            &mut shutting_down
        ));
        assert!(app.shutdown_pending_files.is_empty());
    }

    #[test]
    fn interactive_quit_skips_manual_reverify_without_blocking_shutdown() {
        let dir = tempdir().expect("temp dir should exist");
        let _guard = StateDirectoryGuard::set(dir.path());
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, event_tx, true);
        let file_id = FileId::from("episode.bin");

        app.ensure_session_for_pending_urls();
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id("pkg", "https://mega.nz/folder/root"),
                source_url: "https://mega.nz/folder/root".to_string(),
                key: PackageKey::new("https://mega.nz/folder/root".to_string()),
                display_name: "Package".to_string(),
                files: vec![ResolvedFile {
                    file_id: file_id.clone(),
                    path: "episode.bin".to_string(),
                    size: 128,
                }],
                collision: None,
            },
        });
        app.verifying_files.insert(file_id.clone());
        app.verification_inflight_files.insert(file_id.clone());
        app.verification_targets
            .insert(file_id.clone(), VerificationTarget::Resume);
        app.apply_core_event(CoreEvent::FileVerificationStarted {
            file_id: file_id.clone(),
        });
        app.handle_verification_progress_event(file_id.clone(), 64);
        app.flush_session_persistence();

        app.should_quit = true;
        let mut shutting_down = false;

        assert!(finish_interactive_shutdown_if_ready(
            &mut app,
            &mut shutting_down
        ));
        assert!(shutting_down);
        assert!(app.shutdown_pending_files.is_empty());
        assert!(!app.verifying_files.contains(&file_id));
        assert!(!app.verification_inflight_files.contains(&file_id));

        let latest = crate::core::SessionSnapshot::latest().expect("session should be saved");
        let file = latest
            .find_file("episode.bin")
            .expect("file should exist after shutdown");
        assert_eq!(file.progress.visible_completed_bytes, 64);
        assert_eq!(file.progress.verified_existing_bytes, 0);
        assert_eq!(file.lifecycle, FileLifecycle::Queued);
    }

    #[test]
    fn interactive_quit_skips_resume_validation_without_blocking_shutdown() {
        let dir = tempdir().expect("temp dir should exist");
        let _guard = StateDirectoryGuard::set(dir.path());
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, event_tx, true);
        let file_id = FileId::from("episode.bin");

        app.ensure_session_for_pending_urls();
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id("pkg", "https://mega.nz/folder/root"),
                source_url: "https://mega.nz/folder/root".to_string(),
                key: PackageKey::new("https://mega.nz/folder/root".to_string()),
                display_name: "Package".to_string(),
                files: vec![ResolvedFile {
                    file_id: file_id.clone(),
                    path: "episode.bin".to_string(),
                    size: 128,
                }],
                collision: None,
            },
        });
        app.handle_resume_validation_started_event(file_id.clone(), 0);
        app.handle_verification_progress_event(file_id.clone(), 64);
        app.flush_session_persistence();

        app.should_quit = true;
        let mut shutting_down = false;

        assert!(finish_interactive_shutdown_if_ready(
            &mut app,
            &mut shutting_down
        ));
        assert!(shutting_down);
        assert!(app.shutdown_pending_files.is_empty());
        assert!(!app.verifying_files.contains(&file_id));
        assert!(!app.verification_inflight_files.contains(&file_id));
        assert!(!app.shutdown_blocking_verifications.contains(&file_id));
    }
}
