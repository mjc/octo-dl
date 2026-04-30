use std::collections::{HashMap, HashSet};
use std::path::Path;

use chrono::Utc;
use indexmap::IndexMap;

use crate::core::model::{
    DesiredState, DownloadState, FileId, FileLifecycle, FileProgressState, FileState, PackageId,
    PackageState, PackageStatus, RuntimeState, SessionMeta, UrlId,
};
use crate::core::session::SessionSnapshotV3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesystemFile {
    pub file_id: FileId,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartialFileSnapshot {
    pub file_id: FileId,
    pub bytes: u64,
    pub has_sidecar: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FilesystemSnapshot {
    pub complete_files: Vec<FilesystemFile>,
    pub partial_files: Vec<PartialFileSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartSnapshot {
    pub state: DownloadState,
    pub resume_file_ids: Vec<FileId>,
    pub preexisting_complete_file_ids: Vec<FileId>,
    pub suppressed_file_ids: Vec<FileId>,
    pub legacy_backups: Vec<String>,
}

impl RestartSnapshot {
    #[must_use]
    pub fn resumable_urls(&self) -> Vec<UrlId> {
        self.state
            .packages
            .values()
            .filter(|package| {
                package.error.is_none()
                    && (package.file_ids.is_empty()
                        || package.file_ids.iter().any(|file_id| {
                            self.state.files.get(file_id).is_some_and(|file| {
                                !matches!(
                                    file.lifecycle,
                                    FileLifecycle::Complete
                                        | FileLifecycle::Skipped
                                        | FileLifecycle::Deleted
                                )
                            })
                        }))
            })
            .map(|package| package.source_url.clone())
            .collect()
    }
}

#[must_use]
pub fn scan_filesystem<I, S>(file_ids: I) -> FilesystemSnapshot
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut snapshot = FilesystemSnapshot::default();
    let mut seen = HashSet::<String>::new();
    for file_id in file_ids {
        let file_id = file_id.as_ref().to_string();
        if !seen.insert(file_id.clone()) {
            continue;
        }
        let path = Path::new(&file_id);
        if let Ok(metadata) = std::fs::metadata(path)
            && metadata.is_file()
        {
            snapshot.complete_files.push(FilesystemFile {
                file_id: file_id.clone(),
                size: metadata.len(),
            });
            continue;
        }

        let part_path = crate::download::part_path(&file_id);
        if let Ok(metadata) = std::fs::metadata(&part_path)
            && metadata.is_file()
        {
            snapshot.partial_files.push(PartialFileSnapshot {
                file_id: file_id.clone(),
                bytes: metadata.len(),
                has_sidecar: crate::download::sidecar_path(&file_id).exists(),
            });
        }
    }
    snapshot
}

#[derive(Clone)]
struct CollapsedFile {
    file: FileState,
    precedence: usize,
}

pub fn reconcile_restart(
    session: Option<SessionSnapshotV3>,
    fs: FilesystemSnapshot,
    urls: Vec<UrlId>,
) -> RestartSnapshot {
    let mut state = DownloadState::new(SessionMeta {
        session_id: session
            .as_ref()
            .map_or_else(|| uuid::Uuid::new_v4().to_string(), |snap| snap.id.clone()),
        created: session.as_ref().map_or_else(Utc::now, |snap| snap.created),
        status: session
            .as_ref()
            .map_or(crate::core::model::SessionRunStatus::InProgress, |snap| {
                snap.status
            }),
        config: session
            .as_ref()
            .map_or_else(crate::config::DownloadConfig::default, |snap| {
                snap.config.clone()
            }),
        credentials: session.as_ref().map_or_else(
            || crate::core::session::SavedCredentials::encrypt("", "", None),
            |snap| snap.credentials.clone(),
        ),
    });

    let mut packages = IndexMap::<PackageId, PackageState>::new();
    let mut files = IndexMap::<FileId, FileState>::new();
    let mut resume_file_ids = Vec::new();
    let mut preexisting_complete_file_ids = Vec::new();
    let mut suppressed_file_ids = Vec::new();

    let complete_map: HashMap<_, _> = fs
        .complete_files
        .into_iter()
        .map(|file| (file.file_id, file.size))
        .collect();
    let partial_map: HashMap<_, _> = fs
        .partial_files
        .into_iter()
        .map(|file| (file.file_id.clone(), file))
        .collect();

    if let Some(snapshot) = session {
        for package in snapshot.packages {
            packages.insert(
                package.id.clone(),
                PackageState {
                    id: package.id.clone(),
                    source_url: package.source_url.clone(),
                    display_name: package.display_name.clone(),
                    status: PackageStatus::Pending,
                    file_ids: package.file_ids.clone(),
                    error: package.error.clone(),
                },
            );
        }

        let mut collapsed = HashMap::<FileId, CollapsedFile>::new();
        for file in snapshot.files {
            let precedence = precedence(file.lifecycle);
            let entry = collapsed
                .entry(file.id.clone())
                .or_insert_with(|| CollapsedFile {
                    file: FileState {
                        id: file.id.clone(),
                        package_id: file.package_id.clone(),
                        path: file.path.clone(),
                        size: file.size,
                        lifecycle: file.lifecycle,
                        progress: file.progress.clone(),
                        desired: file.desired,
                        runtime: file.runtime.clone(),
                        message: file.message.clone(),
                    },
                    precedence,
                });
            if precedence < entry.precedence {
                *entry = CollapsedFile {
                    file: FileState {
                        id: file.id.clone(),
                        package_id: file.package_id.clone(),
                        path: file.path.clone(),
                        size: file.size,
                        lifecycle: file.lifecycle,
                        progress: file.progress.clone(),
                        desired: file.desired,
                        runtime: file.runtime.clone(),
                        message: file.message.clone(),
                    },
                    precedence,
                };
            }
        }

        for (file_id, collapsed) in collapsed {
            let mut file = collapsed.file;
            if matches!(
                file.lifecycle,
                FileLifecycle::Skipped | FileLifecycle::Deleted
            ) || matches!(file.desired, DesiredState::Suppressed)
            {
                file.runtime.counts_in_run_totals = false;
                suppressed_file_ids.push(file_id.clone());
                files.insert(file_id, file);
                continue;
            }
            if let Some(size) = complete_map.get(&file.id) {
                if *size == file.size {
                    file.size = *size;
                    file.lifecycle = FileLifecycle::Complete;
                    file.progress.visible_completed_bytes = *size;
                    file.runtime.preexisting_complete = true;
                    file.runtime.counts_in_run_totals = false;
                    preexisting_complete_file_ids.push(file.id.clone());
                    files.insert(file.id.clone(), file);
                    continue;
                }
                file.lifecycle = FileLifecycle::Queued;
                file.runtime.counts_in_run_totals = true;
                file.runtime.preexisting_complete = false;
                file.progress.visible_completed_bytes = 0;
                file.message = None;
                if !resume_file_ids.contains(&file.id) {
                    resume_file_ids.push(file.id.clone());
                }
                files.insert(file.id.clone(), file);
                continue;
            }
            if let Some(partial) = partial_map.get(&file.id) {
                file.lifecycle = FileLifecycle::Queued;
                file.progress.visible_completed_bytes = partial.bytes.min(file.size.max(partial.bytes));
                file.runtime.counts_in_run_totals = true;
                file.runtime.preexisting_complete = false;
                resume_file_ids.push(file.id.clone());
            } else if matches!(file.lifecycle, FileLifecycle::Complete) {
                file.runtime.preexisting_complete = true;
                file.runtime.counts_in_run_totals = false;
                preexisting_complete_file_ids.push(file.id.clone());
            } else {
                file.lifecycle = FileLifecycle::Queued;
                file.runtime.counts_in_run_totals = true;
                file.runtime.preexisting_complete = false;
                file.message = None;
                resume_file_ids.push(file.id.clone());
            }
            files.insert(file.id.clone(), file);
        }
    }

    let mut seen_urls: HashSet<String> = HashSet::new();
    for url in urls {
        if seen_urls.insert(url.clone()) {
            state.url_order.push(url.clone());
        }
        packages.entry(url.clone()).or_insert_with(|| PackageState {
            id: url.clone(),
            source_url: url.clone(),
            display_name: url.clone(),
            status: PackageStatus::Pending,
            file_ids: Vec::new(),
            error: None,
        });
    }

    for (file_id, size) in complete_map {
        if files.contains_key(&file_id) {
            continue;
        }
        let package_id = state
            .url_order
            .first()
            .cloned()
            .unwrap_or_else(|| "local".to_string());
        packages
            .entry(package_id.clone())
            .or_insert_with(|| PackageState {
                id: package_id.clone(),
                source_url: package_id.clone(),
                display_name: package_id.clone(),
                status: PackageStatus::Pending,
                file_ids: Vec::new(),
                error: None,
            })
            .file_ids
            .push(file_id.clone());
        files.insert(
            file_id.clone(),
            FileState {
                id: file_id.clone(),
                package_id,
                path: file_id.clone(),
                size,
                lifecycle: FileLifecycle::Complete,
                progress: FileProgressState {
                    verified_existing_bytes: size,
                    downloaded_network_bytes: 0,
                    visible_completed_bytes: size,
                },
                desired: DesiredState::Present,
                runtime: RuntimeState {
                    counts_in_run_totals: false,
                    active: false,
                    preexisting_complete: true,
                    reused_chunks: 0,
                },
                message: None,
            },
        );
        preexisting_complete_file_ids.push(file_id);
    }

    for partial in partial_map.into_values() {
        if files.contains_key(&partial.file_id) {
            continue;
        }
        let package_id = state
            .url_order
            .first()
            .cloned()
            .unwrap_or_else(|| "local".to_string());
        packages
            .entry(package_id.clone())
            .or_insert_with(|| PackageState {
                id: package_id.clone(),
                source_url: package_id.clone(),
                display_name: package_id.clone(),
                status: PackageStatus::Pending,
                file_ids: Vec::new(),
                error: None,
            })
            .file_ids
            .push(partial.file_id.clone());
        files.insert(
            partial.file_id.clone(),
            FileState {
                id: partial.file_id.clone(),
                package_id,
                path: partial.file_id.clone(),
                size: partial.bytes,
                lifecycle: FileLifecycle::Queued,
                progress: FileProgressState {
                    verified_existing_bytes: 0,
                    downloaded_network_bytes: 0,
                    visible_completed_bytes: partial.bytes,
                },
                desired: DesiredState::Present,
                runtime: RuntimeState {
                    counts_in_run_totals: true,
                    active: false,
                    preexisting_complete: false,
                    reused_chunks: 0,
                },
                message: None,
            },
        );
        resume_file_ids.push(partial.file_id);
    }

    state.packages = packages;
    state.files = files;
    super::reducer::reduce(
        &mut state,
        super::reducer::CoreEvent::Tick {
            now: std::time::Instant::now(),
        },
    );

    RestartSnapshot {
        state,
        resume_file_ids,
        preexisting_complete_file_ids,
        suppressed_file_ids,
        legacy_backups: Vec::new(),
    }
}

fn precedence(lifecycle: FileLifecycle) -> usize {
    match lifecycle {
        FileLifecycle::Deleted | FileLifecycle::Skipped => 0,
        FileLifecycle::Complete => 1,
        FileLifecycle::Downloading | FileLifecycle::Queued | FileLifecycle::Planned => 2,
        FileLifecycle::Failed => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::SessionRunStatus;
    use crate::core::session::{
        FileSnapshot, PackageSnapshot, SavedCredentials, SessionSnapshotV3,
    };

    fn sample_snapshot() -> SessionSnapshotV3 {
        SessionSnapshotV3 {
            version: 3,
            id: "session".to_string(),
            created: Utc::now(),
            status: SessionRunStatus::InProgress,
            packages: vec![PackageSnapshot {
                id: "pkg".to_string(),
                source_url: "https://mega.nz/file/test".to_string(),
                display_name: "pkg".to_string(),
                file_ids: vec!["a.bin".to_string()],
                error: None,
            }],
            files: vec![FileSnapshot {
                id: "a.bin".to_string(),
                package_id: "pkg".to_string(),
                path: "a.bin".to_string(),
                size: 100,
                lifecycle: FileLifecycle::Queued,
                progress: FileProgressState::default(),
                desired: DesiredState::Present,
                runtime: RuntimeState {
                    counts_in_run_totals: true,
                    ..RuntimeState::default()
                },
                message: None,
            }],
            config: crate::config::DownloadConfig::default(),
            credentials: SavedCredentials::encrypt("u", "p", None),
        }
    }

    #[test]
    fn restart_uses_partial_files_as_resumable_queue() {
        let snapshot = sample_snapshot();
        let restart = reconcile_restart(
            Some(snapshot),
            FilesystemSnapshot {
                complete_files: Vec::new(),
                partial_files: vec![PartialFileSnapshot {
                    file_id: "a.bin".to_string(),
                    bytes: 40,
                    has_sidecar: true,
                }],
            },
            vec!["https://mega.nz/file/test".to_string()],
        );
        assert_eq!(restart.resume_file_ids, vec!["a.bin".to_string()]);
        assert_eq!(
            restart.state.files["a.bin"].lifecycle,
            FileLifecycle::Queued
        );
    }

    #[test]
    fn restart_keeps_preexisting_complete_out_of_run_totals() {
        let snapshot = sample_snapshot();
        let restart = reconcile_restart(
            Some(snapshot),
            FilesystemSnapshot {
                complete_files: vec![FilesystemFile {
                    file_id: "a.bin".to_string(),
                    size: 100,
                }],
                partial_files: Vec::new(),
            },
            vec!["https://mega.nz/file/test".to_string()],
        );
        let file = &restart.state.files["a.bin"];
        assert!(file.runtime.preexisting_complete);
        assert!(!file.runtime.counts_in_run_totals);
    }

    #[test]
    fn restart_treats_mismatched_complete_files_as_partial_queue() {
        let snapshot = sample_snapshot();
        let restart = reconcile_restart(
            Some(snapshot),
            FilesystemSnapshot {
                complete_files: vec![FilesystemFile {
                    file_id: "a.bin".to_string(),
                    size: 1000,
                }],
                partial_files: Vec::new(),
            },
            vec!["https://mega.nz/file/test".to_string()],
        );
        let file = &restart.state.files["a.bin"];
        assert_eq!(file.lifecycle, FileLifecycle::Queued);
        assert!(!file.runtime.preexisting_complete);
        assert!(file.runtime.counts_in_run_totals);
        assert_eq!(file.progress.visible_completed_bytes, 0);
        assert_eq!(restart.resume_file_ids, vec!["a.bin".to_string()]);
    }

    #[test]
    fn restart_suppresses_deleted_and_skipped_files() {
        let mut snapshot = sample_snapshot();
        snapshot.files = vec![FileSnapshot {
            id: "a.bin".to_string(),
            package_id: "pkg".to_string(),
            path: "a.bin".to_string(),
            size: 100,
            lifecycle: FileLifecycle::Deleted,
            progress: FileProgressState::default(),
            desired: DesiredState::Suppressed,
            runtime: RuntimeState::default(),
            message: None,
        }];
        let restart = reconcile_restart(Some(snapshot), FilesystemSnapshot::default(), vec![]);
        assert_eq!(restart.suppressed_file_ids, vec!["a.bin".to_string()]);
        assert_eq!(
            restart.state.files["a.bin"].lifecycle,
            FileLifecycle::Deleted
        );
    }

    #[test]
    fn duplicate_rows_collapse_by_precedence() {
        let mut snapshot = sample_snapshot();
        snapshot.files = vec![
            FileSnapshot {
                id: "a.bin".to_string(),
                package_id: "pkg".to_string(),
                path: "a.bin".to_string(),
                size: 100,
                lifecycle: FileLifecycle::Failed,
                progress: FileProgressState::default(),
                desired: DesiredState::Present,
                runtime: RuntimeState::default(),
                message: Some("boom".to_string()),
            },
            FileSnapshot {
                id: "a.bin".to_string(),
                package_id: "pkg".to_string(),
                path: "a.bin".to_string(),
                size: 100,
                lifecycle: FileLifecycle::Complete,
                progress: FileProgressState::default(),
                desired: DesiredState::Present,
                runtime: RuntimeState::default(),
                message: None,
            },
        ];
        let restart = reconcile_restart(Some(snapshot), FilesystemSnapshot::default(), vec![]);
        assert_eq!(
            restart.state.files["a.bin"].lifecycle,
            FileLifecycle::Complete
        );
    }
}
