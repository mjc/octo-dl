//! Application state model.

#[path = "app/bootstrap.rs"]
mod bootstrap;
#[path = "app/overlay.rs"]
mod overlay;
#[path = "app/progress.rs"]
mod progress;
#[path = "app/runtime.rs"]
mod runtime;
#[path = "app/snapshot.rs"]
mod snapshot;

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use indexmap::IndexMap;
use ratatui::widgets::ListState;
use serde::Serialize;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::{
    DownloadConfig, SessionState,
    core::{
        CoreEffect, CoreEvent, DownloadState, ProgressDelta, ResolvedFile, ResolvedPackage,
        RestartSnapshot, SessionMeta, SessionSnapshotV3, reconcile_restart, reduce,
        scan_filesystem,
    },
    format_bytes,
};

use self::progress::TransferRate;
#[cfg(test)]
use crate::{
    FileEntry as SessionFileEntry, FileEntryStatus, SavedCredentials, SessionStatus, UrlEntry,
    UrlStatus, core::FileLifecycle,
};
#[cfg(test)]
use std::time::Duration;

use super::WebOptions;
use super::download;
use super::event::{DownloadEvent, QueuedFile, TokenMessage};
use super::session::{SessionAdapter, SessionFileUpdate, SessionRunUpdate, SessionUrlUpdate};
use super::visible;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Popup {
    None,
    Login,
    Config,
}

/// What to do when `auto_login` finds no credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoCredentialsFallback {
    /// Open the login popup so the user can type them in.
    ShowPopup,
    /// Do nothing — used by headless API mode.
    Silent,
}

pub struct LoginState {
    email: String,
    password: String,
    mfa: String,
    pub active_field: usize,
    pub error: Option<String>,
    pub logging_in: bool,
}

impl LoginState {
    pub const fn new() -> Self {
        Self {
            email: String::new(),
            password: String::new(),
            mfa: String::new(),
            active_field: 0,
            error: None,
            logging_in: false,
        }
    }

    /// Sets credentials, rejecting empty strings.
    ///
    /// Returns `true` if both email and password were non-empty and stored.
    pub fn set_credentials(&mut self, email: String, password: String, mfa: String) -> bool {
        if email.is_empty() || password.is_empty() {
            return false;
        }
        self.email = email;
        self.password = password;
        self.mfa = mfa;
        true
    }

    /// Fills in credentials only where the current value is empty.
    ///
    /// Used for fallback sources (env vars) that should not override
    /// explicit sources (config file, session).
    pub fn set_credentials_if_missing(&mut self, email: &str, password: &str, mfa: &str) {
        if self.email.is_empty() && !email.is_empty() {
            email.clone_into(&mut self.email);
        }
        if self.password.is_empty() && !password.is_empty() {
            password.clone_into(&mut self.password);
        }
        if self.mfa.is_empty() && !mfa.is_empty() {
            mfa.clone_into(&mut self.mfa);
        }
    }

    pub const fn has_credentials(&self) -> bool {
        !self.email.is_empty() && !self.password.is_empty()
    }

    pub fn email(&self) -> &str {
        &self.email
    }

    pub fn password(&self) -> &str {
        &self.password
    }

    pub fn mfa(&self) -> &str {
        &self.mfa
    }

    /// Returns `Some(mfa)` when non-empty, `None` otherwise.
    pub fn mfa_option(&self) -> Option<&str> {
        if self.mfa.is_empty() {
            None
        } else {
            Some(&self.mfa)
        }
    }

    pub const fn active_value_mut(&mut self) -> &mut String {
        match self.active_field {
            0 => &mut self.email,
            1 => &mut self.password,
            _ => &mut self.mfa,
        }
    }

    pub const fn field_count() -> usize {
        3
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ConfigField {
    ChunksPerFile,
    ConcurrentFiles,
    ForceOverwrite,
    CleanupOnError,
}

impl ConfigField {
    pub const ALL: [Self; 4] = [
        Self::ChunksPerFile,
        Self::ConcurrentFiles,
        Self::ForceOverwrite,
        Self::CleanupOnError,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::ChunksPerFile => "Chunks per file",
            Self::ConcurrentFiles => "Concurrent files",
            Self::ForceOverwrite => "Force overwrite",
            Self::CleanupOnError => "Cleanup on error",
        }
    }
}

pub struct ConfigState {
    pub config: DownloadConfig,
    pub active_field: usize,
}

impl ConfigState {
    pub fn new() -> Self {
        Self {
            config: DownloadConfig::default(),
            active_field: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    Queued,
    Downloading,
    Complete,
    Error(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct FileEntry {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub downloaded: u64,
    #[serde(skip)]
    pub source_url: Option<String>,
    #[serde(skip)]
    pub(crate) counts_toward_progress: bool,
    pub status: FileStatus,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct FileUiState {
    pub speed: u64,
    pub rate: TransferRate,
}

#[derive(Debug, Clone)]
pub(crate) struct VisibleFileContext {
    pub id: String,
    pub source_url: Option<String>,
    pub artifact_path: String,
    pub size: u64,
    pub counts_toward_progress: bool,
    pub is_core_backed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitPolicy {
    Enabled,
    Disabled,
}

impl QuitPolicy {
    pub const fn from_bool(enabled: bool) -> Self {
        if enabled {
            Self::Enabled
        } else {
            Self::Disabled
        }
    }

    pub const fn is_enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }
}

pub struct App {
    pub popup: Popup,
    pub should_quit: bool,
    pub quit_policy: QuitPolicy,
    // Auth
    pub login: LoginState,
    pub authenticated: bool,
    // URL input (top bar)
    pub url_input: String,
    // Tracked URLs for session persistence
    pub urls: Vec<String>,
    // File queue (main content)
    pub files: Vec<FileEntry>,
    pub(crate) overlay_files: IndexMap<String, FileEntry>,
    pub(crate) file_ui: HashMap<String, FileUiState>,
    pub file_list_state: ListState,
    // Aggregate stats
    pub total_downloaded: u64,
    pub total_size: u64,
    pub files_completed: usize,
    pub files_total: usize,
    pub current_speed: u64,
    total_network_downloaded: u64,
    aggregate_rate: TransferRate,
    // Status
    pub status: String,
    pub paused: bool,
    // Config
    pub config: ConfigState,
    // Channels
    pub event_tx: mpsc::UnboundedSender<DownloadEvent>,
    /// Always valid — URLs buffer in the channel until the download task starts.
    pub url_tx: mpsc::UnboundedSender<String>,
    /// Taken by `start_download_task` to give the receiver to the download task.
    pub(super) url_rx: Option<mpsc::UnboundedReceiver<String>>,
    /// Broadcasts pause state changes to the background download task.
    pub pause_tx: watch::Sender<bool>,
    /// Taken by `start_download_task` to give the receiver to the download task.
    pub(super) pause_rx: Option<watch::Receiver<bool>>,
    /// Always valid — tokens arrive once the download task is running.
    pub token_rx: mpsc::UnboundedReceiver<TokenMessage>,
    /// Taken by `start_download_task` to give the sender to the download task.
    pub(super) token_tx: Option<mpsc::UnboundedSender<TokenMessage>>,
    /// Receives the authenticated client from the login task.
    pub client_rx: Option<tokio::sync::oneshot::Receiver<(mega::Client, reqwest::Client)>>,
    // Cancellation tokens for active downloads (maps file path to token)
    pub cancellation_tokens: HashMap<String, CancellationToken>,
    // Files deleted from the UI — used to suppress stale download events
    pub deleted_files: HashSet<String>,
    // Session
    pub session: Option<SessionState>,
    pub core_state: DownloadState,
    // API port for display
    pub api_port: u16,
    // API key for authentication
    pub api_key: Option<String>,
    // Resource usage
    pub cpu_usage: f32,
    pub memory_rss: u64,
    // Speed tracking
    pub last_tick: Instant,
}

impl App {
    pub fn sorted_file_indices(&self) -> Vec<usize> {
        visible::sorted_file_indices(&self.files, &self.core_state)
    }

    pub fn selected_file_index(&self) -> Option<usize> {
        visible::selected_file_index(&self.file_list_state, &self.files, &self.core_state)
    }

    pub(crate) fn sync_visible_files(&mut self) {
        visible::sync_visible_files(
            &mut self.files,
            &mut self.overlay_files,
            &mut self.file_ui,
            &mut self.file_list_state,
            &self.core_state,
            &self.deleted_files,
        );
    }

    pub fn new(
        api_port: u16,
        event_tx: mpsc::UnboundedSender<DownloadEvent>,
        quit_enabled: bool,
    ) -> Self {
        let (url_tx, url_rx) = mpsc::unbounded_channel::<String>();
        let (pause_tx, pause_rx) = watch::channel(false);
        let (token_tx, token_rx) = mpsc::unbounded_channel::<TokenMessage>();
        Self {
            popup: Popup::None,
            should_quit: false,
            quit_policy: QuitPolicy::from_bool(quit_enabled),
            login: LoginState::new(),
            authenticated: false,
            url_input: String::new(),
            urls: Vec::new(),
            files: Vec::new(),
            overlay_files: IndexMap::new(),
            file_ui: HashMap::new(),
            file_list_state: ListState::default(),
            total_downloaded: 0,
            total_size: 0,
            files_completed: 0,
            files_total: 0,
            current_speed: 0,
            total_network_downloaded: 0,
            aggregate_rate: Default::default(),
            status: String::new(),
            paused: false,
            config: ConfigState::new(),
            event_tx,
            url_tx,
            url_rx: Some(url_rx),
            pause_tx,
            pause_rx: Some(pause_rx),
            token_rx,
            token_tx: Some(token_tx),
            client_rx: None,
            cancellation_tokens: HashMap::new(),
            deleted_files: HashSet::new(),
            session: None,
            core_state: DownloadState::new(SessionMeta {
                config: DownloadConfig::default(),
                ..SessionMeta::default()
            }),
            api_port,
            api_key: None,
            cpu_usage: 0.0,
            last_tick: Instant::now(),
            memory_rss: 0,
        }
    }

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
        let next = SessionState::from_v3(snapshot);
        let _ = self.mutate_session(|session| SessionAdapter::merge_state(session, next));
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

    pub(crate) fn submit_url(&mut self, url: String) {
        if self.urls.contains(&url) {
            return;
        }
        self.urls.push(url.clone());
        self.apply_core_event(CoreEvent::UrlSubmitted { url: url.clone() });
        self.update_session_url(&url, SessionUrlUpdate::Pending);
        let _ = self.url_tx.send(url);
    }

    fn update_session_url(&mut self, url: &str, update: SessionUrlUpdate<'_>) {
        let _ = self
            .mutate_session_and_save(|session| SessionAdapter::update_url(session, url, update));
    }

    fn update_session_file(&mut self, file_id: &str, update: SessionFileUpdate<'_>) {
        let _ =
            self.mutate_session(|session| SessionAdapter::update_file(session, file_id, update));
    }

    fn register_session_queued_file(&mut self, submitted_url: &str, path: &str, size: u64) -> bool {
        self.mutate_session_and_save(|session| {
            SessionAdapter::register_queued_file(session, submitted_url, path, size)
        })
        .unwrap_or(true)
    }

    fn mutate_session<R>(&mut self, f: impl FnOnce(&mut SessionState) -> R) -> Option<R> {
        self.session.as_mut().map(f)
    }

    fn mutate_session_and_save<R>(&mut self, f: impl FnOnce(&mut SessionState) -> R) -> Option<R> {
        self.session.as_mut().map(|session| {
            let result = f(session);
            let _ = session.save();
            result
        })
    }

    fn read_session<R>(&self, f: impl FnOnce(&SessionState) -> R) -> Option<R> {
        self.session.as_ref().map(f)
    }

    fn update_session_run_status(&mut self, update: SessionRunUpdate) {
        let _ = self.mutate_session(|session| SessionAdapter::apply_run_update(session, update));
    }

    fn install_session(&mut self, session: SessionState) {
        self.session = Some(session);
        self.seed_core_session_from_session();
    }

    fn save_and_install_session(&mut self, session: SessionState) {
        let _ = session.save();
        self.install_session(session);
    }

    pub(crate) fn restore_restart_snapshot(&mut self, snapshot: &RestartSnapshot) {
        self.core_state = snapshot.state.clone();
        self.sync_visible_files();
        self.recompute_totals();
    }

    pub(crate) fn resume_latest_session(&mut self) {
        let Some(session) = SessionState::latest() else {
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
            Some(session.to_v3()),
            scan_filesystem(file_ids),
            session.urls.iter().map(|entry| entry.url.clone()).collect(),
        );

        self.resume_from_restart(session, &restart);
    }

    pub(crate) fn resume_from_restart(
        &mut self,
        mut session: SessionState,
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

    fn sync_session_after_file_complete(&mut self, id: &str) {
        self.update_session_file(id, SessionFileUpdate::Complete);
        if self.files_completed == self.files_total && self.files_total > 0 {
            self.update_session_run_status(SessionRunUpdate::Completed);
        }
    }

    fn update_download_status_message(&mut self) {
        if self.files_completed == self.files_total && self.files_total > 0 {
            self.status = "All downloads complete".to_string();
        } else {
            self.status = format!(
                "Downloading ({}/{})",
                self.files_completed, self.files_total
            );
        }
    }

    fn note_file_error(&mut self, id: &str, error: &str) {
        self.update_session_file(id, SessionFileUpdate::Error(error));
    }

    fn mark_file_skipped(&mut self, id: &str) {
        self.update_session_file(id, SessionFileUpdate::Skipped);
    }

    fn handle_deleted_download_artifact(&mut self, id: &str, artifact_path: &str) -> bool {
        if !self.deleted_files.remove(id) {
            return false;
        }

        self.cancellation_tokens.remove(id);
        download::schedule_resume_artifact_delete(artifact_path.to_string());
        self.mark_file_skipped(id);
        true
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

    fn handle_download_error_event(&mut self, id: Option<String>, name: String, error: String) {
        log::error!("Download error: {name}: {error}");
        if let Some(id) = id.as_ref()
            && self.handle_deleted_download_artifact(id, &name)
        {
            return;
        }

        if self.is_session_url(&name) {
            self.handle_session_url_error(&name, &error);
        } else if let Some(id) = id {
            self.apply_core_event(CoreEvent::FileFailed {
                file_id: id.clone(),
                message: error.clone(),
            });
            self.mark_visible_file_error(&id, &name, &error);
        } else {
            self.show_ui_error_only(&name, &error);
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

    fn handle_file_queued_event(&mut self, file: QueuedFile) {
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

    fn handle_url_resolved_event(&mut self, url: String) {
        self.handle_session_url_fetched(&url);
    }

    fn handle_file_start_event(&mut self, id: String, name: String, size: u64) {
        log::info!("Download started: {name} ({})", format_bytes(size));
        if self.deleted_files.contains(&id) {
            return;
        }
        let source_url = self
            .files
            .iter()
            .find(|entry| entry.id == id)
            .and_then(|entry| entry.source_url.clone())
            .unwrap_or_else(|| id.clone());
        self.ensure_core_file(&id, &source_url, &name, size, true);
        self.apply_core_event(CoreEvent::FileStarted {
            file_id: id.clone(),
            size,
        });
        self.reset_file_ui_rate(&id);
    }

    fn handle_file_progress_event(&mut self, id: std::sync::Arc<str>, delta: ProgressDelta) {
        if self.deleted_files.contains(id.as_ref()) {
            return;
        }
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

    fn handle_resume_reused_event(&mut self, id: String, chunks: usize, bytes: u64) {
        if self.deleted_files.contains(&id) {
            return;
        }
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

    fn handle_file_complete_event(&mut self, id: String, name: String) {
        log::info!("Download complete: {name}");
        if self.handle_deleted_download_artifact(&id, &name) {
            return;
        }
        self.apply_core_event(CoreEvent::FileCompleted {
            file_id: id.clone(),
        });
        self.recompute_totals();
        self.mark_visible_file_complete(&id, &name);
    }

    fn handle_file_cancelled_event(&mut self, id: String, name: String) {
        log::info!("Download cancelled: {name}");
        if self.handle_deleted_download_artifact(&id, &name) {
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
        let is_core_backed = context
            .as_ref()
            .is_some_and(|context| context.is_core_backed);
        let artifact_path = context
            .as_ref()
            .map_or_else(|| id.to_string(), |context| context.artifact_path.clone());
        if let Some(context) = context.as_ref() {
            let _ = self.ensure_core_file_from_context(context);
        }
        self.cancel_file_token(id);
        self.deleted_files.insert(id.to_string());
        if is_core_backed {
            self.apply_core_event(CoreEvent::FileDeleted {
                file_id: id.to_string(),
            });
        } else {
            let _ = self.remove_overlay_file(id);
        }
        download::schedule_download_artifact_delete(artifact_path);
        self.mark_file_skipped(id);
        if !is_core_backed {
            self.recompute_totals();
        }
    }

    pub(crate) fn perform_retry_file_action(&mut self, id: &str) {
        let context = self.visible_file_context(id);
        let source_url = context
            .as_ref()
            .and_then(|context| self.ensure_core_file_from_context(context));
        self.apply_core_event(CoreEvent::FileRetryRequested {
            file_id: id.to_string(),
        });
        if let Some(url) = source_url {
            self.reset_file_ui_rate(id);
            let _ = self.url_tx.send(url);
        } else {
            self.status = format!("Retry unavailable for {id}");
            if !self.core_state.files.contains_key(id) {
                self.show_overlay_error(id, id, "Retry unavailable for this file", true);
            }
        }
    }

    pub(crate) fn perform_reset_file_action(&mut self, id: &str) {
        let Some(context) = self.visible_file_context(id) else {
            return;
        };
        let Some(source_url) = self.ensure_core_file_from_context(&context) else {
            if !self.core_state.files.contains_key(id) {
                self.show_overlay_error(id, id, "Reset unavailable for this file", true);
            }
            self.status = "Reset unavailable for selected file".to_string();
            self.recompute_totals();
            return;
        };

        self.cancel_file_token(id);

        self.apply_core_event(CoreEvent::FileResetRequested {
            file_id: id.to_string(),
        });
        self.reset_file_ui_rate(id);

        download::schedule_download_artifact_delete(context.artifact_path);

        let _ = self.url_tx.send(source_url);
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
            UiAction::RetryFile(id) => self.perform_retry_file_action(&id),
            UiAction::ResetFile(id) => self.perform_reset_file_action(&id),
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

    pub(crate) fn drain_ui_actions(
        &mut self,
        action_rx: &mut mpsc::UnboundedReceiver<UiAction>,
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
            .filter(|file| matches!(file.status, FileStatus::Downloading))
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
                file.status = FileStatus::Queued;
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

    /// Serialises UI-visible state to a JSON string.
    ///
    /// Called by the event loop *only* when state has changed and at least
    /// one SSE/API client is connected — never on a blind timer.
    #[allow(dead_code)]
    pub fn to_json(&self) -> String {
        snapshot::to_json(self)
    }
}

// ---------------------------------------------------------------------------
// Types shared with the API server (channel-based, no locks)
// ---------------------------------------------------------------------------

/// An action sent from an API handler into the single-owner event loop.
#[allow(dead_code)]
#[derive(Debug)]
pub enum UiAction {
    AddUrls(Vec<String>),
    Login {
        email: String,
        password: String,
        mfa: String,
    },
    TogglePause,
    DeleteFile(String),
    RetryFile(String),
    ResetFile(String),
    UpdateConfig {
        chunks_per_file: Option<usize>,
        concurrent_files: Option<usize>,
        force_overwrite: Option<bool>,
        cleanup_on_error: Option<bool>,
    },
}

/// Cheaply cloneable handle given to API handlers.
///
/// * `action_tx` — fire-and-forget mutations into the event loop.
/// * `state_rx`  — latest JSON snapshot; `borrow()` is lock-free.
///
/// No `Mutex`, no `RwLock`, no `broadcast` cloning.
#[derive(Clone)]
pub struct SharedAppState {
    pub action_tx: mpsc::UnboundedSender<UiAction>,
    pub state_rx: watch::Receiver<String>,
}

pub(crate) struct SharedStateChannels {
    pub action_rx: mpsc::UnboundedReceiver<UiAction>,
    pub state_tx: watch::Sender<String>,
    pub shared_state: Option<SharedAppState>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    fn test_app() -> App {
        let (tx, _rx) = mpsc::unbounded_channel();
        App::new(9723, tx, true)
    }

    #[test]
    fn login_state_field_cycling() {
        let mut login = LoginState::new();
        assert_eq!(login.active_field, 0);
        login.active_field = (login.active_field + 1) % LoginState::field_count();
        assert_eq!(login.active_field, 1);
        login.active_field = (login.active_field + 1) % LoginState::field_count();
        assert_eq!(login.active_field, 2);
        login.active_field = (login.active_field + 1) % LoginState::field_count();
        assert_eq!(login.active_field, 0);
    }

    #[test]
    fn config_field_increment_decrement() {
        let mut config = ConfigState::new();
        let initial_chunks = config.config.chunks_per_file;
        config.config.chunks_per_file = config.config.chunks_per_file.saturating_add(1);
        assert_eq!(config.config.chunks_per_file, initial_chunks + 1);
        config.config.chunks_per_file = config.config.chunks_per_file.saturating_sub(1).max(1);
        assert_eq!(config.config.chunks_per_file, initial_chunks);
    }

    #[test]
    fn config_field_toggle_bool() {
        let mut config = ConfigState::new();
        let initial = config.config.force_overwrite;
        config.config.force_overwrite = !config.config.force_overwrite;
        assert_ne!(config.config.force_overwrite, initial);
        config.config.force_overwrite = !config.config.force_overwrite;
        assert_eq!(config.config.force_overwrite, initial);
    }

    #[test]
    fn app_initial_state() {
        let app = test_app();
        assert_eq!(app.popup, Popup::None);
        assert!(!app.should_quit);
        assert!(!app.authenticated);
        assert!(app.url_input.is_empty());
        assert!(app.files.is_empty());
        assert_eq!(app.files_completed, 0);
        assert_eq!(app.files_total, 0);
    }

    #[test]
    fn login_state_active_value_mut() {
        let mut login = LoginState::new();

        login.active_field = 0;
        login.active_value_mut().push_str("test@example.com");
        assert_eq!(login.email(), "test@example.com");

        login.active_field = 1;
        login.active_value_mut().push_str("password123");
        assert_eq!(login.password(), "password123");

        login.active_field = 2;
        login.active_value_mut().push_str("123456");
        assert_eq!(login.mfa(), "123456");
    }

    #[test]
    fn set_credentials_rejects_empty() {
        let mut login = LoginState::new();
        assert!(!login.set_credentials(String::new(), "pass".into(), String::new()));
        assert!(!login.set_credentials("user".into(), String::new(), String::new()));
        assert!(!login.has_credentials());
        assert!(login.set_credentials("user@example.com".into(), "pass".into(), String::new()));
        assert!(login.has_credentials());
    }

    #[test]
    fn set_credentials_if_missing_does_not_override() {
        let mut login = LoginState::new();
        login.set_credentials("orig@example.com".into(), "origpass".into(), String::new());
        login.set_credentials_if_missing("new@example.com", "newpass", "123456");
        assert_eq!(login.email(), "orig@example.com");
        assert_eq!(login.password(), "origpass");
        assert_eq!(login.mfa(), "123456"); // mfa was empty, so it gets filled
    }

    #[test]
    fn set_credentials_if_missing_fills_empty() {
        let mut login = LoginState::new();
        login.set_credentials_if_missing("user@example.com", "pass", "");
        assert_eq!(login.email(), "user@example.com");
        assert_eq!(login.password(), "pass");
        assert!(login.has_credentials());
    }

    #[test]
    fn mfa_option_returns_none_when_empty() {
        let mut login = LoginState::new();
        assert!(login.mfa_option().is_none());
        login.set_credentials("u".into(), "p".into(), "123".into());
        assert_eq!(login.mfa_option(), Some("123"));
    }

    #[test]
    fn config_field_labels() {
        assert_eq!(ConfigField::ChunksPerFile.label(), "Chunks per file");
        assert_eq!(ConfigField::ConcurrentFiles.label(), "Concurrent files");
        assert_eq!(ConfigField::ForceOverwrite.label(), "Force overwrite");
        assert_eq!(ConfigField::CleanupOnError.label(), "Cleanup on error");
    }

    #[test]
    fn quit_policy_converts_from_bool() {
        assert_eq!(QuitPolicy::from_bool(true), QuitPolicy::Enabled);
        assert_eq!(QuitPolicy::from_bool(false), QuitPolicy::Disabled);
        assert!(QuitPolicy::Enabled.is_enabled());
        assert!(!QuitPolicy::Disabled.is_enabled());
    }

    #[test]
    fn to_json_contains_visible_file_state_without_internal_fields() {
        let mut app = test_app();
        app.files.push(FileEntry {
            id: "stable/file.bin".to_string(),
            name: "file.bin".to_string(),
            size: 128,
            downloaded: 64,
            source_url: Some("https://mega.nz/file/abc".to_string()),
            counts_toward_progress: true,
            status: FileStatus::Downloading,
        });
        app.file_ui.insert(
            "stable/file.bin".to_string(),
            FileUiState {
                speed: 32,
                rate: Default::default(),
            },
        );
        app.cpu_usage = 12.5;
        app.memory_rss = 4096;
        app.recompute_totals();

        let snapshot: serde_json::Value =
            serde_json::from_str(&app.to_json()).expect("snapshot should be valid JSON");
        let file = &snapshot["files"][0];

        assert_eq!(file["id"], "stable/file.bin");
        assert_eq!(file["status"], "downloading");
        assert_eq!(
            snapshot["packages"][0]["source_url"],
            "https://mega.nz/file/abc"
        );
        assert_eq!(snapshot["total_downloaded"], 64);
        assert_eq!(snapshot["total_size"], 128);
        assert_eq!(snapshot["run_totals"]["run_total_bytes"], 128);
        assert_eq!(snapshot["displayed_network_rate_bps"], 0);
        assert!(file.get("rate").is_none());
        assert!(file.get("source_url").is_none());
        assert_eq!(snapshot["cpu_usage"], 12.5);
        assert_eq!(snapshot["memory_rss"], 4096);
    }

    #[test]
    fn transfer_rate_smooths_cumulative_samples() {
        let start = Instant::now();
        let mut rate = TransferRate::default();

        rate.reset(0, start);
        rate.record(100_000, start + Duration::from_millis(100));

        assert_eq!(rate.bytes_per_sec(start + Duration::from_millis(100)), 0);

        rate.reset(0, start);
        rate.record(1_000, start + Duration::from_secs(1));
        rate.record(2_000, start + Duration::from_secs(2));

        let current = rate.bytes_per_sec(start + Duration::from_secs(2));
        assert!((950..=1_050).contains(&current));

        let decayed = rate.bytes_per_sec(start + Duration::from_secs(11));
        assert!(decayed < current);
    }

    #[test]
    fn aggregate_rate_uses_progress_since_current_baseline() {
        let start = Instant::now();
        let mut app = test_app();
        app.files.push(FileEntry {
            id: "file.bin".to_string(),
            name: "file.bin".to_string(),
            size: 2_000,
            downloaded: 1_000,
            source_url: None,
            counts_toward_progress: true,
            status: FileStatus::Downloading,
        });
        app.total_downloaded = 1_000;
        app.total_network_downloaded = 1_000;
        app.aggregate_rate.reset(1_000, start);

        app.total_downloaded = app.total_downloaded.saturating_add(100);
        app.total_network_downloaded = app.total_network_downloaded.saturating_add(100);
        app.update_speeds_at(start + Duration::from_secs(1));

        assert!((95..=105).contains(&app.current_speed));
    }

    #[test]
    fn aggregate_rate_ignores_reused_bytes() {
        let start = Instant::now();
        let mut app = test_app();
        app.files.push(FileEntry {
            id: "file.bin".to_string(),
            name: "file.bin".to_string(),
            size: 2_000,
            downloaded: 1_000,
            source_url: None,
            counts_toward_progress: true,
            status: FileStatus::Downloading,
        });
        app.total_downloaded = 1_000;
        app.aggregate_rate.reset(0, start);

        app.total_downloaded = app.total_downloaded.saturating_add(1_000);
        app.update_speeds_at(start + Duration::from_secs(1));

        assert_eq!(app.current_speed, 0);
        assert_eq!(app.total_downloaded, 2_000);
    }

    #[test]
    fn record_progress_caps_downloaded_at_file_size() {
        let mut app = test_app();
        let file = FileEntry {
            id: "file.bin".to_string(),
            name: "file.bin".to_string(),
            size: 100,
            downloaded: 90,
            source_url: None,
            counts_toward_progress: true,
            status: FileStatus::Downloading,
        };
        let now = Instant::now();

        app.file_ui
            .insert("file.bin".to_string(), FileUiState::default());
        app.files.push(file.clone());
        app.files[0].downloaded = 100;
        let accepted = app.update_file_ui_progress("file.bin", 90, now);

        assert_eq!(accepted, 10);
        assert!(app.file_speed("file.bin") <= u64::MAX);
    }

    #[test]
    fn skipped_session_paths_groups_only_skipped_files_by_url() {
        let mut app = test_app();
        let mut session = SessionState::new(
            SavedCredentials::encrypt("test@example.com", "hunter2", None),
            DownloadConfig::default(),
            vec![
                UrlEntry {
                    url: "https://mega.nz/file/a".to_string(),
                    status: UrlStatus::Fetched,
                },
                UrlEntry {
                    url: "https://mega.nz/file/b".to_string(),
                    status: UrlStatus::Fetched,
                },
            ],
        );
        session.files = vec![
            SessionFileEntry {
                key: Some("0:skip-a.bin".to_string()),
                url_index: 0,
                path: "skip-a.bin".to_string(),
                size: 1,
                status: FileEntryStatus::Skipped,
            },
            SessionFileEntry {
                key: Some("1:skip-b.bin".to_string()),
                url_index: 1,
                path: "skip-b.bin".to_string(),
                size: 1,
                status: FileEntryStatus::Skipped,
            },
            SessionFileEntry {
                key: Some("0:pending.bin".to_string()),
                url_index: 0,
                path: "pending.bin".to_string(),
                size: 1,
                status: FileEntryStatus::Pending,
            },
        ];
        app.session = Some(session);

        let skipped = app.skipped_session_paths();

        assert_eq!(skipped.len(), 2);
        assert!(
            skipped["https://mega.nz/file/a"].contains("skip-a.bin"),
            "skipped paths should include skipped file under original URL"
        );
        assert!(
            !skipped["https://mega.nz/file/a"].contains("pending.bin"),
            "non-skipped files must not appear in the snapshot"
        );
        assert!(skipped["https://mega.nz/file/b"].contains("skip-b.bin"));
    }

    #[test]
    fn register_session_queued_file_does_not_revive_skipped_entry() {
        let mut app = test_app();
        let mut session = SessionState::new(
            SavedCredentials::encrypt("test@example.com", "hunter2", None),
            DownloadConfig::default(),
            vec![UrlEntry {
                url: "https://mega.nz/file/a".to_string(),
                status: UrlStatus::Fetched,
            }],
        );
        session.files.push(SessionFileEntry {
            key: None,
            url_index: 0,
            path: "skip-a.bin".to_string(),
            size: 1,
            status: FileEntryStatus::Skipped,
        });
        app.session = Some(session);

        let should_queue =
            app.register_session_queued_file("https://mega.nz/file/a", "skip-a.bin", 1);

        assert!(!should_queue);
        let session = app.session.as_ref().unwrap();
        assert_eq!(session.files.len(), 1);
        assert!(matches!(session.files[0].status, FileEntryStatus::Skipped));
        assert_eq!(session.files[0].key.as_deref(), Some("0:skip-a.bin"));
    }

    #[test]
    fn url_resolved_updates_session_status_and_clears_overlay() {
        let mut app = test_app();
        let url = "https://mega.nz/folder/root".to_string();
        app.session = Some(SessionState::new(
            SavedCredentials::encrypt("test@example.com", "hunter2", None),
            DownloadConfig::default(),
            vec![UrlEntry {
                url: url.clone(),
                status: UrlStatus::Pending,
            }],
        ));

        app.handle_download_event(DownloadEvent::UrlQueued { url: url.clone() });
        assert!(app.overlay_files.contains_key(&url));

        app.handle_download_event(DownloadEvent::UrlResolved { url: url.clone() });

        assert!(!app.overlay_files.contains_key(&url));
        let session = app.session.as_ref().expect("session should remain");
        assert_eq!(session.urls[0].status, UrlStatus::Fetched);
    }

    #[test]
    fn mark_visible_file_error_updates_session_file_status() {
        let mut app = test_app();
        let mut session = SessionState::new(
            SavedCredentials::encrypt("test@example.com", "hunter2", None),
            DownloadConfig::default(),
            vec![UrlEntry {
                url: "https://mega.nz/file/root".to_string(),
                status: UrlStatus::Fetched,
            }],
        );
        session.files.push(SessionFileEntry {
            key: Some("0:file-id".to_string()),
            url_index: 0,
            path: "file-id".to_string(),
            size: 128,
            status: FileEntryStatus::Pending,
        });
        app.session = Some(session);

        app.mark_visible_file_error("file-id", "file-id", "network failure");

        let session = app.session.as_ref().expect("session should remain");
        assert!(matches!(
            session.files[0].status,
            FileEntryStatus::Error(ref msg) if msg == "network failure"
        ));
    }

    #[test]
    fn session_adapter_merge_state_updates_matching_files_and_preserves_unmatched_entries() {
        let mut session = SessionState::new(
            SavedCredentials::encrypt("old@example.com", "hunter2", None),
            DownloadConfig::default(),
            vec![UrlEntry {
                url: "https://mega.nz/file/a".to_string(),
                status: UrlStatus::Pending,
            }],
        );
        session.files = vec![
            SessionFileEntry {
                key: Some("0:keep.bin".to_string()),
                url_index: 0,
                path: "keep.bin".to_string(),
                size: 1,
                status: FileEntryStatus::Pending,
            },
            SessionFileEntry {
                key: Some("0:stale.bin".to_string()),
                url_index: 0,
                path: "stale.bin".to_string(),
                size: 1,
                status: FileEntryStatus::Pending,
            },
        ];

        let mut next = SessionState::new(
            SavedCredentials::encrypt("new@example.com", "hunter2", None),
            DownloadConfig::default(),
            vec![
                UrlEntry {
                    url: "https://mega.nz/file/a".to_string(),
                    status: UrlStatus::Fetched,
                },
                UrlEntry {
                    url: "https://mega.nz/file/b".to_string(),
                    status: UrlStatus::Pending,
                },
            ],
        );
        next.status = SessionStatus::Paused;
        next.files = vec![
            SessionFileEntry {
                key: Some("0:keep.bin".to_string()),
                url_index: 0,
                path: "keep.bin".to_string(),
                size: 5,
                status: FileEntryStatus::Completed,
            },
            SessionFileEntry {
                key: Some("1:new.bin".to_string()),
                url_index: 1,
                path: "new.bin".to_string(),
                size: 2,
                status: FileEntryStatus::Pending,
            },
        ];

        SessionAdapter::merge_state(&mut session, next);

        assert_eq!(session.status, SessionStatus::Paused);
        assert_eq!(session.urls.len(), 2);
        assert!(
            session
                .urls
                .iter()
                .any(|entry| entry.url == "https://mega.nz/file/b"),
            "new URLs should be appended during merge"
        );
        assert_eq!(session.files.len(), 3);
        assert!(
            session.files.iter().any(|file| file.path == "keep.bin"
                && matches!(file.status, FileEntryStatus::Completed)
                && file.size == 5),
            "matching files should be replaced by the newer snapshot"
        );
        assert!(
            session.files.iter().any(|file| file.path == "stale.bin"),
            "existing unmatched files should be retained during partial migration"
        );
        assert!(session.files.iter().any(|file| file.path == "new.bin"));
    }

    #[test]
    fn sorted_file_indices_group_by_package_before_status() {
        let mut app = test_app();
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: "pkg-a".to_string(),
                source_url: "https://mega.nz/folder/a".to_string(),
                display_name: "Package A".to_string(),
                files: vec![
                    ResolvedFile {
                        file_id: "a-queued.bin".to_string(),
                        path: "a-queued.bin".to_string(),
                        size: 10,
                    },
                    ResolvedFile {
                        file_id: "a-complete.bin".to_string(),
                        path: "a-complete.bin".to_string(),
                        size: 10,
                    },
                ],
                collision: None,
            },
        });
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: "pkg-b".to_string(),
                source_url: "https://mega.nz/folder/b".to_string(),
                display_name: "Package B".to_string(),
                files: vec![ResolvedFile {
                    file_id: "b-downloading.bin".to_string(),
                    path: "b-downloading.bin".to_string(),
                    size: 10,
                }],
                collision: None,
            },
        });
        app.apply_core_event(CoreEvent::FileQueued {
            file_id: "a-queued.bin".to_string(),
        });
        app.apply_core_event(CoreEvent::FileCompleted {
            file_id: "a-complete.bin".to_string(),
        });
        app.apply_core_event(CoreEvent::FileStarted {
            file_id: "b-downloading.bin".to_string(),
            size: 10,
        });

        let ordered: Vec<_> = app
            .sorted_file_indices()
            .into_iter()
            .map(|index| app.files[index].id.clone())
            .collect();

        assert_eq!(
            ordered,
            vec![
                "a-queued.bin".to_string(),
                "a-complete.bin".to_string(),
                "b-downloading.bin".to_string(),
            ]
        );
    }

    #[test]
    fn pause_downloads_queues_core_backed_active_files() {
        let mut app = test_app();
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: "pkg".to_string(),
                source_url: "https://mega.nz/folder/root".to_string(),
                display_name: "Package".to_string(),
                files: vec![ResolvedFile {
                    file_id: "episode.bin".to_string(),
                    path: "episode.bin".to_string(),
                    size: 128,
                }],
                collision: None,
            },
        });
        app.apply_core_event(CoreEvent::FileStarted {
            file_id: "episode.bin".to_string(),
            size: 128,
        });
        let token = CancellationToken::new();
        app.cancellation_tokens
            .insert("episode.bin".to_string(), token.clone());

        app.pause_downloads();

        assert!(app.paused);
        assert!(token.is_cancelled());
        assert_eq!(
            app.core_state.files["episode.bin"].lifecycle,
            FileLifecycle::Queued
        );
        assert_eq!(
            app.files
                .iter()
                .find(|file| file.id == "episode.bin")
                .expect("visible row should remain")
                .status,
            FileStatus::Queued
        );
    }

    #[test]
    fn sync_visible_files_prunes_stale_file_ui_state() {
        let mut app = test_app();
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: "pkg".to_string(),
                source_url: "https://mega.nz/file/test".to_string(),
                display_name: "Package".to_string(),
                files: vec![ResolvedFile {
                    file_id: "kept.bin".to_string(),
                    path: "kept.bin".to_string(),
                    size: 128,
                }],
                collision: None,
            },
        });
        app.file_ui.insert(
            "kept.bin".to_string(),
            FileUiState {
                speed: 42,
                rate: Default::default(),
            },
        );
        app.file_ui.insert(
            "stale.bin".to_string(),
            FileUiState {
                speed: 99,
                rate: Default::default(),
            },
        );

        app.sync_visible_files();

        assert!(app.file_ui.contains_key("kept.bin"));
        assert!(!app.file_ui.contains_key("stale.bin"));
    }
}
