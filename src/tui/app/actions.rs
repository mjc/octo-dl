use std::fmt::Write as _;
use std::time::Instant;

use crate::{
    core::{CoreCommand, CoreEvent, FileId, FileLifecycle, PackageId},
    format_bytes,
};

use super::{
    App, ProgressDelta, QueuedFile, SessionAdapter, UiAction, VerificationTarget,
    VisibleFileContext,
};

const MAX_UI_ACTIONS_PER_TICK: usize = 64;

fn reverify_target_for_core_file(file: &crate::core::FileState) -> Option<VerificationTarget> {
    match &file.lifecycle {
        FileLifecycle::Complete => Some(VerificationTarget::Completed),
        FileLifecycle::Downloading => Some(VerificationTarget::Resume),
        FileLifecycle::Planned | FileLifecycle::Queued | FileLifecycle::Failed { .. } => {
            (file.progress.visible_completed_bytes > 0
                || file.progress.verified_existing_bytes > 0
                || file.progress.downloaded_network_bytes > 0)
                .then_some(VerificationTarget::Resume)
        }
    }
}

impl App {
    pub(crate) fn track_shutdown_pending_file(&mut self, id: &FileId) {
        self.shutdown_pending_files.insert(id.clone());
    }

    pub(crate) fn resolve_shutdown_pending_file(&mut self, id: &FileId) {
        self.shutdown_pending_files.remove(id);
    }

    fn clear_verification_state(&mut self, id: &FileId) {
        self.verifying_files.remove(id);
        self.verification_inflight_files.remove(id);
        self.verification_targets.remove(id);
        self.shutdown_blocking_verifications.remove(id);
        self.startup_resume_pending_files.remove(id);
        self.reverify_pending_files.remove(id);
    }

    pub(crate) fn forget_visible_file(&mut self, id: &FileId) {
        self.overlay_files.shift_remove(id);
        self.files.retain(|file| file.id != *id);
        self.visible_file_positions = self
            .files
            .iter()
            .enumerate()
            .map(|(index, file)| (file.id.clone(), index))
            .collect();
        self.file_ui.remove(id);
    }

    pub(crate) fn submit_url(&mut self, url: String) {
        if self.has_tracked_url(&url) {
            return;
        }
        self.ensure_session_for_pending_urls();
        self.queue_url_placeholder(url.clone());
        self.apply_core_command(CoreCommand::SubmitUrl { url });
    }

    fn retry_source_url(&mut self, url: &str) {
        self.ensure_session_for_pending_urls();
        self.queue_url_placeholder(url.to_string());
        self.apply_core_command(CoreCommand::SubmitUrl {
            url: url.to_string(),
        });
    }

    pub(crate) fn drain_ui_actions(
        &mut self,
        action_rx: &mut tokio::sync::mpsc::UnboundedReceiver<UiAction>,
    ) -> bool {
        let mut handled = false;
        for _ in 0..MAX_UI_ACTIONS_PER_TICK {
            let Ok(action) = action_rx.try_recv() else {
                break;
            };
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
        for file_id in downloading_ids {
            self.cancel_file_token(&file_id);
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
        if let Some(source_url) = source_url.as_ref()
            && !self.core_state.files.contains_key(&context.id)
        {
            self.ensure_core_file(
                &context.id,
                source_url,
                &context.artifact_path,
                context.size,
                crate::core::FileAccounting::CurrentRun,
            );
        }
        source_url
    }

    fn cancel_file_token(&mut self, id: &FileId) {
        if let Some(token) = self.cancellation_tokens.remove(id) {
            token.cancel();
        }
    }

    fn reset_is_waiting_for_new_attempt(&self, id: &FileId) -> bool {
        self.reset_pending_files.contains(id)
    }

    fn current_attempt_id(&self, id: &FileId) -> u64 {
        self.file_attempt_ids.get(id).copied().unwrap_or(0)
    }

    fn bump_file_attempt_id(&mut self, id: &FileId) -> u64 {
        let next = self.current_attempt_id(id).saturating_add(1);
        self.file_attempt_ids.insert(id.clone(), next);
        next
    }

    fn event_matches_current_attempt(&self, id: &FileId, attempt_id: u64) -> bool {
        self.current_attempt_id(id) == attempt_id
    }

    fn is_session_url(&self, url: &str) -> bool {
        self.core_state
            .url_order
            .iter()
            .any(|tracked_url| tracked_url == url)
    }

    fn is_tracked_error_scope(&self, scope: &str) -> bool {
        matches!(scope, "setup" | "download")
            || self.core_state.url_order.iter().any(|url| url == scope)
            || self.overlay_files.contains_key(scope)
    }

    fn handle_session_url_error(&mut self, url: &str, error: &str) {
        self.apply_core_event(CoreEvent::UrlFailed {
            url: url.to_string(),
            message: error.to_string(),
        });
        let _ = self
            .overlay_files
            .shift_remove(&FileId::from(url))
            .map(|row| row.file().clone());
        self.sync_visible_files();
        self.show_ui_error_only(url, error);
    }

    pub(crate) fn handle_file_error_event(&mut self, id: FileId, error: String, attempt_id: u64) {
        log::error!("Download error: {id}: {error}");
        if !self.event_matches_current_attempt(&id, attempt_id) {
            log::info!("Ignoring stale download error after retry/reset: {id}");
            return;
        }
        if self.reset_is_waiting_for_new_attempt(&id) {
            log::info!("Ignoring stale download error after reset: {id}");
            return;
        }
        if !self.core_state.files.contains_key(&id) {
            log::info!("Ignoring download error for untracked file: {id}");
            return;
        }

        self.verifying_files.remove(&id);
        self.verification_inflight_files.remove(&id);
        self.verification_targets.remove(&id);
        self.shutdown_blocking_verifications.remove(&id);
        self.cancellation_tokens.remove(&id);
        self.resolve_shutdown_pending_file(&id);
        self.apply_core_event(CoreEvent::FileFailed {
            file_id: id.clone(),
            message: error.clone(),
        });
        self.update_download_status_message();
    }

    pub(crate) fn handle_scope_error_event(&mut self, scope: String, error: String) {
        log::error!("Download error: {scope}: {error}");
        self.clear_verification_state(&FileId::from(scope.as_str()));
        if self.is_session_url(&scope) {
            self.handle_session_url_error(&scope, &error);
        } else if self.is_tracked_error_scope(&scope) {
            self.show_ui_error_only(&scope, &error);
        } else {
            log::info!("Ignoring error for untracked scope: {scope}");
        }
        self.recompute_totals();
    }

    fn register_queued_file(&mut self, file: &QueuedFile) -> bool {
        let package_display_name = file
            .origin
            .package_display_name
            .clone()
            .unwrap_or_else(|| file.origin.source_url.clone());
        let package_key = crate::core::PackageKey::new(package_display_name.clone());
        let existing_package = self.core_state.package_for_key(&package_key);
        let package_id = file
            .origin
            .package_id
            .clone()
            .or_else(|| existing_package.map(|package| package.id))
            .unwrap_or_else(|| PackageId::for_package_key(&package_key));
        if file.origin.submitted_url != file.origin.source_url {
            self.core_state
                .url_order
                .retain(|url| url != &file.origin.submitted_url);
            let submitted_id = FileId::from(file.origin.submitted_url.as_str());
            self.forget_visible_file(&submitted_id);
        }
        self.ensure_core_file_in_package(
            &file.id,
            &package_id.to_string(),
            &package_display_name,
            &file.origin.source_url,
            file.id.as_str(),
            file.size,
            file.accounting,
        );
        true
    }

    pub(crate) fn handle_file_queued_event(&mut self, file: QueuedFile) {
        if !self
            .core_state
            .url_order
            .iter()
            .any(|url| url == &file.origin.source_url || url == &file.origin.submitted_url)
        {
            return;
        }
        if !self.register_queued_file(&file) {
            return;
        }
        self.update_download_status_message();
    }

    fn handle_session_url_fetched(&mut self, url: &str) {
        let url_id = FileId::from(url);
        self.forget_visible_file(&url_id);
        self.sync_visible_files();
        self.apply_core_event(CoreEvent::UrlResolved {
            url: url.to_string(),
        });
        self.recompute_totals();
    }

    pub(crate) fn handle_url_resolved_event(&mut self, url: String) {
        self.handle_session_url_fetched(&url);
    }

    pub(crate) fn handle_file_start_event(&mut self, id: FileId, size: u64, attempt_id: u64) {
        log::info!("Download started: {id} ({})", format_bytes(size));
        if !self.event_matches_current_attempt(&id, attempt_id) {
            log::info!("Ignoring stale download start after retry/reset: {id}");
            return;
        }
        if !self.core_state.files.contains_key(&id) {
            log::info!("Ignoring download start for untracked file: {id}");
            return;
        }
        self.verifying_files.remove(&id);
        self.verification_inflight_files.remove(&id);
        self.verification_targets.remove(&id);
        self.shutdown_blocking_verifications.remove(&id);
        self.reset_pending_files.remove(&id);
        let preserve_resume_progress = self.reverify_pending_files.remove(&id)
            || self.startup_resume_pending_files.remove(&id);
        if preserve_resume_progress {
            self.apply_core_event(CoreEvent::FileResumeStarted {
                file_id: id.clone(),
                size,
            });
        } else {
            self.apply_core_event(CoreEvent::FileStarted {
                file_id: id.clone(),
                size,
            });
        }
        self.reset_file_ui_rate(&id);
        self.update_download_status_message();
    }

    pub(crate) fn handle_resume_validation_started_event(&mut self, id: FileId, attempt_id: u64) {
        if !self.event_matches_current_attempt(&id, attempt_id) {
            log::info!("Ignoring stale resume validation start after retry/reset: {id}");
            return;
        }
        if !self.core_state.files.contains_key(&id) {
            log::info!("Ignoring resume validation start for untracked file: {id}");
            return;
        }
        self.verifying_files.insert(id.clone());
        self.verification_inflight_files.insert(id.clone());
        self.shutdown_blocking_verifications.insert(id.clone());
        self.verification_targets
            .insert(id.clone(), VerificationTarget::Resume);
        self.apply_core_event(CoreEvent::FileVerificationStarted {
            file_id: id.clone(),
        });
        self.refresh_visible_core_file(&id);
    }

    pub(crate) fn handle_file_progress_event(
        &mut self,
        id: FileId,
        delta: ProgressDelta,
        attempt_id: u64,
    ) {
        if !self.event_matches_current_attempt(&id, attempt_id) {
            log::info!("Ignoring stale download progress after retry/reset: {}", id);
            return;
        }
        if !self.core_state.files.contains_key(&id) {
            log::info!("Ignoring download progress for untracked file: {id}");
            return;
        }
        self.reset_pending_files.remove(&id);
        if delta.network_bytes_delta > 0 {
            self.verifying_files.remove(&id);
            self.verification_inflight_files.remove(&id);
            self.verification_targets.remove(&id);
            self.shutdown_blocking_verifications.remove(&id);
        }
        self.apply_core_progress_event(CoreEvent::FileProgress {
            file_id: id.clone(),
            total_bytes_delta: delta.total_bytes_delta,
            network_bytes_delta: delta.network_bytes_delta,
        });
        let _ = self.refresh_visible_progress_file(&id, Instant::now());
    }

    pub(crate) fn handle_verification_progress_event(&mut self, id: FileId, bytes_delta: u64) {
        if !self.verifying_files.contains(&id) {
            log::info!("Ignoring verification progress for non-verifying file: {id}");
            return;
        }
        if !self.verification_inflight_files.contains(&id) {
            log::info!("Ignoring verification progress for skipped file: {id}");
            return;
        }
        if !self.verification_targets.contains_key(&id) {
            log::info!("Ignoring verification progress without target: {id}");
            return;
        }
        if !self.core_state.files.contains_key(&id) {
            log::info!("Ignoring verification progress for untracked file: {id}");
            return;
        }
        self.apply_core_progress_event(CoreEvent::FileVerificationProgress {
            file_id: id.clone(),
            bytes_delta,
        });
        let _ = self.refresh_visible_progress_file(&id, Instant::now());
    }

    pub(crate) fn handle_resume_reused_event(
        &mut self,
        id: FileId,
        chunks: usize,
        bytes: u64,
        attempt_id: u64,
    ) {
        if !self.event_matches_current_attempt(&id, attempt_id) {
            log::info!("Ignoring stale resume reuse event after retry/reset: {id}");
            return;
        }
        if !self.core_state.files.contains_key(&id) {
            log::info!("Ignoring resume reuse for untracked file: {id}");
            return;
        }
        self.reset_pending_files.remove(&id);
        self.apply_core_event(CoreEvent::FileReuseDetected {
            file_id: id.clone(),
            reused_bytes: bytes,
            reused_chunks: chunks,
        });
        self.refresh_visible_core_file(&id);
        log::info!(
            "Reusing {chunks} verified chunk(s) for {id} ({})",
            format_bytes(bytes)
        );
        self.set_resume_reuse_status(&id, chunks, bytes);
    }

    pub(crate) fn handle_resume_reverified_event(&mut self, id: FileId, chunks: usize, bytes: u64) {
        if !self.core_state.files.contains_key(&id) {
            log::info!("Ignoring resume reverify for untracked file: {id}");
            return;
        }
        self.resolve_shutdown_pending_file(&id);
        self.reset_pending_files.remove(&id);
        self.apply_core_event(CoreEvent::FileResumeReverified {
            file_id: id.clone(),
            verified_bytes: bytes,
            verified_chunks: chunks,
        });
        self.verification_inflight_files.remove(&id);
        self.verification_targets.remove(&id);
        self.shutdown_blocking_verifications.remove(&id);
        if !self.reverify_pending_files.contains(&id) {
            self.verifying_files.remove(&id);
        }
        self.refresh_visible_core_file(&id);
        log::info!(
            "Reverified {chunks} chunk(s) for {id} ({})",
            format_bytes(bytes)
        );
        self.set_resume_reuse_status(&id, chunks, bytes);
    }

    pub(crate) fn handle_completed_file_verified_event(&mut self, id: FileId, bytes: u64) {
        self.verifying_files.remove(&id);
        self.verification_inflight_files.remove(&id);
        self.verification_targets.remove(&id);
        self.shutdown_blocking_verifications.remove(&id);
        self.resolve_shutdown_pending_file(&id);
        if !self.core_state.files.contains_key(&id) {
            log::info!("Ignoring completed-file verification for untracked file: {id}");
            return;
        }
        let was_complete = self
            .core_state
            .files
            .get(&id)
            .is_some_and(|file| matches!(file.lifecycle, FileLifecycle::Complete));
        self.apply_core_event(CoreEvent::FileResumeReverified {
            file_id: id.clone(),
            verified_bytes: bytes,
            verified_chunks: 0,
        });
        if was_complete {
            self.apply_core_event(CoreEvent::FileVerificationCompleted {
                file_id: id.clone(),
            });
        } else {
            self.apply_core_event(CoreEvent::FileCompleted {
                file_id: id.clone(),
            });
        }
        self.refresh_visible_core_file(&id);
        let mut status = String::with_capacity(id.as_str().len().saturating_add(24));
        let _ = write!(status, "Verified {id}: ");
        crate::format::push_formatted_bytes(&mut status, bytes);
        self.status = status;
    }

    pub(crate) fn handle_verification_skipped_event(&mut self, id: FileId, completed: bool) {
        self.resolve_shutdown_pending_file(&id);
        self.clear_verification_state(&id);
        if !self.core_state.files.contains_key(&id) {
            log::info!("Ignoring verification skip for untracked file: {id}");
            return;
        }
        if completed {
            self.apply_core_event(CoreEvent::FileVerificationCompleted {
                file_id: id.clone(),
            });
        } else {
            self.apply_core_event(CoreEvent::FileCancelled {
                file_id: id.clone(),
            });
        }
        self.refresh_visible_core_file(&id);
        self.status = format!("Verification skipped for {id}");
    }

    pub(crate) fn handle_file_complete_event(&mut self, id: FileId, attempt_id: u64) {
        log::info!("Download complete: {id}");
        if !self.event_matches_current_attempt(&id, attempt_id) {
            log::info!("Ignoring stale download completion after retry/reset: {id}");
            return;
        }
        if self.reset_is_waiting_for_new_attempt(&id) {
            log::info!("Ignoring stale download completion after reset: {id}");
            return;
        }
        if !self.core_state.files.contains_key(&id) {
            log::info!("Ignoring download completion for untracked file: {id}");
            return;
        }
        self.verifying_files.remove(&id);
        self.verification_inflight_files.remove(&id);
        self.verification_targets.remove(&id);
        self.shutdown_blocking_verifications.remove(&id);
        self.cancellation_tokens.remove(&id);
        self.resolve_shutdown_pending_file(&id);
        self.apply_core_event(CoreEvent::FileCompleted {
            file_id: id.clone(),
        });
        self.reset_file_ui_rate(&id);
        self.update_download_status_message();
    }

    pub(crate) fn handle_file_cancelled_event(&mut self, id: FileId, attempt_id: u64) {
        log::info!("Download cancelled: {id}");
        if !self.event_matches_current_attempt(&id, attempt_id) {
            log::info!("Ignoring stale download cancellation after retry/reset: {id}");
            return;
        }
        if self.reset_is_waiting_for_new_attempt(&id) {
            log::info!("Ignoring stale download cancellation after reset: {id}");
            return;
        }
        if !self.core_state.files.contains_key(&id) {
            log::info!("Ignoring download cancellation for untracked file: {id}");
            return;
        }
        self.cancellation_tokens.remove(&id);
        self.resolve_shutdown_pending_file(&id);
        self.clear_verification_state(&id);
        self.apply_core_event(CoreEvent::FileCancelled {
            file_id: id.clone(),
        });
        self.reset_file_ui_rate(&id);
        self.update_download_status_message();
    }

    pub(crate) fn perform_delete_file_action(&mut self, id: &FileId) {
        let is_core_backed = self.core_state.files.contains_key(id);
        self.cancel_file_token(id);
        self.resolve_shutdown_pending_file(id);
        self.file_attempt_ids.remove(id);
        self.reset_pending_files.remove(id);
        self.clear_verification_state(id);

        if !is_core_backed && self.is_session_url(id.as_str()) {
            let _ = self.mutate_session_and_save(|session| {
                SessionAdapter::remove_url(session, id.as_str())
            });
            self.core_state.url_order.retain(|url| url != id.as_str());
        }
        if is_core_backed {
            self.apply_core_command(CoreCommand::DeleteFile {
                file_id: id.clone(),
            });
        } else {
            self.forget_visible_file(id);
            self.sync_visible_files();
        }
        let _ = self
            .mutate_session_and_save(|session| SessionAdapter::remove_file(session, id.as_str()));
        if !is_core_backed {
            self.recompute_totals();
        }
    }

    pub(crate) fn perform_delete_package_action(&mut self, package_id: PackageId) {
        let file_contexts: Vec<_> = self
            .core_state
            .package_files(&package_id)
            .map(|file| {
                let file_id = file.id.clone();
                let source_url = file.source_url.clone();
                (file_id, source_url)
            })
            .collect();

        let source_ids: Vec<_> = file_contexts
            .iter()
            .map(|(_, source_url)| FileId::from(source_url.as_str()))
            .collect();

        for (file_id, _) in &file_contexts {
            self.cancel_file_token(file_id);
            self.resolve_shutdown_pending_file(file_id);
            self.file_attempt_ids.remove(file_id);
            self.reset_pending_files.remove(file_id);
            self.clear_verification_state(file_id);
        }
        self.apply_core_command(CoreCommand::DeletePackage { package_id });
        for source_id in source_ids {
            self.forget_visible_file(&source_id);
        }
    }

    pub(crate) fn perform_retry_file_action(&mut self, id: &FileId) {
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
                file_id: id.clone(),
                message: message.clone(),
            });
        }
        self.bump_file_attempt_id(id);
        self.reset_pending_files.remove(id);
        self.clear_verification_state(id);
        self.apply_core_command(CoreCommand::RetryFile {
            file_id: id.clone(),
        });
        if has_source_url {
            self.reset_file_ui_rate(id);
        } else {
            self.status = format!("Retry unavailable for {id}");
            if !self.core_state.files.contains_key(id) {
                self.show_overlay_error(id, id.as_str(), "Retry unavailable for this file");
            }
        }
    }

    pub(crate) fn perform_retry_package_action(&mut self, package_id: PackageId) {
        let Some(package) = self.core_state.packages.get(&package_id) else {
            return;
        };
        let package_key = package.key.clone();
        let package_failed = package.error.is_some()
            || matches!(package.status(), crate::core::PackageStatus::Failed);
        let package_files: Vec<_> = self
            .core_state
            .package_files(&package_id)
            .map(|file| {
                (
                    file.id.clone(),
                    file.source_url.clone(),
                    matches!(file.lifecycle, crate::core::FileLifecycle::Failed { .. }),
                )
            })
            .collect();
        let mut source_urls = std::collections::BTreeSet::new();
        let mut retried_file = false;

        for (file_id, source_url, core_failed) in package_files {
            source_urls.insert(source_url);
            let retryable = core_failed
                || self
                    .visible_file_context(&file_id)
                    .is_some_and(|context| matches!(context.status, super::FileStatus::Error(_)));
            if retryable {
                self.perform_retry_file_action(&file_id);
                retried_file = true;
            }
        }

        if !retried_file && package_failed {
            if source_urls.is_empty()
                && self.session.as_ref().is_some_and(|session| {
                    session
                        .urls
                        .iter()
                        .any(|url| url.url == package_key.as_str())
                })
            {
                source_urls.insert(package_key.to_string());
            }
            let _ = self.mutate_session_and_save(|session| {
                session.packages.retain(|package| package.id != package_id);
            });
            self.core_state.packages.shift_remove(&package_id);
            for source_url in source_urls {
                self.retry_source_url(&source_url);
            }
            self.sync_visible_files();
            self.recompute_totals();
        }
    }

    pub(crate) fn perform_reverify_file_action(&mut self, id: &FileId) {
        let Some(context) = self.visible_file_context(id) else {
            return;
        };
        let Some(source_url) = self.ensure_core_file_from_context(&context) else {
            self.status = "Reverify unavailable for selected file".to_string();
            if !self.core_state.files.contains_key(id) {
                self.show_overlay_error(id, id.as_str(), "Reverify unavailable for this file");
            }
            self.recompute_totals();
            return;
        };
        let Some(target) = self
            .core_state
            .files
            .get(id)
            .and_then(reverify_target_for_core_file)
        else {
            self.clear_verification_state(id);
            self.refresh_visible_core_file(id);
            self.status = "Reverify unavailable for selected file".to_string();
            self.recompute_totals();
            return;
        };

        self.cancel_file_token(id);
        self.startup_resume_pending_files.remove(id);
        if matches!(context.status, super::FileStatus::Downloading) {
            self.bump_file_attempt_id(id);
        }
        self.verifying_files.insert(id.clone());
        self.verification_inflight_files.insert(id.clone());
        self.verification_targets.insert(id.clone(), target);
        self.apply_core_event(CoreEvent::FileVerificationStarted {
            file_id: id.clone(),
        });
        self.refresh_visible_core_file(id);
        if matches!(context.status, super::FileStatus::Downloading) {
            self.apply_core_event(CoreEvent::FileCancelled {
                file_id: id.clone(),
            });
            self.reset_file_ui_rate(id);
            self.reverify_pending_files.insert(id.clone());
        }
        self.reset_pending_files.remove(id);
        let request = if target == VerificationTarget::Completed {
            crate::tui::event::DownloadRequest::VerifyCompletedFileIds {
                source_url,
                file_ids: vec![id.clone()],
            }
        } else {
            crate::tui::event::DownloadRequest::ReverifyFileIds {
                source_url,
                file_ids: vec![id.clone()],
            }
        };
        let _ = self.url_tx.send(request);
        self.status = if target == VerificationTarget::Completed {
            format!("Verifying completed file {id}...")
        } else {
            format!("Reverifying resume data for {id}...")
        };
    }

    pub(crate) fn perform_reverify_package_action(&mut self, package_id: PackageId) {
        let mut grouped_resume: Vec<(String, Vec<FileId>)> = Vec::new();
        let mut grouped_completed: Vec<(String, Vec<FileId>)> = Vec::new();
        let mut skipped_stale_files = Vec::new();
        let files = self
            .core_state
            .package_files(&package_id)
            .filter_map(|file| match reverify_target_for_core_file(file) {
                Some(target) => Some((
                    file.id.clone(),
                    file.source_url.clone(),
                    file.lifecycle.clone(),
                    target,
                )),
                None => {
                    skipped_stale_files.push(file.id.clone());
                    None
                }
            })
            .collect::<Vec<_>>();

        for file_id in skipped_stale_files {
            self.clear_verification_state(&file_id);
            self.refresh_visible_core_file(&file_id);
        }

        for (file_id, source_url, lifecycle, target) in files {
            self.cancel_file_token(&file_id);
            self.startup_resume_pending_files.remove(&file_id);
            if matches!(lifecycle, crate::core::FileLifecycle::Downloading) {
                self.bump_file_attempt_id(&file_id);
            }
            self.verifying_files.insert(file_id.clone());
            self.verification_inflight_files.insert(file_id.clone());
            self.verification_targets.insert(file_id.clone(), target);
            self.apply_core_event(CoreEvent::FileVerificationStarted {
                file_id: file_id.clone(),
            });
            self.refresh_visible_core_file(&file_id);
            if matches!(lifecycle, crate::core::FileLifecycle::Downloading) {
                self.apply_core_event(CoreEvent::FileCancelled {
                    file_id: file_id.clone(),
                });
                self.reset_file_ui_rate(&file_id);
                self.reverify_pending_files.insert(file_id.clone());
            }
            self.reset_pending_files.remove(&file_id);

            let grouped = if target == VerificationTarget::Completed {
                &mut grouped_completed
            } else {
                &mut grouped_resume
            };
            if let Some((_, file_ids)) = grouped
                .iter_mut()
                .find(|(group_source_url, _)| group_source_url == &source_url)
            {
                file_ids.push(file_id.clone());
            } else {
                grouped.push((source_url, vec![file_id.clone()]));
            }
        }

        let file_count = grouped_resume
            .iter()
            .chain(grouped_completed.iter())
            .map(|(_, file_ids)| file_ids.len())
            .sum::<usize>();
        if file_count == 0 {
            self.status = "No package file(s) have resume data to verify".to_string();
            return;
        }
        for (source_url, file_ids) in grouped_resume {
            let _ = self
                .url_tx
                .send(crate::tui::event::DownloadRequest::ReverifyFileIds {
                    source_url,
                    file_ids,
                });
        }
        for (source_url, file_ids) in grouped_completed {
            let _ = self
                .url_tx
                .send(crate::tui::event::DownloadRequest::VerifyCompletedFileIds {
                    source_url,
                    file_ids,
                });
        }
        self.status = format!("Verifying {file_count} package file(s), 4 at a time...");
    }

    pub(crate) fn perform_reset_file_action(&mut self, id: &FileId) {
        let Some(context) = self.visible_file_context(id) else {
            return;
        };
        if self.ensure_core_file_from_context(&context).is_none() {
            if !self.core_state.files.contains_key(id) {
                self.show_overlay_error(id, id.as_str(), "Reset unavailable for this file");
            }
            self.status = "Reset unavailable for selected file".to_string();
            self.recompute_totals();
            return;
        };

        self.cancel_file_token(id);
        self.bump_file_attempt_id(id);
        self.reset_pending_files.insert(id.clone());
        self.clear_verification_state(id);

        self.apply_core_command(CoreCommand::ResetFile {
            file_id: id.clone(),
        });
        self.reset_file_ui_rate(id);
    }

    pub(crate) fn perform_reset_package_action(&mut self, package_id: PackageId) {
        let file_ids: Vec<_> = self
            .core_state
            .package_files(&package_id)
            .map(|file| file.id.clone())
            .collect();
        for file_id in file_ids {
            self.perform_reset_file_action(&file_id);
        }
    }

    pub(crate) fn perform_move_package_action(&mut self, package_id: PackageId, delta: isize) {
        self.apply_core_command(CoreCommand::MovePackage { package_id, delta });
    }

    pub(crate) fn perform_move_file_action(&mut self, file_id: FileId, delta: isize) {
        self.apply_core_command(CoreCommand::MoveFile { file_id, delta });
    }

    pub(crate) fn apply_config_update(
        &mut self,
        chunks_per_file: Option<usize>,
        mega_chunks_per_request: Option<usize>,
        concurrent_files: Option<usize>,
        force_overwrite: Option<bool>,
        cleanup_on_error: Option<bool>,
    ) {
        if let Some(value) = chunks_per_file {
            self.config.config.chunks_per_file = value.max(1);
        }
        if let Some(value) = mega_chunks_per_request {
            self.config.config.mega_chunks_per_request = value.max(1);
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
                self.with_deferred_batch_updates(|app| {
                    for url in urls {
                        app.submit_url(url);
                    }
                });
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
            UiAction::DeletePackage(id) => self.perform_delete_package_action(id),
            UiAction::RetryFile(id) => self.perform_retry_file_action(&id),
            UiAction::RetryPackage(id) => self.perform_retry_package_action(id),
            UiAction::ReverifyFile(id) => self.perform_reverify_file_action(&id),
            UiAction::ReverifyPackage(id) => self.perform_reverify_package_action(id),
            UiAction::ResetFile(id) => self.perform_reset_file_action(&id),
            UiAction::ResetPackage(id) => self.perform_reset_package_action(id),
            UiAction::MoveFile { file_id, delta } => self.perform_move_file_action(file_id, delta),
            UiAction::MovePackage { package_id, delta } => {
                self.perform_move_package_action(package_id, delta)
            }
            UiAction::UpdateConfig {
                chunks_per_file,
                mega_chunks_per_request,
                concurrent_files,
                force_overwrite,
                cleanup_on_error,
            } => self.apply_config_update(
                chunks_per_file,
                mega_chunks_per_request,
                concurrent_files,
                force_overwrite,
                cleanup_on_error,
            ),
        }
    }
}
