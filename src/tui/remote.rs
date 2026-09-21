use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use futures_util::StreamExt as _;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::task::JoinHandle;
use tokio::time::timeout;
#[cfg(test)]
use tokio_tungstenite::tungstenite::http::HeaderMap;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        Error as WebSocketError, Message,
        client::IntoClientRequest,
        http::{HeaderValue, Request},
    },
};

use super::dashboard::{
    AttachedDashboard, DashboardChrome, DashboardUiMode, DownloadDashboardState,
};
use super::draw::draw_dashboard;
use super::terminal::wait_for_shutdown_signal;
use super::terminal_support::{
    TerminalGuard, TerminalInputError, TerminalPanicHookGuard, finish_terminal_lifecycle,
    terminal_input_channel,
};
use tokio_util::sync::CancellationToken;

const DASHBOARD_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const DASHBOARD_READER_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(250);

enum DashboardReaderMessage {
    State(DownloadDashboardState),
    Status(String),
    Fatal {
        kind: io::ErrorKind,
        message: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttachConfig {
    pub api_key: Option<String>,
}

impl AttachConfig {
    #[must_use]
    pub fn from_api_key(api_key: Option<String>) -> Self {
        Self { api_key }
    }
}

#[must_use]
pub fn parse_loopback_addr(value: &str) -> Result<SocketAddr, String> {
    let addr = value
        .parse::<SocketAddr>()
        .map_err(|error| format!("invalid socket address {value:?}: {error}"))?;
    if !addr.ip().is_loopback() {
        return Err(format!(
            "{value:?} is not loopback-only; use 127.0.0.1 or ::1"
        ));
    }
    Ok(addr)
}

#[must_use]
pub fn socket_host(addr: SocketAddr) -> String {
    addr.ip().to_string()
}

pub async fn run_attached_dashboard(addr: SocketAddr, config: AttachConfig) -> io::Result<()> {
    let panic_hook_guard = TerminalPanicHookGuard::install();
    let guard = TerminalGuard::new()?;
    let result = run_attached_dashboard_loop(addr, config).await;
    drop(panic_hook_guard);
    finish_terminal_lifecycle(result, guard)
}

async fn run_attached_dashboard_loop(addr: SocketAddr, config: AttachConfig) -> io::Result<()> {
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut app = AttachedDashboard {
        status: format!("Connecting to {addr}"),
        ..AttachedDashboard::default()
    };
    let mut input = terminal_input_channel();
    let attach_api_key = config.api_key.clone();
    let mut dashboard_reader = spawn_dashboard_reader(addr, config);
    let loop_result = async {
        let mut dashboard_error = None;
        let shutdown = wait_for_shutdown_signal();
        tokio::pin!(shutdown);
        terminal.draw(|frame| {
            if let Some(state) = &app.state {
                draw_dashboard(
                    frame,
                    state,
                    &DashboardChrome::read_only(),
                    &mut app.list_state,
                );
            } else {
                let mut state = DownloadDashboardState::empty(
                    DashboardUiMode::Attached,
                    true,
                    &app.status,
                    addr.port(),
                );
                state.status = app.status.clone();
                draw_dashboard(
                    frame,
                    &state,
                    &DashboardChrome::read_only(),
                    &mut app.list_state,
                );
            }
        })?;

        loop {
            tokio::select! {
                () = &mut shutdown => app.should_quit = true,
                input_result = input.recv_result() => {
                    if let Err(error) = handle_attached_input_result(
                        &mut app,
                        input_result,
                        addr,
                        attach_api_key.clone(),
                        dashboard_reader.status_tx.clone(),
                    ) {
                        dashboard_error = Some(error);
                    }
                }
                message = dashboard_reader.receiver.recv() => match message {
                    Some(DashboardReaderMessage::Fatal { kind, message }) => {
                        app.status = message.clone();
                        dashboard_error = Some(io::Error::new(kind, message));
                        app.should_quit = true;
                    }
                    Some(message) => handle_dashboard_reader_message(&mut app, message),
                    None => {
                        let error = io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "dashboard reader stopped unexpectedly",
                        );
                        app.status = error.to_string();
                        dashboard_error = Some(error);
                        app.should_quit = true;
                    }
                },
            }

            if app.should_quit {
                break;
            }

            terminal.draw(|frame| {
                if let Some(state) = &app.state {
                    draw_dashboard(
                        frame,
                        state,
                        &DashboardChrome::read_only(),
                        &mut app.list_state,
                    );
                } else {
                    let mut state = DownloadDashboardState::empty(
                        DashboardUiMode::Attached,
                        true,
                        &app.status,
                        addr.port(),
                    );
                    state.status = app.status.clone();
                    draw_dashboard(
                        frame,
                        &state,
                        &DashboardChrome::read_only(),
                        &mut app.list_state,
                    );
                }
            })?;
        }

        terminal.show_cursor()?;
        dashboard_error.map_or(Ok(()), Err)
    }
    .await;

    let reader_result = dashboard_reader.shutdown().await;
    match (loop_result, reader_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(loop_error), Err(reader_error)) => Err(io::Error::other(format!(
            "{loop_error}; dashboard reader cleanup failed: {reader_error}"
        ))),
    }
}

struct DashboardReader {
    receiver: tokio::sync::mpsc::UnboundedReceiver<DashboardReaderMessage>,
    status_tx: tokio::sync::mpsc::UnboundedSender<DashboardReaderMessage>,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl DashboardReader {
    async fn shutdown(self) -> io::Result<()> {
        self.cancel.cancel();
        let mut task = self.task;
        match timeout(DASHBOARD_READER_SHUTDOWN_TIMEOUT, &mut task).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(io::Error::other(format!(
                "dashboard reader task failed: {error}"
            ))),
            Err(_) => {
                task.abort();
                let _ = task.await;
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "dashboard reader did not shut down before the deadline",
                ))
            }
        }
    }
}

fn spawn_dashboard_reader(addr: SocketAddr, config: AttachConfig) -> DashboardReader {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let status_tx = tx.clone();
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        let ws_url = format!("ws://{addr}/api/dashboard");
        while !task_cancel.is_cancelled() {
            if tx
                .send(DashboardReaderMessage::Status(format!(
                    "Connecting to {addr}"
                )))
                .is_err()
            {
                break;
            }
            match dashboard_reader_session(&ws_url, &config, &tx, &task_cancel).await {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                    let _ = tx.send(DashboardReaderMessage::Fatal {
                        kind: error.kind(),
                        message: error.to_string(),
                    });
                    break;
                }
                Err(error) => {
                    if tx
                        .send(DashboardReaderMessage::Status(format!(
                            "Disconnected from {addr}: {error}; reconnecting in 1s"
                        )))
                        .is_err()
                    {
                        break;
                    }
                }
            }
            tokio::select! {
                _ = task_cancel.cancelled() => break,
                _ = tokio::time::sleep(DASHBOARD_RECONNECT_DELAY) => {}
            }
        }
    });
    DashboardReader {
        receiver: rx,
        status_tx,
        cancel,
        task,
    }
}

async fn dashboard_reader_session(
    ws_url: &str,
    config: &AttachConfig,
    tx: &tokio::sync::mpsc::UnboundedSender<DashboardReaderMessage>,
    cancel: &CancellationToken,
) -> io::Result<()> {
    let request = dashboard_request(ws_url, config)?;
    let (mut socket, _) = tokio::select! {
        _ = cancel.cancelled() => return Ok(()),
        result = connect_async(request) => result
            .map_err(|error| dashboard_connection_error(error, config.api_key.is_some()))?,
    };
    let _ = tx.send(DashboardReaderMessage::Status("Connected".to_string()));

    loop {
        let Some(message) = (tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            message = socket.next() => message,
        }) else {
            break;
        };
        let message = message.map_err(|error| io::Error::other(error.to_string()))?;
        let Some(mut state) = dashboard_state_from_message(message)? else {
            continue;
        };
        state.ui_mode = DashboardUiMode::Attached;
        tx.send(DashboardReaderMessage::State(state))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "dashboard receiver closed"))?;
    }
    Err(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "dashboard websocket closed",
    ))
}

fn handle_attached_input_result(
    app: &mut AttachedDashboard,
    result: Result<Event, TerminalInputError>,
    addr: SocketAddr,
    api_key: Option<String>,
    status_tx: tokio::sync::mpsc::UnboundedSender<DashboardReaderMessage>,
) -> io::Result<()> {
    match result {
        Ok(event) => {
            handle_attached_input(app, event, addr, api_key, status_tx);
            Ok(())
        }
        Err(error) => {
            let error = error.into_io_error();
            app.status = format!("Attached dashboard input failed: {error}");
            app.should_quit = true;
            Err(error)
        }
    }
}

fn dashboard_request(ws_url: &str, config: &AttachConfig) -> io::Result<Request<()>> {
    let mut request = ws_url
        .into_client_request()
        .map_err(|error| io::Error::other(error.to_string()))?;
    if let Some(api_key) = config.api_key.as_deref() {
        let header = HeaderValue::from_str(api_key).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid API key for dashboard attach: {error}"),
            )
        })?;
        request.headers_mut().insert("x-api-key", header);
    }
    Ok(request)
}

fn dashboard_connection_error(error: WebSocketError, api_key_supplied: bool) -> io::Error {
    if let WebSocketError::Http(response) = &error
        && matches!(response.status().as_u16(), 401 | 403)
    {
        let message = if api_key_supplied {
            "dashboard authentication failed: the supplied API key was rejected"
        } else {
            "dashboard authentication required; supply --api-key or configure api.api_key"
        };
        return io::Error::new(io::ErrorKind::PermissionDenied, message);
    }
    io::Error::other(error.to_string())
}

fn dashboard_state_from_message(message: Message) -> io::Result<Option<DownloadDashboardState>> {
    match message {
        Message::Text(text) => serde_json::from_str(text.as_str())
            .map(Some)
            .map_err(|error| io::Error::other(error.to_string())),
        Message::Binary(bytes) => super::dashboard::dashboard_state_from_postcard(&bytes)
            .map(Some)
            .map_err(|error| io::Error::other(error.to_string())),
        Message::Close(_) => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "dashboard websocket closed",
        )),
        _ => Ok(None),
    }
}

fn handle_dashboard_reader_message(app: &mut AttachedDashboard, message: DashboardReaderMessage) {
    match message {
        DashboardReaderMessage::State(state) => {
            app.replace_state(state);
            app.status.clear();
        }
        DashboardReaderMessage::Status(status) => {
            app.status = status;
            if let Some(state) = app.state.as_mut() {
                state.status = app.status.clone();
                state.ui_mode = DashboardUiMode::Attached;
            }
        }
        DashboardReaderMessage::Fatal { message, .. } => {
            app.status = message;
            app.should_quit = true;
        }
    }
}

fn handle_attached_input(
    app: &mut AttachedDashboard,
    event: Event,
    addr: SocketAddr,
    api_key: Option<String>,
    status_tx: tokio::sync::mpsc::UnboundedSender<DashboardReaderMessage>,
) {
    let Event::Key(KeyEvent {
        code, modifiers, ..
    }) = event
    else {
        return;
    };
    if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
        app.should_quit = true;
        return;
    }
    match code {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Char('p') => spawn_remote_action(addr, "pause", None, api_key, app, status_tx),
        KeyCode::Char('d') | KeyCode::Delete => {
            spawn_remote_action(addr, "delete", selected_id(app), api_key, app, status_tx);
        }
        KeyCode::Char('r') if modifiers.contains(KeyModifiers::ALT) => {
            spawn_remote_action(addr, "reverify", selected_id(app), api_key, app, status_tx);
        }
        KeyCode::Char('r') => {
            spawn_remote_action(addr, "retry", selected_id(app), api_key, app, status_tx);
        }
        KeyCode::Char('R') => {
            spawn_remote_action(addr, "reset", selected_id(app), api_key, app, status_tx);
        }
        KeyCode::Up | KeyCode::Char('k') => app.select_delta(-1),
        KeyCode::Down | KeyCode::Char('j') => app.select_delta(1),
        KeyCode::PageUp => app.select_delta(-10),
        KeyCode::PageDown => app.select_delta(10),
        KeyCode::Home | KeyCode::Char('g') => {
            if app
                .state
                .as_ref()
                .is_some_and(|state| !state.rows.is_empty())
            {
                app.list_state.select(Some(0));
            }
        }
        KeyCode::End | KeyCode::Char('G') => {
            if let Some(state) = &app.state
                && !state.rows.is_empty()
            {
                app.list_state.select(Some(state.rows.len() - 1));
            }
        }
        _ => {}
    }
}

fn selected_id(app: &AttachedDashboard) -> Option<String> {
    let row = app.state.as_ref()?.rows.get(app.list_state.selected()?)?;
    match row {
        super::dashboard::DashboardRow::Package { package_id } => Some(package_id.clone()),
        super::dashboard::DashboardRow::File { file_id, .. } => Some(file_id.clone()),
    }
}

fn spawn_remote_action(
    addr: SocketAddr,
    action: &'static str,
    id: Option<String>,
    api_key: Option<String>,
    app: &mut AttachedDashboard,
    status_tx: tokio::sync::mpsc::UnboundedSender<DashboardReaderMessage>,
) {
    if action != "pause" && id.is_none() {
        app.status = "Select a row first".to_string();
        return;
    }
    app.status = format!("Sending {action}");
    let url = format!("http://{addr}/api/{action}");
    tokio::spawn(async move {
        let client = reqwest::Client::new();
        let request = api_key.as_deref().map_or_else(
            || client.post(&url),
            |key| client.post(&url).header("x-api-key", key),
        );
        let result = match id {
            Some(id) => request.json(&serde_json::json!({ "id": id })).send().await,
            None => request.send().await,
        };
        let status = match result {
            Ok(response) if response.status().is_success() => {
                format!("{action} accepted")
            }
            Ok(response) => format!("{action} failed: HTTP {}", response.status()),
            Err(error) => {
                log::error!("remote TUI action failed: {error}");
                format!("{action} failed: {error}")
            }
        };
        let _ = status_tx.send(DashboardReaderMessage::Status(status));
    });
}

#[cfg(test)]
fn api_headers(api_key: Option<&str>) -> io::Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    if let Some(api_key) = api_key {
        let value = HeaderValue::from_str(api_key)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
        headers.insert("x-api-key", value);
    }
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::tungstenite::Message;

    #[test]
    fn loopback_validation_rejects_non_loopback_listeners() {
        assert!(parse_loopback_addr("127.0.0.1:9723").is_ok());
        assert!(parse_loopback_addr("[::1]:9723").is_ok());
        assert!(parse_loopback_addr("0.0.0.0:9723").is_err());
        assert!(parse_loopback_addr("192.168.1.10:9723").is_err());
    }

    #[test]
    fn api_headers_include_the_attach_api_key() {
        let headers = api_headers(Some("secret")).expect("valid API key should make a header");
        assert_eq!(headers.get("x-api-key").unwrap(), "secret");
    }

    #[test]
    fn dashboard_state_from_text_message_parses_json_snapshot() {
        let state = DownloadDashboardState::empty(DashboardUiMode::Attached, true, "ready", 9723);
        let message = Message::Text(serde_json::to_string(&state).unwrap().into());

        let parsed = dashboard_state_from_message(message)
            .expect("message should parse")
            .expect("text message should produce state");

        assert_eq!(parsed.status, "ready");
        assert!(parsed.read_only);
    }

    #[test]
    fn dashboard_state_from_binary_message_parses_postcard_snapshot() {
        let state =
            DownloadDashboardState::empty(DashboardUiMode::Attached, false, "binary ready", 9723);
        let message = Message::Binary(
            crate::tui::dashboard::dashboard_state_to_postcard(state)
                .unwrap()
                .into(),
        );

        let parsed = dashboard_state_from_message(message)
            .expect("message should parse")
            .expect("binary message should produce state");

        assert_eq!(parsed.status, "binary ready");
        assert!(!parsed.read_only);
    }
}
