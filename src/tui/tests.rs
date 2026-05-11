use super::*;
use crate::{
    core::{CoreEvent, FileLifecycle, ResolvedFile, ResolvedPackage, SessionRunStatus},
    test_support::{FileFixtureStatus, UrlFixtureStatus, push_file, session_snapshot},
    tui::{
        draw::draw,
        event::{DownloadEvent, DownloadRequest, QueuedFile},
        input::{handle_input, handle_paste},
        visible::TuiRow,
    },
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend, layout::Position};
use std::env;
use std::path::Path;
use sysinfo::System;
use tempfile::tempdir;
use tokio::sync::{mpsc, watch};

use super::app::{FileStatus, Popup, UiAction};

struct StateDirectoryGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    previous: Option<std::ffi::OsString>,
}

impl StateDirectoryGuard {
    fn set(path: &Path) -> Self {
        let lock = crate::core::session::STATE_DIRECTORY_TEST_LOCK
            .lock()
            .unwrap();
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

    let session = session_snapshot(vec![
        ("https://mega.nz/file/first", UrlFixtureStatus::Pending),
        ("https://mega.nz/file/second", UrlFixtureStatus::Fetched),
    ]);
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
    assert_eq!(
        url_rx.try_recv().unwrap(),
        DownloadRequest::SubmitUrl {
            url: expected_urls[0].clone()
        }
    );
    assert_eq!(
        url_rx.try_recv().unwrap(),
        DownloadRequest::SubmitUrl {
            url: expected_urls[1].clone()
        }
    );
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
fn resume_session_restores_email_password_without_restoring_mfa() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());

    let mut session = session_snapshot(vec![(
        "https://mega.nz/file/pending",
        UrlFixtureStatus::Pending,
    )]);
    session.credentials =
        crate::core::SavedCredentials::encrypt("saved@example.com", "saved-pass", Some("654321"));
    session.save().unwrap();

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);

    app.resume_latest_session();

    assert_eq!(app.login.email(), "saved@example.com");
    assert_eq!(app.login.password(), "saved-pass");
    assert!(app.login.mfa().is_empty());
}

#[test]
fn resume_session_does_not_override_existing_login_credentials() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());

    let mut session = session_snapshot(vec![(
        "https://mega.nz/file/pending",
        UrlFixtureStatus::Pending,
    )]);
    session.credentials =
        crate::core::SavedCredentials::encrypt("stale@example.com", "stale-pass", Some("654321"));
    session.save().unwrap();

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);
    assert!(app.login.set_credentials(
        "config@example.com".to_string(),
        "config-pass".to_string(),
        String::new()
    ));

    app.resume_latest_session();

    assert_eq!(app.login.email(), "config@example.com");
    assert_eq!(app.login.password(), "config-pass");
    assert!(app.login.mfa().is_empty());
}

#[test]
fn resume_session_restores_files_and_only_requeues_remaining_urls() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());

    let mut session = session_snapshot(vec![
        ("https://mega.nz/file/completed", UrlFixtureStatus::Fetched),
        ("https://mega.nz/file/pending", UrlFixtureStatus::Fetched),
    ]);
    push_file(
        &mut session,
        0,
        "completed.mkv",
        128,
        FileFixtureStatus::Completed,
    );
    push_file(
        &mut session,
        1,
        "pending.mkv",
        256,
        FileFixtureStatus::Pending,
    );
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
        DownloadRequest::ResumeFileIds {
            source_url: "https://mega.nz/file/pending".to_string(),
            file_ids: vec!["pending.mkv".to_string()],
            attempt_ids: std::collections::HashMap::new(),
        }
    );
    assert!(url_rx.try_recv().is_err());

    let session_state = app.session.as_ref().expect("session should be present");
    assert!(session_state.packages[0].error.is_none());
    assert!(!session_state.packages[0].file_ids.is_empty());
    assert!(session_state.packages[1].error.is_none());
    assert!(!session_state.packages[1].file_ids.is_empty());
}

#[test]
fn resume_session_requeues_package_url_once_for_multiple_pending_files() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());

    let mut session = session_snapshot(vec![(
        "https://mega.nz/folder/pending",
        UrlFixtureStatus::Fetched,
    )]);
    push_file(
        &mut session,
        0,
        "episode-1.mkv",
        128,
        FileFixtureStatus::Pending,
    );
    push_file(
        &mut session,
        0,
        "episode-2.mkv",
        256,
        FileFixtureStatus::Pending,
    );
    session.save().unwrap();

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);

    app.resume_latest_session();

    assert_eq!(app.urls, vec!["https://mega.nz/folder/pending".to_string()]);

    let mut url_rx = app.url_rx.take().expect("url_rx should exist");
    if let DownloadRequest::ResumeFileIds {
        source_url,
        mut file_ids,
        ..
    } = url_rx.try_recv().unwrap()
    {
        assert_eq!(source_url, "https://mega.nz/folder/pending");
        file_ids.sort();
        assert_eq!(file_ids, vec!["episode-1.mkv", "episode-2.mkv"]);
    } else {
        panic!("expected ResumeFileIds request");
    }
    assert!(url_rx.try_recv().is_err());
}

#[test]
fn resume_session_restores_retryable_errors_as_queued() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());

    let mut session = session_snapshot(vec![(
        "https://mega.nz/file/retry",
        UrlFixtureStatus::Fetched,
    )]);
    push_file(
        &mut session,
        0,
        "retry.mkv",
        256,
        FileFixtureStatus::Error("network failure".to_string()),
    );
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

    let mut session = session_snapshot(vec![(
        "https://mega.nz/file/skipped",
        UrlFixtureStatus::Fetched,
    )]);
    push_file(
        &mut session,
        0,
        "skipped.mkv",
        256,
        FileFixtureStatus::Skipped,
    );
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

    let mut session = session_snapshot(vec![(
        "https://mega.nz/file/root",
        UrlFixtureStatus::Fetched,
    )]);
    push_file(
        &mut session,
        0,
        "completed.mkv",
        128,
        FileFixtureStatus::Completed,
    );
    push_file(
        &mut session,
        0,
        "pending.mkv",
        256,
        FileFixtureStatus::Pending,
    );

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
    app.session = Some(session);

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
    assert_eq!(
        url_rx.try_recv().unwrap(),
        DownloadRequest::SubmitUrl {
            url: "https://mega.nz/file/one".to_string()
        }
    );
    assert_eq!(
        url_rx.try_recv().unwrap(),
        DownloadRequest::SubmitUrl {
            url: "https://mega.nz/file/two".to_string()
        }
    );
    assert!(url_rx.try_recv().is_err());
}

#[test]
fn submitted_url_bootstraps_session_for_shutdown_persistence() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);

    app.submit_url("https://mega.nz/file/pending".to_string());
    app.sync_session_for_shutdown();

    let session = crate::core::SessionSnapshotV3::latest().expect("session should be saved");
    assert_eq!(session.status, SessionRunStatus::Paused);
    assert!(
        session
            .packages
            .iter()
            .any(|package| package.source_url == "https://mega.nz/file/pending")
    );
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
        DownloadRequest::ResumeFileIds {
            source_url: "https://mega.nz/file/error".to_string(),
            file_ids: vec!["error.bin".to_string()],
            attempt_ids: std::collections::HashMap::from([("error.bin".to_string(), 1)]),
        }
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
fn ui_delete_core_backed_file_removes_output_and_resume_artifacts() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("core-backed.bin");
    let part_path = dir.path().join("core-backed.bin.part");
    let sidecar_path = dir.path().join("core-backed.bin.part.meta.json");
    std::fs::write(&file_path, b"done").unwrap();
    std::fs::write(&part_path, b"partial").unwrap();
    std::fs::write(&sidecar_path, b"{}").unwrap();

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);
    let file_id = file_path.to_string_lossy().into_owned();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: "https://mega.nz/file/core-delete".to_string(),
            source_url: "https://mega.nz/file/core-delete".to_string(),
            display_name: "Core Delete".to_string(),
            files: vec![ResolvedFile {
                file_id: file_id.clone(),
                path: file_id.clone(),
                size: 4,
            }],
            collision: None,
        },
    });
    app.apply_core_event(CoreEvent::FileCompleted {
        file_id: file_id.clone(),
    });

    app.handle_ui_action(UiAction::DeleteFile(file_id));

    assert!(!file_path.exists());
    assert!(!part_path.exists());
    assert!(!sidecar_path.exists());
}

#[test]
fn deleted_file_completion_event_redeletes_output_artifacts() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("late-complete.bin");
    let part_path = dir.path().join("late-complete.bin.part");
    let sidecar_path = dir.path().join("late-complete.bin.part.meta.json");
    std::fs::write(&file_path, b"done").unwrap();
    std::fs::write(&part_path, b"partial").unwrap();
    std::fs::write(&sidecar_path, b"{}").unwrap();

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);
    let file_id = file_path.to_string_lossy().into_owned();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: "https://mega.nz/file/late-complete".to_string(),
            source_url: "https://mega.nz/file/late-complete".to_string(),
            display_name: "Late Complete".to_string(),
            files: vec![ResolvedFile {
                file_id: file_id.clone(),
                path: file_id.clone(),
                size: 4,
            }],
            collision: None,
        },
    });
    app.apply_core_event(CoreEvent::FileStarted {
        file_id: file_id.clone(),
        size: 4,
    });

    app.handle_ui_action(UiAction::DeleteFile(file_id.clone()));

    std::fs::write(&file_path, b"done").unwrap();
    std::fs::write(&part_path, b"partial").unwrap();
    std::fs::write(&sidecar_path, b"{}").unwrap();
    app.handle_download_event(DownloadEvent::FileComplete {
        id: file_id,
        attempt_id: 0,
    });

    assert!(!file_path.exists());
    assert!(!part_path.exists());
    assert!(!sidecar_path.exists());
}

#[test]
fn deleted_file_stays_deleted_after_cancel_then_completion_events() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("late-cancel-complete.bin");
    let part_path = dir.path().join("late-cancel-complete.bin.part");
    let sidecar_path = dir.path().join("late-cancel-complete.bin.part.meta.json");
    std::fs::write(&file_path, b"done").unwrap();
    std::fs::write(&part_path, b"partial").unwrap();
    std::fs::write(&sidecar_path, b"{}").unwrap();

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);
    let file_id = file_path.to_string_lossy().into_owned();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: "https://mega.nz/file/late-cancel-complete".to_string(),
            source_url: "https://mega.nz/file/late-cancel-complete".to_string(),
            display_name: "Late Cancel Complete".to_string(),
            files: vec![ResolvedFile {
                file_id: file_id.clone(),
                path: file_id.clone(),
                size: 4,
            }],
            collision: None,
        },
    });
    app.apply_core_event(CoreEvent::FileStarted {
        file_id: file_id.clone(),
        size: 4,
    });

    app.handle_ui_action(UiAction::DeleteFile(file_id.clone()));
    app.handle_download_event(DownloadEvent::FileCancelled {
        id: file_id.clone(),
        attempt_id: 0,
    });

    std::fs::write(&file_path, b"done").unwrap();
    std::fs::write(&part_path, b"partial").unwrap();
    std::fs::write(&sidecar_path, b"{}").unwrap();
    app.handle_download_event(DownloadEvent::FileComplete {
        id: file_id.clone(),
        attempt_id: 0,
    });

    assert!(app.files.is_empty());
    assert!(app.deleted_files.contains(&file_id));
    assert!(!file_path.exists());
    assert!(!part_path.exists());
    assert!(!sidecar_path.exists());
}

#[test]
fn deleted_file_error_event_redeletes_output_artifacts() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("late-error.bin");
    let part_path = dir.path().join("late-error.bin.part");
    let sidecar_path = dir.path().join("late-error.bin.part.meta.json");
    std::fs::write(&file_path, b"done").unwrap();
    std::fs::write(&part_path, b"partial").unwrap();
    std::fs::write(&sidecar_path, b"{}").unwrap();

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);
    let file_id = file_path.to_string_lossy().into_owned();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: "https://mega.nz/file/late-error".to_string(),
            source_url: "https://mega.nz/file/late-error".to_string(),
            display_name: "Late Error".to_string(),
            files: vec![ResolvedFile {
                file_id: file_id.clone(),
                path: file_id.clone(),
                size: 4,
            }],
            collision: None,
        },
    });
    app.apply_core_event(CoreEvent::FileStarted {
        file_id: file_id.clone(),
        size: 4,
    });

    app.handle_ui_action(UiAction::DeleteFile(file_id.clone()));

    std::fs::write(&file_path, b"done").unwrap();
    std::fs::write(&part_path, b"partial").unwrap();
    std::fs::write(&sidecar_path, b"{}").unwrap();
    app.handle_download_event(DownloadEvent::FileError {
        id: file_id,
        error: "boom".to_string(),
        attempt_id: 0,
    });

    assert!(app.files.is_empty());
    assert!(!file_path.exists());
    assert!(!part_path.exists());
    assert!(!sidecar_path.exists());
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
        DownloadRequest::ResumeFileIds {
            source_url: "https://mega.nz/file/reset".to_string(),
            file_ids: vec!["active.bin".to_string()],
            attempt_ids: std::collections::HashMap::from([("active.bin".to_string(), 1)]),
        }
    );
    assert!(!file_path.exists());
    assert!(!part_path.exists());
    assert!(!sidecar_path.exists());
}

#[test]
fn reset_file_ignores_late_completion_until_new_attempt_starts() {
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);
    app.upsert_overlay_file(
        app::FileEntry {
            id: "active.bin".to_string(),
            name: "active.bin".to_string(),
            size: 100,
            downloaded: 80,
            status: FileStatus::Downloading,
        },
        Some("https://mega.nz/file/reset".to_string()),
        true,
    );

    app.handle_ui_action(UiAction::ResetFile("active.bin".to_string()));
    app.handle_download_event(DownloadEvent::FileComplete {
        id: "active.bin".to_string(),
        attempt_id: 0,
    });

    assert_eq!(app.files[0].status, FileStatus::Queued);
    assert_eq!(app.files[0].downloaded, 0);
}

#[test]
fn reset_file_ignores_late_error_until_new_attempt_starts() {
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);
    app.upsert_overlay_file(
        app::FileEntry {
            id: "active.bin".to_string(),
            name: "active.bin".to_string(),
            size: 100,
            downloaded: 80,
            status: FileStatus::Downloading,
        },
        Some("https://mega.nz/file/reset".to_string()),
        true,
    );

    app.handle_ui_action(UiAction::ResetFile("active.bin".to_string()));
    app.handle_download_event(DownloadEvent::FileError {
        id: "active.bin".to_string(),
        error: "boom".to_string(),
        attempt_id: 0,
    });

    assert_eq!(app.files[0].status, FileStatus::Queued);
    assert_eq!(app.files[0].downloaded, 0);
}

#[test]
fn reset_file_accepts_new_terminal_events_after_restart() {
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);
    app.upsert_overlay_file(
        app::FileEntry {
            id: "active.bin".to_string(),
            name: "active.bin".to_string(),
            size: 100,
            downloaded: 80,
            status: FileStatus::Downloading,
        },
        Some("https://mega.nz/file/reset".to_string()),
        true,
    );

    app.handle_ui_action(UiAction::ResetFile("active.bin".to_string()));
    app.handle_download_event(DownloadEvent::FileStart {
        id: "active.bin".to_string(),
        size: 100,
        attempt_id: 1,
    });
    app.handle_download_event(DownloadEvent::FileError {
        id: "active.bin".to_string(),
        error: "boom".to_string(),
        attempt_id: 1,
    });

    assert_eq!(app.files[0].downloaded, 0);
    assert_eq!(app.files[0].status, FileStatus::Error("boom".to_string()));
}

#[test]
fn resume_session_deduplicates_duplicate_file_entries_by_path() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());

    let mut session = session_snapshot(vec![(
        "https://mega.nz/file/root",
        UrlFixtureStatus::Fetched,
    )]);
    push_file(
        &mut session,
        0,
        "duplicate.mkv",
        128,
        FileFixtureStatus::Pending,
    );
    push_file(
        &mut session,
        0,
        "duplicate.mkv",
        128,
        FileFixtureStatus::Completed,
    );
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

struct ScenarioHarness {
    app: App,
    download_tx: mpsc::UnboundedSender<DownloadEvent>,
    download_rx: mpsc::UnboundedReceiver<DownloadEvent>,
    _action_tx: mpsc::UnboundedSender<UiAction>,
    action_rx: mpsc::UnboundedReceiver<UiAction>,
    _state_tx: watch::Sender<String>,
    sys: System,
    pid: Option<sysinfo::Pid>,
    tick_count: u32,
    width: u16,
    height: u16,
}

struct ScenarioSnapshot {
    text: String,
    cursor: Position,
    selected_row: Option<TuiRow>,
    url_input_active: bool,
    popup: Popup,
    status: String,
}

impl ScenarioHarness {
    fn new(width: u16, height: u16) -> Self {
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let app = App::new(9723, event_tx, true);
        let (download_tx, download_rx) = mpsc::unbounded_channel();
        let (action_tx, action_rx) = mpsc::unbounded_channel();
        let (state_tx, _state_rx) = watch::channel(String::new());

        Self {
            app,
            download_tx,
            download_rx,
            _action_tx: action_tx,
            action_rx,
            _state_tx: state_tx,
            sys: System::new(),
            pid: sysinfo::get_current_pid().ok(),
            tick_count: 0,
            width,
            height,
        }
    }

    fn key(&mut self, code: KeyCode) {
        handle_input(&mut self.app, key(code));
    }

    fn paste(&mut self, text: &str) {
        handle_paste(&mut self.app, text);
    }

    fn inject_download(&self, event: DownloadEvent) {
        self.download_tx
            .send(event)
            .expect("download event should send");
    }

    fn tick(&mut self) {
        self.tick_count += 1;
        self.app.handle_terminal_tick(
            &mut self.download_rx,
            &mut self.action_rx,
            self.tick_count,
            &mut self.sys,
            self.pid,
        );
    }

    fn render(&mut self) -> ScenarioSnapshot {
        let backend = TestBackend::new(self.width, self.height);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");
        terminal
            .draw(|frame| draw(frame, &mut self.app))
            .expect("draw should succeed");

        let buffer = terminal.backend().buffer();
        let area = buffer.area;
        let mut text = String::new();
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                let cell = buffer.cell((x, y)).expect("cell should exist");
                text.push_str(cell.symbol());
            }
            text.push('\n');
        }

        ScenarioSnapshot {
            text,
            cursor: terminal
                .get_cursor_position()
                .expect("cursor position should be readable"),
            selected_row: self.app.selected_row(),
            url_input_active: self.app.url_input_active,
            popup: self.app.popup,
            status: self.app.status.clone(),
        }
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

#[test]
fn scenario_add_mode_keeps_cursor_visible_during_live_updates() {
    let mut harness = ScenarioHarness::new(42, 14);

    harness.key(KeyCode::Char('a'));
    harness.paste("https://mega.nz/file/alpha/beta/gamma/tail-marker");
    harness.inject_download(DownloadEvent::StatusMessage(
        "background refresh".to_string(),
    ));
    harness.tick();

    let snapshot = harness.render();

    assert!(snapshot.url_input_active);
    assert_eq!(snapshot.popup, Popup::None);
    assert_eq!(snapshot.status, "background refresh");
    assert!(snapshot.text.contains("tail-marker"));
    assert!(
        !snapshot
            .text
            .contains("https://mega.nz/file/alpha/beta/gamma/tail-marker")
    );
    assert_eq!(snapshot.cursor.y, 2);
    assert!(snapshot.cursor.x > 1);
}

#[test]
fn scenario_selection_falls_back_to_parent_package_after_failed_package_recovers() {
    let mut harness = ScenarioHarness::new(80, 18);

    for (package_id, file_id, name) in [
        ("pkg-a", "a.bin", "Package A"),
        ("pkg-b", "b.bin", "Package B"),
    ] {
        harness.app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id.to_string(),
                source_url: format!("https://mega.nz/folder/{package_id}"),
                display_name: name.to_string(),
                files: vec![ResolvedFile {
                    file_id: file_id.to_string(),
                    path: file_id.to_string(),
                    size: 128,
                }],
                collision: None,
            },
        });
    }

    let _ = harness.render();

    harness.inject_download(DownloadEvent::FileError {
        id: "a.bin".to_string(),
        error: "boom".to_string(),
        attempt_id: 0,
    });
    harness.tick();
    let _ = harness.render();

    harness.key(KeyCode::Down);
    assert_eq!(
        harness.render().selected_row,
        Some(TuiRow::File {
            package_id: "pkg-a".to_string(),
            file_id: "a.bin".to_string(),
        })
    );

    harness.inject_download(DownloadEvent::FileQueued(QueuedFile {
        id: "a.bin".to_string(),
        size: 128,
        count_toward_progress: true,
        origin: crate::tui::event::FileOrigin {
            source_url: "https://mega.nz/folder/pkg-a".to_string(),
            submitted_url: "https://mega.nz/folder/pkg-a".to_string(),
        },
    }));
    harness.tick();

    let snapshot = harness.render();
    assert_eq!(
        snapshot.selected_row,
        Some(TuiRow::Package("pkg-a".to_string()))
    );
    assert!(snapshot.text.contains("Package A"));
    assert!(snapshot.text.contains("Package B"));
}

#[test]
fn scenario_reset_ignores_late_completion_until_restarted_attempt_emits_start() {
    let mut harness = ScenarioHarness::new(80, 18);

    harness.app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: "pkg-a".to_string(),
            source_url: "https://mega.nz/file/reset".to_string(),
            display_name: "Package A".to_string(),
            files: vec![ResolvedFile {
                file_id: "active.bin".to_string(),
                path: "active.bin".to_string(),
                size: 128,
            }],
            collision: None,
        },
    });
    harness.app.apply_core_event(CoreEvent::FileStarted {
        file_id: "active.bin".to_string(),
        size: 128,
    });

    harness
        .app
        .handle_ui_action(UiAction::ResetFile("active.bin".to_string()));
    harness.inject_download(DownloadEvent::FileComplete {
        id: "active.bin".to_string(),
        attempt_id: 0,
    });
    harness.tick();

    let file = harness
        .app
        .files
        .iter()
        .find(|file| file.id == "active.bin")
        .expect("reset file should remain visible");
    assert_eq!(file.status, FileStatus::Queued);
    assert_eq!(file.downloaded, 0);

    harness.inject_download(DownloadEvent::FileStart {
        id: "active.bin".to_string(),
        size: 128,
        attempt_id: 1,
    });
    harness.inject_download(DownloadEvent::FileComplete {
        id: "active.bin".to_string(),
        attempt_id: 0,
    });
    harness.tick();

    let file = harness
        .app
        .files
        .iter()
        .find(|file| file.id == "active.bin")
        .expect("stale completion should not hide the restarted file");
    assert_eq!(file.status, FileStatus::Downloading);
    assert_eq!(file.downloaded, 0);

    harness.inject_download(DownloadEvent::FileComplete {
        id: "active.bin".to_string(),
        attempt_id: 1,
    });
    harness.tick();

    let file = harness
        .app
        .files
        .iter()
        .find(|file| file.id == "active.bin")
        .expect("restarted file should remain visible");
    assert_eq!(file.status, FileStatus::Complete);
    assert_eq!(file.downloaded, 128);
}
