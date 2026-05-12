pub mod commands;
pub mod model;
pub mod progress;
pub mod reducer;
pub mod restart;
pub mod session;

pub use commands::CoreCommand;
pub use model::{
    DesiredState, DownloadState, FileId, FileLifecycle, FileProgressState, FileState, PackageId,
    PackageKey, PackageState, PackageStatus, RuntimeState, SessionMeta, SessionRunStatus,
    TotalsState, UrlId,
};
pub use progress::{DownloadProgressSink, ProgressDelta, RateEstimator};
pub use reducer::{
    CoreEffect, CoreEvent, PackageCollision, ResolvedFile, ResolvedPackage, reduce,
    snapshot_from_state,
};
pub use restart::{
    FilesystemFile, FilesystemSnapshot, PartialFileSnapshot, RestartSnapshot, reconcile_restart,
    scan_filesystem,
};
pub use session::{
    FileSnapshot, PackageSnapshot, SavedCredentials, SessionSnapshotV3, SessionUrlSnapshot,
    decrypt_credential, encrypt_credential,
};
