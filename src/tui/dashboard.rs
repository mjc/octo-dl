use std::fmt::Write as _;
use std::time::Duration;

use indexmap::IndexMap;
use ratatui::widgets::ListState;
use serde::ser::{SerializeSeq, SerializeStruct, SerializeStructVariant};
use serde::{Deserialize, Serialize};

#[cfg(test)]
use std::cell::Cell;

use crate::core::{FileId, FileLifecycle, FileState, PackageId, PackageStatus};
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

impl Serialize for BinaryDashboardPackagesRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let app = self.0;
        if !app.core_state.packages.is_empty() {
            let package_rows = binary_core_package_rows(app);
            let mut seq = serializer.serialize_seq(Some(package_rows.len()))?;
            for row in package_rows {
                seq.serialize_element(&row)?;
            }
            seq.end()
        } else {
            let mut seq = serializer.serialize_seq(Some(app.files.len()))?;
            for file in &app.files {
                seq.serialize_element(&BinaryLegacyPackageRowRef { app, file })?;
            }
            seq.end()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum DashboardRow {
    Package { package_id: String },
    File { package_id: String, file_id: String },
}

impl Serialize for BinaryCorePackageRowRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let status = if self.stats.downloading || self.stats.verifying {
            PackageStatus::Downloading
        } else {
            self.package.status()
        };
        let expanded = self.app.expanded_packages.contains(&self.package.id)
            || matches!(self.package.status(), PackageStatus::Failed);
        let folder_label = (!self.stats.folder_conflict)
            .then_some(self.stats.folder_label)
            .flatten();
        let mut row = serializer.serialize_struct("BinaryDashboardPackageRow", 13)?;
        row.serialize_field("id", &BinaryDashboardPackageIdRef::Core(self.package.id))?;
        row.serialize_field("source_url", self.stats.source_url)?;
        row.serialize_field("display_name", &self.package.display_name)?;
        row.serialize_field("status", &status)?;
        row.serialize_field("file_ids", &PackageFileIdsRef(&self.file_ids))?;
        row.serialize_field("present_files", &self.stats.present_files)?;
        row.serialize_field("completed_files", &self.stats.completed_files)?;
        row.serialize_field("downloaded_bytes", &self.stats.downloaded_bytes)?;
        row.serialize_field("total_bytes", &self.stats.total_bytes)?;
        row.serialize_field(
            "percent",
            &percent(self.stats.downloaded_bytes, self.stats.total_bytes),
        )?;
        row.serialize_field("expanded", &expanded)?;
        row.serialize_field("folder_label", &folder_label)?;
        row.serialize_field("error", &self.package.error.as_deref())?;
        row.end()
    }
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

impl Serialize for BinaryLegacyPackageRowRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let status = if matches!(self.file.status, FileStatus::Error(_)) {
            PackageStatus::Failed
        } else if matches!(self.file.status, FileStatus::Downloading) {
            PackageStatus::Downloading
        } else if matches!(self.file.status, FileStatus::Complete) {
            PackageStatus::Complete
        } else {
            PackageStatus::Queued
        };
        let downloaded = if matches!(self.file.status, FileStatus::Complete) {
            self.file.size
        } else if self.file.size > 0 && self.file.downloaded >= self.file.size {
            self.file.size.saturating_sub(1)
        } else {
            self.file.downloaded.min(self.file.size)
        };
        let source_url = self
            .app
            .overlay_files
            .get(&self.file.id)
            .and_then(|overlay| overlay.source_url())
            .unwrap_or_else(|| self.file.id.as_str());
        let mut row = serializer.serialize_struct("BinaryDashboardPackageRow", 13)?;
        row.serialize_field(
            "id",
            &BinaryDashboardPackageIdRef::Text(self.file.id.as_str()),
        )?;
        row.serialize_field("source_url", source_url)?;
        row.serialize_field("display_name", &self.file.name)?;
        row.serialize_field("status", &status)?;
        row.serialize_field("file_ids", &SingleFileIdRef(&self.file.id))?;
        row.serialize_field("present_files", &1_usize)?;
        row.serialize_field(
            "completed_files",
            &usize::from(matches!(self.file.status, FileStatus::Complete)),
        )?;
        row.serialize_field("downloaded_bytes", &downloaded)?;
        row.serialize_field("total_bytes", &self.file.size)?;
        row.serialize_field("percent", &percent(downloaded, self.file.size))?;
        row.serialize_field("expanded", &false)?;
        row.serialize_field("folder_label", &Option::<&str>::None)?;
        let error = match &self.file.status {
            FileStatus::Error(message) => Some(message.as_str()),
            _ => None,
        };
        row.serialize_field("error", &error)?;
        row.end()
    }
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
    #[serde(default)]
    pub folder_label: Option<String>,
    #[serde(default)]
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
    #[serde(default)]
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
    #[serde(default)]
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

#[derive(Debug, Serialize, Deserialize)]
struct BinaryDownloadDashboardState {
    authenticated: bool,
    paused: bool,
    logging_in: bool,
    login_error: Option<String>,
    popup: Popup,
    ui_mode: DashboardUiMode,
    read_only: bool,
    status: String,
    packages: Vec<BinaryDashboardPackageRow>,
    files: Vec<BinaryDashboardFileRow>,
    rows: Vec<BinaryDashboardRow>,
    totals: DashboardTotals,
    metrics: DashboardMetrics,
    config: DownloadConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum BinaryDashboardPackageId {
    Core(PackageId),
    Text(String),
    Empty,
}

impl BinaryDashboardPackageId {
    fn from_dashboard_string(value: String) -> Self {
        if value.is_empty() {
            Self::Empty
        } else if let Ok(package_id) = value.parse() {
            Self::Core(package_id)
        } else {
            Self::Text(value)
        }
    }

    fn into_dashboard_string(self) -> String {
        match self {
            Self::Core(package_id) => package_id.to_string(),
            Self::Text(value) => value,
            Self::Empty => String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct BinaryDashboardPackageRow {
    id: BinaryDashboardPackageId,
    source_url: String,
    display_name: String,
    status: PackageStatus,
    file_ids: Vec<String>,
    present_files: usize,
    completed_files: usize,
    downloaded_bytes: u64,
    total_bytes: u64,
    percent: u64,
    expanded: bool,
    #[serde(default)]
    folder_label: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

impl From<BinaryDashboardPackageRow> for DashboardPackageRow {
    fn from(row: BinaryDashboardPackageRow) -> Self {
        Self {
            id: row.id.into_dashboard_string(),
            source_url: row.source_url,
            display_name: row.display_name,
            status: row.status,
            file_ids: row.file_ids,
            present_files: row.present_files,
            completed_files: row.completed_files,
            downloaded_bytes: row.downloaded_bytes,
            total_bytes: row.total_bytes,
            percent: row.percent,
            expanded: row.expanded,
            folder_label: row.folder_label,
            error: row.error,
        }
    }
}

impl From<DashboardPackageRow> for BinaryDashboardPackageRow {
    fn from(row: DashboardPackageRow) -> Self {
        Self {
            id: BinaryDashboardPackageId::from_dashboard_string(row.id),
            source_url: row.source_url,
            display_name: row.display_name,
            status: row.status,
            file_ids: row.file_ids,
            present_files: row.present_files,
            completed_files: row.completed_files,
            downloaded_bytes: row.downloaded_bytes,
            total_bytes: row.total_bytes,
            percent: row.percent,
            expanded: row.expanded,
            folder_label: row.folder_label,
            error: row.error,
        }
    }
}

impl From<BinaryDownloadDashboardState> for DownloadDashboardState {
    fn from(state: BinaryDownloadDashboardState) -> Self {
        Self {
            authenticated: state.authenticated,
            paused: state.paused,
            logging_in: state.logging_in,
            login_error: state.login_error,
            popup: state.popup,
            ui_mode: state.ui_mode,
            read_only: state.read_only,
            status: state.status,
            packages: state.packages.into_iter().map(Into::into).collect(),
            files: state.files.into_iter().map(Into::into).collect(),
            rows: state.rows.into_iter().map(Into::into).collect(),
            totals: state.totals,
            metrics: state.metrics,
            config: state.config,
        }
    }
}

impl From<DownloadDashboardState> for BinaryDownloadDashboardState {
    fn from(state: DownloadDashboardState) -> Self {
        Self {
            authenticated: state.authenticated,
            paused: state.paused,
            logging_in: state.logging_in,
            login_error: state.login_error,
            popup: state.popup,
            ui_mode: state.ui_mode,
            read_only: state.read_only,
            status: state.status,
            packages: state.packages.into_iter().map(Into::into).collect(),
            files: state.files.into_iter().map(Into::into).collect(),
            rows: state.rows.into_iter().map(Into::into).collect(),
            totals: state.totals,
            metrics: state.metrics,
            config: state.config,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
enum BinaryDashboardRow {
    Package {
        package_id: BinaryDashboardPackageId,
    },
    File {
        package_id: BinaryDashboardPackageId,
        file_id: String,
    },
}

impl From<BinaryDashboardRow> for DashboardRow {
    fn from(row: BinaryDashboardRow) -> Self {
        match row {
            BinaryDashboardRow::Package { package_id } => Self::Package {
                package_id: package_id.into_dashboard_string(),
            },
            BinaryDashboardRow::File {
                package_id,
                file_id,
            } => Self::File {
                package_id: package_id.into_dashboard_string(),
                file_id,
            },
        }
    }
}

impl From<DashboardRow> for BinaryDashboardRow {
    fn from(row: DashboardRow) -> Self {
        match row {
            DashboardRow::Package { package_id } => Self::Package {
                package_id: BinaryDashboardPackageId::from_dashboard_string(package_id),
            },
            DashboardRow::File {
                package_id,
                file_id,
            } => Self::File {
                package_id: BinaryDashboardPackageId::from_dashboard_string(package_id),
                file_id,
            },
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
enum BinaryDashboardFileStatus {
    Queued,
    Downloading,
    Verifying,
    Complete,
    Error { message: String },
}

impl From<BinaryDashboardFileStatus> for DashboardFileStatus {
    fn from(status: BinaryDashboardFileStatus) -> Self {
        match status {
            BinaryDashboardFileStatus::Queued => Self::Queued,
            BinaryDashboardFileStatus::Downloading => Self::Downloading,
            BinaryDashboardFileStatus::Verifying => Self::Verifying,
            BinaryDashboardFileStatus::Complete => Self::Complete,
            BinaryDashboardFileStatus::Error { message } => Self::Error { message },
        }
    }
}

impl From<DashboardFileStatus> for BinaryDashboardFileStatus {
    fn from(status: DashboardFileStatus) -> Self {
        match status {
            DashboardFileStatus::Queued => Self::Queued,
            DashboardFileStatus::Downloading => Self::Downloading,
            DashboardFileStatus::Verifying => Self::Verifying,
            DashboardFileStatus::Complete => Self::Complete,
            DashboardFileStatus::Error { message } => Self::Error { message },
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct BinaryDashboardFileRow {
    id: String,
    package_id: BinaryDashboardPackageId,
    name: String,
    size: u64,
    downloaded: u64,
    speed: u64,
    status: BinaryDashboardFileStatus,
    package_label: Option<String>,
}

impl From<BinaryDashboardFileRow> for DashboardFileRow {
    fn from(row: BinaryDashboardFileRow) -> Self {
        Self {
            id: row.id,
            package_id: row.package_id.into_dashboard_string(),
            name: row.name,
            size: row.size,
            downloaded: row.downloaded,
            speed: row.speed,
            status: row.status.into(),
            package_label: row.package_label,
        }
    }
}

impl From<DashboardFileRow> for BinaryDashboardFileRow {
    fn from(row: DashboardFileRow) -> Self {
        Self {
            id: row.id,
            package_id: BinaryDashboardPackageId::from_dashboard_string(row.package_id),
            name: row.name,
            size: row.size,
            downloaded: row.downloaded,
            speed: row.speed,
            status: row.status.into(),
            package_label: row.package_label,
        }
    }
}

pub(crate) fn dashboard_state_from_postcard(
    bytes: &[u8],
) -> Result<DownloadDashboardState, postcard::Error> {
    postcard::from_bytes::<BinaryDownloadDashboardState>(bytes).map(Into::into)
}

#[cfg(test)]
pub(crate) fn dashboard_state_to_postcard(
    state: DownloadDashboardState,
) -> Result<Vec<u8>, postcard::Error> {
    postcard::to_stdvec(&BinaryDownloadDashboardState::from(state))
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

#[derive(Serialize)]
#[cfg(test)]
struct DashboardStateRef<'a> {
    authenticated: bool,
    paused: bool,
    logging_in: bool,
    #[serde(default)]
    login_error: Option<&'a str>,
    popup: Popup,
    ui_mode: DashboardUiMode,
    read_only: bool,
    status: &'a str,
    packages: DashboardPackagesRef<'a>,
    files: DashboardFilesRef<'a>,
    rows: DashboardRowsRef<'a>,
    totals: DashboardTotals,
    metrics: DashboardMetrics,
    config: &'a DownloadConfig,
}

#[cfg(test)]
struct DashboardPackagesRef<'a>(&'a App);

struct BinaryDashboardPackagesRef<'a>(&'a App);

#[cfg(test)]
struct DashboardFilesRef<'a>(&'a App);

#[cfg(test)]
struct DashboardRowsRef<'a> {
    rows: super::app::VisibleRowsSnapshot<'a>,
}

#[derive(Serialize)]
struct BinaryDashboardStateRef<'a> {
    authenticated: bool,
    paused: bool,
    logging_in: bool,
    login_error: Option<&'a str>,
    popup: Popup,
    ui_mode: DashboardUiMode,
    read_only: bool,
    status: &'a str,
    packages: BinaryDashboardPackagesRef<'a>,
    files: BinaryDashboardFilesRef<'a>,
    rows: BinaryDashboardRowsRef<'a>,
    totals: DashboardTotals,
    metrics: DashboardMetrics,
    config: &'a DownloadConfig,
}

struct BinaryDashboardFilesRef<'a>(&'a App);

struct BinaryDashboardRowsRef<'a> {
    rows: super::app::VisibleRowsSnapshot<'a>,
}

#[derive(Clone, Copy)]
struct BinaryDashboardFileProjection<'a> {
    file: &'a super::app::FileEntry,
    package_id: BinaryDashboardPackageIdRef<'a>,
    status: BinaryDashboardFileStatusRef<'a>,
    speed: u64,
    package_label: Option<&'a str>,
}

struct BinaryDashboardFileRowRef<'a> {
    projection: BinaryDashboardFileProjection<'a>,
}

#[derive(Clone, Copy)]
enum BinaryDashboardFileStatusRef<'a> {
    Queued,
    Downloading,
    Verifying,
    Complete,
    Error { message: &'a str },
}

struct BinaryDashboardRowRef<'a>(&'a TuiRow);

struct BinaryCorePackageRowRef<'a> {
    app: &'a App,
    package: &'a crate::core::PackageState,
    file_ids: Vec<&'a FileId>,
    stats: CorePackageStats<'a>,
}

struct BinaryCorePackageRowBuilder<'a> {
    package: &'a crate::core::PackageState,
    file_ids: Vec<&'a FileId>,
    stats: CorePackageStats<'a>,
}

struct BinaryLegacyPackageRowRef<'a> {
    app: &'a App,
    file: &'a super::app::FileEntry,
}

#[cfg(test)]
struct CorePackageRowRef<'a> {
    app: &'a App,
    package: &'a crate::core::PackageState,
    stats: CorePackageStats<'a>,
}

#[cfg(test)]
struct LegacyPackageRowRef<'a> {
    app: &'a App,
    file: &'a super::app::FileEntry,
}

#[cfg(test)]
struct DashboardFileRowRef<'a> {
    app: &'a App,
    file: &'a super::app::FileEntry,
}

struct PackageFileIdsRef<'a>(&'a [&'a FileId]);

struct SingleFileIdRef<'a>(&'a FileId);

#[derive(Clone, Copy)]
struct CorePackageStats<'a> {
    source_url: &'a str,
    present_files: usize,
    completed_files: usize,
    downloaded_bytes: u64,
    total_bytes: u64,
    downloading: bool,
    verifying: bool,
    folder_label: Option<&'a str>,
    folder_conflict: bool,
}

#[cfg(test)]
thread_local! {
    static CORE_PACKAGE_STATS_CALLS: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
fn reset_core_package_stats_call_count() {
    CORE_PACKAGE_STATS_CALLS.with(|count| count.set(0));
}

#[cfg(test)]
fn core_package_stats_call_count() -> usize {
    CORE_PACKAGE_STATS_CALLS.with(Cell::get)
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
#[cfg(test)]
enum DashboardFileStatusRef<'a> {
    Queued,
    Downloading,
    Verifying,
    Complete,
    Error { message: &'a str },
}

#[cfg(test)]
impl<'a> DashboardStateRef<'a> {
    fn new(app: &'a App, ui_mode: DashboardUiMode, read_only: bool) -> Self {
        Self {
            authenticated: app.authenticated,
            paused: app.paused,
            logging_in: app.login.logging_in,
            login_error: app.login.error.as_deref(),
            popup: app.popup,
            ui_mode,
            read_only,
            status: &app.status,
            packages: DashboardPackagesRef(app),
            files: DashboardFilesRef(app),
            rows: DashboardRowsRef {
                rows: app.visible_rows_snapshot(),
            },
            totals: app.dashboard_totals(),
            metrics: DashboardMetrics {
                cpu_usage: app.cpu_usage,
                memory_rss: app.memory_rss,
                api_port: app.api_port,
            },
            config: &app.config.config,
        }
    }
}

impl<'a> BinaryDashboardStateRef<'a> {
    fn new(app: &'a App, ui_mode: DashboardUiMode, read_only: bool) -> Self {
        Self {
            authenticated: app.authenticated,
            paused: app.paused,
            logging_in: app.login.logging_in,
            login_error: app.login.error.as_deref(),
            popup: app.popup,
            ui_mode,
            read_only,
            status: &app.status,
            packages: BinaryDashboardPackagesRef(app),
            files: BinaryDashboardFilesRef(app),
            rows: BinaryDashboardRowsRef {
                rows: app.visible_rows_snapshot(),
            },
            totals: app.dashboard_totals(),
            metrics: DashboardMetrics {
                cpu_usage: app.cpu_usage,
                memory_rss: app.memory_rss,
                api_port: app.api_port,
            },
            config: &app.config.config,
        }
    }
}

#[cfg(test)]
impl Serialize for DashboardPackagesRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let app = self.0;
        if !app.core_state.packages.is_empty() {
            let package_rows = app
                .core_state
                .packages
                .values()
                .filter_map(|package| {
                    let stats = CorePackageStats::new(app, package);
                    (stats.present_files > 0).then_some((package, stats))
                })
                .collect::<Vec<_>>();
            let mut seq = serializer.serialize_seq(Some(package_rows.len()))?;
            for (package, stats) in package_rows {
                seq.serialize_element(&CorePackageRowRef {
                    app,
                    package,
                    stats,
                })?;
            }
            seq.end()
        } else {
            let mut seq = serializer.serialize_seq(Some(app.files.len()))?;
            for file in &app.files {
                seq.serialize_element(&LegacyPackageRowRef { app, file })?;
            }
            seq.end()
        }
    }
}

#[cfg(test)]
impl Serialize for DashboardFilesRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(self.0.files.len()))?;
        for file in &self.0.files {
            seq.serialize_element(&DashboardFileRowRef { app: self.0, file })?;
        }
        seq.end()
    }
}

impl Serialize for BinaryDashboardFilesRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(self.0.files.len()))?;
        for file in &self.0.files {
            seq.serialize_element(&BinaryDashboardFileRowRef {
                projection: BinaryDashboardFileProjection::new(self.0, file),
            })?;
        }
        seq.end()
    }
}

#[cfg(test)]
impl Serialize for DashboardRowsRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let rows = self.rows.as_slice();
        let mut seq = serializer.serialize_seq(Some(rows.len()))?;
        for row in rows {
            seq.serialize_element(&DashboardRowRef(row))?;
        }
        seq.end()
    }
}

impl Serialize for BinaryDashboardRowsRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let rows = self.rows.as_slice();
        let mut seq = serializer.serialize_seq(Some(rows.len()))?;
        for row in rows {
            seq.serialize_element(&BinaryDashboardRowRef(row))?;
        }
        seq.end()
    }
}

#[cfg(test)]
impl Serialize for CorePackageRowRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let status = if self.stats.downloading || self.stats.verifying {
            PackageStatus::Downloading
        } else {
            self.package.status()
        };
        let expanded = self.app.expanded_packages.contains(&self.package.id)
            || matches!(self.package.status(), PackageStatus::Failed);
        let folder_label = (!self.stats.folder_conflict)
            .then_some(self.stats.folder_label)
            .flatten();
        let mut row = serializer.serialize_struct("DashboardPackageRow", 13)?;
        let file_ids = self
            .app
            .core_state
            .package_files(&self.package.id)
            .map(|file| &file.id)
            .collect::<Vec<_>>();
        row.serialize_field("id", &DisplayRef(self.package.id))?;
        row.serialize_field("source_url", self.stats.source_url)?;
        row.serialize_field("display_name", &self.package.display_name)?;
        row.serialize_field("status", &status)?;
        row.serialize_field("file_ids", &PackageFileIdsRef(file_ids.as_slice()))?;
        row.serialize_field("present_files", &self.stats.present_files)?;
        row.serialize_field("completed_files", &self.stats.completed_files)?;
        row.serialize_field("downloaded_bytes", &self.stats.downloaded_bytes)?;
        row.serialize_field("total_bytes", &self.stats.total_bytes)?;
        row.serialize_field(
            "percent",
            &percent(self.stats.downloaded_bytes, self.stats.total_bytes),
        )?;
        row.serialize_field("expanded", &expanded)?;
        row.serialize_field("folder_label", &folder_label)?;
        row.serialize_field("error", &self.package.error.as_deref())?;
        row.end()
    }
}

#[cfg(test)]
impl Serialize for LegacyPackageRowRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let status = if matches!(self.file.status, FileStatus::Error(_)) {
            PackageStatus::Failed
        } else if matches!(self.file.status, FileStatus::Downloading) {
            PackageStatus::Downloading
        } else if matches!(self.file.status, FileStatus::Complete) {
            PackageStatus::Complete
        } else {
            PackageStatus::Queued
        };
        let downloaded = if matches!(self.file.status, FileStatus::Complete) {
            self.file.size
        } else if self.file.size > 0 && self.file.downloaded >= self.file.size {
            self.file.size.saturating_sub(1)
        } else {
            self.file.downloaded.min(self.file.size)
        };
        let source_url = self
            .app
            .overlay_files
            .get(&self.file.id)
            .and_then(|overlay| overlay.source_url())
            .unwrap_or_else(|| self.file.id.as_str());
        let mut row = serializer.serialize_struct("DashboardPackageRow", 13)?;
        row.serialize_field("id", self.file.id.as_str())?;
        row.serialize_field("source_url", source_url)?;
        row.serialize_field("display_name", &self.file.name)?;
        row.serialize_field("status", &status)?;
        row.serialize_field("file_ids", &SingleFileIdRef(&self.file.id))?;
        row.serialize_field("present_files", &1_usize)?;
        row.serialize_field(
            "completed_files",
            &usize::from(matches!(self.file.status, FileStatus::Complete)),
        )?;
        row.serialize_field("downloaded_bytes", &downloaded)?;
        row.serialize_field("total_bytes", &self.file.size)?;
        row.serialize_field("percent", &percent(downloaded, self.file.size))?;
        row.serialize_field("expanded", &false)?;
        row.serialize_field("folder_label", &Option::<&str>::None)?;
        let error = match &self.file.status {
            FileStatus::Error(message) => Some(message.as_str()),
            _ => None,
        };
        row.serialize_field("error", &error)?;
        row.end()
    }
}

#[cfg(test)]
impl Serialize for DashboardFileRowRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let package_id = self
            .app
            .core_state
            .files
            .get(&self.file.id)
            .map(|core_file| PackageIdRef::Core(core_file.package_id))
            .or_else(|| {
                self.app
                    .overlay_files
                    .get(&self.file.id)
                    .and_then(|overlay| overlay.source_url().map(PackageIdRef::Overlay))
            })
            .unwrap_or(PackageIdRef::Empty);
        let status = file_status_ref(self.app, self.file);
        let package_label = package_label_for_file_ref(self.app, &self.file.id);
        let mut row = serializer.serialize_struct("DashboardFileRow", 8)?;
        row.serialize_field("id", self.file.id.as_str())?;
        row.serialize_field("package_id", &package_id)?;
        row.serialize_field("name", &self.file.name)?;
        row.serialize_field("size", &self.file.size)?;
        row.serialize_field("downloaded", &self.file.downloaded)?;
        row.serialize_field("speed", &self.app.file_speed(&self.file.id))?;
        row.serialize_field("status", &status)?;
        row.serialize_field("package_label", &package_label)?;
        row.end()
    }
}

impl Serialize for BinaryDashboardFileRowRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let file = self.projection.file;
        let mut row = serializer.serialize_struct("BinaryDashboardFileRow", 8)?;
        row.serialize_field("id", file.id.as_str())?;
        row.serialize_field("package_id", &self.projection.package_id)?;
        row.serialize_field("name", &file.name)?;
        row.serialize_field("size", &file.size)?;
        row.serialize_field("downloaded", &file.downloaded)?;
        row.serialize_field("speed", &self.projection.speed)?;
        row.serialize_field("status", &self.projection.status)?;
        row.serialize_field("package_label", &self.projection.package_label)?;
        row.end()
    }
}

impl Serialize for BinaryDashboardFileStatusRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Queued => {
                serializer.serialize_unit_variant("BinaryDashboardFileStatus", 0, "Queued")
            }
            Self::Downloading => {
                serializer.serialize_unit_variant("BinaryDashboardFileStatus", 1, "Downloading")
            }
            Self::Verifying => {
                serializer.serialize_unit_variant("BinaryDashboardFileStatus", 2, "Verifying")
            }
            Self::Complete => {
                serializer.serialize_unit_variant("BinaryDashboardFileStatus", 3, "Complete")
            }
            Self::Error { message } => {
                let mut row = serializer.serialize_struct_variant(
                    "BinaryDashboardFileStatus",
                    4,
                    "Error",
                    1,
                )?;
                row.serialize_field("message", message)?;
                row.end()
            }
        }
    }
}

impl Serialize for PackageFileIdsRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for file_id in self.0 {
            seq.serialize_element(file_id.as_str())?;
        }
        seq.end()
    }
}

impl Serialize for SingleFileIdRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(1))?;
        seq.serialize_element(self.0.as_str())?;
        seq.end()
    }
}

#[cfg(test)]
struct DashboardRowRef<'a>(&'a TuiRow);

#[cfg(test)]
impl Serialize for DashboardRowRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self.0 {
            TuiRow::Package(package_id) => {
                let mut row = serializer.serialize_struct("DashboardRow", 2)?;
                row.serialize_field("kind", "package")?;
                row.serialize_field("package_id", &DisplayRef(*package_id))?;
                row.end()
            }
            TuiRow::File {
                package_id,
                file_id,
            } => {
                let mut row = serializer.serialize_struct("DashboardRow", 3)?;
                row.serialize_field("kind", "file")?;
                row.serialize_field("package_id", &OptionalPackageIdRef(*package_id))?;
                row.serialize_field("file_id", file_id.as_str())?;
                row.end()
            }
        }
    }
}

impl Serialize for BinaryDashboardRowRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self.0 {
            TuiRow::Package(package_id) => {
                let mut row =
                    serializer.serialize_struct_variant("BinaryDashboardRow", 0, "Package", 1)?;
                row.serialize_field(
                    "package_id",
                    &BinaryDashboardPackageIdRef::Core(*package_id),
                )?;
                row.end()
            }
            TuiRow::File {
                package_id,
                file_id,
            } => {
                let mut row =
                    serializer.serialize_struct_variant("BinaryDashboardRow", 1, "File", 2)?;
                row.serialize_field(
                    "package_id",
                    &BinaryDashboardOptionalPackageIdRef(*package_id),
                )?;
                row.serialize_field("file_id", file_id.as_str())?;
                row.end()
            }
        }
    }
}

#[cfg(test)]
struct DisplayRef<T>(T);

#[cfg(test)]
impl<T> Serialize for DisplayRef<T>
where
    T: std::fmt::Display,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_str(&self.0)
    }
}

#[cfg(test)]
struct OptionalPackageIdRef(Option<PackageId>);

#[cfg(test)]
impl Serialize for OptionalPackageIdRef {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if let Some(package_id) = self.0 {
            serializer.collect_str(&package_id)
        } else {
            serializer.serialize_str("")
        }
    }
}

#[derive(Clone, Copy)]
enum BinaryDashboardPackageIdRef<'a> {
    Core(PackageId),
    Text(&'a str),
    Empty,
}

impl Serialize for BinaryDashboardPackageIdRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Core(package_id) => serializer.serialize_newtype_variant(
                "BinaryDashboardPackageId",
                0,
                "Core",
                package_id,
            ),
            Self::Text(value) => {
                serializer.serialize_newtype_variant("BinaryDashboardPackageId", 1, "Text", value)
            }
            Self::Empty => {
                serializer.serialize_unit_variant("BinaryDashboardPackageId", 2, "Empty")
            }
        }
    }
}

struct BinaryDashboardOptionalPackageIdRef(Option<PackageId>);

impl Serialize for BinaryDashboardOptionalPackageIdRef {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self.0 {
            Some(package_id) => BinaryDashboardPackageIdRef::Core(package_id).serialize(serializer),
            None => BinaryDashboardPackageIdRef::Empty.serialize(serializer),
        }
    }
}

#[cfg(test)]
enum PackageIdRef<'a> {
    Core(PackageId),
    Overlay(&'a str),
    Empty,
}

#[cfg(test)]
impl Serialize for PackageIdRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Core(package_id) => serializer.collect_str(package_id),
            Self::Overlay(package_id) => serializer.serialize_str(package_id),
            Self::Empty => serializer.serialize_str(""),
        }
    }
}

#[cfg(test)]
enum PackageLabelRef<'a> {
    Borrowed(&'a str),
    Folder(&'a str),
}

#[cfg(test)]
impl Serialize for PackageLabelRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Borrowed(label) | Self::Folder(label) => serializer.serialize_str(label),
        }
    }
}

impl<'a> CorePackageStats<'a> {
    #[cfg(test)]
    fn new(app: &'a App, package: &'a crate::core::PackageState) -> Self {
        Self::from_files(app, app.core_state.package_files(&package.id))
    }

    #[cfg(test)]
    fn from_files(app: &'a App, files: impl IntoIterator<Item = &'a FileState>) -> Self {
        CORE_PACKAGE_STATS_CALLS.with(|count| count.set(count.get().saturating_add(1)));
        let mut stats = Self {
            source_url: "",
            present_files: 0,
            completed_files: 0,
            downloaded_bytes: 0,
            total_bytes: 0,
            downloading: false,
            verifying: false,
            folder_label: None,
            folder_conflict: false,
        };
        for file in files {
            stats.record_file(app, file);
        }
        stats
    }

    fn record_file(&mut self, app: &App, file: &'a FileState) {
        if self.source_url.is_empty() {
            self.source_url = &file.source_url;
        }
        self.downloading |= matches!(file.lifecycle, FileLifecycle::Downloading);
        self.verifying |= app.is_verification_active(&file.id);
        let folder = file.path.split('/').next().filter(|part| !part.is_empty());
        match (self.folder_label, folder) {
            (None, Some(folder)) => self.folder_label = Some(folder),
            (Some(existing), Some(folder)) if existing == folder => {}
            (Some(_), Some(_)) => self.folder_conflict = true,
            _ => {}
        }

        let file_complete = matches!(file.lifecycle, FileLifecycle::Complete);
        let visible = if file_complete {
            file.size
        } else {
            crate::core::visible_completed_bytes_for_display(file)
        };
        self.present_files += 1;
        self.completed_files += usize::from(file_complete);
        self.downloaded_bytes = self.downloaded_bytes.saturating_add(visible);
        self.total_bytes = self.total_bytes.saturating_add(file.size);
    }
}

#[cfg(test)]
fn file_status_ref<'a>(app: &App, file: &'a super::app::FileEntry) -> DashboardFileStatusRef<'a> {
    if app.is_verification_active(&file.id) {
        return DashboardFileStatusRef::Verifying;
    }
    match &file.status {
        FileStatus::Queued => DashboardFileStatusRef::Queued,
        FileStatus::Downloading => DashboardFileStatusRef::Downloading,
        FileStatus::Complete => DashboardFileStatusRef::Complete,
        FileStatus::Error(message) => DashboardFileStatusRef::Error { message },
    }
}

impl<'a> BinaryDashboardFileProjection<'a> {
    fn new(app: &'a App, file: &'a super::app::FileEntry) -> Self {
        let core_file = app.core_state.files.get(&file.id);
        let overlay_file = core_file
            .is_none()
            .then(|| app.overlay_files.get(&file.id))
            .flatten();
        let package_id = core_file
            .map(|core_file| BinaryDashboardPackageIdRef::Core(core_file.package_id))
            .or_else(|| {
                overlay_file
                    .and_then(|overlay| overlay.source_url().map(BinaryDashboardPackageIdRef::Text))
            })
            .unwrap_or(BinaryDashboardPackageIdRef::Empty);
        let package_label = if let Some(core_file) = core_file {
            let configured = app
                .core_state
                .packages
                .get(&core_file.package_id)
                .map(|package| package.display_name.as_str());
            if configured.is_some_and(|label| {
                !label.starts_with("http://") && !label.starts_with("https://")
            }) {
                configured
            } else {
                Some(folder_label_from_path_ref(&core_file.path))
            }
        } else {
            overlay_file.and_then(|overlay| {
                overlay
                    .source_url()
                    .filter(|label| !label.starts_with("http://") && !label.starts_with("https://"))
                    .or_else(|| Some(folder_label_from_path_ref(&overlay.file().name)))
            })
        };
        let status = if app.is_verification_active(&file.id) {
            BinaryDashboardFileStatusRef::Verifying
        } else {
            match &file.status {
                FileStatus::Queued => BinaryDashboardFileStatusRef::Queued,
                FileStatus::Downloading => BinaryDashboardFileStatusRef::Downloading,
                FileStatus::Complete => BinaryDashboardFileStatusRef::Complete,
                FileStatus::Error(message) => BinaryDashboardFileStatusRef::Error { message },
            }
        };

        Self {
            file,
            package_id,
            status,
            speed: app.file_ui.get(&file.id).map_or(0, |state| state.speed),
            package_label,
        }
    }
}

fn binary_core_package_rows(app: &App) -> Vec<BinaryCorePackageRowRef<'_>> {
    let package_positions = app.core_state.package_positions();
    let mut package_rows = app
        .core_state
        .packages
        .values()
        .map(|package| {
            #[cfg(test)]
            CORE_PACKAGE_STATS_CALLS.with(|count| count.set(count.get().saturating_add(1)));
            BinaryCorePackageRowBuilder {
                package,
                file_ids: Vec::with_capacity(package.progress.file_count()),
                stats: CorePackageStats {
                    source_url: "",
                    present_files: 0,
                    completed_files: 0,
                    downloaded_bytes: 0,
                    total_bytes: 0,
                    downloading: false,
                    verifying: false,
                    folder_label: None,
                    folder_conflict: false,
                },
            }
        })
        .collect::<Vec<_>>();
    for file in app.core_state.files.values() {
        let Some(&package_index) = package_positions.get(&file.package_id) else {
            continue;
        };
        let package_row = &mut package_rows[package_index];
        package_row.file_ids.push(&file.id);
        package_row.stats.record_file(app, file);
    }
    package_rows
        .into_iter()
        .filter_map(|row| {
            (row.stats.present_files > 0).then_some(BinaryCorePackageRowRef {
                app,
                package: row.package,
                file_ids: row.file_ids,
                stats: row.stats,
            })
        })
        .collect()
}

#[cfg(test)]
fn package_label_for_file_ref<'a>(app: &'a App, file_id: &FileId) -> Option<PackageLabelRef<'a>> {
    if let Some(core_file) = app.core_state.files.get(file_id) {
        let configured = app
            .core_state
            .packages
            .get(&core_file.package_id)
            .map(|package| package.display_name.as_str());
        if configured
            .is_some_and(|label| !label.starts_with("http://") && !label.starts_with("https://"))
        {
            return configured.map(PackageLabelRef::Borrowed);
        }
        return Some(PackageLabelRef::Folder(folder_label_from_path_ref(
            &core_file.path,
        )));
    }

    app.overlay_files.get(file_id).map(|file| {
        file.source_url()
            .filter(|label| !label.starts_with("http://") && !label.starts_with("https://"))
            .map_or_else(
                || PackageLabelRef::Folder(folder_label_from_path_ref(&file.file().name)),
                PackageLabelRef::Borrowed,
            )
    })
}

fn folder_label_from_path_ref(path: &str) -> &str {
    path.split('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or(path)
}

impl App {
    #[cfg_attr(not(test), allow(dead_code))]
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
                    status: if self.is_verification_active(&file.id) {
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

    #[cfg(test)]
    pub(crate) fn borrowed_dashboard_json(
        &self,
        ui_mode: DashboardUiMode,
        read_only: bool,
    ) -> String {
        serde_json::to_string(&DashboardStateRef::new(self, ui_mode, read_only))
            .expect("dashboard state should serialize")
    }

    pub(crate) fn borrowed_dashboard_postcard(
        &self,
        ui_mode: DashboardUiMode,
        read_only: bool,
    ) -> Vec<u8> {
        postcard::to_stdvec(&BinaryDashboardStateRef::new(self, ui_mode, read_only))
            .expect("dashboard state should serialize")
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

    #[cfg_attr(not(test), allow(dead_code))]
    fn dashboard_packages(&self) -> Vec<DashboardPackageRow> {
        if !self.core_state.packages.is_empty() {
            let mut package_files = self
                .core_state
                .packages
                .values()
                .map(|package| {
                    (
                        package.id,
                        Vec::with_capacity(package.progress.file_count()),
                    )
                })
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
                        package_verifying |= self.is_verification_active(&file.id);
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
                            package.status()
                        },
                        file_ids,
                        present_files: present,
                        completed_files: complete,
                        downloaded_bytes: downloaded,
                        total_bytes: size,
                        percent: percent(downloaded, size),
                        expanded: self.expanded_packages.contains(&package.id)
                            || matches!(package.status(), PackageStatus::Failed),
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

    fn assert_borrowed_json_matches_owned_state(
        app: &App,
        ui_mode: DashboardUiMode,
        read_only: bool,
    ) {
        let borrowed: serde_json::Value =
            serde_json::from_str(&app.dashboard_json(ui_mode, read_only)).unwrap();
        let owned = serde_json::to_value(app.dashboard_state(ui_mode, read_only)).unwrap();
        assert_eq!(borrowed, owned);
    }

    fn assert_borrowed_postcard_matches_owned_state(
        app: &App,
        ui_mode: DashboardUiMode,
        read_only: bool,
    ) {
        let borrowed: DownloadDashboardState =
            dashboard_state_from_postcard(&app.borrowed_dashboard_postcard(ui_mode, read_only))
                .unwrap();
        let owned = app.dashboard_state(ui_mode, read_only);
        assert_eq!(borrowed, owned);
    }

    #[test]
    fn borrowed_dashboard_postcard_computes_package_stats_once_per_package() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, tx, true);
        resolve_test_package(
            &mut app,
            "Package",
            vec![
                ("one.bin", "folder/one.bin", 100),
                ("two.bin", "folder/two.bin", 300),
            ],
        );
        reset_core_package_stats_call_count();

        let _: DownloadDashboardState = dashboard_state_from_postcard(
            &app.borrowed_dashboard_postcard(DashboardUiMode::Tui, false),
        )
        .unwrap();

        assert_eq!(core_package_stats_call_count(), 1);
    }

    #[test]
    fn borrowed_dashboard_postcard_does_not_embed_core_package_uuid_text() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, tx, true);
        resolve_test_package(
            &mut app,
            "Package",
            vec![("one.bin", "folder/one.bin", 100)],
        );
        let package_id = app
            .core_state
            .packages
            .keys()
            .next()
            .expect("package should exist")
            .to_string()
            .into_bytes();

        let bytes = app.borrowed_dashboard_postcard(DashboardUiMode::Tui, false);

        assert!(
            !bytes
                .windows(package_id.len())
                .any(|window| window == package_id.as_slice()),
            "binary dashboard payload should not embed package UUID text",
        );
    }

    #[test]
    fn borrowed_dashboard_postcard_preserves_package_file_id_order() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, tx, true);
        let package_id = package_id("pkg", "https://mega.nz/folder/pkg");
        resolve_test_package(
            &mut app,
            "Package",
            vec![
                ("one.bin", "folder/one.bin", 100),
                ("two.bin", "folder/two.bin", 200),
                ("three.bin", "folder/three.bin", 300),
            ],
        );
        app.apply_core_event(CoreEvent::FileMoveRequested {
            file_id: "three.bin".into(),
            delta: -2,
        });

        let state = dashboard_state_from_postcard(
            &app.borrowed_dashboard_postcard(DashboardUiMode::Tui, false),
        )
        .unwrap();

        assert_eq!(state.packages.len(), 1);
        assert_eq!(state.packages[0].id, package_id.to_string());
        assert_eq!(
            state.packages[0].file_ids,
            vec![
                "three.bin".to_string(),
                "one.bin".to_string(),
                "two.bin".to_string()
            ]
        );
    }

    #[test]
    fn borrowed_dashboard_postcard_preserves_text_and_empty_package_ids() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, tx, true);
        app.upsert_overlay_file(
            crate::tui::app::FileEntry {
                id: "overlay/with-source.bin".into(),
                name: "with-source.bin".to_string(),
                size: 10,
                downloaded: 3,
                status: FileStatus::Queued,
            },
            Some("https://mega.nz/folder/text-id".to_string()),
        );
        app.upsert_overlay_file(
            crate::tui::app::FileEntry {
                id: "overlay/no-source.bin".into(),
                name: "no-source.bin".to_string(),
                size: 20,
                downloaded: 0,
                status: FileStatus::Queued,
            },
            None,
        );

        let state = dashboard_state_from_postcard(
            &app.borrowed_dashboard_postcard(DashboardUiMode::Tui, false),
        )
        .unwrap();

        assert!(
            state
                .files
                .iter()
                .any(|file| file.id == "overlay/with-source.bin"
                    && file.package_id == "https://mega.nz/folder/text-id")
        );
        assert!(
            state
                .files
                .iter()
                .any(|file| file.id == "overlay/no-source.bin" && file.package_id.is_empty())
        );
        assert!(state.rows.iter().any(|row| matches!(
            row,
            DashboardRow::File {
                package_id,
                file_id
            } if file_id == "overlay/no-source.bin" && package_id.is_empty()
        )));
    }

    fn resolve_test_package(app: &mut App, display_name: &str, files: Vec<(&str, &str, u64)>) {
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id("pkg", "https://mega.nz/folder/pkg"),
                source_url: "https://mega.nz/folder/pkg".to_string(),
                key: crate::core::PackageKey::new("https://mega.nz/folder/pkg".to_string()),
                display_name: display_name.to_string(),
                files: files
                    .into_iter()
                    .map(|(file_id, path, size)| ResolvedFile {
                        file_id: file_id.into(),
                        path: path.to_string(),
                        size,
                    })
                    .collect(),
                collision: None,
            },
        });
    }

    #[test]
    fn borrowed_dashboard_json_matches_owned_core_package_projection() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, tx, true);
        app.status = "busy".to_string();
        app.paused = true;
        app.login.error = Some("bad credentials".to_string());
        resolve_test_package(
            &mut app,
            "Package",
            vec![
                ("one.bin", "folder/one.bin", 100),
                ("two.bin", "folder/two.bin", 300),
            ],
        );
        app.apply_core_event(CoreEvent::FileStarted {
            file_id: "one.bin".into(),
            size: 100,
        });
        app.apply_core_event(CoreEvent::FileProgress {
            file_id: "one.bin".into(),
            total_bytes_delta: 40,
            network_bytes_delta: 40,
        });

        assert_borrowed_json_matches_owned_state(&app, DashboardUiMode::Headless, true);
        assert_borrowed_postcard_matches_owned_state(&app, DashboardUiMode::Headless, true);
    }

    #[test]
    fn borrowed_dashboard_json_matches_owned_legacy_overlay_error_projection() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, tx, true);
        app.show_overlay_error(&"overlay.bin".into(), "folder/overlay.bin", "boom");
        app.status = "failed".to_string();

        assert_borrowed_json_matches_owned_state(&app, DashboardUiMode::Tui, false);
        assert_borrowed_postcard_matches_owned_state(&app, DashboardUiMode::Tui, false);
    }

    #[test]
    fn borrowed_dashboard_json_matches_owned_verification_projection() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, tx, true);
        resolve_test_package(
            &mut app,
            "https://mega.nz/folder/pkg",
            vec![("file.bin", "folder/file.bin", 100)],
        );
        let file_id: FileId = "file.bin".into();
        app.verifying_files.insert(file_id.clone());
        app.verification_inflight_files.insert(file_id.clone());
        app.verification_targets
            .insert(file_id, crate::tui::app::VerificationTarget::Resume);

        assert_borrowed_json_matches_owned_state(&app, DashboardUiMode::Attached, true);
        assert_borrowed_postcard_matches_owned_state(&app, DashboardUiMode::Attached, true);
    }

    #[test]
    fn borrowed_dashboard_json_matches_owned_expanded_row_projection() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, tx, true);
        let package_id = package_id("pkg", "https://mega.nz/folder/pkg");
        resolve_test_package(
            &mut app,
            "Package",
            vec![("file.bin", "folder/file.bin", 100)],
        );
        app.expanded_packages.insert(package_id);
        app.sync_visible_files();

        assert_borrowed_json_matches_owned_state(&app, DashboardUiMode::Tui, false);
        assert_borrowed_postcard_matches_owned_state(&app, DashboardUiMode::Tui, false);
    }

    #[test]
    fn borrowed_dashboard_json_matches_owned_without_visible_row_cache() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, tx, true);
        app.files.push(super::super::app::FileEntry {
            id: "loose.bin".into(),
            name: "loose.bin".to_string(),
            size: 42,
            downloaded: 7,
            status: FileStatus::Downloading,
        });

        assert_borrowed_json_matches_owned_state(&app, DashboardUiMode::Tui, false);
        assert_borrowed_postcard_matches_owned_state(&app, DashboardUiMode::Tui, false);
    }

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

        assert_eq!(state.files[0].status, DashboardFileStatus::Queued);

        app.verification_targets.insert(
            "file.bin".to_string().into(),
            crate::tui::app::VerificationTarget::Resume,
        );
        let state = app.dashboard_state(DashboardUiMode::Tui, false);

        assert_eq!(state.files[0].status, DashboardFileStatus::Verifying);
    }

    #[test]
    fn dashboard_package_ignores_stale_inflight_without_target() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(9723, tx, true);
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id("pkg", "https://mega.nz/folder/pkg"),
                source_url: "https://mega.nz/folder/pkg".to_string(),
                key: crate::core::PackageKey::new("https://mega.nz/folder/pkg".to_string()),
                display_name: "Package".to_string(),
                files: vec![ResolvedFile {
                    file_id: "file.bin".to_string().into(),
                    path: "file.bin".to_string(),
                    size: 100,
                }],
                collision: None,
            },
        });
        app.verifying_files.insert("file.bin".to_string().into());
        app.verification_inflight_files
            .insert("file.bin".to_string().into());

        let state = app.dashboard_state(DashboardUiMode::Tui, false);

        assert_eq!(state.packages[0].status, PackageStatus::Queued);
        assert_eq!(state.files[0].status, DashboardFileStatus::Queued);
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
                progress: crate::core::model::PackageProgressState::default(),
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
