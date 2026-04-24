//! Session state persistence for resume support.

use std::path::{Path, PathBuf};

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

    /// Returns the file path for this session's state file.
    #[must_use]
    pub fn state_path(&self) -> PathBuf {
        Self::state_dir().join(format!("{}.toml", self.id))
    }

    /// Saves the session state to disk atomically (write tmp + rename).
    ///
    /// # Errors
    ///
    /// Returns an error if the state directory cannot be created or the file
    /// cannot be written.
    pub fn save(&self) -> std::io::Result<()> {
        let dir = Self::state_dir();
        std::fs::create_dir_all(&dir)?;

        let path = self.state_path();
        let tmp_path = path.with_extension("toml.tmp");

        let toml_str = toml::to_string(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        std::fs::write(&tmp_path, toml_str)?;

        // Set restrictive permissions on Unix (credentials are encrypted but still)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&tmp_path, perms)?;
        }

        std::fs::rename(&tmp_path, &path)?;
        Ok(())
    }

    /// Loads a session state from a file path.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or parsed.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        toml::from_str(&contents)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Finds the most recent incomplete session in the state directory.
    ///
    /// All completed sessions and all but the newest incomplete session are
    /// removed so they never accumulate on disk.
    #[must_use]
    pub fn latest() -> Option<Self> {
        let dir = Self::state_dir();
        let read_dir = std::fs::read_dir(&dir).ok()?;

        let mut incomplete: Vec<(PathBuf, Self)> = Vec::new();

        for entry in read_dir.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "toml") {
                continue;
            }
            let Ok(session) = Self::load(&path) else {
                let corrupt_path = path.with_extension("toml.corrupt");
                log::warn!(
                    "Session file failed to parse, renaming to {}: {}",
                    corrupt_path.display(),
                    path.display()
                );
                let _ = std::fs::rename(&path, &corrupt_path);
                continue;
            };
            if session.status == SessionStatus::Completed {
                // Completed sessions are no longer needed — clean up.
                let _ = std::fs::remove_file(&path);
            } else {
                incomplete.push((path, session));
            }
        }

        incomplete.sort_by(|a, b| b.1.created.cmp(&a.1.created));

        // Clean up: remove all stale incomplete sessions except the newest
        for (path, _) in incomplete.iter().skip(1) {
            let _ = std::fs::remove_file(path);
        }

        incomplete.into_iter().next().map(|(_, s)| s)
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
            .filter(|f| {
                !matches!(
                    f.status,
                    FileEntryStatus::Completed | FileEntryStatus::Skipped
                )
            })
            .count()
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
