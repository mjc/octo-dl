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

fn package_percent(core_state: &DownloadState, package_id: &PackageId) -> u64 {
    if !core_state.packages.contains_key(package_id) {
        return 0;
    }
    let (downloaded, size) = core_state
        .package_file_ids(package_id)
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
    package_id: &PackageId,
) -> bool {
    expanded_packages.contains(&package_id.to_string())
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
    let file_ids = core_state.package_file_ids(package_id);

    file_ids.is_empty()
        || file_ids
            .iter()
            .any(|file_id| file_is_visible_in_package(core_state, file_id))
        || (package.error.is_some() && !overlay_files.contains_key(&package_id.to_string()))
}

fn package_has_visible_children(core_state: &DownloadState, package_id: &PackageId) -> bool {
    core_state.packages.contains_key(package_id) && core_state
            .package_file_ids(package_id)
            .iter()
            .any(|file_id| file_is_visible_in_package(core_state, file_id))
}

pub(super) fn visible_rows_for(
    files: &[FileEntry],
    core_state: &DownloadState,
    overlay_files: &IndexMap<String, OverlayFile>,
    expanded_packages: &HashSet<String>,
    sort: &SortState,
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
        rows.push(TuiRow::Package(package_id.to_string()));
        if package_is_auto_expanded_for(expanded_packages, core_state, &package_id)
            && package_has_visible_children(core_state, &package_id)
        {
            let mut file_ids = core_state
                .package_file_ids(&package_id);
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
                        package_id: package_id.to_string(),
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
