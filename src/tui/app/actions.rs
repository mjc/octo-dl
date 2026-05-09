use std::sync::Arc;
use std::time::Instant;

use crate::{
    core::{CoreCommand, CoreEvent},
    format_bytes,
};

use super::{
    App, ProgressDelta, QueuedFile, SessionAdapter, SessionFileUpdate, SessionUrlUpdate, UiAction,
    VisibleFileContext,
};

impl App {
    pub(crate) fn submit_url(&mut self, url: String) {
        if self.urls.contains(&url) {
            return;
        }
        self.deleted_files.remove(&url);
        self.urls.push(url.clone());
        self.ensure_session_for_pending_urls();
        self.apply_core_command(CoreCommand::SubmitUrl { url: url.clone() });
        self.update_session_url(&url, SessionUrlUpdate::Pending);
    }

    pub(crate) fn drain_ui_actions(
        &mut self,
        action_rx: &mut tokio::sync::mpsc::UnboundedReceiver<UiAction>,
    ) -> bool {
        let mut handled = false;
        while let Ok(action) = action_rx.try_recv() {
            self.handle_ui_action(action);
            handled = true;
        }
        handled
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        let _ = self.pause_tx.send(paused);
    }

    pub fn pause_downloads(&mut self) {
        if self.paused {
            return;
        }
        let downloading_ids: Vec<_> = self
            .files
            .iter()
            .filter(|file| matches!(file.status, super::FileStatus::Downloading))
            .map(|file| file.id.clone())
            .collect();
        self.set_paused(true);
        for token in self.cancellation_tokens.values() {
            token.cancel();
        }
        for file_id in downloading_ids {
            if self.core_state.files.contains_key(&file_id) {
                self.apply_core_event(CoreEvent::FileCancelled {
                    file_id: file_id.clone(),
                });
            } else if let Some(file) = self.overlay_file_mut(&file_id) {
                file.status = super::FileStatus::Queued;
                self.sync_visible_files();
            }
            self.reset_file_ui_rate(&file_id);
        }
        self.reset_aggregate_rate();
        self.status = "Paused".to_string();
    }

    pub fn resume_downloads(&mut self) {
        if !self.paused {
            return;
        }
        self.set_paused(false);
        self.status = "Resuming downloads...".to_string();
    }

    fn ensure_core_file_from_context(&mut self, context: &VisibleFileContext) -> Option<String> {
        let source_url = context.source_url.clone();
        if let Some(source_url) = source_url.as_ref() {
            self.ensure_core_file(
                &context.id,
                source_url,
                &context.artifact_path,
                context.size,
                context.counts_toward_progress,
            );
        }
        source_url
    }

    fn cancel_file_token(&mut self, id: &str) {
        if let Some(token) = self.cancellation_tokens.remove(id) {
            token.cancel();
        }
    }

    pub(crate) fn note_file_error(&mut self, id: &str, error: &str) {
        self.update_session_file(id, SessionFileUpdate::Error(error));
    }

    fn mark_file_skipped(&mut self, id: &str) {
        self.update_session_file(id, SessionFileUpdate::Skipped);
    }

    fn handle_deleted_download_artifact(&mut self, id: &str, artifact_path: &str) -> bool {
        if !self.deleted_files.remove(id) {
            return false;
        }

        self.file_attempt_ids.remove(id);
        self.reset_pending_files.remove(id);
        self.cancellation_tokens.remove(id);
        super::super::download::schedule_download_artifact_delete(artifact_path.to_string());
        self.mark_file_skipped(id);
        true
    }

    fn reset_is_waiting_for_new_attempt(&self, id: &str) -> bool {
        self.reset_pending_files.contains(id)
    }

    fn current_attempt_id(&self, id: &str) -> u64 {
        self.file_attempt_ids.get(id).copied().unwrap_or(0)
    }

    fn bump_file_attempt_id(&mut self, id: &str) -> u64 {
        let next = self.current_attempt_id(id).saturating_add(1);
        self.file_attempt_ids.insert(id.to_string(), next);
        next
    }

    fn event_matches_current_attempt(&self, id: &str, attempt_id: u64) -> bool {
        self.current_attempt_id(id) == attempt_id
    }

    fn is_session_url(&self, url: &str) -> bool {
        self.read_session(|session| SessionAdapter::contains_url(session, url))
            .unwrap_or(false)
    }

    fn handle_session_url_error(&mut self, url: &str, error: &str) {
        self.update_session_url(url, SessionUrlUpdate::Error(error));
        let _ = self.remove_overlay_file(url);
        self.show_ui_error_only(url, error);
    }

    pub(crate) fn handle_file_error_event(&mut self, id: String, error: String, attempt_id: u64) {
        log::error!("Download error: {id}: {error}");
        if self.handle_deleted_download_artifact(&id, &id) {
            return;
        }
        if !self.event_matches_current_attempt(&id, attempt_id) {
            log::info!("Ignoring stale download error after retry/reset: {id}");
            return;
        }
        if self.reset_is_waiting_for_new_attempt(&id) {
            log::info!("Ignoring stale download error after reset: {id}");
            return;
        }

        self.apply_core_event(CoreEvent::FileFailed {
            file_id: id.clone(),
            message: error.clone(),
        });
        self.mark_visible_file_error(&id, &id, &error);
        self.recompute_totals();
    }

    pub(crate) fn handle_scope_error_event(&mut self, scope: String, error: String) {
        log::error!("Download error: {scope}: {error}");
        if self.deleted_files.contains(&scope) {
            log::info!("Ignoring stale URL-level error after delete: {scope}");
            return;
        }
        if self.is_session_url(&scope) {
            self.handle_session_url_error(&scope, &error);
        } else {
            self.show_ui_error_only(&scope, &error);
        }
        self.recompute_totals();
    }

    fn register_queued_file(&mut self, file: &QueuedFile) -> bool {
        if !self.register_session_queued_file(&file.origin.submitted_url, &file.id, file.size) {
            return false;
        }
        self.ensure_core_file(
            &file.id,
            &file.origin.source_url,
            &file.id,
            file.size,
            file.count_toward_progress,
        );
        true
    }

    pub(crate) fn handle_file_queued_event(&mut self, file: QueuedFile) {
        if self.deleted_files.contains(&file.id) {
            return;
        }
        if !self.register_queued_file(&file) {
            return;
        }
    }

    fn handle_session_url_fetched(&mut self, url: &str) {
        let _ = self.drop_overlay_file(url);
        self.update_session_url(url, SessionUrlUpdate::Fetched);
        self.recompute_totals();
    }

    pub(crate) fn handle_url_resolved_event(&mut self, url: String) {
        if self.deleted_files.contains(&url) {
            log::info!("Ignoring stale URL resolution after delete: {url}");
            return;
        }
        self.handle_session_url_fetched(&url);
    }

    pub(crate) fn handle_file_start_event(&mut self, id: String, size: u64, attempt_id: u64) {
        log::info!("Download started: {id} ({})", format_bytes(size));
        if self.deleted_files.contains(&id) {
            return;
        }
        if !self.event_matches_current_attempt(&id, attempt_id) {
            log::info!("Ignoring stale download start after retry/reset: {id}");
            return;
        }
        self.reset_pending_files.remove(&id);
        let source_url = self
            .visible_file_context(&id)
            .and_then(|context| context.source_url)
            .unwrap_or_else(|| id.clone());
        self.ensure_core_file(&id, &source_url, &id, size, true);
        self.apply_core_event(CoreEvent::FileStarted {
            file_id: id.clone(),
            size,
        });
        self.reset_file_ui_rate(&id);
    }

    pub(crate) fn handle_file_progress_event(
        &mut self,
        id: Arc<str>,
        delta: ProgressDelta,
        attempt_id: u64,
    ) {
        if self.deleted_files.contains(id.as_ref()) {
            return;
        }
        if !self.event_matches_current_attempt(id.as_ref(), attempt_id) {
            log::info!("Ignoring stale download progress after retry/reset: {}", id);
            return;
        }
        self.reset_pending_files.remove(id.as_ref());
        let previous_downloaded = self
            .files
            .iter()
            .find(|file| file.id == id.as_ref())
            .map_or(0, |file| file.downloaded);
        self.apply_core_event(CoreEvent::FileProgress {
            file_id: id.to_string(),
            total_bytes_delta: delta.total_bytes_delta,
            network_bytes_delta: delta.network_bytes_delta,
        });
        let now = Instant::now();
        let _ = self.update_file_ui_progress(id.as_ref(), previous_downloaded, now);
    }

    pub(crate) fn handle_resume_reused_event(
        &mut self,
        id: String,
        chunks: usize,
        bytes: u64,
        attempt_id: u64,
    ) {
        if self.deleted_files.contains(&id) {
            return;
        }
        if !self.event_matches_current_attempt(&id, attempt_id) {
            log::info!("Ignoring stale resume reuse event after retry/reset: {id}");
            return;
        }
        self.reset_pending_files.remove(&id);
        self.apply_core_event(CoreEvent::FileReuseDetected {
            file_id: id.clone(),
            reused_bytes: bytes,
            reused_chunks: chunks,
        });
        log::info!(
            "Reusing {chunks} verified chunk(s) for {id} ({})",
            format_bytes(bytes)
        );
        self.set_resume_reuse_status(&id, chunks, bytes);
    }

    pub(crate) fn handle_file_complete_event(&mut self, id: String, attempt_id: u64) {
        log::info!("Download complete: {id}");
        if self.handle_deleted_download_artifact(&id, &id) {
            return;
        }
        if !self.event_matches_current_attempt(&id, attempt_id) {
            log::info!("Ignoring stale download completion after retry/reset: {id}");
            return;
        }
        if self.reset_is_waiting_for_new_attempt(&id) {
            log::info!("Ignoring stale download completion after reset: {id}");
            return;
        }
        self.apply_core_event(CoreEvent::FileCompleted {
            file_id: id.clone(),
        });
        self.recompute_totals();
        self.mark_visible_file_complete(&id, &id);
    }

    pub(crate) fn handle_file_cancelled_event(&mut self, id: String, attempt_id: u64) {
        log::info!("Download cancelled: {id}");
        if self.handle_deleted_download_artifact(&id, &id) {
            return;
        }
        if !self.event_matches_current_attempt(&id, attempt_id) {
            log::info!("Ignoring stale download cancellation after retry/reset: {id}");
            return;
        }
        if self.reset_is_waiting_for_new_attempt(&id) {
            log::info!("Ignoring stale download cancellation after reset: {id}");
            return;
        }
        self.cancellation_tokens.remove(&id);
        self.apply_core_event(CoreEvent::FileCancelled {
            file_id: id.clone(),
        });
        self.reset_file_ui_rate(&id);
        if self.paused {
            self.status = "Paused".to_string();
        }
    }

    pub(crate) fn perform_delete_file_action(&mut self, id: &str) {
        let context = self.visible_file_context(id);
        let artifact_path = context
            .as_ref()
            .map_or_else(|| id.to_string(), |context| context.artifact_path.clone());
        if let Some(context) = context.as_ref() {
            let _ = self.ensure_core_file_from_context(context);
        }
        let is_core_backed = self.core_state.files.contains_key(id);
        self.cancel_file_token(id);
        self.file_attempt_ids.remove(id);
        self.reset_pending_files.remove(id);
        self.deleted_files.insert(id.to_string());
        if !is_core_backed && self.is_session_url(id) {
            self.remove_session_url(id);
            self.urls.retain(|url| url != id);
        }
        if is_core_backed {
            self.apply_core_command(CoreCommand::DeleteFile {
                file_id: id.to_string(),
            });
        } else {
            let _ = self.remove_overlay_file(id);
            super::super::download::schedule_download_artifact_delete(artifact_path);
        }
        self.mark_file_skipped(id);
        if !is_core_backed {
            self.recompute_totals();
        }
    }

    pub(crate) fn perform_delete_package_action(&mut self, package_id: &str) {
        for file_id in self.package_file_ids(package_id) {
            self.perform_delete_file_action(&file_id);
        }
    }

    pub(crate) fn perform_retry_file_action(&mut self, id: &str) {
        let had_core_file = self.core_state.files.contains_key(id);
        let context = self.visible_file_context(id);
        let has_source_url = context
            .as_ref()
            .and_then(|context| self.ensure_core_file_from_context(context))
            .is_some();
        if !had_core_file
            && let Some(super::FileStatus::Error(message)) =
                context.as_ref().map(|context| &context.status)
        {
            self.apply_core_event(CoreEvent::FileFailed {
                file_id: id.to_string(),
                message: message.clone(),
            });
        }
        self.bump_file_attempt_id(id);
        self.reset_pending_files.remove(id);
        self.apply_core_command(CoreCommand::RetryFile {
            file_id: id.to_string(),
        });
        if has_source_url {
            self.reset_file_ui_rate(id);
        } else {
            self.status = format!("Retry unavailable for {id}");
            if !self.core_state.files.contains_key(id) {
                self.show_overlay_error(id, id, "Retry unavailable for this file", true);
            }
        }
    }

    pub(crate) fn perform_retry_package_action(&mut self, package_id: &str) {
        for file_id in self.package_file_ids(package_id) {
            let retryable =
                self.core_state.files.get(&file_id).is_some_and(|file| {
                    matches!(file.lifecycle, crate::core::FileLifecycle::Failed)
                }) || self
                    .visible_file_context(&file_id)
                    .is_some_and(|context| matches!(context.status, super::FileStatus::Error(_)));
            if retryable {
                self.perform_retry_file_action(&file_id);
            }
        }
    }

    pub(crate) fn perform_reset_file_action(&mut self, id: &str) {
        let Some(context) = self.visible_file_context(id) else {
            return;
        };
        if self.ensure_core_file_from_context(&context).is_none() {
            if !self.core_state.files.contains_key(id) {
                self.show_overlay_error(id, id, "Reset unavailable for this file", true);
            }
            self.status = "Reset unavailable for selected file".to_string();
            self.recompute_totals();
            return;
        };

        self.cancel_file_token(id);
        self.bump_file_attempt_id(id);
        self.reset_pending_files.insert(id.to_string());

        self.apply_core_command(CoreCommand::ResetFile {
            file_id: id.to_string(),
        });
        self.reset_file_ui_rate(id);
    }

    pub(crate) fn perform_reset_package_action(&mut self, package_id: &str) {
        for file_id in self.package_file_ids(package_id) {
            self.perform_reset_file_action(&file_id);
        }
    }

    pub(crate) fn apply_config_update(
        &mut self,
        chunks_per_file: Option<usize>,
        concurrent_files: Option<usize>,
        force_overwrite: Option<bool>,
        cleanup_on_error: Option<bool>,
    ) {
        if let Some(value) = chunks_per_file {
            self.config.config.chunks_per_file = value.max(1);
        }
        if let Some(value) = concurrent_files {
            self.config.config.concurrent_files = value.max(1);
        }
        if let Some(value) = force_overwrite {
            self.config.config.force_overwrite = value;
        }
        if let Some(value) = cleanup_on_error {
            self.config.config.cleanup_on_error = value;
        }
    }

    pub(crate) fn handle_ui_action(&mut self, action: UiAction) {
        match action {
            UiAction::AddUrls(urls) => {
                let count = urls.len();
                for url in urls {
                    self.submit_url(url);
                }
                self.status = format!("Received {count} URL(s) from bookmarklet");
            }
            UiAction::Login {
                email,
                password,
                mfa,
            } => {
                if self.login.set_credentials(email, password, mfa) {
                    self.begin_login();
                }
            }
            UiAction::TogglePause => {
                if self.paused {
                    self.resume_downloads();
                } else {
                    self.pause_downloads();
                }
            }
            UiAction::DeleteFile(id) => self.perform_delete_file_action(&id),
            UiAction::DeletePackage(id) => self.perform_delete_package_action(&id),
            UiAction::RetryFile(id) => self.perform_retry_file_action(&id),
            UiAction::RetryPackage(id) => self.perform_retry_package_action(&id),
            UiAction::ResetFile(id) => self.perform_reset_file_action(&id),
            UiAction::ResetPackage(id) => self.perform_reset_package_action(&id),
            UiAction::UpdateConfig {
                chunks_per_file,
                concurrent_files,
                force_overwrite,
                cleanup_on_error,
            } => self.apply_config_update(
                chunks_per_file,
                concurrent_files,
                force_overwrite,
                cleanup_on_error,
            ),
        }
    }
}
