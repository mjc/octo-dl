use std::time::Instant;

use crate::core::model::{
    DesiredState, DownloadState, FileId, FileLifecycle, FileProgressState, FileState, PackageId,
    PackageState, PackageStatus, RuntimeState, SessionRunStatus, UrlId,
};
use crate::core::restart::RestartSnapshot;
use crate::core::session::{FileSnapshot, PackageSnapshot, SessionSnapshotV3};

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
    FileRetryRequested {
        file_id: FileId,
    },
    FileResetRequested {
        file_id: FileId,
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
    PersistSession(SessionSnapshotV3),
    EnqueueUrlResolution { url: UrlId },
    EnqueueFileDownload { file_id: FileId },
    DeleteOutputArtifacts { file_id: FileId, path: String },
    DeleteResumeArtifacts { file_id: FileId, path: String },
    PublishStatusMessage(String),
    PublishViewSnapshot,
}

pub fn reduce(state: &mut DownloadState, event: CoreEvent) -> Vec<CoreEffect> {
    let mut effects = Vec::new();
    match event {
        CoreEvent::UrlSubmitted { url } => {
            if !state.url_order.iter().any(|existing| existing == &url) {
                state.url_order.push(url.clone());
            }
            state
                .packages
                .entry(url.clone())
                .or_insert_with(|| PackageState {
                    id: url.clone(),
                    source_url: url.clone(),
                    display_name: url.clone(),
                    status: PackageStatus::Pending,
                    file_ids: Vec::new(),
                    error: None,
                });
            effects.push(CoreEffect::EnqueueUrlResolution { url });
        }
        CoreEvent::PackageResolved { package } => {
            let package_id = package.id.clone();
            let package_entry =
                state
                    .packages
                    .entry(package_id.clone())
                    .or_insert_with(|| PackageState {
                        id: package_id.clone(),
                        source_url: package.source_url.clone(),
                        display_name: package.display_name.clone(),
                        status: PackageStatus::Pending,
                        file_ids: Vec::new(),
                        error: None,
                    });
            package_entry.source_url = package.source_url.clone();
            package_entry.display_name = package.display_name.clone();
            package_entry.error = package
                .collision
                .as_ref()
                .map(|collision| format!("path collision on {}", collision.file_id));

            if let Some(collision) = package.collision {
                effects.push(CoreEffect::PublishStatusMessage(format!(
                    "Package {} rejected file {} because it collides with {}",
                    collision.incoming_package_id, collision.file_id, collision.existing_package_id
                )));
            }

            for resolved in package.files {
                match state.files.get(&resolved.file_id) {
                    Some(existing) if existing.package_id != package_id => {
                        package_entry.error = Some(format!(
                            "path collision on {} with package {}",
                            resolved.file_id, existing.package_id
                        ));
                    }
                    Some(existing) => {
                        if !package_entry.file_ids.iter().any(|id| id == &existing.id) {
                            package_entry.file_ids.push(existing.id.clone());
                        }
                    }
                    None => {
                        let file = FileState {
                            id: resolved.file_id.clone(),
                            package_id: package_id.clone(),
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
                        package_entry.file_ids.push(resolved.file_id.clone());
                        state.files.insert(resolved.file_id, file);
                    }
                }
            }
        }
        CoreEvent::FileQueued { file_id } => {
            if let Some(file) = state.files.get_mut(&file_id) {
                if !matches!(
                    file.lifecycle,
                    FileLifecycle::Skipped | FileLifecycle::Deleted
                ) {
                    file.lifecycle = FileLifecycle::Queued;
                    file.runtime.active = false;
                    file.message = None;
                }
            }
        }
        CoreEvent::FileStarted { file_id, size } => {
            if let Some(file) = state.files.get_mut(&file_id) {
                if !matches!(
                    file.lifecycle,
                    FileLifecycle::Skipped | FileLifecycle::Deleted
                ) {
                    file.size = size;
                    file.lifecycle = FileLifecycle::Downloading;
                    file.runtime.active = true;
                    file.runtime.counts_in_run_totals = true;
                    file.runtime.preexisting_complete = false;
                }
            }
        }
        CoreEvent::FileProgress {
            file_id,
            total_bytes_delta,
            network_bytes_delta,
        } => {
            if let Some(file) = state.files.get_mut(&file_id) {
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
            }
        }
        CoreEvent::FileReuseDetected {
            file_id,
            reused_bytes,
            reused_chunks,
        } => {
            if let Some(file) = state.files.get_mut(&file_id) {
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
            }
        }
        CoreEvent::FileCompleted { file_id } => {
            if let Some(file) = state.files.get_mut(&file_id) {
                file.lifecycle = FileLifecycle::Complete;
                file.runtime.active = false;
                file.progress.visible_completed_bytes = file.size;
                file.message = None;
            }
        }
        CoreEvent::FileFailed { file_id, message } => {
            if let Some(file) = state.files.get_mut(&file_id) {
                if !file.lifecycle.is_terminal() {
                    file.lifecycle = FileLifecycle::Failed;
                    file.runtime.active = false;
                    file.message = Some(message);
                }
            }
        }
        CoreEvent::FileCancelled { file_id } => {
            if let Some(file) = state.files.get_mut(&file_id) {
                if !file.lifecycle.is_terminal() {
                    file.lifecycle = FileLifecycle::Queued;
                    file.runtime.active = false;
                }
            }
        }
        CoreEvent::FileDeleted { file_id } => {
            if let Some(file) = state.files.get_mut(&file_id) {
                file.lifecycle = FileLifecycle::Deleted;
                file.desired = DesiredState::Suppressed;
                file.runtime.active = false;
                file.runtime.counts_in_run_totals = false;
                effects.push(CoreEffect::DeleteOutputArtifacts {
                    file_id: file.id.clone(),
                    path: file.path.clone(),
                });
                effects.push(CoreEffect::DeleteResumeArtifacts {
                    file_id: file.id.clone(),
                    path: file.path.clone(),
                });
            }
        }
        CoreEvent::FileRetryRequested { file_id } => {
            if let Some(file) = state.files.get_mut(&file_id)
                && matches!(file.lifecycle, FileLifecycle::Failed)
            {
                file.lifecycle = FileLifecycle::Queued;
                file.desired = DesiredState::RetryRequested;
                file.runtime.active = false;
                file.runtime.counts_in_run_totals = true;
                file.progress.visible_completed_bytes = 0;
                file.progress.downloaded_network_bytes = 0;
                file.progress.verified_existing_bytes = 0;
                file.message = None;
                effects.push(CoreEffect::EnqueueFileDownload { file_id });
            }
        }
        CoreEvent::FileResetRequested { file_id } => {
            if let Some(file) = state.files.get_mut(&file_id) {
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
                effects.push(CoreEffect::EnqueueFileDownload { file_id });
            }
        }
        CoreEvent::RestartReconciled { snapshot } => {
            *state = snapshot.state;
            for file_id in snapshot.resume_file_ids {
                effects.push(CoreEffect::EnqueueFileDownload { file_id });
            }
            if !snapshot.legacy_backups.is_empty() {
                effects.push(CoreEffect::PublishStatusMessage(format!(
                    "Legacy sessions were backed up: {}",
                    snapshot.legacy_backups.join(", ")
                )));
            }
        }
        CoreEvent::Tick { now } => {
            let _ = now;
        }
    }

    recompute_derived(state);
    effects.push(CoreEffect::PersistSession(snapshot_from_state(state)));
    effects.push(CoreEffect::PublishViewSnapshot);
    debug_assert_invariants(state);
    effects
}

fn snapshot_from_state(state: &DownloadState) -> SessionSnapshotV3 {
    SessionSnapshotV3 {
        version: 3,
        id: state.session_meta.session_id.clone(),
        created: state.session_meta.created,
        status: state.session_meta.status,
        packages: state
            .packages
            .values()
            .map(|package| PackageSnapshot {
                id: package.id.clone(),
                source_url: package.source_url.clone(),
                display_name: package.display_name.clone(),
                file_ids: package.file_ids.clone(),
                error: package.error.clone(),
            })
            .collect(),
        files: state
            .files
            .values()
            .map(|file| FileSnapshot {
                id: file.id.clone(),
                package_id: file.package_id.clone(),
                path: file.path.clone(),
                size: file.size,
                lifecycle: file.lifecycle,
                progress: file.progress.clone(),
                desired: file.desired,
                runtime: file.runtime.clone(),
                message: file.message.clone(),
            })
            .collect(),
        config: state.session_meta.config.clone(),
        credentials: state.session_meta.credentials.clone(),
    }
}

fn recompute_derived(state: &mut DownloadState) {
    for package in state.packages.values_mut() {
        let mut has_downloading = false;
        let mut has_failed = false;
        let mut has_queued = false;
        let mut has_complete = false;
        let mut has_present = false;
        let mut all_skipped = !package.file_ids.is_empty();
        let mut all_deleted = !package.file_ids.is_empty();

        for file_id in &package.file_ids {
            let Some(file) = state.files.get(file_id) else {
                continue;
            };
            has_present = true;
            match file.lifecycle {
                FileLifecycle::Downloading => has_downloading = true,
                FileLifecycle::Failed => has_failed = true,
                FileLifecycle::Queued | FileLifecycle::Planned => has_queued = true,
                FileLifecycle::Complete => has_complete = true,
                FileLifecycle::Skipped => all_deleted = false,
                FileLifecycle::Deleted => all_skipped = false,
            }
            if !matches!(file.lifecycle, FileLifecycle::Skipped) {
                all_skipped = false;
            }
            if !matches!(file.lifecycle, FileLifecycle::Deleted) {
                all_deleted = false;
            }
        }

        package.status = if package.error.is_some() || has_failed {
            PackageStatus::Failed
        } else if has_downloading {
            PackageStatus::Downloading
        } else if all_deleted && has_present {
            PackageStatus::Deleted
        } else if all_skipped && has_present {
            PackageStatus::Skipped
        } else if has_complete
            && (has_queued
                || package.file_ids.iter().any(|file_id| {
                    state
                        .files
                        .get(file_id)
                        .is_some_and(|file| matches!(file.lifecycle, FileLifecycle::Downloading))
                }))
        {
            PackageStatus::Partial
        } else if has_complete && has_present {
            PackageStatus::Complete
        } else if has_queued {
            PackageStatus::Queued
        } else {
            PackageStatus::Pending
        };
    }

    let mut totals = crate::core::model::TotalsState::default();
    for file in state.files.values() {
        if !file.runtime.counts_in_run_totals
            || file.runtime.preexisting_complete
            || matches!(
                file.lifecycle,
                FileLifecycle::Skipped | FileLifecycle::Deleted
            )
        {
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
    if state
        .files
        .values()
        .all(|file| file.lifecycle.is_terminal())
        && !state.files.is_empty()
    {
        state.session_meta.status = SessionRunStatus::Completed;
    }
}

fn debug_assert_invariants(state: &DownloadState) {
    for (file_id, file) in &state.files {
        debug_assert_eq!(file_id, &file.id, "file map key must equal file.id");
        debug_assert!(
            state.packages.contains_key(&file.package_id),
            "every file must belong to a known package"
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

    fn sample_state() -> DownloadState {
        let mut state = DownloadState::new(crate::core::SessionMeta::default());
        state.packages.insert(
            "pkg".to_string(),
            PackageState {
                id: "pkg".to_string(),
                source_url: "pkg".to_string(),
                display_name: "pkg".to_string(),
                status: PackageStatus::Pending,
                file_ids: vec!["file.bin".to_string()],
                error: None,
            },
        );
        state.files.insert(
            "file.bin".to_string(),
            FileState {
                id: "file.bin".to_string(),
                package_id: "pkg".to_string(),
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
                    id: "pkg".to_string(),
                    source_url: "pkg".to_string(),
                    display_name: "pkg".to_string(),
                    files: vec![
                        ResolvedFile {
                            file_id: "a.bin".to_string(),
                            path: "a.bin".to_string(),
                            size: 10,
                        },
                        ResolvedFile {
                            file_id: "a.bin".to_string(),
                            path: "a.bin".to_string(),
                            size: 10,
                        },
                    ],
                    collision: None,
                },
            },
        );
        assert_eq!(state.files.len(), 1);
        assert_eq!(state.packages["pkg"].file_ids, vec!["a.bin".to_string()]);
    }

    #[test]
    fn terminal_state_does_not_revive_without_reset() {
        let mut state = sample_state();
        reduce(
            &mut state,
            CoreEvent::FileDeleted {
                file_id: "file.bin".to_string(),
            },
        );
        reduce(
            &mut state,
            CoreEvent::FileStarted {
                file_id: "file.bin".to_string(),
                size: 100,
            },
        );
        assert_eq!(state.files["file.bin"].lifecycle, FileLifecycle::Deleted);
    }

    #[test]
    fn delete_emits_cleanup_effects() {
        let mut state = sample_state();
        let effects = reduce(
            &mut state,
            CoreEvent::FileDeleted {
                file_id: "file.bin".to_string(),
            },
        );
        assert!(effects.iter().any(|effect| matches!(
            effect,
            CoreEffect::DeleteOutputArtifacts { file_id, .. } if file_id == "file.bin"
        )));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            CoreEffect::DeleteResumeArtifacts { file_id, .. } if file_id == "file.bin"
        )));
    }

    #[test]
    fn package_status_derives_from_mixed_files() {
        let mut state = DownloadState::default();
        reduce(
            &mut state,
            CoreEvent::PackageResolved {
                package: ResolvedPackage {
                    id: "pkg".to_string(),
                    source_url: "pkg".to_string(),
                    display_name: "pkg".to_string(),
                    files: vec![
                        ResolvedFile {
                            file_id: "done.bin".to_string(),
                            path: "done.bin".to_string(),
                            size: 10,
                        },
                        ResolvedFile {
                            file_id: "todo.bin".to_string(),
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
                file_id: "done.bin".to_string(),
            },
        );
        reduce(
            &mut state,
            CoreEvent::FileQueued {
                file_id: "todo.bin".to_string(),
            },
        );
        assert_eq!(state.packages["pkg"].status, PackageStatus::Partial);
    }

    #[test]
    fn reset_is_the_only_way_back_to_active_from_terminal() {
        let mut state = sample_state();
        reduce(
            &mut state,
            CoreEvent::FileCompleted {
                file_id: "file.bin".to_string(),
            },
        );
        let effects = reduce(
            &mut state,
            CoreEvent::FileResetRequested {
                file_id: "file.bin".to_string(),
            },
        );
        assert_eq!(state.files["file.bin"].lifecycle, FileLifecycle::Queued);
        assert!(effects.iter().any(|effect| matches!(
            effect,
            CoreEffect::EnqueueFileDownload { file_id } if file_id == "file.bin"
        )));
    }
}
