use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use rustc_hash::FxBuildHasher;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
#[cfg(test)]
use std::cell::Cell;
use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use crate::config::DownloadConfig;
use crate::core::session::SavedCredentials;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PackageKey(String);

#[cfg(test)]
thread_local! {
    static PENDING_FILE_IDS_CALLS: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_pending_file_ids_call_count() {
    PENDING_FILE_IDS_CALLS.with(|count| count.set(0));
}

#[cfg(test)]
pub(crate) fn pending_file_ids_call_count() -> usize {
    PENDING_FILE_IDS_CALLS.with(Cell::get)
}

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
    use super::{
        DownloadState, FileAccounting, FileId, FileLifecycle, FileProgressState, FileState,
        PackageId, PackageKey, PackageProgressState, PackageStatus, SessionMeta,
    };

    #[test]
    fn package_key_ids_are_stable_across_derivation_paths() {
        let package_key = PackageKey::new("folder/example");
        assert_eq!(
            PackageId::for_package_key(&package_key),
            PackageId::parse_or_key(package_key.as_str(), &package_key)
        );
    }

    #[test]
    fn download_state_files_support_borrowed_str_lookups() {
        let mut state = DownloadState::new(SessionMeta::default());
        let package_id = PackageId::new_v4();
        let file_id = FileId::from("file.bin");
        state.files.insert(
            file_id.clone(),
            FileState {
                id: file_id,
                package_id,
                source_url: "https://example.invalid/file".to_string(),
                path: "file.bin".to_string(),
                size: 42,
                lifecycle: FileLifecycle::Queued,
                progress: FileProgressState::default(),
                accounting: FileAccounting::CurrentRun,
            },
        );

        assert!(state.files.contains_key("file.bin"));
        assert_eq!(
            state.files.get("file.bin").map(|file| file.path.as_str()),
            Some("file.bin")
        );
    }

    #[test]
    fn package_progress_status_preserves_status_precedence() {
        assert_eq!(
            PackageProgressState::default().status(false),
            PackageStatus::Pending
        );
        assert_eq!(
            PackageProgressState {
                queued: 2,
                ..PackageProgressState::default()
            }
            .status(false),
            PackageStatus::Queued
        );
        assert_eq!(
            PackageProgressState {
                complete: 1,
                queued: 1,
                ..PackageProgressState::default()
            }
            .status(false),
            PackageStatus::Partial
        );
        assert_eq!(
            PackageProgressState {
                complete: 1,
                downloading: 1,
                ..PackageProgressState::default()
            }
            .status(false),
            PackageStatus::Downloading
        );
        assert_eq!(
            PackageProgressState {
                complete: 2,
                ..PackageProgressState::default()
            }
            .status(false),
            PackageStatus::Complete
        );
        assert_eq!(
            PackageProgressState {
                failed: 1,
                ..PackageProgressState::default()
            }
            .status(false),
            PackageStatus::Failed
        );
        assert_eq!(
            PackageProgressState {
                complete: 1,
                ..PackageProgressState::default()
            }
            .status(true),
            PackageStatus::Failed
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PackageProgressState {
    pub queued: usize,
    pub downloading: usize,
    pub complete: usize,
    pub failed: usize,
}

impl PackageProgressState {
    #[must_use]
    pub const fn file_count(self) -> usize {
        self.queued + self.downloading + self.complete + self.failed
    }

    #[must_use]
    pub const fn status(self, has_error: bool) -> PackageStatus {
        if has_error || self.failed > 0 {
            PackageStatus::Failed
        } else if self.downloading > 0 {
            PackageStatus::Downloading
        } else if self.complete > 0 && self.queued > 0 {
            PackageStatus::Partial
        } else if self.complete > 0 && self.file_count() > 0 {
            PackageStatus::Complete
        } else if self.queued > 0 {
            PackageStatus::Queued
        } else {
            PackageStatus::Pending
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FileAccounting {
    #[default]
    CurrentRun,
    Preexisting,
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
    #[serde(skip, default)]
    pub progress: PackageProgressState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl PackageState {
    #[must_use]
    pub const fn status(&self) -> PackageStatus {
        self.progress.status(self.error.is_some())
    }
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
    pub accounting: FileAccounting,
}

pub type PackageStateIndex = IndexMap<PackageId, PackageState, FxBuildHasher>;
pub type FileStateIndex = IndexMap<FileId, FileState, FxBuildHasher>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TotalsState {
    pub run_total_bytes: u64,
    pub run_completed_bytes: u64,
    pub run_file_total: usize,
    pub run_file_completed: usize,
    #[serde(default)]
    pub run_file_downloading: usize,
    pub displayed_network_bytes: u64,
    pub displayed_network_rate_bps: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DownloadState {
    pub packages: PackageStateIndex,
    pub files: FileStateIndex,
    pub url_order: Vec<UrlId>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub url_errors: HashMap<UrlId, String, FxBuildHasher>,
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
        self.reorder_files_by_package_order();
        self.reorder_urls_by_package_order();
        true
    }

    pub fn move_file_within_package_by(&mut self, file_id: &FileId, delta: isize) -> bool {
        let Some(file) = self.files.get(file_id) else {
            return false;
        };
        let package_id = file.package_id;
        let indices = self
            .files
            .iter()
            .enumerate()
            .filter_map(|(index, (existing_id, existing_file))| {
                (existing_file.package_id == package_id).then_some((index, existing_id))
            })
            .collect::<Vec<_>>();
        let Some(index) = indices
            .iter()
            .position(|(_, existing)| *existing == file_id)
        else {
            return false;
        };
        let target = index.saturating_add_signed(delta);
        if target >= indices.len() || target == index {
            return false;
        }
        self.files.move_index(indices[index].0, indices[target].0);
        true
    }

    pub fn package_files(&self, package_id: &PackageId) -> impl Iterator<Item = &FileState> + '_ {
        let package_id = *package_id;
        self.files
            .values()
            .filter(move |file| file.package_id == package_id)
    }

    #[must_use]
    pub fn package_has_files(&self, package_id: &PackageId) -> bool {
        self.packages
            .get(package_id)
            .is_some_and(|package| package.progress.file_count() > 0)
    }

    #[must_use]
    pub fn package_file_ids(&self, package_id: &PackageId) -> Vec<FileId> {
        self.package_files(package_id)
            .map(|file| file.id.clone())
            .collect()
    }

    #[must_use]
    pub fn pending_file_ids(&self) -> Vec<FileId> {
        #[cfg(test)]
        PENDING_FILE_IDS_CALLS.with(|count| count.set(count.get().saturating_add(1)));
        self.files
            .values()
            .filter(|file| {
                !matches!(
                    file.lifecycle,
                    FileLifecycle::Downloading
                        | FileLifecycle::Complete
                        | FileLifecycle::Failed { .. }
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

    pub(crate) fn package_insert_index(&self, package_id: &PackageId) -> usize {
        let package_positions = self.package_positions();
        let Some(&package_index) = package_positions.get(package_id) else {
            return self.files.len();
        };
        let mut insert_index = self.files.len();
        for (index, file) in self.files.values().enumerate() {
            let Some(&file_package_index) = package_positions.get(&file.package_id) else {
                continue;
            };
            if file_package_index > package_index {
                return insert_index.min(index);
            }
            insert_index = index.saturating_add(1);
        }
        insert_index
    }

    pub(crate) fn reorder_files_by_package_order(&mut self) {
        let mut grouped =
            HashMap::<PackageId, Vec<(FileId, FileState)>, FxBuildHasher>::with_hasher(
                FxBuildHasher::default(),
            );
        for (file_id, file) in std::mem::take(&mut self.files) {
            grouped
                .entry(file.package_id)
                .or_default()
                .push((file_id, file));
        }

        let mut reordered = FileStateIndex::default();
        for package_id in self.packages.keys().copied() {
            if let Some(files) = grouped.remove(&package_id) {
                for (file_id, file) in files {
                    reordered.insert(file_id, file);
                }
            }
        }
        debug_assert!(
            grouped.is_empty(),
            "all files should belong to a known package before reordering"
        );
        for files in grouped.into_values() {
            for (file_id, file) in files {
                reordered.insert(file_id, file);
            }
        }
        self.files = reordered;
    }

    fn reorder_urls_by_package_order(&mut self) {
        let mut source_url_package_ids =
            HashMap::<&str, PackageId, FxBuildHasher>::with_hasher(FxBuildHasher::default());
        for file in self.files.values() {
            source_url_package_ids
                .entry(file.source_url.as_str())
                .or_insert(file.package_id);
        }
        let mut grouped =
            HashMap::<PackageId, Vec<UrlId>, FxBuildHasher>::with_hasher(FxBuildHasher::default());
        let mut unresolved = Vec::new();
        for url in std::mem::take(&mut self.url_order) {
            let Some(&package_id) = source_url_package_ids.get(url.as_str()) else {
                unresolved.push(url);
                continue;
            };
            grouped.entry(package_id).or_default().push(url);
        }

        let mut reordered = Vec::with_capacity(grouped.len() + unresolved.len());
        for package_id in self.packages.keys().copied() {
            if let Some(urls) = grouped.remove(&package_id) {
                reordered.extend(urls);
            }
        }
        for urls in grouped.into_values() {
            reordered.extend(urls);
        }
        reordered.extend(unresolved);
        self.url_order = reordered;
    }

    pub(crate) fn package_positions(&self) -> HashMap<PackageId, usize, FxBuildHasher> {
        self.packages
            .keys()
            .copied()
            .enumerate()
            .map(|(index, package_id)| (package_id, index))
            .collect()
    }
}
