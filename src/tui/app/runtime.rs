use std::future::Future;
use std::time::Duration;

use sysinfo::{ProcessesToUpdate, System};
use tokio::sync::{mpsc, watch};

use crate::{
    DownloadConfig,
    core::{FileId, SavedCredentials, SessionSnapshot, SessionUrlSnapshot},
    format_bytes,
    tui::dashboard::DashboardUiMode,
};

use super::{App, DownloadEvent, FileEntry, FileStatus, UiAction};

const MAX_DOWNLOAD_EVENTS_PER_TICK: usize = 256;
const MAX_TOKEN_MESSAGES_PER_TICK: usize = 256;

impl App {
    fn flush_pending_progress_events(
        &mut self,
        pending_progress: &mut Vec<(FileId, crate::core::ProgressDelta, u64)>,
    ) -> bool {
        if pending_progress.is_empty() {
            return false;
        }

        for (id, delta, attempt_id) in pending_progress.drain(..) {
            self.handle_file_progress_event(id, delta, attempt_id);
        }

        true
    }

    fn saved_login_credentials(&self) -> SavedCredentials {
        SavedCredentials::encrypt(self.login.email(), self.login.password(), None)
    }

    pub(crate) fn complete_login(&mut self, success: bool, error: Option<String>) {
        self.login.logging_in = false;
        if success {
            self.authenticated = true;
            self.popup = super::Popup::None;
            self.status = "Login successful".to_string();
            if let Err(error) = self.persist_login_credentials_to_config() {
                log::error!("Failed to persist login credentials: {error}");
                self.status = format!("Login successful (config save failed: {error})");
            }
            self.start_download_task();
        } else {
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
        let credentials = self.saved_login_credentials();
        if self.session.is_some() {
            let _ = self.mutate_session_and_save(|session| {
                session.credentials = credentials;
            });
            return;
        }
        if self.urls.is_empty()
            && self.files.is_empty()
            && self.overlay_files.is_empty()
            && self.core_state.files.is_empty()
        {
            return;
        }

        let mut session = SessionSnapshot::new(config.clone(), credentials);
        session.urls = self
            .urls
            .iter()
            .map(|url| SessionUrlSnapshot {
                url: url.clone(),
                error: None,
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
        if !self.overlay_files.contains_key(url.as_str()) {
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
        self.recompute_totals();
    }

    pub(crate) fn set_status_message(&mut self, message: String) {
        self.status = message;
    }

    pub(crate) fn handle_download_event(&mut self, event: DownloadEvent) {
        match event {
            DownloadEvent::LoginResult { success, error } => {
                if success {
                    log::info!("Login successful");
                } else {
                    log::error!("Login failed: {}", error.as_deref().unwrap_or("unknown"));
                }
                self.complete_login(success, error);
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
            DownloadEvent::CompletedFileVerified { id, bytes } => {
                self.handle_completed_file_verified_event(id, bytes);
            }
            DownloadEvent::VerificationSkipped { id, completed } => {
                self.handle_verification_skipped_event(id, completed);
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
                self.queue_url_placeholder(url);
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
        }
    }

    pub(crate) fn drain_download_events(
        &mut self,
        download_rx: &mut mpsc::UnboundedReceiver<DownloadEvent>,
    ) -> bool {
        let mut handled = false;
        let mut pending_progress: Vec<(FileId, crate::core::ProgressDelta, u64)> = Vec::new();
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
                    if let Some((_, pending_delta, _)) =
                        pending_progress
                            .iter_mut()
                            .find(|(pending_id, _, pending_attempt_id)| {
                                pending_id == &id && *pending_attempt_id == attempt_id
                            })
                    {
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
                    let _ = self.flush_pending_progress_events(&mut pending_progress);
                    self.handle_download_event(other);
                    handled = true;
                }
            }
        }
        handled |= self.flush_pending_progress_events(&mut pending_progress);
        handled
    }

    pub(crate) fn drain_token_messages(&mut self) {
        for _ in 0..MAX_TOKEN_MESSAGES_PER_TICK {
            let Ok(msg) = self.token_rx.try_recv() else {
                break;
            };
            self.cancellation_tokens.insert(msg.file_id, msg.token);
        }
    }

    pub(crate) fn log_progress_summary(&mut self) {
        self.update_speeds();
        if self.files_total == 0 {
            return;
        }
        let pct = if self.total_size > 0 {
            self.total_downloaded * 100 / self.total_size
        } else {
            0
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
        state_tx: &watch::Sender<String>,
    ) -> bool {
        self.publish_dashboard_snapshot_if_observed(state_tx, DashboardUiMode::Tui, false)
    }

    pub(crate) fn publish_dashboard_snapshot_if_observed(
        &mut self,
        state_tx: &watch::Sender<String>,
        ui_mode: DashboardUiMode,
        read_only: bool,
    ) -> bool {
        let observed = match ui_mode {
            DashboardUiMode::Tui => state_tx.receiver_count() > 1,
            DashboardUiMode::Headless | DashboardUiMode::Attached => state_tx.receiver_count() > 0,
        };
        if observed {
            state_tx.send_replace(self.cached_dashboard_json(ui_mode, read_only));
            return true;
        }
        false
    }

    fn has_active_dashboard_transfer(&self) -> bool {
        self.files
            .iter()
            .any(|file| matches!(file.status, FileStatus::Downloading))
            || !self.verification_targets.is_empty()
    }

    pub(crate) fn handle_terminal_tick(
        &mut self,
        download_rx: &mut mpsc::UnboundedReceiver<DownloadEvent>,
        action_rx: &mut mpsc::UnboundedReceiver<UiAction>,
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
        self.poll_session_persistence();

        dashboard_dirty
    }

    pub(crate) async fn run_headless_until_shutdown<F>(
        &mut self,
        download_rx: &mut mpsc::UnboundedReceiver<DownloadEvent>,
        action_rx: &mut mpsc::UnboundedReceiver<UiAction>,
        state_tx: Option<&watch::Sender<String>>,
        shutdown: F,
    ) where
        F: Future<Output = ()>,
    {
        let mut progress_interval = tokio::time::interval(Duration::from_secs(30));
        let mut publish_interval = tokio::time::interval(Duration::from_millis(250));
        progress_interval.tick().await;
        publish_interval.tick().await;

        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                () = &mut shutdown => break,
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
    use crate::test_support::StateDirectoryGuard;
    use tempfile::tempdir;

    #[test]
    fn ensure_download_session_refreshes_existing_session_credentials_without_mfa() {
        let dir = tempdir().expect("temp dir should exist");
        let _guard = StateDirectoryGuard::set(dir.path());
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, event_tx, true);
        let mut session = SessionSnapshot::new(
            DownloadConfig::default(),
            SavedCredentials::encrypt("stale@example.com", "stale-pass", Some("654321")),
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
            .decrypt()
            .expect("saved credentials should decrypt");
        assert_eq!(email, "fresh@example.com");
        assert_eq!(password, "fresh-pass");
        assert!(mfa.is_none());
    }

    #[test]
    fn ensure_download_session_does_not_create_empty_session() {
        let dir = tempdir().expect("temp dir should exist");
        let _guard = StateDirectoryGuard::set(dir.path());
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
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
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
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

        let snapshot: serde_json::Value =
            serde_json::from_str(shared_state.state_rx.borrow().as_str())
                .expect("shared state should contain valid JSON");
        assert_eq!(snapshot["status"], "updated from runtime");
    }

    #[test]
    fn tui_dashboard_snapshot_skips_single_internal_receiver() {
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
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
        assert_eq!(shared_state.state_rx.borrow().as_str(), initial);

        let _remote_rx = shared_state.state_rx.clone();

        assert!(
            app.publish_dashboard_snapshot_if_observed(&state_tx, DashboardUiMode::Tui, false,)
        );
        assert!(
            shared_state
                .state_rx
                .borrow()
                .contains("should not publish")
        );
    }

    #[test]
    fn dashboard_snapshot_reuses_cache_until_revision_changes() {
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
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
        assert!(shared_state.state_rx.borrow().contains("first"));

        app.status = "second".to_string();
        assert!(app.publish_dashboard_snapshot_if_observed(
            &state_tx,
            DashboardUiMode::Headless,
            false,
        ));
        assert!(shared_state.state_rx.borrow().contains("first"));

        app.mark_dashboard_dirty();
        assert!(app.publish_dashboard_snapshot_if_observed(
            &state_tx,
            DashboardUiMode::Headless,
            false,
        ));
        assert!(shared_state.state_rx.borrow().contains("second"));
    }

    #[test]
    fn terminal_tick_bounds_download_event_drain_to_keep_input_responsive() {
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, event_tx, true);
        let (download_tx, mut download_rx) = mpsc::unbounded_channel();
        let (_action_tx, mut action_rx) = mpsc::unbounded_channel();
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
    fn terminal_tick_does_not_dirty_dashboard_when_idle() {
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, event_tx, true);
        let (_download_tx, mut download_rx) = mpsc::unbounded_channel();
        let (_action_tx, mut action_rx) = mpsc::unbounded_channel();
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
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, event_tx, true);
        let (_download_tx, mut download_rx) = mpsc::unbounded_channel();
        let (_action_tx, mut action_rx) = mpsc::unbounded_channel();
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
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, event_tx, true);
        let (_download_tx, mut download_rx) = mpsc::unbounded_channel();
        let (_action_tx, mut action_rx) = mpsc::unbounded_channel();
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
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, event_tx, true);
        let (download_tx, mut download_rx) = mpsc::unbounded_channel();

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
                .send(DownloadEvent::Progress {
                    id: "file.bin".into(),
                    delta: crate::core::ProgressDelta {
                        total_bytes_delta: 10,
                        network_bytes_delta: 10,
                    },
                    attempt_id: 0,
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
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, event_tx, true);
        let (download_tx, mut download_rx) = mpsc::unbounded_channel();

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
            .send(DownloadEvent::VerificationProgress {
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
    fn drain_download_events_applies_verification_skip() {
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, event_tx, true);
        let (download_tx, mut download_rx) = mpsc::unbounded_channel();

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
            .send(DownloadEvent::VerificationSkipped {
                id: "file.bin".into(),
                completed: false,
            })
            .expect("verification skip should send");

        assert!(app.drain_download_events(&mut download_rx));
        assert!(!app.verifying_files.contains("file.bin"));
        assert!(!app.verification_inflight_files.contains("file.bin"));
    }
}
