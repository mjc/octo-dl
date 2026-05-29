use std::path::Path;
use std::sync::atomic::Ordering;

use tokio_util::sync::CancellationToken;

use crate::error::Error;

use super::super::callbacks::DownloadProgress;
use super::super::downloader::ResumeReuseSource;
use super::super::sidecar_state::{SidecarValidationInput, TrustedResumeChunkCandidate};
use super::super::sidecar_store::ResumeSidecar;
use super::super::test_support::*;
use super::*;

fn validation_input<'a>(
    boundaries: &'a [mega::MegaChunk],
    part_path: &'a Path,
    sidecar: &'a ResumeSidecar,
    progress: Option<(&'a str, &'a dyn DownloadProgress)>,
) -> SidecarValidationInput<'a> {
    SidecarValidationInput {
        boundaries,
        part_path,
        sidecar,
        file_size: sidecar.file_size,
        expected_condensed_mac: sidecar.expected_condensed_mac,
        aes_key: &TEST_AES_KEY,
        aes_iv: &TEST_AES_IV,
        progress,
    }
}

#[tokio::test]
async fn revalidate_candidate_from_part_reads_through_filesystem_trait() {
    let fs = MockFileSystem::new();
    let part_path = Path::new("virtual.part");
    let file_size = 300_000_u64;
    let data = test_incompressible_plaintext(usize_from_u64(file_size));
    fs.add_bytes(part_path, data.clone());
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let first = boundaries[0];
    let mac = mega::compute_mega_chunk_mac(chunk_data(&data, &first), &TEST_AES_KEY, &TEST_AES_IV);
    let sidecar = sidecar_for_chunk(file_size, [9_u8; 8], first.index, mac);
    let input = validation_input(&boundaries, part_path, &sidecar, None);
    let mut buffer = [0; REVALIDATION_BUFFER_BYTES];

    let trusted = revalidate_candidate_from_part(
        &fs,
        &input,
        TrustedResumeChunkCandidate {
            index: usize_from_u32(first.index),
            length: first.length,
            expected_mac: mac,
        },
        &mut buffer,
        None,
    )
    .await
    .unwrap();

    assert!(trusted);
}

#[tokio::test]
async fn revalidate_candidate_from_part_returns_false_when_filesystem_short_reads() {
    let fs = MockFileSystem::new();
    let part_path = Path::new("virtual.part");
    let file_size = 300_000_u64;
    let data = test_incompressible_plaintext(usize_from_u64(file_size / 2));
    fs.add_bytes(part_path, data.clone());
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let first = boundaries[0];
    let sidecar = sidecar_for_chunk(file_size, [9_u8; 8], first.index, [4_u8; 16]);
    let input = validation_input(&boundaries, part_path, &sidecar, None);
    let mut buffer = [0; REVALIDATION_BUFFER_BYTES];

    let trusted = revalidate_candidate_from_part(
        &fs,
        &input,
        TrustedResumeChunkCandidate {
            index: usize_from_u32(first.index),
            length: first.length,
            expected_mac: [4_u8; 16],
        },
        &mut buffer,
        None,
    )
    .await
    .unwrap();

    assert!(!trusted);
}

#[tokio::test]
async fn revalidate_candidates_from_part_reports_start_and_trusts_matches() {
    let fs = MockFileSystem::new();
    let part_path = Path::new("virtual.part");
    let file_size = 300_000_u64;
    let data = test_incompressible_plaintext(usize_from_u64(file_size));
    fs.add_bytes(part_path, data.clone());
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let first = boundaries[0];
    let mac = mega::compute_mega_chunk_mac(chunk_data(&data, &first), &TEST_AES_KEY, &TEST_AES_IV);
    let sidecar = sidecar_for_chunk(file_size, [9_u8; 8], first.index, mac);
    let progress = RecordingProgress::default();

    let validation = revalidate_candidates_from_part(
        &fs,
        validation_input(
            &boundaries,
            part_path,
            &sidecar,
            Some(("virtual.part", &progress)),
        ),
        vec![TrustedResumeChunkCandidate {
            index: usize_from_u32(first.index),
            length: first.length,
            expected_mac: mac,
        }],
        first.length,
        Some(first.length),
        None,
    )
    .await
    .unwrap();

    assert_eq!(validation.trusted_count, 1);
    assert_eq!(validation.trusted_bytes, first.length);
    assert_eq!(validation.source, Some(ResumeReuseSource::Sidecar));
    assert_eq!(progress.validation_starts.load(Ordering::SeqCst), 1);
    assert_eq!(progress.total.load(Ordering::SeqCst), first.length);
    assert_eq!(progress.network.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn revalidate_candidates_from_part_honors_pre_cancelled_token() {
    let fs = MockFileSystem::new();
    let part_path = Path::new("virtual.part");
    let file_size = 300_000_u64;
    let data = test_incompressible_plaintext(usize_from_u64(file_size));
    fs.add_bytes(part_path, data.clone());
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let first = boundaries[0];
    let mac = mega::compute_mega_chunk_mac(chunk_data(&data, &first), &TEST_AES_KEY, &TEST_AES_IV);
    let sidecar = sidecar_for_chunk(file_size, [9_u8; 8], first.index, mac);
    let token = CancellationToken::new();
    token.cancel();

    let error = revalidate_candidates_from_part(
        &fs,
        validation_input(&boundaries, part_path, &sidecar, None),
        vec![TrustedResumeChunkCandidate {
            index: usize_from_u32(first.index),
            length: first.length,
            expected_mac: mac,
        }],
        first.length,
        Some(first.length),
        Some(&token),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, Error::Cancelled));
}
