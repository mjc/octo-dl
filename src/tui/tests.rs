use super::*;
use crate::state::SavedCredentials as LegacySavedCredentials;
use crate::{
    DownloadConfig, FileEntry, FileEntryStatus, SessionState, UrlEntry, UrlStatus,
    core::{FileLifecycle, SessionRunStatus},
};
use std::env;
use std::path::Path;
use tempfile::tempdir;

use super::app::{FileStatus, UiAction};

struct StateDirectoryGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    previous: Option<std::ffi::OsString>,
}

impl StateDirectoryGuard {
    fn set(path: &Path) -> Self {
        let lock = crate::state::STATE_DIRECTORY_TEST_LOCK.lock().unwrap();
        let previous = env::var_os("STATE_DIRECTORY");
        unsafe { env::set_var("STATE_DIRECTORY", path) };
        Self {
            _lock: lock,
            previous,
        }
    }
}

impl Drop for StateDirectoryGuard {
    fn drop(&mut self) {
        if let Some(ref value) = self.previous {
            unsafe { env::set_var("STATE_DIRECTORY", value) };
        } else {
            unsafe { env::remove_var("STATE_DIRECTORY") };
        }
    }
}

#[test]
fn resume_session_requeues_urls() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());

    let urls = vec![
        UrlEntry {
            url: "https://mega.nz/file/first".to_string(),
            status: UrlStatus::Pending,
        },
        UrlEntry {
            url: "https://mega.nz/file/second".to_string(),
            status: UrlStatus::Fetched,
        },
    ];
    let session = SessionState::new(
        LegacySavedCredentials::encrypt("test@example.com", "hunter2", None),
        DownloadConfig::default(),
        urls,
    );
    session.save().unwrap();

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);

    app.resume_latest_session();

    let expected_urls = vec![
        "https://mega.nz/file/first".to_string(),
        "https://mega.nz/file/second".to_string(),
    ];
    assert_eq!(app.urls, expected_urls);

    let mut url_rx = app.url_rx.take().expect("url_rx should exist");
    assert_eq!(url_rx.try_recv().unwrap(), expected_urls[0]);
    assert_eq!(url_rx.try_recv().unwrap(), expected_urls[1]);
    assert!(url_rx.try_recv().is_err());

    let session_state = app.session.as_ref().expect("session should be present");
    assert!(
        session_state
            .packages
            .iter()
            .all(|package| package.error.is_none() && package.file_ids.is_empty())
    );
}

#[test]
fn resume_session_restores_files_and_only_requeues_remaining_urls() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());

    let mut session = SessionState::new(
        LegacySavedCredentials::encrypt("test@example.com", "hunter2", None),
        DownloadConfig::default(),
        vec![
            UrlEntry {
                url: "https://mega.nz/file/completed".to_string(),
                status: UrlStatus::Fetched,
            },
            UrlEntry {
                url: "https://mega.nz/file/pending".to_string(),
                status: UrlStatus::Fetched,
            },
        ],
    );
    session.files = vec![
        FileEntry {
            key: Some("completed-id".to_string()),
            url_index: 0,
            path: "completed.mkv".to_string(),
            size: 128,
            status: FileEntryStatus::Completed,
        },
        FileEntry {
            key: Some("pending-id".to_string()),
            url_index: 1,
            path: "pending.mkv".to_string(),
            size: 256,
            status: FileEntryStatus::Pending,
        },
    ];
    session.save().unwrap();

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);

    app.resume_latest_session();

    assert_eq!(app.urls, vec!["https://mega.nz/file/pending".to_string()]);
    assert_eq!(app.files.len(), 2);

    let completed = app
        .files
        .iter()
        .find(|file| file.id == "completed.mkv")
        .expect("completed file should be restored");
    assert_eq!(completed.status, FileStatus::Complete);
    assert_eq!(completed.downloaded, completed.size);

    let pending = app
        .files
        .iter()
        .find(|file| file.id == "pending.mkv")
        .expect("pending file should be restored");
    assert_eq!(pending.status, FileStatus::Queued);
    assert_eq!(pending.downloaded, 0);

    let mut url_rx = app.url_rx.take().expect("url_rx should exist");
    assert_eq!(
        url_rx.try_recv().unwrap(),
        "https://mega.nz/file/pending".to_string()
    );
    assert!(url_rx.try_recv().is_err());

    let session_state = app.session.as_ref().expect("session should be present");
    assert!(session_state.packages[0].error.is_none());
    assert!(!session_state.packages[0].file_ids.is_empty());
    assert!(session_state.packages[1].error.is_none());
    assert!(!session_state.packages[1].file_ids.is_empty());
}

#[test]
fn resume_session_restores_retryable_errors_as_queued() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());

    let mut session = SessionState::new(
        LegacySavedCredentials::encrypt("test@example.com", "hunter2", None),
        DownloadConfig::default(),
        vec![UrlEntry {
            url: "https://mega.nz/file/retry".to_string(),
            status: UrlStatus::Fetched,
        }],
    );
    session.files = vec![FileEntry {
        key: Some("retry-id".to_string()),
        url_index: 0,
        path: "retry.mkv".to_string(),
        size: 256,
        status: FileEntryStatus::Error("network failure".to_string()),
    }];
    session.save().unwrap();

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);

    app.resume_latest_session();

    let restored = app
        .files
        .iter()
        .find(|file| file.id == "retry.mkv")
        .expect("retryable error should be restored");
    assert_eq!(restored.status, FileStatus::Queued);
    assert_eq!(restored.downloaded, 0);

    assert_eq!(app.urls, vec!["https://mega.nz/file/retry".to_string()]);
}

#[test]
fn resume_session_does_not_restore_or_requeue_skipped_files() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());

    let mut session = SessionState::new(
        LegacySavedCredentials::encrypt("test@example.com", "hunter2", None),
        DownloadConfig::default(),
        vec![UrlEntry {
            url: "https://mega.nz/file/skipped".to_string(),
            status: UrlStatus::Fetched,
        }],
    );
    session.files = vec![FileEntry {
        key: Some("skipped-id".to_string()),
        url_index: 0,
        path: "skipped.mkv".to_string(),
        size: 256,
        status: FileEntryStatus::Skipped,
    }];
    session.save().unwrap();

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);

    app.resume_latest_session();

    assert!(app.files.is_empty());
    assert!(app.urls.is_empty());

    let mut url_rx = app.url_rx.take().expect("url_rx should exist");
    assert!(url_rx.try_recv().is_err());

    let session_state = app.session.as_ref().expect("session should be present");
    assert!(session_state.packages[0].error.is_none());
    assert_eq!(session_state.files[0].lifecycle, FileLifecycle::Skipped);
}

#[test]
fn sync_session_on_shutdown_keeps_completed_files_in_incomplete_sessions() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());

    let mut session = SessionState::new(
        LegacySavedCredentials::encrypt("test@example.com", "hunter2", None),
        DownloadConfig::default(),
        vec![UrlEntry {
            url: "https://mega.nz/file/root".to_string(),
            status: UrlStatus::Fetched,
        }],
    );
    session.files = vec![
        FileEntry {
            key: Some("completed-id".to_string()),
            url_index: 0,
            path: "completed.mkv".to_string(),
            size: 128,
            status: FileEntryStatus::Completed,
        },
        FileEntry {
            key: Some("pending-id".to_string()),
            url_index: 0,
            path: "pending.mkv".to_string(),
            size: 256,
            status: FileEntryStatus::Pending,
        },
    ];

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);
    app.files = vec![
        app::FileEntry {
            id: "completed.mkv".to_string(),
            name: "completed.mkv".to_string(),
            size: 128,
            downloaded: 128,
            status: FileStatus::Complete,
        },
        app::FileEntry {
            id: "pending.mkv".to_string(),
            name: "pending.mkv".to_string(),
            size: 256,
            downloaded: 0,
            status: FileStatus::Queued,
        },
    ];
    app.session = Some(session.to_v3());

    app.sync_session_for_shutdown();

    let session = app.session.as_ref().expect("session should remain");
    assert_eq!(session.status, SessionRunStatus::Paused);
    assert_eq!(session.files.len(), 2);
    assert!(
        session
            .files
            .iter()
            .any(|file| file.path == "completed.mkv")
    );
    assert!(session.files.iter().any(|file| file.path == "pending.mkv"));
}

#[test]
fn ui_add_urls_enqueues_each_unique_url_once() {
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);
    let mut url_rx = app.url_rx.take().expect("url_rx should exist");

    app.handle_ui_action(UiAction::AddUrls(vec![
        "https://mega.nz/file/one".to_string(),
        "https://mega.nz/file/one".to_string(),
        "https://mega.nz/file/two".to_string(),
    ]));

    assert_eq!(
        app.urls,
        vec![
            "https://mega.nz/file/one".to_string(),
            "https://mega.nz/file/two".to_string()
        ]
    );
    assert_eq!(url_rx.try_recv().unwrap(), "https://mega.nz/file/one");
    assert_eq!(url_rx.try_recv().unwrap(), "https://mega.nz/file/two");
    assert!(url_rx.try_recv().is_err());
}

#[test]
fn ui_retry_file_recomputes_totals() {
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);
    let (url_tx, mut url_rx) = tokio::sync::mpsc::unbounded_channel();
    app.url_tx = url_tx;
    app.upsert_overlay_file(
        app::FileEntry {
            id: "error.bin".to_string(),
            name: "error.bin".to_string(),
            size: 100,
            downloaded: 20,
            status: FileStatus::Error("boom".to_string()),
        },
        Some("https://mega.nz/file/error".to_string()),
        true,
    );
    app.recompute_totals();

    assert_eq!(app.files_total, 0);
    assert_eq!(app.total_downloaded, 20);

    app.handle_ui_action(UiAction::RetryFile("error.bin".to_string()));

    assert_eq!(app.files[0].status, FileStatus::Queued);
    assert_eq!(app.files[0].downloaded, 0);
    assert_eq!(app.files_total, 1);
    assert_eq!(app.total_downloaded, 0);
    assert_eq!(
        url_rx.try_recv().unwrap(),
        "https://mega.nz/file/error".to_string()
    );
}

#[test]
fn ui_delete_file_removes_completed_artifact_from_disk() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("completed.bin");
    std::fs::write(&file_path, b"done").unwrap();

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);
    app.upsert_overlay_file(
        app::FileEntry {
            id: file_path.to_string_lossy().into_owned(),
            name: file_path.to_string_lossy().into_owned(),
            size: 4,
            downloaded: 4,
            status: FileStatus::Complete,
        },
        Some("https://mega.nz/file/completed".to_string()),
        false,
    );

    app.handle_ui_action(UiAction::DeleteFile(
        file_path.to_string_lossy().into_owned(),
    ));

    assert!(app.files.is_empty());
    assert!(!file_path.exists());
}

#[test]
fn ui_reset_file_resets_progress_and_requeues_url() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("active.bin");
    let part_path = dir.path().join("active.bin.part");
    let sidecar_path = dir.path().join("active.bin.part.meta.json");
    std::fs::write(&file_path, b"complete").unwrap();
    std::fs::write(&part_path, b"partial").unwrap();
    std::fs::write(&sidecar_path, b"{}").unwrap();

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);
    let (url_tx, mut url_rx) = tokio::sync::mpsc::unbounded_channel();
    app.url_tx = url_tx;
    app.upsert_overlay_file(
        app::FileEntry {
            id: "active.bin".to_string(),
            name: file_path.to_string_lossy().into_owned(),
            size: 100,
            downloaded: 80,
            status: FileStatus::Downloading,
        },
        Some("https://mega.nz/file/reset".to_string()),
        true,
    );

    app.handle_ui_action(UiAction::ResetFile("active.bin".to_string()));

    assert_eq!(app.files[0].status, FileStatus::Queued);
    assert_eq!(app.files[0].downloaded, 0);
    assert_eq!(
        url_rx.try_recv().unwrap(),
        "https://mega.nz/file/reset".to_string()
    );
    assert!(!file_path.exists());
    assert!(!part_path.exists());
    assert!(!sidecar_path.exists());
}

#[test]
fn resume_session_deduplicates_duplicate_file_entries_by_path() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());

    let mut session = SessionState::new(
        LegacySavedCredentials::encrypt("test@example.com", "hunter2", None),
        DownloadConfig::default(),
        vec![UrlEntry {
            url: "https://mega.nz/file/root".to_string(),
            status: UrlStatus::Fetched,
        }],
    );
    session.files = vec![
        FileEntry {
            key: Some("legacy-key".to_string()),
            url_index: 0,
            path: "duplicate.mkv".to_string(),
            size: 128,
            status: FileEntryStatus::Pending,
        },
        FileEntry {
            key: Some("new-key".to_string()),
            url_index: 0,
            path: "duplicate.mkv".to_string(),
            size: 128,
            status: FileEntryStatus::Completed,
        },
    ];
    session.save().unwrap();

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);

    app.resume_latest_session();

    assert_eq!(app.files.len(), 1);
    let file = app
        .files
        .iter()
        .find(|entry| entry.id == "duplicate.mkv")
        .expect("duplicate file should be collapsed into one row");
    assert_eq!(file.status, FileStatus::Complete);
    assert_eq!(app.urls, Vec::<String>::new());

    let session = app.session.as_ref().expect("session should be present");
    assert_eq!(session.files.len(), 1);
    assert_eq!(session.files[0].path, "duplicate.mkv");
    assert_eq!(session.files[0].lifecycle, FileLifecycle::Complete);
}
