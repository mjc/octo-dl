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

mod helpers;
mod selection;

#[cfg(test)]
mod tests;

use axum::Router;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::HeaderMap;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tower_http::cors::{Any, CorsLayer};

use self::helpers::{infer_host, infer_origin, require_api_key, send_ui_action};
use self::selection::{resolve_file_id, resolve_package_id};
use super::app::{SharedAppState, UiAction};
use super::bookmarklet;
use super::event::DownloadEvent;
pub const DEFAULT_API_PORT: u16 = 9723;

#[derive(Clone)]
pub(super) struct ApiState {
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

    let (urls, count) = helpers::extract_and_dispatch_urls(&state, &payload.text);
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

    let mut urls = crate::extract_urls(&payload.page);
    if urls.is_empty() {
        urls = helpers::extract_and_dispatch_urls_from_html(&payload.page);
    }
    if urls.is_empty() && !payload.fallback.is_empty() {
        urls = crate::extract_urls(&payload.fallback);
    }
    let count = urls.len();
    helpers::dispatch_urls(&state, urls.clone());
    axum::Json(UrlResponse { added: urls, count }).into_response()
}

async fn bookmarklet_page(State(state): State<ApiState>, headers: HeaderMap) -> impl IntoResponse {
    let fallback_host = infer_host(&headers, &state);
    let fallback_origin = infer_origin(&headers, &state);
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
        &fallback_origin,
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
