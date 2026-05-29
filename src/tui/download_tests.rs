use super::super::app::{App, FileEntry, FileStatus, UiAction};
use super::super::event::{DownloadEvent, FileOrigin, QueuedFile};
use super::*;
use crate::core::{CoreEvent, ProgressDelta};
use crate::test_support::StateDirectoryGuard;
use std::collections::{HashMap, HashSet, VecDeque};
use tempfile::tempdir;
use tokio::sync::mpsc;

#[tokio::test]
async fn verification_executor_limits_parallel_work_to_four() {
    let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let max_active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let released = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release_notify = std::sync::Arc::new(tokio::sync::Notify::new());
    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let items = (0..12).collect::<Vec<_>>();

    let verification = tokio::spawn(for_each_verification_item(
        items,
        PACKAGE_REVERIFY_CONCURRENCY,
        {
            let active = std::sync::Arc::clone(&active);
            let max_active = std::sync::Arc::clone(&max_active);
            let released = std::sync::Arc::clone(&released);
            let release_notify = std::sync::Arc::clone(&release_notify);
            let entered_tx = entered_tx.clone();
            move |_| {
                let active = std::sync::Arc::clone(&active);
                let max_active = std::sync::Arc::clone(&max_active);
                let released = std::sync::Arc::clone(&released);
                let release_notify = std::sync::Arc::clone(&release_notify);
                let entered_tx = entered_tx.clone();
                async move {
                    let now = active.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    max_active.fetch_max(now, std::sync::atomic::Ordering::SeqCst);
                    entered_tx.send(()).expect("task entry should be observed");
                    while !released.load(std::sync::atomic::Ordering::SeqCst) {
                        release_notify.notified().await;
                    }
                    active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                }
            }
        },
    ));

    for _ in 0..PACKAGE_REVERIFY_CONCURRENCY {
        entered_rx
            .recv()
            .await
            .expect("concurrent verification tasks should start");
    }

    assert_eq!(
        max_active.load(std::sync::atomic::Ordering::SeqCst),
        PACKAGE_REVERIFY_CONCURRENCY
    );
    released.store(true, std::sync::atomic::Ordering::SeqCst);
    release_notify.notify_waiters();
    verification.await.unwrap();
}

#[test]
fn drain_ready_requests_collects_follow_up_verification_requests_in_order() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let first = DownloadRequest::ReverifyFileIds {
        source_url: "https://mega.nz/folder/root".to_string(),
        file_ids: vec!["resume-a.bin".into()],
    };
    let second = DownloadRequest::VerifyCompletedFileIds {
        source_url: "https://mega.nz/folder/root".to_string(),
        file_ids: vec!["complete-a.bin".into()],
    };
    let late = DownloadRequest::ReverifyFileIds {
        source_url: "https://mega.nz/folder/root".to_string(),
        file_ids: vec!["resume-b.bin".into()],
    };
    tx.send(second.clone())
        .expect("second request should queue");

    let mut pending = VecDeque::from([first.clone()]);
    drain_ready_requests(&mut pending, &mut rx);
    assert_eq!(pending, VecDeque::from([first, second.clone()]));

    let handled_first = pending
        .pop_front()
        .expect("first request should be pending");
    assert!(matches!(
        handled_first,
        DownloadRequest::ReverifyFileIds { .. }
    ));
    tx.send(late.clone()).expect("late request should queue");

    drain_ready_requests(&mut pending, &mut rx);
    assert_eq!(pending, VecDeque::from([second, late]));
    assert!(rx.try_recv().is_err());
}

fn test_app() -> App {
    let (tx, _rx) = mpsc::unbounded_channel();
    App::new(9723, tx, true)
}

mod property_tests {
    use super::*;
    use proptest::prelude::*;

    fn dedup_file_ids(values: &[u8]) -> Vec<FileId> {
        let mut seen = HashSet::new();
        let mut deduped = Vec::new();
        for value in values {
            if seen.insert(*value) {
                deduped.push(FileId::from(format!("file-{value}.bin")));
            }
        }
        deduped
    }

    fn dedup_file_id_set(values: &[u8]) -> HashSet<FileId> {
        dedup_file_ids(values).into_iter().collect()
    }

    fn expected_startable_file_ids(
        pending_queue: &VecDeque<FileId>,
        resume_priority_set: &HashSet<FileId>,
        available_file_ids: &HashSet<FileId>,
        active_downloads: &HashSet<FileId>,
        capacity: usize,
    ) -> Vec<FileId> {
        if capacity == 0 {
            return Vec::new();
        }

        let mut selected_priority = Vec::new();
        for file_id in pending_queue {
            if resume_priority_set.contains(file_id)
                && available_file_ids.contains(file_id)
                && !active_downloads.contains(file_id)
            {
                selected_priority.push(file_id.clone());
                if selected_priority.len() == capacity {
                    return selected_priority;
                }
            }
        }
        if !selected_priority.is_empty() {
            return selected_priority;
        }
        if resume_priority_set
            .iter()
            .any(|file_id| !active_downloads.contains(file_id))
        {
            return Vec::new();
        }

        let mut selected = Vec::new();
        for file_id in pending_queue {
            if available_file_ids.contains(file_id) && !active_downloads.contains(file_id) {
                selected.push(file_id.clone());
                if selected.len() == capacity {
                    break;
                }
            }
        }
        selected
    }

    proptest! {
        #[test]
        fn select_startable_file_ids_matches_resume_priority_contract(
            pending in proptest::collection::vec(0u8..20, 0..12),
            resume_priority in proptest::collection::vec(0u8..20, 0..12),
            available in proptest::collection::vec(0u8..20, 0..12),
            active in proptest::collection::vec(0u8..20, 0..12),
            capacity in 0usize..8,
        ) {
            let pending_queue = VecDeque::from(dedup_file_ids(&pending));
            let resume_priority_set = dedup_file_id_set(&resume_priority);
            let available_set = dedup_file_id_set(&available);
            let active_set = dedup_file_id_set(&active);

            let selected = select_startable_file_ids(
                &pending_queue,
                &resume_priority_set,
                &available_set,
                &active_set,
                capacity,
            );
            let expected = expected_startable_file_ids(
                &pending_queue,
                &resume_priority_set,
                &available_set,
                &active_set,
                capacity,
            );

            prop_assert_eq!(selected, expected);
        }

        #[test]
        fn verification_progress_emits_positive_events_that_sum_to_input(
            deltas in proptest::collection::vec(0u64..(VERIFICATION_PROGRESS_EVENT_BYTES * 2), 0..20),
        ) {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let progress = VerificationProgress::new(tx, "file.bin".into());
            let expected_total = deltas.iter().copied().sum::<u64>();

            for total_bytes_delta in &deltas {
                progress.on_progress(
                    "file.bin",
                    ProgressDelta {
                        total_bytes_delta: *total_bytes_delta,
                        network_bytes_delta: 0,
                    },
                );
            }
            progress.flush_pending();

            let mut actual_total = 0u64;
            let mut events = 0usize;
            while let Ok(event) = rx.try_recv() {
                let DownloadEvent::VerificationProgress { id, bytes_delta } = event else {
                    prop_assert!(false, "unexpected event emitted");
                    continue;
                };
                prop_assert_eq!(id, FileId::from("file.bin"));
                prop_assert!(bytes_delta > 0);
                actual_total = actual_total.saturating_add(bytes_delta);
                events = events.saturating_add(1);
            }

            prop_assert_eq!(actual_total, expected_total);
            if expected_total == 0 {
                prop_assert_eq!(events, 0);
            }
        }

        #[test]
        fn progress_deltas_are_clamped_to_file_size(
            file_size in 1u64..2_000_001,
            deltas in proptest::collection::vec(0u64..2_000_001, 0..20),
        ) {
            let mut app = test_app();
            app.ensure_core_file(
                &"test.bin".to_string().into(),
                "https://mega.nz/file/test",
                "test.bin",
                file_size,
                crate::core::FileAccounting::CurrentRun,
            );

            app.handle_download_event(DownloadEvent::FileStart {
                id: "test.bin".to_string().into(),
                size: file_size,
                attempt_id: 0,
            });

            for delta in &deltas {
                app.handle_download_event(DownloadEvent::Progress {
                    id: "test.bin".into(),
                    delta: ProgressDelta {
                        total_bytes_delta: *delta,
                        network_bytes_delta: *delta,
                    },
                    attempt_id: 0,
                });
            }

            let expected_downloaded = deltas.iter().copied().sum::<u64>().min(file_size);
            let file = app.files.iter().find(|f| f.id == "test.bin").unwrap();
            prop_assert_eq!(file.downloaded, expected_downloaded);
            prop_assert!(file.downloaded <= file.size);
            prop_assert_eq!(app.total_downloaded, expected_downloaded);
        }
    }
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
fn resume_priority_targets_block_other_pending_downloads() {
    let resume_a = FileId::from("resume-a.bin");
    let resume_b = FileId::from("resume-b.bin");
    let new_a = FileId::from("new-a.bin");
    let new_b = FileId::from("new-b.bin");
    let pending_queue = VecDeque::from([
        new_a.clone(),
        resume_a.clone(),
        new_b.clone(),
        resume_b.clone(),
    ]);
    let resume_priority_set = HashSet::from([resume_a.clone(), resume_b.clone()]);
    let available = HashSet::from([
        resume_a.clone(),
        resume_b.clone(),
        new_a.clone(),
        new_b.clone(),
    ]);

    let selected = select_startable_file_ids(
        &pending_queue,
        &resume_priority_set,
        &available,
        &HashSet::new(),
        2,
    );

    assert_eq!(selected, vec![resume_a.clone(), resume_b.clone()]);

    let selected_while_one_resume_active = select_startable_file_ids(
        &pending_queue,
        &resume_priority_set,
        &available,
        &HashSet::from([resume_a]),
        2,
    );

    assert_eq!(
        selected_while_one_resume_active,
        vec![resume_b],
        "new queued files must stay blocked until the Alt-R resume priority queue is drained"
    );
}

#[test]
fn unavailable_resume_priority_blocks_new_downloads_until_reverify_finishes() {
    let resume_a = FileId::from("resume-a.bin");
    let resume_b = FileId::from("resume-b.bin");
    let new_a = FileId::from("new-a.bin");
    let new_b = FileId::from("new-b.bin");
    let pending_queue = VecDeque::from([
        new_a.clone(),
        resume_a.clone(),
        new_b.clone(),
        resume_b.clone(),
    ]);
    let resume_priority_set = HashSet::from([resume_a.clone(), resume_b.clone()]);
    let available = HashSet::from([resume_a.clone(), new_a, new_b]);
    let active = HashSet::from([resume_a]);

    let selected =
        select_startable_file_ids(&pending_queue, &resume_priority_set, &available, &active, 1);

    assert!(
        selected.is_empty(),
        "new queued files must remain blocked while another Alt-R file is still being reverified"
    );
}

#[test]
fn reverify_for_unavailable_file_does_not_leave_ghost_resume_priority_entry() {
    let mut scheduler = SchedulerState::new();
    let ghost = FileId::from("failed-ghost.bin");

    let paused = scheduler.pause_file_ids(std::slice::from_ref(&ghost));
    assert!(paused.is_empty());

    let paused_ids = paused
        .iter()
        .map(|download| FileId::from(download.item.path.as_str()))
        .collect::<Vec<_>>();
    scheduler.mark_resume_priority_file_ids(&paused_ids);

    assert!(
        !scheduler.resume_priority_set.contains(&ghost),
        "failed or otherwise unavailable files must not leave behind resume-priority blockers"
    );
}

#[test]
fn file_id_map_lookup_falls_back_when_ptr_key_differs() {
    let stored = FileId::from(String::from("file.bin"));
    let lookup = FileId::from(String::from("file.bin"));
    let ptrs = HashSet::new();
    let ids = HashMap::from([(stored, ())]);

    assert!(contains_file_id_map_key(&ptrs, &ids, &lookup));
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
        accounting: crate::core::FileAccounting::CurrentRun,
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
        accounting: crate::core::FileAccounting::CurrentRun,
        origin: FileOrigin {
            package_id: None,
            package_display_name: None,
            source_url: "https://mega.nz/file/new".to_string(),
            submitted_url: "https://mega.nz/file/new".to_string(),
        },
    }));
    app.flush_session_persistence();

    let saved = crate::core::SessionSnapshot::latest().expect("session should be saved");
    assert_eq!(saved.urls.len(), 1);
    assert_eq!(saved.urls[0].url, "https://mega.nz/file/new");
    assert_eq!(saved.file_count(), 1);
    assert!(saved.find_file("file-id").is_some());
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
        accounting: crate::core::FileAccounting::CurrentRun,
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
    assert!(!app.tracked_urls().iter().any(|url| url == &source_url));

    app.handle_download_event(DownloadEvent::FileQueued(QueuedFile {
        id: "late.bin".to_string().into(),
        size: 256,
        accounting: crate::core::FileAccounting::CurrentRun,
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
    app.apply_core_event(CoreEvent::UrlSubmitted { url: url.clone() });

    app.handle_download_event(DownloadEvent::UrlQueued { url: url.clone() });
    app.handle_download_event(DownloadEvent::ScopeError {
        scope: url.clone(),
        error: "bad folder".to_string(),
    });

    let overlay = app
        .overlay_files
        .get(url.as_str())
        .expect("url-level errors should remain in overlay");
    assert!(matches!(overlay.file().status, FileStatus::Error(ref msg) if msg == "bad folder"));
    let session = app.session.as_ref().expect("session should remain");
    assert_eq!(session.urls[0].url, url);
    assert_eq!(session.urls[0].error.as_deref(), Some("bad folder"));
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
    );
    app.recompute_totals();

    app.handle_download_event(DownloadEvent::FileQueued(QueuedFile {
        id: "episode.mkv".to_string().into(),
        size: 128,
        accounting: crate::core::FileAccounting::Preexisting,
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
    assert_eq!(app.files_completed, 1);
    assert_eq!(app.files_total, 1);
    assert_eq!(app.total_downloaded, 128);
    assert_eq!(app.total_size, 128);
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
fn cumulative_values_as_deltas_are_capped_at_file_size() {
    let mut app = test_app();
    let file_size: u64 = 1_000_000;
    app.ensure_core_file(
        &"test.bin".to_string().into(),
        "https://mega.nz/file/test",
        "test.bin",
        file_size,
        crate::core::FileAccounting::CurrentRun,
    );

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
