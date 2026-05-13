use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::core::model::{
    DesiredState, DownloadState, FileId, FileLifecycle, FileProgressState, FileState, PackageId,
    PackageState, PackageStatus, SessionMeta, UrlId,
};
use crate::core::session::SessionSnapshot;
use chrono::Utc;
use indexmap::IndexMap;

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
    pub verified_bytes: u64,
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
}

impl RestartSnapshot {
    #[must_use]
    pub fn resumable_urls(&self) -> Vec<UrlId> {
        self.state
            .url_order
            .iter()
            .filter(|url| {
                let mut saw_file_for_url = false;
                let mut has_remaining = false;
                for file in self.state.files.values() {
                    if file.source_url.as_deref() != Some(url.as_str()) {
                        continue;
                    }
                    saw_file_for_url = true;
                    if !matches!(
                        file.lifecycle,
                        FileLifecycle::Complete | FileLifecycle::Skipped | FileLifecycle::Deleted
                    ) {
                        has_remaining = true;
                        break;
                    }
                }
                !saw_file_for_url || has_remaining
            })
            .cloned()
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
                file_id: file_id.clone().into(),
                size: metadata.len(),
            });
            continue;
        }

        let part_path = crate::download::part_path(&file_id);
        if let Ok(metadata) = std::fs::metadata(&part_path)
            && metadata.is_file()
        {
            snapshot.partial_files.push(PartialFileSnapshot {
                file_id: file_id.clone().into(),
                bytes: metadata.len(),
                has_sidecar: crate::download::sidecar_path(&file_id).exists(),
                verified_bytes: crate::download::resume_sidecar_verified_bytes(&file_id)
                    .unwrap_or(0),
            });
        }
    }
    snapshot
}

#[must_use]
pub fn build_restart_snapshot(session: &SessionSnapshot) -> RestartSnapshot {
    reconcile_restart(
        Some(canonical_restart_session(session.clone())),
        scan_filesystem(session.iter_files().map(|file| file.path.clone())),
        session.urls.iter().map(|entry| entry.url.clone()).collect(),
    )
}

pub fn reconcile_restart(
    session: Option<SessionSnapshot>,
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

    if let Some(snapshot) = session.map(canonical_restart_session) {
        let SessionSnapshot {
            urls,
            packages: snapshot_packages,
            ..
        } = snapshot;
        for tracked_url in urls {
            if !state
                .url_order
                .iter()
                .any(|existing| existing == &tracked_url.url)
            {
                state.url_order.push(tracked_url.url);
            }
        }
        for package in &snapshot_packages {
            packages.insert(
                package.id.clone(),
                PackageState {
                    id: package.id.clone(),
                    key: package.key.clone(),
                    display_name: package.display_name.clone(),
                    file_ids: package.files.iter().map(|file| file.id.clone()).collect(),
                    status: PackageStatus::Pending,
                    error: package.error.clone(),
                },
            );
        }

        for file in snapshot_packages
            .into_iter()
            .flat_map(|package| package.files.into_iter())
        {
            let file_id = file.id.clone();
            let mut file = FileState {
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
            };
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
            let observed = crate::download::ObservedLocalFile {
                final_size: complete_map.get(&file.id).copied(),
                part_size: partial_map.get(&file.id).map(|partial| partial.bytes),
                has_sidecar: partial_map
                    .get(&file.id)
                    .is_some_and(|partial| partial.has_sidecar),
                verified_resume_bytes: partial_map
                    .get(&file.id)
                    .map_or(0, |partial| partial.verified_bytes),
            };
            let local = crate::download::classify_observed_local_file(observed, file.size, false);
            if matches!(local.status, crate::download::FileStatus::Complete) {
                file.lifecycle = FileLifecycle::Complete;
                file.progress.visible_completed_bytes = file.size;
                file.runtime.preexisting_complete = true;
                file.runtime.counts_in_run_totals = false;
                preexisting_complete_file_ids.push(file.id.clone());
                files.insert(file.id.clone(), file);
                continue;
            }
            if matches!(local.status, crate::download::FileStatus::Partial) {
                file.lifecycle = FileLifecycle::Queued;
                file.progress = FileProgressState {
                    verified_existing_bytes: 0,
                    downloaded_network_bytes: 0,
                    visible_completed_bytes: local.verified_resume_bytes.min(file.size),
                };
                file.runtime.counts_in_run_totals = true;
                file.runtime.preexisting_complete = false;
                file.message = None;
                resume_file_ids.push(file.id.clone());
            } else if matches!(file.lifecycle, FileLifecycle::Complete) {
                file.runtime.preexisting_complete = true;
                file.runtime.counts_in_run_totals = false;
                preexisting_complete_file_ids.push(file.id.clone());
            } else {
                file.lifecycle = FileLifecycle::Queued;
                file.runtime.counts_in_run_totals = true;
                file.runtime.preexisting_complete = false;
                file.progress = FileProgressState::default();
                file.message = None;
                resume_file_ids.push(file.id.clone());
            }
            files.insert(file.id.clone(), file);
        }
    }

    let mut seen_urls: HashSet<String> = HashSet::new();
    for url in urls {
        if seen_urls.insert(url.clone()) {
            if !state.url_order.iter().any(|existing| existing == &url) {
                state.url_order.push(url);
            }
        }
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
    }
}

fn canonical_restart_session(mut snapshot: SessionSnapshot) -> SessionSnapshot {
    crate::core::session::canonicalize_snapshot(&mut snapshot)
        .expect("restart snapshots should already be canonical");
    crate::core::validate_snapshot(&snapshot).expect("restart snapshots should stay valid");
    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::RuntimeState;
    use crate::core::model::SessionRunStatus;
    use crate::core::session::{
        FileSnapshot, PackageSnapshot, SavedCredentials, SessionSnapshot, SessionUrlSnapshot,
    };

    fn package_id(raw: &str, source_url: &str) -> PackageId {
        PackageId::parse_or_key(raw, &crate::core::PackageKey::new(source_url))
    }

    fn sample_snapshot() -> SessionSnapshot {
        let mut snapshot = SessionSnapshot {
            version: 5,
            id: "session".to_string().into(),
            created: Utc::now(),
            status: SessionRunStatus::InProgress,
            urls: vec![SessionUrlSnapshot {
                url: "https://mega.nz/file/test".to_string(),
                error: None,
            }],
            packages: vec![PackageSnapshot {
                id: package_id("pkg", "https://mega.nz/file/test"),
                key: crate::core::PackageKey::new("https://mega.nz/file/test".to_string().clone()),
                display_name: "pkg".to_string(),
                files: Vec::new(),
                error: None,
            }],
            files: vec![FileSnapshot {
                id: "a.bin".to_string().into(),
                package_id: package_id("pkg", "https://mega.nz/file/test"),
                source_url: Some("https://mega.nz/file/test".to_string()),
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
        };
        snapshot.packages[0].files = snapshot.files.clone();
        snapshot.sync_flat_files_from_packages();
        snapshot
    }

    #[test]
    fn restart_uses_partial_files_as_resumable_queue() {
        let snapshot = sample_snapshot();
        let restart = reconcile_restart(
            Some(snapshot),
            FilesystemSnapshot {
                complete_files: Vec::new(),
                partial_files: vec![PartialFileSnapshot {
                    file_id: "a.bin".to_string().into(),
                    bytes: 40,
                    has_sidecar: true,
                    verified_bytes: 40,
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
                    file_id: "a.bin".to_string().into(),
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
                    file_id: "a.bin".to_string().into(),
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
    fn restart_clears_stale_progress_for_missing_partial_files() {
        let mut snapshot = sample_snapshot();
        let file = snapshot.find_file_mut("a.bin").unwrap();
        file.progress = FileProgressState {
            verified_existing_bytes: 0,
            downloaded_network_bytes: 0,
            visible_completed_bytes: 95,
        };
        file.lifecycle = FileLifecycle::Failed;
        file.message = Some("corrupt".to_string());
        snapshot.sync_flat_files_from_packages();

        let restart = reconcile_restart(
            Some(snapshot),
            FilesystemSnapshot::default(),
            vec!["https://mega.nz/file/test".to_string()],
        );

        let file = &restart.state.files["a.bin"];
        assert_eq!(file.lifecycle, FileLifecycle::Queued);
        assert_eq!(file.progress, FileProgressState::default());
        assert!(file.message.is_none());
        assert_eq!(restart.resume_file_ids, vec!["a.bin".to_string()]);
    }

    #[test]
    fn restart_clamps_partial_progress_to_file_size() {
        let snapshot = sample_snapshot();
        let restart = reconcile_restart(
            Some(snapshot),
            FilesystemSnapshot {
                complete_files: Vec::new(),
                partial_files: vec![PartialFileSnapshot {
                    file_id: "a.bin".to_string().into(),
                    bytes: 140,
                    has_sidecar: true,
                    verified_bytes: 140,
                }],
            },
            vec!["https://mega.nz/file/test".to_string()],
        );

        let file = &restart.state.files["a.bin"];
        assert_eq!(file.lifecycle, FileLifecycle::Queued);
        assert_eq!(file.progress.visible_completed_bytes, 100);
    }

    #[test]
    fn restart_ignores_preallocated_partial_length_without_verified_sidecar_bytes() {
        let snapshot = sample_snapshot();
        let restart = reconcile_restart(
            Some(snapshot),
            FilesystemSnapshot {
                complete_files: Vec::new(),
                partial_files: vec![PartialFileSnapshot {
                    file_id: "a.bin".to_string().into(),
                    bytes: 100,
                    has_sidecar: false,
                    verified_bytes: 0,
                }],
            },
            vec!["https://mega.nz/file/test".to_string()],
        );

        let file = &restart.state.files["a.bin"];
        assert_eq!(file.lifecycle, FileLifecycle::Queued);
        assert_eq!(file.progress.visible_completed_bytes, 0);
    }

    #[test]
    fn restart_rejects_duplicate_packages_for_same_source_url() {
        let source_url = "https://mega.nz/folder/dup".to_string();
        let mut snapshot = sample_snapshot();
        snapshot.packages = vec![
            PackageSnapshot {
                id: package_id(&source_url, &source_url),
                key: crate::core::PackageKey::new(source_url.clone().clone()),
                display_name: source_url.clone(),
                files: vec![FileSnapshot {
                    id: "a.bin".to_string().into(),
                    package_id: package_id(&source_url, &source_url),
                    source_url: Some(source_url.clone()),
                    path: "folder/a.bin".to_string(),
                    size: 10,
                    lifecycle: FileLifecycle::Queued,
                    progress: FileProgressState::default(),
                    desired: DesiredState::Present,
                    runtime: RuntimeState::default(),
                    message: None,
                }],
                error: None,
            },
            PackageSnapshot {
                id: package_id("batch-dup", &source_url),
                key: crate::core::PackageKey::new(source_url.clone().clone()),
                display_name: "Folder".to_string(),
                files: vec![FileSnapshot {
                    id: "b.bin".to_string().into(),
                    package_id: package_id("batch-dup", &source_url),
                    source_url: Some(source_url.clone()),
                    path: "folder/b.bin".to_string(),
                    size: 20,
                    lifecycle: FileLifecycle::Queued,
                    progress: FileProgressState::default(),
                    desired: DesiredState::Present,
                    runtime: RuntimeState::default(),
                    message: None,
                }],
                error: None,
            },
        ];
        snapshot.sync_flat_files_from_packages();
        assert!(crate::core::session::validate_snapshot(&snapshot).is_err());
    }

    #[test]
    fn restart_rejects_empty_synthetic_packages_without_files() {
        let mut snapshot = sample_snapshot();
        snapshot.packages.push(PackageSnapshot {
            id: package_id("batch-folder", "https://mega.nz/file/test"),
            key: crate::core::PackageKey::new("https://mega.nz/file/test".to_string().clone()),
            display_name: "Batch Folder".to_string(),
            files: Vec::new(),
            error: Some("boom".to_string()),
        });
        assert!(crate::core::session::validate_snapshot(&snapshot).is_err());
    }

    #[test]
    fn build_restart_snapshot_prunes_stale_package_rows_from_in_memory_session() {
        let mut snapshot = sample_snapshot();
        snapshot.packages.push(PackageSnapshot {
            id: package_id("stale", "https://mega.nz/file/test"),
            key: crate::core::PackageKey::new("Stale Package"),
            display_name: "Stale Package".to_string(),
            files: Vec::new(),
            error: Some("boom".to_string()),
        });

        let restart = build_restart_snapshot(&snapshot);

        assert_eq!(restart.state.packages.len(), 1);
        let package = restart
            .state
            .packages
            .values()
            .next()
            .expect("canonical package should remain");
        assert_eq!(package.display_name, "pkg");
        assert!(!restart.state.package_file_ids(&package.id).is_empty());
    }

    #[test]
    fn restart_does_not_synthesize_complete_files_missing_from_session() {
        let mut snapshot = SessionSnapshot::new(
            crate::config::DownloadConfig::default(),
            SavedCredentials::encrypt("u", "p", None),
        );
        snapshot.urls.push(SessionUrlSnapshot {
            url: "https://mega.nz/file/test".to_string(),
            error: None,
        });
        let restart = reconcile_restart(
            Some(snapshot),
            FilesystemSnapshot {
                complete_files: vec![FilesystemFile {
                    file_id: "orphan.bin".to_string().into(),
                    size: 100,
                }],
                partial_files: Vec::new(),
            },
            vec!["https://mega.nz/file/test".to_string()],
        );

        assert!(!restart.state.files.contains_key("orphan.bin"));
        assert!(restart.preexisting_complete_file_ids.is_empty());
        assert_eq!(
            restart.resumable_urls(),
            vec!["https://mega.nz/file/test".to_string()]
        );
    }

    #[test]
    fn restart_does_not_synthesize_partial_files_missing_from_session() {
        let mut snapshot = SessionSnapshot::new(
            crate::config::DownloadConfig::default(),
            SavedCredentials::encrypt("u", "p", None),
        );
        snapshot.urls.push(SessionUrlSnapshot {
            url: "https://mega.nz/file/test".to_string(),
            error: None,
        });
        let restart = reconcile_restart(
            Some(snapshot),
            FilesystemSnapshot {
                complete_files: Vec::new(),
                partial_files: vec![PartialFileSnapshot {
                    file_id: "orphan.bin".to_string().into(),
                    bytes: 40,
                    has_sidecar: true,
                    verified_bytes: 40,
                }],
            },
            vec!["https://mega.nz/file/test".to_string()],
        );

        assert!(!restart.state.files.contains_key("orphan.bin"));
        assert!(restart.resume_file_ids.is_empty());
        assert_eq!(
            restart.resumable_urls(),
            vec!["https://mega.nz/file/test".to_string()]
        );
    }

    #[test]
    fn restart_suppresses_deleted_and_skipped_files() {
        let mut snapshot = sample_snapshot();
        snapshot.packages[0].files = vec![FileSnapshot {
            id: "a.bin".to_string().into(),
            package_id: package_id("pkg", "https://mega.nz/file/test"),
            source_url: Some("https://mega.nz/file/test".to_string()),
            path: "a.bin".to_string(),
            size: 100,
            lifecycle: FileLifecycle::Deleted,
            progress: FileProgressState::default(),
            desired: DesiredState::Suppressed,
            runtime: RuntimeState::default(),
            message: None,
        }];
        snapshot.sync_flat_files_from_packages();
        let restart = reconcile_restart(Some(snapshot), FilesystemSnapshot::default(), vec![]);
        assert_eq!(restart.suppressed_file_ids, vec!["a.bin".to_string()]);
        assert_eq!(
            restart.state.files["a.bin"].lifecycle,
            FileLifecycle::Deleted
        );
    }
}
