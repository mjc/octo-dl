use crate::error::Result;
use crate::fs::FileSystem;

use super::callbacks::DownloadProgress;
use super::downloader::Downloader;
use super::resume_state::ResumeReverify;
use super::sidecar::{part_path, sidecar_path};
use super::sidecar_state::persist_revalidated_sidecar;
use super::verify::expected_mac;

impl<F: FileSystem> Downloader<F> {
    /// Revalidates resumable chunk state for a file without downloading new data.
    ///
    /// # Errors
    ///
    /// Returns an error when the remote node is missing required MAC metadata.
    pub async fn reverify_resume_file(
        &self,
        node: &mega::Node,
        path: &str,
    ) -> Result<ResumeReverify> {
        self.reverify_resume_file_with_progress(node, path, None)
            .await
    }

    pub async fn reverify_resume_file_with_progress(
        &self,
        node: &mega::Node,
        path: &str,
        progress: Option<&dyn DownloadProgress>,
    ) -> Result<ResumeReverify> {
        let part_path = part_path(path);
        let sidecar_path = sidecar_path(path);
        let expected_condensed_mac = expected_mac(node)?;
        let boundaries = mega::mega_chunk_boundaries(node.size());
        let validation = self
            .revalidate_resume_chunks(
                node,
                &boundaries,
                &part_path,
                &sidecar_path,
                expected_condensed_mac,
                progress.map(|progress| (path, progress)),
                None,
            )
            .await?;
        if validation.sidecar_loaded {
            persist_revalidated_sidecar(
                &self.fs,
                &sidecar_path,
                &part_path,
                node.size(),
                expected_condensed_mac,
                &validation,
            )
            .await?;
        }
        Ok(ResumeReverify {
            sidecar_loaded: validation.sidecar_loaded,
            chunks: validation.trusted_count,
            bytes: validation.trusted_bytes,
        })
    }
}

#[cfg(test)]
mod tests;
