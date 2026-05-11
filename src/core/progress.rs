use std::time::{Duration, Instant};

use crate::core::model::FileId;
use crate::stats::FileStats;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProgressDelta {
    pub total_bytes_delta: u64,
    pub network_bytes_delta: u64,
}

pub trait DownloadProgressSink: Send + Sync {
    fn on_file_start(&self, _file_id: &FileId, _size: u64) {}
    fn on_progress(&self, _file_id: &FileId, _delta: ProgressDelta) {}
    fn on_reuse_detected(&self, _file_id: &FileId, _reused_chunks: usize, _reused_bytes: u64) {}
    fn on_complete(&self, _file_id: &FileId, _stats: &FileStats) {}
    fn on_error(&self, _file_id: &FileId, _error: &str) {}
}

#[derive(Debug, Clone)]
pub struct RateEstimator {
    start_time: Option<Instant>,
    last_time: Option<Instant>,
    last_total: u64,
    smoothed_bytes_per_sec: f64,
    double_smoothed_bytes_per_sec: f64,
    min_sample_span: Duration,
    decay: Duration,
}

impl Default for RateEstimator {
    fn default() -> Self {
        Self {
            start_time: None,
            last_time: None,
            last_total: 0,
            smoothed_bytes_per_sec: 0.0,
            double_smoothed_bytes_per_sec: 0.0,
            min_sample_span: Duration::from_secs(1),
            decay: Duration::from_secs(30),
        }
    }
}

impl RateEstimator {
    pub fn reset(&mut self, total: u64, now: Instant) {
        self.start_time = Some(now);
        self.last_time = Some(now);
        self.last_total = total;
        self.smoothed_bytes_per_sec = 0.0;
        self.double_smoothed_bytes_per_sec = 0.0;
    }

    pub fn record(&mut self, total: u64, now: Instant) {
        let Some(last_time) = self.last_time else {
            self.reset(total, now);
            return;
        };
        if total < self.last_total || now <= last_time {
            self.reset(total, now);
            return;
        }
        if total == self.last_total {
            return;
        }

        let delta_bytes = total - self.last_total;
        let delta_secs = now.duration_since(last_time).as_secs_f64();
        if delta_secs <= f64::EPSILON {
            return;
        }

        let instant_bps = delta_bytes as f64 / delta_secs;
        let weight = throughput_weight(now.duration_since(last_time), self.decay);
        self.smoothed_bytes_per_sec = self
            .smoothed_bytes_per_sec
            .mul_add(weight, instant_bps * (1.0 - weight));

        let sample_span = self
            .start_time
            .map_or(Duration::ZERO, |start| now.duration_since(start));
        let total_weight = 1.0 - throughput_weight(sample_span, self.decay);
        if total_weight > f64::EPSILON {
            let normalized = self.smoothed_bytes_per_sec / total_weight;
            self.double_smoothed_bytes_per_sec = self
                .double_smoothed_bytes_per_sec
                .mul_add(weight, normalized * (1.0 - weight));
        }

        self.last_total = total;
        self.last_time = Some(now);
    }

    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn bytes_per_sec(&self, now: Instant) -> u64 {
        let Some(start) = self.start_time else {
            return 0;
        };
        let Some(last_time) = self.last_time else {
            return 0;
        };
        let sample_span = now.duration_since(start);
        if sample_span < self.min_sample_span {
            return 0;
        }

        let total_weight = 1.0 - throughput_weight(sample_span, self.decay);
        if total_weight <= f64::EPSILON {
            return 0;
        }

        let reweight = throughput_weight(now.duration_since(last_time), self.decay);
        let single_smoothed = self.smoothed_bytes_per_sec * reweight / total_weight;
        let double_smoothed = self
            .double_smoothed_bytes_per_sec
            .mul_add(reweight, single_smoothed * (1.0 - reweight));
        let bps = double_smoothed / total_weight;
        if !bps.is_finite() || bps < 1.0 {
            return 0;
        }
        if bps >= u64::MAX as f64 {
            u64::MAX
        } else {
            bps as u64
        }
    }
}

#[allow(clippy::cast_precision_loss)]
fn throughput_weight(elapsed: Duration, decay: Duration) -> f64 {
    0.1_f64.powf(elapsed.as_secs_f64() / decay.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::{ProgressDelta, RateEstimator};
    use std::time::{Duration, Instant};

    #[test]
    fn progress_delta_tracks_network_separately() {
        let delta = ProgressDelta {
            total_bytes_delta: 300,
            network_bytes_delta: 120,
        };
        assert_eq!(delta.total_bytes_delta, 300);
        assert_eq!(delta.network_bytes_delta, 120);
    }

    #[test]
    fn rate_estimator_uses_only_recorded_total() {
        let mut rate = RateEstimator::default();
        let start = Instant::now();
        rate.reset(0, start);
        rate.record(100, start + Duration::from_secs(2));
        assert!(rate.bytes_per_sec(start + Duration::from_secs(2)) > 0);
    }
}
