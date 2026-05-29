use std::path::Path;

use super::callbacks::DownloadProgress;
use super::resume_state::ResumeReuseSource;
use super::resume_tracker::ResumeTracker;
use super::sidecar_store::{ResumeSidecar, save_sidecar_atomic};
use crate::error::{Error, Result};
use crate::fs::FileSystem;

#[derive(Debug)]
pub(super) struct ResumeValidation {
    pub(super) trusted_chunks: Vec<Option<[u8; 16]>>,
    pub(super) trusted_count: usize,
    pub(super) trusted_bytes: u64,
    pub(super) sidecar_loaded: bool,
    pub(super) source: Option<ResumeReuseSource>,
}

impl ResumeValidation {
    pub(super) fn empty(chunk_count: usize) -> Self {
        Self {
            trusted_chunks: vec![None; chunk_count],
            trusted_count: 0,
            trusted_bytes: 0,
            sidecar_loaded: false,
            source: None,
        }
    }
}

pub(super) struct SidecarValidationInput<'a> {
    pub(super) boundaries: &'a [mega::MegaChunk],
    pub(super) part_path: &'a Path,
    pub(super) sidecar: &'a ResumeSidecar,
    pub(super) file_size: u64,
    pub(super) expected_condensed_mac: [u8; 8],
    pub(super) aes_key: &'a [u8; 16],
    pub(super) aes_iv: &'a [u8; 8],
    pub(super) progress: Option<(&'a str, &'a dyn DownloadProgress)>,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct TrustedResumeChunkCandidate {
    pub(super) index: usize,
    pub(super) length: u64,
    pub(super) expected_mac: [u8; 16],
}

pub(super) fn trust_resume_candidate(
    validation: &mut ResumeValidation,
    candidate: TrustedResumeChunkCandidate,
) -> bool {
    if validation.trusted_chunks[candidate.index].is_some() {
        return false;
    }
    validation.trusted_chunks[candidate.index] = Some(candidate.expected_mac);
    validation.trusted_count = validation.trusted_count.saturating_add(1);
    validation.trusted_bytes = validation.trusted_bytes.saturating_add(candidate.length);
    true
}

pub(super) async fn persist_revalidated_sidecar<F: FileSystem>(
    fs: &F,
    sidecar_path: &Path,
    part_path: &Path,
    file_size: u64,
    expected_condensed_mac: [u8; 8],
    validation: &ResumeValidation,
) -> Result<()> {
    let mut snapshot = ResumeTracker::new(
        file_size,
        expected_condensed_mac,
        validation.trusted_chunks.clone(),
    )
    .snapshot();
    snapshot.part_fingerprint = fs.file_fingerprint(part_path).await;
    save_sidecar_atomic(sidecar_path, &snapshot)
        .await
        .map_err(Error::from)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::{VerifiedChunkRecord, load_sidecar, sidecar_path};
    use super::{
        ResumeReuseSource, ResumeValidation, TrustedResumeChunkCandidate,
        persist_revalidated_sidecar, trust_resume_candidate,
    };
    use crate::fs::{FileSystem, TokioFileSystem};
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn trust_resume_candidate_counts_each_chunk_once(
            chunk_count in 1usize..16,
            candidates in proptest::collection::vec((0usize..32, 0u64..1_000_001, any::<[u8; 16]>()), 0..32),
        ) {
            let mut validation = ResumeValidation::empty(chunk_count);
            let mut expected_chunks = vec![None; chunk_count];
            let mut expected_count = 0usize;
            let mut expected_bytes = 0u64;

            for (raw_index, length, expected_mac) in candidates {
                let index = raw_index % chunk_count;
                let changed = if expected_chunks[index].is_none() {
                    expected_chunks[index] = Some(expected_mac);
                    expected_count = expected_count.saturating_add(1);
                    expected_bytes = expected_bytes.saturating_add(length);
                    true
                } else {
                    false
                };

                prop_assert_eq!(
                    trust_resume_candidate(
                        &mut validation,
                        TrustedResumeChunkCandidate {
                            index,
                            length,
                            expected_mac,
                        }
                    ),
                    changed
                );
            }

            prop_assert_eq!(validation.trusted_chunks, expected_chunks);
            prop_assert_eq!(validation.trusted_count, expected_count);
            prop_assert_eq!(validation.trusted_bytes, expected_bytes);
            prop_assert!(!validation.sidecar_loaded);
            prop_assert_eq!(validation.source, None);
        }
    }

    #[tokio::test]
    async fn persist_revalidated_sidecar_writes_verified_chunks_and_current_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let file_size = 300_000_u64;
        let expected_condensed_mac = [9_u8; 8];
        let output_path = dir.path().join("payload.bin");
        let sidecar_path = sidecar_path(output_path.to_string_lossy().as_ref());
        let part_path = dir.path().join("payload.bin.part");
        let part_data = test_incompressible_plaintext(usize_from_u64(file_size));
        tokio::fs::write(&part_path, &part_data).await.unwrap();
        let expected_fingerprint = TokioFileSystem::new()
            .file_fingerprint(&part_path)
            .await
            .unwrap();
        let validation = ResumeValidation {
            trusted_chunks: vec![Some([1_u8; 16]), None, Some([3_u8; 16])],
            trusted_count: 2,
            trusted_bytes: file_size,
            sidecar_loaded: true,
            source: Some(ResumeReuseSource::Sidecar),
        };

        persist_revalidated_sidecar(
            &TokioFileSystem::new(),
            &sidecar_path,
            &part_path,
            file_size,
            expected_condensed_mac,
            &validation,
        )
        .await
        .unwrap();

        let persisted = load_sidecar(&sidecar_path).await.unwrap();
        assert_eq!(persisted.file_size, file_size);
        assert_eq!(persisted.expected_condensed_mac, expected_condensed_mac);
        assert_eq!(
            persisted.verified_chunks,
            vec![
                VerifiedChunkRecord {
                    index: 0,
                    mac: [1_u8; 16]
                },
                VerifiedChunkRecord {
                    index: 2,
                    mac: [3_u8; 16]
                }
            ]
            .into()
        );
        assert_eq!(persisted.part_fingerprint, Some(expected_fingerprint));
    }
}
