#[cfg(test)]
use std::cell::Cell;
#[cfg(debug_assertions)]
use std::collections::HashMap;
use std::time::Instant;

use crate::core::model::{
    DownloadState, FileAccounting, FileId, FileLifecycle, FileProgressState, FileState,
    FileStateIndex, PackageId, PackageKey, PackageProgressState, PackageState, PackageStatus,
    SessionRunStatus, UrlId,
};
use crate::core::restart::RestartSnapshot;
use crate::core::session::{FileSnapshot, PackageSnapshot, SessionSnapshot};
use smallvec::SmallVec;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFile {
    pub file_id: FileId,
    pub path: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageCollision {
    pub file_id: FileId,
    pub existing_package_id: PackageId,
    pub incoming_package_id: PackageId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPackage {
    pub id: PackageId,
    pub key: PackageKey,
    pub source_url: UrlId,
    pub display_name: String,
    pub files: Vec<ResolvedFile>,
    pub collision: Option<PackageCollision>,
}

#[derive(Debug, Clone)]
pub enum CoreEvent {
    UrlSubmitted {
        url: UrlId,
    },
    UrlResolved {
        url: UrlId,
    },
    UrlFailed {
        url: UrlId,
        message: String,
    },
    PackageResolved {
        package: ResolvedPackage,
    },
    FileQueued {
        file_id: FileId,
    },
    FileStarted {
        file_id: FileId,
        size: u64,
    },
    FileResumeStarted {
        file_id: FileId,
        size: u64,
    },
    FileProgress {
        file_id: FileId,
        total_bytes_delta: u64,
        network_bytes_delta: u64,
    },
    FileVerificationStarted {
        file_id: FileId,
    },
    FileVerificationProgress {
        file_id: FileId,
        bytes_delta: u64,
    },
    FileVerificationCompleted {
        file_id: FileId,
    },
    FileReuseDetected {
        file_id: FileId,
        reused_bytes: u64,
        reused_chunks: usize,
    },
    FileResumeReverified {
        file_id: FileId,
        verified_bytes: u64,
        verified_chunks: usize,
    },
    FileCompleted {
        file_id: FileId,
    },
    FileFailed {
        file_id: FileId,
        message: String,
    },
    FileCancelled {
        file_id: FileId,
    },
    FileDeleted {
        file_id: FileId,
    },
    PackageDeleted {
        package_id: PackageId,
    },
    FileRetryRequested {
        file_id: FileId,
    },
    FileResetRequested {
        file_id: FileId,
    },
    PackageMoveRequested {
        package_id: PackageId,
        delta: isize,
    },
    FileMoveRequested {
        file_id: FileId,
        delta: isize,
    },
    RestartReconciled {
        snapshot: RestartSnapshot,
    },
    Tick {
        now: Instant,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreEffect {
    PersistSession(SessionSnapshot),
    EnqueueUrlResolution { url: UrlId },
    EnqueueFileDownload { file_id: FileId },
    DeleteOutputArtifacts { path: String },
    DeleteResumeArtifacts { path: String },
    PublishStatusMessage(String),
    PublishViewSnapshot,
}

pub type CoreEffects = SmallVec<[CoreEffect; 2]>;

#[cfg(test)]
thread_local! {
    static SNAPSHOT_FROM_STATE_CALLS: Cell<usize> = const { Cell::new(0) };
}

pub(crate) fn should_persist_session(event: &CoreEvent) -> bool {
    !matches!(
        event,
        CoreEvent::FileProgress { .. }
            | CoreEvent::FileQueued { .. }
            | CoreEvent::FileStarted { .. }
            | CoreEvent::FileResumeStarted { .. }
            | CoreEvent::FileReuseDetected { .. }
            | CoreEvent::FileResumeReverified { .. }
            | CoreEvent::FileVerificationStarted { .. }
            | CoreEvent::FileVerificationProgress { .. }
            | CoreEvent::FileVerificationCompleted { .. }
            | CoreEvent::Tick { .. }
    )
}

#[cfg(test)]
pub(crate) fn reset_snapshot_from_state_call_count() {
    SNAPSHOT_FROM_STATE_CALLS.with(|count| count.set(0));
}

#[cfg(test)]
pub(crate) fn snapshot_from_state_call_count() -> usize {
    SNAPSHOT_FROM_STATE_CALLS.with(Cell::get)
}

fn counts_in_run_totals(file: &FileState) -> bool {
    matches!(file.accounting, FileAccounting::CurrentRun)
}

#[derive(Debug, Clone, Copy)]
struct FileDerivedState {
    package_id: PackageId,
    lifecycle_bucket: PackageProgressBucket,
    size: u64,
    visible_completed_bytes: u64,
    downloaded_network_bytes: u64,
    counts_in_run_totals: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackageProgressBucket {
    Queued,
    Downloading,
    Complete,
    Failed,
}

impl PackageProgressBucket {
    const fn from_lifecycle(lifecycle: &FileLifecycle) -> Self {
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

fn add_totals_contribution(state: &mut DownloadState, file: FileDerivedState) {
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
    if matches!(file.lifecycle_bucket, PackageProgressBucket::Complete) {
        state.totals.run_file_completed = state.totals.run_file_completed.saturating_add(1);
    }
}

fn remove_totals_contribution(state: &mut DownloadState, file: FileDerivedState) {
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
    if matches!(file.lifecycle_bucket, PackageProgressBucket::Complete) {
        state.totals.run_file_completed = state.totals.run_file_completed.saturating_sub(1);
    }
}

fn add_package_progress(
    state: &mut DownloadState,
    package_id: PackageId,
    lifecycle_bucket: PackageProgressBucket,
) {
    if let Some(package) = state.packages.get_mut(&package_id) {
        lifecycle_bucket.add_to(&mut package.progress);
    }
}

fn remove_package_progress(
    state: &mut DownloadState,
    package_id: PackageId,
    lifecycle_bucket: PackageProgressBucket,
) {
    if let Some(package) = state.packages.get_mut(&package_id) {
        lifecycle_bucket.remove_from(&mut package.progress);
    }
}

fn remove_unreferenced_source_url(state: &mut DownloadState, source_url: &UrlId) {
    if !state
        .files
        .values()
        .any(|file| file.source_url == *source_url)
    {
        state.url_order.retain(|url| url != source_url);
        state.url_errors.remove(source_url);
    }
}

#[cfg(any(test, debug_assertions))]
fn package_status_from_files(state: &DownloadState, package_id: PackageId) -> PackageStatus {
    let Some(package) = state.packages.get(&package_id) else {
        return PackageStatus::Pending;
    };

    let mut has_downloading = false;
    let mut has_failed = false;
    let mut has_queued = false;
    let mut has_complete = false;
    let mut file_count = 0_usize;

    for file in state.package_files(&package_id) {
        file_count = file_count.saturating_add(1);
        match file.lifecycle {
            FileLifecycle::Downloading => has_downloading = true,
            FileLifecycle::Failed { .. } => has_failed = true,
            FileLifecycle::Queued | FileLifecycle::Planned => has_queued = true,
            FileLifecycle::Complete => has_complete = true,
        }
    }

    if package.error.is_some() || has_failed {
        PackageStatus::Failed
    } else if has_downloading {
        PackageStatus::Downloading
    } else if has_complete && (has_queued || has_downloading) {
        PackageStatus::Partial
    } else if has_complete && file_count > 0 {
        PackageStatus::Complete
    } else if has_queued {
        PackageStatus::Queued
    } else {
        PackageStatus::Pending
    }
}

fn recompute_session_status(state: &mut DownloadState) {
    state.session_meta.status = if !state.files.is_empty()
        && state
            .packages
            .values()
            .all(|package| matches!(package.status(), PackageStatus::Complete))
    {
        SessionRunStatus::Completed
    } else {
        SessionRunStatus::InProgress
    };
}

fn apply_file_change(
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
        recompute_session_status(state);
    }
}

fn complete_file(state: &mut DownloadState, file_id: &FileId) {
    let mut delta = None;
    if let Some(file) = state.files.get_mut(file_id) {
        let before = FileDerivedState::from(&*file);
        file.lifecycle = FileLifecycle::Complete;
        file.progress.visible_completed_bytes = file.size;
        let after = FileDerivedState::from(&*file);
        delta = Some((before, after));
    }
    if let Some((before, after)) = delta {
        apply_file_change(state, file_id, before, after);
    }
}

fn insert_file_state(state: &mut DownloadState, file: FileState) {
    let derived = FileDerivedState::from(&file);
    add_package_progress(state, file.package_id, derived.lifecycle_bucket);
    add_totals_contribution(state, derived);
    let insert_index = state.package_insert_index(&file.package_id);
    state
        .files
        .shift_insert(insert_index, file.id.clone(), file);
    recompute_session_status(state);
}

#[cfg(debug_assertions)]
fn maybe_debug_assert_invariants(state: &DownloadState) {
    debug_assert_invariants(state);
}

#[cfg(not(debug_assertions))]
fn maybe_debug_assert_invariants(_state: &DownloadState) {}

fn reduce_impl(
    state: &mut DownloadState,
    event: CoreEvent,
    emit_persist_session: bool,
) -> CoreEffects {
    let mut effects = CoreEffects::new();
    let persist_session = emit_persist_session && should_persist_session(&event);
    let mut full_refresh = false;
    match event {
        CoreEvent::UrlSubmitted { url } => {
            if !state.url_order.iter().any(|existing| existing == &url) {
                state.url_order.push(url.clone());
            }
            state.url_errors.remove(&url);
            effects.push(CoreEffect::EnqueueUrlResolution { url });
        }
        CoreEvent::UrlResolved { url } => {
            state.url_errors.remove(&url);
            remove_unreferenced_source_url(state, &url);
        }
        CoreEvent::UrlFailed { url, message } => {
            if !state.url_order.iter().any(|existing| existing == &url) {
                state.url_order.push(url.clone());
            }
            state.url_errors.insert(url, message);
        }
        CoreEvent::PackageResolved { package } => {
            if !state
                .url_order
                .iter()
                .any(|existing| existing == &package.source_url)
            {
                state.url_order.push(package.source_url.clone());
            }
            if package.files.is_empty() {
                if let Some(collision) = package.collision {
                    effects.push(CoreEffect::PublishStatusMessage(format!(
                        "Package {} rejected file {} because it collides with {}",
                        collision.incoming_package_id,
                        collision.file_id,
                        collision.existing_package_id
                    )));
                }
                state
                    .packages
                    .retain(|_, existing| existing.key != package.key);
                remove_unreferenced_source_url(state, &package.source_url);
                if persist_session {
                    effects.push(CoreEffect::PersistSession(snapshot_from_state(state)));
                }
                effects.push(CoreEffect::PublishViewSnapshot);
                maybe_debug_assert_invariants(state);
                return effects;
            }
            let incoming_package_id = package.id.clone();
            let mut reassigned_progress = Vec::new();
            let mut reordered_files = false;
            let previous_package = state
                .packages
                .iter()
                .find(|(_, existing)| existing.key == package.key)
                .map(|(id, existing)| (id.clone(), existing.clone()));

            if let Some((previous_package_id, _)) = previous_package.as_ref()
                && previous_package_id != &incoming_package_id
            {
                if let Some(previous_state) = state.packages.shift_remove(previous_package_id) {
                    reordered_files = previous_state.progress.file_count() > 0;
                    for file in state.files.values_mut() {
                        if file.package_id == *previous_package_id {
                            reassigned_progress
                                .push(PackageProgressBucket::from_lifecycle(&file.lifecycle));
                            file.package_id = incoming_package_id;
                        }
                    }
                }
            }

            let preserve_display_name = previous_package
                .as_ref()
                .map(|(_, existing)| existing)
                .is_some_and(|existing| {
                    existing.display_name != existing.key.as_str()
                        && package.display_name == package.source_url
                });
            let package_display_name = if preserve_display_name {
                previous_package
                    .as_ref()
                    .map(|(_, existing)| existing.display_name.clone())
                    .unwrap_or_else(|| package.display_name.clone())
            } else {
                package.display_name.clone()
            };

            let mut package_error = package
                .collision
                .as_ref()
                .map(|collision| format!("path collision on {}", collision.file_id));
            {
                let package_entry = state
                    .packages
                    .entry(incoming_package_id.clone())
                    .or_insert_with(|| PackageState {
                        id: incoming_package_id.clone(),
                        key: package.key.clone(),
                        display_name: package_display_name.clone(),
                        progress: PackageProgressState::default(),
                        error: None,
                    });
                package_entry.key = package.key.clone();
                if !preserve_display_name {
                    package_entry.display_name = package_display_name;
                }
            }
            for lifecycle_bucket in reassigned_progress {
                add_package_progress(state, incoming_package_id, lifecycle_bucket);
            }

            if let Some(collision) = package.collision {
                effects.push(CoreEffect::PublishStatusMessage(format!(
                    "Package {} rejected file {} because it collides with {}",
                    collision.incoming_package_id, collision.file_id, collision.existing_package_id
                )));
            }

            for resolved in package.files {
                let existing_package_id = state
                    .files
                    .get(&resolved.file_id)
                    .map(|existing| existing.package_id.clone());
                match existing_package_id {
                    Some(existing_package_id) if existing_package_id != incoming_package_id => {
                        package_error = Some(format!(
                            "path collision on {} with package {}",
                            resolved.file_id, existing_package_id
                        ));
                    }
                    Some(_) => {
                        let mut delta = None;
                        if let Some(file) = state.files.get_mut(&resolved.file_id) {
                            let before = FileDerivedState::from(&*file);
                            file.source_url = package.source_url.clone();
                            file.path = resolved.path.clone();
                            file.size = resolved.size;
                            let after = FileDerivedState::from(&*file);
                            delta = Some((before, after));
                        }
                        if let Some((before, after)) = delta {
                            apply_file_change(state, &resolved.file_id, before, after);
                        }
                    }
                    None => {
                        let file = FileState {
                            id: resolved.file_id.clone(),
                            package_id: incoming_package_id.clone(),
                            source_url: package.source_url.clone(),
                            path: resolved.path,
                            size: resolved.size,
                            lifecycle: FileLifecycle::Planned,
                            progress: FileProgressState::default(),
                            accounting: FileAccounting::CurrentRun,
                        };
                        insert_file_state(state, file);
                    }
                }
            }
            if let Some(package_entry) = state.packages.get_mut(&incoming_package_id) {
                package_entry.error = package_error;
            }
            if reordered_files {
                state.reorder_files_by_package_order();
            }
            recompute_session_status(state);
        }
        CoreEvent::FileQueued { file_id } => {
            let mut delta = None;
            if let Some(file) = state.files.get_mut(&file_id) {
                let before = FileDerivedState::from(&*file);
                file.lifecycle = FileLifecycle::Queued;
                let after = FileDerivedState::from(&*file);
                delta = Some((before, after));
            }
            if let Some((before, after)) = delta {
                apply_file_change(state, &file_id, before, after);
            }
        }
        CoreEvent::FileStarted { file_id, size } => {
            let mut delta = None;
            if let Some(file) = state.files.get_mut(&file_id) {
                let before = FileDerivedState::from(&*file);
                file.size = size;
                file.lifecycle = FileLifecycle::Downloading;
                file.progress = FileProgressState::default();
                file.accounting = FileAccounting::CurrentRun;
                let after = FileDerivedState::from(&*file);
                delta = Some((before, after));
            }
            if let Some((before, after)) = delta {
                apply_file_change(state, &file_id, before, after);
            }
        }
        CoreEvent::FileResumeStarted { file_id, size } => {
            let mut delta = None;
            if let Some(file) = state.files.get_mut(&file_id) {
                let before = FileDerivedState::from(&*file);
                file.size = size;
                file.lifecycle = FileLifecycle::Downloading;
                file.accounting = FileAccounting::CurrentRun;
                let after = FileDerivedState::from(&*file);
                delta = Some((before, after));
            }
            if let Some((before, after)) = delta {
                apply_file_change(state, &file_id, before, after);
            }
        }
        CoreEvent::FileProgress {
            file_id,
            total_bytes_delta,
            network_bytes_delta,
        } => {
            let mut delta = None;
            if let Some(file) = state.files.get_mut(&file_id) {
                let before = FileDerivedState::from(&*file);
                if matches!(
                    file.lifecycle,
                    FileLifecycle::Downloading | FileLifecycle::Queued
                ) {
                    file.progress.visible_completed_bytes = file
                        .progress
                        .visible_completed_bytes
                        .saturating_add(total_bytes_delta)
                        .min(file.size);
                    file.progress.downloaded_network_bytes = file
                        .progress
                        .downloaded_network_bytes
                        .saturating_add(network_bytes_delta)
                        .min(file.size);
                    if matches!(file.lifecycle, FileLifecycle::Queued) {
                        file.lifecycle = FileLifecycle::Downloading;
                    }
                }
                let after = FileDerivedState::from(&*file);
                delta = Some((before, after));
            }
            if let Some((before, after)) = delta {
                apply_file_change(state, &file_id, before, after);
            }
        }
        CoreEvent::FileVerificationStarted { file_id } => {
            let mut delta = None;
            if let Some(file) = state.files.get_mut(&file_id) {
                let before = FileDerivedState::from(&*file);
                file.lifecycle = FileLifecycle::Queued;
                file.progress.visible_completed_bytes = 0;
                file.progress.verified_existing_bytes = 0;
                file.progress.downloaded_network_bytes = 0;
                let after = FileDerivedState::from(&*file);
                delta = Some((before, after));
            }
            if let Some((before, after)) = delta {
                apply_file_change(state, &file_id, before, after);
            }
        }
        CoreEvent::FileVerificationProgress {
            file_id,
            bytes_delta,
        } => {
            let mut delta = None;
            if let Some(file) = state.files.get_mut(&file_id) {
                let before = FileDerivedState::from(&*file);
                file.progress.visible_completed_bytes = file
                    .progress
                    .visible_completed_bytes
                    .saturating_add(bytes_delta)
                    .min(file.size);
                let after = FileDerivedState::from(&*file);
                delta = Some((before, after));
            }
            if let Some((before, after)) = delta {
                apply_file_change(state, &file_id, before, after);
            }
        }
        CoreEvent::FileReuseDetected {
            file_id,
            reused_bytes,
            reused_chunks: _,
        } => {
            let mut delta = None;
            if let Some(file) = state.files.get_mut(&file_id) {
                let before = FileDerivedState::from(&*file);
                file.progress.verified_existing_bytes = file
                    .progress
                    .verified_existing_bytes
                    .saturating_add(reused_bytes)
                    .min(file.size);
                file.progress.visible_completed_bytes = file
                    .progress
                    .visible_completed_bytes
                    .saturating_add(reused_bytes)
                    .min(file.size);
                let after = FileDerivedState::from(&*file);
                delta = Some((before, after));
            }
            if let Some((before, after)) = delta {
                apply_file_change(state, &file_id, before, after);
            }
        }
        CoreEvent::FileResumeReverified {
            file_id,
            verified_bytes,
            verified_chunks: _,
        } => {
            let mut delta = None;
            if let Some(file) = state.files.get_mut(&file_id) {
                let before = FileDerivedState::from(&*file);
                let verified = verified_bytes.min(file.size);
                file.progress.verified_existing_bytes = verified;
                file.progress.visible_completed_bytes = verified;
                let after = FileDerivedState::from(&*file);
                delta = Some((before, after));
            }
            if let Some((before, after)) = delta {
                apply_file_change(state, &file_id, before, after);
            }
        }
        CoreEvent::FileCompleted { file_id } => {
            complete_file(state, &file_id);
        }
        CoreEvent::FileVerificationCompleted { file_id } => {
            complete_file(state, &file_id);
        }
        CoreEvent::FileFailed { file_id, message } => {
            let mut delta = None;
            if let Some(file) = state.files.get_mut(&file_id) {
                let before = FileDerivedState::from(&*file);
                if !file.lifecycle.is_terminal() {
                    file.lifecycle = FileLifecycle::Failed { message };
                }
                let after = FileDerivedState::from(&*file);
                delta = Some((before, after));
            }
            if let Some((before, after)) = delta {
                apply_file_change(state, &file_id, before, after);
            }
        }
        CoreEvent::FileCancelled { file_id } => {
            let mut delta = None;
            if let Some(file) = state.files.get_mut(&file_id) {
                let before = FileDerivedState::from(&*file);
                if !file.lifecycle.is_terminal() {
                    file.lifecycle = FileLifecycle::Queued;
                }
                let after = FileDerivedState::from(&*file);
                delta = Some((before, after));
            }
            if let Some((before, after)) = delta {
                apply_file_change(state, &file_id, before, after);
            }
        }
        CoreEvent::FileDeleted { file_id } => {
            if let Some(file) = state.files.shift_remove(&file_id) {
                let before = FileDerivedState::from(&file);
                let source_url = file.source_url.clone();
                remove_totals_contribution(state, before);
                remove_package_progress(state, before.package_id, before.lifecycle_bucket);
                if !state.package_has_files(&before.package_id) {
                    state.packages.shift_remove(&before.package_id);
                }
                remove_unreferenced_source_url(state, &source_url);
                recompute_session_status(state);
            }
        }
        CoreEvent::PackageDeleted { package_id } => {
            if state.packages.shift_remove(&package_id).is_some() {
                let mut removed_source_urls = std::collections::HashSet::new();
                let mut remaining_files = FileStateIndex::default();
                for (file_id, file) in std::mem::take(&mut state.files) {
                    if file.package_id == package_id {
                        let before = FileDerivedState::from(&file);
                        removed_source_urls.insert(file.source_url.clone());
                        remove_totals_contribution(state, before);
                    } else {
                        remaining_files.insert(file_id, file);
                    }
                }
                state.files = remaining_files;
                for source_url in removed_source_urls {
                    remove_unreferenced_source_url(state, &source_url);
                }
                recompute_session_status(state);
            }
        }
        CoreEvent::FileRetryRequested { file_id } => {
            let mut delta = None;
            if let Some(file) = state.files.get_mut(&file_id)
                && matches!(file.lifecycle, FileLifecycle::Failed { .. })
            {
                let before = FileDerivedState::from(&*file);
                file.lifecycle = FileLifecycle::Queued;
                file.accounting = FileAccounting::CurrentRun;
                file.progress.visible_completed_bytes = 0;
                file.progress.downloaded_network_bytes = 0;
                file.progress.verified_existing_bytes = 0;
                effects.push(CoreEffect::DeleteResumeArtifacts {
                    path: file.path.clone(),
                });
                effects.push(CoreEffect::EnqueueFileDownload {
                    file_id: file_id.clone(),
                });
                let after = FileDerivedState::from(&*file);
                delta = Some((before, after));
            }
            if let Some((before, after)) = delta {
                apply_file_change(state, &file_id, before, after);
            }
        }
        CoreEvent::FileResetRequested { file_id } => {
            let mut delta = None;
            if let Some(file) = state.files.get_mut(&file_id) {
                let before = FileDerivedState::from(&*file);
                file.lifecycle = FileLifecycle::Queued;
                file.accounting = FileAccounting::CurrentRun;
                file.progress = FileProgressState::default();
                effects.push(CoreEffect::DeleteOutputArtifacts {
                    path: file.path.clone(),
                });
                effects.push(CoreEffect::DeleteResumeArtifacts {
                    path: file.path.clone(),
                });
                effects.push(CoreEffect::EnqueueFileDownload {
                    file_id: file_id.clone(),
                });
                let after = FileDerivedState::from(&*file);
                delta = Some((before, after));
            }
            if let Some((before, after)) = delta {
                apply_file_change(state, &file_id, before, after);
            }
        }
        CoreEvent::PackageMoveRequested { package_id, delta } => {
            let _ = state.move_package_by(&package_id, delta);
        }
        CoreEvent::FileMoveRequested { file_id, delta } => {
            let _ = state.move_file_within_package_by(&file_id, delta);
        }
        CoreEvent::RestartReconciled { snapshot } => {
            *state = snapshot.state;
            full_refresh = true;
            for file_id in snapshot.resume_file_ids {
                effects.push(CoreEffect::EnqueueFileDownload { file_id });
            }
        }
        CoreEvent::Tick { now } => {
            let _ = now;
        }
    }

    if full_refresh {
        recompute_derived(state);
    }
    if persist_session {
        effects.push(CoreEffect::PersistSession(snapshot_from_state(state)));
    }
    effects.push(CoreEffect::PublishViewSnapshot);
    maybe_debug_assert_invariants(state);
    effects
}

pub fn reduce(state: &mut DownloadState, event: CoreEvent) -> CoreEffects {
    reduce_impl(state, event, true)
}

pub(crate) fn reduce_without_session_persist(
    state: &mut DownloadState,
    event: CoreEvent,
) -> CoreEffects {
    reduce_impl(state, event, false)
}

pub(crate) fn rebuild_derived_state(state: &mut DownloadState) {
    recompute_derived(state);
}

pub fn snapshot_from_state(state: &DownloadState) -> SessionSnapshot {
    #[cfg(test)]
    SNAPSHOT_FROM_STATE_CALLS.with(|count| count.set(count.get().saturating_add(1)));
    let mut url_errors = state.url_errors.clone();
    for file in state.files.values() {
        let source_url = &file.source_url;
        if url_errors.contains_key(source_url) {
            continue;
        }
        let Some(package) = state.packages.get(&file.package_id) else {
            continue;
        };
        let Some(error) = package.error.as_ref() else {
            continue;
        };
        url_errors.insert(source_url.clone(), error.clone());
    }
    let package_positions = state.package_positions();
    let mut package_files = state
        .packages
        .values()
        .map(|package| Vec::with_capacity(package.progress.file_count()))
        .collect::<Vec<Vec<&FileState>>>();
    for file in state.files.values() {
        let Some(&package_index) = package_positions.get(&file.package_id) else {
            continue;
        };
        package_files[package_index].push(file);
    }
    let packages = state
        .packages
        .values()
        .zip(package_files)
        .filter_map(|package| {
            let files = package
                .1
                .into_iter()
                .map(|file| FileSnapshot {
                    id: file.id.clone(),
                    package_id: file.package_id,
                    source_url: file.source_url.clone(),
                    path: file.path.clone(),
                    size: file.size,
                    lifecycle: file.lifecycle.clone(),
                    progress: file.progress.clone(),
                    accounting: file.accounting,
                })
                .collect::<Vec<_>>();
            if files.is_empty() {
                return None;
            }
            Some(PackageSnapshot {
                id: package.0.id,
                key: package.0.key.clone(),
                display_name: package.0.display_name.clone(),
                files,
                error: package.0.error.clone(),
            })
        })
        .collect();
    SessionSnapshot {
        version: 6,
        id: state.session_meta.session_id.clone(),
        created: state.session_meta.created,
        status: state.session_meta.status,
        urls: state
            .url_order
            .iter()
            .map(|url| crate::core::SessionUrlSnapshot {
                url: url.clone(),
                error: url_errors.get(url).cloned(),
            })
            .collect(),
        packages,
        config: state.session_meta.config.clone(),
        credentials: state.session_meta.credentials.clone(),
    }
}

fn recompute_derived(state: &mut DownloadState) {
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

    let mut totals = crate::core::model::TotalsState::default();
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
        if matches!(file.lifecycle, FileLifecycle::Complete) {
            totals.run_file_completed = totals.run_file_completed.saturating_add(1);
        }
    }
    state.totals = totals;
    recompute_session_status(state);
}

#[cfg(debug_assertions)]
fn debug_assert_invariants(state: &DownloadState) {
    let mut package_keys = std::collections::HashSet::new();
    let package_positions = state
        .packages
        .keys()
        .enumerate()
        .map(|(index, package_id)| (*package_id, index))
        .collect::<HashMap<_, _, rustc_hash::FxBuildHasher>>();
    let mut file_counts = HashMap::<PackageId, usize, rustc_hash::FxBuildHasher>::with_hasher(
        rustc_hash::FxBuildHasher::default(),
    );
    for (package_id, package) in &state.packages {
        debug_assert_eq!(
            package_id, &package.id,
            "package map key must equal package.id"
        );
        debug_assert!(
            package_keys.insert(package.key.clone()),
            "only one package may exist per package key"
        );
        debug_assert_eq!(
            package.status(),
            package_status_from_files(state, package.id),
            "package progress cache must match fresh file scan"
        );
    }
    let mut last_package_position = None::<usize>;
    for (file_id, file) in &state.files {
        debug_assert_eq!(file_id, &file.id, "file map key must equal file.id");
        debug_assert!(
            state.packages.contains_key(&file.package_id),
            "every file must belong to a known package"
        );
        let package_position = *package_positions
            .get(&file.package_id)
            .expect("files must belong to known package positions");
        debug_assert!(
            last_package_position.is_none_or(|last| package_position >= last),
            "files must stay grouped in package order"
        );
        last_package_position = Some(package_position);
        *file_counts.entry(file.package_id).or_default() += 1;
        debug_assert!(
            state.totals.displayed_network_bytes <= state.totals.run_completed_bytes,
            "network bytes cannot exceed visible completed bytes"
        );
    }
    for package in state.packages.values() {
        debug_assert_eq!(
            package.progress.file_count(),
            file_counts.get(&package.id).copied().unwrap_or(0),
            "package progress count must match files in canonical order"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package_id(raw: &str, source_url: &str) -> PackageId {
        PackageId::parse_or_key(raw, &crate::core::PackageKey::new(source_url))
    }

    fn insert_package(
        state: &mut DownloadState,
        package_id: PackageId,
        source_url: &str,
        files: &[(&str, u64)],
    ) {
        state.packages.insert(
            package_id,
            PackageState {
                id: package_id,
                key: crate::core::PackageKey::new(source_url.to_string()),
                display_name: source_url.to_string(),
                progress: PackageProgressState {
                    queued: files.len(),
                    ..PackageProgressState::default()
                },
                error: None,
            },
        );
        for (file_id, size) in files {
            state.files.insert(
                (*file_id).to_string().into(),
                FileState {
                    id: (*file_id).to_string().into(),
                    package_id,
                    source_url: source_url.to_string(),
                    path: (*file_id).to_string(),
                    size: *size,
                    lifecycle: FileLifecycle::Queued,
                    progress: FileProgressState::default(),
                    accounting: FileAccounting::CurrentRun,
                },
            );
        }
    }

    fn sample_state() -> DownloadState {
        let pkg_id = package_id("pkg", "pkg");
        let mut state = DownloadState::new(crate::core::SessionMeta::default());
        state.packages.insert(
            pkg_id,
            PackageState {
                id: pkg_id,
                key: crate::core::PackageKey::new("pkg".to_string().clone()),
                display_name: "pkg".to_string(),
                progress: PackageProgressState {
                    queued: 1,
                    ..PackageProgressState::default()
                },
                error: None,
            },
        );
        state.files.insert(
            "file.bin".to_string().into(),
            FileState {
                id: "file.bin".to_string().into(),
                package_id: pkg_id,
                source_url: "pkg".to_string(),
                path: "file.bin".to_string(),
                size: 100,
                lifecycle: FileLifecycle::Queued,
                progress: FileProgressState::default(),
                accounting: FileAccounting::CurrentRun,
            },
        );
        state
    }

    #[test]
    fn file_cannot_be_duplicated_by_repeated_events() {
        let mut state = DownloadState::default();
        reduce(
            &mut state,
            CoreEvent::PackageResolved {
                package: ResolvedPackage {
                    id: package_id("pkg", "pkg"),
                    source_url: "pkg".to_string(),
                    key: crate::core::PackageKey::new("pkg".to_string().clone()),
                    display_name: "pkg".to_string(),
                    files: vec![
                        ResolvedFile {
                            file_id: "a.bin".to_string().into(),
                            path: "a.bin".to_string(),
                            size: 10,
                        },
                        ResolvedFile {
                            file_id: "a.bin".to_string().into(),
                            path: "a.bin".to_string(),
                            size: 10,
                        },
                    ],
                    collision: None,
                },
            },
        );
        assert_eq!(state.files.len(), 1);
        assert_eq!(
            state.package_file_ids(&package_id("pkg", "pkg")),
            vec!["a.bin".to_string()]
        );
    }

    #[test]
    fn package_resolved_creates_only_resolved_package_for_submitted_url() {
        let mut state = DownloadState::default();
        reduce(
            &mut state,
            CoreEvent::UrlSubmitted {
                url: "https://mega.nz/folder/test".to_string(),
            },
        );

        reduce(
            &mut state,
            CoreEvent::PackageResolved {
                package: ResolvedPackage {
                    id: package_id("resolved-folder", "https://mega.nz/folder/test"),
                    source_url: "https://mega.nz/folder/test".to_string(),
                    key: crate::core::PackageKey::new(
                        "https://mega.nz/folder/test".to_string().clone(),
                    ),
                    display_name: "Resolved Folder".to_string(),
                    files: vec![ResolvedFile {
                        file_id: "a.bin".to_string().into(),
                        path: "a.bin".to_string(),
                        size: 10,
                    }],
                    collision: None,
                },
            },
        );

        assert!(!state.packages.contains_key(&package_id(
            "https://mega.nz/folder/test",
            "https://mega.nz/folder/test"
        )));
        assert_eq!(state.packages.len(), 1);
        let resolved_id = package_id("resolved-folder", "https://mega.nz/folder/test");
        assert_eq!(
            state.packages[&resolved_id].key.as_str(),
            "https://mega.nz/folder/test"
        );
        assert_eq!(
            state.package_file_ids(&resolved_id),
            vec!["a.bin".to_string()]
        );
        assert_eq!(
            state.url_order,
            vec!["https://mega.nz/folder/test".to_string()]
        );
    }

    #[test]
    fn package_resolved_tracks_source_url_without_prior_submit() {
        let mut state = DownloadState::default();

        reduce(
            &mut state,
            CoreEvent::PackageResolved {
                package: ResolvedPackage {
                    id: package_id("pkg", "https://mega.nz/folder/persist"),
                    source_url: "https://mega.nz/folder/persist".to_string(),
                    key: crate::core::PackageKey::new(
                        "https://mega.nz/folder/persist".to_string().clone(),
                    ),
                    display_name: "Persist".to_string(),
                    files: vec![ResolvedFile {
                        file_id: "episode-1.mkv".to_string().into(),
                        path: "episode-1.mkv".to_string(),
                        size: 128,
                    }],
                    collision: None,
                },
            },
        );

        assert_eq!(
            state.url_order,
            vec!["https://mega.nz/folder/persist".to_string()]
        );
    }

    #[test]
    fn rebuild_derived_state_restores_package_progress_cache() {
        let pkg_id = package_id("pkg", "pkg");
        let mut state = DownloadState::new(crate::core::SessionMeta::default());
        state.packages.insert(
            pkg_id,
            PackageState {
                id: pkg_id,
                key: crate::core::PackageKey::new("pkg"),
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

        rebuild_derived_state(&mut state);

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
    fn package_resolved_with_no_files_does_not_leave_resume_url_state() {
        let mut state = DownloadState::default();
        let effects = reduce(
            &mut state,
            CoreEvent::PackageResolved {
                package: ResolvedPackage {
                    id: package_id("failed-pkg", "https://mega.nz/folder/failed"),
                    source_url: "https://mega.nz/folder/failed".to_string(),
                    key: crate::core::PackageKey::new(
                        "https://mega.nz/folder/failed".to_string().clone(),
                    ),
                    display_name: "Failed package".to_string(),
                    files: Vec::new(),
                    collision: Some(PackageCollision {
                        file_id: "duplicate.bin".to_string().into(),
                        existing_package_id: package_id(
                            "existing",
                            "https://mega.nz/folder/failed",
                        ),
                        incoming_package_id: package_id(
                            "failed-pkg",
                            "https://mega.nz/folder/failed",
                        ),
                    }),
                },
            },
        );

        assert!(state.packages.is_empty());
        assert!(state.files.is_empty());
        assert!(state.url_order.is_empty());
        assert!(effects.iter().any(|effect| matches!(
            effect,
            CoreEffect::PersistSession(snapshot)
                if snapshot.urls.is_empty() && snapshot.packages.is_empty()
        )));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            CoreEffect::PublishStatusMessage(message)
                if message.contains("duplicate.bin")
        )));
    }

    #[test]
    fn package_resolved_emits_persist_effect_by_default() {
        let mut state = DownloadState::default();
        let effects = reduce(
            &mut state,
            CoreEvent::PackageResolved {
                package: ResolvedPackage {
                    id: package_id("pkg", "https://mega.nz/folder/persist"),
                    source_url: "https://mega.nz/folder/persist".to_string(),
                    key: crate::core::PackageKey::new(
                        "https://mega.nz/folder/persist".to_string().clone(),
                    ),
                    display_name: "Persist".to_string(),
                    files: vec![ResolvedFile {
                        file_id: "episode-1.mkv".to_string().into(),
                        path: "episode-1.mkv".to_string(),
                        size: 128,
                    }],
                    collision: None,
                },
            },
        );

        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, CoreEffect::PersistSession(_)))
        );
    }

    #[test]
    fn package_resolved_can_suppress_persist_effect() {
        let mut state = DownloadState::default();
        let effects = reduce_without_session_persist(
            &mut state,
            CoreEvent::PackageResolved {
                package: ResolvedPackage {
                    id: package_id("pkg", "https://mega.nz/folder/persist"),
                    source_url: "https://mega.nz/folder/persist".to_string(),
                    key: crate::core::PackageKey::new(
                        "https://mega.nz/folder/persist".to_string().clone(),
                    ),
                    display_name: "Persist".to_string(),
                    files: vec![ResolvedFile {
                        file_id: "episode-1.mkv".to_string().into(),
                        path: "episode-1.mkv".to_string(),
                        size: 128,
                    }],
                    collision: None,
                },
            },
        );

        assert!(
            effects
                .iter()
                .all(|effect| !matches!(effect, CoreEffect::PersistSession(_)))
        );
        assert_eq!(
            state.url_order,
            vec!["https://mega.nz/folder/persist".to_string()]
        );
        assert_eq!(state.files.len(), 1);
    }

    #[test]
    fn empty_package_resolution_can_suppress_early_persist_effect() {
        let mut state = DownloadState::default();
        let effects = reduce_without_session_persist(
            &mut state,
            CoreEvent::PackageResolved {
                package: ResolvedPackage {
                    id: package_id("failed-pkg", "https://mega.nz/folder/failed"),
                    source_url: "https://mega.nz/folder/failed".to_string(),
                    key: crate::core::PackageKey::new(
                        "https://mega.nz/folder/failed".to_string().clone(),
                    ),
                    display_name: "Failed package".to_string(),
                    files: Vec::new(),
                    collision: Some(PackageCollision {
                        file_id: "duplicate.bin".to_string().into(),
                        existing_package_id: package_id(
                            "existing",
                            "https://mega.nz/folder/failed",
                        ),
                        incoming_package_id: package_id(
                            "failed-pkg",
                            "https://mega.nz/folder/failed",
                        ),
                    }),
                },
            },
        );

        assert!(
            effects
                .iter()
                .all(|effect| !matches!(effect, CoreEffect::PersistSession(_)))
        );
        assert!(effects.iter().any(|effect| matches!(
            effect,
            CoreEffect::PublishStatusMessage(message)
                if message.contains("duplicate.bin")
        )));
    }

    #[test]
    fn package_resolved_reuses_existing_nonempty_package_for_same_source_url() {
        let mut state = DownloadState::default();
        let existing_id = package_id("batch-existing", "https://mega.nz/folder/test");
        state.packages.insert(
            existing_id,
            PackageState {
                id: existing_id,
                key: crate::core::PackageKey::new(
                    "https://mega.nz/folder/test".to_string().clone(),
                ),
                display_name: "Folder".to_string(),
                progress: PackageProgressState {
                    queued: 1,
                    ..PackageProgressState::default()
                },
                error: None,
            },
        );
        state.files.insert(
            "a.bin".to_string().into(),
            FileState {
                id: "a.bin".to_string().into(),
                package_id: existing_id,
                source_url: "https://mega.nz/folder/test".to_string(),
                path: "folder/a.bin".to_string(),
                size: 10,
                lifecycle: FileLifecycle::Queued,
                progress: FileProgressState::default(),
                accounting: FileAccounting::CurrentRun,
            },
        );

        reduce(
            &mut state,
            CoreEvent::PackageResolved {
                package: ResolvedPackage {
                    id: package_id("https://mega.nz/folder/test", "https://mega.nz/folder/test"),
                    source_url: "https://mega.nz/folder/test".to_string(),
                    key: crate::core::PackageKey::new(
                        "https://mega.nz/folder/test".to_string().clone(),
                    ),
                    display_name: "Folder".to_string(),
                    files: vec![ResolvedFile {
                        file_id: "b.bin".to_string().into(),
                        path: "folder/b.bin".to_string(),
                        size: 20,
                    }],
                    collision: None,
                },
            },
        );

        assert_eq!(state.packages.len(), 1);
        let canonical_id = package_id("https://mega.nz/folder/test", "https://mega.nz/folder/test");
        let package = &state.packages[&canonical_id];
        assert_eq!(package.key.as_str(), "https://mega.nz/folder/test");
        assert_eq!(
            state.package_file_ids(&canonical_id),
            vec!["a.bin".to_string(), "b.bin".to_string()]
        );
        assert_eq!(package.display_name, "Folder");
        assert_eq!(state.files["a.bin"].package_id, canonical_id);
        assert_eq!(state.files["b.bin"].package_id, canonical_id);
    }

    #[test]
    fn package_resolved_placeholder_refresh_does_not_clobber_resolved_display_name() {
        let mut state = DownloadState::default();
        let existing_id = package_id("pkg-a", "https://mega.nz/folder/pkg-a");
        state.packages.insert(
            existing_id,
            PackageState {
                id: existing_id,
                key: crate::core::PackageKey::new(
                    "https://mega.nz/folder/pkg-a".to_string().clone(),
                ),
                display_name: "Package A".to_string(),
                progress: PackageProgressState {
                    queued: 1,
                    ..PackageProgressState::default()
                },
                error: None,
            },
        );
        state.files.insert(
            "a.bin".to_string().into(),
            FileState {
                id: "a.bin".to_string().into(),
                package_id: existing_id,
                source_url: "https://mega.nz/folder/pkg-a".to_string(),
                path: "a.bin".to_string(),
                size: 10,
                lifecycle: FileLifecycle::Failed {
                    message: "boom".to_string(),
                },
                progress: FileProgressState::default(),
                accounting: FileAccounting::CurrentRun,
            },
        );

        reduce(
            &mut state,
            CoreEvent::PackageResolved {
                package: ResolvedPackage {
                    id: package_id(
                        "https://mega.nz/folder/pkg-a",
                        "https://mega.nz/folder/pkg-a",
                    ),
                    source_url: "https://mega.nz/folder/pkg-a".to_string(),
                    key: crate::core::PackageKey::new(
                        "https://mega.nz/folder/pkg-a".to_string().clone(),
                    ),
                    display_name: "https://mega.nz/folder/pkg-a".to_string(),
                    files: vec![ResolvedFile {
                        file_id: "a.bin".to_string().into(),
                        path: "a.bin".to_string(),
                        size: 10,
                    }],
                    collision: None,
                },
            },
        );

        assert_eq!(state.packages.len(), 1);
        let canonical_id = package_id(
            "https://mega.nz/folder/pkg-a",
            "https://mega.nz/folder/pkg-a",
        );
        let package = &state.packages[&canonical_id];
        assert_eq!(package.key.as_str(), "https://mega.nz/folder/pkg-a");
        assert_eq!(package.display_name, "Package A");
        assert_eq!(
            state.package_file_ids(&canonical_id),
            vec!["a.bin".to_string()]
        );
    }

    #[test]
    fn package_resolved_reassigns_existing_files_to_new_package_id_for_same_url() {
        let mut state = DownloadState::default();
        let old_id = package_id("https://mega.nz/folder/test", "https://mega.nz/folder/test");
        state.packages.insert(
            old_id,
            PackageState {
                id: old_id,
                key: crate::core::PackageKey::new(
                    "https://mega.nz/folder/test".to_string().clone(),
                ),
                display_name: "https://mega.nz/folder/test".to_string(),
                progress: PackageProgressState {
                    queued: 1,
                    ..PackageProgressState::default()
                },
                error: None,
            },
        );
        state.files.insert(
            "a.bin".to_string().into(),
            FileState {
                id: "a.bin".to_string().into(),
                package_id: old_id,
                source_url: "https://mega.nz/folder/test".to_string(),
                path: "folder/a.bin".to_string(),
                size: 10,
                lifecycle: FileLifecycle::Queued,
                progress: FileProgressState::default(),
                accounting: FileAccounting::CurrentRun,
            },
        );

        reduce(
            &mut state,
            CoreEvent::PackageResolved {
                package: ResolvedPackage {
                    id: package_id("batch-folder", "https://mega.nz/folder/test"),
                    source_url: "https://mega.nz/folder/test".to_string(),
                    key: crate::core::PackageKey::new(
                        "https://mega.nz/folder/test".to_string().clone(),
                    ),
                    display_name: "Folder".to_string(),
                    files: vec![ResolvedFile {
                        file_id: "b.bin".to_string().into(),
                        path: "folder/b.bin".to_string(),
                        size: 20,
                    }],
                    collision: None,
                },
            },
        );

        assert_eq!(state.packages.len(), 1);
        assert!(!state.packages.contains_key(&old_id));
        let batch_id = package_id("batch-folder", "https://mega.nz/folder/test");
        let package = &state.packages[&batch_id];
        assert_eq!(package.key.as_str(), "https://mega.nz/folder/test");
        assert_eq!(package.display_name, "Folder");
        assert_eq!(
            state.package_file_ids(&batch_id),
            vec!["a.bin".to_string(), "b.bin".to_string()]
        );
        assert_eq!(state.files["a.bin"].package_id, batch_id);
        assert_eq!(state.files["b.bin"].package_id, batch_id);
    }

    #[test]
    fn deleted_file_is_removed_from_core_state() {
        let mut state = sample_state();
        reduce(
            &mut state,
            CoreEvent::FileDeleted {
                file_id: "file.bin".to_string().into(),
            },
        );
        assert!(!state.files.contains_key("file.bin"));
        assert!(state.packages.is_empty());
    }

    #[test]
    fn deleting_last_file_removes_source_url_from_resume_state() {
        let mut state = sample_state();
        state.url_order.push("pkg".to_string());

        let effects = reduce(
            &mut state,
            CoreEvent::FileDeleted {
                file_id: "file.bin".to_string().into(),
            },
        );

        assert!(state.url_order.is_empty());
        let saved = effects.iter().find_map(|effect| match effect {
            CoreEffect::PersistSession(snapshot) => Some(snapshot),
            _ => None,
        });
        assert!(saved.is_some_and(|snapshot| snapshot.urls.is_empty()));
    }

    #[test]
    fn deleting_middle_source_url_preserves_other_resume_urls_in_order() {
        let pkg_a = package_id("pkg-a", "url-a");
        let pkg_b = package_id("pkg-b", "url-b");
        let pkg_c = package_id("pkg-c", "url-c");
        let mut state = DownloadState::new(crate::core::SessionMeta::default());
        state.url_order = vec![
            "url-a".to_string(),
            "url-b".to_string(),
            "url-c".to_string(),
        ];
        for (pkg_id, url, file_id) in [
            (pkg_a, "url-a", "a.bin"),
            (pkg_b, "url-b", "b.bin"),
            (pkg_c, "url-c", "c.bin"),
        ] {
            state.packages.insert(
                pkg_id,
                PackageState {
                    id: pkg_id,
                    key: crate::core::PackageKey::new(url.to_string()),
                    display_name: url.to_string(),
                    progress: PackageProgressState {
                        queued: 1,
                        ..PackageProgressState::default()
                    },
                    error: None,
                },
            );
            state.files.insert(
                file_id.to_string().into(),
                FileState {
                    id: file_id.to_string().into(),
                    package_id: pkg_id,
                    source_url: url.to_string(),
                    path: file_id.to_string(),
                    size: 1,
                    lifecycle: FileLifecycle::Queued,
                    progress: FileProgressState::default(),
                    accounting: FileAccounting::CurrentRun,
                },
            );
        }

        let effects = reduce(
            &mut state,
            CoreEvent::FileDeleted {
                file_id: "b.bin".to_string().into(),
            },
        );

        assert_eq!(
            state.url_order,
            vec!["url-a".to_string(), "url-c".to_string()]
        );
        let saved = effects.iter().find_map(|effect| match effect {
            CoreEffect::PersistSession(snapshot) => Some(snapshot),
            _ => None,
        });
        assert_eq!(
            saved
                .expect("delete should persist session")
                .urls
                .iter()
                .map(|url| url.url.as_str())
                .collect::<Vec<_>>(),
            vec!["url-a", "url-c"]
        );
    }

    #[test]
    fn snapshot_from_state_preserves_package_file_order() {
        let pkg_id = package_id("pkg", "pkg");
        let mut state = DownloadState::new(crate::core::SessionMeta::default());
        state.packages.insert(
            pkg_id,
            PackageState {
                id: pkg_id,
                key: crate::core::PackageKey::new("pkg"),
                display_name: "pkg".to_string(),
                progress: PackageProgressState {
                    queued: 2,
                    ..PackageProgressState::default()
                },
                error: None,
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
        state.files.insert(
            "a.bin".into(),
            FileState {
                id: "a.bin".into(),
                package_id: pkg_id,
                source_url: "pkg".to_string(),
                path: "a.bin".to_string(),
                size: 10,
                lifecycle: FileLifecycle::Queued,
                progress: FileProgressState::default(),
                accounting: FileAccounting::CurrentRun,
            },
        );

        let snapshot = snapshot_from_state(&state);
        let files = &snapshot.packages[0].files;
        assert_eq!(
            files
                .iter()
                .map(|file| file.id.as_str())
                .collect::<Vec<_>>(),
            vec!["b.bin", "a.bin"]
        );
    }

    #[test]
    fn package_move_round_trip_updates_url_order_to_match_package_order() {
        let pkg_a = package_id("pkg-a", "url-a");
        let pkg_b = package_id("pkg-b", "url-b");
        let mut state = DownloadState::new(crate::core::SessionMeta::default());
        state.url_order = vec!["url-a".to_string(), "url-b".to_string()];
        insert_package(&mut state, pkg_a, "url-a", &[("a.bin", 10)]);
        insert_package(&mut state, pkg_b, "url-b", &[("b.bin", 20)]);

        reduce(
            &mut state,
            CoreEvent::PackageMoveRequested {
                package_id: pkg_b,
                delta: -1,
            },
        );

        assert_eq!(
            state
                .packages
                .values()
                .map(|package| package.display_name.as_str())
                .collect::<Vec<_>>(),
            vec!["url-b", "url-a"]
        );
        let snapshot = snapshot_from_state(&state);
        assert_eq!(
            snapshot
                .urls
                .iter()
                .map(|entry| entry.url.as_str())
                .collect::<Vec<_>>(),
            vec!["url-b", "url-a"]
        );
        let restart = crate::core::build_restart_snapshot(&snapshot);
        assert_eq!(
            restart.state.url_order,
            vec!["url-b".to_string(), "url-a".to_string()]
        );
        assert_eq!(
            restart
                .state
                .packages
                .values()
                .map(|package| package.display_name.as_str())
                .collect::<Vec<_>>(),
            vec!["url-b", "url-a"]
        );
        assert_eq!(
            restart.state.package_file_ids(&pkg_b),
            vec!["b.bin".to_string()]
        );
        assert_eq!(
            restart.state.package_file_ids(&pkg_a),
            vec!["a.bin".to_string()]
        );
    }

    #[test]
    fn file_move_round_trip_preserves_package_file_order() {
        let pkg_id = package_id("pkg", "url");
        let mut state = DownloadState::new(crate::core::SessionMeta::default());
        state.url_order = vec!["url".to_string()];
        insert_package(
            &mut state,
            pkg_id,
            "url",
            &[("a.bin", 10), ("b.bin", 20), ("c.bin", 30)],
        );

        reduce(
            &mut state,
            CoreEvent::FileMoveRequested {
                file_id: "c.bin".to_string().into(),
                delta: -2,
            },
        );

        assert_eq!(
            state.package_file_ids(&pkg_id),
            vec![
                "c.bin".to_string(),
                "a.bin".to_string(),
                "b.bin".to_string()
            ]
        );
        let snapshot = snapshot_from_state(&state);
        assert_eq!(
            snapshot.packages[0]
                .files
                .iter()
                .map(|file| file.id.as_str())
                .collect::<Vec<_>>(),
            vec!["c.bin", "a.bin", "b.bin"]
        );
        let restart = crate::core::build_restart_snapshot(&snapshot);
        assert_eq!(
            restart.state.package_file_ids(&pkg_id),
            vec![
                "c.bin".to_string(),
                "a.bin".to_string(),
                "b.bin".to_string()
            ]
        );
    }

    #[test]
    fn deleting_reordered_package_preserves_remaining_url_order() {
        let pkg_a = package_id("pkg-a", "url-a");
        let pkg_b = package_id("pkg-b", "url-b");
        let pkg_c = package_id("pkg-c", "url-c");
        let mut state = DownloadState::new(crate::core::SessionMeta::default());
        state.url_order = vec![
            "url-a".to_string(),
            "url-b".to_string(),
            "url-c".to_string(),
        ];
        insert_package(
            &mut state,
            pkg_a,
            "url-a",
            &[("a-1.bin", 10), ("a-2.bin", 11)],
        );
        insert_package(&mut state, pkg_b, "url-b", &[("b.bin", 20)]);
        insert_package(&mut state, pkg_c, "url-c", &[("c.bin", 30)]);

        reduce(
            &mut state,
            CoreEvent::PackageMoveRequested {
                package_id: pkg_c,
                delta: -2,
            },
        );

        let effects = reduce(&mut state, CoreEvent::PackageDeleted { package_id: pkg_a });

        assert_eq!(
            state
                .packages
                .values()
                .map(|package| package.display_name.as_str())
                .collect::<Vec<_>>(),
            vec!["url-c", "url-b"]
        );
        assert_eq!(
            state
                .files
                .keys()
                .map(|file_id| file_id.as_str())
                .collect::<Vec<_>>(),
            vec!["c.bin", "b.bin"]
        );
        assert_eq!(
            state.url_order,
            vec!["url-c".to_string(), "url-b".to_string()]
        );
        let saved = effects.iter().find_map(|effect| match effect {
            CoreEffect::PersistSession(snapshot) => Some(snapshot),
            _ => None,
        });
        assert_eq!(
            saved
                .expect("delete should persist session")
                .urls
                .iter()
                .map(|url| url.url.as_str())
                .collect::<Vec<_>>(),
            vec!["url-c", "url-b"]
        );
    }

    #[test]
    fn deleting_middle_package_preserves_remaining_file_order_and_totals() {
        let pkg_a = package_id("pkg-a", "url-a");
        let pkg_b = package_id("pkg-b", "url-b");
        let pkg_c = package_id("pkg-c", "url-c");
        let mut state = DownloadState::new(crate::core::SessionMeta::default());
        state.url_order = vec![
            "url-a".to_string(),
            "url-b".to_string(),
            "url-c".to_string(),
        ];
        insert_package(
            &mut state,
            pkg_a,
            "url-a",
            &[("a-1.bin", 10), ("a-2.bin", 11)],
        );
        insert_package(
            &mut state,
            pkg_b,
            "url-b",
            &[("b-1.bin", 20), ("b-2.bin", 21)],
        );
        insert_package(&mut state, pkg_c, "url-c", &[("c-1.bin", 30)]);
        rebuild_derived_state(&mut state);
        reduce(
            &mut state,
            CoreEvent::FileCompleted {
                file_id: "a-1.bin".to_string().into(),
            },
        );
        reduce(
            &mut state,
            CoreEvent::FileCompleted {
                file_id: "b-1.bin".to_string().into(),
            },
        );

        reduce(&mut state, CoreEvent::PackageDeleted { package_id: pkg_b });

        assert_eq!(
            state
                .files
                .keys()
                .map(|file_id| file_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a-1.bin", "a-2.bin", "c-1.bin"]
        );
        assert_eq!(
            state.url_order,
            vec!["url-a".to_string(), "url-c".to_string()]
        );
        assert_eq!(state.totals.run_total_bytes, 51);
        assert_eq!(state.totals.run_completed_bytes, 10);
        assert_eq!(state.totals.run_file_total, 3);
        assert_eq!(state.totals.run_file_completed, 1);
    }

    #[test]
    fn snapshot_from_state_handles_distinct_equal_file_ids() {
        let pkg_id = package_id("pkg", "pkg");
        let mut state = DownloadState::new(crate::core::SessionMeta::default());
        let state_file_id = FileId::from(String::from("file.bin"));
        state.packages.insert(
            pkg_id,
            PackageState {
                id: pkg_id,
                key: crate::core::PackageKey::new("pkg"),
                display_name: "pkg".to_string(),
                progress: PackageProgressState {
                    queued: 1,
                    ..PackageProgressState::default()
                },
                error: None,
            },
        );
        state.files.insert(
            state_file_id.clone(),
            FileState {
                id: state_file_id,
                package_id: pkg_id,
                source_url: "pkg".to_string(),
                path: "file.bin".to_string(),
                size: 10,
                lifecycle: FileLifecycle::Queued,
                progress: FileProgressState::default(),
                accounting: FileAccounting::CurrentRun,
            },
        );

        let snapshot = snapshot_from_state(&state);
        assert_eq!(snapshot.packages.len(), 1);
        assert_eq!(snapshot.packages[0].files.len(), 1);
        assert_eq!(snapshot.packages[0].files[0].id, "file.bin");
    }

    #[test]
    fn starting_file_resets_stale_progress_from_previous_attempt() {
        let mut state = sample_state();
        reduce(
            &mut state,
            CoreEvent::FileProgress {
                file_id: "file.bin".to_string().into(),
                total_bytes_delta: 100,
                network_bytes_delta: 100,
            },
        );
        reduce(
            &mut state,
            CoreEvent::FileCompleted {
                file_id: "file.bin".to_string().into(),
            },
        );
        reduce(
            &mut state,
            CoreEvent::FileResetRequested {
                file_id: "file.bin".to_string().into(),
            },
        );

        reduce(
            &mut state,
            CoreEvent::FileStarted {
                file_id: "file.bin".to_string().into(),
                size: 100,
            },
        );

        let file = &state.files["file.bin"];
        assert_eq!(file.lifecycle, FileLifecycle::Downloading);
        assert_eq!(file.progress, FileProgressState::default());
        assert_eq!(state.totals.run_completed_bytes, 0);
    }

    #[test]
    fn starting_file_resets_stale_resume_reuse_progress() {
        let mut state = sample_state();
        reduce(
            &mut state,
            CoreEvent::FileReuseDetected {
                file_id: "file.bin".to_string().into(),
                reused_bytes: 80,
                reused_chunks: 2,
            },
        );

        reduce(
            &mut state,
            CoreEvent::FileStarted {
                file_id: "file.bin".to_string().into(),
                size: 100,
            },
        );

        let file = &state.files["file.bin"];
        assert_eq!(file.progress.verified_existing_bytes, 0);
        assert_eq!(file.progress.visible_completed_bytes, 0);
        assert_eq!(file.progress.downloaded_network_bytes, 0);
        assert_eq!(state.totals.run_completed_bytes, 0);
        assert_eq!(state.totals.displayed_network_bytes, 0);
    }

    #[test]
    fn verification_started_resets_visible_progress_without_persisting() {
        let mut state = sample_state();
        reduce(
            &mut state,
            CoreEvent::FileStarted {
                file_id: "file.bin".to_string().into(),
                size: 100,
            },
        );
        reduce(
            &mut state,
            CoreEvent::FileReuseDetected {
                file_id: "file.bin".to_string().into(),
                reused_bytes: 60,
                reused_chunks: 1,
            },
        );
        reduce(
            &mut state,
            CoreEvent::FileProgress {
                file_id: "file.bin".to_string().into(),
                total_bytes_delta: 20,
                network_bytes_delta: 20,
            },
        );

        let effects = reduce(
            &mut state,
            CoreEvent::FileVerificationStarted {
                file_id: "file.bin".to_string().into(),
            },
        );

        let file = &state.files["file.bin"];
        assert_eq!(file.lifecycle, FileLifecycle::Queued);
        assert_eq!(file.progress.visible_completed_bytes, 0);
        assert_eq!(file.progress.verified_existing_bytes, 0);
        assert_eq!(file.progress.downloaded_network_bytes, 0);
        assert_eq!(state.totals.run_completed_bytes, 0);
        assert_eq!(state.totals.displayed_network_bytes, 0);
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, CoreEffect::PersistSession(_)))
        );
    }

    #[test]
    fn verification_progress_advances_visible_bytes_without_network_or_lifecycle_change() {
        let mut state = sample_state();
        reduce(
            &mut state,
            CoreEvent::FileVerificationStarted {
                file_id: "file.bin".to_string().into(),
            },
        );

        let effects = reduce(
            &mut state,
            CoreEvent::FileVerificationProgress {
                file_id: "file.bin".to_string().into(),
                bytes_delta: 45,
            },
        );

        let file = &state.files["file.bin"];
        assert_eq!(file.lifecycle, FileLifecycle::Queued);
        assert_eq!(file.progress.visible_completed_bytes, 45);
        assert_eq!(file.progress.verified_existing_bytes, 0);
        assert_eq!(file.progress.downloaded_network_bytes, 0);
        assert_eq!(state.totals.run_completed_bytes, 45);
        assert_eq!(state.totals.displayed_network_bytes, 0);
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, CoreEffect::PersistSession(_)))
        );
    }

    #[test]
    fn verification_completed_restores_complete_state_without_persisting() {
        let mut state = sample_state();
        reduce(
            &mut state,
            CoreEvent::FileCompleted {
                file_id: "file.bin".to_string().into(),
            },
        );
        reduce(
            &mut state,
            CoreEvent::FileVerificationStarted {
                file_id: "file.bin".to_string().into(),
            },
        );

        let effects = reduce(
            &mut state,
            CoreEvent::FileVerificationCompleted {
                file_id: "file.bin".to_string().into(),
            },
        );

        let file = &state.files["file.bin"];
        assert_eq!(file.lifecycle, FileLifecycle::Complete);
        assert_eq!(file.progress.visible_completed_bytes, file.size);
        assert_eq!(state.totals.run_completed_bytes, file.size);
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, CoreEffect::PersistSession(_)))
        );
    }

    #[test]
    fn normal_file_completed_still_persists_session() {
        let mut state = sample_state();

        let effects = reduce(
            &mut state,
            CoreEvent::FileCompleted {
                file_id: "file.bin".to_string().into(),
            },
        );

        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, CoreEffect::PersistSession(_)))
        );
    }

    #[test]
    fn reuse_then_fresh_progress_does_not_double_count_reused_bytes() {
        let mut state = sample_state();
        reduce(
            &mut state,
            CoreEvent::FileStarted {
                file_id: "file.bin".to_string().into(),
                size: 100,
            },
        );
        reduce(
            &mut state,
            CoreEvent::FileReuseDetected {
                file_id: "file.bin".to_string().into(),
                reused_bytes: 40,
                reused_chunks: 1,
            },
        );
        reduce(
            &mut state,
            CoreEvent::FileProgress {
                file_id: "file.bin".to_string().into(),
                total_bytes_delta: 25,
                network_bytes_delta: 25,
            },
        );

        let file = &state.files["file.bin"];
        assert_eq!(file.progress.visible_completed_bytes, 65);
        assert_eq!(file.progress.verified_existing_bytes, 40);
        assert_eq!(file.progress.downloaded_network_bytes, 25);
        assert_eq!(state.totals.run_completed_bytes, 65);
        assert_eq!(state.totals.displayed_network_bytes, 25);
    }

    #[test]
    fn delete_forgets_file_without_cleanup_effects() {
        let mut state = sample_state();
        let effects = reduce(
            &mut state,
            CoreEvent::FileDeleted {
                file_id: "file.bin".to_string().into(),
            },
        );
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            CoreEffect::DeleteOutputArtifacts { .. } | CoreEffect::DeleteResumeArtifacts { .. }
        )));
    }

    #[test]
    fn deleting_completed_file_keeps_artifacts() {
        let mut state = sample_state();
        reduce(
            &mut state,
            CoreEvent::FileCompleted {
                file_id: "file.bin".to_string().into(),
            },
        );

        let effects = reduce(
            &mut state,
            CoreEvent::FileDeleted {
                file_id: "file.bin".to_string().into(),
            },
        );

        assert!(!effects.iter().any(|effect| matches!(
            effect,
            CoreEffect::DeleteOutputArtifacts { .. } | CoreEffect::DeleteResumeArtifacts { .. }
        )));
    }

    #[test]
    fn package_status_derives_from_mixed_files() {
        let mut state = DownloadState::default();
        reduce(
            &mut state,
            CoreEvent::PackageResolved {
                package: ResolvedPackage {
                    id: package_id("pkg", "pkg"),
                    source_url: "pkg".to_string(),
                    key: crate::core::PackageKey::new("pkg".to_string().clone()),
                    display_name: "pkg".to_string(),
                    files: vec![
                        ResolvedFile {
                            file_id: "done.bin".to_string().into(),
                            path: "done.bin".to_string(),
                            size: 10,
                        },
                        ResolvedFile {
                            file_id: "todo.bin".to_string().into(),
                            path: "todo.bin".to_string(),
                            size: 10,
                        },
                    ],
                    collision: None,
                },
            },
        );
        reduce(
            &mut state,
            CoreEvent::FileCompleted {
                file_id: "done.bin".to_string().into(),
            },
        );
        reduce(
            &mut state,
            CoreEvent::FileQueued {
                file_id: "todo.bin".to_string().into(),
            },
        );
        assert_eq!(
            state.packages[&package_id("pkg", "pkg")].status(),
            PackageStatus::Partial
        );
    }

    #[test]
    fn reset_is_the_only_way_back_to_active_from_terminal() {
        let mut state = sample_state();
        reduce(
            &mut state,
            CoreEvent::FileCompleted {
                file_id: "file.bin".to_string().into(),
            },
        );
        let effects = reduce(
            &mut state,
            CoreEvent::FileResetRequested {
                file_id: "file.bin".to_string().into(),
            },
        );
        assert_eq!(state.files["file.bin"].lifecycle, FileLifecycle::Queued);
        assert!(effects.iter().any(|effect| matches!(
            effect,
            CoreEffect::EnqueueFileDownload { file_id } if file_id == "file.bin"
        )));
    }

    #[test]
    fn retry_failed_file_discards_resume_artifacts_before_requeue() {
        let mut state = sample_state();
        reduce(
            &mut state,
            CoreEvent::FileFailed {
                file_id: "file.bin".to_string().into(),
                message: "corrupt".to_string(),
            },
        );

        let effects = reduce(
            &mut state,
            CoreEvent::FileRetryRequested {
                file_id: "file.bin".to_string().into(),
            },
        );

        assert_eq!(state.files["file.bin"].lifecycle, FileLifecycle::Queued);
        assert!(effects.iter().any(|effect| matches!(
            effect,
            CoreEffect::DeleteResumeArtifacts { path } if path == "file.bin"
        )));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            CoreEffect::EnqueueFileDownload { file_id } if file_id == "file.bin"
        )));
    }

    #[test]
    fn progress_events_do_not_emit_session_persist_effect() {
        let mut state = sample_state();
        let effects = reduce(
            &mut state,
            CoreEvent::FileProgress {
                file_id: "file.bin".to_string().into(),
                total_bytes_delta: 10,
                network_bytes_delta: 10,
            },
        );

        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, CoreEffect::PersistSession(..)))
        );
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, CoreEffect::PublishViewSnapshot))
        );
    }

    #[test]
    fn file_started_events_do_not_emit_session_persist_effect() {
        let mut state = sample_state();
        let effects = reduce(
            &mut state,
            CoreEvent::FileStarted {
                file_id: "file.bin".to_string().into(),
                size: 100,
            },
        );

        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, CoreEffect::PersistSession(..)))
        );
    }

    #[test]
    fn file_queued_events_do_not_emit_session_persist_effect() {
        let mut state = sample_state();
        let effects = reduce(
            &mut state,
            CoreEvent::FileQueued {
                file_id: "file.bin".to_string().into(),
            },
        );

        assert_eq!(state.files["file.bin"].lifecycle, FileLifecycle::Queued);
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, CoreEffect::PersistSession(..)))
        );
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, CoreEffect::PublishViewSnapshot))
        );
    }

    #[test]
    fn resume_reuse_events_do_not_emit_session_persist_effect() {
        let mut state = sample_state();
        let effects = reduce(
            &mut state,
            CoreEvent::FileReuseDetected {
                file_id: "file.bin".to_string().into(),
                reused_bytes: 10,
                reused_chunks: 1,
            },
        );

        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, CoreEffect::PersistSession(..)))
        );
    }
}
