use std::sync::atomic::Ordering;

use super::super::test_support::*;
use super::super::{ResumeSidecar, load_sidecar, part_path, save_sidecar_atomic, sidecar_path};
use super::*;
use crate::config::DownloadConfig;
use crate::fake_mega::{FakeMegaFixture, FakeMegaServer, create_fake_mega_fixture};
use crate::fs::{FileSystem, TokioFileSystem};

enum StoredFingerprint {
    Current,
    Stale,
}

struct ResumeReverifyHarness {
    _temp: tempfile::TempDir,
    fixture: FakeMegaFixture,
    server: FakeMegaServer,
    downloader: Downloader,
    nodes: mega::Nodes,
    output_path_string: String,
    part_path: std::path::PathBuf,
    sidecar_path: std::path::PathBuf,
}

impl ResumeReverifyHarness {
    async fn new(seed: u64, config: DownloadConfig) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let fixture_dir = temp.path().join("fixture");
        let output_dir = temp.path().join("output");
        let fixture = create_fake_mega_fixture(&fixture_dir, "payload.bin", 300_000, seed)
            .await
            .unwrap();
        let server = FakeMegaServer::spawn(fixture.clone(), 1).unwrap();
        let client = mega::Client::builder()
            .origin(server.origin().clone())
            .build(reqwest::Client::new())
            .unwrap();
        let nodes = client
            .fetch_public_nodes(&fixture.public_url())
            .await
            .unwrap();
        tokio::fs::create_dir_all(&output_dir).await.unwrap();
        let downloader = Downloader::new(client, config);
        let output_path = output_dir.join(fixture.file_name());
        let output_path_string = output_path.to_string_lossy().into_owned();
        let part_path = part_path(&output_path_string);
        let sidecar_path = sidecar_path(&output_path_string);
        Self {
            _temp: temp,
            fixture,
            server,
            downloader,
            nodes,
            output_path_string,
            part_path,
            sidecar_path,
        }
    }

    fn node(&self) -> &mega::Node {
        self.nodes
            .get_node_by_handle(self.fixture.handle())
            .unwrap()
    }

    async fn seed_first_verified_chunk(
        &self,
        stored_fingerprint: StoredFingerprint,
    ) -> (mega::MegaChunk, crate::fs::FileFingerprint, ResumeSidecar) {
        let node = self.node();
        let first = mega::mega_chunk_boundaries(node.size())[0];
        let mut first_chunk = vec![0u8; usize_from_u64(first.length)];
        self.fixture.fill_plaintext(first.offset, &mut first_chunk);
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
    let node = harness.node();
    let boundaries = mega::mega_chunk_boundaries(node.size());
    let (first, _current_fingerprint, sidecar) = harness
        .seed_first_verified_chunk(StoredFingerprint::Current)
        .await;

    let manual = harness
        .downloader
        .reverify_resume_file(node, &harness.output_path_string)
        .await
        .unwrap();
    let automatic = harness
        .downloader
        .revalidate_resume_chunks(
            node,
            &boundaries,
            &harness.part_path,
            &harness.sidecar_path,
            *node.condensed_mac().unwrap(),
            None,
            None,
        )
        .await
        .unwrap();

    assert_eq!(manual.chunks, 1);
    assert_eq!(manual.bytes, first.length);
    assert_eq!(automatic.trusted_count, manual.chunks);
    assert_eq!(automatic.trusted_bytes, manual.bytes);
    assert_eq!(
        automatic.trusted_chunks[usize_from_u32(first.index)],
        Some(sidecar.verified_chunks[0].mac)
    );

    harness.server.shutdown().await.unwrap();
}

#[test]
fn automatic_restart_revalidation_and_manual_reverify_agree_for_matching_sidecar_and_part() {
    run_with_large_stack_current_thread_runtime("resume-parity-test", || async {
        run_restart_revalidation_and_manual_reverify_parity_test().await;
    });
}

async fn run_manual_reverify_refreshes_sidecar_fingerprint_test() {
    let harness = ResumeReverifyHarness::new(23, DownloadConfig::default()).await;
    let node = harness.node();
    let (first, current_fingerprint, _sidecar) = harness
        .seed_first_verified_chunk(StoredFingerprint::Stale)
        .await;

    let result = harness
        .downloader
        .reverify_resume_file(node, &harness.output_path_string)
        .await
        .unwrap();
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

    harness.server.shutdown().await.unwrap();
}

#[test]
fn manual_reverify_refreshes_sidecar_fingerprint_after_disk_revalidation() {
    run_with_large_stack_current_thread_runtime("manual-reverify-fingerprint-test", || async {
        run_manual_reverify_refreshes_sidecar_fingerprint_test().await;
    });
}

#[tokio::test]
async fn manual_reverify_with_progress_reports_disk_validation_bytes() {
    let harness = ResumeReverifyHarness::new(31, DownloadConfig::default()).await;
    let node = harness.node();
    let (first, _current_fingerprint, _sidecar) = harness
        .seed_first_verified_chunk(StoredFingerprint::Stale)
        .await;

    let progress = RecordingProgress::default();
    let result = harness
        .downloader
        .reverify_resume_file_with_progress(node, &harness.output_path_string, Some(&progress))
        .await
        .unwrap();

    assert_eq!(result.chunks, 1);
    assert_eq!(result.bytes, first.length);
    assert_eq!(progress.validation_starts.load(Ordering::SeqCst), 1);
    assert!(progress.calls.load(Ordering::SeqCst) > 0);
    assert_eq!(progress.total.load(Ordering::SeqCst), first.length);
    assert_eq!(progress.network.load(Ordering::SeqCst), 0);

    harness.server.shutdown().await.unwrap();
}
