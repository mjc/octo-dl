use std::time::{Duration, Instant};

use crate::core::PackageId;

use super::{App, FileStatus};

const MIN_RATE_SAMPLE_SPAN: Duration = Duration::from_secs(1);
const THROUGHPUT_DECAY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Default)]
pub(crate) struct FileUiState {
    pub speed: u64,
    pub rate: TransferRate,
    pub sort_key: Option<crate::tui::visible::CachedFileSortKey>,
    pub package_id: Option<PackageId>,
}

#[derive(Debug, Clone)]
pub(crate) struct TransferRate {
    start_time: Instant,
    last_time: Instant,
    last_total: u64,
    smoothed_bytes_per_sec: f64,
    double_smoothed_bytes_per_sec: f64,
}

impl Default for TransferRate {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            start_time: now,
            last_time: now,
            last_total: 0,
            smoothed_bytes_per_sec: 0.0,
            double_smoothed_bytes_per_sec: 0.0,
        }
    }
}

impl TransferRate {
    pub(crate) fn reset(&mut self, total: u64, now: Instant) {
        self.start_time = now;
        self.last_time = now;
        self.last_total = total;
        self.smoothed_bytes_per_sec = 0.0;
        self.double_smoothed_bytes_per_sec = 0.0;
    }

    pub(crate) fn record(&mut self, total: u64, now: Instant) {
        if total < self.last_total {
            self.reset(total, now);
            return;
        }
        if total == self.last_total || now <= self.last_time {
            return;
        }

        let delta_bytes = total - self.last_total;
        let delta_secs = now.duration_since(self.last_time).as_secs_f64();
        let instant_bytes_per_sec = delta_bytes as f64 / delta_secs;
        let weight = throughput_weight(now.duration_since(self.last_time));

        self.smoothed_bytes_per_sec = self
            .smoothed_bytes_per_sec
            .mul_add(weight, instant_bytes_per_sec * (1.0 - weight));

        let total_weight = 1.0 - throughput_weight(now.duration_since(self.start_time));
        if total_weight > f64::EPSILON {
            let normalized = self.smoothed_bytes_per_sec / total_weight;
            self.double_smoothed_bytes_per_sec = self
                .double_smoothed_bytes_per_sec
                .mul_add(weight, normalized * (1.0 - weight));
        }

        self.last_total = total;
        self.last_time = now;
    }

    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub(crate) fn bytes_per_sec(&self, now: Instant) -> u64 {
        let sample_span = now.duration_since(self.start_time);
        if sample_span < MIN_RATE_SAMPLE_SPAN {
            return 0;
        }

        let total_weight = 1.0 - throughput_weight(sample_span);
        if total_weight <= f64::EPSILON {
            return 0;
        }

        let age = now.duration_since(self.last_time);
        let reweight = throughput_weight(age);
        let single_smoothed = self.smoothed_bytes_per_sec * reweight / total_weight;
        let double_smoothed = self
            .double_smoothed_bytes_per_sec
            .mul_add(reweight, single_smoothed * (1.0 - reweight));
        let bytes_per_sec = double_smoothed / total_weight;
        if bytes_per_sec < 1.0 || !bytes_per_sec.is_finite() {
            return 0;
        }

        if bytes_per_sec >= u64::MAX as f64 {
            u64::MAX
        } else {
            bytes_per_sec as u64
        }
    }
}

#[allow(clippy::cast_precision_loss)]
fn throughput_weight(elapsed: Duration) -> f64 {
    0.1_f64.powf(elapsed.as_secs_f64() / THROUGHPUT_DECAY.as_secs_f64())
}

impl App {
    pub(crate) fn core_file_network_downloaded(file: &crate::core::FileState) -> u64 {
        file.progress.downloaded_network_bytes.min(file.size)
    }

    pub(crate) fn apply_cached_totals(&mut self) {
        self.total_size = self
            .core_state
            .totals
            .run_total_bytes
            .saturating_add(self.overlay_total_size);
        self.total_downloaded = self
            .core_state
            .totals
            .run_completed_bytes
            .saturating_add(self.overlay_total_downloaded);
        self.files_completed = self
            .core_state
            .totals
            .run_file_completed
            .saturating_add(self.overlay_files_completed);
        self.files_total = self
            .core_state
            .totals
            .run_file_total
            .saturating_add(self.overlay_files_total);
        self.total_network_downloaded = self
            .core_state
            .totals
            .displayed_network_bytes
            .saturating_add(self.overlay_total_network_downloaded);
    }

    pub(crate) fn file_speed(&self, file_id: &crate::core::FileId) -> u64 {
        self.file_ui.get(file_id).map_or(0, |state| state.speed)
    }

    fn ensure_file_ui(&mut self, file_id: &crate::core::FileId, downloaded: u64, reset: bool) {
        let state = self.file_ui.entry(file_id.clone()).or_default();
        if reset {
            state.speed = 0;
            state.rate.reset(downloaded, Instant::now());
        }
    }

    pub(crate) fn reset_file_ui_rate(&mut self, file_id: &crate::core::FileId) {
        let downloaded = self
            .core_state
            .files
            .get(file_id)
            .map(Self::core_file_network_downloaded)
            .or_else(|| {
                self.overlay_files
                    .get(file_id)
                    .map(|file| file.file().downloaded)
            })
            .or_else(|| self.visible_file(file_id).map(|file| file.downloaded))
            .unwrap_or(0);
        self.ensure_file_ui(file_id, downloaded, true);
    }

    #[cfg(test)]
    pub(crate) fn update_file_ui_progress(
        &mut self,
        file_id: &crate::core::FileId,
        previous_downloaded: u64,
        downloaded: u64,
        now: Instant,
    ) -> u64 {
        let accepted = downloaded.saturating_sub(previous_downloaded);
        let state = self.file_ui.entry(file_id.clone()).or_default();
        state.rate.record(downloaded, now);
        state.speed = state.rate.bytes_per_sec(now);
        accepted
    }

    pub(crate) fn update_speeds_at(&mut self, now: Instant) {
        self.last_tick = now;

        for file in &self.files {
            if let Some(state) = self.file_ui.get_mut(&file.id) {
                if matches!(file.status, FileStatus::Downloading) {
                    state.speed = state.rate.bytes_per_sec(now);
                } else {
                    state.speed = 0;
                }
            } else {
                self.file_ui.entry(file.id.clone()).or_default();
            }
        }

        self.current_speed = if self.core_state.totals.run_file_downloading > 0 {
            self.aggregate_rate
                .record(self.total_network_downloaded, now);
            self.aggregate_rate.bytes_per_sec(now)
        } else {
            0
        };
    }

    pub fn update_speeds(&mut self) {
        self.update_speeds_at(Instant::now());
    }

    pub fn recompute_totals(&mut self) {
        self.overlay_total_size = 0;
        self.overlay_total_downloaded = 0;
        self.overlay_files_completed = 0;
        self.overlay_files_total = 0;
        self.overlay_total_network_downloaded = 0;
        self.apply_cached_totals();
    }

    pub(crate) fn reset_aggregate_rate(&mut self) {
        self.current_speed = 0;
        self.aggregate_rate
            .reset(self.total_network_downloaded, Instant::now());
    }
}
