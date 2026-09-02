use std::path::Path;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::fs::FileSystem;
use crate::stats::FileStats;

use super::callbacks::DownloadProgress;
use super::downloader::Downloader;
use super::finalize::DownloadFinishContext;
use super::sidecar::{part_path, sidecar_path};

impl<F: FileSystem> Downloader<F> {
    /// Ensures the parent directory exists for a file path.
    async fn ensure_parent_dir(&self, path: &str) -> crate::error::Result<()> {
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
    /// On recoverable errors, keeps the `.part` file for resume unless
    /// `cleanup_on_error` is enabled. Final MAC mismatches always discard
    /// resumable state because the assembled plaintext failed verification.
    /// If a `cancellation_token` is provided, the download can be cancelled.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be created or the download fails.
    ///
    /// # Panics
    ///
    /// Panics if the internal resume tracker mutex is poisoned.
    #[allow(clippy::too_many_lines)]
    pub async fn download_file(
        &self,
        node: &mega::Node,
        path: &str,
        progress: &Arc<dyn DownloadProgress>,
        trust_resume_state: bool,
        cancellation_token: Option<CancellationToken>,
    ) -> crate::error::Result<FileStats> {
        if let Some(stats) = self.complete_existing_file(node, path, progress).await? {
            return Ok(stats);
        }

        self.ensure_parent_dir(path).await?;
        progress.on_file_start(path, node.size());

        let pp = part_path(path);
        let sp = sidecar_path(path);
        let prepared = self
            .prepare_transfer_resume(
                node,
                path,
                progress,
                trust_resume_state,
                &pp,
                &sp,
                cancellation_token.as_ref(),
            )
            .await?;

        let file = self
            .fs
            .open_part_file(&pp, node.size(), prepared.preserve_existing)
            .await?;
        let callbacks: Arc<dyn mega::ParallelDownloadCallbacks> = prepared.callback_state.clone();

        let download_result = if let Some(token) = cancellation_token {
            let download_fut = self
                .client
                .download_node_parallel_resumable_to_file_with_callbacks(
                    node,
                    file,
                    self.config.chunks_per_file,
                    Some(self.config.mega_chunks_per_request),
                    Arc::clone(&prepared.trusted_for_download),
                    Some(callbacks),
                );
            tokio::select! {
                res = download_fut => res.map_err(crate::error::Error::Mega),
                () = token.cancelled() => {
                    Err(crate::error::Error::Cancelled)
                }
            }
        } else {
            self.client
                .download_node_parallel_resumable_to_file_with_callbacks(
                    node,
                    file,
                    self.config.chunks_per_file,
                    Some(self.config.mega_chunks_per_request),
                    prepared.trusted_for_download,
                    Some(callbacks),
                )
                .await
                .map_err(crate::error::Error::Mega)
        };
        let result = self
            .finish_download_result(
                DownloadFinishContext {
                    node,
                    path,
                    part_path: &pp,
                    sidecar_path: &sp,
                    reused_bytes: prepared.trusted_bytes,
                    stats: &prepared.callback_state.progress.stats,
                    chunk_verified: &prepared.callback_state.chunk_verified,
                    progress,
                    name: path,
                },
                download_result,
            )
            .await;
        super::trim_allocator();
        result
    }
}

#[cfg(test)]
mod tests;
