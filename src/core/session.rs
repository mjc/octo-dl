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
use crate::core::model::{
    DesiredState, FileId, FileLifecycle, FileProgressState, PackageId, RuntimeState,
    SessionRunStatus, UrlId,
};

type Aes128CbcDec = cbc::Decryptor<Aes128>;
#[cfg(test)]
type Aes128CbcEnc = cbc::Encryptor<Aes128>;

const CREDENTIAL_VERSION_PREFIX: &str = "v2:";
pub(crate) static LEGACY_WARNING: std::sync::OnceLock<()> = std::sync::OnceLock::new();

#[cfg(test)]
pub(crate) static STATE_DIRECTORY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
pub struct FileSnapshot {
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSnapshotV3 {
    pub version: u32,
    pub id: String,
    pub created: DateTime<Utc>,
    pub status: SessionRunStatus,
    pub packages: Vec<PackageSnapshot>,
    pub files: Vec<FileSnapshot>,
    pub config: DownloadConfig,
    pub credentials: SavedCredentials,
}

impl SessionSnapshotV3 {
    #[must_use]
    pub fn new(config: DownloadConfig, credentials: SavedCredentials) -> Self {
        Self {
            version: 3,
            id: uuid::Uuid::new_v4().to_string(),
            created: Utc::now(),
            status: SessionRunStatus::InProgress,
            packages: Vec::new(),
            files: Vec::new(),
            config,
            credentials,
        }
    }

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

    #[must_use]
    pub fn state_path(&self) -> PathBuf {
        Self::state_dir().join(format!("session-v3-{}.toml", self.id))
    }

    pub fn save(&self) -> std::io::Result<()> {
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
        if snapshot.version != 3 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "session is not version 3",
            ));
        }
        Ok(snapshot)
    }

    pub fn latest() -> Option<Self> {
        Self::latest_with_backups().0
    }

    #[must_use]
    pub fn latest_with_backups() -> (Option<Self>, Vec<String>) {
        let dir = Self::state_dir();
        let read_dir = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => return (None, Vec::new()),
        };

        let mut backups = Vec::new();
        let mut v3_sessions = Vec::new();
        for entry in read_dir.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "toml") {
                continue;
            }
            let contents = match std::fs::read_to_string(&path) {
                Ok(contents) => contents,
                Err(_) => continue,
            };
            let raw: toml::Value = match toml::from_str(&contents) {
                Ok(value) => value,
                Err(_) => {
                    let corrupt = path.with_extension("toml.corrupt");
                    let _ = std::fs::rename(&path, &corrupt);
                    continue;
                }
            };
            if raw
                .get("version")
                .and_then(toml::Value::as_integer)
                .is_some_and(|version| version == 3)
            {
                if let Ok(snapshot) = toml::from_str::<Self>(&contents) {
                    v3_sessions.push((path, snapshot));
                }
                continue;
            }

            let backup = path.with_extension("toml.legacy.bak");
            if std::fs::rename(&path, &backup).is_ok() {
                backups.push(backup.display().to_string());
                if LEGACY_WARNING.get().is_none() {
                    let _ = LEGACY_WARNING.set(());
                    log::warn!("Discarded legacy octo-dl session format during 0.x upgrade");
                }
            }
        }

        v3_sessions.sort_by(|a, b| b.1.created.cmp(&a.1.created));
        for (path, snapshot) in v3_sessions.iter() {
            if snapshot.status == SessionRunStatus::Completed {
                let _ = std::fs::remove_file(path);
            }
        }
        for (path, _) in v3_sessions.iter().skip(1) {
            let _ = std::fs::remove_file(path);
        }

        (
            v3_sessions.into_iter().next().map(|(_, session)| session),
            backups,
        )
    }

    fn matches_file_identity(file: &FileSnapshot, id_or_path: &str) -> bool {
        file.id == id_or_path || file.path == id_or_path
    }

    pub fn mark_file_complete(&mut self, id_or_path: &str) -> std::io::Result<()> {
        if let Some(file) = self
            .files
            .iter_mut()
            .find(|file| Self::matches_file_identity(file, id_or_path))
        {
            file.lifecycle = FileLifecycle::Complete;
            file.progress.visible_completed_bytes = file.size;
            file.runtime.active = false;
            file.runtime.counts_in_run_totals = false;
        }
        self.save()
    }

    pub fn mark_file_error(&mut self, id_or_path: &str, error: &str) -> std::io::Result<()> {
        if let Some(file) = self
            .files
            .iter_mut()
            .find(|file| Self::matches_file_identity(file, id_or_path))
        {
            file.lifecycle = FileLifecycle::Failed;
            file.message = Some(error.to_string());
            file.runtime.active = false;
        }
        self.save()
    }

    pub fn mark_file_skipped(&mut self, id_or_path: &str) -> std::io::Result<()> {
        if let Some(file) = self
            .files
            .iter_mut()
            .find(|file| Self::matches_file_identity(file, id_or_path))
        {
            file.lifecycle = FileLifecycle::Skipped;
            file.desired = DesiredState::Suppressed;
            file.runtime.active = false;
            file.runtime.counts_in_run_totals = false;
        }
        self.save()
    }

    pub fn remove_file(&mut self, id_or_path: &str) -> std::io::Result<()> {
        self.files
            .retain(|file| !Self::matches_file_identity(file, id_or_path));
        for package in &mut self.packages {
            package.file_ids.retain(|file_id| file_id != id_or_path);
        }
        self.save()
    }

    pub fn mark_completed(&mut self) -> std::io::Result<()> {
        self.status = SessionRunStatus::Completed;
        self.save()
    }

    pub fn mark_paused(&mut self) -> std::io::Result<()> {
        self.status = SessionRunStatus::Paused;
        self.save()
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
    let pad_byte = *encrypted.last()? as usize;
    if pad_byte == 0 || pad_byte > 16 {
        return None;
    }
    let unpadded_len = encrypted.len().checked_sub(pad_byte)?;
    if !encrypted[unpadded_len..]
        .iter()
        .all(|&byte| byte as usize == pad_byte)
    {
        return None;
    }
    String::from_utf8(encrypted[..unpadded_len].to_vec()).ok()
}

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
    use std::env;

    struct StateDirectoryGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        previous: Option<std::ffi::OsString>,
    }

    impl StateDirectoryGuard {
        fn set(path: &Path) -> Self {
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
    fn credential_round_trip() {
        let saved = SavedCredentials::encrypt("test@example.com", "hunter2", Some("123456"));
        let (email, password, mfa) = saved.decrypt().unwrap();
        assert_eq!(email, "test@example.com");
        assert_eq!(password, "hunter2");
        assert_eq!(mfa.as_deref(), Some("123456"));
    }

    #[test]
    fn legacy_credential_decryption_still_works() {
        let encrypted = encrypt_credential_legacy_for_test("old-secret");
        assert_eq!(decrypt_credential(&encrypted).unwrap(), "old-secret");
    }

    #[test]
    fn latest_backs_up_legacy_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = StateDirectoryGuard::set(dir.path());
        let legacy_path = SessionSnapshotV3::state_dir().join("legacy.toml");
        std::fs::create_dir_all(SessionSnapshotV3::state_dir()).unwrap();
        std::fs::write(&legacy_path, "id = 'legacy'\n").unwrap();
        let (_session, backups) = SessionSnapshotV3::latest_with_backups();
        assert_eq!(backups.len(), 1);
        assert!(backups[0].ends_with(".legacy.bak"));
    }

    #[test]
    fn latest_prefers_newest_v3_session() {
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
