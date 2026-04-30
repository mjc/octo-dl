use std::collections::{HashMap, HashSet};

use crate::core::{
    DesiredState, FileLifecycle, FileProgressState, FileSnapshot, RuntimeState, SessionMeta,
    SessionRunStatus, SessionSnapshotV3,
};

pub(super) enum SessionFileUpdate<'a> {
    Complete,
    Error(&'a str),
    Skipped,
}

pub(super) enum SessionUrlUpdate<'a> {
    Pending,
    Fetched,
    Error(&'a str),
}

#[derive(Clone, Copy)]
pub(super) enum SessionRunUpdate {
    Completed,
    Paused,
}

pub(super) struct SessionAdapter;

impl SessionAdapter {
    pub(super) fn contains_url(session: &SessionSnapshotV3, url: &str) -> bool {
        session
            .packages
            .iter()
            .any(|package| package.source_url == url)
    }

    pub(super) fn merge_state(session: &mut SessionSnapshotV3, next: SessionSnapshotV3) {
        session.id = next.id;
        session.created = next.created;
        session.status = next.status;
        session.config = next.config;
        session.credentials = next.credentials;

        for next_package in next.packages {
            if let Some(existing) = session
                .packages
                .iter_mut()
                .find(|package| package.id == next_package.id)
            {
                *existing = next_package;
            } else {
                session.packages.push(next_package);
            }
        }

        let mut merged_files = session.files.clone();
        for next_file in next.files {
            if let Some(existing) = merged_files
                .iter_mut()
                .find(|file| file.id == next_file.id || file.path == next_file.path)
            {
                *existing = next_file;
            } else {
                merged_files.push(next_file);
            }
        }
        session.files = merged_files;
    }

    pub(super) fn update_url(
        session: &mut SessionSnapshotV3,
        url: &str,
        update: SessionUrlUpdate<'_>,
    ) {
        let package = Self::ensure_package(session, url);
        match update {
            SessionUrlUpdate::Pending | SessionUrlUpdate::Fetched => {
                package.error = None;
            }
            SessionUrlUpdate::Error(error) => {
                package.error = Some(error.to_string());
            }
        }
    }

    pub(super) fn update_file(
        session: &mut SessionSnapshotV3,
        file_id: &str,
        update: SessionFileUpdate<'_>,
    ) {
        match update {
            SessionFileUpdate::Complete => {
                if let Some(file) = session.files.iter_mut().find(|file| file.id == file_id) {
                    file.lifecycle = FileLifecycle::Complete;
                    file.progress.visible_completed_bytes = file.size;
                    file.runtime.active = false;
                    file.runtime.counts_in_run_totals = false;
                }
            }
            SessionFileUpdate::Error(error) => {
                if let Some(file) = session.files.iter_mut().find(|file| file.id == file_id) {
                    file.lifecycle = FileLifecycle::Failed;
                    file.message = Some(error.to_string());
                    file.runtime.active = false;
                }
            }
            SessionFileUpdate::Skipped => {
                if let Some(file) = session.files.iter_mut().find(|file| file.id == file_id) {
                    file.lifecycle = FileLifecycle::Skipped;
                    file.desired = DesiredState::Suppressed;
                    file.runtime.active = false;
                    file.runtime.counts_in_run_totals = false;
                }
            }
        }
    }

    pub(super) fn meta(session: &SessionSnapshotV3) -> SessionMeta {
        SessionMeta {
            session_id: session.id.clone(),
            created: session.created,
            status: session.status,
            config: session.config.clone(),
            credentials: session.credentials.clone(),
        }
    }

    pub(super) fn skipped_paths_by_url(
        session: &SessionSnapshotV3,
    ) -> HashMap<String, HashSet<String>> {
        let package_urls: HashMap<_, _> = session
            .packages
            .iter()
            .map(|package| (package.id.clone(), package.source_url.clone()))
            .collect();
        let mut skipped = HashMap::<String, HashSet<String>>::new();
        for file in &session.files {
            if !matches!(file.lifecycle, FileLifecycle::Skipped) {
                continue;
            }
            let Some(url) = package_urls.get(&file.package_id) else {
                continue;
            };
            skipped
                .entry(url.clone())
                .or_default()
                .insert(file.path.clone());
        }
        skipped
    }

    pub(super) fn apply_run_update(session: &mut SessionSnapshotV3, update: SessionRunUpdate) {
        match update {
            SessionRunUpdate::Completed => {
                session.status = SessionRunStatus::Completed;
            }
            SessionRunUpdate::Paused => {
                session.status = SessionRunStatus::Paused;
            }
        }
    }

    pub(super) fn apply_restart(
        session: &mut SessionSnapshotV3,
        restart: &crate::core::RestartSnapshot,
    ) -> Vec<String> {
        let resumed_urls = restart.resumable_urls();
        let resumed_url_set: HashSet<_> = resumed_urls.iter().cloned().collect();

        for package in &mut session.packages {
            if resumed_url_set.contains(&package.source_url) {
                package.error = None;
            }
        }

        for package in restart.state.packages.values() {
            if let Some(existing) = session
                .packages
                .iter_mut()
                .find(|entry| entry.id == package.id)
            {
                existing.display_name = package.display_name.clone();
                existing.source_url = package.source_url.clone();
                existing.file_ids = package.file_ids.clone();
                if resumed_url_set.contains(&existing.source_url) {
                    existing.error = None;
                }
            } else {
                session.packages.push(crate::core::PackageSnapshot {
                    id: package.id.clone(),
                    source_url: package.source_url.clone(),
                    display_name: package.display_name.clone(),
                    file_ids: package.file_ids.clone(),
                    error: None,
                });
            }
        }

        session.files = restart
            .state
            .files
            .values()
            .map(Self::snapshot_file_from_state)
            .collect();
        resumed_urls
    }

    pub(super) fn sync_for_shutdown(session: &mut SessionSnapshotV3, visible: &HashSet<String>) {
        if session.status == SessionRunStatus::Completed {
            return;
        }

        session.files.retain(|file| {
            matches!(file.lifecycle, FileLifecycle::Skipped)
                || visible.contains(file.path.as_str())
                || visible.contains(file.id.as_str())
        });

        for package in &mut session.packages {
            package
                .file_ids
                .retain(|file_id| session.files.iter().any(|file| &file.id == file_id));
        }

        if session.files.is_empty() {
            Self::apply_run_update(session, SessionRunUpdate::Completed);
        } else {
            log::info!("Marking session as paused for later resume");
            Self::apply_run_update(session, SessionRunUpdate::Paused);
        }
    }

    pub(super) fn register_queued_file(
        session: &mut SessionSnapshotV3,
        submitted_url: &str,
        path: &str,
        size: u64,
    ) -> bool {
        let package_id = {
            let package = Self::ensure_package(session, submitted_url);
            package.id.clone()
        };
        if let Some(file) = session
            .files
            .iter_mut()
            .find(|file| file.package_id == package_id && file.path == path)
        {
            if matches!(file.lifecycle, FileLifecycle::Skipped) {
                return false;
            }
            return true;
        }

        let file_id = path.to_string();
        if let Some(package) = session
            .packages
            .iter_mut()
            .find(|package| package.id == package_id)
            && !package.file_ids.contains(&file_id)
        {
            package.file_ids.push(file_id.clone());
        }
        session.files.push(FileSnapshot {
            id: file_id,
            package_id,
            path: path.to_string(),
            size,
            lifecycle: FileLifecycle::Queued,
            progress: FileProgressState::default(),
            desired: DesiredState::Present,
            runtime: RuntimeState {
                counts_in_run_totals: true,
                active: false,
                preexisting_complete: false,
                reused_chunks: 0,
            },
            message: None,
        });
        true
    }

    fn ensure_package<'a>(
        session: &'a mut SessionSnapshotV3,
        submitted_url: &str,
    ) -> &'a mut crate::core::PackageSnapshot {
        if let Some(index) = session
            .packages
            .iter()
            .position(|package| package.source_url == submitted_url)
        {
            return &mut session.packages[index];
        }

        session.packages.push(crate::core::PackageSnapshot {
            id: submitted_url.to_string(),
            source_url: submitted_url.to_string(),
            display_name: submitted_url.to_string(),
            file_ids: Vec::new(),
            error: None,
        });
        session
            .packages
            .last_mut()
            .expect("package was just pushed")
    }

    fn snapshot_file_from_state(file: &crate::core::FileState) -> FileSnapshot {
        FileSnapshot {
            id: file.id.clone(),
            package_id: file.package_id.clone(),
            path: file.path.clone(),
            size: file.size,
            lifecycle: file.lifecycle,
            progress: file.progress.clone(),
            desired: file.desired,
            runtime: file.runtime.clone(),
            message: file.message.clone(),
        }
    }
}
