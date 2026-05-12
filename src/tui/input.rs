//! Keyboard and paste input handling.

mod selection;

#[cfg(test)]
mod tests;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::extract_urls;

use self::selection::{
    delete_selected, delete_selected_immediately, move_file_selection, reset_selected,
    retry_selected, select_first_file, select_last_file, select_next_file, select_previous_file,
    toggle_selected_package,
};
use super::app::{App, ConfigField, ConfirmAction, LoginState, Popup, SortKey, UiAction};

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
        KeyCode::Char('r') => retry_selected(app),
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
