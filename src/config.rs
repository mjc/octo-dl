//! Configuration types for download operations.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::core::{
    decode_credential_key, decrypt_credential, decrypt_credential_with_key, encrypt_credential,
    encrypt_credential_with_key, generate_credential_key,
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

/// Configuration for download operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadConfig {
    /// Download directory path (used in service mode).
    #[serde(default = "default_download_path")]
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

fn deserialize_positive_usize<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = usize::deserialize(deserializer)?;
    (value > 0)
        .then_some(value)
        .ok_or_else(|| serde::de::Error::custom("value must be greater than zero"))
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
        option::of(string_regex("[A-Za-z0-9_./-]{0,32}").expect("valid path regex"))
    }

    fn download_config_strategy() -> impl Strategy<Value = DownloadConfig> {
        (
            optional_path(),
            any::<u16>(),
            any::<u16>(),
            any::<u16>(),
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

    /// Encrypts plaintext credentials in place, setting `encrypted = true`.
    pub fn encrypt_in_place(&mut self) {
        if !self.encrypted {
            self.email = encrypt_credential(&self.email);
            self.password = encrypt_credential(&self.password);
            if !self.mfa.is_empty() {
                self.mfa = encrypt_credential(&self.mfa);
            }
            self.encrypted = true;
        }
    }

    /// Decrypts credentials using the random key persisted with the service config.
    #[must_use]
    pub fn decrypt_if_needed_with_key(&self, key: &[u8; 16]) -> Option<(String, String, String)> {
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
    pub fn encrypt_in_place_with_key(&mut self, key: &[u8; 16]) {
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiConfig {
    #[serde(default = "default_api_host")]
    pub host: String,
    #[serde(default = "default_api_port")]
    pub port: u16,
    /// Optional API key for authenticating API and remote-TUI requests.
    #[serde(default)]
    pub api_key: Option<String>,
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
    /// Loads a `ServiceConfig` from a TOML file at `path`.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or parsed.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let contents = std::fs::read_to_string(path)
            .map_err(|error| path_io_error("read config file", path, error))?;
        let mut config: Self = toml::from_str(&contents)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let key_path = credential_key_path(path);
        if key_path.exists() {
            let key = std::fs::read_to_string(&key_path)
                .map_err(|error| path_io_error("read credential key", &key_path, error))?;
            if decode_credential_key(key.trim()).is_none() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid credential key in {}", key_path.display()),
                ));
            }
            config.credential_key = Some(key.trim().to_string());
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
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| path_io_error("create config directory", parent, error))?;
        }

        let template = Self {
            credentials: ServiceCredentials {
                encrypted: false,
                email: String::new(),
                password: String::new(),
                mfa: String::new(),
                saved_session: None,
            },
            credential_key: Some(generate_credential_key()),
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
        use std::io::Write as _;

        let toml_str = toml::to_string(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let file_name = path.file_name().unwrap_or_default().to_string_lossy();
        let temporary_path = parent.join(format!(
            ".{file_name}.{}.{}.tmp",
            std::process::id(),
            TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));

        let result: std::io::Result<()> = (|| {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary_path).map_err(|error| {
                path_io_error("write temporary config file", &temporary_path, error)
            })?;

            file.write_all(toml_str.as_bytes()).map_err(|error| {
                path_io_error("write temporary config file", &temporary_path, error)
            })?;
            file.flush().map_err(|error| {
                path_io_error("flush temporary config file", &temporary_path, error)
            })?;
            file.sync_all().map_err(|error| {
                path_io_error("sync temporary config file", &temporary_path, error)
            })?;

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&temporary_path, std::fs::Permissions::from_mode(0o600))
                    .map_err(|error| {
                    path_io_error("set config file permissions", &temporary_path, error)
                })?;
            }

            std::fs::rename(&temporary_path, path)
                .map_err(|error| path_io_error("replace config file", path, error))?;

            #[cfg(unix)]
            {
                let directory = std::fs::File::open(parent)
                    .map_err(|error| path_io_error("open config directory", parent, error))?;
                directory
                    .sync_all()
                    .map_err(|error| path_io_error("sync config directory", parent, error))?;
            }

            Ok(())
        })();

        if result.is_err() {
            let _ = std::fs::remove_file(&temporary_path);
        }
        result?;

        if let Some(key) = self.credential_key.as_deref() {
            let key_path = credential_key_path(path);
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(&key_path)
                .map_err(|error| path_io_error("write credential key", &key_path, error))?;
            file.write_all(key.as_bytes())
                .map_err(|error| path_io_error("write credential key", &key_path, error))?;
            file.write_all(b"\n")
                .map_err(|error| path_io_error("write credential key", &key_path, error))?;
            file.sync_all()
                .map_err(|error| path_io_error("sync credential key", &key_path, error))?;
        }
        Ok(())
    }
}

fn credential_key_path(path: &Path) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("{}.key", path.display()))
}

fn path_io_error(action: &str, path: &Path, error: std::io::Error) -> std::io::Error {
    std::io::Error::new(
        error.kind(),
        format!("{action} {}: {error}", path.display()),
    )
}

#[cfg(test)]
mod service_config_tests {
    use super::*;
    use proptest::{prelude::*, string::string_regex};

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
        let mut creds = ServiceCredentials {
            encrypted: false,
            email: "test@test.com".to_string(),
            password: "hunter2".to_string(),
            mfa: String::new(),
            saved_session: None,
        };

        let (e, p, m) = creds.decrypt_if_needed().unwrap();
        assert_eq!(e, "test@test.com");
        assert_eq!(p, "hunter2");
        assert!(m.is_empty());

        creds.encrypt_in_place();
        assert!(creds.encrypted);
        assert_ne!(creds.email, "test@test.com");
        assert_ne!(creds.password, "hunter2");

        let (e2, p2, _) = creds.decrypt_if_needed().unwrap();
        assert_eq!(e2, "test@test.com");
        assert_eq!(p2, "hunter2");
    }

    #[test]
    fn service_credentials_use_persisted_random_key_format() {
        let key = decode_credential_key(&generate_credential_key()).unwrap();
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
        assert!(creds.decrypt_if_needed_with_key(&[0; 16]).is_none());
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

            creds.encrypt_in_place();

            prop_assert!(creds.encrypted);
            prop_assert_eq!(
                creds.decrypt_if_needed(),
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

            creds.encrypt_in_place();
            let once = creds.clone();
            creds.encrypt_in_place();

            prop_assert_eq!(creds.encrypted, once.encrypted);
            prop_assert_eq!(creds.email, once.email);
            prop_assert_eq!(creds.password, once.password);
            prop_assert_eq!(creds.mfa, once.mfa);
        }
    }
}
