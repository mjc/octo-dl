use crossterm::event::{KeyCode, KeyEvent};

use crate::tui::app::{App, ConfigField, ConfirmAction, LoginState, Popup, SortKey, UiAction};

pub(super) fn handle_popup_input(app: &mut App, key: KeyEvent) -> bool {
    match app.popup {
        Popup::Login => {
            handle_login_input(app, key);
            true
        }
        Popup::Config => {
            handle_config_input(app, key);
            true
        }
        Popup::Confirm => {
            handle_confirm_input(app, key);
            true
        }
        Popup::Sort => {
            handle_sort_input(app, key);
            true
        }
        Popup::None => false,
    }
}

fn handle_login_input(app: &mut App, key: KeyEvent) {
    if app.login.logging_in {
        if key.code == KeyCode::Esc {
            super::request_quit(app);
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
            super::request_quit(app);
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
                    mega_chunks_per_request: None,
                    concurrent_files: None,
                    force_overwrite: None,
                    cleanup_on_error: None,
                }),
                ConfigField::MegaChunksPerRequest => app.handle_ui_action(UiAction::UpdateConfig {
                    chunks_per_file: None,
                    mega_chunks_per_request: Some(
                        app.config.config.mega_chunks_per_request.saturating_add(1),
                    ),
                    concurrent_files: None,
                    force_overwrite: None,
                    cleanup_on_error: None,
                }),
                ConfigField::ConcurrentFiles => app.handle_ui_action(UiAction::UpdateConfig {
                    chunks_per_file: None,
                    mega_chunks_per_request: None,
                    concurrent_files: Some(app.config.config.concurrent_files.saturating_add(1)),
                    force_overwrite: None,
                    cleanup_on_error: None,
                }),
                ConfigField::ForceOverwrite => app.handle_ui_action(UiAction::UpdateConfig {
                    chunks_per_file: None,
                    mega_chunks_per_request: None,
                    concurrent_files: None,
                    force_overwrite: Some(!app.config.config.force_overwrite),
                    cleanup_on_error: None,
                }),
                ConfigField::CleanupOnError => app.handle_ui_action(UiAction::UpdateConfig {
                    chunks_per_file: None,
                    mega_chunks_per_request: None,
                    concurrent_files: None,
                    force_overwrite: None,
                    cleanup_on_error: Some(!app.config.config.cleanup_on_error),
                }),
            }
        }
        KeyCode::Char('-') | KeyCode::Left => match ConfigField::ALL[app.config.active_field] {
            ConfigField::ChunksPerFile => app.handle_ui_action(UiAction::UpdateConfig {
                chunks_per_file: Some(app.config.config.chunks_per_file.saturating_sub(1).max(1)),
                mega_chunks_per_request: None,
                concurrent_files: None,
                force_overwrite: None,
                cleanup_on_error: None,
            }),
            ConfigField::MegaChunksPerRequest => app.handle_ui_action(UiAction::UpdateConfig {
                chunks_per_file: None,
                mega_chunks_per_request: Some(
                    app.config
                        .config
                        .mega_chunks_per_request
                        .saturating_sub(1)
                        .max(1),
                ),
                concurrent_files: None,
                force_overwrite: None,
                cleanup_on_error: None,
            }),
            ConfigField::ConcurrentFiles => app.handle_ui_action(UiAction::UpdateConfig {
                chunks_per_file: None,
                mega_chunks_per_request: None,
                concurrent_files: Some(app.config.config.concurrent_files.saturating_sub(1).max(1)),
                force_overwrite: None,
                cleanup_on_error: None,
            }),
            ConfigField::ForceOverwrite => app.handle_ui_action(UiAction::UpdateConfig {
                chunks_per_file: None,
                mega_chunks_per_request: None,
                concurrent_files: None,
                force_overwrite: Some(!app.config.config.force_overwrite),
                cleanup_on_error: None,
            }),
            ConfigField::CleanupOnError => app.handle_ui_action(UiAction::UpdateConfig {
                chunks_per_file: None,
                mega_chunks_per_request: None,
                concurrent_files: None,
                force_overwrite: None,
                cleanup_on_error: Some(!app.config.config.cleanup_on_error),
            }),
        },
        KeyCode::Char(' ') => match ConfigField::ALL[app.config.active_field] {
            ConfigField::ForceOverwrite => app.handle_ui_action(UiAction::UpdateConfig {
                chunks_per_file: None,
                mega_chunks_per_request: None,
                concurrent_files: None,
                force_overwrite: Some(!app.config.config.force_overwrite),
                cleanup_on_error: None,
            }),
            ConfigField::CleanupOnError => app.handle_ui_action(UiAction::UpdateConfig {
                chunks_per_file: None,
                mega_chunks_per_request: None,
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
