use std::sync::atomic::{AtomicU64, Ordering};

/// Tracks a cumulative byte counter and yields monotonic deltas.
#[derive(Debug, Default)]
pub struct CumulativeProgress {
    high_water: AtomicU64,
}

impl CumulativeProgress {
    #[must_use]
    pub const fn with_high_water(high_water: u64) -> Self {
        Self {
            high_water: AtomicU64::new(high_water),
        }
    }

    #[must_use]
    pub fn delta(&self, cumulative: u64) -> u64 {
        let previous = self.high_water.fetch_max(cumulative, Ordering::Relaxed);
        cumulative.saturating_sub(previous)
    }
}

#[cfg(test)]
mod tests {
    use super::CumulativeProgress;
    use proptest::{collection::vec, prelude::*};

    #[test]
    fn cumulative_progress_yields_true_deltas() {
        let tracker = CumulativeProgress::with_high_water(0);
        assert_eq!(tracker.delta(100), 100);
        assert_eq!(tracker.delta(350), 250);
        assert_eq!(tracker.delta(700), 350);
        assert_eq!(tracker.delta(1_000), 300);
    }

    #[test]
    fn cumulative_progress_ignores_regressions() {
        let tracker = CumulativeProgress::with_high_water(0);
        assert_eq!(tracker.delta(200), 200);
        assert_eq!(tracker.delta(150), 0);
        assert_eq!(tracker.delta(250), 50);
    }

    #[test]
    fn cumulative_progress_can_start_after_reused_bytes() {
        let tracker = CumulativeProgress::with_high_water(500);
        assert_eq!(tracker.delta(500), 0);
        assert_eq!(tracker.delta(700), 200);
    }

    #[test]
    fn cumulative_progress_ignores_initial_high_water_callback() {
        let tracker = CumulativeProgress::with_high_water(1_024);
        assert_eq!(tracker.delta(1_024), 0);
    }

    #[test]
    fn cumulative_progress_reports_network_bytes_after_high_water() {
        let tracker = CumulativeProgress::with_high_water(1_024);
        assert_eq!(tracker.delta(1_536), 512);
    }

    #[test]
    fn cumulative_progress_ignores_duplicate_or_out_of_order_totals_after_high_water() {
        let tracker = CumulativeProgress::with_high_water(1_024);
        assert_eq!(tracker.delta(1_024), 0);
        assert_eq!(tracker.delta(1_023), 0);
        assert_eq!(tracker.delta(1_280), 256);
        assert_eq!(tracker.delta(1_152), 0);
        assert_eq!(tracker.delta(1_536), 256);
    }

    proptest! {
        #[test]
        fn cumulative_progress_matches_max_tracking_oracle(
            initial_high_water in any::<u64>(),
            cumulatives in vec(any::<u64>(), 0..64),
        ) {
            let tracker = CumulativeProgress::with_high_water(initial_high_water);
            let mut expected_high_water = initial_high_water;

            for cumulative in cumulatives {
                let expected_delta = cumulative.saturating_sub(expected_high_water);
                prop_assert_eq!(tracker.delta(cumulative), expected_delta);
                expected_high_water = expected_high_water.max(cumulative);
            }
        }

        #[test]
        fn cumulative_progress_total_growth_matches_final_high_water(
            initial_high_water in any::<u64>(),
            cumulatives in vec(any::<u64>(), 0..64),
        ) {
            let tracker = CumulativeProgress::with_high_water(initial_high_water);
            let total_delta: u128 = cumulatives
                .iter()
                .map(|&cumulative| u128::from(tracker.delta(cumulative)))
                .sum();
            let final_high_water = cumulatives
                .iter()
                .copied()
                .fold(initial_high_water, u64::max);

            prop_assert_eq!(
                total_delta,
                u128::from(final_high_water.saturating_sub(initial_high_water))
            );
        }
    }
}
