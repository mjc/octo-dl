use super::super::app::{App, ConfirmAction, FileEntry, FileStatus, Popup, QuitPolicy, SortKey};
use super::*;
use crate::core::{CoreEvent, PackageCollision, ResolvedFile, ResolvedPackage};
use crate::test_support::{
    FileFixtureStatus, StateDirectoryGuard, UrlFixtureStatus, package_id, push_file,
    session_snapshot,
};
use crate::tui::event::DownloadRequest;
use crate::tui::visible::TuiRow;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use tempfile::tempdir;
use tokio::sync::mpsc;

fn test_app() -> App {
    let path = tempdir()
        .expect("test state directory should exist")
        .keep();
    std::mem::forget(StateDirectoryGuard::set(&path));
    let (tx, _rx) = mpsc::unbounded_channel();
    App::new(9723, tx, true)
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

fn ctrl_key(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::CONTROL,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

fn confirm(app: &mut App) {
    assert_eq!(app.popup, Popup::Confirm);
    handle_input(app, key(KeyCode::Char('y')));
    assert_eq!(app.popup, Popup::None);
    assert_eq!(app.pending_confirmation, None);
}

#[test]
fn handle_main_input_quit() {
    let mut app = test_app();
    assert!(!app.should_quit);
    handle_input(&mut app, key(KeyCode::Char('q')));
    assert!(app.should_quit);
}

#[test]
fn handle_main_input_quit_disabled_via_flag() {
    let mut app = test_app();
    app.quit_policy = QuitPolicy::Disabled;
    assert!(!app.should_quit);
    handle_input(&mut app, key(KeyCode::Char('q')));
    assert!(!app.should_quit);
}

#[test]
fn handle_main_input_ctrl_c_matches_quit_policy() {
    let mut app = test_app();
    handle_input(&mut app, ctrl_key(KeyCode::Char('c')));
    assert!(app.should_quit);

    let mut disabled = test_app();
    disabled.quit_policy = QuitPolicy::Disabled;
    handle_input(&mut disabled, ctrl_key(KeyCode::Char('c')));
    assert!(!disabled.should_quit);
}

#[test]
fn handle_main_input_esc_quit_when_empty() {
    let mut app = test_app();
    handle_input(&mut app, key(KeyCode::Esc));
    assert!(app.should_quit);
}

#[test]
fn handle_main_input_esc_clears_url_when_nonempty() {
    let mut app = test_app();
    app.url_input = "some text".to_string();
    app.url_input_active = true;
    handle_input(&mut app, key(KeyCode::Esc));
    assert!(!app.should_quit);
    assert!(app.url_input.is_empty());
    assert!(!app.url_input_active);
}

#[test]
fn handle_main_input_typing() {
    let mut app = test_app();
    handle_input(&mut app, key(KeyCode::Char('a')));
    handle_input(&mut app, key(KeyCode::Char('h')));
    handle_input(&mut app, key(KeyCode::Char('i')));
    assert_eq!(app.url_input, "hi");
}

#[test]
fn handle_main_input_ignores_unmapped_keys_in_command_mode() {
    let mut app = test_app();
    handle_input(&mut app, key(KeyCode::Char('h')));
    assert!(app.url_input.is_empty());
    assert!(!app.should_quit);
}

#[test]
fn handle_main_input_typing_q_does_not_quit_in_url_mode() {
    let mut app = test_app();
    app.url_input = "https://example".to_string();
    app.url_input_active = true;
    handle_input(&mut app, key(KeyCode::Char('q')));
    assert!(!app.should_quit);
    assert_eq!(app.url_input, "https://exampleq");
}

#[test]
fn handle_main_input_backspace() {
    let mut app = test_app();
    app.url_input = "abc".to_string();
    app.url_input_active = true;
    handle_input(&mut app, key(KeyCode::Backspace));
    assert_eq!(app.url_input, "ab");
}

#[test]
fn handle_main_input_pause_toggle() {
    let mut app = test_app();
    assert!(!app.paused);
    handle_input(&mut app, key(KeyCode::Char('p')));
    assert!(app.paused);
    handle_input(&mut app, key(KeyCode::Char('p')));
    assert!(!app.paused);
}

#[test]
fn handle_main_input_config_popup() {
    let mut app = test_app();
    handle_input(&mut app, key(KeyCode::Char('c')));
    assert_eq!(app.popup, Popup::Config);
}

#[test]
fn handle_main_input_navigation_keys_move_selection() {
    let mut app = test_app();
    for i in 0..12 {
        app.files.push(FileEntry {
            id: format!("file-{i}").into(),
            name: format!("file-{i}"),
            size: 1,
            downloaded: 0,
            status: FileStatus::Queued,
        });
    }
    app.file_list_state.select(Some(0));

    handle_input(&mut app, key(KeyCode::Char('j')));
    assert_eq!(app.file_list_state.selected(), Some(1));

    handle_input(&mut app, key(KeyCode::Char('k')));
    assert_eq!(app.file_list_state.selected(), Some(0));

    handle_input(&mut app, key(KeyCode::PageDown));
    assert_eq!(app.file_list_state.selected(), Some(10));

    handle_input(&mut app, key(KeyCode::PageUp));
    assert_eq!(app.file_list_state.selected(), Some(0));

    handle_input(&mut app, key(KeyCode::End));
    assert_eq!(app.file_list_state.selected(), Some(11));

    handle_input(&mut app, key(KeyCode::Char('g')));
    assert_eq!(app.file_list_state.selected(), Some(0));
}

#[test]
fn handle_main_input_delete_cancels_downloading() {
    let mut app = test_app();
    let token = tokio_util::sync::CancellationToken::new();
    app.files.push(FileEntry {
        id: "test.zip".to_string().into(),
        name: "test.zip".to_string(),
        size: 1000,
        downloaded: 500,
        status: FileStatus::Downloading,
    });
    app.cancellation_tokens
        .insert("test.zip".to_string().into(), token.clone());
    app.file_list_state.select(Some(0));

    handle_input(&mut app, key(KeyCode::Char('d')));
    assert_eq!(
        app.pending_confirmation,
        Some(ConfirmAction::DeleteFile("test.zip".to_string().into()))
    );
    assert!(!token.is_cancelled());
    confirm(&mut app);
    assert!(token.is_cancelled());
    assert!(app.files.is_empty());
}

#[test]
fn handle_confirm_cancel_leaves_destructive_action_unapplied() {
    let mut app = test_app();
    app.files.push(FileEntry {
        id: "keep.bin".to_string().into(),
        name: "keep.bin".to_string(),
        size: 10,
        downloaded: 0,
        status: FileStatus::Queued,
    });
    app.file_list_state.select(Some(0));

    handle_input(&mut app, key(KeyCode::Char('d')));
    assert_eq!(
        app.pending_confirmation,
        Some(ConfirmAction::DeleteFile("keep.bin".to_string().into()))
    );

    handle_input(&mut app, key(KeyCode::Esc));

    assert_eq!(app.popup, Popup::None);
    assert_eq!(app.pending_confirmation, None);
    assert_eq!(app.files.len(), 1);
    assert_eq!(app.files[0].id, "keep.bin");
}

#[test]
fn handle_main_input_delete_core_backed_entry() {
    let mut app = test_app();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id("https://mega.nz/file/core", "https://mega.nz/file/core"),
            source_url: "https://mega.nz/file/core".to_string(),
            key: crate::core::PackageKey::new("https://mega.nz/file/core".to_string().clone()),
            display_name: "Core".to_string(),
            files: vec![ResolvedFile {
                file_id: "core.bin".to_string().into(),
                path: "core.bin".to_string(),
                size: 10,
            }],
            collision: None,
        },
    });
    app.apply_core_event(CoreEvent::FileCompleted {
        file_id: "core.bin".to_string().into(),
    });
    app.file_list_state.select(Some(0));

    handle_input(&mut app, key(KeyCode::Delete));
    assert_eq!(
        app.pending_confirmation,
        Some(ConfirmAction::DeletePackage(package_id(
            "https://mega.nz/file/core",
            "https://mega.nz/file/core"
        )))
    );
    confirm(&mut app);

    assert!(app.files.is_empty());
}

#[test]
fn handle_main_input_expands_package_and_file_action_targets_child() {
    let mut app = test_app();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id("pkg", "https://mega.nz/folder/pkg"),
            source_url: "https://mega.nz/folder/pkg".to_string(),
            key: crate::core::PackageKey::new("https://mega.nz/folder/pkg".to_string().clone()),
            display_name: "Package".to_string(),
            files: vec![
                ResolvedFile {
                    file_id: "first.bin".to_string().into(),
                    path: "first.bin".to_string(),
                    size: 10,
                },
                ResolvedFile {
                    file_id: "second.bin".to_string().into(),
                    path: "second.bin".to_string(),
                    size: 20,
                },
            ],
            collision: None,
        },
    });
    app.file_list_state.select(Some(0));
    assert_eq!(app.visible_rows().len(), 1);

    handle_input(&mut app, key(KeyCode::Enter));
    assert_eq!(app.visible_rows().len(), 3);

    handle_input(&mut app, key(KeyCode::Down));
    handle_input(&mut app, key(KeyCode::Delete));
    assert_eq!(
        app.pending_confirmation,
        Some(ConfirmAction::DeleteFile("first.bin".to_string().into()))
    );
}

#[test]
fn handle_main_input_reset_package_targets_package_row() {
    let mut app = test_app();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id("pkg", "https://mega.nz/folder/pkg"),
            source_url: "https://mega.nz/folder/pkg".to_string(),
            key: crate::core::PackageKey::new("https://mega.nz/folder/pkg".to_string().clone()),
            display_name: "Package".to_string(),
            files: vec![ResolvedFile {
                file_id: "file.bin".to_string().into(),
                path: "file.bin".to_string(),
                size: 10,
            }],
            collision: None,
        },
    });
    app.file_list_state.select(Some(0));

    handle_input(&mut app, key(KeyCode::Char('R')));

    assert_eq!(
        app.pending_confirmation,
        Some(ConfirmAction::ResetPackage(package_id(
            "pkg",
            "https://mega.nz/folder/pkg"
        )))
    );
}

#[test]
fn handle_sort_popup_selects_key_and_direction() {
    let mut app = test_app();

    handle_input(&mut app, key(KeyCode::Char('s')));
    assert_eq!(app.popup, Popup::Sort);

    handle_input(&mut app, key(KeyCode::Down));
    handle_input(&mut app, key(KeyCode::Enter));
    assert_eq!(app.sort.key, SortKey::Status);
    assert_eq!(app.popup, Popup::None);

    handle_input(&mut app, key(KeyCode::Char('s')));
    for _ in 0..(SortKey::ALL.len() - 1) {
        handle_input(&mut app, key(KeyCode::Down));
    }
    handle_input(&mut app, key(KeyCode::Char(' ')));
    assert_eq!(app.sort.direction, super::super::app::SortDirection::Desc);
}

#[test]
fn handle_sort_popup_keeps_selected_row_identity_when_order_changes() {
    let mut app = test_app();
    for (raw_package_id, display_name) in [("pkg-z", "Zulu"), ("pkg-a", "Alpha")] {
        app.apply_core_event(CoreEvent::PackageResolved {
            package: ResolvedPackage {
                id: package_id(
                    raw_package_id,
                    &format!("https://mega.nz/folder/{raw_package_id}"),
                ),
                source_url: format!("https://mega.nz/folder/{raw_package_id}"),
                key: crate::core::PackageKey::new(
                    format!("https://mega.nz/folder/{raw_package_id}").clone(),
                ),
                display_name: display_name.to_string(),
                files: vec![ResolvedFile {
                    file_id: format!("{raw_package_id}.bin").into(),
                    path: format!("{raw_package_id}.bin"),
                    size: 10,
                }],
                collision: None,
            },
        });
    }

    app.file_list_state.select(Some(0));
    assert_eq!(
        app.selected_row(),
        Some(TuiRow::Package(package_id(
            "pkg-z",
            "https://mega.nz/folder/pkg-z"
        )))
    );

    handle_input(&mut app, key(KeyCode::Char('s')));
    handle_input(&mut app, key(KeyCode::Down));
    handle_input(&mut app, key(KeyCode::Down));
    handle_input(&mut app, key(KeyCode::Enter));

    assert_eq!(app.sort.key, SortKey::Name);
    assert_eq!(
        app.selected_row(),
        Some(TuiRow::Package(package_id(
            "pkg-z",
            "https://mega.nz/folder/pkg-z"
        )))
    );
    assert_eq!(app.file_list_state.selected(), Some(1));
}

#[test]
fn handle_main_input_delete_removes_session_entry_and_keeps_selection() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let mut app = test_app();
    app.files = vec![
        FileEntry {
            id: "first.bin".to_string().into(),
            name: "first.bin".to_string(),
            size: 10,
            downloaded: 0,
            status: FileStatus::Queued,
        },
        FileEntry {
            id: "second.bin".to_string().into(),
            name: "second.bin".to_string(),
            size: 20,
            downloaded: 0,
            status: FileStatus::Queued,
        },
    ];
    app.recompute_totals();
    app.file_list_state.select(Some(0));

    let mut session = session_snapshot(vec![(
        "https://mega.nz/folder/root",
        UrlFixtureStatus::Fetched,
    )]);
    push_file(&mut session, 0, "first.bin", 10, FileFixtureStatus::Pending);
    push_file(
        &mut session,
        0,
        "second.bin",
        20,
        FileFixtureStatus::Pending,
    );
    let session_path = session.state_path();
    app.session = Some(session);

    handle_input(&mut app, key(KeyCode::Delete));
    assert_eq!(
        app.pending_confirmation,
        Some(ConfirmAction::DeleteFile("first.bin".to_string().into()))
    );
    confirm(&mut app);

    assert_eq!(app.files.len(), 1);
    assert_eq!(app.files[0].id, "second.bin");
    assert_eq!(app.file_list_state.selected(), Some(0));
    assert_eq!(app.total_size, 20);
    assert!(app.deleted_files.contains("first.bin"));
    assert!(session_path.exists());

    let session = app.session.as_ref().expect("session should remain");
    let statuses: Vec<_> = session
        .files
        .iter()
        .map(|file| (file.path.as_str(), &file.lifecycle))
        .collect();
    assert_eq!(
        statuses,
        vec![
            ("first.bin", &crate::core::FileLifecycle::Skipped),
            ("second.bin", &crate::core::FileLifecycle::Queued),
        ]
    );
}

#[test]
fn handle_main_input_delete_uses_visible_sorted_row() {
    let mut app = test_app();
    app.files = vec![
        FileEntry {
            id: "complete.bin".to_string().into(),
            name: "complete.bin".to_string(),
            size: 10,
            downloaded: 10,
            status: FileStatus::Complete,
        },
        FileEntry {
            id: "active.bin".to_string().into(),
            name: "active.bin".to_string(),
            size: 20,
            downloaded: 5,
            status: FileStatus::Downloading,
        },
    ];
    app.file_list_state.select(Some(0));

    handle_input(&mut app, key(KeyCode::Delete));
    assert_eq!(
        app.pending_confirmation,
        Some(ConfirmAction::DeleteFile("active.bin".to_string().into()))
    );
    confirm(&mut app);

    assert_eq!(app.files.len(), 1);
    assert_eq!(app.files[0].id, "complete.bin");
    assert_eq!(app.file_list_state.selected(), Some(0));
}

#[test]
fn handle_main_input_delete_removes_failed_file() {
    let mut app = test_app();
    app.files.push(FileEntry {
        id: "failed.bin".to_string().into(),
        name: "failed.bin".to_string(),
        size: 10,
        downloaded: 4,
        status: FileStatus::Error("boom".to_string()),
    });
    app.file_list_state.select(Some(0));

    handle_input(&mut app, key(KeyCode::Delete));
    assert_eq!(
        app.pending_confirmation,
        Some(ConfirmAction::DeleteFile("failed.bin".to_string().into()))
    );
    confirm(&mut app);

    assert!(app.files.is_empty());
    assert!(app.deleted_files.contains("failed.bin"));
}

#[test]
fn handle_main_input_delete_does_not_surface_failed_package_without_files() {
    let mut app = test_app();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id("failed-pkg", "https://mega.nz/folder/failed"),
            source_url: "https://mega.nz/folder/failed".to_string(),
            key: crate::core::PackageKey::new("https://mega.nz/folder/failed".to_string().clone()),
            display_name: "Failed package".to_string(),
            files: Vec::new(),
            collision: Some(PackageCollision {
                file_id: "duplicate.bin".to_string().into(),
                existing_package_id: package_id("existing", "https://mega.nz/folder/failed"),
                incoming_package_id: package_id("failed-pkg", "https://mega.nz/folder/failed"),
            }),
        },
    });
    assert!(app.visible_rows().is_empty());
    assert!(!app.core_state.packages.contains_key(&package_id(
        "https://mega.nz/folder/failed",
        "https://mega.nz/folder/failed"
    )));
    handle_input(&mut app, key(KeyCode::Delete));
    assert_eq!(app.pending_confirmation, None);
}

#[test]
fn handle_main_input_shift_d_deletes_without_confirmation() {
    let mut app = test_app();
    app.files.push(FileEntry {
        id: "remove.bin".to_string().into(),
        name: "remove.bin".to_string(),
        size: 10,
        downloaded: 0,
        status: FileStatus::Queued,
    });
    app.file_list_state.select(Some(0));

    handle_input(&mut app, key(KeyCode::Char('D')));

    assert_eq!(app.popup, Popup::None);
    assert_eq!(app.pending_confirmation, None);
    assert!(app.files.is_empty());
    assert!(app.deleted_files.contains("remove.bin"));
}

#[test]
fn handle_main_input_shift_d_removes_completed_file_and_artifacts() {
    let dir = tempdir().unwrap();
    let final_path = dir.path().join("shift-delete-complete.bin");
    let final_path_string = final_path.to_string_lossy();
    let part_path = std::path::PathBuf::from(format!("{final_path_string}.part"));
    let sidecar_path = std::path::PathBuf::from(format!("{final_path_string}.part.meta.json"));
    std::fs::write(&final_path, b"complete").unwrap();
    std::fs::write(&part_path, b"partial").unwrap();
    std::fs::write(&sidecar_path, b"metadata").unwrap();

    let mut app = test_app();
    app.upsert_overlay_file(
        FileEntry {
            id: "shift-delete-complete.bin".to_string().into(),
            name: final_path.to_string_lossy().into_owned(),
            size: 100,
            downloaded: 100,
            status: FileStatus::Complete,
        },
        Some("https://mega.nz/file/shift-delete-complete".to_string()),
        false,
    );
    app.file_list_state.select(Some(0));

    handle_input(&mut app, key(KeyCode::Char('D')));

    assert_eq!(app.popup, Popup::None);
    assert_eq!(app.pending_confirmation, None);
    assert!(app.files.is_empty());
    assert!(!final_path.exists());
    assert!(!part_path.exists());
    assert!(!sidecar_path.exists());
}

#[test]
fn handle_main_input_shift_d_does_not_surface_failed_package_without_files() {
    let mut app = test_app();
    app.apply_core_event(CoreEvent::PackageResolved {
        package: ResolvedPackage {
            id: package_id("failed-pkg", "https://mega.nz/folder/failed"),
            source_url: "https://mega.nz/folder/failed".to_string(),
            key: crate::core::PackageKey::new("https://mega.nz/folder/failed".to_string().clone()),
            display_name: "Failed package".to_string(),
            files: Vec::new(),
            collision: Some(PackageCollision {
                file_id: "duplicate.bin".to_string().into(),
                existing_package_id: package_id("existing", "https://mega.nz/folder/failed"),
                incoming_package_id: package_id("failed-pkg", "https://mega.nz/folder/failed"),
            }),
        },
    });
    assert!(app.visible_rows().is_empty());
    assert!(!app.core_state.packages.contains_key(&package_id(
        "https://mega.nz/folder/failed",
        "https://mega.nz/folder/failed"
    )));
    handle_input(&mut app, key(KeyCode::Char('D')));
    assert!(app.visible_rows().is_empty());
}

#[test]
fn handle_main_input_shift_r_resets_selected_file_from_scratch() {
    let dir = tempdir().unwrap();
    let final_path = dir.path().join("active.bin");
    let final_path_string = final_path.to_string_lossy();
    let part_path = std::path::PathBuf::from(format!("{final_path_string}.part"));
    let sidecar_path = std::path::PathBuf::from(format!("{final_path_string}.part.meta.json"));
    std::fs::write(&final_path, b"complete").unwrap();
    std::fs::write(&part_path, b"partial").unwrap();
    std::fs::write(&sidecar_path, b"metadata").unwrap();

    let mut app = test_app();
    let (url_tx, mut url_rx) = mpsc::unbounded_channel();
    app.url_tx = url_tx;
    let token = tokio_util::sync::CancellationToken::new();
    app.upsert_overlay_file(
        FileEntry {
            id: "active.bin".to_string().into(),
            name: final_path.to_string_lossy().into_owned(),
            size: 100,
            downloaded: 80,
            status: FileStatus::Downloading,
        },
        Some("https://mega.nz/file/reset".to_string()),
        true,
    );
    app.cancellation_tokens
        .insert("active.bin".to_string().into(), token.clone());
    app.file_list_state.select(Some(0));

    handle_input(&mut app, key(KeyCode::Char('R')));
    assert_eq!(
        app.pending_confirmation,
        Some(ConfirmAction::ResetFile("active.bin".to_string().into()))
    );
    assert!(!token.is_cancelled());
    confirm(&mut app);

    assert!(token.is_cancelled());
    assert_eq!(app.files[0].status, FileStatus::Queued);
    assert_eq!(app.files[0].downloaded, 0);
    assert_eq!(app.file_speed(&"active.bin".into()), 0);
    assert_eq!(
        url_rx.try_recv().unwrap(),
        DownloadRequest::ResumeFileIds {
            source_url: "https://mega.nz/file/reset".to_string(),
            file_ids: vec!["active.bin".to_string().into()],
            attempt_ids: std::collections::HashMap::from([("active.bin".to_string().into(), 1)]),
        }
    );
    assert!(!final_path.exists());
    assert!(!part_path.exists());
    assert!(!sidecar_path.exists());
}

#[test]
fn handle_main_input_delete_removes_completed_file_and_artifacts() {
    let dir = tempdir().unwrap();
    let final_path = dir.path().join("complete.bin");
    let final_path_string = final_path.to_string_lossy();
    let part_path = std::path::PathBuf::from(format!("{final_path_string}.part"));
    let sidecar_path = std::path::PathBuf::from(format!("{final_path_string}.part.meta.json"));
    std::fs::write(&final_path, b"complete").unwrap();
    std::fs::write(&part_path, b"partial").unwrap();
    std::fs::write(&sidecar_path, b"metadata").unwrap();

    let mut app = test_app();
    app.upsert_overlay_file(
        FileEntry {
            id: "complete.bin".to_string().into(),
            name: final_path.to_string_lossy().into_owned(),
            size: 100,
            downloaded: 100,
            status: FileStatus::Complete,
        },
        Some("https://mega.nz/file/complete".to_string()),
        false,
    );
    app.file_list_state.select(Some(0));

    handle_input(&mut app, key(KeyCode::Char('d')));
    assert_eq!(
        app.pending_confirmation,
        Some(ConfirmAction::DeleteFile("complete.bin".to_string().into()))
    );
    confirm(&mut app);

    assert!(app.files.is_empty());
    assert!(!final_path.exists());
    assert!(!part_path.exists());
    assert!(!sidecar_path.exists());
}

#[test]
fn handle_login_input_validates_empty() {
    let mut app = test_app();
    app.popup = Popup::Login;
    handle_input(&mut app, key(KeyCode::Enter));
    assert_eq!(
        app.login.error,
        Some("Email and password are required".to_string())
    );
}

#[test]
fn handle_login_input_tab_cycles() {
    let mut app = test_app();
    app.popup = Popup::Login;
    assert_eq!(app.login.active_field, 0);
    handle_input(&mut app, key(KeyCode::Tab));
    assert_eq!(app.login.active_field, 1);
    handle_input(&mut app, key(KeyCode::Tab));
    assert_eq!(app.login.active_field, 2);
    handle_input(&mut app, key(KeyCode::Tab));
    assert_eq!(app.login.active_field, 0);
}

#[test]
fn handle_paste_appends_to_url_input() {
    let mut app = test_app();
    handle_paste(&mut app, "https://mega.nz/file/abc");
    assert_eq!(app.url_input, "https://mega.nz/file/abc");
    assert!(app.url_input_active);
}

#[test]
fn handle_paste_replaces_newlines() {
    let mut app = test_app();
    handle_paste(&mut app, "url1\nurl2\r\nurl3");
    assert_eq!(app.url_input, "url1 url2  url3");
    assert!(app.url_input_active);
}

#[test]
fn handle_paste_login_trims() {
    let mut app = test_app();
    app.popup = Popup::Login;
    app.login.active_field = 0;
    handle_paste(&mut app, "  user@example.com  ");
    assert_eq!(app.login.email(), "user@example.com");
}

#[test]
fn add_url_deduplicates() {
    let dir = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(dir.path());
    let mut app = test_app();
    let mut url_rx = app.url_rx.take().expect("url_rx should exist");
    app.submit_url("https://mega.nz/file/abc".to_string());
    app.submit_url("https://mega.nz/file/abc".to_string());
    assert_eq!(app.urls.len(), 1);
    assert_eq!(
        url_rx.try_recv().unwrap(),
        DownloadRequest::SubmitUrl {
            url: "https://mega.nz/file/abc".to_string()
        }
    );
    assert!(url_rx.try_recv().is_err());
}

#[test]
fn retry_recomputes_totals_for_errored_file() {
    let mut app = test_app();
    let (url_tx, mut url_rx) = mpsc::unbounded_channel();
    app.url_tx = url_tx;
    app.upsert_overlay_file(
        FileEntry {
            id: "error.bin".to_string().into(),
            name: "error.bin".to_string(),
            size: 100,
            downloaded: 42,
            status: FileStatus::Error("boom".to_string()),
        },
        Some("https://mega.nz/file/error".to_string()),
        true,
    );
    app.recompute_totals();
    app.file_list_state.select(Some(0));

    assert_eq!(app.files_total, 0);
    assert_eq!(app.total_downloaded, 42);

    handle_input(&mut app, key(KeyCode::Char('r')));

    assert_eq!(app.files[0].status, FileStatus::Queued);
    assert_eq!(app.files[0].downloaded, 0);
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
fn handle_main_input_url_submit() {
    let mut app = test_app();
    let (url_tx, mut url_rx) = mpsc::unbounded_channel();
    app.url_tx = url_tx;
    app.url_input = "https://mega.nz/file/test123".to_string();
    app.url_input_active = true;

    handle_input(&mut app, key(KeyCode::Enter));

    assert!(app.url_input.is_empty());
    assert!(!app.url_input_active);
    let received = url_rx.try_recv().unwrap();
    assert_eq!(
        received,
        DownloadRequest::SubmitUrl {
            url: "https://mega.nz/file/test123".to_string()
        }
    );
}

#[test]
fn handle_main_input_empty_url_submit_sets_guidance_status() {
    let mut app = test_app();
    app.url_input = "   ".to_string();
    app.url_input_active = true;

    handle_input(&mut app, key(KeyCode::Enter));

    assert_eq!(app.status, "Enter a URL or press Esc to cancel");
    assert_eq!(app.url_input, "   ");
    assert!(app.url_input_active);
}

#[test]
fn handle_main_input_invalid_url_submit_sets_error_status() {
    let mut app = test_app();
    app.url_input = "not a mega url".to_string();
    app.url_input_active = true;

    handle_input(&mut app, key(KeyCode::Enter));

    assert_eq!(app.status, "No valid URLs found in input");
    assert_eq!(app.url_input, "not a mega url");
    assert!(app.url_input_active);
}
