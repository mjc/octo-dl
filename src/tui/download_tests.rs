#![allow(clippy::zero_sized_map_values)]

use super::super::app::{App, FileEntry, FileStatus, UiAction};
use super::super::event::{DownloadEvent, DownloadEventSender, FileOrigin, QueuedFile};
use super::*;
use crate::core::{CoreEvent, FileLifecycle, ProgressDelta};
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
    let (tx, mut rx) = mpsc::channel(64);
    let first = DownloadRequest::ReverifyFileIds {
        source_url: "https://mega.nz/folder/root".to_string(),
        file_ids: vec!["resume-a.bin".into()],
    };
    let second = DownloadRequest::VerifyCompletedFileIdsWithOperations {
        source_url: "https://mega.nz/folder/root".to_string(),
        file_ids: vec!["complete-a.bin".into()],
        operation_ids: HashMap::new(),
    };
    let late = DownloadRequest::ReverifyFileIds {
        source_url: "https://mega.nz/folder/root".to_string(),
        file_ids: vec!["resume-b.bin".into()],
    };
    tx.try_send(second.clone())
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
    tx.try_send(late.clone())
        .expect("late request should queue");

    drain_ready_requests(&mut pending, &mut rx);
    assert_eq!(pending, VecDeque::from([second, late]));
    assert!(rx.try_recv().is_err());
}

fn test_app() -> App {
    let (tx, _rx) = mpsc::unbounded_channel();
    App::new(9723, tx, true)
}

#[tokio::test]
async fn resubmitted_deleted_download_uses_current_file_attempt_through_request_path() {
    let directory = tempdir().unwrap();
    let _guard = StateDirectoryGuard::set(directory.path());
    let fixture =
        crate::fake_mega::create_fake_mega_fixture(directory.path(), "resubmit.bin", 32, 8)
            .await
            .unwrap();
    let server = crate::fake_mega::FakeMegaServer::spawn(fixture.clone(), 1).unwrap();
    let http = Arc::new(crate::download::build_http_client().unwrap());
    let client = mega::Client::builder()
        .origin(server.origin().clone())
        .build((*http).clone())
        .unwrap();
    let config = DownloadConfig {
        path: Some(
            directory
                .path()
                .join("downloads")
                .to_string_lossy()
                .into_owned(),
        ),
        ..DownloadConfig::default()
    };
    let downloader = Arc::new(crate::Downloader::new(client, config));
    let cache = Arc::new(DlcKeyCache::new());
    let (event_tx, mut event_rx) = DownloadEventSender::channel();
    let mut app = App::new(9723, event_tx.clone(), true);
    let mut requests = app.url_rx.take().unwrap();
    let url = fixture.public_url();
    let mut scheduler = SchedulerState::new();
    let mut previous_item: Option<QueuedDownload> = None;

    // Resolve, delete, and submit repeatedly: URL retries must preserve the
    // app's per-file generation without assigning it to newly discovered files.
    for generation in 0..3 {
        if generation == 2 {
            let (request_tx, request_rx) = mpsc::channel(1);
            app.url_tx = request_tx;
            requests = request_rx;
            app.url_tx
                .try_send(DownloadRequest::SyncPendingOrder {
                    file_ids: Vec::new(),
                })
                .unwrap();
        }
        app.submit_url(url.clone());
        if generation == 2 {
            assert!(app.pending_url_submissions.contains(&url));
            assert!(matches!(
                requests.try_recv().unwrap(),
                DownloadRequest::SyncPendingOrder { .. }
            ));
            app.retry_pending_requests();
        }
        let request = requests.try_recv().unwrap();
        if let Some(stale) = &previous_item {
            event_tx
                .send(DownloadEvent::FileQueued(
                    stale.queued_event(crate::core::FileAccounting::CurrentRun),
                ))
                .unwrap();
            app.drain_download_events(&mut event_rx);
            assert!(
                app.core_state.files.is_empty(),
                "stale events must be rejected after resubmission too"
            );
        }
        queue_download_request_events(&request, &event_tx);
        let resolved = resolve_download_requests_with_fetch(
            &[request],
            DownloadAttemptId::new(0),
            &http,
            &cache,
            &event_tx,
            |source| {
                let downloader = &downloader;
                async move {
                    downloader
                        .client()
                        .fetch_public_nodes(&source.source_url)
                        .await
                        .map_err(|error| error.to_string())
                }
            },
        )
        .await;
        let progress = collection_progress(&event_tx, &resolved);
        let batch = collect_batch(&resolved, &downloader, &progress).await;
        assert_eq!(batch.queued_items.len(), 1);
        let item = batch.queued_items[0].clone();
        let id = FileId::from(item.item.path.as_str());
        let batch = scheduler.register_resolved_batch(batch);
        batch.emit_events(&event_tx);
        app.drain_download_events(&mut event_rx);
        assert!(
            app.core_state.files.contains_key(&id),
            "generation {generation} should be restored by real queued events"
        );
        assert_eq!(item.attempt_id, DownloadAttemptId::new(generation));
        scheduler.sync_pending_order(app.core_state.pending_file_ids());
        assert!(scheduler.pending_queue.contains(&id));

        if generation == 0 {
            app.perform_delete_file_action(&id);
        } else {
            let package_id = app.core_state.files[&id].package_id;
            app.perform_delete_package_action(package_id);
        }
        // Late events from the deleted generation must remain rejected.
        event_tx
            .send(DownloadEvent::FileQueued(
                item.queued_event(crate::core::FileAccounting::CurrentRun),
            ))
            .unwrap();
        app.drain_download_events(&mut event_rx);
        assert!(!app.core_state.files.contains_key(&id));
        previous_item = Some(item);
    }
    app.flush_session_persistence();
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn register_download_token_delivers_token_to_application_channel() {
    let (token_tx, mut token_rx) = mpsc::channel(1);
    let file_id: FileId = "episode.mkv".into();

    let cancel_token = register_download_token(file_id.clone(), &token_tx)
        .await
        .expect("a live token channel should accept registration");
    let message = token_rx
        .recv()
        .await
        .expect("registered token should arrive at the application");

    assert_eq!(message.file_id, file_id);
    assert!(!message.token.is_cancelled());
    cancel_token.cancel();
    assert!(message.token.is_cancelled());
}

#[tokio::test]
async fn register_download_token_reports_closed_application_channel() {
    let (token_tx, token_rx) = mpsc::channel(1);
    drop(token_rx);

    let result = register_download_token("episode.mkv".into(), &token_tx).await;

    assert!(result.is_err());
}

#[tokio::test]
async fn url_submission_attempt_is_not_used_as_a_new_file_attempt() {
    let directory = tempdir().expect("fixture directory should exist");
    let fixture =
        crate::fake_mega::create_fake_mega_fixture(directory.path(), "payload.bin", 32, 8)
            .await
            .expect("fake MEGA fixture should be created");
    let server = crate::fake_mega::FakeMegaServer::spawn(fixture.clone(), 1)
        .expect("fake MEGA server should start");
    let http = mega::http_client_builder()
        .expect("MEGA HTTP builder should exist")
        .build()
        .expect("HTTP client should build");
    let client = mega::Client::builder()
        .origin(server.origin().clone())
        .build(http)
        .expect("MEGA client should build");
    let nodes = client
        .fetch_public_nodes(&fixture.public_url())
        .await
        .expect("fake public node should resolve");
    let node = nodes
        .roots()
        .next()
        .expect("fixture should have one root node")
        .clone();
    let item = crate::OwnedDownloadItem {
        path: "payload.bin".to_string(),
        node,
        was_partial: false,
    };

    let queued = visible_downloads(
        vec![item],
        &ResolvedUrl::direct(&fixture.public_url()),
        &RequestedFiles::All,
        &HashMap::new(),
        crate::tui::event::DownloadAttemptId::new(1),
    );

    assert_eq!(
        queued[0].attempt_id,
        crate::tui::event::DownloadAttemptId::new(0),
        "a URL retry generation must not masquerade as a file retry generation"
    );
    server.shutdown().await.expect("fake server should stop");
}

#[test]
fn scheduler_claim_moves_a_file_to_active_state_as_one_transition() {
    let mut scheduler = SchedulerState::new();
    let file_id: FileId = "claimed.bin".into();
    scheduler.pending_queue.push_back(file_id.clone());
    scheduler.resume_priority_set.insert(file_id.clone());

    assert!(scheduler.claim_download(&file_id));
    assert!(!scheduler.pending_queue.contains(&file_id));
    assert!(!scheduler.resume_priority_set.contains(&file_id));
    assert!(scheduler.active_downloads.contains(&file_id));
    assert!(!scheduler.claim_download(&file_id));
}

#[test]
fn scheduler_claim_release_restores_queue_and_clears_both_active_indexes() {
    let mut scheduler = SchedulerState::new();
    let file_id: FileId = "released.bin".into();
    scheduler.pending_queue.push_back(file_id.clone());

    assert!(scheduler.claim_download(&file_id));
    scheduler.release_download_claim(file_id.clone());

    assert_eq!(scheduler.pending_queue, VecDeque::from([file_id.clone()]));
    assert!(!scheduler.active_downloads.contains(&file_id));
}

#[tokio::test]
async fn panicked_download_task_releases_its_scheduler_slot() {
    let mut scheduler = SchedulerState::new();
    let file_id: FileId = "panic.bin".into();
    scheduler.active_downloads.insert(file_id.clone());
    let handle = scheduler.join_set.spawn(async {
        panic!("download task panic");
        #[allow(unreachable_code)]
        DownloadTaskResult {
            task_id: tokio::task::id(),
            id: "panic.bin".into(),
            attempt_id: crate::tui::event::DownloadAttemptId::new(4),
            result: Ok(crate::FileStats {
                size: 0,
                network_bytes: 0,
                reused_bytes: 0,
                elapsed: std::time::Duration::ZERO,
                average_speed: 0,
                peak_speed: 0,
                ramp_up_time: None,
            }),
        }
    });
    scheduler.active_task_files.insert(
        handle.id(),
        (
            file_id.clone(),
            crate::tui::event::DownloadAttemptId::new(4),
        ),
    );
    let (event_tx, _event_rx) = super::super::event::DownloadEventSender::channel();
    let result = scheduler.join_set.join_next().await.unwrap();

    handle_download_join_result(result, &mut scheduler, &event_tx);

    assert!(!scheduler.active_downloads.contains(&file_id));
    assert!(!scheduler.available_downloads.contains_key(&file_id));
    assert!(scheduler.active_task_files.is_empty());
}

#[tokio::test]
async fn finishing_old_attempt_keeps_newer_resolved_download_available() {
    let directory = tempdir().expect("fixture directory should exist");
    let fixture =
        crate::fake_mega::create_fake_mega_fixture(directory.path(), "payload.bin", 32, 7)
            .await
            .expect("fake MEGA fixture should be created");
    let server = crate::fake_mega::FakeMegaServer::spawn(fixture.clone(), 1)
        .expect("fake MEGA server should start");
    let http = mega::http_client_builder()
        .expect("MEGA HTTP builder should exist")
        .build()
        .expect("HTTP client should build");
    let client = mega::Client::builder()
        .origin(server.origin().clone())
        .build(http)
        .expect("MEGA client should build");
    let nodes = client
        .fetch_public_nodes(&fixture.public_url())
        .await
        .expect("fake public node should resolve");
    let node = nodes
        .roots()
        .next()
        .expect("fixture should have one root node")
        .clone();
    let file_id = FileId::from("payload.bin");
    let replacement = QueuedDownload {
        resolved: ResolvedUrl::direct("https://mega.nz/file/retry"),
        item: crate::OwnedDownloadItem {
            path: file_id.to_string(),
            node,
            was_partial: false,
        },
        attempt_id: crate::tui::event::DownloadAttemptId::new(1),
        trust_resume_state: false,
    };
    let mut scheduler = SchedulerState::new();
    scheduler.active_downloads.insert(file_id.clone());
    scheduler
        .available_downloads
        .insert(file_id.clone(), replacement);
    scheduler.desired_pending_order.push(file_id.clone());
    scheduler.desired_pending_set.insert(file_id.clone());
    let task_file_id = file_id.clone();
    let handle = scheduler.join_set.spawn(async move {
        DownloadTaskResult {
            task_id: tokio::task::id(),
            id: task_file_id,
            attempt_id: crate::tui::event::DownloadAttemptId::new(0),
            result: Ok(crate::FileStats {
                size: 32,
                network_bytes: 32,
                reused_bytes: 0,
                elapsed: std::time::Duration::ZERO,
                average_speed: 0,
                peak_speed: 0,
                ramp_up_time: None,
            }),
        }
    });
    scheduler.active_task_files.insert(
        handle.id(),
        (
            FileId::from("payload.bin"),
            crate::tui::event::DownloadAttemptId::new(0),
        ),
    );
    let (event_tx, _event_rx) = super::super::event::DownloadEventSender::channel();

    handle_download_join_result(
        scheduler
            .join_set
            .join_next()
            .await
            .expect("task should join"),
        &mut scheduler,
        &event_tx,
    );

    assert_eq!(
        scheduler
            .available_downloads
            .get("payload.bin")
            .map(|item| item.attempt_id),
        Some(crate::tui::event::DownloadAttemptId::new(1))
    );
    assert_eq!(scheduler.pending_queue, VecDeque::from([file_id]));
    server.shutdown().await.expect("fake server should stop");
}

#[tokio::test]
async fn panicked_download_task_is_removed_from_available_and_pending_state() {
    let mut scheduler = SchedulerState::new();
    let file_id: FileId = "panic-queued.bin".into();
    scheduler.active_downloads.insert(file_id.clone());
    scheduler.desired_pending_order.push(file_id.clone());
    scheduler.desired_pending_set.insert(file_id.clone());
    scheduler.pending_queue.push_back(file_id.clone());

    let handle = scheduler.join_set.spawn(async {
        panic!("download task panic");
        #[allow(unreachable_code)]
        DownloadTaskResult {
            task_id: tokio::task::id(),
            id: "panic-queued.bin".into(),
            attempt_id: crate::tui::event::DownloadAttemptId::new(4),
            result: Ok(crate::FileStats {
                size: 0,
                network_bytes: 0,
                reused_bytes: 0,
                elapsed: std::time::Duration::ZERO,
                average_speed: 0,
                peak_speed: 0,
                ramp_up_time: None,
            }),
        }
    });
    scheduler.active_task_files.insert(
        handle.id(),
        (
            file_id.clone(),
            crate::tui::event::DownloadAttemptId::new(4),
        ),
    );
    let (event_tx, _event_rx) = super::super::event::DownloadEventSender::channel();
    let result = scheduler.join_set.join_next().await.unwrap();

    handle_download_join_result(result, &mut scheduler, &event_tx);

    assert!(!scheduler.active_downloads.contains(&file_id));
    assert!(!scheduler.pending_queue.contains(&file_id));
    assert!(scheduler.active_task_files.is_empty());
}

#[tokio::test]
async fn failed_transfer_leaves_scheduler_and_fake_server_usable() {
    let temp = tempdir().expect("test directory should exist");
    let fixture_dir = temp.path().join("fixture");
    let output_dir = temp.path().join("output");
    let fixture = crate::fake_mega::create_fake_mega_fixture(&fixture_dir, "payload.bin", 32, 71)
        .await
        .expect("fake fixture should be created");
    let server = crate::fake_mega::FakeMegaServer::spawn(fixture.clone(), 2)
        .expect("fake server should start");
    let http = mega::http_client_builder()
        .expect("MEGA HTTP builder should exist")
        .build()
        .expect("HTTP client should build");
    let client = mega::Client::builder()
        .origin(server.origin().clone())
        .build(http.clone())
        .expect("MEGA client should build");
    let nodes = client
        .fetch_public_nodes(&fixture.public_url())
        .await
        .expect("metadata request should succeed");
    let node = nodes
        .get_node_by_handle(fixture.handle())
        .expect("fixture node should be present")
        .clone();
    let config = DownloadConfig {
        chunks_per_file: 1,
        concurrent_files: 1,
        force_overwrite: true,
        ..DownloadConfig::default()
    };
    let runtime = DownloadRuntime {
        downloader: Arc::new(crate::Downloader::new(client, config)),
        http: Arc::new(http),
        dlc_cache: Arc::new(DlcKeyCache::new()),
        concurrent_files: 1,
    };
    let resolved = ResolvedUrl::direct(&fixture.public_url());
    let failed_path = output_dir.join("failed.bin").to_string_lossy().into_owned();
    let succeeding_path = output_dir
        .join("succeeding.bin")
        .to_string_lossy()
        .into_owned();
    let failed = QueuedDownload {
        resolved: resolved.clone(),
        item: crate::OwnedDownloadItem {
            path: failed_path.clone(),
            node: node.clone(),
            was_partial: false,
        },
        attempt_id: crate::tui::event::DownloadAttemptId::new(0),
        trust_resume_state: false,
    };
    let succeeding = QueuedDownload {
        resolved,
        item: crate::OwnedDownloadItem {
            path: succeeding_path.clone(),
            node,
            was_partial: false,
        },
        attempt_id: crate::tui::event::DownloadAttemptId::new(0),
        trust_resume_state: false,
    };
    let (event_tx, mut event_rx) = DownloadEventSender::channel();
    let mut app = App::new(9723, event_tx.clone(), true);
    app.apply_core_event(CoreEvent::UrlSubmitted {
        url: fixture.public_url(),
    });
    event_tx
        .send(DownloadEvent::UrlQueued {
            url: fixture.public_url(),
        })
        .expect("URL event should be accepted");
    for item in [&failed, &succeeding] {
        event_tx
            .send(DownloadEvent::FileQueued(
                item.queued_event(FileAccounting::CurrentRun),
            ))
            .expect("file event should be accepted");
    }
    for _ in 0..8 {
        if !app.drain_download_events(&mut event_rx) {
            break;
        }
    }
    assert!(app.core_state.files.contains_key(failed_path.as_str()));
    assert!(app.core_state.files.contains_key(succeeding_path.as_str()));

    let mut scheduler = SchedulerState::new();
    scheduler.register_resolved_batch(CollectedBatch {
        queued_items: vec![failed.clone(), succeeding.clone()],
        completed_items: Vec::new(),
        skipped_count: 0,
        partial_count: 0,
        successful_submitted_urls: Vec::new(),
    });
    scheduler.sync_pending_order(vec![
        failed_path.clone().into(),
        succeeding_path.clone().into(),
    ]);
    let (token_tx, _token_rx) = mpsc::channel(4);
    let (_pause_tx, pause_rx) = tokio::sync::watch::channel(false);

    let mut api_server = None;
    let mut api_port = None;
    for _ in 0..5 {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("API probe port should be available")
            .local_addr()
            .expect("API probe listener should have an address")
            .port();
        match app
            .start_api_server(
                "127.0.0.1".to_string(),
                port,
                Some("127.0.0.1".to_string()),
                None,
                false,
            )
            .await
        {
            Ok(server) => {
                api_port = Some(port);
                api_server = Some(server);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {}
            Err(error) => panic!("API server should start before the transfer: {error}"),
        }
    }
    let api_port = api_port.expect("an available API port should be found after retries");
    let api_server = api_server.expect("API server should start before the transfer");
    let api = mega::http_client_builder()
        .expect("API HTTP builder should exist")
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .expect("API HTTP client should build");
    let health_url = format!("http://127.0.0.1:{api_port}/api/health");
    let health = api
        .get(&health_url)
        .send()
        .await
        .expect("API should respond before a transfer");
    assert!(health.status().is_success());

    server.fail_next_download_request();
    assert!(
        start_pending_downloads(&runtime, &mut scheduler, &event_tx, &token_tx, &pause_rx).await
    );
    handle_download_join_result(
        scheduler
            .join_set
            .join_next()
            .await
            .expect("failed transfer should join"),
        &mut scheduler,
        &event_tx,
    );
    app.drain_download_events(&mut event_rx);
    assert!(
        app.core_state.files[failed_path.as_str()]
            .lifecycle
            .is_failed()
    );

    let health = api
        .get(&health_url)
        .send()
        .await
        .expect("API should respond after a failed transfer");
    assert!(health.status().is_success());

    assert!(
        start_pending_downloads(&runtime, &mut scheduler, &event_tx, &token_tx, &pause_rx).await
    );
    handle_download_join_result(
        scheduler
            .join_set
            .join_next()
            .await
            .expect("subsequent transfer should join"),
        &mut scheduler,
        &event_tx,
    );
    app.drain_download_events(&mut event_rx);
    assert_eq!(
        app.core_state.files[succeeding_path.as_str()].lifecycle,
        FileLifecycle::Complete
    );
    assert!(output_dir.join("succeeding.bin").exists());
    api_server
        .shutdown()
        .await
        .expect("API server should shut down cleanly");
    server.shutdown().await.expect("fake server should stop");
}

#[test]
fn expand_dlc_path_expands_tilde_prefix() {
    let home = dirs::home_dir().expect("home dir should exist for test");
    let expanded = expand_dlc_path("~/Downloads/example.dlc").unwrap();

    assert_eq!(
        expanded,
        format!("{}/Downloads/example.dlc", home.to_string_lossy())
    );
}

#[test]
fn expand_dlc_path_leaves_absolute_paths_unchanged() {
    let path = "/tmp/example.dlc";

    assert_eq!(expand_dlc_path(path).unwrap(), path);
}

mod property_tests {
    use super::*;
    use proptest::prelude::*;
    use proptest::test_runner::{Config as ProptestConfig, RngSeed};

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

    fn current_attempt(app: &App, file_id: &FileId) -> crate::tui::event::DownloadAttemptId {
        app.file_attempt_ids
            .get(file_id)
            .copied()
            .unwrap_or(crate::tui::event::DownloadAttemptId::new(0))
    }

    fn stale_attempt(
        current: crate::tui::event::DownloadAttemptId,
    ) -> crate::tui::event::DownloadAttemptId {
        crate::tui::event::DownloadAttemptId::new(current.raw().checked_sub(1).unwrap_or(u64::MAX))
    }

    fn lifecycle_snapshot(
        app: &App,
        file_id: &FileId,
    ) -> Option<(
        crate::core::FileLifecycle,
        u64,
        crate::core::FileAccounting,
        u64,
    )> {
        app.core_state.files.get(file_id).map(|file| {
            (
                file.lifecycle.clone(),
                file.progress.visible_completed_bytes,
                file.accounting,
                app.core_state.totals.run_completed_bytes,
            )
        })
    }

    fn queue_stale_attempt_history(
        app: &mut App,
        event_tx: &DownloadEventSender,
        event_rx: &mut mpsc::Receiver<DownloadEvent>,
        file_id: &FileId,
        attempt_id: crate::tui::event::DownloadAttemptId,
    ) {
        enqueue_and_drain(
            app,
            event_tx,
            event_rx,
            DownloadEvent::FileStart {
                id: file_id.clone(),
                size: 100,
                attempt_id,
            },
        );
        enqueue_and_drain(
            app,
            event_tx,
            event_rx,
            DownloadEvent::Progress {
                id: file_id.clone(),
                delta: ProgressDelta {
                    total_bytes_delta: 17,
                    network_bytes_delta: 17,
                },
                attempt_id,
            },
        );
        enqueue_and_drain(
            app,
            event_tx,
            event_rx,
            DownloadEvent::FileComplete {
                id: file_id.clone(),
                attempt_id,
            },
        );
        enqueue_and_drain(
            app,
            event_tx,
            event_rx,
            DownloadEvent::FileError {
                id: file_id.clone(),
                error: "late failure".to_string(),
                attempt_id,
            },
        );
    }

    fn enqueue_and_drain(
        app: &mut App,
        event_tx: &DownloadEventSender,
        event_rx: &mut mpsc::Receiver<DownloadEvent>,
        event: DownloadEvent,
    ) {
        assert!(
            event_tx.send(event).is_ok(),
            "generated lifecycle history event should be admitted"
        );
        drain_all_download_events(app, event_tx, event_rx);
    }

    fn drain_all_download_events(
        app: &mut App,
        event_tx: &DownloadEventSender,
        event_rx: &mut mpsc::Receiver<DownloadEvent>,
    ) {
        for _ in 0..32 {
            let handled = app.drain_download_events(event_rx);
            if !handled && event_rx.is_empty() && !event_tx.has_pending_lifecycle_events() {
                break;
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 64,
            rng_seed: RngSeed::Fixed(0x4f43_544f_3131_34),
            ..ProptestConfig::default()
        })]
        #[test]
        fn generated_lifecycle_histories_preserve_independent_invariants(
            operations in proptest::collection::vec((0u8..4, 0u8..5), 1..48),
            capacity in 1usize..=3,
        ) {
            let directory = tempdir().expect("state directory should exist");
            let _guard = StateDirectoryGuard::set(directory.path());
            let (event_tx, mut event_rx) = DownloadEventSender::channel_with_capacity(capacity);
            let mut app = App::new(9723, event_tx.clone(), true);
            let source_url = "https://mega.nz/file/history";
            enqueue_and_drain(
                &mut app,
                &event_tx,
                &mut event_rx,
                DownloadEvent::UrlQueued {
                    url: source_url.to_string(),
                },
            );
            let file_ids = ["history-a.bin", "history-b.bin", "history-c.bin", "history-d.bin"]
                .map(FileId::from);
            for file_id in &file_ids {
                enqueue_and_drain(
                    &mut app,
                    &event_tx,
                    &mut event_rx,
                    DownloadEvent::FileQueued(QueuedFile {
                        id: file_id.clone(),
                        attempt_id: crate::tui::event::DownloadAttemptId::new(0),
                        size: 100,
                        accounting: FileAccounting::CurrentRun,
                        origin: FileOrigin {
                            package_id: None,
                            package_display_name: None,
                            source_url: source_url.to_string(),
                            submitted_url: source_url.to_string(),
                        },
                    }),
                );
            }

            // Each lifecycle class has its own seeded fixture. Generated
            // operations exercise retry, reset, reverify, delete/re-add, and
            // progress on separate files without repeating delayed registration.
            let mut history = vec![
                (0, 0), // retry after failure
                (1, 1), // reset
                (2, 2), // reverify after completion
                (3, 3), // delete and re-add
                (4, 4), // delayed registration on a distinct, initially absent file
                (1, 5), // duplicate terminal event and late progress
            ];
            history.extend(operations);

            for (file_index, operation) in history {
                let file_id = if file_index == 4 {
                    FileId::from("delayed.bin")
                } else {
                    file_ids[usize::from(file_index)].clone()
                };
                let current = current_attempt(&app, &file_id);
                let before = lifecycle_snapshot(&app, &file_id);
                queue_stale_attempt_history(
                    &mut app,
                    &event_tx,
                    &mut event_rx,
                    &file_id,
                    stale_attempt(current),
                );
                prop_assert_eq!(lifecycle_snapshot(&app, &file_id), before);

                match operation {
                    0 => {
                        let attempt_id = current_attempt(&app, &file_id);
                        enqueue_and_drain(&mut app, &event_tx, &mut event_rx, DownloadEvent::FileError {
                            id: file_id.clone(), error: "retry fixture".to_string(), attempt_id,
                        });
                        app.perform_retry_file_action(&file_id);
                        let attempt_id = current_attempt(&app, &file_id);
                        enqueue_and_drain(&mut app, &event_tx, &mut event_rx, DownloadEvent::FileQueued(QueuedFile {
                            id: file_id.clone(), attempt_id, size: 100,
                            accounting: FileAccounting::CurrentRun,
                            origin: FileOrigin {
                                package_id: None, package_display_name: None,
                                source_url: source_url.to_string(), submitted_url: source_url.to_string(),
                            },
                        }));
                        enqueue_and_drain(&mut app, &event_tx, &mut event_rx, DownloadEvent::FileStart { id: file_id.clone(), size: 100, attempt_id });
                        enqueue_and_drain(&mut app, &event_tx, &mut event_rx, DownloadEvent::Progress {
                            id: file_id.clone(),
                            delta: ProgressDelta { total_bytes_delta: 5, network_bytes_delta: 5 },
                            attempt_id,
                        });
                    }
                    1 => {
                        app.perform_reset_file_action(&file_id);
                        let attempt_id = current_attempt(&app, &file_id);
                        enqueue_and_drain(&mut app, &event_tx, &mut event_rx, DownloadEvent::FileQueued(QueuedFile {
                            id: file_id.clone(), attempt_id, size: 100,
                            accounting: FileAccounting::CurrentRun,
                            origin: FileOrigin {
                                package_id: None, package_display_name: None,
                                source_url: source_url.to_string(), submitted_url: source_url.to_string(),
                            },
                        }));
                    }
                    2 => {
                        let attempt_id = current_attempt(&app, &file_id);
                        enqueue_and_drain(&mut app, &event_tx, &mut event_rx, DownloadEvent::FileComplete { id: file_id.clone(), attempt_id });
                        let completed_bytes = app.core_state.totals.run_completed_bytes;
                        enqueue_and_drain(&mut app, &event_tx, &mut event_rx, DownloadEvent::FileComplete { id: file_id.clone(), attempt_id });
                        prop_assert_eq!(app.core_state.totals.run_completed_bytes, completed_bytes);
                        app.perform_reverify_file_action(&file_id);
                    }
                    3 => {
                        let old_attempt = current_attempt(&app, &file_id);
                        app.perform_delete_file_action(&file_id);
                        let attempt_id = current_attempt(&app, &file_id);
                        enqueue_and_drain(&mut app, &event_tx, &mut event_rx, DownloadEvent::UrlQueued { url: source_url.to_string() });
                        enqueue_and_drain(&mut app, &event_tx, &mut event_rx, DownloadEvent::FileQueued(QueuedFile {
                            id: file_id.clone(), attempt_id, size: 100,
                            accounting: FileAccounting::CurrentRun,
                            origin: FileOrigin {
                                package_id: None, package_display_name: None,
                                source_url: source_url.to_string(), submitted_url: source_url.to_string(),
                            },
                        }));
                        let replacement = lifecycle_snapshot(&app, &file_id);
                        queue_stale_attempt_history(
                            &mut app,
                            &event_tx,
                            &mut event_rx,
                            &file_id,
                            old_attempt,
                        );
                        prop_assert_eq!(lifecycle_snapshot(&app, &file_id), replacement);
                    }
                    5 => {
                        let attempt_id = current_attempt(&app, &file_id);
                        enqueue_and_drain(&mut app, &event_tx, &mut event_rx, DownloadEvent::FileComplete { id: file_id.clone(), attempt_id });
                        enqueue_and_drain(&mut app, &event_tx, &mut event_rx, DownloadEvent::FileComplete { id: file_id.clone(), attempt_id });
                        let complete = lifecycle_snapshot(&app, &file_id);
                        enqueue_and_drain(&mut app, &event_tx, &mut event_rx, DownloadEvent::Progress {
                            id: file_id.clone(),
                            delta: ProgressDelta { total_bytes_delta: 73, network_bytes_delta: 73 },
                            attempt_id,
                        });
                        enqueue_and_drain(&mut app, &event_tx, &mut event_rx, DownloadEvent::FileStart { id: file_id.clone(), size: 100, attempt_id });
                        prop_assert_eq!(lifecycle_snapshot(&app, &file_id), complete);
                    }
                    _ => {
                        let attempt_id = current_attempt(&app, &file_id);
                        enqueue_and_drain(&mut app, &event_tx, &mut event_rx, DownloadEvent::Progress {
                            id: file_id.clone(),
                            delta: ProgressDelta { total_bytes_delta: 3, network_bytes_delta: 3 },
                            attempt_id,
                        });
                    }
                }
                if let Some(file) = app.core_state.files.get(&file_id) {
                    prop_assert!(file.progress.visible_completed_bytes <= file.size);
                }
            }
        }
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

            let mut scheduler = SchedulerState::new();
            scheduler.pending_queue = pending_queue.clone();
            scheduler
                .resume_priority_set
                .extend(resume_priority_set.iter().cloned());
            scheduler
                .active_downloads
                .extend(active_set.iter().cloned());
            let selected = scheduler
                .select_startable_file_ids(capacity, |file_id| available_set.contains(file_id));
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
                attempt_id: crate::tui::event::DownloadAttemptId::new(0),
            });

            for delta in &deltas {
                app.handle_download_event(DownloadEvent::Progress {
                    id: "test.bin".into(),
                    delta: ProgressDelta {
                        total_bytes_delta: *delta,
                        network_bytes_delta: *delta,
                    },
                    attempt_id: crate::tui::event::DownloadAttemptId::new(0),
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
fn pausing_active_reverify_keeps_desired_download_for_requeue() {
    let file_id: FileId = "active.bin".into();
    let mut scheduler = SchedulerState::new();
    scheduler.desired_pending_order.push(file_id.clone());
    scheduler.desired_pending_set.insert(file_id.clone());
    scheduler.active_downloads.insert(file_id.clone());

    let paused = scheduler.pause_file_ids(std::slice::from_ref(&file_id));

    assert!(
        paused.is_empty(),
        "active work has not yielded its queued item"
    );
    assert!(scheduler.desired_pending_set.contains(&file_id));
    assert_eq!(scheduler.desired_pending_order, vec![file_id]);
}

#[test]
fn retry_cleanup_finishes_before_a_new_attempt_can_be_queued() {
    let directory = tempdir().expect("temporary cleanup directory should exist");
    let output = directory.path().join("file.bin");
    let output = output.to_string_lossy().into_owned();
    let artifacts = [
        crate::download::part_path(&output),
        crate::download::sidecar_path(&output),
        crate::download::legacy_binary_sidecar_path(&output),
        crate::download::legacy_json_sidecar_path(&output),
    ];
    for artifact in &artifacts {
        std::fs::write(artifact, b"stale").expect("stale resume artifact should be writable");
    }
    std::fs::write(&output, b"completed").expect("output should be writable");

    schedule_resume_artifact_delete(output.clone());

    assert!(artifacts.iter().all(|artifact| !artifact.exists()));
    assert!(std::path::Path::new(&output).exists());
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
    let available = HashSet::from([resume_a.clone(), resume_b.clone(), new_a, new_b]);

    let mut scheduler = SchedulerState::new();
    scheduler.pending_queue = pending_queue;
    scheduler
        .resume_priority_set
        .extend(resume_priority_set.iter().cloned());
    let selected = scheduler.select_startable_file_ids(2, |file_id| available.contains(file_id));

    assert_eq!(selected, vec![resume_a.clone(), resume_b.clone()]);

    scheduler.active_downloads.insert(resume_a);
    let selected_while_one_resume_active =
        scheduler.select_startable_file_ids(2, |file_id| available.contains(file_id));

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
    let resume_priority_set = HashSet::from([resume_a.clone(), resume_b]);
    let available = HashSet::from([resume_a.clone(), new_a, new_b]);
    let active = HashSet::from([resume_a]);

    let mut scheduler = SchedulerState::new();
    scheduler.pending_queue = pending_queue;
    scheduler
        .resume_priority_set
        .extend(resume_priority_set.iter().cloned());
    scheduler.active_downloads.extend(active.iter().cloned());
    let selected = scheduler.select_startable_file_ids(1, |file_id| available.contains(file_id));

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
fn scheduler_uses_value_identity_for_file_ids() {
    let stored = FileId::from(String::from("file.bin"));
    let lookup = FileId::from(String::from("file.bin"));
    let ids = HashMap::from([(stored, ())]);

    assert!(ids.contains_key(&lookup));
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
        attempt_id: crate::tui::event::DownloadAttemptId::new(0),
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
        attempt_id: crate::tui::event::DownloadAttemptId::new(0),
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
        attempt_id: crate::tui::event::DownloadAttemptId::new(0),
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
        attempt_id: crate::tui::event::DownloadAttemptId::new(0),
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
        attempt_id: crate::tui::event::DownloadAttemptId::new(0),
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
        attempt_id: crate::tui::event::DownloadAttemptId::new(0),
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
    let resolved = [
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
            submission_attempt_id: crate::tui::event::DownloadAttemptId::new(0),
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
            submission_attempt_id: crate::tui::event::DownloadAttemptId::new(0),
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
            submission_attempt_id: crate::tui::event::DownloadAttemptId::new(0),
            emit_url_resolved: true,
        },
    ];

    let urls = successful_submitted_urls(resolved.iter());

    assert!(urls.is_empty());
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
        ..left
    };
    let same_size_and_date_without_checksum = BatchItemSnapshot {
        sparse_checksum: None,
        ..left
    };
    let different_size = BatchItemSnapshot {
        size: 90,
        sparse_checksum: None,
        ..left
    };

    assert!(remote_files_match(&left, &same_checksum_different_date));
    assert!(remote_files_match(
        &BatchItemSnapshot {
            sparse_checksum: None,
            ..left
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
fn duplicate_path_suffixes_are_reserved_across_packages() {
    let mut used_paths = HashSet::from([
        "folder/file.mkv".to_string(),
        "folder/file (2).mkv".to_string(),
    ]);

    assert_eq!(
        next_available_duplicate_path("folder/file.mkv", &mut used_paths),
        "folder/file (3).mkv"
    );
    assert!(used_paths.contains("folder/file (3).mkv"));
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
        attempt_id: crate::tui::event::DownloadAttemptId::new(0),
    });

    let cumulatives = [100_000u64, 350_000, 700_000, 900_000, 1_000_000];
    for c in cumulatives {
        app.handle_download_event(DownloadEvent::Progress {
            id: "test.bin".into(),
            delta: ProgressDelta {
                total_bytes_delta: c,
                network_bytes_delta: c,
            },
            attempt_id: crate::tui::event::DownloadAttemptId::new(0),
        });
    }

    let file = app.files.iter().find(|f| f.id == "test.bin").unwrap();
    assert_eq!(file.downloaded, file_size);
    assert_eq!(app.total_downloaded, file_size);
}
