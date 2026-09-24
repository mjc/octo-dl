//! Download event types and TUI progress adapter.

#![allow(
    clippy::result_large_err,
    clippy::match_same_arms,
    clippy::needless_pass_by_value,
    clippy::significant_drop_tightening,
    clippy::unused_self
)]

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use crate::{
    DownloadProgress, FileStats,
    core::{FileAccounting, FileId, PackageId, ProgressDelta},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Identity of one download attempt for one file.
///
/// This is intentionally distinct from both [`FileId`] and
/// [`VerificationOperationId`]. It is an internal TUI/runtime identity and is
/// not persisted in [`crate::core::FileLifecycle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DownloadAttemptId(u64);

impl DownloadAttemptId {
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

impl From<u64> for DownloadAttemptId {
    fn from(raw: u64) -> Self {
        Self::new(raw)
    }
}

/// Identity of one explicit verification operation for one file.
///
/// This is intentionally distinct from a download attempt. A verification
/// result may arrive after a reset or retry, so the receiver must be able to
/// prove which operation produced it before mutating state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VerificationOperationId(u64);

impl VerificationOperationId {
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone)]
pub struct TokenMessage {
    pub file_id: FileId,
    pub attempt_id: DownloadAttemptId,
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
    pub attempt_id: DownloadAttemptId,
    pub size: u64,
    pub accounting: FileAccounting,
    pub origin: FileOrigin,
}

const DOWNLOAD_EVENT_CAPACITY: usize = 256;
const LIFECYCLE_BACKLOG_CAPACITY: usize = 256;

/// A bounded event ingress for the download worker.
///
/// Progress is accumulated by file and attempt, then sealed into the ordered
/// queue before each lifecycle event. Lifecycle events use the queue directly when
/// possible and otherwise enter a durable FIFO. The FIFO is separate from the
/// bounded channel so state-changing lifecycle events remain available for
/// the application instead of being dropped while the channel is full.
#[derive(Clone)]
pub struct DownloadEventSender {
    tx: mpsc::Sender<DownloadEvent>,
    compatibility_tx: Option<mpsc::UnboundedSender<DownloadEvent>>,
    progress: Arc<PendingProgress>,
    lifecycle: Arc<PendingLifecycle>,
}

struct PendingProgress {
    values: Mutex<HashMap<(FileId, DownloadAttemptId), crate::core::ProgressDelta>>,
    capacity: usize,
    wakeup_pending: std::sync::atomic::AtomicBool,
}

struct PendingLifecycle {
    events: Mutex<PendingLifecycleState>,
}

struct PendingLifecycleState {
    events: VecDeque<DownloadEvent>,
    failure: Option<DownloadEventDeliveryFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadEventDeliveryFailure {
    event: &'static str,
    reason: DeliveryFailureReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryFailureReason {
    ChannelClosed,
}

impl std::fmt::Display for DownloadEventDeliveryFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = match self.reason {
            DeliveryFailureReason::ChannelClosed => "event channel is closed",
        };
        write!(formatter, "{reason} while delivering {}", self.event)
    }
}

const MAX_TRACKED_FILE_IDS: usize = 4096;

struct TrackedFileIds {
    by_name: HashMap<String, FileId>,
    order: VecDeque<String>,
}

impl DownloadEventSender {
    #[must_use]
    pub fn channel() -> (Self, mpsc::Receiver<DownloadEvent>) {
        Self::channel_with_capacity(DOWNLOAD_EVENT_CAPACITY)
    }

    #[must_use]
    pub fn channel_with_capacity(capacity: usize) -> (Self, mpsc::Receiver<DownloadEvent>) {
        Self::channel_with_capacities(capacity, LIFECYCLE_BACKLOG_CAPACITY)
    }

    pub(crate) fn channel_with_capacities(
        capacity: usize,
        _lifecycle_capacity: usize,
    ) -> (Self, mpsc::Receiver<DownloadEvent>) {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        (
            Self {
                tx,
                compatibility_tx: None,
                progress: Arc::new(PendingProgress {
                    values: Mutex::new(HashMap::new()),
                    capacity: capacity.max(1),
                    wakeup_pending: std::sync::atomic::AtomicBool::new(false),
                }),
                lifecycle: Arc::new(PendingLifecycle {
                    events: Mutex::new(PendingLifecycleState {
                        events: VecDeque::new(),
                        failure: None,
                    }),
                }),
            },
            rx,
        )
    }

    /// Adapts legacy test and embedding senders while callers migrate to the
    /// bounded event ingress. Production construction uses [`Self::channel`].
    #[cfg(test)]
    pub(crate) fn from_unbounded(tx: mpsc::UnboundedSender<DownloadEvent>) -> Self {
        let (bounded_tx, _bounded_rx) = mpsc::channel(1);
        Self {
            tx: bounded_tx,
            compatibility_tx: Some(tx),
            progress: Arc::new(PendingProgress {
                values: Mutex::new(HashMap::new()),
                capacity: DOWNLOAD_EVENT_CAPACITY,
                wakeup_pending: std::sync::atomic::AtomicBool::new(false),
            }),
            lifecycle: Arc::new(PendingLifecycle {
                events: Mutex::new(PendingLifecycleState {
                    events: VecDeque::new(),
                    failure: None,
                }),
            }),
        }
    }

    /// Attempts to admit one event to the bounded ingress.
    ///
    /// Control events are never reordered behind an asynchronous waiter task:
    /// a full or closed queue is reported to the caller. Progress is
    /// coalesced by file and attempt, but the number of distinct pending
    /// identities is bounded by the event queue capacity.
    pub fn send(
        &self,
        event: DownloadEvent,
    ) -> Result<(), mpsc::error::TrySendError<DownloadEvent>> {
        if let Some(tx) = &self.compatibility_tx {
            return tx
                .send(event)
                .map_err(|error| mpsc::error::TrySendError::Closed(error.0));
        }
        if let DownloadEvent::Progress {
            id,
            delta,
            attempt_id,
        } = event
        {
            return self.send_progress(id, delta, attempt_id);
        }
        // URL submissions are API commands, not worker lifecycle state. They
        // must preserve the caller's immediate backpressure result instead of
        // being accepted into the durable worker-event backlog.
        if let DownloadEvent::StatusMessage(_) = &event {
            return self.tx.try_send(event);
        }
        if let DownloadEvent::UrlsReceived { .. } = &event {
            return self.tx.try_send(event);
        }

        self.send_lifecycle(event)
    }

    fn send_lifecycle(
        &self,
        event: DownloadEvent,
    ) -> Result<(), mpsc::error::TrySendError<DownloadEvent>> {
        // Seal accumulated progress before the lifecycle boundary. Holding
        // both locks prevents a later delta from overtaking its start, or an
        // earlier delta from arriving after completion/cancellation/failure.
        let mut values = self.progress.values.lock().unwrap();
        let mut lifecycle = self.lifecycle.events.lock().unwrap();
        for ((id, attempt_id), delta) in values.drain() {
            self.send_ordered_event_locked(
                DownloadEvent::Progress {
                    id,
                    delta,
                    attempt_id,
                },
                &mut lifecycle,
            )?;
        }
        self.send_ordered_event_locked(event, &mut lifecycle)
    }

    fn send_ordered_event_locked(
        &self,
        event: DownloadEvent,
        lifecycle: &mut PendingLifecycleState,
    ) -> Result<(), mpsc::error::TrySendError<DownloadEvent>> {
        if !lifecycle.events.is_empty() {
            lifecycle.events.push_back(event);
            return Ok(());
        }

        match self.tx.try_send(event) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(event)) => {
                lifecycle.events.push_back(event);
                Ok(())
            }
            Err(mpsc::error::TrySendError::Closed(event)) => {
                self.record_failure_locked(lifecycle, &event, DeliveryFailureReason::ChannelClosed);
                Err(mpsc::error::TrySendError::Closed(event))
            }
        }
    }

    fn record_failure_locked(
        &self,
        lifecycle: &mut PendingLifecycleState,
        event: &DownloadEvent,
        reason: DeliveryFailureReason,
    ) {
        lifecycle
            .failure
            .get_or_insert_with(|| DownloadEventDeliveryFailure {
                event: download_event_name(event),
                reason,
            });
    }

    /// Flushes retained lifecycle events in FIFO order into the bounded
    /// channel. A full channel leaves the FIFO untouched for the next app
    /// tick; a closed channel records a delivery failure and abandons only the
    /// events that can no longer be delivered.
    pub(crate) fn flush_lifecycle_events(&self) -> bool {
        let mut lifecycle = self.lifecycle.events.lock().unwrap();
        let mut flushed = false;
        while let Some(event) = lifecycle.events.pop_front() {
            match self.tx.try_send(event) {
                Ok(()) => flushed = true,
                Err(mpsc::error::TrySendError::Full(event)) => {
                    lifecycle.events.push_front(event);
                    break;
                }
                Err(mpsc::error::TrySendError::Closed(event)) => {
                    self.record_failure_locked(
                        &mut lifecycle,
                        &event,
                        DeliveryFailureReason::ChannelClosed,
                    );
                    lifecycle.events.clear();
                    break;
                }
            }
        }
        flushed
    }

    pub(crate) fn has_pending_lifecycle_events(&self) -> bool {
        !self.lifecycle.events.lock().unwrap().events.is_empty()
    }

    pub(crate) fn take_delivery_failure(&self) -> Option<DownloadEventDeliveryFailure> {
        self.lifecycle.events.lock().unwrap().failure.take()
    }

    fn send_progress(
        &self,
        id: FileId,
        delta: crate::core::ProgressDelta,
        attempt_id: DownloadAttemptId,
    ) -> Result<(), mpsc::error::TrySendError<DownloadEvent>> {
        let event = || DownloadEvent::Progress {
            id: id.clone(),
            delta,
            attempt_id,
        };
        let mut values = self.progress.values.lock().unwrap();
        let key = (id.clone(), attempt_id);
        if !values.contains_key(&key) && values.len() >= self.progress.capacity {
            return Err(mpsc::error::TrySendError::Full(event()));
        }

        let lifecycle_pending = self.has_pending_lifecycle_events();
        if lifecycle_pending {
            let pending = values.entry(key).or_default();
            pending.total_bytes_delta = pending
                .total_bytes_delta
                .saturating_add(delta.total_bytes_delta);
            pending.network_bytes_delta = pending
                .network_bytes_delta
                .saturating_add(delta.network_bytes_delta);
            return Ok(());
        }

        if !self
            .progress
            .wakeup_pending
            .load(std::sync::atomic::Ordering::Acquire)
        {
            match self.tx.try_send(DownloadEvent::ProgressWakeup) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    let pending = values.entry(key).or_default();
                    pending.total_bytes_delta = pending
                        .total_bytes_delta
                        .saturating_add(delta.total_bytes_delta);
                    pending.network_bytes_delta = pending
                        .network_bytes_delta
                        .saturating_add(delta.network_bytes_delta);
                    return Err(mpsc::error::TrySendError::Full(event()));
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    values.clear();
                    return Err(mpsc::error::TrySendError::Closed(event()));
                }
            }
            self.progress
                .wakeup_pending
                .store(true, std::sync::atomic::Ordering::Release);
        }

        let pending = values.entry(key).or_default();
        pending.total_bytes_delta = pending
            .total_bytes_delta
            .saturating_add(delta.total_bytes_delta);
        pending.network_bytes_delta = pending
            .network_bytes_delta
            .saturating_add(delta.network_bytes_delta);
        Ok(())
    }

    pub(crate) fn take_pending_progress(
        &self,
        receiver: &mpsc::Receiver<DownloadEvent>,
    ) -> Vec<(FileId, crate::core::ProgressDelta, DownloadAttemptId)> {
        let mut values = self.progress.values.lock().unwrap();
        let lifecycle = self.lifecycle.events.lock().unwrap();
        // Check while holding the sender locks: a worker must not enqueue a
        // start and its progress between the empty check and the drain.
        if !lifecycle.events.is_empty() || !receiver.is_empty() {
            return Vec::new();
        }
        let pending = values
            .drain()
            .map(|((id, attempt_id), delta)| (id, delta, attempt_id))
            .collect();
        self.progress
            .wakeup_pending
            .store(false, std::sync::atomic::Ordering::Release);
        pending
    }
}

#[cfg(test)]
impl From<mpsc::UnboundedSender<DownloadEvent>> for DownloadEventSender {
    fn from(tx: mpsc::UnboundedSender<DownloadEvent>) -> Self {
        Self::from_unbounded(tx)
    }
}

#[cfg(test)]
impl From<mpsc::Sender<DownloadEvent>> for DownloadEventSender {
    fn from(tx: mpsc::Sender<DownloadEvent>) -> Self {
        let capacity = tx.max_capacity();
        Self {
            tx,
            compatibility_tx: None,
            progress: Arc::new(PendingProgress {
                values: Mutex::new(HashMap::new()),
                capacity,
                wakeup_pending: std::sync::atomic::AtomicBool::new(false),
            }),
            lifecycle: Arc::new(PendingLifecycle {
                events: Mutex::new(PendingLifecycleState {
                    events: VecDeque::new(),
                    failure: None,
                }),
            }),
        }
    }
}

const fn download_event_name(event: &DownloadEvent) -> &'static str {
    match event {
        DownloadEvent::FileStart { .. } => "file start",
        DownloadEvent::ResumeValidationStarted { .. } => "resume validation start",
        DownloadEvent::Progress { .. } => "progress",
        DownloadEvent::VerificationProgress { .. } => "verification progress",
        DownloadEvent::VerificationProgressForOperation { .. } => "verification progress",
        DownloadEvent::ResumeReused { .. } => "resume reuse",
        DownloadEvent::ResumeReverified { .. } => "resume reverify",
        DownloadEvent::ResumeReverifiedForOperation { .. } => "resume reverify",
        DownloadEvent::CompletedFileVerified { .. } => "completed-file verification",
        DownloadEvent::CompletedFileVerifiedForOperation { .. } => "completed-file verification",
        DownloadEvent::VerificationSkipped { .. } => "verification skip",
        DownloadEvent::VerificationFailed { .. } => "verification failure",
        DownloadEvent::FileComplete { .. } => "file complete",
        DownloadEvent::FileCancelled { .. } => "file cancellation",
        DownloadEvent::FileError { .. } => "file error",
        DownloadEvent::ScopeError { .. } => "scope error",
        DownloadEvent::LoginResult { .. } => "login result",
        DownloadEvent::FilesCollected { .. } => "files collected",
        DownloadEvent::UrlQueued { .. } => "URL queued",
        DownloadEvent::FileQueued(_) => "file queued",
        DownloadEvent::UrlResolved { .. } => "URL resolved",
        DownloadEvent::StatusMessage(_) => "status message",
        DownloadEvent::UrlsReceived { .. } => "URLs received",
        DownloadEvent::ProgressWakeup => "progress wakeup",
    }
}

/// Channel endpoints consumed by the background download task.
pub struct AuthenticatedClient {
    mega: mega::Client,
    http: reqwest::Client,
}

impl AuthenticatedClient {
    pub(crate) const fn new(mega: mega::Client, http: reqwest::Client) -> Self {
        Self { mega, http }
    }

    pub(crate) fn into_parts(self) -> (mega::Client, reqwest::Client) {
        (self.mega, self.http)
    }
}

pub struct DownloadChannels {
    pub client_rx: Option<tokio::sync::oneshot::Receiver<AuthenticatedClient>>,
    pub event_tx: DownloadEventSender,
    pub url_rx: mpsc::Receiver<DownloadRequest>,
    pub token_tx: mpsc::Sender<TokenMessage>,
    pub pause_rx: tokio::sync::watch::Receiver<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadRequest {
    SubmitUrl {
        url: String,
        /// Retained file generations, including deleted paths. New paths start at zero.
        attempt_ids: HashMap<FileId, DownloadAttemptId>,
    },
    ResumeFileIds {
        source_url: String,
        file_ids: Vec<FileId>,
        attempt_ids: HashMap<FileId, DownloadAttemptId>,
    },
    ReverifyFileIds {
        source_url: String,
        file_ids: Vec<FileId>,
    },
    ReverifyFileIdsWithOperations {
        source_url: String,
        file_ids: Vec<FileId>,
        operation_ids: HashMap<FileId, VerificationOperationId>,
    },
    VerifyCompletedFileIdsWithOperations {
        source_url: String,
        file_ids: Vec<FileId>,
        operation_ids: HashMap<FileId, VerificationOperationId>,
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
        attempt_id: DownloadAttemptId,
    },
    ResumeValidationStarted {
        id: FileId,
        attempt_id: DownloadAttemptId,
    },
    Progress {
        id: FileId,
        delta: ProgressDelta,
        attempt_id: DownloadAttemptId,
    },
    VerificationProgress {
        id: FileId,
        bytes_delta: u64,
    },
    VerificationProgressForOperation {
        id: FileId,
        operation_id: VerificationOperationId,
        bytes_delta: u64,
    },
    ResumeReused {
        id: FileId,
        chunks: usize,
        bytes: u64,
        attempt_id: DownloadAttemptId,
    },
    ResumeReverified {
        id: FileId,
        chunks: usize,
        bytes: u64,
    },
    ResumeReverifiedForOperation {
        id: FileId,
        operation_id: VerificationOperationId,
        chunks: usize,
        bytes: u64,
    },
    CompletedFileVerified {
        id: FileId,
        bytes: u64,
    },
    CompletedFileVerifiedForOperation {
        id: FileId,
        operation_id: VerificationOperationId,
        bytes: u64,
    },
    VerificationSkipped {
        id: FileId,
        completed: bool,
    },
    VerificationFailed {
        id: FileId,
        operation_id: VerificationOperationId,
        error: String,
    },
    FileComplete {
        id: FileId,
        attempt_id: DownloadAttemptId,
    },
    FileCancelled {
        id: FileId,
        attempt_id: DownloadAttemptId,
    },
    FileError {
        id: FileId,
        error: String,
        attempt_id: DownloadAttemptId,
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
    ProgressWakeup,
}

pub struct TuiProgress {
    pub tx: DownloadEventSender,
    ids: Mutex<TrackedFileIds>,
    default_attempt_id: DownloadAttemptId,
    attempt_ids: HashMap<FileId, DownloadAttemptId>,
}

impl TuiProgress {
    #[cfg(test)]
    pub fn new<E>(tx: E) -> Self
    where
        E: Into<DownloadEventSender>,
    {
        Self::with_attempt_ids(tx, DownloadAttemptId::new(0), HashMap::new())
    }

    pub fn with_attempt_ids<E>(
        tx: E,
        default_attempt_id: DownloadAttemptId,
        attempt_ids: HashMap<FileId, DownloadAttemptId>,
    ) -> Self
    where
        E: Into<DownloadEventSender>,
    {
        Self {
            tx: tx.into(),
            ids: Mutex::new(TrackedFileIds {
                by_name: HashMap::new(),
                order: VecDeque::new(),
            }),
            default_attempt_id,
            attempt_ids,
        }
    }

    fn intern_id(&self, name: &str) -> FileId {
        let mut ids = self.ids.lock().unwrap();
        if let Some(id) = ids.by_name.get(name) {
            return id.clone();
        }
        let id = FileId::from(name);
        if ids.by_name.len() >= MAX_TRACKED_FILE_IDS
            && let Some(oldest) = ids.order.pop_front()
        {
            ids.by_name.remove(&oldest);
        }
        ids.by_name.insert(name.to_string(), id.clone());
        ids.order.push_back(name.to_string());
        id
    }

    fn attempt_id(&self, id: &FileId) -> DownloadAttemptId {
        self.attempt_ids
            .get(id)
            .copied()
            .unwrap_or(self.default_attempt_id)
    }
}

impl DownloadProgress for TuiProgress {
    fn on_file_start(&self, name: &str, size: u64) {
        let id = self.intern_id(name);
        let attempt_id = self.attempt_id(&id);
        let _ = self.tx.send(DownloadEvent::FileStart {
            id,
            size,
            attempt_id,
        });
    }

    fn on_resume_validation_start(&self, name: &str) {
        let id = self.intern_id(name);
        let attempt_id = self.attempt_id(&id);
        let _ = self
            .tx
            .send(DownloadEvent::ResumeValidationStarted { id, attempt_id });
    }

    fn on_resume_validation_chunk(&self, name: &str, bytes_delta: u64) {
        let id = self.intern_id(name);
        let attempt_id = self.attempt_id(&id);
        let _ = self
            .tx
            .send(DownloadEvent::VerificationProgressForOperation {
                id,
                operation_id: VerificationOperationId::new(attempt_id.raw()),
                bytes_delta,
            });
    }

    fn on_progress(&self, name: &str, delta: ProgressDelta) {
        let id = self.intern_id(name);
        let attempt_id = self.attempt_id(&id);
        let _ = self.tx.send(DownloadEvent::Progress {
            id,
            delta,
            attempt_id,
        });
    }

    fn on_resume_reused(&self, name: &str, chunks: usize, bytes: u64) {
        let id = self.intern_id(name);
        let attempt_id = self.attempt_id(&id);
        let _ = self.tx.send(DownloadEvent::ResumeReused {
            id,
            chunks,
            bytes,
            attempt_id,
        });
    }

    fn on_file_complete(&self, name: &str, _stats: &FileStats) {
        let id = self.intern_id(name);
        let attempt_id = self.attempt_id(&id);
        let _ = self.tx.send(DownloadEvent::FileComplete { id, attempt_id });
    }

    fn on_error(&self, name: &str, error: &str) {
        let id = self.intern_id(name);
        let attempt_id = self.attempt_id(&id);
        let _ = self.tx.send(DownloadEvent::FileError {
            id,
            error: error.to_string(),
            attempt_id,
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

    #[test]
    fn authenticated_client_is_owned_until_the_download_task_consumes_it() {
        let http = mega::http_client_builder()
            .expect("the MEGA HTTP client builder should be available")
            .build()
            .expect("the MEGA HTTP client should be constructible");
        let mega = mega::Client::builder()
            .build(http.clone())
            .expect("a client built from the HTTP transport should be valid");
        let authenticated = AuthenticatedClient::new(mega, http);
        let (mega, http) = authenticated.into_parts();

        drop((mega, http));
    }

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

    #[test]
    fn control_events_report_full_without_reordering_or_background_waiters() {
        let (tx, mut rx) = DownloadEventSender::channel_with_capacity(1);
        let first = DownloadEvent::StatusMessage("first".to_string());
        let second = DownloadEvent::StatusMessage("second".to_string());

        tx.send(first).expect("first event should be admitted");
        assert!(matches!(
            tx.send(second),
            Err(mpsc::error::TrySendError::Full(_))
        ));

        assert!(matches!(
            rx.blocking_recv().expect("first event should remain queued"),
            DownloadEvent::StatusMessage(message) if message == "first"
        ));
        tx.send(DownloadEvent::StatusMessage("second".to_string()))
            .expect("retry should be admitted after capacity is freed");
        assert!(matches!(
            rx.blocking_recv().expect("retry should remain ordered"),
            DownloadEvent::StatusMessage(message) if message == "second"
        ));
    }

    #[test]
    fn lifecycle_events_are_retained_and_flushed_in_fifo_order() {
        let (tx, mut rx) = DownloadEventSender::channel_with_capacities(1, 2);
        let first_id = FileId::from("first");
        let second_id = FileId::from("second");
        let third_id = FileId::from("third");
        let fourth_id = FileId::from("fourth");

        tx.send(DownloadEvent::FileStart {
            id: first_id.clone(),
            size: 1,
            attempt_id: crate::tui::event::DownloadAttemptId::new(0),
        })
        .expect("first lifecycle event should enter the channel");
        tx.send(DownloadEvent::FileComplete {
            id: second_id.clone(),
            attempt_id: crate::tui::event::DownloadAttemptId::new(0),
        })
        .expect("first retained lifecycle event should enter the backlog");
        tx.send(DownloadEvent::FileComplete {
            id: third_id.clone(),
            attempt_id: crate::tui::event::DownloadAttemptId::new(0),
        })
        .expect("second retained lifecycle event should enter the backlog");
        tx.send(DownloadEvent::FileComplete {
            id: fourth_id.clone(),
            attempt_id: crate::tui::event::DownloadAttemptId::new(0),
        })
        .expect("state-changing lifecycle events must not be dropped when the channel is full");
        assert!(tx.take_delivery_failure().is_none());

        assert!(matches!(
            rx.blocking_recv().expect("first event should be queued"),
            DownloadEvent::FileStart { id, .. } if id == first_id
        ));
        assert!(tx.flush_lifecycle_events());
        assert!(matches!(
            rx.blocking_recv().expect("first retained event should flush"),
            DownloadEvent::FileComplete { id, .. } if id == second_id
        ));
        assert!(tx.flush_lifecycle_events());
        assert!(matches!(
            rx.blocking_recv().expect("second retained event should flush"),
            DownloadEvent::FileComplete { id, .. } if id == third_id
        ));
        assert!(tx.flush_lifecycle_events());
        assert!(matches!(
            rx.blocking_recv().expect("third retained event should flush"),
            DownloadEvent::FileComplete { id, .. } if id == fourth_id
        ));
        assert!(!tx.has_pending_lifecycle_events());
    }

    #[test]
    fn progress_does_not_overtake_a_retained_lifecycle_event() {
        let (tx, mut rx) = DownloadEventSender::channel_with_capacities(1, 2);
        let lifecycle_id = FileId::from("lifecycle");
        let progress_id = FileId::from("progress");

        tx.send(DownloadEvent::FileStart {
            id: lifecycle_id,
            size: 1,
            attempt_id: crate::tui::event::DownloadAttemptId::new(0),
        })
        .expect("first lifecycle event should enter the channel");
        tx.send(DownloadEvent::FileComplete {
            id: FileId::from("retained"),
            attempt_id: crate::tui::event::DownloadAttemptId::new(0),
        })
        .expect("second lifecycle event should enter the backlog");
        tx.send(DownloadEvent::Progress {
            id: progress_id.clone(),
            delta: ProgressDelta {
                total_bytes_delta: 5,
                network_bytes_delta: 5,
            },
            attempt_id: crate::tui::event::DownloadAttemptId::new(1),
        })
        .expect("progress should remain coalesced while lifecycle is retained");

        assert!(matches!(
            rx.blocking_recv()
                .expect("first lifecycle event should be queued"),
            DownloadEvent::FileStart { .. }
        ));
        assert!(tx.flush_lifecycle_events());
        assert!(matches!(
            rx.blocking_recv()
                .expect("retained lifecycle event should flush"),
            DownloadEvent::FileComplete { .. }
        ));
        assert_eq!(
            tx.take_pending_progress(&rx),
            vec![(
                progress_id,
                ProgressDelta {
                    total_bytes_delta: 5,
                    network_bytes_delta: 5,
                },
                crate::tui::event::DownloadAttemptId::new(1),
            )]
        );
    }

    #[test]
    fn progress_is_coalesced_and_distinct_pending_identities_are_bounded() {
        let (tx, mut rx) = DownloadEventSender::channel_with_capacity(1);
        let first_id = FileId::from("first");
        let second_id = FileId::from("second");

        tx.send(DownloadEvent::Progress {
            id: first_id.clone(),
            delta: ProgressDelta {
                total_bytes_delta: 2,
                network_bytes_delta: 1,
            },
            attempt_id: crate::tui::event::DownloadAttemptId::new(7),
        })
        .expect("first progress should be admitted");
        tx.send(DownloadEvent::Progress {
            id: first_id.clone(),
            delta: ProgressDelta {
                total_bytes_delta: 3,
                network_bytes_delta: 4,
            },
            attempt_id: crate::tui::event::DownloadAttemptId::new(7),
        })
        .expect("same progress identity should coalesce");
        assert!(matches!(
            tx.send(DownloadEvent::Progress {
                id: second_id,
                delta: ProgressDelta {
                    total_bytes_delta: 1,
                    network_bytes_delta: 1,
                },
                attempt_id: crate::tui::event::DownloadAttemptId::new(1),
            }),
            Err(mpsc::error::TrySendError::Full(
                DownloadEvent::Progress { .. }
            ))
        ));

        assert!(matches!(
            rx.blocking_recv()
                .expect("progress wakeup should be queued"),
            DownloadEvent::ProgressWakeup
        ));
        assert_eq!(
            tx.take_pending_progress(&rx),
            vec![(
                first_id,
                ProgressDelta {
                    total_bytes_delta: 5,
                    network_bytes_delta: 5,
                },
                crate::tui::event::DownloadAttemptId::new(7),
            )]
        );
    }

    #[test]
    fn pending_progress_waits_for_starts_already_in_the_channel() {
        let (tx, mut rx) = DownloadEventSender::channel();
        let attempt_id = DownloadAttemptId::new(0);
        tx.send(DownloadEvent::FileStart {
            id: "file.bin".into(),
            size: 10,
            attempt_id,
        })
        .unwrap();
        tx.send(DownloadEvent::Progress {
            id: "file.bin".into(),
            delta: ProgressDelta {
                total_bytes_delta: 5,
                network_bytes_delta: 5,
            },
            attempt_id,
        })
        .unwrap();

        assert!(tx.take_pending_progress(&rx).is_empty());
        assert!(matches!(
            rx.try_recv().unwrap(),
            DownloadEvent::FileStart { .. }
        ));
        assert!(tx.take_pending_progress(&rx).is_empty());
        assert!(matches!(
            rx.try_recv().unwrap(),
            DownloadEvent::ProgressWakeup
        ));
        assert_eq!(tx.take_pending_progress(&rx)[0].1.network_bytes_delta, 5);
    }

    #[test]
    fn full_progress_is_retained_until_the_receiver_drains() {
        let (tx, mut rx) = DownloadEventSender::channel_with_capacity(1);
        let id = FileId::from("retained-progress");
        tx.send(DownloadEvent::StatusMessage("occupy queue".to_string()))
            .expect("status should occupy the queue");

        assert!(matches!(
            tx.send(DownloadEvent::Progress {
                id: id.clone(),
                delta: ProgressDelta {
                    total_bytes_delta: 5,
                    network_bytes_delta: 3,
                },
                attempt_id: crate::tui::event::DownloadAttemptId::new(7),
            }),
            Err(mpsc::error::TrySendError::Full(
                DownloadEvent::Progress { .. }
            ))
        ));

        assert!(matches!(
            rx.blocking_recv().expect("status should remain queued"),
            DownloadEvent::StatusMessage(message) if message == "occupy queue"
        ));
        assert_eq!(
            tx.take_pending_progress(&rx),
            vec![(
                id,
                ProgressDelta {
                    total_bytes_delta: 5,
                    network_bytes_delta: 3,
                },
                crate::tui::event::DownloadAttemptId::new(7),
            )]
        );
    }

    #[test]
    fn closed_event_queue_reports_control_and_progress_failures() {
        let (tx, mut rx) = DownloadEventSender::channel_with_capacity(1);
        rx.close();

        assert!(matches!(
            tx.send(DownloadEvent::StatusMessage("closed".to_string())),
            Err(mpsc::error::TrySendError::Closed(DownloadEvent::StatusMessage(message)))
                if message == "closed"
        ));
        assert!(matches!(
            tx.send(DownloadEvent::Progress {
                id: FileId::from("closed"),
                delta: ProgressDelta {
                    total_bytes_delta: 1,
                    network_bytes_delta: 1,
                },
                attempt_id: crate::tui::event::DownloadAttemptId::new(1),
            }),
            Err(mpsc::error::TrySendError::Closed(
                DownloadEvent::Progress { .. }
            ))
        ));
        assert!(matches!(
            tx.send(DownloadEvent::FileStart {
                id: FileId::from("closed-lifecycle"),
                size: 1,
                attempt_id: crate::tui::event::DownloadAttemptId::new(1),
            }),
            Err(mpsc::error::TrySendError::Closed(
                DownloadEvent::FileStart { .. }
            ))
        ));
        assert!(tx.take_pending_progress(&rx).is_empty());
        assert_eq!(
            tx.take_delivery_failure(),
            Some(DownloadEventDeliveryFailure {
                event: "file start",
                reason: DeliveryFailureReason::ChannelClosed,
            })
        );
    }

    #[test]
    fn file_id_interning_has_a_bounded_identity_cache() {
        let (tx, _rx) = DownloadEventSender::channel();
        let progress = TuiProgress::new(tx);
        for index in 0..=MAX_TRACKED_FILE_IDS {
            progress.intern_id(&format!("file-{index}"));
        }

        let ids = progress.ids.lock().unwrap();
        assert_eq!(ids.by_name.len(), MAX_TRACKED_FILE_IDS);
        assert_eq!(ids.order.len(), MAX_TRACKED_FILE_IDS);
    }

    #[test]
    fn collection_resume_progress_uses_per_file_attempt_identity() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let file_id = FileId::from("resume.bin");
        let progress = TuiProgress::with_attempt_ids(
            tx,
            crate::tui::event::DownloadAttemptId::new(17),
            HashMap::from([(
                file_id.clone(),
                crate::tui::event::DownloadAttemptId::new(4),
            )]),
        );

        progress.on_resume_validation_start(file_id.as_str());
        assert!(matches!(
            rx.blocking_recv().expect("validation start should be emitted"),
            DownloadEvent::ResumeValidationStarted { id, attempt_id }
                if id == file_id && attempt_id == crate::tui::event::DownloadAttemptId::new(4)
        ));

        progress.on_resume_validation_chunk(file_id.as_str(), 32);
        assert!(matches!(
            rx.blocking_recv().expect("validation progress should be emitted"),
            DownloadEvent::VerificationProgressForOperation {
                id,
                operation_id,
                bytes_delta,
            } if id == file_id
                && operation_id == VerificationOperationId::new(4)
                && bytes_delta == 32
        ));

        progress.on_resume_validation_start("new-file.bin");
        assert!(matches!(
            rx.blocking_recv().expect("default validation start should be emitted"),
            DownloadEvent::ResumeValidationStarted { attempt_id, .. } if attempt_id == crate::tui::event::DownloadAttemptId::new(17)
        ));
    }
}
