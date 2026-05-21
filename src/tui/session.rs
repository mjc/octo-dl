use std::collections::HashSet;

use crate::core::{
    FileLifecycle, FileSnapshot, PackageId, PackageKey, SessionMeta, SessionRunStatus,
    SessionSnapshot, SessionUrlSnapshot, validate_snapshot,
};

pub(super) struct SessionAdapter;

impl SessionAdapter {
    pub(super) fn contains_url(session: &SessionSnapshot, url: &str) -> bool {
        session.urls.iter().any(|entry| entry.url == url)
    }

    #[cfg(test)]
    pub(super) fn replace_state(session: &mut SessionSnapshot, next: SessionSnapshot) {
        *session = next;
    }

    pub(super) fn mark_url_pending(session: &mut SessionSnapshot, url: &str) {
        Self::ensure_url(session, url).error = None;
    }

    pub(super) fn mark_url_fetched(session: &mut SessionSnapshot, url: &str) {
        if let Some(tracked_url) = session.urls.iter_mut().find(|entry| entry.url == url) {
            tracked_url.error = None;
        }
    }

    pub(super) fn mark_url_error(session: &mut SessionSnapshot, url: &str, error: &str) {
        Self::ensure_url(session, url).error = Some(error.to_string());
    }

    pub(super) fn remove_url(session: &mut SessionSnapshot, url: &str) {
        session.urls.retain(|entry| entry.url != url);
        for package in &mut session.packages {
            package
                .files
                .retain(|file| file.source_url != url && file.id != url);
        }
        rebuild_packages(session);
    }

    pub(super) fn remove_file(session: &mut SessionSnapshot, file_id: &str) {
        let mut removed_source_urls = HashSet::new();
        for package in &mut session.packages {
            package.files.retain(|file| {
                let remove = file.id == file_id || file.path == file_id;
                if remove {
                    removed_source_urls.insert(file.source_url.clone());
                }
                !remove
            });
        }
        rebuild_packages(session);
        remove_orphaned_urls(session, &removed_source_urls);
    }

    pub(super) fn meta(session: &SessionSnapshot) -> SessionMeta {
        SessionMeta {
            session_id: session.id.clone(),
            created: session.created,
            status: session.status,
            config: session.config.clone(),
            credentials: session.credentials.clone(),
        }
    }

    pub(super) fn apply_restart(
        session: &mut SessionSnapshot,
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

        session.packages = restart
            .state
            .packages
            .values()
            .map(|package| crate::core::PackageSnapshot {
                id: package.id,
                key: package.key.clone(),
                display_name: package.display_name.clone(),
                files: package
                    .file_ids
                    .iter()
                    .filter_map(|file_id| restart.state.files.get(file_id))
                    .map(Self::snapshot_file_from_state)
                    .collect(),
                error: package.error.clone(),
            })
            .collect();
        rebuild_packages(session);
        resumed_urls
    }

    pub(super) fn sync_for_shutdown(session: &mut SessionSnapshot, visible: &HashSet<String>) {
        if session.status == SessionRunStatus::Completed {
            return;
        }

        for package in &mut session.packages {
            package.files.retain(|file| {
                visible.contains(file.path.as_str()) || visible.contains(file.id.as_str())
            });
        }

        rebuild_packages(session);

        let has_pending_urls = session.urls.iter().any(|tracked_url| {
            !session
                .iter_files()
                .any(|file| file.source_url == tracked_url.url)
                || session.iter_files().any(|file| {
                    file.source_url == tracked_url.url
                        && !matches!(file.lifecycle, FileLifecycle::Complete)
                })
        });

        if session.iter_files().next().is_none() && !has_pending_urls {
            session.status = SessionRunStatus::Completed;
        } else {
            log::info!("Marking session as paused for later resume");
            session.status = SessionRunStatus::Paused;
        }
    }

    pub(super) fn register_queued_file(
        session: &mut SessionSnapshot,
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
        let existing = session
            .packages
            .iter()
            .enumerate()
            .find_map(|(package_index, package)| {
                package
                    .files
                    .iter()
                    .position(|file| {
                        file.path == path
                            && (file.package_id == package_id || file.source_url == source_url)
                    })
                    .map(|file_index| (package_index, file_index))
            });
        if let Some((package_index, file_index)) = existing {
            if session.packages[package_index].id == package_id {
                let file = &mut session.packages[package_index].files[file_index];
                file.package_id = package_id.clone();
                file.source_url = source_url.to_string();
                file.size = size;
                file.path = path.to_string();
                file.lifecycle = FileLifecycle::Queued;
                file.accounting = crate::core::FileAccounting::CurrentRun;
            } else {
                let mut file = session.packages[package_index].files.remove(file_index);
                file.package_id = package_id.clone();
                file.source_url = source_url.to_string();
                file.size = size;
                file.path = path.to_string();
                file.lifecycle = FileLifecycle::Queued;
                file.accounting = crate::core::FileAccounting::CurrentRun;
                let package = session
                    .packages
                    .iter_mut()
                    .find(|package| package.id == package_id)
                    .expect("package identity should exist before queuing a file");
                package.files.push(file);
            }
            rebuild_packages(session);
            return true;
        }

        let file_id = path.to_string();
        let package = session
            .packages
            .iter_mut()
            .find(|package| package.id == package_id)
            .expect("package identity should exist before queuing a file");
        package.files.push(crate::core::queued_file_snapshot(
            file_id,
            package_id,
            source_url.to_string(),
            path.to_string(),
            size,
        ));
        rebuild_packages(session);
        true
    }

    fn ensure_url<'a>(session: &'a mut SessionSnapshot, url: &str) -> &'a mut SessionUrlSnapshot {
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
            lifecycle: file.lifecycle.clone(),
            progress: file.progress.clone(),
            accounting: file.accounting,
        }
    }
}

fn ensure_package_identity(
    session: &mut SessionSnapshot,
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
        for package in &mut session.packages {
            for file in &mut package.files {
                if file.package_id == previous.id {
                    file.package_id = package_id;
                }
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
    session: &mut SessionSnapshot,
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
        files: Vec::new(),
        error,
    });
}

fn rebuild_packages(session: &mut SessionSnapshot) {
    session.prune_empty_packages();
    if session.urls.is_empty() && session.packages.is_empty() {
        return;
    }
    validate_snapshot(session).expect("live session snapshots should stay canonical");
}

fn remove_orphaned_urls(session: &mut SessionSnapshot, candidate_urls: &HashSet<String>) {
    if candidate_urls.is_empty() {
        return;
    }

    let referenced_urls = session
        .iter_files()
        .map(|file| file.source_url.clone())
        .collect::<HashSet<_>>();
    session.urls.retain(|entry| {
        !candidate_urls.contains(&entry.url) || referenced_urls.contains(&entry.url)
    });
    rebuild_packages(session);
}
