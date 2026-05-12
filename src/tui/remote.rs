use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use futures_util::StreamExt as _;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use super::dashboard::{
    AttachedDashboard, DashboardChrome, DashboardUiMode, DownloadDashboardState,
};
use super::draw::draw_dashboard;
use super::terminal_support::{TerminalGuard, TerminalPanicHookGuard, terminal_input_channel};

const DASHBOARD_RECONNECT_DELAY: Duration = Duration::from_secs(1);

enum DashboardReaderMessage {
    State(DownloadDashboardState),
    Status(String),
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

pub async fn run_attached_dashboard(addr: SocketAddr) -> io::Result<()> {
    let panic_hook_guard = TerminalPanicHookGuard::install();
    let guard = TerminalGuard::new()?;
    let result = run_attached_dashboard_loop(addr).await;
    drop(panic_hook_guard);
    drop(guard);
    result
}

async fn run_attached_dashboard_loop(addr: SocketAddr) -> io::Result<()> {
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut app = AttachedDashboard {
        status: format!("Connecting to {addr}"),
        ..AttachedDashboard::default()
    };
    let mut render_tick = tokio::time::interval(Duration::from_millis(100));
    let mut input = terminal_input_channel();
    let mut dashboard_rx = spawn_dashboard_reader(addr);

    loop {
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
                state.status.clone_from(&app.status);
                draw_dashboard(
                    frame,
                    &state,
                    &DashboardChrome::read_only(),
                    &mut app.list_state,
                );
            }
        })?;

        tokio::select! {
            Some(event) = input.recv() => handle_attached_input(&mut app, event),
            Some(message) = dashboard_rx.recv() => handle_dashboard_reader_message(&mut app, message),
            _ = render_tick.tick() => {}
        }

        if app.should_quit {
            break;
        }
    }

    terminal.show_cursor()?;
    Ok(())
}

fn spawn_dashboard_reader(
    addr: SocketAddr,
) -> tokio::sync::mpsc::UnboundedReceiver<DashboardReaderMessage> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let ws_url = format!("ws://{addr}/api/dashboard");
        loop {
            if tx
                .send(DashboardReaderMessage::Status(format!(
                    "Connecting to {addr}"
                )))
                .is_err()
            {
                break;
            }
            match dashboard_reader_session(&ws_url, &tx).await {
                Ok(()) => {}
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
            tokio::time::sleep(DASHBOARD_RECONNECT_DELAY).await;
        }
    });
    rx
}

async fn dashboard_reader_session(
    ws_url: &str,
    tx: &tokio::sync::mpsc::UnboundedSender<DashboardReaderMessage>,
) -> io::Result<()> {
    let (mut socket, _) = connect_async(ws_url)
        .await
        .map_err(|error| io::Error::other(error.to_string()))?;
    let _ = tx.send(DashboardReaderMessage::Status("Connected".to_string()));

    while let Some(message) = socket.next().await {
        let message = message.map_err(|error| io::Error::other(error.to_string()))?;
        let Some(mut state) = dashboard_state_from_message(message)? else {
            continue;
        };
        state.ui_mode = DashboardUiMode::Attached;
        state.read_only = true;
        tx.send(DashboardReaderMessage::State(state))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "dashboard receiver closed"))?;
    }
    Err(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "dashboard websocket closed",
    ))
}

fn dashboard_state_from_message(message: Message) -> io::Result<Option<DownloadDashboardState>> {
    match message {
        Message::Text(text) => serde_json::from_str(text.as_str())
            .map(Some)
            .map_err(|error| io::Error::other(error.to_string())),
        Message::Binary(bytes) => serde_json::from_slice(&bytes)
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
                state.status.clone_from(&app.status);
                state.ui_mode = DashboardUiMode::Attached;
                state.read_only = true;
            }
        }
    }
}

fn handle_attached_input(app: &mut AttachedDashboard, event: Event) {
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
    fn dashboard_state_from_text_message_parses_json_snapshot() {
        let state = DownloadDashboardState::empty(DashboardUiMode::Attached, true, "ready", 9723);
        let message = Message::Text(serde_json::to_string(&state).unwrap().into());

        let parsed = dashboard_state_from_message(message)
            .expect("message should parse")
            .expect("text message should produce state");

        assert_eq!(parsed.status, "ready");
        assert!(parsed.read_only);
    }
}
