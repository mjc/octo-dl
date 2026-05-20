use super::super::app::{App, FileEntry, FileStatus, UiAction};
use super::super::event::{DownloadEvent, FileOrigin, QueuedFile};
use super::*;
use crate::core::{CoreEvent, FileLifecycle, ProgressDelta, SessionRunStatus};
use crate::test_support::{
    FileFixtureStatus, StateDirectoryGuard, UrlFixtureStatus, push_file, session_snapshot,
};
use tempfile::tempdir;
use tokio::sync::mpsc;

fn test_app() -> App {
    let (tx, _rx) = mpsc::unbounded_channel();
    App::new(9723, tx, true)
}

fn session_with_file(path: &str, size: u64) -> crate::SessionSnapshot {
    let mut session = session_snapshot(vec![(
        "https://mega.nz/folder/root",
        UrlFixtureStatus::Fetched,
    )]);
    push_file(&mut session, 0, path, size, FileFixtureStatus::Pending);
    session
}

#[test]
fn describe_panic_handles_known_and_unknown_payloads() {
    let static_msg: &(dyn std::any::Any + Send) = &"static boom";
    let string_msg: &(dyn std::any::Any + Send) = &String::from("owned boom");
    let unknown_msg: &(dyn std::any::Any + Send) = &123_u32;

    assert_eq!(describe_panic(static_msg), "static boom");
    assert_eq!(describe_panic(string_msg), "owned boom");
    assert_eq!(describe_panic(unknown_msg), "unknown panic payload");
}

#[test]
fn handle_file_complete_marks_session_file_complete() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let mut app = test_app();
    app.files.push(FileEntry {
        id: "first.bin".to_string().into(),
        name: "first.bin".to_string(),
        size: 64,
        downloaded: 16,
        status: FileStatus::Downloading,
    });
    app.recompute_totals();
    let session = session_with_file("first.bin", 64);
    let session_path = session.state_path();
    app.session = Some(session);

    app.mark_visible_file_complete(&"first.bin".into(), "renamed.bin");

    let file = app
        .files
        .iter()
        .find(|file| file.id == "first.bin")
        .expect("file should remain visible");
    assert_eq!(file.name, "renamed.bin");
    assert_eq!(file.status, FileStatus::Complete);
    assert_eq!(file.downloaded, 64);
    assert_eq!(app.status, "All downloads complete");
    assert!(session_path.exists());

    let session = app.session.as_ref().expect("session should remain");
    assert_eq!(session.file_count(), 1);
    assert_eq!(
        session.find_file("first.bin").unwrap().lifecycle,
        FileLifecycle::Complete
    );
    assert_eq!(session.status, SessionRunStatus::Completed);
}

#[test]
fn file_queued_clears_stale_error_state() {
    let mut app = test_app();
    app.apply_core_event(CoreEvent::UrlSubmitted {
        url: "https://mega.nz/folder/root".to_string(),
    });
    app.files.push(FileEntry {
        id: "file-id".to_string().into(),
        name: "old-name.mkv".to_string(),
        size: 64,
        downloaded: 17,
        status: FileStatus::Error("stale error".to_string()),
    });

    app.handle_download_event(DownloadEvent::FileQueued(QueuedFile {
        id: "file-id".to_string().into(),
        size: 128,
        count_toward_progress: true,
        origin: FileOrigin {
            package_id: None,
            package_display_name: None,
            source_url: "https://mega.nz/file/new".to_string(),
            submitted_url: "https://mega.nz/folder/root".to_string(),
        },
    }));

    let file = app.files.iter().find(|file| file.id == "file-id").unwrap();
    assert_eq!(file.name, "file-id");
    assert_eq!(file.size, 128);
    assert_eq!(
        app.visible_file_context(&"file-id".into())
            .and_then(|context| context.source_url),
        Some("https://mega.nz/file/new".to_string())
    );
    assert_eq!(file.status, FileStatus::Queued);
    assert_eq!(file.downloaded, 0);
    assert_eq!(app.file_speed(&"file-id".into()), 0);
}

#[test]
fn file_queued_bootstraps_and_saves_session() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let mut app = test_app();
    app.submit_url("https://mega.nz/file/new".to_string());

    app.handle_download_event(DownloadEvent::FileQueued(QueuedFile {
        id: "file-id".to_string().into(),
        size: 128,
        count_toward_progress: true,
        origin: FileOrigin {
            package_id: None,
            package_display_name: None,
            source_url: "https://mega.nz/file/new".to_string(),
            submitted_url: "https://mega.nz/file/new".to_string(),
        },
    }));

    let saved = crate::core::SessionSnapshot::latest().expect("session should be saved");
    assert_eq!(saved.urls.len(), 1);
    assert_eq!(saved.urls[0].url, "https://mega.nz/file/new");
    assert_eq!(saved.file_count(), 1);
    assert!(saved.find_file("file-id").is_some());
}

#[test]
fn file_queued_does_not_clear_deleted_file_guard() {
    let mut app = test_app();
    let file_id = crate::core::FileId::from("episode.mkv");
    app.deleted_files.insert(file_id.clone());

    app.handle_download_event(DownloadEvent::FileQueued(QueuedFile {
        id: file_id.clone(),
        size: 128,
        count_toward_progress: true,
        origin: FileOrigin {
            package_id: None,
            package_display_name: None,
            source_url: "https://mega.nz/file/root".to_string(),
            submitted_url: "https://mega.nz/file/root".to_string(),
        },
    }));

    assert!(app.deleted_files.contains(&file_id));
    assert!(
        app.files.is_empty(),
        "visible files after stale queue: {:?}",
        app.files
            .iter()
            .map(|file| file.id.to_string())
            .collect::<Vec<_>>()
    );
    assert!(app.core_state.files.is_empty());
}

#[test]
fn file_queued_after_package_delete_is_ignored_when_source_is_untracked() {
    let mut app = test_app();
    let source_url = "https://mega.nz/folder/delete-me".to_string();
    let package_id = crate::test_support::package_id("delete-me", "Delete Me");

    app.submit_url(source_url.clone());
    app.handle_download_event(DownloadEvent::FileQueued(QueuedFile {
        id: "known.bin".to_string().into(),
        size: 128,
        count_toward_progress: true,
        origin: FileOrigin {
            package_id: Some(package_id),
            package_display_name: Some("Delete Me".to_string()),
            source_url: source_url.clone(),
            submitted_url: source_url.clone(),
        },
    }));
    assert_eq!(app.core_state.files.len(), 1);

    app.handle_ui_action(UiAction::DeletePackage(package_id));
    assert!(app.core_state.files.is_empty());
    assert!(app.core_state.packages.is_empty());
    assert!(!app.urls.iter().any(|url| url == &source_url));

    app.handle_download_event(DownloadEvent::FileQueued(QueuedFile {
        id: "late.bin".to_string().into(),
        size: 256,
        count_toward_progress: true,
        origin: FileOrigin {
            package_id: Some(package_id),
            package_display_name: Some("Delete Me".to_string()),
            source_url,
            submitted_url: "https://mega.nz/folder/delete-me".to_string(),
        },
    }));

    assert!(
        app.files.is_empty(),
        "visible files after stale queue: {:?}",
        app.files
            .iter()
            .map(|file| file.id.to_string())
            .collect::<Vec<_>>()
    );
    assert!(app.core_state.files.is_empty());
    assert!(app.core_state.packages.is_empty());
}

#[test]
fn url_placeholder_lives_in_overlay_until_resolved() {
    let mut app = test_app();
    let url = "https://mega.nz/folder/root".to_string();

    app.handle_download_event(DownloadEvent::UrlQueued { url: url.clone() });
    assert!(app.overlay_files.contains_key(url.as_str()));
    assert!(app.files.iter().any(|file| file.id == url));

    app.handle_download_event(DownloadEvent::UrlResolved { url: url.clone() });
    assert!(!app.overlay_files.contains_key(url.as_str()));
    assert!(!app.files.iter().any(|file| file.id == url));
}

#[test]
fn url_level_error_replaces_placeholder_in_overlay() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let mut app = test_app();
    let url = "https://mega.nz/folder/root".to_string();
    let session = session_snapshot(vec![(url.as_str(), UrlFixtureStatus::Pending)]);
    app.session = Some(session);

    app.handle_download_event(DownloadEvent::UrlQueued { url: url.clone() });
    app.handle_download_event(DownloadEvent::ScopeError {
        scope: url.clone(),
        error: "bad folder".to_string(),
    });

    let overlay = app
        .overlay_files
        .get(url.as_str())
        .expect("url-level errors should remain in overlay");
    assert!(matches!(overlay.file.status, FileStatus::Error(ref msg) if msg == "bad folder"));
    let session = app.session.as_ref().expect("session should remain");
    assert_eq!(session.urls[0].url, url);
    assert_eq!(session.urls[0].error.as_deref(), Some("bad folder"));
}

#[test]
fn handle_file_complete_is_idempotent_for_visible_complete_rows() {
    let mut app = test_app();
    app.upsert_overlay_file(
        FileEntry {
            id: "file-id".to_string().into(),
            name: "file.mkv".to_string(),
            size: 128,
            downloaded: 128,
            status: FileStatus::Complete,
        },
        None,
        true,
    );
    app.recompute_totals();
    assert_eq!(app.files_completed, 1);

    app.mark_visible_file_complete(&"file-id".into(), "file.mkv");

    assert_eq!(app.files_completed, 1);
    let file = app.files.iter().find(|file| file.id == "file-id").unwrap();
    assert_eq!(file.status, FileStatus::Complete);
    assert_eq!(file.downloaded, 128);
}

#[test]
fn completed_file_cannot_be_duplicated_by_startup_queue_events() {
    let mut app = test_app();
    app.apply_core_event(CoreEvent::UrlSubmitted {
        url: "https://mega.nz/file/root".to_string(),
    });
    app.upsert_overlay_file(
        FileEntry {
            id: "episode.mkv".to_string().into(),
            name: "episode.mkv".to_string(),
            size: 128,
            downloaded: 128,
            status: FileStatus::Complete,
        },
        Some("https://mega.nz/file/root".to_string()),
        false,
    );
    app.recompute_totals();

    app.handle_download_event(DownloadEvent::FileQueued(QueuedFile {
        id: "episode.mkv".to_string().into(),
        size: 128,
        count_toward_progress: false,
        origin: FileOrigin {
            package_id: None,
            package_display_name: None,
            source_url: "https://mega.nz/file/root".to_string(),
            submitted_url: "https://mega.nz/file/root".to_string(),
        },
    }));
    app.handle_download_event(DownloadEvent::FileComplete {
        id: "episode.mkv".to_string().into(),
        attempt_id: 0,
    });

    assert_eq!(app.files.len(), 1);
    let file = app
        .files
        .iter()
        .find(|file| file.id == "episode.mkv")
        .unwrap();
    assert_eq!(file.status, FileStatus::Complete);
    assert_eq!(file.downloaded, 128);
    assert_eq!(app.files_completed, 0);
    assert_eq!(app.files_total, 1);
}

#[test]
fn successful_submitted_urls_deduplicates_only_fetched_submissions() {
    let resolved = vec![
        FetchedNodeSet {
            resolved: ResolvedUrl {
                source_url: "https://mega.nz/file/one".to_string(),
                submitted_url: "bundle.dlc".to_string(),
                package_id: None,
                package_display_name: None,
            },
            nodes: None,
            requested_files: RequestedFiles::All,
            requested_attempt_ids: HashMap::new(),
            emit_url_resolved: true,
        },
        FetchedNodeSet {
            resolved: ResolvedUrl {
                source_url: "https://mega.nz/file/two".to_string(),
                submitted_url: "bundle.dlc".to_string(),
                package_id: None,
                package_display_name: None,
            },
            nodes: None,
            requested_files: RequestedFiles::All,
            requested_attempt_ids: HashMap::new(),
            emit_url_resolved: true,
        },
        FetchedNodeSet {
            resolved: ResolvedUrl {
                source_url: "https://mega.nz/file/three".to_string(),
                submitted_url: "https://mega.nz/folder/direct".to_string(),
                package_id: None,
                package_display_name: None,
            },
            nodes: None,
            requested_files: RequestedFiles::All,
            requested_attempt_ids: HashMap::new(),
            emit_url_resolved: true,
        },
    ];

    let urls = successful_submitted_urls(resolved.iter());

    assert_eq!(
        urls,
        vec![
            "bundle.dlc".to_string(),
            "https://mega.nz/folder/direct".to_string()
        ]
    );
}

#[test]
fn queued_events_keep_distinct_source_urls_in_distinct_packages() {
    let left = ResolvedUrl {
        source_url: "https://mega.nz/folder/one".to_string(),
        submitted_url: "bundle.dlc".to_string(),
        package_id: None,
        package_display_name: None,
    };
    let right = ResolvedUrl {
        source_url: "https://mega.nz/folder/two".to_string(),
        submitted_url: "bundle.dlc".to_string(),
        package_id: None,
        package_display_name: None,
    };

    let left_origin = left.file_origin();
    let right_origin = right.file_origin();

    assert_eq!(left_origin.package_id, None);
    assert_eq!(right_origin.package_id, None);
    assert_ne!(left_origin.source_url, right_origin.source_url);
    assert_eq!(left_origin.submitted_url, "bundle.dlc");
    assert_eq!(right_origin.submitted_url, "bundle.dlc");
}

#[test]
fn remote_files_match_prefers_sparse_checksum_then_size_and_date() {
    let left = BatchItemSnapshot {
        size: 100,
        modified_at: Some(123),
        sparse_checksum: Some([7; 16]),
    };
    let same_checksum_different_date = BatchItemSnapshot {
        modified_at: Some(456),
        ..left.clone()
    };
    let same_size_and_date_without_checksum = BatchItemSnapshot {
        sparse_checksum: None,
        ..left.clone()
    };
    let different_size = BatchItemSnapshot {
        size: 90,
        sparse_checksum: None,
        ..left.clone()
    };

    assert!(remote_files_match(&left, &same_checksum_different_date));
    assert!(remote_files_match(
        &BatchItemSnapshot {
            sparse_checksum: None,
            ..left.clone()
        },
        &same_size_and_date_without_checksum
    ));
    assert!(!remote_files_match(&left, &different_size));
}

#[test]
fn duplicate_path_renames_file_inside_folder_preserving_extension() {
    assert_eq!(duplicate_path("folder/file.mkv", 2), "folder/file (2).mkv");
    assert_eq!(duplicate_path("folder/file", 3), "folder/file (3)");
}

#[test]
fn resolved_url_direct_uses_same_source_and_submission() {
    let resolved = ResolvedUrl::direct("https://mega.nz/file/test");

    assert_eq!(resolved.source_url, "https://mega.nz/file/test");
    assert_eq!(resolved.submitted_url, "https://mega.nz/file/test");
}

#[test]
fn expand_dlc_path_leaves_non_filesystem_inputs_unchanged() {
    assert_eq!(
        expand_dlc_path("bundle.dlc").unwrap(),
        "bundle.dlc".to_string()
    );
    assert_eq!(
        expand_dlc_path("/tmp/archive.dlc").unwrap(),
        "/tmp/archive.dlc".to_string()
    );
}

#[test]
fn progress_deltas_do_not_exceed_file_size() {
    let mut app = test_app();
    let file_size: u64 = 1_000_000;

    app.handle_download_event(DownloadEvent::FileStart {
        id: "test.bin".to_string().into(),
        size: file_size,
        attempt_id: 0,
    });

    let deltas = [100_000u64, 250_000, 350_000, 200_000, 100_000];
    for d in deltas {
        app.handle_download_event(DownloadEvent::Progress {
            id: "test.bin".into(),
            delta: ProgressDelta {
                total_bytes_delta: d,
                network_bytes_delta: d,
            },
            attempt_id: 0,
        });
    }

    let file = app.files.iter().find(|f| f.id == "test.bin").unwrap();
    assert_eq!(
        file.downloaded, file_size,
        "downloading rows may reach full byte progress before completion"
    );
    assert!(
        file.downloaded <= file.size,
        "downloaded ({}) must not exceed size ({})",
        file.downloaded,
        file.size,
    );
    assert_eq!(app.total_downloaded, file_size);
}

#[test]
fn cumulative_values_as_deltas_are_capped_at_file_size() {
    let mut app = test_app();
    let file_size: u64 = 1_000_000;

    app.handle_download_event(DownloadEvent::FileStart {
        id: "test.bin".to_string().into(),
        size: file_size,
        attempt_id: 0,
    });

    let cumulatives = [100_000u64, 350_000, 700_000, 900_000, 1_000_000];
    for c in cumulatives {
        app.handle_download_event(DownloadEvent::Progress {
            id: "test.bin".into(),
            delta: ProgressDelta {
                total_bytes_delta: c,
                network_bytes_delta: c,
            },
            attempt_id: 0,
        });
    }

    let file = app.files.iter().find(|f| f.id == "test.bin").unwrap();
    assert_eq!(file.downloaded, file_size);
    assert_eq!(app.total_downloaded, file_size);
}
