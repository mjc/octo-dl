//! Configuration types for download operations.

use std::path::PathBuf;
use serde::{Deserialize, Serialize};

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

/// Path configuration for download and state directories.
#[derive(Debug, Clone)]
pub struct PathConfig {
    /// Directory where downloaded files are saved.
    pub download_dir: PathBuf,
    /// Directory where configuration files are read from.
    pub config_dir: PathBuf,
    /// Directory where session state files are saved.
    pub state_dir: PathBuf,
}

impl Default for PathConfig {
    fn default() -> Self {
        let data_dir = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
        let config_dir = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));

        Self {
            download_dir: PathBuf::from("."),
            config_dir: config_dir.join("octo-dl"),
            state_dir: data_dir.join("octo-dl").join("sessions"),
        }
    }
}

/// API server configuration.
#[derive(Debug, Clone)]
pub struct ApiConfig {
    /// Whether to enable the API server.
    pub enabled: bool,
    /// API server bind address.
    pub host: String,
    /// API server port.
    pub port: u16,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: "127.0.0.1".to_string(),
            port: 9723,
        }
    }
}

/// Complete application configuration combining download, path, and API settings.
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// Download configuration.
    pub download: DownloadConfig,
    /// Path configuration.
    pub paths: PathConfig,
    /// API configuration.
    pub api: ApiConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            download: DownloadConfig::default(),
            paths: PathConfig::default(),
            api: ApiConfig::default(),
        }
    }
}

impl AppConfig {
    /// Creates a new config with default values.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Loads configuration from defaults.
    /// In the future, this can be extended to load from config files.
    pub fn load() -> crate::Result<Self> {
        Ok(Self::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_download_config() {
        let config = DownloadConfig::default();
        assert_eq!(config.chunks_per_file, 2);
        assert_eq!(config.concurrent_files, 4);
        assert!(!config.force_overwrite);
        assert!(config.cleanup_on_error);
    }

    #[test]
    fn download_config_builder_pattern() {
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
    fn download_config_serializes_to_toml() {
        let config = DownloadConfig::default();
        let toml_str = toml::to_string(&config).unwrap();
        let deserialized: DownloadConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(deserialized.chunks_per_file, config.chunks_per_file);
        assert_eq!(deserialized.concurrent_files, config.concurrent_files);
        assert_eq!(deserialized.force_overwrite, config.force_overwrite);
        assert_eq!(deserialized.cleanup_on_error, config.cleanup_on_error);
    }

    #[test]
    fn default_path_config() {
        let config = PathConfig::default();
        assert_eq!(config.download_dir, PathBuf::from("."));
        assert!(config.state_dir.to_string_lossy().contains("octo-dl"));
        assert!(config.state_dir.to_string_lossy().contains("sessions"));
    }

    #[test]
    fn default_api_config() {
        let config = ApiConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 9723);
    }

    #[test]
    fn default_app_config() {
        let config = AppConfig::default();
        assert_eq!(config.download.chunks_per_file, 2);
        assert!(!config.api.enabled);
        assert_eq!(config.paths.download_dir, PathBuf::from("."));
    }

    #[test]
    fn app_config_load() {
        let config = AppConfig::load().unwrap();
        assert_eq!(config.download.chunks_per_file, 2);
    }
}
