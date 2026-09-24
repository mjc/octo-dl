use std::sync::OnceLock;
#[cfg(any(feature = "cli", feature = "tui", test))]
use std::{
    io,
    path::{Path, PathBuf},
};

use crate::config::CredentialKey;
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const CREDENTIAL_VERSION_PREFIX: &str = "v2:";
const RANDOM_CREDENTIAL_VERSION_PREFIX: &str = "v3:";
const SESSION_CREDENTIAL_VERSION_PREFIX: &str = "v4:";

impl CredentialKey {
    fn session_key_id(&self) -> String {
        format!("{:x}", Sha256::digest(self.as_bytes()))
    }

    #[cfg(any(feature = "cli", feature = "tui", test))]
    fn session_key_directory() -> PathBuf {
        super::SessionSnapshot::state_dir().join("credential-keys")
    }

    #[cfg(any(feature = "cli", feature = "tui", test))]
    fn load_session_key(directory: &Path, id: &str) -> io::Result<Self> {
        if id.len() != 64 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid session key identity",
            ));
        }
        let encoded = std::fs::read_to_string(directory.join(format!("{id}.key")))?;
        let key = Self::decode(encoded.trim()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid stored session key")
        })?;
        if key.session_key_id() != id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session key identity mismatch",
            ));
        }
        Ok(key)
    }

    /// Retain the original key independently of mutable service configs before
    /// publishing a session that refers to it. Keys are never rotated in place.
    #[cfg(any(feature = "cli", feature = "tui", test))]
    pub(crate) fn persist_for_sessions(&self) -> io::Result<()> {
        let directory = Self::session_key_directory();
        let missing = directory
            .ancestors()
            .take_while(|path| !path.exists())
            .collect::<Vec<_>>();
        std::fs::create_dir_all(&directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        }
        for path in missing.into_iter().rev() {
            if let Some(parent) = path.parent() {
                crate::config::sync_directory(parent)?;
            }
        }
        let id = self.session_key_id();
        match Self::load_session_key(&directory, &id) {
            Ok(_) => return crate::config::sync_directory(&directory),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let temporary = directory.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            crate::config::write_durable_temp_file(
                &temporary,
                self.encode().as_bytes(),
                "session credential key",
            )?;
            std::fs::rename(&temporary, directory.join(format!("{id}.key")))?;
            crate::config::sync_directory(&directory)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }
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

    hostname::get().map_or_else(
        |_| derive_machine_key_from_parts("unknown-host", &whoami::username()),
        |hostname| {
            let hostname = hostname.to_string_lossy().into_owned();
            let username = whoami::username();
            *MACHINE_KEY.get_or_init(|| derive_machine_key_from_parts(&hostname, &username))
        },
    )
}

/// Encrypts a credential using the machine-derived key.
///
/// # Panics
///
/// Panics only if the authenticated-encryption implementation rejects the
/// generated nonce and plaintext combination.
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

/// Creates a fresh, non-deterministic 128-bit key for a service config.
#[must_use]
pub fn generate_credential_key() -> String {
    BASE64.encode(uuid::Uuid::new_v4().as_bytes())
}

/// Decodes a persisted service-config credential key.
#[must_use]
pub fn decode_credential_key(encoded: &str) -> Option<[u8; 16]> {
    let bytes = BASE64.decode(encoded).ok()?;
    bytes.try_into().ok()
}

/// Encrypts a credential using an explicitly supplied key.
///
/// # Panics
///
/// Panics only if the authenticated-encryption implementation rejects the
/// generated nonce and plaintext combination.
#[must_use]
pub fn encrypt_credential_with_key(plaintext: &str, key: &CredentialKey) -> String {
    let cipher = Aes128Gcm::new(key.as_bytes().into());
    let nonce_uuid = uuid::Uuid::new_v4();
    let nonce_bytes = &nonce_uuid.as_bytes()[..12];
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(nonce_bytes), plaintext.as_bytes())
        .expect("AES-GCM encryption should succeed");
    let mut encoded = Vec::with_capacity(nonce_bytes.len() + ciphertext.len());
    encoded.extend_from_slice(nonce_bytes);
    encoded.extend_from_slice(&ciphertext);
    format!(
        "{RANDOM_CREDENTIAL_VERSION_PREFIX}{}",
        BASE64.encode(encoded)
    )
}

#[must_use]
pub fn decrypt_credential_with_key(encrypted: &str, key: &CredentialKey) -> Option<String> {
    let encoded = encrypted.strip_prefix(RANDOM_CREDENTIAL_VERSION_PREFIX)?;
    let data = BASE64.decode(encoded).ok()?;
    if data.len() < 13 {
        return None;
    }
    let (nonce_bytes, ciphertext) = data.split_at(12);
    let cipher = Aes128Gcm::new(key.as_bytes().into());
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
    pub fn encrypt_with_key(
        email: &str,
        password: &str,
        mfa: Option<&str>,
        key: &CredentialKey,
    ) -> Self {
        Self {
            // Keep the existing postcard field layout. Only the encrypted
            // email string gains an envelope containing a non-secret key ID.
            email: format!(
                "{SESSION_CREDENTIAL_VERSION_PREFIX}{}:{}",
                key.session_key_id(),
                encrypt_credential_with_key(email, key)
            ),
            password: encrypt_credential_with_key(password, key),
            mfa: mfa.map(|value| encrypt_credential_with_key(value, key)),
        }
    }

    #[must_use]
    pub fn decrypt_with_key(
        &self,
        key: &CredentialKey,
    ) -> Option<(String, String, Option<String>)> {
        let email =
            if let Some(envelope) = self.email.strip_prefix(SESSION_CREDENTIAL_VERSION_PREFIX) {
                let (id, encrypted) = envelope.split_once(':')?;
                if id != key.session_key_id() {
                    return None;
                }
                encrypted
            } else {
                &self.email
            };
        let email = decrypt_credential_with_key(email, key)?;
        let password = decrypt_credential_with_key(&self.password, key)?;
        let mfa = match self.mfa.as_deref() {
            Some(value) => Some(decrypt_credential_with_key(value, key)?),
            None => None,
        };
        Some((email, password, mfa))
    }

    /// Resolve identified snapshots without consulting the current config.
    /// Unassociated v3 snapshots can use an archived key or their original
    /// config once; the next save adds an explicit identity. v2 remains readable.
    #[cfg(any(feature = "cli", feature = "tui", test))]
    pub(crate) fn resume_key(
        &self,
        legacy_config_key: impl FnOnce() -> io::Result<CredentialKey>,
    ) -> io::Result<CredentialKey> {
        let directory = CredentialKey::session_key_directory();
        let key = if let Some(envelope) = self.email.strip_prefix(SESSION_CREDENTIAL_VERSION_PREFIX)
        {
            let (id, _) = envelope.split_once(':').ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid session credential envelope",
                )
            })?;
            match CredentialKey::load_session_key(&directory, id) {
                Ok(key) => key,
                // Recover a missing archive only from the exact original key;
                // decrypt_with_key below verifies its identity as well as GCM.
                Err(error) if error.kind() == io::ErrorKind::NotFound => legacy_config_key()?,
                Err(error) => return Err(error),
            }
        } else if self.decrypt_legacy().is_some() {
            CredentialKey::generate()
        } else {
            let archived = std::fs::read_dir(&directory).ok().and_then(|entries| {
                entries.filter_map(Result::ok).find_map(|entry| {
                    let path = entry.path();
                    if path.extension()?.to_str()? != "key" {
                        return None;
                    }
                    let id = path.file_stem()?.to_str()?;
                    let key = CredentialKey::load_session_key(&directory, id).ok()?;
                    self.decrypt_with_key(&key).map(|_| key)
                })
            });
            match archived {
                Some(key) => key,
                None => legacy_config_key()?,
            }
        };
        if self
            .decrypt_with_key(&key)
            .or_else(|| self.decrypt_legacy())
            .is_none()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "failed to decrypt session credentials",
            ));
        }
        key.persist_for_sessions()?;
        Ok(key)
    }

    /// Decrypts credentials from the legacy session snapshot format.
    #[must_use]
    pub fn decrypt_legacy(&self) -> Option<(String, String, Option<String>)> {
        let email = decrypt_credential(&self.email)?;
        let password = decrypt_credential(&self.password)?;
        let mfa = self.mfa.as_deref().and_then(decrypt_credential);
        Some((email, password, mfa))
    }
}

impl SavedMegaSession {
    #[must_use]
    pub fn encrypt(email: &str, session: &str, key: &CredentialKey) -> Self {
        Self {
            email: encrypt_credential_with_key(email, key),
            session: encrypt_credential_with_key(session, key),
        }
    }

    #[must_use]
    pub fn decrypt_with_key(&self, key: &CredentialKey) -> Option<(String, String)> {
        Some((
            decrypt_credential_with_key(&self.email, key)?,
            decrypt_credential_with_key(&self.session, key)?,
        ))
    }

    /// Decrypts v2 data for the one-time config migration only.
    #[must_use]
    pub fn decrypt(&self) -> Option<(String, String)> {
        Some((
            decrypt_credential(&self.email)?,
            decrypt_credential(&self.session)?,
        ))
    }

    /// Re-encrypts a legacy v2 session with the service config's random key.
    #[must_use]
    pub fn reencrypt_legacy_with_key(&self, key: &CredentialKey) -> Option<Self> {
        let (email, session) = self.decrypt()?;
        Some(Self::encrypt(&email, &session, key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CredentialKey;

    #[test]
    fn credential_round_trip() {
        let key = CredentialKey::generate();
        let saved =
            SavedCredentials::encrypt_with_key("test@example.com", "hunter2", Some("123456"), &key);
        assert!(saved.email.starts_with(SESSION_CREDENTIAL_VERSION_PREFIX));
        let (email, password, mfa) = saved.decrypt_with_key(&key).unwrap();
        assert_eq!(email, "test@example.com");
        assert_eq!(password, "hunter2");
        assert_eq!(mfa.as_deref(), Some("123456"));
        assert!(saved.decrypt_with_key(&CredentialKey::generate()).is_none());
    }

    #[test]
    fn mega_session_round_trip() {
        let key = CredentialKey::generate();
        let saved = SavedMegaSession::encrypt("test@example.com", "serialized-session", &key);
        assert!(saved.email.starts_with(RANDOM_CREDENTIAL_VERSION_PREFIX));
        let (email, session) = saved.decrypt_with_key(&key).unwrap();
        assert_eq!(email, "test@example.com");
        assert_eq!(session, "serialized-session");
        assert!(saved.decrypt_with_key(&CredentialKey::generate()).is_none());
    }

    #[test]
    fn legacy_mega_session_migrates_to_the_explicit_key() {
        let legacy = SavedMegaSession {
            email: encrypt_credential("test@example.com"),
            session: encrypt_credential("serialized-session"),
        };
        let key = CredentialKey::generate();

        let migrated = legacy.reencrypt_legacy_with_key(&key).unwrap();

        assert!(migrated.email.starts_with(RANDOM_CREDENTIAL_VERSION_PREFIX));
        assert!(
            migrated
                .session
                .starts_with(RANDOM_CREDENTIAL_VERSION_PREFIX)
        );
        assert_eq!(
            migrated.decrypt_with_key(&key),
            Some((
                "test@example.com".to_string(),
                "serialized-session".to_string()
            ))
        );
        assert!(legacy.reencrypt_legacy_with_key(&key).is_some());
    }

    #[test]
    fn unversioned_credential_decryption_is_rejected() {
        assert!(decrypt_credential("old-secret").is_none());
    }

    #[test]
    fn identified_session_uses_durable_original_key_without_config_lookup() {
        let directory = tempfile::tempdir().unwrap();
        let _state = crate::test_support::StateDirectoryGuard::set(directory.path());
        let key = CredentialKey::generate();
        key.persist_for_sessions().unwrap();
        key.persist_for_sessions().unwrap();
        let saved = SavedCredentials::encrypt_with_key("user", "secret", Some("123456"), &key);
        let resolved = saved
            .resume_key(|| panic!("current config must not be consulted"))
            .unwrap();
        assert_eq!(resolved, key);
        assert_eq!(
            saved.decrypt_with_key(&resolved),
            Some(("user".into(), "secret".into(), Some("123456".into())))
        );
        assert!(!saved.email.contains(&key.encode()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = CredentialKey::session_key_directory()
                .join(format!("{}.key", key.session_key_id()));
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(CredentialKey::session_key_directory())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn missing_or_corrupt_archive_never_substitutes_a_different_key() {
        let directory = tempfile::tempdir().unwrap();
        let _state = crate::test_support::StateDirectoryGuard::set(directory.path());
        let key = CredentialKey::generate();
        let other = CredentialKey::generate();
        let saved = SavedCredentials::encrypt_with_key("user", "secret", None, &key);
        assert!(saved.resume_key(|| Ok(other)).is_err());
        assert!(!CredentialKey::session_key_directory().exists());
        // The original config can recover a missing archive, but only by ID.
        assert_eq!(saved.resume_key(|| Ok(key)).unwrap(), key);
        let path =
            CredentialKey::session_key_directory().join(format!("{}.key", key.session_key_id()));
        std::fs::write(&path, other.encode()).unwrap();
        assert!(
            saved
                .resume_key(|| panic!("corruption must be reported"))
                .is_err()
        );
        assert!(key.persist_for_sessions().is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), other.encode());
    }

    #[test]
    fn old_postcard_credentials_decode_and_migrate_without_changing_field_layout() {
        let directory = tempfile::tempdir().unwrap();
        let _state = crate::test_support::StateDirectoryGuard::set(directory.path());
        let key = CredentialKey::generate();
        // Historical postcard layout: two strings and Option<String>, no key ID field.
        let historical = (
            encrypt_credential_with_key("old-user", &key),
            encrypt_credential_with_key("old-password", &key),
            Some(encrypt_credential_with_key("old-mfa", &key)),
        );
        let bytes = postcard::to_stdvec(&historical).unwrap();
        let saved: SavedCredentials = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(postcard::to_stdvec(&saved).unwrap(), bytes);
        assert_eq!(saved.resume_key(|| Ok(key)).unwrap(), key);
        // Once imported, even unassociated v3 credentials find their archived key.
        assert_eq!(
            saved
                .resume_key(|| panic!("original config is gone"))
                .unwrap(),
            key
        );
        let (email, password, mfa) = saved.decrypt_with_key(&key).unwrap();
        let migrated = SavedCredentials::encrypt_with_key(&email, &password, mfa.as_deref(), &key);
        let encoded = postcard::to_stdvec(&migrated).unwrap();
        let _: (String, String, Option<String>) = postcard::from_bytes(&encoded).unwrap();
        let restored: SavedCredentials = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(
            restored
                .resume_key(|| panic!("migrated snapshot must identify its key"))
                .unwrap(),
            key
        );
        assert_eq!(
            restored.decrypt_with_key(&key),
            Some((email, password, mfa))
        );
    }

    #[test]
    fn legacy_v2_resume_gets_a_durable_key_without_a_service_config() {
        let directory = tempfile::tempdir().unwrap();
        let _state = crate::test_support::StateDirectoryGuard::set(directory.path());
        let saved = SavedCredentials {
            email: encrypt_credential("legacy-user"),
            password: encrypt_credential("legacy-password"),
            mfa: Some(encrypt_credential("654321")),
        };
        let key = saved
            .resume_key(|| panic!("v2 needs no config key"))
            .unwrap();
        let (email, password, mfa) = saved.decrypt_legacy().unwrap();
        assert_eq!(mfa.as_deref(), Some("654321"));
        let migrated = SavedCredentials::encrypt_with_key(&email, &password, None, &key);
        assert_eq!(
            migrated.resume_key(|| panic!("use archived key")).unwrap(),
            key
        );
        assert_eq!(
            migrated.decrypt_with_key(&key),
            Some((email, password, None))
        );
    }

    #[test]
    fn invalid_session_key_identity_cannot_escape_key_store() {
        let saved = SavedCredentials {
            email: "v4:../../config:v3:invalid".into(),
            password: "v3:invalid".into(),
            mfa: None,
        };
        assert_eq!(
            saved
                .resume_key(|| panic!("invalid ID must be rejected"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
}
