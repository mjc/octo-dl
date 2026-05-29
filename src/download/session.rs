use std::sync::Arc;

use crate::error::Result;
use crate::fs::FileSystem;
use crate::stats::SessionStats;

use super::callbacks::DownloadProgress;
use super::collect::{DownloadItem, OwnedDownloadItem};
use super::downloader::Downloader;
use super::session_run::download_all_items;

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
        download_all_items(self, files, progress, skipped_count).await
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
        download_all_items(self, files, progress, skipped_count).await
    }
}

#[cfg(test)]
mod tests;
