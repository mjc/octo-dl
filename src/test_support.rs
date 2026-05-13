use crate::config::DownloadConfig;
use crate::core::{
    DesiredState, FileLifecycle, FileProgressState, FileSnapshot, PackageId, PackageKey,
    PackageSnapshot, RuntimeState, SavedCredentials, SessionSnapshotV3, SessionUrlSnapshot,
    normalize_snapshot,
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

pub fn package_id(raw: &str, source_url: &str) -> PackageId {
    PackageId::parse_or_key(raw, &PackageKey::new(source_url))
}

pub fn session_snapshot(urls: Vec<(&str, UrlFixtureStatus)>) -> SessionSnapshotV3 {
    let mut session = SessionSnapshotV3::new(DownloadConfig::default(), test_credentials());
    session.urls = urls
        .into_iter()
        .map(|(url, status)| SessionUrlSnapshot {
            url: url.to_string(),
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
    let source_url = session
        .urls
        .get(package_index)
        .map(|entry| entry.url.clone())
        .expect("package_index should map to a tracked url");
    let package_display_name = path
        .split('/')
        .next()
        .unwrap_or(source_url.as_str())
        .to_string();
    let package_id = if let Some(package) = session
        .packages
        .iter()
        .find(|package| package.display_name == package_display_name)
    {
        package.id
    } else {
        let package_id = package_id(&package_display_name, &package_display_name);
        session.packages.push(PackageSnapshot {
            id: package_id,
            key: PackageKey::new(package_display_name.clone()),
            display_name: package_display_name.clone(),
            file_ids: Vec::new(),
            error: None,
        });
        package_id
    };

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
        id: path.to_string().into(),
        package_id,
        source_url: Some(source_url.clone()),
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
    normalize_snapshot(session).expect("test fixture sessions should stay canonical");
}
