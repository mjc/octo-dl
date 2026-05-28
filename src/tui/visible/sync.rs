use indexmap::IndexMap;
use ratatui::widgets::ListState;

use crate::core::{DownloadState, FileId, FileLifecycle, FileState};

use super::TuiRow;
use super::rows::{build_file_sort_key, cached_file_sort_key_matches, visible_rows_for};
use crate::tui::app::{
    ExpandedPackages, FileEntry, FileIdSet, FileStatus, FileUiMap, SortState, TransientRow,
    VisibleFilePositions,
};

fn project_core_file(
    file: &FileState,
    package_order: usize,
    file_ui: &mut FileUiMap,
    existing: Option<FileEntry>,
) -> Option<FileEntry> {
    let status = match &file.lifecycle {
        FileLifecycle::Planned | FileLifecycle::Queued => FileStatus::Queued,
        FileLifecycle::Downloading => FileStatus::Downloading,
        FileLifecycle::Complete => FileStatus::Complete,
        FileLifecycle::Failed { message } => FileStatus::Error(message.clone()),
    };
    let downloading = matches!(status, FileStatus::Downloading);

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
        state.sort_key = Some(build_file_sort_key(&file.path, &status, package_order, ""));
        state.package_id = Some(file.package_id);
        if !downloading {
            state.speed = 0;
        }
    } else if let Some(state) = file_ui.get_mut(&file.id) {
        state.package_id = Some(file.package_id);
        if !downloading {
            state.speed = 0;
        }
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
    visible_file_positions: &mut VisibleFilePositions,
    overlay_files: &mut IndexMap<FileId, TransientRow>,
    file_ui: &mut FileUiMap,
    file_list_state: &mut ListState,
    core_state: &DownloadState,
    expanded_packages: &ExpandedPackages,
    sort: &SortState,
    selected_row_identity: Option<TuiRow>,
) -> Vec<TuiRow> {
    let selected_row = file_list_state.selected().unwrap_or(0);
    let mut existing_files = std::mem::take(files)
        .into_iter()
        .map(Some)
        .collect::<Vec<_>>();
    let mut next_files = Vec::new();
    next_files.reserve(core_state.files.len().saturating_add(overlay_files.len()));
    let mut remaining_files = core_state.files.values().peekable();
    for (package_order, package) in core_state.packages.values().enumerate() {
        while remaining_files
            .peek()
            .is_some_and(|file| file.package_id == package.id)
        {
            let file = remaining_files
                .next()
                .expect("peeked file should remain available");
            let existing = visible_file_positions
                .get(&file.id)
                .copied()
                .and_then(|index| existing_files.get_mut(index))
                .and_then(Option::take);
            if let Some(entry) = project_core_file(file, package_order, file_ui, existing) {
                next_files.push(entry);
            }
        }
    }
    debug_assert!(
        remaining_files.next().is_none(),
        "visible sync expects files grouped in package order"
    );

    for (id, entry) in overlay_files.iter() {
        if !core_state.files.contains_key(id) {
            let source_url = entry.source_url().unwrap_or(id.as_str());
            let existing = visible_file_positions
                .get(id)
                .copied()
                .and_then(|index| existing_files.get_mut(index))
                .and_then(Option::take);
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
            if !matches!(entry.file().status, FileStatus::Downloading) {
                state.speed = 0;
            }
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
    visible_file_positions.clear();
    visible_file_positions.reserve(files.len());
    for (index, file) in files.iter().enumerate() {
        visible_file_positions.insert(file.id.clone(), index);
    }
    let visible_ids: FileIdSet = files.iter().map(|file| file.id.clone()).collect();
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
