use std::path::Path;

use crate::fs::FileSystem;

use super::{
    legacy_binary_sidecar_path, legacy_json_sidecar_path, part_path, resume_sidecar_verified_bytes,
    sidecar_path,
};

/// Classification of a file's current state on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    /// File exists with the expected size — fully downloaded.
    Complete,
    /// A `.part` file exists (partial download from a previous run).
    Partial,
    /// Neither the final file nor a `.part` file exists.
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ObservedLocalFile {
    pub final_size: Option<u64>,
    pub part_size: Option<u64>,
    pub part_allocated_bytes: Option<u64>,
    pub has_sidecar: bool,
    pub verified_resume_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InspectedLocalFile {
    pub status: FileStatus,
    pub existing_partial_bytes: u64,
    pub has_resume_sidecar: bool,
    pub verified_resume_bytes: u64,
}

impl Default for InspectedLocalFile {
    fn default() -> Self {
        Self {
            status: FileStatus::Missing,
            existing_partial_bytes: 0,
            has_resume_sidecar: false,
            verified_resume_bytes: 0,
        }
    }
}

pub(crate) fn classify_observed_local_file(
    observed: ObservedLocalFile,
    expected_size: u64,
    force_overwrite: bool,
) -> InspectedLocalFile {
    if force_overwrite {
        return InspectedLocalFile {
            status: FileStatus::Missing,
            ..InspectedLocalFile::default()
        };
    }

    if observed.final_size == Some(expected_size) {
        return InspectedLocalFile {
            status: FileStatus::Complete,
            ..InspectedLocalFile::default()
        };
    }

    if let Some(part_size) = observed.part_size {
        let baseline_partial_bytes = observed.part_allocated_bytes.unwrap_or_else(|| {
            if observed.has_sidecar {
                observed.verified_resume_bytes
            } else {
                part_size
            }
        });
        let existing_partial_bytes = baseline_partial_bytes
            .max(observed.verified_resume_bytes)
            .min(part_size)
            .min(expected_size);
        return InspectedLocalFile {
            status: FileStatus::Partial,
            existing_partial_bytes,
            has_resume_sidecar: observed.has_sidecar,
            verified_resume_bytes: observed
                .verified_resume_bytes
                .min(part_size)
                .min(expected_size),
        };
    }

    InspectedLocalFile {
        status: FileStatus::Missing,
        ..InspectedLocalFile::default()
    }
}

pub(crate) async fn inspect_local_file<F: FileSystem>(
    fs: &F,
    path: &str,
    expected_size: u64,
    force_overwrite: bool,
) -> InspectedLocalFile {
    let part_path = part_path(path);
    let binary_sidecar_path = sidecar_path(path);
    let legacy_binary_sidecar_path = legacy_binary_sidecar_path(path);
    let legacy_sidecar_path = legacy_json_sidecar_path(path);
    let part_fingerprint = fs.file_fingerprint(&part_path).await;
    let observed = ObservedLocalFile {
        final_size: fs.file_size(Path::new(path)).await,
        part_size: fs.file_size(&part_path).await,
        part_allocated_bytes: part_fingerprint.and_then(|fingerprint| fingerprint.allocated_bytes),
        has_sidecar: fs.file_exists(&binary_sidecar_path).await
            || fs.file_exists(&legacy_binary_sidecar_path).await
            || fs.file_exists(&legacy_sidecar_path).await,
        verified_resume_bytes: resume_sidecar_verified_bytes(path).unwrap_or(0),
    };
    classify_observed_local_file(observed, expected_size, force_overwrite)
}

#[cfg(test)]
mod tests;
