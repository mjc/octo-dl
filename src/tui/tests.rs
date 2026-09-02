use super::*;
use crate::{
    core::{
        CoreEvent, FileLifecycle, PackageSnapshot, PackageState, ResolvedFile, ResolvedPackage,
        SessionRunStatus, SessionSnapshot,
    },
    test_support::{
        CurrentDirGuard, FileFixtureStatus, StateDirectoryGuard, UrlFixtureStatus, package_id,
        push_file, session_snapshot, write_dummy_legacy_resume_sidecar_for_path,
    },
    tui::{
        draw::draw,
        event::{DownloadEvent, DownloadRequest, QueuedFile},
        input::{handle_input, handle_paste},
        visible::TuiRow,
    },
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend, layout::Position};
use sysinfo::System;
use tempfile::tempdir;
use tokio::sync::{mpsc, watch};

use super::app::{FileStatus, Popup, UiAction};

fn resolve_active_test_file(app: &mut App, source_url: &str, file_id: &str, path: String) {
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id(source_url, source_url),
            source_url: source_url.to_string(),
            key: crate::core::PackageKey::new(source_url.to_string()),
            display_name: "Test Package".to_string(),
            files: vec![ResolvedFile {
                file_id: file_id.to_string().into(),
                path,
                size: 100,
            }],
            collision: None,
        },
    });
    app.apply_core_event(CoreEvent::FileStarted {
        file_id: file_id.to_string().into(),
        size: 100,
    });
    app.apply_core_event(CoreEvent::FileProgress {
        file_id: file_id.to_string().into(),
        total_bytes_delta: 80,
        network_bytes_delta: 80,
    });
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
    assert_eq!(app.tracked_urls(), expected_urls.as_slice());
    assert_eq!(
        app.visible_rows(),
        vec![
            TuiRow::File {
                package_id: None,
                file_id: expected_urls[0].clone().into(),
            },
            TuiRow::File {
                package_id: None,
                file_id: expected_urls[1].clone().into(),
            },
        ]
    );

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
    assert!(session_state.packages.is_empty());
    assert!(session_state.urls.iter().all(|entry| entry.error.is_none()));
}

#[test]
fn resume_session_clears_empty_failed_package_errors_and_requeues_urls() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());

    let session = session_snapshot(vec![(
        "https://mega.nz/file/stale-error",
        UrlFixtureStatus::Error("boom".to_string()),
    )]);
    session.save().unwrap();

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);

    app.resume_latest_session();

    assert_eq!(
        app.tracked_urls(),
        ["https://mega.nz/file/stale-error".to_string()].as_slice()
    );
    assert_eq!(
        app.visible_rows(),
        vec![TuiRow::File {
            package_id: None,
            file_id: "https://mega.nz/file/stale-error".to_string().into(),
        }]
    );
    assert!(!app.core_state.packages.contains_key(&package_id(
        "https://mega.nz/file/stale-error",
        "https://mega.nz/file/stale-error"
    )));

    let mut url_rx = app.url_rx.take().expect("url_rx should exist");
    assert_eq!(
        url_rx.try_recv().unwrap(),
        DownloadRequest::SubmitUrl {
            url: "https://mega.nz/file/stale-error".to_string()
        }
    );
    assert!(url_rx.try_recv().is_err());
}

#[test]
fn save_rejects_empty_synthetic_package_placeholders() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());

    let mut session = session_snapshot(vec![(
        "https://mega.nz/file/stale-error",
        UrlFixtureStatus::Pending,
    )]);
    session.packages.push(PackageSnapshot {
        id: package_id("batch-folder", "https://mega.nz/file/stale-error"),
        key: crate::core::PackageKey::new("https://mega.nz/file/stale-error".to_string().clone()),
        display_name: "Batch Folder".to_string(),
        files: Vec::new(),
        error: Some("boom".to_string()),
    });
    assert!(session.save().is_err());
    assert!(SessionSnapshot::latest().is_none());
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
    let _cwd = CurrentDirGuard::set(dir.path());

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
    std::fs::write("completed.mkv", vec![0_u8; 128]).unwrap();
    session.save().unwrap();

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);

    app.resume_latest_session();

    assert_eq!(
        app.tracked_urls(),
        ["https://mega.nz/file/pending".to_string()].as_slice()
    );
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
            file_ids: vec!["pending.mkv".to_string().into()],
            attempt_ids: std::collections::HashMap::new(),
        }
    );
    assert!(url_rx.try_recv().is_err());

    let session_state = app.session.as_ref().expect("session should be present");
    assert!(session_state.packages[0].error.is_none());
    assert!(!session_state.packages[0].files.is_empty());
    assert!(session_state.packages[1].error.is_none());
    assert!(!session_state.packages[1].files.is_empty());
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

    assert_eq!(
        app.tracked_urls(),
        ["https://mega.nz/folder/pending".to_string()].as_slice()
    );

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
fn resume_session_requeues_each_source_url_for_merged_package() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());

    let source_a = "https://mega.nz/folder/source-a";
    let source_b = "https://mega.nz/folder/source-b";
    let mut session = session_snapshot(vec![
        (source_a, UrlFixtureStatus::Fetched),
        (source_b, UrlFixtureStatus::Fetched),
    ]);
    let package_id =
        crate::core::PackageId::for_package_key(&crate::core::PackageKey::new("Merged Folder"));
    session.packages.push(PackageSnapshot {
        id: package_id,
        key: crate::core::PackageKey::new("Merged Folder"),
        display_name: "Merged Folder".to_string(),
        files: Vec::new(),
        error: None,
    });
    session.packages[0].files = vec![
        crate::core::FileSnapshot {
            id: "Merged Folder/a.mkv".to_string().into(),
            package_id,
            source_url: source_a.to_string(),
            path: "Merged Folder/a.mkv".to_string(),
            size: 128,
            lifecycle: FileLifecycle::Queued,
            progress: crate::core::FileProgressState::default(),
            accounting: crate::core::FileAccounting::CurrentRun,
        },
        crate::core::FileSnapshot {
            id: "Merged Folder/b.mkv".to_string().into(),
            package_id,
            source_url: source_b.to_string(),
            path: "Merged Folder/b.mkv".to_string(),
            size: 256,
            lifecycle: FileLifecycle::Queued,
            progress: crate::core::FileProgressState::default(),
            accounting: crate::core::FileAccounting::CurrentRun,
        },
    ];
    session.save().unwrap();

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);

    app.resume_latest_session();

    assert_eq!(
        app.tracked_urls(),
        [source_a.to_string(), source_b.to_string()].as_slice()
    );
    assert_eq!(app.visible_rows(), vec![TuiRow::Package(package_id)]);

    let mut url_rx = app.url_rx.take().expect("url_rx should exist");
    let first = url_rx.try_recv().unwrap();
    let second = url_rx.try_recv().unwrap();
    assert!(url_rx.try_recv().is_err());

    let mut requests = vec![first, second];
    requests.sort_by(|left, right| match (left, right) {
        (
            DownloadRequest::ResumeFileIds {
                source_url: left, ..
            },
            DownloadRequest::ResumeFileIds {
                source_url: right, ..
            },
        ) => left.cmp(right),
        _ => std::cmp::Ordering::Equal,
    });

    assert_eq!(
        requests,
        vec![
            DownloadRequest::ResumeFileIds {
                source_url: source_a.to_string(),
                file_ids: vec!["Merged Folder/a.mkv".to_string().into()],
                attempt_ids: std::collections::HashMap::new(),
            },
            DownloadRequest::ResumeFileIds {
                source_url: source_b.to_string(),
                file_ids: vec!["Merged Folder/b.mkv".to_string().into()],
                attempt_ids: std::collections::HashMap::new(),
            },
        ]
    );
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

    assert_eq!(
        app.tracked_urls(),
        ["https://mega.nz/file/retry".to_string()].as_slice()
    );
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
            id: "completed.mkv".to_string().into(),
            name: "completed.mkv".to_string(),
            size: 128,
            downloaded: 128,
            status: FileStatus::Complete,
        },
        app::FileEntry {
            id: "pending.mkv".to_string().into(),
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
    assert_eq!(session.file_count(), 2);
    assert!(
        session
            .iter_files()
            .any(|file| file.path == "completed.mkv")
    );
    assert!(session.iter_files().any(|file| file.path == "pending.mkv"));
}

#[test]
fn ui_add_urls_enqueues_each_unique_url_once() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);
    let mut url_rx = app.url_rx.take().expect("url_rx should exist");

    app.handle_ui_action(UiAction::AddUrls(vec![
        "https://mega.nz/file/one".to_string(),
        "https://mega.nz/file/one".to_string(),
        "https://mega.nz/file/two".to_string(),
    ]));

    assert_eq!(
        app.tracked_urls(),
        [
            "https://mega.nz/file/one".to_string(),
            "https://mega.nz/file/two".to_string()
        ]
        .as_slice()
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
    app.flush_session_persistence();

    let session = crate::core::SessionSnapshot::latest().expect("session should be saved");
    assert_eq!(session.status, SessionRunStatus::Paused);
    assert!(
        session
            .urls
            .iter()
            .any(|entry| entry.url == "https://mega.nz/file/pending")
    );
}

#[test]
fn resume_session_missing_completed_file_stays_complete_and_is_not_requeued() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let _cwd = CurrentDirGuard::set(dir.path());

    let mut session = session_snapshot(vec![(
        "https://mega.nz/file/completed",
        UrlFixtureStatus::Fetched,
    )]);
    push_file(
        &mut session,
        0,
        "completed.mkv",
        128,
        FileFixtureStatus::Completed,
    );
    session.save().unwrap();

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);

    app.resume_latest_session();

    let completed = app
        .files
        .iter()
        .find(|file| file.id == "completed.mkv")
        .expect("completed file should stay visible");
    assert_eq!(completed.status, FileStatus::Complete);
    assert_eq!(completed.downloaded, 128);
    assert!(app.tracked_urls().is_empty());

    let mut url_rx = app.url_rx.take().expect("url_rx should exist");
    assert!(url_rx.try_recv().is_err());
}

#[test]
fn resume_session_missing_completed_file_preserves_preexisting_startup_accounting() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let _cwd = CurrentDirGuard::set(dir.path());

    let mut session = session_snapshot(vec![(
        "https://mega.nz/file/completed",
        UrlFixtureStatus::Fetched,
    )]);
    push_file(
        &mut session,
        0,
        "completed.mkv",
        128,
        FileFixtureStatus::Completed,
    );
    session.save().unwrap();

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);

    app.resume_latest_session();

    assert_eq!(app.files_completed, 1);
    assert_eq!(app.total_downloaded, 128);
}

#[test]
fn resume_session_partial_without_sidecar_surfaces_existing_bytes_on_startup() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let _cwd = CurrentDirGuard::set(dir.path());
    std::fs::write("partial.bin.part", vec![0_u8; 80]).unwrap();

    let mut session = session_snapshot(vec![(
        "https://mega.nz/file/partial",
        UrlFixtureStatus::Fetched,
    )]);
    push_file(
        &mut session,
        0,
        "partial.bin",
        128,
        FileFixtureStatus::Pending,
    );
    session.save().unwrap();

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);

    app.resume_latest_session();

    let partial = app
        .files
        .iter()
        .find(|file| file.id == "partial.bin")
        .expect("partial file should stay visible");
    assert_eq!(partial.status, FileStatus::Queued);
    assert_eq!(partial.downloaded, 80);
    assert_eq!(app.total_downloaded, 80);
}

#[test]
fn ui_retry_file_recomputes_totals() {
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);
    let (url_tx, mut url_rx) = tokio::sync::mpsc::unbounded_channel();
    app.url_tx = url_tx;
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id("https://mega.nz/file/error", "https://mega.nz/file/error"),
            source_url: "https://mega.nz/file/error".to_string(),
            key: crate::core::PackageKey::new("https://mega.nz/file/error".to_string()),
            display_name: "Error".to_string(),
            files: vec![ResolvedFile {
                file_id: "error.bin".to_string().into(),
                path: "error.bin".to_string(),
                size: 100,
            }],
            collision: None,
        },
    });
    app.apply_core_event(CoreEvent::FileStarted {
        file_id: "error.bin".to_string().into(),
        size: 100,
    });
    app.apply_core_event(CoreEvent::FileProgress {
        file_id: "error.bin".to_string().into(),
        total_bytes_delta: 20,
        network_bytes_delta: 20,
    });
    app.apply_core_event(CoreEvent::FileFailed {
        file_id: "error.bin".to_string().into(),
        message: "boom".to_string(),
    });
    app.expanded_packages.insert(package_id(
        "https://mega.nz/file/error",
        "https://mega.nz/file/error",
    ));
    app.sync_visible_files();
    app.recompute_totals();

    assert_eq!(app.files_total, 1);
    assert_eq!(app.total_downloaded, 20);

    app.handle_ui_action(UiAction::RetryFile("error.bin".to_string().into()));

    let file = app
        .files
        .iter()
        .find(|file| file.id == "error.bin")
        .expect("retried file should remain visible");
    assert_eq!(file.status, FileStatus::Queued);
    assert_eq!(file.downloaded, 0);
    assert_eq!(app.files_total, 1);
    assert_eq!(app.total_downloaded, 0);
    assert_eq!(
        url_rx.try_recv().unwrap(),
        DownloadRequest::ResumeFileIds {
            source_url: "https://mega.nz/file/error".to_string(),
            file_ids: vec!["error.bin".to_string().into()],
            attempt_ids: std::collections::HashMap::from([("error.bin".to_string().into(), 1)]),
        }
    );
}

#[test]
fn ui_retry_empty_failed_package_requeues_source_url() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);
    let (url_tx, mut url_rx) = tokio::sync::mpsc::unbounded_channel();
    app.url_tx = url_tx;

    let source_url = "https://mega.nz/folder/retry".to_string();
    let package_id = package_id("batch-folder", &source_url);
    app.session = Some(session_snapshot(vec![(
        source_url.as_str(),
        UrlFixtureStatus::Pending,
    )]));
    let _ = app.mutate_session_and_save(|session| {
        crate::tui::session::SessionAdapter::mark_url_error(session, &source_url, "boom")
    });
    app.session
        .as_mut()
        .expect("session should be installed")
        .packages
        .push(PackageSnapshot {
            id: package_id.clone(),
            key: crate::core::PackageKey::new(source_url.clone().clone()),
            display_name: "Retry Folder".to_string(),
            files: Vec::new(),
            error: Some("boom".to_string()),
        });
    app.core_state.url_order.push(source_url.clone());
    app.core_state.packages.insert(
        package_id.clone(),
        PackageState {
            id: package_id.clone(),
            key: crate::core::PackageKey::new(source_url.clone().clone()),
            display_name: "Retry Folder".to_string(),
            progress: crate::core::model::PackageProgressState::default(),
            error: Some("boom".to_string()),
        },
    );
    app.sync_visible_files();

    assert!(app.visible_rows().is_empty());

    app.handle_ui_action(UiAction::RetryPackage(package_id));

    assert!(!app.core_state.packages.contains_key(&package_id));
    assert_eq!(
        url_rx.try_recv().unwrap(),
        DownloadRequest::SubmitUrl {
            url: source_url.clone()
        }
    );
    let session = app.session.as_ref().expect("session should remain");
    let tracked_url = session
        .urls
        .iter()
        .find(|entry| entry.url == source_url)
        .expect("source URL should remain tracked");
    assert!(tracked_url.error.is_none());
    assert!(
        session
            .packages
            .iter()
            .all(|package| package.id != package_id)
    );
}

#[test]
fn ui_delete_file_removes_completed_artifacts_from_disk() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("completed.bin");
    let part_path = dir.path().join("completed.bin.part");
    std::fs::write(&file_path, b"done").unwrap();
    std::fs::write(&part_path, b"partial").unwrap();
    let sidecar_path = write_dummy_legacy_resume_sidecar_for_path(&file_path);

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);
    app.upsert_overlay_file(
        app::FileEntry {
            id: file_path.to_string_lossy().into_owned().into(),
            name: file_path.to_string_lossy().into_owned(),
            size: 4,
            downloaded: 4,
            status: FileStatus::Complete,
        },
        Some("https://mega.nz/file/completed".to_string()),
    );

    app.handle_ui_action(UiAction::DeleteFile(
        file_path.to_string_lossy().into_owned().into(),
    ));

    assert!(app.files.is_empty());
    assert!(!file_path.exists());
    assert!(!part_path.exists());
    assert!(!sidecar_path.exists());
}

#[test]
fn ui_delete_core_backed_completed_file_removes_filesystem_artifacts() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("core-backed.bin");
    let part_path = dir.path().join("core-backed.bin.part");
    std::fs::write(&file_path, b"done").unwrap();
    std::fs::write(&part_path, b"partial").unwrap();
    let sidecar_path = write_dummy_legacy_resume_sidecar_for_path(&file_path);

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);
    let file_id = file_path.to_string_lossy().into_owned();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id(
                "https://mega.nz/file/core-delete",
                "https://mega.nz/file/core-delete",
            ),
            source_url: "https://mega.nz/file/core-delete".to_string(),
            key: crate::core::PackageKey::new(
                "https://mega.nz/file/core-delete".to_string().clone(),
            ),
            display_name: "Core Delete".to_string(),
            files: vec![ResolvedFile {
                file_id: file_id.clone().into(),
                path: file_id.clone(),
                size: 4,
            }],
            collision: None,
        },
    });
    app.apply_core_event(CoreEvent::FileCompleted {
        file_id: file_id.clone().into(),
    });

    app.handle_ui_action(UiAction::DeleteFile(file_id.into()));

    assert!(!file_path.exists());
    assert!(!part_path.exists());
    assert!(!sidecar_path.exists());
}

#[test]
fn ui_delete_completed_package_removes_filesystem_artifacts() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("pkg-complete.bin");
    let part_path = dir.path().join("pkg-complete.bin.part");
    std::fs::write(&file_path, b"done").unwrap();
    std::fs::write(&part_path, b"partial").unwrap();
    let sidecar_path = write_dummy_legacy_resume_sidecar_for_path(&file_path);

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);
    let file_id = file_path.to_string_lossy().into_owned();
    let package_id = package_id(
        "https://mega.nz/file/core-delete-package",
        "https://mega.nz/file/core-delete-package",
    );
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id,
            source_url: "https://mega.nz/file/core-delete-package".to_string(),
            key: crate::core::PackageKey::new(
                "https://mega.nz/file/core-delete-package"
                    .to_string()
                    .clone(),
            ),
            display_name: "Core Delete Package".to_string(),
            files: vec![ResolvedFile {
                file_id: file_id.clone().into(),
                path: file_id.clone(),
                size: 4,
            }],
            collision: None,
        },
    });
    app.apply_core_event(CoreEvent::FileCompleted {
        file_id: file_id.clone().into(),
    });

    app.handle_ui_action(UiAction::DeletePackage(package_id));

    assert!(!file_path.exists());
    assert!(!part_path.exists());
    assert!(!sidecar_path.exists());
    assert!(app.files.is_empty());
}

#[test]
fn deleted_file_completion_event_is_ignored_and_leaves_artifacts() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("late-complete.bin");
    let part_path = dir.path().join("late-complete.bin.part");
    std::fs::write(&file_path, b"done").unwrap();
    std::fs::write(&part_path, b"partial").unwrap();
    let sidecar_path = write_dummy_legacy_resume_sidecar_for_path(&file_path);

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);
    let file_id = file_path.to_string_lossy().into_owned();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id(
                "https://mega.nz/file/late-complete",
                "https://mega.nz/file/late-complete",
            ),
            source_url: "https://mega.nz/file/late-complete".to_string(),
            key: crate::core::PackageKey::new(
                "https://mega.nz/file/late-complete".to_string().clone(),
            ),
            display_name: "Late Complete".to_string(),
            files: vec![ResolvedFile {
                file_id: file_id.clone().into(),
                path: file_id.clone(),
                size: 4,
            }],
            collision: None,
        },
    });
    app.apply_core_event(CoreEvent::FileStarted {
        file_id: file_id.clone().into(),
        size: 4,
    });

    app.handle_ui_action(UiAction::DeleteFile(file_id.clone().into()));

    std::fs::write(&file_path, b"done").unwrap();
    std::fs::write(&part_path, b"partial").unwrap();
    let _ = write_dummy_legacy_resume_sidecar_for_path(&file_path);
    app.handle_download_event(DownloadEvent::FileComplete {
        id: file_id.into(),
        attempt_id: 0,
    });

    assert!(app.files.is_empty());
    assert!(app.core_state.files.is_empty());
    assert!(file_path.exists());
    assert!(part_path.exists());
    assert!(sidecar_path.exists());
}

#[test]
fn deleted_file_stays_deleted_after_cancel_then_completion_events() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("late-cancel-complete.bin");
    let part_path = dir.path().join("late-cancel-complete.bin.part");
    std::fs::write(&file_path, b"done").unwrap();
    std::fs::write(&part_path, b"partial").unwrap();
    let sidecar_path = write_dummy_legacy_resume_sidecar_for_path(&file_path);

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);
    let file_id = file_path.to_string_lossy().into_owned();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id(
                "https://mega.nz/file/late-cancel-complete",
                "https://mega.nz/file/late-cancel-complete",
            ),
            source_url: "https://mega.nz/file/late-cancel-complete".to_string(),
            key: crate::core::PackageKey::new(
                "https://mega.nz/file/late-cancel-complete"
                    .to_string()
                    .clone(),
            ),
            display_name: "Late Cancel Complete".to_string(),
            files: vec![ResolvedFile {
                file_id: file_id.clone().into(),
                path: file_id.clone(),
                size: 4,
            }],
            collision: None,
        },
    });
    app.apply_core_event(CoreEvent::FileStarted {
        file_id: file_id.clone().into(),
        size: 4,
    });

    app.handle_ui_action(UiAction::DeleteFile(file_id.clone().into()));
    app.handle_download_event(DownloadEvent::FileCancelled {
        id: file_id.clone().into(),
        attempt_id: 0,
    });

    std::fs::write(&file_path, b"done").unwrap();
    std::fs::write(&part_path, b"partial").unwrap();
    let _ = write_dummy_legacy_resume_sidecar_for_path(&file_path);
    app.handle_download_event(DownloadEvent::FileComplete {
        id: file_id.clone().into(),
        attempt_id: 0,
    });

    assert!(app.files.is_empty());
    assert!(app.core_state.files.is_empty());
    assert!(file_path.exists());
    assert!(part_path.exists());
    assert!(sidecar_path.exists());
}

#[test]
fn deleted_file_error_event_is_ignored_and_leaves_artifacts() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("late-error.bin");
    let part_path = dir.path().join("late-error.bin.part");
    std::fs::write(&file_path, b"done").unwrap();
    std::fs::write(&part_path, b"partial").unwrap();
    let sidecar_path = write_dummy_legacy_resume_sidecar_for_path(&file_path);

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);
    let file_id = file_path.to_string_lossy().into_owned();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id(
                "https://mega.nz/file/late-error",
                "https://mega.nz/file/late-error",
            ),
            source_url: "https://mega.nz/file/late-error".to_string(),
            key: crate::core::PackageKey::new(
                "https://mega.nz/file/late-error".to_string().clone(),
            ),
            display_name: "Late Error".to_string(),
            files: vec![ResolvedFile {
                file_id: file_id.clone().into(),
                path: file_id.clone(),
                size: 4,
            }],
            collision: None,
        },
    });
    app.apply_core_event(CoreEvent::FileStarted {
        file_id: file_id.clone().into(),
        size: 4,
    });

    app.handle_ui_action(UiAction::DeleteFile(file_id.clone().into()));

    std::fs::write(&file_path, b"done").unwrap();
    std::fs::write(&part_path, b"partial").unwrap();
    let _ = write_dummy_legacy_resume_sidecar_for_path(&file_path);
    app.handle_download_event(DownloadEvent::FileError {
        id: file_id.into(),
        error: "boom".to_string(),
        attempt_id: 0,
    });

    assert!(app.files.is_empty());
    assert!(app.core_state.files.is_empty());
    assert!(file_path.exists());
    assert!(part_path.exists());
    assert!(sidecar_path.exists());
}

#[test]
fn ui_reset_file_resets_progress_and_requeues_url() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("active.bin");
    let part_path = dir.path().join("active.bin.part");
    std::fs::write(&file_path, b"complete").unwrap();
    std::fs::write(&part_path, b"partial").unwrap();
    let sidecar_path = write_dummy_legacy_resume_sidecar_for_path(&file_path);

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);
    let (url_tx, mut url_rx) = tokio::sync::mpsc::unbounded_channel();
    app.url_tx = url_tx;
    resolve_active_test_file(
        &mut app,
        "https://mega.nz/file/reset",
        "active.bin",
        file_path.to_string_lossy().into_owned(),
    );

    app.handle_ui_action(UiAction::ResetFile("active.bin".to_string().into()));

    assert_eq!(app.files[0].status, FileStatus::Queued);
    assert_eq!(app.files[0].downloaded, 0);
    assert_eq!(
        url_rx.try_recv().unwrap(),
        DownloadRequest::ResumeFileIds {
            source_url: "https://mega.nz/file/reset".to_string(),
            file_ids: vec!["active.bin".to_string().into()],
            attempt_ids: std::collections::HashMap::from([("active.bin".to_string().into(), 1)]),
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
    resolve_active_test_file(
        &mut app,
        "https://mega.nz/file/reset",
        "active.bin",
        "active.bin".to_string(),
    );

    app.handle_ui_action(UiAction::ResetFile("active.bin".to_string().into()));
    app.handle_download_event(DownloadEvent::FileComplete {
        id: "active.bin".to_string().into(),
        attempt_id: 0,
    });

    assert_eq!(app.files[0].status, FileStatus::Queued);
    assert_eq!(app.files[0].downloaded, 0);
}

#[test]
fn reset_file_ignores_late_error_until_new_attempt_starts() {
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(0, event_tx, true);
    resolve_active_test_file(
        &mut app,
        "https://mega.nz/file/reset",
        "active.bin",
        "active.bin".to_string(),
    );

    app.handle_ui_action(UiAction::ResetFile("active.bin".to_string().into()));
    app.handle_download_event(DownloadEvent::FileError {
        id: "active.bin".to_string().into(),
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
    resolve_active_test_file(
        &mut app,
        "https://mega.nz/file/reset",
        "active.bin",
        "active.bin".to_string(),
    );

    app.handle_ui_action(UiAction::ResetFile("active.bin".to_string().into()));
    app.handle_download_event(DownloadEvent::FileStart {
        id: "active.bin".to_string().into(),
        size: 100,
        attempt_id: 1,
    });
    app.handle_download_event(DownloadEvent::FileError {
        id: "active.bin".to_string().into(),
        error: "boom".to_string(),
        attempt_id: 1,
    });

    assert_eq!(app.files[0].downloaded, 0);
    assert_eq!(app.files[0].status, FileStatus::Error("boom".to_string()));
}

#[test]
fn save_rejects_duplicate_file_entries_by_path() {
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
    assert!(session.save().is_err());
}

struct ScenarioHarness {
    app: App,
    download_tx: mpsc::UnboundedSender<DownloadEvent>,
    download_rx: mpsc::UnboundedReceiver<DownloadEvent>,
    _action_tx: mpsc::UnboundedSender<UiAction>,
    action_rx: mpsc::UnboundedReceiver<UiAction>,
    _state_tx: watch::Sender<bytes::Bytes>,
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
        let (state_tx, _state_rx) = watch::channel(bytes::Bytes::new());

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
            false,
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

    for (raw_package_id, file_id, name) in [
        ("pkg-a", "a.bin", "Package A"),
        ("pkg-b", "b.bin", "Package B"),
    ] {
        harness.app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id(
                    raw_package_id,
                    &format!("https://mega.nz/folder/{raw_package_id}"),
                ),
                source_url: format!("https://mega.nz/folder/{raw_package_id}"),
                key: crate::core::PackageKey::new(
                    format!("https://mega.nz/folder/{raw_package_id}").clone(),
                ),
                display_name: name.to_string(),
                files: vec![ResolvedFile {
                    file_id: file_id.to_string().into(),
                    path: file_id.to_string(),
                    size: 128,
                }],
                collision: None,
            },
        });
    }

    let _ = harness.render();

    harness.inject_download(DownloadEvent::FileError {
        id: "a.bin".to_string().into(),
        error: "boom".to_string(),
        attempt_id: 0,
    });
    harness.tick();
    let _ = harness.render();

    harness.key(KeyCode::Down);
    assert_eq!(
        harness.render().selected_row,
        Some(TuiRow::File {
            package_id: Some(package_id("pkg-a", "https://mega.nz/folder/pkg-a")),
            file_id: "a.bin".to_string().into(),
        })
    );

    harness.inject_download(DownloadEvent::FileQueued(QueuedFile {
        id: "a.bin".to_string().into(),
        size: 128,
        accounting: crate::core::FileAccounting::CurrentRun,
        origin: crate::tui::event::FileOrigin {
            package_id: None,
            package_display_name: None,
            source_url: "https://mega.nz/folder/pkg-a".to_string(),
            submitted_url: "https://mega.nz/folder/pkg-a".to_string(),
        },
    }));
    harness.tick();

    let snapshot = harness.render();
    assert_eq!(
        snapshot.selected_row,
        Some(TuiRow::Package(package_id(
            "pkg-a",
            "https://mega.nz/folder/pkg-a"
        )))
    );
    assert!(snapshot.text.contains("Package A"));
    assert!(snapshot.text.contains("Package B"));
}

#[test]
fn scenario_reset_ignores_late_completion_until_restarted_attempt_emits_start() {
    let mut harness = ScenarioHarness::new(80, 18);

    harness.app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id("pkg-a", "https://mega.nz/file/reset"),
            source_url: "https://mega.nz/file/reset".to_string(),
            key: crate::core::PackageKey::new("https://mega.nz/file/reset".to_string().clone()),
            display_name: "Package A".to_string(),
            files: vec![ResolvedFile {
                file_id: "active.bin".to_string().into(),
                path: "active.bin".to_string(),
                size: 128,
            }],
            collision: None,
        },
    });
    harness.app.apply_core_event(CoreEvent::FileStarted {
        file_id: "active.bin".to_string().into(),
        size: 128,
    });

    harness
        .app
        .handle_ui_action(UiAction::ResetFile("active.bin".to_string().into()));
    harness.inject_download(DownloadEvent::FileComplete {
        id: "active.bin".to_string().into(),
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
        id: "active.bin".to_string().into(),
        size: 128,
        attempt_id: 1,
    });
    harness.inject_download(DownloadEvent::FileComplete {
        id: "active.bin".to_string().into(),
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
        id: "active.bin".to_string().into(),
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
