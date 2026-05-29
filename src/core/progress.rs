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
    use proptest::prelude::*;
    use std::time::{Duration, Instant};

    proptest! {
        #[test]
        fn progress_delta_round_trips_fields(total_bytes_delta in any::<u64>(), network_bytes_delta in any::<u64>()) {
            let delta = ProgressDelta {
                total_bytes_delta,
                network_bytes_delta,
            };
            prop_assert_eq!(delta.total_bytes_delta, total_bytes_delta);
            prop_assert_eq!(delta.network_bytes_delta, network_bytes_delta);
        }

        #[test]
        fn throughput_weight_stays_in_unit_interval(
            decay_secs in 1u64..1_001,
            elapsed_secs in 0u64..10_001,
        ) {
            let weight =
                super::throughput_weight(Duration::from_secs(elapsed_secs), Duration::from_secs(decay_secs));
            prop_assert!(weight.is_finite());
            prop_assert!(weight >= 0.0);
            prop_assert!(weight <= 1.0);
        }

        #[test]
        fn throughput_weight_decreases_with_elapsed(
            decay_secs in 1u64..1_001,
            first_secs in 0u64..5_001,
            extra_secs in 0u64..5_001,
        ) {
            let decay = Duration::from_secs(decay_secs);
            let first = super::throughput_weight(Duration::from_secs(first_secs), decay);
            let second = super::throughput_weight(Duration::from_secs(first_secs + extra_secs), decay);
            prop_assert!(second <= first);
        }

        #[test]
        fn rate_estimator_reports_zero_before_min_sample_span(
            total in any::<u64>(),
            millis in 0u64..1_000,
        ) {
            let mut rate = RateEstimator::default();
            let start = Instant::now();
            rate.reset(total, start);
            prop_assert_eq!(rate.bytes_per_sec(start + Duration::from_millis(millis)), 0);
        }

        #[test]
        fn rate_estimator_reports_positive_after_forward_progress(
            start_total in 0u64..1_000_001,
            delta in 1u64..1_000_001,
            secs in 1u64..121,
        ) {
            let mut rate = RateEstimator::default();
            let start = Instant::now();
            let now = start + Duration::from_secs(secs);
            rate.reset(start_total, start);
            rate.record(start_total + delta, now);
            prop_assert!(rate.bytes_per_sec(now) > 0);
        }

        #[test]
        fn rate_estimator_resets_when_total_goes_backwards(
            start_total in 1u64..1_000_001,
            lower_total in 0u64..1_000_001,
            secs in 1u64..121,
        ) {
            prop_assume!(lower_total < start_total);
            let mut rate = RateEstimator::default();
            let start = Instant::now();
            let now = start + Duration::from_secs(secs);
            rate.reset(start_total, start);
            rate.record(lower_total, now);
            prop_assert_eq!(rate.bytes_per_sec(now + Duration::from_secs(1)), 0);
        }

        #[test]
        fn rate_estimator_resets_when_time_goes_backwards(
            delta in 1u64..1_000_001,
            initial_secs in 2u64..121,
            earlier_secs in 0u64..120,
        ) {
            prop_assume!(earlier_secs < initial_secs);
            let mut rate = RateEstimator::default();
            let start = Instant::now();
            let first = start + Duration::from_secs(initial_secs);
            let earlier = start + Duration::from_secs(earlier_secs);
            rate.reset(0, start);
            rate.record(delta, first);
            prop_assert!(rate.bytes_per_sec(first) > 0);
            rate.record(delta.saturating_add(1), earlier);
            prop_assert_eq!(rate.bytes_per_sec(earlier + Duration::from_secs(1)), 0);
        }
    }
}
