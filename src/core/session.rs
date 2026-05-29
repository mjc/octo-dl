#[cfg(test)]
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::DownloadConfig;
use crate::core::model::{
    FileAccounting, FileId, FileLifecycle, FileProgressState, PackageId, PackageKey,
    SessionRunStatus, UrlId,
};

const SESSION_VERSION: u32 = 6;
const SESSION_FILE_PREFIX: &str = "session-v6-";
const SESSION_POSTCARD_EXTENSION: &str = "postcard";
const SESSION_TOML_EXTENSION: &str = "toml";

mod codec;
mod credentials;

use codec::{
    decode_snapshot, encode_snapshot, is_canonical_session_path, should_replace_session_candidate,
    temporary_save_path,
};
pub use credentials::{SavedCredentials, SavedMegaSession, decrypt_credential, encrypt_credential};

#[cfg(test)]
thread_local! {
    static TEST_STATE_DIRECTORY: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

#[cfg(test)]
pub(crate) struct StateDirectoryTestGuard {
    previous: Option<PathBuf>,
}

#[cfg(test)]
impl Drop for StateDirectoryTestGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        TEST_STATE_DIRECTORY.with(|state_dir| {
            *state_dir.borrow_mut() = previous;
        });
    }
}

#[cfg(test)]
pub(crate) fn set_state_directory_for_test(path: &Path) -> StateDirectoryTestGuard {
    TEST_STATE_DIRECTORY.with(|state_dir| {
        let previous = state_dir.replace(Some(path.to_path_buf()));
        StateDirectoryTestGuard { previous }
    })
}

#[cfg(test)]
fn test_state_dir() -> Option<PathBuf> {
    TEST_STATE_DIRECTORY.with(|state_dir| state_dir.borrow().clone())
}

#[cfg(test)]
fn default_test_state_dir() -> PathBuf {
    static DEFAULT_TEST_STATE_DIRECTORY: OnceLock<PathBuf> = OnceLock::new();
    DEFAULT_TEST_STATE_DIRECTORY
        .get_or_init(|| {
            let path = std::env::temp_dir().join(format!("octo-dl-tests-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&path);
            path
        })
        .clone()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackageSnapshot {
    pub id: PackageId,
    pub key: PackageKey,
    pub display_name: String,
    #[serde(default)]
    pub files: Vec<FileSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionUrlSnapshot {
    pub url: UrlId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileSnapshot {
    pub id: FileId,
    pub package_id: PackageId,
    pub source_url: UrlId,
    pub path: String,
    pub size: u64,
    pub lifecycle: FileLifecycle,
    pub progress: FileProgressState,
    pub accounting: FileAccounting,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub version: u32,
    pub id: String,
    pub created: DateTime<Utc>,
    pub status: SessionRunStatus,
    #[serde(default)]
    pub urls: Vec<SessionUrlSnapshot>,
    pub packages: Vec<PackageSnapshot>,
    pub config: DownloadConfig,
    pub credentials: SavedCredentials,
}

impl SessionSnapshot {
    fn canonical_state_path(id: &str) -> PathBuf {
        Self::state_dir().join(format!(
            "{SESSION_FILE_PREFIX}{id}.{SESSION_POSTCARD_EXTENSION}"
        ))
    }

    #[cfg(test)]
    fn legacy_state_path_for(id: &str) -> PathBuf {
        Self::state_dir().join(format!(
            "{SESSION_FILE_PREFIX}{id}.{SESSION_TOML_EXTENSION}"
        ))
    }

    #[must_use]
    pub fn new(config: DownloadConfig, credentials: SavedCredentials) -> Self {
        Self {
            version: SESSION_VERSION,
            id: uuid::Uuid::new_v4().to_string(),
            created: Utc::now(),
            status: SessionRunStatus::InProgress,
            urls: Vec::new(),
            packages: Vec::new(),
            config,
            credentials,
        }
    }

    #[must_use]
    pub fn state_dir() -> PathBuf {
        #[cfg(test)]
        {
            if let Some(state_dir) = test_state_dir() {
                return state_dir.join("sessions");
            }
            default_test_state_dir().join("sessions")
        }

        #[cfg(not(test))]
        std::env::var("STATE_DIRECTORY").map_or_else(
            |_| {
                dirs::data_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join("octo-dl")
                    .join("sessions")
            },
            |state_dir| PathBuf::from(state_dir).join("sessions"),
        )
    }

    #[must_use]
    pub fn state_path(&self) -> PathBuf {
        Self::canonical_state_path(&self.id)
    }

    pub fn save(&self) -> std::io::Result<()> {
        self.save_to_path(&self.state_path())
    }

    pub(crate) fn save_to_path(&self, path: &Path) -> std::io::Result<()> {
        validate_snapshot(self)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(&dir)?;
        let tmp = temporary_save_path(path);
        let bytes = encode_snapshot(path, self)?;
        std::fs::write(&tmp, bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    pub fn load(path: &Path) -> std::io::Result<Self> {
        let snapshot = decode_snapshot(path)?;
        validate_snapshot(&snapshot)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        Ok(snapshot)
    }

    pub fn latest() -> Option<Self> {
        let dir = Self::state_dir();
        let read_dir = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => return None,
        };

        let mut canonical_sessions =
            HashMap::<String, (PathBuf, SessionSnapshot, Option<SystemTime>)>::new();
        for entry in read_dir.filter_map(Result::ok) {
            let path = entry.path();
            if !is_canonical_session_path(&path) {
                continue;
            }
            let modified = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok());
            match Self::load(&path) {
                Ok(snapshot) => match canonical_sessions.entry(snapshot.id.clone()) {
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert((path, snapshot, modified));
                    }
                    std::collections::hash_map::Entry::Occupied(mut entry) => {
                        let (existing_path, _, existing_modified) = entry.get();
                        if should_replace_session_candidate(
                            &path,
                            modified,
                            existing_path,
                            *existing_modified,
                        ) {
                            entry.insert((path, snapshot, modified));
                        }
                    }
                },
                Err(error) => {
                    log::error!(
                        "Rejecting session {} during latest() scan: {error}",
                        path.display()
                    );
                }
            }
        }

        let mut canonical_sessions = canonical_sessions
            .into_values()
            .map(|(_, session, _)| session)
            .collect::<Vec<_>>();
        canonical_sessions.sort_by(|a, b| {
            session_resume_priority(b)
                .cmp(&session_resume_priority(a))
                .then_with(|| b.created.cmp(&a.created))
        });

        canonical_sessions.into_iter().next()
    }

    pub fn mark_file_complete(&mut self, file_id: &str) {
        if let Some(file) = self.find_file_mut(file_id) {
            file.lifecycle = FileLifecycle::Complete;
            file.progress.visible_completed_bytes = file.size;
            file.accounting = FileAccounting::Preexisting;
        }
    }

    pub fn mark_file_error(&mut self, file_id: &str, error: &str) {
        if let Some(file) = self.find_file_mut(file_id) {
            file.lifecycle = FileLifecycle::Failed {
                message: error.to_string(),
            };
        }
    }

    #[must_use]
    pub fn completed_count(&self) -> usize {
        self.iter_files()
            .filter(|file| matches!(file.lifecycle, FileLifecycle::Complete))
            .count()
    }

    #[must_use]
    pub fn file_count(&self) -> usize {
        self.iter_files().count()
    }

    #[must_use]
    pub fn remaining_count(&self) -> usize {
        self.iter_files()
            .filter(|file| !matches!(file.lifecycle, FileLifecycle::Complete))
            .count()
    }

    pub fn find_file(&self, file_id: &str) -> Option<&FileSnapshot> {
        self.iter_files().find(|file| file.id == file_id)
    }

    pub fn find_file_mut(&mut self, file_id: &str) -> Option<&mut FileSnapshot> {
        self.packages
            .iter_mut()
            .find_map(|package| package.files.iter_mut().find(|file| file.id == file_id))
    }

    pub fn iter_files(&self) -> impl Iterator<Item = &FileSnapshot> {
        self.packages
            .iter()
            .flat_map(|package| package.files.iter())
    }

    pub fn prune_empty_packages(&mut self) {
        self.packages.retain(|package| !package.files.is_empty());
    }
}

#[must_use]
pub fn queued_file_snapshot(
    file_id: impl Into<FileId>,
    package_id: PackageId,
    source_url: UrlId,
    path: impl Into<String>,
    size: u64,
) -> FileSnapshot {
    let file_id = file_id.into();
    let path = path.into();
    FileSnapshot {
        id: file_id,
        package_id,
        source_url,
        path,
        size,
        lifecycle: FileLifecycle::Queued,
        progress: FileProgressState::default(),
        accounting: FileAccounting::CurrentRun,
    }
}

fn session_resume_priority(snapshot: &SessionSnapshot) -> u8 {
    match snapshot.status {
        SessionRunStatus::Paused => 2,
        SessionRunStatus::InProgress => 1,
        SessionRunStatus::Completed => 0,
    }
}
pub fn validate_snapshot(snapshot: &SessionSnapshot) -> Result<(), String> {
    if snapshot.version != SESSION_VERSION {
        return Err(format!("unsupported session version {}", snapshot.version));
    }
    if snapshot.urls.is_empty() && snapshot.packages.is_empty() {
        return Err("empty sessions cannot be persisted".to_string());
    }

    let mut packages_by_id = std::collections::HashSet::new();
    let mut tracked_urls = std::collections::HashSet::new();
    for url in &snapshot.urls {
        if !tracked_urls.insert(url.url.clone()) {
            return Err(format!("duplicate tracked url {}", url.url));
        }
    }

    let mut package_keys = std::collections::HashSet::new();
    for package in &snapshot.packages {
        if !packages_by_id.insert(package.id) {
            return Err(format!("duplicate package id {}", package.id));
        }
        if !package_keys.insert(package.key.clone()) {
            return Err(format!("duplicate package key {}", package.key));
        }
    }

    let mut file_ids = std::collections::HashSet::new();
    for package in &snapshot.packages {
        if package.files.is_empty() {
            return Err(format!("empty package {} is unsupported", package.id));
        }
        for file in &package.files {
            if !file_ids.insert(file.id.clone()) {
                return Err(format!("duplicate file id {}", file.id));
            }
            if file.package_id != package.id {
                return Err(format!(
                    "file {} package_id {} does not match package {}",
                    file.id, file.package_id, package.id
                ));
            }
            if !tracked_urls.contains(&file.source_url) {
                return Err(format!(
                    "file {} references untracked source_url {}",
                    file.id, file.source_url
                ));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::StateDirectoryGuard;

    #[test]
    fn latest_ignores_non_v6_sessions_without_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = StateDirectoryGuard::set(dir.path());
        let old_path = SessionSnapshot::state_dir().join("legacy.toml");
        std::fs::create_dir_all(SessionSnapshot::state_dir()).unwrap();
        std::fs::write(&old_path, "id = 'legacy'\n").unwrap();

        assert!(SessionSnapshot::latest().is_none());
        assert!(old_path.exists());
    }

    #[test]
    fn latest_prefers_newest_canonical_session() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = StateDirectoryGuard::set(dir.path());
        let mut first = SessionSnapshot::new(
            DownloadConfig::default(),
            SavedCredentials::encrypt("a", "b", None),
        );
        first.created = Utc::now() - chrono::TimeDelta::minutes(5);
        first.urls.push(SessionUrlSnapshot {
            url: "https://mega.nz/folder/first".to_string(),
            error: None,
        });
        first.save().unwrap();

        let mut second = SessionSnapshot::new(
            DownloadConfig::default(),
            SavedCredentials::encrypt("a", "b", None),
        );
        second.urls.push(SessionUrlSnapshot {
            url: "https://mega.nz/folder/second".to_string(),
            error: None,
        });
        let second_id = second.id.clone();
        second.save().unwrap();

        let latest = SessionSnapshot::latest().unwrap();
        assert_eq!(latest.id, second_id);
    }

    #[test]
    fn latest_prefers_paused_session_over_newer_completed_stub() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = StateDirectoryGuard::set(dir.path());

        let mut paused = SessionSnapshot::new(
            DownloadConfig::default(),
            SavedCredentials::encrypt("a", "b", None),
        );
        paused.created = Utc::now() - chrono::TimeDelta::minutes(5);
        paused.status = SessionRunStatus::Paused;
        paused.urls.push(SessionUrlSnapshot {
            url: "https://mega.nz/folder/root".to_string(),
            error: None,
        });
        paused.packages.push(PackageSnapshot {
            id: PackageId::for_package_key(&PackageKey::new("Folder")),
            key: PackageKey::new("Folder"),
            display_name: "Folder".to_string(),
            files: Vec::new(),
            error: None,
        });
        paused.packages[0].files.push(FileSnapshot {
            id: "folder/file.bin".to_string().into(),
            package_id: PackageId::for_package_key(&PackageKey::new("Folder")),
            source_url: "https://mega.nz/folder/root".to_string(),
            path: "folder/file.bin".to_string(),
            size: 10,
            lifecycle: FileLifecycle::Queued,
            progress: FileProgressState::default(),
            accounting: FileAccounting::CurrentRun,
        });
        paused.save().unwrap();

        let mut completed = SessionSnapshot::new(
            DownloadConfig::default(),
            SavedCredentials::encrypt("a", "b", None),
        );
        completed.status = SessionRunStatus::Completed;
        completed.urls.push(SessionUrlSnapshot {
            url: "https://mega.nz/folder/newer".to_string(),
            error: None,
        });
        completed.save().unwrap();

        let latest = SessionSnapshot::latest().unwrap();
        assert_eq!(latest.status, SessionRunStatus::Paused);
        assert_eq!(latest.file_count(), 1);
        assert!(completed.state_path().exists());
        assert!(paused.state_path().exists());
    }

    #[test]
    fn load_supports_legacy_toml_session_paths() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = StateDirectoryGuard::set(dir.path());
        let mut session = SessionSnapshot::new(
            DownloadConfig::default(),
            SavedCredentials::encrypt("a", "b", None),
        );
        session.urls.push(SessionUrlSnapshot {
            url: "https://mega.nz/folder/legacy".to_string(),
            error: None,
        });
        let legacy_path = SessionSnapshot::legacy_state_path_for(&session.id);

        session.save_to_path(&legacy_path).unwrap();

        let loaded = SessionSnapshot::load(&legacy_path).unwrap();
        assert_eq!(loaded.id, session.id);
        assert_eq!(loaded.urls, session.urls);
    }

    #[test]
    fn latest_prefers_postcard_snapshot_over_legacy_toml_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = StateDirectoryGuard::set(dir.path());
        let mut session = SessionSnapshot::new(
            DownloadConfig::default(),
            SavedCredentials::encrypt("a", "b", None),
        );
        session.urls.push(SessionUrlSnapshot {
            url: "https://mega.nz/folder/root".to_string(),
            error: None,
        });
        let legacy_path = SessionSnapshot::legacy_state_path_for(&session.id);
        session.save_to_path(&legacy_path).unwrap();

        session.status = SessionRunStatus::Paused;
        session.save().unwrap();
        assert_eq!(
            SessionSnapshot::load(&session.state_path()).unwrap().status,
            SessionRunStatus::Paused
        );

        let latest = SessionSnapshot::latest().unwrap();
        assert_eq!(latest.id, session.id);
        assert_eq!(latest.status, SessionRunStatus::Paused);
    }

    #[test]
    fn latest_loads_canonical_multi_source_package_session() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = StateDirectoryGuard::set(dir.path());

        let package_key = PackageKey::new("Folder");
        let package_id = PackageId::for_package_key(&package_key);
        let mut session = SessionSnapshot::new(
            DownloadConfig::default(),
            SavedCredentials::encrypt("a", "b", None),
        );
        session.urls = vec![
            SessionUrlSnapshot {
                url: "https://mega.nz/folder/one".to_string(),
                error: None,
            },
            SessionUrlSnapshot {
                url: "https://mega.nz/folder/two".to_string(),
                error: None,
            },
        ];
        session.packages.push(PackageSnapshot {
            id: package_id,
            key: package_key,
            display_name: "Folder".to_string(),
            files: Vec::new(),
            error: None,
        });
        session.packages[0].files = vec![
            FileSnapshot {
                id: "folder/a.bin".to_string().into(),
                package_id,
                source_url: "https://mega.nz/folder/one".to_string(),
                path: "folder/a.bin".to_string(),
                size: 10,
                lifecycle: FileLifecycle::Queued,
                progress: FileProgressState::default(),
                accounting: FileAccounting::CurrentRun,
            },
            FileSnapshot {
                id: "folder/b.bin".to_string().into(),
                package_id,
                source_url: "https://mega.nz/folder/two".to_string(),
                path: "folder/b.bin".to_string(),
                size: 20,
                lifecycle: FileLifecycle::Queued,
                progress: FileProgressState::default(),
                accounting: FileAccounting::CurrentRun,
            },
        ];
        session.save().unwrap();

        let latest = SessionSnapshot::latest().unwrap();
        assert_eq!(latest.packages.len(), 1);
        assert_eq!(latest.file_count(), 2);
        assert_eq!(latest.packages[0].display_name, "Folder");
    }

    #[test]
    fn old_and_empty_sessions_are_ignored_without_legacy_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = StateDirectoryGuard::set(dir.path());

        let path = SessionSnapshot::state_dir().join("session-v5-empty.toml");
        std::fs::create_dir_all(SessionSnapshot::state_dir()).unwrap();
        std::fs::write(
            &path,
            r#"version = 5
id = "empty"
created = "2024-01-01T00:00:00Z"
status = "completed"
urls = []
packages = []
"#,
        )
        .unwrap();

        assert!(SessionSnapshot::latest().is_none());
        assert!(path.exists());
    }
}
