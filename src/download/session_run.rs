use std::sync::Arc;

use futures::{StreamExt, stream};

use crate::error::Result;
use crate::fs::FileSystem;
use crate::stats::{SessionStats, SessionStatsBuilder};

use super::callbacks::DownloadProgress;
use super::collect::{DownloadItem, OwnedDownloadItem};
use super::downloader::Downloader;

pub(super) trait SessionDownloadItem {
    fn node(&self) -> &mega::Node;
    fn path(&self) -> &str;
}

impl SessionDownloadItem for DownloadItem<'_> {
    fn node(&self) -> &mega::Node {
        self.node
    }

    fn path(&self) -> &str {
        &self.path
    }
}

impl SessionDownloadItem for OwnedDownloadItem {
    fn node(&self) -> &mega::Node {
        &self.node
    }

    fn path(&self) -> &str {
        &self.path
    }
}

pub(super) async fn download_all_items<T, F>(
    downloader: &Downloader<F>,
    files: &[T],
    progress: &Arc<dyn DownloadProgress>,
    skipped_count: usize,
) -> Result<SessionStats>
where
    T: SessionDownloadItem,
    F: FileSystem,
{
    let mut builder = SessionStatsBuilder::new();
    builder.set_skipped(skipped_count);

    if files.is_empty() {
        return Ok(builder.build());
    }

    let mut peak_speed = 0;
    let mut downloads = stream::iter(files.iter())
        .map(|item| async move {
            downloader
                .download_file(item.node(), item.path(), progress, false, None)
                .await
        })
        .buffer_unordered(downloader.config.concurrent_files);

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
