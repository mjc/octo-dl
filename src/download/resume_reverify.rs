use std::path::Path;

use crate::error::Result;
use crate::fs::FileSystem;

use super::callbacks::DownloadProgress;
use super::downloader::Downloader;
use super::resume_state::ResumeReverify;
use super::resume_tracker::ResumeTracker;
use super::resume_validation::ResumeValidation;
use super::sidecar::{part_path, sidecar_path};
use super::sidecar_store::save_sidecar_atomic;
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

async fn persist_revalidated_sidecar<F: FileSystem>(
    fs: &F,
    sidecar_path: &Path,
    part_path: &Path,
    file_size: u64,
    expected_condensed_mac: [u8; 8],
    validation: &ResumeValidation,
) -> Result<()> {
    let mut snapshot = ResumeTracker::new(
        file_size,
        expected_condensed_mac,
        validation.trusted_chunks.clone(),
    )
    .snapshot();
    snapshot.part_fingerprint = fs.file_fingerprint(part_path).await;
    save_sidecar_atomic(sidecar_path, &snapshot).await?;
    Ok(())
}

#[cfg(test)]
mod tests;
