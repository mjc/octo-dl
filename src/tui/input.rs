//! Keyboard and paste input handling.

mod popup;
mod selection;

#[cfg(test)]
mod tests;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use tui_input::backend::crossterm::to_input_request;
use tui_input::{Input, InputRequest};

use crate::extract_urls;

use self::popup::handle_popup_input;
use self::selection::{
    delete_selected, delete_selected_immediately, move_file_selection, move_selected_queue_item,
    reset_selected, retry_selected, reverify_selected, select_first_file, select_last_file,
    select_next_file, select_previous_file, toggle_selected_package,
};
use super::app::{App, Popup, UiAction};

pub fn handle_input(app: &mut App, key: KeyEvent) {
    // Global quit
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        request_quit(app);
        return;
    }

    if handle_popup_input(app, key) {
        return;
    }

    handle_main_input(app, key);
}

pub(crate) const fn request_quit(app: &mut App) {
    if app.quit_policy.is_enabled() {
        app.should_quit = true;
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
            app.url_input_cursor = app.url_input.chars().count();
        }
        KeyCode::Char('p') => {
            app.handle_ui_action(UiAction::TogglePause);
        }
        KeyCode::Char('r' | 'R') if key.modifiers.contains(KeyModifiers::ALT) => {
            reverify_selected(app);
        }
        KeyCode::Char('D') => delete_selected_immediately(app),
        KeyCode::Char('d') | KeyCode::Delete => delete_selected(app),
        KeyCode::Char('R') => reset_selected(app),
        KeyCode::Char('r') => retry_selected(app),
        KeyCode::Char('c') => {
            app.popup = Popup::Config;
        }
        KeyCode::Char('s') => {
            app.popup = Popup::Sort;
        }
        KeyCode::Char('+') | KeyCode::Char('=') => move_selected_queue_item(app, -1),
        KeyCode::Char('-') => move_selected_queue_item(app, 1),
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
                app.url_input_cursor = 0;
                app.url_input_active = false;
            } else if trimmed.is_empty() {
                app.status = "Enter a URL or press Esc to cancel".to_string();
            } else {
                app.status = "No valid URLs found in input".to_string();
            }
        }
        KeyCode::Esc => {
            app.url_input.clear();
            app.url_input_cursor = 0;
            app.url_input_active = false;
        }
        _ => handle_url_edit_key(app, key),
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
            if !app.url_input_active {
                app.url_input_cursor = app.url_input.chars().count();
            }
            app.url_input_active = true;
            let mut input = url_input_state(app);
            let text = text.replace(['\n', '\r'], " ");
            for c in text.chars() {
                input.handle(InputRequest::InsertChar(c));
            }
            sync_url_input(app, input);
        }
    }
}

fn handle_url_edit_key(app: &mut App, key: KeyEvent) {
    let mut input = url_input_state(app);
    let request = match (key.code, key.modifiers) {
        (KeyCode::Left, modifiers) if modifiers.contains(KeyModifiers::ALT) => {
            Some(InputRequest::GoToPrevWord)
        }
        (KeyCode::Right, modifiers) if modifiers.contains(KeyModifiers::ALT) => {
            Some(InputRequest::GoToNextWord)
        }
        _ => to_input_request(&Event::Key(key)),
    };
    if let Some(request) = request {
        input.handle(request);
        sync_url_input(app, input);
    }
}

fn url_input_state(app: &App) -> Input {
    Input::new(app.url_input.clone()).with_cursor(app.url_input_cursor)
}

fn sync_url_input(app: &mut App, input: Input) {
    app.url_input = input.value().to_string();
    app.url_input_cursor = input.cursor();
}
