use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

use crate::config::DownloadConfig;
use crate::core::session::SavedCredentials;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PackageId(uuid::Uuid);

impl PackageId {
    #[must_use]
    pub fn new_v4() -> Self {
        Self(uuid::Uuid::new_v4())
    }

    #[must_use]
    pub fn for_source_url(source_url: &str) -> Self {
        Self(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, source_url.as_bytes()))
    }

    #[must_use]
    pub fn parse_or_source_url(raw: &str, source_url: &str) -> Self {
        raw.parse().unwrap_or_else(|_| {
            if raw == source_url {
                return Self::for_source_url(source_url);
            }
            let scope = format!("{source_url}\0{raw}");
            Self(uuid::Uuid::new_v5(
                &uuid::Uuid::NAMESPACE_URL,
                scope.as_bytes(),
            ))
        })
    }

    #[must_use]
    pub const fn as_uuid(self) -> uuid::Uuid {
        self.0
    }
}

impl Default for PackageId {
    fn default() -> Self {
        Self::new_v4()
    }
}

impl fmt::Display for PackageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<uuid::Uuid> for PackageId {
    fn from(value: uuid::Uuid) -> Self {
        Self(value)
    }
}

impl From<PackageId> for uuid::Uuid {
    fn from(value: PackageId) -> Self {
        value.0
    }
}

impl FromStr for PackageId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        uuid::Uuid::parse_str(s).map(Self)
    }
}

impl PartialEq<&str> for PackageId {
    fn eq(&self, other: &&str) -> bool {
        self.to_string() == *other
    }
}

impl PartialEq<String> for PackageId {
    fn eq(&self, other: &String) -> bool {
        self.to_string() == *other
    }
}

pub type FileId = String;
pub type UrlId = String;

#[cfg(test)]
mod tests {
    use super::PackageId;

    #[test]
    fn source_url_ids_are_stable_across_derivation_paths() {
        let source_url = "https://mega.nz/folder/example";
        assert_eq!(
            PackageId::for_source_url(source_url),
            PackageId::parse_or_source_url(source_url, source_url)
        );
    }
}

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileState {
    pub id: FileId,
    pub package_id: PackageId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<UrlId>,
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

    #[must_use]
    pub fn package_file_ids(&self, package_id: &PackageId) -> Vec<FileId> {
        self.files
            .values()
            .filter(|file| &file.package_id == package_id)
            .map(|file| file.id.clone())
            .collect()
    }

    #[must_use]
    pub fn package_for_source_url(&self, source_url: &str) -> Option<&PackageState> {
        self.packages
            .values()
            .find(|package| package.source_url == source_url)
    }
}
