use std::time::Instant;

use crate::core::model::{
    DesiredState, DownloadState, FileId, FileLifecycle, FileProgressState, FileState, PackageId,
    PackageKey, PackageState, PackageStatus, RuntimeState, SessionRunStatus, UrlId,
};
use crate::core::restart::RestartSnapshot;
use crate::core::session::{FileSnapshot, PackageSnapshot, SessionSnapshot};

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
    FileProgress {
        file_id: FileId,
        total_bytes_delta: u64,
        network_bytes_delta: u64,
    },
    FileReuseDetected {
        file_id: FileId,
        reused_bytes: u64,
        reused_chunks: usize,
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
    DeleteOutputArtifacts { file_id: FileId, path: String },
    DeleteResumeArtifacts { file_id: FileId, path: String },
    PublishStatusMessage(String),
    PublishViewSnapshot,
}

fn should_persist_session(event: &CoreEvent) -> bool {
    !matches!(
        event,
        CoreEvent::FileProgress { .. }
            | CoreEvent::FileReuseDetected { .. }
            | CoreEvent::Tick { .. }
    )
}

fn counts_in_run_totals(file: &FileState) -> bool {
    file.runtime.counts_in_run_totals && !file.runtime.preexisting_complete
}

#[derive(Debug, Clone, Copy)]
struct FileDerivedState {
    package_id: PackageId,
    lifecycle: FileLifecycle,
    size: u64,
    visible_completed_bytes: u64,
    downloaded_network_bytes: u64,
    counts_in_run_totals: bool,
}

impl From<&FileState> for FileDerivedState {
    fn from(file: &FileState) -> Self {
        Self {
            package_id: file.package_id,
            lifecycle: file.lifecycle,
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
    if matches!(file.lifecycle, FileLifecycle::Complete) {
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
    if matches!(file.lifecycle, FileLifecycle::Complete) {
        state.totals.run_file_completed = state.totals.run_file_completed.saturating_sub(1);
    }
}

fn insert_file_into_package_index(
    state: &mut DownloadState,
    file_id: &FileId,
    package_id: PackageId,
) {
    if let Some(package) = state.packages.get_mut(&package_id)
        && !package.file_ids.contains(file_id)
    {
        package.file_ids.push(file_id.clone());
    }
}

fn remove_file_from_package_index(
    state: &mut DownloadState,
    file_id: &FileId,
    package_id: PackageId,
) {
    if let Some(package) = state.packages.get_mut(&package_id) {
        let file_ids = &mut package.file_ids;
        if let Some(index) = file_ids.iter().position(|existing| existing == file_id) {
            file_ids.swap_remove(index);
        }
    }
}

fn remove_unreferenced_source_url(state: &mut DownloadState, source_url: &UrlId) {
    if !state
        .files
        .values()
        .any(|file| file.source_url.as_deref() == Some(source_url.as_str()))
    {
        state.url_order.retain(|url| url != source_url);
    }
}

fn package_status_from_files(state: &DownloadState, package_id: PackageId) -> PackageStatus {
    let Some(package) = state.packages.get(&package_id) else {
        return PackageStatus::Pending;
    };
    let file_ids = &package.file_ids;

    let mut has_downloading = false;
    let mut has_failed = false;
    let mut has_queued = false;
    let mut has_complete = false;

    for file_id in file_ids {
        let Some(file) = state.files.get(file_id) else {
            continue;
        };
        match file.lifecycle {
            FileLifecycle::Downloading => has_downloading = true,
            FileLifecycle::Failed => has_failed = true,
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
    } else if has_complete && !file_ids.is_empty() {
        PackageStatus::Complete
    } else if has_queued {
        PackageStatus::Queued
    } else {
        PackageStatus::Pending
    }
}

fn recompute_package_status(state: &mut DownloadState, package_id: PackageId) {
    let status = package_status_from_files(state, package_id);
    if let Some(package) = state.packages.get_mut(&package_id) {
        package.status = status;
    }
}

fn recompute_session_status(state: &mut DownloadState) {
    state.session_meta.status = if !state.files.is_empty()
        && state
            .packages
            .values()
            .all(|package| matches!(package.status, PackageStatus::Complete))
    {
        SessionRunStatus::Completed
    } else {
        SessionRunStatus::InProgress
    };
}

fn apply_file_change(
    state: &mut DownloadState,
    file_id: &FileId,
    before: FileDerivedState,
    after: FileDerivedState,
) {
    remove_totals_contribution(state, before);
    if before.package_id != after.package_id {
        remove_file_from_package_index(state, file_id, before.package_id);
        insert_file_into_package_index(state, file_id, after.package_id);
    }
    add_totals_contribution(state, after);
    recompute_package_status(state, before.package_id);
    if before.package_id != after.package_id {
        recompute_package_status(state, after.package_id);
    }
    recompute_session_status(state);
}

fn insert_file_state(state: &mut DownloadState, file: FileState) {
    let derived = FileDerivedState::from(&file);
    insert_file_into_package_index(state, &file.id, file.package_id);
    add_totals_contribution(state, derived);
    let package_id = file.package_id;
    state.files.insert(file.id.clone(), file);
    recompute_package_status(state, package_id);
    recompute_session_status(state);
}

#[cfg(debug_assertions)]
fn maybe_debug_assert_invariants(state: &DownloadState) {
    debug_assert_invariants(state);
}

#[cfg(not(debug_assertions))]
fn maybe_debug_assert_invariants(_state: &DownloadState) {}

pub fn reduce(state: &mut DownloadState, event: CoreEvent) -> Vec<CoreEffect> {
    let mut effects = Vec::new();
    let persist_session = should_persist_session(&event);
    let mut full_refresh = false;
    match event {
        CoreEvent::UrlSubmitted { url } => {
            if !state.url_order.iter().any(|existing| existing == &url) {
                state.url_order.push(url.clone());
            }
            effects.push(CoreEffect::EnqueueUrlResolution { url });
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
                if persist_session {
                    effects.push(CoreEffect::PersistSession(snapshot_from_state(state)));
                }
                effects.push(CoreEffect::PublishViewSnapshot);
                maybe_debug_assert_invariants(state);
                return effects;
            }
            let incoming_package_id = package.id.clone();
            let mut reassigned_file_ids = Vec::new();
            let previous_package = state
                .packages
                .iter()
                .find(|(_, existing)| existing.key == package.key)
                .map(|(id, existing)| (id.clone(), existing.clone()));

            if let Some((previous_package_id, _)) = previous_package.as_ref()
                && previous_package_id != &incoming_package_id
            {
                if let Some(previous_state) = state.packages.shift_remove(previous_package_id) {
                    reassigned_file_ids = previous_state.file_ids;
                    for file_id in &reassigned_file_ids {
                        if let Some(file) = state.files.get_mut(file_id) {
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
                        file_ids: Vec::new(),
                        status: PackageStatus::Pending,
                        error: None,
                    });
                package_entry.key = package.key.clone();
                if !preserve_display_name {
                    package_entry.display_name = package_display_name;
                }
                for file_id in &reassigned_file_ids {
                    if !package_entry.file_ids.contains(file_id) {
                        package_entry.file_ids.push(file_id.clone());
                    }
                }
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
                            file.source_url = Some(package.source_url.clone());
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
                            source_url: Some(package.source_url.clone()),
                            path: resolved.path,
                            size: resolved.size,
                            lifecycle: FileLifecycle::Planned,
                            progress: FileProgressState::default(),
                            desired: DesiredState::Present,
                            runtime: RuntimeState {
                                counts_in_run_totals: true,
                                ..RuntimeState::default()
                            },
                            message: None,
                        };
                        insert_file_state(state, file);
                    }
                }
            }
            if let Some(package_entry) = state.packages.get_mut(&incoming_package_id) {
                package_entry.error = package_error;
            }
            recompute_package_status(state, incoming_package_id);
            recompute_session_status(state);
        }
        CoreEvent::FileQueued { file_id } => {
            let mut delta = None;
            if let Some(file) = state.files.get_mut(&file_id) {
                let before = FileDerivedState::from(&*file);
                file.lifecycle = FileLifecycle::Queued;
                file.runtime.active = false;
                file.message = None;
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
                file.runtime.active = true;
                file.runtime.counts_in_run_totals = true;
                file.runtime.preexisting_complete = false;
                file.runtime.reused_chunks = 0;
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
                        file.runtime.active = true;
                    }
                }
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
            reused_chunks,
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
                file.runtime.reused_chunks =
                    file.runtime.reused_chunks.saturating_add(reused_chunks);
                let after = FileDerivedState::from(&*file);
                delta = Some((before, after));
            }
            if let Some((before, after)) = delta {
                apply_file_change(state, &file_id, before, after);
            }
        }
        CoreEvent::FileCompleted { file_id } => {
            let mut delta = None;
            if let Some(file) = state.files.get_mut(&file_id) {
                let before = FileDerivedState::from(&*file);
                file.lifecycle = FileLifecycle::Complete;
                file.runtime.active = false;
                file.progress.visible_completed_bytes = file.size;
                file.message = None;
                let after = FileDerivedState::from(&*file);
                delta = Some((before, after));
            }
            if let Some((before, after)) = delta {
                apply_file_change(state, &file_id, before, after);
            }
        }
        CoreEvent::FileFailed { file_id, message } => {
            let mut delta = None;
            if let Some(file) = state.files.get_mut(&file_id) {
                let before = FileDerivedState::from(&*file);
                if !file.lifecycle.is_terminal() {
                    file.lifecycle = FileLifecycle::Failed;
                    file.runtime.active = false;
                    file.message = Some(message);
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
                    file.runtime.active = false;
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
                remove_file_from_package_index(state, &file_id, before.package_id);
                recompute_package_status(state, before.package_id);
                if state
                    .packages
                    .get(&before.package_id)
                    .is_some_and(|package| package.file_ids.is_empty())
                {
                    state.packages.shift_remove(&before.package_id);
                }
                if let Some(source_url) = source_url {
                    remove_unreferenced_source_url(state, &source_url);
                }
                recompute_session_status(state);
            }
        }
        CoreEvent::PackageDeleted { package_id } => {
            if let Some(package) = state.packages.shift_remove(&package_id) {
                let mut removed_source_urls = std::collections::HashSet::new();
                for file_id in package.file_ids {
                    if let Some(file) = state.files.shift_remove(&file_id) {
                        let before = FileDerivedState::from(&file);
                        if let Some(source_url) = file.source_url.clone() {
                            removed_source_urls.insert(source_url);
                        }
                        remove_totals_contribution(state, before);
                    }
                }
                for source_url in removed_source_urls {
                    remove_unreferenced_source_url(state, &source_url);
                }
                recompute_session_status(state);
            }
        }
        CoreEvent::FileRetryRequested { file_id } => {
            let mut delta = None;
            if let Some(file) = state.files.get_mut(&file_id)
                && matches!(file.lifecycle, FileLifecycle::Failed)
            {
                let before = FileDerivedState::from(&*file);
                file.lifecycle = FileLifecycle::Queued;
                file.desired = DesiredState::RetryRequested;
                file.runtime.active = false;
                file.runtime.counts_in_run_totals = true;
                file.progress.visible_completed_bytes = 0;
                file.progress.downloaded_network_bytes = 0;
                file.progress.verified_existing_bytes = 0;
                file.message = None;
                effects.push(CoreEffect::DeleteResumeArtifacts {
                    file_id: file.id.clone(),
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
                file.desired = DesiredState::ResetRequested;
                file.runtime.active = false;
                file.runtime.counts_in_run_totals = true;
                file.runtime.preexisting_complete = false;
                file.progress = FileProgressState::default();
                file.message = None;
                effects.push(CoreEffect::DeleteOutputArtifacts {
                    file_id: file.id.clone(),
                    path: file.path.clone(),
                });
                effects.push(CoreEffect::DeleteResumeArtifacts {
                    file_id: file.id.clone(),
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

pub fn snapshot_from_state(state: &DownloadState) -> SessionSnapshot {
    let mut url_errors = std::collections::HashMap::<UrlId, String>::new();
    for file in state.files.values() {
        let Some(source_url) = file.source_url.as_ref() else {
            continue;
        };
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
    let packages = state
        .packages
        .values()
        .filter_map(|package| {
            let files = package
                .file_ids
                .iter()
                .filter_map(|file_id| state.files.get(file_id))
                .map(|file| FileSnapshot {
                    id: file.id.clone(),
                    package_id: file.package_id.clone(),
                    source_url: file.source_url.clone(),
                    path: file.path.clone(),
                    size: file.size,
                    lifecycle: file.lifecycle,
                    progress: file.progress.clone(),
                    desired: file.desired,
                    runtime: file.runtime.clone(),
                    message: file.message.clone(),
                })
                .collect::<Vec<_>>();
            if files.is_empty() {
                return None;
            }
            Some(PackageSnapshot {
                id: package.id.clone(),
                key: package.key.clone(),
                display_name: package.display_name.clone(),
                files,
                error: package.error.clone(),
            })
        })
        .collect();
    SessionSnapshot {
        version: 5,
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
        files: Vec::new(),
        config: state.session_meta.config.clone(),
        credentials: state.session_meta.credentials.clone(),
    }
}

fn recompute_derived(state: &mut DownloadState) {
    let package_statuses: Vec<_> = state
        .packages
        .keys()
        .copied()
        .map(|package_id| (package_id, package_status_from_files(state, package_id)))
        .collect();
    for (package_id, status) in package_statuses {
        if let Some(package) = state.packages.get_mut(&package_id) {
            package.status = status;
        }
    }

    let mut totals = crate::core::model::TotalsState::default();
    for file in state.files.values() {
        if !file.runtime.counts_in_run_totals || file.runtime.preexisting_complete {
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
    for (package_id, package) in &state.packages {
        debug_assert_eq!(
            package_id, &package.id,
            "package map key must equal package.id"
        );
        debug_assert!(
            package_keys.insert(package.key.clone()),
            "only one package may exist per package key"
        );
        debug_assert!(
            !package.file_ids.is_empty(),
            "packages without files are invalid in canonical state"
        );
        for file_id in &package.file_ids {
            debug_assert!(
                state.files.contains_key(file_id),
                "package file_ids must reference known files"
            );
        }
    }
    for (file_id, file) in &state.files {
        debug_assert_eq!(file_id, &file.id, "file map key must equal file.id");
        debug_assert!(
            state.packages.contains_key(&file.package_id),
            "every file must belong to a known package"
        );
        debug_assert!(
            state
                .packages
                .get(&file.package_id)
                .is_some_and(|package| package.file_ids.contains(file_id)),
            "every file must appear in its package file_ids"
        );
        debug_assert!(
            !matches!(file.lifecycle, FileLifecycle::Complete) || !file.runtime.active,
            "complete files cannot remain active"
        );
        debug_assert!(
            state.totals.displayed_network_bytes <= state.totals.run_completed_bytes,
            "network bytes cannot exceed visible completed bytes"
        );
        if file.runtime.preexisting_complete {
            debug_assert!(
                !file.runtime.counts_in_run_totals,
                "preexisting complete files cannot count in current-run totals"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package_id(raw: &str, source_url: &str) -> PackageId {
        PackageId::parse_or_key(raw, &crate::core::PackageKey::new(source_url))
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
                file_ids: vec!["file.bin".to_string().into()],
                status: PackageStatus::Pending,
                error: None,
            },
        );
        state.files.insert(
            "file.bin".to_string().into(),
            FileState {
                id: "file.bin".to_string().into(),
                package_id: pkg_id,
                source_url: Some("pkg".to_string()),
                path: "file.bin".to_string(),
                size: 100,
                lifecycle: FileLifecycle::Queued,
                progress: FileProgressState::default(),
                desired: DesiredState::Present,
                runtime: RuntimeState {
                    counts_in_run_totals: true,
                    ..RuntimeState::default()
                },
                message: None,
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
    fn package_resolved_with_no_files_does_not_create_package_state() {
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
                file_ids: vec!["a.bin".to_string().into()],
                status: PackageStatus::Queued,
                error: None,
            },
        );
        state.files.insert(
            "a.bin".to_string().into(),
            FileState {
                id: "a.bin".to_string().into(),
                package_id: existing_id,
                source_url: Some("https://mega.nz/folder/test".to_string()),
                path: "folder/a.bin".to_string(),
                size: 10,
                lifecycle: FileLifecycle::Queued,
                progress: FileProgressState::default(),
                desired: DesiredState::Present,
                runtime: RuntimeState {
                    counts_in_run_totals: true,
                    ..RuntimeState::default()
                },
                message: None,
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
                file_ids: vec!["a.bin".to_string().into()],
                status: PackageStatus::Queued,
                error: None,
            },
        );
        state.files.insert(
            "a.bin".to_string().into(),
            FileState {
                id: "a.bin".to_string().into(),
                package_id: existing_id,
                source_url: Some("https://mega.nz/folder/pkg-a".to_string()),
                path: "a.bin".to_string(),
                size: 10,
                lifecycle: FileLifecycle::Failed,
                progress: FileProgressState::default(),
                desired: DesiredState::Present,
                runtime: RuntimeState {
                    counts_in_run_totals: true,
                    ..RuntimeState::default()
                },
                message: Some("boom".to_string()),
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
                file_ids: vec!["a.bin".to_string().into()],
                status: PackageStatus::Queued,
                error: None,
            },
        );
        state.files.insert(
            "a.bin".to_string().into(),
            FileState {
                id: "a.bin".to_string().into(),
                package_id: old_id,
                source_url: Some("https://mega.nz/folder/test".to_string()),
                path: "folder/a.bin".to_string(),
                size: 10,
                lifecycle: FileLifecycle::Queued,
                progress: FileProgressState::default(),
                desired: DesiredState::Present,
                runtime: RuntimeState {
                    counts_in_run_totals: true,
                    ..RuntimeState::default()
                },
                message: None,
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
        assert_eq!(file.runtime.reused_chunks, 0);
        assert_eq!(state.totals.run_completed_bytes, 0);
        assert_eq!(state.totals.displayed_network_bytes, 0);
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
            state.packages[&package_id("pkg", "pkg")].status,
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
            CoreEffect::DeleteResumeArtifacts { file_id, .. } if file_id == "file.bin"
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
