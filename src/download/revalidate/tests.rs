use std::path::PathBuf;

use super::super::revalidation_buffer::REVALIDATION_BUFFER_BYTES;
use super::super::test_support::*;
use super::super::{
    ResumeSidecar, VerifiedChunkRecord, resume_validation_percent, save_sidecar_atomic,
};
use super::*;
use crate::fs::{FileFingerprint, FileSystem, TokioFileSystem};

fn fingerprint_with_allocated_bytes(len: u64, allocated_bytes: Option<u64>) -> FileFingerprint {
    FileFingerprint {
        len,
        modified_ns: 123,
        allocated_bytes,
        dev: Some(7),
        ino: Some(11),
    }
}

mod property_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn resume_validation_percent_clamps_checked_bytes(
            checked_bytes in 0u64..2_000_000,
            total_bytes in 1u64..1_000_001,
        ) {
            let expected =
                ((u128::from(checked_bytes.min(total_bytes)) * 100) / u128::from(total_bytes)) as u64;
            let actual = resume_validation_percent(checked_bytes, total_bytes);
            prop_assert_eq!(actual, expected);
            prop_assert!(actual <= 100);
        }

        #[test]
        fn resume_validation_percent_is_zero_for_empty_total(
            checked_bytes in any::<u64>(),
        ) {
            prop_assert_eq!(resume_validation_percent(checked_bytes, 0), 0);
        }
    }
}

#[tokio::test]
async fn revalidate_sidecar_without_part_fingerprint_recomputes_from_part() {
    let dir = tempfile::tempdir().unwrap();
    let part = dir.path().join("file.bin.part");
    let file_size = 300_000_u64;
    let data = test_incompressible_plaintext(usize_from_u64(file_size));
    tokio::fs::write(&part, &data).await.unwrap();

    let expected = [9u8; 8];
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let first = &boundaries[0];
    let mac = mega::compute_mega_chunk_mac(chunk_data(&data, first), &TEST_AES_KEY, &TEST_AES_IV);
    let sidecar = sidecar_for_chunk(file_size, expected, first.index, mac);

    let validation = tokio_downloader()
        .revalidate_sidecar_chunks(
            SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac: expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            },
            None,
        )
        .await
        .unwrap();

    assert!(validation.sidecar_loaded);
    assert_eq!(validation.trusted_count, 1);
    assert_eq!(validation.trusted_bytes, first.length);
    assert_eq!(validation.trusted_chunks[0], Some(mac));
    assert!(validation.trusted_chunks[1].is_none());
}

#[tokio::test]
async fn revalidate_sidecar_trusts_matching_part_fingerprint_without_reread() {
    let part = PathBuf::from("file.bin.part");
    let file_size = 300_000_u64;

    let expected = [9u8; 8];
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let first = &boundaries[0];
    let mut sidecar = sidecar_for_chunk(file_size, expected, first.index, [4u8; 16]);
    let fingerprint = fingerprint_with_allocated_bytes(file_size, Some(first.length));
    sidecar.part_fingerprint = Some(fingerprint);
    let fs = MockFileSystem::new();
    fs.add_file(&part, file_size);
    fs.add_fingerprint(&part, fingerprint);

    let validation = mock_downloader(fs)
        .revalidate_sidecar_chunks(
            SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac: expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            },
            None,
        )
        .await
        .unwrap();

    assert_eq!(validation.trusted_count, 1);
    assert_eq!(validation.trusted_bytes, first.length);
    assert_eq!(validation.trusted_chunks[0], Some([4u8; 16]));
}

#[tokio::test]
async fn revalidate_sidecar_trusts_matching_fingerprint_without_allocated_bytes() {
    let part = PathBuf::from("file.bin.part");
    let file_size = 300_000_u64;
    let expected = [9u8; 8];
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let first = &boundaries[0];
    let mut sidecar = sidecar_for_chunk(file_size, expected, first.index, [4u8; 16]);
    let fingerprint = fingerprint_with_allocated_bytes(file_size, None);
    sidecar.part_fingerprint = Some(fingerprint);
    let fs = MockFileSystem::new();
    fs.add_file(&part, file_size);
    fs.add_fingerprint(&part, fingerprint);

    let validation = mock_downloader(fs)
        .revalidate_sidecar_chunks(
            SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac: expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            },
            None,
        )
        .await
        .unwrap();

    assert_eq!(validation.trusted_count, 1);
    assert_eq!(validation.trusted_bytes, first.length);
    assert_eq!(validation.trusted_chunks[0], Some([4u8; 16]));
}

#[tokio::test]
async fn revalidate_sidecar_recomputes_old_fingerprint_chunk_from_part() {
    let dir = tempfile::tempdir().unwrap();
    let part = dir.path().join("file.bin.part");
    let file_size = 300_000_u64;
    let data = test_incompressible_plaintext(usize_from_u64(file_size));
    tokio::fs::write(&part, &data).await.unwrap();

    let expected = [9u8; 8];
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let first = &boundaries[0];
    let mac = mega::compute_mega_chunk_mac(chunk_data(&data, first), &TEST_AES_KEY, &TEST_AES_IV);
    let mut sidecar = sidecar_for_chunk(file_size, expected, first.index, mac);
    sidecar.part_fingerprint = TokioFileSystem::new().file_fingerprint(&part).await;
    if let Some(fingerprint) = sidecar.part_fingerprint.as_mut() {
        fingerprint.allocated_bytes = None;
    }

    let validation = tokio_downloader()
        .revalidate_sidecar_chunks(
            SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac: expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            },
            None,
        )
        .await
        .unwrap();

    assert_eq!(validation.trusted_count, 1);
    assert_eq!(validation.trusted_bytes, first.length);
    assert_eq!(
        validation.trusted_chunks[usize_from_u32(first.index)],
        Some(mac)
    );
    assert_eq!(validation.source, Some(ResumeReuseSource::Sidecar));
}

#[tokio::test]
async fn revalidate_sidecar_recomputes_chunk_index_at_its_offset() {
    let dir = tempfile::tempdir().unwrap();
    let part = dir.path().join("file.bin.part");
    let file_size = 300_000_u64;
    let mut data = test_incompressible_plaintext(usize_from_u64(file_size));
    data[..32].fill(0);
    tokio::fs::write(&part, &data).await.unwrap();

    let expected = [9u8; 8];
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let second = &boundaries[1];
    let mac = mega::compute_mega_chunk_mac(chunk_data(&data, second), &TEST_AES_KEY, &TEST_AES_IV);
    let mut sidecar = sidecar_for_chunk(file_size, expected, second.index, mac);
    sidecar.part_fingerprint = TokioFileSystem::new().file_fingerprint(&part).await;
    if let Some(fingerprint) = sidecar.part_fingerprint.as_mut() {
        fingerprint.allocated_bytes = None;
    }

    let validation = tokio_downloader()
        .revalidate_sidecar_chunks(
            SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac: expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            },
            None,
        )
        .await
        .unwrap();

    assert_eq!(validation.trusted_count, 1);
    assert_eq!(validation.trusted_bytes, second.length);
    assert_eq!(validation.trusted_chunks[0], None);
    assert_eq!(
        validation.trusted_chunks[usize_from_u32(second.index)],
        Some(mac)
    );
}

#[tokio::test]
async fn revalidate_sidecar_trusts_matching_fingerprint_even_with_low_allocation_hint() {
    let part = PathBuf::from("file.bin.part");
    let file_size = 300_000_u64;
    let expected = [9u8; 8];
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let first = &boundaries[0];
    let second = &boundaries[1];
    let mut sidecar = ResumeSidecar {
        version: CURRENT_RESUME_SIDECAR_VERSION,
        file_size,
        expected_condensed_mac: expected,
        verified_chunks: vec![
            VerifiedChunkRecord {
                index: first.index,
                mac: [4u8; 16],
            },
            VerifiedChunkRecord {
                index: second.index,
                mac: [5u8; 16],
            },
        ]
        .into(),
        part_fingerprint: None,
    };
    let allocated = first.length.saturating_add(second.length).saturating_sub(1);
    let fingerprint = fingerprint_with_allocated_bytes(file_size, Some(allocated));
    sidecar.part_fingerprint = Some(fingerprint);
    let fs = MockFileSystem::new();
    fs.add_file(&part, file_size);
    fs.add_fingerprint(&part, fingerprint);

    let validation = mock_downloader(fs)
        .revalidate_sidecar_chunks(
            SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac: expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            },
            None,
        )
        .await
        .unwrap();

    assert_eq!(validation.trusted_count, 2);
    assert_eq!(validation.trusted_bytes, first.length + second.length);
    assert_eq!(validation.trusted_chunks[0], Some([4u8; 16]));
    assert_eq!(validation.trusted_chunks[1], Some([5u8; 16]));
}

#[tokio::test]
async fn revalidate_sidecar_trusts_multiple_chunks_without_allocated_bytes() {
    let part = PathBuf::from("file.bin.part");
    let file_size = 300_000_u64;
    let expected = [9u8; 8];
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let first = &boundaries[0];
    let second = &boundaries[1];
    let sidecar = ResumeSidecar {
        version: CURRENT_RESUME_SIDECAR_VERSION,
        file_size,
        expected_condensed_mac: expected,
        verified_chunks: vec![
            VerifiedChunkRecord {
                index: first.index,
                mac: [4u8; 16],
            },
            VerifiedChunkRecord {
                index: second.index,
                mac: [5u8; 16],
            },
        ]
        .into(),
        part_fingerprint: Some(fingerprint_with_allocated_bytes(file_size, None)),
    };
    let fs = MockFileSystem::new();
    fs.add_file(&part, file_size);
    fs.add_fingerprint(&part, fingerprint_with_allocated_bytes(file_size, Some(1)));

    let validation = mock_downloader(fs)
        .revalidate_sidecar_chunks(
            SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac: expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            },
            None,
        )
        .await
        .unwrap();

    assert_eq!(validation.trusted_count, 2);
    assert_eq!(validation.trusted_bytes, first.length + second.length);
    assert_eq!(validation.trusted_chunks[0], Some([4u8; 16]));
    assert_eq!(validation.trusted_chunks[1], Some([5u8; 16]));
}

#[tokio::test]
async fn revalidate_sidecar_rejects_matching_allocation_when_device_changes() {
    let part = PathBuf::from("file.bin.part");
    let file_size = 300_000_u64;
    let expected = [9u8; 8];
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let first = &boundaries[0];
    let mut actual = fingerprint_with_allocated_bytes(file_size, Some(first.length));
    actual.dev = Some(actual.dev.unwrap_or(0).saturating_add(1));
    let mut sidecar = sidecar_for_chunk(file_size, expected, first.index, [4u8; 16]);
    sidecar.part_fingerprint = Some(fingerprint_with_allocated_bytes(
        file_size,
        Some(first.length),
    ));
    let fs = MockFileSystem::new();
    fs.add_file(&part, file_size);
    fs.add_fingerprint(&part, actual);

    let validation = mock_downloader(fs)
        .revalidate_sidecar_chunks(
            SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac: expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            },
            None,
        )
        .await
        .unwrap();

    assert_eq!(validation.trusted_count, 0);
    assert_eq!(validation.trusted_bytes, 0);
    assert!(validation.trusted_chunks.iter().all(Option::is_none));
}

#[tokio::test]
async fn revalidate_sidecar_trusts_matching_fingerprint_with_sufficient_allocation() {
    let part = PathBuf::from("file.bin.part");
    let file_size = 300_000_u64;
    let expected = [9u8; 8];
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let first = &boundaries[0];
    let mut sidecar = sidecar_for_chunk(file_size, expected, first.index, [4u8; 16]);
    let fingerprint = fingerprint_with_allocated_bytes(file_size, Some(first.length));
    sidecar.part_fingerprint = Some(fingerprint);
    let fs = MockFileSystem::new();
    fs.add_file(&part, file_size);
    fs.add_fingerprint(&part, fingerprint);

    let validation = mock_downloader(fs)
        .revalidate_sidecar_chunks(
            SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac: expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            },
            None,
        )
        .await
        .unwrap();

    assert_eq!(validation.trusted_count, 1);
    assert_eq!(validation.trusted_bytes, first.length);
    assert_eq!(validation.trusted_chunks[0], Some([4u8; 16]));
}

#[tokio::test]
async fn revalidate_sidecar_reports_progress_for_fast_trusted_bytes() {
    let part = PathBuf::from("file.bin.part");
    let file_size = 300_000_u64;
    let expected = [9u8; 8];
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let first = &boundaries[0];
    let mut sidecar = sidecar_for_chunk(file_size, expected, first.index, [4u8; 16]);
    let fingerprint = fingerprint_with_allocated_bytes(file_size, Some(first.length));
    sidecar.part_fingerprint = Some(fingerprint);
    let fs = MockFileSystem::new();
    fs.add_file(&part, file_size);
    fs.add_fingerprint(&part, fingerprint);
    let progress = RecordingProgress::default();

    let validation = mock_downloader(fs)
        .revalidate_sidecar_chunks(
            SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac: expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: Some(("file.bin", &progress)),
            },
            None,
        )
        .await
        .unwrap();

    assert_eq!(validation.trusted_count, 1);
    assert_eq!(validation.trusted_bytes, first.length);
    assert_eq!(
        progress.total.load(std::sync::atomic::Ordering::SeqCst),
        first.length
    );
    assert_eq!(
        progress.network.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(progress.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        progress
            .validation_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(
        progress
            .validation_starts
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

#[tokio::test]
async fn revalidate_sidecar_reports_progress_for_disk_revalidation_reads() {
    let dir = tempfile::tempdir().unwrap();
    let part = dir.path().join("file.bin.part");
    let file_size = 131_072;
    let data = test_incompressible_plaintext(usize_from_u64(file_size));
    tokio::fs::write(&part, &data).await.unwrap();

    let expected = [9u8; 8];
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let first = &boundaries[0];
    let mac = mega::compute_mega_chunk_mac(chunk_data(&data, first), &TEST_AES_KEY, &TEST_AES_IV);
    let sidecar = sidecar_for_chunk(file_size, expected, first.index, mac);
    let progress = RecordingProgress::default();

    let validation = tokio_downloader()
        .revalidate_sidecar_chunks(
            SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac: expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: Some(("file.bin", &progress)),
            },
            None,
        )
        .await
        .unwrap();

    assert_eq!(validation.trusted_count, 1);
    assert_eq!(validation.trusted_bytes, first.length);
    assert_eq!(
        progress.total.load(std::sync::atomic::Ordering::SeqCst),
        first.length
    );
    assert_eq!(
        progress.network.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert!(progress.calls.load(std::sync::atomic::Ordering::SeqCst) > 0);
    assert_eq!(
        progress
            .validation_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(
        progress
            .validation_starts
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

#[tokio::test]
async fn revalidate_sidecar_disk_revalidation_honors_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let part = dir.path().join("file.bin.part");
    let file_size = 300_000_u64;
    let data = test_incompressible_plaintext(usize_from_u64(file_size));
    tokio::fs::write(&part, &data).await.unwrap();

    let expected = [9u8; 8];
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let second = &boundaries[1];
    let mac = mega::compute_mega_chunk_mac(chunk_data(&data, second), &TEST_AES_KEY, &TEST_AES_IV);
    let sidecar = sidecar_for_chunk(file_size, expected, second.index, mac);
    let token = CancellationToken::new();
    let progress = CancelOnProgress {
        token: token.clone(),
        calls: std::sync::atomic::AtomicUsize::new(0),
    };

    let err = tokio_downloader()
        .revalidate_sidecar_chunks(
            SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac: expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: Some(("file.bin", &progress)),
            },
            Some(&token),
        )
        .await
        .unwrap_err();

    assert!(matches!(err, Error::Cancelled));
    assert!(progress.calls.load(std::sync::atomic::Ordering::SeqCst) > 0);
}

#[tokio::test]
async fn revalidate_sidecar_trusts_nothing_when_part_fingerprint_is_stale() {
    let dir = tempfile::tempdir().unwrap();
    let part = dir.path().join("file.bin.part");
    let file_size = 300_000_u64;
    let data = test_plaintext(usize_from_u64(file_size));
    tokio::fs::write(&part, &data).await.unwrap();
    let mut stale_fingerprint = TokioFileSystem::new().file_fingerprint(&part).await;
    if let Some(fingerprint) = stale_fingerprint.as_mut() {
        fingerprint.len = fingerprint.len.saturating_add(1);
    }
    let mut changed = data.clone();
    changed[0] ^= 0xff;
    tokio::fs::write(&part, &changed).await.unwrap();

    let expected = [9u8; 8];
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let first = &boundaries[0];
    let first_data = chunk_data(&data, first);
    let mac = mega::compute_mega_chunk_mac(first_data, &TEST_AES_KEY, &TEST_AES_IV);
    let mut sidecar = sidecar_for_chunk(file_size, expected, first.index, mac);
    sidecar.part_fingerprint = stale_fingerprint;

    let validation = tokio_downloader()
        .revalidate_sidecar_chunks(
            SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac: expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            },
            None,
        )
        .await
        .unwrap();

    assert_eq!(validation.trusted_count, 0);
    assert_eq!(validation.trusted_bytes, 0);
    assert!(validation.trusted_chunks.iter().all(Option::is_none));
}

#[tokio::test]
async fn revalidate_sidecar_trusts_nothing_when_allocated_bytes_hint_is_stale() {
    let dir = tempfile::tempdir().unwrap();
    let part = dir.path().join("file.bin.part");
    let file_size = 300_000_u64;
    let data = test_plaintext(usize_from_u64(file_size));
    tokio::fs::write(&part, &data).await.unwrap();

    let expected = [9u8; 8];
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let first = &boundaries[0];
    let first_data = chunk_data(&data, first);
    let mac = mega::compute_mega_chunk_mac(first_data, &TEST_AES_KEY, &TEST_AES_IV);
    let mut stale_fingerprint = TokioFileSystem::new()
        .file_fingerprint(&part)
        .await
        .unwrap();
    stale_fingerprint.allocated_bytes = stale_fingerprint
        .allocated_bytes
        .map(|allocated| allocated.saturating_add(512))
        .or(Some(first.length.saturating_add(512)));

    let mut changed = data.clone();
    changed[0] ^= 0xff;
    tokio::fs::write(&part, &changed).await.unwrap();

    let mut sidecar = sidecar_for_chunk(file_size, expected, first.index, mac);
    sidecar.part_fingerprint = Some(stale_fingerprint);

    let validation = tokio_downloader()
        .revalidate_sidecar_chunks(
            SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac: expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            },
            None,
        )
        .await
        .unwrap();

    assert_eq!(validation.trusted_count, 0);
    assert_eq!(validation.trusted_bytes, 0);
    assert!(validation.trusted_chunks.iter().all(Option::is_none));
}

#[tokio::test]
async fn revalidate_sidecar_trusts_nothing_when_modified_time_hint_is_stale() {
    let dir = tempfile::tempdir().unwrap();
    let part = dir.path().join("file.bin.part");
    let file_size = 300_000_u64;
    let data = test_plaintext(usize_from_u64(file_size));
    tokio::fs::write(&part, &data).await.unwrap();

    let expected = [9u8; 8];
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let first = &boundaries[0];
    let first_data = chunk_data(&data, first);
    let mac = mega::compute_mega_chunk_mac(first_data, &TEST_AES_KEY, &TEST_AES_IV);
    let mut stale_fingerprint = TokioFileSystem::new()
        .file_fingerprint(&part)
        .await
        .unwrap();
    stale_fingerprint.modified_ns = stale_fingerprint.modified_ns.saturating_add(1);

    let mut changed = data.clone();
    changed[0] ^= 0xff;
    tokio::fs::write(&part, &changed).await.unwrap();

    let mut sidecar = sidecar_for_chunk(file_size, expected, first.index, mac);
    sidecar.part_fingerprint = Some(stale_fingerprint);

    let validation = tokio_downloader()
        .revalidate_sidecar_chunks(
            SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac: expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            },
            None,
        )
        .await
        .unwrap();

    assert_eq!(validation.trusted_count, 0);
    assert_eq!(validation.trusted_bytes, 0);
    assert!(validation.trusted_chunks.iter().all(Option::is_none));
}

#[tokio::test]
async fn revalidate_sidecar_recomputes_when_later_writes_stale_fingerprint() {
    let dir = tempfile::tempdir().unwrap();
    let part = dir.path().join("file.bin.part");
    let file_size = 300_000_u64;
    let mut data = test_incompressible_plaintext(usize_from_u64(file_size));
    tokio::fs::write(&part, &data).await.unwrap();
    let stale_fingerprint = TokioFileSystem::new().file_fingerprint(&part).await;

    let expected = [9u8; 8];
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let first = &boundaries[0];
    let second = &boundaries[1];
    let mac = mega::compute_mega_chunk_mac(chunk_data(&data, first), &TEST_AES_KEY, &TEST_AES_IV);
    let mut sidecar = sidecar_for_chunk(file_size, expected, first.index, mac);
    sidecar.part_fingerprint = stale_fingerprint;

    let second_start = usize_from_u64(second.offset);
    data[second_start] ^= 0xff;
    tokio::fs::write(&part, &data).await.unwrap();

    let validation = tokio_downloader()
        .revalidate_sidecar_chunks(
            SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac: expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            },
            None,
        )
        .await
        .unwrap();

    assert_eq!(validation.trusted_count, 1);
    assert_eq!(validation.trusted_bytes, first.length);
    assert_eq!(validation.trusted_chunks[0], Some(mac));
}

#[tokio::test]
async fn revalidate_sidecar_trusts_when_only_modified_time_changes() {
    let dir = tempfile::tempdir().unwrap();
    let part = dir.path().join("file.bin.part");
    let file_size = 300_000_u64;
    let data = test_plaintext(usize_from_u64(file_size));
    tokio::fs::write(&part, &data).await.unwrap();

    let expected = [9u8; 8];
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let first = &boundaries[0];
    let mac = mega::compute_mega_chunk_mac(chunk_data(&data, first), &TEST_AES_KEY, &TEST_AES_IV);
    let mut sidecar = sidecar_for_chunk(file_size, expected, first.index, mac);
    let mut stale_fingerprint = TokioFileSystem::new()
        .file_fingerprint(&part)
        .await
        .unwrap();
    stale_fingerprint.modified_ns = stale_fingerprint.modified_ns.saturating_add(1);
    sidecar.part_fingerprint = Some(stale_fingerprint);

    let validation = tokio_downloader()
        .revalidate_sidecar_chunks(
            SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac: expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            },
            None,
        )
        .await
        .unwrap();

    assert_eq!(validation.trusted_count, 1);
    assert_eq!(validation.trusted_bytes, first.length);
    assert_eq!(validation.trusted_chunks[0], Some(mac));
}

#[tokio::test]
async fn revalidate_sidecar_trusts_when_only_allocated_bytes_change() {
    let dir = tempfile::tempdir().unwrap();
    let part = dir.path().join("file.bin.part");
    let file_size = 300_000_u64;
    let data = test_plaintext(usize_from_u64(file_size));
    tokio::fs::write(&part, &data).await.unwrap();

    let expected = [9u8; 8];
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let first = &boundaries[0];
    let mac = mega::compute_mega_chunk_mac(chunk_data(&data, first), &TEST_AES_KEY, &TEST_AES_IV);
    let mut sidecar = sidecar_for_chunk(file_size, expected, first.index, mac);
    let mut stale_fingerprint = TokioFileSystem::new()
        .file_fingerprint(&part)
        .await
        .unwrap();
    stale_fingerprint.allocated_bytes = stale_fingerprint
        .allocated_bytes
        .map(|allocated| allocated.saturating_add(512))
        .or(Some(512));
    sidecar.part_fingerprint = Some(stale_fingerprint);

    let validation = tokio_downloader()
        .revalidate_sidecar_chunks(
            SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac: expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            },
            None,
        )
        .await
        .unwrap();

    assert_eq!(validation.trusted_count, 1);
    assert_eq!(validation.trusted_bytes, first.length);
    assert_eq!(validation.trusted_chunks[0], Some(mac));
}

#[tokio::test]
async fn revalidate_sidecar_revalidates_when_only_modified_time_changes() {
    let dir = tempfile::tempdir().unwrap();
    let part = dir.path().join("file.bin.part");
    let file_size = 300_000_u64;
    let data = test_plaintext(usize_from_u64(file_size));
    tokio::fs::write(&part, &data).await.unwrap();

    let expected = [9u8; 8];
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let second = &boundaries[1];
    let mac = mega::compute_mega_chunk_mac(chunk_data(&data, second), &TEST_AES_KEY, &TEST_AES_IV);
    let mut sidecar = sidecar_for_chunk(file_size, expected, second.index, mac);
    let mut fingerprint = TokioFileSystem::new()
        .file_fingerprint(&part)
        .await
        .unwrap();
    fingerprint.modified_ns = fingerprint.modified_ns.saturating_add(1);
    sidecar.part_fingerprint = Some(fingerprint);
    let progress = RecordingProgress::default();

    let validation = tokio_downloader()
        .revalidate_sidecar_chunks(
            SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac: expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: Some(("file.bin", &progress)),
            },
            None,
        )
        .await
        .unwrap();

    assert_eq!(validation.trusted_count, 1);
    assert_eq!(validation.trusted_bytes, second.length);
    assert_eq!(
        validation.trusted_chunks[usize_from_u32(second.index)],
        Some(mac)
    );
    assert_eq!(
        progress.total.load(std::sync::atomic::Ordering::SeqCst),
        second.length
    );
    assert_eq!(
        progress.network.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(
        progress.calls.load(std::sync::atomic::Ordering::SeqCst),
        second.length.div_ceil(REVALIDATION_BUFFER_BYTES as u64) as usize
    );
    assert_eq!(
        progress.max_delta.load(std::sync::atomic::Ordering::SeqCst),
        REVALIDATION_BUFFER_BYTES as u64
    );
}

#[tokio::test]
async fn revalidate_sidecar_revalidates_when_only_allocated_bytes_change() {
    let dir = tempfile::tempdir().unwrap();
    let part = dir.path().join("file.bin.part");
    let file_size = 300_000_u64;
    let data = test_plaintext(usize_from_u64(file_size));
    tokio::fs::write(&part, &data).await.unwrap();

    let expected = [9u8; 8];
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let second = &boundaries[1];
    let mac = mega::compute_mega_chunk_mac(chunk_data(&data, second), &TEST_AES_KEY, &TEST_AES_IV);
    let mut sidecar = sidecar_for_chunk(file_size, expected, second.index, mac);
    let mut fingerprint = TokioFileSystem::new()
        .file_fingerprint(&part)
        .await
        .unwrap();
    fingerprint.allocated_bytes = fingerprint
        .allocated_bytes
        .map(|allocated| allocated.saturating_add(512))
        .or(Some(512));
    sidecar.part_fingerprint = Some(fingerprint);
    let progress = RecordingProgress::default();

    let validation = tokio_downloader()
        .revalidate_sidecar_chunks(
            SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac: expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: Some(("file.bin", &progress)),
            },
            None,
        )
        .await
        .unwrap();

    assert_eq!(validation.trusted_count, 1);
    assert_eq!(validation.trusted_bytes, second.length);
    assert_eq!(
        validation.trusted_chunks[usize_from_u32(second.index)],
        Some(mac)
    );
    assert_eq!(
        progress.total.load(std::sync::atomic::Ordering::SeqCst),
        second.length
    );
    assert_eq!(
        progress.network.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(
        progress.calls.load(std::sync::atomic::Ordering::SeqCst),
        second.length.div_ceil(REVALIDATION_BUFFER_BYTES as u64) as usize
    );
    assert_eq!(
        progress.max_delta.load(std::sync::atomic::Ordering::SeqCst),
        REVALIDATION_BUFFER_BYTES as u64
    );
}

#[tokio::test]
async fn revalidate_sidecar_revalidates_when_modified_time_and_allocated_bytes_change() {
    let dir = tempfile::tempdir().unwrap();
    let part = dir.path().join("file.bin.part");
    let file_size = 300_000_u64;
    let data = test_plaintext(usize_from_u64(file_size));
    tokio::fs::write(&part, &data).await.unwrap();

    let expected = [9u8; 8];
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let second = &boundaries[1];
    let mac = mega::compute_mega_chunk_mac(chunk_data(&data, second), &TEST_AES_KEY, &TEST_AES_IV);
    let mut sidecar = sidecar_for_chunk(file_size, expected, second.index, mac);
    let mut fingerprint = TokioFileSystem::new()
        .file_fingerprint(&part)
        .await
        .unwrap();
    fingerprint.modified_ns = fingerprint.modified_ns.saturating_add(1);
    fingerprint.allocated_bytes = fingerprint
        .allocated_bytes
        .map(|allocated| allocated.saturating_add(512))
        .or(Some(512));
    sidecar.part_fingerprint = Some(fingerprint);
    let progress = RecordingProgress::default();

    let validation = tokio_downloader()
        .revalidate_sidecar_chunks(
            SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac: expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: Some(("file.bin", &progress)),
            },
            None,
        )
        .await
        .unwrap();

    assert_eq!(validation.trusted_count, 1);
    assert_eq!(validation.trusted_bytes, second.length);
    assert_eq!(
        validation.trusted_chunks[usize_from_u32(second.index)],
        Some(mac)
    );
    assert_eq!(
        progress.total.load(std::sync::atomic::Ordering::SeqCst),
        second.length
    );
    assert_eq!(
        progress.network.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(
        progress.calls.load(std::sync::atomic::Ordering::SeqCst),
        second.length.div_ceil(REVALIDATION_BUFFER_BYTES as u64) as usize
    );
    assert_eq!(
        progress.max_delta.load(std::sync::atomic::Ordering::SeqCst),
        REVALIDATION_BUFFER_BYTES as u64
    );
}

#[tokio::test]
async fn revalidate_sidecar_fast_trusts_when_saved_fingerprint_lacks_device_and_inode() {
    let dir = tempfile::tempdir().unwrap();
    let part = dir.path().join("file.bin.part");
    let file_size = 300_000_u64;
    let data = test_plaintext(usize_from_u64(file_size));
    tokio::fs::write(&part, &data).await.unwrap();

    let expected = [9u8; 8];
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let second = &boundaries[1];
    let mac = mega::compute_mega_chunk_mac(chunk_data(&data, second), &TEST_AES_KEY, &TEST_AES_IV);
    let mut sidecar = sidecar_for_chunk(file_size, expected, second.index, mac);
    let mut fingerprint = TokioFileSystem::new()
        .file_fingerprint(&part)
        .await
        .unwrap();
    fingerprint.dev = None;
    fingerprint.ino = None;
    sidecar.part_fingerprint = Some(fingerprint);
    let progress = RecordingProgress::default();

    let validation = tokio_downloader()
        .revalidate_sidecar_chunks(
            SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac: expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: Some(("file.bin", &progress)),
            },
            None,
        )
        .await
        .unwrap();

    assert_eq!(validation.trusted_count, 1);
    assert_eq!(validation.trusted_bytes, second.length);
    assert_eq!(
        validation.trusted_chunks[usize_from_u32(second.index)],
        Some(mac)
    );
    assert_eq!(
        progress.total.load(std::sync::atomic::Ordering::SeqCst),
        second.length
    );
    assert_eq!(
        progress.network.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(progress.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        progress.max_delta.load(std::sync::atomic::Ordering::SeqCst),
        second.length
    );
}

#[tokio::test]
async fn revalidate_sidecar_with_matching_fingerprint_keeps_first_duplicate_chunk() {
    let part = PathBuf::from("file.bin.part");
    let file_size = 300_000_u64;
    let data = test_plaintext(usize_from_u64(file_size));

    let expected = [9u8; 8];
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let first = &boundaries[0];
    let first_data = chunk_data(&data, first);
    let mac = mega::compute_mega_chunk_mac(first_data, &TEST_AES_KEY, &TEST_AES_IV);
    let mut sidecar = ResumeSidecar {
        version: CURRENT_RESUME_SIDECAR_VERSION,
        file_size,
        expected_condensed_mac: expected,
        verified_chunks: vec![
            VerifiedChunkRecord {
                index: first.index,
                mac: [1u8; 16],
            },
            VerifiedChunkRecord {
                index: first.index,
                mac,
            },
        ]
        .into(),
        part_fingerprint: None,
    };
    let fingerprint = fingerprint_with_allocated_bytes(file_size, Some(first.length));
    sidecar.part_fingerprint = Some(fingerprint);
    let fs = MockFileSystem::new();
    fs.add_file(&part, file_size);
    fs.add_fingerprint(&part, fingerprint);

    let validation = mock_downloader(fs)
        .revalidate_sidecar_chunks(
            SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac: expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            },
            None,
        )
        .await
        .unwrap();

    assert_eq!(validation.trusted_count, 1);
    assert_eq!(validation.trusted_bytes, first.length);
    assert_eq!(validation.trusted_chunks[0], Some([1u8; 16]));
}

#[tokio::test]
async fn revalidate_sidecar_rejects_bad_chunk_mac() {
    let dir = tempfile::tempdir().unwrap();
    let part = dir.path().join("file.bin.part");
    let file_size = 300_000_u64;
    let data = test_plaintext(usize_from_u64(file_size));
    tokio::fs::write(&part, &data).await.unwrap();

    let expected = [9u8; 8];
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let sidecar = sidecar_for_chunk(file_size, expected, 0, [1u8; 16]);

    let validation = tokio_downloader()
        .revalidate_sidecar_chunks(
            SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac: expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            },
            None,
        )
        .await
        .unwrap();

    assert!(validation.sidecar_loaded);
    assert_eq!(validation.trusted_count, 0);
    assert_eq!(validation.trusted_bytes, 0);
    assert!(validation.trusted_chunks.iter().all(Option::is_none));
}

#[tokio::test]
async fn revalidate_sidecar_rejects_short_part_file() {
    let dir = tempfile::tempdir().unwrap();
    let part = dir.path().join("file.bin.part");
    let file_size = 300_000_u64;
    let data = test_plaintext(usize_from_u64(file_size));

    let expected = [9u8; 8];
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let second = &boundaries[1];
    let second_data = chunk_data(&data, second);
    let mac = mega::compute_mega_chunk_mac(second_data, &TEST_AES_KEY, &TEST_AES_IV);
    let sidecar = sidecar_for_chunk(file_size, expected, second.index, mac);

    tokio::fs::write(&part, &data[..usize_from_u64(second.offset)])
        .await
        .unwrap();

    let validation = tokio_downloader()
        .revalidate_sidecar_chunks(
            SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac: expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            },
            None,
        )
        .await
        .unwrap();

    assert_eq!(validation.trusted_count, 0);
    assert_eq!(validation.trusted_bytes, 0);
    assert!(validation.trusted_chunks.iter().all(Option::is_none));
}

#[tokio::test]
async fn revalidate_sidecar_rejects_stale_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let part = dir.path().join("file.bin.part");
    let file_size = 300_000_u64;
    let data = test_plaintext(usize_from_u64(file_size));
    tokio::fs::write(&part, &data).await.unwrap();

    let expected = [9u8; 8];
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let sidecar = sidecar_for_chunk(file_size, [0u8; 8], 0, [1u8; 16]);

    let validation = tokio_downloader()
        .revalidate_sidecar_chunks(
            SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac: expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            },
            None,
        )
        .await
        .unwrap();

    assert!(validation.sidecar_loaded);
    assert_eq!(validation.trusted_count, 0);
    assert_eq!(validation.trusted_bytes, 0);
    assert!(validation.trusted_chunks.iter().all(Option::is_none));
}

#[tokio::test]
async fn stale_sidecar_without_matching_metadata_trusts_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let part = dir.path().join("file.bin.part");
    let sidecar = dir.path().join("file.bin.part.meta.json");
    let file_size = 300_000_u64;
    let data = test_plaintext(200_000);
    tokio::fs::write(&part, &data).await.unwrap();

    let expected = [9u8; 8];
    let boundaries = mega::mega_chunk_boundaries(file_size);
    save_sidecar_atomic(
        &sidecar,
        &ResumeSidecar {
            version: CURRENT_RESUME_SIDECAR_VERSION,
            file_size,
            expected_condensed_mac: [0u8; 8],
            verified_chunks: vec![VerifiedChunkRecord {
                index: 0,
                mac: [1u8; 16],
            }]
            .into(),
            part_fingerprint: None,
        },
    )
    .await
    .unwrap();
    let loaded_sidecar = load_sidecar(&sidecar).await.unwrap();
    let validation = tokio_downloader()
        .revalidate_sidecar_chunks(
            SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &loaded_sidecar,
                file_size,
                expected_condensed_mac: expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            },
            None,
        )
        .await
        .unwrap();

    assert!(validation.sidecar_loaded);
    assert_eq!(validation.trusted_count, 0);
    assert_eq!(validation.trusted_bytes, 0);
    assert!(validation.trusted_chunks.iter().all(Option::is_none));
}

#[tokio::test]
async fn legacy_v1_sidecar_trusts_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let part = dir.path().join("file.bin.part");
    let sidecar = dir.path().join("file.bin.part.meta.json");
    let file_size = 300_000_u64;
    let data = test_plaintext(usize_from_u64(file_size));
    tokio::fs::write(&part, &data).await.unwrap();

    let expected = [9u8; 8];
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let first = &boundaries[0];
    let first_data = chunk_data(&data, first);
    let mac = mega::compute_mega_chunk_mac(first_data, &TEST_AES_KEY, &TEST_AES_IV);
    save_sidecar_atomic(
        &sidecar,
        &ResumeSidecar {
            version: 1,
            file_size,
            expected_condensed_mac: expected,
            verified_chunks: vec![VerifiedChunkRecord {
                index: first.index,
                mac,
            }]
            .into(),
            part_fingerprint: None,
        },
    )
    .await
    .unwrap();

    let loaded_sidecar = load_sidecar(&sidecar).await.unwrap();
    let validation = tokio_downloader()
        .revalidate_sidecar_chunks(
            SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &loaded_sidecar,
                file_size,
                expected_condensed_mac: expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            },
            None,
        )
        .await
        .unwrap();

    assert!(validation.sidecar_loaded);
    assert_eq!(validation.trusted_count, 0);
    assert_eq!(validation.trusted_bytes, 0);
    assert!(validation.trusted_chunks.iter().all(Option::is_none));
}
