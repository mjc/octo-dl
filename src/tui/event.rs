//! Download event types and TUI progress adapter.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::{
    DownloadProgress, FileStats,
    core::{FileAccounting, FileId, PackageId, ProgressDelta},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct TokenMessage {
    pub file_id: FileId,
    pub token: CancellationToken,
}

#[derive(Debug, Clone)]
pub struct FileOrigin {
    pub package_id: Option<PackageId>,
    pub package_display_name: Option<String>,
    pub source_url: String,
    pub submitted_url: String,
}

#[derive(Debug, Clone)]
pub struct QueuedFile {
    pub id: FileId,
    pub size: u64,
    pub accounting: FileAccounting,
    pub origin: FileOrigin,
}

/// Channel endpoints consumed by the background download task.
pub struct DownloadChannels {
    pub client_rx: Option<tokio::sync::oneshot::Receiver<(mega::Client, reqwest::Client)>>,
    pub event_tx: mpsc::UnboundedSender<DownloadEvent>,
    pub url_rx: mpsc::UnboundedReceiver<DownloadRequest>,
    pub token_tx: mpsc::UnboundedSender<TokenMessage>,
    pub pause_rx: tokio::sync::watch::Receiver<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadRequest {
    SubmitUrl {
        url: String,
    },
    ResumeFileIds {
        source_url: String,
        file_ids: Vec<FileId>,
        attempt_ids: HashMap<FileId, u64>,
    },
    ReverifyFileIds {
        source_url: String,
        file_ids: Vec<FileId>,
    },
    VerifyCompletedFileIds {
        source_url: String,
        file_ids: Vec<FileId>,
    },
    SyncPendingOrder {
        file_ids: Vec<FileId>,
    },
}

#[derive(Debug)]
pub enum DownloadEvent {
    FileStart {
        id: FileId,
        size: u64,
        attempt_id: u64,
    },
    ResumeValidationStarted {
        id: FileId,
        attempt_id: u64,
    },
    Progress {
        id: FileId,
        delta: ProgressDelta,
        attempt_id: u64,
    },
    VerificationProgress {
        id: FileId,
        bytes_delta: u64,
    },
    ResumeReused {
        id: FileId,
        chunks: usize,
        bytes: u64,
        attempt_id: u64,
    },
    ResumeReverified {
        id: FileId,
        chunks: usize,
        bytes: u64,
    },
    CompletedFileVerified {
        id: FileId,
        bytes: u64,
    },
    VerificationSkipped {
        id: FileId,
        completed: bool,
    },
    FileComplete {
        id: FileId,
        attempt_id: u64,
    },
    FileCancelled {
        id: FileId,
        attempt_id: u64,
    },
    FileError {
        id: FileId,
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
        saved_session: Option<crate::core::SavedMegaSession>,
        clear_saved_session: bool,
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
    ids: Mutex<HashMap<String, FileId>>,
}

impl TuiProgress {
    pub fn new(tx: mpsc::UnboundedSender<DownloadEvent>) -> Self {
        Self {
            tx,
            ids: Mutex::new(HashMap::new()),
        }
    }

    fn intern_id(&self, name: &str) -> FileId {
        let mut ids = self.ids.lock().unwrap();
        if let Some(id) = ids.get(name) {
            return id.clone();
        }
        let id = FileId::from(name);
        ids.insert(name.to_string(), id.clone());
        id
    }
}

impl DownloadProgress for TuiProgress {
    fn on_file_start(&self, name: &str, size: u64) {
        let id = self.intern_id(name);
        let _ = self.tx.send(DownloadEvent::FileStart {
            id,
            size,
            attempt_id: 0,
        });
    }

    fn on_resume_validation_start(&self, name: &str) {
        let id = self.intern_id(name);
        let _ = self
            .tx
            .send(DownloadEvent::ResumeValidationStarted { id, attempt_id: 0 });
    }

    fn on_resume_validation_chunk(&self, name: &str, bytes_delta: u64) {
        let id = self.intern_id(name);
        let _ = self
            .tx
            .send(DownloadEvent::VerificationProgress { id, bytes_delta });
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
        let id = self.intern_id(name);
        let _ = self.tx.send(DownloadEvent::ResumeReused {
            id,
            chunks,
            bytes,
            attempt_id: 0,
        });
    }

    fn on_file_complete(&self, name: &str, _stats: &FileStats) {
        let id = self.intern_id(name);
        let _ = self
            .tx
            .send(DownloadEvent::FileComplete { id, attempt_id: 0 });
    }

    fn on_error(&self, name: &str, error: &str) {
        let id = self.intern_id(name);
        let _ = self.tx.send(DownloadEvent::FileError {
            id,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn file_id_ptr_key(file_id: &FileId) -> (usize, usize) {
        let raw = file_id.as_str().as_bytes();
        (raw.as_ptr() as usize, raw.len())
    }

    #[test]
    fn stable_file_ids_are_reused_for_all_progress_events() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let progress = TuiProgress::new(tx);
        let stats = FileStats {
            size: 100,
            network_bytes: 0,
            reused_bytes: 0,
            elapsed: std::time::Duration::ZERO,
            average_speed: 0,
            peak_speed: 0,
            ramp_up_time: None,
        };

        progress.on_file_start("episode.mkv", 100);
        let DownloadEvent::FileStart { id: started, .. } = rx.blocking_recv().unwrap() else {
            panic!("expected FileStart");
        };
        progress.on_resume_reused("episode.mkv", 1, 60);
        let DownloadEvent::ResumeReused { id: reused, .. } = rx.blocking_recv().unwrap() else {
            panic!("expected ResumeReused");
        };
        progress.on_file_complete("episode.mkv", &stats);
        let DownloadEvent::FileComplete { id: completed, .. } = rx.blocking_recv().unwrap() else {
            panic!("expected FileComplete");
        };
        progress.on_error("episode.mkv", "boom");
        let DownloadEvent::FileError { id: errored, .. } = rx.blocking_recv().unwrap() else {
            panic!("expected FileError");
        };

        let expected = file_id_ptr_key(&started);
        assert_eq!(file_id_ptr_key(&reused), expected);
        assert_eq!(file_id_ptr_key(&completed), expected);
        assert_eq!(file_id_ptr_key(&errored), expected);
    }
}
