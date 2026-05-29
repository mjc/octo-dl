use std::sync::atomic::Ordering;

use super::super::test_support::*;
use super::super::{load_sidecar, part_path, save_sidecar_atomic, sidecar_path};
use super::*;
use crate::config::DownloadConfig;
use crate::fake_mega::{FakeMegaServer, create_fake_mega_fixture};
use crate::fs::{FileSystem, TokioFileSystem};

async fn run_restart_revalidation_and_manual_reverify_parity_test() {
    let temp = tempfile::tempdir().unwrap();
    let fixture_dir = temp.path().join("fixture");
    let output_dir = temp.path().join("output");
    let fixture = create_fake_mega_fixture(&fixture_dir, "payload.bin", 300_000, 19)
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
            .with_concurrent_files(1),
    );
    let output_path = output_dir.join(fixture.file_name());
    let output_path_string = output_path.to_string_lossy().into_owned();
    let part_path = part_path(&output_path_string);
    let sidecar_path = sidecar_path(&output_path_string);
    let boundaries = mega::mega_chunk_boundaries(node.size());
    let first = boundaries[0];
    let mut first_chunk = vec![0u8; usize_from_u64(first.length)];
    fixture.fill_plaintext(first.offset, &mut first_chunk);
    tokio::fs::write(&part_path, &first_chunk).await.unwrap();

    let mut sidecar = sidecar_for_chunk(
        node.size(),
        *node.condensed_mac().unwrap(),
        first.index,
        mega::compute_mega_chunk_mac(&first_chunk, node.aes_key(), node.aes_iv().unwrap()),
    );
    sidecar.part_fingerprint = TokioFileSystem::new().file_fingerprint(&part_path).await;
    save_sidecar_atomic(&sidecar_path, &sidecar).await.unwrap();

    let manual = downloader
        .reverify_resume_file(node, &output_path_string)
        .await
        .unwrap();
    let automatic = downloader
        .revalidate_resume_chunks(
            node,
            &boundaries,
            &part_path,
            &sidecar_path,
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

    server.shutdown().await.unwrap();
}

#[test]
fn automatic_restart_revalidation_and_manual_reverify_agree_for_matching_sidecar_and_part() {
    std::thread::Builder::new()
        .name("resume-parity-test".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(run_restart_revalidation_and_manual_reverify_parity_test());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn run_manual_reverify_refreshes_sidecar_fingerprint_test() {
    let temp = tempfile::tempdir().unwrap();
    let fixture_dir = temp.path().join("fixture");
    let output_dir = temp.path().join("output");
    let fixture = create_fake_mega_fixture(&fixture_dir, "payload.bin", 300_000, 23)
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
    let part_path = part_path(&output_path_string);
    let sidecar_path = sidecar_path(&output_path_string);
    let boundaries = mega::mega_chunk_boundaries(node.size());
    let first = boundaries[0];
    let mut first_chunk = vec![0u8; usize_from_u64(first.length)];
    fixture.fill_plaintext(first.offset, &mut first_chunk);
    tokio::fs::write(&part_path, &first_chunk).await.unwrap();

    let current_fingerprint = TokioFileSystem::new()
        .file_fingerprint(&part_path)
        .await
        .unwrap();
    let mut stale_fingerprint = current_fingerprint;
    stale_fingerprint.len = stale_fingerprint.len.saturating_add(1);
    let mut sidecar = sidecar_for_chunk(
        node.size(),
        *node.condensed_mac().unwrap(),
        first.index,
        mega::compute_mega_chunk_mac(&first_chunk, node.aes_key(), node.aes_iv().unwrap()),
    );
    sidecar.part_fingerprint = Some(stale_fingerprint);
    save_sidecar_atomic(&sidecar_path, &sidecar).await.unwrap();

    let result = downloader
        .reverify_resume_file(node, &output_path_string)
        .await
        .unwrap();
    assert_eq!(result.chunks, 1);
    assert_eq!(result.bytes, first.length);

    let refreshed = load_sidecar(&sidecar_path)
        .await
        .expect("manual reverify should leave a sidecar behind");
    assert_eq!(
        refreshed.part_fingerprint,
        Some(current_fingerprint),
        "manual reverify should refresh the sidecar fingerprint to the current .part state"
    );

    server.shutdown().await.unwrap();
}

#[test]
fn manual_reverify_refreshes_sidecar_fingerprint_after_disk_revalidation() {
    std::thread::Builder::new()
        .name("manual-reverify-fingerprint-test".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(run_manual_reverify_refreshes_sidecar_fingerprint_test());
        })
        .unwrap()
        .join()
        .unwrap();
}

#[tokio::test]
async fn manual_reverify_with_progress_reports_disk_validation_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let fixture_dir = temp.path().join("fixture");
    let output_dir = temp.path().join("output");
    let fixture = create_fake_mega_fixture(&fixture_dir, "payload.bin", 300_000, 31)
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
    let part_path = part_path(&output_path_string);
    let sidecar_path = sidecar_path(&output_path_string);
    let boundaries = mega::mega_chunk_boundaries(node.size());
    let first = boundaries[0];
    let mut first_chunk = vec![0u8; usize_from_u64(first.length)];
    fixture.fill_plaintext(first.offset, &mut first_chunk);
    tokio::fs::write(&part_path, &first_chunk).await.unwrap();

    let current_fingerprint = TokioFileSystem::new()
        .file_fingerprint(&part_path)
        .await
        .unwrap();
    let mut stale_fingerprint = current_fingerprint;
    stale_fingerprint.len = stale_fingerprint.len.saturating_add(1);
    let mut sidecar = sidecar_for_chunk(
        node.size(),
        *node.condensed_mac().unwrap(),
        first.index,
        mega::compute_mega_chunk_mac(&first_chunk, node.aes_key(), node.aes_iv().unwrap()),
    );
    sidecar.part_fingerprint = Some(stale_fingerprint);
    save_sidecar_atomic(&sidecar_path, &sidecar).await.unwrap();

    let progress = RecordingProgress::default();
    let result = downloader
        .reverify_resume_file_with_progress(node, &output_path_string, Some(&progress))
        .await
        .unwrap();

    assert_eq!(result.chunks, 1);
    assert_eq!(result.bytes, first.length);
    assert_eq!(progress.validation_starts.load(Ordering::SeqCst), 1);
    assert!(progress.calls.load(Ordering::SeqCst) > 0);
    assert_eq!(progress.total.load(Ordering::SeqCst), first.length);
    assert_eq!(progress.network.load(Ordering::SeqCst), 0);

    server.shutdown().await.unwrap();
}
