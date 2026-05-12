use std::collections::{HashMap, HashSet};

use crate::core::{
    CoreCommand, CoreEffect, CoreEvent, PackageId, ResolvedFile, ResolvedPackage, RestartSnapshot,
    SavedCredentials, SessionSnapshotV3, reconcile_restart, reduce, scan_filesystem,
    snapshot_from_state,
};

use super::{
    App, FileStatus, SessionAdapter, SessionFileUpdate, SessionRunUpdate, SessionUrlUpdate,
};
use crate::tui::event::DownloadRequest;

fn core_event_requires_visible_sync(event: &CoreEvent) -> bool {
    !matches!(
        event,
        CoreEvent::FileProgress { .. }
            | CoreEvent::FileReuseDetected { .. }
            | CoreEvent::Tick { .. }
    )
}

impl App {
    pub(crate) fn seed_core_session_from_session(&mut self) {
        if let Some(meta) = self.read_session(SessionAdapter::meta) {
            self.core_state.session_meta = meta;
        } else {
            self.core_state.session_meta.config = self.config.config.clone();
        }
    }

    pub(crate) fn skipped_session_paths(&self) -> HashMap<String, HashSet<String>> {
        self.read_session(SessionAdapter::skipped_paths_by_url)
            .unwrap_or_default()
    }

    pub(crate) fn apply_core_event(&mut self, event: CoreEvent) {
        let should_sync_visible = core_event_requires_visible_sync(&event);
        let selected_row_identity = should_sync_visible.then(|| self.selected_row()).flatten();
        self.seed_core_session_from_session();
        let effects = reduce(&mut self.core_state, event);
        self.apply_core_effects(effects);
        if should_sync_visible {
            self.sync_visible_files_preserving(selected_row_identity);
        }
        self.recompute_totals();
    }

    pub(crate) fn refresh_visible_core_file(&mut self, file_id: &str) {
        let Some(core_file) = self.core_state.files.get(file_id) else {
            return;
        };
        let Some(visible_file) = self.files.iter_mut().find(|file| file.id == file_id) else {
            return;
        };

        visible_file.name = core_file.path.clone();
        visible_file.size = core_file.size;
        visible_file.downloaded = match core_file.lifecycle {
            crate::core::FileLifecycle::Complete => core_file.size,
            _ => core_file
                .progress
                .visible_completed_bytes
                .min(core_file.size),
        };
        visible_file.status = match core_file.lifecycle {
            crate::core::FileLifecycle::Planned | crate::core::FileLifecycle::Queued => {
                FileStatus::Queued
            }
            crate::core::FileLifecycle::Downloading => FileStatus::Downloading,
            crate::core::FileLifecycle::Complete => FileStatus::Complete,
            crate::core::FileLifecycle::Failed => FileStatus::Error(
                core_file
                    .message
                    .clone()
                    .unwrap_or_else(|| "failed".to_string()),
            ),
            crate::core::FileLifecycle::Skipped | crate::core::FileLifecycle::Deleted => {
                visible_file.status.clone()
            }
        };
    }

    pub(crate) fn apply_core_command(&mut self, command: CoreCommand) {
        self.apply_core_event(command.into_event());
    }

    fn apply_core_effects(&mut self, effects: Vec<CoreEffect>) {
        let mut queued_file_map: HashMap<String, HashSet<String>> = HashMap::new();
        for effect in effects {
            match effect {
                CoreEffect::PersistSession(snapshot) => {
                    self.persist_core_session_snapshot(snapshot);
                }
                CoreEffect::EnqueueUrlResolution { url } => {
                    let _ = self.url_tx.send(DownloadRequest::SubmitUrl { url });
                }
                CoreEffect::DeleteOutputArtifacts { path, .. } => {
                    super::super::download::schedule_output_artifact_delete(path);
                }
                CoreEffect::DeleteResumeArtifacts { path, .. } => {
                    super::super::download::schedule_resume_artifact_delete(path);
                }
                CoreEffect::PublishStatusMessage(message) => {
                    self.status = message;
                }
                CoreEffect::EnqueueFileDownload { file_id } => {
                    let Some(source_url) = self
                        .core_state
                        .files
                        .get(&file_id)
                        .and_then(|file| file.source_url.clone())
                    else {
                        continue;
                    };
                    queued_file_map
                        .entry(source_url)
                        .or_default()
                        .insert(file_id.clone());
                }
                CoreEffect::PublishViewSnapshot => {}
            }
        }

        for (source_url, file_ids) in queued_file_map {
            if file_ids.is_empty() {
                continue;
            }
            let file_ids = file_ids.into_iter().collect::<Vec<_>>();
            let attempt_ids = file_ids
                .iter()
                .filter_map(|file_id| {
                    self.file_attempt_ids
                        .get(file_id)
                        .copied()
                        .filter(|attempt_id| *attempt_id > 0)
                        .map(|attempt_id| (file_id.clone(), attempt_id))
                })
                .collect();
            let _ = self.url_tx.send(DownloadRequest::ResumeFileIds {
                source_url,
                file_ids,
                attempt_ids,
            });
        }
    }

    fn persist_core_session_snapshot(&mut self, snapshot: SessionSnapshotV3) {
        self.persist_session(snapshot);
    }

    pub(crate) fn ensure_session_for_pending_urls(&mut self) {
        if self.session.is_some() {
            return;
        }

        let credentials =
            SavedCredentials::encrypt(self.login.email(), self.login.password(), None);
        let session = SessionSnapshotV3::new(self.config.config.clone(), credentials);
        self.save_and_install_session(session);
    }

    fn refresh_session_from_core_state(&mut self) {
        if self.session.is_none() {
            return;
        }
        if self.core_state.packages.is_empty() && self.core_state.files.is_empty() {
            return;
        }

        self.seed_core_session_from_session();
        let snapshot = snapshot_from_state(&self.core_state);
        self.persist_core_session_snapshot(snapshot);
    }

    pub(crate) fn ensure_core_file(
        &mut self,
        file_id: &str,
        source_url: &str,
        path: &str,
        size: u64,
        counts_toward_progress: bool,
    ) {
        self.ensure_core_file_in_package(
            file_id,
            source_url,
            source_url,
            source_url,
            path,
            size,
            counts_toward_progress,
        );
    }

    pub(crate) fn ensure_core_file_in_package(
        &mut self,
        file_id: &str,
        package_id: &str,
        package_display_name: &str,
        source_url: &str,
        path: &str,
        size: u64,
        counts_toward_progress: bool,
    ) {
        self.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: PackageId::parse_or_key(
                    package_id,
                    &crate::core::PackageKey::new(package_display_name),
                ),
                key: crate::core::PackageKey::new(package_display_name),
                source_url: source_url.to_string(),
                display_name: package_display_name.to_string(),
                files: vec![ResolvedFile {
                    file_id: file_id.to_string(),
                    path: path.to_string(),
                    size,
                }],
                collision: None,
            },
        });
        self.apply_core_event(CoreEvent::FileQueued {
            file_id: file_id.to_string(),
        });
        if let Some(file) = self.core_state.files.get_mut(file_id) {
            file.source_url = Some(source_url.to_string());
            file.size = size;
            file.path = path.to_string();
            file.runtime.counts_in_run_totals = counts_toward_progress;
            if !counts_toward_progress {
                file.runtime.preexisting_complete = true;
            }
        }
    }

    pub(crate) fn update_session_url(&mut self, url: &str, update: SessionUrlUpdate<'_>) {
        let _ = self
            .mutate_session_and_save(|session| SessionAdapter::update_url(session, url, update));
    }

    pub(crate) fn remove_session_url(&mut self, url: &str) {
        let _ = self.mutate_session_and_save(|session| SessionAdapter::remove_url(session, url));
    }

    pub(crate) fn update_session_file(&mut self, file_id: &str, update: SessionFileUpdate<'_>) {
        let _ = self.mutate_session_and_save(|session| {
            SessionAdapter::update_file(session, file_id, update)
        });
    }

    pub(crate) fn register_session_queued_file(
        &mut self,
        package_id: &str,
        package_display_name: &str,
        submitted_url: &str,
        source_url: &str,
        path: &str,
        size: u64,
    ) -> bool {
        self.mutate_session_and_save(|session| {
            SessionAdapter::register_queued_file(
                session,
                package_id,
                package_display_name,
                submitted_url,
                source_url,
                path,
                size,
            )
        })
        .unwrap_or(true)
    }

    pub(crate) fn mutate_session_and_save<R>(
        &mut self,
        f: impl FnOnce(&mut SessionSnapshotV3) -> R,
    ) -> Option<R> {
        self.session.clone().map(|mut session| {
            let result = f(&mut session);
            let _ = self.persist_session(session);
            result
        })
    }

    pub(crate) fn read_session<R>(&self, f: impl FnOnce(&SessionSnapshotV3) -> R) -> Option<R> {
        self.session.as_ref().map(f)
    }

    pub(crate) fn update_session_run_status(&mut self, update: SessionRunUpdate) {
        let _ = self
            .mutate_session_and_save(|session| SessionAdapter::apply_run_update(session, update));
    }

    pub(crate) fn install_session(&mut self, session: SessionSnapshotV3) {
        self.session = Some(session);
        self.seed_core_session_from_session();
    }

    pub(crate) fn save_and_install_session(&mut self, session: SessionSnapshotV3) {
        let _ = self.persist_session(session);
    }

    pub(crate) fn restore_restart_snapshot(&mut self, snapshot: &RestartSnapshot) {
        self.apply_core_event(CoreEvent::RestartReconciled {
            snapshot: snapshot.clone(),
        });
    }

    pub(crate) fn resume_latest_session(&mut self) {
        let Some(session) = SessionSnapshotV3::latest() else {
            return;
        };
        log::info!("Resuming session {}", session.id);

        if let Some((email, password, _mfa)) = session.credentials.decrypt() {
            self.login.set_credentials_if_missing(&email, &password, "");
        }

        let file_ids = session
            .files
            .iter()
            .map(|file| file.path.clone())
            .collect::<Vec<_>>();
        let restart = reconcile_restart(
            Some(session.clone()),
            scan_filesystem(file_ids),
            session.urls.iter().map(|entry| entry.url.clone()).collect(),
        );

        self.resume_from_restart(session, &restart);
        self.log_state_diagnostics("resume_latest_session");
    }

    pub(crate) fn resume_from_restart(
        &mut self,
        mut session: SessionSnapshotV3,
        restart: &RestartSnapshot,
    ) {
        self.restore_restart_snapshot(restart);

        let resumed_urls = SessionAdapter::apply_restart(&mut session, restart);
        self.urls.clone_from(&resumed_urls);
        for url in resumed_urls {
            let has_files_for_url = restart
                .state
                .files
                .values()
                .any(|file| file.source_url.as_deref() == Some(url.as_str()));
            if !has_files_for_url {
                self.queue_url_placeholder(url.clone());
                let _ = self.url_tx.send(DownloadRequest::SubmitUrl { url });
            }
        }
        self.save_and_install_session(session);
    }

    pub(crate) fn sync_session_for_shutdown(&mut self) {
        self.refresh_session_from_core_state();
        let visible: HashSet<String> = self.files.iter().map(|file| file.id.clone()).collect();
        let _ = self.mutate_session_and_save(|session| {
            SessionAdapter::sync_for_shutdown(session, &visible)
        });
    }

    pub(crate) fn sync_session_after_file_complete(&mut self, id: &str) {
        self.update_session_file(id, SessionFileUpdate::Complete);
        if self.files_completed == self.files_total && self.files_total > 0 {
            self.update_session_run_status(SessionRunUpdate::Completed);
        }
    }

    pub(crate) fn update_download_status_message(&mut self) {
        if self.files_completed == self.files_total && self.files_total > 0 {
            self.status = "All downloads complete".to_string();
        } else {
            self.status = format!(
                "Downloading ({}/{})",
                self.files_completed, self.files_total
            );
        }
    }

    fn persist_session(&mut self, session: SessionSnapshotV3) -> bool {
        if let Err(error) = session.save() {
            log::error!("Failed to save session {}: {error}", session.id);
            self.status = format!("Failed to save session: {error}");
            return false;
        }

        match SessionSnapshotV3::load(&session.state_path()) {
            Ok(saved) => {
                self.install_session(saved);
                true
            }
            Err(error) => {
                log::error!(
                    "Failed to reload canonical session {} after save: {error}",
                    session.id
                );
                self.status = format!("Failed to save session: {error}");
                false
            }
        }
    }
}
