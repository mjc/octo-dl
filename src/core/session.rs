#[cfg(test)]
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::DownloadConfig;
use crate::core::model::{
    FileAccounting, FileId, FileLifecycle, FileProgressState, PackageId, PackageKey,
    SessionRunStatus, UrlId,
};

const CREDENTIAL_VERSION_PREFIX: &str = "v2:";
const SESSION_VERSION: u32 = 6;
const SESSION_FILE_PREFIX: &str = "session-v6-";
const SESSION_POSTCARD_EXTENSION: &str = "postcard";
const SESSION_TOML_EXTENSION: &str = "toml";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionEncoding {
    Postcard,
    Toml,
}

fn session_encoding_for_path(path: &Path) -> SessionEncoding {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some(SESSION_POSTCARD_EXTENSION) => SessionEncoding::Postcard,
        _ => SessionEncoding::Toml,
    }
}

fn is_canonical_session_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some(SESSION_POSTCARD_EXTENSION | SESSION_TOML_EXTENSION)
    ) && path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(SESSION_FILE_PREFIX))
}

fn session_path_preference(path: &Path) -> u8 {
    match session_encoding_for_path(path) {
        SessionEncoding::Postcard => 1,
        SessionEncoding::Toml => 0,
    }
}

fn should_replace_session_candidate(
    candidate_path: &Path,
    candidate_modified: Option<SystemTime>,
    existing_path: &Path,
    existing_modified: Option<SystemTime>,
) -> bool {
    let candidate_preference = session_path_preference(candidate_path);
    let existing_preference = session_path_preference(existing_path);
    if candidate_preference != existing_preference {
        return candidate_preference > existing_preference;
    }

    matches!(
        (candidate_modified, existing_modified),
        (Some(candidate), Some(existing)) if candidate > existing
    )
}

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
pub struct SavedMegaSession {
    pub email: String,
    pub session: String,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PostcardSessionUrlSnapshot {
    url: UrlId,
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PostcardPackageSnapshot {
    id: PackageId,
    key: PackageKey,
    display_name: String,
    files: Vec<FileSnapshot>,
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PostcardSessionSnapshot {
    version: u32,
    id: String,
    created: DateTime<Utc>,
    status: SessionRunStatus,
    urls: Vec<PostcardSessionUrlSnapshot>,
    packages: Vec<PostcardPackageSnapshot>,
    config: DownloadConfig,
    credentials: SavedCredentials,
}

impl From<&SessionUrlSnapshot> for PostcardSessionUrlSnapshot {
    fn from(url: &SessionUrlSnapshot) -> Self {
        Self {
            url: url.url.clone(),
            error: url.error.clone(),
        }
    }
}

impl From<PostcardSessionUrlSnapshot> for SessionUrlSnapshot {
    fn from(url: PostcardSessionUrlSnapshot) -> Self {
        Self {
            url: url.url,
            error: url.error,
        }
    }
}

impl From<&PackageSnapshot> for PostcardPackageSnapshot {
    fn from(package: &PackageSnapshot) -> Self {
        Self {
            id: package.id,
            key: package.key.clone(),
            display_name: package.display_name.clone(),
            files: package.files.clone(),
            error: package.error.clone(),
        }
    }
}

impl From<PostcardPackageSnapshot> for PackageSnapshot {
    fn from(package: PostcardPackageSnapshot) -> Self {
        Self {
            id: package.id,
            key: package.key,
            display_name: package.display_name,
            files: package.files,
            error: package.error,
        }
    }
}

impl From<&SessionSnapshot> for PostcardSessionSnapshot {
    fn from(snapshot: &SessionSnapshot) -> Self {
        Self {
            version: snapshot.version,
            id: snapshot.id.clone(),
            created: snapshot.created,
            status: snapshot.status,
            urls: snapshot
                .urls
                .iter()
                .map(PostcardSessionUrlSnapshot::from)
                .collect(),
            packages: snapshot
                .packages
                .iter()
                .map(PostcardPackageSnapshot::from)
                .collect(),
            config: snapshot.config.clone(),
            credentials: snapshot.credentials.clone(),
        }
    }
}

impl From<PostcardSessionSnapshot> for SessionSnapshot {
    fn from(snapshot: PostcardSessionSnapshot) -> Self {
        Self {
            version: snapshot.version,
            id: snapshot.id,
            created: snapshot.created,
            status: snapshot.status,
            urls: snapshot
                .urls
                .into_iter()
                .map(SessionUrlSnapshot::from)
                .collect(),
            packages: snapshot
                .packages
                .into_iter()
                .map(PackageSnapshot::from)
                .collect(),
            config: snapshot.config,
            credentials: snapshot.credentials,
        }
    }
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
        let tmp = path.with_extension(format!(
            "{}.tmp",
            path.extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or(SESSION_POSTCARD_EXTENSION)
        ));
        let bytes = match session_encoding_for_path(path) {
            SessionEncoding::Postcard => postcard::to_stdvec(&PostcardSessionSnapshot::from(self))
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?,
            SessionEncoding::Toml => toml::to_string(self)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?
                .into_bytes(),
        };
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
        let snapshot = match session_encoding_for_path(path) {
            SessionEncoding::Postcard => {
                let contents = std::fs::read(path)?;
                postcard::from_bytes::<PostcardSessionSnapshot>(&contents)
                    .map(SessionSnapshot::from)
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?
            }
            SessionEncoding::Toml => {
                let contents = std::fs::read_to_string(path)?;
                toml::from_str(&contents)
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?
            }
        };
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

fn derive_machine_key_from_parts(hostname: &str, username: &str) -> [u8; 16] {
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

fn derive_machine_key() -> [u8; 16] {
    static MACHINE_KEY: OnceLock<[u8; 16]> = OnceLock::new();
    if let Some(key) = MACHINE_KEY.get() {
        return *key;
    }

    match hostname::get() {
        Ok(hostname) => {
            let hostname = hostname.to_string_lossy().into_owned();
            let username = whoami::username();
            *MACHINE_KEY.get_or_init(|| derive_machine_key_from_parts(&hostname, &username))
        }
        Err(_) => derive_machine_key_from_parts("unknown-host", &whoami::username()),
    }
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

impl SavedMegaSession {
    #[must_use]
    pub fn encrypt(email: &str, session: &str) -> Self {
        Self {
            email: encrypt_credential(email),
            session: encrypt_credential(session),
        }
    }

    #[must_use]
    pub fn decrypt(&self) -> Option<(String, String)> {
        Some((
            decrypt_credential(&self.email)?,
            decrypt_credential(&self.session)?,
        ))
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
    fn mega_session_round_trip() {
        let saved = SavedMegaSession::encrypt("test@example.com", "serialized-session");
        let (email, session) = saved.decrypt().unwrap();
        assert_eq!(email, "test@example.com");
        assert_eq!(session, "serialized-session");
    }

    #[test]
    fn unversioned_credential_decryption_is_rejected() {
        assert!(decrypt_credential("old-secret").is_none());
    }

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

        std::thread::sleep(std::time::Duration::from_millis(10));
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
