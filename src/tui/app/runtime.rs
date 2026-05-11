use std::future::Future;
use std::time::Duration;

use sysinfo::{ProcessesToUpdate, System};
use tokio::sync::{mpsc, watch};

use crate::{
    DownloadConfig,
    core::{PackageSnapshot, SavedCredentials, SessionSnapshotV3},
    format_bytes,
};

use super::{App, DownloadEvent, FileEntry, FileStatus, UiAction};

const MAX_DOWNLOAD_EVENTS_PER_TICK: usize = 256;
const MAX_TOKEN_MESSAGES_PER_TICK: usize = 256;

impl App {
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
            self.login.error = error;
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

        let channels = super::super::event::DownloadChannels {
            client_rx: self.client_rx.take(),
            event_tx: tx,
            url_rx,
            token_tx,
            pause_rx,
            skipped_session_paths: self.skipped_session_paths(),
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

        let mut session = SessionSnapshotV3::new(config.clone(), credentials);
        session.packages = self
            .urls
            .iter()
            .map(|url| PackageSnapshot {
                id: url.clone(),
                source_url: url.clone(),
                display_name: url.clone(),
                file_ids: Vec::new(),
                error: None,
            })
            .collect();
        self.save_and_install_session(session);
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

    pub(crate) fn set_resume_reuse_status(&mut self, id: &str, chunks: usize, bytes: u64) {
        self.status = format!(
            "Reusing {chunks} verified chunk(s) for {id} ({})",
            format_bytes(bytes)
        );
    }

    pub(crate) fn queue_url_placeholder(&mut self, url: String) {
        if !self.overlay_files.contains_key(&url) {
            self.upsert_overlay_file(
                FileEntry {
                    id: url.clone(),
                    name: url,
                    size: 0,
                    downloaded: 0,
                    status: FileStatus::Queued,
                },
                None,
                false,
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
            DownloadEvent::ResumeReused {
                id,
                chunks,
                bytes,
                attempt_id,
            } => {
                self.handle_resume_reused_event(id, chunks, bytes, attempt_id);
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
                if self.deleted_files.contains(&url) {
                    return;
                }
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
        for _ in 0..MAX_DOWNLOAD_EVENTS_PER_TICK {
            let Ok(event) = download_rx.try_recv() else {
                break;
            };
            self.handle_download_event(event);
            handled = true;
        }
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

    pub(crate) fn publish_snapshot_if_observed(&self, state_tx: &watch::Sender<String>) -> bool {
        if state_tx.receiver_count() > 1 {
            state_tx.send_replace(self.to_json());
            return true;
        }
        false
    }

    pub(crate) fn handle_terminal_tick(
        &mut self,
        download_rx: &mut mpsc::UnboundedReceiver<DownloadEvent>,
        action_rx: &mut mpsc::UnboundedReceiver<UiAction>,
        tick_count: u32,
        sys: &mut System,
        pid: Option<sysinfo::Pid>,
    ) {
        if tick_count.is_multiple_of(50) {
            self.refresh_resource_usage(sys, pid);
        }

        let _ = self.drain_download_events(download_rx);
        self.update_speeds();
        if tick_count.is_multiple_of(50) {
            self.log_progress_summary();
        }
        self.drain_token_messages();
        let _ = self.drain_ui_actions(action_rx);
    }

    pub(crate) async fn run_headless_until_shutdown<F>(
        &mut self,
        download_rx: &mut mpsc::UnboundedReceiver<DownloadEvent>,
        shutdown: F,
    ) where
        F: Future<Output = ()>,
    {
        let mut progress_interval = tokio::time::interval(Duration::from_secs(30));
        progress_interval.tick().await;

        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                () = &mut shutdown => break,
                event = download_rx.recv() => {
                    if let Some(evt) = event {
                        self.handle_download_event(evt);
                    } else {
                        log::warn!("Event channel closed");
                        break;
                    }
                }
                _ = progress_interval.tick() => {
                    self.log_progress_summary();
                }
            }

            let _ = self.drain_download_events(download_rx);
            self.drain_token_messages();
        }
    }

    #[allow(dead_code)]
    pub(crate) async fn run_web_until_shutdown<F>(
        &mut self,
        download_rx: &mut mpsc::UnboundedReceiver<DownloadEvent>,
        action_rx: &mut mpsc::UnboundedReceiver<UiAction>,
        state_tx: &watch::Sender<String>,
        shutdown: F,
    ) where
        F: Future<Output = ()>,
    {
        let mut progress_interval = tokio::time::interval(Duration::from_secs(30));
        progress_interval.tick().await;

        let mut fast_tick = tokio::time::interval(Duration::from_millis(100));
        fast_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut resource_tick = tokio::time::interval(Duration::from_secs(5));
        resource_tick.tick().await;
        let mut sys = System::new();
        let pid = sysinfo::get_current_pid().ok();

        let mut dirty = true;

        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                () = &mut shutdown => break,
                event = download_rx.recv() => {
                    if let Some(evt) = event {
                        self.handle_download_event(evt);
                        let _ = self.drain_download_events(download_rx);
                        self.drain_token_messages();
                        dirty = true;
                    } else {
                        log::warn!("Event channel closed");
                        break;
                    }
                }
                Some(action) = action_rx.recv() => {
                    self.handle_ui_action(action);
                    dirty = true;
                }
                _ = fast_tick.tick() => {
                    dirty |= self.drain_download_events(download_rx);
                    self.update_speeds();
                    self.drain_token_messages();
                    dirty |= self.drain_ui_actions(action_rx);
                }
                _ = resource_tick.tick() => {
                    self.refresh_resource_usage(&mut sys, pid);
                    dirty = true;
                }
                _ = progress_interval.tick() => {
                    self.log_progress_summary();
                }
            }

            if dirty && self.publish_snapshot_if_observed(state_tx) {
                dirty = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::tempdir;

    struct StateDirectoryGuard {
        _guard: crate::core::session::StateDirectoryTestGuard,
    }

    impl StateDirectoryGuard {
        fn set(path: &Path) -> Self {
            Self {
                _guard: crate::core::session::set_state_directory_for_test(path),
            }
        }
    }

    #[test]
    fn ensure_download_session_refreshes_existing_session_credentials_without_mfa() {
        let dir = tempdir().expect("temp dir should exist");
        let _guard = StateDirectoryGuard::set(dir.path());
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, event_tx, true);
        app.session = Some(SessionSnapshotV3::new(
            DownloadConfig::default(),
            SavedCredentials::encrypt("stale@example.com", "stale-pass", Some("654321")),
        ));
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

        app.handle_terminal_tick(&mut download_rx, &mut action_rx, 1, &mut sys, None);

        assert_eq!(download_rx.len(), 10);
        assert_eq!(
            app.status,
            format!("status {}", MAX_DOWNLOAD_EVENTS_PER_TICK - 1)
        );
    }
}
