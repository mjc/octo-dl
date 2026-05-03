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
#[path = "app/snapshot.rs"]
mod snapshot;
#[path = "app/state.rs"]
mod state;
#[cfg(test)]
#[path = "app/tests.rs"]
mod tests;
#[path = "app/types.rs"]
mod types;

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use indexmap::IndexMap;
use ratatui::widgets::ListState;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::core::{DownloadState, ProgressDelta, SessionSnapshotV3};

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
use super::session::{SessionAdapter, SessionFileUpdate, SessionRunUpdate, SessionUrlUpdate};
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
    pub url_input_active: bool,
    // Tracked URLs for session persistence
    pub urls: Vec<String>,
    // File queue (main content)
    pub files: Vec<FileEntry>,
    pub(crate) overlay_files: IndexMap<String, OverlayFile>,
    pub(crate) file_ui: HashMap<String, FileUiState>,
    pub file_list_state: ListState,
    pub expanded_packages: HashSet<String>,
    pub sort: SortState,
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
    // Cancellation tokens for active downloads (maps file path to token)
    pub cancellation_tokens: HashMap<String, CancellationToken>,
    // Files deleted from the UI — used to suppress stale download events
    pub deleted_files: HashSet<String>,
    // Session
    pub session: Option<SessionSnapshotV3>,
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
    #[allow(dead_code)]
    pub fn sorted_file_indices(&self) -> Vec<usize> {
        visible::sorted_file_indices(&self.files, &self.core_state, &self.overlay_files)
    }

    #[allow(dead_code)]
    pub fn selected_file_index(&self) -> Option<usize> {
        visible::selected_file_index(
            &self.file_list_state,
            &self.files,
            &self.core_state,
            &self.overlay_files,
        )
    }

    pub fn visible_rows(&self) -> Vec<visible::TuiRow> {
        visible::visible_rows(self)
    }

    pub fn selected_row(&self) -> Option<visible::TuiRow> {
        let selected = self.file_list_state.selected()?;
        self.visible_rows().get(selected).cloned()
    }

    pub fn package_file_ids(&self, package_id: &str) -> Vec<String> {
        self.core_state
            .packages
            .get(package_id)
            .map(|package| package.file_ids.clone())
            .unwrap_or_default()
    }

    pub fn package_display_name(&self, package_id: &str) -> String {
        self.core_state
            .packages
            .get(package_id)
            .map(|package| package.display_name.clone())
            .unwrap_or_else(|| package_id.to_string())
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

    /// Serialises UI-visible state to a JSON string.
    ///
    /// Called by the event loop *only* when state has changed and at least
    /// one SSE/API client is connected — never on a blind timer.
    #[allow(dead_code)]
    pub fn to_json(&self) -> String {
        snapshot::to_json(self)
    }
}
