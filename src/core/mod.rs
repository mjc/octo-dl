pub mod commands;
pub mod model;
pub mod progress;
pub mod reducer;
pub mod restart;
pub mod session;

pub use commands::CoreCommand;
pub use model::{
    DownloadState, FileAccounting, FileId, FileLifecycle, FileProgressState, FileState, PackageId,
    PackageKey, PackageProgressState, PackageState, PackageStatus, SessionMeta, SessionRunStatus,
    TotalsState, UrlId, visible_completed_bytes_for_display,
};
pub use progress::{DownloadProgressSink, ProgressDelta, RateEstimator};
pub use reducer::{
    CoreEffect, CoreEffects, CoreEvent, PackageCollision, ResolvedFile, ResolvedPackage, reduce,
    snapshot_from_state,
};
pub use restart::{
    FilesystemFile, FilesystemSnapshot, PartialFileSnapshot, RestartSnapshot,
    build_restart_snapshot, reconcile_restart, scan_filesystem,
};
#[cfg(feature = "tui")]
pub(crate) use session::SessionCompletionFacts;
pub use session::{
    FileSnapshot, PackageSnapshot, SavedCredentials, SavedMegaSession, SessionSnapshot,
    SessionUrlSnapshot, decode_credential_key, decrypt_credential, decrypt_credential_with_key,
    encrypt_credential, encrypt_credential_with_key, generate_credential_key, queued_file_snapshot,
    validate_snapshot,
};
