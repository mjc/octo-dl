use std::sync::Arc;

use futures::{StreamExt, stream};

use crate::error::Result;
use crate::fs::FileSystem;
use crate::stats::{SessionStats, SessionStatsBuilder};

use super::callbacks::DownloadProgress;
use super::collect::{DownloadItem, OwnedDownloadItem};
use super::downloader::Downloader;

impl<F: FileSystem> Downloader<F> {
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

        let mut peak_speed = 0;
        let mut downloads = stream::iter(files)
            .map(|item| async move {
                self.download_file(item.node, &item.path, progress, false, None)
                    .await
            })
            .buffer_unordered(self.config.concurrent_files);

        while let Some(result) = downloads.next().await {
            match result {
                Ok(file_stats) => {
                    peak_speed = peak_speed.max(file_stats.peak_speed);
                    builder.add_download(&file_stats);
                }
                Err(e) => {
                    log::error!("Download failed: {e}");
                }
            }
        }
        builder.set_peak_speed(peak_speed);

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

        let mut peak_speed = 0;
        let mut downloads = stream::iter(files)
            .map(|item| async move {
                self.download_file(&item.node, &item.path, progress, false, None)
                    .await
            })
            .buffer_unordered(self.config.concurrent_files);

        while let Some(result) = downloads.next().await {
            match result {
                Ok(file_stats) => {
                    peak_speed = peak_speed.max(file_stats.peak_speed);
                    builder.add_download(&file_stats);
                }
                Err(e) => {
                    log::error!("Download failed: {e}");
                }
            }
        }
        builder.set_peak_speed(peak_speed);

        Ok(builder.build())
    }
}

#[cfg(test)]
mod tests;
