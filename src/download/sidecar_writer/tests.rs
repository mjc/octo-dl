use super::super::{VerifiedChunkRecord, load_sidecar};
use crate::download::{CURRENT_RESUME_SIDECAR_VERSION, part_path, sidecar_path};
use crate::fs::{FileFingerprint, FileSystem, TokioFileSystem};

use super::{
    LazySidecarWriter, ResumeSidecar, SidecarGeneration, SidecarWriterShutdown,
    fingerprint_part_sync,
};

#[tokio::test]
async fn sidecar_writer_persists_verified_snapshots_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let sidecar_path = dir.path().join("file.bin.part.postcard");
    let part_path = dir.path().join("file.bin.part");
    tokio::fs::write(&part_path, b"partial").await.unwrap();
    let writer = LazySidecarWriter::new(sidecar_path.clone(), part_path.clone());
    let first = ResumeSidecar {
        version: CURRENT_RESUME_SIDECAR_VERSION,
        file_size: 42,
        expected_condensed_mac: [9u8; 8],
        verified_chunks: vec![VerifiedChunkRecord {
            index: 0,
            mac: [1u8; 16],
        }],
        part_fingerprint: None,
    };
    let second = ResumeSidecar {
        verified_chunks: vec![
            VerifiedChunkRecord {
                index: 0,
                mac: [1u8; 16],
            },
            VerifiedChunkRecord {
                index: 1,
                mac: [2u8; 16],
            },
        ],
        ..first.clone()
    };

    writer.persist_verified_snapshot(SidecarGeneration::new(1), first);
    writer.persist_verified_snapshot(SidecarGeneration::new(2), second.clone());
    writer.finish(SidecarWriterShutdown::Flush).await;

    let loaded = load_sidecar(&sidecar_path).await.unwrap();
    assert_eq!(loaded.verified_chunks, second.verified_chunks);
    assert_eq!(
        loaded.part_fingerprint,
        TokioFileSystem::new().file_fingerprint(&part_path).await
    );
}

#[tokio::test]
async fn sidecar_writer_saves_snapshot_without_fingerprint_when_part_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let sidecar_path = dir.path().join("file.bin.part.postcard");
    let part_path = dir.path().join("file.bin.part");
    let writer = LazySidecarWriter::new(sidecar_path.clone(), part_path.clone());
    let snapshot = ResumeSidecar {
        version: CURRENT_RESUME_SIDECAR_VERSION,
        file_size: 42,
        expected_condensed_mac: [9u8; 8],
        verified_chunks: vec![VerifiedChunkRecord {
            index: 0,
            mac: [1u8; 16],
        }],
        part_fingerprint: Some(FileFingerprint {
            len: 999,
            modified_ns: 999,
            allocated_bytes: Some(999),
            dev: None,
            ino: None,
        }),
    };

    writer.persist_verified_snapshot(SidecarGeneration::new(1), snapshot);
    writer.finish(SidecarWriterShutdown::Flush).await;

    let loaded = load_sidecar(&sidecar_path).await.unwrap();
    assert_eq!(loaded.verified_chunks.len(), 1);
    assert_eq!(loaded.part_fingerprint, None);
}

#[tokio::test]
async fn sidecar_writer_allows_equal_generation_for_final_flush() {
    let dir = tempfile::tempdir().unwrap();
    let sidecar_path = dir.path().join("file.bin.part.postcard");
    let part_path = dir.path().join("file.bin.part");
    tokio::fs::write(&part_path, b"partial").await.unwrap();
    let writer = LazySidecarWriter::new(sidecar_path.clone(), part_path);
    let first = ResumeSidecar {
        version: CURRENT_RESUME_SIDECAR_VERSION,
        file_size: 42,
        expected_condensed_mac: [9u8; 8],
        verified_chunks: vec![VerifiedChunkRecord {
            index: 0,
            mac: [1u8; 16],
        }],
        part_fingerprint: None,
    };
    let final_snapshot = ResumeSidecar {
        verified_chunks: vec![
            VerifiedChunkRecord {
                index: 0,
                mac: [1u8; 16],
            },
            VerifiedChunkRecord {
                index: 1,
                mac: [2u8; 16],
            },
        ],
        ..first.clone()
    };

    writer.persist_verified_snapshot(SidecarGeneration::new(2), first);
    writer.persist_final_snapshot(SidecarGeneration::new(2), final_snapshot.clone());
    writer.finish(SidecarWriterShutdown::Flush).await;

    let loaded = load_sidecar(&sidecar_path).await.unwrap();
    assert_eq!(loaded.verified_chunks, final_snapshot.verified_chunks);
}

#[tokio::test]
async fn sidecar_writer_rejects_older_final_snapshot_after_newer_generation() {
    let dir = tempfile::tempdir().unwrap();
    let sidecar_path = dir.path().join("file.bin.part.postcard");
    let part_path = dir.path().join("file.bin.part");
    tokio::fs::write(&part_path, b"partial").await.unwrap();
    let writer = LazySidecarWriter::new(sidecar_path.clone(), part_path);
    let older = ResumeSidecar {
        version: CURRENT_RESUME_SIDECAR_VERSION,
        file_size: 42,
        expected_condensed_mac: [9u8; 8],
        verified_chunks: vec![VerifiedChunkRecord {
            index: 0,
            mac: [1u8; 16],
        }],
        part_fingerprint: None,
    };
    let newer = ResumeSidecar {
        verified_chunks: vec![
            VerifiedChunkRecord {
                index: 0,
                mac: [1u8; 16],
            },
            VerifiedChunkRecord {
                index: 1,
                mac: [2u8; 16],
            },
        ],
        ..older.clone()
    };

    writer.persist_verified_snapshot(SidecarGeneration::new(3), newer.clone());
    writer.persist_final_snapshot(SidecarGeneration::new(2), older);
    writer.finish(SidecarWriterShutdown::Flush).await;

    let loaded = load_sidecar(&sidecar_path).await.unwrap();
    assert_eq!(loaded.verified_chunks, newer.verified_chunks);
}

#[tokio::test]
async fn sidecar_writer_rejects_older_verified_snapshot_after_newer_generation() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("file.bin");
    let file_path = file_path.to_string_lossy().into_owned();
    let part_path = part_path(&file_path);
    let sidecar_path = sidecar_path(&file_path);
    tokio::fs::write(&part_path, b"partial").await.unwrap();
    let writer = LazySidecarWriter::new(sidecar_path.clone(), part_path);
    let first_snapshot = ResumeSidecar {
        version: CURRENT_RESUME_SIDECAR_VERSION,
        file_size: 300_000,
        expected_condensed_mac: [9_u8; 8],
        verified_chunks: vec![VerifiedChunkRecord {
            index: 1,
            mac: [1_u8; 16],
        }],
        part_fingerprint: None,
    };
    let second_snapshot = ResumeSidecar {
        verified_chunks: vec![
            VerifiedChunkRecord {
                index: 1,
                mac: [1_u8; 16],
            },
            VerifiedChunkRecord {
                index: 2,
                mac: [2_u8; 16],
            },
        ],
        ..first_snapshot.clone()
    };

    writer.persist_verified_snapshot(SidecarGeneration::new(2), second_snapshot);
    writer.persist_verified_snapshot(SidecarGeneration::new(1), first_snapshot);
    writer.finish(SidecarWriterShutdown::Flush).await;

    let loaded = load_sidecar(&sidecar_path)
        .await
        .expect("sidecar should be present after both writes");
    assert_eq!(
        loaded.verified_chunks.len(),
        2,
        "an older snapshot must not overwrite newer trusted-chunk state"
    );
    assert_eq!(loaded.verified_chunks[0].index, 1);
    assert_eq!(loaded.verified_chunks[0].mac, [1_u8; 16]);
    assert_eq!(loaded.verified_chunks[1].index, 2);
    assert_eq!(loaded.verified_chunks[1].mac, [2_u8; 16]);
}

#[tokio::test]
async fn sidecar_writer_ignores_persist_requests_after_finish() {
    let dir = tempfile::tempdir().unwrap();
    let sidecar_path = dir.path().join("file.bin.part.postcard");
    let part_path = dir.path().join("file.bin.part");
    tokio::fs::write(&part_path, b"partial").await.unwrap();
    let writer = LazySidecarWriter::new(sidecar_path.clone(), part_path);
    let first = ResumeSidecar {
        version: CURRENT_RESUME_SIDECAR_VERSION,
        file_size: 42,
        expected_condensed_mac: [9u8; 8],
        verified_chunks: vec![VerifiedChunkRecord {
            index: 0,
            mac: [1u8; 16],
        }],
        part_fingerprint: None,
    };
    let second = ResumeSidecar {
        verified_chunks: vec![
            VerifiedChunkRecord {
                index: 0,
                mac: [1u8; 16],
            },
            VerifiedChunkRecord {
                index: 1,
                mac: [2u8; 16],
            },
        ],
        ..first.clone()
    };

    writer.persist_verified_snapshot(SidecarGeneration::new(1), first.clone());
    writer.finish(SidecarWriterShutdown::Flush).await;
    writer.persist_verified_snapshot(SidecarGeneration::new(2), second);

    let loaded = load_sidecar(&sidecar_path).await.unwrap();
    assert_eq!(loaded.verified_chunks, first.verified_chunks);
}

#[tokio::test]
async fn sync_and_fingerprint_part_reports_missing_files_as_untrusted() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.part");

    assert_eq!(fingerprint_part_sync(&missing), None);
}
