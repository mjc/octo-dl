//! Keyboard and paste input handling.

mod popup;
mod selection;

#[cfg(test)]
mod tests;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::widgets::{Block, Borders};
use tui_input::backend::crossterm::to_input_request;
use tui_input::{Input, InputRequest};

use crate::{extract_urls, url::DownloadSource};

use self::popup::handle_popup_input;
use self::selection::{
    delete_selected, delete_selected_immediately, move_file_selection, move_selected_queue_item,
    reset_selected, retry_selected, reverify_selected, select_first_file, select_last_file,
    select_next_file, select_previous_file, toggle_selected_package,
};
use super::app::{App, Popup, UiAction};

pub fn handle_input(app: &mut App, key: KeyEvent) {
    if key.kind == KeyEventKind::Release {
        return;
    }

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

pub(super) fn handle_event(app: &mut App, event: Event, terminal_area: Rect) {
    match event {
        Event::Key(key) => handle_input(app, key),
        Event::Paste(text) => handle_paste(app, &text),
        Event::Mouse(mouse) => handle_mouse(app, mouse, terminal_area),
        _ => {}
    }
}

fn handle_mouse(app: &mut App, mouse: MouseEvent, terminal_area: Rect) {
    // Popups and URL editing have their own keyboard-only interaction model.
    // Ignoring mouse input here avoids accidentally applying a main-dashboard
    // action to a control that is visually covered by a popup.
    if app.popup != Popup::None || app.url_input_active {
        return;
    }

    let Some(row) = list_row_at(app, mouse.column, mouse.row, terminal_area) else {
        return;
    };

    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => app.file_list_state.select(Some(row)),
        MouseEventKind::ScrollUp => select_previous_file(app),
        MouseEventKind::ScrollDown => select_next_file(app),
        _ => {}
    }
}

fn list_row_at(app: &App, column: u16, row: u16, terminal_area: Rect) -> Option<usize> {
    let rows = app.visible_rows();
    if rows.is_empty() {
        return None;
    }

    let outer = Block::default().borders(Borders::ALL);
    let inner = outer.inner(terminal_area);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);
    let list_inner = Block::default().borders(Borders::ALL).inner(chunks[1]);

    if !list_inner.contains(Position::new(column, row)) {
        return None;
    }

    let index = app
        .file_list_state
        .offset()
        .saturating_add(usize::from(row.saturating_sub(list_inner.y)));
    (index < rows.len()).then_some(index)
}

pub(super) const fn request_quit(app: &mut App) {
    if app.quit_policy.is_enabled() {
        app.should_quit = true;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DownloadKeyCommand {
    Retry,
    Reset,
    Reverify,
}

pub(super) const fn decode_download_key(
    code: KeyCode,
    modifiers: KeyModifiers,
) -> Option<DownloadKeyCommand> {
    match (code, modifiers.contains(KeyModifiers::ALT)) {
        (KeyCode::Char('r' | 'R'), true) => Some(DownloadKeyCommand::Reverify),
        (KeyCode::Char('r'), false) => Some(DownloadKeyCommand::Retry),
        (KeyCode::Char('R'), false) => Some(DownloadKeyCommand::Reset),
        _ => None,
    }
}

fn handle_main_input(app: &mut App, key: KeyEvent) {
    if app.url_input_active {
        handle_url_input(app, key);
        return;
    }

    if let Some(command) = decode_download_key(key.code, key.modifiers) {
        match command {
            DownloadKeyCommand::Retry => retry_selected(app),
            DownloadKeyCommand::Reset => reset_selected(app),
            DownloadKeyCommand::Reverify => reverify_selected(app),
        }
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
        KeyCode::Char('D') => delete_selected_immediately(app),
        KeyCode::Char('d') | KeyCode::Delete => delete_selected(app),
        KeyCode::Char('c') => {
            app.popup = Popup::Config;
        }
        KeyCode::Char('s') => {
            app.popup = Popup::Sort;
        }
        KeyCode::Char('+' | '=') => move_selected_queue_item(app, -1),
        KeyCode::Char('-') => move_selected_queue_item(app, 1),
        KeyCode::Enter | KeyCode::Char(' ') => toggle_selected_package(app),
        KeyCode::Up | KeyCode::Char('k') => select_previous_file(app),
        KeyCode::Down | KeyCode::Char('j') => select_next_file(app),
        KeyCode::PageUp => move_file_selection(app, -10),
        KeyCode::PageDown => move_file_selection(app, 10),
        KeyCode::Home | KeyCode::Char('g') => select_first_file(app),
        KeyCode::End | KeyCode::Char('G') => select_last_file(app),
        KeyCode::Char('q') | KeyCode::Esc => request_quit(app),
        _ => {}
    }
}

fn handle_url_input(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => {
            let trimmed = app.url_input.trim();
            let extracted = extract_urls(trimmed)
                .into_iter()
                .filter_map(|candidate| {
                    DownloadSource::parse(&candidate)
                        .ok()
                        .map(DownloadSource::into_string)
                })
                .collect::<Vec<_>>();
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
            sync_url_input(app, &input);
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
        sync_url_input(app, &input);
    }
}

fn url_input_state(app: &App) -> Input {
    Input::new(app.url_input.clone()).with_cursor(app.url_input_cursor)
}

fn sync_url_input(app: &mut App, input: &Input) {
    app.url_input = input.value().to_string();
    app.url_input_cursor = input.cursor();
}
