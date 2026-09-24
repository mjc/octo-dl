use crate::core::model::{
    DownloadState, FileAccounting, FileId, FileLifecycle, FileProgressState, FileState, PackageId,
    PackageProgressState, TotalsState,
};

#[cfg(test)]
use crate::core::model::{PackageKey, PackageState, PackageStatus};

const fn counts_in_run_totals(file: &FileState) -> bool {
    match file.accounting {
        FileAccounting::CurrentRun => true,
        FileAccounting::Preexisting => false,
    }
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
    const fn is_downloading(self) -> bool {
        match self {
            Self::Downloading => true,
            Self::Queued | Self::Complete | Self::Failed => false,
        }
    }

    const fn is_complete(self) -> bool {
        match self {
            Self::Complete => true,
            Self::Queued | Self::Downloading | Self::Failed => false,
        }
    }
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

    const fn add_to(self, progress: &mut PackageProgressState) {
        match self {
            Self::Queued => progress.queued = progress.queued.saturating_add(1),
            Self::Downloading => progress.downloading = progress.downloading.saturating_add(1),
            Self::Complete => progress.complete = progress.complete.saturating_add(1),
            Self::Failed => progress.failed = progress.failed.saturating_add(1),
        }
    }

    const fn remove_from(self, progress: &mut PackageProgressState) {
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

pub(super) const fn add_totals_contribution(totals: &mut TotalsState, file: FileDerivedState) {
    if !file.counts_in_run_totals {
        return;
    }
    totals.run_total_bytes = totals.run_total_bytes.saturating_add(file.size);
    totals.run_completed_bytes = totals
        .run_completed_bytes
        .saturating_add(file.visible_completed_bytes);
    totals.displayed_network_bytes = totals
        .displayed_network_bytes
        .saturating_add(file.downloaded_network_bytes);
    totals.run_file_total = totals.run_file_total.saturating_add(1);
    if file.lifecycle_bucket.is_downloading() {
        totals.run_file_downloading = totals.run_file_downloading.saturating_add(1);
    }
    if file.lifecycle_bucket.is_complete() {
        totals.run_file_completed = totals.run_file_completed.saturating_add(1);
    }
}

pub(super) const fn remove_totals_contribution(totals: &mut TotalsState, file: FileDerivedState) {
    if !file.counts_in_run_totals {
        return;
    }
    totals.run_total_bytes = totals.run_total_bytes.saturating_sub(file.size);
    totals.run_completed_bytes = totals
        .run_completed_bytes
        .saturating_sub(file.visible_completed_bytes);
    totals.displayed_network_bytes = totals
        .displayed_network_bytes
        .saturating_sub(file.downloaded_network_bytes);
    totals.run_file_total = totals.run_file_total.saturating_sub(1);
    if file.lifecycle_bucket.is_downloading() {
        totals.run_file_downloading = totals.run_file_downloading.saturating_sub(1);
    }
    if file.lifecycle_bucket.is_complete() {
        totals.run_file_completed = totals.run_file_completed.saturating_sub(1);
    }
}

pub(super) fn normalize_completed_file_progress(progress: &mut FileProgressState, size: u64) {
    if progress.verification_origin_complete {
        progress.downloaded_network_bytes = progress
            .verification_restore_downloaded_network_bytes
            .min(size);
    }
    progress.visible_completed_bytes = size;
    progress.verified_existing_bytes = 0;
    progress.verification_origin_complete = false;
    progress.verification_restore_downloaded_network_bytes = 0;
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
    remove_totals_contribution(&mut state.totals, before);
    if before.package_id != after.package_id || before.lifecycle_bucket != after.lifecycle_bucket {
        remove_package_progress(state, before.package_id, before.lifecycle_bucket);
        add_package_progress(state, after.package_id, after.lifecycle_bucket);
    }
    add_totals_contribution(&mut state.totals, after);
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

    state.totals = TotalsState::default();
    for file in state.files.values() {
        add_totals_contribution(&mut state.totals, FileDerivedState::from(file));
    }
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

    #[test]
    fn totals_contribution_updates_detached_totals() {
        let mut totals = TotalsState::default();
        let file = FileDerivedState {
            package_id: PackageId::new_v4(),
            lifecycle_bucket: PackageProgressBucket::Complete,
            size: 20,
            visible_completed_bytes: 15,
            downloaded_network_bytes: 10,
            counts_in_run_totals: true,
        };

        add_totals_contribution(&mut totals, file);

        assert_eq!(
            totals,
            TotalsState {
                run_total_bytes: 20,
                run_completed_bytes: 15,
                run_file_total: 1,
                run_file_completed: 1,
                displayed_network_bytes: 10,
                ..TotalsState::default()
            }
        );
    }

    #[test]
    fn completed_progress_normalization_restores_verified_file_accounting() {
        let mut progress = FileProgressState {
            verified_existing_bytes: 80,
            downloaded_network_bytes: 40,
            visible_completed_bytes: 25,
            verification_origin_complete: true,
            verification_restore_downloaded_network_bytes: 100,
        };

        normalize_completed_file_progress(&mut progress, 60);

        assert_eq!(
            progress,
            FileProgressState {
                downloaded_network_bytes: 60,
                visible_completed_bytes: 60,
                ..FileProgressState::default()
            }
        );
    }
}
