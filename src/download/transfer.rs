use std::path::Path;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::fs::FileSystem;
use crate::stats::FileStats;

use super::*;

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
        let expected_condensed_mac = expected_mac(node)?;
        let boundaries = mega::mega_chunk_boundaries(node.size());
        log::debug!(
            "Download resume setup for {path}: size={} trust_resume_state={} force_overwrite={} part={} sidecar={} chunks={}",
            node.size(),
            trust_resume_state,
            self.config.force_overwrite,
            pp.display(),
            sp.display(),
            boundaries.len()
        );
        let reuse_resume_state =
            should_reuse_resume_state(self.config.force_overwrite, trust_resume_state);
        let resume_validation = if reuse_resume_state {
            let resume_status_progress = ResumeValidationStatusProgress::new(progress.as_ref());
            self.revalidate_resume_chunks(
                node,
                &boundaries,
                &pp,
                &sp,
                expected_condensed_mac,
                Some((path, &resume_status_progress)),
                cancellation_token.as_ref(),
            )
            .await?
        } else {
            ResumeValidation::empty(boundaries.len())
        };
        log::debug!(
            "Download resume validation for {path}: sidecar_loaded={} trusted_chunks={} trusted_bytes={} source={:?}",
            resume_validation.sidecar_loaded,
            resume_validation.trusted_count,
            resume_validation.trusted_bytes,
            resume_validation.source
        );
        let preserve_existing = resume_validation.trusted_count > 0;
        if !preserve_existing {
            let _ = delete_sidecar(&sp).await;
        }
        if resume_validation.sidecar_loaded && resume_validation.trusted_count == 0 {
            log::debug!("Resume sidecar found for {path}, but no chunks were reusable");
        }
        let trusted_bytes = resume_validation.trusted_bytes;

        if trusted_bytes > 0 {
            progress.on_resume_reused(path, resume_validation.trusted_count, trusted_bytes);
        }

        let file = self
            .fs
            .open_part_file(&pp, node.size(), preserve_existing)
            .await?;

        let trusted_for_download: Arc<[Option<[u8; 16]>]> =
            resume_validation.trusted_chunks.clone().into();
        let callback_state = Arc::new(DownloadCallbackState::new(
            ProgressCallbackState::new(
                path.to_string(),
                node.size().saturating_sub(trusted_bytes),
                trusted_bytes,
                Arc::clone(progress),
            ),
            ChunkVerifiedState::new(
                ResumeTracker::new(
                    node.size(),
                    expected_condensed_mac,
                    resume_validation.trusted_chunks,
                ),
                LazySidecarWriter::new(sp.clone(), pp.clone()),
            ),
        ));
        let callbacks: Arc<dyn mega::ParallelDownloadCallbacks> = callback_state.clone();

        let download_result = if let Some(token) = cancellation_token {
            let download_fut = self
                .client
                .download_node_parallel_resumable_to_file_with_callbacks(
                    node,
                    file,
                    self.config.chunks_per_file,
                    Some(self.config.mega_chunks_per_request),
                    Arc::clone(&trusted_for_download),
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
                    trusted_for_download,
                    Some(callbacks),
                )
                .await
                .map_err(crate::error::Error::Mega)
        };
        self.finish_download_result(
            DownloadFinishContext {
                node,
                path,
                part_path: &pp,
                sidecar_path: &sp,
                reused_bytes: trusted_bytes,
                stats: &callback_state.progress.stats,
                chunk_verified: &callback_state.chunk_verified,
                progress,
                name: path,
            },
            download_result,
        )
        .await
    }
}

#[cfg(test)]
mod tests;
