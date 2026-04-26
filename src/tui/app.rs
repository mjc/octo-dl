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

use crate::{
    SessionState,
    core::{DownloadState, ProgressDelta},
};

pub(crate) use self::progress::FileUiState;
use self::progress::TransferRate;
pub use self::types::{
    ConfigField, ConfigState, FileEntry, FileStatus, LoginState, NoCredentialsFallback, Popup,
    QuitPolicy, SharedAppState, UiAction,
};
pub(crate) use self::types::{OverlayFile, SharedStateChannels, VisibleFileContext};

use super::event::{DownloadEvent, QueuedFile, TokenMessage};
use super::session::{SessionAdapter, SessionFileUpdate, SessionRunUpdate, SessionUrlUpdate};
use super::visible;

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
    pub(crate) overlay_files: IndexMap<String, OverlayFile>,
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
        visible::sorted_file_indices(&self.files, &self.core_state, &self.overlay_files)
    }

    pub fn selected_file_index(&self) -> Option<usize> {
        visible::selected_file_index(
            &self.file_list_state,
            &self.files,
            &self.core_state,
            &self.overlay_files,
        )
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
