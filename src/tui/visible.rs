mod rows;
mod sync;

use indexmap::IndexMap;
use ratatui::widgets::ListState;

use super::app::App;
use crate::core::{DownloadState, FileId, PackageId};
use crate::tui::app::{
    ExpandedPackages, FileEntry, FileUiMap, SortState, TransientRow, VisibleFilePositions,
};

pub(crate) use rows::CachedFileSortKey;
#[cfg(test)]
pub(crate) use rows::{
    build_file_sort_key_call_count, reset_build_file_sort_key_call_count,
    reset_visible_rows_for_call_count, visible_rows_for_call_count,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TuiRow {
    Package(PackageId),
    File {
        package_id: Option<PackageId>,
        file_id: FileId,
    },
}

pub(super) fn visible_rows(app: &App) -> Vec<TuiRow> {
    rows::visible_rows_for(
        &app.files,
        &app.file_ui,
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
    overlay_files: &IndexMap<FileId, TransientRow>,
) -> Vec<usize> {
    rows::sorted_file_indices(files, core_state, overlay_files)
}

pub(super) fn sync_visible_files(
    files: &mut Vec<FileEntry>,
    visible_file_positions: &mut VisibleFilePositions,
    overlay_files: &mut IndexMap<FileId, TransientRow>,
    file_ui: &mut FileUiMap,
    file_list_state: &mut ListState,
    core_state: &DownloadState,
    expanded_packages: &ExpandedPackages,
    sort: &SortState,
    selected_row_identity: Option<TuiRow>,
) -> Vec<TuiRow> {
    sync::sync_visible_files(
        files,
        visible_file_positions,
        overlay_files,
        file_ui,
        file_list_state,
        core_state,
        expanded_packages,
        sort,
        selected_row_identity,
    )
}
