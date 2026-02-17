//! Keyboard and paste input handling.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::extract_urls;

use super::app::{App, ConfigField, FileStatus, LoginState, Popup};
use super::download::start_login;

pub fn handle_input(app: &mut App, key: KeyEvent) {
    // Global quit
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.should_quit = true;
        return;
    }

    match app.popup {
        Popup::Login => handle_login_input(app, key),
        Popup::Config => handle_config_input(app, key),
        Popup::None => handle_main_input(app, key),
    }
}

fn handle_login_input(app: &mut App, key: KeyEvent) {
    if app.login.logging_in {
        // Don't accept input while logging in (except Esc to quit)
        if key.code == KeyCode::Esc {
            app.should_quit = true;
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
            if app.login.email.is_empty() || app.login.password.is_empty() {
                app.login.error = Some("Email and password are required".to_string());
            } else {
                app.login.error = None;
                app.login.logging_in = true;
                start_login(app);
            }
        }
        KeyCode::Char(c) => {
            app.login.active_value_mut().push(c);
        }
        KeyCode::Backspace => {
            app.login.active_value_mut().pop();
        }
        KeyCode::Esc => {
            app.should_quit = true;
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
                ConfigField::ChunksPerFile => {
                    app.config.config.chunks_per_file =
                        app.config.config.chunks_per_file.saturating_add(1);
                }
                ConfigField::ConcurrentFiles => {
                    app.config.config.concurrent_files =
                        app.config.config.concurrent_files.saturating_add(1);
                }
                ConfigField::ForceOverwrite => {
                    app.config.config.force_overwrite = !app.config.config.force_overwrite;
                }
                ConfigField::CleanupOnError => {
                    app.config.config.cleanup_on_error = !app.config.config.cleanup_on_error;
                }
            }
        }
        KeyCode::Char('-') | KeyCode::Left => match ConfigField::ALL[app.config.active_field] {
            ConfigField::ChunksPerFile => {
                app.config.config.chunks_per_file =
                    app.config.config.chunks_per_file.saturating_sub(1).max(1);
            }
            ConfigField::ConcurrentFiles => {
                app.config.config.concurrent_files =
                    app.config.config.concurrent_files.saturating_sub(1).max(1);
            }
            ConfigField::ForceOverwrite => {
                app.config.config.force_overwrite = !app.config.config.force_overwrite;
            }
            ConfigField::CleanupOnError => {
                app.config.config.cleanup_on_error = !app.config.config.cleanup_on_error;
            }
        },
        KeyCode::Char(' ') => match ConfigField::ALL[app.config.active_field] {
            ConfigField::ForceOverwrite => {
                app.config.config.force_overwrite = !app.config.config.force_overwrite;
            }
            ConfigField::CleanupOnError => {
                app.config.config.cleanup_on_error = !app.config.config.cleanup_on_error;
            }
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
                for url in extracted {
                    add_url(app, url);
                }
                app.url_input.clear();
            }
        }
        KeyCode::Char('p') if app.url_input.is_empty() => {
            app.paused = !app.paused;
        }
        KeyCode::Char('d') | KeyCode::Delete if app.url_input.is_empty() => {
            if let Some(selected) = app.file_list_state.selected()
                && selected < app.files.len()
            {
                let file = &app.files[selected];
                let can_remove = matches!(
                    file.status,
                    FileStatus::Queued | FileStatus::Error(_) | FileStatus::Downloading
                );
                if can_remove {
                    let file_name = file.name.clone();
                    // Cancel the download if active
                    if matches!(file.status, FileStatus::Downloading)
                        && let Some(token) = app.cancellation_tokens.remove(&file_name)
                    {
                        token.cancel();
                    }
                    // Track so we can ignore stale events
                    app.deleted_files.insert(file_name.clone());
                    app.files.remove(selected);
                    app.recompute_totals();
                    // Remove from session state
                    if let Some(ref mut session) = app.session {
                        let _ = session.remove_file(&file_name);
                    }
                    if app.files.is_empty() {
                        app.file_list_state.select(None);
                    } else {
                        app.file_list_state
                            .select(Some(selected.min(app.files.len() - 1)));
                    }
                }
            }
        }
        KeyCode::Char('r') if app.url_input.is_empty() => {
            // Retry selected errored file — re-queue it
            if let Some(selected) = app.file_list_state.selected()
                && selected < app.files.len()
                && matches!(app.files[selected].status, FileStatus::Error(_))
            {
                app.files[selected].status = FileStatus::Queued;
                app.files[selected].downloaded = 0;
                app.files[selected].speed = 0;
                // TODO: actually re-submit to download task
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
            app.should_quit = true;
        }
        KeyCode::Esc => {
            if app.url_input.is_empty() {
                app.should_quit = true;
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

/// Adds a URL and sends it to the download task if authenticated.
pub fn add_url(app: &mut App, url: String) {
    if !app.urls.contains(&url) {
        app.urls.push(url.clone());
    }
    // Persist the URL in the session so it survives restarts
    if let Some(ref mut session) = app.session
        && !session.urls.iter().any(|u| u.url == url)
    {
        session.urls.push(crate::UrlEntry {
            url: url.clone(),
            status: crate::UrlStatus::Pending,
        });
        let _ = session.save();
    }
    if let Some(ref url_tx) = app.url_tx {
        let _ = url_tx.send(url);
    }
}

#[cfg(test)]
mod tests {
    use super::super::app::{App, FileEntry, FileStatus, Popup};
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use tokio::sync::mpsc;

    fn test_app() -> App {
        let (tx, _rx) = mpsc::unbounded_channel();
        App::new(9723, tx)
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
            name: "test.zip".to_string(),
            size: 1000,
            downloaded: 500,
            speed: 100,
            speed_accum: 0,
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
    fn handle_login_input_validates_empty() {
        let mut app = test_app();
        app.popup = Popup::Login;
        app.login.email.clear();
        app.login.password.clear();
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
        app.login.email.clear();
        handle_paste(&mut app, "  user@example.com  ");
        assert_eq!(app.login.email, "user@example.com");
    }

    #[test]
    fn add_url_deduplicates() {
        let mut app = test_app();
        add_url(&mut app, "https://mega.nz/file/abc".to_string());
        add_url(&mut app, "https://mega.nz/file/abc".to_string());
        assert_eq!(app.urls.len(), 1);
    }

    #[test]
    fn handle_main_input_url_submit() {
        let mut app = test_app();
        let (url_tx, mut url_rx) = mpsc::unbounded_channel();
        app.url_tx = Some(url_tx);
        app.url_input = "https://mega.nz/file/test123".to_string();

        handle_input(&mut app, key(KeyCode::Enter));

        assert!(app.url_input.is_empty());
        let received = url_rx.try_recv().unwrap();
        assert_eq!(received, "https://mega.nz/file/test123");
    }
}
