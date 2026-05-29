//! Core download logic and abstractions.

mod callbacks;
mod collect;
mod downloader;
mod finalize;
mod inspect;
mod resume_reverify;
mod revalidate;
mod session;
mod sidecar;
mod sidecar_state;
mod sidecar_store;
mod sidecar_writer;
#[cfg(test)]
mod test_support;
mod transfer;
mod verify;

use self::callbacks::{
    ChunkVerifiedState, DownloadCallbackState, ProgressCallbackState,
    ResumeValidationStatusProgress,
};
pub use self::callbacks::{DownloadProgress, NoProgress};
pub use self::collect::{
    CollectedFiles, DownloadItem, OwnedDownloadItem, infer_package_display_name, infer_package_id,
};
pub(crate) use self::downloader::resume_validation_percent;
use self::downloader::{CURRENT_RESUME_SIDECAR_VERSION, should_reuse_resume_state};
pub use self::downloader::{
    Downloader, ResumeReuse, ResumeReuseSource, ResumeReverify, delete_download_artifacts,
    delete_resume_artifacts, fetch_public_nodes,
};
use self::finalize::DownloadFinishContext;
pub use self::inspect::FileStatus;
pub(crate) use self::inspect::{ObservedLocalFile, classify_observed_local_file};
pub(crate) use self::revalidate::REVALIDATION_BUFFER_BYTES;
use self::sidecar::*;
pub(crate) use self::sidecar::{
    has_resume_sidecar, legacy_binary_sidecar_path, legacy_json_sidecar_path, part_path,
    resume_sidecar_verified_bytes, sidecar_path,
};
use self::sidecar_state::*;
use self::sidecar_store::*;
use self::sidecar_writer::*;
pub use self::verify::CompletedFileVerify;
pub(crate) use self::verify::expected_mac;
