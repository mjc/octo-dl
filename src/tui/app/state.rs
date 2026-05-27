use std::collections::HashSet;

use indexmap::IndexMap;

use crate::core::reducer::{reduce_without_session_persist, should_persist_session};
use crate::core::{
    CoreCommand, CoreEffect, CoreEffects, CoreEvent, FileAccounting, FileId, PackageId,
    ResolvedFile, ResolvedPackage, RestartSnapshot, SavedCredentials, SessionSnapshot,
    build_restart_snapshot, reduce, snapshot_from_state, validate_snapshot,
};

use super::persistence::SessionPersistenceError;
use super::{App, FileStatus, SessionAdapter};
use crate::tui::event::DownloadRequest;

fn core_event_requires_visible_sync(event: &CoreEvent) -> bool {
    !matches!(
        event,
        CoreEvent::FileProgress { .. }
            | CoreEvent::FileReuseDetected { .. }
            | CoreEvent::Tick { .. }
    )
}

fn core_event_requires_pending_sync(event: &CoreEvent) -> bool {
    matches!(
        event,
        CoreEvent::PackageResolved { .. }
            | CoreEvent::FileQueued { .. }
            | CoreEvent::FileCancelled { .. }
            | CoreEvent::FileDeleted { .. }
            | CoreEvent::PackageDeleted { .. }
            | CoreEvent::FileRetryRequested { .. }
            | CoreEvent::FileResetRequested { .. }
            | CoreEvent::PackageMoveRequested { .. }
            | CoreEvent::FileMoveRequested { .. }
            | CoreEvent::RestartReconciled { .. }
    )
}

#[derive(Clone, Copy)]
struct CoreApplyPolicy {
    sync_visible: bool,
    sync_pending: bool,
    recompute_totals: bool,
}

impl CoreApplyPolicy {
    fn for_event(event: &CoreEvent) -> Self {
        Self {
            sync_visible: core_event_requires_visible_sync(event),
            sync_pending: core_event_requires_pending_sync(event),
            recompute_totals: true,
        }
    }

    const fn for_progress() -> Self {
        Self {
            sync_visible: false,
            sync_pending: false,
            recompute_totals: false,
        }
    }
}

impl App {
    fn merge_previous_url_errors_into_core_snapshot(&self, session: &mut SessionSnapshot) {
        let Some(previous) = self.session.as_ref() else {
            return;
        };

        for url in &mut session.urls {
            if url.error.is_some() {
                continue;
            }
            if self
                .core_state
                .files
                .values()
                .any(|file| file.source_url == url.url)
            {
                continue;
            }
            if let Some(error) = previous
                .urls
                .iter()
                .find(|previous_url| previous_url.url == url.url)
                .and_then(|previous_url| previous_url.error.clone())
            {
                url.error = Some(error);
            }
        }
    }

    pub(crate) fn visible_file(&self, file_id: &FileId) -> Option<&crate::tui::app::FileEntry> {
        let &visible_index = self.visible_file_positions.get(file_id)?;
        self.files.get(visible_index)
    }

    pub(crate) fn seed_core_session_from_session(&mut self) {
        if let Some(meta) = self.read_session(SessionAdapter::meta) {
            self.core_state.session_meta = meta;
        } else {
            self.core_state.session_meta.config = self.config.config.clone();
        }
    }

    pub(crate) fn apply_core_event(&mut self, event: CoreEvent) {
        let policy = CoreApplyPolicy::for_event(&event);
        self.apply_core_event_with_policy(event, policy);
    }

    pub(crate) fn apply_core_progress_event(&mut self, event: CoreEvent) {
        self.apply_core_event_with_policy(event, CoreApplyPolicy::for_progress());
    }

    pub(crate) fn refresh_visible_core_file(&mut self, file_id: &FileId) -> Option<(u64, u64)> {
        let Some(core_file) = self.core_state.files.get(file_id) else {
            return None;
        };
        let Some(&visible_index) = self.visible_file_positions.get(file_id) else {
            return None;
        };
        let Some(visible_file) = self.files.get_mut(visible_index) else {
            return None;
        };

        let previous_downloaded = visible_file.downloaded;
        visible_file.name = core_file.path.clone();
        visible_file.size = core_file.size;
        visible_file.downloaded = match &core_file.lifecycle {
            crate::core::FileLifecycle::Complete => core_file.size,
            _ => crate::core::visible_completed_bytes_for_display(core_file),
        };
        visible_file.status = match &core_file.lifecycle {
            crate::core::FileLifecycle::Planned | crate::core::FileLifecycle::Queued => {
                FileStatus::Queued
            }
            crate::core::FileLifecycle::Downloading => FileStatus::Downloading,
            crate::core::FileLifecycle::Complete => FileStatus::Complete,
            crate::core::FileLifecycle::Failed { message } => FileStatus::Error(message.clone()),
        };
        Some((previous_downloaded, visible_file.downloaded))
    }

    pub(crate) fn refresh_visible_progress_file(
        &mut self,
        file_id: &FileId,
        now: std::time::Instant,
    ) -> Option<u64> {
        let core_file = self.core_state.files.get(file_id)?;
        let downloaded = match &core_file.lifecycle {
            crate::core::FileLifecycle::Complete => core_file.size,
            _ => crate::core::visible_completed_bytes_for_display(core_file),
        };
        let network_downloaded = crate::tui::app::App::core_file_network_downloaded(core_file);
        let complete = matches!(core_file.lifecycle, crate::core::FileLifecycle::Complete);
        let failure_message = core_file.lifecycle.failure_message().map(str::to_owned);

        let &visible_index = self.visible_file_positions.get(file_id)?;
        let visible_file = self.files.get_mut(visible_index)?;
        let previous_downloaded = visible_file.downloaded;
        visible_file.downloaded = downloaded;
        visible_file.status = if complete {
            FileStatus::Complete
        } else if let Some(message) = failure_message {
            FileStatus::Error(message)
        } else {
            FileStatus::Downloading
        };

        let accepted = downloaded.saturating_sub(previous_downloaded);
        let state = self.file_ui.entry(file_id.clone()).or_default();
        state.rate.record(network_downloaded, now);
        state.speed = state.rate.bytes_per_sec(now);
        Some(accepted)
    }

    pub(crate) fn apply_core_command(&mut self, command: CoreCommand) {
        self.apply_core_event(command.into_event());
    }

    fn apply_core_event_with_policy(&mut self, event: CoreEvent, policy: CoreApplyPolicy) {
        let selected_row_identity = if policy.sync_visible {
            if self.visible_sync_defer_depth > 0 {
                self.pending_visible_selection
                    .clone()
                    .flatten()
                    .or_else(|| self.selected_row_from_last_sync())
            } else {
                self.selected_row()
            }
        } else {
            None
        };
        let defer_core_session_persistence =
            self.session_persist_defer_depth > 0 && should_persist_session(&event);
        self.seed_core_session_from_session();
        let effects = if defer_core_session_persistence {
            reduce_without_session_persist(&mut self.core_state, event)
        } else {
            reduce(&mut self.core_state, event)
        };
        self.pending_core_state_session_persistence |= defer_core_session_persistence;
        self.apply_core_effects(effects, policy.sync_pending);
        if policy.sync_visible {
            self.sync_visible_files_preserving(selected_row_identity);
        }
        if policy.recompute_totals {
            self.recompute_totals();
        } else {
            self.apply_cached_totals();
        }
    }

    fn apply_core_effects(&mut self, effects: CoreEffects, should_sync_pending: bool) {
        let mut queued_file_map = std::mem::take(&mut self.queued_file_effects);
        queued_file_map.clear();
        for effect in effects {
            match effect {
                CoreEffect::PersistSession(snapshot) => {
                    let _ = self.persist_core_session_snapshot(snapshot);
                }
                CoreEffect::EnqueueUrlResolution { url } => {
                    let _ = self.url_tx.send(DownloadRequest::SubmitUrl { url });
                }
                CoreEffect::DeleteOutputArtifacts { path } => {
                    super::super::download::schedule_output_artifact_delete(path);
                }
                CoreEffect::DeleteResumeArtifacts { path } => {
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
                        .map(|file| file.source_url.clone())
                    else {
                        continue;
                    };
                    let entry = queued_file_map.entry(source_url).or_default();
                    if !entry.contains(&file_id) {
                        entry.push(file_id.clone());
                    }
                }
                CoreEffect::PublishViewSnapshot => {}
            }
        }

        self.enqueue_batched_file_downloads(&mut queued_file_map);
        self.queued_file_effects = queued_file_map;
        self.sync_scheduler_pending_order(should_sync_pending);
    }

    fn enqueue_batched_file_downloads(
        &mut self,
        queued_file_map: &mut IndexMap<String, Vec<FileId>>,
    ) {
        for (source_url, file_ids) in queued_file_map.iter_mut() {
            if file_ids.is_empty() {
                continue;
            }
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
                source_url: source_url.clone(),
                file_ids: std::mem::take(file_ids),
                attempt_ids,
            });
        }
    }

    fn sync_scheduler_pending_order(&mut self, should_sync_pending: bool) {
        if !should_sync_pending {
            return;
        }
        if self.scheduler_pending_sync_defer_depth > 0 {
            self.pending_scheduler_pending_order_sync = true;
            return;
        }
        self.flush_scheduler_pending_order();
    }

    pub(super) fn flush_scheduler_pending_order(&mut self) {
        if self.download_task_running {
            let _ = self.url_tx.send(DownloadRequest::SyncPendingOrder {
                file_ids: self.core_state.pending_file_ids(),
            });
        }
    }

    pub(crate) fn ensure_session_for_pending_urls(&mut self) {
        if self.session.is_some() {
            return;
        }

        let credentials =
            SavedCredentials::encrypt(self.login.email(), self.login.password(), None);
        let session = SessionSnapshot::new(self.config.config.clone(), credentials);
        self.save_session(session);
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
        let _ = self.persist_session(snapshot);
    }

    pub(crate) fn ensure_core_file(
        &mut self,
        file_id: &FileId,
        source_url: &str,
        path: &str,
        size: u64,
        accounting: FileAccounting,
    ) {
        self.ensure_core_file_in_package(
            file_id, source_url, source_url, source_url, path, size, accounting,
        );
    }

    pub(crate) fn ensure_core_file_in_package(
        &mut self,
        file_id: &FileId,
        package_id: &str,
        package_display_name: &str,
        source_url: &str,
        path: &str,
        size: u64,
        accounting: FileAccounting,
    ) {
        self.with_deferred_batch_updates(|app| {
            app.apply_core_event(CoreEvent::PackageResolved {
                package: ResolvedPackage {
                    id: PackageId::parse_or_key(
                        package_id,
                        &crate::core::PackageKey::new(package_display_name),
                    ),
                    key: crate::core::PackageKey::new(package_display_name),
                    source_url: source_url.to_string(),
                    display_name: package_display_name.to_string(),
                    files: vec![ResolvedFile {
                        file_id: file_id.clone(),
                        path: path.to_string(),
                        size,
                    }],
                    collision: None,
                },
            });
            app.apply_core_event(CoreEvent::FileQueued {
                file_id: file_id.clone(),
            });
        });
        if let Some(file) = self.core_state.files.get_mut(file_id) {
            file.source_url = source_url.to_string();
            file.size = size;
            file.path = path.to_string();
            file.accounting = accounting;
        }
    }

    pub(crate) fn mutate_session_and_save<R>(
        &mut self,
        f: impl FnOnce(&mut SessionSnapshot) -> R,
    ) -> Option<R> {
        self.materialize_deferred_core_session();
        self.session.clone().map(|mut session| {
            let result = f(&mut session);
            let _ = self.persist_session(session);
            result
        })
    }

    pub(crate) fn read_session<R>(&self, f: impl FnOnce(&SessionSnapshot) -> R) -> Option<R> {
        self.session.as_ref().map(f)
    }

    pub(crate) fn install_session(&mut self, session: SessionSnapshot) {
        self.session = Some(session);
        self.seed_core_session_from_session();
    }

    pub(crate) fn save_session(&mut self, session: SessionSnapshot) {
        let _ = self.persist_session(session);
    }

    pub(crate) fn restore_restart_snapshot(&mut self, snapshot: &RestartSnapshot) {
        self.apply_core_event(CoreEvent::RestartReconciled {
            snapshot: snapshot.clone(),
        });
    }

    pub(crate) fn resume_latest_session(&mut self) {
        let Some(session) = SessionSnapshot::latest() else {
            return;
        };
        log::info!("Resuming session {}", session.id);

        if let Some((email, password, _mfa)) = session.credentials.decrypt() {
            self.login.set_credentials_if_missing(&email, &password, "");
        }

        let restart = build_restart_snapshot(&session);

        self.resume_from_restart(session, &restart);
    }

    pub(crate) fn resume_from_restart(
        &mut self,
        mut session: SessionSnapshot,
        restart: &RestartSnapshot,
    ) {
        self.restore_restart_snapshot(restart);

        let resumed_urls = SessionAdapter::apply_restart(&mut session, restart);
        for url in resumed_urls {
            let has_files_for_url = restart
                .state
                .files
                .values()
                .any(|file| file.source_url == *url);
            if !has_files_for_url {
                self.queue_url_placeholder(url.clone());
                let _ = self.url_tx.send(DownloadRequest::SubmitUrl { url });
            }
        }
        self.save_session(session);
    }

    pub(crate) fn sync_session_for_shutdown(&mut self) {
        self.refresh_session_from_core_state();
        let visible: HashSet<String> = self.files.iter().map(|file| file.id.to_string()).collect();
        let _ = self.mutate_session_and_save(|session| {
            SessionAdapter::sync_for_shutdown(session, &visible)
        });
        self.flush_session_persistence();
    }

    pub(crate) fn poll_session_persistence(&mut self) {
        for error in self.session_persistence.drain_errors() {
            match error {
                SessionPersistenceError::Save { id, error } => {
                    log::error!("Failed to save session {id}: {error}");
                    self.status = format!("Failed to save session: {error}");
                }
                SessionPersistenceError::Remove { path, error } => {
                    log::error!("Failed to remove empty session {}: {error}", path.display());
                    self.status = format!("Failed to remove empty session: {error}");
                }
            }
        }
    }

    pub(crate) fn flush_session_persistence(&mut self) {
        self.session_persistence.flush();
        self.poll_session_persistence();
    }

    pub(crate) fn update_download_status_message(&mut self) {
        if self.paused {
            self.status = "Paused".to_string();
        } else if self.files_completed == self.files_total && self.files_total > 0 {
            self.status = "All downloads complete".to_string();
        } else if self.files_total > 0 {
            let activity = if self
                .files
                .iter()
                .any(|file| matches!(file.status, super::FileStatus::Downloading))
            {
                "Downloading"
            } else {
                "Queued"
            };
            self.status = format!("{activity} ({}/{})", self.files_completed, self.files_total);
        }
    }

    pub(super) fn flush_deferred_session_persistence(
        &mut self,
        pending: super::PendingSessionPersistence,
    ) {
        match pending {
            super::PendingSessionPersistence::Save(session) => self.persist_session_now(session),
            super::PendingSessionPersistence::Remove(path) => {
                #[cfg(test)]
                {
                    self.session_persist_count += 1;
                }
                self.session_persistence.remove(path);
            }
        }
    }

    fn persist_session_now(&mut self, session: SessionSnapshot) {
        #[cfg(test)]
        {
            self.session_persist_count += 1;
        }
        self.session_persistence
            .save(session.clone(), session.state_path());
        self.install_session(session);
    }

    fn materialize_deferred_core_session(&mut self) {
        if !self.pending_core_state_session_persistence {
            return;
        }
        self.pending_core_state_session_persistence = false;
        self.pending_session_persistence = None;
        let mut session = snapshot_from_state(&self.core_state);
        self.merge_previous_url_errors_into_core_snapshot(&mut session);
        self.install_session(session);
    }

    pub(super) fn persist_core_session_snapshot(&mut self, mut session: SessionSnapshot) -> bool {
        self.merge_previous_url_errors_into_core_snapshot(&mut session);
        self.persist_session(session)
    }

    pub(super) fn persist_session(&mut self, session: SessionSnapshot) -> bool {
        self.pending_core_state_session_persistence = false;
        let session = session;
        if session.urls.is_empty() && session.packages.is_empty() {
            let path = session.state_path();
            self.install_session(session);
            if self.session_persist_defer_depth > 0 {
                self.pending_session_persistence =
                    Some(super::PendingSessionPersistence::Remove(path));
            } else {
                #[cfg(test)]
                {
                    self.session_persist_count += 1;
                }
                self.session_persistence.remove(path);
            }
            return true;
        }

        if let Err(error) = validate_snapshot(&session)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
        {
            log::error!("Failed to save session {}: {error}", session.id);
            self.status = format!("Failed to save session: {error}");
            return false;
        }

        if self.session_persist_defer_depth > 0 {
            self.pending_session_persistence =
                Some(super::PendingSessionPersistence::Save(session.clone()));
            self.install_session(session);
        } else {
            self.persist_session_now(session);
        }
        true
    }
}
