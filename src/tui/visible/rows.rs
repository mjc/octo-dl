use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;

use crate::core::{DownloadState, FileId, FileLifecycle, PackageId, PackageStatus};

use super::TuiRow;
use crate::tui::app::{FileEntry, FileStatus, SortDirection, SortKey, SortState, TransientRow};

struct PackageProjection<'a> {
    order: usize,
    display_name: &'a str,
    files: Vec<&'a crate::core::FileState>,
}

fn package_projections(
    core_state: &DownloadState,
) -> (
    IndexMap<PackageId, PackageProjection<'_>>,
    HashMap<&str, (usize, &str)>,
) {
    let mut projections = core_state
        .packages
        .iter()
        .enumerate()
        .map(|(order, (package_id, package))| {
            (
                *package_id,
                PackageProjection {
                    order,
                    display_name: package.display_name.as_str(),
                    files: Vec::new(),
                },
            )
        })
        .collect::<IndexMap<_, _>>();
    let mut file_sort_keys = HashMap::new();

    let package_ids = projections.keys().cloned().collect::<Vec<_>>();
    for package_id in package_ids {
        for file_id in core_state.package_file_ids(&package_id) {
            let Some(file) = core_state.files.get(&file_id) else {
                continue;
            };
            if let Some(package) = projections.get_mut(&package_id) {
                package.files.push(file);
                file_sort_keys.insert(file.id.as_str(), (package.order, package.display_name));
            }
        }
    }

    (projections, file_sort_keys)
}

fn package_sort_key_for<'a>(
    file_sort_keys: &'a HashMap<&'a str, (usize, &'a str)>,
    overlay_files: &'a IndexMap<FileId, TransientRow>,
    file: &'a FileEntry,
) -> (usize, &'a str) {
    if let Some((order, display_name)) = file_sort_keys.get(file.id.as_str()) {
        return (*order, display_name);
    }

    (
        usize::MAX,
        overlay_files
            .get(&file.id)
            .and_then(TransientRow::source_url)
            .unwrap_or(file.id.as_str()),
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

fn sorted_file_indices_with_keys(
    files: &[FileEntry],
    file_sort_keys: &HashMap<&str, (usize, &str)>,
    overlay_files: &IndexMap<FileId, TransientRow>,
) -> Vec<usize> {
    let mut indices: Vec<_> = (0..files.len()).collect();
    indices.sort_by(|&left, &right| {
        let left_file = &files[left];
        let right_file = &files[right];
        let left_package = package_sort_key_for(file_sort_keys, overlay_files, left_file);
        let right_package = package_sort_key_for(file_sort_keys, overlay_files, right_file);

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

#[cfg(test)]
pub(super) fn sorted_file_indices(
    files: &[FileEntry],
    core_state: &DownloadState,
    overlay_files: &IndexMap<FileId, TransientRow>,
) -> Vec<usize> {
    let (_, file_sort_keys) = package_projections(core_state);
    sorted_file_indices_with_keys(files, &file_sort_keys, overlay_files)
}

fn package_percent(package: &PackageProjection<'_>) -> u64 {
    let (downloaded, size) =
        package
            .files
            .iter()
            .fold((0_u64, 0_u64), |(downloaded, size), file| {
                let visible = if matches!(file.lifecycle, FileLifecycle::Complete) {
                    file.size
                } else {
                    crate::core::visible_completed_bytes_for_display(file)
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

fn file_is_visible_in_package(core_state: &DownloadState, file_id: &FileId) -> bool {
    core_state.files.contains_key(file_id)
}

fn overlay_row_is_hidden_placeholder(file: &FileEntry, overlay: Option<&TransientRow>) -> bool {
    matches!(file.status, FileStatus::Queued)
        && file.size == 0
        && overlay.is_some_and(|overlay| matches!(overlay, TransientRow::UiError { .. }))
}

fn package_has_visible_content(
    package_projections: &IndexMap<PackageId, PackageProjection<'_>>,
    core_state: &DownloadState,
    overlay_files: &IndexMap<FileId, TransientRow>,
    package_id: &PackageId,
) -> bool {
    let Some(package) = core_state.packages.get(package_id) else {
        return false;
    };
    let Some(package_projection) = package_projections.get(package_id) else {
        return false;
    };
    let has_visible_files = package_projection
        .files
        .iter()
        .any(|file| file_is_visible_in_package(core_state, &file.id));

    has_visible_files
        || (package.error.is_some() && !package_projection.files.is_empty() && {
            let package_overlay_id = package_id.to_string();
            !overlay_files.contains_key(package_overlay_id.as_str())
        })
}

fn package_has_visible_children(
    package_projections: &IndexMap<PackageId, PackageProjection<'_>>,
    core_state: &DownloadState,
    package_id: &PackageId,
) -> bool {
    package_projections.get(package_id).is_some_and(|package| {
        package
            .files
            .iter()
            .any(|file| file_is_visible_in_package(core_state, &file.id))
    })
}

pub(super) fn visible_rows_for(
    files: &[FileEntry],
    core_state: &DownloadState,
    overlay_files: &IndexMap<FileId, TransientRow>,
    expanded_packages: &HashSet<PackageId>,
    sort: &SortState,
) -> Vec<TuiRow> {
    let (package_projections, file_sort_keys) = package_projections(core_state);
    let package_percents = package_projections
        .iter()
        .map(|(package_id, package)| (*package_id, package_percent(package)))
        .collect::<HashMap<_, _>>();
    if core_state.packages.is_empty() {
        return sorted_file_indices_with_keys(files, &file_sort_keys, overlay_files)
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
            SortKey::Percent => package_percents.get(left).cmp(&package_percents.get(right)),
        };

        let ordering = if matches!(sort.key, SortKey::Queue) {
            ordering
        } else {
            ordering
                .then_with(|| left_package.display_name.cmp(&right_package.display_name))
                .then_with(|| left.cmp(right))
        };

        match sort.direction {
            SortDirection::Asc => ordering,
            SortDirection::Desc => ordering.reverse(),
        }
    });

    let mut rows = Vec::new();
    for package_id in package_ids {
        if !package_has_visible_content(
            &package_projections,
            core_state,
            overlay_files,
            &package_id,
        ) {
            continue;
        }
        rows.push(TuiRow::Package(package_id));
        if package_is_auto_expanded_for(expanded_packages, core_state, &package_id)
            && package_has_visible_children(&package_projections, core_state, &package_id)
        {
            let mut package_files = package_projections
                .get(&package_id)
                .map(|package| package.files.clone())
                .unwrap_or_default();
            package_files.sort_by(|left, right| {
                let left_status = file_status_from_core(left);
                let right_status = file_status_from_core(right);
                let ordering = file_status_rank(&left_status).cmp(&file_status_rank(&right_status));
                match sort.direction {
                    SortDirection::Asc => ordering,
                    SortDirection::Desc => ordering.reverse(),
                }
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
        sorted_file_indices_with_keys(files, &file_sort_keys, overlay_files)
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
    match &file.lifecycle {
        FileLifecycle::Planned | FileLifecycle::Queued => FileStatus::Queued,
        FileLifecycle::Downloading => FileStatus::Downloading,
        FileLifecycle::Complete => FileStatus::Complete,
        FileLifecycle::Failed { message } => FileStatus::Error(message.clone()),
    }
}

fn natural_cmp(left: &str, right: &str) -> Ordering {
    let mut left_index = 0;
    let mut right_index = 0;

    loop {
        match (
            left[left_index..].chars().next(),
            right[right_index..].chars().next(),
        ) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(left_char), Some(right_char))
                if left_char.is_ascii_digit() && right_char.is_ascii_digit() =>
            {
                let left_number = digit_run(left, left_index);
                let right_number = digit_run(right, right_index);
                let number_order = compare_digit_runs(left_number, right_number);
                if number_order != Ordering::Equal {
                    return number_order;
                }
                left_index += left_number.len();
                right_index += right_number.len();
            }
            (Some(left_char), Some(right_char)) => {
                match left_char
                    .to_ascii_lowercase()
                    .cmp(&right_char.to_ascii_lowercase())
                {
                    Ordering::Equal => {
                        left_index += left_char.len_utf8();
                        right_index += right_char.len_utf8();
                    }
                    other => return other,
                }
            }
        }
    }
}

fn digit_run(value: &str, start: usize) -> &str {
    let mut end = start;
    for ch in value[start..].chars() {
        if !ch.is_ascii_digit() {
            break;
        }
        end += ch.len_utf8();
    }
    &value[start..end]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_cmp_orders_digit_runs_without_lexical_surprises() {
        assert_eq!(natural_cmp("file-2.mkv", "file-10.mkv"), Ordering::Less);
        assert_eq!(natural_cmp("file-02.mkv", "file-2.mkv"), Ordering::Greater);
        assert_eq!(natural_cmp("File-2.mkv", "file-2.mkv"), Ordering::Equal);
    }
}
