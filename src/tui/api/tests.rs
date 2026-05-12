use super::super::app::{SharedAppState, UiAction};
use super::super::event::DownloadEvent;
use super::helpers::{self, infer_host, require_api_key};
use super::selection;
use super::*;
use axum::http::{HeaderValue, StatusCode};
use tokio::sync::watch;

fn state_without_shared() -> (ApiState, mpsc::UnboundedReceiver<DownloadEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (
        ApiState {
            tx,
            host: "127.0.0.1".to_string(),
            shared: None,
            remote_tui_stream: false,
            bookmarklet_host: None,
            api_key: None,
        },
        rx,
    )
}

fn state_with_snapshot(snapshot: &str) -> (ApiState, mpsc::UnboundedReceiver<UiAction>) {
    state_with_snapshot_options(snapshot, None, None)
}

fn state_with_snapshot_options(
    snapshot: &str,
    bookmarklet_host: Option<String>,
    api_key: Option<String>,
) -> (ApiState, mpsc::UnboundedReceiver<UiAction>) {
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let (action_tx, action_rx) = mpsc::unbounded_channel();
    let (_state_tx, state_rx) = watch::channel(snapshot.to_string());
    (
        ApiState {
            tx: event_tx,
            host: "127.0.0.1".to_string(),
            shared: Some(SharedAppState {
                action_tx,
                state_rx,
            }),
            remote_tui_stream: false,
            bookmarklet_host,
            api_key,
        },
        action_rx,
    )
}

#[test]
fn dispatch_urls_without_shared_state_sends_download_event() {
    let (state, mut rx) = state_without_shared();
    let urls = vec!["https://mega.nz/file/abc#key".to_string()];

    helpers::dispatch_urls(&state, urls.clone());

    match rx.try_recv().expect("download event should be sent") {
        DownloadEvent::UrlsReceived { urls: received } => assert_eq!(received, urls),
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn dispatch_urls_with_shared_state_sends_ui_action() {
    let (state, mut rx) = state_with_snapshot(r#"{"files":[]}"#);
    let urls = vec!["https://mega.nz/folder/abc#key".to_string()];

    helpers::dispatch_urls(&state, urls.clone());

    match rx.try_recv().expect("UI action should be sent") {
        UiAction::AddUrls(received) => assert_eq!(received, urls),
        other => panic!("unexpected UI action: {other:?}"),
    }
}

#[test]
fn resolve_file_id_by_id_does_not_require_shared_state() {
    let (state, _rx) = state_without_shared();

    let id = selection::resolve_file_id(&state, Some("file-id".to_string()), None)
        .expect("explicit id should resolve");

    assert_eq!(id, "file-id");
}

#[test]
fn resolve_file_id_by_name_reports_all_lookup_cases() {
    let (state, _rx) = state_with_snapshot(
        r#"{"files":[{"id":"one","name":"unique.mkv"},{"id":"two","name":"dup.mkv"},{"id":"three","name":"dup.mkv"}]}"#,
    );

    let id = selection::resolve_file_id(&state, None, Some("unique.mkv".to_string()))
        .expect("unique name should resolve");
    assert_eq!(id, "one");

    let missing = selection::resolve_file_id(&state, None, None).expect_err("missing selector");
    assert_eq!(missing.status(), StatusCode::BAD_REQUEST);

    let not_found = selection::resolve_file_id(&state, None, Some("missing.mkv".to_string()))
        .expect_err("not found");
    assert_eq!(not_found.status(), StatusCode::NOT_FOUND);

    let duplicate = selection::resolve_file_id(&state, None, Some("dup.mkv".to_string()))
        .expect_err("duplicate");
    assert_eq!(duplicate.status(), StatusCode::CONFLICT);
}

#[test]
fn resolve_package_id_matches_package_rows() {
    let (state, _rx) = state_with_snapshot(
        r#"{"packages":[{"id":"pkg","source_url":"https://mega.nz/folder/pkg","display_name":"Package"},{"id":"other","source_url":"https://mega.nz/folder/other","display_name":"Other"}],"files":[]}"#,
    );

    let by_id = selection::resolve_package_id(&state, Some("pkg"), None)
        .expect("package lookup should succeed")
        .expect("package should resolve");
    assert_eq!(by_id, "pkg");

    let by_name = selection::resolve_package_id(&state, None, Some("Package"))
        .expect("package lookup should succeed")
        .expect("package should resolve");
    assert_eq!(by_name, "pkg");
}

#[tokio::test]
async fn retry_api_dispatches_package_action_for_package_id() {
    let (state, mut rx) = state_with_snapshot(
        r#"{"packages":[{"id":"pkg","source_url":"https://mega.nz/folder/pkg","display_name":"Package"}],"files":[]}"#,
    );

    let _ = api_retry(
        State(state),
        axum::Json(RetryRequest {
            id: Some("pkg".to_string()),
            name: None,
        }),
    )
    .await
    .into_response();

    match rx.try_recv().expect("UI action should be sent") {
        UiAction::RetryPackage(id) => assert_eq!(id, "pkg"),
        other => panic!("unexpected UI action: {other:?}"),
    }
}

#[tokio::test]
async fn dashboard_url_submission_still_requires_configured_api_key() {
    let (state, mut rx) = state_with_snapshot_options(
        r#"{"files":[]}"#,
        Some("127.0.0.1".to_string()),
        Some("secret".to_string()),
    );

    let response = api_post_urls(
        State(state),
        HeaderMap::new(),
        axum::Json(UrlRequest {
            text: "https://mega.nz/file/abc#key".to_string(),
        }),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(rx.try_recv().is_err());
}

#[test]
fn resolve_file_id_by_name_requires_valid_shared_state() {
    let (state, _rx) = state_without_shared();
    let unavailable = selection::resolve_file_id(&state, None, Some("file.mkv".to_string()))
        .expect_err("no dashboard");
    assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);

    let (state, _rx) = state_with_snapshot("{not-json");
    let invalid = selection::resolve_file_id(&state, None, Some("file.mkv".to_string()))
        .expect_err("bad state");
    assert_eq!(invalid.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[test]
fn require_api_key_accepts_header_and_bearer_token() {
    let (mut state, _rx) = state_without_shared();
    state.api_key = Some("secret".to_string());

    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("secret"));
    assert!(require_api_key(&state, &headers).is_none());

    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        HeaderValue::from_static("Bearer secret"),
    );
    assert!(require_api_key(&state, &headers).is_none());
}

#[test]
fn require_api_key_rejects_missing_or_wrong_key() {
    let (mut state, _rx) = state_without_shared();
    state.api_key = Some("secret".to_string());

    let headers = HeaderMap::new();
    let missing = require_api_key(&state, &headers).expect("missing key should reject");
    assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);

    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", HeaderValue::from_static("wrong"));
    let wrong = require_api_key(&state, &headers).expect("wrong key should reject");
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
}

#[test]
fn forwarded_header_drives_public_host() {
    let (state, _rx) = state_without_shared();
    let mut headers = HeaderMap::new();
    headers.insert(
        "forwarded",
        HeaderValue::from_static(r#"for=192.0.2.10;proto=https;host="octo.example""#),
    );

    assert_eq!(infer_host(&headers, &state), "octo.example");
}

#[test]
fn forwarded_host_precedence_matches_proxy_conventions() {
    let (state, _rx) = state_without_shared();
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-forwarded-host",
        HeaderValue::from_static("public.example, internal.example"),
    );
    headers.insert(
        "forwarded",
        HeaderValue::from_static("proto=http;host=ignored.example"),
    );

    assert_eq!(infer_host(&headers, &state), "public.example");
}
