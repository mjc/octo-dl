//! Core download logic and abstractions.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::{StreamExt, stream};
use tokio_util::compat::TokioAsyncWriteCompatExt;
use tokio_util::sync::CancellationToken;

use crate::config::DownloadConfig;
use crate::error::{Error, Result};
use crate::fs::{FileSystem, TokioFileSystem};
use crate::stats::{DownloadStatsTracker, FileStats, SessionStats, SessionStatsBuilder};

/// Classification of a file's current state on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    /// File exists with the expected size — fully downloaded.
    Complete,
    /// A `.part` file exists (partial download from a previous run).
    Partial,
    /// Neither the final file nor a `.part` file exists.
    Missing,
}

/// Trait for receiving download progress updates.
///
/// Implement this trait to receive callbacks during download operations.
/// All methods have default no-op implementations for convenience.
pub trait DownloadProgress: Send + Sync {
    /// Called when a file download starts.
    fn on_file_start(&self, _name: &str, _size: u64) {}

    /// Called periodically with the number of bytes downloaded since the last call.
    fn on_progress(&self, _name: &str, _bytes_delta: u64, _speed: u64) {}

    /// Called when a file download completes successfully.
    fn on_file_complete(&self, _name: &str, _stats: &FileStats) {}

    /// Called when a file download fails.
    fn on_error(&self, _name: &str, _error: &str) {}

    /// Called when a partial `.part` file is detected from a previous run.
    fn on_partial_detected(&self, _name: &str, _existing_size: u64, _expected_size: u64) {}
}

/// A null progress implementation that ignores all events.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoProgress;

impl DownloadProgress for NoProgress {}

/// A file to be downloaded with its destination path.
pub struct DownloadItem<'a> {
    /// Local file path where the file will be saved.
    pub path: String,
    /// Reference to the MEGA node to download.
    pub node: &'a mega::Node,
}

/// Result of collecting files from nodes.
pub struct CollectedFiles<'a> {
    /// Files that need to be downloaded.
    pub to_download: Vec<DownloadItem<'a>>,
    /// Number of files skipped (already exist with correct size).
    pub skipped: usize,
    /// Number of files with partial `.part` downloads detected.
    pub partial: usize,
}

impl CollectedFiles<'_> {
    /// Returns the total size of files to download in bytes.
    #[must_use]
    pub fn total_size(&self) -> u64 {
        self.to_download.iter().map(|i| i.node.size()).sum()
    }

    /// Returns true if there are no files to download.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.to_download.is_empty()
    }

    /// Converts borrowed download items into owned items by cloning the nodes.
    ///
    /// This is useful when the items need to be sent to a `tokio::spawn`'d task,
    /// which requires `'static` data.
    #[must_use]
    pub fn into_owned(self) -> Vec<OwnedDownloadItem> {
        self.to_download
            .into_iter()
            .map(|item| OwnedDownloadItem {
                path: item.path,
                node: item.node.clone(),
            })
            .collect()
    }
}

/// A file to be downloaded with an owned node (no lifetime parameter).
///
/// Use this instead of [`DownloadItem`] when the items need to cross
/// `tokio::spawn` boundaries (which require `'static` data).
pub struct OwnedDownloadItem {
    /// Local file path where the file will be saved.
    pub path: String,
    /// Owned copy of the MEGA node to download.
    pub node: mega::Node,
}

/// Returns the `.part` file path for a given final path.
fn part_path(path: &str) -> PathBuf {
    PathBuf::from(format!("{path}.part"))
}

/// Core downloader that handles MEGA file downloads.
pub struct Downloader<F: FileSystem = TokioFileSystem> {
    client: mega::Client,
    config: DownloadConfig,
    fs: F,
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
    async fn classify_file(&self, path: &str, expected_size: u64) -> FileStatus {
        if self.config.force_overwrite {
            return FileStatus::Missing;
        }
        // Check final file first
        if self
            .fs
            .file_size(Path::new(path))
            .await
            .is_some_and(|size| size == expected_size)
        {
            return FileStatus::Complete;
        }
        // Check for .part file
        let pp = part_path(path);
        if self.fs.file_exists(&pp).await {
            return FileStatus::Partial;
        }
        FileStatus::Missing
    }

    /// Collects files from nodes, checking which need to be downloaded.
    pub async fn collect_files<'a>(
        &self,
        nodes: &'a mega::Nodes,
        progress: &Arc<dyn DownloadProgress>,
    ) -> CollectedFiles<'a> {
        let all_items: Vec<_> = nodes
            .roots()
            .flat_map(|root| {
                if root.kind().is_folder() {
                    collect_files_recursive(nodes, root)
                } else {
                    vec![DownloadItem {
                        path: root.name().to_string(),
                        node: root,
                    }]
                }
            })
            .collect();

        let mut to_download = Vec::new();
        let mut skipped = 0;
        let mut partial = 0;

        for item in all_items {
            match self.classify_file(&item.path, item.node.size()).await {
                FileStatus::Complete => skipped += 1,
                FileStatus::Partial => {
                    let pp = part_path(&item.path);
                    let existing_size = self.fs.file_size(&pp).await.unwrap_or(0);
                    progress.on_partial_detected(item.node.name(), existing_size, item.node.size());
                    partial += 1;
                    to_download.push(item);
                }
                FileStatus::Missing => {
                    to_download.push(item);
                }
            }
        }

        CollectedFiles {
            to_download,
            skipped,
            partial,
        }
    }

    /// Ensures the parent directory exists for a file path.
    async fn ensure_parent_dir(&self, path: &str) -> Result<()> {
        if let Some(parent) = Path::new(path)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
        {
            self.fs.create_dir_all(parent).await?;
        }
        Ok(())
    }

    /// Downloads a single file using atomic `.part` file semantics.
    ///
    /// Writes to `{path}.part` during download, then renames to `{path}` on success.
    /// On error, cleans up the `.part` file if `cleanup_on_error` is enabled.
    /// If a `cancellation_token` is provided, the download can be cancelled.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be created or the download fails.
    pub async fn download_file(
        &self,
        node: &mega::Node,
        path: &str,
        progress: &Arc<dyn DownloadProgress>,
        cancellation_token: Option<CancellationToken>,
    ) -> Result<FileStats> {
        self.ensure_parent_dir(path).await?;

        let pp = part_path(path);
        let stats = Arc::new(DownloadStatsTracker::new(node.size()));
        let name = node.name().to_string();

        progress.on_file_start(&name, node.size());

        // Create .part file with pre-allocated size
        let file = self.fs.create_file(&pp, node.size()).await?;

        let name_clone = name.clone();

        // Wrap tokio file for futures::AsyncWrite/AsyncSeek compatibility
        let file = file.compat_write();

        // The mega library calls the progress callback with the *cumulative*
        // total bytes downloaded so far, NOT a delta.  We use fetch_max (not
        // swap) so that out-of-order callbacks from parallel workers never
        // regress the high-water mark.
        let prev_bytes = Arc::new(AtomicU64::new(0));
        let stats_clone = Arc::clone(&stats);
        let progress_clone = Arc::clone(progress);
        let name_for_cb = name.clone();
        let progress_cb = move |cumulative: u64| {
            let previous = prev_bytes.fetch_max(cumulative, Ordering::Relaxed);
            let delta = cumulative.saturating_sub(previous);
            if delta > 0 {
                let speed = stats_clone.record_bytes(delta);
                progress_clone.on_progress(&name_for_cb, delta, speed);
            }
        };

        // Download with progress callback, optionally with cancellation support
        let download_result = if let Some(token) = cancellation_token {
            let download_fut = self.client.download_node_parallel_with_progress(
                node,
                file,
                self.config.chunks_per_file,
                Some(progress_cb),
            );
            tokio::select! {
                res = download_fut => res.map_err(Error::Mega),
                () = token.cancelled() => {
                    Err(Error::Cancelled)
                }
            }
        } else {
            self.client
                .download_node_parallel_with_progress(
                    node,
                    file,
                    self.config.chunks_per_file,
                    Some(progress_cb),
                )
                .await
                .map_err(Error::Mega)
        };

        match download_result {
            Ok(()) => {
                // Rename .part → final
                self.fs.rename_file(&pp, Path::new(path)).await?;

                let file_stats = FileStats {
                    size: node.size(),
                    elapsed: stats.elapsed(),
                    average_speed: stats.average_speed(),
                    peak_speed: stats.peak_speed(),
                    ramp_up_time: stats.time_to_80pct(),
                };
                progress.on_file_complete(&name_clone, &file_stats);
                Ok(file_stats)
            }
            Err(e) => {
                // Clean up .part file on error/cancellation only if configured
                if self.config.cleanup_on_error {
                    let _ = self.fs.remove_file(&pp).await;
                }
                if !matches!(e, Error::Cancelled) {
                    progress.on_error(&name_clone, &e.to_string());
                }
                Err(e)
            }
        }
    }

    /// Downloads all collected files with concurrent downloads.
    ///
    /// Returns session statistics on completion.
    ///
    /// # Errors
    ///
    /// Individual file download errors are logged but do not cause the
    /// entire operation to fail. The returned stats will reflect which
    /// files succeeded.
    pub async fn download_all(
        &self,
        files: &[DownloadItem<'_>],
        progress: &Arc<dyn DownloadProgress>,
        skipped_count: usize,
    ) -> Result<SessionStats> {
        let mut builder = SessionStatsBuilder::new();
        builder.set_skipped(skipped_count);

        if files.is_empty() {
            return Ok(builder.build());
        }

        let peak_speed = Arc::new(AtomicU64::new(0));

        let results: Vec<_> = stream::iter(files)
            .map(|item| {
                let peak_tracker = Arc::clone(&peak_speed);
                async move {
                    let result = self
                        .download_file(item.node, &item.path, progress, None)
                        .await;
                    if let Ok(ref stats) = result {
                        peak_tracker.fetch_max(stats.peak_speed, Ordering::Relaxed);
                    }
                    result
                }
            })
            .buffer_unordered(self.config.concurrent_files)
            .collect()
            .await;

        builder.set_peak_speed(peak_speed.load(Ordering::Relaxed));

        for result in results {
            match result {
                Ok(file_stats) => builder.add_download(&file_stats),
                Err(e) => {
                    log::error!("Download failed: {e}");
                }
            }
        }

        Ok(builder.build())
    }

    /// Downloads all owned items with concurrent downloads.
    ///
    /// This is the same as [`download_all`](Self::download_all) but takes
    /// [`OwnedDownloadItem`] values, making it safe to call from inside
    /// `tokio::spawn` (which requires `'static` futures).
    ///
    /// # Errors
    ///
    /// Individual file download errors are logged but do not cause the
    /// entire operation to fail. The returned stats will reflect which
    /// files succeeded.
    pub async fn download_all_owned(
        &self,
        files: &[OwnedDownloadItem],
        progress: &Arc<dyn DownloadProgress>,
        skipped_count: usize,
    ) -> Result<SessionStats> {
        let mut builder = SessionStatsBuilder::new();
        builder.set_skipped(skipped_count);

        if files.is_empty() {
            return Ok(builder.build());
        }

        let peak_speed = Arc::new(AtomicU64::new(0));

        let results: Vec<_> = stream::iter(files)
            .map(|item| {
                let peak_tracker = Arc::clone(&peak_speed);
                async move {
                    let result = self
                        .download_file(&item.node, &item.path, progress, None)
                        .await;
                    if let Ok(ref stats) = result {
                        peak_tracker.fetch_max(stats.peak_speed, Ordering::Relaxed);
                    }
                    result
                }
            })
            .buffer_unordered(self.config.concurrent_files)
            .collect()
            .await;

        builder.set_peak_speed(peak_speed.load(Ordering::Relaxed));

        for result in results {
            match result {
                Ok(file_stats) => builder.add_download(&file_stats),
                Err(e) => {
                    log::error!("Download failed: {e}");
                }
            }
        }

        Ok(builder.build())
    }
}

/// Recursively collects files from a folder node.
fn collect_files_recursive<'a>(
    nodes: &'a mega::Nodes,
    node: &'a mega::Node,
) -> Vec<DownloadItem<'a>> {
    let (folders, files): (Vec<_>, Vec<_>) = node
        .children()
        .iter()
        .filter_map(|hash| nodes.get_node_by_handle(hash))
        .partition(|n| n.kind().is_folder());

    let current_files = files.into_iter().map(|file| DownloadItem {
        path: build_path(nodes, node, file),
        node: file,
    });

    let nested_files = folders
        .into_iter()
        .flat_map(|folder| collect_files_recursive(nodes, folder));

    current_files.chain(nested_files).collect()
}

/// Builds the full path for a file within a folder structure.
fn build_path(nodes: &mega::Nodes, parent: &mega::Node, file: &mega::Node) -> String {
    // Try to build full path with grandparent, fallback to parent/file if no grandparent
    if let Some(gp_handle) = parent.parent()
        && let Some(grandparent) = nodes.get_node_by_handle(gp_handle)
    {
        return format!("{}/{}/{}", grandparent.name(), parent.name(), file.name());
    }
    // Parent is at root level, just use parent/file
    format!("{}/{}", parent.name(), file.name())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_progress_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<NoProgress>();
    }

    #[test]
    fn collected_files_total_size() {
        let collected = CollectedFiles {
            to_download: vec![],
            skipped: 5,
            partial: 0,
        };
        assert_eq!(collected.total_size(), 0);
        assert!(collected.is_empty());
    }

    #[test]
    fn part_path_appends_extension() {
        assert_eq!(part_path("foo/bar.zip"), PathBuf::from("foo/bar.zip.part"));
        assert_eq!(part_path("file.txt"), PathBuf::from("file.txt.part"));
    }

    #[test]
    fn file_status_variants() {
        assert_ne!(FileStatus::Complete, FileStatus::Partial);
        assert_ne!(FileStatus::Partial, FileStatus::Missing);
        assert_ne!(FileStatus::Complete, FileStatus::Missing);
    }

    // =========================================================================
    // Mock-based classify_file tests
    // =========================================================================

    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A mock file system for testing `classify_file` behavior.
    struct MockFileSystem {
        /// Maps path → file size (if the file exists).
        files: Mutex<HashMap<PathBuf, u64>>,
    }

    impl MockFileSystem {
        fn new() -> Self {
            Self {
                files: Mutex::new(HashMap::new()),
            }
        }

        fn add_file(&self, path: impl Into<PathBuf>, size: u64) {
            self.files.lock().unwrap().insert(path.into(), size);
        }
    }

    #[async_trait::async_trait]
    impl crate::fs::FileSystem for MockFileSystem {
        async fn file_exists(&self, path: &Path) -> bool {
            self.files.lock().unwrap().contains_key(path)
        }

        async fn file_size(&self, path: &Path) -> Option<u64> {
            self.files.lock().unwrap().get(path).copied()
        }

        async fn create_dir_all(&self, _path: &Path) -> std::io::Result<()> {
            Ok(())
        }

        async fn create_file(&self, _path: &Path, _size: u64) -> std::io::Result<tokio::fs::File> {
            // Not needed for classify_file tests
            Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "mock"))
        }

        async fn rename_file(&self, _from: &Path, _to: &Path) -> std::io::Result<()> {
            Ok(())
        }

        async fn remove_file(&self, _path: &Path) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn mock_downloader(fs: MockFileSystem) -> Downloader<MockFileSystem> {
        let http = reqwest::Client::new();
        let client = mega::Client::builder().build(http).unwrap();
        Downloader::with_fs(client, DownloadConfig::default(), fs)
    }

    fn mock_downloader_force(fs: MockFileSystem) -> Downloader<MockFileSystem> {
        let http = reqwest::Client::new();
        let client = mega::Client::builder().build(http).unwrap();
        let config = DownloadConfig {
            force_overwrite: true,
            ..DownloadConfig::default()
        };
        Downloader::with_fs(client, config, fs)
    }

    #[tokio::test]
    async fn classify_file_complete() {
        let fs = MockFileSystem::new();
        fs.add_file("movie.mkv", 1_000_000);
        let dl = mock_downloader(fs);
        assert_eq!(
            dl.classify_file("movie.mkv", 1_000_000).await,
            FileStatus::Complete
        );
    }

    #[tokio::test]
    async fn classify_file_size_mismatch_checks_part() {
        let fs = MockFileSystem::new();
        // File exists but wrong size, no .part file
        fs.add_file("movie.mkv", 500);
        let dl = mock_downloader(fs);
        assert_eq!(
            dl.classify_file("movie.mkv", 1_000_000).await,
            FileStatus::Missing
        );
    }

    #[tokio::test]
    async fn classify_file_partial() {
        let fs = MockFileSystem::new();
        // No final file, but .part file exists
        fs.add_file("movie.mkv.part", 500_000);
        let dl = mock_downloader(fs);
        assert_eq!(
            dl.classify_file("movie.mkv", 1_000_000).await,
            FileStatus::Partial
        );
    }

    #[tokio::test]
    async fn classify_file_missing() {
        let fs = MockFileSystem::new();
        let dl = mock_downloader(fs);
        assert_eq!(
            dl.classify_file("movie.mkv", 1_000_000).await,
            FileStatus::Missing
        );
    }

    #[tokio::test]
    async fn classify_file_force_overwrite() {
        let fs = MockFileSystem::new();
        // File exists with correct size, but force_overwrite is on
        fs.add_file("movie.mkv", 1_000_000);
        let dl = mock_downloader_force(fs);
        assert_eq!(
            dl.classify_file("movie.mkv", 1_000_000).await,
            FileStatus::Missing
        );
    }
}
