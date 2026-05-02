//! Terminal-focused HTTP server that streams the real `ratatui` TUI via xterm.js.

use std::io::Read;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, OriginalUri, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, oneshot};
use tower_http::cors::{Any, CorsLayer};

use crate::{DlcKeyCache, extract_urls, parse_dlc_data};

use super::terminal::TerminalBridge;
use super::web;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use portable_pty::{PtySize, native_pty_system};
    use std::io::Read;

    fn build_test_state() -> (TerminalApiState, std::io::BufReader<Box<dyn Read + Send>>) {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 5,
                cols: 20,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("failed to open pty");
        let writer = pair
            .master
            .take_writer()
            .expect("failed to take writer from PTY");
        let bridge = Arc::new(TerminalBridge::new(pair.master, writer));
        let reader = bridge.try_clone_reader().expect("failed to clone reader");
        let session = Arc::new(TerminalSession::new(bridge.clone()));
        let state = TerminalApiState {
            host: "127.0.0.1".to_string(),
            port: 9723,
            session,
            http_client: Arc::new(build_http_client().expect("failed to build HTTP client")),
            dlc_cache: Arc::new(DlcKeyCache::new()),
            upstream_api_base: None,
            api_key: None,
        };
        let _slave = pair.slave; // keep slave alive so the PTY stays usable
        (state, std::io::BufReader::new(reader))
    }

    #[test]
    fn dispatch_urls_writes_lines_in_order() {
        let (state, mut reader) = build_test_state();
        let urls = vec![
            "https://mega.nz/folder/abc123".to_string(),
            "https://mega.nz/file/xyz789".to_string(),
        ];

        dispatch_urls(&state, urls.clone());
        std::thread::sleep(std::time::Duration::from_millis(20));

        let mut buf = [0u8; 4096];
        let n = std::io::Read::read(&mut reader, &mut buf).expect("failed to read PTY output");
        let received = String::from_utf8_lossy(&buf[..n]).into_owned();

        // Verify all URLs are present in order
        for url in urls {
            assert!(received.contains(&url));
        }
    }

    #[test]
    fn terminal_session_records_output_and_snapshots_screen() {
        let (state, _reader) = build_test_state();
        let session = state.session;

        session.record(b"hello");

        let snapshot = String::from_utf8(session.snapshot()).expect("snapshot should be utf8");
        assert!(snapshot.contains("hello"));
        assert!(session.initial_frame().starts_with(b"\x1bc"));
    }

    #[test]
    fn terminal_session_broadcasts_recorded_bytes() {
        let (state, _reader) = build_test_state();
        let session = state.session;
        let mut rx = session.subscribe();

        session.record(b"abc");

        let chunk = rx.try_recv().expect("broadcast should receive bytes");
        assert_eq!(chunk, b"abc");
    }

    #[test]
    fn parse_forwarded_param_handles_quotes_and_multiple_entries() {
        let header = r#"for=192.0.2.1;proto=http, for=192.0.2.2;proto=https;host="octo.example""#;

        assert_eq!(
            parse_forwarded_param(header, "host"),
            Some("octo.example".to_string())
        );
        assert_eq!(
            parse_forwarded_param(header, "proto"),
            Some("http".to_string())
        );
        assert_eq!(parse_forwarded_param(header, "missing"), None);
    }

    #[test]
    fn infer_scheme_prefers_forwarded_proto_then_port() {
        let (mut state, _reader) = build_test_state();
        let mut headers = HeaderMap::new();
        headers.insert("forwarded", HeaderValue::from_static("proto=https"));

        assert_eq!(infer_scheme(&headers, &state), "https");

        headers.clear();
        state.port = 443;
        assert_eq!(infer_scheme(&headers, &state), "https");
    }

    #[test]
    fn infer_host_formats_ipv6_fallback_and_prefers_proxy_header() {
        let (mut state, _reader) = build_test_state();
        state.host = "::1".to_string();
        let mut headers = HeaderMap::new();

        assert_eq!(infer_host(&headers, &state, "http"), "[::1]:9723");

        headers.insert(
            "x-forwarded-host",
            HeaderValue::from_static("public.example, internal.example"),
        );
        assert_eq!(infer_host(&headers, &state, "http"), "public.example");
    }

    #[test]
    fn require_api_key_accepts_header_and_bearer_token() {
        let (mut state, _reader) = build_test_state();
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
        let (mut state, _reader) = build_test_state();
        state.api_key = Some("secret".to_string());

        let headers = HeaderMap::new();
        let missing = require_api_key(&state, &headers).expect("missing key should reject");
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);

        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("wrong"));
        let wrong = require_api_key(&state, &headers).expect("wrong key should reject");
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    }
}

#[derive(Clone)]
struct TerminalApiState {
    host: String,
    port: u16,
    session: Arc<TerminalSession>,
    http_client: Arc<reqwest::Client>,
    dlc_cache: Arc<DlcKeyCache>,
    upstream_api_base: Option<String>,
    api_key: Option<String>,
}

struct TerminalSession {
    bridge: Arc<TerminalBridge>,
    parser: Mutex<vt100::Parser>,
    output_tx: broadcast::Sender<Vec<u8>>,
}

impl TerminalSession {
    fn new(bridge: Arc<TerminalBridge>) -> Self {
        let (output_tx, _) = broadcast::channel(256);
        Self {
            bridge,
            parser: Mutex::new(vt100::Parser::new(40, 120, 0)),
            output_tx,
        }
    }

    fn subscribe(&self) -> broadcast::Receiver<Vec<u8>> {
        self.output_tx.subscribe()
    }

    fn snapshot(&self) -> Vec<u8> {
        self.parser.lock().screen().state_formatted()
    }

    fn initial_frame(&self) -> Vec<u8> {
        let mut frame = b"\x1bc".to_vec();
        frame.extend_from_slice(&self.snapshot());
        frame
    }

    fn record(&self, bytes: &[u8]) {
        self.parser.lock().process(bytes);
        let _ = self.output_tx.send(bytes.to_vec());
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

fn infer_scheme(headers: &HeaderMap, state: &TerminalApiState) -> String {
    if let Some(proto) = header_to_str(headers, "x-forwarded-proto") {
        let proto = proto.split(',').next().unwrap_or(proto).trim();
        if matches!(proto, "http" | "https") {
            return proto.to_ascii_lowercase();
        }
    }
    if let Some(forwarded) = header_to_str(headers, "forwarded")
        && let Some(proto) = parse_forwarded_param(forwarded, "proto")
        && matches!(proto.as_str(), "http" | "https")
    {
        return proto.to_ascii_lowercase();
    }
    match state.port {
        443 => "https".to_string(),
        _ => "http".to_string(),
    }
}

fn infer_host(headers: &HeaderMap, state: &TerminalApiState, scheme: &str) -> String {
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
    web::format_script_host(&state.host, state.port, scheme)
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

#[derive(Deserialize)]
struct DlcRequest {
    content: String,
    #[serde(default)]
    filename: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ClientMessage {
    #[serde(rename = "resize")]
    Resize { cols: u16, rows: u16 },
}

#[derive(Serialize)]
struct UrlResponse {
    added: Vec<String>,
    count: usize,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

pub async fn run_terminal_server(
    host: &str,
    port: u16,
    bridge: Arc<TerminalBridge>,
    shutdown: oneshot::Receiver<()>,
    upstream_api_port: Option<u16>,
    api_key: Option<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let http_client = Arc::new(build_http_client()?);
    let dlc_cache = Arc::new(DlcKeyCache::new());
    let session = Arc::new(TerminalSession::new(bridge));
    let reader = session.bridge.try_clone_reader()?;
    let reader_session = session.clone();
    let _reader_task = tokio::task::spawn_blocking(move || {
        let mut reader = reader;
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => reader_session.record(&buf[..n]),
                Err(e) => {
                    log::error!("PTY read error: {e}");
                    break;
                }
            }
        }
    });
    let state = TerminalApiState {
        host: host.to_string(),
        port,
        session,
        http_client,
        dlc_cache,
        upstream_api_base: upstream_api_port.map(|port| format!("http://127.0.0.1:{port}")),
        api_key,
    };

    let app = Router::new()
        .route("/", get(root))
        .route("/ws", get(ws_handler))
        .route("/bookmarklet", get(bookmarklet))
        .route("/manifest.json", get(manifest))
        .route("/sw.js", get(service_worker))
        .route("/icon-192.svg", get(icon))
        .route("/icon-512.svg", get(icon))
        .route("/api/health", get(api_health))
        .route("/api/state", get(proxy_api_get))
        .route("/api/events", get(proxy_api_get))
        .route("/api/login", post(proxy_api_post))
        .route("/api/pause", post(proxy_api_post))
        .route("/api/delete", post(proxy_api_post))
        .route("/api/retry", post(proxy_api_post))
        .route("/api/config", post(proxy_api_post))
        .route(
            "/api/urls",
            post(api_post_urls).layer(DefaultBodyLimit::max(10 * 1024 * 1024)),
        )
        .route(
            "/api/dlc",
            post(api_post_dlc).layer(DefaultBodyLimit::max(10 * 1024 * 1024)),
        )
        .route(
            "/api/parse",
            post(api_parse_page).layer(DefaultBodyLimit::max(10 * 1024 * 1024)),
        )
        .route("/share", get(share_get))
        .route("/share", post(share_post))
        .with_state(state)
        .layer(cors);

    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app.into_make_service())
        .with_graceful_shutdown(async move {
            let _ = shutdown.await;
        })
        .await?;
    Ok(())
}

fn build_http_client() -> Result<reqwest::Client, reqwest::Error> {
    use std::time::Duration;

    reqwest::Client::builder()
        .pool_idle_timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(8)
        .tcp_keepalive(Duration::from_secs(30))
        .build()
}

fn dispatch_urls(state: &TerminalApiState, urls: Vec<String>) {
    if urls.is_empty() {
        return;
    }
    for url in urls {
        let _ = state.session.bridge.write_line(&url);
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

fn require_api_key(
    state: &TerminalApiState,
    headers: &HeaderMap,
) -> Option<axum::response::Response> {
    let expected_key = state.api_key.as_ref()?;
    if provided_api_key(headers).is_some_and(|provided| provided == expected_key) {
        return None;
    }

    Some(
        (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({"error": "invalid api key"})),
        )
            .into_response(),
    )
}

async fn root(State(state): State<TerminalApiState>, headers: HeaderMap) -> impl IntoResponse {
    let scheme = infer_scheme(&headers, &state);
    let ws_scheme = if scheme.eq_ignore_ascii_case("https") {
        "wss".to_string()
    } else {
        "ws".to_string()
    };
    let host = infer_host(&headers, &state, &scheme);
    Html(web::index_html(&host, &ws_scheme))
}

async fn bookmarklet(
    State(state): State<TerminalApiState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let fallback_host = headers
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or(&format!("{}:{}", state.host, state.port))
        .to_string();
    Html(web::bookmarklet_html(&fallback_host))
}

async fn manifest(State(state): State<TerminalApiState>) -> impl IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "application/manifest+json",
        )],
        web::manifest_json(&state.host, state.port),
    )
}

async fn service_worker() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/javascript")],
        web::service_worker_js(),
    )
}

async fn icon() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "image/svg+xml")],
        web::icon_svg(),
    )
}

async fn api_health(State(_state): State<TerminalApiState>) -> impl IntoResponse {
    axum::Json(HealthResponse { status: "ok" })
}

async fn proxy_api_get(
    State(state): State<TerminalApiState>,
    OriginalUri(uri): OriginalUri,
) -> impl IntoResponse {
    proxy_api_request(&state, reqwest::Method::GET, &uri, Bytes::new()).await
}

async fn proxy_api_post(
    State(state): State<TerminalApiState>,
    OriginalUri(uri): OriginalUri,
    body: Bytes,
) -> impl IntoResponse {
    proxy_api_request(&state, reqwest::Method::POST, &uri, body).await
}

async fn proxy_api_request(
    state: &TerminalApiState,
    method: reqwest::Method,
    uri: &axum::http::Uri,
    body: Bytes,
) -> axum::response::Response {
    let Some(base) = &state.upstream_api_base else {
        return (StatusCode::SERVICE_UNAVAILABLE, "API server not available").into_response();
    };
    let url = format!("{base}{uri}");
    let response = match state
        .http_client
        .request(method, url)
        .body(body)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("API proxy request failed: {error}"),
            )
                .into_response();
        }
    };
    let status = response.status();
    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .cloned();
    let mut proxied = axum::response::Response::builder().status(status);
    if let Some(content_type) = content_type {
        proxied = proxied.header(axum::http::header::CONTENT_TYPE, content_type);
    }
    proxied
        .body(Body::from_stream(response.bytes_stream()))
        .unwrap_or_else(|error| {
            (
                StatusCode::BAD_GATEWAY,
                format!("API proxy response failed: {error}"),
            )
                .into_response()
        })
}

async fn api_post_urls(
    State(state): State<TerminalApiState>,
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

async fn api_post_dlc(
    State(state): State<TerminalApiState>,
    headers: HeaderMap,
    axum::Json(payload): axum::Json<DlcRequest>,
) -> axum::response::Response {
    if let Some(response) = require_api_key(&state, &headers) {
        return response;
    }

    match parse_dlc_data(&payload.content, &state.http_client, &state.dlc_cache).await {
        Ok(urls) => {
            let count = urls.len();
            if let Some(name) = payload.filename {
                log::info!("DLC upload received from {name}: {count} link(s)");
            } else {
                log::info!("DLC upload received ({count} link(s))");
            }
            dispatch_urls(&state, urls.clone());
            axum::Json(UrlResponse { added: urls, count }).into_response()
        }
        Err(err) => (StatusCode::BAD_REQUEST, err.to_string()).into_response(),
    }
}

async fn api_parse_page(
    State(state): State<TerminalApiState>,
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

async fn share_get(
    State(state): State<TerminalApiState>,
    axum::extract::Query(params): axum::extract::Query<ShareParams>,
) -> impl IntoResponse {
    dispatch_urls(&state, extract_urls(&params.combined()));
    axum::response::Redirect::to("/")
}

async fn share_post(
    State(state): State<TerminalApiState>,
    axum::Form(params): axum::Form<ShareParams>,
) -> impl IntoResponse {
    dispatch_urls(&state, extract_urls(&params.combined()));
    axum::response::Redirect::to("/")
}

#[derive(Deserialize)]
struct ShareParams {
    #[serde(default)]
    title: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    url: String,
}

impl ShareParams {
    fn combined(&self) -> String {
        format!("{} {} {}", self.title, self.text, self.url)
    }
}

async fn ws_handler(
    State(state): State<TerminalApiState>,
    upgrade: WebSocketUpgrade,
) -> impl IntoResponse {
    upgrade.on_upgrade(move |socket| handle_ws(state, socket))
}

async fn handle_ws(state: TerminalApiState, ws: WebSocket) {
    let bridge = state.session.bridge.clone();
    let mut output_rx = state.session.subscribe();
    let (mut sink, mut stream) = ws.split();
    let (write_tx, mut write_rx) = mpsc::unbounded_channel::<Message>();
    let _ = write_tx.send(Message::Binary(state.session.initial_frame().into()));

    let writer = tokio::spawn(async move {
        while let Some(msg) = write_rx.recv().await {
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    let reader_tx = write_tx.clone();
    let fanout = tokio::spawn(async move {
        loop {
            match output_rx.recv().await {
                Ok(chunk) => {
                    if reader_tx.send(Message::Binary(chunk.into())).is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    while let Some(msg) = stream.next().await {
        match msg {
            Ok(Message::Text(text)) => match serde_json::from_str::<ClientMessage>(&text) {
                Ok(ClientMessage::Resize { cols, rows }) => {
                    if let Err(error) = bridge.resize(cols, rows) {
                        log::warn!("Failed to resize PTY: {error}");
                    }
                }
                Err(_) => {
                    let _ = bridge.write(text.as_bytes());
                }
            },
            Ok(Message::Binary(data)) => {
                let _ = bridge.write(&data);
            }
            Ok(Message::Ping(payload)) => {
                let _ = write_tx.send(Message::Pong(payload));
            }
            Ok(Message::Close(_)) | Err(_) => break,
            _ => {}
        }
    }

    drop(write_tx);
    let _ = writer.await;
    let _ = fanout.await;
}
