//! Application state model.

#[path = "app/actions.rs"]
mod actions;
#[path = "app/bootstrap.rs"]
mod bootstrap;
#[path = "app/overlay.rs"]
mod overlay;
#[path = "app/persistence.rs"]
mod persistence;
#[path = "app/progress.rs"]
mod progress;
#[path = "app/runtime.rs"]
mod runtime;
#[path = "app/state.rs"]
mod state;
#[cfg(test)]
#[path = "app/tests.rs"]
mod tests;
#[path = "app/types.rs"]
mod types;

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::Instant;

use indexmap::IndexMap;
use ratatui::widgets::ListState;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::core::{DownloadState, FileId, PackageId, ProgressDelta, SessionSnapshot};
use crate::tui::dashboard::DashboardUiMode;

use self::persistence::SessionPersistence;
pub(crate) use self::progress::FileUiState;
use self::progress::TransferRate;
pub use self::types::{
    ConfigField, ConfigState, ConfirmAction, FileEntry, FileStatus, LoginState,
    NoCredentialsFallback, Popup, QuitPolicy, SharedAppState, SortDirection, SortKey, SortState,
    UiAction,
};
pub(crate) use self::types::{SharedStateChannels, TransientRow, VisibleFileContext};

use super::event::DownloadRequest;
use super::event::{DownloadEvent, QueuedFile, TokenMessage};
use super::session::SessionAdapter;
use super::visible;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct VisibleRowsCacheKey {
    files_len: usize,
    core_files_len: usize,
    core_packages_len: usize,
    overlay_files_len: usize,
    expanded_hash: u64,
    sort_key: u8,
    sort_direction: u8,
}

pub struct App {
    pub popup: Popup,
    pub pending_confirmation: Option<ConfirmAction>,
    pub should_quit: bool,
    pub quit_policy: QuitPolicy,
    // Auth
    pub login: LoginState,
    pub authenticated: bool,
    // URL input (top bar)
    pub url_input: String,
    pub url_input_cursor: usize,
    pub url_input_active: bool,
    // Tracked URLs for session persistence
    pub urls: Vec<String>,
    // File queue (main content)
    pub files: Vec<FileEntry>,
    cached_visible_rows: Vec<visible::TuiRow>,
    cached_visible_rows_key: VisibleRowsCacheKey,
    pub(crate) visible_file_positions: HashMap<FileId, usize>,
    pub(crate) overlay_files: IndexMap<FileId, TransientRow>,
    pub(crate) file_ui: HashMap<FileId, FileUiState>,
    pub(crate) queued_file_effects: IndexMap<String, Vec<FileId>>,
    pub file_list_state: ListState,
    pub expanded_packages: HashSet<PackageId>,
    pub sort: SortState,
    // Aggregate stats
    pub total_downloaded: u64,
    pub total_size: u64,
    pub files_completed: usize,
    pub files_total: usize,
    pub current_speed: u64,
    total_network_downloaded: u64,
    overlay_total_downloaded: u64,
    overlay_total_size: u64,
    overlay_files_completed: usize,
    overlay_files_total: usize,
    overlay_total_network_downloaded: u64,
    aggregate_rate: TransferRate,
    // Status
    pub status: String,
    pub paused: bool,
    // Config
    pub config: ConfigState,
    // Channels
    pub event_tx: mpsc::UnboundedSender<DownloadEvent>,
    /// Always valid — URLs buffer in the channel until the download task starts.
    pub url_tx: mpsc::UnboundedSender<DownloadRequest>,
    /// Taken by `start_download_task` to give the receiver to the download task.
    pub(super) url_rx: Option<mpsc::UnboundedReceiver<DownloadRequest>>,
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
    pub(super) download_task_running: bool,
    // Cancellation tokens for active downloads (maps file path to token)
    pub cancellation_tokens: HashMap<FileId, CancellationToken>,
    // Per-file download attempt IDs for retry/reset flows
    pub file_attempt_ids: HashMap<FileId, u64>,
    // Files reset from the UI — used to suppress stale terminal events from the old attempt
    pub reset_pending_files: HashSet<FileId>,
    // Files paused for Alt-R reverify — their restart should preserve verified progress.
    pub reverify_pending_files: HashSet<FileId>,
    // Files currently running an explicit verification pass.
    pub verifying_files: HashSet<FileId>,
    // Session
    pub session: Option<SessionSnapshot>,
    pub(crate) session_persistence: SessionPersistence,
    pub core_state: DownloadState,
    // API port for display
    pub api_port: u16,
    // API key for authentication
    pub api_key: Option<String>,
    pub(crate) persist_config_path: Option<PathBuf>,
    // Resource usage
    pub cpu_usage: f32,
    pub memory_rss: u64,
    // Speed tracking
    pub last_tick: Instant,
}

impl App {
    fn visible_rows_cache_key(&self) -> VisibleRowsCacheKey {
        let mut expanded = self.expanded_packages.iter().copied().collect::<Vec<_>>();
        expanded.sort_unstable();
        let mut hasher = DefaultHasher::new();
        expanded.hash(&mut hasher);

        VisibleRowsCacheKey {
            files_len: self.files.len(),
            core_files_len: self.core_state.files.len(),
            core_packages_len: self.core_state.packages.len(),
            overlay_files_len: self.overlay_files.len(),
            expanded_hash: hasher.finish(),
            sort_key: match self.sort.key {
                SortKey::Queue => 0,
                SortKey::Status => 1,
                SortKey::Name => 2,
                SortKey::Percent => 3,
            },
            sort_direction: match self.sort.direction {
                SortDirection::Asc => 0,
                SortDirection::Desc => 1,
            },
        }
    }

    fn current_visible_rows(&self) -> Vec<visible::TuiRow> {
        if self.cached_visible_rows_key != self.visible_rows_cache_key() {
            return visible::visible_rows(self);
        }
        self.cached_visible_rows.clone()
    }

    pub fn visible_rows(&self) -> Vec<visible::TuiRow> {
        self.current_visible_rows()
    }

    pub(crate) fn cached_visible_rows(&self) -> &[visible::TuiRow] {
        &self.cached_visible_rows
    }

    pub(crate) fn ensure_visible_rows_cache(&mut self) {
        if self.cached_visible_rows_key != self.visible_rows_cache_key() {
            let visible_rows = visible::visible_rows(self);
            self.visible_file_positions = self
                .files
                .iter()
                .enumerate()
                .map(|(index, file)| (file.id.clone(), index))
                .collect();
            self.cached_visible_rows_key = self.visible_rows_cache_key();
            self.cached_visible_rows = visible_rows;
        }
    }

    pub fn selected_row(&self) -> Option<visible::TuiRow> {
        let selected = self.file_list_state.selected()?;
        if self.cached_visible_rows_key == self.visible_rows_cache_key() {
            return self.cached_visible_rows.get(selected).cloned();
        }
        visible::visible_rows(self).get(selected).cloned()
    }

    pub(crate) fn sync_visible_files(&mut self) {
        let selected_row_identity = self.selected_row();
        self.sync_visible_files_preserving(selected_row_identity);
    }

    pub(crate) fn sync_visible_files_preserving(
        &mut self,
        selected_row_identity: Option<visible::TuiRow>,
    ) {
        let visible_rows = visible::sync_visible_files(
            &mut self.files,
            &mut self.visible_file_positions,
            &mut self.overlay_files,
            &mut self.file_ui,
            &mut self.file_list_state,
            &self.core_state,
            &self.expanded_packages,
            &self.sort,
            selected_row_identity,
        );
        self.cached_visible_rows_key = self.visible_rows_cache_key();
        self.cached_visible_rows = visible_rows;
    }

    pub fn dashboard_json(&self, ui_mode: DashboardUiMode, read_only: bool) -> String {
        serde_json::to_string(&self.dashboard_state(ui_mode, read_only))
            .expect("dashboard state should serialize")
    }
}
