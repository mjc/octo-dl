use std::sync::atomic::Ordering;

use super::super::resume_state::ResumeReuseSource;
use super::super::resume_validation::ResumeValidation;
use super::super::sidecar::part_path;
use super::super::sidecar_store::{
    ResumeSidecar, VerifiedChunkRecord, load_sidecar, save_sidecar_atomic,
};
use super::super::test_support::*;
use super::persist_revalidated_sidecar;
use crate::config::DownloadConfig;
use crate::download::sidecar_path;
use crate::fs::{FileSystem, TokioFileSystem};

enum StoredFingerprint {
    Current,
    Stale,
}

struct ResumeReverifyHarness {
    base: FakeMegaDownloadHarness,
    output_path_string: String,
    part_path: std::path::PathBuf,
    sidecar_path: std::path::PathBuf,
}

impl ResumeReverifyHarness {
    async fn new(seed: u64, config: DownloadConfig) -> Self {
        let base = FakeMegaDownloadHarness::new(seed, 300_000, config).await;
        tokio::fs::create_dir_all(&base.output_dir).await.unwrap();
        let output_path = base.output_path(base.fixture.file_name());
        let output_path_string = output_path.to_string_lossy().into_owned();
        let part_path = part_path(&output_path_string);
        let sidecar_path = sidecar_path(&output_path_string);
        Self {
            base,
            output_path_string,
            part_path,
            sidecar_path,
        }
    }

    fn node(&self) -> &mega::Node {
        self.base.node()
    }

    async fn seed_first_verified_chunk(
        &self,
        stored_fingerprint: StoredFingerprint,
    ) -> (mega::MegaChunk, crate::fs::FileFingerprint, ResumeSidecar) {
        let node = self.node();
        let first = mega::mega_chunk_boundaries(node.size())[0];
        let mut first_chunk = vec![0u8; usize_from_u64(first.length)];
        self.base
            .fixture
            .fill_plaintext(first.offset, &mut first_chunk);
        tokio::fs::write(&self.part_path, &first_chunk)
            .await
            .unwrap();

        let current_fingerprint = TokioFileSystem::new()
            .file_fingerprint(&self.part_path)
            .await
            .unwrap();
        let mut persisted_fingerprint = current_fingerprint;
        if matches!(stored_fingerprint, StoredFingerprint::Stale) {
            persisted_fingerprint.len = persisted_fingerprint.len.saturating_add(1);
        }

        let mut sidecar = sidecar_for_chunk(
            node.size(),
            *node.condensed_mac().unwrap(),
            first.index,
            mega::compute_mega_chunk_mac(&first_chunk, node.aes_key(), node.aes_iv().unwrap()),
        );
        sidecar.part_fingerprint = Some(persisted_fingerprint);
        save_sidecar_atomic(&self.sidecar_path, &sidecar)
            .await
            .unwrap();

        (first, current_fingerprint, sidecar)
    }
}

async fn run_restart_revalidation_and_manual_reverify_parity_test() {
    let harness = ResumeReverifyHarness::new(
        19,
        DownloadConfig::default()
            .with_chunks_per_file(1)
            .with_concurrent_files(1),
    )
    .await;
    let (boundaries, condensed_mac) = {
        let node = harness.node();
        (
            mega::mega_chunk_boundaries(node.size()),
            *node.condensed_mac().unwrap(),
        )
    };
    let (first, _current_fingerprint, sidecar) = harness
        .seed_first_verified_chunk(StoredFingerprint::Current)
        .await;

    let (manual, automatic) = {
        let node = harness.node();
        let manual = harness
            .base
            .downloader
            .reverify_resume_file(node, &harness.output_path_string)
            .await
            .unwrap();
        let automatic = harness
            .base
            .downloader
            .revalidate_resume_chunks(
                node,
                &boundaries,
                &harness.part_path,
                &harness.sidecar_path,
                condensed_mac,
                None,
                None,
            )
            .await
            .unwrap();
        (manual, automatic)
    };

    assert_eq!(manual.chunks, 1);
    assert_eq!(manual.bytes, first.length);
    assert_eq!(automatic.trusted_count, manual.chunks);
    assert_eq!(automatic.trusted_bytes, manual.bytes);
    assert_eq!(
        automatic.trusted_chunks[usize_from_u32(first.index)],
        Some(sidecar.verified_chunks[0].mac)
    );

    harness.base.shutdown().await;
}

#[tokio::test]
async fn persist_revalidated_sidecar_writes_verified_chunks_and_current_fingerprint() {
    let dir = tempfile::tempdir().unwrap();
    let file_size = 300_000_u64;
    let expected_condensed_mac = [9_u8; 8];
    let output_path = dir.path().join("payload.bin");
    let sidecar_path = sidecar_path(output_path.to_string_lossy().as_ref());
    let part_path = dir.path().join("payload.bin.part");
    let part_data = test_incompressible_plaintext(usize_from_u64(file_size));
    tokio::fs::write(&part_path, &part_data).await.unwrap();
    let expected_fingerprint = TokioFileSystem::new()
        .file_fingerprint(&part_path)
        .await
        .unwrap();
    let validation = ResumeValidation {
        trusted_chunks: vec![Some([1_u8; 16]), None, Some([3_u8; 16])],
        trusted_count: 2,
        trusted_bytes: file_size,
        sidecar_loaded: true,
        source: Some(ResumeReuseSource::Sidecar),
    };

    persist_revalidated_sidecar(
        &TokioFileSystem::new(),
        &sidecar_path,
        &part_path,
        file_size,
        expected_condensed_mac,
        &validation,
    )
    .await
    .unwrap();

    let persisted = load_sidecar(&sidecar_path).await.unwrap();
    assert_eq!(persisted.file_size, file_size);
    assert_eq!(persisted.expected_condensed_mac, expected_condensed_mac);
    assert_eq!(
        persisted.verified_chunks,
        vec![
            VerifiedChunkRecord {
                index: 0,
                mac: [1_u8; 16]
            },
            VerifiedChunkRecord {
                index: 2,
                mac: [3_u8; 16]
            }
        ]
        .into()
    );
    assert_eq!(persisted.part_fingerprint, Some(expected_fingerprint));
}

#[test]
fn automatic_restart_revalidation_and_manual_reverify_agree_for_matching_sidecar_and_part() {
    run_with_large_stack_current_thread_runtime("resume-parity-test", || async {
        run_restart_revalidation_and_manual_reverify_parity_test().await;
    });
}

async fn run_manual_reverify_refreshes_sidecar_fingerprint_test() {
    let harness = ResumeReverifyHarness::new(23, DownloadConfig::default()).await;
    let (first, current_fingerprint, _sidecar) = harness
        .seed_first_verified_chunk(StoredFingerprint::Stale)
        .await;

    let result = {
        let node = harness.node();
        harness
            .base
            .downloader
            .reverify_resume_file(node, &harness.output_path_string)
            .await
            .unwrap()
    };
    assert_eq!(result.chunks, 1);
    assert_eq!(result.bytes, first.length);

    let refreshed = load_sidecar(&harness.sidecar_path)
        .await
        .expect("manual reverify should leave a sidecar behind");
    assert_eq!(
        refreshed.part_fingerprint,
        Some(current_fingerprint),
        "manual reverify should refresh the sidecar fingerprint to the current .part state"
    );

    harness.base.shutdown().await;
}

#[test]
fn manual_reverify_refreshes_sidecar_fingerprint_after_disk_revalidation() {
    run_with_large_stack_current_thread_runtime("manual-reverify-fingerprint-test", || async {
        run_manual_reverify_refreshes_sidecar_fingerprint_test().await;
    });
}

async fn run_manual_reverify_with_progress_reports_disk_validation_bytes_test() {
    let harness = ResumeReverifyHarness::new(31, DownloadConfig::default()).await;
    let (first, _current_fingerprint, _sidecar) = harness
        .seed_first_verified_chunk(StoredFingerprint::Stale)
        .await;

    let progress = RecordingProgress::default();
    let result = {
        let node = harness.node();
        harness
            .base
            .downloader
            .reverify_resume_file_with_progress(node, &harness.output_path_string, Some(&progress))
            .await
            .unwrap()
    };

    assert_eq!(result.chunks, 1);
    assert_eq!(result.bytes, first.length);
    assert_eq!(progress.validation_starts.load(Ordering::SeqCst), 1);
    assert!(progress.calls.load(Ordering::SeqCst) > 0);
    assert_eq!(progress.total.load(Ordering::SeqCst), first.length);
    assert_eq!(progress.network.load(Ordering::SeqCst), 0);

    harness.base.shutdown().await;
}

#[test]
fn manual_reverify_with_progress_reports_disk_validation_bytes() {
    run_with_large_stack_current_thread_runtime("manual-reverify-progress-test", || async {
        run_manual_reverify_with_progress_reports_disk_validation_bytes_test().await;
    });
}
