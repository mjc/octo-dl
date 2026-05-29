pub(super) const REVALIDATION_BUFFER_BYTES: usize = 128 * 1024;

pub(super) fn revalidation_buffer_len(remaining: u64) -> usize {
    usize::try_from(remaining.min(REVALIDATION_BUFFER_BYTES as u64))
        .unwrap_or(REVALIDATION_BUFFER_BYTES)
}

#[cfg(test)]
mod tests {
    use super::{REVALIDATION_BUFFER_BYTES, revalidation_buffer_len};
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn revalidation_buffer_len_clamps_to_shared_buffer_size(remaining in any::<u64>()) {
            let expected = usize::try_from(remaining.min(REVALIDATION_BUFFER_BYTES as u64))
                .unwrap_or(REVALIDATION_BUFFER_BYTES);
            prop_assert_eq!(revalidation_buffer_len(remaining), expected);
        }
    }
}
