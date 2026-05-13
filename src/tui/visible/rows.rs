use std::cmp::Ordering;
use std::collections::HashSet;

use indexmap::IndexMap;

use crate::core::{DownloadState, FileLifecycle, PackageId, PackageStatus};

use super::TuiRow;
use crate::tui::app::{FileEntry, FileStatus, OverlayFile, SortDirection, SortKey, SortState};

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
            .unwrap_or_else(|| core_file.package_id.to_string());
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

fn file_status_rank(status: &FileStatus) -> u8 {
    match status {
        FileStatus::Error(_) => 0,
        FileStatus::Downloading => 1,
        FileStatus::Queued => 2,
        FileStatus::Complete => 3,
    }
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

        let left_rank = file_status_rank(&left_file.status);
        let right_rank = file_status_rank(&right_file.status);
        left_rank
            .cmp(&right_rank)
            .then_with(|| natural_cmp(&left_file.name, &right_file.name))
            .then_with(|| left_file.id.cmp(&right_file.id))
    });
    indices
}

fn package_percent(core_state: &DownloadState, package_id: &PackageId) -> u64 {
    if !core_state.packages.contains_key(package_id) {
        return 0;
    }
    let (downloaded, size) =
        core_state
            .package_files(package_id)
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
    expanded_packages: &HashSet<PackageId>,
    core_state: &DownloadState,
    package_id: &PackageId,
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
    package_id: &PackageId,
) -> bool {
    let Some(package) = core_state.packages.get(package_id) else {
        return false;
    };
    let has_visible_files = core_state
        .package_files(package_id)
        .any(|file| file_is_visible_in_package(core_state, &file.id));

    has_visible_files
        || (package.error.is_some()
            && core_state.package_files(package_id).next().is_some()
            && !overlay_files.contains_key(&package_id.to_string()))
}

fn package_has_visible_children(core_state: &DownloadState, package_id: &PackageId) -> bool {
    core_state.packages.contains_key(package_id)
        && core_state
            .package_files(package_id)
            .any(|file| file_is_visible_in_package(core_state, &file.id))
}

pub(super) fn visible_rows_for(
    files: &[FileEntry],
    core_state: &DownloadState,
    overlay_files: &IndexMap<String, OverlayFile>,
    expanded_packages: &HashSet<PackageId>,
    sort: &SortState,
) -> Vec<TuiRow> {
    if core_state.packages.is_empty() {
        return sorted_file_indices(files, core_state, overlay_files)
            .into_iter()
            .map(|index| TuiRow::File {
                package_id: None,
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
        rows.push(TuiRow::Package(package_id));
        if package_is_auto_expanded_for(expanded_packages, core_state, &package_id)
            && package_has_visible_children(core_state, &package_id)
        {
            let mut package_files = core_state.package_files(&package_id).collect::<Vec<_>>();
            package_files.sort_by(|left, right| {
                let left_status = file_status_from_core(left);
                let right_status = file_status_from_core(right);
                file_status_rank(&left_status)
                    .cmp(&file_status_rank(&right_status))
                    .then_with(|| natural_cmp(&left.path, &right.path))
                    .then_with(|| left.id.cmp(&right.id))
            });
            rows.extend(
                package_files
                    .into_iter()
                    .filter(|file| file_is_visible_in_package(core_state, &file.id))
                    .map(|file| TuiRow::File {
                        package_id: Some(package_id),
                        file_id: file.id.clone(),
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
                    package_id: None,
                    file_id: file.id.clone(),
                })
            }),
    );
    rows
}

fn file_status_from_core(file: &crate::core::FileState) -> FileStatus {
    match file.lifecycle {
        FileLifecycle::Planned | FileLifecycle::Queued => FileStatus::Queued,
        FileLifecycle::Downloading => FileStatus::Downloading,
        FileLifecycle::Complete => FileStatus::Complete,
        FileLifecycle::Failed => {
            FileStatus::Error(file.message.clone().unwrap_or_else(|| "failed".to_string()))
        }
        FileLifecycle::Skipped | FileLifecycle::Deleted => FileStatus::Complete,
    }
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
