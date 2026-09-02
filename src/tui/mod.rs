//! octo-tui - Interactive TUI for downloading MEGA files.

mod api;
mod app;
mod bookmarklet;
mod dashboard;
mod download;
mod draw;
mod event;
mod input;
mod remote;
mod session;
mod terminal;
mod terminal_support;
mod visible;

use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

use self::api::DEFAULT_API_PORT;
use self::app::App;
pub use self::dashboard::{DashboardUiMode, DownloadDashboardState};
use self::event::DownloadEvent;
use crate::ServiceConfig;

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Run the interactive terminal TUI.
///
/// If `api_host` is `Some`, the HTTP API server is started. When the inner
/// `Option` is `None`, the host from config (or default) is used. When `Some(host)`,
/// that explicit host is used.
/// When `tui_listen` is set the remote TUI stream is published alongside the API.
/// When `config_path` is provided, credentials and download settings are
/// loaded from the config file.
///
/// # Errors
/// Returns an error if terminal setup fails or TUI operations encounter I/O errors.
#[allow(clippy::too_many_lines, clippy::unused_async)]
pub async fn run(
    api_host: Option<Option<String>>,
    config_path: Option<&Path>,
    tui_listen: Option<SocketAddr>,
) -> io::Result<()> {
    let (download_tx, mut download_rx) = mpsc::unbounded_channel::<DownloadEvent>();

    let (mut app, api_bind_host, api_port) =
        App::new_with_optional_service_config(download_tx, true, config_path, DEFAULT_API_PORT)?;

    let api_enabled = api_host.is_some() || tui_listen.is_some();
    let app::SharedStateChannels {
        mut action_rx,
        state_tx,
        shared_state,
    } = app.shared_state_channels(api_enabled, DashboardUiMode::Tui);

    // Start the API server (if enabled)
    if let Some(listen) = tui_listen {
        let host = remote::socket_host(listen);
        app.api_port = listen.port();
        app.spawn_api_server(
            host.clone(),
            listen.port(),
            Some(host.clone()),
            shared_state,
            true,
        );
    } else if let Some(explicit_host) = api_host {
        let host = explicit_host.unwrap_or(api_bind_host);
        app.spawn_api_server(host.clone(), api_port, Some(host), shared_state, false);
    }

    app.prepare_interactive_startup();

    terminal::run_interactive_tui(
        &mut app,
        &mut download_rx,
        &mut action_rx,
        &state_tx,
        api_enabled,
    )
    .await
}

/// Run the API server in headless mode (no TUI, no CLI).
///
/// Loads configuration from `config_path`, encrypts plaintext credentials
/// in-place, starts the API server, auto-logs in, and runs an event loop
/// that processes download events until SIGTERM/SIGINT.
///
/// # Errors
/// Returns an error if configuration loading fails, server startup fails, or I/O operations fail.
///
pub async fn run_api_only(
    explicit_api_host: Option<Option<String>>,
    config_path: Option<&Path>,
    tui_listen: Option<SocketAddr>,
) -> io::Result<()> {
    let (download_tx, mut download_rx) = mpsc::unbounded_channel::<DownloadEvent>();
    let (mut app, api_host, api_port) =
        App::new_with_optional_service_config(download_tx, true, config_path, DEFAULT_API_PORT)?;
    app.prepare_headless_startup()?;
    let app::SharedStateChannels {
        mut action_rx,
        state_tx,
        shared_state,
    } = app.shared_state_channels(true, DashboardUiMode::Headless);

    // Start the API server (with optional remote TUI publishing)
    if let Some(listen) = tui_listen {
        let host = remote::socket_host(listen);
        app.api_port = listen.port();
        log::info!(
            "Starting API server with remote TUI stream on {host}:{}",
            listen.port()
        );
        app.spawn_api_server(
            host.clone(),
            listen.port(),
            Some(host.clone()),
            shared_state,
            true,
        );
    } else {
        let host = explicit_api_host.flatten().unwrap_or(api_host);
        log::info!("Starting API server on {host}:{api_port}");
        app.spawn_api_server(host.clone(), api_port, Some(host), shared_state, false);
    }

    log::info!("Entering headless event loop");
    app.run_headless_until_shutdown(
        &mut download_rx,
        &mut action_rx,
        Some(&state_tx),
        terminal::wait_for_shutdown_signal(),
    )
    .await;

    app.sync_session_for_shutdown();
    log::info!("Shutdown complete");
    Ok(())
}

/// Attach an interactive terminal UI to a running service.
///
/// # Errors
///
/// Returns an error if terminal setup or remote communication fails.
pub async fn run_attach(addr: SocketAddr, api_key: Option<String>) -> io::Result<()> {
    remote::run_attached_dashboard(addr, api_key).await
}

/// Resolve the credentials used by an attached TUI without creating or
/// modifying the service configuration.
pub fn attach_api_key(config_path: Option<&Path>) -> io::Result<Option<String>> {
    if let Some(key) = std::env::var_os("OCTO_API_KEY").and_then(|value| {
        let value = value.to_string_lossy().trim().to_string();
        (!value.is_empty()).then_some(value)
    }) {
        return Ok(Some(key));
    }

    if let Some(path) = std::env::var_os("OCTO_API_KEY_FILE") {
        let key = std::fs::read_to_string(&path)?.trim().to_string();
        return Ok((!key.is_empty()).then_some(key));
    }

    let path = config_path.map(PathBuf::from).or_else(|| {
        let path = PathBuf::from("config.toml");
        path.exists().then_some(path)
    });
    path.map_or(Ok(None), |path| {
        ServiceConfig::load(&path).map(|config| config.api.api_key)
    })
}

pub fn parse_loopback_addr(value: &str) -> Result<SocketAddr, String> {
    remote::parse_loopback_addr(value)
}

#[cfg(test)]
mod tests;
