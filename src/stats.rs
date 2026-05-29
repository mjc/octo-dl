//! Download statistics types.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Computes average speed in bytes per second from total bytes and elapsed time.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn compute_average_speed(total_bytes: u64, elapsed: Duration) -> u64 {
    let secs = elapsed.as_secs_f64();
    if secs > 0.0 {
        (total_bytes as f64 / secs) as u64
    } else {
        0
    }
}

/// Statistics for a single file download.
#[derive(Debug, Clone)]
pub struct FileStats {
    /// Completed file size in bytes.
    pub size: u64,
    /// Bytes fetched from the network during this run.
    pub network_bytes: u64,
    /// Bytes reused from verified partial chunks.
    pub reused_bytes: u64,
    /// Time taken to download the file.
    pub elapsed: Duration,
    /// Average download speed in bytes per second.
    pub average_speed: u64,
    /// Peak download speed in bytes per second.
    pub peak_speed: u64,
    /// Time to reach 80% of peak speed (ramp-up time).
    pub ramp_up_time: Option<Duration>,
}

/// Statistics for an entire download session.
#[derive(Debug, Clone)]
pub struct SessionStats {
    /// Number of files successfully downloaded.
    pub files_downloaded: usize,
    /// Number of files skipped (already existed).
    pub files_skipped: usize,
    /// Total completed file size.
    pub total_bytes: u64,
    /// Bytes fetched from the network during this run.
    pub network_bytes: u64,
    /// Bytes reused from verified partial chunks.
    pub reused_bytes: u64,
    /// Total elapsed time for the session.
    pub elapsed: Duration,
    /// Peak aggregate download speed in bytes per second.
    pub peak_speed: u64,
    /// Average ramp-up time across all files.
    pub average_ramp_up: Option<Duration>,
}

impl Default for SessionStats {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStats {
    /// Creates a new empty session stats.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            files_downloaded: 0,
            files_skipped: 0,
            total_bytes: 0,
            network_bytes: 0,
            reused_bytes: 0,
            elapsed: Duration::ZERO,
            peak_speed: 0,
            average_ramp_up: None,
        }
    }

    /// Returns the average download speed in bytes per second.
    #[must_use]
    pub fn average_speed(&self) -> u64 {
        compute_average_speed(self.network_bytes, self.elapsed)
    }
}

/// Internal helper for tracking per-file download statistics during download.
pub struct DownloadStatsTracker {
    start_time: Instant,
    total_bytes: u64,
    downloaded: AtomicU64,
    peak_speed: AtomicU64,
    peak_history: Mutex<Vec<(u64, u64)>>,
}

impl DownloadStatsTracker {
    /// Creates a new stats tracker for a file of the given size.
    #[must_use]
    pub fn new(total_bytes: u64) -> Self {
        Self {
            start_time: Instant::now(),
            total_bytes,
            downloaded: AtomicU64::new(0),
            peak_speed: AtomicU64::new(0),
            peak_history: Mutex::new(Vec::new()),
        }
    }

    /// Records downloaded bytes and computes current speed (bytes/sec).
    ///
    /// Returns the computed speed.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn record_bytes(&self, bytes: u64) -> u64 {
        self.downloaded.fetch_add(bytes, Ordering::Relaxed);
        let total = self.downloaded.load(Ordering::Relaxed);
        let secs = self.start_time.elapsed().as_secs_f64();
        if secs > 0.0 {
            let speed = (total as f64 / secs) as u64;
            self.update_speed(speed);
            speed
        } else {
            0
        }
    }

    /// Updates the speed tracker with the current speed.
    /// Tracks peak speed and time to reach 80% of peak.
    pub fn update_speed(&self, speed: u64) {
        let prev_peak = self.peak_speed.fetch_max(speed, Ordering::Relaxed);
        if speed > prev_peak {
            // Record only new peak samples; the earliest sample at >= 80% of the
            // final peak must also be a running peak.
            let ms = self
                .start_time
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX);
            self.peak_history.lock().unwrap().push((speed, ms));
        }
    }

    /// Returns the elapsed time since the download started.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.start_time.elapsed()
    }

    /// Returns the average speed in bytes per second.
    #[must_use]
    pub fn average_speed(&self) -> u64 {
        compute_average_speed(self.total_bytes, self.elapsed())
    }

    /// Returns bytes recorded as fetched from the network.
    #[must_use]
    pub fn downloaded_bytes(&self) -> u64 {
        self.downloaded.load(Ordering::Relaxed)
    }

    /// Returns the peak speed recorded.
    #[must_use]
    pub fn peak_speed(&self) -> u64 {
        self.peak_speed.load(Ordering::Relaxed)
    }

    /// Returns the time to reach 80% of peak speed, if achieved.
    #[must_use]
    pub fn time_to_80pct(&self) -> Option<Duration> {
        let peak = self.peak_speed();
        if peak == 0 {
            return None;
        }
        self.peak_history
            .lock()
            .unwrap()
            .iter()
            .find(|(speed, _)| u128::from(*speed) * 5 >= u128::from(peak) * 4)
            .map(|(_, ms)| Duration::from_millis(*ms))
    }

    /// Converts this tracker into final file statistics.
    #[must_use]
    pub fn into_file_stats(self) -> FileStats {
        FileStats {
            size: self.total_bytes,
            network_bytes: self.downloaded.load(Ordering::Relaxed),
            reused_bytes: 0,
            elapsed: self.elapsed(),
            average_speed: self.average_speed(),
            peak_speed: self.peak_speed(),
            ramp_up_time: self.time_to_80pct(),
        }
    }
}

/// Builder for accumulating session statistics during downloads.
pub struct SessionStatsBuilder {
    files_downloaded: usize,
    files_skipped: usize,
    total_bytes: u64,
    network_bytes: u64,
    reused_bytes: u64,
    start_time: Instant,
    peak_speed: u64,
    total_ramp_up_ms: u64,
    ramp_up_count: u64,
}

impl Default for SessionStatsBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStatsBuilder {
    /// Creates a new session stats builder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            files_downloaded: 0,
            files_skipped: 0,
            total_bytes: 0,
            network_bytes: 0,
            reused_bytes: 0,
            start_time: Instant::now(),
            peak_speed: 0,
            total_ramp_up_ms: 0,
            ramp_up_count: 0,
        }
    }

    /// Sets the number of skipped files.
    pub const fn set_skipped(&mut self, count: usize) {
        self.files_skipped = count;
    }

    /// Sets the peak speed observed.
    pub const fn set_peak_speed(&mut self, speed: u64) {
        self.peak_speed = speed;
    }

    /// Records a completed file download.
    pub fn add_download(&mut self, file_stats: &FileStats) {
        self.files_downloaded += 1;
        self.total_bytes += file_stats.size;
        self.network_bytes += file_stats.network_bytes;
        self.reused_bytes += file_stats.reused_bytes;
        if let Some(ramp) = file_stats.ramp_up_time {
            self.total_ramp_up_ms += ramp.as_millis().try_into().unwrap_or(u64::MAX);
            self.ramp_up_count += 1;
        }
    }

    /// Builds the final session statistics.
    #[must_use]
    pub fn build(self) -> SessionStats {
        let average_ramp_up = if self.ramp_up_count > 0 {
            Some(Duration::from_millis(
                self.total_ramp_up_ms / self.ramp_up_count,
            ))
        } else {
            None
        };

        SessionStats {
            files_downloaded: self.files_downloaded,
            files_skipped: self.files_skipped,
            total_bytes: self.total_bytes,
            network_bytes: self.network_bytes,
            reused_bytes: self.reused_bytes,
            elapsed: self.start_time.elapsed(),
            peak_speed: self.peak_speed,
            average_ramp_up,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::{collection::vec, option, prelude::*};

    fn file_stats_strategy() -> impl Strategy<Value = FileStats> {
        (
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
            0u64..10_001,
            any::<u32>(),
            any::<u32>(),
            option::of(0u64..10_001),
        )
            .prop_map(
                |(
                    size,
                    network_bytes,
                    reused_bytes,
                    elapsed_secs,
                    average_speed,
                    peak_speed,
                    ramp_up_ms,
                )| FileStats {
                    size: u64::from(size),
                    network_bytes: u64::from(network_bytes),
                    reused_bytes: u64::from(reused_bytes),
                    elapsed: Duration::from_secs(elapsed_secs),
                    average_speed: u64::from(average_speed),
                    peak_speed: u64::from(peak_speed),
                    ramp_up_time: ramp_up_ms.map(Duration::from_millis),
                },
            )
    }

    #[test]
    fn session_stats_default() {
        let stats = SessionStats::default();
        assert_eq!(stats.files_downloaded, 0);
        assert_eq!(stats.files_skipped, 0);
        assert_eq!(stats.total_bytes, 0);
    }

    #[test]
    fn session_stats_average_speed_zero_elapsed() {
        let stats = SessionStats {
            files_downloaded: 1,
            files_skipped: 0,
            total_bytes: 1000,
            network_bytes: 0,
            reused_bytes: 0,
            elapsed: Duration::ZERO,
            peak_speed: 0,
            average_ramp_up: None,
        };
        assert_eq!(stats.average_speed(), 0);
    }

    #[test]
    fn session_stats_average_speed() {
        let stats = SessionStats {
            files_downloaded: 1,
            files_skipped: 0,
            total_bytes: 1000,
            network_bytes: 1000,
            reused_bytes: 0,
            elapsed: Duration::from_secs(2),
            peak_speed: 600,
            average_ramp_up: None,
        };
        assert_eq!(stats.average_speed(), 500);
    }

    #[test]
    fn download_stats_tracker_peak_speed() {
        let tracker = DownloadStatsTracker::new(1000);
        tracker.update_speed(100);
        tracker.update_speed(500);
        tracker.update_speed(300);
        assert_eq!(tracker.peak_speed(), 500);
    }

    #[test]
    fn download_stats_tracker_time_to_80pct() {
        let tracker = DownloadStatsTracker {
            start_time: Instant::now() - Duration::from_millis(2),
            total_bytes: 1000,
            downloaded: AtomicU64::new(0),
            peak_speed: AtomicU64::new(0),
            peak_history: Mutex::new(Vec::new()),
        };
        tracker.update_speed(10);
        tracker.update_speed(400);
        let ramp_up = tracker
            .time_to_80pct()
            .expect("400 should reach 80% of peak");
        tracker.update_speed(500);

        assert_eq!(tracker.time_to_80pct(), Some(ramp_up));
    }

    #[test]
    fn session_stats_builder() {
        let mut builder = SessionStatsBuilder::new();
        builder.set_skipped(2);
        builder.set_peak_speed(1000);

        let file_stats = FileStats {
            size: 500,
            network_bytes: 300,
            reused_bytes: 200,
            elapsed: Duration::from_secs(1),
            average_speed: 500,
            peak_speed: 600,
            ramp_up_time: Some(Duration::from_millis(200)),
        };
        builder.add_download(&file_stats);

        let stats = builder.build();
        assert_eq!(stats.files_downloaded, 1);
        assert_eq!(stats.files_skipped, 2);
        assert_eq!(stats.total_bytes, 500);
        assert_eq!(stats.network_bytes, 300);
        assert_eq!(stats.reused_bytes, 200);
        assert_eq!(stats.peak_speed, 1000);
        assert!(stats.average_ramp_up.is_some());
    }

    proptest! {
        #[test]
        fn compute_average_speed_matches_integer_division_for_whole_seconds(
            total_bytes in 0u64..1_000_000_000_000,
            elapsed_secs in 1u64..10_001,
        ) {
            prop_assert_eq!(
                compute_average_speed(total_bytes, Duration::from_secs(elapsed_secs)),
                total_bytes / elapsed_secs
            );
        }

        #[test]
        fn session_stats_average_speed_matches_integer_division_for_whole_seconds(
            network_bytes in 0u64..1_000_000_000_000,
            elapsed_secs in 1u64..10_001,
        ) {
            let stats = SessionStats {
                files_downloaded: 0,
                files_skipped: 0,
                total_bytes: 0,
                network_bytes,
                reused_bytes: 0,
                elapsed: Duration::from_secs(elapsed_secs),
                peak_speed: 0,
                average_ramp_up: None,
            };

            prop_assert_eq!(stats.average_speed(), network_bytes / elapsed_secs);
        }

        #[test]
        fn session_stats_builder_aggregates_generated_file_stats_exactly(
            skipped in any::<u16>(),
            peak_speed in any::<u64>(),
            files in vec(file_stats_strategy(), 0..32),
        ) {
            let mut builder = SessionStatsBuilder::new();
            builder.set_skipped(usize::from(skipped));
            builder.set_peak_speed(peak_speed);

            let mut expected_total_bytes = 0u64;
            let mut expected_network_bytes = 0u64;
            let mut expected_reused_bytes = 0u64;
            let mut ramp_up_total_ms = 0u64;
            let mut ramp_up_count = 0u64;

            for file in &files {
                builder.add_download(file);
                expected_total_bytes += file.size;
                expected_network_bytes += file.network_bytes;
                expected_reused_bytes += file.reused_bytes;
                if let Some(ramp) = file.ramp_up_time {
                    ramp_up_total_ms += ramp.as_millis().try_into().unwrap_or(u64::MAX);
                    ramp_up_count += 1;
                }
            }

            let stats = builder.build();
            let expected_average_ramp_up = if ramp_up_count == 0 {
                None
            } else {
                Some(Duration::from_millis(ramp_up_total_ms / ramp_up_count))
            };

            prop_assert_eq!(stats.files_downloaded, files.len());
            prop_assert_eq!(stats.files_skipped, usize::from(skipped));
            prop_assert_eq!(stats.total_bytes, expected_total_bytes);
            prop_assert_eq!(stats.network_bytes, expected_network_bytes);
            prop_assert_eq!(stats.reused_bytes, expected_reused_bytes);
            prop_assert_eq!(stats.peak_speed, peak_speed);
            prop_assert_eq!(stats.average_ramp_up, expected_average_ramp_up);
        }
    }
}
