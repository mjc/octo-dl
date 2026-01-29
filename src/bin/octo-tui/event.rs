//! Download event types and TUI progress adapter.

use octo_dl::{DownloadProgress, FileStats};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct TokenMessage {
    pub file_path: String,
    pub token: CancellationToken,
}

/// Channel endpoints consumed by the background download task.
pub struct DownloadChannels {
    pub client_rx: Option<tokio::sync::oneshot::Receiver<(mega::Client, reqwest::Client)>>,
    pub event_tx: mpsc::UnboundedSender<DownloadEvent>,
    pub url_rx: mpsc::UnboundedReceiver<String>,
    pub token_tx: mpsc::UnboundedSender<TokenMessage>,
}

/// Borrowed sender references passed into download helper functions.
pub struct DownloadSenders<'a> {
    pub event_tx: &'a mpsc::UnboundedSender<DownloadEvent>,
    pub token_tx: &'a mpsc::UnboundedSender<TokenMessage>,
}

#[derive(Debug)]
pub enum DownloadEvent {
    FileStart {
        name: String,
        size: u64,
    },
    Progress {
        name: String,
        bytes_delta: u64,
        speed: u64,
    },
    FileComplete {
        name: String,
    },
    Error {
        name: String,
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
    FileQueued {
        name: String,
        size: u64,
    },
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
}

impl DownloadProgress for TuiProgress {
    fn on_file_start(&self, name: &str, size: u64) {
        let _ = self.tx.send(DownloadEvent::FileStart {
            name: name.to_string(),
            size,
        });
    }

    fn on_progress(&self, name: &str, bytes_delta: u64, speed: u64) {
        let _ = self.tx.send(DownloadEvent::Progress {
            name: name.to_string(),
            bytes_delta,
            speed,
        });
    }

    fn on_file_complete(&self, name: &str, _stats: &FileStats) {
        let _ = self.tx.send(DownloadEvent::FileComplete {
            name: name.to_string(),
        });
    }

    fn on_error(&self, name: &str, error: &str) {
        let _ = self.tx.send(DownloadEvent::Error {
            name: name.to_string(),
            error: error.to_string(),
        });
    }

    fn on_partial_detected(&self, name: &str, existing_size: u64, expected_size: u64) {
        let _ = self.tx.send(DownloadEvent::StatusMessage(format!(
            "Partial download detected: {name} ({existing_size}/{expected_size} bytes)"
        )));
    }
}
