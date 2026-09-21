use std::sync::Arc;
use std::time::Duration;

use super::super::NoProgress;
use super::super::test_support::*;
use super::*;
use crate::config::DownloadConfig;

#[tokio::test]
async fn cancellation_waits_for_the_outer_mega_future_to_finish_cleanup() {
    let cancellation = CancellationToken::new();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(super::await_download_or_cancel(
        async move {
            release_rx.await.unwrap();
            Ok::<(), mega::Error>(())
        },
        cancellation.clone(),
    ));

    cancellation.cancel();
    tokio::time::timeout(Duration::from_millis(20), async {
        while !task.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect_err("cancellation must not drop the outer future");
    release_tx.send(()).unwrap();
    let result = task.await.unwrap();
    assert!(matches!(result, Err(crate::Error::Cancelled)));
}

#[tokio::test]
async fn ensure_parent_dir_creates_missing_ancestors() {
    let temp = tempfile::tempdir().unwrap();
    let downloader = tokio_downloader();
    let output_path = temp
        .path()
        .join("a")
        .join("deep")
        .join("tree")
        .join("payload.bin");
    let output_path_string = output_path.to_string_lossy().into_owned();

    downloader
        .ensure_parent_dir(&output_path_string)
        .await
        .unwrap();

    assert!(
        tokio::fs::try_exists(output_path.parent().unwrap())
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn download_file_creates_missing_parent_dirs_and_writes_plaintext() {
    let harness = FakeMegaDownloadHarness::new(
        43,
        262_219,
        DownloadConfig::default()
            .with_chunks_per_file(1)
            .with_concurrent_files(1)
            .with_force_overwrite(true),
    )
    .await;
    let progress: Arc<dyn DownloadProgress> = Arc::new(NoProgress);
    let output_path = harness.output_path(
        std::path::Path::new("nested")
            .join("leaf")
            .join(harness.fixture.file_name()),
    );
    let output_path_string = output_path.to_string_lossy().into_owned();
    let total_bytes;
    let stats = {
        let node = harness.node();
        total_bytes = node.size();
        harness
            .downloader
            .download_file(node, &output_path_string, &progress, false, None)
            .await
            .unwrap()
    };

    let actual = tokio::fs::read(&output_path).await.unwrap();
    let mut expected = vec![0u8; actual.len()];
    harness.fixture.fill_plaintext(0, &mut expected);
    assert_eq!(actual, expected);
    assert_eq!(stats.size, total_bytes);
    assert_eq!(stats.network_bytes, total_bytes);
    assert!(
        tokio::fs::try_exists(output_path.parent().unwrap())
            .await
            .unwrap()
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn download_file_short_circuits_for_verified_existing_output() {
    let harness = FakeMegaDownloadHarness::new(47, 300_000, DownloadConfig::default()).await;
    let progress: Arc<dyn DownloadProgress> = Arc::new(NoProgress);
    tokio::fs::create_dir_all(&harness.output_dir)
        .await
        .unwrap();
    let output_path = harness.output_path(harness.fixture.file_name());
    let output_path_string = output_path.to_string_lossy().into_owned();
    let total_bytes = {
        let node = harness.node();
        node.size()
    };
    let mut expected = vec![0u8; usize_from_u64(total_bytes)];
    harness.fixture.fill_plaintext(0, &mut expected);
    tokio::fs::write(&output_path, &expected).await.unwrap();

    let stats = {
        let node = harness.node();
        harness
            .downloader
            .download_file(node, &output_path_string, &progress, false, None)
            .await
            .unwrap()
    };

    assert_eq!(stats.size, total_bytes);
    assert_eq!(stats.network_bytes, 0);
    assert_eq!(stats.reused_bytes, 0);
    assert!(
        !tokio::fs::try_exists(part_path(&output_path_string))
            .await
            .unwrap()
    );

    harness.shutdown().await;
}
