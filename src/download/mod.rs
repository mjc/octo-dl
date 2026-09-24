//! Core download logic and abstractions.

use std::time::Duration;

mod callbacks;
mod collect;
mod downloader;
mod finalize;
mod inspect;
mod package_identity;
mod path;
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

pub(crate) use path::resolve_output_path_under_root;

#[allow(dead_code)]
pub(crate) fn build_http_client() -> mega::Result<reqwest::Client> {
    Ok(mega::http_client_builder()?
        .pool_idle_timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(8)
        .tcp_keepalive(Duration::from_secs(30))
        .build()?)
}

/// Returns unused glibc heap pages to the OS after a large transfer.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub(crate) fn trim_allocator() {
    unsafe extern "C" {
        fn malloc_trim(pad: usize) -> std::ffi::c_int;
    }

    // `malloc_trim` is thread-safe and only releases unused allocator pages.
    unsafe {
        let _ = malloc_trim(0);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub(crate) const fn trim_allocator() {}

pub use self::callbacks::{DownloadProgress, NoProgress};
pub use self::collect::{CollectedFiles, DownloadItem, OwnedDownloadItem};
pub use self::downloader::{
    Downloader, delete_download_artifacts, delete_resume_artifacts, fetch_public_nodes,
};
pub use self::inspect::FileStatus;
pub(crate) use self::inspect::{ObservedLocalFile, classify_observed_local_file};
pub use self::package_identity::{infer_package_display_name, infer_package_id};
#[cfg(feature = "cli")]
pub(crate) use self::resume_state::resume_validation_percent;
pub use self::resume_state::{ResumeReuse, ResumeReuseSource, ResumeReverify};
pub(crate) fn legacy_part_path(path: &str) -> std::path::PathBuf {
    self::sidecar::legacy_part_path(path)
}
pub(crate) use self::sidecar::{has_resume_sidecar, part_path, resume_sidecar_verified_bytes};
#[cfg(any(feature = "tui", test))]
pub(crate) use self::sidecar::{
    legacy_binary_sidecar_path, legacy_json_sidecar_path, sidecar_path,
};
pub use self::verify::CompletedFileVerify;

#[cfg(test)]
mod tests {
    use super::build_http_client;

    #[test]
    fn shared_http_client_constructor_builds() {
        assert!(build_http_client().is_ok());
    }
}
