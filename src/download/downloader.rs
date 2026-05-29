use std::io;
use std::sync::Arc;

use crate::config::DownloadConfig;
use crate::error::{Error, Result};
use crate::fs::{FileSystem, TokioFileSystem};

use super::callbacks::DownloadProgress;
use super::collect::{CollectedFiles, collect_files_with_downloader};
use super::inspect::{InspectedLocalFile, inspect_local_file as inspect_local_file_with_fs};
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
pub async fn delete_resume_artifacts(path: &str) -> io::Result<()> {
    sidecar::delete_resume_artifacts_for_path(path).await
}

/// Deletes the final output and resumable download artifacts for a path.
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
    pub const fn new(client: mega::Client, config: DownloadConfig) -> Self {
        Self {
            client,
            config,
            fs: TokioFileSystem,
        }
    }
}

impl<F: FileSystem> Downloader<F> {
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
