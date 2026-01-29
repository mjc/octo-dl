//! Configuration types for download operations.

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
