//! Event types for TUI mode.

use crate::FileEntryStatus;
use tokio_util::sync::CancellationToken;

/// Events that can be sent from download tasks to the TUI.
#[derive(Debug, Clone)]
pub enum DownloadEvent {
    /// URLs received from the bookmarklet API.
    UrlsReceived { urls: Vec<String> },
    /// File download started.
    FileStart { name: String, size: u64 },
    /// Download progress for a file.
    FileProgress { name: String, delta: u64, speed: u64 },
    /// File download completed.
    FileComplete { name: String, status: FileEntryStatus },
    /// File download failed.
    FileError { name: String, error: String },
    /// Partial file detected.
    PartialDetected { name: String, existing: u64, expected: u64 },
}

/// Message containing a cancellation token for a file download.
pub struct TokenMessage {
    pub file_path: String,
    pub token: CancellationToken,
}
