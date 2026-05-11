use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use ratatui::widgets::ListState;

use crate::core::{DownloadState, FileLifecycle, FileState, PackageState, PackageStatus};

use super::app::{App, FileEntry, FileStatus, FileUiState, OverlayFile, SortDirection, SortKey};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TuiRow {
    Package(String),
    File { package_id: String, file_id: String },
}

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

fn file_name_for_sort(core_state: &DownloadState, file_id: &str) -> String {
    core_state
        .files
        .get(file_id)
        .map(|file| file.path.clone())
        .unwrap_or_else(|| file_id.to_string())
}

fn package_percent(core_state: &DownloadState, package_id: &str) -> u64 {
    let Some(package) = core_state.packages.get(package_id) else {
        return 0;
    };
    let (downloaded, size) = package
        .file_ids
        .iter()
        .filter_map(|file_id| core_state.files.get(file_id))
        .fold((0_u64, 0_u64), |(downloaded, size), file| {
            let visible = if matches!(file.lifecycle, FileLifecycle::Complete) {
                file.size
            } else {
                file.progress.visible_completed_bytes.min(file.size)
            };
            (
                downloaded.saturating_add(visible),
                size.saturating_add(file.size),
            )
        });
    if size == 0 {
        0
    } else {
        downloaded.saturating_mul(100).saturating_div(size).min(100)
    }
}

fn package_status_rank(status: PackageStatus) -> u8 {
    match status {
        PackageStatus::Downloading => 0,
        PackageStatus::Failed => 1,
        PackageStatus::Queued | PackageStatus::Pending => 2,
        PackageStatus::Partial => 3,
        PackageStatus::Complete => 4,
        PackageStatus::Skipped | PackageStatus::Deleted => 5,
    }
}

fn package_is_auto_expanded_for(
    expanded_packages: &HashSet<String>,
    core_state: &DownloadState,
    package_id: &str,
) -> bool {
    expanded_packages.contains(package_id)
        || core_state
            .packages
            .get(package_id)
            .is_some_and(|package| matches!(package.status, PackageStatus::Failed))
}

fn file_is_visible_in_package(core_state: &DownloadState, file_id: &str) -> bool {
    core_state.files.get(file_id).is_some_and(|file| {
        !matches!(
            file.lifecycle,
            FileLifecycle::Skipped | FileLifecycle::Deleted
        )
    })
}

fn overlay_row_is_hidden_placeholder(file: &FileEntry, overlay: Option<&OverlayFile>) -> bool {
    matches!(file.status, FileStatus::Queued)
        && file.size == 0
        && overlay
            .is_some_and(|overlay| overlay.source_url.is_none() && !overlay.counts_toward_progress)
}

fn package_has_visible_content(
    core_state: &DownloadState,
    overlay_files: &IndexMap<String, OverlayFile>,
    package_id: &str,
) -> bool {
    let Some(package) = core_state.packages.get(package_id) else {
        return false;
    };

    package
        .file_ids
        .iter()
        .any(|file_id| file_is_visible_in_package(core_state, file_id))
        || (package.error.is_some() && !overlay_files.contains_key(package_id))
}

fn package_has_visible_files(core_state: &DownloadState, package_id: &str) -> bool {
    core_state.packages.get(package_id).is_some_and(|package| {
        package
            .file_ids
            .iter()
            .any(|file_id| file_is_visible_in_package(core_state, file_id))
    })
}

fn package_has_visible_children(core_state: &DownloadState, package_id: &str) -> bool {
    package_has_visible_files(core_state, package_id)
}

fn visible_rows_for(
    files: &[FileEntry],
    core_state: &DownloadState,
    overlay_files: &IndexMap<String, OverlayFile>,
    expanded_packages: &HashSet<String>,
    sort: &super::app::SortState,
) -> Vec<TuiRow> {
    if core_state.packages.is_empty() {
        return sorted_file_indices(files, core_state, overlay_files)
            .into_iter()
            .map(|index| TuiRow::File {
                package_id: String::new(),
                file_id: files[index].id.clone(),
            })
            .collect();
    }

    let mut package_ids: Vec<_> = core_state.packages.keys().cloned().collect();
    package_ids.sort_by(|left, right| {
        let left_package = &core_state.packages[left];
        let right_package = &core_state.packages[right];
        let ordering = match sort.key {
            SortKey::Queue => core_state
                .packages
                .get_index_of(left)
                .cmp(&core_state.packages.get_index_of(right)),
            SortKey::Status => package_status_rank(left_package.status)
                .cmp(&package_status_rank(right_package.status)),
            SortKey::Name => left_package.display_name.cmp(&right_package.display_name),
            SortKey::Percent => {
                package_percent(core_state, left).cmp(&package_percent(core_state, right))
            }
        }
        .then_with(|| left_package.display_name.cmp(&right_package.display_name))
        .then_with(|| left.cmp(right));

        match sort.direction {
            SortDirection::Asc => ordering,
            SortDirection::Desc => ordering.reverse(),
        }
    });

    let mut rows = Vec::new();
    for package_id in package_ids {
        if !package_has_visible_content(core_state, overlay_files, &package_id) {
            continue;
        }
        rows.push(TuiRow::Package(package_id.clone()));
        if package_is_auto_expanded_for(expanded_packages, core_state, &package_id)
            && package_has_visible_children(core_state, &package_id)
        {
            let mut file_ids = core_state
                .packages
                .get(&package_id)
                .map(|package| package.file_ids.clone())
                .unwrap_or_default();
            file_ids.sort_by(|left, right| {
                natural_cmp(
                    &file_name_for_sort(core_state, left),
                    &file_name_for_sort(core_state, right),
                )
                .then_with(|| left.cmp(right))
            });
            rows.extend(
                file_ids
                    .into_iter()
                    .filter(|file_id| file_is_visible_in_package(core_state, file_id))
                    .map(|file_id| TuiRow::File {
                        package_id: package_id.clone(),
                        file_id,
                    }),
            );
        }
    }

    rows.extend(
        sorted_file_indices(files, core_state, overlay_files)
            .into_iter()
            .filter_map(|index| {
                let file = &files[index];
                if core_state.files.contains_key(&file.id)
                    || overlay_row_is_hidden_placeholder(file, overlay_files.get(&file.id))
                {
                    return None;
                }

                Some(TuiRow::File {
                    package_id: String::new(),
                    file_id: file.id.clone(),
                })
            }),
    );
    rows
}

pub(super) fn visible_rows(app: &App) -> Vec<TuiRow> {
    visible_rows_for(
        &app.files,
        &app.core_state,
        &app.overlay_files,
        &app.expanded_packages,
        &app.sort,
    )
}

fn natural_cmp(left: &str, right: &str) -> Ordering {
    let mut left = left.chars().peekable();
    let mut right = right.chars().peekable();

    loop {
        match (left.peek(), right.peek()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(left_char), Some(right_char))
                if left_char.is_ascii_digit() && right_char.is_ascii_digit() =>
            {
                let left_number = take_digits(&mut left);
                let right_number = take_digits(&mut right);
                let number_order = compare_digit_runs(&left_number, &right_number);
                if number_order != Ordering::Equal {
                    return number_order;
                }
            }
            (Some(_), Some(_)) => {
                let left_char = left.next().expect("peeked char should exist");
                let right_char = right.next().expect("peeked char should exist");
                match left_char
                    .to_ascii_lowercase()
                    .cmp(&right_char.to_ascii_lowercase())
                {
                    Ordering::Equal => {}
                    other => return other,
                }
            }
        }
    }
}

fn take_digits<I>(chars: &mut std::iter::Peekable<I>) -> String
where
    I: Iterator<Item = char>,
{
    let mut digits = String::new();
    while chars.peek().is_some_and(char::is_ascii_digit) {
        digits.push(chars.next().expect("peeked digit should exist"));
    }
    digits
}

fn compare_digit_runs(left: &str, right: &str) -> Ordering {
    let left_trimmed = left.trim_start_matches('0');
    let right_trimmed = right.trim_start_matches('0');
    let left_normalized = if left_trimmed.is_empty() {
        "0"
    } else {
        left_trimmed
    };
    let right_normalized = if right_trimmed.is_empty() {
        "0"
    } else {
        right_trimmed
    };

    left_normalized
        .len()
        .cmp(&right_normalized.len())
        .then_with(|| left_normalized.cmp(right_normalized))
        .then_with(|| left.len().cmp(&right.len()))
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
    expanded_packages: &HashSet<String>,
    sort: &super::app::SortState,
    deleted_files: &HashSet<String>,
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
    let TuiRow::File { package_id, .. } = selected_row_identity else {
        return None;
    };

    visible_rows
        .iter()
        .position(|row| matches!(row, TuiRow::Package(id) if id == package_id))
}
