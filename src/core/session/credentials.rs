use std::sync::OnceLock;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const CREDENTIAL_VERSION_PREFIX: &str = "v2:";

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
}
