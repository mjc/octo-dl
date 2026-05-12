use std::time::Duration;

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
    Complete,
    Error { message: String },
}

impl DashboardFileStatus {
    #[must_use]
    pub const fn is_downloading(&self) -> bool {
        matches!(self, Self::Downloading)
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
    pub url_input_active: bool,
}

impl<'a> DashboardChrome<'a> {
    #[must_use]
    pub const fn new(url_input: &'a str, url_input_active: bool) -> Self {
        Self {
            url_input,
            url_input_active,
        }
    }

    #[must_use]
    pub const fn read_only() -> Self {
        Self {
            url_input: "",
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
                    .map(|core_file| core_file.package_id.clone())
                    .or_else(|| {
                        self.overlay_files
                            .get(&file.id)
                            .and_then(|overlay| overlay.source_url.clone())
                    })
                    .unwrap_or_default();
                DashboardFileRow {
                    id: file.id.clone(),
                    package_id,
                    name: file.name.clone(),
                    size: file.size,
                    downloaded: file.downloaded,
                    speed: self.file_speed(&file.id),
                    status: DashboardFileStatus::from(&file.status),
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
                    TuiRow::Package(package_id) => DashboardRow::Package { package_id },
                    TuiRow::File {
                        package_id,
                        file_id,
                    } => DashboardRow::File {
                        package_id,
                        file_id,
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
            return self
                .core_state
                .packages
                .values()
                .map(|package| {
                    let (present, complete, downloaded, size) = package
                        .file_ids
                        .iter()
                        .filter_map(|id| self.core_state.files.get(id))
                        .fold(
                            (0_usize, 0_usize, 0_u64, 0_u64),
                            |(present, complete, downloaded, size), file| {
                                if matches!(
                                    file.lifecycle,
                                    FileLifecycle::Skipped | FileLifecycle::Deleted
                                ) {
                                    return (present, complete, downloaded, size);
                                }
                                let file_complete =
                                    matches!(file.lifecycle, FileLifecycle::Complete);
                                let visible = if file_complete {
                                    file.size
                                } else {
                                    file.progress.visible_completed_bytes.min(file.size)
                                };
                                (
                                    present + 1,
                                    complete + usize::from(file_complete),
                                    downloaded.saturating_add(visible),
                                    size.saturating_add(file.size),
                                )
                            },
                        );
                    DashboardPackageRow {
                        id: package.id.clone(),
                        source_url: package.source_url.clone(),
                        display_name: package.display_name.clone(),
                        status: package.status,
                        file_ids: package.file_ids.clone(),
                        present_files: present,
                        completed_files: complete,
                        downloaded_bytes: downloaded,
                        total_bytes: size,
                        percent: percent(downloaded, size),
                        expanded: self.expanded_packages.contains(&package.id)
                            || matches!(package.status, PackageStatus::Failed),
                        folder_label: self.folder_label_from_package_files(&package.id),
                        error: package.error.clone(),
                    }
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
                } else {
                    file.downloaded.min(file.size)
                };
                DashboardPackageRow {
                    id: file.id.clone(),
                    source_url: self
                        .overlay_files
                        .get(&file.id)
                        .and_then(|overlay| overlay.source_url.clone())
                        .unwrap_or_else(|| file.id.clone()),
                    display_name: file.name.clone(),
                    status,
                    file_ids: vec![file.id.clone()],
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

    fn folder_label_from_package_files(&self, package_id: &str) -> Option<String> {
        let package = self.core_state.packages.get(package_id)?;
        let mut common: Option<&str> = None;
        for file_id in &package.file_ids {
            let file = self.core_state.files.get(file_id)?;
            let folder = file
                .path
                .split('/')
                .next()
                .filter(|part| !part.is_empty())?;
            match common {
                None => common = Some(folder),
                Some(existing) if existing == folder => {}
                Some(_) => return None,
            }
        }
        common.map(str::to_string)
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

    let speed = format!("{}/s", format_bytes(state.totals.current_speed));
    let remaining = state
        .totals
        .total_size
        .saturating_sub(state.totals.total_downloaded);
    if remaining == 0 {
        return speed;
    }

    let eta_secs = remaining.div_ceil(state.totals.current_speed).max(1);
    format!(
        "{speed}  eta {}",
        format_duration(Duration::from_secs(eta_secs))
    )
}

#[must_use]
pub fn aggregate_activity_label(state: &DownloadDashboardState) -> String {
    if state.totals.current_speed > 0 || state.files.iter().any(|file| file.status.is_downloading())
    {
        return "active".to_string();
    }

    let queued = state
        .files
        .iter()
        .filter(|file| file.status.is_queued())
        .count();
    if queued > 0 {
        return format!("{queued} queued");
    }

    "idle".to_string()
}

#[must_use]
pub fn file_detail(file: &DashboardFileRow) -> String {
    match &file.status {
        DashboardFileStatus::Downloading => {
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
            let speed = if file.speed > 0 {
                format!("  {}/s", format_bytes(file.speed))
            } else {
                "  active".to_string()
            };
            format!("[{bar}] {pct}%{speed}")
        }
        DashboardFileStatus::Queued => "queued".to_string(),
        DashboardFileStatus::Complete => format!("{}  done", format_bytes(file.size)),
        DashboardFileStatus::Error { message } => message.clone(),
    }
}

fn progress_bar(downloaded: u64, total: u64, width: usize) -> String {
    if total == 0 {
        return "\u{2591}".repeat(width);
    }
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let filled = ((downloaded as f64 / total as f64) * width as f64) as usize;
    let filled = filled.min(width);
    let empty = width - filled;
    format!("{}{}", "\u{2588}".repeat(filled), "\u{2591}".repeat(empty))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{CoreEvent, ResolvedFile, ResolvedPackage};
    use tokio::sync::mpsc;

    #[test]
    fn dashboard_projection_uses_core_package_totals() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, tx, true);
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: "pkg".to_string(),
                source_url: "https://mega.nz/folder/pkg".to_string(),
                display_name: "Package".to_string(),
                files: vec![ResolvedFile {
                    file_id: "file.bin".to_string(),
                    path: "file.bin".to_string(),
                    size: 100,
                }],
                collision: None,
            },
        });
        app.apply_core_event(CoreEvent::FileStarted {
            file_id: "file.bin".to_string(),
            size: 100,
        });
        app.apply_core_event(CoreEvent::FileProgress {
            file_id: "file.bin".to_string(),
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
    fn dashboard_snapshot_serializes_error_file_status() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, tx, true);
        app.files.push(super::super::app::FileEntry {
            id: "file.bin".to_string(),
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
