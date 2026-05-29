use std::path::Path;
use std::sync::Arc;

use crate::config::DownloadConfig;
use crate::error::{Error, Result};
use crate::fs::FileSystem;
use crate::stats::{DownloadStatsTracker, FileStats};

use super::callbacks::ChunkVerifiedState;
use super::callbacks::DownloadProgress;
use super::downloader::Downloader;
use super::sidecar::delete_sidecar;
use super::sidecar_writer::SidecarWriterShutdown;

pub(super) struct DownloadFinishContext<'a> {
    pub(super) node: &'a mega::Node,
    pub(super) path: &'a str,
    pub(super) part_path: &'a Path,
    pub(super) sidecar_path: &'a Path,
    pub(super) reused_bytes: u64,
    pub(super) stats: &'a DownloadStatsTracker,
    pub(super) chunk_verified: &'a ChunkVerifiedState,
    pub(super) progress: &'a Arc<dyn DownloadProgress>,
    pub(super) name: &'a str,
}

const fn is_condensed_mac_mismatch(error: &Error) -> bool {
    matches!(error, Error::Mega(mega::Error::CondensedMacMismatch))
}

pub(super) const fn should_delete_resume_state_on_error(
    config: &DownloadConfig,
    error: &Error,
) -> bool {
    is_condensed_mac_mismatch(error)
        || (config.cleanup_on_error && !matches!(error, Error::Cancelled))
}

impl<F: FileSystem> Downloader<F> {
    pub(super) async fn finish_download_result(
        &self,
        ctx: DownloadFinishContext<'_>,
        download_result: Result<()>,
    ) -> Result<FileStats> {
        match download_result {
            Ok(()) => {
                ctx.chunk_verified
                    .finish_sidecar_writer(SidecarWriterShutdown::Abort)
                    .await;
                if self.config.force_overwrite {
                    let _ = self.fs.remove_file(Path::new(ctx.path)).await;
                }
                self.fs
                    .rename_file(ctx.part_path, Path::new(ctx.path))
                    .await?;
                delete_sidecar(ctx.sidecar_path).await?;

                let file_stats = FileStats {
                    size: ctx.node.size(),
                    network_bytes: ctx.stats.downloaded_bytes(),
                    reused_bytes: ctx.reused_bytes,
                    elapsed: ctx.stats.elapsed(),
                    average_speed: ctx.stats.average_speed(),
                    peak_speed: ctx.stats.peak_speed(),
                    ramp_up_time: ctx.stats.time_to_80pct(),
                };
                ctx.progress.on_file_complete(ctx.name, &file_stats);
                Ok(file_stats)
            }
            Err(e) => {
                if should_delete_resume_state_on_error(&self.config, &e) {
                    ctx.chunk_verified
                        .finish_sidecar_writer(SidecarWriterShutdown::Abort)
                        .await;
                    let _ = self.fs.remove_file(ctx.part_path).await;
                    let _ = delete_sidecar(ctx.sidecar_path).await;
                } else {
                    match self.fs.sync_file(ctx.part_path).await {
                        Ok(()) => {
                            ctx.chunk_verified
                                .finish_sidecar_writer(SidecarWriterShutdown::Flush)
                                .await;
                        }
                        Err(sync_err) => {
                            ctx.chunk_verified
                                .finish_sidecar_writer(SidecarWriterShutdown::Abort)
                                .await;
                            log::warn!(
                                "Failed to sync partial file {} before saving resume sidecar: {sync_err}",
                                ctx.part_path.display()
                            );
                        }
                    }
                }
                if !matches!(e, Error::Cancelled) {
                    ctx.progress.on_error(ctx.name, &e.to_string());
                }
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::config::DownloadConfig;
    use crate::error::Error;

    use super::should_delete_resume_state_on_error;

    #[test]
    fn cleanup_policy_preserves_recoverable_errors_by_default() {
        let config = DownloadConfig::default();

        assert!(!should_delete_resume_state_on_error(
            &config,
            &Error::Download("temporary network failure".to_string()),
        ));
        assert!(!should_delete_resume_state_on_error(
            &config,
            &Error::Cancelled,
        ));
        assert!(should_delete_resume_state_on_error(
            &config,
            &Error::Mega(mega::Error::CondensedMacMismatch),
        ));
    }

    #[test]
    fn cleanup_policy_honors_explicit_cleanup_except_cancel() {
        let config = DownloadConfig {
            cleanup_on_error: true,
            ..DownloadConfig::default()
        };

        assert!(should_delete_resume_state_on_error(
            &config,
            &Error::Download("temporary network failure".to_string()),
        ));
        assert!(!should_delete_resume_state_on_error(
            &config,
            &Error::Cancelled,
        ));
    }
}
