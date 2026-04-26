use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use ratatui::widgets::ListState;

use crate::core::{DownloadState, FileLifecycle, FileState, PackageState};

use super::app::{FileEntry, FileStatus, FileUiState, OverlayFile};

fn package_sort_key_for(
    core_state: &DownloadState,
    overlay_files: &IndexMap<String, OverlayFile>,
    file: &FileEntry,
) -> (usize, String) {
    if let Some(core_file) = core_state.files.get(&file.id) {
        let package_order = core_state
            .packages
            .get_index_of(&core_file.package_id)
            .unwrap_or(usize::MAX);
        let display_name = core_state
            .packages
            .get(&core_file.package_id)
            .map(|package| package.display_name.clone())
            .unwrap_or_else(|| core_file.package_id.clone());
        return (package_order, display_name);
    }

    (
        usize::MAX,
        overlay_files
            .get(&file.id)
            .and_then(|file| file.source_url.clone())
            .unwrap_or_else(|| file.id.clone()),
    )
}

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
        FileLifecycle::Skipped | FileLifecycle::Deleted => return None,
    };

    let downloaded = match file.lifecycle {
        FileLifecycle::Complete => file.size,
        _ => file.progress.visible_completed_bytes.min(file.size),
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

pub(super) fn sorted_file_indices(
    files: &[FileEntry],
    core_state: &DownloadState,
    overlay_files: &IndexMap<String, OverlayFile>,
) -> Vec<usize> {
    let mut indices: Vec<_> = (0..files.len()).collect();
    indices.sort_by(|&left, &right| {
        let left_file = &files[left];
        let right_file = &files[right];
        let left_package = package_sort_key_for(core_state, overlay_files, left_file);
        let right_package = package_sort_key_for(core_state, overlay_files, right_file);

        match left_package.cmp(&right_package) {
            Ordering::Equal => {}
            other => return other,
        }

        let left_rank = match &left_file.status {
            FileStatus::Downloading => 0,
            FileStatus::Queued => 1,
            FileStatus::Complete => 2,
            FileStatus::Error(_) => 3,
        };
        let right_rank = match &right_file.status {
            FileStatus::Downloading => 0,
            FileStatus::Queued => 1,
            FileStatus::Complete => 2,
            FileStatus::Error(_) => 3,
        };
        left_rank
            .cmp(&right_rank)
            .then_with(|| left_file.name.cmp(&right_file.name))
            .then_with(|| left_file.id.cmp(&right_file.id))
    });
    indices
}

pub(super) fn selected_file_index(
    file_list_state: &ListState,
    files: &[FileEntry],
    core_state: &DownloadState,
    overlay_files: &IndexMap<String, OverlayFile>,
) -> Option<usize> {
    let selected = file_list_state.selected()?;
    sorted_file_indices(files, core_state, overlay_files)
        .get(selected)
        .copied()
}

pub(super) fn seed_overlay_from_visible(
    files: &[FileEntry],
    core_state: &DownloadState,
    deleted_files: &HashSet<String>,
    overlay_files: &mut IndexMap<String, OverlayFile>,
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
    overlay_files: &mut IndexMap<String, OverlayFile>,
    file_ui: &mut HashMap<String, FileUiState>,
    file_list_state: &mut ListState,
    core_state: &DownloadState,
    deleted_files: &HashSet<String>,
) {
    let selected_id = selected_file_index(file_list_state, files, core_state, overlay_files)
        .map(|index| files[index].id.clone());
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
    let visible_ids: HashSet<_> = files.iter().map(|file| file.id.clone()).collect();
    file_ui.retain(|file_id, _| visible_ids.contains(file_id));
    if let Some(selected_id) = selected_id {
        if let Some(display_row) = sorted_file_indices(files, core_state, overlay_files)
            .into_iter()
            .position(|index| files[index].id == selected_id)
        {
            file_list_state.select(Some(display_row));
            return;
        }
    }

    if files.is_empty() {
        file_list_state.select(None);
    } else {
        file_list_state.select(Some(selected_row.min(files.len() - 1)));
    }
}
