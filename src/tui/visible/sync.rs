use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use ratatui::widgets::ListState;

use crate::core::{DownloadState, FileId, FileLifecycle, FileState, PackageId, PackageState};

use super::TuiRow;
use super::rows::{build_file_sort_key, cached_file_sort_key_matches, visible_rows_for};
use crate::tui::app::{FileEntry, FileStatus, FileUiState, SortState, TransientRow};

fn project_core_file(
    file: &FileState,
    package: Option<&PackageState>,
    package_order: usize,
    file_ui: &mut HashMap<FileId, FileUiState>,
    existing: Option<FileEntry>,
) -> Option<FileEntry> {
    let status = match &file.lifecycle {
        FileLifecycle::Planned | FileLifecycle::Queued => FileStatus::Queued,
        FileLifecycle::Downloading => FileStatus::Downloading,
        FileLifecycle::Complete => FileStatus::Complete,
        FileLifecycle::Failed { message } => FileStatus::Error(message.clone()),
    };

    let downloaded = match &file.lifecycle {
        FileLifecycle::Complete => file.size,
        _ => crate::core::visible_completed_bytes_for_display(file),
    };
    let reuse_cached_sort_key = existing.as_ref().is_some_and(|existing| {
        existing.name == file.path
            && existing.status == status
            && file_ui
                .get(&file.id)
                .and_then(|ui| ui.sort_key.as_ref())
                .is_some_and(|sort_key| cached_file_sort_key_matches(sort_key, package_order, ""))
    });
    if !reuse_cached_sort_key {
        let state = file_ui.entry(file.id.clone()).or_default();
        state.sort_key = Some(build_file_sort_key(
            &file.path,
            &status,
            package_order,
            package.map_or("", |_| ""),
        ));
        state.package_id = Some(file.package_id);
    } else if let Some(state) = file_ui.get_mut(&file.id) {
        state.package_id = Some(file.package_id);
    }
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

pub(super) fn sync_visible_files(
    files: &mut Vec<FileEntry>,
    visible_file_positions: &mut HashMap<FileId, usize>,
    overlay_files: &mut IndexMap<FileId, TransientRow>,
    file_ui: &mut HashMap<FileId, FileUiState>,
    file_list_state: &mut ListState,
    core_state: &DownloadState,
    expanded_packages: &HashSet<PackageId>,
    sort: &SortState,
    selected_row_identity: Option<TuiRow>,
) -> Vec<TuiRow> {
    let selected_row = file_list_state.selected().unwrap_or(0);
    let core_file_ids: HashSet<_> = core_state.files.keys().cloned().collect();
    let existing: HashMap<_, _> = std::mem::take(files)
        .into_iter()
        .map(|file| (file.id.clone(), file))
        .collect();

    let mut existing = existing;
    let mut next_files = Vec::new();
    for file in core_state.files.values() {
        let package = core_state.packages.get(&file.package_id);
        let package_order = core_state
            .packages
            .get_index_of(&file.package_id)
            .unwrap_or(usize::MAX);
        let existing = existing.remove(&file.id);
        if let Some(entry) = project_core_file(file, package, package_order, file_ui, existing) {
            next_files.push(entry);
        }
    }

    for (id, entry) in overlay_files.iter() {
        if !core_file_ids.contains(id) {
            let source_url = entry.source_url().unwrap_or(id.as_str());
            let existing = existing.remove(id);
            let reuse_cached_sort_key = existing.as_ref().is_some_and(|existing| {
                existing.name == entry.file().name
                    && existing.status == entry.file().status
                    && file_ui
                        .get(id)
                        .and_then(|ui| ui.sort_key.as_ref())
                        .is_some_and(|sort_key| {
                            cached_file_sort_key_matches(sort_key, usize::MAX, source_url)
                        })
            });
            let state = file_ui.entry(id.clone()).or_default();
            if !reuse_cached_sort_key {
                state.sort_key = Some(build_file_sort_key(
                    &entry.file().name,
                    &entry.file().status,
                    usize::MAX,
                    source_url,
                ));
            }
            state.package_id = None;
            if let Some(mut existing) = existing {
                existing.name = entry.file().name.clone();
                existing.size = entry.file().size;
                existing.downloaded = entry.file().downloaded;
                existing.status = entry.file().status.clone();
                next_files.push(existing);
            } else {
                next_files.push(entry.file().clone());
            }
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
    let visible_rows = visible_rows_for(
        files,
        file_ui,
        core_state,
        overlay_files,
        expanded_packages,
        sort,
    );
    if let Some(selected_row_identity) = selected_row_identity {
        if let Some(display_row) = visible_rows
            .iter()
            .position(|row| *row == selected_row_identity)
        {
            file_list_state.select(Some(display_row));
            return visible_rows;
        }

        if let Some(display_row) = fallback_selection_row(&selected_row_identity, &visible_rows) {
            file_list_state.select(Some(display_row));
            return visible_rows;
        }
    }

    if visible_rows.is_empty() {
        file_list_state.select(None);
    } else {
        file_list_state.select(Some(selected_row.min(visible_rows.len().saturating_sub(1))));
    }
    visible_rows
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
