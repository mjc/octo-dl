//! Configuration types for download operations.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize, de};
use thiserror::Error;

use crate::core::{
    decode_credential_key, decrypt_credential, decrypt_credential_with_key,
    encrypt_credential_with_key,
};

const fn default_download_path() -> Option<String> {
    None
}

const fn default_mega_chunks_per_request() -> usize {
    2
}

const fn default_chunks_per_file() -> usize {
    2
}

const fn default_concurrent_files() -> usize {
    4
}

/// Random per-config key used to encrypt persisted MEGA credentials and sessions.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CredentialKey([u8; 16]);

impl CredentialKey {
    /// Creates a fresh random credential key.
    #[must_use]
    pub fn generate() -> Self {
        let bytes = *uuid::Uuid::new_v4().as_bytes();
        Self(bytes)
    }

    /// Decodes a persisted credential key.
    #[must_use]
    pub fn decode(encoded: &str) -> Option<Self> {
        decode_credential_key(encoded).map(Self)
    }

    /// Encodes this key for the existing config sidecar format.
    #[must_use]
    pub fn encode(self) -> String {
        BASE64.encode(self.0)
    }

    /// Returns the key bytes for authenticated encryption.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl std::fmt::Debug for CredentialKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CredentialKey([REDACTED])")
    }
}

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Configuration for download operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadConfig {
    /// Download directory path (used in service mode).
    #[serde(
        default = "default_download_path",
        deserialize_with = "deserialize_non_empty_path"
    )]
    pub path: Option<String>,
    /// Number of parallel chunks per file download.
    #[serde(
        default = "default_chunks_per_file",
        deserialize_with = "deserialize_positive_usize"
    )]
    pub chunks_per_file: usize,
    /// Maximum adjacent MEGA chunks fetched per HTTP request.
    #[serde(
        default = "default_mega_chunks_per_request",
        deserialize_with = "deserialize_positive_usize"
    )]
    pub mega_chunks_per_request: usize,
    /// Number of concurrent file downloads.
    #[serde(
        default = "default_concurrent_files",
        deserialize_with = "deserialize_positive_usize"
    )]
    pub concurrent_files: usize,
    /// Whether to overwrite existing files.
    #[serde(default)]
    pub force_overwrite: bool,
    /// Whether to clean up `.part` files on recoverable download errors.
    #[serde(default)]
    pub cleanup_on_error: bool,
}

/// Validation failures for values that control download parallelism or output
/// location.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DownloadConfigError {
    /// A positive download limit was configured as zero.
    #[error("{field} must be greater than zero")]
    NonPositive {
        /// Name of the invalid configuration field.
        field: &'static str,
    },
    /// The configured download root is ambiguous.
    #[error("download path must not be empty")]
    EmptyPath,
}

fn deserialize_positive_usize<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = usize::deserialize(deserializer)?;
    (value > 0)
        .then_some(value)
        .ok_or_else(|| serde::de::Error::custom("value must be greater than zero"))
}

fn deserialize_non_empty_path<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let path = Option::<String>::deserialize(deserializer)?;
    path.map_or(Ok(None), |path| {
        (!path.is_empty())
            .then_some(Some(path))
            .ok_or_else(|| serde::de::Error::custom("download path must not be empty"))
    })
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            path: None,
            chunks_per_file: 2,
            mega_chunks_per_request: default_mega_chunks_per_request(),
            concurrent_files: 4,
            force_overwrite: false,
            cleanup_on_error: false,
        }
    }
}

impl DownloadConfig {
    /// Creates a new configuration with default values.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Validates values that would otherwise make download execution
    /// ambiguous or unable to make progress.
    ///
    /// # Errors
    ///
    /// Returns an error when a required positive setting is zero or when the
    /// configured download path is empty.
    pub fn validate(&self) -> Result<(), DownloadConfigError> {
        for (field, value) in [
            ("chunks_per_file", self.chunks_per_file),
            ("mega_chunks_per_request", self.mega_chunks_per_request),
            ("concurrent_files", self.concurrent_files),
        ] {
            if value == 0 {
                return Err(DownloadConfigError::NonPositive { field });
            }
        }
        if self.path.as_deref().is_some_and(str::is_empty) {
            return Err(DownloadConfigError::EmptyPath);
        }
        Ok(())
    }

    /// Sets the number of chunks per file.
    #[must_use]
    pub const fn with_chunks_per_file(mut self, chunks: usize) -> Self {
        self.chunks_per_file = chunks;
        self
    }

    /// Sets the maximum adjacent MEGA chunks fetched per request.
    #[must_use]
    pub const fn with_mega_chunks_per_request(mut self, chunks: usize) -> Self {
        self.mega_chunks_per_request = chunks;
        self
    }

    /// Sets the number of concurrent file downloads.
    #[must_use]
    pub const fn with_concurrent_files(mut self, concurrent: usize) -> Self {
        self.concurrent_files = concurrent;
        self
    }

    /// Sets whether to force overwrite existing files.
    #[must_use]
    pub const fn with_force_overwrite(mut self, force: bool) -> Self {
        self.force_overwrite = force;
        self
    }

    /// Sets whether to clean up `.part` files on download error.
    #[must_use]
    pub const fn with_cleanup_on_error(mut self, cleanup: bool) -> Self {
        self.cleanup_on_error = cleanup;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::{option, prelude::*, string::string_regex};

    fn optional_path() -> impl Strategy<Value = Option<String>> {
        option::of(string_regex("[A-Za-z0-9_./-]{1,32}").expect("valid path regex"))
    }

    fn download_config_strategy() -> impl Strategy<Value = DownloadConfig> {
        (
            optional_path(),
            1..=u16::MAX,
            1..=u16::MAX,
            1..=u16::MAX,
            any::<bool>(),
            any::<bool>(),
        )
            .prop_map(
                |(
                    path,
                    chunks_per_file,
                    mega_chunks_per_request,
                    concurrent_files,
                    force_overwrite,
                    cleanup_on_error,
                )| DownloadConfig {
                    path,
                    chunks_per_file: usize::from(chunks_per_file),
                    mega_chunks_per_request: usize::from(mega_chunks_per_request),
                    concurrent_files: usize::from(concurrent_files),
                    force_overwrite,
                    cleanup_on_error,
                },
            )
    }

    #[test]
    fn default_config() {
        let config = DownloadConfig::default();
        assert_eq!(config.chunks_per_file, 2);
        assert_eq!(config.mega_chunks_per_request, 2);
        assert_eq!(config.concurrent_files, 4);
        assert!(!config.force_overwrite);
        assert!(!config.cleanup_on_error);
    }

    #[test]
    fn validation_rejects_zero_chunk_and_concurrency_limits() {
        for (field, config) in [
            (
                "chunks_per_file",
                DownloadConfig::default().with_chunks_per_file(0),
            ),
            (
                "mega_chunks_per_request",
                DownloadConfig::default().with_mega_chunks_per_request(0),
            ),
            (
                "concurrent_files",
                DownloadConfig::default().with_concurrent_files(0),
            ),
        ] {
            let error = config.validate().expect_err("zero limit should be invalid");
            assert_eq!(error, DownloadConfigError::NonPositive { field });
        }
    }

    #[test]
    fn validation_rejects_an_empty_download_root() {
        let config = DownloadConfig {
            path: Some(String::new()),
            ..DownloadConfig::default()
        };

        assert_eq!(config.validate(), Err(DownloadConfigError::EmptyPath));
    }

    #[test]
    fn builder_pattern() {
        let config = DownloadConfig::new()
            .with_chunks_per_file(8)
            .with_mega_chunks_per_request(3)
            .with_concurrent_files(2)
            .with_force_overwrite(true)
            .with_cleanup_on_error(true);

        assert_eq!(config.chunks_per_file, 8);
        assert_eq!(config.mega_chunks_per_request, 3);
        assert_eq!(config.concurrent_files, 2);
        assert!(config.force_overwrite);
        assert!(config.cleanup_on_error);
    }

    #[test]
    fn config_serializes_to_toml() {
        let config = DownloadConfig::default();
        let toml_str = toml::to_string(&config).unwrap();
        let deserialized: DownloadConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(deserialized.chunks_per_file, config.chunks_per_file);
        assert_eq!(
            deserialized.mega_chunks_per_request,
            config.mega_chunks_per_request
        );
        assert_eq!(deserialized.concurrent_files, config.concurrent_files);
        assert_eq!(deserialized.force_overwrite, config.force_overwrite);
        assert_eq!(deserialized.cleanup_on_error, config.cleanup_on_error);
    }

    proptest! {
        #[test]
        fn builder_methods_set_exact_fields(
            path in optional_path(),
            chunks_per_file in any::<u16>(),
            mega_chunks_per_request in any::<u16>(),
            concurrent_files in any::<u16>(),
            force_overwrite in any::<bool>(),
            cleanup_on_error in any::<bool>(),
        ) {
            let config = DownloadConfig {
                path: path.clone(),
                ..DownloadConfig::new()
            }
            .with_chunks_per_file(usize::from(chunks_per_file))
            .with_mega_chunks_per_request(usize::from(mega_chunks_per_request))
            .with_concurrent_files(usize::from(concurrent_files))
            .with_force_overwrite(force_overwrite)
            .with_cleanup_on_error(cleanup_on_error);

            prop_assert_eq!(config.path, path);
            prop_assert_eq!(config.chunks_per_file, usize::from(chunks_per_file));
            prop_assert_eq!(
                config.mega_chunks_per_request,
                usize::from(mega_chunks_per_request)
            );
            prop_assert_eq!(config.concurrent_files, usize::from(concurrent_files));
            prop_assert_eq!(config.force_overwrite, force_overwrite);
            prop_assert_eq!(config.cleanup_on_error, cleanup_on_error);
        }

        #[test]
        fn download_config_toml_round_trips(config in download_config_strategy()) {
            let toml_str = toml::to_string(&config).unwrap();
            let deserialized: DownloadConfig = toml::from_str(&toml_str).unwrap();
            prop_assert_eq!(deserialized, config);
        }
    }
}

// ============================================================================
// Service configuration (headless / systemd mode)
// ============================================================================

/// Default API host.
///
/// Binds to loopback only by default so the unauthenticated API is not
/// exposed on external interfaces. To expose externally, configure a
/// non-loopback address and place behind an auth-protecting proxy or VPN.
fn default_api_host() -> String {
    "127.0.0.1".to_string()
}

const fn default_api_port() -> u16 {
    9723
}

/// Credentials section of the service config file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceCredentials {
    #[serde(default)]
    pub encrypted: bool,
    pub email: String,
    pub password: String,
    #[serde(default)]
    pub mfa: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_session: Option<crate::core::SavedMegaSession>,
}

impl ServiceCredentials {
    /// Returns `true` if both email and password are non-empty.
    #[must_use]
    pub const fn has_credentials(&self) -> bool {
        !self.email.is_empty() && !self.password.is_empty()
    }

    /// Returns decrypted `(email, password, mfa)`.
    ///
    /// If `encrypted` is true, decrypts each field first.
    /// Returns `None` if decryption fails.
    #[must_use]
    pub fn decrypt_if_needed(&self) -> Option<(String, String, String)> {
        if self.encrypted {
            let email = decrypt_credential(&self.email)?;
            let password = decrypt_credential(&self.password)?;
            let mfa = if self.mfa.is_empty() {
                String::new()
            } else {
                decrypt_credential(&self.mfa)?
            };
            Some((email, password, mfa))
        } else {
            Some((self.email.clone(), self.password.clone(), self.mfa.clone()))
        }
    }

    /// Decrypts credentials using the random key persisted with the service config.
    #[must_use]
    pub fn decrypt_if_needed_with_key(
        &self,
        key: &CredentialKey,
    ) -> Option<(String, String, String)> {
        if !self.encrypted {
            return Some((self.email.clone(), self.password.clone(), self.mfa.clone()));
        }
        let email = decrypt_credential_with_key(&self.email, key)?;
        let password = decrypt_credential_with_key(&self.password, key)?;
        let mfa = if self.mfa.is_empty() {
            String::new()
        } else {
            decrypt_credential_with_key(&self.mfa, key)?
        };
        Some((email, password, mfa))
    }

    /// Encrypts plaintext credentials with the service config's random key.
    pub fn encrypt_in_place_with_key(&mut self, key: &CredentialKey) {
        if !self.encrypted {
            self.email = encrypt_credential_with_key(&self.email, key);
            self.password = encrypt_credential_with_key(&self.password, key);
            if !self.mfa.is_empty() {
                self.mfa = encrypt_credential_with_key(&self.mfa, key);
            }
            self.encrypted = true;
        }
    }
}

/// API server bind configuration.
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ApiKey(String);

impl ApiKey {
    /// Constructs an API key, rejecting values that cannot authenticate a request.
    ///
    /// # Errors
    ///
    /// Returns [`ApiKeyError::Empty`] when the supplied value is empty or only
    /// whitespace.
    pub fn new(value: impl Into<String>) -> Result<Self, ApiKeyError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(ApiKeyError::Empty);
        }
        Ok(Self(value))
    }

    /// Exposes the key at a protocol boundary that must send it to a peer.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("ApiKey")
            .field(&"<redacted>")
            .finish()
    }
}

impl<'de> Deserialize<'de> for ApiKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

impl TryFrom<String> for ApiKey {
    type Error = ApiKeyError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for ApiKey {
    type Error = ApiKeyError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ApiKeyError {
    #[error("API key must not be empty")]
    Empty,
}

fn deserialize_optional_api_key<'de, D>(deserializer: D) -> Result<Option<ApiKey>, D::Error>
where
    D: de::Deserializer<'de>,
{
    match Option::<String>::deserialize(deserializer)? {
        None => Ok(None),
        Some(value) if value.trim().is_empty() => Ok(None),
        Some(value) => ApiKey::new(value).map(Some).map_err(de::Error::custom),
    }
}

/// API server bind configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiConfig {
    #[serde(default = "default_api_host")]
    pub host: String,
    #[serde(default = "default_api_port")]
    pub port: u16,
    /// Optional API key for authenticating API and remote-TUI requests.
    #[serde(default, deserialize_with = "deserialize_optional_api_key")]
    pub api_key: Option<ApiKey>,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            host: default_api_host(),
            port: default_api_port(),
            api_key: None,
        }
    }
}

/// Top-level service configuration loaded from a TOML file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceConfig {
    pub credentials: ServiceCredentials,
    /// Random per-config key used for service credentials. Older configs omit it
    /// and are migrated after their legacy machine-bound credentials decrypt.
    #[serde(skip)]
    pub credential_key: Option<String>,
    #[serde(default)]
    pub api: ApiConfig,
    #[serde(default)]
    pub download: DownloadConfig,
}

impl ServiceConfig {
    fn migrate_legacy_saved_session(&mut self) -> bool {
        let Some(saved_session) = self.credentials.saved_session.as_ref() else {
            return false;
        };
        if !saved_session.email.starts_with("v2:") || !saved_session.session.starts_with("v2:") {
            return false;
        }

        let key = self
            .credential_key
            .as_deref()
            .and_then(CredentialKey::decode)
            .unwrap_or_else(CredentialKey::generate);
        let Some(migrated) = saved_session.reencrypt_legacy_with_key(&key) else {
            return false;
        };

        self.credentials.saved_session = Some(migrated);
        self.credential_key = Some(key.encode());
        true
    }

    /// Loads a `ServiceConfig` from a TOML file at `path`.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or parsed.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let contents = std::fs::read_to_string(path)
            .map_err(|error| path_io_error("read config file", path, &error))?;
        let mut config: Self = toml::from_str(&contents)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let key_path = credential_key_path(path);
        if key_path.exists() {
            let key = std::fs::read_to_string(&key_path)
                .map_err(|error| path_io_error("read credential key", &key_path, &error))?;
            if decode_credential_key(key.trim()).is_none() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid credential key in {}", key_path.display()),
                ));
            }
            config.credential_key = Some(key.trim().to_string());
        }
        if config.migrate_legacy_saved_session() {
            config.save(path)?;
        }
        Ok(config)
    }

    /// Loads from `path`, or creates a template config file if it doesn't exist.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read/written or parsed.
    pub fn load_or_create(path: &Path) -> std::io::Result<Self> {
        if path.exists() {
            return Self::load(path);
        }

        // Ensure parent directory exists
        let parent = config_parent(path);
        std::fs::create_dir_all(parent)
            .map_err(|error| path_io_error("create config directory", parent, &error))?;

        let template = Self {
            credentials: ServiceCredentials {
                encrypted: false,
                email: String::new(),
                password: String::new(),
                mfa: String::new(),
                saved_session: None,
            },
            credential_key: Some(CredentialKey::generate().encode()),
            api: ApiConfig::default(),
            download: DownloadConfig {
                path: None,
                ..DownloadConfig::default()
            },
        };
        template.save(path)?;
        Ok(template)
    }

    /// Saves the config back to disk with 0o600 permissions.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be written.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let mut config = self.clone();
        config.migrate_legacy_saved_session();
        let toml_str = toml::to_string(&config)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        let parent = config_parent(path);
        let save_id = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let file_name = path.file_name().unwrap_or_default().to_string_lossy();
        let temporary_path = parent.join(format!(
            ".{file_name}.{}.{}.tmp",
            std::process::id(),
            save_id
        ));

        let result = write_durable_temp_file(
            &temporary_path,
            toml_str.as_bytes(),
            "temporary config file",
        );

        if result.is_err() {
            let _ = std::fs::remove_file(&temporary_path);
        }
        result?;

        let Some(key) = config.credential_key.as_deref() else {
            return replace_config_file(&temporary_path, path, parent);
        };

        let key_path = credential_key_path(path);
        if key_path.exists() && !key_path.is_file() {
            let _ = std::fs::remove_file(&temporary_path);
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("credential key path is not a file: {}", key_path.display()),
            ));
        }
        let key_temporary_path = parent.join(format!(
            ".{}.{}.{}.tmp",
            key_path.file_name().unwrap_or_default().to_string_lossy(),
            std::process::id(),
            save_id
        ));
        let key_contents = format!("{key}\n");
        let key_result = write_durable_temp_file(
            &key_temporary_path,
            key_contents.as_bytes(),
            "temporary credential key file",
        );
        if let Err(error) = key_result {
            let _ = std::fs::remove_file(&temporary_path);
            let _ = std::fs::remove_file(&key_temporary_path);
            return Err(error);
        }

        let key_backup_path = parent.join(format!(
            ".{}.{}.{}.bak",
            key_path.file_name().unwrap_or_default().to_string_lossy(),
            std::process::id(),
            save_id
        ));
        let had_key = key_path.exists();
        if had_key {
            std::fs::copy(&key_path, &key_backup_path).map_err(|error| {
                path_io_error("backup credential key", &key_backup_path, &error)
            })?;
        }
        if let Err(error) = std::fs::rename(&key_temporary_path, &key_path) {
            let _ = std::fs::remove_file(&temporary_path);
            let _ = std::fs::remove_file(&key_temporary_path);
            let _ = std::fs::remove_file(&key_backup_path);
            return Err(path_io_error("replace credential key", &key_path, &error));
        }
        if let Err(error) = std::fs::rename(&temporary_path, path) {
            let _ = std::fs::remove_file(&temporary_path);
            let restore_result = if had_key {
                std::fs::remove_file(&key_path)
                    .and_then(|()| std::fs::rename(&key_backup_path, &key_path))
            } else {
                std::fs::remove_file(&key_path)
            };
            return Err(match restore_result {
                Ok(()) => path_io_error("replace config file", path, &error),
                Err(restore_error) => std::io::Error::new(
                    error.kind(),
                    format!(
                        "replace config file {}: {error}; failed to restore credential key {}: {restore_error}",
                        path.display(),
                        key_path.display()
                    ),
                ),
            });
        }
        let _ = std::fs::remove_file(&key_backup_path);
        sync_directory(parent)
    }
}

fn config_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

pub(crate) fn write_durable_temp_file(
    path: &Path,
    contents: &[u8],
    description: &str,
) -> std::io::Result<()> {
    use std::io::Write as _;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| path_io_error(&format!("write {description}"), path, &error))?;
    file.write_all(contents)
        .map_err(|error| path_io_error(&format!("write {description}"), path, &error))?;
    file.flush()
        .map_err(|error| path_io_error(&format!("flush {description}"), path, &error))?;
    file.sync_all()
        .map_err(|error| path_io_error(&format!("sync {description}"), path, &error))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(
            |error| path_io_error(&format!("set {description} permissions"), path, &error),
        )?;
    }

    Ok(())
}

fn replace_config_file(temporary_path: &Path, path: &Path, parent: &Path) -> std::io::Result<()> {
    std::fs::rename(temporary_path, path)
        .map_err(|error| path_io_error("replace config file", path, &error))?;
    sync_directory(parent)
}

pub(crate) fn sync_directory(parent: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let directory = std::fs::File::open(parent)
            .map_err(|error| path_io_error("open config directory", parent, &error))?;
        directory
            .sync_all()
            .map_err(|error| path_io_error("sync config directory", parent, &error))?;
    }
    #[cfg(not(unix))]
    let _ = parent;
    Ok(())
}

fn credential_key_path(path: &Path) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("{}.key", path.display()))
}

fn path_io_error(action: &str, path: &Path, error: &std::io::Error) -> std::io::Error {
    std::io::Error::new(
        error.kind(),
        format!("{action} {}: {error}", path.display()),
    )
}

#[cfg(test)]
mod service_config_tests {
    use super::*;
    use proptest::{prelude::*, string::string_regex};

    #[test]
    fn api_key_rejects_empty_values_and_redacts_debug() {
        assert!(ApiKey::new("").is_err());
        assert!(ApiKey::new("   ").is_err());
        assert!(toml::from_str::<ApiKey>(r#""""#).is_err());

        let key = ApiKey::new("secret").expect("test API key should be valid");
        let debug = format!("{key:?}");
        assert!(debug.contains("redacted"));
        assert!(!debug.contains("secret"));
        assert_eq!(key.expose_secret(), "secret");
    }

    #[test]
    fn empty_legacy_api_key_loads_as_unconfigured() {
        let config: ServiceConfig = toml::from_str(
            r#"
                [credentials]
                email = ""
                password = ""

                [api]
                api_key = ""
            "#,
        )
        .expect("legacy empty API key should remain readable");

        assert!(config.api.api_key.is_none());
    }

    fn credential_field() -> impl Strategy<Value = String> {
        string_regex("[A-Za-z0-9_.:@+/-]{0,32}").expect("valid credential regex")
    }

    #[test]
    fn service_config_round_trip() {
        let config = ServiceConfig {
            credentials: ServiceCredentials {
                encrypted: false,
                email: "user@example.com".to_string(),
                password: "secret".to_string(),
                mfa: String::new(),
                saved_session: None,
            },
            credential_key: None,
            api: ApiConfig::default(),
            download: DownloadConfig::default(),
        };

        let toml_str = toml::to_string(&config).unwrap();
        let loaded: ServiceConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(loaded.credentials.email, "user@example.com");
        assert_eq!(loaded.api.port, 9723);
        assert_eq!(loaded.download.mega_chunks_per_request, 2);
        assert_eq!(loaded.download.concurrent_files, 4);
    }

    #[test]
    fn service_credentials_encrypt_decrypt() {
        let key = CredentialKey::generate();
        let mut creds = ServiceCredentials {
            encrypted: false,
            email: "test@test.com".to_string(),
            password: "hunter2".to_string(),
            mfa: String::new(),
            saved_session: None,
        };

        creds.encrypt_in_place_with_key(&key);
        assert!(creds.encrypted);
        assert!(creds.email.starts_with("v3:"));
        assert!(creds.password.starts_with("v3:"));

        let (e2, p2, m2) = creds.decrypt_if_needed_with_key(&key).unwrap();
        assert_eq!(e2, "test@test.com");
        assert_eq!(p2, "hunter2");
        assert!(m2.is_empty());
    }

    #[test]
    fn service_credentials_use_persisted_random_key_format() {
        let key = CredentialKey::generate();
        let mut creds = ServiceCredentials {
            encrypted: false,
            email: "test@test.com".to_string(),
            password: "hunter2".to_string(),
            mfa: String::new(),
            saved_session: None,
        };

        creds.encrypt_in_place_with_key(&key);

        assert!(creds.email.starts_with("v3:"));
        assert!(creds.decrypt_if_needed_with_key(&key).is_some());
        assert!(
            creds
                .decrypt_if_needed_with_key(&CredentialKey::generate())
                .is_none()
        );
    }

    #[test]
    fn service_config_save_load() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");

        let config = ServiceConfig {
            credentials: ServiceCredentials {
                encrypted: false,
                email: "a@b.com".to_string(),
                password: "pass".to_string(),
                mfa: String::new(),
                saved_session: None,
            },
            credential_key: None,
            api: ApiConfig::default(),
            download: DownloadConfig::default(),
        };

        config.save(&path).unwrap();
        let loaded = ServiceConfig::load(&path).unwrap();
        assert_eq!(loaded.credentials.email, "a@b.com");
        assert!(!loaded.credentials.encrypted);
    }

    #[test]
    fn service_config_save_load_persists_separate_credential_key() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let credential_key = CredentialKey::generate().encode();
        let config = ServiceConfig {
            credentials: ServiceCredentials {
                encrypted: false,
                email: "a@b.com".to_string(),
                password: "pass".to_string(),
                mfa: String::new(),
                saved_session: None,
            },
            credential_key: Some(credential_key.clone()),
            api: ApiConfig::default(),
            download: DownloadConfig::default(),
        };

        config.save(&path).unwrap();

        assert_eq!(
            std::fs::read_to_string(credential_key_path(&path))
                .unwrap()
                .trim(),
            credential_key
        );
        assert_eq!(
            ServiceConfig::load(&path)
                .unwrap()
                .credential_key
                .as_deref(),
            Some(credential_key.as_str())
        );
    }

    #[test]
    fn save_restores_key_when_config_replacement_fails() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let key_path = credential_key_path(&path);
        let old_key = CredentialKey::generate().encode();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(&key_path, format!("{old_key}\n")).unwrap();

        let config = ServiceConfig {
            credentials: ServiceCredentials {
                encrypted: false,
                email: "a@b.com".to_string(),
                password: "pass".to_string(),
                mfa: String::new(),
                saved_session: None,
            },
            credential_key: Some(CredentialKey::generate().encode()),
            api: ApiConfig::default(),
            download: DownloadConfig::default(),
        };

        assert!(config.save(&path).is_err());
        assert!(path.is_dir());
        assert_eq!(
            std::fs::read_to_string(key_path).unwrap(),
            format!("{old_key}\n")
        );
    }

    #[test]
    fn save_does_not_replace_config_when_key_path_is_not_a_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let initial = ServiceConfig {
            credentials: ServiceCredentials {
                encrypted: false,
                email: "old@example.com".to_string(),
                password: "old-pass".to_string(),
                mfa: String::new(),
                saved_session: None,
            },
            credential_key: None,
            api: ApiConfig::default(),
            download: DownloadConfig::default(),
        };
        initial.save(&path).unwrap();
        let original = std::fs::read(&path).unwrap();
        std::fs::create_dir(credential_key_path(&path)).unwrap();

        let replacement = ServiceConfig {
            credentials: initial.credentials,
            credential_key: Some(CredentialKey::generate().encode()),
            api: initial.api,
            download: initial.download,
        };
        assert!(replacement.save(&path).is_err());
        assert_eq!(std::fs::read(path).unwrap(), original);
    }

    #[test]
    fn minimal_toml_uses_defaults() {
        let toml_str = r#"
[credentials]
email = "x@y.com"
password = "pw"
"#;
        let config: ServiceConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.api.host, "127.0.0.1");
        assert_eq!(config.api.port, 9723);
        assert_eq!(config.download.mega_chunks_per_request, 2);
        assert_eq!(config.download.concurrent_files, 4);
        assert!(!config.credentials.encrypted);
        assert!(config.credentials.mfa.is_empty());
    }

    #[test]
    fn partial_download_table_uses_field_defaults() {
        let toml_str = r#"
[credentials]
email = "x@y.com"
password = "pw"

[download]
path = "/tmp/downloads"
"#;
        let config: ServiceConfig = toml::from_str(toml_str).unwrap();

        assert_eq!(config.download.path.as_deref(), Some("/tmp/downloads"));
        assert_eq!(config.download.chunks_per_file, 2);
        assert_eq!(config.download.mega_chunks_per_request, 2);
        assert_eq!(config.download.concurrent_files, 4);
        assert!(!config.download.force_overwrite);
        assert!(!config.download.cleanup_on_error);
    }

    #[test]
    fn zero_download_limits_are_rejected() {
        for field in [
            "chunks_per_file",
            "mega_chunks_per_request",
            "concurrent_files",
        ] {
            let toml_str = format!(
                "[credentials]\nemail = \"x@y.com\"\npassword = \"pw\"\n\n[download]\n{field} = 0\n"
            );
            let error = toml::from_str::<ServiceConfig>(&toml_str)
                .expect_err("zero download limits should be invalid");
            assert!(error.to_string().contains("greater than zero"));
        }
    }

    #[test]
    fn first_run_explicit_config_does_not_assume_nixos_download_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let config = ServiceConfig::load_or_create(&path).unwrap();

        assert_eq!(config.download.path, None);
        assert_eq!(ServiceConfig::load(&path).unwrap().download.path, None);
    }

    #[cfg(unix)]
    #[test]
    fn save_writes_config_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = ServiceConfig {
            credentials: ServiceCredentials {
                encrypted: false,
                email: "a@b.com".to_string(),
                password: "pass".to_string(),
                mfa: String::new(),
                saved_session: None,
            },
            credential_key: None,
            api: ApiConfig::default(),
            download: DownloadConfig::default(),
        };

        config.save(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn load_or_create_accepts_a_bare_filename_in_the_current_directory() {
        let dir = tempfile::tempdir().unwrap();
        let _cwd = crate::test_support::CurrentDirGuard::set(dir.path());
        ServiceConfig::load_or_create(Path::new("config.toml")).unwrap();

        assert!(dir.path().join("config.toml").is_file());
        assert!(dir.path().join("config.toml.key").is_file());
    }

    #[test]
    fn service_config_load_reports_path_in_io_errors() {
        let path = Path::new("/definitely/missing/octo-dl-config.toml");
        let error = ServiceConfig::load(path).expect_err("missing config should fail");
        let message = error.to_string();

        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(message.contains("read config file"));
        assert!(message.contains(&path.display().to_string()));
    }

    proptest! {
        #[test]
        fn service_credentials_encrypt_decrypt_round_trip(
            email in credential_field(),
            password in credential_field(),
            mfa in credential_field(),
        ) {
            let mut creds = ServiceCredentials {
                encrypted: false,
                email: email.clone(),
                password: password.clone(),
                mfa: mfa.clone(),
                saved_session: None,
            };

            prop_assert_eq!(
                creds.decrypt_if_needed(),
                Some((email.clone(), password.clone(), mfa.clone()))
            );

            let key = CredentialKey::generate();
            creds.encrypt_in_place_with_key(&key);

            prop_assert!(creds.encrypted);
            prop_assert_eq!(
                creds.decrypt_if_needed_with_key(&key),
                Some((email, password, mfa.clone()))
            );
            if mfa.is_empty() {
                prop_assert!(creds.mfa.is_empty());
            } else {
                prop_assert_ne!(creds.mfa, mfa);
            }
        }

        #[test]
        fn encrypt_in_place_is_idempotent(
            email in credential_field(),
            password in credential_field(),
            mfa in credential_field(),
        ) {
            let mut creds = ServiceCredentials {
                encrypted: false,
                email,
                password,
                mfa,
                saved_session: None,
            };

            let key = CredentialKey::generate();
            creds.encrypt_in_place_with_key(&key);
            let once = creds.clone();
            creds.encrypt_in_place_with_key(&key);

            prop_assert_eq!(creds.encrypted, once.encrypted);
            prop_assert_eq!(creds.email, once.email);
            prop_assert_eq!(creds.password, once.password);
            prop_assert_eq!(creds.mfa, once.mfa);
        }
    }
}
