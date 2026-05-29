use crate::core::model::{
    DownloadState, FileAccounting, FileId, FileLifecycle, FileState, PackageId,
    PackageProgressState, TotalsState,
};

#[cfg(test)]
use crate::core::model::{FileProgressState, PackageKey, PackageState, PackageStatus};

fn counts_in_run_totals(file: &FileState) -> bool {
    matches!(file.accounting, FileAccounting::CurrentRun)
}

#[derive(Debug, Clone, Copy)]
pub(super) struct FileDerivedState {
    pub(super) package_id: PackageId,
    pub(super) lifecycle_bucket: PackageProgressBucket,
    size: u64,
    visible_completed_bytes: u64,
    downloaded_network_bytes: u64,
    counts_in_run_totals: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PackageProgressBucket {
    Queued,
    Downloading,
    Complete,
    Failed,
}

impl PackageProgressBucket {
    pub(super) const fn from_lifecycle(lifecycle: &FileLifecycle) -> Self {
        match lifecycle {
            FileLifecycle::Planned | FileLifecycle::Queued => Self::Queued,
            FileLifecycle::Downloading => Self::Downloading,
            FileLifecycle::Complete => Self::Complete,
            FileLifecycle::Failed { .. } => Self::Failed,
        }
    }

    fn add_to(self, progress: &mut PackageProgressState) {
        match self {
            Self::Queued => progress.queued = progress.queued.saturating_add(1),
            Self::Downloading => progress.downloading = progress.downloading.saturating_add(1),
            Self::Complete => progress.complete = progress.complete.saturating_add(1),
            Self::Failed => progress.failed = progress.failed.saturating_add(1),
        }
    }

    fn remove_from(self, progress: &mut PackageProgressState) {
        match self {
            Self::Queued => progress.queued = progress.queued.saturating_sub(1),
            Self::Downloading => progress.downloading = progress.downloading.saturating_sub(1),
            Self::Complete => progress.complete = progress.complete.saturating_sub(1),
            Self::Failed => progress.failed = progress.failed.saturating_sub(1),
        }
    }
}

impl From<&FileState> for FileDerivedState {
    fn from(file: &FileState) -> Self {
        Self {
            package_id: file.package_id,
            lifecycle_bucket: PackageProgressBucket::from_lifecycle(&file.lifecycle),
            size: file.size,
            visible_completed_bytes: file.progress.visible_completed_bytes.min(file.size),
            downloaded_network_bytes: file.progress.downloaded_network_bytes.min(file.size),
            counts_in_run_totals: counts_in_run_totals(file),
        }
    }
}

pub(super) fn add_totals_contribution(state: &mut DownloadState, file: FileDerivedState) {
    if !file.counts_in_run_totals {
        return;
    }
    state.totals.run_total_bytes = state.totals.run_total_bytes.saturating_add(file.size);
    state.totals.run_completed_bytes = state
        .totals
        .run_completed_bytes
        .saturating_add(file.visible_completed_bytes);
    state.totals.displayed_network_bytes = state
        .totals
        .displayed_network_bytes
        .saturating_add(file.downloaded_network_bytes);
    state.totals.run_file_total = state.totals.run_file_total.saturating_add(1);
    if matches!(file.lifecycle_bucket, PackageProgressBucket::Downloading) {
        state.totals.run_file_downloading = state.totals.run_file_downloading.saturating_add(1);
    }
    if matches!(file.lifecycle_bucket, PackageProgressBucket::Complete) {
        state.totals.run_file_completed = state.totals.run_file_completed.saturating_add(1);
    }
}

pub(super) fn remove_totals_contribution(state: &mut DownloadState, file: FileDerivedState) {
    if !file.counts_in_run_totals {
        return;
    }
    state.totals.run_total_bytes = state.totals.run_total_bytes.saturating_sub(file.size);
    state.totals.run_completed_bytes = state
        .totals
        .run_completed_bytes
        .saturating_sub(file.visible_completed_bytes);
    state.totals.displayed_network_bytes = state
        .totals
        .displayed_network_bytes
        .saturating_sub(file.downloaded_network_bytes);
    state.totals.run_file_total = state.totals.run_file_total.saturating_sub(1);
    if matches!(file.lifecycle_bucket, PackageProgressBucket::Downloading) {
        state.totals.run_file_downloading = state.totals.run_file_downloading.saturating_sub(1);
    }
    if matches!(file.lifecycle_bucket, PackageProgressBucket::Complete) {
        state.totals.run_file_completed = state.totals.run_file_completed.saturating_sub(1);
    }
}

pub(super) fn add_package_progress(
    state: &mut DownloadState,
    package_id: PackageId,
    lifecycle_bucket: PackageProgressBucket,
) {
    if let Some(package) = state.packages.get_mut(&package_id) {
        lifecycle_bucket.add_to(&mut package.progress);
    }
}

pub(super) fn remove_package_progress(
    state: &mut DownloadState,
    package_id: PackageId,
    lifecycle_bucket: PackageProgressBucket,
) {
    if let Some(package) = state.packages.get_mut(&package_id) {
        lifecycle_bucket.remove_from(&mut package.progress);
    }
}

pub(super) fn apply_file_change(
    state: &mut DownloadState,
    _file_id: &FileId,
    before: FileDerivedState,
    after: FileDerivedState,
) {
    remove_totals_contribution(state, before);
    if before.package_id != after.package_id {
        remove_package_progress(state, before.package_id, before.lifecycle_bucket);
        add_package_progress(state, after.package_id, after.lifecycle_bucket);
    } else if before.lifecycle_bucket != after.lifecycle_bucket {
        remove_package_progress(state, before.package_id, before.lifecycle_bucket);
        add_package_progress(state, after.package_id, after.lifecycle_bucket);
    }
    add_totals_contribution(state, after);
    if before.package_id != after.package_id || before.lifecycle_bucket != after.lifecycle_bucket {
        super::recompute_session_status(state);
    }
}

pub(super) fn recompute_derived(state: &mut DownloadState) {
    for package in state.packages.values_mut() {
        package.progress = PackageProgressState::default();
    }
    let package_lifecycles: Vec<_> = state
        .files
        .values()
        .map(|file| {
            (
                file.package_id,
                PackageProgressBucket::from_lifecycle(&file.lifecycle),
            )
        })
        .collect();
    for (package_id, lifecycle_bucket) in package_lifecycles {
        add_package_progress(state, package_id, lifecycle_bucket);
    }

    let mut totals = TotalsState::default();
    for file in state.files.values() {
        if !matches!(file.accounting, FileAccounting::CurrentRun) {
            continue;
        }
        totals.run_total_bytes = totals.run_total_bytes.saturating_add(file.size);
        totals.run_completed_bytes = totals
            .run_completed_bytes
            .saturating_add(file.progress.visible_completed_bytes.min(file.size));
        totals.displayed_network_bytes = totals
            .displayed_network_bytes
            .saturating_add(file.progress.downloaded_network_bytes.min(file.size));
        totals.run_file_total = totals.run_file_total.saturating_add(1);
        if matches!(file.lifecycle, FileLifecycle::Downloading) {
            totals.run_file_downloading = totals.run_file_downloading.saturating_add(1);
        }
        if matches!(file.lifecycle, FileLifecycle::Complete) {
            totals.run_file_completed = totals.run_file_completed.saturating_add(1);
        }
    }
    state.totals = totals;
    super::recompute_session_status(state);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package_id(raw: &str, source_url: &str) -> PackageId {
        PackageId::parse_or_key(raw, &PackageKey::new(source_url))
    }

    #[test]
    fn rebuild_derived_state_restores_package_progress_cache() {
        let pkg_id = package_id("pkg", "pkg");
        let mut state = DownloadState::new(crate::core::SessionMeta::default());
        state.packages.insert(
            pkg_id,
            PackageState {
                id: pkg_id,
                key: PackageKey::new("pkg"),
                display_name: "pkg".to_string(),
                progress: PackageProgressState::default(),
                error: None,
            },
        );
        state.files.insert(
            "a.bin".into(),
            FileState {
                id: "a.bin".into(),
                package_id: pkg_id,
                source_url: "pkg".to_string(),
                path: "a.bin".to_string(),
                size: 10,
                lifecycle: FileLifecycle::Complete,
                progress: FileProgressState {
                    visible_completed_bytes: 10,
                    ..FileProgressState::default()
                },
                accounting: FileAccounting::CurrentRun,
            },
        );
        state.files.insert(
            "b.bin".into(),
            FileState {
                id: "b.bin".into(),
                package_id: pkg_id,
                source_url: "pkg".to_string(),
                path: "b.bin".to_string(),
                size: 20,
                lifecycle: FileLifecycle::Queued,
                progress: FileProgressState::default(),
                accounting: FileAccounting::CurrentRun,
            },
        );

        recompute_derived(&mut state);

        assert_eq!(
            state.packages[&pkg_id].progress,
            PackageProgressState {
                queued: 1,
                complete: 1,
                ..PackageProgressState::default()
            }
        );
        assert_eq!(state.packages[&pkg_id].status(), PackageStatus::Partial);
    }

    #[test]
    fn rebuild_derived_state_restores_downloading_totals() {
        let pkg_id = package_id("pkg", "pkg");
        let mut state = DownloadState::new(crate::core::SessionMeta::default());
        state.packages.insert(
            pkg_id,
            PackageState {
                id: pkg_id,
                key: PackageKey::new("pkg"),
                display_name: "pkg".to_string(),
                progress: PackageProgressState::default(),
                error: None,
            },
        );
        state.files.insert(
            "downloading.bin".into(),
            FileState {
                id: "downloading.bin".into(),
                package_id: pkg_id,
                source_url: "pkg".to_string(),
                path: "downloading.bin".to_string(),
                size: 20,
                lifecycle: FileLifecycle::Downloading,
                progress: FileProgressState::default(),
                accounting: FileAccounting::CurrentRun,
            },
        );
        state.files.insert(
            "complete.bin".into(),
            FileState {
                id: "complete.bin".into(),
                package_id: pkg_id,
                source_url: "pkg".to_string(),
                path: "complete.bin".to_string(),
                size: 10,
                lifecycle: FileLifecycle::Complete,
                progress: FileProgressState {
                    visible_completed_bytes: 10,
                    downloaded_network_bytes: 10,
                    ..FileProgressState::default()
                },
                accounting: FileAccounting::CurrentRun,
            },
        );

        recompute_derived(&mut state);

        assert_eq!(state.totals.run_file_total, 2);
        assert_eq!(state.totals.run_file_completed, 1);
        assert_eq!(state.totals.run_file_downloading, 1);
        assert_eq!(state.packages[&pkg_id].progress.downloading, 1);
    }
}
