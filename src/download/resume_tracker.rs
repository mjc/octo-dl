use super::resume_state::CURRENT_RESUME_SIDECAR_VERSION;
use super::sidecar_store::{ResumeSidecar, VerifiedChunkRecord, VerifiedChunks};

#[derive(Debug)]
pub(super) struct ResumeTracker {
    file_size: u64,
    expected_condensed_mac: [u8; 8],
    chunk_macs: Vec<Option<[u8; 16]>>,
    verified_chunks: VerifiedChunks,
    verified_positions: Vec<Option<usize>>,
}

impl ResumeTracker {
    pub(super) fn new(
        file_size: u64,
        expected_condensed_mac: [u8; 8],
        chunk_macs: Vec<Option<[u8; 16]>>,
    ) -> Self {
        let mut verified_chunks =
            VerifiedChunks::with_capacity(chunk_macs.iter().flatten().count());
        let mut verified_positions = vec![None; chunk_macs.len()];
        for (position, mac) in chunk_macs.iter().copied().enumerate() {
            let Some(mac) = mac else {
                continue;
            };
            let index = u32::try_from(position).expect("verified chunk index fits in u32");
            verified_positions[position] = Some(verified_chunks.len());
            verified_chunks.push(VerifiedChunkRecord { index, mac });
        }
        Self {
            file_size,
            expected_condensed_mac,
            chunk_macs,
            verified_chunks,
            verified_positions,
        }
    }

    pub(super) fn mark_verified(&mut self, index: u32, mac: [u8; 16]) -> bool {
        let position = index as usize;
        let Some(slot) = self.chunk_macs.get_mut(position) else {
            return false;
        };
        let changed = slot.as_ref() != Some(&mac);
        if !changed {
            return false;
        }
        *slot = Some(mac);
        if let Some(verified_position) = self.verified_positions[position] {
            self.verified_chunks[verified_position].mac = mac;
        } else {
            let insert_at = self
                .verified_chunks
                .binary_search_by_key(&index, |record| record.index)
                .unwrap_or_else(|insert_at| insert_at);
            self.verified_chunks
                .insert(insert_at, VerifiedChunkRecord { index, mac });
            for (offset, record) in self.verified_chunks[insert_at..].iter().enumerate() {
                self.verified_positions[record.index as usize] = Some(insert_at + offset);
            }
        }
        true
    }

    pub(super) fn snapshot(&self) -> ResumeSidecar {
        ResumeSidecar {
            version: CURRENT_RESUME_SIDECAR_VERSION,
            file_size: self.file_size,
            expected_condensed_mac: self.expected_condensed_mac,
            verified_chunks: self.verified_chunks.clone(),
            part_fingerprint: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{VerifiedChunkRecord, VerifiedChunks};
    use super::ResumeTracker;
    use proptest::prelude::*;

    fn expected_verified_chunks(chunk_macs: &[Option<[u8; 16]>]) -> VerifiedChunks {
        chunk_macs
            .iter()
            .enumerate()
            .filter_map(|(index, mac)| {
                mac.map(|mac| VerifiedChunkRecord {
                    index: u32::try_from(index).unwrap(),
                    mac,
                })
            })
            .collect()
    }

    proptest! {
        #[test]
        fn resume_tracker_snapshot_matches_latest_verified_state(
            initial in proptest::collection::vec(proptest::option::of(any::<[u8; 16]>()), 0..16),
            updates in proptest::collection::vec((0usize..32, any::<[u8; 16]>()), 0..32),
        ) {
            let mut tracker = ResumeTracker::new(300_000, [9_u8; 8], initial.clone());
            let mut expected = initial;

            for (raw_index, mac) in updates {
                let changed = if let Some(slot) = expected.get_mut(raw_index) {
                    let changed = slot.as_ref() != Some(&mac);
                    if changed {
                        *slot = Some(mac);
                    }
                    changed
                } else {
                    false
                };
                prop_assert_eq!(tracker.mark_verified(raw_index as u32, mac), changed);
            }

            let snapshot = tracker.snapshot();
            prop_assert_eq!(snapshot.file_size, 300_000);
            prop_assert_eq!(snapshot.expected_condensed_mac, [9_u8; 8]);
            prop_assert_eq!(snapshot.verified_chunks, expected_verified_chunks(&expected));
        }
    }

    #[test]
    fn resume_tracker_rejects_out_of_range_chunk_indexes() {
        let mut tracker = ResumeTracker::new(300_000, [9_u8; 8], vec![None; 2]);

        assert!(!tracker.mark_verified(5, [1_u8; 16]));
        assert!(tracker.snapshot().verified_chunks.is_empty());
    }
}
