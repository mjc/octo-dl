#[cfg(any(feature = "cli", feature = "tui"))]
use crate::config::DownloadConfig;
use crate::core::SavedCredentials;
#[cfg(any(feature = "cli", feature = "tui"))]
use crate::core::{
    FileAccounting, FileLifecycle, FileProgressState, FileSnapshot, PackageId, PackageKey,
    PackageSnapshot, SessionSnapshot, SessionUrlSnapshot,
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
    #[allow(clippy::disallowed_methods)]
    pub fn set(path: &Path) -> Self {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let lock = LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::env::current_dir().expect("current directory should resolve");
        std::env::set_current_dir(path).expect("current directory should update");
        Self {
            _lock: lock,
            previous,
        }
    }
}

impl Drop for CurrentDirGuard {
    #[allow(clippy::disallowed_methods)]
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.previous);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(any(feature = "cli", feature = "tui"))]
pub enum UrlFixtureStatus {
    Pending,
    Fetched,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(any(feature = "cli", feature = "tui"))]
#[cfg_attr(not(feature = "tui"), derive(Copy))]
pub enum FileFixtureStatus {
    Pending,
    Completed,
    #[cfg(feature = "tui")]
    Error(String),
}

pub fn test_credentials() -> SavedCredentials {
    let key = crate::config::CredentialKey::generate();
    key.persist_for_sessions()
        .expect("test session key should persist");
    SavedCredentials::encrypt_with_key("test@example.com", "hunter2", None, &key)
}

#[cfg(any(feature = "cli", feature = "tui"))]
pub fn package_id(raw: &str, source_url: &str) -> PackageId {
    PackageId::parse_or_key(raw, &PackageKey::new(source_url))
}

pub fn legacy_resume_sidecar_path(path: &str) -> PathBuf {
    crate::download::legacy_json_sidecar_path(path)
}

pub fn write_dummy_legacy_resume_sidecar(path: &str) -> PathBuf {
    let sidecar_path = legacy_resume_sidecar_path(path);
    std::fs::write(&sidecar_path, b"metadata").expect("dummy legacy sidecar should write");
    sidecar_path
}

pub fn write_dummy_legacy_resume_sidecar_for_path(path: &Path) -> PathBuf {
    write_dummy_legacy_resume_sidecar(path.to_string_lossy().as_ref())
}

#[cfg(any(feature = "cli", feature = "tui"))]
pub fn session_snapshot(urls: Vec<(&str, UrlFixtureStatus)>) -> SessionSnapshot {
    let mut session = SessionSnapshot::new(DownloadConfig::default(), test_credentials());
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

#[cfg(any(feature = "cli", feature = "tui"))]
pub fn push_file(
    session: &mut SessionSnapshot,
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
            display_name: package_display_name,
            files: Vec::new(),
            error: None,
        });
        package_id
    };

    let (lifecycle, accounting, visible_completed_bytes) = match status {
        FileFixtureStatus::Pending => (FileLifecycle::Queued, FileAccounting::CurrentRun, 0),
        FileFixtureStatus::Completed => {
            (FileLifecycle::Complete, FileAccounting::Preexisting, size)
        }
        #[cfg(feature = "tui")]
        FileFixtureStatus::Error(message) => (
            FileLifecycle::Failed { message },
            FileAccounting::CurrentRun,
            0,
        ),
    };

    let file = FileSnapshot {
        id: path.to_string().into(),
        package_id,
        source_url,
        path: path.to_string(),
        size,
        lifecycle,
        progress: FileProgressState {
            visible_completed_bytes,
            ..FileProgressState::default()
        },
        accounting,
    };
    let package = session
        .packages
        .iter_mut()
        .find(|package| package.id == package_id)
        .expect("package should exist before pushing fixture file");
    package.files.push(file);
}
