use std::time::Instant;

use tokio_util::sync::CancellationToken;

use crate::error::{Error, Result};
use crate::fs::FileSystem;

use super::resume_state::ResumeReuseSource;
use super::resume_validation::{
    ResumeValidation, SidecarValidationInput, TrustedResumeChunkCandidate,
    should_emit_resume_validation_progress, trust_resume_candidate,
};
use super::revalidation_buffer::{REVALIDATION_BUFFER_BYTES, revalidation_buffer_len};

pub(super) async fn revalidate_candidate_from_part<F: FileSystem>(
    fs: &F,
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

    while offset < end {
        if cancellation_token.is_some_and(|token| token.is_cancelled()) {
            return Err(Error::Cancelled);
        }
        let read_len = revalidation_buffer_len(end - offset);
        let read_buffer = &mut buffer[..read_len];
        if fs
            .read_exact_at(input.part_path, offset, read_buffer)
            .await
            .is_err()
        {
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

pub(super) async fn revalidate_candidates_from_part<F: FileSystem>(
    fs: &F,
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
        if validation
            .trusted_chunks
            .get(candidate.index)
            .is_some_and(Option::is_some)
        {
            continue;
        }
        if revalidate_candidate_from_part(fs, &input, candidate, &mut buffer, cancellation_token)
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

#[cfg(test)]
mod tests;
