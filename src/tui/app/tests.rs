use super::*;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::{
    core::{CoreEvent, FileLifecycle, ResolvedFile, ResolvedPackage, SessionRunStatus},
    test_support::{FileFixtureStatus, UrlFixtureStatus, push_file, session_snapshot},
};

fn test_app() -> App {
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
fn to_json_contains_visible_file_state_without_internal_fields() {
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
        serde_json::from_str(&app.to_json()).expect("snapshot should be valid JSON");
    let file = &snapshot["files"][0];

    assert_eq!(file["id"], "stable/file.bin");
    assert_eq!(file["status"], "downloading");
    assert_eq!(
        snapshot["packages"][0]["source_url"],
        "https://mega.nz/file/abc"
    );
    assert_eq!(snapshot["total_downloaded"], 64);
    assert_eq!(snapshot["total_size"], 128);
    assert_eq!(snapshot["run_totals"]["run_total_bytes"], 128);
    assert_eq!(snapshot["displayed_network_rate_bps"], 0);
    assert!(file.get("rate").is_none());
    assert!(file.get("source_url").is_none());
    assert_eq!(snapshot["cpu_usage"], 12.5);
    assert_eq!(snapshot["memory_rss"], 4096);
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
    let accepted = app.update_file_ui_progress("file.bin", 90, now);

    assert_eq!(accepted, 10);
    assert_eq!(app.files[0].downloaded, 100);
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
    let mut app = test_app();
    let mut session = session_snapshot(vec![("https://mega.nz/file/a", UrlFixtureStatus::Fetched)]);
    push_file(&mut session, 0, "skip-a.bin", 1, FileFixtureStatus::Skipped);
    app.session = Some(session);

    let should_queue = app.register_session_queued_file("https://mega.nz/file/a", "skip-a.bin", 1);

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
fn url_resolved_updates_session_status_and_clears_overlay() {
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
    assert_eq!(session.packages[0].source_url, url);
    assert!(session.packages[0].error.is_none());
}

#[test]
fn mark_visible_file_error_updates_session_file_status() {
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
fn session_adapter_merge_state_updates_matching_files_and_preserves_unmatched_entries() {
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

    SessionAdapter::merge_state(&mut session, next);

    assert_eq!(session.status, crate::core::SessionRunStatus::Paused);
    assert_eq!(session.packages.len(), 2);
    assert!(
        session
            .packages
            .iter()
            .any(|entry| entry.source_url == "https://mega.nz/file/b"),
        "new URLs should be appended during merge"
    );
    assert_eq!(session.files.len(), 3);
    assert!(
        session.files.iter().any(|file| file.path == "keep.bin"
            && matches!(file.lifecycle, crate::core::FileLifecycle::Complete)
            && file.size == 5),
        "matching files should be replaced by the newer snapshot"
    );
    assert!(
        session.files.iter().any(|file| file.path == "stale.bin"),
        "existing unmatched files should be retained during partial migration"
    );
    assert!(session.files.iter().any(|file| file.path == "new.bin"));
}

#[test]
fn sorted_file_indices_group_by_package_before_status() {
    let mut app = test_app();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: "pkg-a".to_string(),
            source_url: "https://mega.nz/folder/a".to_string(),
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
            id: "pkg-b".to_string(),
            source_url: "https://mega.nz/folder/b".to_string(),
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

    let ordered: Vec<_> = app
        .sorted_file_indices()
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
fn pause_downloads_queues_core_backed_active_files() {
    let mut app = test_app();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: "pkg".to_string(),
            source_url: "https://mega.nz/folder/root".to_string(),
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
            id: "pkg".to_string(),
            source_url: "https://mega.nz/file/test".to_string(),
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
