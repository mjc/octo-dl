use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;

use crate::core::{
    DesiredState, FileLifecycle, FileProgressState, FileSnapshot, PackageId, PackageKey,
    RuntimeState, SessionMeta, SessionRunStatus, SessionSnapshotV3, SessionUrlSnapshot,
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
        session.urls.iter().any(|entry| entry.url == url)
    }

    #[cfg(test)]
    pub(super) fn replace_state(session: &mut SessionSnapshotV3, next: SessionSnapshotV3) {
        *session = next;
    }

    pub(super) fn update_url(
        session: &mut SessionSnapshotV3,
        url: &str,
        update: SessionUrlUpdate<'_>,
    ) {
        match update {
            SessionUrlUpdate::Pending => {
                let tracked_url = Self::ensure_url(session, url);
                tracked_url.error = None;
            }
            SessionUrlUpdate::Fetched => {
                let Some(tracked_url) = session.urls.iter_mut().find(|entry| entry.url == url)
                else {
                    return;
                };
                tracked_url.error = None;
            }
            SessionUrlUpdate::Error(error) => {
                let tracked_url = Self::ensure_url(session, url);
                tracked_url.error = Some(error.to_string());
            }
        }
    }

    pub(super) fn remove_url(session: &mut SessionSnapshotV3, url: &str) {
        session.urls.retain(|entry| entry.url != url);
        session
            .files
            .retain(|file| file.source_url.as_deref() != Some(url) && file.id != url);
        rebuild_packages(session);
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
        let mut skipped = HashMap::<String, HashSet<String>>::new();
        for file in &session.files {
            if !matches!(file.lifecycle, FileLifecycle::Skipped) {
                continue;
            }
            let Some(url) = file.source_url.as_ref() else {
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

        session.urls = restart
            .state
            .url_order
            .iter()
            .map(|url| SessionUrlSnapshot {
                url: url.clone(),
                error: None,
            })
            .collect();

        for package in restart.state.packages.values() {
            upsert_package_metadata(
                session,
                package.id.clone(),
                package.key.clone(),
                package.display_name.clone(),
                package.error.clone(),
            );
        }

        for tracked_url in &mut session.urls {
            if resumed_url_set.contains(&tracked_url.url) {
                tracked_url.error = None;
            }
        }

        session.files = restart
            .state
            .files
            .values()
            .map(Self::snapshot_file_from_state)
            .collect();
        rebuild_packages(session);
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

        rebuild_packages(session);

        let has_pending_urls = session.urls.iter().any(|tracked_url| {
            !session.files.iter().any(|file| {
                file.source_url.as_deref() == Some(tracked_url.url.as_str())
                    && matches!(
                        file.lifecycle,
                        FileLifecycle::Complete | FileLifecycle::Skipped | FileLifecycle::Deleted
                    )
            }) || session.files.iter().any(|file| {
                file.source_url.as_deref() == Some(tracked_url.url.as_str())
                    && !matches!(
                        file.lifecycle,
                        FileLifecycle::Complete | FileLifecycle::Skipped | FileLifecycle::Deleted
                    )
            })
        });

        if session.files.is_empty() && !has_pending_urls {
            Self::apply_run_update(session, SessionRunUpdate::Completed);
        } else {
            log::info!("Marking session as paused for later resume");
            Self::apply_run_update(session, SessionRunUpdate::Paused);
        }
    }

    pub(super) fn register_queued_file(
        session: &mut SessionSnapshotV3,
        package_id: &str,
        package_display_name: &str,
        submitted_url: &str,
        source_url: &str,
        path: &str,
        size: u64,
    ) -> bool {
        Self::ensure_url(session, source_url);
        if submitted_url != source_url {
            session.urls.retain(|entry| entry.url != submitted_url);
        } else {
            Self::ensure_url(session, submitted_url);
        }
        let package_id = ensure_package_identity(session, package_id, package_display_name);
        if let Some(file) = session.files.iter_mut().find(|file| {
            file.path == path
                && (file.package_id == package_id || file.source_url.as_deref() == Some(source_url))
        }) {
            if matches!(file.lifecycle, FileLifecycle::Skipped) {
                return false;
            }
            file.package_id = package_id.clone();
            file.source_url = Some(source_url.to_string());
            file.size = size;
            file.path = path.to_string();
            rebuild_packages(session);
            return true;
        }

        let file_id = path.to_string();
        session.files.push(FileSnapshot {
            id: file_id,
            package_id,
            source_url: Some(source_url.to_string()),
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
        rebuild_packages(session);
        true
    }

    fn ensure_url<'a>(session: &'a mut SessionSnapshotV3, url: &str) -> &'a mut SessionUrlSnapshot {
        if let Some(index) = session.urls.iter().position(|entry| entry.url == url) {
            return &mut session.urls[index];
        }

        session.urls.push(SessionUrlSnapshot {
            url: url.to_string(),
            error: None,
        });
        session.urls.last_mut().expect("url was just pushed")
    }

    fn snapshot_file_from_state(file: &crate::core::FileState) -> FileSnapshot {
        FileSnapshot {
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
        }
    }
}

fn ensure_package_identity(
    session: &mut SessionSnapshotV3,
    package_id: &str,
    display_name: &str,
) -> PackageId {
    let package_key = PackageKey::new(display_name);
    let package_id = PackageId::parse_or_key(package_id, &package_key);
    let existing = session
        .packages
        .iter()
        .find(|package| package.id == package_id || package.key == package_key)
        .cloned();
    let existing_error = existing.as_ref().and_then(|package| package.error.clone());

    if let Some(previous) = existing
        && previous.id != package_id
    {
        for file in &mut session.files {
            if file.package_id == previous.id {
                file.package_id = package_id;
            }
        }
    }

    upsert_package_metadata(
        session,
        package_id,
        package_key,
        display_name.to_string(),
        existing_error,
    );
    package_id
}

fn upsert_package_metadata(
    session: &mut SessionSnapshotV3,
    package_id: PackageId,
    package_key: PackageKey,
    display_name: String,
    error: Option<String>,
) {
    if let Some(package) = session
        .packages
        .iter_mut()
        .find(|package| package.id == package_id || package.key == package_key)
    {
        package.id = package_id;
        package.key = package_key;
        package.display_name = display_name;
        package.error = error;
        return;
    }

    session.packages.push(crate::core::PackageSnapshot {
        id: package_id,
        key: package_key,
        display_name,
        file_ids: Vec::new(),
        error,
    });
}

fn rebuild_packages(session: &mut SessionSnapshotV3) {
    let existing = session
        .packages
        .iter()
        .cloned()
        .map(|package| (package.id, package))
        .collect::<IndexMap<_, _>>();
    let mut grouped = IndexMap::<PackageId, Vec<String>>::new();
    for file in &session.files {
        grouped
            .entry(file.package_id)
            .or_default()
            .push(file.id.clone());
    }

    let mut rebuilt = Vec::with_capacity(grouped.len());
    for (package_id, file_ids) in grouped {
        let existing_package = existing.get(&package_id);
        let display_name = existing_package
            .map(|package| package.display_name.clone())
            .or_else(|| common_path_root(&file_ids))
            .unwrap_or_else(|| package_id.to_string());
        let key = existing_package
            .map(|package| package.key.clone())
            .unwrap_or_else(|| PackageKey::new(display_name.clone()));
        rebuilt.push(crate::core::PackageSnapshot {
            id: package_id,
            key,
            display_name,
            file_ids,
            error: existing_package.and_then(|package| package.error.clone()),
        });
    }
    session.packages = rebuilt;
}

fn common_path_root(paths: &[String]) -> Option<String> {
    let mut roots = paths.iter().filter_map(|path| {
        let root = path.split('/').next()?;
        (!root.is_empty()).then(|| root.to_string())
    });
    let first = roots.next()?;
    roots.all(|root| root == first).then_some(first)
}
