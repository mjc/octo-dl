//! HTTP API server for receiving URLs from the bookmarklet and serving the web UI.
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

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event as SseEvent, KeepAlive};
use axum::response::{Html, IntoResponse, Sse};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::BroadcastStream;
use tower_http::cors::{Any, CorsLayer};

use crate::extract_urls;

use super::app::{SharedAppState, UiAction};
use super::event::DownloadEvent;
use super::web;
use super::WebOptions;

pub const DEFAULT_API_PORT: u16 = 9723;

#[derive(Clone)]
struct ApiState {
    tx: Arc<mpsc::UnboundedSender<DownloadEvent>>,
    host: String,
    port: u16,
    shared: Option<SharedAppState>,
    web_opts: Option<WebOptions>,
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
    web_ui: bool,
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
    name: String,
}

#[derive(Deserialize)]
struct RetryRequest {
    name: String,
}

#[derive(Deserialize)]
struct ConfigUpdateRequest {
    chunks_per_file: Option<usize>,
    concurrent_files: Option<usize>,
    force_overwrite: Option<bool>,
    cleanup_on_error: Option<bool>,
}

#[derive(Deserialize)]
struct ShareTargetQuery {
    #[serde(default)]
    title: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    url: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Sends a `UiAction` to the event loop, returning 503 if shared state is absent.
fn send_ui_action(state: &ApiState, action: UiAction) -> axum::response::Response {
    if let Some(ref shared) = state.shared {
        let _ = shared.action_tx.send(action);
        axum::Json(serde_json::json!({"ok": true})).into_response()
    } else {
        (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "Web UI not enabled",
        )
            .into_response()
    }
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

// ---------------------------------------------------------------------------
// Existing endpoints
// ---------------------------------------------------------------------------

async fn api_health(State(state): State<ApiState>) -> impl IntoResponse {
    axum::Json(HealthResponse {
        status: "ok".to_string(),
        web_ui: state.web_opts.is_some(),
    })
}

async fn api_post_urls(
    State(state): State<ApiState>,
    axum::Json(payload): axum::Json<UrlRequest>,
) -> impl IntoResponse {
    let urls = extract_urls(&payload.text);
    let count = urls.len();
    dispatch_urls(&state, urls.clone());
    axum::Json(UrlResponse { added: urls, count })
}

async fn api_parse_page(
    State(state): State<ApiState>,
    axum::Json(payload): axum::Json<ParseRequest>,
) -> impl IntoResponse {
    let mut urls = extract_urls(&payload.page);
    if urls.is_empty() && !payload.fallback.is_empty() {
        urls = extract_urls(&payload.fallback);
    }
    let count = urls.len();
    dispatch_urls(&state, urls.clone());
    axum::Json(UrlResponse { added: urls, count })
}

async fn bookmarklet_page(State(state): State<ApiState>, headers: HeaderMap) -> impl IntoResponse {
    let fallback_host = headers
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or(&format!("{}:{}", state.host, state.port))
        .to_string();

    Html(format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>octo-dl bookmarklet</title>
<style>
  body {{ font-family: system-ui, sans-serif; max-width: 480px; margin: 60px auto; color: #e0e0e0; background: #1a1a2e; }}
  h1 {{ font-size: 1.4rem; }}
  p {{ line-height: 1.5; }}
  a.bookmarklet {{
    display: inline-block; padding: 10px 20px; margin: 20px 0;
    background: #0f3460; color: #e94560; border-radius: 6px;
    text-decoration: none; font-weight: bold; font-size: 1.1rem;
    border: 2px solid #e94560; cursor: grab;
  }}
  a.bookmarklet:hover {{ background: #16213e; }}
  code {{ background: #16213e; padding: 2px 6px; border-radius: 3px; }}
</style>
</head>
<body>
<h1>octo-dl bookmarklet</h1>
<p>Drag this link to your bookmarks bar:</p>
<a class="bookmarklet" href="javascript:void(function(){{var page=document.documentElement.outerHTML;var selected=window.getSelection().toString();var proto=window.location.protocol;var h=proto+'//{fallback_host}';fetch(h+'/api/parse',{{method:'POST',headers:{{'Content-Type':'application/json'}},body:JSON.stringify({{page:page,fallback:selected}})}}).then(function(r){{return r.json()}}).then(function(d){{if(d.count>0){{alert('Sent '+d.count+' URL(s) to octo-dl')}}else{{alert('No URLs found on this page')}}}}).catch(function(e){{alert('Error: '+e)}})}})()">
  Send to octo-dl
</a>
<p>Click it on any page to send the page HTML (with selected text as fallback) to octo-dl for download.</p>
<p>Configured to use <code>{fallback_host}</code></p>
</body>
</html>"#
    ))
}

// ---------------------------------------------------------------------------
// New web UI endpoints
// ---------------------------------------------------------------------------

/// GET /api/state — returns the full application snapshot as JSON.
async fn api_get_state(State(state): State<ApiState>) -> impl IntoResponse {
    if let Some(ref shared) = state.shared {
        let snap = shared.snapshot.read().await;
        axum::Json((*snap).clone()).into_response()
    } else {
        (axum::http::StatusCode::SERVICE_UNAVAILABLE, "Web UI not enabled").into_response()
    }
}

/// GET /api/events — SSE stream of application state updates.
async fn api_events(
    State(state): State<ApiState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<SseEvent, Infallible>>> {
    let rx = state
        .shared
        .as_ref()
        .map(|s| s.broadcast_tx.subscribe())
        .expect("SSE requires shared state");

    let stream = BroadcastStream::new(rx).filter_map(|result| {
        match result {
            Ok(snapshot) => {
                let json = serde_json::to_string(&snapshot).unwrap_or_default();
                Some(Ok(SseEvent::default().data(json)))
            }
            Err(_) => None, // lagged — skip
        }
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}

/// POST /api/login — submit login credentials from the web UI.
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
    send_ui_action(&state, UiAction::DeleteFile(payload.name))
}

/// POST /api/retry — retry a failed file.
async fn api_retry(
    State(state): State<ApiState>,
    axum::Json(payload): axum::Json<RetryRequest>,
) -> impl IntoResponse {
    send_ui_action(&state, UiAction::RetryFile(payload.name))
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

/// GET /share — Web Share Target handler (PWA share sheet integration).
/// Receives shared data via query parameters, extracts URLs, and redirects to the web UI.
async fn share_target(
    State(state): State<ApiState>,
    axum::extract::Query(params): axum::extract::Query<ShareTargetQuery>,
) -> impl IntoResponse {
    let combined = format!("{} {} {}", params.title, params.text, params.url);
    dispatch_urls(&state, extract_urls(&combined));
    axum::response::Redirect::to("/")
}

/// POST /share — Web Share Target handler for POST form submissions.
async fn share_target_post(
    State(state): State<ApiState>,
    axum::Form(params): axum::Form<ShareTargetQuery>,
) -> impl IntoResponse {
    let combined = format!("{} {} {}", params.title, params.text, params.url);
    dispatch_urls(&state, extract_urls(&combined));
    axum::response::Redirect::to("/")
}

// ---------------------------------------------------------------------------
// Web UI pages (served inline)
// ---------------------------------------------------------------------------

/// GET / — serves the main web UI SPA.
async fn web_ui_index(State(state): State<ApiState>) -> impl IntoResponse {
    let port = state.port;
    let host = state
        .web_opts
        .as_ref()
        .map_or_else(|| state.host.clone(), |w| w.public_host.clone());
    Html(web::index_html(&host, port))
}

/// GET /manifest.json — PWA manifest.
async fn web_ui_manifest(State(state): State<ApiState>) -> impl IntoResponse {
    let port = state.port;
    let host = state
        .web_opts
        .as_ref()
        .map_or_else(|| state.host.clone(), |w| w.public_host.clone());
    (
        [(axum::http::header::CONTENT_TYPE, "application/manifest+json")],
        web::manifest_json(&host, port),
    )
}

/// GET /sw.js — Service worker for PWA offline support.
async fn web_ui_sw() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/javascript")],
        web::service_worker_js(),
    )
}

/// GET /icon-192.svg — SVG icon for PWA.
async fn web_ui_icon() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "image/svg+xml")],
        web::icon_svg(),
    )
}

// ---------------------------------------------------------------------------
// Server setup
// ---------------------------------------------------------------------------

/// Starts the HTTP API server for receiving URLs from the bookmarklet.
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
    web_opts: Option<&WebOptions>,
    shared: Option<SharedAppState>,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let state = ApiState {
        tx: Arc::new(tx),
        host: host.to_string(),
        port,
        shared,
        web_opts: web_opts.cloned(),
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

    // Web UI routes (only when --web is enabled)
    if web_opts.is_some() {
        app = app
            .route("/", get(web_ui_index))
            .route("/manifest.json", get(web_ui_manifest))
            .route("/sw.js", get(web_ui_sw))
            .route("/icon-192.svg", get(web_ui_icon))
            .route("/icon-512.svg", get(web_ui_icon))
            .route("/api/state", get(api_get_state))
            .route("/api/events", get(api_events))
            .route("/api/login", post(api_login))
            .route("/api/pause", post(api_pause))
            .route("/api/delete", post(api_delete))
            .route("/api/retry", post(api_retry))
            .route("/api/config", post(api_config))
            .route("/share", get(share_target).post(share_target_post));
    }

    let app = app
        .layer(cors)
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024)) // 10MB limit
        .with_state(state);

    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
