#![cfg(feature = "tui")]

use std::sync::Arc;

use octo_dl::fake_mega::{BenchOptions, FakeMegaServer, create_fake_mega_fixture, run_bench};
use octo_dl::{DownloadConfig, DownloadProgress, Downloader, NoProgress};
use tempfile::tempdir;

#[tokio::test]
async fn public_link_round_trips_through_fake_server() {
    let temp = tempdir().unwrap();
    let fixture = create_fake_mega_fixture(temp.path(), "sample.bin", 131_231, 7)
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

    assert_eq!(nodes.len(), 1);
    assert_eq!(node.name(), fixture.file_name());
    assert_eq!(node.size(), fixture.size());
    assert_eq!(node.download_id(), Some(fixture.handle()));
    assert!(node.aes_iv().is_some());
    assert!(node.condensed_mac().is_some());

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn downloader_fetches_and_decrypts_fake_public_file() {
    let temp = tempdir().unwrap();
    let fixture_dir = temp.path().join("fixture");
    let output_dir = temp.path().join("output");
    let fixture = create_fake_mega_fixture(&fixture_dir, "payload.bin", 262_219, 19)
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
    let downloader = Downloader::new(
        client,
        DownloadConfig::default()
            .with_chunks_per_file(1)
            .with_concurrent_files(1)
            .with_force_overwrite(true),
    );
    let progress: Arc<dyn DownloadProgress> = Arc::new(NoProgress);
    let output_path = output_dir.join(fixture.file_name());
    let output_path_string = output_path.to_string_lossy().into_owned();

    downloader
        .download_file(node, &output_path_string, &progress, false, None)
        .await
        .unwrap();

    let actual = tokio::fs::read(&output_path).await.unwrap();
    let mut expected = vec![0u8; actual.len()];
    fixture.fill_plaintext(0, &mut expected);
    assert_eq!(actual, expected);

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn bench_run_verifies_condensed_mac() {
    let temp = tempdir().unwrap();
    let root_dir = temp.path().join("bench");
    let result = run_bench(&BenchOptions {
        root_dir: root_dir.clone(),
        file_name: "bench.bin".to_string(),
        size_bytes: 1_572_864,
        seed: 23,
        chunks_per_file: 2,
        server_worker_threads: 2,
        mega_chunks_per_request: 4,
    })
    .await
    .unwrap();

    assert_eq!(result.bytes, 1_572_864);
    assert!(result.public_url.starts_with("https://mega.nz/file/"));
    assert_eq!(
        result.output_path,
        root_dir.join("download").join("bench.bin")
    );
}
