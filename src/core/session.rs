#[cfg(test)]
use std::cell::RefCell;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::OnceLock;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::DownloadConfig;
use crate::core::model::{
    DesiredState, FileId, FileLifecycle, FileProgressState, PackageId, RuntimeState,
    SessionRunStatus, UrlId,
};

const CREDENTIAL_VERSION_PREFIX: &str = "v2:";

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
pub struct SavedCredentials {
    pub email: String,
    pub password: String,
    pub mfa: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackageSnapshot {
    pub id: PackageId,
    pub source_url: UrlId,
    pub display_name: String,
    pub file_ids: Vec<FileId>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSnapshotV3 {
    pub version: u32,
    pub id: String,
    pub created: DateTime<Utc>,
    pub status: SessionRunStatus,
    #[serde(default)]
    pub urls: Vec<SessionUrlSnapshot>,
    pub packages: Vec<PackageSnapshot>,
    pub files: Vec<FileSnapshot>,
    pub config: DownloadConfig,
    pub credentials: SavedCredentials,
}

impl SessionSnapshotV3 {
    #[must_use]
    pub fn new(config: DownloadConfig, credentials: SavedCredentials) -> Self {
        Self {
            version: 4,
            id: uuid::Uuid::new_v4().to_string(),
            created: Utc::now(),
            status: SessionRunStatus::InProgress,
            urls: Vec::new(),
            packages: Vec::new(),
            files: Vec::new(),
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
        Self::state_dir().join(format!("session-v4-{}.toml", self.id))
    }

    pub fn save(&self) -> std::io::Result<()> {
        validate_snapshot(self).map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, error)
        })?;
        let dir = Self::state_dir();
        std::fs::create_dir_all(&dir)?;
        let path = self.state_path();
        let tmp = path.with_extension("toml.tmp");
        let toml = toml::to_string(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&tmp, toml)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    pub fn load(path: &Path) -> std::io::Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let snapshot: Self = toml::from_str(&contents)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if snapshot.version != 4 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "session is not canonical version 4",
            ));
        }
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

        let mut canonical_sessions = Vec::new();
        for entry in read_dir.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "toml") {
                continue;
            }
            match Self::load(&path) {
                Ok(snapshot) => canonical_sessions.push((path, snapshot)),
                Err(_) => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }

        canonical_sessions.sort_by(|a, b| b.1.created.cmp(&a.1.created));
        for (path, snapshot) in canonical_sessions.iter() {
            if snapshot.status == SessionRunStatus::Completed {
                let _ = std::fs::remove_file(path);
            }
        }
        for (path, _) in canonical_sessions.iter().skip(1) {
            let _ = std::fs::remove_file(path);
        }

        canonical_sessions
            .into_iter()
            .next()
            .map(|(_, session)| session)
    }

    pub fn mark_file_complete(&mut self, file_id: &str) {
        if let Some(file) = self.files.iter_mut().find(|file| file.id == file_id) {
            file.lifecycle = FileLifecycle::Complete;
            file.progress.visible_completed_bytes = file.size;
            file.runtime.active = false;
            file.runtime.counts_in_run_totals = false;
        }
    }

    pub fn mark_file_error(&mut self, file_id: &str, error: &str) {
        if let Some(file) = self.files.iter_mut().find(|file| file.id == file_id) {
            file.lifecycle = FileLifecycle::Failed;
            file.message = Some(error.to_string());
            file.runtime.active = false;
        }
    }

    #[must_use]
    pub fn completed_count(&self) -> usize {
        self.files
            .iter()
            .filter(|file| matches!(file.lifecycle, FileLifecycle::Complete))
            .count()
    }

    #[must_use]
    pub fn remaining_count(&self) -> usize {
        self.files
            .iter()
            .filter(|file| {
                !matches!(
                    file.lifecycle,
                    FileLifecycle::Complete | FileLifecycle::Skipped | FileLifecycle::Deleted
                )
            })
            .count()
    }
}

pub fn validate_snapshot(snapshot: &SessionSnapshotV3) -> Result<(), String> {
    if snapshot.version != 4 {
        return Err(format!("unsupported session version {}", snapshot.version));
    }

    let mut packages_by_id = IndexMap::new();
    let mut tracked_urls = std::collections::HashSet::new();
    for url in &snapshot.urls {
        if !tracked_urls.insert(url.url.clone()) {
            return Err(format!("duplicate tracked url {}", url.url));
        }
    }

    let mut package_source_urls = std::collections::HashSet::new();
    for package in &snapshot.packages {
        if packages_by_id
            .insert(package.id.clone(), package)
            .is_some()
        {
            return Err(format!("duplicate package id {}", package.id));
        }
        if !tracked_urls.contains(&package.source_url) {
            return Err(format!(
                "package {} references untracked source_url {}",
                package.id, package.source_url
            ));
        }
        if !package_source_urls.insert(package.source_url.clone()) {
            return Err(format!(
                "duplicate package source_url {}",
                package.source_url
            ));
        }
    }

    let mut grouped_files = IndexMap::<String, Vec<String>>::new();
    let mut file_ids = std::collections::HashSet::new();
    for file in &snapshot.files {
        if !file_ids.insert(file.id.clone()) {
            return Err(format!("duplicate file id {}", file.id));
        }
        let Some(package) = packages_by_id.get(&file.package_id) else {
            return Err(format!(
                "file {} references unknown package {}",
                file.id, file.package_id
            ));
        };
        if let Some(source_url) = &file.source_url
            && source_url != &package.source_url
        {
            return Err(format!(
                "file {} source_url does not match package {}",
                file.id, file.package_id
            ));
        }
        grouped_files
            .entry(file.package_id.clone())
            .or_default()
            .push(file.id.clone());
    }

    for package in &snapshot.packages {
        let grouped = grouped_files.get(&package.id).cloned().unwrap_or_default();
        if grouped != package.file_ids {
            return Err(format!(
                "package {} file_ids do not match grouped files",
                package.id
            ));
        }
        if grouped.is_empty() {
            return Err(format!("empty package {} is unsupported", package.id));
        }
    }

    Ok(())
}

fn derive_machine_key() -> [u8; 16] {
    let hostname = hostname::get().map_or_else(
        |_| "unknown-host".to_string(),
        |h| h.to_string_lossy().into_owned(),
    );
    let username = whoami::username();

    let mut hasher = Sha256::new();
    hasher.update(hostname.as_bytes());
    hasher.update(b":");
    hasher.update(username.as_bytes());
    hasher.update(b":octo-dl-session-key");
    let hash = hasher.finalize();

    let mut key = [0u8; 16];
    key.copy_from_slice(&hash[..16]);
    key
}

#[must_use]
pub fn encrypt_credential(plaintext: &str) -> String {
    let key = derive_machine_key();
    let cipher = Aes128Gcm::new(&key.into());
    let nonce_uuid = uuid::Uuid::new_v4();
    let nonce_bytes = &nonce_uuid.as_bytes()[..12];
    let nonce = Nonce::from_slice(nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .expect("AES-GCM encryption should succeed");

    let mut encoded = Vec::with_capacity(nonce_bytes.len() + ciphertext.len());
    encoded.extend_from_slice(nonce_bytes);
    encoded.extend_from_slice(&ciphertext);
    format!("{CREDENTIAL_VERSION_PREFIX}{}", BASE64.encode(encoded))
}

fn decrypt_credential_v2(encrypted: &str) -> Option<String> {
    let encoded = encrypted.strip_prefix(CREDENTIAL_VERSION_PREFIX)?;
    let data = BASE64.decode(encoded).ok()?;
    if data.len() < 13 {
        return None;
    }
    let (nonce_bytes, ciphertext) = data.split_at(12);
    let key = derive_machine_key();
    let cipher = Aes128Gcm::new(&key.into());
    let plaintext = cipher
        .decrypt(Nonce::from_slice(nonce_bytes), ciphertext)
        .ok()?;
    String::from_utf8(plaintext).ok()
}

#[must_use]
pub fn decrypt_credential(encrypted: &str) -> Option<String> {
    decrypt_credential_v2(encrypted)
}

impl SavedCredentials {
    #[must_use]
    pub fn encrypt(email: &str, password: &str, mfa: Option<&str>) -> Self {
        Self {
            email: encrypt_credential(email),
            password: encrypt_credential(password),
            mfa: mfa.map(encrypt_credential),
        }
    }

    #[must_use]
    pub fn decrypt(&self) -> Option<(String, String, Option<String>)> {
        let email = decrypt_credential(&self.email)?;
        let password = decrypt_credential(&self.password)?;
        let mfa = self.mfa.as_deref().and_then(decrypt_credential);
        Some((email, password, mfa))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::StateDirectoryGuard;

    #[test]
    fn credential_round_trip() {
        let saved = SavedCredentials::encrypt("test@example.com", "hunter2", Some("123456"));
        let (email, password, mfa) = saved.decrypt().unwrap();
        assert_eq!(email, "test@example.com");
        assert_eq!(password, "hunter2");
        assert_eq!(mfa.as_deref(), Some("123456"));
    }

    #[test]
    fn unversioned_credential_decryption_is_rejected() {
        assert!(decrypt_credential("old-secret").is_none());
    }

    #[test]
    fn latest_deletes_non_canonical_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = StateDirectoryGuard::set(dir.path());
        let old_path = SessionSnapshotV3::state_dir().join("legacy.toml");
        std::fs::create_dir_all(SessionSnapshotV3::state_dir()).unwrap();
        std::fs::write(&old_path, "id = 'legacy'\n").unwrap();

        assert!(SessionSnapshotV3::latest().is_none());
        assert!(!old_path.exists());
    }

    #[test]
    fn latest_prefers_newest_canonical_session() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = StateDirectoryGuard::set(dir.path());
        let mut first = SessionSnapshotV3::new(
            DownloadConfig::default(),
            SavedCredentials::encrypt("a", "b", None),
        );
        first.created = Utc::now() - chrono::TimeDelta::minutes(5);
        first.save().unwrap();

        let second = SessionSnapshotV3::new(
            DownloadConfig::default(),
            SavedCredentials::encrypt("a", "b", None),
        );
        let second_id = second.id.clone();
        second.save().unwrap();

        let latest = SessionSnapshotV3::latest().unwrap();
        assert_eq!(latest.id, second_id);
    }
}
