use std::sync::atomic::{AtomicU64, Ordering};

/// Tracks a cumulative byte counter and yields monotonic deltas.
#[derive(Debug, Default)]
pub struct CumulativeProgress {
    high_water: AtomicU64,
}

impl CumulativeProgress {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            high_water: AtomicU64::new(0),
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

    #[test]
    fn cumulative_progress_yields_true_deltas() {
        let tracker = CumulativeProgress::new();
        assert_eq!(tracker.delta(100), 100);
        assert_eq!(tracker.delta(350), 250);
        assert_eq!(tracker.delta(700), 350);
        assert_eq!(tracker.delta(1_000), 300);
    }

    #[test]
    fn cumulative_progress_ignores_regressions() {
        let tracker = CumulativeProgress::new();
        assert_eq!(tracker.delta(200), 200);
        assert_eq!(tracker.delta(150), 0);
        assert_eq!(tracker.delta(250), 50);
    }
}
