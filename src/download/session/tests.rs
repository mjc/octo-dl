use std::sync::Arc;

use super::super::NoProgress;
use super::super::test_support::*;
use super::*;
use crate::config::DownloadConfig;
use crate::fake_mega::{FakeMegaServer, create_fake_mega_fixture};

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
    let temp = tempfile::tempdir().unwrap();
    let fixture_dir = temp.path().join("fixture");
    let output_dir = temp.path().join("output");
    let fixture = create_fake_mega_fixture(&fixture_dir, "payload.bin", 262_219, 37)
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
    let downloader = Downloader::new(
        client,
        DownloadConfig::default()
            .with_chunks_per_file(1)
            .with_concurrent_files(1)
            .with_force_overwrite(true),
    );
    let progress: Arc<dyn DownloadProgress> = Arc::new(NoProgress);
    let output_path = output_dir.join("nested").join(fixture.file_name());
    let output_path_string = output_path.to_string_lossy().into_owned();
    let files = [DownloadItem {
        path: output_path_string.clone(),
        node,
        was_partial: false,
    }];

    let stats = downloader.download_all(&files, &progress, 2).await.unwrap();

    assert_eq!(stats.files_downloaded, 1);
    assert_eq!(stats.files_skipped, 2);
    assert_eq!(stats.total_bytes, node.size());
    assert_eq!(stats.network_bytes, node.size());
    assert!(tokio::fs::try_exists(&output_path).await.unwrap());

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn download_all_owned_aggregates_successful_download_stats() {
    let temp = tempfile::tempdir().unwrap();
    let fixture_dir = temp.path().join("fixture");
    let output_dir = temp.path().join("output");
    let fixture = create_fake_mega_fixture(&fixture_dir, "payload.bin", 262_219, 41)
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
    let downloader = Downloader::new(
        client,
        DownloadConfig::default()
            .with_chunks_per_file(1)
            .with_concurrent_files(1)
            .with_force_overwrite(true),
    );
    let progress: Arc<dyn DownloadProgress> = Arc::new(NoProgress);
    let output_path = output_dir.join("owned").join(fixture.file_name());
    let output_path_string = output_path.to_string_lossy().into_owned();
    let files = [OwnedDownloadItem {
        path: output_path_string.clone(),
        node: node.clone(),
        was_partial: false,
    }];

    let stats = downloader
        .download_all_owned(&files, &progress, 1)
        .await
        .unwrap();

    assert_eq!(stats.files_downloaded, 1);
    assert_eq!(stats.files_skipped, 1);
    assert_eq!(stats.total_bytes, node.size());
    assert_eq!(stats.network_bytes, node.size());
    assert!(tokio::fs::try_exists(&output_path).await.unwrap());

    server.shutdown().await.unwrap();
}
