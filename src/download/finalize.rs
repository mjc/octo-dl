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
    use std::sync::Arc;

    use crate::config::DownloadConfig;
    use crate::error::Error;
    use crate::stats::DownloadStatsTracker;

    use super::{DownloadFinishContext, should_delete_resume_state_on_error};

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

    #[test]
    fn force_overwrite_keeps_destination_when_replacement_fails() {
        super::super::test_support::run_with_large_stack_current_thread_runtime(
            "force-overwrite-finalize-test",
            || async {
                let harness = super::super::test_support::FakeMegaDownloadHarness::new(
                    61,
                    300_000,
                    DownloadConfig {
                        force_overwrite: true,
                        ..DownloadConfig::default()
                    },
                )
                .await;
                tokio::fs::create_dir_all(&harness.output_dir)
                    .await
                    .unwrap();
                let output_path = harness.output_path(harness.fixture.file_name());
                let output_path_string = output_path.to_string_lossy().into_owned();
                let part_path = super::super::sidecar::part_path(&output_path_string);
                let sidecar_path = super::super::sidecar::sidecar_path(&output_path_string);
                let old_contents = b"keep this valid destination";
                tokio::fs::write(&output_path, old_contents).await.unwrap();
                let node = harness.node();
                let chunk_verified = super::super::callbacks::ChunkVerifiedState::new(
                    super::super::resume_tracker::ResumeTracker::new(
                        node.size(),
                        *node.condensed_mac().unwrap(),
                        vec![None; mega::mega_chunk_boundaries(node.size()).len()],
                    ),
                    super::super::sidecar_writer::LazySidecarWriter::new(
                        sidecar_path.clone(),
                        part_path.clone(),
                    ),
                );
                let progress: Arc<dyn super::super::callbacks::DownloadProgress> =
                    Arc::new(super::super::callbacks::NoProgress);
                let stats = DownloadStatsTracker::new(node.size());

                let result = harness
                    .downloader
                    .finish_download_result(
                        DownloadFinishContext {
                            node,
                            path: &output_path_string,
                            part_path: &part_path,
                            sidecar_path: &sidecar_path,
                            reused_bytes: 0,
                            stats: &stats,
                            chunk_verified: &chunk_verified,
                            progress: &progress,
                            name: &output_path_string,
                        },
                        Ok(()),
                    )
                    .await;

                assert!(result.is_err());
                assert_eq!(tokio::fs::read(&output_path).await.unwrap(), old_contents);
                harness.shutdown().await;
            },
        );
    }
}
