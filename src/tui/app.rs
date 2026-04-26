//! Application state model.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::env;
use std::future::Future;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use ratatui::widgets::ListState;
use serde::Serialize;
use sysinfo::{ProcessesToUpdate, System};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::{
    DownloadConfig, FileEntry as SessionFileEntry, FileEntryStatus, ServiceConfig, SessionState,
    SessionStatus, UrlEntry, UrlStatus,
    core::{
        CoreEffect, CoreEvent, DownloadState, FileLifecycle, FileState, PackageState, ResolvedFile,
        ResolvedPackage, RestartSnapshot, SessionMeta, reconcile_restart, reduce, scan_filesystem,
    },
    file_key, format_bytes,
};

use super::WebOptions;
use super::api;
use super::download;
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
    fn seed_overlay_from_visible(&mut self) {
        for file in &self.files {
            if !self.core_state.files.contains_key(&file.id)
                && !self.deleted_files.contains(&file.id)
            {
                self.overlay_files
                    .entry(file.id.clone())
                    .or_insert_with(|| file.clone());
            }
        }
    }

    pub(crate) fn file_speed(&self, file_id: &str) -> u64 {
        self.file_ui.get(file_id).map_or(0, |state| state.speed)
    }

    fn ensure_file_ui(&mut self, file_id: &str, downloaded: u64, reset: bool) {
        let state = self.file_ui.entry(file_id.to_string()).or_default();
        if reset {
            state.speed = 0;
            state.rate.reset(downloaded, Instant::now());
        }
    }

    pub(crate) fn reset_file_ui_rate(&mut self, file_id: &str) {
        let downloaded = self
            .files
            .iter()
            .find(|file| file.id == file_id)
            .map_or(0, |file| file.downloaded);
        self.ensure_file_ui(file_id, downloaded, true);
    }

    pub(crate) fn update_file_ui_progress(
        &mut self,
        file_id: &str,
        previous_downloaded: u64,
        now: Instant,
    ) -> u64 {
        let downloaded = self
            .files
            .iter()
            .find(|file| file.id == file_id)
            .map_or(previous_downloaded, |file| file.downloaded);
        let accepted = downloaded.saturating_sub(previous_downloaded);
        let state = self.file_ui.entry(file_id.to_string()).or_default();
        state.rate.record(downloaded, now);
        state.speed = state.rate.bytes_per_sec(now);
        accepted
    }

    fn package_sort_key_for(&self, file: &FileEntry) -> (usize, String) {
        if let Some(core_file) = self.core_state.files.get(&file.id) {
            let package_order = self
                .core_state
                .packages
                .get_index_of(&core_file.package_id)
                .unwrap_or(usize::MAX);
            let display_name = self
                .core_state
                .packages
                .get(&core_file.package_id)
                .map(|package| package.display_name.clone())
                .unwrap_or_else(|| core_file.package_id.clone());
            return (package_order, display_name);
        }

        (
            usize::MAX,
            file.source_url.clone().unwrap_or_else(|| file.id.clone()),
        )
    }

    pub(crate) fn package_label_for_file(&self, file_id: &str) -> Option<String> {
        if let Some(core_file) = self.core_state.files.get(file_id) {
            return self
                .core_state
                .packages
                .get(&core_file.package_id)
                .map(|package| package.display_name.clone());
        }

        self.files
            .iter()
            .find(|file| file.id == file_id)
            .and_then(|file| file.source_url.clone())
            .or_else(|| {
                self.overlay_files
                    .get(file_id)
                    .and_then(|file| file.source_url.clone())
            })
    }

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
        if let Some(mut existing) = existing {
            existing.name = file.path.clone();
            existing.size = file.size;
            existing.downloaded = downloaded;
            existing.source_url = source_url;
            existing.counts_toward_progress = counts_toward_progress;
            existing.status = status;
            return Some(existing);
        }

        Some(FileEntry {
            id: file.id.clone(),
            name: file.path.clone(),
            size: file.size,
            downloaded,
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
            let package_id = file.source_url.clone().unwrap_or_else(|| file.id.clone());
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
        indices.sort_by(|&left, &right| {
            let left_file = &self.files[left];
            let right_file = &self.files[right];
            let left_package = self.package_sort_key_for(left_file);
            let right_package = self.package_sort_key_for(right_file);

            match left_package.cmp(&right_package) {
                Ordering::Equal => {}
                other => return other,
            }

            let left_rank = match &left_file.status {
                FileStatus::Downloading => 0,
                FileStatus::Queued => 1,
                FileStatus::Complete => 2,
                FileStatus::Error(_) => 3,
            };
            let right_rank = match &right_file.status {
                FileStatus::Downloading => 0,
                FileStatus::Queued => 1,
                FileStatus::Complete => 2,
                FileStatus::Error(_) => 3,
            };
            left_rank
                .cmp(&right_rank)
                .then_with(|| left_file.name.cmp(&right_file.name))
                .then_with(|| left_file.id.cmp(&right_file.id))
        });
        indices
    }

    pub fn selected_file_index(&self) -> Option<usize> {
        let selected = self.file_list_state.selected()?;
        self.sorted_file_indices().get(selected).copied()
    }

    pub(crate) fn sync_visible_files(&mut self) {
        let selected_id = self
            .selected_file_index()
            .map(|index| self.files[index].id.clone());
        let selected_row = self.file_list_state.selected().unwrap_or(0);
        let core_file_ids: HashSet<_> = self.core_state.files.keys().cloned().collect();
        let existing: IndexMap<_, _> = std::mem::take(&mut self.files)
            .into_iter()
            .map(|file| (file.id.clone(), file))
            .collect();

        let mut existing = existing;
        for (id, file) in &existing {
            if !core_file_ids.contains(id) && !self.deleted_files.contains(id) {
                self.overlay_files
                    .entry(id.clone())
                    .or_insert_with(|| file.clone());
            }
        }
        let mut files = Vec::new();
        for file in self.core_state.files.values() {
            let package = self.core_state.packages.get(&file.package_id);
            let existing = existing.shift_remove(&file.id);
            if let Some(entry) = Self::project_core_file(file, package, existing) {
                files.push(entry);
            }
        }

        for (id, entry) in &self.overlay_files {
            if !core_file_ids.contains(id) && !self.deleted_files.contains(id) {
                files.push(entry.clone());
            }
        }

        self.files = files;
        let visible_ids: HashSet<_> = self.files.iter().map(|file| file.id.clone()).collect();
        self.file_ui
            .retain(|file_id, _| visible_ids.contains(file_id));
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

    pub(crate) fn upsert_overlay_file(&mut self, file: FileEntry) {
        self.overlay_files.insert(file.id.clone(), file);
        self.sync_visible_files();
    }

    pub(crate) fn overlay_file_mut(&mut self, id: &str) -> Option<&mut FileEntry> {
        if !self.overlay_files.contains_key(id) {
            self.seed_overlay_from_visible();
        }
        self.overlay_files.get_mut(id)
    }

    pub(crate) fn remove_overlay_file(&mut self, id: &str) -> Option<FileEntry> {
        if !self.overlay_files.contains_key(id) {
            self.seed_overlay_from_visible();
        }
        let removed = self.overlay_files.shift_remove(id);
        self.sync_visible_files();
        removed
    }

    pub(crate) fn drop_overlay_file(&mut self, id: &str) -> Option<FileEntry> {
        self.deleted_files.insert(id.to_string());
        let removed = self.overlay_files.shift_remove(id);
        self.sync_visible_files();
        self.deleted_files.remove(id);
        removed
    }

    fn update_speeds_at(&mut self, now: Instant) {
        self.last_tick = now;

        for file in &self.files {
            if let Some(state) = self.file_ui.get_mut(&file.id) {
                if matches!(file.status, FileStatus::Downloading) {
                    state.speed = state.rate.bytes_per_sec(now);
                } else {
                    state.speed = 0;
                }
            } else {
                self.file_ui.entry(file.id.clone()).or_default();
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
            self.total_network_downloaded = self
                .total_network_downloaded
                .saturating_add(file.downloaded);
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

    pub(crate) fn new_with_optional_service_config(
        event_tx: mpsc::UnboundedSender<DownloadEvent>,
        quit_enabled: bool,
        config_path: Option<&Path>,
        default_api_port: u16,
    ) -> io::Result<(Self, String, u16)> {
        if let Some(path) = config_path {
            let mut app = Self::new(0, event_tx, quit_enabled);
            let (host, port) = app.apply_service_config(path)?;
            app.api_port = port;
            return Ok((app, host, port));
        }

        let api_port = env::var("OCTO_API_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(default_api_port);
        Ok((
            Self::new(api_port, event_tx, quit_enabled),
            "127.0.0.1".to_string(),
            api_port,
        ))
    }

    pub(crate) fn require_credentials(&self, config_path: &Path) -> io::Result<()> {
        if self.login.has_credentials() {
            return Ok(());
        }

        log::error!(
            "No credentials configured. Edit {} and set email/password under [credentials], then restart.",
            config_path.display()
        );
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("No credentials in {}", config_path.display()),
        ))
    }

    pub(crate) fn prepare_interactive_startup(&mut self) {
        self.resume_latest_session();
        self.load_credentials_from_env();
        self.auto_login(NoCredentialsFallback::ShowPopup);
    }

    pub(crate) fn prepare_headless_startup(&mut self, config_path: &Path) -> io::Result<()> {
        self.load_credentials_from_env();
        self.require_credentials(config_path)?;
        self.resume_latest_session();
        self.auto_login(NoCredentialsFallback::Silent);
        Ok(())
    }

    pub(crate) fn shared_state_channels(&self, enabled: bool) -> SharedStateChannels {
        let (action_tx, action_rx) = mpsc::unbounded_channel::<UiAction>();
        let (state_tx, state_rx) = watch::channel(self.to_json());
        let shared_state = enabled.then_some(SharedAppState {
            action_tx,
            state_rx,
        });
        SharedStateChannels {
            action_rx,
            state_tx,
            shared_state,
        }
    }

    pub(crate) fn spawn_api_server(
        &self,
        host: String,
        port: u16,
        web_opts: Option<WebOptions>,
        shared_state: Option<SharedAppState>,
    ) {
        let api_tx = self.event_tx.clone();
        let api_key = self.api_key.clone();
        tokio::spawn(async move {
            if let Err(e) = api::run_api_server(
                api_tx,
                &host,
                port,
                web_opts.as_ref(),
                shared_state,
                api_key,
            )
            .await
            {
                log::error!("API server error: {e}");
            }
        });
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
        self.sync_visible_files();
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
                            if let Some(existing) = merged_files.iter_mut().find(|file| {
                                file.path == next_file.path || file.key == next_file.key
                            }) {
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

    pub(crate) fn submit_url(&mut self, url: String) {
        if self.urls.contains(&url) {
            return;
        }
        self.urls.push(url.clone());
        self.apply_core_event(CoreEvent::UrlSubmitted { url: url.clone() });
        self.session_add_pending_url(&url);
        let _ = self.url_tx.send(url);
    }

    pub(crate) fn begin_login(&mut self) {
        self.login.error = None;
        self.login.logging_in = true;
        self.status = "Logging in...".to_string();
        download::start_login(self);
    }

    pub(crate) fn auto_login(&mut self, fallback: NoCredentialsFallback) -> bool {
        if self.login.has_credentials() {
            self.begin_login();
            true
        } else {
            if fallback == NoCredentialsFallback::ShowPopup {
                self.popup = Popup::Login;
            }
            false
        }
    }

    pub(crate) fn load_credentials_from_env(&mut self) {
        let email = env::var("MEGA_EMAIL").unwrap_or_default();
        let password = env::var("MEGA_PASSWORD").unwrap_or_default();
        let mfa = env::var("MEGA_MFA").unwrap_or_default();
        if !email.is_empty() || !password.is_empty() {
            log::info!("Using MEGA credentials from environment variables");
        }
        self.login
            .set_credentials_if_missing(&email, &password, &mfa);
    }

    pub(crate) fn apply_service_config(&mut self, config_path: &Path) -> io::Result<(String, u16)> {
        let mut service_config = ServiceConfig::load_or_create(config_path)?;
        log::info!("Loaded config from {}", config_path.display());

        if let Some(ref dl_path) = service_config.download.path {
            let download_dir = Path::new(dl_path);
            if !download_dir.exists() {
                std::fs::create_dir_all(download_dir)?;
            }
            std::env::set_current_dir(download_dir)?;
            log::info!("Download directory: {dl_path}");
        }

        self.config.config = service_config.download.clone();
        self.api_key.clone_from(&service_config.api.api_key);

        let mut credentials_from_config = false;
        if service_config.credentials.has_credentials() {
            if let Some((email, password, mfa)) = service_config.credentials.decrypt_if_needed() {
                log::info!("Loaded credentials from config file");
                credentials_from_config = self.login.set_credentials(email, password, mfa);

                if !service_config.credentials.encrypted {
                    log::info!("Encrypting plaintext credentials in config file");
                    service_config.credentials.encrypt_in_place();
                    service_config.save(config_path)?;
                }
            } else {
                log::warn!(
                    "Failed to decrypt credentials from config (machine key mismatch?). Falling back to environment variables."
                );
            }
        }

        if !credentials_from_config {
            if let (Ok(email), Ok(password)) = (env::var("MEGA_EMAIL"), env::var("MEGA_PASSWORD")) {
                log::info!(
                    "Using credentials from MEGA_EMAIL and MEGA_PASSWORD environment variables"
                );
                self.login.set_credentials(
                    email,
                    password,
                    env::var("MEGA_MFA").unwrap_or_default(),
                );
            } else if service_config.credentials.has_credentials() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Failed to decrypt credentials from config file. Set MEGA_EMAIL and MEGA_PASSWORD environment variables, or re-create the config file as the current user.",
                ));
            }
        }

        if service_config.api.api_key.is_none() {
            let key = uuid::Uuid::new_v4().simple().to_string();
            log::info!("Generated API key: {key}");
            service_config.api.api_key = Some(key);
            service_config.save(config_path)?;
            self.api_key.clone_from(&service_config.api.api_key);
        }

        Ok((service_config.api.host, service_config.api.port))
    }

    pub(crate) fn session_add_pending_url(&mut self, url: &str) {
        if let Some(ref mut session) = self.session
            && !session.urls.iter().any(|entry| entry.url == url)
        {
            session.urls.push(UrlEntry {
                url: url.to_string(),
                status: UrlStatus::Pending,
            });
            let _ = session.save();
        }
    }

    pub(crate) fn session_mark_file_complete(&mut self, file_id: &str) {
        if let Some(ref mut session) = self.session {
            let _ = session.mark_file_complete(file_id);
        }
    }

    pub(crate) fn session_mark_file_error(&mut self, file_id: &str, error: &str) {
        if let Some(ref mut session) = self.session {
            let _ = session.mark_file_error(file_id, error);
        }
    }

    pub(crate) fn session_mark_file_skipped(&mut self, file_id: &str) {
        if let Some(ref mut session) = self.session {
            let _ = session.mark_file_skipped(file_id);
        }
    }

    pub(crate) fn session_mark_completed(&mut self) {
        if let Some(ref mut session) = self.session {
            let _ = session.mark_completed();
        }
    }

    pub(crate) fn session_set_url_status(&mut self, url: &str, status: UrlStatus) {
        if let Some(ref mut session) = self.session {
            if let Some(entry) = session.urls.iter_mut().find(|entry| entry.url == url) {
                entry.status = status;
            }
            let _ = session.save();
        }
    }

    pub(crate) fn session_has_skipped_file(&self, session_url: &str, path: &str) -> bool {
        self.session.as_ref().is_some_and(|session| {
            session
                .urls
                .iter()
                .position(|entry| entry.url == session_url)
                .is_some_and(|url_index| {
                    session.files.iter().any(|file| {
                        file.url_index == url_index
                            && file.path == path
                            && matches!(file.status, FileEntryStatus::Skipped)
                    })
                })
        })
    }

    pub(crate) fn session_register_queued_file(
        &mut self,
        session_url: &str,
        path: &str,
        size: u64,
    ) {
        if let Some(ref mut session) = self.session {
            let url_index = session
                .urls
                .iter()
                .position(|entry| entry.url == session_url)
                .unwrap_or(0);
            let stable_key = file_key(url_index, path);

            if let Some(file) = session
                .files
                .iter_mut()
                .find(|file| file.url_index == url_index && file.path == path)
            {
                if file.key.is_none() {
                    file.key = Some(stable_key);
                    let _ = session.save();
                }
            } else {
                session.files.push(SessionFileEntry {
                    key: Some(stable_key),
                    url_index,
                    path: path.to_string(),
                    size,
                    status: FileEntryStatus::Pending,
                });
                let _ = session.save();
            }
        }
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

        let resumed_urls = restart.resumable_urls();
        let resumed_url_set: HashSet<_> = resumed_urls.iter().cloned().collect();
        for entry in &mut session.urls {
            if !matches!(entry.status, UrlStatus::Error(_)) {
                entry.status = if resumed_url_set.contains(&entry.url) {
                    UrlStatus::Pending
                } else {
                    UrlStatus::Fetched
                };
            }
        }
        session.files = restart
            .state
            .files
            .values()
            .map(|file| {
                let url_index = session
                    .urls
                    .iter()
                    .position(|entry| {
                        restart
                            .state
                            .packages
                            .get(&file.package_id)
                            .is_some_and(|package| package.source_url == entry.url)
                    })
                    .unwrap_or(0);
                SessionFileEntry {
                    key: Some(file.id.clone()),
                    url_index,
                    path: file.path.clone(),
                    size: file.size,
                    status: match file.lifecycle {
                        FileLifecycle::Planned | FileLifecycle::Queued => FileEntryStatus::Pending,
                        FileLifecycle::Downloading => FileEntryStatus::Downloading,
                        FileLifecycle::Complete => FileEntryStatus::Completed,
                        FileLifecycle::Skipped | FileLifecycle::Deleted => FileEntryStatus::Skipped,
                        FileLifecycle::Failed => FileEntryStatus::Error(
                            file.message.clone().unwrap_or_else(|| "failed".to_string()),
                        ),
                    },
                }
            })
            .collect();
        let _ = session.save();
        self.urls.clone_from(&resumed_urls);
        for url in resumed_urls {
            let _ = self.url_tx.send(url);
        }
        self.session = Some(session);
        self.seed_core_session_from_session();
    }

    pub(crate) fn sync_session_for_shutdown(&mut self) {
        let Some(ref mut session) = self.session else {
            return;
        };
        if session.status == SessionStatus::Completed {
            return;
        }

        let visible: HashSet<&str> = self.files.iter().map(|file| file.id.as_str()).collect();

        session.files.retain(|file| {
            matches!(file.status, FileEntryStatus::Skipped)
                || visible.contains(file.path.as_str())
                || visible.contains(file.key_or_path())
        });

        if session.files.is_empty() {
            let _ = session.mark_completed();
        } else {
            log::info!("Marking session as paused for later resume");
            let _ = session.mark_paused();
        }
    }

    pub(crate) fn visible_file_context(&self, id: &str) -> Option<VisibleFileContext> {
        self.files.iter().find(|file| file.id == id).map(|file| {
            let source_url = file.source_url.clone();
            VisibleFileContext {
                id: file.id.clone(),
                artifact_path: file.name.clone(),
                size: file.size,
                counts_toward_progress: file.counts_toward_progress,
                is_core_backed: self.core_state.files.contains_key(id) || source_url.is_some(),
                source_url,
            }
        })
    }

    pub(crate) fn mark_visible_file_complete(&mut self, id: &str, name: &str) {
        self.cancellation_tokens.remove(id);
        if !self.core_state.files.contains_key(id)
            && let Some(file) = self.overlay_file_mut(id)
        {
            file.name = name.to_string();
            file.status = FileStatus::Complete;
            file.downloaded = file.size;
            self.sync_visible_files();
        }
        self.reset_file_ui_rate(id);
        self.session_mark_file_complete(id);

        self.recompute_totals();
        if self.files_completed == self.files_total && self.files_total > 0 {
            self.session_mark_completed();
            self.status = "All downloads complete".to_string();
        } else {
            self.status = format!(
                "Downloading ({}/{})",
                self.files_completed, self.files_total
            );
        }
    }

    pub(crate) fn show_overlay_error(
        &mut self,
        id: &str,
        name: &str,
        error: &str,
        counts_toward_progress: bool,
    ) {
        self.cancellation_tokens.remove(id);
        if self.core_state.files.contains_key(id) {
            // Core-backed rows are projected back into the TUI view.
        } else if let Some(file) = self.overlay_file_mut(id) {
            file.status = FileStatus::Error(error.to_string());
            file.name = name.to_string();
            self.sync_visible_files();
        } else {
            self.upsert_overlay_file(FileEntry {
                id: id.to_string(),
                name: name.to_string(),
                size: 0,
                downloaded: 0,
                source_url: None,
                counts_toward_progress,
                status: FileStatus::Error(error.to_string()),
            });
        }
        self.reset_file_ui_rate(id);
    }

    pub(crate) fn mark_visible_file_error(&mut self, id: &str, name: &str, error: &str) {
        self.show_overlay_error(id, name, error, true);
        self.session_mark_file_error(id, error);
    }

    pub(crate) fn show_ui_error_only(&mut self, name: &str, error: &str) {
        self.show_overlay_error(name, name, error, false);
    }

    pub(crate) fn perform_delete_file_action(&mut self, id: &str) {
        let context = self.visible_file_context(id);
        let is_core_backed = context
            .as_ref()
            .is_some_and(|context| context.is_core_backed);
        let artifact_path = context
            .as_ref()
            .map_or_else(|| id.to_string(), |context| context.artifact_path.clone());
        if let Some(context) = context.as_ref()
            && let Some(source_url) = context.source_url.as_ref()
        {
            self.ensure_core_file(
                &context.id,
                source_url,
                &context.artifact_path,
                context.size,
                context.counts_toward_progress,
            );
        }
        if let Some(token) = self.cancellation_tokens.remove(id) {
            token.cancel();
        }
        self.deleted_files.insert(id.to_string());
        if is_core_backed {
            self.apply_core_event(CoreEvent::FileDeleted {
                file_id: id.to_string(),
            });
        } else {
            let _ = self.remove_overlay_file(id);
        }
        download::schedule_download_artifact_delete(artifact_path);
        self.session_mark_file_skipped(id);
        if !is_core_backed {
            self.recompute_totals();
        }
    }

    pub(crate) fn perform_retry_file_action(&mut self, id: &str) {
        let context = self.visible_file_context(id);
        let source_url = context
            .as_ref()
            .and_then(|context| context.source_url.clone());
        if let Some(context) = context.as_ref()
            && let Some(source_url) = context.source_url.as_ref()
        {
            self.ensure_core_file(
                &context.id,
                source_url,
                &context.artifact_path,
                context.size,
                context.counts_toward_progress,
            );
        }
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
        let Some(source_url) = context.source_url.clone() else {
            if !self.core_state.files.contains_key(id) {
                self.show_overlay_error(id, id, "Reset unavailable for this file", true);
            }
            self.status = "Reset unavailable for selected file".to_string();
            self.recompute_totals();
            return;
        };

        self.ensure_core_file(
            &context.id,
            &source_url,
            &context.artifact_path,
            context.size,
            context.counts_toward_progress,
        );

        if let Some(token) = self.cancellation_tokens.remove(id) {
            token.cancel();
        }

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

    pub(crate) fn complete_login(&mut self, success: bool, error: Option<String>) {
        self.login.logging_in = false;
        if success {
            self.authenticated = true;
            self.popup = Popup::None;
            self.status = "Login successful".to_string();
            download::start_download_task(self);
        } else {
            self.login.error = error;
            self.popup = Popup::Login;
        }
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
            self.upsert_overlay_file(FileEntry {
                id: url.clone(),
                name: url,
                size: 0,
                downloaded: 0,
                source_url: None,
                counts_toward_progress: false,
                status: FileStatus::Queued,
            });
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
            DownloadEvent::FileStart { id, name, size } => {
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
            DownloadEvent::Progress { id, delta } => {
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
            DownloadEvent::ResumeReused { id, chunks, bytes } => {
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
            DownloadEvent::FileComplete { id, name } => {
                log::info!("Download complete: {name}");
                if self.deleted_files.remove(&id) {
                    self.cancellation_tokens.remove(&id);
                    download::schedule_resume_artifact_delete(name);
                    self.session_mark_file_skipped(&id);
                    return;
                }
                self.apply_core_event(CoreEvent::FileCompleted {
                    file_id: id.clone(),
                });
                self.recompute_totals();
                self.mark_visible_file_complete(&id, &name);
            }
            DownloadEvent::FileCancelled { id, name } => {
                log::info!("Download cancelled: {name}");
                if self.deleted_files.remove(&id) {
                    self.cancellation_tokens.remove(&id);
                    download::schedule_resume_artifact_delete(name);
                    self.session_mark_file_skipped(&id);
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
            DownloadEvent::Error { id, name, error } => {
                log::error!("Download error: {name}: {error}");
                if let Some(id) = id.as_ref()
                    && self.deleted_files.remove(id)
                {
                    self.cancellation_tokens.remove(id);
                    download::schedule_resume_artifact_delete(name);
                    self.session_mark_file_skipped(id);
                    return;
                }
                if self
                    .session
                    .as_ref()
                    .is_some_and(|session| session.urls.iter().any(|u| u.url == name))
                {
                    self.session_set_url_status(&name, UrlStatus::Error(error.clone()));
                    let _ = self.remove_overlay_file(&name);
                    self.show_ui_error_only(&name, &error);
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
            DownloadEvent::UrlQueued { url } => {
                if self.deleted_files.contains(&url) {
                    return;
                }
                self.queue_url_placeholder(url);
            }
            DownloadEvent::FileQueued {
                id,
                name,
                size,
                count_toward_progress,
                source_url,
                session_url,
            } => {
                if self.deleted_files.contains(&id) {
                    return;
                }
                if self.session_has_skipped_file(&session_url, &name) {
                    return;
                }
                self.ensure_core_file(&id, &source_url, &name, size, count_toward_progress);
                self.session_register_queued_file(&session_url, &name, size);
            }
            DownloadEvent::UrlResolved { url } => {
                let _ = self.drop_overlay_file(&url);
                self.session_set_url_status(&url, UrlStatus::Fetched);
                self.recompute_totals();
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
    ) {
        while let Ok(event) = download_rx.try_recv() {
            self.handle_download_event(event);
        }
    }

    pub(crate) fn drain_token_messages(&mut self) {
        while let Ok(msg) = self.token_rx.try_recv() {
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

        self.drain_download_events(download_rx);
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

            self.drain_download_events(download_rx);
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
                        self.drain_download_events(download_rx);
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
                    self.drain_download_events(download_rx);
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

    pub(crate) fn reset_aggregate_rate(&mut self) {
        self.current_speed = 0;
        self.aggregate_rate
            .reset(self.total_network_downloaded, Instant::now());
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
        #[derive(Serialize)]
        struct RunTotals {
            run_total_bytes: u64,
            run_completed_bytes: u64,
            run_file_total: usize,
            run_file_completed: usize,
            displayed_network_rate_bps: u64,
        }

        #[derive(Serialize)]
        struct SnapshotFile<'a> {
            id: &'a str,
            name: &'a str,
            size: u64,
            downloaded: u64,
            speed: u64,
            status: &'a FileStatus,
        }

        #[derive(Serialize)]
        struct Snapshot<'a> {
            authenticated: bool,
            paused: bool,
            logging_in: bool,
            login_error: Option<&'a str>,
            popup: Popup,
            packages: Vec<serde_json::Value>,
            files: Vec<SnapshotFile<'a>>,
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
            files: self
                .files
                .iter()
                .map(|file| SnapshotFile {
                    id: &file.id,
                    name: &file.name,
                    size: file.size,
                    downloaded: file.downloaded,
                    speed: self.file_speed(&file.id),
                    status: &file.status,
                })
                .collect(),
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
