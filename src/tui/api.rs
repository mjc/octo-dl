//! HTTP API server for receiving URLs from the bookmarklet, and WebSocket
//! server for streaming the ratatui TUI to xterm.js in the browser.
//!
//! # Architecture
//!
//! The **`--web`** mode renders ratatui into an in-memory buffer
//! ([`super::BufWriter`]) and streams the ANSI output over a WebSocket to
//! an xterm.js terminal in the browser.  Keyboard input travels back over
//! the same WebSocket and is translated to crossterm events via
//! [`super::ansi_input::parse_xterm_input`].
//!
//! # Security Notice
//!
//! This API server has **no authentication** and accepts requests from any origin (CORS: `*`).
//! It should only be used:
//! - On `localhost` / `127.0.0.1` for local-only access
//! - Behind Tailscale or similar VPN for trusted network access
//! - **Never** exposed directly to the public internet

use std::net::SocketAddr;

use axum::Router;
use axum::extract::{DefaultBodyLimit, State, WebSocketUpgrade};
use axum::extract::ws::{Message, WebSocket};
use axum::http::HeaderMap;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tower_http::cors::{Any, CorsLayer};

use crate::extract_urls;

use super::event::DownloadEvent;

pub const DEFAULT_API_PORT: u16 = 9723;

/// Channel type for sending raw keyboard bytes from a WebSocket client
/// into the event loop.
pub type WsInputTx = mpsc::UnboundedSender<Vec<u8>>;

/// Channel type for receiving ANSI frame bytes from the event loop
/// to send to a WebSocket client.
pub type WsFrameRx = tokio::sync::watch::Receiver<Vec<u8>>;

#[derive(Clone)]
struct AppState {
    tx: mpsc::UnboundedSender<DownloadEvent>,
    host: String,
    port: u16,
    /// When web mode is active, the event loop listens on this channel
    /// for raw keyboard input from WebSocket clients.
    ws_input_tx: Option<WsInputTx>,
    /// When web mode is active, the event loop publishes ANSI frames here.
    ws_frame_rx: Option<WsFrameRx>,
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

async fn api_health(State(state): State<AppState>) -> impl IntoResponse {
    axum::Json(HealthResponse {
        status: "ok".to_string(),
        web_ui: state.ws_input_tx.is_some(),
    })
}

#[allow(dead_code)]
fn emit_urls_event(tx: &mpsc::UnboundedSender<DownloadEvent>, urls: Vec<String>) -> UrlResponse {
    let count = urls.len();
    if !urls.is_empty() {
        let _ = tx.send(DownloadEvent::UrlsReceived { urls: urls.clone() });
    }

    UrlResponse { added: urls, count }
}

async fn api_post_urls(
    State(state): State<AppState>,
    axum::Json(payload): axum::Json<UrlRequest>,
) -> impl IntoResponse {
    let urls = extract_urls(&payload.text);

    let count = urls.len();
    if !urls.is_empty() {
        let _ = state
            .tx
            .send(DownloadEvent::UrlsReceived { urls: urls.clone() });
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
        let _ = state
            .tx
            .send(DownloadEvent::UrlsReceived { urls: urls.clone() });
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
<p>Click it on any page to send the page HTML (with selected text as fallback) to octo-dl for download.</p>
<p>Configured to use <code>{fallback_host}</code></p>
</body>
</html>"#
    ))
}

/// Starts the HTTP API server for receiving URLs from the bookmarklet.
///
/// When `ws_input_tx` and `ws_frame_rx` are provided (web mode), also
/// serves the xterm.js web terminal at `/` and a WebSocket endpoint at `/ws`.
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
    ws_input_tx: Option<WsInputTx>,
    ws_frame_rx: Option<WsFrameRx>,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let web_mode = ws_input_tx.is_some();
    let state = AppState {
        tx,
        host: host.to_string(),
        port,
        ws_input_tx,
        ws_frame_rx,
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

    if web_mode {
        app = app
            .route("/", get(xterm_page))
            .route("/ws", get(ws_upgrade))
            .route("/static/xterm.min.js", get(serve_xterm_js))
            .route("/static/xterm.min.css", get(serve_xterm_css))
            .route("/static/addon-fit.min.js", get(serve_xterm_fit_js));
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

// ---------------------------------------------------------------------------
// WebSocket handler — streams ANSI frames to xterm.js, receives keyboard input
// ---------------------------------------------------------------------------

/// Upgrade GET /ws to a WebSocket connection.
async fn ws_upgrade(
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| ws_handler(socket, state))
}

/// Bidirectional WebSocket handler:
/// - Sends ANSI frame bytes to the browser (binary messages)
/// - Receives raw keyboard bytes from xterm.js (binary messages)
async fn ws_handler(mut socket: WebSocket, state: AppState) {
    let Some(input_tx) = state.ws_input_tx else {
        return;
    };
    let Some(mut frame_rx) = state.ws_frame_rx else {
        return;
    };

    loop {
        tokio::select! {
            // New frame from the render loop → send to browser
            result = frame_rx.changed() => {
                if result.is_err() {
                    break; // sender dropped
                }
                let frame = frame_rx.borrow_and_update().clone();
                if !frame.is_empty() {
                    if socket.send(Message::Binary(frame.into())).await.is_err() {
                        break;
                    }
                }
            }
            // Keyboard input from browser → forward to the event loop
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Binary(data))) => {
                        let _ = input_tx.send(data.to_vec());
                    }
                    Some(Ok(Message::Text(text))) => {
                        let _ = input_tx.send(text.as_bytes().to_vec());
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// xterm.js web page + static assets
// ---------------------------------------------------------------------------

/// GET / — serves the xterm.js web terminal page.
async fn xterm_page(State(_state): State<AppState>) -> impl IntoResponse {
    Html(format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>octo-dl</title>
<link rel="stylesheet" href="/static/xterm.min.css">
<style>
  * {{ margin: 0; padding: 0; box-sizing: border-box; }}
  html, body {{ height: 100%; background: #1a1a2e; overflow: hidden; }}
  #terminal {{ height: 100%; width: 100%; }}
  #conn-badge {{
    display: none; position: fixed; top: 8px; right: 8px; z-index: 10;
    background: #e94560; color: #fff; padding: 4px 12px;
    border-radius: 4px; font: 13px/1 system-ui;
  }}
  #conn-badge.show {{ display: block; }}
</style>
</head>
<body>
<div id="conn-badge">Disconnected</div>
<div id="terminal"></div>
<script src="/static/xterm.min.js"></script>
<script src="/static/addon-fit.min.js"></script>
<script>
(function() {{
  const term = new Terminal({{
    cursorBlink: true,
    convertEol: true,
    fontFamily: "'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace",
    fontSize: 14,
    theme: {{
      background: '#1a1a2e',
      foreground: '#e0e0e0',
      cursor: '#e94560',
      selectionBackground: '#0f3460',
    }},
  }});
  const fitAddon = new FitAddon.FitAddon();
  term.loadAddon(fitAddon);
  term.open(document.getElementById('terminal'));
  fitAddon.fit();

  const badge = document.getElementById('conn-badge');
  let ws;
  let reconnectTimer;

  function connect() {{
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    ws = new WebSocket(proto + '//' + location.host + '/ws');
    ws.binaryType = 'arraybuffer';

    ws.onopen = function() {{
      badge.classList.remove('show');
      // Send initial terminal size
      const cols = term.cols;
      const rows = term.rows;
      ws.send(JSON.stringify({{ type: 'resize', cols: cols, rows: rows }}));
    }};

    ws.onmessage = function(e) {{
      if (e.data instanceof ArrayBuffer) {{
        term.write(new Uint8Array(e.data));
      }} else {{
        term.write(e.data);
      }}
    }};

    ws.onclose = function() {{
      badge.classList.add('show');
      clearTimeout(reconnectTimer);
      reconnectTimer = setTimeout(connect, 2000);
    }};

    ws.onerror = function() {{
      ws.close();
    }};
  }}

  // Forward keyboard input to the server
  term.onData(function(data) {{
    if (ws && ws.readyState === WebSocket.OPEN) {{
      // Send as binary for raw terminal input
      const encoder = new TextEncoder();
      ws.send(encoder.encode(data));
    }}
  }});

  // Handle terminal resize
  window.addEventListener('resize', function() {{
    fitAddon.fit();
    if (ws && ws.readyState === WebSocket.OPEN) {{
      ws.send(JSON.stringify({{ type: 'resize', cols: term.cols, rows: term.rows }}));
    }}
  }});

  connect();
}})();
</script>
</body>
</html>"#
    ))
}

/// GET /static/xterm.min.js
async fn serve_xterm_js() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/javascript")],
        include_str!("../../static/xterm.min.js"),
    )
}

/// GET /static/xterm.min.css
async fn serve_xterm_css() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/css")],
        include_str!("../../static/xterm.min.css"),
    )
}

/// GET /static/addon-fit.min.js
async fn serve_xterm_fit_js() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/javascript")],
        include_str!("../../static/addon-fit.min.js"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emit_urls_event_sends_download_event_for_non_empty_urls() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let urls = vec![
            "https://mega.nz/file/abc#key".to_string(),
            "https://mega.nz/file/def#key".to_string(),
        ];

        let response = emit_urls_event(&tx, urls.clone());
        assert_eq!(response.count, 2);
        assert_eq!(response.added, urls);

        match rx.try_recv() {
            Ok(DownloadEvent::UrlsReceived { urls }) => {
                assert_eq!(urls.len(), 2);
            }
            other => panic!("expected UrlsReceived event, got {other:?}"),
        }
    }

    #[test]
    fn emit_urls_event_skips_send_for_empty_urls() {
        let (tx, mut rx) = mpsc::unbounded_channel();

        let response = emit_urls_event(&tx, Vec::new());
        assert_eq!(response.count, 0);
        assert!(response.added.is_empty());
        assert!(rx.try_recv().is_err());
    }
}
