//! Application state model.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use ratatui::widgets::ListState;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{DownloadConfig, SessionState};

use super::event::{DownloadEvent, TokenMessage};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    pub fn new() -> Self {
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
            self.email = email.to_owned();
        }
        if self.password.is_empty() && !password.is_empty() {
            self.password = password.to_owned();
        }
        if self.mfa.is_empty() && !mfa.is_empty() {
            self.mfa = mfa.to_owned();
        }
    }

    pub fn has_credentials(&self) -> bool {
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
    /// Always valid — URLs buffer in the channel until the download task starts.
    pub url_tx: mpsc::UnboundedSender<String>,
    /// Taken by `start_download_task` to give the receiver to the download task.
    pub(super) url_rx: Option<mpsc::UnboundedReceiver<String>>,
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
        let (url_tx, url_rx) = mpsc::unbounded_channel::<String>();
        let (token_tx, token_rx) = mpsc::unbounded_channel::<TokenMessage>();
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
            url_tx,
            url_rx: Some(url_rx),
            token_rx,
            token_tx: Some(token_tx),
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
}
