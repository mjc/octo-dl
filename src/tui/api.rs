//! HTTP API server for receiving URLs from the bookmarklet.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::response::{Html, IntoResponse};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tower_http::cors::{Any, CorsLayer};

use crate::extract_urls;

use super::event::DownloadEvent;

pub const DEFAULT_API_PORT: u16 = 9723;

#[derive(Clone)]
struct AppState {
    tx: Arc<mpsc::UnboundedSender<DownloadEvent>>,
    host: String,
    port: u16,
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
}

async fn api_health() -> impl IntoResponse {
    axum::Json(HealthResponse {
        status: "ok".to_string(),
    })
}

async fn api_post_urls(
    State(state): State<AppState>,
    axum::Json(payload): axum::Json<UrlRequest>,
) -> impl IntoResponse {
    let urls = extract_urls(&payload.text);

    let count = urls.len();
    if !urls.is_empty() {
        let _ = state.tx.send(DownloadEvent::UrlsReceived { urls: urls.clone() });
    }

    axum::Json(UrlResponse { added: urls, count })
}

async fn api_parse_page(
    State(state): State<AppState>,
    axum::Json(payload): axum::Json<ParseRequest>,
) -> impl IntoResponse {
    // Try to extract URLs from the full page HTML first
    let mut urls = extract_urls(&payload.page);

    // If none found, fall back to selected text
    if urls.is_empty() && !payload.fallback.is_empty() {
        urls = extract_urls(&payload.fallback);
    }

    let count = urls.len();
    if !urls.is_empty() {
        let _ = state.tx.send(DownloadEvent::UrlsReceived { urls: urls.clone() });
    }

    axum::Json(UrlResponse { added: urls, count })
}

async fn bookmarklet_page(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    // Fallback for proxy scenarios where Host header might be wrong
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
<p>Click it on any page to send the selected text (or the page URL) to octo-dl for download.</p>
<p>Configured to use <code>{fallback_host}</code></p>
</body>
</html>"#
    ))
}

/// Starts the HTTP API server for receiving URLs from the bookmarklet.
///
/// # Errors
///
/// Returns an error if the server cannot bind to the specified address.
pub async fn run_api_server(
    tx: mpsc::UnboundedSender<DownloadEvent>,
    host: &str,
    port: u16,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let state = AppState {
        tx: Arc::new(tx),
        host: host.to_string(),
        port,
    };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/bookmarklet", get(bookmarklet_page))
        .route("/api/health", get(api_health))
        .route("/api/urls", post(api_post_urls))
        .route("/api/parse", post(api_parse_page))
        .layer(cors)
        .with_state(state);

    let addr: SocketAddr = format!("{}:{}", host, port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
