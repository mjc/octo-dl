//! Session state persistence for resume support.

use std::path::{Path, PathBuf};

use crate::core::session::{FileSnapshot, PackageSnapshot, SessionSnapshotV3};
use aes::Aes128;
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
#[cfg(test)]
use cbc::cipher::BlockEncryptMut;
use cbc::cipher::{BlockDecryptMut, KeyIvInit};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::DownloadConfig;

type Aes128CbcDec = cbc::Decryptor<Aes128>;
#[cfg(test)]
type Aes128CbcEnc = cbc::Encryptor<Aes128>;

const CREDENTIAL_VERSION_PREFIX: &str = "v2:";

#[cfg(test)]
pub(crate) static STATE_DIRECTORY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Overall session status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionStatus {
    InProgress,
    Completed,
    Paused,
}

/// Status of a URL entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UrlStatus {
    Pending,
    Fetched,
    Error(String),
}

/// Status of a file entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileEntryStatus {
    Pending,
    Downloading,
    Completed,
    Skipped,
    Error(String),
}

impl FileEntryStatus {
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Skipped)
    }
}

/// Encrypted credentials stored in the session file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedCredentials {
    pub email: String,
    pub password: String,
    pub mfa: Option<String>,
}

/// A URL entry in the session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UrlEntry {
    pub url: String,
    pub status: UrlStatus,
}

/// A file entry in the session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub url_index: usize,
    pub path: String,
    pub size: u64,
    pub status: FileEntryStatus,
}

/// Builds a stable session/UI key for a file path from a specific source URL.
#[must_use]
pub fn file_key(url_index: usize, path: &str) -> String {
    format!("{url_index}:{path}")
}

impl FileEntry {
    /// Returns the stable key for this file, falling back to legacy path-only identity.
    #[must_use]
    pub fn key_or_path(&self) -> &str {
        self.key.as_deref().unwrap_or(&self.path)
    }

    fn matches_identity(&self, id_or_path: &str) -> bool {
        self.key.as_deref() == Some(id_or_path) || self.path == id_or_path
    }
}

/// Persistent session state for resume support.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    pub id: String,
    pub created: DateTime<Utc>,
    pub status: SessionStatus,
    pub credentials: SavedCredentials,
    pub config: DownloadConfig,
    pub urls: Vec<UrlEntry>,
    pub files: Vec<FileEntry>,
}

impl SessionState {
    /// Creates a new session state with the given parameters.
    #[must_use]
    pub fn new(credentials: SavedCredentials, config: DownloadConfig, urls: Vec<UrlEntry>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            created: Utc::now(),
            status: SessionStatus::InProgress,
            credentials,
            config,
            urls,
            files: Vec::new(),
        }
    }

    /// Returns the directory where session state files are stored.
    ///
    /// Uses `STATE_DIRECTORY` (set by systemd when `StateDirectory=` is configured),
    /// falling back to `$XDG_DATA_HOME/octo-dl` for interactive use.
    #[must_use]
    pub fn state_dir() -> PathBuf {
        SessionSnapshotV3::state_dir()
    }

    /// Returns the file path for this session's state file.
    #[must_use]
    pub fn state_path(&self) -> PathBuf {
        Self::state_dir().join(format!("session-v3-{}.toml", self.id))
    }

    /// Saves the session state to disk atomically (write tmp + rename).
    ///
    /// # Errors
    ///
    /// Returns an error if the state directory cannot be created or the file
    /// cannot be written.
    pub fn save(&self) -> std::io::Result<()> {
        self.to_v3().save()
    }

    /// Loads a session state from a file path.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or parsed.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        match SessionSnapshotV3::load(path) {
            Ok(snapshot) => Ok(Self::from_v3(snapshot)),
            Err(_) => {
                let contents = std::fs::read_to_string(path)?;
                toml::from_str(&contents)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
            }
        }
    }

    /// Finds the most recent incomplete session in the state directory.
    ///
    /// All completed sessions and all but the newest incomplete session are
    /// removed so they never accumulate on disk.
    #[must_use]
    pub fn latest() -> Option<Self> {
        SessionSnapshotV3::latest().map(Self::from_v3)
    }

    /// Marks a file as completed by its path and saves the state.
    ///
    /// # Errors
    ///
    /// Returns an error if the state file cannot be written.
    pub fn mark_file_complete(&mut self, path: &str) -> std::io::Result<()> {
        if let Some(entry) = self.files.iter_mut().find(|f| f.matches_identity(path)) {
            entry.status = FileEntryStatus::Completed;
        }
        self.save()
    }

    /// Marks a file as errored by its path and saves the state.
    ///
    /// # Errors
    ///
    /// Returns an error if the state file cannot be written.
    pub fn mark_file_error(&mut self, path: &str, error: &str) -> std::io::Result<()> {
        if let Some(entry) = self.files.iter_mut().find(|f| f.matches_identity(path)) {
            entry.status = FileEntryStatus::Error(error.to_string());
        }
        self.save()
    }

    /// Marks a file as skipped by the user and saves the state.
    ///
    /// # Errors
    ///
    /// Returns an error if the state file cannot be written.
    pub fn mark_file_skipped(&mut self, path: &str) -> std::io::Result<()> {
        if let Some(entry) = self.files.iter_mut().find(|f| f.matches_identity(path)) {
            entry.status = FileEntryStatus::Skipped;
        }
        self.save()
    }

    /// Removes a file entry by path and saves the state.
    ///
    /// # Errors
    ///
    /// Returns an error if the state file cannot be written.
    pub fn remove_file(&mut self, path: &str) -> std::io::Result<()> {
        self.files.retain(|f| !f.matches_identity(path));
        self.save()
    }

    /// Marks the session as completed and saves.
    ///
    /// # Errors
    ///
    /// Returns an error if the state file cannot be written.
    pub fn mark_completed(&mut self) -> std::io::Result<()> {
        self.status = SessionStatus::Completed;
        self.save()
    }

    /// Marks the session as paused and saves.
    ///
    /// # Errors
    ///
    /// Returns an error if the state file cannot be written.
    pub fn mark_paused(&mut self) -> std::io::Result<()> {
        self.status = SessionStatus::Paused;
        self.save()
    }

    /// Returns true when a URL should be resumed on startup.
    ///
    /// Pending URLs always resume. Fetched URLs resume only when they either
    /// have no file-level bookkeeping yet or still have non-terminal files.
    #[must_use]
    pub fn url_should_resume(&self, url_index: usize) -> bool {
        let Some(entry) = self.urls.get(url_index) else {
            return false;
        };

        match entry.status {
            UrlStatus::Pending => true,
            UrlStatus::Fetched => {
                let mut saw_file = false;
                for file in &self.files {
                    if file.url_index != url_index {
                        continue;
                    }
                    saw_file = true;
                    if !file.status.is_terminal() {
                        return true;
                    }
                }
                !saw_file
            }
            UrlStatus::Error(_) => false,
        }
    }

    /// Returns the number of completed files.
    #[must_use]
    pub fn completed_count(&self) -> usize {
        self.files
            .iter()
            .filter(|f| f.status == FileEntryStatus::Completed)
            .count()
    }

    /// Returns the number of pending or errored files that need downloading.
    #[must_use]
    pub fn remaining_count(&self) -> usize {
        self.files
            .iter()
            .filter(|f| !f.status.is_terminal())
            .count()
    }

    pub(crate) fn to_v3(&self) -> SessionSnapshotV3 {
        let mut packages = Vec::with_capacity(self.urls.len());
        for (url_index, url_entry) in self.urls.iter().enumerate() {
            let file_ids = self
                .files
                .iter()
                .filter(|file| file.url_index == url_index)
                .map(|file| file.path.clone())
                .collect();
            packages.push(PackageSnapshot {
                id: url_entry.url.clone(),
                source_url: url_entry.url.clone(),
                display_name: url_entry.url.clone(),
                file_ids,
                error: match &url_entry.status {
                    UrlStatus::Error(message) => Some(message.clone()),
                    UrlStatus::Pending | UrlStatus::Fetched => None,
                },
            });
        }

        let files = self
            .files
            .iter()
            .filter_map(|file| {
                let package_id = self.urls.get(file.url_index)?.url.clone();
                Some(FileSnapshot {
                    id: file.path.clone(),
                    package_id,
                    path: file.path.clone(),
                    size: file.size,
                    lifecycle: match &file.status {
                        FileEntryStatus::Pending => crate::core::FileLifecycle::Queued,
                        FileEntryStatus::Downloading => crate::core::FileLifecycle::Downloading,
                        FileEntryStatus::Completed => crate::core::FileLifecycle::Complete,
                        FileEntryStatus::Skipped => crate::core::FileLifecycle::Skipped,
                        FileEntryStatus::Error(message) => {
                            let _ = message;
                            crate::core::FileLifecycle::Failed
                        }
                    },
                    progress: crate::core::FileProgressState {
                        verified_existing_bytes: 0,
                        downloaded_network_bytes: 0,
                        visible_completed_bytes: if matches!(file.status, FileEntryStatus::Completed)
                        {
                            file.size
                        } else {
                            0
                        },
                    },
                    desired: if matches!(file.status, FileEntryStatus::Skipped) {
                        crate::core::DesiredState::Suppressed
                    } else {
                        crate::core::DesiredState::Present
                    },
                    runtime: crate::core::RuntimeState {
                        counts_in_run_totals: !matches!(
                            file.status,
                            FileEntryStatus::Completed | FileEntryStatus::Skipped
                        ),
                        active: matches!(file.status, FileEntryStatus::Downloading),
                        preexisting_complete: false,
                        reused_chunks: 0,
                    },
                    message: match &file.status {
                        FileEntryStatus::Error(message) => Some(message.clone()),
                        _ => None,
                    },
                })
            })
            .collect();

        SessionSnapshotV3 {
            version: 3,
            id: self.id.clone(),
            created: self.created,
            status: match self.status {
                SessionStatus::InProgress => crate::core::SessionRunStatus::InProgress,
                SessionStatus::Completed => crate::core::SessionRunStatus::Completed,
                SessionStatus::Paused => crate::core::SessionRunStatus::Paused,
            },
            packages,
            files,
            config: self.config.clone(),
            credentials: crate::core::SavedCredentials {
                email: self.credentials.email.clone(),
                password: self.credentials.password.clone(),
                mfa: self.credentials.mfa.clone(),
            },
        }
    }

    pub(crate) fn from_v3(snapshot: SessionSnapshotV3) -> Self {
        let urls: Vec<UrlEntry> = snapshot
            .packages
            .iter()
            .map(|package| UrlEntry {
                url: package.source_url.clone(),
                status: package.error.as_ref().map_or_else(
                    || {
                        if package.file_ids.is_empty() {
                            UrlStatus::Pending
                        } else {
                            UrlStatus::Fetched
                        }
                    },
                    |message| UrlStatus::Error(message.clone()),
                ),
            })
            .collect();
        let files = snapshot
            .files
            .into_iter()
            .map(|file| {
                let url_index = snapshot
                    .packages
                    .iter()
                    .position(|package| package.id == file.package_id)
                    .unwrap_or(0);
                FileEntry {
                    key: Some(file.path.clone()),
                    url_index,
                    path: file.path.clone(),
                    size: file.size,
                    status: match file.lifecycle {
                        crate::core::FileLifecycle::Planned
                        | crate::core::FileLifecycle::Queued => FileEntryStatus::Pending,
                        crate::core::FileLifecycle::Downloading => FileEntryStatus::Downloading,
                        crate::core::FileLifecycle::Complete => FileEntryStatus::Completed,
                        crate::core::FileLifecycle::Skipped
                        | crate::core::FileLifecycle::Deleted => FileEntryStatus::Skipped,
                        crate::core::FileLifecycle::Failed => {
                            FileEntryStatus::Error(file.message.unwrap_or_else(|| "failed".to_string()))
                        }
                    },
                }
            })
            .collect();

        Self {
            id: snapshot.id,
            created: snapshot.created,
            status: match snapshot.status {
                crate::core::SessionRunStatus::InProgress => SessionStatus::InProgress,
                crate::core::SessionRunStatus::Completed => SessionStatus::Completed,
                crate::core::SessionRunStatus::Paused => SessionStatus::Paused,
            },
            credentials: SavedCredentials {
                email: snapshot.credentials.email,
                password: snapshot.credentials.password,
                mfa: snapshot.credentials.mfa,
            },
            config: snapshot.config,
            urls,
            files,
        }
    }
}

// ============================================================================
// Credential encryption
// ============================================================================

/// Derives a 16-byte encryption key from a machine-specific seed.
///
/// Uses hostname + username as seed material, hashed with SHA-256,
/// then truncated to 16 bytes for AES-128.
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

/// Encrypts a plaintext string using AES-128-GCM with the machine key.
/// Returns the encrypted data as a base64-encoded string.
///
/// The encoded value is versioned as `v2:<base64(nonce || ciphertext_and_tag)>`.
/// Session/config files are also saved with 0o600 permissions as defense in depth.
///
/// # Panics
///
/// Panics if authenticated encryption fails (should never happen for AES-GCM).
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

/// Decrypts legacy AES-128-CBC credentials written by older octo-dl versions.
fn decrypt_credential_legacy(encrypted: &str) -> Option<String> {
    let key = derive_machine_key();
    let iv = key;

    let mut data = BASE64.decode(encrypted).ok()?;
    if data.is_empty() || data.len() % 16 != 0 {
        return None;
    }

    let cipher = Aes128CbcDec::new(&key.into(), &iv.into());
    let encrypted = cipher
        .decrypt_padded_mut::<cbc::cipher::block_padding::NoPadding>(&mut data)
        .ok()?;

    // Remove PKCS7 padding
    let pad_byte = *encrypted.last()? as usize;
    if pad_byte == 0 || pad_byte > 16 {
        return None;
    }
    let unpadded_len = encrypted.len().checked_sub(pad_byte)?;
    // Verify padding bytes
    if !encrypted[unpadded_len..]
        .iter()
        .all(|&b| b as usize == pad_byte)
    {
        return None;
    }

    String::from_utf8(encrypted[..unpadded_len].to_vec()).ok()
}

/// Decrypts a versioned AES-128-GCM credential, falling back to legacy CBC.
///
/// # Errors
///
/// Returns `None` if decryption or decoding fails.
#[must_use]
pub fn decrypt_credential(encrypted: &str) -> Option<String> {
    if encrypted.starts_with(CREDENTIAL_VERSION_PREFIX) {
        return decrypt_credential_v2(encrypted);
    }
    decrypt_credential_legacy(encrypted)
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)]
fn encrypt_credential_legacy_for_test(plaintext: &str) -> String {
    let key = derive_machine_key();
    let iv = key;

    let plaintext_bytes = plaintext.as_bytes();
    let padded_len = ((plaintext_bytes.len() / 16) + 1) * 16;
    let mut buf = vec![0u8; padded_len];
    buf[..plaintext_bytes.len()].copy_from_slice(plaintext_bytes);

    let pad_byte = (padded_len - plaintext_bytes.len()) as u8;
    buf[plaintext_bytes.len()..].fill(pad_byte);

    let cipher = Aes128CbcEnc::new(&key.into(), &iv.into());
    let encrypted = cipher
        .encrypt_padded_mut::<cbc::cipher::block_padding::NoPadding>(&mut buf, padded_len)
        .expect("buffer size is correct");

    BASE64.encode(encrypted)
}

impl SavedCredentials {
    /// Creates encrypted credentials from plaintext values.
    #[must_use]
    pub fn encrypt(email: &str, password: &str, mfa: Option<&str>) -> Self {
        Self {
            email: encrypt_credential(email),
            password: encrypt_credential(password),
            mfa: mfa.map(encrypt_credential),
        }
    }

    /// Decrypts the stored credentials.
    /// Returns `(email, password, mfa)` or `None` if decryption fails.
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
    use std::env;

    struct StateDirectoryGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        previous: Option<std::ffi::OsString>,
    }

    impl StateDirectoryGuard {
        fn unset() -> Self {
            let lock = STATE_DIRECTORY_TEST_LOCK.lock().unwrap();
            let previous = env::var_os("STATE_DIRECTORY");
            unsafe { env::remove_var("STATE_DIRECTORY") };
            Self {
                _lock: lock,
                previous,
            }
        }

        fn set(path: &str) -> Self {
            let lock = STATE_DIRECTORY_TEST_LOCK.lock().unwrap();
            let previous = env::var_os("STATE_DIRECTORY");
            unsafe { env::set_var("STATE_DIRECTORY", path) };
            Self {
                _lock: lock,
                previous,
            }
        }
    }

    impl Drop for StateDirectoryGuard {
        fn drop(&mut self) {
            if let Some(ref value) = self.previous {
                unsafe { env::set_var("STATE_DIRECTORY", value) };
            } else {
                unsafe { env::remove_var("STATE_DIRECTORY") };
            }
        }
    }

    #[test]
    fn credential_encryption_round_trip() {
        let email = "test@example.com";
        let password = "s3cret!";
        let mfa = Some("123456");

        let saved = SavedCredentials::encrypt(email, password, mfa);
        assert!(saved.email.starts_with(CREDENTIAL_VERSION_PREFIX));
        assert!(saved.password.starts_with(CREDENTIAL_VERSION_PREFIX));
        // Encrypted values should not be plaintext
        assert_ne!(saved.email, email);
        assert_ne!(saved.password, password);

        let (dec_email, dec_password, dec_mfa) = saved.decrypt().unwrap();
        assert_eq!(dec_email, email);
        assert_eq!(dec_password, password);
        assert_eq!(dec_mfa.as_deref(), mfa);
    }

    #[test]
    fn credential_encryption_no_mfa() {
        let saved = SavedCredentials::encrypt("user@test.com", "pass", None);
        let (email, password, mfa) = saved.decrypt().unwrap();
        assert_eq!(email, "user@test.com");
        assert_eq!(password, "pass");
        assert!(mfa.is_none());
    }

    #[test]
    fn encrypt_decrypt_empty_string() {
        let encrypted = encrypt_credential("");
        let decrypted = decrypt_credential(&encrypted).unwrap();
        assert_eq!(decrypted, "");
    }

    #[test]
    fn credential_decryption_rejects_tampering() {
        let mut encrypted = encrypt_credential("secret");
        encrypted.push('A');
        assert!(decrypt_credential(&encrypted).is_none());
    }

    #[test]
    fn legacy_credential_decryption_still_works() {
        let encrypted = encrypt_credential_legacy_for_test("old-secret");
        assert!(!encrypted.starts_with(CREDENTIAL_VERSION_PREFIX));
        assert_eq!(decrypt_credential(&encrypted).unwrap(), "old-secret");
    }

    #[test]
    fn encrypt_decrypt_long_string() {
        let long = "a".repeat(1000);
        let encrypted = encrypt_credential(&long);
        let decrypted = decrypt_credential(&encrypted).unwrap();
        assert_eq!(decrypted, long);
    }

    #[test]
    fn decrypt_invalid_base64_returns_none() {
        assert!(decrypt_credential("not-valid-base64!!!").is_none());
    }

    #[test]
    fn decrypt_wrong_data_returns_none() {
        // Valid base64 but not valid AES-CBC encrypted data
        assert!(decrypt_credential("AAAAAAAAAAAAAAAAAAAAAA==").is_none());
    }

    #[test]
    fn session_state_round_trip() {
        let state = SessionState::new(
            SavedCredentials::encrypt("test@test.com", "password123", None),
            DownloadConfig::default(),
            vec![UrlEntry {
                url: "https://mega.nz/folder/test".to_string(),
                status: UrlStatus::Fetched,
            }],
        );

        let toml_str = toml::to_string(&state).unwrap();
        let loaded: SessionState = toml::from_str(&toml_str).unwrap();

        assert_eq!(loaded.id, state.id);
        assert_eq!(loaded.status, state.status);
        assert_eq!(loaded.urls.len(), 1);
        assert_eq!(loaded.urls[0].url, "https://mega.nz/folder/test");
    }

    #[test]
    fn session_state_save_and_load() {
        let state = SessionState::new(
            SavedCredentials::encrypt("test@test.com", "pass", None),
            DownloadConfig::default(),
            vec![],
        );

        // Save to a temp location
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("test-session.toml");
        let toml_str = toml::to_string(&state).unwrap();
        std::fs::write(&path, toml_str).unwrap();

        let loaded = SessionState::load(&path).unwrap();
        assert_eq!(loaded.id, state.id);
        assert_eq!(loaded.status, SessionStatus::InProgress);
    }

    #[test]
    fn session_completed_and_remaining_counts() {
        let mut state = SessionState::new(
            SavedCredentials::encrypt("t@t.com", "p", None),
            DownloadConfig::default(),
            vec![],
        );

        state.files = vec![
            FileEntry {
                key: None,
                url_index: 0,
                path: "file1.txt".to_string(),
                size: 100,
                status: FileEntryStatus::Completed,
            },
            FileEntry {
                key: None,
                url_index: 0,
                path: "file2.txt".to_string(),
                size: 200,
                status: FileEntryStatus::Pending,
            },
            FileEntry {
                key: None,
                url_index: 0,
                path: "file3.txt".to_string(),
                size: 300,
                status: FileEntryStatus::Error("timeout".to_string()),
            },
            FileEntry {
                key: None,
                url_index: 0,
                path: "file4.txt".to_string(),
                size: 400,
                status: FileEntryStatus::Skipped,
            },
        ];

        assert_eq!(state.completed_count(), 1);
        assert_eq!(state.remaining_count(), 2);
    }

    #[test]
    fn url_should_resume_matrix() {
        struct Case {
            name: &'static str,
            url_status: UrlStatus,
            files: Vec<FileEntry>,
            expected: bool,
        }

        let cases = vec![
            Case {
                name: "pending url resumes without file bookkeeping",
                url_status: UrlStatus::Pending,
                files: vec![],
                expected: true,
            },
            Case {
                name: "fetched url resumes without file bookkeeping",
                url_status: UrlStatus::Fetched,
                files: vec![],
                expected: true,
            },
            Case {
                name: "fetched url with completed file does not resume",
                url_status: UrlStatus::Fetched,
                files: vec![FileEntry {
                    key: Some("0:done.bin".to_string()),
                    url_index: 0,
                    path: "done.bin".to_string(),
                    size: 10,
                    status: FileEntryStatus::Completed,
                }],
                expected: false,
            },
            Case {
                name: "fetched url with skipped file does not resume",
                url_status: UrlStatus::Fetched,
                files: vec![FileEntry {
                    key: Some("0:skip.bin".to_string()),
                    url_index: 0,
                    path: "skip.bin".to_string(),
                    size: 20,
                    status: FileEntryStatus::Skipped,
                }],
                expected: false,
            },
            Case {
                name: "fetched url with errored file resumes",
                url_status: UrlStatus::Fetched,
                files: vec![FileEntry {
                    key: Some("0:error.bin".to_string()),
                    url_index: 0,
                    path: "error.bin".to_string(),
                    size: 30,
                    status: FileEntryStatus::Error("boom".to_string()),
                }],
                expected: true,
            },
            Case {
                name: "fetched url with mixed terminal and pending files resumes",
                url_status: UrlStatus::Fetched,
                files: vec![
                    FileEntry {
                        key: Some("0:done.bin".to_string()),
                        url_index: 0,
                        path: "done.bin".to_string(),
                        size: 10,
                        status: FileEntryStatus::Completed,
                    },
                    FileEntry {
                        key: Some("0:skip.bin".to_string()),
                        url_index: 0,
                        path: "skip.bin".to_string(),
                        size: 20,
                        status: FileEntryStatus::Skipped,
                    },
                    FileEntry {
                        key: Some("0:todo.bin".to_string()),
                        url_index: 0,
                        path: "todo.bin".to_string(),
                        size: 30,
                        status: FileEntryStatus::Pending,
                    },
                ],
                expected: true,
            },
            Case {
                name: "errored url does not resume",
                url_status: UrlStatus::Error("fetch failed".to_string()),
                files: vec![],
                expected: false,
            },
        ];

        for case in cases {
            let mut state = SessionState::new(
                SavedCredentials::encrypt("t@t.com", "p", None),
                DownloadConfig::default(),
                vec![UrlEntry {
                    url: "https://mega.nz/file/test".to_string(),
                    status: case.url_status,
                }],
            );
            state.files = case.files;
            assert_eq!(state.url_should_resume(0), case.expected, "{}", case.name);
        }
    }

    #[test]
    fn state_dir_default_ends_in_sessions() {
        let _guard = StateDirectoryGuard::unset();
        let dir = SessionState::state_dir();
        assert!(dir.ends_with("sessions"));

        if dirs::data_dir().is_some() {
            assert!(dir.ends_with("octo-dl/sessions"));
        }
    }

    #[test]
    fn state_dir_uses_override() {
        let _guard = StateDirectoryGuard::set("/tmp/octo-state");
        let dir = SessionState::state_dir();
        assert_eq!(dir, PathBuf::from("/tmp/octo-state/sessions"));
    }
}
