use std::io;
use std::sync::Arc;

use crate::config::DownloadConfig;
use crate::error::{Error, Result};
use crate::fs::{FileSystem, TokioFileSystem};

use super::callbacks::DownloadProgress;
use super::collect::{CollectedFiles, DownloadItem, collect_download_items};
use super::inspect::{
    FileStatus, InspectedLocalFile, inspect_local_file as inspect_local_file_with_fs,
};
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

/// Bytes and chunks reused from resumable state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeReuse {
    pub chunks: usize,
    pub bytes: u64,
    pub source: ResumeReuseSource,
}

/// Result of manually checking resumable state without starting a download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeReverify {
    pub sidecar_loaded: bool,
    pub chunks: usize,
    pub bytes: u64,
}

/// Source of reused chunk state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeReuseSource {
    Sidecar,
}

pub(super) const CURRENT_RESUME_SIDECAR_VERSION: u32 = 2;

/// Deletes resumable download artifacts for a final output path.
pub async fn delete_resume_artifacts(path: &str) -> io::Result<()> {
    sidecar::delete_resume_artifacts_for_path(path).await
}

/// Deletes the final output and resumable download artifacts for a path.
pub async fn delete_download_artifacts(path: &str) -> io::Result<()> {
    sidecar::delete_download_artifacts_for_path(path).await
}

pub(super) const fn should_reuse_resume_state(
    force_overwrite: bool,
    trust_resume_state: bool,
) -> bool {
    !force_overwrite && trust_resume_state
}

#[must_use]
pub(crate) fn resume_validation_percent(checked_bytes: u64, total_bytes: u64) -> u64 {
    if total_bytes == 0 {
        return 0;
    }
    ((u128::from(checked_bytes.min(total_bytes)) * 100) / u128::from(total_bytes)) as u64
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
        let all_items = collect_download_items(nodes);

        let mut to_download = Vec::new();
        let mut completed = Vec::new();
        let mut skipped = 0;
        let mut partial = 0;

        for item in all_items {
            let local = self.inspect_local_file(&item.path, item.node.size()).await;
            match local.status {
                FileStatus::Complete => {
                    skipped += 1;
                    completed.push(item);
                }
                FileStatus::Partial => {
                    progress.on_partial_detected(
                        &item.path,
                        local.existing_partial_bytes,
                        item.node.size(),
                    );
                    partial += 1;
                    to_download.push(DownloadItem {
                        was_partial: true,
                        ..item
                    });
                }
                FileStatus::Missing => {
                    to_download.push(item);
                }
            }
        }

        CollectedFiles {
            to_download,
            completed,
            skipped,
            partial,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::super::test_support::*;
    use super::super::{NoProgress, part_path};
    use super::*;
    use crate::fake_mega::{FakeMegaFixture, FakeMegaServer, create_fake_mega_fixture};

    #[derive(Default)]
    struct PartialRecordingProgress {
        detected: Mutex<Vec<(String, u64, u64)>>,
    }

    impl DownloadProgress for PartialRecordingProgress {
        fn on_partial_detected(&self, name: &str, existing_size: u64, expected_size: u64) {
            self.detected
                .lock()
                .unwrap()
                .push((name.to_string(), existing_size, expected_size));
        }
    }

    async fn single_file_nodes(
        seed: u64,
    ) -> (
        tempfile::TempDir,
        FakeMegaFixture,
        FakeMegaServer,
        mega::Nodes,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let fixture_dir = temp.path().join("fixture");
        let fixture = create_fake_mega_fixture(&fixture_dir, "payload.bin", 262_219, seed)
            .await
            .unwrap();
        let server = FakeMegaServer::spawn(fixture.clone(), 1).unwrap();
        let client = mega::Client::builder()
            .origin(server.origin().clone())
            .build(reqwest::Client::new())
            .unwrap();
        let nodes = client
            .fetch_public_nodes(&fixture.public_url())
            .await
            .unwrap();
        (temp, fixture, server, nodes)
    }

    #[test]
    fn resume_state_is_reused_only_for_session_tracked_files() {
        assert!(should_reuse_resume_state(false, true));
        assert!(!should_reuse_resume_state(false, false));
        assert!(!should_reuse_resume_state(true, true));
    }

    #[tokio::test]
    async fn collect_files_marks_existing_output_complete() {
        let (_temp, _fixture, server, nodes) = single_file_nodes(51).await;
        let items = collect_download_items(&nodes);
        let path = items[0].path.clone();
        let size = items[0].node.size();
        let fs = MockFileSystem::new();
        fs.add_file(&path, size);
        let downloader = mock_downloader(fs);
        let progress: Arc<dyn DownloadProgress> = Arc::new(NoProgress);

        let collected = downloader.collect_files(&nodes, &progress).await;

        assert_eq!(collected.skipped, 1);
        assert_eq!(collected.partial, 0);
        assert!(collected.to_download.is_empty());
        assert_eq!(collected.completed.len(), 1);
        assert_eq!(collected.completed[0].path, path);

        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn collect_files_marks_existing_partials_and_reports_detected_bytes() {
        let (_temp, _fixture, server, nodes) = single_file_nodes(53).await;
        let items = collect_download_items(&nodes);
        let path = items[0].path.clone();
        let size = items[0].node.size();
        let partial_bytes = size / 3;
        let fs = MockFileSystem::new();
        fs.add_file(part_path(&path), partial_bytes);
        let downloader = mock_downloader(fs);
        let progress_impl = Arc::new(PartialRecordingProgress::default());
        let progress: Arc<dyn DownloadProgress> = progress_impl.clone();

        let collected = downloader.collect_files(&nodes, &progress).await;

        assert_eq!(collected.skipped, 0);
        assert_eq!(collected.partial, 1);
        assert_eq!(collected.to_download.len(), 1);
        assert!(collected.to_download[0].was_partial);
        assert_eq!(collected.to_download[0].path, path);
        assert!(collected.completed.is_empty());
        assert_eq!(
            progress_impl.detected.lock().unwrap().as_slice(),
            &[(path, partial_bytes, size)]
        );

        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn collect_files_force_overwrite_treats_existing_output_as_missing() {
        let (_temp, _fixture, server, nodes) = single_file_nodes(59).await;
        let items = collect_download_items(&nodes);
        let path = items[0].path.clone();
        let size = items[0].node.size();
        let fs = MockFileSystem::new();
        fs.add_file(&path, size);
        let downloader = mock_downloader_force(fs);
        let progress: Arc<dyn DownloadProgress> = Arc::new(NoProgress);

        let collected = downloader.collect_files(&nodes, &progress).await;

        assert_eq!(collected.skipped, 0);
        assert_eq!(collected.partial, 0);
        assert_eq!(collected.to_download.len(), 1);
        assert!(!collected.to_download[0].was_partial);
        assert_eq!(collected.to_download[0].path, path);
        assert!(collected.completed.is_empty());

        server.shutdown().await.unwrap();
    }
}
