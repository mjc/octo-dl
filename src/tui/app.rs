//! Application state model.

#[path = "app/actions.rs"]
mod actions;
#[path = "app/bootstrap.rs"]
mod bootstrap;
#[path = "app/overlay.rs"]
mod overlay;
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

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use indexmap::IndexMap;
use ratatui::widgets::ListState;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::core::{DownloadState, FileId, PackageId, ProgressDelta, SessionSnapshot};
use crate::tui::dashboard::DashboardUiMode;

pub(crate) use self::progress::FileUiState;
use self::progress::TransferRate;
pub use self::types::{
    ConfigField, ConfigState, ConfirmAction, FileEntry, FileStatus, LoginState,
    NoCredentialsFallback, Popup, QuitPolicy, SharedAppState, SortDirection, SortKey, SortState,
    UiAction,
};
pub(crate) use self::types::{OverlayFile, SharedStateChannels, VisibleFileContext};

use super::event::DownloadRequest;
use super::event::{DownloadEvent, QueuedFile, TokenMessage};
use super::session::SessionAdapter;
use super::visible;

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
    pub(crate) visible_file_positions: HashMap<FileId, usize>,
    pub(crate) overlay_files: IndexMap<FileId, OverlayFile>,
    pub(crate) file_ui: HashMap<FileId, FileUiState>,
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
    // Session
    pub session: Option<SessionSnapshot>,
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
    pub fn visible_rows(&self) -> Vec<visible::TuiRow> {
        visible::visible_rows(self)
    }

    pub fn selected_row(&self) -> Option<visible::TuiRow> {
        let selected = self.file_list_state.selected()?;
        self.visible_rows().get(selected).cloned()
    }

    pub(crate) fn sync_visible_files(&mut self) {
        let selected_row_identity = self.selected_row();
        self.sync_visible_files_preserving(selected_row_identity);
    }

    pub(crate) fn sync_visible_files_preserving(
        &mut self,
        selected_row_identity: Option<visible::TuiRow>,
    ) {
        visible::sync_visible_files(
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
    }

    pub fn dashboard_json(&self, ui_mode: DashboardUiMode, read_only: bool) -> String {
        serde_json::to_string(&self.dashboard_state(ui_mode, read_only))
            .expect("dashboard state should serialize")
    }
}
