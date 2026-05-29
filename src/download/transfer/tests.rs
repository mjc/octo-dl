use std::sync::Arc;

use super::super::NoProgress;
use super::super::test_support::*;
use super::*;
use crate::config::DownloadConfig;
use crate::fake_mega::{FakeMegaServer, create_fake_mega_fixture};

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
    let temp = tempfile::tempdir().unwrap();
    let fixture_dir = temp.path().join("fixture");
    let output_dir = temp.path().join("output");
    let fixture = create_fake_mega_fixture(&fixture_dir, "payload.bin", 262_219, 43)
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
    let output_path = output_dir
        .join("nested")
        .join("leaf")
        .join(fixture.file_name());
    let output_path_string = output_path.to_string_lossy().into_owned();

    let stats = downloader
        .download_file(node, &output_path_string, &progress, false, None)
        .await
        .unwrap();

    let actual = tokio::fs::read(&output_path).await.unwrap();
    let mut expected = vec![0u8; actual.len()];
    fixture.fill_plaintext(0, &mut expected);
    assert_eq!(actual, expected);
    assert_eq!(stats.size, node.size());
    assert_eq!(stats.network_bytes, node.size());
    assert!(
        tokio::fs::try_exists(output_path.parent().unwrap())
            .await
            .unwrap()
    );

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn download_file_short_circuits_for_verified_existing_output() {
    let temp = tempfile::tempdir().unwrap();
    let fixture_dir = temp.path().join("fixture");
    let output_dir = temp.path().join("output");
    let fixture = create_fake_mega_fixture(&fixture_dir, "payload.bin", 300_000, 47)
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
    let progress: Arc<dyn DownloadProgress> = Arc::new(NoProgress);
    let output_path = output_dir.join(fixture.file_name());
    let output_path_string = output_path.to_string_lossy().into_owned();
    let mut expected = vec![0u8; usize_from_u64(node.size())];
    fixture.fill_plaintext(0, &mut expected);
    tokio::fs::write(&output_path, &expected).await.unwrap();

    let stats = downloader
        .download_file(node, &output_path_string, &progress, false, None)
        .await
        .unwrap();

    assert_eq!(stats.size, node.size());
    assert_eq!(stats.network_bytes, 0);
    assert_eq!(stats.reused_bytes, 0);
    assert!(
        !tokio::fs::try_exists(part_path(&output_path_string))
            .await
            .unwrap()
    );

    server.shutdown().await.unwrap();
}
