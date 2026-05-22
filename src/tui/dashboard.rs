use std::fmt::Write as _;
use std::time::Duration;

use indexmap::IndexMap;
use ratatui::widgets::ListState;
use serde::{Deserialize, Serialize};

use crate::core::{FileLifecycle, PackageStatus};
use crate::{DownloadConfig, format_bytes, format_duration};

use super::app::{App, FileStatus, Popup};
use super::visible::TuiRow;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DashboardUiMode {
    Headless,
    Tui,
    Attached,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum DashboardRow {
    Package { package_id: String },
    File { package_id: String, file_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum DashboardFileStatus {
    Queued,
    Downloading,
    Verifying,
    Complete,
    Error { message: String },
}

impl DashboardFileStatus {
    #[must_use]
    pub const fn is_downloading(&self) -> bool {
        matches!(self, Self::Downloading)
    }

    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(self, Self::Downloading | Self::Verifying)
    }

    #[must_use]
    pub const fn is_queued(&self) -> bool {
        matches!(self, Self::Queued)
    }

    #[must_use]
    pub const fn is_error(&self) -> bool {
        matches!(self, Self::Error { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardPackageRow {
    pub id: String,
    pub source_url: String,
    pub display_name: String,
    pub status: PackageStatus,
    pub file_ids: Vec<String>,
    pub present_files: usize,
    pub completed_files: usize,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub percent: u64,
    pub expanded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardFileRow {
    pub id: String,
    pub package_id: String,
    pub name: String,
    pub size: u64,
    pub downloaded: u64,
    pub speed: u64,
    pub status: DashboardFileStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardTotals {
    pub total_downloaded: u64,
    pub total_size: u64,
    pub files_completed: usize,
    pub files_total: usize,
    pub current_speed: u64,
    pub run_total_bytes: u64,
    pub run_completed_bytes: u64,
    pub run_file_total: usize,
    pub run_file_completed: usize,
}

impl DashboardTotals {
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            total_downloaded: 0,
            total_size: 0,
            files_completed: 0,
            files_total: 0,
            current_speed: 0,
            run_total_bytes: 0,
            run_completed_bytes: 0,
            run_file_total: 0,
            run_file_completed: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DashboardMetrics {
    pub cpu_usage: f32,
    pub memory_rss: u64,
    pub api_port: u16,
}

impl DashboardMetrics {
    #[must_use]
    pub const fn empty(api_port: u16) -> Self {
        Self {
            cpu_usage: 0.0,
            memory_rss: 0,
            api_port,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DownloadDashboardState {
    pub authenticated: bool,
    pub paused: bool,
    pub logging_in: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_error: Option<String>,
    pub popup: Popup,
    pub ui_mode: DashboardUiMode,
    pub read_only: bool,
    pub status: String,
    pub packages: Vec<DashboardPackageRow>,
    pub files: Vec<DashboardFileRow>,
    pub rows: Vec<DashboardRow>,
    pub totals: DashboardTotals,
    pub metrics: DashboardMetrics,
    pub config: DownloadConfig,
}

impl DownloadDashboardState {
    #[must_use]
    pub fn empty(
        ui_mode: DashboardUiMode,
        read_only: bool,
        status: impl Into<String>,
        api_port: u16,
    ) -> Self {
        Self {
            authenticated: false,
            paused: false,
            logging_in: false,
            login_error: None,
            popup: Popup::None,
            ui_mode,
            read_only,
            status: status.into(),
            packages: Vec::new(),
            files: Vec::new(),
            rows: Vec::new(),
            totals: DashboardTotals::empty(),
            metrics: DashboardMetrics::empty(api_port),
            config: DownloadConfig::default(),
        }
    }
}

pub struct DashboardChrome<'a> {
    pub url_input: &'a str,
    pub url_input_cursor: usize,
    pub url_input_active: bool,
}

impl<'a> DashboardChrome<'a> {
    #[must_use]
    pub const fn read_only() -> Self {
        Self {
            url_input: "",
            url_input_cursor: 0,
            url_input_active: false,
        }
    }
}

#[derive(Default)]
pub struct AttachedDashboard {
    pub state: Option<DownloadDashboardState>,
    pub list_state: ListState,
    pub should_quit: bool,
    pub status: String,
}

impl AttachedDashboard {
    pub fn replace_state(&mut self, state: DownloadDashboardState) {
        self.state = Some(state);
        if let Some(state) = &self.state {
            clamp_selection(&mut self.list_state, state.rows.len());
        }
    }

    pub fn select_delta(&mut self, delta: isize) {
        let len = self.state.as_ref().map_or(0, |state| state.rows.len());
        if len == 0 {
            self.list_state.select(None);
            return;
        }
        let current = self.list_state.selected().unwrap_or(0);
        let next = current
            .saturating_add_signed(delta)
            .min(len.saturating_sub(1));
        self.list_state.select(Some(next));
    }
}

pub fn clamp_selection(list_state: &mut ListState, row_count: usize) {
    if row_count == 0 {
        list_state.select(None);
    } else if list_state.selected().is_none() {
        list_state.select(Some(0));
    } else if let Some(selected) = list_state.selected()
        && selected >= row_count
    {
        list_state.select(Some(row_count.saturating_sub(1)));
    }
}

impl From<&FileStatus> for DashboardFileStatus {
    fn from(status: &FileStatus) -> Self {
        match status {
            FileStatus::Queued => Self::Queued,
            FileStatus::Downloading => Self::Downloading,
            FileStatus::Complete => Self::Complete,
            FileStatus::Error(message) => Self::Error {
                message: message.clone(),
            },
        }
    }
}

impl App {
    #[must_use]
    pub fn dashboard_state(
        &self,
        ui_mode: DashboardUiMode,
        read_only: bool,
    ) -> DownloadDashboardState {
        let packages = self.dashboard_packages();
        let files = self
            .files
            .iter()
            .map(|file| {
                let package_id = self
                    .core_state
                    .files
                    .get(&file.id)
                    .map(|core_file| core_file.package_id.to_string())
                    .or_else(|| {
                        self.overlay_files
                            .get(&file.id)
                            .and_then(|overlay| overlay.source_url().map(str::to_string))
                    })
                    .unwrap_or_default();
                DashboardFileRow {
                    id: file.id.to_string(),
                    package_id,
                    name: file.name.clone(),
                    size: file.size,
                    downloaded: file.downloaded,
                    speed: self.file_speed(&file.id),
                    status: if self.verification_inflight_files.contains(&file.id) {
                        DashboardFileStatus::Verifying
                    } else {
                        DashboardFileStatus::from(&file.status)
                    },
                    package_label: self.package_label_for_file(&file.id),
                }
            })
            .collect();
        let totals = self.dashboard_totals();
        let metrics = DashboardMetrics {
            cpu_usage: self.cpu_usage,
            memory_rss: self.memory_rss,
            api_port: self.api_port,
        };
        DownloadDashboardState {
            authenticated: self.authenticated,
            paused: self.paused,
            logging_in: self.login.logging_in,
            login_error: self.login.error.clone(),
            popup: self.popup,
            ui_mode,
            read_only,
            status: self.status.clone(),
            packages,
            files,
            rows: self
                .visible_rows()
                .into_iter()
                .map(|row| match row {
                    TuiRow::Package(package_id) => DashboardRow::Package {
                        package_id: package_id.to_string(),
                    },
                    TuiRow::File {
                        package_id,
                        file_id,
                    } => DashboardRow::File {
                        package_id: package_id.map_or_else(String::new, |id| id.to_string()),
                        file_id: file_id.to_string(),
                    },
                })
                .collect(),
            totals,
            metrics,
            config: self.config.config.clone(),
        }
    }

    fn dashboard_totals(&self) -> DashboardTotals {
        let (run_total_bytes, run_completed_bytes, run_file_total, run_file_completed) =
            if self.core_state.files.is_empty() {
                (
                    self.total_size,
                    self.total_downloaded,
                    self.files_total,
                    self.files_completed,
                )
            } else {
                (
                    self.core_state.totals.run_total_bytes,
                    self.core_state.totals.run_completed_bytes,
                    self.core_state.totals.run_file_total,
                    self.core_state.totals.run_file_completed,
                )
            };
        DashboardTotals {
            total_downloaded: self.total_downloaded,
            total_size: self.total_size,
            files_completed: self.files_completed,
            files_total: self.files_total,
            current_speed: self.current_speed,
            run_total_bytes,
            run_completed_bytes,
            run_file_total,
            run_file_completed,
        }
    }

    fn dashboard_packages(&self) -> Vec<DashboardPackageRow> {
        if !self.core_state.packages.is_empty() {
            let mut package_files = self
                .core_state
                .packages
                .keys()
                .copied()
                .map(|package_id| (package_id, Vec::new()))
                .collect::<IndexMap<_, Vec<&crate::core::FileState>>>();
            for file in self.core_state.files.values() {
                if let Some(files) = package_files.get_mut(&file.package_id) {
                    files.push(file);
                }
            }
            return self
                .core_state
                .packages
                .values()
                .filter_map(|package| {
                    let package_files = package_files.get(&package.id)?;
                    if package_files.is_empty() {
                        return None;
                    }
                    let file_ids = package_files
                        .iter()
                        .map(|file| file.id.to_string())
                        .collect::<Vec<_>>();
                    let mut present = 0_usize;
                    let mut complete = 0_usize;
                    let mut downloaded = 0_u64;
                    let mut size = 0_u64;
                    let mut source_url = None;
                    let mut common_folder = None;
                    let mut folder_conflict = false;
                    let mut package_downloading = false;
                    let mut package_verifying = false;

                    for file in package_files {
                        source_url = source_url.or_else(|| Some(file.source_url.clone()));
                        package_downloading |= matches!(file.lifecycle, FileLifecycle::Downloading);
                        package_verifying |= self.verification_inflight_files.contains(&file.id);
                        let folder = file.path.split('/').next().filter(|part| !part.is_empty());
                        match (common_folder, folder) {
                            (None, Some(folder)) => common_folder = Some(folder),
                            (Some(existing), Some(folder)) if existing == folder => {}
                            (Some(_), Some(_)) => folder_conflict = true,
                            _ => {}
                        }

                        let file_complete = matches!(file.lifecycle, FileLifecycle::Complete);
                        let visible = if file_complete {
                            file.size
                        } else {
                            crate::core::visible_completed_bytes_for_display(file)
                        };
                        present += 1;
                        complete += usize::from(file_complete);
                        downloaded = downloaded.saturating_add(visible);
                        size = size.saturating_add(file.size);
                    }

                    Some(DashboardPackageRow {
                        id: package.id.to_string(),
                        source_url: source_url.unwrap_or_default(),
                        display_name: package.display_name.clone(),
                        status: if package_downloading || package_verifying {
                            PackageStatus::Downloading
                        } else {
                            package.status
                        },
                        file_ids,
                        present_files: present,
                        completed_files: complete,
                        downloaded_bytes: downloaded,
                        total_bytes: size,
                        percent: percent(downloaded, size),
                        expanded: self.expanded_packages.contains(&package.id)
                            || matches!(package.status, PackageStatus::Failed),
                        folder_label: (!folder_conflict)
                            .then(|| common_folder.map(str::to_string))
                            .flatten(),
                        error: package.error.clone(),
                    })
                })
                .collect();
        }

        self.files
            .iter()
            .map(|file| {
                let status = if matches!(file.status, FileStatus::Error(_)) {
                    PackageStatus::Failed
                } else if matches!(file.status, FileStatus::Downloading) {
                    PackageStatus::Downloading
                } else if matches!(file.status, FileStatus::Complete) {
                    PackageStatus::Complete
                } else {
                    PackageStatus::Queued
                };
                let downloaded = if matches!(file.status, FileStatus::Complete) {
                    file.size
                } else if file.size > 0 && file.downloaded >= file.size {
                    file.size.saturating_sub(1)
                } else {
                    file.downloaded.min(file.size)
                };
                DashboardPackageRow {
                    id: file.id.to_string(),
                    source_url: self
                        .overlay_files
                        .get(&file.id)
                        .and_then(|overlay| overlay.source_url().map(str::to_string))
                        .unwrap_or_else(|| file.id.to_string()),
                    display_name: file.name.clone(),
                    status,
                    file_ids: vec![file.id.to_string()],
                    present_files: 1,
                    completed_files: usize::from(matches!(file.status, FileStatus::Complete)),
                    downloaded_bytes: downloaded,
                    total_bytes: file.size,
                    percent: percent(downloaded, file.size),
                    expanded: false,
                    folder_label: None,
                    error: match &file.status {
                        FileStatus::Error(message) => Some(message.clone()),
                        _ => None,
                    },
                }
            })
            .collect()
    }
}

fn percent(downloaded: u64, size: u64) -> u64 {
    if size == 0 {
        0
    } else {
        downloaded.saturating_mul(100).saturating_div(size).min(100)
    }
}

#[must_use]
pub fn aggregate_transfer_label(state: &DownloadDashboardState) -> String {
    if state.totals.current_speed == 0 {
        return aggregate_activity_label(state);
    }

    let mut speed = String::with_capacity(16);
    let _ = write!(speed, "{}/s", format_bytes(state.totals.current_speed));
    let remaining = state
        .totals
        .total_size
        .saturating_sub(state.totals.total_downloaded);
    if remaining == 0 {
        return speed;
    }

    let eta_secs = remaining.div_ceil(state.totals.current_speed).max(1);
    let eta = format_duration(Duration::from_secs(eta_secs));
    let mut label = String::with_capacity(speed.len() + eta.len() + 7);
    label.push_str(&speed);
    label.push_str("  eta ");
    label.push_str(&eta);
    label
}

#[must_use]
pub fn aggregate_activity_label(state: &DownloadDashboardState) -> String {
    if state.totals.current_speed > 0 || state.files.iter().any(|file| file.status.is_active()) {
        return "active".to_string();
    }

    let queued = state
        .files
        .iter()
        .filter(|file| file.status.is_queued())
        .count();
    if queued > 0 {
        let mut label = String::with_capacity(16);
        let _ = write!(label, "{queued} queued");
        return label;
    }

    "idle".to_string()
}

#[must_use]
pub fn file_detail(file: &DashboardFileRow) -> String {
    match &file.status {
        DashboardFileStatus::Downloading | DashboardFileStatus::Verifying => {
            #[allow(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss
            )]
            let pct = if file.size > 0 {
                ((file.downloaded as f64 / file.size as f64 * 100.0) as u64).min(100)
            } else {
                0
            };
            let bar = progress_bar(file.downloaded, file.size, 10);
            let speed = if matches!(file.status, DashboardFileStatus::Verifying) {
                "  verify".to_string()
            } else if file.speed > 0 {
                let mut speed = String::with_capacity(18);
                let _ = write!(speed, "  {}/s", format_bytes(file.speed));
                speed
            } else {
                "  active".to_string()
            };
            let mut detail = String::with_capacity(bar.len() + speed.len() + 8);
            let _ = write!(detail, "[{bar}] {pct}%{speed}");
            detail
        }
        DashboardFileStatus::Queued => "queued".to_string(),
        DashboardFileStatus::Complete => {
            let mut detail = String::with_capacity(24);
            let _ = write!(detail, "{}", format_bytes(file.size));
            detail.push_str("  done");
            detail
        }
        DashboardFileStatus::Error { message } => message.clone(),
    }
}

fn progress_bar(downloaded: u64, total: u64, width: usize) -> String {
    if total == 0 {
        let mut bar = String::with_capacity(width * "\u{2591}".len());
        for _ in 0..width {
            bar.push('\u{2591}');
        }
        return bar;
    }
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let filled = ((downloaded as f64 / total as f64) * width as f64) as usize;
    let filled = filled.min(width);
    let empty = width - filled;
    let mut bar = String::with_capacity(width * "\u{2588}".len());
    for _ in 0..filled {
        bar.push('\u{2588}');
    }
    for _ in 0..empty {
        bar.push('\u{2591}');
    }
    bar
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{CoreEvent, ResolvedFile, ResolvedPackage};
    use crate::test_support::package_id;
    use tokio::sync::mpsc;

    #[test]
    fn dashboard_projection_uses_core_package_totals() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, tx, true);
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id("pkg", "https://mega.nz/folder/pkg"),
                source_url: "https://mega.nz/folder/pkg".to_string(),
                key: crate::core::PackageKey::new("https://mega.nz/folder/pkg".to_string().clone()),
                display_name: "Package".to_string(),
                files: vec![ResolvedFile {
                    file_id: "file.bin".to_string().into(),
                    path: "file.bin".to_string(),
                    size: 100,
                }],
                collision: None,
            },
        });
        app.apply_core_event(CoreEvent::FileStarted {
            file_id: "file.bin".to_string().into(),
            size: 100,
        });
        app.apply_core_event(CoreEvent::FileProgress {
            file_id: "file.bin".to_string().into(),
            total_bytes_delta: 40,
            network_bytes_delta: 40,
        });

        let state = app.dashboard_state(DashboardUiMode::Tui, false);

        assert_eq!(state.packages.len(), 1);
        assert_eq!(state.packages[0].present_files, 1);
        assert_eq!(state.packages[0].downloaded_bytes, 40);
        assert_eq!(state.packages[0].percent, 40);
        assert_eq!(state.files[0].status, DashboardFileStatus::Downloading);
    }

    #[test]
    fn dashboard_projection_allows_full_progress_before_complete_status() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, tx, true);
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id("pkg", "https://mega.nz/folder/pkg"),
                source_url: "https://mega.nz/folder/pkg".to_string(),
                key: crate::core::PackageKey::new("https://mega.nz/folder/pkg".to_string().clone()),
                display_name: "Package".to_string(),
                files: vec![ResolvedFile {
                    file_id: "file.bin".to_string().into(),
                    path: "file.bin".to_string(),
                    size: 100,
                }],
                collision: None,
            },
        });
        app.handle_download_event(crate::tui::event::DownloadEvent::FileStart {
            id: "file.bin".to_string().into(),
            size: 100,
            attempt_id: 0,
        });
        app.handle_download_event(crate::tui::event::DownloadEvent::Progress {
            id: "file.bin".to_string().into(),
            delta: crate::core::ProgressDelta {
                total_bytes_delta: 100,
                network_bytes_delta: 100,
            },
            attempt_id: 0,
        });

        let state = app.dashboard_state(DashboardUiMode::Tui, false);

        assert_eq!(state.packages[0].downloaded_bytes, 100);
        assert_eq!(state.packages[0].percent, 100);
        assert_eq!(state.files[0].downloaded, 100);
        assert_eq!(state.files[0].status, DashboardFileStatus::Downloading);
    }

    #[test]
    fn dashboard_projection_uses_inflight_verification_for_verify_status() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, tx, true);
        app.files.push(super::super::app::FileEntry {
            id: "file.bin".to_string().into(),
            name: "file.bin".to_string(),
            size: 100,
            downloaded: 0,
            status: FileStatus::Queued,
        });
        app.verifying_files.insert("file.bin".to_string().into());

        let state = app.dashboard_state(DashboardUiMode::Tui, false);

        assert_eq!(state.files[0].status, DashboardFileStatus::Queued);

        app.verification_inflight_files
            .insert("file.bin".to_string().into());
        let state = app.dashboard_state(DashboardUiMode::Tui, false);

        assert_eq!(state.files[0].status, DashboardFileStatus::Verifying);
    }

    #[test]
    fn dashboard_projection_hides_empty_failed_packages() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, tx, true);
        let package_id = package_id("failed", "https://mega.nz/folder/failed");
        app.core_state.packages.insert(
            package_id,
            crate::core::PackageState {
                id: package_id,
                key: crate::core::PackageKey::new(
                    "https://mega.nz/folder/failed".to_string().clone(),
                ),
                display_name: "Failed".to_string(),
                file_ids: Vec::new(),
                status: PackageStatus::Failed,
                error: Some("boom".to_string()),
            },
        );

        let state = app.dashboard_state(DashboardUiMode::Tui, false);

        assert!(state.packages.is_empty());
        assert!(state.rows.is_empty());
    }

    #[test]
    fn dashboard_snapshot_serializes_error_file_status() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, tx, true);
        app.files.push(super::super::app::FileEntry {
            id: "file.bin".to_string().into(),
            name: "file.bin".to_string(),
            size: 100,
            downloaded: 0,
            status: FileStatus::Error("network failed".to_string()),
        });
        app.recompute_totals();

        let state = app.dashboard_state(DashboardUiMode::Tui, false);
        let json = serde_json::to_string(&state).expect("error snapshots should serialize");
        assert!(json.contains(r#""kind":"error""#));
        assert!(json.contains(r#""message":"network failed""#));

        let decoded: DownloadDashboardState =
            serde_json::from_str(&json).expect("error snapshots should deserialize");
        assert!(matches!(
            decoded.files[0].status,
            DashboardFileStatus::Error { .. }
        ));
    }

    #[test]
    fn dashboard_snapshot_round_trips_for_remote_attach() {
        let state = DownloadDashboardState::empty(DashboardUiMode::Headless, true, "ready", 9723);

        let json = serde_json::to_string(&state).expect("dashboard should serialize");
        let decoded: DownloadDashboardState =
            serde_json::from_str(&json).expect("dashboard should deserialize");
        assert_eq!(decoded.status, "ready");
        assert!(decoded.read_only);
    }
}
