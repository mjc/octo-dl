use std::sync::Arc;

use super::super::NoProgress;
use super::super::test_support::*;
use super::*;
use crate::config::DownloadConfig;

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
