//! Application state model.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use ratatui::widgets::ListState;
use serde::Serialize;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::{
    DownloadConfig, SessionState,
    core::{
        CoreEffect, CoreEvent, DownloadState, FileLifecycle, FileState, PackageState,
        ResolvedFile, ResolvedPackage, SessionMeta, reduce,
    },
};

use super::event::{DownloadEvent, TokenMessage};

const MIN_RATE_SAMPLE_SPAN: Duration = Duration::from_secs(1);
const THROUGHPUT_DECAY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub(crate) struct TransferRate {
    start_time: Instant,
    last_time: Instant,
    last_total: u64,
    smoothed_bytes_per_sec: f64,
    double_smoothed_bytes_per_sec: f64,
}

impl Default for TransferRate {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            start_time: now,
            last_time: now,
            last_total: 0,
            smoothed_bytes_per_sec: 0.0,
            double_smoothed_bytes_per_sec: 0.0,
        }
    }
}

impl TransferRate {
    pub(crate) fn reset(&mut self, total: u64, now: Instant) {
        self.start_time = now;
        self.last_time = now;
        self.last_total = total;
        self.smoothed_bytes_per_sec = 0.0;
        self.double_smoothed_bytes_per_sec = 0.0;
    }

    pub(crate) fn record(&mut self, total: u64, now: Instant) {
        if total < self.last_total {
            self.reset(total, now);
            return;
        }
        if total == self.last_total || now <= self.last_time {
            return;
        }

        let delta_bytes = total - self.last_total;
        let delta_secs = now.duration_since(self.last_time).as_secs_f64();
        let instant_bytes_per_sec = delta_bytes as f64 / delta_secs;
        let weight = throughput_weight(now.duration_since(self.last_time));

        self.smoothed_bytes_per_sec = self
            .smoothed_bytes_per_sec
            .mul_add(weight, instant_bytes_per_sec * (1.0 - weight));

        let total_weight = 1.0 - throughput_weight(now.duration_since(self.start_time));
        if total_weight > f64::EPSILON {
            let normalized = self.smoothed_bytes_per_sec / total_weight;
            self.double_smoothed_bytes_per_sec = self
                .double_smoothed_bytes_per_sec
                .mul_add(weight, normalized * (1.0 - weight));
        }

        self.last_total = total;
        self.last_time = now;
    }

    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub(crate) fn bytes_per_sec(&self, now: Instant) -> u64 {
        let sample_span = now.duration_since(self.start_time);
        if sample_span < MIN_RATE_SAMPLE_SPAN {
            return 0;
        }

        let total_weight = 1.0 - throughput_weight(sample_span);
        if total_weight <= f64::EPSILON {
            return 0;
        }

        let age = now.duration_since(self.last_time);
        let reweight = throughput_weight(age);
        let single_smoothed = self.smoothed_bytes_per_sec * reweight / total_weight;
        let double_smoothed = self
            .double_smoothed_bytes_per_sec
            .mul_add(reweight, single_smoothed * (1.0 - reweight));
        let bytes_per_sec = double_smoothed / total_weight;
        if bytes_per_sec < 1.0 || !bytes_per_sec.is_finite() {
            return 0;
        }

        if bytes_per_sec >= u64::MAX as f64 {
            u64::MAX
        } else {
            bytes_per_sec as u64
        }
    }
}

#[allow(clippy::cast_precision_loss)]
fn throughput_weight(elapsed: Duration) -> f64 {
    0.1_f64.powf(elapsed.as_secs_f64() / THROUGHPUT_DECAY.as_secs_f64())
}

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
    pub speed: u64,
    #[serde(skip)]
    pub(crate) rate: TransferRate,
    #[serde(skip)]
    pub source_url: Option<String>,
    #[serde(skip)]
    pub(crate) counts_toward_progress: bool,
    pub status: FileStatus,
}

impl FileEntry {
    pub(crate) fn reset_rate(&mut self) {
        self.speed = 0;
        self.rate.reset(self.downloaded, Instant::now());
    }

    pub(crate) fn record_progress(&mut self, bytes_delta: u64, now: Instant) -> u64 {
        let next = self.downloaded.saturating_add(bytes_delta);
        let next = if self.size > 0 {
            next.min(self.size)
        } else {
            next
        };
        let accepted = next.saturating_sub(self.downloaded);
        self.downloaded = next;
        self.rate.record(self.downloaded, now);
        self.speed = self.rate.bytes_per_sec(now);
        accepted
    }
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
    fn project_core_file(
        file: &FileState,
        package: Option<&PackageState>,
        existing: Option<FileEntry>,
    ) -> Option<FileEntry> {
        let status = match file.lifecycle {
            FileLifecycle::Planned | FileLifecycle::Queued => FileStatus::Queued,
            FileLifecycle::Downloading => FileStatus::Downloading,
            FileLifecycle::Complete => FileStatus::Complete,
            FileLifecycle::Failed => {
                FileStatus::Error(file.message.clone().unwrap_or_else(|| "failed".to_string()))
            }
            FileLifecycle::Skipped | FileLifecycle::Deleted => return None,
        };

        let downloaded = match file.lifecycle {
            FileLifecycle::Complete => file.size,
            _ => file.progress.visible_completed_bytes.min(file.size),
        };
        let source_url = package.map(|package| package.source_url.clone());
        let counts_toward_progress =
            file.runtime.counts_in_run_totals && !file.runtime.preexisting_complete;
        let now = Instant::now();

        if let Some(mut existing) = existing {
            existing.name = file.path.clone();
            existing.size = file.size;
            existing.downloaded = downloaded;
            existing.source_url = source_url;
            existing.counts_toward_progress = counts_toward_progress;
            if matches!(status, FileStatus::Downloading) {
                if !matches!(existing.status, FileStatus::Downloading)
                    || existing.downloaded > downloaded
                {
                    existing.rate.reset(downloaded, now);
                    existing.speed = 0;
                }
            } else {
                existing.speed = 0;
            }
            existing.status = status;
            return Some(existing);
        }

        let mut rate = TransferRate::default();
        if matches!(status, FileStatus::Downloading) {
            rate.reset(downloaded, now);
        }
        Some(FileEntry {
            id: file.id.clone(),
            name: file.path.clone(),
            size: file.size,
            downloaded,
            speed: 0,
            rate,
            source_url,
            counts_toward_progress,
            status,
        })
    }

    fn snapshot_packages(&self) -> Vec<serde_json::Value> {
        if !self.core_state.packages.is_empty() {
            return self
                .core_state
                .packages
                .values()
                .map(|package| {
                    serde_json::json!({
                        "id": package.id,
                        "source_url": package.source_url,
                        "display_name": package.display_name,
                        "status": package.status,
                        "file_ids": package.file_ids,
                    })
                })
                .collect();
        }
        let mut packages = IndexMap::<String, Vec<&FileEntry>>::new();
        for file in &self.files {
            let package_id = file
                .source_url
                .clone()
                .unwrap_or_else(|| file.id.clone());
            packages.entry(package_id).or_default().push(file);
        }

        packages
            .into_iter()
            .map(|(package_id, files)| {
                let status = if files
                    .iter()
                    .any(|file| matches!(file.status, FileStatus::Error(_)))
                {
                    "failed"
                } else if files
                    .iter()
                    .any(|file| matches!(file.status, FileStatus::Downloading))
                {
                    "downloading"
                } else if files.iter().all(|file| matches!(file.status, FileStatus::Complete)) {
                    "complete"
                } else if files.iter().any(|file| matches!(file.status, FileStatus::Complete)) {
                    "partial"
                } else {
                    "queued"
                };
                serde_json::json!({
                    "id": package_id,
                    "source_url": files[0].source_url.clone().unwrap_or_else(|| files[0].id.clone()),
                    "display_name": files[0].source_url.clone().unwrap_or_else(|| files[0].name.clone()),
                    "status": status,
                    "file_ids": files.iter().map(|file| file.id.clone()).collect::<Vec<_>>(),
                })
            })
            .collect()
    }

    pub fn sorted_file_indices(&self) -> Vec<usize> {
        let mut indices: Vec<_> = (0..self.files.len()).collect();
        indices.sort_by_key(|&i| match &self.files[i].status {
            FileStatus::Downloading => 0,
            FileStatus::Queued => 1,
            FileStatus::Complete => 2,
            FileStatus::Error(_) => 3,
        });
        indices
    }

    pub fn selected_file_index(&self) -> Option<usize> {
        let selected = self.file_list_state.selected()?;
        self.sorted_file_indices().get(selected).copied()
    }

    pub(crate) fn sync_visible_files_from_core(&mut self) {
        let selected_id = self.selected_file_index().map(|index| self.files[index].id.clone());
        let selected_row = self.file_list_state.selected().unwrap_or(0);
        let core_file_ids: HashSet<_> = self.core_state.files.keys().cloned().collect();
        let existing: IndexMap<_, _> = std::mem::take(&mut self.files)
            .into_iter()
            .map(|file| (file.id.clone(), file))
            .collect();

        let mut existing = existing;
        let mut files = Vec::new();
        for file in self.core_state.files.values() {
            let package = self.core_state.packages.get(&file.package_id);
            let existing = existing.shift_remove(&file.id);
            if let Some(entry) = Self::project_core_file(file, package, existing) {
                files.push(entry);
            }
        }

        for (id, entry) in existing {
            if !core_file_ids.contains(&id) {
                files.push(entry);
            }
        }

        self.files = files;
        if let Some(selected_id) = selected_id {
            if let Some(display_row) = self
                .sorted_file_indices()
                .into_iter()
                .position(|index| self.files[index].id == selected_id)
            {
                self.file_list_state.select(Some(display_row));
                return;
            }
        }

        if self.files.is_empty() {
            self.file_list_state.select(None);
        } else {
            self.file_list_state
                .select(Some(selected_row.min(self.files.len() - 1)));
        }
    }

    fn update_speeds_at(&mut self, now: Instant) {
        self.last_tick = now;

        for f in &mut self.files {
            if matches!(f.status, FileStatus::Downloading) {
                f.speed = f.rate.bytes_per_sec(now);
            } else {
                f.speed = 0;
            }
        }

        self.current_speed = if self
            .files
            .iter()
            .any(|f| matches!(f.status, FileStatus::Downloading))
        {
            self.aggregate_rate
                .record(self.total_network_downloaded, now);
            self.aggregate_rate.bytes_per_sec(now)
        } else {
            0
        };
    }

    /// Computes per-file transfer rates from timestamped cumulative samples.
    pub fn update_speeds(&mut self) {
        self.update_speeds_at(Instant::now());
    }

    /// Recomputes aggregate totals from the current files list.
    ///
    /// Call after deleting files to keep counters consistent.
    pub fn recompute_totals(&mut self) {
        self.total_size = self.core_state.totals.run_total_bytes;
        self.total_downloaded = self.core_state.totals.run_completed_bytes;
        self.files_completed = self.core_state.totals.run_file_completed;
        self.files_total = self.core_state.totals.run_file_total;
        self.total_network_downloaded = self.core_state.totals.displayed_network_bytes;

        for file in &self.files {
            if self.core_state.files.contains_key(&file.id) || !file.counts_toward_progress {
                continue;
            }
            self.total_size = self.total_size.saturating_add(file.size);
            self.total_downloaded = self.total_downloaded.saturating_add(file.downloaded);
            self.total_network_downloaded =
                self.total_network_downloaded.saturating_add(file.downloaded);
            if matches!(file.status, FileStatus::Complete) {
                self.files_completed = self.files_completed.saturating_add(1);
            }
            if !matches!(file.status, FileStatus::Error(_)) {
                self.files_total = self.files_total.saturating_add(1);
            }
        }

        self.current_speed = 0;
        self.aggregate_rate
            .reset(self.total_network_downloaded, Instant::now());
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

    pub fn find_file_mut(&mut self, id: &str) -> Option<&mut FileEntry> {
        self.files.iter_mut().find(|f| f.id == id)
    }

    pub(crate) fn seed_core_session_from_session(&mut self) {
        if let Some(session) = self.session.as_ref() {
            self.core_state.session_meta = SessionMeta {
                session_id: session.id.clone(),
                created: session.created,
                status: match session.status {
                    crate::SessionStatus::InProgress => crate::core::SessionRunStatus::InProgress,
                    crate::SessionStatus::Completed => crate::core::SessionRunStatus::Completed,
                    crate::SessionStatus::Paused => crate::core::SessionRunStatus::Paused,
                },
                config: session.config.clone(),
                credentials: crate::core::SavedCredentials {
                    email: session.credentials.email.clone(),
                    password: session.credentials.password.clone(),
                    mfa: session.credentials.mfa.clone(),
                },
            };
        } else {
            self.core_state.session_meta.config = self.config.config.clone();
        }
    }

    pub(crate) fn apply_core_event(&mut self, event: CoreEvent) {
        self.seed_core_session_from_session();
        let effects = reduce(&mut self.core_state, event);
        self.apply_core_effects(effects);
        self.sync_visible_files_from_core();
        self.recompute_totals();
    }

    fn apply_core_effects(&mut self, effects: Vec<CoreEffect>) {
        for effect in effects {
            match effect {
                CoreEffect::PersistSession(snapshot) => {
                    if let Some(ref mut session) = self.session {
                        let next = SessionState::from_v3(snapshot);
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

    pub(crate) fn reset_aggregate_rate(&mut self) {
        self.current_speed = 0;
        self.aggregate_rate
            .reset(self.total_network_downloaded, Instant::now());
    }

    pub(crate) fn record_total_progress(
        &mut self,
        bytes_delta: u64,
        network_bytes_delta: u64,
        now: Instant,
    ) {
        self.total_downloaded = self.total_downloaded.saturating_add(bytes_delta);
        self.total_network_downloaded = self
            .total_network_downloaded
            .saturating_add(network_bytes_delta);
        let _ = now;
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        let _ = self.pause_tx.send(paused);
    }

    pub fn pause_downloads(&mut self) {
        if self.paused {
            return;
        }
        self.set_paused(true);
        for token in self.cancellation_tokens.values() {
            token.cancel();
        }
        for file in &mut self.files {
            if matches!(file.status, FileStatus::Downloading) {
                file.status = FileStatus::Queued;
                file.reset_rate();
            }
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
        #[derive(Serialize)]
        struct RunTotals {
            run_total_bytes: u64,
            run_completed_bytes: u64,
            run_file_total: usize,
            run_file_completed: usize,
            displayed_network_rate_bps: u64,
        }

        #[derive(Serialize)]
        struct Snapshot<'a> {
            authenticated: bool,
            paused: bool,
            logging_in: bool,
            login_error: Option<&'a str>,
            popup: Popup,
            packages: Vec<serde_json::Value>,
            files: &'a [FileEntry],
            total_downloaded: u64,
            total_size: u64,
            files_completed: usize,
            files_total: usize,
            current_speed: u64,
            displayed_network_rate_bps: u64,
            run_totals: RunTotals,
            cpu_usage: f32,
            memory_rss: u64,
            api_port: u16,
            config: &'a DownloadConfig,
        }

        let run_totals = if !self.core_state.files.is_empty() {
            RunTotals {
                run_total_bytes: self.core_state.totals.run_total_bytes,
                run_completed_bytes: self.core_state.totals.run_completed_bytes,
                run_file_total: self.core_state.totals.run_file_total,
                run_file_completed: self.core_state.totals.run_file_completed,
                displayed_network_rate_bps: self.current_speed,
            }
        } else {
            RunTotals {
                run_total_bytes: self.total_size,
                run_completed_bytes: self.total_downloaded,
                run_file_total: self.files_total,
                run_file_completed: self.files_completed,
                displayed_network_rate_bps: self.current_speed,
            }
        };

        let snap = Snapshot {
            authenticated: self.authenticated,
            paused: self.paused,
            logging_in: self.login.logging_in,
            login_error: self.login.error.as_deref(),
            popup: self.popup,
            packages: self.snapshot_packages(),
            files: &self.files,
            total_downloaded: self.total_downloaded,
            total_size: self.total_size,
            files_completed: self.files_completed,
            files_total: self.files_total,
            current_speed: self.current_speed,
            displayed_network_rate_bps: self.current_speed,
            run_totals,
            cpu_usage: self.cpu_usage,
            memory_rss: self.memory_rss,
            api_port: self.api_port,
            config: &self.config.config,
        };
        serde_json::to_string(&snap).unwrap_or_default()
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
            speed: 32,
            rate: Default::default(),
            source_url: Some("https://mega.nz/file/abc".to_string()),
            counts_toward_progress: true,
            status: FileStatus::Downloading,
        });
        app.cpu_usage = 12.5;
        app.memory_rss = 4096;
        app.recompute_totals();

        let snapshot: serde_json::Value =
            serde_json::from_str(&app.to_json()).expect("snapshot should be valid JSON");
        let file = &snapshot["files"][0];

        assert_eq!(file["id"], "stable/file.bin");
        assert_eq!(file["status"], "downloading");
        assert_eq!(snapshot["packages"][0]["source_url"], "https://mega.nz/file/abc");
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
            speed: 0,
            rate: Default::default(),
            source_url: None,
            counts_toward_progress: true,
            status: FileStatus::Downloading,
        });
        app.total_downloaded = 1_000;
        app.total_network_downloaded = 1_000;
        app.aggregate_rate.reset(1_000, start);

        app.record_total_progress(100, 100, start + Duration::from_secs(1));
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
            speed: 0,
            rate: Default::default(),
            source_url: None,
            counts_toward_progress: true,
            status: FileStatus::Downloading,
        });
        app.total_downloaded = 1_000;
        app.aggregate_rate.reset(0, start);

        app.record_total_progress(1_000, 0, start + Duration::from_secs(1));
        app.update_speeds_at(start + Duration::from_secs(1));

        assert_eq!(app.current_speed, 0);
        assert_eq!(app.total_downloaded, 2_000);
    }

    #[test]
    fn record_progress_caps_downloaded_at_file_size() {
        let mut file = FileEntry {
            id: "file.bin".to_string(),
            name: "file.bin".to_string(),
            size: 100,
            downloaded: 90,
            speed: 0,
            rate: Default::default(),
            source_url: None,
            counts_toward_progress: true,
            status: FileStatus::Downloading,
        };

        let accepted = file.record_progress(25, Instant::now());

        assert_eq!(accepted, 10);
        assert_eq!(file.downloaded, 100);
    }
}
