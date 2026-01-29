//! Core download logic and abstractions.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::{StreamExt, stream};

use crate::config::DownloadConfig;
use crate::error::{Error, Result};
use crate::fs::{FileSystem, TokioFileSystem};
use crate::stats::{DownloadStatsTracker, FileStats, SessionStats, SessionStatsBuilder};

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

    /// Collects files from nodes, checking which need to be downloaded.
    pub async fn collect_files<'a>(&self, nodes: &'a mega::Nodes) -> CollectedFiles<'a> {
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

        for item in all_items {
            if self.should_skip(&item.path, item.node.size()).await {
                skipped += 1;
            } else {
                to_download.push(item);
            }
        }

        CollectedFiles {
            to_download,
            skipped,
        }
    }

    /// Checks if a file should be skipped based on existence and size.
    async fn should_skip(&self, path: &str, expected_size: u64) -> bool {
        if self.config.force_overwrite {
            return false;
        }
        self.fs
            .file_size(Path::new(path))
            .await
            .is_some_and(|size| size == expected_size)
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

    /// Downloads a single file.
    ///
    /// Returns statistics about the download on success.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be created or the download fails.
    pub async fn download_file(
        &self,
        node: &mega::Node,
        path: &str,
        progress: &dyn DownloadProgress,
    ) -> Result<FileStats> {
        self.ensure_parent_dir(path).await?;

        let stats = Arc::new(DownloadStatsTracker::new(node.size()));
        let name = node.name().to_string();

        progress.on_file_start(&name, node.size());

        // Create file with pre-allocated size
        let file = self.fs.create_file(Path::new(path), node.size()).await?;

        let name_clone = name.clone();
        let stats_clone = Arc::clone(&stats);

        // Download with progress callback
        let result = self
            .client
            .download_node_parallel(
                node,
                file,
                self.config.chunks_per_file,
                Some(move |delta| {
                    stats_clone.update_speed(delta * 4); // Rough speed estimate
                    // Note: actual speed tracking happens in the CLI via indicatif
                }),
            )
            .await;

        match result {
            Ok(()) => {
                // We need to reconstruct stats since we can't easily get indicatif's per_sec
                // The CLI will handle detailed progress tracking
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
                progress.on_error(&name_clone, &e.to_string());
                Err(Error::Mega(e))
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
        progress: &dyn DownloadProgress,
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
                    let result = self.download_file(item.node, &item.path, progress).await;
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
                    // Log error but continue with other files
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
        // Can't easily test without mock nodes, but we can test the empty case
        let collected = CollectedFiles {
            to_download: vec![],
            skipped: 5,
        };
        assert_eq!(collected.total_size(), 0);
        assert!(collected.is_empty());
    }
}
