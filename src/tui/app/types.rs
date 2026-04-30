use serde::Serialize;
use tokio::sync::{mpsc, watch};

use crate::DownloadConfig;

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

    pub fn set_credentials(&mut self, email: String, password: String, mfa: String) -> bool {
        if email.is_empty() || password.is_empty() {
            return false;
        }
        self.email = email;
        self.password = password;
        self.mfa = mfa;
        true
    }

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
    pub status: FileStatus,
}

#[derive(Debug, Clone)]
pub(crate) struct OverlayFile {
    pub file: FileEntry,
    pub source_url: Option<String>,
    pub counts_toward_progress: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct VisibleFileContext {
    pub id: String,
    pub status: FileStatus,
    pub source_url: Option<String>,
    pub artifact_path: String,
    pub size: u64,
    pub counts_toward_progress: bool,
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
