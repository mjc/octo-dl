//! Core download logic and abstractions.

mod callbacks;
mod collect;
mod downloader;
mod finalize;
mod inspect;
mod package_identity;
mod resume_reverify;
mod resume_state;
mod resume_tracker;
mod resume_validation;
mod revalidate;
mod revalidate_part;
mod revalidation_buffer;
mod session;
mod session_run;
mod sidecar;
mod sidecar_store;
mod sidecar_writer;
#[cfg(test)]
mod test_support;
mod transfer;
mod transfer_prepare;
mod verify;

pub use self::callbacks::{DownloadProgress, NoProgress};
pub use self::collect::{CollectedFiles, DownloadItem, OwnedDownloadItem};
pub use self::downloader::{
    Downloader, delete_download_artifacts, delete_resume_artifacts, fetch_public_nodes,
};
pub use self::inspect::FileStatus;
pub(crate) use self::inspect::{ObservedLocalFile, classify_observed_local_file};
pub use self::package_identity::{infer_package_display_name, infer_package_id};
pub(crate) use self::resume_state::resume_validation_percent;
pub use self::resume_state::{ResumeReuse, ResumeReuseSource, ResumeReverify};
pub(crate) use self::sidecar::{
    has_resume_sidecar, legacy_binary_sidecar_path, legacy_json_sidecar_path, part_path,
    resume_sidecar_verified_bytes, sidecar_path,
};
pub use self::verify::CompletedFileVerify;
