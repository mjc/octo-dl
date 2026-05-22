use crate::tui::{
    app::{App, ConfirmAction, FileStatus, Popup, UiAction},
    visible::TuiRow,
};

pub(super) fn retry_selected(app: &mut App) {
    match app.selected_row() {
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
    }
}

pub(super) fn reverify_selected(app: &mut App) {
    match app.selected_row() {
        Some(TuiRow::Package(package_id)) => {
            app.handle_ui_action(UiAction::ReverifyPackage(package_id));
        }
        Some(TuiRow::File { file_id, .. }) => {
            app.handle_ui_action(UiAction::ReverifyFile(file_id));
        }
        None => {}
    }
}

pub(super) fn select_previous_file(app: &mut App) {
    let len = app.visible_rows().len();
    if len > 0 {
        let i = app.file_list_state.selected().unwrap_or(0);
        app.file_list_state
            .select(Some(if i == 0 { len - 1 } else { i - 1 }));
    }
}

pub(super) fn select_next_file(app: &mut App) {
    let len = app.visible_rows().len();
    if len > 0 {
        let i = app.file_list_state.selected().unwrap_or(0);
        app.file_list_state.select(Some((i + 1) % len));
    }
}

pub(super) fn move_file_selection(app: &mut App, delta: isize) {
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

pub(super) fn select_first_file(app: &mut App) {
    if !app.visible_rows().is_empty() {
        app.file_list_state.select(Some(0));
    }
}

pub(super) fn select_last_file(app: &mut App) {
    let len = app.visible_rows().len();
    if len > 0 {
        app.file_list_state.select(Some(len - 1));
    }
}

pub(super) fn reset_selected(app: &mut App) {
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

pub(super) fn delete_selected(app: &mut App) {
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

pub(super) fn delete_selected_immediately(app: &mut App) {
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

pub(super) fn toggle_selected_package(app: &mut App) {
    if let Some(TuiRow::Package(package_id)) = app.selected_row() {
        if !app.expanded_packages.insert(package_id) {
            app.expanded_packages.remove(&package_id);
        }
        app.sync_visible_files();
    }
}

pub(super) fn move_selected_queue_item(app: &mut App, delta: isize) {
    let selected = app.selected_row();
    if !matches!(app.sort.key, crate::tui::app::SortKey::Queue) {
        app.sort.key = crate::tui::app::SortKey::Queue;
        app.sync_visible_files_preserving(selected.clone());
    }
    match selected {
        Some(TuiRow::Package(package_id)) => {
            app.handle_ui_action(UiAction::MovePackage { package_id, delta });
        }
        Some(TuiRow::File {
            package_id: Some(_),
            file_id,
        }) => {
            app.handle_ui_action(UiAction::MoveFile { file_id, delta });
        }
        _ => {}
    }
}
