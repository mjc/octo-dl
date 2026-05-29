use std::path::Path;
use std::time::{Duration, Instant};

use super::callbacks::DownloadProgress;
use super::resume_state::ResumeReuseSource;
use super::sidecar_store::ResumeSidecar;
use crate::fs::FileFingerprint;

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

pub(crate) fn should_emit_resume_validation_progress(
    last_report_at: Instant,
    now: Instant,
) -> bool {
    now.saturating_duration_since(last_report_at) >= Duration::from_secs(30)
}

pub(crate) fn resume_fingerprint_matches(
    expected: FileFingerprint,
    actual: FileFingerprint,
) -> bool {
    expected.len == actual.len
        && (expected.modified_ns == 0 || expected.modified_ns == actual.modified_ns)
        && expected
            .allocated_bytes
            .is_none_or(|allocated| actual.allocated_bytes == Some(allocated))
        && expected.dev.is_none_or(|dev| actual.dev == Some(dev))
        && expected.ino.is_none_or(|ino| actual.ino == Some(ino))
}

#[cfg(test)]
mod tests {
    use super::{
        ResumeValidation, TrustedResumeChunkCandidate, resume_fingerprint_matches,
        should_emit_resume_validation_progress, trust_resume_candidate,
    };
    use crate::fs::FileFingerprint;
    use proptest::prelude::*;
    use std::time::{Duration, Instant};

    fn matching_fingerprint(
        len: u64,
        modified_ns: u128,
        allocated_bytes: Option<u64>,
    ) -> FileFingerprint {
        FileFingerprint {
            len,
            modified_ns,
            allocated_bytes,
            dev: Some(7),
            ino: Some(11),
        }
    }

    proptest! {
        #[test]
        fn resume_validation_progress_stays_silent_before_threshold(
            secs in 0u64..30,
        ) {
            let start = Instant::now();
            prop_assert!(!should_emit_resume_validation_progress(
                start,
                start + Duration::from_secs(secs),
            ));
        }

        #[test]
        fn resume_validation_progress_emits_at_or_after_threshold(
            secs in 30u64..300,
        ) {
            let start = Instant::now();
            prop_assert!(should_emit_resume_validation_progress(
                start,
                start + Duration::from_secs(secs),
            ));
        }

        #[test]
        fn resume_fingerprint_matches_ignores_missing_allocated_bytes(
            len in any::<u64>(),
            actual_allocated_bytes in any::<Option<u64>>(),
        ) {
            let expected = matching_fingerprint(len, 123, None);
            let mut actual = matching_fingerprint(len, 123, Some(512));
            actual.allocated_bytes = actual_allocated_bytes;

            prop_assert!(resume_fingerprint_matches(expected, actual));
        }

        #[test]
        fn resume_fingerprint_matches_ignores_zero_modified_time(
            len in any::<u64>(),
            actual_modified_ns in any::<u128>(),
        ) {
            let expected = matching_fingerprint(len, 0, Some(512));
            let mut actual = matching_fingerprint(len, 123, Some(512));
            actual.modified_ns = actual_modified_ns;

            prop_assert!(resume_fingerprint_matches(expected, actual));
        }

        #[test]
        fn resume_fingerprint_matches_rejects_modified_time_mismatch(
            len in any::<u64>(),
            modified_ns in 1u128..u128::MAX,
            delta in 1u128..1024,
        ) {
            let expected = matching_fingerprint(len, modified_ns, Some(512));
            let mut actual = expected;
            actual.modified_ns = actual.modified_ns.wrapping_add(delta);

            prop_assert!(!resume_fingerprint_matches(expected, actual));
        }

        #[test]
        fn resume_fingerprint_matches_rejects_length_mismatch(
            len in any::<u64>(),
            delta in 1u64..1024,
        ) {
            let expected = matching_fingerprint(len, 123, Some(512));
            let mut actual = expected;
            actual.len = actual.len.wrapping_add(delta);

            prop_assert!(!resume_fingerprint_matches(expected, actual));
        }

        #[test]
        fn resume_fingerprint_matches_rejects_allocated_bytes_mismatch_when_expected_present(
            len in any::<u64>(),
            allocated_bytes in any::<u64>(),
            delta in 1u64..1024,
        ) {
            let expected = matching_fingerprint(len, 123, Some(allocated_bytes));
            let actual = matching_fingerprint(len, 123, Some(allocated_bytes.wrapping_add(delta)));

            prop_assert!(!resume_fingerprint_matches(expected, actual));
        }

        #[test]
        fn resume_fingerprint_matches_ignores_missing_device_in_expected(
            len in any::<u64>(),
            actual_dev in any::<Option<u64>>(),
        ) {
            let mut expected = matching_fingerprint(len, 123, Some(512));
            expected.dev = None;
            let mut actual = matching_fingerprint(len, 123, Some(512));
            actual.dev = actual_dev;

            prop_assert!(resume_fingerprint_matches(expected, actual));
        }

        #[test]
        fn resume_fingerprint_matches_rejects_device_mismatch(
            len in any::<u64>(),
            dev in any::<u64>(),
            delta in 1u64..1024,
        ) {
            let expected = matching_fingerprint(len, 123, Some(512));
            let mut actual = expected;
            actual.dev = Some(dev.wrapping_add(delta));

            prop_assume!(actual.dev != expected.dev);
            prop_assert!(!resume_fingerprint_matches(expected, actual));
        }

        #[test]
        fn resume_fingerprint_matches_ignores_missing_inode_in_expected(
            len in any::<u64>(),
            actual_ino in any::<Option<u64>>(),
        ) {
            let mut expected = matching_fingerprint(len, 123, Some(512));
            expected.ino = None;
            let mut actual = matching_fingerprint(len, 123, Some(512));
            actual.ino = actual_ino;

            prop_assert!(resume_fingerprint_matches(expected, actual));
        }

        #[test]
        fn resume_fingerprint_matches_rejects_inode_mismatch(
            len in any::<u64>(),
            ino in any::<u64>(),
            delta in 1u64..1024,
        ) {
            let expected = matching_fingerprint(len, 123, Some(512));
            let mut actual = expected;
            actual.ino = Some(ino.wrapping_add(delta));

            prop_assume!(actual.ino != expected.ino);
            prop_assert!(!resume_fingerprint_matches(expected, actual));
        }

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
