use crate::config::DownloadConfig;
use crate::core::{
    DesiredState, FileLifecycle, FileProgressState, FileSnapshot, PackageSnapshot, RuntimeState,
    SavedCredentials, SessionSnapshotV3,
};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

pub struct StateDirectoryGuard {
    _guard: crate::core::session::StateDirectoryTestGuard,
}

impl StateDirectoryGuard {
    pub fn set(path: &Path) -> Self {
        Self {
            _guard: crate::core::session::set_state_directory_for_test(path),
        }
    }
}

pub struct CurrentDirGuard {
    _lock: MutexGuard<'static, ()>,
    previous: PathBuf,
}

impl CurrentDirGuard {
    pub fn set(path: &Path) -> Self {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let lock = LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::current_dir().expect("current directory should resolve");
        std::env::set_current_dir(path).expect("current directory should update");
        Self {
            _lock: lock,
            previous,
        }
    }
}

impl Drop for CurrentDirGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.previous);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlFixtureStatus {
    Pending,
    Fetched,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileFixtureStatus {
    Pending,
    Completed,
    Skipped,
    Error(String),
}

pub fn test_credentials() -> SavedCredentials {
    SavedCredentials::encrypt("test@example.com", "hunter2", None)
}

pub fn session_snapshot(urls: Vec<(&str, UrlFixtureStatus)>) -> SessionSnapshotV3 {
    let mut session = SessionSnapshotV3::new(DownloadConfig::default(), test_credentials());
    session.packages = urls
        .into_iter()
        .map(|(url, status)| PackageSnapshot {
            id: url.to_string(),
            source_url: url.to_string(),
            display_name: url.to_string(),
            file_ids: Vec::new(),
            error: match status {
                UrlFixtureStatus::Error(message) => Some(message),
                UrlFixtureStatus::Pending | UrlFixtureStatus::Fetched => None,
            },
        })
        .collect();
    session
}

pub fn push_file(
    session: &mut SessionSnapshotV3,
    package_index: usize,
    path: &str,
    size: u64,
    status: FileFixtureStatus,
) {
    let package = session
        .packages
        .get_mut(package_index)
        .expect("package_index should exist");
    if !package.file_ids.iter().any(|file_id| file_id == path) {
        package.file_ids.push(path.to_string());
    }

    let (lifecycle, desired, active, counts_in_run_totals, visible_completed_bytes, message) =
        match status {
            FileFixtureStatus::Pending => (
                FileLifecycle::Queued,
                DesiredState::Present,
                false,
                true,
                0,
                None,
            ),
            FileFixtureStatus::Completed => (
                FileLifecycle::Complete,
                DesiredState::Present,
                false,
                false,
                size,
                None,
            ),
            FileFixtureStatus::Skipped => (
                FileLifecycle::Skipped,
                DesiredState::Suppressed,
                false,
                false,
                0,
                None,
            ),
            FileFixtureStatus::Error(message) => (
                FileLifecycle::Failed,
                DesiredState::Present,
                false,
                true,
                0,
                Some(message),
            ),
        };

    session.files.push(FileSnapshot {
        id: path.to_string(),
        package_id: package.id.clone(),
        source_url: Some(package.source_url.clone()),
        path: path.to_string(),
        size,
        lifecycle,
        progress: FileProgressState {
            visible_completed_bytes,
            ..FileProgressState::default()
        },
        desired,
        runtime: RuntimeState {
            counts_in_run_totals,
            active,
            preexisting_complete: false,
            reused_chunks: 0,
        },
        message,
    });
}
