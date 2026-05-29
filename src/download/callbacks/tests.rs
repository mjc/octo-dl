use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use super::super::{load_sidecar, wait_for_persist_event};
use super::{
    ChunkVerifiedState, DownloadCallbackState, DownloadProgress, LazySidecarWriter, NoProgress,
    ProgressCallbackState, ResumeTracker, ResumeValidationStatusProgress, SidecarWriterShutdown,
};
use crate::core::ProgressDelta;
use crate::download::{part_path, sidecar_path};

#[derive(Default)]
struct RecordingProgress {
    calls: AtomicUsize,
    validation_calls: AtomicUsize,
    validation_checked: AtomicU64,
}

impl DownloadProgress for RecordingProgress {
    fn on_resume_validation_progress(&self, _name: &str, checked_bytes: u64, _total_bytes: u64) {
        self.validation_calls.fetch_add(1, Ordering::SeqCst);
        self.validation_checked
            .store(checked_bytes, Ordering::SeqCst);
    }

    fn on_progress(&self, _name: &str, _delta: ProgressDelta) {
        self.calls.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn no_progress_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<NoProgress>();
}

#[test]
fn download_callbacks_request_durable_chunk_syncs() {
    let callbacks = DownloadCallbackState::new(
        ProgressCallbackState::new("file.bin".to_string(), 1024, 0, Arc::new(NoProgress)),
        ChunkVerifiedState::new(
            ResumeTracker::new(1024, [0; 8], vec![None; 1]),
            LazySidecarWriter::new("file.bin.part.postcard".into(), "file.bin.part".into()),
        ),
    );

    assert!(mega::ParallelDownloadCallbacks::tracks_chunk_verification(
        &callbacks
    ));
}

#[test]
fn resume_validation_status_progress_suppresses_regular_progress() {
    let progress = RecordingProgress::default();
    let wrapper = ResumeValidationStatusProgress::new(&progress);

    wrapper.on_progress(
        "file.bin",
        ProgressDelta {
            total_bytes_delta: 64,
            network_bytes_delta: 0,
        },
    );
    wrapper.on_resume_validation_progress("file.bin", 25, 100);

    assert_eq!(progress.calls.load(Ordering::SeqCst), 0);
    assert_eq!(progress.validation_calls.load(Ordering::SeqCst), 1);
    assert_eq!(progress.validation_checked.load(Ordering::SeqCst), 25);
}

#[tokio::test]
async fn chunk_verified_persists_sidecar_after_each_chunk() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("file.bin");
    let file_path = file_path.to_string_lossy().into_owned();
    let part_path = part_path(&file_path);
    let sidecar_path = sidecar_path(&file_path);
    let file_size = 300_000_u64;
    let boundaries = mega::mega_chunk_boundaries(file_size);
    tokio::fs::write(&part_path, vec![0_u8; usize::try_from(file_size).unwrap()])
        .await
        .unwrap();

    let writer = LazySidecarWriter::new(sidecar_path.clone(), part_path.clone());
    let persist_events = writer.persist_event_listener();
    let verified = ChunkVerifiedState::new(
        ResumeTracker::new(file_size, [9_u8; 8], vec![None; boundaries.len()]),
        writer,
    );

    verified.mark_verified(boundaries[0].index, [1_u8; 16]);
    assert!(wait_for_persist_event(persist_events.clone()).await);
    let first = load_sidecar(&sidecar_path)
        .await
        .expect("first verified chunk should be persisted");
    assert_eq!(first.verified_chunks.len(), 1);
    assert_eq!(first.verified_chunks[0].index, boundaries[0].index);
    assert_eq!(first.verified_chunks[0].mac, [1_u8; 16]);
    assert!(first.part_fingerprint.is_some());

    verified.mark_verified(boundaries[1].index, [2_u8; 16]);
    assert!(wait_for_persist_event(persist_events).await);
    let second = load_sidecar(&sidecar_path)
        .await
        .expect("second verified chunk should be persisted");
    assert_eq!(second.verified_chunks.len(), 2);
    assert_eq!(second.verified_chunks[0].index, boundaries[0].index);
    assert_eq!(second.verified_chunks[0].mac, [1_u8; 16]);
    assert_eq!(second.verified_chunks[1].index, boundaries[1].index);
    assert_eq!(second.verified_chunks[1].mac, [2_u8; 16]);
    assert!(second.part_fingerprint.is_some());

    verified
        .finish_sidecar_writer(SidecarWriterShutdown::Flush)
        .await;
}

#[tokio::test]
async fn chunk_verified_flush_persists_queued_sidecar_updates() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("file.bin");
    let file_path = file_path.to_string_lossy().into_owned();
    let part_path = part_path(&file_path);
    let sidecar_path = sidecar_path(&file_path);
    let file_size = 300_000_u64;
    let boundaries = mega::mega_chunk_boundaries(file_size);
    tokio::fs::write(&part_path, vec![0_u8; usize::try_from(file_size).unwrap()])
        .await
        .unwrap();

    let verified = ChunkVerifiedState::new(
        ResumeTracker::new(file_size, [9_u8; 8], vec![None; boundaries.len()]),
        LazySidecarWriter::new(sidecar_path.clone(), part_path.clone()),
    );

    verified.mark_verified(boundaries[0].index, [1_u8; 16]);
    verified.mark_verified(boundaries[1].index, [2_u8; 16]);
    verified
        .finish_sidecar_writer(SidecarWriterShutdown::Flush)
        .await;

    let first = load_sidecar(&sidecar_path)
        .await
        .expect("queued verified chunks should persist when the writer flushes");
    assert_eq!(first.verified_chunks[0].index, boundaries[0].index);
    assert_eq!(first.verified_chunks[0].mac, [1_u8; 16]);
    assert_eq!(first.verified_chunks[1].index, boundaries[1].index);
    assert_eq!(first.verified_chunks[1].mac, [2_u8; 16]);
    assert!(first.part_fingerprint.is_some());
}

#[tokio::test]
async fn chunk_verified_replaces_existing_sidecar_record_without_duplication() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("file.bin");
    let file_path = file_path.to_string_lossy().into_owned();
    let part_path = part_path(&file_path);
    let sidecar_path = sidecar_path(&file_path);
    tokio::fs::write(&part_path, vec![0_u8; 300_000])
        .await
        .unwrap();

    let verified = ChunkVerifiedState::new(
        ResumeTracker::new(300_000, [9_u8; 8], vec![None; 1]),
        LazySidecarWriter::new(sidecar_path.clone(), part_path),
    );

    verified.mark_verified(0, [1_u8; 16]);
    verified.mark_verified(0, [7_u8; 16]);
    verified
        .finish_sidecar_writer(SidecarWriterShutdown::Flush)
        .await;

    let sidecar = load_sidecar(&sidecar_path).await.unwrap();
    assert_eq!(sidecar.verified_chunks.len(), 1);
    assert_eq!(sidecar.verified_chunks[0].index, 0);
    assert_eq!(sidecar.verified_chunks[0].mac, [7_u8; 16]);
}

#[tokio::test]
async fn chunk_verified_flush_keeps_sidecar_chunks_sorted_after_out_of_order_marks() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("file.bin");
    let file_path = file_path.to_string_lossy().into_owned();
    let part_path = part_path(&file_path);
    let sidecar_path = sidecar_path(&file_path);
    let file_size = 3_000_000_u64;
    let boundaries = mega::mega_chunk_boundaries(file_size);
    tokio::fs::write(&part_path, vec![0_u8; usize::try_from(file_size).unwrap()])
        .await
        .unwrap();

    let verified = ChunkVerifiedState::new(
        ResumeTracker::new(file_size, [9_u8; 8], vec![None; boundaries.len()]),
        LazySidecarWriter::new(sidecar_path.clone(), part_path),
    );

    verified.mark_verified(boundaries[2].index, [3_u8; 16]);
    verified.mark_verified(boundaries[0].index, [1_u8; 16]);
    verified.mark_verified(boundaries[1].index, [2_u8; 16]);
    verified
        .finish_sidecar_writer(SidecarWriterShutdown::Flush)
        .await;

    let sidecar = load_sidecar(&sidecar_path).await.unwrap();
    assert_eq!(
        sidecar
            .verified_chunks
            .iter()
            .map(|record| (record.index, record.mac))
            .collect::<Vec<_>>(),
        vec![
            (boundaries[0].index, [1_u8; 16]),
            (boundaries[1].index, [2_u8; 16]),
            (boundaries[2].index, [3_u8; 16]),
        ]
    );
}
