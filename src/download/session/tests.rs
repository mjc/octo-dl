use std::sync::Arc;

use super::super::NoProgress;
use super::super::test_support::*;
use super::*;
use crate::config::DownloadConfig;

#[tokio::test]
async fn download_all_returns_skipped_stats_when_empty() {
    let downloader = tokio_downloader();
    let progress: Arc<dyn DownloadProgress> = Arc::new(NoProgress);

    let stats = downloader.download_all(&[], &progress, 3).await.unwrap();

    assert_eq!(stats.files_downloaded, 0);
    assert_eq!(stats.files_skipped, 3);
    assert_eq!(stats.total_bytes, 0);
    assert_eq!(stats.network_bytes, 0);
}

#[tokio::test]
async fn download_all_aggregates_successful_download_stats() {
    let harness = FakeMegaDownloadHarness::new(
        37,
        262_219,
        DownloadConfig::default()
            .with_chunks_per_file(1)
            .with_concurrent_files(1)
            .with_force_overwrite(true),
    )
    .await;
    let progress: Arc<dyn DownloadProgress> = Arc::new(NoProgress);
    let output_path =
        harness.output_path(std::path::Path::new("nested").join(harness.fixture.file_name()));
    let output_path_string = output_path.to_string_lossy().into_owned();
    let total_bytes;
    let stats = {
        let node = harness.node();
        total_bytes = node.size();
        let files = [DownloadItem {
            path: output_path_string.clone(),
            node,
            was_partial: false,
        }];
        harness
            .downloader
            .download_all(&files, &progress, 2)
            .await
            .unwrap()
    };

    assert_eq!(stats.files_downloaded, 1);
    assert_eq!(stats.files_skipped, 2);
    assert_eq!(stats.total_bytes, total_bytes);
    assert_eq!(stats.network_bytes, total_bytes);
    assert!(tokio::fs::try_exists(&output_path).await.unwrap());

    harness.shutdown().await;
}

#[tokio::test]
async fn download_all_owned_aggregates_successful_download_stats() {
    let harness = FakeMegaDownloadHarness::new(
        41,
        262_219,
        DownloadConfig::default()
            .with_chunks_per_file(1)
            .with_concurrent_files(1)
            .with_force_overwrite(true),
    )
    .await;
    let progress: Arc<dyn DownloadProgress> = Arc::new(NoProgress);
    let output_path =
        harness.output_path(std::path::Path::new("owned").join(harness.fixture.file_name()));
    let output_path_string = output_path.to_string_lossy().into_owned();
    let (node, total_bytes) = {
        let node = harness.node();
        (node.clone(), node.size())
    };
    let files = [OwnedDownloadItem {
        path: output_path_string.clone(),
        node: node.clone(),
        was_partial: false,
    }];

    let stats = harness
        .downloader
        .download_all_owned(&files, &progress, 1)
        .await
        .unwrap();

    assert_eq!(stats.files_downloaded, 1);
    assert_eq!(stats.files_skipped, 1);
    assert_eq!(stats.total_bytes, total_bytes);
    assert_eq!(stats.network_bytes, total_bytes);
    assert!(tokio::fs::try_exists(&output_path).await.unwrap());

    harness.shutdown().await;
}
