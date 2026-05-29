use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use super::super::resume_state::CURRENT_RESUME_SIDECAR_VERSION;
use super::super::sidecar_store::{
    LegacyJsonVerifiedChunkRecord, ResumeSidecar, VerifiedChunkRecord,
};
use super::super::test_support::*;
use super::*;

#[test]
fn file_status_variants() {
    assert_ne!(FileStatus::Complete, FileStatus::Partial);
    assert_ne!(FileStatus::Partial, FileStatus::Missing);
    assert_ne!(FileStatus::Complete, FileStatus::Missing);
}

mod property_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn classify_force_overwrite_always_reports_missing(
            final_size in proptest::option::of(0u64..1_000_001),
            part_size in proptest::option::of(0u64..1_000_001),
            part_allocated_bytes in proptest::option::of(0u64..2_000_001),
            has_sidecar in any::<bool>(),
            verified_resume_bytes in 0u64..2_000_001,
            expected_size in 0u64..1_000_001,
        ) {
            let local = classify_observed_local_file(
                ObservedLocalFile {
                    final_size,
                    part_size,
                    part_allocated_bytes,
                    has_sidecar,
                    verified_resume_bytes,
                },
                expected_size,
                true,
            );

            prop_assert_eq!(local, InspectedLocalFile::default());
        }

        #[test]
        fn classify_matching_final_size_is_complete_when_not_force_overwrite(
            part_size in proptest::option::of(0u64..1_000_001),
            part_allocated_bytes in proptest::option::of(0u64..2_000_001),
            has_sidecar in any::<bool>(),
            verified_resume_bytes in 0u64..2_000_001,
            expected_size in 0u64..1_000_001,
        ) {
            let local = classify_observed_local_file(
                ObservedLocalFile {
                    final_size: Some(expected_size),
                    part_size,
                    part_allocated_bytes,
                    has_sidecar,
                    verified_resume_bytes,
                },
                expected_size,
                false,
            );

            prop_assert_eq!(local.status, FileStatus::Complete);
            prop_assert_eq!(local.existing_partial_bytes, 0);
            prop_assert!(!local.has_resume_sidecar);
            prop_assert_eq!(local.verified_resume_bytes, 0);
        }

        #[test]
        fn classify_partial_clamps_bytes_and_preserves_sidecar_flag(
            part_size in 0u64..1_000_001,
            expected_size in 0u64..1_000_001,
            part_allocated_bytes in proptest::option::of(0u64..2_000_001),
            has_sidecar in any::<bool>(),
            verified_resume_bytes in 0u64..2_000_001,
        ) {
            let local = classify_observed_local_file(
                ObservedLocalFile {
                    final_size: None,
                    part_size: Some(part_size),
                    part_allocated_bytes,
                    has_sidecar,
                    verified_resume_bytes,
                },
                expected_size,
                false,
            );

            let verified_clamped = verified_resume_bytes.min(part_size).min(expected_size);
            prop_assert_eq!(local.status, FileStatus::Partial);
            prop_assert_eq!(local.has_resume_sidecar, has_sidecar);
            prop_assert_eq!(local.verified_resume_bytes, verified_clamped);
            prop_assert!(local.existing_partial_bytes >= verified_clamped);
            prop_assert!(local.existing_partial_bytes <= part_size.min(expected_size));

            if part_allocated_bytes.is_none() && !has_sidecar {
                prop_assert_eq!(local.existing_partial_bytes, part_size.min(expected_size));
            }
        }
    }
}

#[tokio::test]
async fn classify_file_complete() {
    let fs = MockFileSystem::new();
    fs.add_file("movie.mkv", 1_000_000);
    let dl = mock_downloader(fs);
    assert_eq!(
        dl.inspect_local_file("movie.mkv", 1_000_000).await.status,
        FileStatus::Complete
    );
}

#[tokio::test]
async fn classify_file_size_mismatch_checks_part() {
    let fs = MockFileSystem::new();
    fs.add_file("movie.mkv", 500);
    let dl = mock_downloader(fs);
    assert_eq!(
        dl.inspect_local_file("movie.mkv", 1_000_000).await.status,
        FileStatus::Missing
    );
}

#[tokio::test]
async fn classify_file_partial() {
    let fs = MockFileSystem::new();
    fs.add_file("movie.mkv.part", 500_000);
    let dl = mock_downloader(fs);
    assert_eq!(
        dl.inspect_local_file("movie.mkv", 1_000_000).await.status,
        FileStatus::Partial
    );
}

#[tokio::test]
async fn classify_file_partial_detects_legacy_resume_sidecar() {
    let fs = MockFileSystem::new();
    fs.add_file("movie.mkv.part", 500_000);
    fs.add_file("movie.mkv.part.meta.json", 128);
    let dl = mock_downloader(fs);
    let local = dl.inspect_local_file("movie.mkv", 1_000_000).await;

    assert_eq!(local.status, FileStatus::Partial);
    assert!(local.has_resume_sidecar);
}

#[tokio::test]
async fn classify_file_partial_detects_legacy_binary_resume_sidecar() {
    let fs = MockFileSystem::new();
    fs.add_file("movie.mkv.part", 500_000);
    fs.add_file("movie.mkv.part.meta.bin", 128);
    let dl = mock_downloader(fs);
    let local = dl.inspect_local_file("movie.mkv", 1_000_000).await;

    assert_eq!(local.status, FileStatus::Partial);
    assert!(local.has_resume_sidecar);
}

#[tokio::test]
async fn classify_file_missing() {
    let fs = MockFileSystem::new();
    let dl = mock_downloader(fs);
    assert_eq!(
        dl.inspect_local_file("movie.mkv", 1_000_000).await.status,
        FileStatus::Missing
    );
}

#[tokio::test]
async fn classify_file_force_overwrite() {
    let fs = MockFileSystem::new();
    fs.add_file("movie.mkv", 1_000_000);
    let dl = mock_downloader_force(fs);
    assert_eq!(
        dl.inspect_local_file("movie.mkv", 1_000_000).await.status,
        FileStatus::Missing
    );
}

#[tokio::test]
async fn inspect_local_file_clamps_binary_sidecar_verified_bytes_to_part_size() {
    let dir = tempfile::tempdir().unwrap();
    let path_str = dir.path().join("movie.mkv").to_string_lossy().to_string();
    let file_size = 300_000_u64;
    let boundaries = mega::mega_chunk_boundaries(file_size);
    tokio::fs::write(
        part_path(&path_str),
        vec![0u8; usize_from_u64(boundaries[0].length)],
    )
    .await
    .unwrap();
    let sidecar = ResumeSidecar {
        version: CURRENT_RESUME_SIDECAR_VERSION,
        file_size,
        expected_condensed_mac: [9u8; 8],
        verified_chunks: vec![
            VerifiedChunkRecord {
                index: boundaries[0].index,
                mac: [1u8; 16],
            },
            VerifiedChunkRecord {
                index: boundaries[1].index,
                mac: [2u8; 16],
            },
        ],
        part_fingerprint: None,
    };
    tokio::fs::write(
        &sidecar_path(&path_str),
        postcard::to_stdvec(&sidecar).unwrap(),
    )
    .await
    .unwrap();

    let local = tokio_downloader()
        .inspect_local_file(&path_str, file_size)
        .await;

    assert_eq!(local.status, FileStatus::Partial);
    assert_eq!(local.existing_partial_bytes, boundaries[0].length);
    assert_eq!(local.verified_resume_bytes, boundaries[0].length);
}

#[tokio::test]
async fn inspect_local_file_uses_allocated_bytes_for_preallocated_sparse_part() {
    let fs = MockFileSystem::new();
    let expected_size = 1_000_000;
    fs.add_file("movie.mkv.part", expected_size);
    fs.add_fingerprint(
        "movie.mkv.part",
        fingerprint_with_allocated_bytes(expected_size, Some(64 * 1024)),
    );
    let dl = mock_downloader(fs);

    let local = dl.inspect_local_file("movie.mkv", expected_size).await;

    assert_eq!(local.status, FileStatus::Partial);
    assert_eq!(local.existing_partial_bytes, 64 * 1024);
    assert_eq!(local.verified_resume_bytes, 0);
}

#[tokio::test]
async fn inspect_local_file_reports_exact_sized_part_as_existing_bytes_without_sidecar() {
    let fs = MockFileSystem::new();
    let expected_size = 1_000_000;
    fs.add_file("movie.mkv.part", expected_size);
    let dl = mock_downloader(fs);

    let local = dl.inspect_local_file("movie.mkv", expected_size).await;

    assert_eq!(local.status, FileStatus::Partial);
    assert_eq!(local.existing_partial_bytes, expected_size);
    assert_eq!(local.verified_resume_bytes, 0);
}

#[tokio::test]
async fn inspect_local_file_reports_oversized_part_as_expected_existing_bytes_without_sidecar() {
    let fs = MockFileSystem::new();
    let expected_size = 1_000_000;
    fs.add_file("movie.mkv.part", expected_size + 512);
    let dl = mock_downloader(fs);

    let local = dl.inspect_local_file("movie.mkv", expected_size).await;

    assert_eq!(local.status, FileStatus::Partial);
    assert_eq!(local.existing_partial_bytes, expected_size);
    assert_eq!(local.verified_resume_bytes, 0);
}

#[tokio::test]
async fn inspect_local_file_clamps_legacy_sidecar_verified_bytes_to_part_size() {
    let dir = tempfile::tempdir().unwrap();
    let path_str = dir.path().join("movie.mkv").to_string_lossy().to_string();
    let file_size = 300_000_u64;
    let boundaries = mega::mega_chunk_boundaries(file_size);
    tokio::fs::write(
        part_path(&path_str),
        vec![0u8; usize_from_u64(boundaries[0].length)],
    )
    .await
    .unwrap();
    let mut legacy =
        legacy_json_sidecar_for_chunk(file_size, [9u8; 8], boundaries[0].index, [1u8; 16]);
    legacy.verified_chunks = vec![
        LegacyJsonVerifiedChunkRecord {
            index: boundaries[0].index,
            mac_b64: STANDARD.encode([1u8; 16]),
        },
        LegacyJsonVerifiedChunkRecord {
            index: boundaries[1].index,
            mac_b64: STANDARD.encode([2u8; 16]),
        },
    ];
    write_legacy_json_sidecar(&legacy_json_sidecar_path(&path_str), &legacy)
        .await
        .unwrap();

    let local = tokio_downloader()
        .inspect_local_file(&path_str, file_size)
        .await;

    assert_eq!(local.status, FileStatus::Partial);
    assert_eq!(local.existing_partial_bytes, boundaries[0].length);
    assert_eq!(local.verified_resume_bytes, boundaries[0].length);
}
