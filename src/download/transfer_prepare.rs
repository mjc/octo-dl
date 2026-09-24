#![allow(clippy::struct_field_names, clippy::too_many_arguments)]

use std::path::Path;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::error::Result;
use crate::fs::FileSystem;

use super::callbacks::{
    ChunkVerifiedState, DownloadCallbackState, DownloadProgress, ProgressCallbackState,
    ResumeValidationStatusProgress,
};
use super::downloader::Downloader;
use super::resume_state::should_reuse_resume_state;
use super::resume_tracker::ResumeTracker;
use super::resume_validation::ResumeValidation;
use super::sidecar::{
    delete_sidecar, legacy_binary_sidecar_path, legacy_json_sidecar_path, legacy_part_path,
    legacy_postcard_sidecar_path,
};
use super::sidecar_store::{ResumeSidecar, load_sidecar_sync, save_sidecar_atomic};
use super::sidecar_writer::LazySidecarWriter;
use super::verify::expected_mac;

pub(super) struct PreparedTransferResume {
    pub(super) callback_state: Arc<DownloadCallbackState>,
    pub(super) trusted_for_download: Arc<[Option<[u8; 16]>]>,
    pub(super) trusted_bytes: u64,
    pub(super) preserve_existing: bool,
}

impl<F: FileSystem> Downloader<F> {
    pub(super) async fn prepare_transfer_resume(
        &self,
        node: &mega::Node,
        path: &str,
        progress: &Arc<dyn DownloadProgress>,
        trust_resume_state: bool,
        part_path: &Path,
        sidecar_path: &Path,
        cancellation_token: Option<&CancellationToken>,
    ) -> Result<PreparedTransferResume> {
        self.validate_output_path(path)?;
        let expected_condensed_mac = expected_mac(node)?;
        migrate_legacy_resume_state(
            &self.fs,
            node.size(),
            expected_condensed_mac,
            path,
            part_path,
            sidecar_path,
        )
        .await?;
        let boundaries = mega::mega_chunk_boundaries(node.size());
        log::debug!(
            "Download resume setup for {path}: size={} trust_resume_state={} force_overwrite={} part={} sidecar={} chunks={}",
            node.size(),
            trust_resume_state,
            self.config.force_overwrite,
            part_path.display(),
            sidecar_path.display(),
            boundaries.len()
        );
        let reuse_resume_state =
            should_reuse_resume_state(self.config.force_overwrite, trust_resume_state);
        let resume_validation = if reuse_resume_state {
            let resume_status_progress = ResumeValidationStatusProgress::new(progress.as_ref());
            self.revalidate_resume_chunks(
                node,
                &boundaries,
                part_path,
                sidecar_path,
                expected_condensed_mac,
                Some((path, &resume_status_progress)),
                cancellation_token,
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
        let preserve_sidecar_after_stat_failure = resume_validation.sidecar_loaded
            && self.fs.file_exists(part_path).await
            && self.fs.file_fingerprint(part_path).await.is_none();
        if !preserve_existing && !preserve_sidecar_after_stat_failure {
            let _ = delete_sidecar(sidecar_path).await;
        }
        if resume_validation.sidecar_loaded && resume_validation.trusted_count == 0 {
            log::debug!("Resume sidecar found for {path}, but no chunks were reusable");
        }
        let trusted_bytes = resume_validation.trusted_bytes;
        if trusted_bytes > 0 {
            progress.on_resume_reused(path, resume_validation.trusted_count, trusted_bytes);
        }

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
                LazySidecarWriter::new(sidecar_path.to_path_buf(), part_path.to_path_buf())?,
            ),
        ));

        Ok(PreparedTransferResume {
            callback_state,
            trusted_for_download,
            trusted_bytes,
            preserve_existing,
        })
    }
}

pub(super) async fn migrate_legacy_resume_state(
    fs: &impl FileSystem,
    file_size: u64,
    expected_condensed_mac: [u8; 8],
    path: &str,
    part_path: &Path,
    sidecar_path: &Path,
) -> std::io::Result<()> {
    let legacy_part = &legacy_part_path(path);
    let legacy_sidecars = [
        legacy_postcard_sidecar_path(path),
        legacy_binary_sidecar_path(path),
        legacy_json_sidecar_path(path),
    ];
    if tokio::fs::try_exists(part_path).await? || tokio::fs::try_exists(sidecar_path).await? {
        return Ok(());
    }
    let part_metadata = match tokio::fs::symlink_metadata(legacy_part).await {
        Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_file() => metadata,
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if part_metadata.len() > file_size {
        return Ok(());
    }

    let mut matching_sidecar: Option<ResumeSidecar> = None;
    for candidate in &legacy_sidecars {
        match tokio::fs::symlink_metadata(candidate).await {
            Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_file() => {}
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        }
        let Some(sidecar) = load_sidecar_sync(candidate, candidate, candidate) else {
            continue;
        };
        if sidecar.file_size == file_size
            && sidecar.expected_condensed_mac == expected_condensed_mac
        {
            matching_sidecar = Some(sidecar);
            break;
        }
    }
    let Some(matching_sidecar) = matching_sidecar else {
        return Ok(());
    };

    let Some(actual_len) = fs.file_size(legacy_part).await else {
        return Ok(());
    };
    let mut source = tokio::fs::File::open(legacy_part).await?;
    let mut target = fs.open_part_file(part_path, actual_len, false).await?;
    if let Err(error) = tokio::io::copy(&mut source, &mut target).await {
        let _ = fs.remove_file(part_path).await;
        return Err(error);
    }
    drop(target);
    if let Err(error) = save_sidecar_atomic(sidecar_path, &matching_sidecar).await {
        let _ = fs.remove_file(part_path).await;
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use super::super::sidecar::{part_path, sidecar_path};
    use super::super::sidecar_store::save_sidecar_atomic;
    use super::super::test_support::*;
    use super::*;
    use crate::config::DownloadConfig;
    use crate::fs::{FileSystem, TokioFileSystem};

    #[derive(Default)]
    struct ReuseRecordingProgress {
        reused_calls: AtomicUsize,
        reused_chunks: AtomicUsize,
        reused_bytes: AtomicU64,
    }

    impl DownloadProgress for ReuseRecordingProgress {
        fn on_resume_reused(&self, _name: &str, chunks: usize, bytes: u64) {
            self.reused_calls.fetch_add(1, Ordering::SeqCst);
            self.reused_chunks.store(chunks, Ordering::SeqCst);
            self.reused_bytes.store(bytes, Ordering::SeqCst);
        }
    }

    #[test]
    fn prepare_transfer_resume_reuses_verified_chunks_and_reports_progress() {
        run_with_large_stack_current_thread_runtime("prepare-transfer-reuse-test", || async {
            let harness =
                FakeMegaDownloadHarness::new(53, 300_000, DownloadConfig::default()).await;
            tokio::fs::create_dir_all(&harness.output_dir)
                .await
                .unwrap();
            let output_path = harness.output_path(harness.fixture.file_name());
            let output_path_string = output_path.to_string_lossy().into_owned();
            let part_path = part_path(&output_path_string);
            let sidecar_path = sidecar_path(&output_path_string);
            let node = harness.node();
            let first = mega::mega_chunk_boundaries(node.size())[0];
            let mut first_chunk = vec![0u8; usize_from_u64(first.length)];
            harness
                .fixture
                .fill_plaintext(first.offset, &mut first_chunk);
            tokio::fs::write(&part_path, &first_chunk).await.unwrap();
            let expected_fingerprint = TokioFileSystem::new()
                .file_fingerprint(&part_path)
                .await
                .unwrap();
            let expected_mac =
                mega::compute_mega_chunk_mac(&first_chunk, node.aes_key(), node.aes_iv().unwrap());
            let mut sidecar = sidecar_for_chunk(
                node.size(),
                *node.condensed_mac().unwrap(),
                first.index,
                expected_mac,
            );
            sidecar.part_fingerprint = Some(expected_fingerprint);
            save_sidecar_atomic(&sidecar_path, &sidecar).await.unwrap();
            let progress = Arc::new(ReuseRecordingProgress::default());
            let progress_obj: Arc<dyn DownloadProgress> = progress.clone();

            let prepared = harness
                .downloader
                .prepare_transfer_resume(
                    node,
                    &output_path_string,
                    &progress_obj,
                    true,
                    &part_path,
                    &sidecar_path,
                    None,
                )
                .await
                .unwrap();

            assert!(prepared.preserve_existing);
            assert_eq!(prepared.trusted_bytes, first.length);
            assert_eq!(
                prepared.trusted_for_download[usize_from_u32(first.index)],
                Some(expected_mac)
            );
            assert!(
                prepared
                    .trusted_for_download
                    .iter()
                    .enumerate()
                    .all(|(index, mac)| index == usize_from_u32(first.index) || mac.is_none())
            );
            assert_eq!(progress.reused_calls.load(Ordering::SeqCst), 1);
            assert_eq!(progress.reused_chunks.load(Ordering::SeqCst), 1);
            assert_eq!(progress.reused_bytes.load(Ordering::SeqCst), first.length);
            assert!(tokio::fs::try_exists(&sidecar_path).await.unwrap());

            harness.shutdown().await;
        });
    }

    #[test]
    fn legacy_resume_state_is_copied_verified_and_left_intact() {
        run_with_large_stack_current_thread_runtime(
            "prepare-transfer-legacy-migration-test",
            || async {
                let harness =
                    FakeMegaDownloadHarness::new(67, 300_000, DownloadConfig::default()).await;
                tokio::fs::create_dir_all(&harness.output_dir)
                    .await
                    .unwrap();
                let output_path = harness.output_path(harness.fixture.file_name());
                let output_path_string = output_path.to_string_lossy().into_owned();
                let new_part_path = part_path(&output_path_string);
                let new_sidecar_path = sidecar_path(&output_path_string);
                let old_part_path = legacy_part_path(&output_path_string);
                let old_sidecar_path = legacy_postcard_sidecar_path(&output_path_string);
                let node = harness.node();
                let first = mega::mega_chunk_boundaries(node.size())[0];
                let mut first_chunk = vec![0u8; usize_from_u64(first.length)];
                harness
                    .fixture
                    .fill_plaintext(first.offset, &mut first_chunk);
                tokio::fs::write(&old_part_path, &first_chunk)
                    .await
                    .unwrap();
                let expected_mac = mega::compute_mega_chunk_mac(
                    &first_chunk,
                    node.aes_key(),
                    node.aes_iv().unwrap(),
                );
                let sidecar = sidecar_for_chunk(
                    node.size(),
                    *node.condensed_mac().unwrap(),
                    first.index,
                    expected_mac,
                );
                save_sidecar_atomic(&old_sidecar_path, &sidecar)
                    .await
                    .unwrap();
                let progress_obj: Arc<dyn DownloadProgress> =
                    Arc::new(ReuseRecordingProgress::default());

                let prepared = harness
                    .downloader
                    .prepare_transfer_resume(
                        node,
                        &output_path_string,
                        &progress_obj,
                        true,
                        &new_part_path,
                        &new_sidecar_path,
                        None,
                    )
                    .await
                    .unwrap();

                assert!(prepared.preserve_existing);
                assert_eq!(prepared.trusted_bytes, first.length);
                assert_eq!(tokio::fs::read(&old_part_path).await.unwrap(), first_chunk);
                assert!(old_sidecar_path.exists());
                assert!(new_part_path.exists());
                assert!(new_sidecar_path.exists());

                super::super::sidecar::delete_resume_artifacts_for_path(&output_path_string)
                    .await
                    .unwrap();
                assert!(!new_part_path.exists());
                assert!(!new_sidecar_path.exists());
                assert!(old_part_path.exists());
                assert!(old_sidecar_path.exists());

                harness.shutdown().await;
            },
        );
    }

    #[test]
    fn prepare_transfer_resume_deletes_sidecar_when_nothing_is_reusable() {
        run_with_large_stack_current_thread_runtime("prepare-transfer-clear-test", || async {
            let harness =
                FakeMegaDownloadHarness::new(59, 300_000, DownloadConfig::default()).await;
            tokio::fs::create_dir_all(&harness.output_dir)
                .await
                .unwrap();
            let output_path = harness.output_path(harness.fixture.file_name());
            let output_path_string = output_path.to_string_lossy().into_owned();
            let part_path = part_path(&output_path_string);
            let sidecar_path = sidecar_path(&output_path_string);
            let node = harness.node();
            let first = mega::mega_chunk_boundaries(node.size())[0];
            let expected_mac = [5u8; 16];
            let sidecar = sidecar_for_chunk(
                node.size(),
                *node.condensed_mac().unwrap(),
                first.index,
                expected_mac,
            );
            save_sidecar_atomic(&sidecar_path, &sidecar).await.unwrap();
            let progress = Arc::new(ReuseRecordingProgress::default());
            let progress_obj: Arc<dyn DownloadProgress> = progress.clone();

            let prepared = harness
                .downloader
                .prepare_transfer_resume(
                    node,
                    &output_path_string,
                    &progress_obj,
                    true,
                    &part_path,
                    &sidecar_path,
                    None,
                )
                .await
                .unwrap();

            assert!(!prepared.preserve_existing);
            assert_eq!(prepared.trusted_bytes, 0);
            assert!(prepared.trusted_for_download.iter().all(Option::is_none));
            assert_eq!(progress.reused_calls.load(Ordering::SeqCst), 0);
            assert_eq!(progress.reused_chunks.load(Ordering::SeqCst), 0);
            assert_eq!(progress.reused_bytes.load(Ordering::SeqCst), 0);
            assert!(!tokio::fs::try_exists(&sidecar_path).await.unwrap());

            harness.shutdown().await;
        });
    }
}
