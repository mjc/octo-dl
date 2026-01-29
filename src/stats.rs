//! Download statistics types.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Statistics for a single file download.
#[derive(Debug, Clone)]
pub struct FileStats {
    /// Total size of the file in bytes.
    pub size: u64,
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
    /// Total bytes downloaded.
    pub total_bytes: u64,
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
            elapsed: Duration::ZERO,
            peak_speed: 0,
            average_ramp_up: None,
        }
    }

    /// Returns the average download speed in bytes per second.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn average_speed(&self) -> u64 {
        let secs = self.elapsed.as_secs_f64();
        if secs > 0.0 {
            (self.total_bytes as f64 / secs) as u64
        } else {
            0
        }
    }
}

/// Internal helper for tracking per-file download statistics during download.
pub struct DownloadStatsTracker {
    start_time: Instant,
    total_bytes: u64,
    downloaded: AtomicU64,
    peak_speed: AtomicU64,
    time_to_80pct_ms: AtomicU64,
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
            time_to_80pct_ms: AtomicU64::new(0),
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
        let peak = prev_peak.max(speed);

        // Track time to reach 80% of peak
        if self.time_to_80pct_ms.load(Ordering::Relaxed) == 0 && speed >= peak * 4 / 5 {
            // u128 -> u64: saturate at MAX for durations > 584 million years
            let ms = self
                .start_time
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX);
            self.time_to_80pct_ms.store(ms, Ordering::Relaxed);
        }
    }

    /// Returns the elapsed time since the download started.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.start_time.elapsed()
    }

    /// Returns the average speed in bytes per second.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn average_speed(&self) -> u64 {
        let secs = self.elapsed().as_secs_f64();
        if secs > 0.0 {
            (self.total_bytes as f64 / secs) as u64
        } else {
            0
        }
    }

    /// Returns the peak speed recorded.
    #[must_use]
    pub fn peak_speed(&self) -> u64 {
        self.peak_speed.load(Ordering::Relaxed)
    }

    /// Returns the time to reach 80% of peak speed, if achieved.
    #[must_use]
    pub fn time_to_80pct(&self) -> Option<Duration> {
        let ms = self.time_to_80pct_ms.load(Ordering::Relaxed);
        if ms > 0 {
            Some(Duration::from_millis(ms))
        } else {
            None
        }
    }

    /// Converts this tracker into final file statistics.
    #[must_use]
    pub fn into_file_stats(self) -> FileStats {
        FileStats {
            size: self.total_bytes,
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
            elapsed: self.start_time.elapsed(),
            peak_speed: self.peak_speed,
            average_ramp_up,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let tracker = DownloadStatsTracker::new(1000);
        // Start with a low speed, then ramp up
        tracker.update_speed(10);
        // Sleep briefly to ensure elapsed time > 0
        std::thread::sleep(Duration::from_millis(2));
        // Now hit 80% of the eventual peak (500)
        tracker.update_speed(500);
        // 400 >= 500 * 4 / 5 = 400, so we should record the time
        assert!(tracker.time_to_80pct().is_some());
    }

    #[test]
    fn session_stats_builder() {
        let mut builder = SessionStatsBuilder::new();
        builder.set_skipped(2);
        builder.set_peak_speed(1000);

        let file_stats = FileStats {
            size: 500,
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
        assert_eq!(stats.peak_speed, 1000);
        assert!(stats.average_ramp_up.is_some());
    }
}
