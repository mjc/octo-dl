use std::time::Instant;

use crate::core::model::{
    DesiredState, DownloadState, FileId, FileLifecycle, FileProgressState, FileState, PackageId,
    PackageKey, PackageState, PackageStatus, RuntimeState, SessionRunStatus, UrlId,
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

fn should_persist_session(event: &CoreEvent) -> bool {
    !matches!(
        event,
        CoreEvent::FileProgress { .. }
            | CoreEvent::FileReuseDetected { .. }
            | CoreEvent::Tick { .. }
    )
}

pub fn reduce(state: &mut DownloadState, event: CoreEvent) -> Vec<CoreEffect> {
    let mut effects = Vec::new();
    let persist_session = should_persist_session(&event);
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
                recompute_derived(state);
                if persist_session {
                    effects.push(CoreEffect::PersistSession(snapshot_from_state(state)));
                }
                effects.push(CoreEffect::PublishViewSnapshot);
                debug_assert_invariants(state);
                return effects;
            }
            let incoming_package_id = package.id.clone();
            let previous_package = state
                .packages
                .iter()
                .find(|(_, existing)| existing.key == package.key)
                .map(|(id, existing)| (id.clone(), existing.clone()));

            if let Some((previous_package_id, _)) = previous_package.as_ref()
                && previous_package_id != &incoming_package_id
            {
                state.packages.shift_remove(previous_package_id);
                for file in state.files.values_mut() {
                    if &file.package_id == previous_package_id {
                        file.package_id = incoming_package_id.clone();
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

            let package_entry = state
                .packages
                .entry(incoming_package_id.clone())
                .or_insert_with(|| PackageState {
                    id: incoming_package_id.clone(),
                    key: package.key.clone(),
                    display_name: package_display_name.clone(),
                    status: PackageStatus::Pending,
                    error: None,
                });
            package_entry.key = package.key.clone();
            if !preserve_display_name {
                package_entry.display_name = package_display_name;
            }
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
                let existing_package_id = state
                    .files
                    .get(&resolved.file_id)
                    .map(|existing| existing.package_id.clone());
                match existing_package_id {
                    Some(existing_package_id) if existing_package_id != incoming_package_id => {
                        package_entry.error = Some(format!(
                            "path collision on {} with package {}",
                            resolved.file_id, existing_package_id
                        ));
                    }
                    Some(_) => {
                        if let Some(file) = state.files.get_mut(&resolved.file_id) {
                            file.source_url = Some(package.source_url.clone());
                            file.path = resolved.path.clone();
                            file.size = resolved.size;
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
                effects.push(CoreEffect::DeleteResumeArtifacts {
                    file_id: file.id.clone(),
                    path: file.path.clone(),
                });
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
        }
        CoreEvent::Tick { now } => {
            let _ = now;
        }
    }

    recompute_derived(state);
    if persist_session {
        effects.push(CoreEffect::PersistSession(snapshot_from_state(state)));
    }
    effects.push(CoreEffect::PublishViewSnapshot);
    debug_assert_invariants(state);
    effects
}

pub fn snapshot_from_state(state: &DownloadState) -> SessionSnapshotV3 {
    let packages = state
        .packages
        .values()
        .filter_map(|package| {
            let file_ids = state.package_file_ids(&package.id);
            if file_ids.is_empty() {
                return None;
            }
            Some(PackageSnapshot {
                id: package.id.clone(),
                key: package.key.clone(),
                display_name: package.display_name.clone(),
                file_ids,
                error: package.error.clone(),
            })
        })
        .collect();
    SessionSnapshotV3 {
        version: 4,
        id: state.session_meta.session_id.clone(),
        created: state.session_meta.created,
        status: state.session_meta.status,
        urls: state
            .url_order
            .iter()
            .map(|url| crate::core::SessionUrlSnapshot {
                url: url.clone(),
                error: state
                    .packages
                    .values()
                    .find(|package| package_has_source_url(state, &package.id, url))
                    .and_then(|package| package.error.clone()),
            })
            .collect(),
        packages,
        files: state
            .files
            .values()
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
            .collect(),
        config: state.session_meta.config.clone(),
        credentials: state.session_meta.credentials.clone(),
    }
}

fn package_has_source_url(state: &DownloadState, package_id: &PackageId, source_url: &str) -> bool {
    state.files.values().any(|file| {
        &file.package_id == package_id && file.source_url.as_deref() == Some(source_url)
    })
}

fn recompute_derived(state: &mut DownloadState) {
    let package_ids: Vec<_> = state.packages.keys().cloned().collect();
    for package_id in package_ids {
        let file_ids = state.package_file_ids(&package_id);
        let Some(package) = state.packages.get_mut(&package_id) else {
            continue;
        };
        let mut has_downloading = false;
        let mut has_failed = false;
        let mut has_queued = false;
        let mut has_complete = false;
        let mut has_present = false;
        let mut all_skipped = !file_ids.is_empty();
        let mut all_deleted = !file_ids.is_empty();

        for file_id in &file_ids {
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
                || file_ids.iter().any(|file_id| {
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
        let file_ids = state.package_file_ids(package_id);
        debug_assert!(
            !file_ids.is_empty(),
            "packages without files are invalid in canonical state"
        );
    }
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
                status: PackageStatus::Pending,
                error: None,
            },
        );
        state.files.insert(
            "file.bin".to_string(),
            FileState {
                id: "file.bin".to_string(),
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
                        file_id: "a.bin".to_string(),
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
                        file_id: "episode-1.mkv".to_string(),
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
                        file_id: "duplicate.bin".to_string(),
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
                status: PackageStatus::Queued,
                error: None,
            },
        );
        state.files.insert(
            "a.bin".to_string(),
            FileState {
                id: "a.bin".to_string(),
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
                        file_id: "b.bin".to_string(),
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
                status: PackageStatus::Queued,
                error: None,
            },
        );
        state.files.insert(
            "a.bin".to_string(),
            FileState {
                id: "a.bin".to_string(),
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
                        file_id: "a.bin".to_string(),
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
                status: PackageStatus::Queued,
                error: None,
            },
        );
        state.files.insert(
            "a.bin".to_string(),
            FileState {
                id: "a.bin".to_string(),
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
                        file_id: "b.bin".to_string(),
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
                    id: package_id("pkg", "pkg"),
                    source_url: "pkg".to_string(),
                    key: crate::core::PackageKey::new("pkg".to_string().clone()),
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

    #[test]
    fn retry_failed_file_discards_resume_artifacts_before_requeue() {
        let mut state = sample_state();
        reduce(
            &mut state,
            CoreEvent::FileFailed {
                file_id: "file.bin".to_string(),
                message: "corrupt".to_string(),
            },
        );

        let effects = reduce(
            &mut state,
            CoreEvent::FileRetryRequested {
                file_id: "file.bin".to_string(),
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
                file_id: "file.bin".to_string(),
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
                file_id: "file.bin".to_string(),
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
