use super::*;
use std::time::{Duration, Instant};

use tempfile::tempdir;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    core::{
        CoreEvent, FileLifecycle, FilesystemSnapshot, PartialFileSnapshot, ResolvedFile,
        ResolvedPackage, SessionRunStatus, reconcile_restart,
    },
    test_support::{
        FileFixtureStatus, StateDirectoryGuard, UrlFixtureStatus, package_id, push_file,
        session_snapshot,
    },
    tui::{
        DashboardUiMode,
        app::VerificationTarget,
        event::{DownloadEvent, QueuedFile},
        visible::TuiRow,
    },
};

fn test_app() -> App {
    let path = tempdir().expect("test state directory should exist").keep();
    std::mem::forget(StateDirectoryGuard::set(&path));
    let (tx, _rx) = mpsc::unbounded_channel();
    App::new(9723, tx, true)
}

fn resolve_package(
    app: &mut App,
    source_url: &str,
    files: &[(&str, u64)],
) -> crate::core::PackageId {
    let package_id = package_id(source_url, source_url);
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id,
            source_url: source_url.to_string(),
            key: crate::core::PackageKey::new(source_url.to_string()),
            display_name: source_url.to_string(),
            files: files
                .iter()
                .map(|(path, size)| ResolvedFile {
                    file_id: (*path).to_string().into(),
                    path: (*path).to_string(),
                    size: *size,
                })
                .collect(),
            collision: None,
        },
    });
    package_id
}

fn mark_verification_inflight(app: &mut App, id: &str) -> crate::core::FileId {
    let file_id = crate::core::FileId::from(id);
    app.verifying_files.insert(file_id.clone());
    app.verification_inflight_files.insert(file_id.clone());
    app.verification_targets
        .insert(file_id.clone(), VerificationTarget::Resume);
    app.apply_core_event(CoreEvent::FileVerificationStarted {
        file_id: file_id.clone(),
    });
    file_id
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
    assert_eq!(
        ConfigField::MegaChunksPerRequest.label(),
        "MEGA chunks/request"
    );
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
            id: "stable/file.bin".to_string().into(),
            name: "file.bin".to_string(),
            size: 128,
            downloaded: 64,
            status: FileStatus::Downloading,
        },
        Some("https://mega.nz/file/abc".to_string()),
    );
    app.file_ui.insert(
        "stable/file.bin".to_string().into(),
        FileUiState {
            speed: 32,
            rate: Default::default(),
            sort_key: None,
            package_id: None,
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
    assert_eq!(snapshot["totals"]["total_downloaded"], 0);
    assert_eq!(snapshot["totals"]["total_size"], 0);
    assert_eq!(snapshot["totals"]["run_total_bytes"], 0);
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
        id: "file.bin".to_string().into(),
        name: "file.bin".to_string(),
        size: 2_000,
        downloaded: 1_000,
        status: FileStatus::Downloading,
    });
    app.total_downloaded = 1_000;
    app.total_network_downloaded = 1_000;
    app.core_state.totals.run_file_downloading = 1;
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
        id: "file.bin".to_string().into(),
        name: "file.bin".to_string(),
        size: 2_000,
        downloaded: 1_000,
        status: FileStatus::Downloading,
    });
    app.total_downloaded = 1_000;
    app.core_state.totals.run_file_downloading = 1;
    app.aggregate_rate.reset(0, start);

    app.total_downloaded = app.total_downloaded.saturating_add(1_000);
    app.update_speeds_at(start + Duration::from_secs(1));

    assert_eq!(app.current_speed, 0);
    assert_eq!(app.total_downloaded, 2_000);
}

#[test]
fn update_speeds_only_initializes_downloading_file_ui() {
    let start = Instant::now();
    let mut app = test_app();
    app.files.push(FileEntry {
        id: "downloading.bin".to_string().into(),
        name: "downloading.bin".to_string(),
        size: 2_000,
        downloaded: 1_000,
        status: FileStatus::Downloading,
    });
    app.files.push(FileEntry {
        id: "queued.bin".to_string().into(),
        name: "queued.bin".to_string(),
        size: 2_000,
        downloaded: 0,
        status: FileStatus::Queued,
    });
    app.files.push(FileEntry {
        id: "done.bin".to_string().into(),
        name: "done.bin".to_string(),
        size: 2_000,
        downloaded: 2_000,
        status: FileStatus::Complete,
    });
    app.core_state.totals.run_file_downloading = 1;
    app.aggregate_rate.reset(1_000, start);

    app.update_speeds_at(start + Duration::from_secs(1));

    assert!(app.file_ui.contains_key("downloading.bin"));
    assert!(!app.file_ui.contains_key("queued.bin"));
    assert!(!app.file_ui.contains_key("done.bin"));
}

#[test]
fn record_progress_caps_downloaded_at_file_size() {
    let mut app = test_app();
    let file = FileEntry {
        id: "file.bin".to_string().into(),
        name: "file.bin".to_string(),
        size: 100,
        downloaded: 90,
        status: FileStatus::Downloading,
    };
    let now = Instant::now();

    app.file_ui
        .insert("file.bin".to_string().into(), FileUiState::default());
    app.files.push(file);
    app.files[0].downloaded = 100;
    let accepted = app.update_file_ui_progress(&"file.bin".into(), 90, 100, now);

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
                file_id: "file.bin".to_string().into(),
                path: "file.bin".to_string(),
                size: 100,
            }],
            collision: None,
        },
    });
    app.apply_core_event(CoreEvent::FileStarted {
        file_id: "file.bin".to_string().into(),
        size: 100,
    });

    app.handle_file_progress_event(
        "file.bin".into(),
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
fn verification_progress_updates_visible_file_without_network_rate() {
    let mut app = test_app();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id("pkg", "https://mega.nz/file/root"),
            source_url: "https://mega.nz/file/root".to_string(),
            key: crate::core::PackageKey::new("https://mega.nz/file/root".to_string()),
            display_name: "Package".to_string(),
            files: vec![ResolvedFile {
                file_id: "file.bin".to_string().into(),
                path: "file.bin".to_string(),
                size: 100,
            }],
            collision: None,
        },
    });
    app.apply_core_event(CoreEvent::FileCompleted {
        file_id: "file.bin".to_string().into(),
    });
    app.verifying_files.insert("file.bin".to_string().into());
    app.verification_inflight_files
        .insert("file.bin".to_string().into());
    app.verification_targets
        .insert("file.bin".to_string().into(), VerificationTarget::Completed);
    app.apply_core_event(CoreEvent::FileVerificationStarted {
        file_id: "file.bin".to_string().into(),
    });
    app.refresh_visible_core_file(&"file.bin".to_string().into());

    app.handle_verification_progress_event("file.bin".into(), 45);

    let file = app
        .files
        .iter()
        .find(|file| file.id == "file.bin")
        .expect("file should remain visible");
    assert_eq!(file.downloaded, 45);
    assert!(app.verifying_files.contains("file.bin"));
    assert_eq!(
        app.core_state.files["file.bin"]
            .progress
            .downloaded_network_bytes,
        0
    );
    assert_eq!(app.file_speed(&"file.bin".into()), 0);
}

#[test]
fn completed_file_sync_clears_stale_file_speed() {
    let mut app = test_app();
    let file_id: crate::core::FileId = "file.bin".to_string().into();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id("pkg", "https://mega.nz/file/root"),
            source_url: "https://mega.nz/file/root".to_string(),
            key: crate::core::PackageKey::new("https://mega.nz/file/root".to_string()),
            display_name: "Package".to_string(),
            files: vec![ResolvedFile {
                file_id: file_id.clone(),
                path: "file.bin".to_string(),
                size: 100,
            }],
            collision: None,
        },
    });
    app.apply_core_event(CoreEvent::FileStarted {
        file_id: file_id.clone(),
        size: 100,
    });
    app.file_ui.insert(
        file_id.clone(),
        FileUiState {
            speed: 123,
            rate: TransferRate::default(),
            sort_key: None,
            package_id: Some(package_id("pkg", "https://mega.nz/file/root")),
        },
    );

    app.apply_core_event(CoreEvent::FileCompleted {
        file_id: file_id.clone(),
    });

    assert_eq!(app.file_speed(&file_id), 0);
}

#[test]
fn completed_file_verified_preserves_existing_complete_lifecycle() {
    let mut app = test_app();
    let file_id: crate::core::FileId = "file.bin".to_string().into();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id("pkg", "https://mega.nz/file/root"),
            source_url: "https://mega.nz/file/root".to_string(),
            key: crate::core::PackageKey::new("https://mega.nz/file/root".to_string()),
            display_name: "Package".to_string(),
            files: vec![ResolvedFile {
                file_id: file_id.clone(),
                path: "file.bin".to_string(),
                size: 100,
            }],
            collision: None,
        },
    });
    app.apply_core_event(CoreEvent::FileCompleted {
        file_id: file_id.clone(),
    });
    app.verifying_files.insert(file_id.clone());
    app.verification_inflight_files.insert(file_id.clone());
    app.verification_targets
        .insert(file_id.clone(), VerificationTarget::Completed);
    app.apply_core_event(CoreEvent::FileVerificationStarted {
        file_id: file_id.clone(),
    });
    app.refresh_visible_core_file(&file_id);
    assert_eq!(
        app.core_state.files.get(&file_id).unwrap().lifecycle,
        FileLifecycle::Queued
    );

    app.handle_completed_file_verified_event(file_id.clone(), 100);

    assert!(!app.verifying_files.contains(&file_id));
    assert!(matches!(
        app.core_state.files.get(&file_id).unwrap().lifecycle,
        FileLifecycle::Complete
    ));
    assert_eq!(
        app.visible_file(&file_id).unwrap().status,
        FileStatus::Complete
    );
    assert_eq!(app.status, "Verified file.bin: 100 B");
}

#[test]
fn verification_progress_is_ignored_for_skipped_file() {
    let mut app = test_app();
    let file_id: crate::core::FileId = "file.bin".to_string().into();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id("pkg", "https://mega.nz/file/root"),
            source_url: "https://mega.nz/file/root".to_string(),
            key: crate::core::PackageKey::new("https://mega.nz/file/root".to_string()),
            display_name: "Package".to_string(),
            files: vec![ResolvedFile {
                file_id: file_id.clone(),
                path: "file.bin".to_string(),
                size: 100,
            }],
            collision: None,
        },
    });
    app.verifying_files.insert(file_id.clone());
    app.apply_core_event(CoreEvent::FileVerificationStarted {
        file_id: file_id.clone(),
    });

    app.handle_verification_progress_event(file_id.clone(), 45);

    assert_eq!(
        app.core_state.files[&file_id]
            .progress
            .visible_completed_bytes,
        0
    );
    assert_eq!(app.visible_file(&file_id).unwrap().downloaded, 0);
}

#[test]
fn verification_skipped_clears_resume_verification_state() {
    let mut app = test_app();
    let file_id: crate::core::FileId = "file.bin".to_string().into();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id("pkg", "https://mega.nz/file/root"),
            source_url: "https://mega.nz/file/root".to_string(),
            key: crate::core::PackageKey::new("https://mega.nz/file/root".to_string()),
            display_name: "Package".to_string(),
            files: vec![ResolvedFile {
                file_id: file_id.clone(),
                path: "file.bin".to_string(),
                size: 100,
            }],
            collision: None,
        },
    });
    app.verifying_files.insert(file_id.clone());
    app.verification_inflight_files.insert(file_id.clone());
    app.verification_targets
        .insert(file_id.clone(), VerificationTarget::Resume);
    app.apply_core_event(CoreEvent::FileVerificationStarted {
        file_id: file_id.clone(),
    });

    app.handle_verification_skipped_event(file_id.clone(), false);

    assert!(!app.verifying_files.contains(&file_id));
    assert!(!app.verification_inflight_files.contains(&file_id));
    assert_eq!(
        app.core_state.files[&file_id]
            .progress
            .visible_completed_bytes,
        0
    );
    assert_eq!(app.visible_file(&file_id).unwrap().downloaded, 0);
}

#[test]
fn verification_skipped_restores_completed_file_lifecycle() {
    let mut app = test_app();
    let file_id: crate::core::FileId = "file.bin".to_string().into();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id("pkg", "https://mega.nz/file/root"),
            source_url: "https://mega.nz/file/root".to_string(),
            key: crate::core::PackageKey::new("https://mega.nz/file/root".to_string()),
            display_name: "Package".to_string(),
            files: vec![ResolvedFile {
                file_id: file_id.clone(),
                path: "file.bin".to_string(),
                size: 100,
            }],
            collision: None,
        },
    });
    app.apply_core_event(CoreEvent::FileCompleted {
        file_id: file_id.clone(),
    });
    app.verifying_files.insert(file_id.clone());
    app.verification_inflight_files.insert(file_id.clone());
    app.verification_targets
        .insert(file_id.clone(), VerificationTarget::Completed);
    app.apply_core_event(CoreEvent::FileVerificationStarted {
        file_id: file_id.clone(),
    });

    app.handle_verification_skipped_event(file_id.clone(), true);

    assert!(!app.verifying_files.contains(&file_id));
    assert!(!app.verification_inflight_files.contains(&file_id));
    assert!(matches!(
        app.core_state.files[&file_id].lifecycle,
        FileLifecycle::Complete
    ));
    assert_eq!(
        app.visible_file(&file_id).unwrap().status,
        FileStatus::Complete
    );
}

#[test]
fn verification_skipped_ignores_late_progress() {
    let mut app = test_app();
    resolve_package(&mut app, "https://mega.nz/file/root", &[("file.bin", 100)]);
    let file_id = mark_verification_inflight(&mut app, "file.bin");

    app.handle_verification_skipped_event(file_id.clone(), false);
    app.handle_verification_progress_event(file_id.clone(), 90);

    assert!(!app.verifying_files.contains(&file_id));
    assert!(!app.verification_inflight_files.contains(&file_id));
    assert_eq!(
        app.core_state.files[&file_id]
            .progress
            .visible_completed_bytes,
        0
    );
    assert_eq!(app.visible_file(&file_id).unwrap().downloaded, 0);
}

#[test]
fn file_error_clears_verification_state() {
    let mut app = test_app();
    resolve_package(&mut app, "https://mega.nz/file/root", &[("file.bin", 100)]);
    let file_id = mark_verification_inflight(&mut app, "file.bin");
    let token = CancellationToken::new();
    app.cancellation_tokens
        .insert(file_id.clone(), token.clone());
    app.track_shutdown_pending_file(&file_id);

    app.handle_file_error_event(file_id.clone(), "network failed".to_string(), 0);

    assert!(!app.verifying_files.contains(&file_id));
    assert!(!app.verification_inflight_files.contains(&file_id));
    assert!(!app.cancellation_tokens.contains_key(&file_id));
    assert!(!app.shutdown_pending_files.contains(&file_id));
    assert!(!token.is_cancelled());
    assert!(matches!(
        app.core_state.files[&file_id].lifecycle,
        FileLifecycle::Failed { .. }
    ));
}

#[test]
fn file_cancelled_clears_verification_state() {
    let mut app = test_app();
    resolve_package(&mut app, "https://mega.nz/file/root", &[("file.bin", 100)]);
    let file_id = mark_verification_inflight(&mut app, "file.bin");

    app.handle_file_cancelled_event(file_id.clone(), 0);

    assert!(!app.verifying_files.contains(&file_id));
    assert!(!app.verification_inflight_files.contains(&file_id));
    assert!(matches!(
        app.core_state.files[&file_id].lifecycle,
        FileLifecycle::Queued
    ));
}

#[test]
fn file_complete_clears_verification_state() {
    let mut app = test_app();
    resolve_package(&mut app, "https://mega.nz/file/root", &[("file.bin", 100)]);
    let file_id = mark_verification_inflight(&mut app, "file.bin");
    let token = CancellationToken::new();
    app.cancellation_tokens
        .insert(file_id.clone(), token.clone());
    app.track_shutdown_pending_file(&file_id);

    app.handle_file_complete_event(file_id.clone(), 0);

    assert!(!app.verifying_files.contains(&file_id));
    assert!(!app.verification_inflight_files.contains(&file_id));
    assert!(!app.cancellation_tokens.contains_key(&file_id));
    assert!(!app.shutdown_pending_files.contains(&file_id));
    assert!(!token.is_cancelled());
    assert!(matches!(
        app.core_state.files[&file_id].lifecycle,
        FileLifecycle::Complete
    ));
}

#[test]
fn file_start_clears_verification_state_and_pending_reverify() {
    let mut app = test_app();
    resolve_package(&mut app, "https://mega.nz/file/root", &[("file.bin", 100)]);
    let file_id = mark_verification_inflight(&mut app, "file.bin");
    app.reverify_pending_files.insert(file_id.clone());

    app.handle_file_start_event(file_id.clone(), 100, 0);

    assert!(!app.verifying_files.contains(&file_id));
    assert!(!app.verification_inflight_files.contains(&file_id));
    assert!(!app.reverify_pending_files.contains(&file_id));
    assert!(matches!(
        app.core_state.files[&file_id].lifecycle,
        FileLifecycle::Downloading
    ));
}

#[test]
fn resume_reverified_without_pending_resume_finishes_verification_state() {
    let mut app = test_app();
    resolve_package(&mut app, "https://mega.nz/file/root", &[("file.bin", 100)]);
    let file_id = mark_verification_inflight(&mut app, "file.bin");

    app.handle_resume_reverified_event(file_id.clone(), 1, 64);

    assert!(!app.verifying_files.contains(&file_id));
    assert!(!app.verification_inflight_files.contains(&file_id));
    assert_eq!(
        app.core_state.files[&file_id]
            .progress
            .verified_existing_bytes,
        64
    );
}

#[test]
fn retry_file_clears_verification_state() {
    let mut app = test_app();
    resolve_package(&mut app, "https://mega.nz/file/root", &[("file.bin", 100)]);
    let file_id = mark_verification_inflight(&mut app, "file.bin");
    app.reverify_pending_files.insert(file_id.clone());

    app.perform_retry_file_action(&file_id);

    assert!(!app.verifying_files.contains(&file_id));
    assert!(!app.verification_inflight_files.contains(&file_id));
    assert!(!app.reverify_pending_files.contains(&file_id));
}

#[test]
fn reset_file_clears_verification_state() {
    let mut app = test_app();
    resolve_package(&mut app, "https://mega.nz/file/root", &[("file.bin", 100)]);
    let file_id = mark_verification_inflight(&mut app, "file.bin");
    app.reverify_pending_files.insert(file_id.clone());

    app.perform_reset_file_action(&file_id);

    assert!(!app.verifying_files.contains(&file_id));
    assert!(!app.verification_inflight_files.contains(&file_id));
    assert!(!app.reverify_pending_files.contains(&file_id));
    assert!(app.reset_pending_files.contains(&file_id));
}

#[test]
fn delete_file_clears_verification_state() {
    let mut app = test_app();
    resolve_package(&mut app, "https://mega.nz/file/root", &[("file.bin", 100)]);
    let file_id = mark_verification_inflight(&mut app, "file.bin");
    app.reverify_pending_files.insert(file_id.clone());

    app.perform_delete_file_action(&file_id);

    assert!(!app.verifying_files.contains(&file_id));
    assert!(!app.verification_inflight_files.contains(&file_id));
    assert!(!app.reverify_pending_files.contains(&file_id));
}

#[test]
fn delete_package_clears_verification_state_for_all_files() {
    let mut app = test_app();
    let package_id = resolve_package(
        &mut app,
        "https://mega.nz/folder/root",
        &[("one.bin", 100), ("two.bin", 100)],
    );
    let one = mark_verification_inflight(&mut app, "one.bin");
    let two = mark_verification_inflight(&mut app, "two.bin");
    app.reverify_pending_files.insert(one.clone());
    app.reverify_pending_files.insert(two.clone());

    app.perform_delete_package_action(package_id);

    for file_id in [one, two] {
        assert!(!app.verifying_files.contains(&file_id));
        assert!(!app.verification_inflight_files.contains(&file_id));
        assert!(!app.reverify_pending_files.contains(&file_id));
    }
}

#[test]
fn delete_package_clears_shutdown_pending_state_for_all_files() {
    let mut app = test_app();
    let package_id = resolve_package(
        &mut app,
        "https://mega.nz/folder/root",
        &[("one.bin", 100), ("two.bin", 100)],
    );
    let one = crate::core::FileId::from("one.bin");
    let two = crate::core::FileId::from("two.bin");
    app.track_shutdown_pending_file(&one);
    app.track_shutdown_pending_file(&two);

    app.perform_delete_package_action(package_id);

    for file_id in [one, two] {
        assert!(!app.shutdown_pending_files.contains(&file_id));
    }
}

#[test]
fn reverify_package_with_only_never_started_files_is_noop() {
    let mut app = test_app();
    let (url_tx, mut url_rx) = mpsc::unbounded_channel();
    app.url_tx = url_tx;
    let package_id = resolve_package(
        &mut app,
        "https://mega.nz/folder/root",
        &[("one.bin", 100), ("two.bin", 100)],
    );

    app.perform_reverify_package_action(package_id);

    assert!(app.verifying_files.is_empty());
    assert!(app.verification_inflight_files.is_empty());
    assert!(url_rx.try_recv().is_err());
    assert_eq!(app.status, "No package file(s) have resume data to verify");
}

#[test]
fn reverify_package_clears_stale_verify_state_for_never_started_files() {
    let mut app = test_app();
    let (url_tx, mut url_rx) = mpsc::unbounded_channel();
    app.url_tx = url_tx;
    let package_id = resolve_package(
        &mut app,
        "https://mega.nz/folder/root",
        &[("one.bin", 100), ("two.bin", 100)],
    );
    let one = crate::core::FileId::from("one.bin");
    let two = crate::core::FileId::from("two.bin");
    app.verifying_files.insert(one.clone());
    app.verifying_files.insert(two.clone());
    app.verification_inflight_files.insert(one.clone());
    app.verification_inflight_files.insert(two.clone());

    app.perform_reverify_package_action(package_id);

    for file_id in [one, two] {
        assert!(!app.verifying_files.contains(&file_id));
        assert!(!app.verification_inflight_files.contains(&file_id));
        assert_eq!(
            app.visible_file(&file_id).unwrap().status,
            FileStatus::Queued
        );
        assert_eq!(app.visible_file(&file_id).unwrap().downloaded, 0);
    }
    assert!(url_rx.try_recv().is_err());
    assert_eq!(app.status, "No package file(s) have resume data to verify");
}

#[test]
fn verification_progress_requires_explicit_target() {
    let mut app = test_app();
    let file_id: crate::core::FileId = "file.bin".to_string().into();
    resolve_package(&mut app, "https://mega.nz/file/root", &[("file.bin", 100)]);
    app.verifying_files.insert(file_id.clone());
    app.verification_inflight_files.insert(file_id.clone());
    app.apply_core_event(CoreEvent::FileVerificationStarted {
        file_id: file_id.clone(),
    });

    app.handle_verification_progress_event(file_id.clone(), 75);

    assert_eq!(
        app.core_state.files[&file_id]
            .progress
            .visible_completed_bytes,
        0
    );
    assert_eq!(app.visible_file(&file_id).unwrap().downloaded, 0);
}

#[test]
fn reverify_package_includes_failed_file_with_partial_progress() {
    let mut app = test_app();
    let (url_tx, mut url_rx) = mpsc::unbounded_channel();
    app.url_tx = url_tx;
    let source_url = "https://mega.nz/folder/root";
    let package_id = resolve_package(&mut app, source_url, &[("failed.bin", 100)]);
    app.apply_core_event(CoreEvent::FileStarted {
        file_id: "failed.bin".to_string().into(),
        size: 100,
    });
    app.apply_core_event(CoreEvent::FileProgress {
        file_id: "failed.bin".to_string().into(),
        total_bytes_delta: 25,
        network_bytes_delta: 25,
    });
    app.apply_core_event(CoreEvent::FileFailed {
        file_id: "failed.bin".to_string().into(),
        message: "boom".to_string(),
    });

    app.perform_reverify_package_action(package_id);

    assert!(app.verifying_files.contains("failed.bin"));
    assert!(app.verification_inflight_files.contains("failed.bin"));
    assert_eq!(
        url_rx.try_recv().unwrap(),
        crate::tui::event::DownloadRequest::ReverifyFileIds {
            source_url: source_url.to_string(),
            file_ids: vec!["failed.bin".to_string().into()],
        }
    );
    assert!(url_rx.try_recv().is_err());
}

#[test]
fn reverify_active_file_bumps_attempt_generation() {
    let mut app = test_app();
    let (url_tx, mut url_rx) = mpsc::unbounded_channel();
    app.url_tx = url_tx;
    let file_id: crate::core::FileId = "active.bin".to_string().into();
    resolve_package(
        &mut app,
        "https://mega.nz/file/root",
        &[("active.bin", 100)],
    );
    app.apply_core_event(CoreEvent::FileStarted {
        file_id: file_id.clone(),
        size: 100,
    });
    app.apply_core_event(CoreEvent::FileProgress {
        file_id: file_id.clone(),
        total_bytes_delta: 47,
        network_bytes_delta: 47,
    });
    app.sync_visible_files();

    app.perform_reverify_file_action(&file_id);

    assert_eq!(app.file_attempt_ids.get(&file_id), Some(&1));
    assert!(app.verifying_files.contains(&file_id));
    assert_eq!(
        url_rx.try_recv().unwrap(),
        crate::tui::event::DownloadRequest::ReverifyFileIds {
            source_url: "https://mega.nz/file/root".to_string(),
            file_ids: vec![file_id],
        }
    );
    assert!(url_rx.try_recv().is_err());
}

#[test]
fn stale_old_attempt_progress_is_ignored_during_alt_r_reverify() {
    let mut app = test_app();
    let (url_tx, _url_rx) = mpsc::unbounded_channel();
    app.url_tx = url_tx;
    let file_id: crate::core::FileId = "active.bin".to_string().into();
    resolve_package(
        &mut app,
        "https://mega.nz/file/root",
        &[("active.bin", 100)],
    );
    app.apply_core_event(CoreEvent::FileStarted {
        file_id: file_id.clone(),
        size: 100,
    });
    app.apply_core_event(CoreEvent::FileProgress {
        file_id: file_id.clone(),
        total_bytes_delta: 47,
        network_bytes_delta: 47,
    });
    app.sync_visible_files();

    app.perform_reverify_file_action(&file_id);
    app.handle_download_event(crate::tui::event::DownloadEvent::Progress {
        id: file_id.clone(),
        delta: crate::core::ProgressDelta {
            total_bytes_delta: 5,
            network_bytes_delta: 5,
        },
        attempt_id: 0,
    });

    let file = app
        .visible_file(&file_id)
        .expect("file should remain visible");
    assert_eq!(file.downloaded, 0);
    assert!(app.verifying_files.contains(&file_id));
    assert!(app.verification_inflight_files.contains(&file_id));
    assert_eq!(
        app.file_attempt_ids.get(&file_id),
        Some(&1),
        "stale old-attempt events must no longer match after Alt-R on an active file"
    );
}

#[test]
fn stale_old_attempt_cancel_is_ignored_after_alt_r_resume() {
    let mut app = test_app();
    let (url_tx, _url_rx) = mpsc::unbounded_channel();
    app.url_tx = url_tx;
    let file_id: crate::core::FileId = "active.bin".to_string().into();
    resolve_package(
        &mut app,
        "https://mega.nz/file/root",
        &[("active.bin", 100)],
    );
    app.apply_core_event(CoreEvent::FileStarted {
        file_id: file_id.clone(),
        size: 100,
    });
    app.apply_core_event(CoreEvent::FileProgress {
        file_id: file_id.clone(),
        total_bytes_delta: 47,
        network_bytes_delta: 47,
    });
    app.sync_visible_files();

    app.perform_reverify_file_action(&file_id);
    app.handle_download_event(crate::tui::event::DownloadEvent::ResumeReverified {
        id: file_id.clone(),
        chunks: 1,
        bytes: 47,
    });
    app.handle_download_event(crate::tui::event::DownloadEvent::FileStart {
        id: file_id.clone(),
        size: 100,
        attempt_id: 1,
    });
    app.handle_download_event(crate::tui::event::DownloadEvent::FileCancelled {
        id: file_id.clone(),
        attempt_id: 0,
    });

    let file = app
        .visible_file(&file_id)
        .expect("file should remain visible");
    assert_eq!(file.downloaded, 47);
    assert_eq!(file.status, FileStatus::Downloading);
}

#[test]
fn verification_progress_is_ignored_after_verification_finishes() {
    let mut app = test_app();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id("pkg", "https://mega.nz/file/root"),
            source_url: "https://mega.nz/file/root".to_string(),
            key: crate::core::PackageKey::new("https://mega.nz/file/root".to_string()),
            display_name: "Package".to_string(),
            files: vec![ResolvedFile {
                file_id: "file.bin".to_string().into(),
                path: "file.bin".to_string(),
                size: 100,
            }],
            collision: None,
        },
    });
    app.sync_visible_files();

    app.handle_verification_progress_event("file.bin".into(), 45);

    assert_eq!(
        app.core_state.files["file.bin"]
            .progress
            .visible_completed_bytes,
        0
    );
    let file = app
        .files
        .iter()
        .find(|file| file.id == "file.bin")
        .expect("file should remain visible");
    assert_eq!(file.downloaded, 0);
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
                file_id: "file.bin".to_string().into(),
                path: "file.bin".to_string(),
                size: 100,
            }],
            collision: None,
        },
    });
    app.apply_core_event(CoreEvent::FileStarted {
        file_id: "file.bin".to_string().into(),
        size: 100,
    });

    assert_eq!(app.visible_file_positions.get("file.bin"), Some(&0));

    app.handle_file_progress_event(
        "file.bin".into(),
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
                file_id: "file.bin".to_string().into(),
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
        .visible_file_context(&"file.bin".into())
        .expect("context should exist");

    assert_eq!(context.artifact_path, "fresh.bin");
    assert_eq!(context.size, 321);
    assert!(matches!(context.status, FileStatus::Queued));
    assert_eq!(
        context.source_url.as_deref(),
        Some("https://mega.nz/file/root")
    );
}

#[test]
fn session_adapter_register_queued_file_preserves_explicit_package_identity() {
    let mut session = session_snapshot(vec![(
        "https://mega.nz/folder/root",
        UrlFixtureStatus::Fetched,
    )]);

    let should_queue = SessionAdapter::register_queued_file(
        &mut session,
        "batch-folder",
        "Batch Folder",
        "https://mega.nz/folder/root",
        "https://mega.nz/folder/root",
        "episode-1.mkv",
        128,
    );

    assert!(should_queue);
    assert_eq!(session.packages.len(), 1);
    assert_eq!(
        session.packages[0].id,
        package_id("batch-folder", "Batch Folder")
    );
    assert_eq!(session.packages[0].key.as_str(), "Batch Folder");
    assert_eq!(session.packages[0].display_name, "Batch Folder");
    assert_eq!(
        session.packages[0]
            .files
            .iter()
            .map(|file| file.id.clone())
            .collect::<Vec<_>>(),
        vec![crate::core::FileId::from("episode-1.mkv")]
    );
    assert_eq!(session.file_count(), 1);
    assert_eq!(
        session.find_file("episode-1.mkv").unwrap().package_id,
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
                file_id: "episode-1.mkv".to_string().into(),
                path: "episode-1.mkv".to_string(),
                size: 128,
            }],
            collision: None,
        },
    });

    app.handle_download_event(DownloadEvent::FileQueued(QueuedFile {
        id: "episode-1.mkv".to_string().into(),
        size: 128,
        accounting: crate::core::FileAccounting::CurrentRun,
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
        session
            .iter_files()
            .map(|file| file.id.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        session.packages[0].id,
        package_id("batch-folder", "https://mega.nz/folder/root")
    );
}

#[test]
fn file_queued_does_not_demote_completed_file() {
    let mut app = test_app();
    let source_url = "https://mega.nz/folder/root";
    app.core_state.url_order.push(source_url.to_string());
    resolve_package(&mut app, source_url, &[("episode-1.mkv", 128)]);
    app.apply_core_event(CoreEvent::FileCompleted {
        file_id: "episode-1.mkv".to_string().into(),
    });

    app.handle_download_event(DownloadEvent::FileQueued(QueuedFile {
        id: "episode-1.mkv".to_string().into(),
        size: 128,
        accounting: crate::core::FileAccounting::CurrentRun,
        origin: crate::tui::event::FileOrigin {
            package_id: None,
            package_display_name: None,
            source_url: source_url.to_string(),
            submitted_url: source_url.to_string(),
        },
    }));

    assert_eq!(
        app.core_state.files["episode-1.mkv"].lifecycle,
        FileLifecycle::Complete
    );
}

#[test]
fn session_adapter_register_queued_file_uses_resolved_source_url_for_package_identity() {
    let mut session = session_snapshot(vec![("bundle.dlc", UrlFixtureStatus::Fetched)]);

    let should_queue = SessionAdapter::register_queued_file(
        &mut session,
        "batch-folder",
        "Batch Folder",
        "bundle.dlc",
        "https://mega.nz/folder/resolved",
        "episode-1.mkv",
        128,
    );

    assert!(should_queue);
    assert_eq!(
        session.packages.len(),
        1,
        "urls={:?} files={:?}",
        session.urls,
        session
            .iter_files()
            .map(|file| file.id.clone())
            .collect::<Vec<_>>()
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
    let file = session.find_file("episode-1.mkv").unwrap();
    assert_eq!(file.package_id, package_id("batch-folder", "Batch Folder"));
    assert_eq!(file.source_url, "https://mega.nz/folder/resolved");
}

#[test]
fn session_adapter_register_queued_file_dedupes_same_source_url_across_package_ids() {
    let mut session = session_snapshot(vec![(
        "https://mega.nz/folder/root",
        UrlFixtureStatus::Fetched,
    )]);

    assert!(SessionAdapter::register_queued_file(
        &mut session,
        "pkg-a",
        "Package A",
        "https://mega.nz/folder/root",
        "https://mega.nz/folder/root",
        "episode-1.mkv",
        128,
    ));
    assert!(SessionAdapter::register_queued_file(
        &mut session,
        "pkg-b",
        "Package B",
        "https://mega.nz/folder/root",
        "https://mega.nz/folder/root",
        "episode-2.mkv",
        256,
    ));

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
    assert!(session.iter_files().any(|file| {
        file.package_id == package_id("pkg-a", "Package A")
            && file.source_url == "https://mega.nz/folder/root"
    }));
    assert!(session.iter_files().any(|file| {
        file.package_id == package_id("pkg-b", "Package B")
            && file.source_url == "https://mega.nz/folder/root"
    }));
}

#[test]
fn file_queued_retires_submitted_url_alias_after_resolution() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let mut app = test_app();
    app.core_state.url_order.push("bundle.dlc".to_string());
    app.session = Some(session_snapshot(vec![(
        "bundle.dlc",
        UrlFixtureStatus::Pending,
    )]));
    app.queue_url_placeholder("bundle.dlc".to_string());
    app.apply_core_event(CoreEvent::UrlSubmitted {
        url: "bundle.dlc".to_string(),
    });

    app.handle_download_event(DownloadEvent::FileQueued(QueuedFile {
        id: "episode-1.mkv".to_string().into(),
        size: 128,
        accounting: crate::core::FileAccounting::CurrentRun,
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
        app.tracked_urls(),
        ["https://mega.nz/folder/resolved".to_string()].as_slice()
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
    app.apply_core_event(CoreEvent::UrlSubmitted { url: url.clone() });

    app.handle_download_event(DownloadEvent::UrlQueued { url: url.clone() });
    assert!(app.overlay_files.contains_key(url.as_str()));

    app.handle_download_event(DownloadEvent::UrlResolved { url: url.clone() });

    assert!(!app.overlay_files.contains_key(url.as_str()));
    let session = app.session.as_ref().expect("session should remain");
    assert!(session.urls.is_empty());
}

#[test]
fn pending_empty_package_placeholder_is_visible() {
    let mut app = test_app();

    app.submit_url("https://mega.nz/folder/root".to_string());

    assert_eq!(
        app.visible_rows(),
        vec![TuiRow::File {
            package_id: None,
            file_id: "https://mega.nz/folder/root".to_string().into(),
        }]
    );
    assert_eq!(
        app.selected_row(),
        Some(TuiRow::File {
            package_id: None,
            file_id: "https://mega.nz/folder/root".to_string().into(),
        })
    );
}

#[test]
fn deleting_pending_url_removes_it_from_core_state_and_session() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let mut app = test_app();
    let url = "https://mega.nz/folder/root".to_string();

    app.submit_url(url.clone());
    app.handle_ui_action(UiAction::DeleteFile(url.clone().into()));
    app.sync_session_for_shutdown();
    app.flush_session_persistence();

    assert!(app.tracked_urls().is_empty());
    assert!(app.core_state.url_order.is_empty());
    assert!(
        app.session
            .as_ref()
            .is_some_and(|session| session.urls.is_empty() && session.packages.is_empty())
    );
    assert!(crate::core::SessionSnapshot::latest().is_none());
}

#[test]
fn deleting_url_level_error_stays_deleted_after_shutdown_sync() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let mut app = test_app();
    let url = "https://mega.nz/folder/bad".to_string();

    app.submit_url(url.clone());
    app.handle_download_event(crate::tui::event::DownloadEvent::ScopeError {
        scope: url.clone(),
        error: "bad folder".to_string(),
    });
    app.handle_ui_action(UiAction::DeleteFile(url.clone().into()));
    app.sync_session_for_shutdown();
    app.flush_session_persistence();

    assert!(app.visible_rows().is_empty());
    assert!(app.tracked_urls().is_empty());
    assert!(app.core_state.url_order.is_empty());
    assert!(
        app.session
            .as_ref()
            .is_some_and(|session| session.urls.is_empty() && session.packages.is_empty())
    );
    assert!(crate::core::SessionSnapshot::latest().is_none());
}

#[test]
fn empty_package_resolution_stays_gone_after_shutdown_restart() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let mut app = test_app();
    let url = "https://mega.nz/folder/empty".to_string();

    app.submit_url(url.clone());
    app.handle_download_event(DownloadEvent::UrlQueued { url: url.clone() });
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id(&url, &url),
            source_url: url.clone(),
            key: crate::core::PackageKey::new(url.clone()),
            display_name: "Empty package".to_string(),
            files: Vec::new(),
            collision: None,
        },
    });
    app.handle_download_event(DownloadEvent::UrlResolved { url: url.clone() });
    app.sync_session_for_shutdown();
    app.flush_session_persistence();

    assert!(app.tracked_urls().is_empty());
    assert!(app.core_state.url_order.is_empty());
    assert!(crate::core::SessionSnapshot::latest().is_none());

    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let mut resumed = App::new(0, event_tx, true);
    resumed.resume_latest_session();

    assert!(resumed.tracked_urls().is_empty());
    assert!(resumed.visible_rows().is_empty());
    let mut url_rx = resumed.url_rx.take().expect("url_rx should exist");
    assert!(url_rx.try_recv().is_err());
}

#[test]
fn deleting_one_pending_url_preserves_other_pending_urls_across_shutdown() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let mut app = test_app();
    let removed = "https://mega.nz/folder/remove".to_string();
    let kept = "https://mega.nz/folder/keep".to_string();

    app.submit_url(removed.clone());
    app.submit_url(kept.clone());
    app.handle_ui_action(UiAction::DeleteFile(removed.clone().into()));
    app.sync_session_for_shutdown();
    app.flush_session_persistence();

    assert_eq!(app.tracked_urls(), [kept.clone()].as_slice());
    assert_eq!(app.core_state.url_order, vec![kept.clone()]);
    let session = app.session.as_ref().expect("session should remain");
    assert_eq!(session.urls.len(), 1);
    assert_eq!(session.urls[0].url, kept);
    assert!(session.urls[0].error.is_none());
    let latest = crate::core::SessionSnapshot::latest().expect("session should be saved");
    assert_eq!(latest.urls.len(), 1);
    assert_eq!(latest.urls[0].url, "https://mega.nz/folder/keep");
}

#[test]
fn selected_row_uses_valid_visible_row_cache() {
    let mut app = test_app();
    let cached_row = TuiRow::File {
        package_id: None,
        file_id: "cached-placeholder".to_string().into(),
    };
    app.cached_visible_rows_key = app.visible_rows_cache_key();
    app.cached_visible_rows = vec![cached_row.clone()];
    app.file_list_state.select(Some(0));

    assert_eq!(app.selected_row(), Some(cached_row));
}

#[test]
fn ensure_core_file_in_package_collapses_visible_syncs() {
    let mut app = test_app();
    crate::core::reducer::reset_snapshot_from_state_call_count();

    app.ensure_core_file_in_package(
        &"episode-1.mkv".into(),
        "pkg",
        "Package",
        "https://mega.nz/folder/root",
        "episode-1.mkv",
        128,
        crate::core::FileAccounting::CurrentRun,
    );

    app.flush_session_persistence();

    assert_eq!(app.visible_sync_count, 1);
    assert_eq!(app.session_persist_count, 1);
    assert_eq!(crate::core::reducer::snapshot_from_state_call_count(), 1);
    assert!(
        app.core_state
            .files
            .contains_key(&FileId::from("episode-1.mkv"))
    );
}

#[test]
fn add_urls_collapses_placeholder_visible_syncs() {
    let mut app = test_app();
    crate::core::reducer::reset_snapshot_from_state_call_count();

    app.handle_ui_action(UiAction::AddUrls(vec![
        "https://mega.nz/folder/one".to_string(),
        "https://mega.nz/folder/two".to_string(),
        "https://mega.nz/folder/three".to_string(),
    ]));

    app.flush_session_persistence();

    assert_eq!(app.visible_sync_count, 1);
    assert_eq!(app.session_persist_count, 1);
    assert_eq!(crate::core::reducer::snapshot_from_state_call_count(), 1);
    assert_eq!(app.tracked_urls().len(), 3);
    assert_eq!(app.overlay_files.len(), 3);
}

#[test]
fn deferred_batch_persistence_waits_for_poll_before_writing_snapshot() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let mut app = test_app();

    app.with_deferred_batch_updates(|app| {
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id("pkg", "https://mega.nz/folder/root"),
                source_url: "https://mega.nz/folder/root".to_string(),
                key: crate::core::PackageKey::new("https://mega.nz/folder/root".to_string()),
                display_name: "Root".to_string(),
                files: vec![ResolvedFile {
                    file_id: "episode-1.mkv".to_string().into(),
                    path: "episode-1.mkv".to_string(),
                    size: 128,
                }],
                collision: None,
            },
        });
    });

    assert!(crate::core::SessionSnapshot::latest().is_none());
    assert_eq!(
        app.session
            .as_ref()
            .map(|session| session.file_count())
            .unwrap_or_default(),
        1
    );
    assert_eq!(app.session_persist_count, 0);

    let Some(super::PendingSessionPersistence::SaveCurrent { queued_at }) =
        app.pending_session_persistence.as_mut()
    else {
        panic!("session save should stay queued until poll");
    };
    // Simulate the debounce window expiring so poll_session_persistence flushes
    // the queued save without waiting in real time.
    *queued_at = std::time::Instant::now() - super::persistence::SESSION_SAVE_DEBOUNCE;
    app.poll_session_persistence();
    assert_eq!(app.session_persist_count, 1);
    assert!(crate::core::SessionSnapshot::latest().is_none());

    app.flush_session_persistence();

    let latest = crate::core::SessionSnapshot::latest().expect("session should be saved");
    assert_eq!(latest.file_count(), 1);
}

#[test]
fn bookmarklet_added_url_placeholder_is_visible_with_existing_packages() {
    let mut app = test_app();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id("pkg", "https://mega.nz/file/root"),
            source_url: "https://mega.nz/file/root".to_string(),
            key: crate::core::PackageKey::new("https://mega.nz/file/root".to_string()),
            display_name: "Package".to_string(),
            files: vec![ResolvedFile {
                file_id: "file.bin".to_string().into(),
                path: "file.bin".to_string(),
                size: 100,
            }],
            collision: None,
        },
    });

    app.handle_ui_action(UiAction::AddUrls(vec![
        "https://mega.nz/folder/bookmarklet".to_string(),
    ]));

    assert!(app.visible_rows().contains(&TuiRow::File {
        package_id: None,
        file_id: "https://mega.nz/folder/bookmarklet".to_string().into(),
    }));
    let placeholder = app
        .visible_file(&"https://mega.nz/folder/bookmarklet".into())
        .expect("bookmarklet placeholder should be visible");
    assert_eq!(placeholder.name, "https://mega.nz/folder/bookmarklet");
    assert!(matches!(placeholder.status, FileStatus::Queued));
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
                file_id: "episode-1.mkv".to_string().into(),
                path: "episode-1.mkv".to_string(),
                size: 128,
            }],
            collision: None,
        },
    });
    app.flush_session_persistence();

    let session = crate::core::SessionSnapshot::latest().expect("session should be saved");
    assert_eq!(session.packages.len(), 1);
    assert_eq!(
        session.packages[0].key.as_str(),
        "https://mega.nz/folder/root"
    );
    assert_eq!(session.packages[0].display_name, "Root");
    assert_eq!(session.file_count(), 1);
    assert_eq!(
        session.find_file("episode-1.mkv").unwrap().path,
        "episode-1.mkv"
    );
}

#[test]
fn shutdown_persists_latest_file_progress_after_non_persisted_progress_events() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let mut app = test_app();
    let file_id: FileId = "episode-1.mkv".to_string().into();

    app.ensure_session_for_pending_urls();
    resolve_package(
        &mut app,
        "https://mega.nz/folder/root",
        &[("episode-1.mkv", 1_000)],
    );
    app.apply_core_event(CoreEvent::FileStarted {
        file_id: file_id.clone(),
        size: 1_000,
    });
    app.flush_session_persistence();

    app.handle_file_progress_event(
        file_id.clone(),
        crate::core::ProgressDelta {
            total_bytes_delta: 400,
            network_bytes_delta: 400,
        },
        0,
    );

    let latest_before_shutdown =
        crate::core::SessionSnapshot::latest().expect("session should exist before shutdown");
    assert_eq!(
        latest_before_shutdown
            .find_file("episode-1.mkv")
            .expect("file should exist before shutdown")
            .progress
            .visible_completed_bytes,
        0
    );

    app.sync_session_for_shutdown();

    let latest = crate::core::SessionSnapshot::latest().expect("session should be saved");
    let file = latest
        .find_file("episode-1.mkv")
        .expect("file should exist after shutdown");
    assert_eq!(file.progress.visible_completed_bytes, 400);
    assert_eq!(file.progress.downloaded_network_bytes, 400);
    assert_eq!(file.lifecycle, FileLifecycle::Downloading);
}

#[test]
fn deferred_core_persistence_materializes_before_session_mutation() {
    let mut app = test_app();
    crate::core::reducer::reset_snapshot_from_state_call_count();

    app.with_deferred_batch_updates(|app| {
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id("pkg", "https://mega.nz/folder/root"),
                source_url: "https://mega.nz/folder/root".to_string(),
                key: crate::core::PackageKey::new("https://mega.nz/folder/root".to_string()),
                display_name: "Root".to_string(),
                files: vec![ResolvedFile {
                    file_id: "episode-1.mkv".to_string().into(),
                    path: "episode-1.mkv".to_string(),
                    size: 128,
                }],
                collision: None,
            },
        });
        let _ = app.mutate_session_and_save(|session| {
            let tracked_url = session
                .urls
                .iter_mut()
                .find(|tracked_url| tracked_url.url == "https://mega.nz/folder/root")
                .expect("package-resolved snapshot should seed the tracked url");
            tracked_url.error = Some("boom".to_string());
        });
    });

    app.flush_session_persistence();

    assert_eq!(app.session_persist_count, 1);
    assert_eq!(crate::core::reducer::snapshot_from_state_call_count(), 1);
    assert_eq!(
        app.session
            .as_ref()
            .and_then(|session| session
                .urls
                .iter()
                .find(|url| { url.url == "https://mega.nz/folder/root" }))
            .and_then(|url| url.error.as_deref()),
        Some("boom")
    );
}

#[test]
fn url_level_error_survives_later_core_persist() {
    let mut app = test_app();
    let errored_url = "https://mega.nz/folder/error".to_string();
    let other_url = "https://mega.nz/folder/other".to_string();

    app.submit_url(errored_url.clone());
    app.handle_download_event(crate::tui::event::DownloadEvent::ScopeError {
        scope: errored_url.clone(),
        error: "bad folder".to_string(),
    });

    assert_eq!(
        app.session
            .as_ref()
            .and_then(|session| session.urls.iter().find(|entry| entry.url == errored_url))
            .and_then(|entry| entry.error.as_deref()),
        Some("bad folder")
    );

    app.submit_url(other_url);

    assert_eq!(
        app.session
            .as_ref()
            .and_then(|session| session.urls.iter().find(|entry| entry.url == errored_url))
            .and_then(|entry| entry.error.as_deref()),
        Some("bad folder")
    );
}

#[test]
fn deferred_core_persistence_materializes_once_for_nested_batches() {
    let mut app = test_app();
    crate::core::reducer::reset_snapshot_from_state_call_count();

    app.with_deferred_batch_updates(|app| {
        app.with_deferred_batch_updates(|app| {
            app.apply_core_event(CoreEvent::PackageResolved {
                package: ResolvedPackage {
                    id: package_id("pkg-a", "https://mega.nz/folder/a"),
                    source_url: "https://mega.nz/folder/a".to_string(),
                    key: crate::core::PackageKey::new("https://mega.nz/folder/a".to_string()),
                    display_name: "Package A".to_string(),
                    files: vec![ResolvedFile {
                        file_id: "episode-a.mkv".to_string().into(),
                        path: "episode-a.mkv".to_string(),
                        size: 128,
                    }],
                    collision: None,
                },
            });
            app.apply_core_event(CoreEvent::PackageResolved {
                package: ResolvedPackage {
                    id: package_id("pkg-b", "https://mega.nz/folder/b"),
                    source_url: "https://mega.nz/folder/b".to_string(),
                    key: crate::core::PackageKey::new("https://mega.nz/folder/b".to_string()),
                    display_name: "Package B".to_string(),
                    files: vec![ResolvedFile {
                        file_id: "episode-b.mkv".to_string().into(),
                        path: "episode-b.mkv".to_string(),
                        size: 256,
                    }],
                    collision: None,
                },
            });
        });
    });

    app.flush_session_persistence();

    assert_eq!(app.session_persist_count, 1);
    assert_eq!(crate::core::reducer::snapshot_from_state_call_count(), 1);
    assert_eq!(app.core_state.files.len(), 2);
    assert_eq!(app.core_state.packages.len(), 2);
}

#[test]
fn deferred_visible_sync_does_not_rebuild_visible_rows_during_batch_selection_capture() {
    let mut app = test_app();
    resolve_package(
        &mut app,
        "https://mega.nz/folder/existing",
        &[("existing.mkv", 128)],
    );
    app.file_list_state.select(Some(0));
    crate::tui::visible::reset_visible_rows_for_call_count();

    app.with_deferred_batch_updates(|app| {
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id("pkg-a", "https://mega.nz/folder/a"),
                source_url: "https://mega.nz/folder/a".to_string(),
                key: crate::core::PackageKey::new("https://mega.nz/folder/a".to_string()),
                display_name: "Package A".to_string(),
                files: vec![ResolvedFile {
                    file_id: "episode-a.mkv".to_string().into(),
                    path: "episode-a.mkv".to_string(),
                    size: 128,
                }],
                collision: None,
            },
        });
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id("pkg-b", "https://mega.nz/folder/b"),
                source_url: "https://mega.nz/folder/b".to_string(),
                key: crate::core::PackageKey::new("https://mega.nz/folder/b".to_string()),
                display_name: "Package B".to_string(),
                files: vec![ResolvedFile {
                    file_id: "episode-b.mkv".to_string().into(),
                    path: "episode-b.mkv".to_string(),
                    size: 256,
                }],
                collision: None,
            },
        });
    });

    assert_eq!(crate::tui::visible::visible_rows_for_call_count(), 1);
}

#[test]
fn sync_visible_files_reuses_overlay_sort_keys_for_unchanged_rows() {
    let mut app = test_app();
    app.upsert_overlay_file(
        FileEntry {
            id: "https://mega.nz/folder/root".to_string().into(),
            name: "https://mega.nz/folder/root".to_string(),
            size: 0,
            downloaded: 0,
            status: FileStatus::Queued,
        },
        Some("https://mega.nz/folder/root".to_string()),
    );
    crate::tui::visible::reset_build_file_sort_key_call_count();

    app.sync_visible_files();

    assert_eq!(crate::tui::visible::build_file_sort_key_call_count(), 0);
}

#[test]
fn sync_visible_files_reuses_core_sort_keys_for_unchanged_rows() {
    let mut app = test_app();
    resolve_package(
        &mut app,
        "https://mega.nz/folder/root",
        &[("episode-1.mkv", 128)],
    );
    crate::tui::visible::reset_build_file_sort_key_call_count();

    let _ = app.mutate_session_and_save(|session| {
        session.status = SessionRunStatus::Paused;
    });
    app.sync_visible_files();

    assert_eq!(crate::tui::visible::build_file_sort_key_call_count(), 0);
}

#[test]
fn deferred_core_persistence_skips_snapshot_for_non_persist_events() {
    let mut app = test_app();
    resolve_package(
        &mut app,
        "https://mega.nz/folder/root",
        &[("episode-1.mkv", 128)],
    );
    app.session_persist_count = 0;
    crate::core::reducer::reset_snapshot_from_state_call_count();

    app.with_deferred_batch_updates(|app| {
        app.apply_core_event(CoreEvent::FileQueued {
            file_id: "episode-1.mkv".to_string().into(),
        });
        app.apply_core_event(CoreEvent::FileStarted {
            file_id: "episode-1.mkv".to_string().into(),
            size: 128,
        });
    });

    assert_eq!(app.session_persist_count, 0);
    assert_eq!(crate::core::reducer::snapshot_from_state_call_count(), 0);
    assert!(matches!(
        app.core_state.files["episode-1.mkv"].lifecycle,
        FileLifecycle::Downloading
    ));
}

#[test]
fn deferred_core_persistence_materializes_once_for_multiple_session_mutations() {
    let mut app = test_app();
    crate::core::reducer::reset_snapshot_from_state_call_count();

    app.with_deferred_batch_updates(|app| {
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id("pkg", "https://mega.nz/folder/root"),
                source_url: "https://mega.nz/folder/root".to_string(),
                key: crate::core::PackageKey::new("https://mega.nz/folder/root".to_string()),
                display_name: "Root".to_string(),
                files: vec![ResolvedFile {
                    file_id: "episode-1.mkv".to_string().into(),
                    path: "episode-1.mkv".to_string(),
                    size: 128,
                }],
                collision: None,
            },
        });
        let _ = app.mutate_session_and_save(|session| {
            session.urls[0].error = Some("boom".to_string());
        });
        let _ = app.mutate_session_and_save(|session| {
            session.packages[0].display_name = "Renamed".to_string();
        });
    });

    app.flush_session_persistence();

    assert_eq!(app.session_persist_count, 1);
    assert_eq!(crate::core::reducer::snapshot_from_state_call_count(), 1);
    let session = app.session.as_ref().expect("session should exist");
    assert_eq!(session.urls[0].error.as_deref(), Some("boom"));
    assert_eq!(session.packages[0].display_name, "Renamed");
}

#[test]
fn deferred_core_persistence_allows_session_remove_to_override_batch_snapshot() {
    let mut app = test_app();
    crate::core::reducer::reset_snapshot_from_state_call_count();

    app.with_deferred_batch_updates(|app| {
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id("pkg", "https://mega.nz/folder/root"),
                source_url: "https://mega.nz/folder/root".to_string(),
                key: crate::core::PackageKey::new("https://mega.nz/folder/root".to_string()),
                display_name: "Root".to_string(),
                files: vec![ResolvedFile {
                    file_id: "episode-1.mkv".to_string().into(),
                    path: "episode-1.mkv".to_string(),
                    size: 128,
                }],
                collision: None,
            },
        });
        let _ = app.mutate_session_and_save(|session| {
            session.urls.clear();
            session.packages.clear();
        });
    });

    app.flush_session_persistence();

    assert_eq!(app.session_persist_count, 1);
    assert_eq!(crate::core::reducer::snapshot_from_state_call_count(), 1);
    let session = app.session.as_ref().expect("session should exist");
    assert!(session.urls.is_empty());
    assert!(session.packages.is_empty());
}

#[test]
fn pending_order_sync_is_immediate_outside_batch() {
    let mut app = test_app();
    let (url_tx, mut url_rx) = mpsc::unbounded_channel();
    app.url_tx = url_tx;
    app.download_task_running = true;
    crate::core::model::reset_pending_file_ids_call_count();

    resolve_package(
        &mut app,
        "https://mega.nz/folder/root",
        &[("episode-1.mkv", 128)],
    );

    assert_eq!(crate::core::model::pending_file_ids_call_count(), 1);
    assert_eq!(
        url_rx.try_recv().unwrap(),
        crate::tui::event::DownloadRequest::SyncPendingOrder {
            file_ids: vec!["episode-1.mkv".to_string().into()],
        }
    );
    assert!(url_rx.try_recv().is_err());
}

#[test]
fn pending_order_sync_collapses_for_nested_batches() {
    let mut app = test_app();
    let (url_tx, mut url_rx) = mpsc::unbounded_channel();
    app.url_tx = url_tx;
    app.download_task_running = true;
    crate::core::model::reset_pending_file_ids_call_count();

    app.with_deferred_batch_updates(|app| {
        app.with_deferred_batch_updates(|app| {
            app.apply_core_event(CoreEvent::PackageResolved {
                package: ResolvedPackage {
                    id: package_id("pkg-a", "https://mega.nz/folder/a"),
                    source_url: "https://mega.nz/folder/a".to_string(),
                    key: crate::core::PackageKey::new("https://mega.nz/folder/a".to_string()),
                    display_name: "Package A".to_string(),
                    files: vec![ResolvedFile {
                        file_id: "episode-a.mkv".to_string().into(),
                        path: "episode-a.mkv".to_string(),
                        size: 128,
                    }],
                    collision: None,
                },
            });
            app.apply_core_event(CoreEvent::PackageResolved {
                package: ResolvedPackage {
                    id: package_id("pkg-b", "https://mega.nz/folder/b"),
                    source_url: "https://mega.nz/folder/b".to_string(),
                    key: crate::core::PackageKey::new("https://mega.nz/folder/b".to_string()),
                    display_name: "Package B".to_string(),
                    files: vec![ResolvedFile {
                        file_id: "episode-b.mkv".to_string().into(),
                        path: "episode-b.mkv".to_string(),
                        size: 256,
                    }],
                    collision: None,
                },
            });
        });
    });

    assert_eq!(crate::core::model::pending_file_ids_call_count(), 1);
    assert_eq!(
        url_rx.try_recv().unwrap(),
        crate::tui::event::DownloadRequest::SyncPendingOrder {
            file_ids: vec![
                "episode-a.mkv".to_string().into(),
                "episode-b.mkv".to_string().into(),
            ],
        }
    );
    assert!(url_rx.try_recv().is_err());
}

#[test]
fn pending_order_sync_skips_noop_batches() {
    let mut app = test_app();
    let (url_tx, mut url_rx) = mpsc::unbounded_channel();
    app.url_tx = url_tx;
    app.download_task_running = true;
    resolve_package(
        &mut app,
        "https://mega.nz/folder/root",
        &[("episode-1.mkv", 128)],
    );
    assert!(matches!(
        url_rx.try_recv().unwrap(),
        crate::tui::event::DownloadRequest::SyncPendingOrder { .. }
    ));
    crate::core::model::reset_pending_file_ids_call_count();

    app.with_deferred_batch_updates(|app| {
        app.apply_core_event(CoreEvent::FileStarted {
            file_id: "episode-1.mkv".to_string().into(),
            size: 128,
        });
    });

    assert_eq!(crate::core::model::pending_file_ids_call_count(), 0);
    assert!(url_rx.try_recv().is_err());
}

#[test]
fn download_status_message_reflects_actual_activity() {
    let mut app = test_app();

    app.upsert_overlay_file(
        FileEntry {
            id: "episode-1.mkv".to_string().into(),
            name: "episode-1.mkv".to_string(),
            size: 128,
            downloaded: 0,
            status: FileStatus::Queued,
        },
        Some("https://mega.nz/folder/root".to_string()),
    );
    app.recompute_totals();
    app.update_download_status_message();

    assert_eq!(app.status, "");

    app.overlay_file_mut(&"episode-1.mkv".into())
        .unwrap()
        .status = FileStatus::Downloading;
    app.sync_visible_files();
    app.update_download_status_message();

    assert_eq!(app.status, "");
}

#[test]
fn download_status_message_uses_core_downloading_totals() {
    let mut app = test_app();

    resolve_package(
        &mut app,
        "https://mega.nz/folder/root",
        &[("episode-1.mkv", 128)],
    );
    app.apply_core_event(CoreEvent::FileStarted {
        file_id: "episode-1.mkv".to_string().into(),
        size: 128,
    });
    app.files.clear();

    app.update_download_status_message();

    assert_eq!(app.status, "Downloading (0/1)");
}

#[test]
fn login_failure_status_adds_single_context_prefix() {
    let mut app = test_app();

    app.complete_login(
        false,
        Some("invalid RSA private key format".to_string()),
        None,
        false,
    );

    assert_eq!(
        app.login.error.as_deref(),
        Some("invalid RSA private key format")
    );
    assert_eq!(app.status, "Login failed: invalid RSA private key format");
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
            progress: crate::core::model::PackageProgressState::default(),
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
                file_id: "ghost.bin".to_string().into(),
                path: "ghost.bin".to_string(),
                size: 1,
            }],
            collision: None,
        },
    });
    app.apply_core_event(CoreEvent::FileDeleted {
        file_id: "ghost.bin".to_string().into(),
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
                file_id: "good.bin".to_string().into(),
                path: "good.bin".to_string(),
                size: 1,
            }],
            collision: None,
        },
    });
    app.submit_url("https://mega.nz/folder/bad".to_string());

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
        file_id: "https://mega.nz/folder/bad".to_string().into(),
    }));
}

#[test]
fn url_level_overlay_error_does_not_also_render_empty_package_row() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let mut app = test_app();
    let url = "https://mega.nz/folder/bad".to_string();
    app.apply_core_event(CoreEvent::UrlSubmitted { url: url.clone() });

    app.handle_download_event(crate::tui::event::DownloadEvent::ScopeError {
        scope: url.clone(),
        error: "bad folder".to_string(),
    });

    assert_eq!(
        app.visible_rows(),
        vec![TuiRow::File {
            package_id: None,
            file_id: url.into(),
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
    app.handle_ui_action(UiAction::DeleteFile(url.clone().into()));

    assert!(app.visible_rows().is_empty());
    assert!(!app.tracked_urls().contains(&url));
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
fn shutdown_sync_refreshes_session_progress_skipped_during_hot_events() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let mut app = test_app();
    let url = "https://mega.nz/file/root".to_string();
    let mut session = session_snapshot(vec![(url.as_str(), UrlFixtureStatus::Fetched)]);
    push_file(&mut session, 0, "file-id", 128, FileFixtureStatus::Pending);
    app.session = Some(session);
    app.ensure_core_file(
        &"file-id".into(),
        &url,
        "file-id",
        128,
        crate::core::FileAccounting::CurrentRun,
    );

    app.apply_core_event(CoreEvent::FileStarted {
        file_id: "file-id".to_string().into(),
        size: 128,
    });
    app.apply_core_event(CoreEvent::FileProgress {
        file_id: "file-id".to_string().into(),
        total_bytes_delta: 64,
        network_bytes_delta: 64,
    });

    let session = app.session.as_ref().expect("session should remain");
    let file = session
        .iter_files()
        .find(|file| file.id == "file-id")
        .expect("file should exist in session");
    assert_eq!(file.progress.visible_completed_bytes, 0);

    app.sync_session_for_shutdown();

    let session = app.session.as_ref().expect("session should remain");
    let file = session
        .iter_files()
        .find(|file| file.id == "file-id")
        .expect("file should exist in session");
    assert_eq!(file.progress.visible_completed_bytes, 64);
    assert_eq!(file.progress.downloaded_network_bytes, 64);
}

#[test]
fn downloading_file_can_reach_full_progress_before_complete_event() {
    let mut app = test_app();
    let url = "https://mega.nz/file/root";
    app.ensure_core_file(
        &"file-id".into(),
        url,
        "file-id",
        100,
        crate::core::FileAccounting::CurrentRun,
    );

    app.handle_download_event(crate::tui::event::DownloadEvent::FileStart {
        id: "file-id".to_string().into(),
        size: 100,
        attempt_id: 0,
    });
    app.handle_download_event(crate::tui::event::DownloadEvent::Progress {
        id: "file-id".to_string().into(),
        delta: crate::core::ProgressDelta {
            total_bytes_delta: 100,
            network_bytes_delta: 100,
        },
        attempt_id: 0,
    });

    let file = app
        .visible_file(&"file-id".into())
        .expect("file should be visible");
    assert_eq!(file.downloaded, 100);
    assert_eq!(file.status, FileStatus::Downloading);
    assert_eq!(app.total_downloaded, 100);
    assert_eq!(app.total_size, 100);

    app.handle_download_event(crate::tui::event::DownloadEvent::FileComplete {
        id: "file-id".to_string().into(),
        attempt_id: 0,
    });

    let file = app
        .visible_file(&"file-id".into())
        .expect("file should be visible");
    assert_eq!(file.downloaded, 100);
    assert_eq!(file.status, FileStatus::Complete);
    assert_eq!(app.total_downloaded, 100);
}

#[test]
fn stale_start_does_not_demote_completed_file() {
    let mut app = test_app();
    let url = "https://mega.nz/file/root";
    app.ensure_core_file(
        &"file-id".into(),
        url,
        "file-id",
        100,
        crate::core::FileAccounting::CurrentRun,
    );
    app.apply_core_event(CoreEvent::FileCompleted {
        file_id: "file-id".to_string().into(),
    });
    app.sync_visible_files();

    app.handle_download_event(crate::tui::event::DownloadEvent::FileStart {
        id: "file-id".to_string().into(),
        size: 100,
        attempt_id: 0,
    });

    let file = app
        .visible_file(&"file-id".into())
        .expect("file should be visible");
    assert_eq!(file.downloaded, 100);
    assert_eq!(file.status, FileStatus::Complete);
    assert_eq!(app.total_downloaded, 100);
}

#[test]
fn restarting_partial_file_preserves_visible_progress_before_new_deltas() {
    let mut app = test_app();
    let url = "https://mega.nz/file/root".to_string();
    let mut session = session_snapshot(vec![(&url, UrlFixtureStatus::Pending)]);
    push_file(&mut session, 0, "file-id", 100, FileFixtureStatus::Pending);
    let restart = reconcile_restart(
        Some(session.clone()),
        FilesystemSnapshot {
            complete_files: Vec::new(),
            partial_files: vec![PartialFileSnapshot {
                file_id: "file-id".to_string().into(),
                bytes: 45,
                has_sidecar: false,
                verified_bytes: 0,
            }],
        },
        vec![url],
    );
    app.resume_from_restart(session, &restart);

    let file = app
        .visible_file(&"file-id".into())
        .expect("file should be visible");
    assert_eq!(file.downloaded, 45);
    assert_eq!(file.status, FileStatus::Queued);

    app.handle_download_event(crate::tui::event::DownloadEvent::FileStart {
        id: "file-id".to_string().into(),
        size: 100,
        attempt_id: 0,
    });

    let file = app
        .visible_file(&"file-id".into())
        .expect("file should be visible");
    assert_eq!(file.downloaded, 45);
    assert_eq!(file.status, FileStatus::Downloading);
    assert_eq!(app.total_downloaded, 45);
}

#[test]
fn resume_reuse_then_progress_keeps_file_bandwidth_on_fresh_bytes_only() {
    let mut app = test_app();
    let url = "https://mega.nz/file/root";
    app.ensure_core_file(
        &"file-id".into(),
        url,
        "file-id",
        100,
        crate::core::FileAccounting::CurrentRun,
    );

    app.handle_download_event(crate::tui::event::DownloadEvent::FileStart {
        id: "file-id".to_string().into(),
        size: 100,
        attempt_id: 0,
    });
    app.handle_download_event(crate::tui::event::DownloadEvent::ResumeReused {
        id: "file-id".to_string().into(),
        chunks: 1,
        bytes: 60,
        attempt_id: 0,
    });
    app.handle_download_event(crate::tui::event::DownloadEvent::Progress {
        id: "file-id".to_string().into(),
        delta: crate::core::ProgressDelta {
            total_bytes_delta: 25,
            network_bytes_delta: 25,
        },
        attempt_id: 0,
    });

    let file_id = crate::core::FileId::from("file-id");
    let core_file = app
        .core_state
        .files
        .get(&file_id)
        .expect("core file should exist");
    assert_eq!(core_file.progress.visible_completed_bytes, 85);
    assert_eq!(core_file.progress.verified_existing_bytes, 60);
    assert_eq!(core_file.progress.downloaded_network_bytes, 25);
    assert_eq!(app.total_downloaded, 85);
    assert_eq!(app.total_network_downloaded, 25);
}

#[test]
fn fast_trusted_resume_progress_is_not_double_counted_when_reuse_event_arrives() {
    let mut app = test_app();
    let url = "https://mega.nz/file/root";
    let file_id = crate::core::FileId::from("file-id");
    app.ensure_core_file(
        &file_id,
        url,
        "file-id",
        100,
        crate::core::FileAccounting::CurrentRun,
    );

    app.handle_download_event(crate::tui::event::DownloadEvent::FileStart {
        id: file_id.clone(),
        size: 100,
        attempt_id: 0,
    });
    app.handle_download_event(crate::tui::event::DownloadEvent::Progress {
        id: file_id.clone(),
        delta: crate::core::ProgressDelta {
            total_bytes_delta: 60,
            network_bytes_delta: 0,
        },
        attempt_id: 0,
    });
    app.handle_download_event(crate::tui::event::DownloadEvent::ResumeReused {
        id: file_id.clone(),
        chunks: 1,
        bytes: 60,
        attempt_id: 0,
    });

    let core_file = app
        .core_state
        .files
        .get(&file_id)
        .expect("core file should exist");
    assert_eq!(core_file.progress.visible_completed_bytes, 60);
    assert_eq!(core_file.progress.verified_existing_bytes, 60);
    assert_eq!(core_file.progress.downloaded_network_bytes, 0);
    assert_eq!(app.total_downloaded, 60);
    assert_eq!(app.total_network_downloaded, 0);
}

#[test]
fn resume_validation_progress_transitions_to_download_progress_on_network_bytes() {
    let mut app = test_app();
    let url = "https://mega.nz/file/root";
    let file_id = crate::core::FileId::from("file-id");
    app.ensure_core_file(
        &file_id,
        url,
        "file-id",
        100,
        crate::core::FileAccounting::CurrentRun,
    );

    app.handle_download_event(crate::tui::event::DownloadEvent::FileStart {
        id: file_id.clone(),
        size: 100,
        attempt_id: 0,
    });
    app.handle_download_event(crate::tui::event::DownloadEvent::ResumeValidationStarted {
        id: file_id.clone(),
        attempt_id: 0,
    });
    app.handle_download_event(crate::tui::event::DownloadEvent::VerificationProgress {
        id: file_id.clone(),
        bytes_delta: 40,
    });

    let file = app
        .visible_file(&file_id)
        .expect("file should be visible during repair");
    assert_eq!(file.downloaded, 40);
    assert!(app.verifying_files.contains(&file_id));
    assert!(app.verification_targets.contains_key(&file_id));

    app.handle_download_event(crate::tui::event::DownloadEvent::Progress {
        id: file_id.clone(),
        delta: crate::core::ProgressDelta {
            total_bytes_delta: 15,
            network_bytes_delta: 15,
        },
        attempt_id: 0,
    });

    let file = app
        .visible_file(&file_id)
        .expect("file should remain visible");
    assert_eq!(file.downloaded, 55);
    assert_eq!(file.status, FileStatus::Downloading);
    assert!(!app.verifying_files.contains(&file_id));
    assert!(!app.verification_inflight_files.contains(&file_id));
    assert!(!app.verification_targets.contains_key(&file_id));
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
    assert_eq!(session.file_count(), 2);
    assert!(
        session.iter_files().any(|file| file.path == "keep.bin"
            && matches!(file.lifecycle, crate::core::FileLifecycle::Complete)
            && file.size == 5),
        "matching files should be replaced by the newer snapshot"
    );
    assert!(!session.iter_files().any(|file| file.path == "stale.bin"));
    assert!(session.iter_files().any(|file| file.path == "new.bin"));
}

#[test]
fn session_adapter_replace_state_replaces_stale_package_rows() {
    let mut session = session_snapshot(vec![("https://mega.nz/file/a", UrlFixtureStatus::Pending)]);
    session.packages.push(crate::core::PackageSnapshot {
        id: package_id("batch-stale", "https://mega.nz/file/a"),
        key: crate::core::PackageKey::new("https://mega.nz/file/a".to_string().clone()),
        display_name: "Stale Batch".to_string(),
        files: Vec::new(),
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
        files: Vec::new(),
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
        session.packages[0]
            .files
            .iter()
            .map(|file| file.id.clone())
            .collect::<Vec<_>>(),
        vec![crate::core::FileId::from("episode-1.mkv")]
    );
    assert_eq!(session.file_count(), 1);
    assert_eq!(
        session.find_file("episode-1.mkv").unwrap().package_id,
        session.packages[0].id
    );
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
        session.prune_empty_packages();
    });

    let session = app.session.as_ref().expect("session should remain");
    assert_eq!(
        session
            .iter_files()
            .map(|file| file.id.clone())
            .collect::<Vec<_>>(),
        vec![crate::core::FileId::from("episode-1.mkv")]
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
        session.find_file_mut("episode-1.mkv").unwrap().source_url =
            "https://mega.nz/file/other".to_string();
    });

    assert_eq!(
        app.status,
        format!(
            "Failed to save session: file {} references untracked source_url {}",
            session.find_file("episode-1.mkv").unwrap().id,
            "https://mega.nz/file/other"
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
                    file_id: "a-queued.bin".to_string().into(),
                    path: "a-queued.bin".to_string(),
                    size: 10,
                },
                ResolvedFile {
                    file_id: "a-complete.bin".to_string().into(),
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
                file_id: "b-downloading.bin".to_string().into(),
                path: "b-downloading.bin".to_string(),
                size: 10,
            }],
            collision: None,
        },
    });
    app.apply_core_event(CoreEvent::FileQueued {
        file_id: "a-queued.bin".to_string().into(),
    });
    app.apply_core_event(CoreEvent::FileCompleted {
        file_id: "a-complete.bin".to_string().into(),
    });
    app.apply_core_event(CoreEvent::FileStarted {
        file_id: "b-downloading.bin".to_string().into(),
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
fn expanded_package_orders_files_failed_downloading_queued_complete() {
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
                    file_id: "queued.bin".to_string().into(),
                    path: "queued.bin".to_string(),
                    size: 10,
                },
                ResolvedFile {
                    file_id: "complete.bin".to_string().into(),
                    path: "complete.bin".to_string(),
                    size: 10,
                },
                ResolvedFile {
                    file_id: "downloading.bin".to_string().into(),
                    path: "downloading.bin".to_string(),
                    size: 10,
                },
                ResolvedFile {
                    file_id: "error.bin".to_string().into(),
                    path: "error.bin".to_string(),
                    size: 10,
                },
            ],
            collision: None,
        },
    });
    app.expanded_packages.insert(package_id);
    app.apply_core_event(CoreEvent::FileQueued {
        file_id: "queued.bin".to_string().into(),
    });
    app.apply_core_event(CoreEvent::FileCompleted {
        file_id: "complete.bin".to_string().into(),
    });
    app.apply_core_event(CoreEvent::FileStarted {
        file_id: "downloading.bin".to_string().into(),
        size: 10,
    });
    app.apply_core_event(CoreEvent::FileFailed {
        file_id: "error.bin".to_string().into(),
        message: "boom".to_string(),
    });

    assert_eq!(
        app.visible_rows(),
        vec![
            TuiRow::Package(package_id),
            TuiRow::File {
                package_id: Some(package_id),
                file_id: "error.bin".to_string().into(),
            },
            TuiRow::File {
                package_id: Some(package_id),
                file_id: "downloading.bin".to_string().into(),
            },
            TuiRow::File {
                package_id: Some(package_id),
                file_id: "queued.bin".to_string().into(),
            },
            TuiRow::File {
                package_id: Some(package_id),
                file_id: "complete.bin".to_string().into(),
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
                file_id: "episode.bin".to_string().into(),
                path: "episode.bin".to_string(),
                size: 128,
            }],
            collision: None,
        },
    });
    app.apply_core_event(CoreEvent::FileStarted {
        file_id: "episode.bin".to_string().into(),
        size: 128,
    });
    let token = CancellationToken::new();
    app.cancellation_tokens
        .insert("episode.bin".to_string().into(), token.clone());

    app.pause_downloads();

    assert!(app.paused);
    assert!(token.is_cancelled());
    assert!(!app.cancellation_tokens.contains_key("episode.bin"));
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
                file_id: "kept.bin".to_string().into(),
                path: "kept.bin".to_string(),
                size: 128,
            }],
            collision: None,
        },
    });
    app.file_ui.insert(
        "kept.bin".to_string().into(),
        FileUiState {
            speed: 42,
            rate: Default::default(),
            sort_key: None,
            package_id: None,
        },
    );
    app.file_ui.insert(
        "stale.bin".to_string().into(),
        FileUiState {
            speed: 99,
            rate: Default::default(),
            sort_key: None,
            package_id: None,
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
                    file_id: "episode-1.bin".to_string().into(),
                    path: "episode-1.bin".to_string(),
                    size: 128,
                },
                ResolvedFile {
                    file_id: "episode-2.bin".to_string().into(),
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
        file_id: "episode-1.bin".to_string().into(),
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

#[test]
fn drain_download_events_collapses_visible_syncs_for_batched_files() {
    let mut app = test_app();
    crate::core::reducer::reset_snapshot_from_state_call_count();
    crate::core::model::reset_pending_file_ids_call_count();
    let (url_tx, mut url_rx) = mpsc::unbounded_channel();
    app.url_tx = url_tx;
    app.download_task_running = true;
    app.core_state
        .url_order
        .push("https://mega.nz/folder/resolved".to_string());
    let (_download_tx, mut download_rx) = mpsc::unbounded_channel();
    for (name, size) in [
        ("episode-1.mkv", 128),
        ("episode-2.mkv", 256),
        ("episode-3.mkv", 512),
    ] {
        _download_tx
            .send(DownloadEvent::FileQueued(QueuedFile {
                id: name.to_string().into(),
                size,
                accounting: crate::core::FileAccounting::CurrentRun,
                origin: crate::tui::event::FileOrigin {
                    package_id: Some(package_id("pkg", "Package")),
                    package_display_name: Some("Package".to_string()),
                    source_url: "https://mega.nz/folder/resolved".to_string(),
                    submitted_url: "https://mega.nz/folder/resolved".to_string(),
                },
            }))
            .expect("download event should send");
    }

    assert!(app.drain_download_events(&mut download_rx));

    app.flush_session_persistence();

    assert_eq!(app.visible_sync_count, 1);
    assert_eq!(app.session_persist_count, 1);
    assert_eq!(crate::core::reducer::snapshot_from_state_call_count(), 1);
    assert_eq!(crate::core::model::pending_file_ids_call_count(), 1);
    assert_eq!(
        url_rx.try_recv().unwrap(),
        crate::tui::event::DownloadRequest::SyncPendingOrder {
            file_ids: vec![
                "episode-1.mkv".to_string().into(),
                "episode-2.mkv".to_string().into(),
                "episode-3.mkv".to_string().into(),
            ],
        }
    );
    assert!(url_rx.try_recv().is_err());
    assert_eq!(app.core_state.files.len(), 3);
}
