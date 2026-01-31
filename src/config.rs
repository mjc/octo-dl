//! Configuration types for download operations.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::state::{decrypt_credential, encrypt_credential};

/// Configuration for download operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadConfig {
    /// Number of parallel chunks per file download.
    pub chunks_per_file: usize,
    /// Number of concurrent file downloads.
    pub concurrent_files: usize,
    /// Whether to overwrite existing files.
    pub force_overwrite: bool,
    /// Whether to clean up `.part` files on download error.
    pub cleanup_on_error: bool,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            chunks_per_file: 2,
            concurrent_files: 4,
            force_overwrite: false,
            cleanup_on_error: true,
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

    #[test]
    fn default_config() {
        let config = DownloadConfig::default();
        assert_eq!(config.chunks_per_file, 2);
        assert_eq!(config.concurrent_files, 4);
        assert!(!config.force_overwrite);
        assert!(config.cleanup_on_error);
    }

    #[test]
    fn builder_pattern() {
        let config = DownloadConfig::new()
            .with_chunks_per_file(8)
            .with_concurrent_files(2)
            .with_force_overwrite(true)
            .with_cleanup_on_error(false);

        assert_eq!(config.chunks_per_file, 8);
        assert_eq!(config.concurrent_files, 2);
        assert!(config.force_overwrite);
        assert!(!config.cleanup_on_error);
    }

    #[test]
    fn config_serializes_to_toml() {
        let config = DownloadConfig::default();
        let toml_str = toml::to_string(&config).unwrap();
        let deserialized: DownloadConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(deserialized.chunks_per_file, config.chunks_per_file);
        assert_eq!(deserialized.concurrent_files, config.concurrent_files);
        assert_eq!(deserialized.force_overwrite, config.force_overwrite);
        assert_eq!(deserialized.cleanup_on_error, config.cleanup_on_error);
    }
}

// ============================================================================
// Service configuration (headless / systemd mode)
// ============================================================================

fn default_api_host() -> String {
    "0.0.0.0".to_string()
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
}

impl ServiceCredentials {
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
}

/// API server bind configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiConfig {
    #[serde(default = "default_api_host")]
    pub host: String,
    #[serde(default = "default_api_port")]
    pub port: u16,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            host: default_api_host(),
            port: default_api_port(),
        }
    }
}

/// Top-level service configuration loaded from a TOML file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceConfig {
    pub credentials: ServiceCredentials,
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
        let contents = std::fs::read_to_string(path)?;
        toml::from_str(&contents)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Saves the config back to disk with 0o600 permissions.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be written.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let toml_str = toml::to_string(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, &toml_str)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(path, perms)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod service_config_tests {
    use super::*;

    #[test]
    fn service_config_round_trip() {
        let config = ServiceConfig {
            credentials: ServiceCredentials {
                encrypted: false,
                email: "user@example.com".to_string(),
                password: "secret".to_string(),
                mfa: String::new(),
            },
            api: ApiConfig::default(),
            download: DownloadConfig::default(),
        };

        let toml_str = toml::to_string(&config).unwrap();
        let loaded: ServiceConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(loaded.credentials.email, "user@example.com");
        assert_eq!(loaded.api.port, 9723);
        assert_eq!(loaded.download.concurrent_files, 4);
    }

    #[test]
    fn service_credentials_encrypt_decrypt() {
        let mut creds = ServiceCredentials {
            encrypted: false,
            email: "test@test.com".to_string(),
            password: "hunter2".to_string(),
            mfa: String::new(),
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
    fn service_config_save_load() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");

        let config = ServiceConfig {
            credentials: ServiceCredentials {
                encrypted: false,
                email: "a@b.com".to_string(),
                password: "pass".to_string(),
                mfa: String::new(),
            },
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
        assert_eq!(config.api.host, "0.0.0.0");
        assert_eq!(config.api.port, 9723);
        assert_eq!(config.download.concurrent_files, 4);
        assert!(!config.credentials.encrypted);
        assert!(config.credentials.mfa.is_empty());
    }
}
