/// Bytes and chunks reused from resumable state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeReuse {
    pub chunks: usize,
    pub bytes: u64,
    pub source: ResumeReuseSource,
}

/// Result of manually checking resumable state without starting a download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeReverify {
    pub sidecar_loaded: bool,
    pub chunks: usize,
    pub bytes: u64,
}

/// Source of reused chunk state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeReuseSource {
    Sidecar,
}

pub(super) const CURRENT_RESUME_SIDECAR_VERSION: u32 = 2;

pub(super) const fn should_reuse_resume_state(
    force_overwrite: bool,
    trust_resume_state: bool,
) -> bool {
    !force_overwrite && trust_resume_state
}

#[must_use]
pub(crate) fn resume_validation_percent(checked_bytes: u64, total_bytes: u64) -> u64 {
    if total_bytes == 0 {
        return 0;
    }
    ((u128::from(checked_bytes.min(total_bytes)) * 100) / u128::from(total_bytes)) as u64
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{resume_validation_percent, should_reuse_resume_state};

    #[test]
    fn resume_state_is_reused_only_for_session_tracked_files() {
        assert!(should_reuse_resume_state(false, true));
        assert!(!should_reuse_resume_state(false, false));
        assert!(!should_reuse_resume_state(true, true));
    }

    proptest! {
        #[test]
        fn resume_validation_percent_clamps_checked_bytes(
            checked_bytes in any::<u64>(),
            total_bytes in 1u64..u64::MAX,
        ) {
            let expected =
                ((u128::from(checked_bytes.min(total_bytes)) * 100) / u128::from(total_bytes)) as u64;
            prop_assert_eq!(resume_validation_percent(checked_bytes, total_bytes), expected);
        }

        #[test]
        fn resume_validation_percent_is_zero_for_empty_totals(checked_bytes in any::<u64>()) {
            prop_assert_eq!(resume_validation_percent(checked_bytes, 0), 0);
        }
    }
}
