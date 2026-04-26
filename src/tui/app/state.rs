use std::collections::{HashMap, HashSet};

use crate::core::{
    CoreEffect, CoreEvent, ResolvedFile, ResolvedPackage, RestartSnapshot, SessionSnapshotV3,
    reconcile_restart, reduce, scan_filesystem,
};

use super::{App, SessionAdapter, SessionFileUpdate, SessionRunUpdate, SessionUrlUpdate};

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
        self.seed_core_session_from_session();
        let effects = reduce(&mut self.core_state, event);
        self.apply_core_effects(effects);
        self.sync_visible_files();
        self.recompute_totals();
    }

    fn apply_core_effects(&mut self, effects: Vec<CoreEffect>) {
        for effect in effects {
            match effect {
                CoreEffect::PersistSession(snapshot) => {
                    self.persist_core_session_snapshot(snapshot);
                }
                CoreEffect::PublishStatusMessage(message) => {
                    self.status = message;
                }
                CoreEffect::EnqueueUrlResolution { .. }
                | CoreEffect::EnqueueFileDownload { .. }
                | CoreEffect::DeleteOutputArtifacts { .. }
                | CoreEffect::DeleteResumeArtifacts { .. }
                | CoreEffect::PublishViewSnapshot => {}
            }
        }
    }

    fn persist_core_session_snapshot(&mut self, snapshot: SessionSnapshotV3) {
        let _ = self.mutate_session(|session| SessionAdapter::merge_state(session, snapshot));
    }

    pub(crate) fn ensure_core_file(
        &mut self,
        file_id: &str,
        source_url: &str,
        path: &str,
        size: u64,
        counts_toward_progress: bool,
    ) {
        self.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: source_url.to_string(),
                source_url: source_url.to_string(),
                display_name: source_url.to_string(),
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

    pub(crate) fn update_session_file(&mut self, file_id: &str, update: SessionFileUpdate<'_>) {
        let _ =
            self.mutate_session(|session| SessionAdapter::update_file(session, file_id, update));
    }

    pub(crate) fn register_session_queued_file(
        &mut self,
        submitted_url: &str,
        path: &str,
        size: u64,
    ) -> bool {
        self.mutate_session_and_save(|session| {
            SessionAdapter::register_queued_file(session, submitted_url, path, size)
        })
        .unwrap_or(true)
    }

    pub(crate) fn mutate_session<R>(
        &mut self,
        f: impl FnOnce(&mut SessionSnapshotV3) -> R,
    ) -> Option<R> {
        self.session.as_mut().map(f)
    }

    pub(crate) fn mutate_session_and_save<R>(
        &mut self,
        f: impl FnOnce(&mut SessionSnapshotV3) -> R,
    ) -> Option<R> {
        self.session.as_mut().map(|session| {
            let result = f(session);
            let _ = session.save();
            result
        })
    }

    pub(crate) fn read_session<R>(&self, f: impl FnOnce(&SessionSnapshotV3) -> R) -> Option<R> {
        self.session.as_ref().map(f)
    }

    pub(crate) fn update_session_run_status(&mut self, update: SessionRunUpdate) {
        let _ = self.mutate_session(|session| SessionAdapter::apply_run_update(session, update));
    }

    pub(crate) fn install_session(&mut self, session: SessionSnapshotV3) {
        self.session = Some(session);
        self.seed_core_session_from_session();
    }

    pub(crate) fn save_and_install_session(&mut self, session: SessionSnapshotV3) {
        let _ = session.save();
        self.install_session(session);
    }

    pub(crate) fn restore_restart_snapshot(&mut self, snapshot: &RestartSnapshot) {
        self.core_state = snapshot.state.clone();
        self.sync_visible_files();
        self.recompute_totals();
    }

    pub(crate) fn resume_latest_session(&mut self) {
        let Some(session) = SessionSnapshotV3::latest() else {
            return;
        };
        log::info!("Resuming session {}", session.id);

        if let Some((email, password, mfa)) = session.credentials.decrypt() {
            self.login
                .set_credentials(email, password, mfa.unwrap_or_default());
        }

        let file_ids = session
            .files
            .iter()
            .map(|file| file.path.clone())
            .collect::<Vec<_>>();
        let restart = reconcile_restart(
            Some(session.clone()),
            scan_filesystem(file_ids),
            session
                .packages
                .iter()
                .map(|package| package.source_url.clone())
                .collect(),
        );

        self.resume_from_restart(session, &restart);
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
            let _ = self.url_tx.send(url);
        }
        self.save_and_install_session(session);
    }

    pub(crate) fn sync_session_for_shutdown(&mut self) {
        let visible: HashSet<String> = self.files.iter().map(|file| file.id.clone()).collect();
        let _ = self.mutate_session(|session| SessionAdapter::sync_for_shutdown(session, &visible));
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
}
