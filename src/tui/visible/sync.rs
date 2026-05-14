use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use ratatui::widgets::ListState;

use crate::core::{DownloadState, FileId, FileLifecycle, FileState, PackageId, PackageState};

use super::TuiRow;
use super::rows::visible_rows_for;
use crate::tui::app::{FileEntry, FileStatus, FileUiState, OverlayFile, SortState};

fn project_core_file(
    file: &FileState,
    _package: Option<&PackageState>,
    existing: Option<FileEntry>,
) -> Option<FileEntry> {
    let status = match file.lifecycle {
        FileLifecycle::Planned | FileLifecycle::Queued => FileStatus::Queued,
        FileLifecycle::Downloading => FileStatus::Downloading,
        FileLifecycle::Complete => FileStatus::Complete,
        FileLifecycle::Failed => {
            FileStatus::Error(file.message.clone().unwrap_or_else(|| "failed".to_string()))
        }
    };

    let downloaded = match file.lifecycle {
        FileLifecycle::Complete => file.size,
        _ => crate::core::visible_completed_bytes_for_display(file),
    };
    if let Some(mut existing) = existing {
        existing.name = file.path.clone();
        existing.size = file.size;
        existing.downloaded = downloaded;
        existing.status = status;
        return Some(existing);
    }

    Some(FileEntry {
        id: file.id.clone(),
        name: file.path.clone(),
        size: file.size,
        downloaded,
        status,
    })
}

pub(super) fn seed_overlay_from_visible(
    files: &[FileEntry],
    core_state: &DownloadState,
    deleted_files: &HashSet<FileId>,
    overlay_files: &mut IndexMap<FileId, OverlayFile>,
) {
    for file in files {
        if !core_state.files.contains_key(&file.id) && !deleted_files.contains(&file.id) {
            overlay_files
                .entry(file.id.clone())
                .or_insert_with(|| OverlayFile {
                    file: file.clone(),
                    source_url: None,
                    counts_toward_progress: true,
                });
        }
    }
}

pub(super) fn sync_visible_files(
    files: &mut Vec<FileEntry>,
    visible_file_positions: &mut HashMap<FileId, usize>,
    overlay_files: &mut IndexMap<FileId, OverlayFile>,
    file_ui: &mut HashMap<FileId, FileUiState>,
    file_list_state: &mut ListState,
    core_state: &DownloadState,
    expanded_packages: &HashSet<PackageId>,
    sort: &SortState,
    deleted_files: &HashSet<FileId>,
    selected_row_identity: Option<TuiRow>,
) {
    let selected_row = file_list_state.selected().unwrap_or(0);
    let core_file_ids: HashSet<_> = core_state.files.keys().cloned().collect();
    let existing: IndexMap<_, _> = std::mem::take(files)
        .into_iter()
        .map(|file| (file.id.clone(), file))
        .collect();

    let mut existing = existing;
    for (id, file) in &existing {
        if !core_file_ids.contains(id) && !deleted_files.contains(id) {
            overlay_files
                .entry(id.clone())
                .or_insert_with(|| OverlayFile {
                    file: file.clone(),
                    source_url: None,
                    counts_toward_progress: true,
                });
        }
    }
    let mut next_files = Vec::new();
    for file in core_state.files.values() {
        let package = core_state.packages.get(&file.package_id);
        let existing = existing.shift_remove(&file.id);
        if let Some(entry) = project_core_file(file, package, existing) {
            next_files.push(entry);
        }
    }

    for (id, entry) in overlay_files.iter() {
        if !core_file_ids.contains(id) && !deleted_files.contains(id) {
            next_files.push(entry.file.clone());
        }
    }

    *files = next_files;
    *visible_file_positions = files
        .iter()
        .enumerate()
        .map(|(index, file)| (file.id.clone(), index))
        .collect();
    let visible_ids: HashSet<_> = files.iter().map(|file| file.id.clone()).collect();
    file_ui.retain(|file_id, _| visible_ids.contains(file_id));
    let visible_rows = visible_rows_for(files, core_state, overlay_files, expanded_packages, sort);
    if let Some(selected_row_identity) = selected_row_identity {
        if let Some(display_row) = visible_rows
            .iter()
            .position(|row| *row == selected_row_identity)
        {
            file_list_state.select(Some(display_row));
            return;
        }

        if let Some(display_row) = fallback_selection_row(&selected_row_identity, &visible_rows) {
            file_list_state.select(Some(display_row));
            return;
        }
    }

    if visible_rows.is_empty() {
        file_list_state.select(None);
    } else {
        file_list_state.select(Some(selected_row.min(visible_rows.len().saturating_sub(1))));
    }
}

fn fallback_selection_row(
    selected_row_identity: &TuiRow,
    visible_rows: &[TuiRow],
) -> Option<usize> {
    let TuiRow::File {
        package_id: Some(package_id),
        ..
    } = selected_row_identity
    else {
        return None;
    };

    visible_rows
        .iter()
        .position(|row| matches!(row, TuiRow::Package(id) if id == package_id))
}
