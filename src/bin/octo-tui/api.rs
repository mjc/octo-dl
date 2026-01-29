//! HTTP API server for receiving URLs from the bookmarklet.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tower_http::cors::{Any, CorsLayer};

use octo_dl::extract_urls;

use crate::event::DownloadEvent;

pub const DEFAULT_API_PORT: u16 = 9723;

#[derive(Deserialize)]
struct UrlRequest {
    text: String,
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

type ApiState = Arc<mpsc::UnboundedSender<DownloadEvent>>;

async fn api_health() -> impl IntoResponse {
    axum::Json(HealthResponse {
        status: "ok".to_string(),
    })
}

async fn api_post_urls(
    State(tx): State<ApiState>,
    axum::Json(payload): axum::Json<UrlRequest>,
) -> impl IntoResponse {
    let urls = extract_urls(&payload.text);

    let count = urls.len();
    if !urls.is_empty() {
        let _ = tx.send(DownloadEvent::UrlsReceived { urls: urls.clone() });
    }

    axum::Json(UrlResponse { added: urls, count })
}

/// Starts the HTTP API server for receiving URLs from the bookmarklet.
///
/// # Errors
///
/// Returns an error if the server cannot bind to the specified port.
pub async fn run_api_server(
    tx: mpsc::UnboundedSender<DownloadEvent>,
    port: u16,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let state: ApiState = Arc::new(tx);

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/api/health", get(api_health))
        .route("/api/urls", post(api_post_urls))
        .layer(cors)
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
