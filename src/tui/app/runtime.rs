#![allow(clippy::too_many_lines, clippy::useless_let_if_seq)]

#[cfg(test)]
use crate::tui::event::DownloadEventSender;
use std::future::Future;
use std::io;
use std::time::Duration;

use sysinfo::{ProcessesToUpdate, System};
use tokio::sync::{mpsc, watch};

use crate::{
    DownloadConfig,
    core::{FileId, SavedCredentials, SavedMegaSession, SessionSnapshot, SessionUrlSnapshot},
    format_bytes,
    tui::dashboard::DashboardUiMode,
};

use super::{App, DownloadEvent, FileEntry, FileIdSet, FileStatus, UiAction};

const MAX_DOWNLOAD_EVENTS_PER_TICK: usize = 256;
const MAX_TOKEN_MESSAGES_PER_TICK: usize = 256;
const HEADLESS_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

fn progress_summary_pct(
    files_total: usize,
    total_downloaded: u64,
    total_size: u64,
    total_network_downloaded: u64,
) -> Option<u64> {
    if files_total == 0 || total_size == 0 || total_network_downloaded == 0 {
        return None;
    }
    let pct = total_downloaded * 100 / total_size;
    (pct > 0 && pct < 100).then_some(pct)
}

impl App {
    fn active_shutdown_file_ids(&self) -> Vec<FileId> {
        let mut pending = FileIdSet::default();
        pending.extend(
            self.files
                .iter()
                .filter(|file| file.status.is_downloading())
                .map(|file| file.id.clone()),
        );
        pending.extend(self.shutdown_blocking_verifications.iter().cloned());
        pending.extend(self.cancellation_tokens.keys().cloned());
        pending.into_iter().collect()
    }

    fn skip_nonblocking_shutdown_verifications(&mut self) {
        let file_ids = self
            .verification_inflight_files
            .iter()
            .filter(|id| !self.shutdown_blocking_verifications.contains(*id))
            .cloned()
            .collect::<Vec<_>>();
        for id in file_ids {
            let completed =
                self.verification_targets.get(&id) == Some(&super::VerificationTarget::Completed);
            self.handle_verification_skipped_event(id, completed);
        }
    }

    pub(crate) fn skip_all_shutdown_verifications(&mut self) {
        let file_ids = self
            .verification_inflight_files
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        for id in file_ids {
            let completed =
                self.verification_targets.get(&id) == Some(&super::VerificationTarget::Completed);
            self.handle_verification_skipped_event(id, completed);
        }
    }

    pub(crate) fn begin_shutdown(&mut self) -> bool {
        self.drain_token_messages();
        self.skip_nonblocking_shutdown_verifications();
        self.shutdown_pending_files.clear();
        for id in self.active_shutdown_file_ids() {
            self.track_shutdown_pending_file(&id);
        }
        self.pause_downloads();
        for (id, token) in self.cancellation_tokens.clone() {
            token.cancel();
            self.track_shutdown_pending_file(&id);
        }
        !self.shutdown_pending_files.is_empty()
    }

    fn flush_pending_progress_events(
        &mut self,
        pending_progress: &mut Vec<(
            FileId,
            crate::core::ProgressDelta,
            super::super::event::DownloadAttemptId,
        )>,
    ) -> bool {
        if pending_progress.is_empty() {
            return false;
        }

        for (id, delta, attempt_id) in pending_progress.drain(..) {
            self.handle_file_progress_event(id, delta, attempt_id);
        }

        true
    }

    fn saved_login_credentials(&self) -> io::Result<SavedCredentials> {
        let key = self.persisted_credential_key()?;
        Ok(SavedCredentials::encrypt_with_key(
            self.login.email(),
            self.login.password(),
            None,
            &key,
        ))
    }

    pub(crate) fn complete_login(
        &mut self,
        success: bool,
        error: Option<String>,
        saved_session: Option<SavedMegaSession>,
        clear_saved_session: bool,
    ) {
        self.login.logging_in = false;
        if !success {
            self.client_rx = None;
        }
        if clear_saved_session {
            self.saved_mega_session = None;
        }
        if success {
            self.authenticated = true;
            self.popup = super::Popup::None;
            if let Some(saved_session) = saved_session {
                self.saved_mega_session = Some(saved_session);
            }
            self.status = "Login successful".to_string();
            if let Err(error) = self.persist_login_credentials_to_config() {
                log::error!("Failed to persist login credentials: {error}");
                self.status = format!("Login successful (config save failed: {error})");
            }
            self.start_download_task();
        } else {
            if clear_saved_session
                && let Err(persist_error) = self.persist_login_credentials_to_config()
            {
                log::error!("Failed to clear saved MEGA session: {persist_error}");
            }
            let status = error.as_deref().map_or_else(
                || "Login failed".to_string(),
                |error| format!("Login failed: {error}"),
            );
            self.login.error = error;
            self.status = status;
            self.popup = super::Popup::Login;
        }
    }

    pub(crate) fn start_download_task(&mut self) {
        let tx = self.event_tx.clone();
        let config = self.config.config.clone();

        let url_rx = self
            .url_rx
            .take()
            .expect("start_download_task called twice");
        let pause_rx = self
            .pause_rx
            .take()
            .expect("start_download_task called twice");
        let token_tx = self
            .token_tx
            .take()
            .expect("start_download_task called twice");

        self.ensure_download_session(&config);
        self.download_task_running = true;

        let channels = super::super::event::DownloadChannels {
            client_rx: self.client_rx.take(),
            event_tx: tx,
            url_rx,
            token_tx,
            pause_rx,
        };

        tokio::spawn(async move {
            super::super::download::run_download(channels, config).await;
        });
    }

    fn ensure_download_session(&mut self, config: &DownloadConfig) {
        let credentials = match self.saved_login_credentials() {
            Ok(credentials) => credentials,
            Err(error) => {
                log::error!("Cannot persist session credentials: {error}");
                return;
            }
        };
        if self.session.is_some() {
            let _ = self.mutate_session_and_save(|session| {
                session.credentials = credentials;
            });
            return;
        }
        if self.core_state.url_order.is_empty()
            && self.files.is_empty()
            && self.overlay_files.is_empty()
            && self.core_state.files.is_empty()
        {
            return;
        }

        let mut session = SessionSnapshot::new(config.clone(), credentials);
        session.urls = self
            .core_state
            .url_order
            .iter()
            .map(|url| SessionUrlSnapshot {
                url: url.clone(),
                error: self.core_state.url_errors.get(url).cloned(),
            })
            .collect();
        self.save_session(session);
    }

    pub(crate) fn set_collection_status(
        &mut self,
        total: usize,
        skipped: usize,
        partial: usize,
        total_bytes: u64,
    ) {
        self.status = format!(
            "Found {total} files ({skipped} skipped, {partial} partial, {})",
            format_bytes(total_bytes)
        );
    }

    pub(crate) fn set_resume_reuse_status(&mut self, id: &FileId, chunks: usize, bytes: u64) {
        self.status = format!(
            "Reusing {chunks} verified chunk(s) for {id} ({})",
            format_bytes(bytes)
        );
    }

    pub(crate) fn queue_url_placeholder(&mut self, url: String) {
        if let Some(row) = self.overlay_files.get_mut(url.as_str()) {
            if row.source_url().is_some() {
                let file = row.file_mut();
                file.name.clone_from(&url);
                file.status = FileStatus::Queued;
                file.downloaded = 0;
                file.size = 0;
            }
        } else {
            self.upsert_overlay_file(
                FileEntry {
                    id: url.clone().into(),
                    name: url.clone(),
                    size: 0,
                    downloaded: 0,
                    status: FileStatus::Queued,
                },
                Some(url),
            );
        }
        self.sync_visible_files();
        self.recompute_totals();
    }

    pub(crate) fn set_status_message(&mut self, message: String) {
        self.status = message;
    }

    pub(crate) fn handle_download_event(&mut self, event: DownloadEvent) {
        match event {
            DownloadEvent::LoginResult {
                success,
                error,
                saved_session,
                clear_saved_session,
            } => {
                if success {
                    log::info!("Login successful");
                } else {
                    log::error!("Login failed: {}", error.as_deref().unwrap_or("unknown"));
                }
                self.complete_login(success, error, saved_session, clear_saved_session);
            }
            DownloadEvent::FilesCollected {
                total,
                skipped,
                partial,
                total_bytes,
            } => {
                log::info!(
                    "Files collected: {total} total, {skipped} skipped, {partial} partial, {}",
                    format_bytes(total_bytes)
                );
                self.set_collection_status(total, skipped, partial, total_bytes);
            }
            DownloadEvent::FileStart {
                id,
                size,
                attempt_id,
            } => {
                self.handle_file_start_event(id, size, attempt_id);
            }
            DownloadEvent::ResumeValidationStarted { id, attempt_id } => {
                self.handle_resume_validation_started_event(id, attempt_id);
            }
            DownloadEvent::Progress {
                id,
                delta,
                attempt_id,
            } => {
                self.handle_file_progress_event(id, delta, attempt_id);
            }
            DownloadEvent::VerificationProgress { id, bytes_delta } => {
                self.handle_verification_progress_event(id, bytes_delta);
            }
            DownloadEvent::VerificationProgressForOperation {
                id,
                operation_id,
                bytes_delta,
            } => {
                self.handle_verification_progress_for_operation(id, operation_id, bytes_delta);
            }
            DownloadEvent::ResumeReused {
                id,
                chunks,
                bytes,
                attempt_id,
            } => {
                self.handle_resume_reused_event(id, chunks, bytes, attempt_id);
            }
            DownloadEvent::ResumeReverified { id, chunks, bytes } => {
                self.handle_resume_reverified_event(id, chunks, bytes);
            }
            DownloadEvent::ResumeReverifiedForOperation {
                id,
                operation_id,
                chunks,
                bytes,
            } => {
                self.handle_resume_reverified_for_operation(id, operation_id, chunks, bytes);
            }
            DownloadEvent::CompletedFileVerified { id, bytes } => {
                self.handle_completed_file_verified_event(id, bytes);
            }
            DownloadEvent::CompletedFileVerifiedForOperation {
                id,
                operation_id,
                bytes,
            } => {
                self.handle_completed_file_verified_for_operation(id, operation_id, bytes);
            }
            DownloadEvent::VerificationSkipped { id, completed } => {
                self.handle_verification_skipped_event(id, completed);
            }
            DownloadEvent::VerificationFailed {
                id,
                operation_id,
                error,
            } => {
                self.handle_verification_failed_event(id, operation_id, error);
            }
            DownloadEvent::FileComplete { id, attempt_id } => {
                self.handle_file_complete_event(id, attempt_id);
            }
            DownloadEvent::FileCancelled { id, attempt_id } => {
                self.handle_file_cancelled_event(id, attempt_id);
            }
            DownloadEvent::FileError {
                id,
                error,
                attempt_id,
            } => {
                self.handle_file_error_event(id, error, attempt_id);
            }
            DownloadEvent::ScopeError { scope, error } => {
                self.handle_scope_error_event(scope, error);
            }
            DownloadEvent::UrlQueued { url } => {
                if self.deleted_url_tombstones.contains(&url) {
                    log::info!("Ignoring queued URL for deleted submission: {url}");
                } else {
                    self.queue_url_placeholder(url);
                }
            }
            DownloadEvent::FileQueued(file) => {
                self.handle_file_queued_event(file);
            }
            DownloadEvent::UrlResolved { url } => {
                self.handle_url_resolved_event(url);
            }
            DownloadEvent::StatusMessage(message) => {
                log::info!("Status: {message}");
                self.set_status_message(message);
            }
            DownloadEvent::UrlsReceived { urls } => {
                self.handle_ui_action(UiAction::AddUrls(urls));
            }
            DownloadEvent::ProgressWakeup => {
                // The wakeup shares the channel with lifecycle events. Drain
                // coalesced deltas only after the receiver has processed all
                // events that were queued behind this wakeup.
            }
        }
    }

    fn handle_event_delivery_failure(&mut self) -> bool {
        let Some(failure) = self.event_tx.take_delivery_failure() else {
            return false;
        };
        let message = format!("Download event delivery failed: {failure}");
        log::error!("{message}");
        self.status = message;
        true
    }

    pub(crate) fn drain_download_events(
        &mut self,
        download_rx: &mut mpsc::Receiver<DownloadEvent>,
    ) -> bool {
        self.event_tx.flush_lifecycle_events();
        let dashboard_dirty = self.with_deferred_batch_updates(|app| {
            let mut handled = false;
            let mut pending_progress: Vec<(
                FileId,
                crate::core::ProgressDelta,
                super::super::event::DownloadAttemptId,
            )> = Vec::new();
            for _ in 0..MAX_DOWNLOAD_EVENTS_PER_TICK {
                let Ok(event) = download_rx.try_recv() else {
                    break;
                };
                match event {
                    DownloadEvent::Progress {
                        id,
                        delta,
                        attempt_id,
                    } => {
                        if let Some((_, pending_delta, _)) = pending_progress.iter_mut().find(
                            |(pending_id, _, pending_attempt_id)| {
                                pending_id == &id && *pending_attempt_id == attempt_id
                            },
                        ) {
                            pending_delta.total_bytes_delta = pending_delta
                                .total_bytes_delta
                                .saturating_add(delta.total_bytes_delta);
                            pending_delta.network_bytes_delta = pending_delta
                                .network_bytes_delta
                                .saturating_add(delta.network_bytes_delta);
                        } else {
                            pending_progress.push((id, delta, attempt_id));
                        }
                        handled = true;
                    }
                    other => {
                        let _ = app.flush_pending_progress_events(&mut pending_progress);
                        app.handle_download_event(other);
                        handled = true;
                    }
                }
            }
            handled |= app.flush_pending_progress_events(&mut pending_progress);
            app.event_tx.flush_lifecycle_events();
            for (id, delta, attempt_id) in app.event_tx.take_pending_progress(download_rx) {
                app.handle_file_progress_event(id, delta, attempt_id);
                handled = true;
            }
            handled |= app.handle_event_delivery_failure();
            handled
        });
        self.retry_pending_requests();
        dashboard_dirty
    }

    pub(crate) fn drain_token_messages(&mut self) {
        for _ in 0..MAX_TOKEN_MESSAGES_PER_TICK {
            let Ok(msg) = self.token_rx.try_recv() else {
                break;
            };
            self.handle_token_message(msg);
        }
    }

    fn handle_token_message(&mut self, msg: super::TokenMessage) {
        let file_id = msg.file_id;
        let token = msg.token;
        if !self.accepts_current_attempt_update(&file_id, msg.attempt_id) {
            token.cancel();
            return;
        }
        if self.paused {
            token.cancel();
            if !self.shutdown_pending_files.contains(&file_id) {
                return;
            }
        }
        self.cancellation_tokens.insert(file_id, token);
    }

    pub(crate) fn log_progress_summary(&mut self) {
        self.update_speeds();
        let Some(pct) = progress_summary_pct(
            self.files_total,
            self.total_downloaded,
            self.total_size,
            self.total_network_downloaded,
        ) else {
            return;
        };
        if pct > 0 && pct < 100 {
            log::info!(
                "[progress] {}/{} files, {} / {} ({}%), {}/s",
                self.files_completed,
                self.files_total,
                format_bytes(self.total_downloaded),
                format_bytes(self.total_size),
                pct,
                format_bytes(self.current_speed),
            );
        }
    }

    pub(crate) fn refresh_resource_usage(&mut self, sys: &mut System, pid: Option<sysinfo::Pid>) {
        if let Some(pid) = pid {
            sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), false);
            if let Some(proc) = sys.process(pid) {
                self.cpu_usage = proc.cpu_usage();
                self.memory_rss = proc.memory();
            }
        }
    }

    pub(crate) fn publish_snapshot_if_observed(
        &mut self,
        state_tx: &watch::Sender<bytes::Bytes>,
    ) -> bool {
        self.publish_dashboard_snapshot_if_observed(state_tx, DashboardUiMode::Tui, false)
    }

    pub(crate) fn publish_dashboard_snapshot_if_observed(
        &mut self,
        state_tx: &watch::Sender<bytes::Bytes>,
        ui_mode: DashboardUiMode,
        read_only: bool,
    ) -> bool {
        let observed = match ui_mode {
            DashboardUiMode::Tui => state_tx.receiver_count() > 1,
            DashboardUiMode::Headless | DashboardUiMode::Attached => state_tx.receiver_count() > 0,
        };
        if observed {
            state_tx.send_replace(self.cached_dashboard_binary(ui_mode, read_only));
            return true;
        }
        false
    }

    pub(crate) fn has_active_dashboard_transfer(&self) -> bool {
        self.files.iter().any(|file| file.status.is_downloading())
            || !self.verification_targets.is_empty()
    }

    pub(crate) fn handle_terminal_tick(
        &mut self,
        download_rx: &mut mpsc::Receiver<DownloadEvent>,
        action_rx: &mut mpsc::Receiver<UiAction>,
        tick_count: u32,
        sys: &mut System,
        pid: Option<sysinfo::Pid>,
        publish_active_transfer_ticks: bool,
    ) -> bool {
        let mut dashboard_dirty = false;

        if tick_count.is_multiple_of(50) {
            self.refresh_resource_usage(sys, pid);
            dashboard_dirty = true;
        }

        dashboard_dirty |= self.drain_download_events(download_rx);
        dashboard_dirty |= publish_active_transfer_ticks && self.has_active_dashboard_transfer();
        self.update_speeds();
        if tick_count.is_multiple_of(50) {
            self.log_progress_summary();
        }
        self.drain_token_messages();
        dashboard_dirty |= self.drain_ui_actions(action_rx);
        self.retry_pending_requests();
        self.poll_session_persistence();
        dashboard_dirty |= self.poll_deferred_auto_login();

        dashboard_dirty
    }

    pub(crate) async fn run_headless_until_shutdown<F>(
        &mut self,
        download_rx: &mut mpsc::Receiver<DownloadEvent>,
        action_rx: &mut mpsc::Receiver<UiAction>,
        state_tx: Option<&watch::Sender<bytes::Bytes>>,
        shutdown: F,
    ) where
        F: Future<Output = ()>,
    {
        self.run_headless_until_shutdown_with_timeout(
            download_rx,
            action_rx,
            state_tx,
            shutdown,
            HEADLESS_SHUTDOWN_TIMEOUT,
        )
        .await;
    }

    async fn run_headless_until_shutdown_with_timeout<F>(
        &mut self,
        download_rx: &mut mpsc::Receiver<DownloadEvent>,
        action_rx: &mut mpsc::Receiver<UiAction>,
        state_tx: Option<&watch::Sender<bytes::Bytes>>,
        shutdown: F,
        shutdown_timeout: Duration,
    ) where
        F: Future<Output = ()>,
    {
        let mut progress_interval = tokio::time::interval(Duration::from_secs(30));
        let mut publish_interval = tokio::time::interval(Duration::from_millis(250));
        progress_interval.tick().await;
        publish_interval.tick().await;

        tokio::pin!(shutdown);
        let mut shutdown_deadline = Box::pin(tokio::time::sleep(Duration::from_hours(24)));
        let mut shutting_down = false;
        let mut shutdown_deadline_armed = false;

        loop {
            tokio::select! {
                () = &mut shutdown, if !shutting_down => {
                    shutting_down = true;
                    log::info!("Shutdown requested; cancelling active downloads");
                    if !self.begin_shutdown() {
                        break;
                    }
                    shutdown_deadline
                        .as_mut()
                        .reset(tokio::time::Instant::now() + shutdown_timeout);
                    shutdown_deadline_armed = true;
                },
                () = &mut shutdown_deadline, if shutdown_deadline_armed => {
                    log::error!(
                        "Shutdown deadline elapsed with {} file(s) still pending; forcing shutdown",
                        self.shutdown_pending_files.len(),
                    );
                    self.drain_token_messages();
                    for token in self.cancellation_tokens.values() {
                        token.cancel();
                    }
                    self.cancellation_tokens.clear();
                    self.shutdown_pending_files.clear();
                    break;
                },
                Some(message) = self.token_rx.recv(), if shutting_down => {
                    self.handle_token_message(message);
                    self.drain_token_messages();
                },
                event = download_rx.recv() => {
                    if let Some(evt) = event {
                        self.handle_download_event(evt);
                        if let Some(state_tx) = state_tx {
                            self.mark_dashboard_dirty();
                            let _ = self.publish_dashboard_snapshot_if_observed(
                                state_tx,
                                DashboardUiMode::Headless,
                                false,
                            );
                        }
                    } else {
                        log::warn!("Event channel closed");
                        break;
                    }
                }
                _ = progress_interval.tick() => {
                    self.log_progress_summary();
                }
                _ = publish_interval.tick(), if state_tx.is_some() => {
                    if let Some(state_tx) = state_tx {
                        let _ = self.publish_dashboard_snapshot_if_observed(
                            state_tx,
                            DashboardUiMode::Headless,
                            false,
                        );
                    }
                }
            }

            let dashboard_dirty =
                self.drain_download_events(download_rx) | self.drain_ui_actions(action_rx);
            self.drain_token_messages();
            self.poll_session_persistence();
            if shutting_down && self.shutdown_pending_files.is_empty() {
                break;
            }
            if dashboard_dirty && let Some(state_tx) = state_tx {
                self.mark_dashboard_dirty();
                let _ = self.publish_dashboard_snapshot_if_observed(
                    state_tx,
                    DashboardUiMode::Headless,
                    false,
                );
            }
        }
        self.flush_session_persistence();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::dashboard::DownloadDashboardState;
    use crate::{
        core::{CoreEvent, PackageKey, ResolvedFile, ResolvedPackage},
        test_support::{StateDirectoryGuard, package_id},
        tui::event::TokenMessage,
    };
    use tempfile::tempdir;
    use tokio::sync::oneshot;

    fn shared_snapshot(shared_state: &crate::tui::app::SharedAppState) -> DownloadDashboardState {
        crate::tui::dashboard::dashboard_state_from_postcard(
            shared_state.state_rx.borrow().as_ref(),
        )
        .expect("shared state should contain valid postcard")
    }

    async fn assert_task_stays_pending<T>(handle: &tokio::task::JoinHandle<T>) {
        for _ in 0..8 {
            tokio::task::yield_now().await;
            assert!(
                !handle.is_finished(),
                "task finished before the gated event was released"
            );
        }
    }

    #[test]
    fn ensure_download_session_refreshes_existing_session_credentials_without_mfa() {
        let dir = tempdir().expect("temp dir should exist");
        let _guard = StateDirectoryGuard::set(dir.path());
        let config_path = dir.path().join("config.toml");
        let config =
            crate::ServiceConfig::load_or_create(&config_path).expect("config should exist");
        let key = crate::config::CredentialKey::decode(config.credential_key.as_deref().unwrap())
            .expect("config key should decode");
        let (event_tx, _event_rx) = DownloadEventSender::channel();
        let mut app = App::new(9723, event_tx, true);
        app.persist_config_path = Some(config_path);
        let mut session = SessionSnapshot::new(
            DownloadConfig::default(),
            SavedCredentials::encrypt_with_key(
                "stale@example.com",
                "stale-pass",
                Some("654321"),
                &key,
            ),
        );
        session.urls.push(SessionUrlSnapshot {
            url: "https://mega.nz/folder/root".to_string(),
            error: None,
        });
        app.session = Some(session);
        assert!(app.login.set_credentials(
            "fresh@example.com".to_string(),
            "fresh-pass".to_string(),
            "123456".to_string()
        ));

        app.ensure_download_session(&DownloadConfig::default());

        let (email, password, mfa) = app
            .session
            .as_ref()
            .expect("session should remain installed")
            .credentials
            .decrypt_with_key(&app.persisted_credential_key().unwrap())
            .expect("saved credentials should decrypt");
        assert_eq!(email, "fresh@example.com");
        assert_eq!(password, "fresh-pass");
        assert!(mfa.is_none());
    }

    #[test]
    fn progress_summary_pct_waits_for_network_bytes() {
        assert_eq!(progress_summary_pct(2, 6_360, 54_790, 0), None);
        assert_eq!(progress_summary_pct(2, 6_360, 54_790, 128), Some(11));
    }

    #[test]
    fn begin_shutdown_cancels_active_tokens() {
        let (event_tx, _event_rx) = DownloadEventSender::channel();
        let mut app = App::new(9723, event_tx, true);
        let token = tokio_util::sync::CancellationToken::new();
        app.cancellation_tokens
            .insert("episode.bin".to_string().into(), token.clone());

        let waiting = app.begin_shutdown();

        assert!(waiting);
        assert!(app.paused);
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn headless_shutdown_waits_for_file_cancellation_events() {
        let dir = tempdir().expect("temp dir should exist");
        let _guard = StateDirectoryGuard::set(dir.path());
        let (event_tx, download_rx) = DownloadEventSender::channel();
        let (_action_tx, action_rx) = mpsc::channel(64);
        let mut app = App::new(9723, event_tx.clone(), true);
        let file_id = crate::core::FileId::from("episode.bin");
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id("pkg", "https://mega.nz/folder/root"),
                source_url: "https://mega.nz/folder/root".to_string(),
                key: PackageKey::new("https://mega.nz/folder/root".to_string()),
                display_name: "Package".to_string(),
                files: vec![ResolvedFile {
                    file_id: file_id.clone(),
                    path: "episode.bin".to_string(),
                    size: 128,
                }],
                collision: None,
            },
        });
        app.apply_core_event(CoreEvent::FileStarted {
            file_id: file_id.clone(),
            size: 128,
        });
        let token = tokio_util::sync::CancellationToken::new();
        app.cancellation_tokens
            .insert(file_id.clone(), token.clone());
        let cancelled_id = file_id.clone();
        let (release_cancel_tx, release_cancel_rx) = oneshot::channel();
        tokio::spawn(async move {
            let _ = release_cancel_rx.await;
            let _ = event_tx.send(DownloadEvent::FileCancelled {
                id: cancelled_id,
                attempt_id: crate::tui::event::DownloadAttemptId::new(0),
            });
        });

        let handle = tokio::spawn(async move {
            let mut download_rx = download_rx;
            let mut action_rx = action_rx;
            app.run_headless_until_shutdown(
                &mut download_rx,
                &mut action_rx,
                None,
                std::future::ready(()),
            )
            .await;
            app
        });

        while !token.is_cancelled() {
            tokio::task::yield_now().await;
        }
        assert_task_stays_pending(&handle).await;
        release_cancel_tx.send(()).unwrap();

        let app = tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("headless shutdown should complete after cancellation drains")
            .expect("headless task should join");

        assert!(token.is_cancelled());
        assert!(app.cancellation_tokens.is_empty());
        assert!(app.shutdown_pending_files.is_empty());
        assert!(app.paused);
    }

    #[tokio::test]
    async fn headless_shutdown_forces_completion_after_workers_miss_deadline() {
        let (event_tx, mut download_rx) = DownloadEventSender::channel();
        let (_action_tx, mut action_rx) = mpsc::channel(64);
        let mut app = App::new(9723, event_tx, true);
        let token = tokio_util::sync::CancellationToken::new();
        app.cancellation_tokens
            .insert("episode.bin".into(), token.clone());

        tokio::time::timeout(
            Duration::from_secs(1),
            app.run_headless_until_shutdown_with_timeout(
                &mut download_rx,
                &mut action_rx,
                None,
                std::future::ready(()),
                Duration::from_millis(10),
            ),
        )
        .await
        .expect("shutdown should be bounded");

        assert!(token.is_cancelled());
        assert!(app.shutdown_pending_files.is_empty());
    }

    #[tokio::test]
    async fn headless_shutdown_waits_for_late_download_token_registration() {
        let dir = tempdir().expect("temp dir should exist");
        let _guard = StateDirectoryGuard::set(dir.path());
        let (event_tx, download_rx) = DownloadEventSender::channel();
        let (_action_tx, action_rx) = mpsc::channel(64);
        let mut app = App::new(9723, event_tx.clone(), true);
        let token_tx = app
            .token_tx
            .as_ref()
            .expect("token tx should exist")
            .clone();
        let file_id = crate::core::FileId::from("episode.bin");

        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id("pkg", "https://mega.nz/folder/root"),
                source_url: "https://mega.nz/folder/root".to_string(),
                key: PackageKey::new("https://mega.nz/folder/root".to_string()),
                display_name: "Package".to_string(),
                files: vec![ResolvedFile {
                    file_id: file_id.clone(),
                    path: "episode.bin".to_string(),
                    size: 128,
                }],
                collision: None,
            },
        });
        app.apply_core_event(CoreEvent::FileStarted {
            file_id: file_id.clone(),
            size: 128,
        });

        let token = tokio_util::sync::CancellationToken::new();
        let sent_token = token.clone();
        let sent_id = file_id.clone();
        let send_tx = event_tx.clone();
        let (release_token_tx, release_token_rx) = oneshot::channel();
        let (release_cancel_tx, release_cancel_rx) = oneshot::channel();
        tokio::spawn(async move {
            let _ = release_token_rx.await;
            let _ = token_tx
                .send(TokenMessage {
                    file_id: sent_id.clone(),
                    attempt_id: crate::tui::event::DownloadAttemptId::new(0),
                    token: sent_token,
                })
                .await;
            let _ = release_cancel_rx.await;
            let _ = send_tx.send(DownloadEvent::FileCancelled {
                id: sent_id,
                attempt_id: crate::tui::event::DownloadAttemptId::new(0),
            });
        });

        let handle = tokio::spawn(async move {
            let mut download_rx = download_rx;
            let mut action_rx = action_rx;
            app.run_headless_until_shutdown(
                &mut download_rx,
                &mut action_rx,
                None,
                std::future::ready(()),
            )
            .await;
            app
        });

        assert_task_stays_pending(&handle).await;
        release_token_tx.send(()).unwrap();
        while !token.is_cancelled() {
            tokio::task::yield_now().await;
        }
        assert_task_stays_pending(&handle).await;
        release_cancel_tx.send(()).unwrap();

        let app = tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("headless shutdown should finish after late cancellation")
            .expect("headless task should join");

        assert!(token.is_cancelled());
        assert!(app.shutdown_pending_files.is_empty());
        assert!(app.cancellation_tokens.is_empty());
        assert!(app.paused);
    }

    #[tokio::test]
    async fn delayed_token_and_result_from_deleted_attempt_cannot_affect_readded_file() {
        let dir = tempdir().expect("temp dir should exist");
        let _guard = StateDirectoryGuard::set(dir.path());
        let (event_tx, _event_rx) = DownloadEventSender::channel();
        let mut app = App::new(9723, event_tx, true);
        let file_id = FileId::from("episode.bin");
        let source_url = "https://mega.nz/folder/root";
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id("pkg", source_url),
                source_url: source_url.to_string(),
                key: PackageKey::new(source_url),
                display_name: "Package".to_string(),
                files: vec![ResolvedFile {
                    file_id: file_id.clone(),
                    path: file_id.to_string(),
                    size: 128,
                }],
                collision: None,
            },
        });
        app.handle_file_start_event(
            file_id.clone(),
            128,
            crate::tui::event::DownloadAttemptId::new(0),
        );
        let old_token = tokio_util::sync::CancellationToken::new();

        app.perform_delete_file_action(&file_id);
        app.ensure_core_file(
            &file_id,
            source_url,
            file_id.as_str(),
            128,
            crate::core::FileAccounting::CurrentRun,
        );
        app.handle_file_start_event(
            file_id.clone(),
            128,
            crate::tui::event::DownloadAttemptId::new(1),
        );

        let current_token = tokio_util::sync::CancellationToken::new();
        app.handle_token_message(TokenMessage {
            file_id: file_id.clone(),
            attempt_id: crate::tui::event::DownloadAttemptId::new(1),
            token: current_token.clone(),
        });
        app.handle_token_message(TokenMessage {
            file_id: file_id.clone(),
            attempt_id: crate::tui::event::DownloadAttemptId::new(0),
            token: old_token.clone(),
        });

        assert!(old_token.is_cancelled());
        assert!(!current_token.is_cancelled());
        assert!(app.cancellation_tokens.contains_key(&file_id));

        app.handle_file_complete_event(
            file_id.clone(),
            crate::tui::event::DownloadAttemptId::new(0),
        );
        let file = app
            .core_state
            .files
            .get(&file_id)
            .expect("file should remain tracked");
        assert_eq!(file.lifecycle, crate::core::FileLifecycle::Downloading);
    }

    #[tokio::test]
    async fn headless_shutdown_waits_for_late_resume_validation_token_registration() {
        let dir = tempdir().expect("temp dir should exist");
        let _guard = StateDirectoryGuard::set(dir.path());
        let (event_tx, download_rx) = DownloadEventSender::channel();
        let (_action_tx, action_rx) = mpsc::channel(64);
        let mut app = App::new(9723, event_tx.clone(), true);
        let token_tx = app
            .token_tx
            .as_ref()
            .expect("token tx should exist")
            .clone();
        let file_id = crate::core::FileId::from("episode.bin");

        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id("pkg", "https://mega.nz/folder/root"),
                source_url: "https://mega.nz/folder/root".to_string(),
                key: PackageKey::new("https://mega.nz/folder/root".to_string()),
                display_name: "Package".to_string(),
                files: vec![ResolvedFile {
                    file_id: file_id.clone(),
                    path: "episode.bin".to_string(),
                    size: 128,
                }],
                collision: None,
            },
        });
        app.handle_resume_validation_started_event(
            file_id.clone(),
            crate::tui::event::DownloadAttemptId::new(0),
        );

        let token = tokio_util::sync::CancellationToken::new();
        let sent_token = token.clone();
        let sent_id = file_id.clone();
        let send_tx = event_tx.clone();
        let (release_token_tx, release_token_rx) = oneshot::channel();
        let (release_cancel_tx, release_cancel_rx) = oneshot::channel();
        tokio::spawn(async move {
            let _ = release_token_rx.await;
            let _ = token_tx
                .send(TokenMessage {
                    file_id: sent_id.clone(),
                    attempt_id: crate::tui::event::DownloadAttemptId::new(0),
                    token: sent_token,
                })
                .await;
            let _ = release_cancel_rx.await;
            let _ = send_tx.send(DownloadEvent::FileCancelled {
                id: sent_id,
                attempt_id: crate::tui::event::DownloadAttemptId::new(0),
            });
        });

        let handle = tokio::spawn(async move {
            let mut download_rx = download_rx;
            let mut action_rx = action_rx;
            app.run_headless_until_shutdown(
                &mut download_rx,
                &mut action_rx,
                None,
                std::future::ready(()),
            )
            .await;
            app
        });

        assert_task_stays_pending(&handle).await;
        release_token_tx.send(()).unwrap();
        while !token.is_cancelled() {
            tokio::task::yield_now().await;
        }
        assert_task_stays_pending(&handle).await;
        release_cancel_tx.send(()).unwrap();

        let app = tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("headless shutdown should finish after late verification cancellation")
            .expect("headless task should join");

        assert!(token.is_cancelled());
        assert!(app.shutdown_pending_files.is_empty());
        assert!(app.cancellation_tokens.is_empty());
        assert!(app.paused);
    }

    #[test]
    fn ensure_download_session_does_not_create_empty_session() {
        let dir = tempdir().expect("temp dir should exist");
        let _guard = StateDirectoryGuard::set(dir.path());
        let (event_tx, _event_rx) = DownloadEventSender::channel();
        let mut app = App::new(9723, event_tx, true);
        assert!(app.login.set_credentials(
            "fresh@example.com".to_string(),
            "fresh-pass".to_string(),
            String::new()
        ));

        app.ensure_download_session(&DownloadConfig::default());

        assert!(app.session.is_none());
        assert!(SessionSnapshot::latest().is_none());
    }

    #[test]
    fn publish_dashboard_snapshot_updates_single_shared_receiver() {
        let (event_tx, _event_rx) = DownloadEventSender::channel();
        let mut app = App::new(9723, event_tx, true);
        let crate::tui::app::SharedStateChannels {
            state_tx,
            shared_state,
            ..
        } = app.shared_state_channels(true, DashboardUiMode::Headless);
        let shared_state = shared_state.expect("shared state should be enabled");

        app.status = "updated from runtime".to_string();

        assert!(app.publish_dashboard_snapshot_if_observed(
            &state_tx,
            DashboardUiMode::Headless,
            false,
        ));

        let snapshot = shared_snapshot(&shared_state);
        assert_eq!(snapshot.status, "updated from runtime");
    }

    #[test]
    fn tui_dashboard_snapshot_skips_single_internal_receiver() {
        let (event_tx, _event_rx) = DownloadEventSender::channel();
        let mut app = App::new(9723, event_tx, true);
        let crate::tui::app::SharedStateChannels {
            state_tx,
            shared_state,
            ..
        } = app.shared_state_channels(true, DashboardUiMode::Tui);
        let shared_state = shared_state.expect("shared state should be enabled");
        let initial = shared_state.state_rx.borrow().clone();

        app.status = "should not publish".to_string();

        assert!(!app.publish_dashboard_snapshot_if_observed(
            &state_tx,
            DashboardUiMode::Tui,
            false,
        ));
        assert_eq!(shared_state.state_rx.borrow().as_ref(), initial.as_ref());

        let _remote_rx = shared_state.state_rx.clone();

        assert!(
            app.publish_dashboard_snapshot_if_observed(&state_tx, DashboardUiMode::Tui, false,)
        );
        assert_eq!(shared_snapshot(&shared_state).status, "should not publish");
    }

    #[test]
    fn dashboard_snapshot_reuses_cache_until_revision_changes() {
        let (event_tx, _event_rx) = DownloadEventSender::channel();
        let mut app = App::new(9723, event_tx, true);
        let crate::tui::app::SharedStateChannels {
            state_tx,
            shared_state,
            ..
        } = app.shared_state_channels(true, DashboardUiMode::Headless);
        let shared_state = shared_state.expect("shared state should be enabled");

        app.status = "first".to_string();
        app.mark_dashboard_dirty();
        assert!(app.publish_dashboard_snapshot_if_observed(
            &state_tx,
            DashboardUiMode::Headless,
            false,
        ));
        assert_eq!(shared_snapshot(&shared_state).status, "first");

        app.status = "second".to_string();
        assert!(app.publish_dashboard_snapshot_if_observed(
            &state_tx,
            DashboardUiMode::Headless,
            false,
        ));
        assert_eq!(shared_snapshot(&shared_state).status, "first");

        app.mark_dashboard_dirty();
        assert!(app.publish_dashboard_snapshot_if_observed(
            &state_tx,
            DashboardUiMode::Headless,
            false,
        ));
        assert_eq!(shared_snapshot(&shared_state).status, "second");
    }

    #[test]
    fn dashboard_snapshot_reuses_binary_cache_until_revision_changes() {
        let (event_tx, _event_rx) = DownloadEventSender::channel();
        let mut app = App::new(9723, event_tx, true);
        let crate::tui::app::SharedStateChannels {
            state_tx,
            shared_state,
            ..
        } = app.shared_state_channels(true, DashboardUiMode::Headless);
        let shared_state = shared_state.expect("shared state should be enabled");

        app.status = "first".to_string();
        app.mark_dashboard_dirty();
        assert!(app.publish_dashboard_snapshot_if_observed(
            &state_tx,
            DashboardUiMode::Headless,
            false,
        ));
        let first = shared_state.state_rx.borrow().clone();
        let first_ptr = first.as_ptr();

        app.status = "second".to_string();
        assert!(app.publish_dashboard_snapshot_if_observed(
            &state_tx,
            DashboardUiMode::Headless,
            false,
        ));
        assert_eq!(shared_state.state_rx.borrow().as_ptr(), first_ptr);
        assert_eq!(shared_snapshot(&shared_state).status, "first");

        app.mark_dashboard_dirty();
        assert!(app.publish_dashboard_snapshot_if_observed(
            &state_tx,
            DashboardUiMode::Headless,
            false,
        ));
        assert_eq!(shared_snapshot(&shared_state).status, "second");
    }

    #[test]
    fn terminal_tick_bounds_download_event_drain_to_keep_input_responsive() {
        let (event_tx, _event_rx) = DownloadEventSender::channel();
        let mut app = App::new(9723, event_tx, true);
        let (download_tx, mut download_rx) =
            DownloadEventSender::channel_with_capacity(MAX_DOWNLOAD_EVENTS_PER_TICK + 10);
        let (_action_tx, mut action_rx) = mpsc::channel(64);
        let mut sys = System::new();

        for index in 0..MAX_DOWNLOAD_EVENTS_PER_TICK + 10 {
            download_tx
                .send(DownloadEvent::StatusMessage(format!("status {index}")))
                .expect("download event should send");
        }

        assert!(app.handle_terminal_tick(
            &mut download_rx,
            &mut action_rx,
            1,
            &mut sys,
            None,
            false,
        ));

        assert_eq!(download_rx.len(), 10);
        assert_eq!(
            app.status,
            format!("status {}", MAX_DOWNLOAD_EVENTS_PER_TICK - 1)
        );
    }

    #[test]
    fn drain_download_events_flushes_retained_lifecycle_events_on_later_ticks() {
        let (event_tx, mut download_rx) = DownloadEventSender::channel_with_capacities(1, 1);
        let mut app = App::new(9723, event_tx.clone(), true);
        let first = DownloadEvent::ScopeError {
            scope: "setup".to_string(),
            error: "one".to_string(),
        };
        let second = DownloadEvent::ScopeError {
            scope: "download".to_string(),
            error: "two".to_string(),
        };

        event_tx
            .send(first)
            .expect("first lifecycle event should enter the channel");
        event_tx
            .send(second)
            .expect("second lifecycle event should enter the sender backlog");

        assert!(app.drain_download_events(&mut download_rx));
        assert!(app.overlay_files.contains_key("setup"));
        assert!(app.drain_download_events(&mut download_rx));
        assert!(app.overlay_files.contains_key("download"));
    }

    #[test]
    fn drain_download_events_preserves_all_lifecycle_events() {
        let (event_tx, mut download_rx) = DownloadEventSender::channel_with_capacities(1, 1);
        let mut app = App::new(9723, event_tx.clone(), true);

        event_tx
            .send(DownloadEvent::ScopeError {
                scope: "setup".to_string(),
                error: "one".to_string(),
            })
            .expect("first lifecycle event should enter the channel");
        event_tx
            .send(DownloadEvent::ScopeError {
                scope: "download".to_string(),
                error: "two".to_string(),
            })
            .expect("second lifecycle event should enter the backlog");
        event_tx
            .send(DownloadEvent::ScopeError {
                scope: "verify".to_string(),
                error: "three".to_string(),
            })
            .expect("third lifecycle event should enter the durable backlog");

        assert!(app.drain_download_events(&mut download_rx));
        assert!(app.overlay_files.contains_key("setup"));
        assert!(app.drain_download_events(&mut download_rx));
        assert!(app.overlay_files.contains_key("download"));
        assert!(app.drain_download_events(&mut download_rx));
        assert!(!event_tx.has_pending_lifecycle_events());
    }

    #[test]
    fn terminal_tick_does_not_dirty_dashboard_when_idle() {
        let (event_tx, _event_rx) = DownloadEventSender::channel();
        let mut app = App::new(9723, event_tx, true);
        let (_download_tx, mut download_rx) = mpsc::channel(64);
        let (_action_tx, mut action_rx) = mpsc::channel(64);
        let mut sys = System::new();

        assert!(!app.handle_terminal_tick(
            &mut download_rx,
            &mut action_rx,
            1,
            &mut sys,
            None,
            false,
        ));
    }

    #[test]
    fn terminal_tick_dirties_dashboard_for_active_transfer_speed_refresh() {
        let (event_tx, _event_rx) = DownloadEventSender::channel();
        let mut app = App::new(9723, event_tx, true);
        let (_download_tx, mut download_rx) = mpsc::channel(64);
        let (_action_tx, mut action_rx) = mpsc::channel(64);
        let mut sys = System::new();

        app.files.push(FileEntry {
            id: "file.bin".into(),
            name: "file.bin".to_string(),
            size: 100,
            downloaded: 1,
            status: FileStatus::Downloading,
        });

        assert!(app.handle_terminal_tick(
            &mut download_rx,
            &mut action_rx,
            1,
            &mut sys,
            None,
            true,
        ));
    }

    #[test]
    fn terminal_tick_skips_active_transfer_dashboard_publish_without_remote_client() {
        let (event_tx, _event_rx) = DownloadEventSender::channel();
        let mut app = App::new(9723, event_tx, true);
        let (_download_tx, mut download_rx) = mpsc::channel(64);
        let (_action_tx, mut action_rx) = mpsc::channel(64);
        let mut sys = System::new();

        app.files.push(FileEntry {
            id: "file.bin".into(),
            name: "file.bin".to_string(),
            size: 100,
            downloaded: 1,
            status: FileStatus::Downloading,
        });

        assert!(!app.handle_terminal_tick(
            &mut download_rx,
            &mut action_rx,
            1,
            &mut sys,
            None,
            false,
        ));
    }

    #[test]
    fn drain_download_events_coalesces_progress_for_same_file_within_tick() {
        let (event_tx, _event_rx) = DownloadEventSender::channel();
        let mut app = App::new(9723, event_tx, true);
        let (download_tx, mut download_rx) = mpsc::channel(64);

        app.apply_core_event(crate::core::CoreEvent::PackageResolved {
            package: crate::core::ResolvedPackage {
                id: crate::test_support::package_id("pkg", "https://mega.nz/file/root"),
                source_url: "https://mega.nz/file/root".to_string(),
                key: crate::core::PackageKey::new("https://mega.nz/file/root".to_string()),
                display_name: "Package".to_string(),
                files: vec![crate::core::ResolvedFile {
                    file_id: "file.bin".to_string().into(),
                    path: "file.bin".to_string(),
                    size: 100,
                }],
                collision: None,
            },
        });
        app.apply_core_event(crate::core::CoreEvent::FileStarted {
            file_id: "file.bin".to_string().into(),
            size: 100,
        });

        for _ in 0..3 {
            download_tx
                .try_send(DownloadEvent::Progress {
                    id: "file.bin".into(),
                    delta: crate::core::ProgressDelta {
                        total_bytes_delta: 10,
                        network_bytes_delta: 10,
                    },
                    attempt_id: crate::tui::event::DownloadAttemptId::new(0),
                })
                .expect("progress event should send");
        }

        assert!(app.drain_download_events(&mut download_rx));

        let file = app
            .core_state
            .files
            .get("file.bin")
            .expect("file should exist");
        assert_eq!(file.progress.visible_completed_bytes, 30);
        assert_eq!(file.progress.downloaded_network_bytes, 30);
    }

    #[test]
    fn drain_download_events_applies_verification_progress_without_attempt_id() {
        let (event_tx, _event_rx) = DownloadEventSender::channel();
        let mut app = App::new(9723, event_tx, true);
        let (download_tx, mut download_rx) = mpsc::channel(64);

        app.apply_core_event(crate::core::CoreEvent::PackageResolved {
            package: crate::core::ResolvedPackage {
                id: crate::test_support::package_id("pkg", "https://mega.nz/file/root"),
                source_url: "https://mega.nz/file/root".to_string(),
                key: crate::core::PackageKey::new("https://mega.nz/file/root".to_string()),
                display_name: "Package".to_string(),
                files: vec![crate::core::ResolvedFile {
                    file_id: "file.bin".to_string().into(),
                    path: "file.bin".to_string(),
                    size: 100,
                }],
                collision: None,
            },
        });
        app.verifying_files.insert("file.bin".to_string().into());
        app.verification_inflight_files
            .insert("file.bin".to_string().into());
        app.verification_targets.insert(
            "file.bin".to_string().into(),
            crate::tui::app::VerificationTarget::Resume,
        );
        app.apply_core_event(crate::core::CoreEvent::FileVerificationStarted {
            file_id: "file.bin".to_string().into(),
        });

        download_tx
            .try_send(DownloadEvent::VerificationProgress {
                id: "file.bin".into(),
                bytes_delta: 25,
            })
            .expect("verification progress event should send");

        assert!(app.drain_download_events(&mut download_rx));

        let file = app
            .core_state
            .files
            .get("file.bin")
            .expect("file should exist");
        assert_eq!(file.progress.visible_completed_bytes, 25);
        assert_eq!(file.progress.downloaded_network_bytes, 0);
    }

    #[test]
    fn drain_download_events_starts_resume_validation_from_download_event() {
        let (event_tx, _event_rx) = DownloadEventSender::channel();
        let mut app = App::new(9723, event_tx, true);
        let (download_tx, mut download_rx) = mpsc::channel(64);

        app.apply_core_event(crate::core::CoreEvent::PackageResolved {
            package: crate::core::ResolvedPackage {
                id: crate::test_support::package_id("pkg", "https://mega.nz/file/root"),
                source_url: "https://mega.nz/file/root".to_string(),
                key: crate::core::PackageKey::new("https://mega.nz/file/root".to_string()),
                display_name: "Package".to_string(),
                files: vec![crate::core::ResolvedFile {
                    file_id: "file.bin".to_string().into(),
                    path: "file.bin".to_string(),
                    size: 100,
                }],
                collision: None,
            },
        });

        download_tx
            .try_send(DownloadEvent::ResumeValidationStarted {
                id: "file.bin".into(),
                attempt_id: crate::tui::event::DownloadAttemptId::new(0),
            })
            .expect("resume validation start should send");

        assert!(app.drain_download_events(&mut download_rx));

        assert!(app.verifying_files.contains("file.bin"));
        assert!(app.verification_inflight_files.contains("file.bin"));
        assert_eq!(
            app.verification_targets.get("file.bin"),
            Some(&crate::tui::app::VerificationTarget::Resume)
        );
        let file = app
            .core_state
            .files
            .get("file.bin")
            .expect("file should exist");
        assert_eq!(file.progress.visible_completed_bytes, 0);
    }

    #[test]
    fn drain_download_events_applies_verification_skip() {
        let (event_tx, _event_rx) = DownloadEventSender::channel();
        let mut app = App::new(9723, event_tx, true);
        let (download_tx, mut download_rx) = mpsc::channel(64);

        app.apply_core_event(crate::core::CoreEvent::PackageResolved {
            package: crate::core::ResolvedPackage {
                id: crate::test_support::package_id("pkg", "https://mega.nz/file/root"),
                source_url: "https://mega.nz/file/root".to_string(),
                key: crate::core::PackageKey::new("https://mega.nz/file/root".to_string()),
                display_name: "Package".to_string(),
                files: vec![crate::core::ResolvedFile {
                    file_id: "file.bin".to_string().into(),
                    path: "file.bin".to_string(),
                    size: 100,
                }],
                collision: None,
            },
        });
        app.verifying_files.insert("file.bin".to_string().into());
        app.verification_inflight_files
            .insert("file.bin".to_string().into());
        app.verification_targets.insert(
            "file.bin".to_string().into(),
            crate::tui::app::VerificationTarget::Resume,
        );
        app.apply_core_event(crate::core::CoreEvent::FileVerificationStarted {
            file_id: "file.bin".to_string().into(),
        });

        download_tx
            .try_send(DownloadEvent::VerificationSkipped {
                id: "file.bin".into(),
                completed: false,
            })
            .expect("verification skip should send");

        assert!(app.drain_download_events(&mut download_rx));
        assert!(!app.verifying_files.contains("file.bin"));
        assert!(!app.verification_inflight_files.contains("file.bin"));
    }

    #[test]
    fn progress_wakeup_does_not_move_later_progress_before_file_start() {
        for capacity in [256, 1] {
            let (event_tx, mut download_rx) = DownloadEventSender::channel_with_capacity(capacity);
            let mut app = App::new(9723, event_tx.clone(), true);
            let file_id = FileId::from("file-b.bin");
            app.apply_core_event(CoreEvent::PackageResolved {
                package: ResolvedPackage {
                    id: crate::test_support::package_id("pkg", "https://mega.nz/file/root"),
                    source_url: "https://mega.nz/file/root".to_string(),
                    key: crate::core::PackageKey::new("https://mega.nz/file/root".to_string()),
                    display_name: "Package".to_string(),
                    files: vec![ResolvedFile {
                        file_id: file_id.clone(),
                        path: file_id.to_string(),
                        size: 100,
                    }],
                    collision: None,
                },
            });
            let attempt_id = crate::tui::event::DownloadAttemptId::new(0);

            event_tx
                .send(DownloadEvent::Progress {
                    id: "file-a.bin".into(),
                    delta: crate::core::ProgressDelta {
                        total_bytes_delta: 1,
                        network_bytes_delta: 1,
                    },
                    attempt_id,
                })
                .expect("first transfer should enqueue a progress wakeup");
            event_tx
                .send(DownloadEvent::FileStart {
                    id: file_id.clone(),
                    size: 100,
                    attempt_id,
                })
                .expect("file start should remain ordered after the wakeup");
            event_tx
                .send(DownloadEvent::Progress {
                    id: file_id.clone(),
                    delta: crate::core::ProgressDelta {
                        total_bytes_delta: 10,
                        network_bytes_delta: 10,
                    },
                    attempt_id,
                })
                .expect("second transfer progress should be retained");

            for _ in 0..8 {
                if !app.drain_download_events(&mut download_rx) {
                    break;
                }
            }

            let file = app
                .core_state
                .files
                .get(&file_id)
                .expect("file should remain tracked");
            assert_eq!(file.progress.downloaded_network_bytes, 10);
            assert_eq!(file.progress.visible_completed_bytes, 10);
        }
    }

    #[test]
    fn token_for_completed_file_does_not_create_shutdown_work() {
        let (event_tx, _event_rx) = DownloadEventSender::channel();
        let mut app = App::new(9723, event_tx, true);
        let file_id = FileId::from("already-complete.bin");
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: crate::test_support::package_id("pkg", "https://mega.nz/file/root"),
                source_url: "https://mega.nz/file/root".to_string(),
                key: crate::core::PackageKey::new("https://mega.nz/file/root".to_string()),
                display_name: "Package".to_string(),
                files: vec![ResolvedFile {
                    file_id: file_id.clone(),
                    path: file_id.to_string(),
                    size: 100,
                }],
                collision: None,
            },
        });
        app.handle_file_start_event(
            file_id.clone(),
            100,
            crate::tui::event::DownloadAttemptId::new(0),
        );
        app.handle_file_complete_event(
            file_id.clone(),
            crate::tui::event::DownloadAttemptId::new(0),
        );

        app.handle_token_message(super::super::TokenMessage {
            file_id: file_id.clone(),
            attempt_id: crate::tui::event::DownloadAttemptId::new(0),
            token: tokio_util::sync::CancellationToken::new(),
        });

        assert!(!app.cancellation_tokens.contains_key(&file_id));
        assert!(!app.begin_shutdown());
    }

    fn check_sender_progress_before_terminal(terminal: &str) {
        for capacity in [256, 1] {
            let (event_tx, mut download_rx) = DownloadEventSender::channel_with_capacity(capacity);
            let mut app = App::new(9723, event_tx.clone(), true);
            let file_id = FileId::from("ordered.bin");
            app.apply_core_event(CoreEvent::PackageResolved {
                package: ResolvedPackage {
                    id: crate::test_support::package_id("pkg", "https://mega.nz/file/root"),
                    source_url: "https://mega.nz/file/root".to_string(),
                    key: crate::core::PackageKey::new("https://mega.nz/file/root"),
                    display_name: "Package".to_string(),
                    files: vec![ResolvedFile {
                        file_id: file_id.clone(),
                        path: file_id.to_string(),
                        size: 100,
                    }],
                    collision: None,
                },
            });
            let attempt_id = crate::tui::event::DownloadAttemptId::new(0);
            event_tx
                .send(DownloadEvent::FileStart {
                    id: file_id.clone(),
                    size: 100,
                    attempt_id,
                })
                .unwrap();
            // A full channel still retains this delta in the real sender.
            let _ = event_tx.send(DownloadEvent::Progress {
                id: file_id.clone(),
                delta: crate::core::ProgressDelta {
                    total_bytes_delta: 25,
                    network_bytes_delta: 25,
                },
                attempt_id,
            });
            event_tx
                .send(match terminal {
                    "complete" => DownloadEvent::FileComplete {
                        id: file_id.clone(),
                        attempt_id,
                    },
                    "cancel" => DownloadEvent::FileCancelled {
                        id: file_id.clone(),
                        attempt_id,
                    },
                    "error" => DownloadEvent::FileError {
                        id: file_id.clone(),
                        attempt_id,
                        error: "failed".to_string(),
                    },
                    _ => unreachable!(),
                })
                .unwrap();
            for _ in 0..8 {
                if !app.drain_download_events(&mut download_rx) {
                    break;
                }
            }
            let file = &app.core_state.files[&file_id];
            assert_eq!(
                file.progress.downloaded_network_bytes, 25,
                "{terminal}, channel capacity {capacity}"
            );
            let expected = match terminal {
                "complete" => crate::core::FileLifecycle::Complete,
                "cancel" => crate::core::FileLifecycle::Queued,
                "error" => crate::core::FileLifecycle::Failed {
                    message: "failed".to_string(),
                },
                _ => unreachable!(),
            };
            assert_eq!(file.lifecycle, expected, "channel capacity {capacity}");
            let snapshot = crate::core::snapshot_from_state(&app.core_state);
            assert_eq!(
                snapshot.packages[0].files[0]
                    .progress
                    .downloaded_network_bytes,
                25
            );
        }
    }

    #[test]
    fn sender_progress_precedes_completion() {
        check_sender_progress_before_terminal("complete");
    }

    #[test]
    fn sender_progress_precedes_cancellation() {
        check_sender_progress_before_terminal("cancel");
    }

    #[test]
    fn sender_progress_precedes_failure() {
        check_sender_progress_before_terminal("error");
    }
}
