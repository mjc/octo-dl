use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::core::ProgressDelta;
use crate::progress::CumulativeProgress;
use crate::stats::{DownloadStatsTracker, FileStats};

use super::{LazySidecarWriter, ResumeTracker, SidecarGeneration, SidecarWriterShutdown};

/// Trait for receiving download progress updates.
///
/// Implement this trait to receive callbacks during download operations.
/// All methods have default no-op implementations for convenience.
pub trait DownloadProgress: Send + Sync {
    /// Called when a file download starts.
    fn on_file_start(&self, _name: &str, _size: u64) {}

    /// Called when resume validation starts for a local partial download.
    fn on_resume_validation_start(&self, _name: &str) {}

    /// Called periodically while disk resume validation scans local partial data.
    fn on_resume_validation_progress(&self, _name: &str, _checked_bytes: u64, _total_bytes: u64) {}

    /// Called as local partial data is read during resume validation.
    fn on_resume_validation_chunk(&self, name: &str, bytes_delta: u64) {
        self.on_progress(
            name,
            ProgressDelta {
                total_bytes_delta: bytes_delta,
                network_bytes_delta: 0,
            },
        );
    }

    /// Called periodically with the number of bytes advanced since the last call.
    ///
    /// `total_bytes_delta` includes any locally reused bytes revealed by
    /// resume revalidation, while `network_bytes_delta` counts only fresh
    /// bytes received from the network during this callback interval.
    fn on_progress(&self, _name: &str, _delta: ProgressDelta) {}

    /// Called when a file download completes successfully.
    fn on_file_complete(&self, _name: &str, _stats: &FileStats) {}

    /// Called when a file download fails.
    fn on_error(&self, _name: &str, _error: &str) {}

    /// Called when a partial `.part` file is detected from a previous run.
    fn on_partial_detected(&self, _name: &str, _existing_size: u64, _expected_size: u64) {}

    /// Called when previously verified chunks will be reused.
    fn on_resume_reused(&self, _name: &str, _chunks: usize, _bytes: u64) {}
}

/// A null progress implementation that ignores all events.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoProgress;

impl DownloadProgress for NoProgress {}

pub(super) struct ResumeValidationStatusProgress<'a> {
    inner: &'a dyn DownloadProgress,
}

impl<'a> ResumeValidationStatusProgress<'a> {
    pub(super) const fn new(inner: &'a dyn DownloadProgress) -> Self {
        Self { inner }
    }
}

impl DownloadProgress for ResumeValidationStatusProgress<'_> {
    fn on_resume_validation_progress(&self, name: &str, checked_bytes: u64, total_bytes: u64) {
        self.inner
            .on_resume_validation_progress(name, checked_bytes, total_bytes);
    }

    fn on_resume_validation_chunk(&self, name: &str, bytes_delta: u64) {
        self.inner.on_resume_validation_chunk(name, bytes_delta);
    }
}

pub(super) struct ProgressCallbackState {
    name: String,
    pub(super) stats: DownloadStatsTracker,
    cumulative: CumulativeProgress,
    progress: Arc<dyn DownloadProgress>,
}

impl ProgressCallbackState {
    pub(super) fn new(
        name: String,
        expected_network_bytes: u64,
        trusted_bytes: u64,
        progress: Arc<dyn DownloadProgress>,
    ) -> Self {
        Self {
            name,
            stats: DownloadStatsTracker::new(expected_network_bytes),
            cumulative: CumulativeProgress::with_high_water(trusted_bytes),
            progress,
        }
    }

    fn record_cumulative(&self, cumulative_bytes: u64) {
        let delta = self.cumulative.delta(cumulative_bytes);
        if delta == 0 {
            return;
        }
        let _ = self.stats.record_bytes(delta);
        self.progress.on_progress(
            &self.name,
            ProgressDelta {
                total_bytes_delta: delta,
                network_bytes_delta: delta,
            },
        );
    }
}

pub(super) struct ChunkVerifiedState {
    pub(super) tracker: Mutex<ResumeTracker>,
    pub(super) sidecar_writer: LazySidecarWriter,
    next_generation: AtomicU64,
}

impl ChunkVerifiedState {
    pub(super) fn new(tracker: ResumeTracker, sidecar_writer: LazySidecarWriter) -> Self {
        Self {
            tracker: Mutex::new(tracker),
            sidecar_writer,
            next_generation: AtomicU64::new(0),
        }
    }

    pub(super) fn mark_verified(&self, index: u32, mac: [u8; 16]) {
        let snapshot = {
            let mut guard = self.tracker.lock().unwrap();
            guard.mark_verified(index, mac).then(|| {
                let generation = SidecarGeneration::new(
                    self.next_generation.fetch_add(1, Ordering::Relaxed) + 1,
                );
                (generation, guard.snapshot())
            })
        };
        if let Some((generation, snapshot)) = snapshot {
            self.sidecar_writer
                .persist_verified_snapshot(generation, snapshot);
        }
    }

    pub(super) async fn finish_sidecar_writer(&self, shutdown: SidecarWriterShutdown) {
        if matches!(shutdown, SidecarWriterShutdown::Flush) {
            let snapshot = {
                let guard = self.tracker.lock().unwrap();
                let generation =
                    SidecarGeneration::new(self.next_generation.load(Ordering::Relaxed));
                (generation, guard.snapshot())
            };
            self.sidecar_writer
                .persist_final_snapshot(snapshot.0, snapshot.1);
        }
        self.sidecar_writer.finish(shutdown).await;
    }
}

pub(super) struct DownloadCallbackState {
    pub(super) progress: ProgressCallbackState,
    pub(super) chunk_verified: ChunkVerifiedState,
}

impl DownloadCallbackState {
    pub(super) const fn new(
        progress: ProgressCallbackState,
        chunk_verified: ChunkVerifiedState,
    ) -> Self {
        Self {
            progress,
            chunk_verified,
        }
    }
}

impl mega::ParallelDownloadCallbacks for DownloadCallbackState {
    fn progress(&self, cumulative_bytes: u64) {
        self.progress.record_cumulative(cumulative_bytes);
    }

    fn chunk_verified(&self, index: u32, mac: [u8; 16]) {
        self.chunk_verified.mark_verified(index, mac);
    }

    fn tracks_chunk_verification(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests;
