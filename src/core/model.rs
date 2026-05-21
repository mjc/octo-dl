use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use crate::config::DownloadConfig;
use crate::core::session::SavedCredentials;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PackageKey(String);

impl PackageKey {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PackageKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<String> for PackageKey {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for PackageKey {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PackageId(uuid::Uuid);

impl PackageId {
    #[must_use]
    pub fn new_v4() -> Self {
        Self(uuid::Uuid::new_v4())
    }

    #[must_use]
    pub fn for_package_key(package_key: &PackageKey) -> Self {
        Self(uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_OID,
            package_key.as_str().as_bytes(),
        ))
    }

    #[must_use]
    pub fn parse_or_key(raw: &str, package_key: &PackageKey) -> Self {
        raw.parse().unwrap_or_else(|_| {
            if raw == package_key.as_str() {
                return Self::for_package_key(package_key);
            }
            let scope = format!("{}\0{raw}", package_key.as_str());
            Self(uuid::Uuid::new_v5(
                &uuid::Uuid::NAMESPACE_OID,
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

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId(Arc<str>);

impl FileId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<String> for FileId {
    fn from(value: String) -> Self {
        Self(Arc::<str>::from(value))
    }
}

impl From<&str> for FileId {
    fn from(value: &str) -> Self {
        Self(Arc::<str>::from(value))
    }
}

impl From<Arc<str>> for FileId {
    fn from(value: Arc<str>) -> Self {
        Self(value)
    }
}

impl AsRef<str> for FileId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::borrow::Borrow<str> for FileId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq<&str> for FileId {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<str> for FileId {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<String> for FileId {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Serialize for FileId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for FileId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::from)
    }
}
pub type UrlId = String;

#[cfg(test)]
mod tests {
    use super::{PackageId, PackageKey};

    #[test]
    fn package_key_ids_are_stable_across_derivation_paths() {
        let package_key = PackageKey::new("folder/example");
        assert_eq!(
            PackageId::for_package_key(&package_key),
            PackageId::parse_or_key(package_key.as_str(), &package_key)
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FileAccounting {
    #[default]
    CurrentRun,
    Preexisting,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RuntimeState {
    pub active: bool,
    pub accounting: FileAccounting,
    pub reused_chunks: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FileLifecycle {
    #[default]
    Planned,
    Queued,
    Downloading,
    Complete,
    Failed {
        message: String,
    },
}

impl FileLifecycle {
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Complete)
    }

    #[must_use]
    pub fn failure_message(&self) -> Option<&str> {
        match self {
            Self::Failed { message } => Some(message),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FileProgressState {
    pub verified_existing_bytes: u64,
    pub downloaded_network_bytes: u64,
    pub visible_completed_bytes: u64,
}

#[must_use]
pub fn visible_completed_bytes_for_display(file: &FileState) -> u64 {
    file.progress.visible_completed_bytes.min(file.size)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageState {
    pub id: PackageId,
    pub key: PackageKey,
    pub display_name: String,
    #[serde(default)]
    pub file_ids: Vec<FileId>,
    pub status: PackageStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileState {
    pub id: FileId,
    pub package_id: PackageId,
    pub source_url: UrlId,
    pub path: String,
    pub size: u64,
    pub lifecycle: FileLifecycle,
    pub progress: FileProgressState,
    pub runtime: RuntimeState,
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

    pub fn move_package_by(&mut self, package_id: &PackageId, delta: isize) -> bool {
        let Some(index) = self.packages.get_index_of(package_id) else {
            return false;
        };
        let target = index.saturating_add_signed(delta);
        if target >= self.packages.len() || target == index {
            return false;
        }
        self.packages.swap_indices(index, target);
        true
    }

    pub fn move_file_within_package_by(&mut self, file_id: &FileId, delta: isize) -> bool {
        let Some(file) = self.files.get(file_id) else {
            return false;
        };
        let Some(package) = self.packages.get_mut(&file.package_id) else {
            return false;
        };
        let file_ids = &mut package.file_ids;
        let Some(index) = file_ids.iter().position(|existing| existing == file_id) else {
            return false;
        };
        let target = index.saturating_add_signed(delta);
        if target >= file_ids.len() || target == index {
            return false;
        }
        file_ids.swap(index, target);
        true
    }

    pub fn package_files(&self, package_id: &PackageId) -> impl Iterator<Item = &FileState> + '_ {
        self.packages
            .get(package_id)
            .into_iter()
            .flat_map(|package| package.file_ids.iter())
            .filter_map(|file_id| self.files.get(file_id))
    }

    #[must_use]
    pub fn package_has_files(&self, package_id: &PackageId) -> bool {
        self.packages
            .get(package_id)
            .is_some_and(|package| !package.file_ids.is_empty())
    }

    #[must_use]
    pub fn package_file_ids(&self, package_id: &PackageId) -> Vec<FileId> {
        self.packages
            .get(package_id)
            .map(|package| package.file_ids.clone())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn pending_file_ids(&self) -> Vec<FileId> {
        self.packages
            .values()
            .flat_map(|package| package.file_ids.iter())
            .filter_map(|file_id| self.files.get(file_id))
            .filter(|file| {
                !file.runtime.active
                    && !matches!(
                        file.lifecycle,
                        FileLifecycle::Complete | FileLifecycle::Failed { .. }
                    )
            })
            .map(|file| file.id.clone())
            .collect()
    }

    #[must_use]
    pub fn package_for_key(&self, package_key: &PackageKey) -> Option<&PackageState> {
        self.packages
            .values()
            .find(|package| &package.key == package_key)
    }
}
