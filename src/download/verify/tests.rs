use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use super::super::NoProgress;
use super::super::test_support::*;
use super::*;
use crate::config::DownloadConfig;

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

mod property_tests {
    use super::*;
    use proptest::prelude::*;

    fn block_on_test<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(24))]

        #[test]
        fn completed_file_mac_progress_matches_buffering_contract(
            size in 0usize..(2 * 1280 * 1024 + 257),
        ) {
            let data = test_incompressible_plaintext(size);
            let expected_calls: usize = mega::mega_chunk_boundaries_iter(data.len() as u64)
                .map(|chunk| chunk.length.div_ceil(REVALIDATION_BUFFER_BYTES as u64) as usize)
                .sum();

            let (actual, expected, total, network, calls, max_delta) = block_on_test(async {
                let dir = tempfile::tempdir().unwrap();
                let path = write_test_file(&dir, "complete.bin", &data).await;
                let progress = RecordingProgress::default();
                let actual = compute_completed_file_mac_from_file(
                    &path,
                    data.len() as u64,
                    &TEST_AES_KEY,
                    &TEST_AES_IV,
                    Some(("complete.bin", &progress)),
                )
                .await
                .unwrap();
                (
                    actual,
                    expected_condensed_mac(&data).await,
                    progress.total.load(Ordering::SeqCst),
                    progress.network.load(Ordering::SeqCst),
                    progress.calls.load(Ordering::SeqCst),
                    progress.max_delta.load(Ordering::SeqCst),
                )
            });

            prop_assert_eq!(actual, expected);
            prop_assert_eq!(total, data.len() as u64);
            prop_assert_eq!(network, 0);
            prop_assert_eq!(calls, expected_calls);
            prop_assert!(max_delta <= REVALIDATION_BUFFER_BYTES as u64);
        }

        #[test]
        fn completed_file_mac_rejects_generated_short_files(
            size in 0usize..(2 * 1280 * 1024 + 257),
            short_by in 1usize..5,
        ) {
            let data = test_incompressible_plaintext(size);
            let err = block_on_test(async {
                let dir = tempfile::tempdir().unwrap();
                let path = write_test_file(&dir, "short.bin", &data).await;
                compute_completed_file_mac_from_file(
                    &path,
                    data.len() as u64 + short_by as u64,
                    &TEST_AES_KEY,
                    &TEST_AES_IV,
                    None,
                )
                .await
                .unwrap_err()
            });

            prop_assert!(matches!(err, Error::Io(_)));
        }
    }
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

async fn run_complete_existing_file_rejects_same_size_corrupt_final_file_test() {
    let harness = FakeMegaDownloadHarness::new(29, 300_000, DownloadConfig::default()).await;
    tokio::fs::create_dir_all(&harness.output_dir)
        .await
        .unwrap();
    let output_path = harness.output_path(harness.fixture.file_name());
    let output_path_string = output_path.to_string_lossy().into_owned();
    let progress: Arc<dyn DownloadProgress> = Arc::new(NoProgress);
    let mut corrupt = vec![0u8; usize_from_u64(harness.node().size())];
    harness.fixture.fill_plaintext(0, &mut corrupt);
    corrupt[0] ^= 0xff;
    tokio::fs::write(&output_path, &corrupt).await.unwrap();

    let existing = {
        let node = harness.node();
        harness
            .downloader
            .complete_existing_file(node, &output_path_string, &progress)
            .await
            .unwrap()
    };

    assert!(
        existing.is_none(),
        "same-size corrupted completed files must not be accepted as already complete"
    );

    harness.shutdown().await;
}

#[test]
fn complete_existing_file_rejects_same_size_corrupt_final_file() {
    run_with_large_stack_current_thread_runtime("complete-existing-corrupt-file-test", || async {
        run_complete_existing_file_rejects_same_size_corrupt_final_file_test().await;
    });
}
