//! Keyboard and paste input handling.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::extract_urls;

use super::app::{App, ConfigField, FileStatus, LoginState, Popup, UiAction};

pub fn handle_input(app: &mut App, key: KeyEvent) {
    // Global quit
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        request_quit(app);
        return;
    }

    match app.popup {
        Popup::Login => handle_login_input(app, key),
        Popup::Config => handle_config_input(app, key),
        Popup::None => handle_main_input(app, key),
    }
}

const fn request_quit(app: &mut App) {
    if app.quit_policy.is_enabled() {
        app.should_quit = true;
    }
}

fn handle_login_input(app: &mut App, key: KeyEvent) {
    if app.login.logging_in {
        // Don't accept input while logging in (except Esc to quit)
        if key.code == KeyCode::Esc {
            request_quit(app);
        }
        return;
    }

    match key.code {
        KeyCode::Tab => {
            app.login.active_field = (app.login.active_field + 1) % LoginState::field_count();
        }
        KeyCode::BackTab => {
            app.login.active_field = if app.login.active_field == 0 {
                LoginState::field_count() - 1
            } else {
                app.login.active_field - 1
            };
        }
        KeyCode::Enter => {
            if app.login.has_credentials() {
                app.handle_ui_action(UiAction::Login {
                    email: app.login.email().to_string(),
                    password: app.login.password().to_string(),
                    mfa: app.login.mfa().to_string(),
                });
            } else {
                app.login.error = Some("Email and password are required".to_string());
            }
        }
        KeyCode::Char(c) => {
            app.login.active_value_mut().push(c);
        }
        KeyCode::Backspace => {
            app.login.active_value_mut().pop();
        }
        KeyCode::Esc => {
            request_quit(app);
        }
        _ => {}
    }
}

fn handle_config_input(app: &mut App, key: KeyEvent) {
    let field_count = ConfigField::ALL.len();

    match key.code {
        KeyCode::Up | KeyCode::BackTab => {
            app.config.active_field = if app.config.active_field == 0 {
                field_count - 1
            } else {
                app.config.active_field - 1
            };
        }
        KeyCode::Down | KeyCode::Tab => {
            app.config.active_field = (app.config.active_field + 1) % field_count;
        }
        KeyCode::Char('+' | '=') | KeyCode::Right => {
            match ConfigField::ALL[app.config.active_field] {
                ConfigField::ChunksPerFile => app.handle_ui_action(UiAction::UpdateConfig {
                    chunks_per_file: Some(app.config.config.chunks_per_file.saturating_add(1)),
                    concurrent_files: None,
                    force_overwrite: None,
                    cleanup_on_error: None,
                }),
                ConfigField::ConcurrentFiles => app.handle_ui_action(UiAction::UpdateConfig {
                    chunks_per_file: None,
                    concurrent_files: Some(app.config.config.concurrent_files.saturating_add(1)),
                    force_overwrite: None,
                    cleanup_on_error: None,
                }),
                ConfigField::ForceOverwrite => app.handle_ui_action(UiAction::UpdateConfig {
                    chunks_per_file: None,
                    concurrent_files: None,
                    force_overwrite: Some(!app.config.config.force_overwrite),
                    cleanup_on_error: None,
                }),
                ConfigField::CleanupOnError => app.handle_ui_action(UiAction::UpdateConfig {
                    chunks_per_file: None,
                    concurrent_files: None,
                    force_overwrite: None,
                    cleanup_on_error: Some(!app.config.config.cleanup_on_error),
                }),
            }
        }
        KeyCode::Char('-') | KeyCode::Left => match ConfigField::ALL[app.config.active_field] {
            ConfigField::ChunksPerFile => app.handle_ui_action(UiAction::UpdateConfig {
                chunks_per_file: Some(app.config.config.chunks_per_file.saturating_sub(1).max(1)),
                concurrent_files: None,
                force_overwrite: None,
                cleanup_on_error: None,
            }),
            ConfigField::ConcurrentFiles => app.handle_ui_action(UiAction::UpdateConfig {
                chunks_per_file: None,
                concurrent_files: Some(app.config.config.concurrent_files.saturating_sub(1).max(1)),
                force_overwrite: None,
                cleanup_on_error: None,
            }),
            ConfigField::ForceOverwrite => app.handle_ui_action(UiAction::UpdateConfig {
                chunks_per_file: None,
                concurrent_files: None,
                force_overwrite: Some(!app.config.config.force_overwrite),
                cleanup_on_error: None,
            }),
            ConfigField::CleanupOnError => app.handle_ui_action(UiAction::UpdateConfig {
                chunks_per_file: None,
                concurrent_files: None,
                force_overwrite: None,
                cleanup_on_error: Some(!app.config.config.cleanup_on_error),
            }),
        },
        KeyCode::Char(' ') => match ConfigField::ALL[app.config.active_field] {
            ConfigField::ForceOverwrite => app.handle_ui_action(UiAction::UpdateConfig {
                chunks_per_file: None,
                concurrent_files: None,
                force_overwrite: Some(!app.config.config.force_overwrite),
                cleanup_on_error: None,
            }),
            ConfigField::CleanupOnError => app.handle_ui_action(UiAction::UpdateConfig {
                chunks_per_file: None,
                concurrent_files: None,
                force_overwrite: None,
                cleanup_on_error: Some(!app.config.config.cleanup_on_error),
            }),
            _ => {}
        },
        KeyCode::Enter | KeyCode::Esc => {
            app.popup = Popup::None;
        }
        _ => {}
    }
}

fn handle_main_input(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => {
            let extracted = extract_urls(&app.url_input);
            if !extracted.is_empty() {
                app.handle_ui_action(UiAction::AddUrls(extracted));
                app.url_input.clear();
            }
        }
        KeyCode::Char('p') if app.url_input.is_empty() => {
            app.handle_ui_action(UiAction::TogglePause);
        }
        KeyCode::Char('d') | KeyCode::Delete if app.url_input.is_empty() => delete_selected(app),
        KeyCode::Char('R') if app.url_input.is_empty() => reset_selected(app),
        KeyCode::Char('r')
            if app.url_input.is_empty() && key.modifiers.contains(KeyModifiers::SHIFT) =>
        {
            reset_selected(app);
        }
        KeyCode::Char('r') if app.url_input.is_empty() => {
            // Retry selected errored file — re-queue it
            if let Some(selected) = app.selected_file_index()
                && matches!(app.files[selected].status, FileStatus::Error(_))
            {
                let file_id = app.files[selected].id.clone();
                app.handle_ui_action(UiAction::RetryFile(file_id));
            }
        }
        KeyCode::Char('c') if app.url_input.is_empty() => {
            app.popup = Popup::Config;
        }
        KeyCode::Up if app.url_input.is_empty() => {
            let len = app.files.len();
            if len > 0 {
                let i = app.file_list_state.selected().unwrap_or(0);
                app.file_list_state
                    .select(Some(if i == 0 { len - 1 } else { i - 1 }));
            }
        }
        KeyCode::Down if app.url_input.is_empty() => {
            let len = app.files.len();
            if len > 0 {
                let i = app.file_list_state.selected().unwrap_or(0);
                app.file_list_state.select(Some((i + 1) % len));
            }
        }
        KeyCode::Char('q') if app.url_input.is_empty() => {
            request_quit(app);
        }
        KeyCode::Esc => {
            if app.url_input.is_empty() {
                request_quit(app);
            } else {
                app.url_input.clear();
            }
        }
        KeyCode::Char(c) => {
            app.url_input.push(c);
        }
        KeyCode::Backspace => {
            app.url_input.pop();
        }
        _ => {}
    }
}

fn reset_selected(app: &mut App) {
    let Some(selected) = app.selected_file_index() else {
        return;
    };
    app.handle_ui_action(UiAction::ResetFile(app.files[selected].id.clone()));
}

fn delete_selected(app: &mut App) {
    let Some(selected_file) = app.selected_file_index() else {
        return;
    };
    let file = &app.files[selected_file];
    let file_status = file.status.clone();
    let can_remove = matches!(
        file_status,
        FileStatus::Queued | FileStatus::Error(_) | FileStatus::Downloading | FileStatus::Complete
    );
    if !can_remove {
        return;
    }
    app.handle_ui_action(UiAction::DeleteFile(file.id.clone()));
}

pub fn handle_paste(app: &mut App, text: &str) {
    match app.popup {
        Popup::Login => {
            if !app.login.logging_in {
                app.login.active_value_mut().push_str(text.trim());
            }
        }
        Popup::Config => {}
        Popup::None => {
            // Append pasted text to URL input, replacing newlines with spaces
            app.url_input.push_str(&text.replace(['\n', '\r'], " "));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::app::{App, FileEntry, FileStatus, Popup, QuitPolicy};
    use super::*;
    use crate::{
        DownloadConfig, FileEntry as SessionFileEntry, FileEntryStatus, SavedCredentials,
        SessionState, UrlEntry, UrlStatus,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use std::env;
    use std::path::Path;
    use tempfile::tempdir;
    use tokio::sync::mpsc;

    struct StateDirectoryGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        previous: Option<std::ffi::OsString>,
    }

    impl StateDirectoryGuard {
        fn set(path: &Path) -> Self {
            let lock = crate::state::STATE_DIRECTORY_TEST_LOCK.lock().unwrap();
            let previous = env::var_os("STATE_DIRECTORY");
            unsafe { env::set_var("STATE_DIRECTORY", path) };
            Self {
                _lock: lock,
                previous,
            }
        }
    }

    impl Drop for StateDirectoryGuard {
        fn drop(&mut self) {
            if let Some(ref value) = self.previous {
                unsafe { env::set_var("STATE_DIRECTORY", value) };
            } else {
                unsafe { env::remove_var("STATE_DIRECTORY") };
            }
        }
    }

    fn test_app() -> App {
        let (tx, _rx) = mpsc::unbounded_channel();
        App::new(9723, tx, true)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn handle_main_input_quit() {
        let mut app = test_app();
        assert!(!app.should_quit);
        handle_input(&mut app, key(KeyCode::Char('q')));
        assert!(app.should_quit);
    }

    #[test]
    fn handle_main_input_quit_disabled_via_flag() {
        let mut app = test_app();
        app.quit_policy = QuitPolicy::Disabled;
        assert!(!app.should_quit);
        handle_input(&mut app, key(KeyCode::Char('q')));
        assert!(!app.should_quit);
    }

    #[test]
    fn handle_main_input_esc_quit_when_empty() {
        let mut app = test_app();
        handle_input(&mut app, key(KeyCode::Esc));
        assert!(app.should_quit);
    }

    #[test]
    fn handle_main_input_esc_clears_url_when_nonempty() {
        let mut app = test_app();
        app.url_input = "some text".to_string();
        handle_input(&mut app, key(KeyCode::Esc));
        assert!(!app.should_quit);
        assert!(app.url_input.is_empty());
    }

    #[test]
    fn handle_main_input_typing() {
        let mut app = test_app();
        handle_input(&mut app, key(KeyCode::Char('h')));
        handle_input(&mut app, key(KeyCode::Char('i')));
        assert_eq!(app.url_input, "hi");
    }

    #[test]
    fn handle_main_input_typing_q_does_not_quit_while_editing() {
        let mut app = test_app();
        app.url_input = "https://example".to_string();
        handle_input(&mut app, key(KeyCode::Char('q')));
        assert!(!app.should_quit);
        assert_eq!(app.url_input, "https://exampleq");
    }

    #[test]
    fn handle_main_input_backspace() {
        let mut app = test_app();
        app.url_input = "abc".to_string();
        handle_input(&mut app, key(KeyCode::Backspace));
        assert_eq!(app.url_input, "ab");
    }

    #[test]
    fn handle_main_input_pause_toggle() {
        let mut app = test_app();
        assert!(!app.paused);
        handle_input(&mut app, key(KeyCode::Char('p')));
        assert!(app.paused);
        handle_input(&mut app, key(KeyCode::Char('p')));
        assert!(!app.paused);
    }

    #[test]
    fn handle_main_input_config_popup() {
        let mut app = test_app();
        handle_input(&mut app, key(KeyCode::Char('c')));
        assert_eq!(app.popup, Popup::Config);
    }

    #[test]
    fn handle_main_input_delete_cancels_downloading() {
        let mut app = test_app();
        let token = tokio_util::sync::CancellationToken::new();
        app.files.push(FileEntry {
            id: "test.zip".to_string(),
            name: "test.zip".to_string(),
            size: 1000,
            downloaded: 500,
            source_url: None,
            status: FileStatus::Downloading,
        });
        app.cancellation_tokens
            .insert("test.zip".to_string(), token.clone());
        app.file_list_state.select(Some(0));

        handle_input(&mut app, key(KeyCode::Char('d')));
        assert!(token.is_cancelled());
        assert!(app.files.is_empty());
    }

    #[test]
    fn handle_main_input_delete_removes_session_entry_and_keeps_selection() {
        let dir = tempdir().unwrap();
        let _guard = StateDirectoryGuard::set(dir.path());
        let mut app = test_app();
        app.files = vec![
            FileEntry {
                id: "first.bin".to_string(),
                name: "first.bin".to_string(),
                size: 10,
                downloaded: 0,
                source_url: Some("https://mega.nz/file/first".to_string()),
                status: FileStatus::Queued,
            },
            FileEntry {
                id: "second.bin".to_string(),
                name: "second.bin".to_string(),
                size: 20,
                downloaded: 0,
                source_url: Some("https://mega.nz/file/second".to_string()),
                status: FileStatus::Queued,
            },
        ];
        app.recompute_totals();
        app.file_list_state.select(Some(0));

        let mut session = SessionState::new(
            SavedCredentials::encrypt("test@example.com", "hunter2", None),
            DownloadConfig::default(),
            vec![UrlEntry {
                url: "https://mega.nz/folder/root".to_string(),
                status: UrlStatus::Fetched,
            }],
        );
        session.files = vec![
            SessionFileEntry {
                key: None,
                url_index: 0,
                path: "first.bin".to_string(),
                size: 10,
                status: FileEntryStatus::Pending,
            },
            SessionFileEntry {
                key: None,
                url_index: 0,
                path: "second.bin".to_string(),
                size: 20,
                status: FileEntryStatus::Pending,
            },
        ];
        let session_path = session.state_path();
        app.session = Some(session);

        handle_input(&mut app, key(KeyCode::Delete));

        assert_eq!(app.files.len(), 1);
        assert_eq!(app.files[0].id, "second.bin");
        assert_eq!(app.file_list_state.selected(), Some(0));
        assert_eq!(app.total_size, 20);
        assert!(app.deleted_files.contains("first.bin"));
        assert!(session_path.exists());

        let session = app.session.as_ref().expect("session should remain");
        let statuses: Vec<_> = session
            .files
            .iter()
            .map(|file| (file.path.as_str(), &file.status))
            .collect();
        assert_eq!(
            statuses,
            vec![
                ("first.bin", &FileEntryStatus::Skipped),
                ("second.bin", &FileEntryStatus::Pending),
            ]
        );
    }

    #[test]
    fn handle_main_input_delete_uses_visible_sorted_row() {
        let mut app = test_app();
        app.files = vec![
            FileEntry {
                id: "complete.bin".to_string(),
                name: "complete.bin".to_string(),
                size: 10,
                downloaded: 10,
                source_url: None,
                status: FileStatus::Complete,
            },
            FileEntry {
                id: "active.bin".to_string(),
                name: "active.bin".to_string(),
                size: 20,
                downloaded: 5,
                source_url: None,
                status: FileStatus::Downloading,
            },
        ];
        app.file_list_state.select(Some(0));

        handle_input(&mut app, key(KeyCode::Delete));

        assert_eq!(app.files.len(), 1);
        assert_eq!(app.files[0].id, "complete.bin");
        assert_eq!(app.file_list_state.selected(), Some(0));
    }

    #[test]
    fn handle_main_input_shift_r_resets_selected_file_from_scratch() {
        let dir = tempdir().unwrap();
        let final_path = dir.path().join("active.bin");
        let final_path_string = final_path.to_string_lossy();
        let part_path = std::path::PathBuf::from(format!("{final_path_string}.part"));
        let sidecar_path = std::path::PathBuf::from(format!("{final_path_string}.part.meta.json"));
        std::fs::write(&final_path, b"complete").unwrap();
        std::fs::write(&part_path, b"partial").unwrap();
        std::fs::write(&sidecar_path, b"metadata").unwrap();

        let mut app = test_app();
        let (url_tx, mut url_rx) = mpsc::unbounded_channel();
        app.url_tx = url_tx;
        let token = tokio_util::sync::CancellationToken::new();
        app.files.push(FileEntry {
            id: "active.bin".to_string(),
            name: final_path.to_string_lossy().into_owned(),
            size: 100,
            downloaded: 80,
            source_url: Some("https://mega.nz/file/reset".to_string()),
            status: FileStatus::Downloading,
        });
        app.cancellation_tokens
            .insert("active.bin".to_string(), token.clone());
        app.file_list_state.select(Some(0));

        handle_input(&mut app, key(KeyCode::Char('R')));

        assert!(token.is_cancelled());
        assert_eq!(app.files[0].status, FileStatus::Queued);
        assert_eq!(app.files[0].downloaded, 0);
        assert_eq!(app.file_speed("active.bin"), 0);
        assert_eq!(
            url_rx.try_recv().unwrap(),
            "https://mega.nz/file/reset".to_string()
        );
        assert!(!final_path.exists());
        assert!(!part_path.exists());
        assert!(!sidecar_path.exists());
    }

    #[test]
    fn handle_main_input_delete_removes_completed_file_and_artifacts() {
        let dir = tempdir().unwrap();
        let final_path = dir.path().join("complete.bin");
        let final_path_string = final_path.to_string_lossy();
        let part_path = std::path::PathBuf::from(format!("{final_path_string}.part"));
        let sidecar_path = std::path::PathBuf::from(format!("{final_path_string}.part.meta.json"));
        std::fs::write(&final_path, b"complete").unwrap();
        std::fs::write(&part_path, b"partial").unwrap();
        std::fs::write(&sidecar_path, b"metadata").unwrap();

        let mut app = test_app();
        app.upsert_overlay_file(
            FileEntry {
                id: "complete.bin".to_string(),
                name: final_path.to_string_lossy().into_owned(),
                size: 100,
                downloaded: 100,
                source_url: Some("https://mega.nz/file/complete".to_string()),
                status: FileStatus::Complete,
            },
            false,
        );
        app.file_list_state.select(Some(0));

        handle_input(&mut app, key(KeyCode::Char('d')));

        assert!(app.files.is_empty());
        assert!(!final_path.exists());
        assert!(!part_path.exists());
        assert!(!sidecar_path.exists());
    }

    #[test]
    fn handle_login_input_validates_empty() {
        let mut app = test_app();
        app.popup = Popup::Login;
        // LoginState starts with empty credentials by default
        handle_input(&mut app, key(KeyCode::Enter));
        assert_eq!(
            app.login.error,
            Some("Email and password are required".to_string())
        );
    }

    #[test]
    fn handle_login_input_tab_cycles() {
        let mut app = test_app();
        app.popup = Popup::Login;
        assert_eq!(app.login.active_field, 0);
        handle_input(&mut app, key(KeyCode::Tab));
        assert_eq!(app.login.active_field, 1);
        handle_input(&mut app, key(KeyCode::Tab));
        assert_eq!(app.login.active_field, 2);
        handle_input(&mut app, key(KeyCode::Tab));
        assert_eq!(app.login.active_field, 0);
    }

    #[test]
    fn handle_paste_appends_to_url_input() {
        let mut app = test_app();
        handle_paste(&mut app, "https://mega.nz/file/abc");
        assert_eq!(app.url_input, "https://mega.nz/file/abc");
    }

    #[test]
    fn handle_paste_replaces_newlines() {
        let mut app = test_app();
        handle_paste(&mut app, "url1\nurl2\r\nurl3");
        assert_eq!(app.url_input, "url1 url2  url3");
    }

    #[test]
    fn handle_paste_login_trims() {
        let mut app = test_app();
        app.popup = Popup::Login;
        app.login.active_field = 0;
        handle_paste(&mut app, "  user@example.com  ");
        assert_eq!(app.login.email(), "user@example.com");
    }

    #[test]
    fn add_url_deduplicates() {
        let mut app = test_app();
        let mut url_rx = app.url_rx.take().expect("url_rx should exist");
        app.submit_url("https://mega.nz/file/abc".to_string());
        app.submit_url("https://mega.nz/file/abc".to_string());
        assert_eq!(app.urls.len(), 1);
        assert_eq!(url_rx.try_recv().unwrap(), "https://mega.nz/file/abc");
        assert!(url_rx.try_recv().is_err());
    }

    #[test]
    fn retry_recomputes_totals_for_errored_file() {
        let mut app = test_app();
        let (url_tx, mut url_rx) = mpsc::unbounded_channel();
        app.url_tx = url_tx;
        app.files.push(FileEntry {
            id: "error.bin".to_string(),
            name: "error.bin".to_string(),
            size: 100,
            downloaded: 42,
            source_url: Some("https://mega.nz/file/error".to_string()),
            status: FileStatus::Error("boom".to_string()),
        });
        app.recompute_totals();
        app.file_list_state.select(Some(0));

        assert_eq!(app.files_total, 0);
        assert_eq!(app.total_downloaded, 42);

        handle_input(&mut app, key(KeyCode::Char('r')));

        assert_eq!(app.files[0].status, FileStatus::Queued);
        assert_eq!(app.files[0].downloaded, 0);
        assert_eq!(app.files_total, 1);
        assert_eq!(app.total_downloaded, 0);
        assert_eq!(
            url_rx.try_recv().unwrap(),
            "https://mega.nz/file/error".to_string()
        );
    }

    #[test]
    fn handle_main_input_url_submit() {
        let mut app = test_app();
        // Replace the url_tx so we can observe what's sent
        let (url_tx, mut url_rx) = mpsc::unbounded_channel();
        app.url_tx = url_tx;
        app.url_input = "https://mega.nz/file/test123".to_string();

        handle_input(&mut app, key(KeyCode::Enter));

        assert!(app.url_input.is_empty());
        let received = url_rx.try_recv().unwrap();
        assert_eq!(received, "https://mega.nz/file/test123");
    }
}
