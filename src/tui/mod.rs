//! octo-tui - Interactive TUI for downloading MEGA files.

mod api;
mod app;
mod download;
mod draw;
mod event;
mod input;
mod session;
mod terminal;
mod terminal_server;
mod visible;
pub mod web;

use std::io;
use std::path::Path;
use tokio::sync::mpsc;

use self::api::DEFAULT_API_PORT;
use self::app::App;
use self::event::DownloadEvent;

/// Options for the web UI server.
///
/// Passed to [`api::run_api_server`] when `--web` is enabled so that the
/// bookmarklet page and PWA manifest use the correct publicly-reachable host
/// (which may differ from the bind address when behind a reverse proxy).
#[derive(Clone)]
pub struct WebOptions {
    pub public_host: String,
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Run the interactive terminal TUI.
///
/// If `api_host` is `Some`, the HTTP API server is started. When the inner
/// `Option` is `None`, the host from config (or default) is used. When `Some(host)`,
/// that explicit host is used.
/// When `web` is true the web UI is served alongside the API.
/// When `config_path` is provided, credentials and download settings are
/// loaded from the config file.
///
/// # Errors
/// Returns an error if terminal setup fails or TUI operations encounter I/O errors.
#[allow(clippy::too_many_lines, clippy::unused_async)]
pub async fn run(
    api_host: Option<Option<String>>,
    web: bool,
    config_path: Option<&Path>,
) -> io::Result<()> {
    let (download_tx, mut download_rx) = mpsc::unbounded_channel::<DownloadEvent>();

    let (mut app, api_bind_host, api_port) =
        App::new_with_optional_service_config(download_tx, true, config_path, DEFAULT_API_PORT)?;

    let app::SharedStateChannels {
        mut action_rx,
        state_tx,
        shared_state,
    } = app.shared_state_channels(web);

    // Start the API server (if enabled)
    if let Some(explicit_host) = api_host {
        let host = explicit_host.unwrap_or(api_bind_host);
        let web_opts = if web {
            Some(WebOptions {
                public_host: host.clone(),
            })
        } else {
            None
        };
        app.spawn_api_server(host, api_port, web_opts, shared_state);
    }

    app.prepare_interactive_startup();

    terminal::run_interactive_tui(&mut app, &mut download_rx, &mut action_rx, &state_tx, web).await
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
pub async fn run_api_only(config_path: &Path) -> io::Result<()> {
    let (download_tx, mut download_rx) = mpsc::unbounded_channel::<DownloadEvent>();
    let (mut app, api_host, api_port) = App::new_with_optional_service_config(
        download_tx,
        true,
        Some(config_path),
        DEFAULT_API_PORT,
    )?;
    app.prepare_headless_startup(config_path)?;

    // Start the API server (headless — no web UI)
    log::info!("Starting API server on {api_host}:{api_port}");
    app.spawn_api_server(api_host, api_port, None, None);

    log::info!("Entering headless event loop");
    app.run_headless_until_shutdown(&mut download_rx, terminal::wait_for_shutdown_signal())
        .await;

    app.sync_session_for_shutdown();
    log::info!("Shutdown complete");
    Ok(())
}

/// Run the web TUI as the primary interface (no terminal TUI).
///
/// Starts the API + web UI server and an event loop that processes
/// download events and web UI actions until SIGTERM/SIGINT.  The user
/// logs in and manages downloads through the browser.
///
/// When `config_path` is provided, credentials and download settings
/// are loaded from the config file (same as `--api` mode).  Otherwise
/// the user logs in via the web UI.
///
/// # Errors
/// Returns an error if server startup fails or I/O operations fail.
///
pub async fn run_web(api_host: &str, config_path: Option<&Path>) -> io::Result<()> {
    terminal::run_terminal_web_mode(api_host, config_path, DEFAULT_API_PORT).await
}

#[cfg(test)]
mod tests;
