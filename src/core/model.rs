use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::config::DownloadConfig;
use crate::core::session::SavedCredentials;

pub type PackageId = String;
pub type FileId = String;
pub type UrlId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SessionRunStatus {
    #[default]
    InProgress,
    Completed,
    Paused,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: String,
    pub created: DateTime<Utc>,
    pub status: SessionRunStatus,
    pub config: DownloadConfig,
    pub credentials: SavedCredentials,
}

impl Default for SessionMeta {
    fn default() -> Self {
        Self {
            session_id: uuid::Uuid::new_v4().to_string(),
            created: Utc::now(),
            status: SessionRunStatus::InProgress,
            config: DownloadConfig::default(),
            credentials: SavedCredentials::encrypt("", "", None),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PackageStatus {
    #[default]
    Pending,
    Queued,
    Downloading,
    Partial,
    Complete,
    Failed,
    Skipped,
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DesiredState {
    #[default]
    Present,
    RetryRequested,
    ResetRequested,
    Suppressed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RuntimeState {
    pub counts_in_run_totals: bool,
    pub active: bool,
    pub preexisting_complete: bool,
    pub reused_chunks: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FileLifecycle {
    #[default]
    Planned,
    Queued,
    Downloading,
    Complete,
    Skipped,
    Deleted,
    Failed,
}

impl FileLifecycle {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Skipped | Self::Deleted)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FileProgressState {
    pub verified_existing_bytes: u64,
    pub downloaded_network_bytes: u64,
    pub visible_completed_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageState {
    pub id: PackageId,
    pub source_url: UrlId,
    pub display_name: String,
    pub status: PackageStatus,
    pub file_ids: Vec<FileId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileState {
    pub id: FileId,
    pub package_id: PackageId,
    pub path: String,
    pub size: u64,
    pub lifecycle: FileLifecycle,
    pub progress: FileProgressState,
    pub desired: DesiredState,
    pub runtime: RuntimeState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TotalsState {
    pub run_total_bytes: u64,
    pub run_completed_bytes: u64,
    pub run_file_total: usize,
    pub run_file_completed: usize,
    pub displayed_network_bytes: u64,
    pub displayed_network_rate_bps: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DownloadState {
    pub packages: IndexMap<PackageId, PackageState>,
    pub files: IndexMap<FileId, FileState>,
    pub url_order: Vec<UrlId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<FileId>,
    pub totals: TotalsState,
    pub session_meta: SessionMeta,
}

impl DownloadState {
    #[must_use]
    pub fn new(session_meta: SessionMeta) -> Self {
        Self {
            session_meta,
            ..Self::default()
        }
    }
}
