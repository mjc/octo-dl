use std::fmt::Write as _;
use std::io::{Read, Seek};
use std::path::Path;
use std::sync::Arc;

use tokio_util::compat::TokioAsyncReadCompatExt;

use crate::error::{Error, Result};
use crate::fs::FileSystem;
use crate::stats::FileStats;

use super::callbacks::DownloadProgress;
use super::downloader::Downloader;
use super::revalidate::{REVALIDATION_BUFFER_BYTES, revalidation_buffer_len};

/// Result of manually checking a completed final file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompletedFileVerify {
    pub bytes: u64,
}

pub(crate) fn expected_mac(node: &mega::Node) -> Result<[u8; 8]> {
    let mac = node
        .condensed_mac()
        .ok_or(mega::Error::MissingCondensedMac)?;
    Ok(*mac)
}

async fn compute_completed_file_mac_from_file(
    final_path: &Path,
    file_size: u64,
    aes_key: &[u8; 16],
    aes_iv: &[u8; 8],
    progress: Option<(&str, &dyn DownloadProgress)>,
) -> Result<[u8; 8]> {
    if file_size == 0 {
        let file = tokio::fs::File::open(final_path).await?;
        return Ok(mega::compute_condensed_mac(file.compat(), file_size, aes_key, aes_iv).await?);
    }

    let mut condensed_mac = mega::MegaCondensedMac::new(aes_key);
    let mut file = std::fs::File::open(final_path)?;
    let mut buffer = [0; REVALIDATION_BUFFER_BYTES];
    for boundary in mega::mega_chunk_boundaries_iter(file_size) {
        let mut mac = mega::MegaChunkMac::new(aes_key, aes_iv);
        let mut offset = boundary.offset;
        let end = boundary.offset.saturating_add(boundary.length);
        file.seek(std::io::SeekFrom::Start(boundary.offset))?;
        while offset < end {
            let read_len = revalidation_buffer_len(end - offset);
            let read_buffer = &mut buffer[..read_len];
            file.read_exact(read_buffer)?;
            mac.update(read_buffer);
            if let Some((name, progress)) = progress {
                progress.on_progress(
                    name,
                    crate::core::ProgressDelta {
                        total_bytes_delta: u64::try_from(read_len).unwrap_or(0),
                        network_bytes_delta: 0,
                    },
                );
            }
            offset = offset.saturating_add(u64::try_from(read_len).unwrap_or(0));
        }
        condensed_mac.update_chunk_mac(&mac.finalize());
    }
    Ok(condensed_mac.finalize())
}

impl<F: FileSystem> Downloader<F> {
    pub(super) async fn complete_existing_file(
        &self,
        node: &mega::Node,
        path: &str,
        progress: &Arc<dyn DownloadProgress>,
    ) -> Result<Option<FileStats>> {
        if self.config.force_overwrite
            || self
                .fs
                .file_size(Path::new(path))
                .await
                .is_none_or(|size| size != node.size())
        {
            return Ok(None);
        }
        match self.verify_completed_file(node, path).await {
            Ok(verified) => {
                let stats = FileStats {
                    size: verified.bytes,
                    network_bytes: 0,
                    reused_bytes: 0,
                    elapsed: std::time::Duration::ZERO,
                    average_speed: 0,
                    peak_speed: 0,
                    ramp_up_time: None,
                };
                progress.on_file_complete(path, &stats);
                Ok(Some(stats))
            }
            Err(error) => {
                log::warn!(
                    "Existing completed file {} failed verification; deleting and redownloading: {error}",
                    path
                );
                self.fs.remove_file(Path::new(path)).await?;
                Ok(None)
            }
        }
    }

    /// Verifies the completed destination file against the remote node MAC.
    ///
    /// # Errors
    ///
    /// Returns an error when the file is missing, has the wrong size, or its
    /// computed MEGA condensed MAC does not match the node.
    pub async fn verify_completed_file(
        &self,
        node: &mega::Node,
        path: &str,
    ) -> Result<CompletedFileVerify> {
        self.verify_completed_file_with_progress(node, path, None)
            .await
    }

    pub async fn verify_completed_file_with_progress(
        &self,
        node: &mega::Node,
        path: &str,
        progress: Option<&dyn DownloadProgress>,
    ) -> Result<CompletedFileVerify> {
        let final_path = Path::new(path);
        let size = self.fs.file_size(final_path).await.ok_or_else(|| {
            let mut message =
                String::with_capacity("Completed file is missing: ".len() + path.len());
            message.push_str("Completed file is missing: ");
            message.push_str(path);
            Error::Download(message)
        })?;
        if size != node.size() {
            let mut message = String::with_capacity(
                "Completed file size mismatch for : local= remote=".len() + path.len() + 40,
            );
            let _ = write!(
                message,
                "Completed file size mismatch for {path}: local={size} remote={}",
                node.size()
            );
            return Err(Error::Download(message));
        }

        let aes_iv = node.aes_iv().ok_or(mega::Error::MissingNodeAesIv)?;
        let expected_mac = *node
            .condensed_mac()
            .ok_or(mega::Error::MissingCondensedMac)?;
        let actual_mac = self
            .compute_completed_file_mac(final_path, node, *aes_iv, progress.map(|p| (path, p)))
            .await?;
        if actual_mac != expected_mac {
            return Err(Error::Mega(mega::Error::CondensedMacMismatch));
        }

        Ok(CompletedFileVerify { bytes: size })
    }

    async fn compute_completed_file_mac(
        &self,
        final_path: &Path,
        node: &mega::Node,
        aes_iv: [u8; 8],
        progress: Option<(&str, &dyn DownloadProgress)>,
    ) -> Result<[u8; 8]> {
        compute_completed_file_mac_from_file(
            final_path,
            node.size(),
            node.aes_key(),
            &aes_iv,
            progress,
        )
        .await
    }
}

#[cfg(test)]
mod tests;
