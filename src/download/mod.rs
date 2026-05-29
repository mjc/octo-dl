//! Core download logic and abstractions.

mod callbacks;
mod collect;
mod downloader;
mod finalize;
mod inspect;
mod resume_reverify;
mod revalidate;
mod revalidate_part;
mod session;
mod sidecar;
mod sidecar_state;
mod sidecar_store;
mod sidecar_writer;
#[cfg(test)]
mod test_support;
mod transfer;
mod verify;

pub use self::callbacks::{DownloadProgress, NoProgress};
pub use self::collect::{
    CollectedFiles, DownloadItem, OwnedDownloadItem, infer_package_display_name, infer_package_id,
};
pub(crate) use self::downloader::resume_validation_percent;
pub use self::downloader::{
    Downloader, ResumeReuse, ResumeReuseSource, ResumeReverify, delete_download_artifacts,
    delete_resume_artifacts, fetch_public_nodes,
};
pub use self::inspect::FileStatus;
pub(crate) use self::inspect::{ObservedLocalFile, classify_observed_local_file};
pub(crate) use self::sidecar::{
    has_resume_sidecar, legacy_binary_sidecar_path, legacy_json_sidecar_path, part_path,
    resume_sidecar_verified_bytes, sidecar_path,
};
use self::sidecar_store::*;
use self::sidecar_writer::*;
pub use self::verify::CompletedFileVerify;
