use std::io;
use std::sync::Arc;

use crate::config::DownloadConfig;
use crate::error::{Error, Result};
use crate::fs::{FileSystem, TokioFileSystem};

use super::callbacks::DownloadProgress;
use super::collect::{CollectedFiles, collect_files_with_downloader};
use super::inspect::{InspectedLocalFile, inspect_local_file as inspect_local_file_with_fs};
use super::path::{DownloadRoot, RelativeOutputPath, has_reserved_artifact_name};
use super::sidecar;

/// Fetches public-link metadata with a fresh anonymous MEGA client.
///
/// Public-link browsing should not depend on the caller's authenticated client
/// state. Using a fresh client avoids cross-talk between account session state
/// and public-link metadata fetches.
///
/// # Errors
///
/// Returns an error if the MEGA client cannot be created or the public link
/// metadata fetch fails.
pub async fn fetch_public_nodes(http: &reqwest::Client, url: &str) -> Result<mega::Nodes> {
    let client = mega::Client::builder().build(http.clone())?;
    client.fetch_public_nodes(url).await.map_err(Error::Mega)
}

/// Deletes resumable download artifacts for a final output path.
///
/// # Errors
///
/// Returns an error when an artifact cannot be removed.
pub async fn delete_resume_artifacts(path: &str) -> io::Result<()> {
    sidecar::delete_resume_artifacts_for_path(path).await
}

/// Deletes the final output and resumable download artifacts for a path.
///
/// # Errors
///
/// Returns an error when an artifact cannot be removed.
pub async fn delete_download_artifacts(path: &str) -> io::Result<()> {
    sidecar::delete_download_artifacts_for_path(path).await
}

/// Core downloader that handles MEGA file downloads.
pub struct Downloader<F: FileSystem = TokioFileSystem> {
    pub(super) client: mega::Client,
    pub(super) config: DownloadConfig,
    pub(super) fs: F,
}

impl Downloader<TokioFileSystem> {
    /// Creates a new downloader with the default file system.
    #[must_use]
    pub fn new(client: mega::Client, config: DownloadConfig) -> Self {
        Self {
            client,
            fs: TokioFileSystem::new()
                .with_download_root(config.path.as_deref().map(std::path::PathBuf::from)),
            config,
        }
    }
}

impl<F: FileSystem> Downloader<F> {
    pub(super) fn validate_config(&self) -> Result<()> {
        self.config.validate().map_err(Error::from)
    }

    /// Validates a configured relative output path without allowing it to
    /// escape the configured download root.
    pub(super) fn validate_output_path(&self, path: &str) -> Result<()> {
        if has_reserved_artifact_name(std::path::Path::new(path)) {
            return Err(Error::Download(
                "output path uses a reserved download artifact name".into(),
            ));
        }
        let Some(root) = self.config.path.as_deref() else {
            return Ok(());
        };
        let root = DownloadRoot::new(root).map_err(|message| Error::Download(message.into()))?;
        let output =
            RelativeOutputPath::new(path).map_err(|message| Error::Download(message.into()))?;
        root.validate_existing_ancestors(&output)
            .map_err(|error| Error::Download(error.to_string()))?;
        Ok(())
    }

    /// Creates a new downloader with a custom file system implementation.
    #[must_use]
    pub const fn with_fs(client: mega::Client, config: DownloadConfig, fs: F) -> Self {
        Self { client, config, fs }
    }

    /// Returns a reference to the underlying MEGA client.
    #[must_use]
    pub const fn client(&self) -> &mega::Client {
        &self.client
    }

    /// Returns a mutable reference to the underlying MEGA client.
    pub const fn client_mut(&mut self) -> &mut mega::Client {
        &mut self.client
    }

    /// Returns a reference to the download configuration.
    #[must_use]
    pub const fn config(&self) -> &DownloadConfig {
        &self.config
    }

    /// Classifies a file's current status on disk.
    pub(super) async fn inspect_local_file(
        &self,
        path: &str,
        expected_size: u64,
    ) -> InspectedLocalFile {
        inspect_local_file_with_fs(&self.fs, path, expected_size, self.config.force_overwrite).await
    }

    /// Collects files from nodes, checking which need to be downloaded.
    pub async fn collect_files<'a>(
        &self,
        nodes: &'a mega::Nodes,
        progress: &Arc<dyn DownloadProgress>,
    ) -> CollectedFiles<'a> {
        collect_files_with_downloader(self, nodes, progress).await
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::symlink;

    use tempfile::TempDir;

    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn output_path_rejects_symlinked_ancestor_outside_download_root_at_io() {
        let workspace = TempDir::new().unwrap();
        let root = workspace.path().join("downloads");
        let outside = workspace.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join("linked")).unwrap();

        let client = mega::Client::builder()
            .build(mega::http_client_builder().unwrap().build().unwrap())
            .unwrap();
        let config = DownloadConfig {
            path: Some(root.to_string_lossy().into_owned()),
            ..DownloadConfig::default()
        };
        let downloader = Downloader::new(client, config);

        let outside_output = root.join("linked/payload.bin.part");
        let io_error = downloader
            .fs
            .open_part_file(&outside_output, 16, false)
            .await
            .expect_err("filesystem must reject writes through an escaping symlink");
        assert_eq!(io_error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(!outside.join("payload.bin.part").exists());

        assert!(
            downloader
                .validate_output_path("linked/payload.bin")
                .is_err(),
            "a path resolving through a symlink outside the configured root must be rejected"
        );
    }
}
