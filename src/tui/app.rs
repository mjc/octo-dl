//! Application state model.

use std::collections::{HashMap, HashSet};
use std::env;
use std::sync::Arc;
use std::time::Instant;

use ratatui::widgets::ListState;
use serde::Serialize;
use tokio::sync::{broadcast, mpsc, RwLock};
use tokio_util::sync::CancellationToken;

use crate::{DownloadConfig, SessionState};

use super::event::{DownloadEvent, TokenMessage};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Popup {
    None,
    Login,
    Config,
}

pub struct LoginState {
    pub email: String,
    pub password: String,
    pub mfa: String,
    pub active_field: usize,
    pub error: Option<String>,
    pub logging_in: bool,
}

impl LoginState {
    pub fn new() -> Self {
        Self {
            email: env::var("MEGA_EMAIL").unwrap_or_default(),
            password: env::var("MEGA_PASSWORD").unwrap_or_default(),
            mfa: env::var("MEGA_MFA").unwrap_or_default(),
            active_field: 0,
            error: None,
            logging_in: false,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileStatus {
    Queued,
    Downloading,
    Complete,
    Error(String),
}

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub name: String,
    pub size: u64,
    pub downloaded: u64,
    pub speed: u64,
    /// Bytes received since the last speed calculation (reset each tick).
    pub speed_accum: u64,
    pub status: FileStatus,
}

pub struct App {
    pub popup: Popup,
    pub should_quit: bool,
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
    // Status
    pub status: String,
    pub paused: bool,
    // Config
    pub config: ConfigState,
    // Channels
    pub event_tx: mpsc::UnboundedSender<DownloadEvent>,
    pub url_tx: Option<mpsc::UnboundedSender<String>>,
    pub token_rx: Option<mpsc::UnboundedReceiver<TokenMessage>>,
    /// Receives the authenticated client from the login task.
    pub client_rx: Option<tokio::sync::oneshot::Receiver<(mega::Client, reqwest::Client)>>,
    // Cancellation tokens for active downloads (maps file path to token)
    pub cancellation_tokens: HashMap<String, CancellationToken>,
    // Files deleted from the UI — used to suppress stale download events
    pub deleted_files: HashSet<String>,
    // Session
    pub session: Option<SessionState>,
    // API port for display
    pub api_port: u16,
    // Resource usage
    pub cpu_usage: f32,
    pub memory_rss: u64,
    // Speed tracking
    pub last_tick: Instant,
}

impl App {
    /// Computes per-file instantaneous speeds from accumulated bytes since last tick.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn update_speeds(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f64();
        self.last_tick = now;

        if dt > 0.0 {
            for f in &mut self.files {
                if matches!(f.status, FileStatus::Downloading) {
                    f.speed = (f.speed_accum as f64 / dt) as u64;
                }
                f.speed_accum = 0;
            }
        }

        self.current_speed = self
            .files
            .iter()
            .filter(|f| matches!(f.status, FileStatus::Downloading))
            .map(|f| f.speed)
            .sum();
    }

    /// Recomputes aggregate totals from the current files list.
    ///
    /// Call after deleting files to keep counters consistent.
    pub fn recompute_totals(&mut self) {
        self.total_size = self.files.iter().map(|f| f.size).sum();
        self.total_downloaded = self.files.iter().map(|f| f.downloaded).sum();
        self.files_completed = self
            .files
            .iter()
            .filter(|f| matches!(f.status, FileStatus::Complete))
            .count();
        self.files_total = self
            .files
            .iter()
            .filter(|f| !matches!(f.status, FileStatus::Error(_)))
            .count();
        self.current_speed = self
            .files
            .iter()
            .filter(|f| matches!(f.status, FileStatus::Downloading))
            .map(|f| f.speed)
            .sum();
    }

    pub fn new(api_port: u16, event_tx: mpsc::UnboundedSender<DownloadEvent>) -> Self {
        Self {
            popup: Popup::None,
            should_quit: false,
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
            status: String::new(),
            paused: false,
            config: ConfigState::new(),
            event_tx,
            url_tx: None,
            token_rx: None,
            client_rx: None,
            cancellation_tokens: HashMap::new(),
            deleted_files: HashSet::new(),
            session: None,
            api_port,
            cpu_usage: 0.0,
            last_tick: Instant::now(),
            memory_rss: 0,
        }
    }
}

// =============================================================================
// Web UI shared state types
// =============================================================================

/// Serializable snapshot of a file entry for the web UI.
#[derive(Debug, Clone, Serialize)]
pub struct FileEntrySnapshot {
    pub name: String,
    pub size: u64,
    pub downloaded: u64,
    pub speed: u64,
    pub status: String,
    pub error: Option<String>,
}

impl From<&FileEntry> for FileEntrySnapshot {
    fn from(f: &FileEntry) -> Self {
        let (status, error) = match &f.status {
            FileStatus::Queued => ("queued".to_string(), None),
            FileStatus::Downloading => ("downloading".to_string(), None),
            FileStatus::Complete => ("complete".to_string(), None),
            FileStatus::Error(e) => ("error".to_string(), Some(e.clone())),
        };
        Self {
            name: f.name.clone(),
            size: f.size,
            downloaded: f.downloaded,
            speed: f.speed,
            status,
            error,
        }
    }
}

/// Serializable snapshot of the full application state for the web UI.
#[derive(Debug, Clone, Serialize)]
pub struct AppSnapshot {
    pub authenticated: bool,
    pub logging_in: bool,
    pub paused: bool,
    pub status: String,
    pub files: Vec<FileEntrySnapshot>,
    pub total_downloaded: u64,
    pub total_size: u64,
    pub files_completed: usize,
    pub files_total: usize,
    pub current_speed: u64,
    pub cpu_usage: f32,
    pub memory_rss: u64,
    pub config: DownloadConfigSnapshot,
    pub url_input: String,
}

/// Serializable snapshot of download configuration.
#[derive(Debug, Clone, Serialize)]
pub struct DownloadConfigSnapshot {
    pub chunks_per_file: usize,
    pub concurrent_files: usize,
    pub force_overwrite: bool,
    pub cleanup_on_error: bool,
}

impl From<&DownloadConfig> for DownloadConfigSnapshot {
    fn from(c: &DownloadConfig) -> Self {
        Self {
            chunks_per_file: c.chunks_per_file,
            concurrent_files: c.concurrent_files,
            force_overwrite: c.force_overwrite,
            cleanup_on_error: c.cleanup_on_error,
        }
    }
}

impl App {
    /// Creates a serializable snapshot of the current application state.
    pub fn snapshot(&self) -> AppSnapshot {
        AppSnapshot {
            authenticated: self.authenticated,
            logging_in: self.login.logging_in,
            paused: self.paused,
            status: self.status.clone(),
            files: self.files.iter().map(FileEntrySnapshot::from).collect(),
            total_downloaded: self.total_downloaded,
            total_size: self.total_size,
            files_completed: self.files_completed,
            files_total: self.files_total,
            current_speed: self.current_speed,
            cpu_usage: self.cpu_usage,
            memory_rss: self.memory_rss,
            config: DownloadConfigSnapshot::from(&self.config.config),
            url_input: self.url_input.clone(),
        }
    }
}

/// Actions that the web UI can send to the backend event loop.
#[derive(Debug, Clone)]
pub enum UiAction {
    Login {
        email: String,
        password: String,
        mfa: String,
    },
    AddUrls(Vec<String>),
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

/// Shared state container accessible from both the event loop and API handlers.
#[derive(Clone)]
pub struct SharedAppState {
    /// Latest application snapshot, updated each tick.
    pub snapshot: Arc<RwLock<AppSnapshot>>,
    /// Broadcast channel for SSE — subscribers receive snapshots.
    pub broadcast_tx: broadcast::Sender<AppSnapshot>,
    /// Channel for web UI actions directed at the event loop.
    pub action_tx: mpsc::UnboundedSender<UiAction>,
}

impl Default for AppSnapshot {
    fn default() -> Self {
        Self {
            authenticated: false,
            logging_in: false,
            paused: false,
            status: String::new(),
            files: Vec::new(),
            total_downloaded: 0,
            total_size: 0,
            files_completed: 0,
            files_total: 0,
            current_speed: 0,
            cpu_usage: 0.0,
            memory_rss: 0,
            config: DownloadConfigSnapshot {
                chunks_per_file: 2,
                concurrent_files: 4,
                force_overwrite: false,
                cleanup_on_error: true,
            },
            url_input: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    fn test_app() -> App {
        let (tx, _rx) = mpsc::unbounded_channel();
        App::new(9723, tx)
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
        login.email.clear();
        login.password.clear();
        login.mfa.clear();

        login.active_field = 0;
        login.active_value_mut().push_str("test@example.com");
        assert_eq!(login.email, "test@example.com");

        login.active_field = 1;
        login.active_value_mut().push_str("password123");
        assert_eq!(login.password, "password123");

        login.active_field = 2;
        login.active_value_mut().push_str("123456");
        assert_eq!(login.mfa, "123456");
    }

    #[test]
    fn config_field_labels() {
        assert_eq!(ConfigField::ChunksPerFile.label(), "Chunks per file");
        assert_eq!(ConfigField::ConcurrentFiles.label(), "Concurrent files");
        assert_eq!(ConfigField::ForceOverwrite.label(), "Force overwrite");
        assert_eq!(ConfigField::CleanupOnError.label(), "Cleanup on error");
    }
}
