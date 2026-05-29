use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use super::super::NoProgress;
use super::*;
use crate::config::DownloadConfig;
use crate::fake_mega::{FakeMegaServer, create_fake_mega_fixture};

static TEST_AES_KEY: [u8; 16] = [7u8; 16];
static TEST_AES_IV: [u8; 8] = [3u8; 8];

#[derive(Default)]
struct RecordingProgress {
    total: AtomicU64,
    network: AtomicU64,
    max_delta: AtomicU64,
    calls: AtomicUsize,
}

impl DownloadProgress for RecordingProgress {
    fn on_progress(&self, _name: &str, delta: crate::core::ProgressDelta) {
        self.total
            .fetch_add(delta.total_bytes_delta, Ordering::SeqCst);
        self.network
            .fetch_add(delta.network_bytes_delta, Ordering::SeqCst);
        self.max_delta
            .fetch_max(delta.total_bytes_delta, Ordering::SeqCst);
        self.calls.fetch_add(1, Ordering::SeqCst);
    }
}

fn test_incompressible_plaintext(size: usize) -> Vec<u8> {
    let mut state = 0x1234_5678_9abc_def0_u64;
    (0..size)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

async fn expected_condensed_mac(data: &[u8]) -> [u8; 8] {
    mega::compute_condensed_mac(
        futures::io::Cursor::new(data),
        data.len() as u64,
        &TEST_AES_KEY,
        &TEST_AES_IV,
    )
    .await
    .unwrap()
}

async fn write_test_file(dir: &tempfile::TempDir, name: &str, data: &[u8]) -> PathBuf {
    let path = dir.path().join(name);
    tokio::fs::write(&path, data).await.unwrap();
    path
}

fn usize_from_u64(value: u64) -> usize {
    usize::try_from(value).unwrap()
}

#[tokio::test]
async fn completed_file_mac_matches_mega_condensed_mac_for_boundary_cases() {
    for size in [
        0,
        1,
        15,
        16,
        17,
        REVALIDATION_BUFFER_BYTES - 1,
        REVALIDATION_BUFFER_BYTES,
        REVALIDATION_BUFFER_BYTES + 1,
        1280 * 1024,
        1280 * 1024 + 13,
        2 * 1024 * 1024 + 17,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let data = test_incompressible_plaintext(size);
        let path = write_test_file(&dir, "complete.bin", &data).await;

        let actual = compute_completed_file_mac_from_file(
            &path,
            data.len() as u64,
            &TEST_AES_KEY,
            &TEST_AES_IV,
            None,
        )
        .await
        .unwrap();

        assert_eq!(actual, expected_condensed_mac(&data).await, "size {size}");
    }
}

#[tokio::test]
async fn completed_file_mac_reports_progress_for_each_fixed_read_buffer() {
    let dir = tempfile::tempdir().unwrap();
    let data = test_incompressible_plaintext(1280 * 1024 + 13);
    let path = write_test_file(&dir, "complete.bin", &data).await;
    let progress = RecordingProgress::default();
    let expected_calls: usize = mega::mega_chunk_boundaries_iter(data.len() as u64)
        .map(|chunk| chunk.length.div_ceil(REVALIDATION_BUFFER_BYTES as u64) as usize)
        .sum();

    let actual = compute_completed_file_mac_from_file(
        &path,
        data.len() as u64,
        &TEST_AES_KEY,
        &TEST_AES_IV,
        Some(("complete.bin", &progress)),
    )
    .await
    .unwrap();

    assert_eq!(actual, expected_condensed_mac(&data).await);
    assert_eq!(progress.total.load(Ordering::SeqCst), data.len() as u64);
    assert_eq!(progress.network.load(Ordering::SeqCst), 0);
    assert_eq!(progress.calls.load(Ordering::SeqCst), expected_calls);
    assert!(progress.max_delta.load(Ordering::SeqCst) <= REVALIDATION_BUFFER_BYTES as u64);
}

#[tokio::test]
async fn completed_file_mac_does_not_emit_progress_for_empty_file() {
    let dir = tempfile::tempdir().unwrap();
    let data = Vec::new();
    let path = write_test_file(&dir, "empty.bin", &data).await;
    let progress = RecordingProgress::default();

    let actual = compute_completed_file_mac_from_file(
        &path,
        0,
        &TEST_AES_KEY,
        &TEST_AES_IV,
        Some(("empty.bin", &progress)),
    )
    .await
    .unwrap();

    assert_eq!(actual, expected_condensed_mac(&data).await);
    assert_eq!(progress.total.load(Ordering::SeqCst), 0);
    assert_eq!(progress.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn completed_file_mac_rejects_short_files() {
    let dir = tempfile::tempdir().unwrap();
    let data = test_incompressible_plaintext(64 * 1024);
    let path = write_test_file(&dir, "short.bin", &data).await;

    let err = compute_completed_file_mac_from_file(
        &path,
        data.len() as u64 + 1,
        &TEST_AES_KEY,
        &TEST_AES_IV,
        None,
    )
    .await
    .unwrap_err();

    assert!(matches!(err, Error::Io(_)));
}

async fn run_complete_existing_file_rejects_same_size_corrupt_final_file_test() {
    let temp = tempfile::tempdir().unwrap();
    let fixture_dir = temp.path().join("fixture");
    let output_dir = temp.path().join("output");
    let fixture = create_fake_mega_fixture(&fixture_dir, "payload.bin", 300_000, 29)
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
    let node = nodes.get_node_by_handle(fixture.handle()).unwrap();
    tokio::fs::create_dir_all(&output_dir).await.unwrap();
    let downloader = Downloader::new(client, DownloadConfig::default());
    let output_path = output_dir.join(fixture.file_name());
    let output_path_string = output_path.to_string_lossy().into_owned();
    let progress: Arc<dyn DownloadProgress> = Arc::new(NoProgress);
    let mut corrupt = vec![0u8; usize_from_u64(node.size())];
    fixture.fill_plaintext(0, &mut corrupt);
    corrupt[0] ^= 0xff;
    tokio::fs::write(&output_path, &corrupt).await.unwrap();

    let existing = downloader
        .complete_existing_file(node, &output_path_string, &progress)
        .await
        .unwrap();

    assert!(
        existing.is_none(),
        "same-size corrupted completed files must not be accepted as already complete"
    );

    server.shutdown().await.unwrap();
}

#[test]
fn complete_existing_file_rejects_same_size_corrupt_final_file() {
    std::thread::Builder::new()
        .name("complete-existing-corrupt-file-test".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime
                .block_on(run_complete_existing_file_rejects_same_size_corrupt_final_file_test());
        })
        .unwrap()
        .join()
        .unwrap();
}
