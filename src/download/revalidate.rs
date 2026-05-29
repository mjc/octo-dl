use std::io::{Read, Seek};
use std::path::Path;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::core::ProgressDelta;
use crate::error::{Error, Result};
use crate::fs::{FileFingerprint, FileSystem};

use super::callbacks::DownloadProgress;
use super::downloader::{CURRENT_RESUME_SIDECAR_VERSION, Downloader, ResumeReuseSource};
use super::sidecar_state::{
    ResumeValidation, SidecarValidationInput, TrustedResumeChunkCandidate, trust_resume_candidate,
};
use super::sidecar_store::load_sidecar;

pub(crate) const REVALIDATION_BUFFER_BYTES: usize = 128 * 1024;

pub(crate) fn should_emit_resume_validation_progress(
    last_report_at: Instant,
    now: Instant,
) -> bool {
    now.saturating_duration_since(last_report_at) >= Duration::from_secs(30)
}

pub(super) fn revalidation_buffer_len(remaining: u64) -> usize {
    usize::try_from(remaining.min(REVALIDATION_BUFFER_BYTES as u64))
        .unwrap_or(REVALIDATION_BUFFER_BYTES)
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

impl<F: FileSystem> Downloader<F> {
    pub(super) async fn revalidate_resume_chunks(
        &self,
        node: &mega::Node,
        boundaries: &[mega::MegaChunk],
        part_path: &Path,
        sidecar_path: &Path,
        expected_condensed_mac: [u8; 8],
        progress: Option<(&str, &dyn DownloadProgress)>,
        cancellation_token: Option<&CancellationToken>,
    ) -> Result<ResumeValidation> {
        if cancellation_token.is_some_and(|token| token.is_cancelled()) {
            return Err(Error::Cancelled);
        }
        let Some(sidecar) = load_sidecar(sidecar_path).await else {
            return Ok(ResumeValidation::empty(boundaries.len()));
        };
        let aes_iv = node.aes_iv().ok_or(mega::Error::MissingNodeAesIv)?;

        self.revalidate_sidecar_chunks(
            SidecarValidationInput {
                boundaries,
                part_path,
                sidecar: &sidecar,
                file_size: node.size(),
                expected_condensed_mac,
                aes_key: node.aes_key(),
                aes_iv,
                progress,
            },
            cancellation_token,
        )
        .await
    }

    pub(super) async fn revalidate_candidate_from_part(
        &self,
        input: &SidecarValidationInput<'_>,
        candidate: TrustedResumeChunkCandidate,
        buffer: &mut [u8; REVALIDATION_BUFFER_BYTES],
        cancellation_token: Option<&CancellationToken>,
    ) -> Result<bool> {
        if cancellation_token.is_some_and(|token| token.is_cancelled()) {
            return Err(Error::Cancelled);
        }
        let Some(boundary) = input.boundaries.get(candidate.index) else {
            return Ok(false);
        };
        let mut mac = mega::MegaChunkMac::new(input.aes_key, input.aes_iv);
        let mut offset = boundary.offset;
        let end = boundary.offset.saturating_add(boundary.length);
        let Ok(mut file) = std::fs::File::open(input.part_path) else {
            return Ok(false);
        };
        if file
            .seek(std::io::SeekFrom::Start(boundary.offset))
            .is_err()
        {
            return Ok(false);
        }

        while offset < end {
            if cancellation_token.is_some_and(|token| token.is_cancelled()) {
                return Err(Error::Cancelled);
            }
            let read_len = revalidation_buffer_len(end - offset);
            let read_buffer = &mut buffer[..read_len];
            if file.read_exact(read_buffer).is_err() {
                return Ok(false);
            }
            mac.update(read_buffer);
            if let Some((name, progress)) = input.progress {
                progress.on_resume_validation_chunk(name, u64::try_from(read_len).unwrap_or(0));
            }
            offset = offset.saturating_add(u64::try_from(read_len).unwrap_or(0));
            tokio::task::yield_now().await;
        }

        Ok(mac.finalize() == candidate.expected_mac)
    }

    pub(super) async fn revalidate_sidecar_chunks(
        &self,
        input: SidecarValidationInput<'_>,
        cancellation_token: Option<&CancellationToken>,
    ) -> Result<ResumeValidation> {
        let mut validation = ResumeValidation {
            sidecar_loaded: true,
            ..ResumeValidation::empty(input.boundaries.len())
        };

        if input.sidecar.version != CURRENT_RESUME_SIDECAR_VERSION
            || input.sidecar.file_size != input.file_size
            || input.sidecar.expected_condensed_mac != input.expected_condensed_mac
        {
            log::debug!(
                "Resume sidecar rejected for {}: metadata mismatch version={} file_size={} expected_file_size={}",
                input.part_path.display(),
                input.sidecar.version,
                input.sidecar.file_size,
                input.file_size
            );
            return Ok(validation);
        }

        let part_size = self.fs.file_size(input.part_path).await.unwrap_or(0);
        let mut candidates = Vec::with_capacity(input.sidecar.verified_chunks.len());

        for record in &input.sidecar.verified_chunks {
            let Ok(index) = usize::try_from(record.index) else {
                continue;
            };
            let Some(boundary) = input.boundaries.get(index).copied() else {
                continue;
            };
            if boundary.offset.saturating_add(boundary.length) > part_size {
                continue;
            }
            candidates.push(TrustedResumeChunkCandidate {
                index,
                length: boundary.length,
                expected_mac: record.mac,
            });
        }

        log::debug!(
            "Resume sidecar loaded for {}: records={} candidates={} part_size={} file_size={} fingerprint_present={}",
            input.part_path.display(),
            input.sidecar.verified_chunks.len(),
            candidates.len(),
            part_size,
            input.file_size,
            input.sidecar.part_fingerprint.is_some()
        );

        let mut seen_candidate_indexes = vec![false; input.boundaries.len()];
        let trusted_candidate_bytes = candidates
            .iter()
            .filter_map(|candidate| {
                let seen = seen_candidate_indexes.get_mut(candidate.index)?;
                if *seen {
                    return None;
                }
                *seen = true;
                Some(candidate.length)
            })
            .sum::<u64>();
        let part_fingerprint = input.sidecar.part_fingerprint;
        let Some(expected_fingerprint) = part_fingerprint else {
            log::debug!(
                "Resume sidecar for {} has no part fingerprint; falling back to disk revalidation",
                input.part_path.display()
            );
            return self
                .revalidate_candidates_from_part(
                    input,
                    candidates,
                    trusted_candidate_bytes,
                    None,
                    cancellation_token,
                )
                .await;
        };
        let Some(actual_fingerprint) = self.fs.file_fingerprint(input.part_path).await else {
            log::debug!(
                "Resume sidecar for {} could not fingerprint part file; falling back to disk revalidation",
                input.part_path.display()
            );
            return self
                .revalidate_candidates_from_part(
                    input,
                    candidates,
                    trusted_candidate_bytes,
                    expected_fingerprint.allocated_bytes,
                    cancellation_token,
                )
                .await;
        };
        let fingerprint_matches =
            resume_fingerprint_matches(expected_fingerprint, actual_fingerprint);
        if !fingerprint_matches {
            log::debug!(
                "Resume sidecar for {} has stale part fingerprint; falling back to disk revalidation: expected={expected_fingerprint:?} actual={actual_fingerprint:?}",
                input.part_path.display()
            );
        }

        if fingerprint_matches {
            for candidate in candidates {
                trust_resume_candidate(&mut validation, candidate);
            }
            if validation.trusted_bytes > 0
                && let Some((name, progress)) = input.progress
            {
                progress.on_progress(
                    name,
                    ProgressDelta {
                        total_bytes_delta: validation.trusted_bytes,
                        network_bytes_delta: 0,
                    },
                );
            }
            log::debug!(
                "Resume sidecar fast-trusted for {}: chunks={} bytes={}",
                input.part_path.display(),
                validation.trusted_count,
                validation.trusted_bytes
            );
        } else {
            validation = self
                .revalidate_candidates_from_part(
                    input,
                    candidates,
                    trusted_candidate_bytes,
                    expected_fingerprint.allocated_bytes,
                    cancellation_token,
                )
                .await?;
        }

        if validation.trusted_count > 0 {
            validation.source = Some(ResumeReuseSource::Sidecar);
        }

        Ok(validation)
    }

    pub(super) async fn revalidate_candidates_from_part(
        &self,
        input: SidecarValidationInput<'_>,
        candidates: Vec<TrustedResumeChunkCandidate>,
        trusted_candidate_bytes: u64,
        allocated_bytes: Option<u64>,
        cancellation_token: Option<&CancellationToken>,
    ) -> Result<ResumeValidation> {
        let mut validation = ResumeValidation {
            sidecar_loaded: true,
            ..ResumeValidation::empty(input.boundaries.len())
        };
        if cancellation_token.is_some_and(|token| token.is_cancelled()) {
            return Err(Error::Cancelled);
        }
        if !candidates.is_empty()
            && let Some((name, progress)) = input.progress
        {
            progress.on_resume_validation_start(name);
        }
        log::debug!(
            "Resume sidecar for {} needs disk revalidation: candidate_bytes={} allocated_bytes={allocated_bytes:?}",
            input.part_path.display(),
            trusted_candidate_bytes
        );
        let mut buffer = [0; REVALIDATION_BUFFER_BYTES];
        let mut revalidated = 0usize;
        let mut rejected = 0usize;
        let mut checked_bytes = 0_u64;
        let mut last_progress_report = Instant::now();
        for candidate in candidates {
            if cancellation_token.is_some_and(|token| token.is_cancelled()) {
                return Err(Error::Cancelled);
            }
            if validation.trusted_chunks[candidate.index].is_some() {
                continue;
            }
            if self
                .revalidate_candidate_from_part(&input, candidate, &mut buffer, cancellation_token)
                .await?
            {
                revalidated = revalidated.saturating_add(1);
                trust_resume_candidate(&mut validation, candidate);
            } else {
                rejected = rejected.saturating_add(1);
            }
            checked_bytes = checked_bytes.saturating_add(candidate.length);
            let now = Instant::now();
            if let Some((name, progress)) = input.progress
                && trusted_candidate_bytes > 0
                && should_emit_resume_validation_progress(last_progress_report, now)
            {
                progress.on_resume_validation_progress(
                    name,
                    checked_bytes.min(trusted_candidate_bytes),
                    trusted_candidate_bytes,
                );
                last_progress_report = now;
            }
        }

        log::debug!(
            "Resume sidecar disk revalidation finished for {}: trusted={} rejected={} bytes={}",
            input.part_path.display(),
            revalidated,
            rejected,
            validation.trusted_bytes
        );

        if validation.trusted_count > 0 {
            validation.source = Some(ResumeReuseSource::Sidecar);
        }

        Ok(validation)
    }
}

#[cfg(test)]
mod tests;
