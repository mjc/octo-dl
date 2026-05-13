use super::*;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tempfile::tempdir;
use tokio::sync::mpsc;

use crate::{
    core::{CoreEvent, FileLifecycle, ResolvedFile, ResolvedPackage, SessionRunStatus},
    test_support::{
        FileFixtureStatus, StateDirectoryGuard, UrlFixtureStatus, package_id, push_file,
        session_snapshot,
    },
    tui::{DashboardUiMode, visible::TuiRow},
};

fn test_app() -> App {
    let path = tempdir()
        .expect("test state directory should exist")
        .into_path();
    std::mem::forget(StateDirectoryGuard::set(&path));
    let (tx, _rx) = mpsc::unbounded_channel();
    App::new(9723, tx, true)
}

#[test]
fn login_state_field_cycling() {
    let mut login = LoginState::new();
    assert_eq!(login.active_field, 0);
    login.active_field = (login.active_field + 1) % LoginState::field_count();
    assert_eq!(login.active_field, 1);
    login.active_field = (login.active_field + 1) % LoginState::field_count();
    assert_eq!(login.active_field, 2);
    login.active_field = (login.active_field + 1) % LoginState::field_count();
    assert_eq!(login.active_field, 0);
}

#[test]
fn config_field_increment_decrement() {
    let mut config = ConfigState::new();
    let initial_chunks = config.config.chunks_per_file;
    config.config.chunks_per_file = config.config.chunks_per_file.saturating_add(1);
    assert_eq!(config.config.chunks_per_file, initial_chunks + 1);
    config.config.chunks_per_file = config.config.chunks_per_file.saturating_sub(1).max(1);
    assert_eq!(config.config.chunks_per_file, initial_chunks);
}

#[test]
fn config_field_toggle_bool() {
    let mut config = ConfigState::new();
    let initial = config.config.force_overwrite;
    config.config.force_overwrite = !config.config.force_overwrite;
    assert_ne!(config.config.force_overwrite, initial);
    config.config.force_overwrite = !config.config.force_overwrite;
    assert_eq!(config.config.force_overwrite, initial);
}

#[test]
fn app_initial_state() {
    let app = test_app();
    assert_eq!(app.popup, Popup::None);
    assert_eq!(app.pending_confirmation, None);
    assert!(!app.should_quit);
    assert!(!app.authenticated);
    assert!(app.url_input.is_empty());
    assert!(!app.url_input_active);
    assert!(app.files.is_empty());
    assert_eq!(app.files_completed, 0);
    assert_eq!(app.files_total, 0);
}

#[test]
fn login_state_active_value_mut() {
    let mut login = LoginState::new();

    login.active_field = 0;
    login.active_value_mut().push_str("test@example.com");
    assert_eq!(login.email(), "test@example.com");

    login.active_field = 1;
    login.active_value_mut().push_str("password123");
    assert_eq!(login.password(), "password123");

    login.active_field = 2;
    login.active_value_mut().push_str("123456");
    assert_eq!(login.mfa(), "123456");
}

#[test]
fn set_credentials_rejects_empty() {
    let mut login = LoginState::new();
    assert!(!login.set_credentials(String::new(), "pass".into(), String::new()));
    assert!(!login.set_credentials("user".into(), String::new(), String::new()));
    assert!(!login.has_credentials());
    assert!(login.set_credentials("user@example.com".into(), "pass".into(), String::new()));
    assert!(login.has_credentials());
}

#[test]
fn set_credentials_if_missing_does_not_override() {
    let mut login = LoginState::new();
    login.set_credentials("orig@example.com".into(), "origpass".into(), String::new());
    login.set_credentials_if_missing("new@example.com", "newpass", "123456");
    assert_eq!(login.email(), "orig@example.com");
    assert_eq!(login.password(), "origpass");
    assert_eq!(login.mfa(), "123456");
}

#[test]
fn set_credentials_if_missing_fills_empty() {
    let mut login = LoginState::new();
    login.set_credentials_if_missing("user@example.com", "pass", "");
    assert_eq!(login.email(), "user@example.com");
    assert_eq!(login.password(), "pass");
    assert!(login.has_credentials());
}

#[test]
fn mfa_option_returns_none_when_empty() {
    let mut login = LoginState::new();
    assert!(login.mfa_option().is_none());
    login.set_credentials("u".into(), "p".into(), "123".into());
    assert_eq!(login.mfa_option(), Some("123"));
}

#[test]
fn config_field_labels() {
    assert_eq!(ConfigField::ChunksPerFile.label(), "Chunks per file");
    assert_eq!(ConfigField::ConcurrentFiles.label(), "Concurrent files");
    assert_eq!(ConfigField::ForceOverwrite.label(), "Force overwrite");
    assert_eq!(ConfigField::CleanupOnError.label(), "Cleanup on error");
}

#[test]
fn quit_policy_converts_from_bool() {
    assert_eq!(QuitPolicy::from_bool(true), QuitPolicy::Enabled);
    assert_eq!(QuitPolicy::from_bool(false), QuitPolicy::Disabled);
    assert!(QuitPolicy::Enabled.is_enabled());
    assert!(!QuitPolicy::Disabled.is_enabled());
}

#[test]
fn dashboard_json_contains_visible_file_state_without_internal_fields() {
    let mut app = test_app();
    app.upsert_overlay_file(
        FileEntry {
            id: "stable/file.bin".to_string(),
            name: "file.bin".to_string(),
            size: 128,
            downloaded: 64,
            status: FileStatus::Downloading,
        },
        Some("https://mega.nz/file/abc".to_string()),
        true,
    );
    app.file_ui.insert(
        "stable/file.bin".to_string(),
        FileUiState {
            speed: 32,
            rate: Default::default(),
        },
    );
    app.cpu_usage = 12.5;
    app.memory_rss = 4096;
    app.recompute_totals();

    let snapshot: serde_json::Value =
        serde_json::from_str(&app.dashboard_json(DashboardUiMode::Tui, false))
            .expect("snapshot should be valid JSON");
    let file = &snapshot["files"][0];

    assert_eq!(file["id"], "stable/file.bin");
    assert_eq!(file["status"]["kind"], "downloading");
    assert_eq!(
        snapshot["packages"][0]["source_url"],
        "https://mega.nz/file/abc"
    );
    assert_eq!(snapshot["totals"]["total_downloaded"], 64);
    assert_eq!(snapshot["totals"]["total_size"], 128);
    assert_eq!(snapshot["totals"]["run_total_bytes"], 128);
    assert!(file.get("rate").is_none());
    assert!(file.get("source_url").is_none());
    assert_eq!(snapshot["metrics"]["cpu_usage"], 12.5);
    assert_eq!(snapshot["metrics"]["memory_rss"], 4096);
    assert!(snapshot.get("total_downloaded").is_none());
    assert!(snapshot.get("run_totals").is_none());
    assert!(snapshot.get("cpu_usage").is_none());
    assert!(
        snapshot["totals"]
            .get("displayed_network_rate_bps")
            .is_none()
    );
}

#[test]
fn transfer_rate_smooths_cumulative_samples() {
    let start = Instant::now();
    let mut rate = TransferRate::default();

    rate.reset(0, start);
    rate.record(100_000, start + Duration::from_millis(100));

    assert_eq!(rate.bytes_per_sec(start + Duration::from_millis(100)), 0);

    rate.reset(0, start);
    rate.record(1_000, start + Duration::from_secs(1));
    rate.record(2_000, start + Duration::from_secs(2));

    let current = rate.bytes_per_sec(start + Duration::from_secs(2));
    assert!((950..=1_050).contains(&current));

    let decayed = rate.bytes_per_sec(start + Duration::from_secs(11));
    assert!(decayed < current);
}

#[test]
fn aggregate_rate_uses_progress_since_current_baseline() {
    let start = Instant::now();
    let mut app = test_app();
    app.files.push(FileEntry {
        id: "file.bin".to_string(),
        name: "file.bin".to_string(),
        size: 2_000,
        downloaded: 1_000,
        status: FileStatus::Downloading,
    });
    app.total_downloaded = 1_000;
    app.total_network_downloaded = 1_000;
    app.aggregate_rate.reset(1_000, start);

    app.total_downloaded = app.total_downloaded.saturating_add(100);
    app.total_network_downloaded = app.total_network_downloaded.saturating_add(100);
    app.update_speeds_at(start + Duration::from_secs(1));

    assert!((95..=105).contains(&app.current_speed));
}

#[test]
fn aggregate_rate_ignores_reused_bytes() {
    let start = Instant::now();
    let mut app = test_app();
    app.files.push(FileEntry {
        id: "file.bin".to_string(),
        name: "file.bin".to_string(),
        size: 2_000,
        downloaded: 1_000,
        status: FileStatus::Downloading,
    });
    app.total_downloaded = 1_000;
    app.aggregate_rate.reset(0, start);

    app.total_downloaded = app.total_downloaded.saturating_add(1_000);
    app.update_speeds_at(start + Duration::from_secs(1));

    assert_eq!(app.current_speed, 0);
    assert_eq!(app.total_downloaded, 2_000);
}

#[test]
fn record_progress_caps_downloaded_at_file_size() {
    let mut app = test_app();
    let file = FileEntry {
        id: "file.bin".to_string(),
        name: "file.bin".to_string(),
        size: 100,
        downloaded: 90,
        status: FileStatus::Downloading,
    };
    let now = Instant::now();

    app.file_ui
        .insert("file.bin".to_string(), FileUiState::default());
    app.files.push(file);
    app.files[0].downloaded = 100;
    let accepted = app.update_file_ui_progress("file.bin", 90, 100, now);

    assert_eq!(accepted, 10);
    assert_eq!(app.files[0].downloaded, 100);
}

#[test]
fn progress_event_updates_visible_file_without_full_visible_sync() {
    let mut app = test_app();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id("pkg", "https://mega.nz/file/root"),
            source_url: "https://mega.nz/file/root".to_string(),
            key: crate::core::PackageKey::new("https://mega.nz/file/root".to_string().clone()),
            display_name: "Package".to_string(),
            files: vec![ResolvedFile {
                file_id: "file.bin".to_string(),
                path: "file.bin".to_string(),
                size: 100,
            }],
            collision: None,
        },
    });
    app.apply_core_event(CoreEvent::FileStarted {
        file_id: "file.bin".to_string(),
        size: 100,
    });

    app.handle_file_progress_event(
        Arc::<str>::from("file.bin"),
        crate::core::ProgressDelta {
            total_bytes_delta: 40,
            network_bytes_delta: 40,
        },
        0,
    );

    let file = app
        .files
        .iter()
        .find(|file| file.id == "file.bin")
        .expect("file should remain visible");
    assert_eq!(file.downloaded, 40);
    assert!(matches!(file.status, FileStatus::Downloading));
}

#[test]
fn sync_visible_files_rebuilds_visible_file_positions_for_core_rows() {
    let mut app = test_app();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id("pkg", "https://mega.nz/file/root"),
            source_url: "https://mega.nz/file/root".to_string(),
            key: crate::core::PackageKey::new("https://mega.nz/file/root".to_string()),
            display_name: "Package".to_string(),
            files: vec![ResolvedFile {
                file_id: "file.bin".to_string(),
                path: "file.bin".to_string(),
                size: 100,
            }],
            collision: None,
        },
    });
    app.apply_core_event(CoreEvent::FileStarted {
        file_id: "file.bin".to_string(),
        size: 100,
    });

    assert_eq!(app.visible_file_positions.get("file.bin"), Some(&0));

    app.handle_file_progress_event(
        Arc::<str>::from("file.bin"),
        crate::core::ProgressDelta {
            total_bytes_delta: 25,
            network_bytes_delta: 25,
        },
        0,
    );

    assert_eq!(app.visible_file_positions.get("file.bin"), Some(&0));
    assert_eq!(app.files[0].downloaded, 25);
}

#[test]
fn visible_file_context_prefers_core_state_over_stale_visible_row() {
    let mut app = test_app();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id("pkg", "https://mega.nz/file/root"),
            source_url: "https://mega.nz/file/root".to_string(),
            key: crate::core::PackageKey::new("https://mega.nz/file/root".to_string()),
            display_name: "Package".to_string(),
            files: vec![ResolvedFile {
                file_id: "file.bin".to_string(),
                path: "fresh.bin".to_string(),
                size: 321,
            }],
            collision: None,
        },
    });
    app.files[0].name = "stale.bin".to_string();
    app.files[0].size = 999;
    app.files[0].status = FileStatus::Error("stale".to_string());

    let context = app
        .visible_file_context("file.bin")
        .expect("context should exist");

    assert_eq!(context.artifact_path, "fresh.bin");
    assert_eq!(context.size, 321);
    assert!(matches!(context.status, FileStatus::Queued));
    assert_eq!(context.source_url.as_deref(), Some("https://mega.nz/file/root"));
}

#[test]
fn skipped_session_paths_groups_only_skipped_files_by_url() {
    let mut app = test_app();
    let mut session = session_snapshot(vec![
        ("https://mega.nz/file/a", UrlFixtureStatus::Fetched),
        ("https://mega.nz/file/b", UrlFixtureStatus::Fetched),
    ]);
    push_file(&mut session, 0, "skip-a.bin", 1, FileFixtureStatus::Skipped);
    push_file(&mut session, 1, "skip-b.bin", 1, FileFixtureStatus::Skipped);
    push_file(
        &mut session,
        0,
        "pending.bin",
        1,
        FileFixtureStatus::Pending,
    );
    app.session = Some(session);

    let skipped = app.skipped_session_paths();

    assert_eq!(skipped.len(), 2);
    assert!(
        skipped["https://mega.nz/file/a"].contains("skip-a.bin"),
        "skipped paths should include skipped file under original URL"
    );
    assert!(
        !skipped["https://mega.nz/file/a"].contains("pending.bin"),
        "non-skipped files must not appear in the snapshot"
    );
    assert!(skipped["https://mega.nz/file/b"].contains("skip-b.bin"));
}

#[test]
fn register_session_queued_file_does_not_revive_skipped_entry() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let mut app = test_app();
    let mut session = session_snapshot(vec![("https://mega.nz/file/a", UrlFixtureStatus::Fetched)]);
    push_file(&mut session, 0, "skip-a.bin", 1, FileFixtureStatus::Skipped);
    app.session = Some(session);

    let should_queue = app.register_session_queued_file(
        "https://mega.nz/file/a",
        "https://mega.nz/file/a",
        "https://mega.nz/file/a",
        "https://mega.nz/file/a",
        "skip-a.bin",
        1,
    );

    assert!(!should_queue);
    let session = app.session.as_ref().unwrap();
    assert_eq!(session.files.len(), 1);
    assert!(matches!(
        session.files[0].lifecycle,
        crate::core::FileLifecycle::Skipped
    ));
    assert_eq!(session.files[0].id, "skip-a.bin");
}

#[test]
fn register_session_queued_file_preserves_explicit_package_identity() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let mut app = test_app();
    app.session = Some(session_snapshot(vec![(
        "https://mega.nz/folder/root",
        UrlFixtureStatus::Fetched,
    )]));

    let should_queue = app.register_session_queued_file(
        "batch-folder",
        "Batch Folder",
        "https://mega.nz/folder/root",
        "https://mega.nz/folder/root",
        "episode-1.mkv",
        128,
    );

    assert!(should_queue);
    let session = app.session.as_ref().unwrap();
    assert_eq!(session.packages.len(), 1);
    assert_eq!(
        session.packages[0].id,
        package_id("batch-folder", "Batch Folder")
    );
    assert_eq!(session.packages[0].key.as_str(), "Batch Folder");
    assert_eq!(session.packages[0].display_name, "Batch Folder");
    assert_eq!(
        session.packages[0].file_ids,
        vec!["episode-1.mkv".to_string()]
    );
    assert_eq!(session.files.len(), 1);
    assert_eq!(
        session.files[0].package_id,
        package_id("batch-folder", "Batch Folder")
    );
}

#[test]
fn file_queued_without_explicit_package_id_reuses_existing_package_for_url() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let mut app = test_app();
    app.session = Some(session_snapshot(vec![(
        "https://mega.nz/folder/root",
        UrlFixtureStatus::Fetched,
    )]));

    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id("batch-folder", "https://mega.nz/folder/root"),
            source_url: "https://mega.nz/folder/root".to_string(),
            key: crate::core::PackageKey::new("https://mega.nz/folder/root".to_string().clone()),
            display_name: "Batch Folder".to_string(),
            files: vec![ResolvedFile {
                file_id: "episode-1.mkv".to_string(),
                path: "episode-1.mkv".to_string(),
                size: 128,
            }],
            collision: None,
        },
    });

    app.handle_download_event(DownloadEvent::FileQueued(QueuedFile {
        id: "episode-1.mkv".to_string(),
        size: 128,
        count_toward_progress: true,
        origin: crate::tui::event::FileOrigin {
            package_id: None,
            package_display_name: None,
            source_url: "https://mega.nz/folder/root".to_string(),
            submitted_url: "https://mega.nz/folder/root".to_string(),
        },
    }));

    let file = app
        .core_state
        .files
        .get("episode-1.mkv")
        .expect("queued file should be present");
    assert_eq!(
        file.package_id,
        package_id("batch-folder", "https://mega.nz/folder/root")
    );

    let session = app.session.as_ref().expect("session should be present");
    assert_eq!(
        session.packages.len(),
        1,
        "status={} urls={:?} files={:?}",
        app.status,
        session.urls,
        session.files
    );
    assert_eq!(
        session.packages[0].id,
        package_id("batch-folder", "https://mega.nz/folder/root")
    );
}

#[test]
fn register_session_queued_file_uses_resolved_source_url_for_package_identity() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let mut app = test_app();
    app.session = Some(session_snapshot(vec![(
        "bundle.dlc",
        UrlFixtureStatus::Fetched,
    )]));

    let should_queue = app.register_session_queued_file(
        "batch-folder",
        "Batch Folder",
        "bundle.dlc",
        "https://mega.nz/folder/resolved",
        "episode-1.mkv",
        128,
    );

    assert!(should_queue);
    let session = app.session.as_ref().unwrap();
    assert_eq!(
        session.packages.len(),
        1,
        "status={} urls={:?} files={:?}",
        app.status,
        session.urls,
        session.files
    );
    assert_eq!(
        session.packages[0].id,
        package_id("batch-folder", "Batch Folder")
    );
    assert_eq!(session.packages[0].key.as_str(), "Batch Folder");
    assert_eq!(
        session
            .urls
            .iter()
            .map(|entry| entry.url.as_str())
            .collect::<Vec<_>>(),
        vec!["https://mega.nz/folder/resolved"]
    );
    assert_eq!(
        session.files[0].package_id,
        package_id("batch-folder", "Batch Folder")
    );
    assert_eq!(
        session.files[0].source_url.as_deref(),
        Some("https://mega.nz/folder/resolved")
    );
}

#[test]
fn register_session_queued_file_dedupes_same_source_url_across_package_ids() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let mut app = test_app();
    app.session = Some(session_snapshot(vec![(
        "https://mega.nz/folder/root",
        UrlFixtureStatus::Fetched,
    )]));

    assert!(app.register_session_queued_file(
        "pkg-a",
        "Package A",
        "https://mega.nz/folder/root",
        "https://mega.nz/folder/root",
        "episode-1.mkv",
        128,
    ));
    assert!(app.register_session_queued_file(
        "pkg-b",
        "Package B",
        "https://mega.nz/folder/root",
        "https://mega.nz/folder/root",
        "episode-2.mkv",
        256,
    ));

    let session = app.session.as_ref().unwrap();
    assert_eq!(session.packages.len(), 2);
    assert!(
        session
            .packages
            .iter()
            .any(|package| package.id == package_id("pkg-a", "Package A"))
    );
    assert!(
        session
            .packages
            .iter()
            .any(|package| package.id == package_id("pkg-b", "Package B"))
    );
    assert!(session.files.iter().any(|file| {
        file.package_id == package_id("pkg-a", "Package A")
            && file.source_url.as_deref() == Some("https://mega.nz/folder/root")
    }));
    assert!(session.files.iter().any(|file| {
        file.package_id == package_id("pkg-b", "Package B")
            && file.source_url.as_deref() == Some("https://mega.nz/folder/root")
    }));
}

#[test]
fn file_queued_retires_submitted_url_alias_after_resolution() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let mut app = test_app();
    app.urls.push("bundle.dlc".to_string());
    app.session = Some(session_snapshot(vec![(
        "bundle.dlc",
        UrlFixtureStatus::Pending,
    )]));
    app.queue_url_placeholder("bundle.dlc".to_string());
    app.apply_core_event(CoreEvent::UrlSubmitted {
        url: "bundle.dlc".to_string(),
    });

    app.handle_download_event(DownloadEvent::FileQueued(QueuedFile {
        id: "episode-1.mkv".to_string(),
        size: 128,
        count_toward_progress: true,
        origin: crate::tui::event::FileOrigin {
            package_id: Some(crate::test_support::package_id(
                "batch-folder",
                "Batch Folder",
            )),
            package_display_name: Some("Batch Folder".to_string()),
            source_url: "https://mega.nz/folder/resolved".to_string(),
            submitted_url: "bundle.dlc".to_string(),
        },
    }));

    assert_eq!(
        app.urls,
        vec!["https://mega.nz/folder/resolved".to_string()]
    );
    assert_eq!(
        app.core_state.url_order,
        vec!["https://mega.nz/folder/resolved".to_string()]
    );
    assert!(!app.overlay_files.contains_key("bundle.dlc"));

    let session = app.session.as_ref().unwrap();
    assert_eq!(
        session
            .urls
            .iter()
            .map(|entry| entry.url.as_str())
            .collect::<Vec<_>>(),
        vec!["https://mega.nz/folder/resolved"]
    );
}

#[test]
fn url_resolved_updates_session_status_and_clears_overlay() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let mut app = test_app();
    let url = "https://mega.nz/folder/root".to_string();
    app.session = Some(session_snapshot(vec![(
        url.as_str(),
        UrlFixtureStatus::Pending,
    )]));

    app.handle_download_event(DownloadEvent::UrlQueued { url: url.clone() });
    assert!(app.overlay_files.contains_key(&url));

    app.handle_download_event(DownloadEvent::UrlResolved { url: url.clone() });

    assert!(!app.overlay_files.contains_key(&url));
    let session = app.session.as_ref().expect("session should remain");
    assert_eq!(session.urls[0].url, url);
    assert!(session.urls[0].error.is_none());
}

#[test]
fn pending_empty_package_placeholder_is_visible() {
    let mut app = test_app();

    app.submit_url("https://mega.nz/folder/root".to_string());

    assert_eq!(
        app.visible_rows(),
        vec![TuiRow::File {
            package_id: None,
            file_id: "https://mega.nz/folder/root".to_string(),
        }]
    );
    assert_eq!(
        app.selected_row(),
        Some(TuiRow::File {
            package_id: None,
            file_id: "https://mega.nz/folder/root".to_string(),
        })
    );
}

#[test]
fn core_persisted_session_snapshot_is_saved_to_disk() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let mut app = test_app();
    app.ensure_session_for_pending_urls();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id("pkg", "https://mega.nz/folder/root"),
            source_url: "https://mega.nz/folder/root".to_string(),
            key: crate::core::PackageKey::new("https://mega.nz/folder/root".to_string().clone()),
            display_name: "Root".to_string(),
            files: vec![ResolvedFile {
                file_id: "episode-1.mkv".to_string(),
                path: "episode-1.mkv".to_string(),
                size: 128,
            }],
            collision: None,
        },
    });

    let session = crate::core::SessionSnapshotV3::latest().expect("session should be saved");
    assert_eq!(session.packages.len(), 1);
    assert_eq!(session.packages[0].key.as_str(), "Root");
    assert_eq!(session.files.len(), 1);
    assert_eq!(session.files[0].path, "episode-1.mkv");
}

#[test]
fn download_status_message_reflects_actual_activity() {
    let mut app = test_app();

    app.upsert_overlay_file(
        FileEntry {
            id: "episode-1.mkv".to_string(),
            name: "episode-1.mkv".to_string(),
            size: 128,
            downloaded: 0,
            status: FileStatus::Queued,
        },
        Some("https://mega.nz/folder/root".to_string()),
        true,
    );
    app.recompute_totals();
    app.update_download_status_message();

    assert_eq!(app.status, "Queued (0/1)");

    app.overlay_file_mut("episode-1.mkv").unwrap().status = FileStatus::Downloading;
    app.sync_visible_files();
    app.update_download_status_message();

    assert_eq!(app.status, "Downloading (0/1)");
}

#[test]
fn visible_rows_hide_empty_failed_packages() {
    let mut app = test_app();
    let package_id = package_id("failed", "https://mega.nz/folder/failed");
    app.core_state.packages.insert(
        package_id,
        crate::core::PackageState {
            id: package_id,
            key: crate::core::PackageKey::new("https://mega.nz/folder/failed".to_string().clone()),
            display_name: "Failed".to_string(),
            status: crate::core::PackageStatus::Failed,
            error: Some("boom".to_string()),
        },
    );

    assert!(app.visible_rows().is_empty());
}

#[test]
fn deleted_package_with_no_remaining_visible_files_is_hidden() {
    let mut app = test_app();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id("pkg", "https://mega.nz/folder/root"),
            source_url: "https://mega.nz/folder/root".to_string(),
            key: crate::core::PackageKey::new("https://mega.nz/folder/root".to_string().clone()),
            display_name: "https://mega.nz/folder/root".to_string(),
            files: vec![ResolvedFile {
                file_id: "ghost.bin".to_string(),
                path: "ghost.bin".to_string(),
                size: 1,
            }],
            collision: None,
        },
    });
    app.apply_core_event(CoreEvent::FileDeleted {
        file_id: "ghost.bin".to_string(),
    });

    assert!(app.visible_rows().is_empty());
    assert!(app.file_list_state.selected().is_none());
}

#[test]
fn overlay_error_remains_visible_alongside_core_package_rows() {
    let mut app = test_app();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id("pkg", "https://mega.nz/folder/good"),
            source_url: "https://mega.nz/folder/good".to_string(),
            key: crate::core::PackageKey::new("https://mega.nz/folder/good".to_string().clone()),
            display_name: "Good Package".to_string(),
            files: vec![ResolvedFile {
                file_id: "good.bin".to_string(),
                path: "good.bin".to_string(),
                size: 1,
            }],
            collision: None,
        },
    });

    app.handle_download_event(crate::tui::event::DownloadEvent::ScopeError {
        scope: "https://mega.nz/folder/bad".to_string(),
        error: "bad folder".to_string(),
    });

    let rows = app.visible_rows();
    assert!(rows.contains(&TuiRow::Package(package_id(
        "pkg",
        "https://mega.nz/folder/good"
    ))));
    assert!(rows.contains(&TuiRow::File {
        package_id: None,
        file_id: "https://mega.nz/folder/bad".to_string(),
    }));
}

#[test]
fn url_level_overlay_error_does_not_also_render_empty_package_row() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let mut app = test_app();
    let url = "https://mega.nz/folder/bad".to_string();
    app.session = Some(session_snapshot(vec![(
        url.as_str(),
        UrlFixtureStatus::Pending,
    )]));

    app.handle_download_event(crate::tui::event::DownloadEvent::ScopeError {
        scope: url.clone(),
        error: "bad folder".to_string(),
    });

    assert_eq!(
        app.visible_rows(),
        vec![TuiRow::File {
            package_id: None,
            file_id: url,
        }]
    );
}

#[test]
fn deleting_url_level_error_removes_session_url_and_ignores_late_events() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let mut app = test_app();
    let url = "https://mega.nz/folder/bad".to_string();
    app.session = Some(session_snapshot(vec![(
        url.as_str(),
        UrlFixtureStatus::Pending,
    )]));

    app.handle_download_event(crate::tui::event::DownloadEvent::ScopeError {
        scope: url.clone(),
        error: "bad folder".to_string(),
    });
    app.handle_ui_action(UiAction::DeleteFile(url.clone()));

    assert!(app.visible_rows().is_empty());
    assert!(!app.urls.contains(&url));
    let session = app.session.as_ref().expect("session should remain");
    assert!(
        session
            .packages
            .iter()
            .all(|package| package.display_name != url)
    );

    app.handle_download_event(crate::tui::event::DownloadEvent::UrlResolved { url: url.clone() });
    app.handle_download_event(crate::tui::event::DownloadEvent::ScopeError {
        scope: url.clone(),
        error: "late folder error".to_string(),
    });

    assert!(app.visible_rows().is_empty());
    let session = app.session.as_ref().expect("session should remain");
    assert!(
        session
            .packages
            .iter()
            .all(|package| package.display_name != url)
    );
}

#[test]
fn resubmitting_deleted_url_clears_late_event_fence() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let mut app = test_app();
    let url = "https://mega.nz/folder/bad".to_string();
    app.deleted_files.insert(url.clone());

    app.submit_url(url.clone());

    assert!(app.urls.contains(&url));
    assert!(!app.deleted_files.contains(&url));
}

#[test]
fn shutdown_sync_refreshes_session_progress_skipped_during_hot_events() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let mut app = test_app();
    let url = "https://mega.nz/file/root".to_string();
    let mut session = session_snapshot(vec![(url.as_str(), UrlFixtureStatus::Fetched)]);
    push_file(&mut session, 0, "file-id", 128, FileFixtureStatus::Pending);
    app.session = Some(session);
    app.ensure_core_file("file-id", &url, "file-id", 128, true);

    app.apply_core_event(CoreEvent::FileStarted {
        file_id: "file-id".to_string(),
        size: 128,
    });
    app.apply_core_event(CoreEvent::FileProgress {
        file_id: "file-id".to_string(),
        total_bytes_delta: 64,
        network_bytes_delta: 64,
    });

    let session = app.session.as_ref().expect("session should remain");
    let file = session
        .files
        .iter()
        .find(|file| file.id == "file-id")
        .expect("file should exist in session");
    assert_eq!(file.progress.visible_completed_bytes, 0);

    app.sync_session_for_shutdown();

    let session = app.session.as_ref().expect("session should remain");
    let file = session
        .files
        .iter()
        .find(|file| file.id == "file-id")
        .expect("file should exist in session");
    assert_eq!(file.progress.visible_completed_bytes, 64);
    assert_eq!(file.progress.downloaded_network_bytes, 64);
}

#[test]
fn mark_visible_file_error_updates_session_file_status() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let mut app = test_app();
    let mut session = session_snapshot(vec![(
        "https://mega.nz/file/root",
        UrlFixtureStatus::Fetched,
    )]);
    push_file(&mut session, 0, "file-id", 128, FileFixtureStatus::Pending);
    app.session = Some(session);

    app.mark_visible_file_error("file-id", "file-id", "network failure");

    let session = app.session.as_ref().expect("session should remain");
    assert!(matches!(
        session.files[0].lifecycle,
        crate::core::FileLifecycle::Failed
    ));
    assert_eq!(session.files[0].message.as_deref(), Some("network failure"));
}

#[test]
fn session_adapter_replace_state_discards_stale_unmatched_entries() {
    let mut session = session_snapshot(vec![("https://mega.nz/file/a", UrlFixtureStatus::Pending)]);
    push_file(&mut session, 0, "keep.bin", 1, FileFixtureStatus::Pending);
    push_file(&mut session, 0, "stale.bin", 1, FileFixtureStatus::Pending);

    let mut next = session_snapshot(vec![
        ("https://mega.nz/file/a", UrlFixtureStatus::Fetched),
        ("https://mega.nz/file/b", UrlFixtureStatus::Pending),
    ]);
    next.status = SessionRunStatus::Paused;
    push_file(&mut next, 0, "keep.bin", 5, FileFixtureStatus::Completed);
    push_file(&mut next, 1, "new.bin", 2, FileFixtureStatus::Pending);

    SessionAdapter::replace_state(&mut session, next);

    assert_eq!(session.status, crate::core::SessionRunStatus::Paused);
    assert_eq!(session.packages.len(), 2);
    assert!(
        session
            .packages
            .iter()
            .any(|entry| entry.display_name == "new.bin"),
        "new URLs should be appended during merge"
    );
    assert_eq!(session.files.len(), 2);
    assert!(
        session.files.iter().any(|file| file.path == "keep.bin"
            && matches!(file.lifecycle, crate::core::FileLifecycle::Complete)
            && file.size == 5),
        "matching files should be replaced by the newer snapshot"
    );
    assert!(!session.files.iter().any(|file| file.path == "stale.bin"));
    assert!(session.files.iter().any(|file| file.path == "new.bin"));
}

#[test]
fn session_adapter_replace_state_replaces_stale_package_rows() {
    let mut session = session_snapshot(vec![("https://mega.nz/file/a", UrlFixtureStatus::Pending)]);
    session.packages.push(crate::core::PackageSnapshot {
        id: package_id("batch-stale", "https://mega.nz/file/a"),
        key: crate::core::PackageKey::new("https://mega.nz/file/a".to_string().clone()),
        display_name: "Stale Batch".to_string(),
        file_ids: vec!["old.bin".to_string()],
        error: None,
    });

    let next = session_snapshot(vec![("https://mega.nz/file/a", UrlFixtureStatus::Fetched)]);

    SessionAdapter::replace_state(&mut session, next);

    assert!(session.packages.is_empty());
    assert_eq!(session.urls.len(), 1);
    assert_eq!(session.urls[0].url, "https://mega.nz/file/a");
}

#[test]
fn session_adapter_register_queued_file_rebuilds_package_membership_immediately() {
    let mut session = session_snapshot(vec![(
        "https://mega.nz/folder/root",
        UrlFixtureStatus::Fetched,
    )]);
    session.packages.push(crate::core::PackageSnapshot {
        id: package_id("stale", "Stale Folder"),
        key: crate::core::PackageKey::new("Stale Folder"),
        display_name: "Stale Folder".to_string(),
        file_ids: vec!["ghost.bin".to_string()],
        error: Some("boom".to_string()),
    });

    assert!(SessionAdapter::register_queued_file(
        &mut session,
        "batch-folder",
        "Batch Folder",
        "https://mega.nz/folder/root",
        "https://mega.nz/folder/root",
        "episode-1.mkv",
        128,
    ));

    assert_eq!(session.packages.len(), 1);
    assert_eq!(session.packages[0].display_name, "Batch Folder");
    assert_eq!(
        session.packages[0].id,
        package_id("batch-folder", "Batch Folder")
    );
    assert_eq!(
        session.packages[0].file_ids,
        vec!["episode-1.mkv".to_string()]
    );
    assert_eq!(session.files.len(), 1);
    assert_eq!(session.files[0].package_id, session.packages[0].id);
}

#[test]
fn mutate_session_and_save_reloads_canonical_snapshot() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let mut app = test_app();
    let mut session = session_snapshot(vec![(
        "https://mega.nz/file/root",
        UrlFixtureStatus::Fetched,
    )]);
    push_file(
        &mut session,
        0,
        "episode-1.mkv",
        128,
        FileFixtureStatus::Pending,
    );
    session.save().unwrap();
    app.install_session(session);

    let _ = app.mutate_session_and_save(|session| {
        session.packages[0].file_ids.clear();
    });

    let session = app.session.as_ref().expect("session should remain");
    assert_eq!(
        session.packages[0].file_ids,
        vec!["episode-1.mkv".to_string()]
    );
}

#[test]
fn mutate_session_and_save_preserves_in_memory_state_on_failed_save() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let mut app = test_app();
    let mut session = session_snapshot(vec![(
        "https://mega.nz/file/root",
        UrlFixtureStatus::Fetched,
    )]);
    push_file(
        &mut session,
        0,
        "episode-1.mkv",
        128,
        FileFixtureStatus::Pending,
    );
    session.save().unwrap();
    app.install_session(session.clone());

    let _ = app.mutate_session_and_save(|session| {
        session.files[0].source_url = Some("https://mega.nz/file/other".to_string());
    });

    assert_eq!(
        app.status,
        format!(
            "Failed to save session: file {} references untracked source_url {}",
            session.files[0].id, "https://mega.nz/file/other"
        )
    );
    let saved = app.session.as_ref().expect("session should remain");
    assert_eq!(saved, &session);
}

#[test]
fn sorted_file_indices_group_by_package_before_status() {
    let mut app = test_app();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id("pkg-a", "https://mega.nz/folder/a"),
            source_url: "https://mega.nz/folder/a".to_string(),
            key: crate::core::PackageKey::new("https://mega.nz/folder/a".to_string().clone()),
            display_name: "Package A".to_string(),
            files: vec![
                ResolvedFile {
                    file_id: "a-queued.bin".to_string(),
                    path: "a-queued.bin".to_string(),
                    size: 10,
                },
                ResolvedFile {
                    file_id: "a-complete.bin".to_string(),
                    path: "a-complete.bin".to_string(),
                    size: 10,
                },
            ],
            collision: None,
        },
    });
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id("pkg-b", "https://mega.nz/folder/b"),
            source_url: "https://mega.nz/folder/b".to_string(),
            key: crate::core::PackageKey::new("https://mega.nz/folder/b".to_string().clone()),
            display_name: "Package B".to_string(),
            files: vec![ResolvedFile {
                file_id: "b-downloading.bin".to_string(),
                path: "b-downloading.bin".to_string(),
                size: 10,
            }],
            collision: None,
        },
    });
    app.apply_core_event(CoreEvent::FileQueued {
        file_id: "a-queued.bin".to_string(),
    });
    app.apply_core_event(CoreEvent::FileCompleted {
        file_id: "a-complete.bin".to_string(),
    });
    app.apply_core_event(CoreEvent::FileStarted {
        file_id: "b-downloading.bin".to_string(),
        size: 10,
    });

    let ordered: Vec<_> =
        super::super::visible::sorted_file_indices(&app.files, &app.core_state, &app.overlay_files)
            .into_iter()
            .map(|index| app.files[index].id.clone())
            .collect();

    assert_eq!(
        ordered,
        vec![
            "a-queued.bin".to_string(),
            "a-complete.bin".to_string(),
            "b-downloading.bin".to_string(),
        ]
    );
}

#[test]
fn expanded_package_orders_files_error_downloading_queued_complete() {
    let mut app = test_app();
    let package_id = package_id("pkg", "https://mega.nz/folder/root");
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id,
            source_url: "https://mega.nz/folder/root".to_string(),
            key: crate::core::PackageKey::new("https://mega.nz/folder/root".to_string().clone()),
            display_name: "Package".to_string(),
            files: vec![
                ResolvedFile {
                    file_id: "queued.bin".to_string(),
                    path: "queued.bin".to_string(),
                    size: 10,
                },
                ResolvedFile {
                    file_id: "complete.bin".to_string(),
                    path: "complete.bin".to_string(),
                    size: 10,
                },
                ResolvedFile {
                    file_id: "downloading.bin".to_string(),
                    path: "downloading.bin".to_string(),
                    size: 10,
                },
                ResolvedFile {
                    file_id: "error.bin".to_string(),
                    path: "error.bin".to_string(),
                    size: 10,
                },
            ],
            collision: None,
        },
    });
    app.expanded_packages.insert(package_id);
    app.apply_core_event(CoreEvent::FileQueued {
        file_id: "queued.bin".to_string(),
    });
    app.apply_core_event(CoreEvent::FileCompleted {
        file_id: "complete.bin".to_string(),
    });
    app.apply_core_event(CoreEvent::FileStarted {
        file_id: "downloading.bin".to_string(),
        size: 10,
    });
    app.apply_core_event(CoreEvent::FileFailed {
        file_id: "error.bin".to_string(),
        message: "boom".to_string(),
    });

    assert_eq!(
        app.visible_rows(),
        vec![
            TuiRow::Package(package_id),
            TuiRow::File {
                package_id: Some(package_id),
                file_id: "error.bin".to_string(),
            },
            TuiRow::File {
                package_id: Some(package_id),
                file_id: "downloading.bin".to_string(),
            },
            TuiRow::File {
                package_id: Some(package_id),
                file_id: "queued.bin".to_string(),
            },
            TuiRow::File {
                package_id: Some(package_id),
                file_id: "complete.bin".to_string(),
            },
        ]
    );
}

#[test]
fn pause_downloads_queues_core_backed_active_files() {
    let mut app = test_app();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id("pkg", "https://mega.nz/folder/root"),
            source_url: "https://mega.nz/folder/root".to_string(),
            key: crate::core::PackageKey::new("https://mega.nz/folder/root".to_string().clone()),
            display_name: "Package".to_string(),
            files: vec![ResolvedFile {
                file_id: "episode.bin".to_string(),
                path: "episode.bin".to_string(),
                size: 128,
            }],
            collision: None,
        },
    });
    app.apply_core_event(CoreEvent::FileStarted {
        file_id: "episode.bin".to_string(),
        size: 128,
    });
    let token = CancellationToken::new();
    app.cancellation_tokens
        .insert("episode.bin".to_string(), token.clone());

    app.pause_downloads();

    assert!(app.paused);
    assert!(token.is_cancelled());
    assert_eq!(
        app.core_state.files["episode.bin"].lifecycle,
        FileLifecycle::Queued
    );
    assert_eq!(
        app.files
            .iter()
            .find(|file| file.id == "episode.bin")
            .expect("visible row should remain")
            .status,
        FileStatus::Queued
    );
}

#[test]
fn sync_visible_files_prunes_stale_file_ui_state() {
    let mut app = test_app();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id("pkg", "https://mega.nz/file/test"),
            source_url: "https://mega.nz/file/test".to_string(),
            key: crate::core::PackageKey::new("https://mega.nz/file/test".to_string().clone()),
            display_name: "Package".to_string(),
            files: vec![ResolvedFile {
                file_id: "kept.bin".to_string(),
                path: "kept.bin".to_string(),
                size: 128,
            }],
            collision: None,
        },
    });
    app.file_ui.insert(
        "kept.bin".to_string(),
        FileUiState {
            speed: 42,
            rate: Default::default(),
        },
    );
    app.file_ui.insert(
        "stale.bin".to_string(),
        FileUiState {
            speed: 99,
            rate: Default::default(),
        },
    );

    app.sync_visible_files();

    assert!(app.file_ui.contains_key("kept.bin"));
    assert!(!app.file_ui.contains_key("stale.bin"));
}

#[test]
fn sync_visible_files_keeps_package_row_selected_when_failed_package_auto_expands() {
    let mut app = test_app();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id("pkg", "https://mega.nz/folder/test"),
            source_url: "https://mega.nz/folder/test".to_string(),
            key: crate::core::PackageKey::new("https://mega.nz/folder/test".to_string().clone()),
            display_name: "Package".to_string(),
            files: vec![
                ResolvedFile {
                    file_id: "episode-1.bin".to_string(),
                    path: "episode-1.bin".to_string(),
                    size: 128,
                },
                ResolvedFile {
                    file_id: "episode-2.bin".to_string(),
                    path: "episode-2.bin".to_string(),
                    size: 256,
                },
            ],
            collision: None,
        },
    });
    app.file_list_state.select(Some(0));
    assert_eq!(
        app.selected_row(),
        Some(TuiRow::Package(package_id(
            "pkg",
            "https://mega.nz/folder/test"
        )))
    );

    app.apply_core_event(CoreEvent::FileFailed {
        file_id: "episode-1.bin".to_string(),
        message: "boom".to_string(),
    });

    assert_eq!(app.file_list_state.selected(), Some(0));
    assert_eq!(
        app.selected_row(),
        Some(TuiRow::Package(package_id(
            "pkg",
            "https://mega.nz/folder/test"
        )))
    );
    assert_eq!(app.visible_rows().len(), 3);
}
