//! HTTP API server for receiving URLs and serving the remote TUI stream.
//!
//! # Security Notice
//!
//! This API server has **no authentication** and accepts requests from any origin (CORS: `*`).
//! It should only be used:
//! - On `localhost` / `127.0.0.1` for local-only access
//! - Behind Tailscale or similar VPN for trusted network access
//! - **Never** exposed directly to the public internet
//!
//! The server accepts arbitrary HTML content and URL lists from clients. While request bodies
//! are limited to 10MB, this is not a substitute for authentication. For production deployments,
//! consider adding reverse proxy authentication (e.g., Tailscale, Caddy with auth middleware).

use axum::Router;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::HeaderMap;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tower_http::cors::{Any, CorsLayer};

use crate::extract_urls;

use super::app::{SharedAppState, UiAction};
use super::bookmarklet;
use super::event::DownloadEvent;
pub const DEFAULT_API_PORT: u16 = 9723;

#[derive(Clone)]
struct ApiState {
    tx: mpsc::UnboundedSender<DownloadEvent>,
    host: String,
    shared: Option<SharedAppState>,
    remote_tui_stream: bool,
    bookmarklet_host: Option<String>,
    api_key: Option<String>,
}

#[derive(Deserialize)]
struct UrlRequest {
    text: String,
}

#[derive(Deserialize)]
struct ParseRequest {
    page: String,
    fallback: String,
}

#[derive(Serialize)]
struct UrlResponse {
    added: Vec<String>,
    count: usize,
}

#[derive(Serialize)]
struct HealthResponse {
    status: String,
    remote_tui_stream: bool,
}

#[derive(Deserialize)]
struct LoginRequest {
    email: String,
    password: String,
    #[serde(default)]
    mfa: String,
}

#[derive(Deserialize)]
struct DeleteRequest {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct RetryRequest {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct ConfigUpdateRequest {
    chunks_per_file: Option<usize>,
    concurrent_files: Option<usize>,
    force_overwrite: Option<bool>,
    cleanup_on_error: Option<bool>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Sends a `UiAction` to the event loop, returning 503 if shared state is absent.
fn send_ui_action(state: &ApiState, action: UiAction) -> axum::response::Response {
    state.shared.as_ref().map_or_else(
        || {
            (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "interactive state not enabled",
            )
                .into_response()
        },
        |shared| {
            let _ = shared.action_tx.send(action);
            axum::Json(serde_json::json!({"ok": true})).into_response()
        },
    )
}

/// Dispatches extracted URLs — via `UiAction` if shared state is available,
/// otherwise directly as a `DownloadEvent`.
fn dispatch_urls(state: &ApiState, urls: Vec<String>) {
    if urls.is_empty() {
        return;
    }
    if let Some(ref shared) = state.shared {
        let _ = shared.action_tx.send(UiAction::AddUrls(urls));
    } else {
        let _ = state.tx.send(DownloadEvent::UrlsReceived { urls });
    }
}

fn provided_api_key(headers: &HeaderMap) -> Option<&str> {
    if let Some(key) = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.trim().is_empty())
    {
        return Some(key);
    }

    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.trim().is_empty())
}

fn require_api_key(state: &ApiState, headers: &HeaderMap) -> Option<axum::response::Response> {
    let expected_key = state.api_key.as_ref()?;
    if provided_api_key(headers).is_some_and(|provided| provided == expected_key) {
        return None;
    }

    Some(
        (
            axum::http::StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({"error": "invalid api key"})),
        )
            .into_response(),
    )
}

#[derive(Deserialize)]
struct SnapshotFile {
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct SnapshotPackage {
    id: String,
    #[serde(default)]
    source_url: String,
    #[serde(default)]
    display_name: String,
}

#[derive(Deserialize)]
struct SnapshotState {
    #[serde(default)]
    files: Vec<SnapshotFile>,
    #[serde(default)]
    packages: Vec<SnapshotPackage>,
}

fn snapshot_state(state: &ApiState) -> Result<SnapshotState, Box<axum::response::Response>> {
    let Some(shared) = state.shared.as_ref() else {
        return Err(Box::new(
            (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({"error": "interactive state not enabled"})),
            )
                .into_response(),
        ));
    };

    serde_json::from_str(shared.state_rx.borrow().as_str()).map_err(|_| {
        Box::new(
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({"error": "invalid app state"})),
            )
                .into_response(),
        )
    })
}

fn resolve_package_id(
    state: &ApiState,
    id: Option<&str>,
    name: Option<&str>,
) -> Result<Option<String>, Box<axum::response::Response>> {
    let Some(selector) = id.or(name) else {
        return Ok(None);
    };

    let Ok(snapshot) = snapshot_state(state) else {
        return Ok(None);
    };

    let matches: Vec<_> = snapshot
        .packages
        .into_iter()
        .filter(|package| {
            package.id == selector
                || package.display_name == selector
                || package.source_url == selector
        })
        .collect();
    match matches.as_slice() {
        [] => Ok(None),
        [package] => Ok(Some(package.id.clone())),
        _ => Err(Box::new(
            (
                axum::http::StatusCode::CONFLICT,
                axum::Json(serde_json::json!({"error": "ambiguous package name; use id"})),
            )
                .into_response(),
        )),
    }
}

fn resolve_file_id(
    state: &ApiState,
    id: Option<String>,
    name: Option<String>,
) -> Result<String, Box<axum::response::Response>> {
    if let Some(id) = id {
        return Ok(id);
    }

    let Some(name) = name else {
        return Err(Box::new(
            (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": "missing id or name"})),
            )
                .into_response(),
        ));
    };

    let snapshot = snapshot_state(state)?;

    let matches: Vec<_> = snapshot
        .files
        .into_iter()
        .filter(|file| file.name == name)
        .collect();
    match matches.as_slice() {
        [] => Err(Box::new(
            (
                axum::http::StatusCode::NOT_FOUND,
                axum::Json(serde_json::json!({"error": "file not found"})),
            )
                .into_response(),
        )),
        [file] => Ok(file.id.clone()),
        _ => Err(Box::new(
            (
                axum::http::StatusCode::CONFLICT,
                axum::Json(serde_json::json!({"error": "ambiguous file name; use id"})),
            )
                .into_response(),
        )),
    }
}

fn header_to_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.trim().is_empty())
}

fn parse_forwarded_param(value: &str, key: &str) -> Option<String> {
    for entry in value.split(',') {
        for part in entry.split(';') {
            let mut segments = part.trim().splitn(2, '=');
            if let (Some(param), Some(raw_value)) = (segments.next(), segments.next())
                && param.eq_ignore_ascii_case(key)
            {
                let cleaned = raw_value.trim().trim_matches('"');
                if !cleaned.is_empty() {
                    return Some(cleaned.to_string());
                }
            }
        }
    }
    None
}

fn infer_host(headers: &HeaderMap, state: &ApiState) -> String {
    if let Some(host) = header_to_str(headers, "x-forwarded-host") {
        return host.split(',').next().unwrap_or(host).trim().to_string();
    }
    if let Some(forwarded) = header_to_str(headers, "forwarded")
        && let Some(host) = parse_forwarded_param(forwarded, "host")
    {
        return host;
    }
    if let Some(host) = header_to_str(headers, "host") {
        return host.to_string();
    }
    state
        .bookmarklet_host
        .clone()
        .unwrap_or_else(|| state.host.clone())
}

// ---------------------------------------------------------------------------
// Existing endpoints
// ---------------------------------------------------------------------------

async fn api_health(State(state): State<ApiState>) -> impl IntoResponse {
    axum::Json(HealthResponse {
        status: "ok".to_string(),
        remote_tui_stream: state.remote_tui_stream,
    })
}

async fn api_post_urls(
    State(state): State<ApiState>,
    headers: HeaderMap,
    axum::Json(payload): axum::Json<UrlRequest>,
) -> impl IntoResponse {
    if let Some(response) = require_api_key(&state, &headers) {
        return response;
    }

    let urls = extract_urls(&payload.text);
    let count = urls.len();
    dispatch_urls(&state, urls.clone());
    axum::Json(UrlResponse { added: urls, count }).into_response()
}

async fn api_parse_page(
    State(state): State<ApiState>,
    headers: HeaderMap,
    axum::Json(payload): axum::Json<ParseRequest>,
) -> impl IntoResponse {
    if let Some(response) = require_api_key(&state, &headers) {
        return response;
    }

    let mut urls = extract_urls(&payload.page);
    if urls.is_empty() && !payload.fallback.is_empty() {
        urls = extract_urls(&payload.fallback);
    }
    let count = urls.len();
    dispatch_urls(&state, urls.clone());
    axum::Json(UrlResponse { added: urls, count }).into_response()
}

async fn bookmarklet_page(State(state): State<ApiState>, headers: HeaderMap) -> impl IntoResponse {
    let fallback_host = infer_host(&headers, &state);
    let api_key_header = state.api_key.as_ref().map_or_else(
        || "{}".to_string(),
        |key| {
            serde_json::to_string(&serde_json::json!({
                "x-api-key": key,
            }))
            .expect("serializing API key header should not fail")
        },
    );

    Html(bookmarklet::bookmarklet_html(
        &fallback_host,
        &api_key_header,
    ))
}

// ---------------------------------------------------------------------------
// Remote TUI stream
// ---------------------------------------------------------------------------

/// GET /api/dashboard — websocket stream of application state updates for attached TUI clients.
async fn api_dashboard_ws(
    State(state): State<ApiState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    match (state.remote_tui_stream, state.shared.as_ref()) {
        (false, _) => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "remote TUI stream not enabled",
        )
            .into_response(),
        (true, Some(shared)) => ws.on_upgrade({
            let rx = shared.state_rx.clone();
            move |socket| dashboard_socket(socket, rx)
        }),
        (true, None) => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "interactive state not enabled",
        )
            .into_response(),
    }
}

async fn dashboard_socket(mut socket: WebSocket, mut rx: tokio::sync::watch::Receiver<String>) {
    loop {
        let json = rx.borrow().clone();
        if socket.send(WsMessage::Text(json.into())).await.is_err() {
            break;
        }
        if rx.changed().await.is_err() {
            break;
        }
    }
}

/// POST /api/login — submit login credentials to the shared runtime.
async fn api_login(
    State(state): State<ApiState>,
    axum::Json(payload): axum::Json<LoginRequest>,
) -> impl IntoResponse {
    send_ui_action(
        &state,
        UiAction::Login {
            email: payload.email,
            password: payload.password,
            mfa: payload.mfa,
        },
    )
}

/// POST /api/pause — toggle pause state.
async fn api_pause(State(state): State<ApiState>) -> impl IntoResponse {
    send_ui_action(&state, UiAction::TogglePause)
}

/// POST /api/delete — delete a file by name.
async fn api_delete(
    State(state): State<ApiState>,
    axum::Json(payload): axum::Json<DeleteRequest>,
) -> impl IntoResponse {
    match resolve_package_id(&state, payload.id.as_deref(), payload.name.as_deref()) {
        Ok(Some(id)) => return send_ui_action(&state, UiAction::DeletePackage(id)),
        Ok(None) => {}
        Err(response) => return *response,
    }
    match resolve_file_id(&state, payload.id, payload.name) {
        Ok(id) => send_ui_action(&state, UiAction::DeleteFile(id)),
        Err(response) => *response,
    }
}

/// POST /api/retry — retry a failed file.
async fn api_retry(
    State(state): State<ApiState>,
    axum::Json(payload): axum::Json<RetryRequest>,
) -> impl IntoResponse {
    match resolve_package_id(&state, payload.id.as_deref(), payload.name.as_deref()) {
        Ok(Some(id)) => return send_ui_action(&state, UiAction::RetryPackage(id)),
        Ok(None) => {}
        Err(response) => return *response,
    }
    match resolve_file_id(&state, payload.id, payload.name) {
        Ok(id) => send_ui_action(&state, UiAction::RetryFile(id)),
        Err(response) => *response,
    }
}

/// POST /api/config — update download configuration.
async fn api_config(
    State(state): State<ApiState>,
    axum::Json(payload): axum::Json<ConfigUpdateRequest>,
) -> impl IntoResponse {
    send_ui_action(
        &state,
        UiAction::UpdateConfig {
            chunks_per_file: payload.chunks_per_file,
            concurrent_files: payload.concurrent_files,
            force_overwrite: payload.force_overwrite,
            cleanup_on_error: payload.cleanup_on_error,
        },
    )
}

// ---------------------------------------------------------------------------
// Server setup
// ---------------------------------------------------------------------------

/// Starts the HTTP API server for receiving URLs, bookmarklet requests,
/// and optional remote TUI attach connections.
///
/// # Security
///
/// This server has no authentication. Only bind to `localhost` or use behind
/// a trusted network (e.g., Tailscale). Never expose directly to the internet.
///
/// # Errors
///
/// Returns an error if the server cannot bind to the specified address.
pub async fn run_api_server(
    tx: mpsc::UnboundedSender<DownloadEvent>,
    host: &str,
    port: u16,
    bookmarklet_host: Option<&str>,
    shared: Option<SharedAppState>,
    remote_tui_stream: bool,
    api_key: Option<String>,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let state = ApiState {
        tx,
        host: host.to_string(),
        shared,
        remote_tui_stream,
        bookmarklet_host: bookmarklet_host.map(str::to_string),
        api_key,
    };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let mut app = Router::new()
        .route("/bookmarklet", get(bookmarklet_page))
        .route("/api/health", get(api_health))
        .route("/api/urls", post(api_post_urls))
        .route("/api/parse", post(api_parse_page));

    // Interactive mutation routes are exposed whenever the API is connected to app state.
    if state.shared.is_some() {
        app = app
            .route("/api/login", post(api_login))
            .route("/api/pause", post(api_pause))
            .route("/api/delete", post(api_delete))
            .route("/api/retry", post(api_retry))
            .route("/api/config", post(api_config));
    }

    if state.remote_tui_stream {
        app = app.route("/api/dashboard", get(api_dashboard_ws));
    }

    let app = app
        .layer(cors)
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024)) // 10MB limit
        .with_state(state);

    let listener = tokio::net::TcpListener::bind((host, port)).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
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

        dispatch_urls(&state, urls.clone());

        match rx.try_recv().expect("download event should be sent") {
            DownloadEvent::UrlsReceived { urls: received } => assert_eq!(received, urls),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn dispatch_urls_with_shared_state_sends_ui_action() {
        let (state, mut rx) = state_with_snapshot(r#"{"files":[]}"#);
        let urls = vec!["https://mega.nz/folder/abc#key".to_string()];

        dispatch_urls(&state, urls.clone());

        match rx.try_recv().expect("UI action should be sent") {
            UiAction::AddUrls(received) => assert_eq!(received, urls),
            other => panic!("unexpected UI action: {other:?}"),
        }
    }

    #[test]
    fn resolve_file_id_by_id_does_not_require_shared_state() {
        let (state, _rx) = state_without_shared();

        let id = resolve_file_id(&state, Some("file-id".to_string()), None)
            .expect("explicit id should resolve");

        assert_eq!(id, "file-id");
    }

    #[test]
    fn resolve_file_id_by_name_reports_all_lookup_cases() {
        let (state, _rx) = state_with_snapshot(
            r#"{"files":[{"id":"one","name":"unique.mkv"},{"id":"two","name":"dup.mkv"},{"id":"three","name":"dup.mkv"}]}"#,
        );

        let id = resolve_file_id(&state, None, Some("unique.mkv".to_string()))
            .expect("unique name should resolve");
        assert_eq!(id, "one");

        let missing = resolve_file_id(&state, None, None).expect_err("missing selector");
        assert_eq!(missing.status(), StatusCode::BAD_REQUEST);

        let not_found =
            resolve_file_id(&state, None, Some("missing.mkv".to_string())).expect_err("not found");
        assert_eq!(not_found.status(), StatusCode::NOT_FOUND);

        let duplicate =
            resolve_file_id(&state, None, Some("dup.mkv".to_string())).expect_err("duplicate");
        assert_eq!(duplicate.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn resolve_package_id_matches_package_rows() {
        let (state, _rx) = state_with_snapshot(
            r#"{"packages":[{"id":"pkg","source_url":"https://mega.nz/folder/pkg","display_name":"Package"},{"id":"other","source_url":"https://mega.nz/folder/other","display_name":"Other"}],"files":[]}"#,
        );

        let by_id = resolve_package_id(&state, Some("pkg"), None)
            .expect("package lookup should succeed")
            .expect("package should resolve");
        assert_eq!(by_id, "pkg");

        let by_name = resolve_package_id(&state, None, Some("Package"))
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
        let unavailable =
            resolve_file_id(&state, None, Some("file.mkv".to_string())).expect_err("no dashboard");
        assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);

        let (state, _rx) = state_with_snapshot("{not-json");
        let invalid =
            resolve_file_id(&state, None, Some("file.mkv".to_string())).expect_err("bad state");
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
}
