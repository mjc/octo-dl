use indexmap::IndexMap;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use std::cmp::Ordering;

use crate::core::{DownloadState, FileId, PackageId, PackageStatus};

use super::TuiRow;
use crate::tui::app::{
    ExpandedPackages, FileEntry, FileStatus, FileUiMap, SortDirection, SortKey, SortState,
    TransientRow,
};

#[cfg(test)]
use std::cell::Cell;

#[cfg(test)]
thread_local! {
    static VISIBLE_ROWS_FOR_CALLS: Cell<usize> = const { Cell::new(0) };
    static BUILD_FILE_SORT_KEY_CALLS: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_visible_rows_for_call_count() {
    VISIBLE_ROWS_FOR_CALLS.with(|count| count.set(0));
}

#[cfg(test)]
pub(crate) fn visible_rows_for_call_count() -> usize {
    VISIBLE_ROWS_FOR_CALLS.with(Cell::get)
}

#[cfg(test)]
pub(crate) fn reset_build_file_sort_key_call_count() {
    BUILD_FILE_SORT_KEY_CALLS.with(|count| count.set(0));
}

#[cfg(test)]
pub(crate) fn build_file_sort_key_call_count() -> usize {
    BUILD_FILE_SORT_KEY_CALLS.with(Cell::get)
}

struct PackageProjection<'a> {
    order: usize,
    display_name: &'a str,
    files: Vec<&'a FileEntry>,
}

fn package_projections<'a>(
    files: &'a [FileEntry],
    file_ui: &'a FileUiMap,
    core_state: &'a DownloadState,
) -> IndexMap<PackageId, PackageProjection<'a>> {
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

    for file in files {
        let Some(package_id) = file_ui.get(&file.id).and_then(|state| state.package_id) else {
            continue;
        };
        if let Some(package) = projections.get_mut(&package_id) {
            package.files.push(file);
        }
    }

    projections
}

fn file_status_rank(status: &FileStatus) -> u8 {
    match status {
        FileStatus::Error(_) => 0,
        FileStatus::Downloading => 1,
        FileStatus::Queued => 2,
        FileStatus::Complete => 3,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NaturalSortChunk {
    Text(String),
    Number { raw_len: usize, normalized: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NaturalSortKey(SmallVec<[NaturalSortChunk; 8]>);

impl NaturalSortKey {
    fn new(value: &str) -> Self {
        let mut chunks = SmallVec::new();
        let mut index = 0;
        while index < value.len() {
            let rest = &value[index..];
            let Some(ch) = rest.chars().next() else {
                break;
            };
            if ch.is_ascii_digit() {
                let raw = digit_run(value, index);
                let trimmed = raw.trim_start_matches('0');
                let normalized = if trimmed.is_empty() { "0" } else { trimmed };
                chunks.push(NaturalSortChunk::Number {
                    raw_len: raw.len(),
                    normalized: normalized.to_string(),
                });
                index += raw.len();
                continue;
            }

            let start = index;
            index += ch.len_utf8();
            while index < value.len() {
                let Some(next) = value[index..].chars().next() else {
                    break;
                };
                if next.is_ascii_digit() {
                    break;
                }
                index += next.len_utf8();
            }
            chunks.push(NaturalSortChunk::Text(
                value[start..index]
                    .chars()
                    .map(|segment| segment.to_ascii_lowercase())
                    .collect(),
            ));
        }
        Self(chunks)
    }
}

fn cmp_natural_sort_keys(left: &NaturalSortKey, right: &NaturalSortKey) -> Ordering {
    for (left_chunk, right_chunk) in left.0.iter().zip(right.0.iter()) {
        let ordering = match (left_chunk, right_chunk) {
            (NaturalSortChunk::Text(left), NaturalSortChunk::Text(right)) => left.cmp(right),
            (
                NaturalSortChunk::Number {
                    normalized: left,
                    raw_len: left_raw_len,
                },
                NaturalSortChunk::Number {
                    normalized: right,
                    raw_len: right_raw_len,
                },
            ) => compare_digit_runs(left, *left_raw_len, right, *right_raw_len),
            (NaturalSortChunk::Text(left), NaturalSortChunk::Number { normalized, .. }) => {
                left.as_str().cmp(normalized.as_str())
            }
            (NaturalSortChunk::Number { normalized, .. }, NaturalSortChunk::Text(right)) => {
                normalized.as_str().cmp(right.as_str())
            }
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }

    left.0.len().cmp(&right.0.len())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CachedFileSortKey {
    pub(super) package_order: usize,
    pub(super) package_display_name: String,
    pub(super) status_rank: u8,
    natural_name: NaturalSortKey,
}

pub(super) fn cached_file_sort_key_matches(
    cached: &CachedFileSortKey,
    package_order: usize,
    package_display_name: &str,
) -> bool {
    cached.package_order == package_order && cached.package_display_name == package_display_name
}

pub(crate) fn build_file_sort_key(
    name: &str,
    status: &FileStatus,
    package_order: usize,
    package_display_name: &str,
) -> CachedFileSortKey {
    #[cfg(test)]
    BUILD_FILE_SORT_KEY_CALLS.with(|count| count.set(count.get().saturating_add(1)));
    CachedFileSortKey {
        package_order,
        package_display_name: package_display_name.to_string(),
        status_rank: file_status_rank(status),
        natural_name: NaturalSortKey::new(name),
    }
}

#[cfg(test)]
enum FileSortProjection<'a> {
    Borrowed(&'a CachedFileSortKey),
    Owned(CachedFileSortKey),
}

#[cfg(test)]
impl FileSortProjection<'_> {
    fn key(&self) -> &CachedFileSortKey {
        match self {
            Self::Borrowed(key) => key,
            Self::Owned(key) => key,
        }
    }
}

#[cfg(test)]
fn sorted_file_indices_with_keys(
    files: &[FileEntry],
    file_ui: &FileUiMap,
    overlay_files: &IndexMap<FileId, TransientRow>,
) -> Vec<usize> {
    let sort_projections = files
        .iter()
        .map(|file| {
            file_ui
                .get(&file.id)
                .and_then(|state| state.sort_key.as_ref())
                .map_or_else(
                    || {
                        FileSortProjection::Owned(build_file_sort_key(
                            &file.name,
                            &file.status,
                            usize::MAX,
                            overlay_files
                                .get(&file.id)
                                .and_then(TransientRow::source_url)
                                .unwrap_or(file.id.as_str()),
                        ))
                    },
                    FileSortProjection::Borrowed,
                )
        })
        .collect::<Vec<_>>();
    let mut indices: Vec<_> = (0..files.len()).collect();
    indices.sort_unstable_by(|&left, &right| {
        let left_projection = sort_projections[left].key();
        let right_projection = sort_projections[right].key();
        (
            left_projection.package_order,
            left_projection.package_display_name.as_str(),
        )
            .cmp(&(
                right_projection.package_order,
                right_projection.package_display_name.as_str(),
            ))
            .then_with(|| {
                left_projection
                    .status_rank
                    .cmp(&right_projection.status_rank)
            })
            .then_with(|| {
                cmp_natural_sort_keys(
                    &left_projection.natural_name,
                    &right_projection.natural_name,
                )
            })
            .then_with(|| files[left].id.cmp(&files[right].id))
    });
    indices
}

fn file_percent(file: &FileEntry) -> u64 {
    if file.size == 0 {
        0
    } else {
        file.downloaded
            .min(file.size)
            .saturating_mul(100)
            .saturating_div(file.size)
    }
}

fn compare_file_entries(
    left: &FileEntry,
    right: &FileEntry,
    left_queue: usize,
    right_queue: usize,
    sort: &SortState,
) -> Ordering {
    let ordering = match sort.key {
        SortKey::Queue => left_queue.cmp(&right_queue),
        SortKey::Status => file_status_rank(&left.status).cmp(&file_status_rank(&right.status)),
        SortKey::Name => cmp_natural_sort_keys(
            &NaturalSortKey::new(&left.name),
            &NaturalSortKey::new(&right.name),
        )
        .then_with(|| left.id.cmp(&right.id)),
        SortKey::Percent => file_percent(left)
            .cmp(&file_percent(right))
            .then_with(|| {
                cmp_natural_sort_keys(
                    &NaturalSortKey::new(&left.name),
                    &NaturalSortKey::new(&right.name),
                )
            })
            .then_with(|| left.id.cmp(&right.id)),
    };
    match sort.direction {
        SortDirection::Asc => ordering,
        SortDirection::Desc => ordering.reverse(),
    }
}

fn sorted_file_indices_for_sort(files: &[FileEntry], sort: &SortState) -> Vec<usize> {
    let mut indices = (0..files.len()).collect::<Vec<_>>();
    indices.sort_by(|&left, &right| {
        compare_file_entries(&files[left], &files[right], left, right, sort)
    });
    indices
}

fn sorted_overlay_file_ids_for_sort(
    overlay_files: &IndexMap<FileId, TransientRow>,
    sort: &SortState,
) -> Vec<FileId> {
    let entries = overlay_files.iter().collect::<Vec<_>>();
    let mut indices = (0..entries.len()).collect::<Vec<_>>();
    indices.sort_by(|&left, &right| {
        compare_file_entries(
            entries[left].1.file(),
            entries[right].1.file(),
            left,
            right,
            sort,
        )
    });
    indices
        .into_iter()
        .map(|index| entries[index].0.clone())
        .collect()
}

#[cfg(test)]
pub(super) fn sorted_file_indices(
    files: &[FileEntry],
    core_state: &DownloadState,
    overlay_files: &IndexMap<FileId, TransientRow>,
) -> Vec<usize> {
    let file_ui = files
        .iter()
        .map(|file| {
            let (package_order, package_label) = core_state
                .files
                .get(&file.id)
                .and_then(|core_file| {
                    core_state
                        .packages
                        .get_index_of(&core_file.package_id)
                        .map(|order| (order, ""))
                })
                .unwrap_or_else(|| {
                    (
                        usize::MAX,
                        overlay_files
                            .get(&file.id)
                            .and_then(TransientRow::source_url)
                            .unwrap_or(file.id.as_str()),
                    )
                });
            (
                file.id.clone(),
                crate::tui::app::FileUiState {
                    speed: 0,
                    rate: Default::default(),
                    sort_key: Some(build_file_sort_key(
                        &file.name,
                        &file.status,
                        package_order,
                        package_label,
                    )),
                    package_id: core_state
                        .files
                        .get(&file.id)
                        .map(|core_file| core_file.package_id),
                },
            )
        })
        .collect::<FxHashMap<_, _>>();
    sorted_file_indices_with_keys(files, &file_ui, overlay_files)
}

fn package_percent(package: &PackageProjection<'_>) -> u64 {
    let (downloaded, size) =
        package
            .files
            .iter()
            .fold((0_u64, 0_u64), |(downloaded, size), file| {
                (
                    downloaded.saturating_add(file.downloaded.min(file.size)),
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
    expanded_packages: &ExpandedPackages,
    core_state: &DownloadState,
    package_id: &PackageId,
) -> bool {
    expanded_packages.contains(package_id)
        || core_state
            .packages
            .get(package_id)
            .is_some_and(|package| matches!(package.status(), PackageStatus::Failed))
}

fn overlay_row_is_hidden_placeholder(file: &FileEntry, overlay: Option<&TransientRow>) -> bool {
    matches!(file.status, FileStatus::Queued)
        && file.size == 0
        && overlay.is_some_and(|overlay| matches!(overlay, TransientRow::UiError { .. }))
}

fn package_has_visible_content(
    package_projections: &IndexMap<PackageId, PackageProjection<'_>>,
    package_id: &PackageId,
) -> bool {
    package_projections
        .get(package_id)
        .is_some_and(|package_projection| !package_projection.files.is_empty())
}

pub(super) fn visible_rows_for(
    files: &[FileEntry],
    file_ui: &FileUiMap,
    core_state: &DownloadState,
    overlay_files: &IndexMap<FileId, TransientRow>,
    expanded_packages: &ExpandedPackages,
    sort: &SortState,
) -> Vec<TuiRow> {
    #[cfg(test)]
    VISIBLE_ROWS_FOR_CALLS.with(|count| count.set(count.get().saturating_add(1)));
    let package_projections = package_projections(files, file_ui, core_state);
    if core_state.packages.is_empty() {
        return sorted_file_indices_for_sort(files, sort)
            .into_iter()
            .map(|index| TuiRow::File {
                package_id: None,
                file_id: files[index].id.clone(),
            })
            .collect();
    }
    let package_percents = matches!(sort.key, SortKey::Percent).then(|| {
        package_projections
            .iter()
            .map(|(package_id, package)| (*package_id, package_percent(package)))
            .collect::<FxHashMap<_, _>>()
    });

    let mut package_ids: Vec<_> = core_state.packages.keys().cloned().collect();
    package_ids.sort_by(|left, right| {
        let left_projection = &package_projections[left];
        let right_projection = &package_projections[right];
        let left_package = &core_state.packages[left];
        let right_package = &core_state.packages[right];
        let ordering = match sort.key {
            SortKey::Queue => left_projection.order.cmp(&right_projection.order),
            SortKey::Status => package_status_rank(left_package.status())
                .cmp(&package_status_rank(right_package.status())),
            SortKey::Name => left_projection
                .display_name
                .cmp(&right_projection.display_name),
            SortKey::Percent => package_percents
                .as_ref()
                .map_or(Ordering::Equal, |percents| {
                    percents.get(left).cmp(&percents.get(right))
                }),
        };

        let ordering = if matches!(sort.key, SortKey::Queue) {
            ordering
        } else {
            ordering
                .then_with(|| {
                    left_projection
                        .display_name
                        .cmp(&right_projection.display_name)
                })
                .then_with(|| left.cmp(right))
        };

        match sort.direction {
            SortDirection::Asc => ordering,
            SortDirection::Desc => ordering.reverse(),
        }
    });

    let mut rows = Vec::new();
    for package_id in package_ids {
        if !package_has_visible_content(&package_projections, &package_id) {
            continue;
        }
        rows.push(TuiRow::Package(package_id));
        // The earlier package_has_visible_content filter should already guarantee
        // children exist; keep this guard to stay resilient to future refactors.
        if package_is_auto_expanded_for(expanded_packages, core_state, &package_id)
            && package_has_visible_content(&package_projections, &package_id)
        {
            let Some(package) = package_projections.get(&package_id) else {
                continue;
            };
            let mut package_files = package.files.clone();
            package_files.sort_by(|left, right| {
                let left_queue = package
                    .files
                    .iter()
                    .position(|file| file.id == left.id)
                    .unwrap_or(usize::MAX);
                let right_queue = package
                    .files
                    .iter()
                    .position(|file| file.id == right.id)
                    .unwrap_or(usize::MAX);
                compare_file_entries(left, right, left_queue, right_queue, sort)
            });
            rows.extend(package_files.into_iter().map(|file| TuiRow::File {
                package_id: Some(package_id),
                file_id: file.id.clone(),
            }));
        }
    }

    rows.extend(
        sorted_overlay_file_ids_for_sort(overlay_files, sort)
            .into_iter()
            .filter_map(|file_id| {
                let overlay = overlay_files.get(&file_id)?;
                if overlay_row_is_hidden_placeholder(overlay.file(), Some(overlay)) {
                    return None;
                }

                Some(TuiRow::File {
                    package_id: None,
                    file_id,
                })
            }),
    );
    rows
}

#[cfg(test)]
fn natural_cmp(left: &str, right: &str) -> Ordering {
    cmp_natural_sort_keys(&NaturalSortKey::new(left), &NaturalSortKey::new(right))
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

fn compare_digit_runs(
    left_normalized: &str,
    left_len: usize,
    right_normalized: &str,
    right_len: usize,
) -> Ordering {
    left_normalized
        .len()
        .cmp(&right_normalized.len())
        .then_with(|| left_normalized.cmp(right_normalized))
        .then_with(|| left_len.cmp(&right_len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;

    #[test]
    fn natural_cmp_orders_digit_runs_without_lexical_surprises() {
        assert_eq!(natural_cmp("file-2.mkv", "file-10.mkv"), Ordering::Less);
        assert_eq!(natural_cmp("file-02.mkv", "file-2.mkv"), Ordering::Greater);
        assert_eq!(natural_cmp("File-2.mkv", "file-2.mkv"), Ordering::Equal);
    }

    #[test]
    fn natural_cmp_handles_text_digit_boundaries() {
        assert_eq!(natural_cmp("a2", "aa"), Ordering::Less);
        assert_eq!(natural_cmp("002", "0a"), Ordering::Greater);
        assert_eq!(natural_cmp("z9", "z10"), Ordering::Less);
    }

    #[test]
    fn sorted_file_indices_keep_natural_name_order_with_cached_keys() {
        let files = vec![
            FileEntry {
                id: "file-10.mkv".into(),
                name: "file-10.mkv".to_string(),
                size: 10,
                downloaded: 0,
                status: FileStatus::Queued,
            },
            FileEntry {
                id: "file-2.mkv".into(),
                name: "file-2.mkv".to_string(),
                size: 10,
                downloaded: 0,
                status: FileStatus::Queued,
            },
            FileEntry {
                id: "file-02.mkv".into(),
                name: "file-02.mkv".to_string(),
                size: 10,
                downloaded: 0,
                status: FileStatus::Queued,
            },
        ];
        let file_ui = files
            .iter()
            .map(|file| {
                (
                    file.id.clone(),
                    crate::tui::app::FileUiState {
                        speed: 0,
                        rate: Default::default(),
                        sort_key: Some(build_file_sort_key(&file.name, &file.status, 0, "package")),
                        package_id: None,
                    },
                )
            })
            .collect::<FxHashMap<_, _>>();

        let indices = sorted_file_indices_with_keys(&files, &file_ui, &IndexMap::new());
        let ordered = indices
            .into_iter()
            .map(|index| files[index].name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(ordered, vec!["file-2.mkv", "file-02.mkv", "file-10.mkv"]);
    }

    fn file(id: &str, name: &str, size: u64, downloaded: u64, status: FileStatus) -> FileEntry {
        FileEntry {
            id: id.into(),
            name: name.to_string(),
            size,
            downloaded,
            status,
        }
    }

    fn file_ids(rows: &[TuiRow]) -> Vec<&str> {
        rows.iter()
            .filter_map(|row| match row {
                TuiRow::File { file_id, .. } => Some(file_id.as_str()),
                TuiRow::Package(_) => None,
            })
            .collect()
    }

    #[test]
    fn ungrouped_files_honor_name_and_direction_sorting() {
        let files = vec![
            file("z", "z-complete", 100, 100, FileStatus::Complete),
            file("a", "a-queued", 100, 0, FileStatus::Queued),
        ];
        let core = DownloadState::default();
        let file_ui = FileUiMap::default();
        let overlays = IndexMap::new();

        let ascending = visible_rows_for(
            &files,
            &file_ui,
            &core,
            &overlays,
            &ExpandedPackages::default(),
            &SortState {
                key: SortKey::Name,
                direction: SortDirection::Asc,
                active_field: 0,
            },
        );
        let descending = visible_rows_for(
            &files,
            &file_ui,
            &core,
            &overlays,
            &ExpandedPackages::default(),
            &SortState {
                key: SortKey::Name,
                direction: SortDirection::Desc,
                active_field: 0,
            },
        );

        assert_eq!(file_ids(&ascending), vec!["a", "z"]);
        assert_eq!(file_ids(&descending), vec!["z", "a"]);
    }

    #[test]
    fn expanded_children_honor_name_sorting_instead_of_fixed_status_order() {
        let package_id = crate::test_support::package_id("pkg", "https://example.test/package");
        let mut core = DownloadState::default();
        let package = crate::core::PackageState {
            id: package_id,
            key: crate::core::PackageKey::new("https://example.test/package".to_string()),
            display_name: "Package".to_string(),
            progress: Default::default(),
            error: None,
        };
        core.packages.insert(package_id, package);
        let files = vec![
            file("complete", "a-complete", 100, 100, FileStatus::Complete),
            file("queued", "z-queued", 100, 0, FileStatus::Queued),
        ];
        let file_ui = files
            .iter()
            .map(|entry| {
                (
                    entry.id.clone(),
                    crate::tui::app::FileUiState {
                        package_id: Some(package_id),
                        ..Default::default()
                    },
                )
            })
            .collect();
        let mut expanded = ExpandedPackages::default();
        expanded.insert(package_id);
        let rows = visible_rows_for(
            &files,
            &file_ui,
            &core,
            &IndexMap::new(),
            &expanded,
            &SortState {
                key: SortKey::Name,
                direction: SortDirection::Asc,
                active_field: 0,
            },
        );

        assert_eq!(file_ids(&rows), vec!["complete", "queued"]);
    }
}
