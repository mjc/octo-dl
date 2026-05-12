mod rows;
mod sync;

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use ratatui::widgets::ListState;

use super::app::App;
use crate::core::DownloadState;
use crate::tui::app::{FileEntry, FileUiState, OverlayFile, SortState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TuiRow {
    Package(String),
    File { package_id: String, file_id: String },
}

pub(super) fn visible_rows(app: &App) -> Vec<TuiRow> {
    rows::visible_rows_for(
        &app.files,
        &app.core_state,
        &app.overlay_files,
        &app.expanded_packages,
        &app.sort,
    )
}

#[cfg(test)]
pub(super) fn sorted_file_indices(
    files: &[FileEntry],
    core_state: &DownloadState,
    overlay_files: &IndexMap<String, OverlayFile>,
) -> Vec<usize> {
    rows::sorted_file_indices(files, core_state, overlay_files)
}

pub(super) fn seed_overlay_from_visible(
    files: &[FileEntry],
    core_state: &DownloadState,
    deleted_files: &HashSet<String>,
    overlay_files: &mut IndexMap<String, OverlayFile>,
) {
    sync::seed_overlay_from_visible(files, core_state, deleted_files, overlay_files);
}

pub(super) fn sync_visible_files(
    files: &mut Vec<FileEntry>,
    overlay_files: &mut IndexMap<String, OverlayFile>,
    file_ui: &mut HashMap<String, FileUiState>,
    file_list_state: &mut ListState,
    core_state: &DownloadState,
    expanded_packages: &HashSet<String>,
    sort: &SortState,
    deleted_files: &HashSet<String>,
    selected_row_identity: Option<TuiRow>,
) {
    sync::sync_visible_files(
        files,
        overlay_files,
        file_ui,
        file_list_state,
        core_state,
        expanded_packages,
        sort,
        deleted_files,
        selected_row_identity,
    );
}
