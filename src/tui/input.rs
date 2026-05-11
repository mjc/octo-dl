//! Keyboard and paste input handling.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::extract_urls;
#[cfg(test)]
use crate::tui::event::DownloadRequest;

use super::app::{
    App, ConfigField, ConfirmAction, FileStatus, LoginState, Popup, SortKey, UiAction,
};
use super::visible::TuiRow;

pub fn handle_input(app: &mut App, key: KeyEvent) {
    // Global quit
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        request_quit(app);
        return;
    }

    match app.popup {
        Popup::Login => handle_login_input(app, key),
        Popup::Config => handle_config_input(app, key),
        Popup::Confirm => handle_confirm_input(app, key),
        Popup::Sort => handle_sort_input(app, key),
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

fn handle_confirm_input(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
            if let Some(action) = app.pending_confirmation.take() {
                app.popup = Popup::None;
                match action {
                    ConfirmAction::DeleteFile(id) => {
                        app.handle_ui_action(UiAction::DeleteFile(id));
                    }
                    ConfirmAction::DeletePackage(id) => {
                        app.handle_ui_action(UiAction::DeletePackage(id));
                    }
                    ConfirmAction::ResetFile(id) => {
                        app.handle_ui_action(UiAction::ResetFile(id));
                    }
                    ConfirmAction::ResetPackage(id) => {
                        app.handle_ui_action(UiAction::ResetPackage(id));
                    }
                }
            } else {
                app.popup = Popup::None;
            }
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            app.pending_confirmation = None;
            app.popup = Popup::None;
        }
        _ => {}
    }
}

fn handle_sort_input(app: &mut App, key: KeyEvent) {
    let mut sort_changed = false;
    let selected_row_identity = app.selected_row();

    match key.code {
        KeyCode::Up | KeyCode::BackTab => {
            app.sort.active_field = if app.sort.active_field == 0 {
                SortKey::ALL.len()
            } else {
                app.sort.active_field - 1
            };
        }
        KeyCode::Down | KeyCode::Tab => {
            app.sort.active_field = (app.sort.active_field + 1) % (SortKey::ALL.len() + 1);
        }
        KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') => {
            if app.sort.active_field == SortKey::ALL.len() {
                app.sort.direction = app.sort.direction.toggled();
                sort_changed = true;
            } else {
                app.sort.key = SortKey::ALL[app.sort.active_field];
                sort_changed = true;
            }
        }
        KeyCode::Enter => {
            if app.sort.active_field < SortKey::ALL.len() {
                app.sort.key = SortKey::ALL[app.sort.active_field];
                sort_changed = true;
            }
            app.popup = Popup::None;
        }
        KeyCode::Esc | KeyCode::Char('s') => {
            app.popup = Popup::None;
        }
        _ => {}
    }

    if sort_changed {
        app.sync_visible_files_preserving(selected_row_identity);
    }
}

fn handle_main_input(app: &mut App, key: KeyEvent) {
    if app.url_input_active {
        handle_url_input(app, key);
        return;
    }

    match key.code {
        KeyCode::Char('a' | 'i') => {
            app.url_input_active = true;
        }
        KeyCode::Char('p') => {
            app.handle_ui_action(UiAction::TogglePause);
        }
        KeyCode::Char('D') => delete_selected_immediately(app),
        KeyCode::Char('d') | KeyCode::Delete => delete_selected(app),
        KeyCode::Char('R') => reset_selected(app),
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::SHIFT) => reset_selected(app),
        KeyCode::Char('r') => match app.selected_row() {
            Some(TuiRow::Package(package_id)) => {
                app.handle_ui_action(UiAction::RetryPackage(package_id));
            }
            Some(TuiRow::File { file_id, .. }) => {
                if app
                    .files
                    .iter()
                    .find(|file| file.id == file_id)
                    .is_some_and(|file| matches!(file.status, FileStatus::Error(_)))
                {
                    app.handle_ui_action(UiAction::RetryFile(file_id));
                }
            }
            None => {}
        },
        KeyCode::Char('c') => {
            app.popup = Popup::Config;
        }
        KeyCode::Char('s') => {
            app.popup = Popup::Sort;
        }
        KeyCode::Enter | KeyCode::Char(' ') => toggle_selected_package(app),
        KeyCode::Up | KeyCode::Char('k') => select_previous_file(app),
        KeyCode::Down | KeyCode::Char('j') => select_next_file(app),
        KeyCode::PageUp => move_file_selection(app, -10),
        KeyCode::PageDown => move_file_selection(app, 10),
        KeyCode::Home | KeyCode::Char('g') => select_first_file(app),
        KeyCode::End | KeyCode::Char('G') => select_last_file(app),
        KeyCode::Char('q') => {
            request_quit(app);
        }
        KeyCode::Esc => {
            request_quit(app);
        }
        _ => {}
    }
}

fn handle_url_input(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => {
            let trimmed = app.url_input.trim();
            let extracted = extract_urls(trimmed);
            if !extracted.is_empty() {
                app.handle_ui_action(UiAction::AddUrls(extracted));
                app.url_input.clear();
                app.url_input_active = false;
            } else if trimmed.is_empty() {
                app.status = "Enter a URL or press Esc to cancel".to_string();
            } else {
                app.status = "No valid URLs found in input".to_string();
            }
        }
        KeyCode::Esc => {
            app.url_input.clear();
            app.url_input_active = false;
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

fn select_previous_file(app: &mut App) {
    let len = app.visible_rows().len();
    if len > 0 {
        let i = app.file_list_state.selected().unwrap_or(0);
        app.file_list_state
            .select(Some(if i == 0 { len - 1 } else { i - 1 }));
    }
}

fn select_next_file(app: &mut App) {
    let len = app.visible_rows().len();
    if len > 0 {
        let i = app.file_list_state.selected().unwrap_or(0);
        app.file_list_state.select(Some((i + 1) % len));
    }
}

fn move_file_selection(app: &mut App, delta: isize) {
    let len = app.visible_rows().len();
    if len == 0 {
        return;
    }
    let current = app.file_list_state.selected().unwrap_or(0);
    let next = current
        .saturating_add_signed(delta)
        .min(len.saturating_sub(1));
    app.file_list_state.select(Some(next));
}

fn select_first_file(app: &mut App) {
    if !app.visible_rows().is_empty() {
        app.file_list_state.select(Some(0));
    }
}

fn select_last_file(app: &mut App) {
    let len = app.visible_rows().len();
    if len > 0 {
        app.file_list_state.select(Some(len - 1));
    }
}

fn reset_selected(app: &mut App) {
    match app.selected_row() {
        Some(TuiRow::Package(package_id)) => {
            app.pending_confirmation = Some(ConfirmAction::ResetPackage(package_id));
            app.popup = Popup::Confirm;
        }
        Some(TuiRow::File { file_id, .. }) => {
            app.pending_confirmation = Some(ConfirmAction::ResetFile(file_id));
            app.popup = Popup::Confirm;
        }
        None => {}
    }
}

fn delete_selected(app: &mut App) {
    match app.selected_row() {
        Some(TuiRow::Package(package_id)) => {
            app.pending_confirmation = Some(ConfirmAction::DeletePackage(package_id));
            app.popup = Popup::Confirm;
        }
        Some(TuiRow::File { file_id, .. }) => {
            app.pending_confirmation = Some(ConfirmAction::DeleteFile(file_id));
            app.popup = Popup::Confirm;
        }
        None => {}
    }
}

fn delete_selected_immediately(app: &mut App) {
    match app.selected_row() {
        Some(TuiRow::Package(package_id)) => {
            app.handle_ui_action(UiAction::DeletePackage(package_id));
        }
        Some(TuiRow::File { file_id, .. }) => {
            app.handle_ui_action(UiAction::DeleteFile(file_id));
        }
        None => {}
    }
}

fn toggle_selected_package(app: &mut App) {
    if let Some(TuiRow::Package(package_id)) = app.selected_row() {
        if !app.expanded_packages.insert(package_id.clone()) {
            app.expanded_packages.remove(&package_id);
        }
    }
}

pub fn handle_paste(app: &mut App, text: &str) {
    match app.popup {
        Popup::Login => {
            if !app.login.logging_in {
                app.login.active_value_mut().push_str(text.trim());
            }
        }
        Popup::Config | Popup::Confirm | Popup::Sort => {}
        Popup::None => {
            // Append pasted text to URL input, replacing newlines with spaces
            app.url_input_active = true;
            app.url_input.push_str(&text.replace(['\n', '\r'], " "));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::app::{App, FileEntry, FileStatus, Popup, QuitPolicy};
    use super::*;
    use crate::core::{CoreEvent, PackageCollision, ResolvedFile, ResolvedPackage};
    use crate::test_support::{FileFixtureStatus, UrlFixtureStatus, push_file, session_snapshot};
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
            let lock = crate::core::session::STATE_DIRECTORY_TEST_LOCK
                .lock()
                .unwrap();
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

    fn confirm(app: &mut App) {
        assert_eq!(app.popup, Popup::Confirm);
        handle_input(app, key(KeyCode::Char('y')));
        assert_eq!(app.popup, Popup::None);
        assert_eq!(app.pending_confirmation, None);
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
        app.url_input_active = true;
        handle_input(&mut app, key(KeyCode::Esc));
        assert!(!app.should_quit);
        assert!(app.url_input.is_empty());
        assert!(!app.url_input_active);
    }

    #[test]
    fn handle_main_input_typing() {
        let mut app = test_app();
        handle_input(&mut app, key(KeyCode::Char('a')));
        handle_input(&mut app, key(KeyCode::Char('h')));
        handle_input(&mut app, key(KeyCode::Char('i')));
        assert_eq!(app.url_input, "hi");
    }

    #[test]
    fn handle_main_input_ignores_unmapped_keys_in_command_mode() {
        let mut app = test_app();
        handle_input(&mut app, key(KeyCode::Char('h')));
        assert!(app.url_input.is_empty());
        assert!(!app.should_quit);
    }

    #[test]
    fn handle_main_input_typing_q_does_not_quit_in_url_mode() {
        let mut app = test_app();
        app.url_input = "https://example".to_string();
        app.url_input_active = true;
        handle_input(&mut app, key(KeyCode::Char('q')));
        assert!(!app.should_quit);
        assert_eq!(app.url_input, "https://exampleq");
    }

    #[test]
    fn handle_main_input_backspace() {
        let mut app = test_app();
        app.url_input = "abc".to_string();
        app.url_input_active = true;
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
    fn handle_main_input_navigation_keys_move_selection() {
        let mut app = test_app();
        for i in 0..12 {
            app.files.push(FileEntry {
                id: format!("file-{i}"),
                name: format!("file-{i}"),
                size: 1,
                downloaded: 0,
                status: FileStatus::Queued,
            });
        }
        app.file_list_state.select(Some(0));

        handle_input(&mut app, key(KeyCode::Char('j')));
        assert_eq!(app.file_list_state.selected(), Some(1));

        handle_input(&mut app, key(KeyCode::Char('k')));
        assert_eq!(app.file_list_state.selected(), Some(0));

        handle_input(&mut app, key(KeyCode::PageDown));
        assert_eq!(app.file_list_state.selected(), Some(10));

        handle_input(&mut app, key(KeyCode::PageUp));
        assert_eq!(app.file_list_state.selected(), Some(0));

        handle_input(&mut app, key(KeyCode::End));
        assert_eq!(app.file_list_state.selected(), Some(11));

        handle_input(&mut app, key(KeyCode::Char('g')));
        assert_eq!(app.file_list_state.selected(), Some(0));
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
            status: FileStatus::Downloading,
        });
        app.cancellation_tokens
            .insert("test.zip".to_string(), token.clone());
        app.file_list_state.select(Some(0));

        handle_input(&mut app, key(KeyCode::Char('d')));
        assert_eq!(
            app.pending_confirmation,
            Some(ConfirmAction::DeleteFile("test.zip".to_string()))
        );
        assert!(!token.is_cancelled());
        confirm(&mut app);
        assert!(token.is_cancelled());
        assert!(app.files.is_empty());
    }

    #[test]
    fn handle_confirm_cancel_leaves_destructive_action_unapplied() {
        let mut app = test_app();
        app.files.push(FileEntry {
            id: "keep.bin".to_string(),
            name: "keep.bin".to_string(),
            size: 10,
            downloaded: 0,
            status: FileStatus::Queued,
        });
        app.file_list_state.select(Some(0));

        handle_input(&mut app, key(KeyCode::Char('d')));
        assert_eq!(
            app.pending_confirmation,
            Some(ConfirmAction::DeleteFile("keep.bin".to_string()))
        );

        handle_input(&mut app, key(KeyCode::Esc));

        assert_eq!(app.popup, Popup::None);
        assert_eq!(app.pending_confirmation, None);
        assert_eq!(app.files.len(), 1);
        assert_eq!(app.files[0].id, "keep.bin");
    }

    #[test]
    fn handle_main_input_delete_core_backed_entry() {
        let mut app = test_app();
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: "https://mega.nz/file/core".to_string(),
                source_url: "https://mega.nz/file/core".to_string(),
                display_name: "Core".to_string(),
                files: vec![ResolvedFile {
                    file_id: "core.bin".to_string(),
                    path: "core.bin".to_string(),
                    size: 10,
                }],
                collision: None,
            },
        });
        app.apply_core_event(CoreEvent::FileCompleted {
            file_id: "core.bin".to_string(),
        });
        app.file_list_state.select(Some(0));

        handle_input(&mut app, key(KeyCode::Delete));
        assert_eq!(
            app.pending_confirmation,
            Some(ConfirmAction::DeletePackage(
                "https://mega.nz/file/core".to_string()
            ))
        );
        confirm(&mut app);

        assert!(app.files.is_empty());
    }

    #[test]
    fn handle_main_input_expands_package_and_file_action_targets_child() {
        let mut app = test_app();
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: "pkg".to_string(),
                source_url: "https://mega.nz/folder/pkg".to_string(),
                display_name: "Package".to_string(),
                files: vec![
                    ResolvedFile {
                        file_id: "first.bin".to_string(),
                        path: "first.bin".to_string(),
                        size: 10,
                    },
                    ResolvedFile {
                        file_id: "second.bin".to_string(),
                        path: "second.bin".to_string(),
                        size: 20,
                    },
                ],
                collision: None,
            },
        });
        app.file_list_state.select(Some(0));
        assert_eq!(app.visible_rows().len(), 1);

        handle_input(&mut app, key(KeyCode::Enter));
        assert_eq!(app.visible_rows().len(), 3);

        handle_input(&mut app, key(KeyCode::Down));
        handle_input(&mut app, key(KeyCode::Delete));
        assert_eq!(
            app.pending_confirmation,
            Some(ConfirmAction::DeleteFile("first.bin".to_string()))
        );
    }

    #[test]
    fn handle_main_input_reset_package_targets_package_row() {
        let mut app = test_app();
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: "pkg".to_string(),
                source_url: "https://mega.nz/folder/pkg".to_string(),
                display_name: "Package".to_string(),
                files: vec![ResolvedFile {
                    file_id: "file.bin".to_string(),
                    path: "file.bin".to_string(),
                    size: 10,
                }],
                collision: None,
            },
        });
        app.file_list_state.select(Some(0));

        handle_input(&mut app, key(KeyCode::Char('R')));

        assert_eq!(
            app.pending_confirmation,
            Some(ConfirmAction::ResetPackage("pkg".to_string()))
        );
    }

    #[test]
    fn handle_sort_popup_selects_key_and_direction() {
        let mut app = test_app();

        handle_input(&mut app, key(KeyCode::Char('s')));
        assert_eq!(app.popup, Popup::Sort);

        handle_input(&mut app, key(KeyCode::Down));
        handle_input(&mut app, key(KeyCode::Enter));
        assert_eq!(app.sort.key, SortKey::Status);
        assert_eq!(app.popup, Popup::None);

        handle_input(&mut app, key(KeyCode::Char('s')));
        for _ in 0..(SortKey::ALL.len() - 1) {
            handle_input(&mut app, key(KeyCode::Down));
        }
        handle_input(&mut app, key(KeyCode::Char(' ')));
        assert_eq!(app.sort.direction, super::super::app::SortDirection::Desc);
    }

    #[test]
    fn handle_sort_popup_keeps_selected_row_identity_when_order_changes() {
        let mut app = test_app();
        for (package_id, display_name) in [("pkg-z", "Zulu"), ("pkg-a", "Alpha")] {
            app.apply_core_event(CoreEvent::PackageResolved {
                package: ResolvedPackage {
                    id: package_id.to_string(),
                    source_url: format!("https://mega.nz/folder/{package_id}"),
                    display_name: display_name.to_string(),
                    files: vec![ResolvedFile {
                        file_id: format!("{package_id}.bin"),
                        path: format!("{package_id}.bin"),
                        size: 10,
                    }],
                    collision: None,
                },
            });
        }

        app.file_list_state.select(Some(0));
        assert_eq!(
            app.selected_row(),
            Some(TuiRow::Package("pkg-z".to_string()))
        );

        handle_input(&mut app, key(KeyCode::Char('s')));
        handle_input(&mut app, key(KeyCode::Down));
        handle_input(&mut app, key(KeyCode::Down));
        handle_input(&mut app, key(KeyCode::Enter));

        assert_eq!(app.sort.key, SortKey::Name);
        assert_eq!(
            app.selected_row(),
            Some(TuiRow::Package("pkg-z".to_string()))
        );
        assert_eq!(app.file_list_state.selected(), Some(1));
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
                status: FileStatus::Queued,
            },
            FileEntry {
                id: "second.bin".to_string(),
                name: "second.bin".to_string(),
                size: 20,
                downloaded: 0,
                status: FileStatus::Queued,
            },
        ];
        app.recompute_totals();
        app.file_list_state.select(Some(0));

        let mut session = session_snapshot(vec![(
            "https://mega.nz/folder/root",
            UrlFixtureStatus::Fetched,
        )]);
        push_file(&mut session, 0, "first.bin", 10, FileFixtureStatus::Pending);
        push_file(
            &mut session,
            0,
            "second.bin",
            20,
            FileFixtureStatus::Pending,
        );
        let session_path = session.state_path();
        app.session = Some(session);

        handle_input(&mut app, key(KeyCode::Delete));
        assert_eq!(
            app.pending_confirmation,
            Some(ConfirmAction::DeleteFile("first.bin".to_string()))
        );
        confirm(&mut app);

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
            .map(|file| (file.path.as_str(), &file.lifecycle))
            .collect();
        assert_eq!(
            statuses,
            vec![
                ("first.bin", &crate::core::FileLifecycle::Skipped),
                ("second.bin", &crate::core::FileLifecycle::Queued),
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
                status: FileStatus::Complete,
            },
            FileEntry {
                id: "active.bin".to_string(),
                name: "active.bin".to_string(),
                size: 20,
                downloaded: 5,
                status: FileStatus::Downloading,
            },
        ];
        app.file_list_state.select(Some(0));

        handle_input(&mut app, key(KeyCode::Delete));
        assert_eq!(
            app.pending_confirmation,
            Some(ConfirmAction::DeleteFile("active.bin".to_string()))
        );
        confirm(&mut app);

        assert_eq!(app.files.len(), 1);
        assert_eq!(app.files[0].id, "complete.bin");
        assert_eq!(app.file_list_state.selected(), Some(0));
    }

    #[test]
    fn handle_main_input_delete_removes_failed_file() {
        let mut app = test_app();
        app.files.push(FileEntry {
            id: "failed.bin".to_string(),
            name: "failed.bin".to_string(),
            size: 10,
            downloaded: 4,
            status: FileStatus::Error("boom".to_string()),
        });
        app.file_list_state.select(Some(0));

        handle_input(&mut app, key(KeyCode::Delete));
        assert_eq!(
            app.pending_confirmation,
            Some(ConfirmAction::DeleteFile("failed.bin".to_string()))
        );
        confirm(&mut app);

        assert!(app.files.is_empty());
        assert!(app.deleted_files.contains("failed.bin"));
    }

    #[test]
    fn handle_main_input_delete_removes_failed_package_without_files() {
        let mut app = test_app();
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: "failed-pkg".to_string(),
                source_url: "https://mega.nz/folder/failed".to_string(),
                display_name: "Failed package".to_string(),
                files: Vec::new(),
                collision: Some(PackageCollision {
                    file_id: "duplicate.bin".to_string(),
                    existing_package_id: "existing".to_string(),
                    incoming_package_id: "failed-pkg".to_string(),
                }),
            },
        });
        app.file_list_state.select(Some(0));

        handle_input(&mut app, key(KeyCode::Delete));
        assert_eq!(
            app.pending_confirmation,
            Some(ConfirmAction::DeletePackage("failed-pkg".to_string()))
        );
        confirm(&mut app);

        assert!(app.visible_rows().is_empty());
        assert!(!app.core_state.packages.contains_key("failed-pkg"));
        assert!(app.deleted_files.contains("failed-pkg"));
        assert!(app.deleted_files.contains("https://mega.nz/folder/failed"));
    }

    #[test]
    fn handle_main_input_shift_d_deletes_without_confirmation() {
        let mut app = test_app();
        app.files.push(FileEntry {
            id: "remove.bin".to_string(),
            name: "remove.bin".to_string(),
            size: 10,
            downloaded: 0,
            status: FileStatus::Queued,
        });
        app.file_list_state.select(Some(0));

        handle_input(&mut app, key(KeyCode::Char('D')));

        assert_eq!(app.popup, Popup::None);
        assert_eq!(app.pending_confirmation, None);
        assert!(app.files.is_empty());
        assert!(app.deleted_files.contains("remove.bin"));
    }

    #[test]
    fn handle_main_input_shift_d_removes_completed_file_and_artifacts() {
        let dir = tempdir().unwrap();
        let final_path = dir.path().join("shift-delete-complete.bin");
        let final_path_string = final_path.to_string_lossy();
        let part_path = std::path::PathBuf::from(format!("{final_path_string}.part"));
        let sidecar_path = std::path::PathBuf::from(format!("{final_path_string}.part.meta.json"));
        std::fs::write(&final_path, b"complete").unwrap();
        std::fs::write(&part_path, b"partial").unwrap();
        std::fs::write(&sidecar_path, b"metadata").unwrap();

        let mut app = test_app();
        app.upsert_overlay_file(
            FileEntry {
                id: "shift-delete-complete.bin".to_string(),
                name: final_path.to_string_lossy().into_owned(),
                size: 100,
                downloaded: 100,
                status: FileStatus::Complete,
            },
            Some("https://mega.nz/file/shift-delete-complete".to_string()),
            false,
        );
        app.file_list_state.select(Some(0));

        handle_input(&mut app, key(KeyCode::Char('D')));

        assert_eq!(app.popup, Popup::None);
        assert_eq!(app.pending_confirmation, None);
        assert!(app.files.is_empty());
        assert!(!final_path.exists());
        assert!(!part_path.exists());
        assert!(!sidecar_path.exists());
    }

    #[test]
    fn handle_main_input_shift_d_removes_failed_package_without_files() {
        let mut app = test_app();
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: "failed-pkg".to_string(),
                source_url: "https://mega.nz/folder/failed".to_string(),
                display_name: "Failed package".to_string(),
                files: Vec::new(),
                collision: Some(PackageCollision {
                    file_id: "duplicate.bin".to_string(),
                    existing_package_id: "existing".to_string(),
                    incoming_package_id: "failed-pkg".to_string(),
                }),
            },
        });
        app.file_list_state.select(Some(0));
        assert_eq!(
            app.visible_rows(),
            vec![TuiRow::Package("failed-pkg".to_string())]
        );

        handle_input(&mut app, key(KeyCode::Char('D')));

        assert!(app.visible_rows().is_empty());
        assert!(!app.core_state.packages.contains_key("failed-pkg"));
        assert!(app.deleted_files.contains("failed-pkg"));
        assert!(app.deleted_files.contains("https://mega.nz/folder/failed"));
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
        app.upsert_overlay_file(
            FileEntry {
                id: "active.bin".to_string(),
                name: final_path.to_string_lossy().into_owned(),
                size: 100,
                downloaded: 80,
                status: FileStatus::Downloading,
            },
            Some("https://mega.nz/file/reset".to_string()),
            true,
        );
        app.cancellation_tokens
            .insert("active.bin".to_string(), token.clone());
        app.file_list_state.select(Some(0));

        handle_input(&mut app, key(KeyCode::Char('R')));
        assert_eq!(
            app.pending_confirmation,
            Some(ConfirmAction::ResetFile("active.bin".to_string()))
        );
        assert!(!token.is_cancelled());
        confirm(&mut app);

        assert!(token.is_cancelled());
        assert_eq!(app.files[0].status, FileStatus::Queued);
        assert_eq!(app.files[0].downloaded, 0);
        assert_eq!(app.file_speed("active.bin"), 0);
        assert_eq!(
            url_rx.try_recv().unwrap(),
            DownloadRequest::ResumeFileIds {
                source_url: "https://mega.nz/file/reset".to_string(),
                file_ids: vec!["active.bin".to_string()],
                attempt_ids: std::collections::HashMap::from([("active.bin".to_string(), 1)]),
            }
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
                status: FileStatus::Complete,
            },
            Some("https://mega.nz/file/complete".to_string()),
            false,
        );
        app.file_list_state.select(Some(0));

        handle_input(&mut app, key(KeyCode::Char('d')));
        assert_eq!(
            app.pending_confirmation,
            Some(ConfirmAction::DeleteFile("complete.bin".to_string()))
        );
        confirm(&mut app);

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
        assert!(app.url_input_active);
    }

    #[test]
    fn handle_paste_replaces_newlines() {
        let mut app = test_app();
        handle_paste(&mut app, "url1\nurl2\r\nurl3");
        assert_eq!(app.url_input, "url1 url2  url3");
        assert!(app.url_input_active);
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
        assert_eq!(
            url_rx.try_recv().unwrap(),
            DownloadRequest::SubmitUrl {
                url: "https://mega.nz/file/abc".to_string()
            }
        );
        assert!(url_rx.try_recv().is_err());
    }

    #[test]
    fn retry_recomputes_totals_for_errored_file() {
        let mut app = test_app();
        let (url_tx, mut url_rx) = mpsc::unbounded_channel();
        app.url_tx = url_tx;
        app.upsert_overlay_file(
            FileEntry {
                id: "error.bin".to_string(),
                name: "error.bin".to_string(),
                size: 100,
                downloaded: 42,
                status: FileStatus::Error("boom".to_string()),
            },
            Some("https://mega.nz/file/error".to_string()),
            true,
        );
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
            DownloadRequest::ResumeFileIds {
                source_url: "https://mega.nz/file/error".to_string(),
                file_ids: vec!["error.bin".to_string()],
                attempt_ids: std::collections::HashMap::from([("error.bin".to_string(), 1)]),
            }
        );
    }

    #[test]
    fn handle_main_input_url_submit() {
        let mut app = test_app();
        // Replace the url_tx so we can observe what's sent
        let (url_tx, mut url_rx) = mpsc::unbounded_channel();
        app.url_tx = url_tx;
        app.url_input = "https://mega.nz/file/test123".to_string();
        app.url_input_active = true;

        handle_input(&mut app, key(KeyCode::Enter));

        assert!(app.url_input.is_empty());
        assert!(!app.url_input_active);
        let received = url_rx.try_recv().unwrap();
        assert_eq!(
            received,
            DownloadRequest::SubmitUrl {
                url: "https://mega.nz/file/test123".to_string()
            }
        );
    }

    #[test]
    fn handle_main_input_empty_url_submit_sets_guidance_status() {
        let mut app = test_app();
        app.url_input = "   ".to_string();
        app.url_input_active = true;

        handle_input(&mut app, key(KeyCode::Enter));

        assert_eq!(app.status, "Enter a URL or press Esc to cancel");
        assert_eq!(app.url_input, "   ");
        assert!(app.url_input_active);
    }

    #[test]
    fn handle_main_input_invalid_url_submit_sets_error_status() {
        let mut app = test_app();
        app.url_input = "not a mega url".to_string();
        app.url_input_active = true;

        handle_input(&mut app, key(KeyCode::Enter));

        assert_eq!(app.status, "No valid URLs found in input");
        assert_eq!(app.url_input, "not a mega url");
        assert!(app.url_input_active);
    }
}
