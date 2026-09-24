use std::io;
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
    match error {
        Error::Mega(mega::Error::CondensedMacMismatch) => true,
        Error::Mega(_)
        | Error::Dlc(_)
        | Error::Io(_)
        | Error::FileExists { .. }
        | Error::Download(_)
        | Error::InvalidDownloadConfig(_)
        | Error::Http(_)
        | Error::Cancelled => false,
    }
}

pub(super) const fn should_delete_resume_state_on_error(
    config: &DownloadConfig,
    error: &Error,
) -> bool {
    is_condensed_mac_mismatch(error) || (config.cleanup_on_error && !error.is_cancelled())
}

impl<F: FileSystem> Downloader<F> {
    pub(super) async fn finish_download_result(
        &self,
        ctx: DownloadFinishContext<'_>,
        download_result: Result<()>,
    ) -> Result<FileStats> {
        match download_result {
            Ok(()) => {
                if let Err(error) = ctx
                    .chunk_verified
                    .finish_sidecar_writer(SidecarWriterShutdown::Abort)
                    .await
                {
                    log::warn!("Resume sidecar writer reported an earlier failure: {error}");
                }
                self.fs
                    .rename_file(ctx.part_path, Path::new(ctx.path))
                    .await?;
                // Rename publishes the output path; this sync is the separate
                // durability acknowledgement required before completion.
                let output_parent = Path::new(ctx.path)
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
                self.fs
                    .sync_directory(output_parent)
                    .await
                    .map_err(|error| {
                        Error::Io(io::Error::new(
                            error.kind(),
                            format!(
                                "output rename completed but durability acknowledgement failed for {}: {error}",
                                ctx.path
                            ),
                        ))
                    })?;
                delete_sidecar(ctx.sidecar_path).await.map_err(|error| {
                    Error::Io(io::Error::new(
                        error.kind(),
                        format!(
                            "output {} is durable, but resume sidecar cleanup failed: {error}",
                            ctx.path
                        ),
                    ))
                })?;

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
                    if let Err(error) = ctx
                        .chunk_verified
                        .finish_sidecar_writer(SidecarWriterShutdown::Abort)
                        .await
                    {
                        log::warn!("Failed to stop resume sidecar writer: {error}");
                    }
                    let _ = self.fs.remove_file(ctx.part_path).await;
                    if let Err(error) = delete_sidecar(ctx.sidecar_path).await {
                        log::warn!(
                            "Failed to clean resume sidecar after download failure: {error}"
                        );
                    }
                } else {
                    match self.fs.sync_file(ctx.part_path).await {
                        Ok(()) => {
                            ctx.chunk_verified
                                .finish_sidecar_writer(SidecarWriterShutdown::Flush)
                                .await
                                .map_err(Error::Io)?;
                        }
                        Err(sync_err) => {
                            ctx.chunk_verified
                                .finish_sidecar_writer(SidecarWriterShutdown::Abort)
                                .await
                                .map_err(Error::Io)
                                .unwrap_or_else(|error| {
                                    log::warn!("Failed to stop resume sidecar writer: {error}");
                                });
                            log::warn!(
                                "Failed to sync partial file {} before saving resume sidecar: {sync_err}",
                                ctx.part_path.display()
                            );
                        }
                    }
                }
                if !e.is_cancelled() {
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
                    )
                    .expect("sidecar writer should start"),
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

    #[tokio::test]
    async fn output_directory_sync_failure_is_not_reported_as_completion() {
        let harness = super::super::test_support::FakeMegaDownloadHarness::new(
            62,
            300_000,
            DownloadConfig::default(),
        )
        .await;
        let node = harness.node();
        let output_path = harness.output_path("result.bin");
        let output = output_path.to_string_lossy().into_owned();
        let part_path = super::super::sidecar::part_path(&output);
        let sidecar_path = super::super::sidecar::sidecar_path(&output);
        let fs = super::super::test_support::MockFileSystem::new();
        fs.fail_directory_sync("injected output directory sync failure");
        let downloader = super::super::test_support::mock_downloader(fs);
        let chunk_verified = super::super::callbacks::ChunkVerifiedState::new(
            super::super::resume_tracker::ResumeTracker::new(
                node.size(),
                *node.condensed_mac().unwrap(),
                vec![None; mega::mega_chunk_boundaries(node.size()).len()],
            ),
            super::super::sidecar_writer::LazySidecarWriter::new(
                sidecar_path.clone(),
                part_path.clone(),
            )
            .expect("sidecar writer should start"),
        );
        let progress = Arc::new(super::super::test_support::RecordingProgress::default());
        let progress_callback: Arc<dyn super::super::callbacks::DownloadProgress> =
            progress.clone();
        let stats = DownloadStatsTracker::new(node.size());

        let result = downloader
            .finish_download_result(
                DownloadFinishContext {
                    node,
                    path: &output,
                    part_path: &part_path,
                    sidecar_path: &sidecar_path,
                    reused_bytes: 0,
                    stats: &stats,
                    chunk_verified: &chunk_verified,
                    progress: &progress_callback,
                    name: &output,
                },
                Ok(()),
            )
            .await;

        let error = result.expect_err("directory sync failure must fail completion");
        assert!(error.to_string().contains("rename completed"));
        assert_eq!(
            progress.completed.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "completion callback must follow durable directory acknowledgement"
        );
        harness.shutdown().await;
    }

    #[tokio::test]
    async fn sidecar_cleanup_failure_is_returned_after_durable_output() {
        let harness = super::super::test_support::FakeMegaDownloadHarness::new(
            63,
            300_000,
            DownloadConfig::default(),
        )
        .await;
        let node = harness.node();
        let output_path = harness.output_path("result.bin");
        let output = output_path.to_string_lossy().into_owned();
        let part_path = super::super::sidecar::part_path(&output);
        let sidecar_path = super::super::sidecar::sidecar_path(&output);
        tokio::fs::create_dir_all(output_path.parent().unwrap())
            .await
            .expect("output parent should be available for sidecar setup");
        tokio::fs::create_dir(&sidecar_path)
            .await
            .expect("directory at sidecar path should force cleanup failure");
        let fs = super::super::test_support::MockFileSystem::new();
        let downloader = super::super::test_support::mock_downloader(fs);
        let chunk_verified = super::super::callbacks::ChunkVerifiedState::new(
            super::super::resume_tracker::ResumeTracker::new(
                node.size(),
                *node.condensed_mac().unwrap(),
                vec![None; mega::mega_chunk_boundaries(node.size()).len()],
            ),
            super::super::sidecar_writer::LazySidecarWriter::new(
                sidecar_path.clone(),
                part_path.clone(),
            )
            .expect("sidecar writer should start"),
        );
        let progress = Arc::new(super::super::test_support::RecordingProgress::default());
        let progress_callback: Arc<dyn super::super::callbacks::DownloadProgress> =
            progress.clone();
        let stats = DownloadStatsTracker::new(node.size());

        let result = downloader
            .finish_download_result(
                DownloadFinishContext {
                    node,
                    path: &output,
                    part_path: &part_path,
                    sidecar_path: &sidecar_path,
                    reused_bytes: 0,
                    stats: &stats,
                    chunk_verified: &chunk_verified,
                    progress: &progress_callback,
                    name: &output,
                },
                Ok(()),
            )
            .await;

        let error = result.expect_err("sidecar cleanup failure must be returned");
        assert!(error.to_string().contains("output "));
        assert!(error.to_string().contains("sidecar cleanup failed"));
        assert_eq!(
            progress.completed.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "completion callback must follow sidecar cleanup"
        );
        harness.shutdown().await;
    }
}
