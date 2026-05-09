//! Download event types and TUI progress adapter.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::{DownloadProgress, FileStats, core::ProgressDelta};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct TokenMessage {
    pub file_id: String,
    pub token: CancellationToken,
}

#[derive(Debug, Clone)]
pub struct FileOrigin {
    pub source_url: String,
    pub submitted_url: String,
}

#[derive(Debug, Clone)]
pub struct QueuedFile {
    pub id: String,
    pub size: u64,
    pub count_toward_progress: bool,
    pub origin: FileOrigin,
}

/// Channel endpoints consumed by the background download task.
pub struct DownloadChannels {
    pub client_rx: Option<tokio::sync::oneshot::Receiver<(mega::Client, reqwest::Client)>>,
    pub event_tx: mpsc::UnboundedSender<DownloadEvent>,
    pub url_rx: mpsc::UnboundedReceiver<DownloadRequest>,
    pub token_tx: mpsc::UnboundedSender<TokenMessage>,
    pub pause_rx: tokio::sync::watch::Receiver<bool>,
    pub skipped_session_paths: HashMap<String, HashSet<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadRequest {
    SubmitUrl {
        url: String,
    },
    ResumeFileIds {
        source_url: String,
        file_ids: Vec<String>,
        attempt_ids: HashMap<String, u64>,
    },
}

#[derive(Debug)]
pub enum DownloadEvent {
    FileStart {
        id: String,
        size: u64,
        attempt_id: u64,
    },
    Progress {
        id: Arc<str>,
        delta: ProgressDelta,
        attempt_id: u64,
    },
    ResumeReused {
        id: String,
        chunks: usize,
        bytes: u64,
        attempt_id: u64,
    },
    FileComplete {
        id: String,
        attempt_id: u64,
    },
    FileCancelled {
        id: String,
        attempt_id: u64,
    },
    FileError {
        id: String,
        error: String,
        attempt_id: u64,
    },
    ScopeError {
        scope: String,
        error: String,
    },
    LoginResult {
        success: bool,
        error: Option<String>,
    },
    FilesCollected {
        total: usize,
        skipped: usize,
        partial: usize,
        total_bytes: u64,
    },
    UrlQueued {
        url: String,
    },
    FileQueued(QueuedFile),
    UrlResolved {
        url: String,
    },
    StatusMessage(String),
    UrlsReceived {
        urls: Vec<String>,
    },
}

pub struct TuiProgress {
    pub tx: mpsc::UnboundedSender<DownloadEvent>,
    ids: Mutex<HashMap<String, Arc<str>>>,
}

impl TuiProgress {
    pub fn new(tx: mpsc::UnboundedSender<DownloadEvent>) -> Self {
        Self {
            tx,
            ids: Mutex::new(HashMap::new()),
        }
    }

    fn intern_id(&self, name: &str) -> Arc<str> {
        let mut ids = self.ids.lock().unwrap();
        if let Some(id) = ids.get(name) {
            return Arc::clone(id);
        }
        let id = Arc::<str>::from(name);
        ids.insert(name.to_string(), Arc::clone(&id));
        id
    }
}

impl DownloadProgress for TuiProgress {
    fn on_file_start(&self, name: &str, size: u64) {
        let _ = self.intern_id(name);
        let _ = self.tx.send(DownloadEvent::FileStart {
            id: name.to_string(),
            size,
            attempt_id: 0,
        });
    }

    fn on_progress(&self, name: &str, delta: ProgressDelta) {
        let id = self.intern_id(name);
        let _ = self.tx.send(DownloadEvent::Progress {
            id,
            delta,
            attempt_id: 0,
        });
    }

    fn on_resume_reused(&self, name: &str, chunks: usize, bytes: u64) {
        let _ = self.intern_id(name);
        let _ = self.tx.send(DownloadEvent::ResumeReused {
            id: name.to_string(),
            chunks,
            bytes,
            attempt_id: 0,
        });
    }

    fn on_file_complete(&self, name: &str, _stats: &FileStats) {
        let _ = self.tx.send(DownloadEvent::FileComplete {
            id: name.to_string(),
            attempt_id: 0,
        });
    }

    fn on_error(&self, name: &str, error: &str) {
        let _ = self.tx.send(DownloadEvent::FileError {
            id: name.to_string(),
            error: error.to_string(),
            attempt_id: 0,
        });
    }

    fn on_partial_detected(&self, name: &str, existing_size: u64, expected_size: u64) {
        let _ = self.tx.send(DownloadEvent::StatusMessage(format!(
            "Partial download detected: {name} ({existing_size}/{expected_size} bytes)"
        )));
    }
}
