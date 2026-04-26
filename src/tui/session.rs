use std::collections::{HashMap, HashSet};

use crate::{
    FileEntry as SessionFileEntry, FileEntryStatus, SessionState, SessionStatus, UrlEntry,
    UrlStatus,
    core::{FileLifecycle, FileState, RestartSnapshot, SessionMeta},
    file_key,
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
    pub(super) fn contains_url(session: &SessionState, url: &str) -> bool {
        session.urls.iter().any(|entry| entry.url == url)
    }

    pub(super) fn merge_state(session: &mut SessionState, next: SessionState) {
        session.id = next.id;
        session.created = next.created;
        session.status = next.status;
        session.config = next.config;
        session.credentials = next.credentials;

        let mut merged_files = session.files.clone();
        for next_file in next.files {
            if let Some(existing) = merged_files
                .iter_mut()
                .find(|file| file.path == next_file.path || file.key == next_file.key)
            {
                *existing = next_file;
            } else {
                merged_files.push(next_file);
            }
        }
        session.files = merged_files;

        for url in next.urls {
            if !session.urls.iter().any(|entry| entry.url == url.url) {
                session.urls.push(url);
            }
        }
    }

    pub(super) fn url_index(session: &SessionState, submitted_url: &str) -> usize {
        session
            .urls
            .iter()
            .position(|entry| entry.url == submitted_url)
            .unwrap_or(0)
    }

    pub(super) fn update_url(session: &mut SessionState, url: &str, update: SessionUrlUpdate<'_>) {
        match update {
            SessionUrlUpdate::Pending => {
                if !Self::contains_url(session, url) {
                    session.urls.push(UrlEntry {
                        url: url.to_string(),
                        status: UrlStatus::Pending,
                    });
                }
            }
            SessionUrlUpdate::Fetched => {
                if let Some(entry) = session.urls.iter_mut().find(|entry| entry.url == url) {
                    entry.status = UrlStatus::Fetched;
                }
            }
            SessionUrlUpdate::Error(error) => {
                if let Some(entry) = session.urls.iter_mut().find(|entry| entry.url == url) {
                    entry.status = UrlStatus::Error(error.to_string());
                }
            }
        }
    }

    pub(super) fn update_file(
        session: &mut SessionState,
        file_id: &str,
        update: SessionFileUpdate<'_>,
    ) {
        match update {
            SessionFileUpdate::Complete => {
                let _ = session.mark_file_complete(file_id);
            }
            SessionFileUpdate::Error(error) => {
                let _ = session.mark_file_error(file_id, error);
            }
            SessionFileUpdate::Skipped => {
                let _ = session.mark_file_skipped(file_id);
            }
        }
    }

    pub(super) fn meta(session: &SessionState) -> SessionMeta {
        SessionMeta {
            session_id: session.id.clone(),
            created: session.created,
            status: match session.status {
                SessionStatus::InProgress => crate::core::SessionRunStatus::InProgress,
                SessionStatus::Completed => crate::core::SessionRunStatus::Completed,
                SessionStatus::Paused => crate::core::SessionRunStatus::Paused,
            },
            config: session.config.clone(),
            credentials: crate::core::SavedCredentials {
                email: session.credentials.email.clone(),
                password: session.credentials.password.clone(),
                mfa: session.credentials.mfa.clone(),
            },
        }
    }

    pub(super) fn skipped_paths_by_url(session: &SessionState) -> HashMap<String, HashSet<String>> {
        let mut skipped = HashMap::<String, HashSet<String>>::new();
        for file in &session.files {
            if !matches!(file.status, FileEntryStatus::Skipped) {
                continue;
            }
            let Some(url) = session
                .urls
                .get(file.url_index)
                .map(|entry| entry.url.clone())
            else {
                continue;
            };
            skipped.entry(url).or_default().insert(file.path.clone());
        }
        skipped
    }

    pub(super) fn apply_run_update(session: &mut SessionState, update: SessionRunUpdate) {
        match update {
            SessionRunUpdate::Completed => {
                let _ = session.mark_completed();
            }
            SessionRunUpdate::Paused => {
                let _ = session.mark_paused();
            }
        }
    }

    pub(super) fn apply_restart(
        session: &mut SessionState,
        restart: &RestartSnapshot,
    ) -> Vec<String> {
        let resumed_urls = restart.resumable_urls();
        let resumed_url_set: HashSet<_> = resumed_urls.iter().cloned().collect();
        for entry in &mut session.urls {
            if !matches!(entry.status, UrlStatus::Error(_)) {
                entry.status = if resumed_url_set.contains(&entry.url) {
                    UrlStatus::Pending
                } else {
                    UrlStatus::Fetched
                };
            }
        }
        session.files = restart
            .state
            .files
            .values()
            .map(|file| Self::restart_file_entry(session, restart, file))
            .collect();
        resumed_urls
    }

    fn restart_file_entry(
        session: &SessionState,
        restart: &RestartSnapshot,
        file: &FileState,
    ) -> SessionFileEntry {
        let url_index = session
            .urls
            .iter()
            .position(|entry| {
                restart
                    .state
                    .packages
                    .get(&file.package_id)
                    .is_some_and(|package| package.source_url == entry.url)
            })
            .unwrap_or(0);
        SessionFileEntry {
            key: Some(file.id.clone()),
            url_index,
            path: file.path.clone(),
            size: file.size,
            status: match file.lifecycle {
                FileLifecycle::Planned | FileLifecycle::Queued => FileEntryStatus::Pending,
                FileLifecycle::Downloading => FileEntryStatus::Downloading,
                FileLifecycle::Complete => FileEntryStatus::Completed,
                FileLifecycle::Skipped | FileLifecycle::Deleted => FileEntryStatus::Skipped,
                FileLifecycle::Failed => FileEntryStatus::Error(
                    file.message.clone().unwrap_or_else(|| "failed".to_string()),
                ),
            },
        }
    }

    pub(super) fn sync_for_shutdown(session: &mut SessionState, visible: &HashSet<String>) {
        if session.status == SessionStatus::Completed {
            return;
        }

        session.files.retain(|file| {
            matches!(file.status, FileEntryStatus::Skipped)
                || visible.contains(file.path.as_str())
                || visible.contains(file.key_or_path())
        });

        if session.files.is_empty() {
            Self::apply_run_update(session, SessionRunUpdate::Completed);
        } else {
            log::info!("Marking session as paused for later resume");
            Self::apply_run_update(session, SessionRunUpdate::Paused);
        }
    }

    pub(super) fn register_queued_file(
        session: &mut SessionState,
        submitted_url: &str,
        path: &str,
        size: u64,
    ) -> bool {
        let url_index = Self::url_index(session, submitted_url);
        let stable_key = file_key(url_index, path);

        if let Some(file) = session
            .files
            .iter_mut()
            .find(|file| file.url_index == url_index && file.path == path)
        {
            if matches!(file.status, FileEntryStatus::Skipped) {
                if file.key.is_none() {
                    file.key = Some(stable_key);
                }
                return false;
            }
            if file.key.is_none() {
                file.key = Some(stable_key);
            }
            return true;
        }

        session.files.push(SessionFileEntry {
            key: Some(stable_key),
            url_index,
            path: path.to_string(),
            size,
            status: FileEntryStatus::Pending,
        });
        true
    }
}
