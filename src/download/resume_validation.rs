use std::path::Path;

use super::callbacks::DownloadProgress;
use super::resume_state::ResumeReuseSource;
use super::sidecar_store::ResumeSidecar;

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

#[cfg(test)]
mod tests {
    use super::{ResumeValidation, TrustedResumeChunkCandidate, trust_resume_candidate};
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
}
