//! TUI mode for octo - interactive terminal user interface.

use std::io;
use crate::AppConfig;

/// Runs the TUI mode with the given configuration.
///
/// # Errors
///
/// Returns an error if the TUI cannot be initialized or run.
pub async fn run(config: AppConfig) -> io::Result<()> {
    // TODO: Implement TUI mode with module structure from src/bin/octo-tui/
    log::info!("TUI mode not yet fully implemented");
    log::info!("Download dir: {}", config.paths.download_dir.display());
    log::info!("State dir: {}", config.paths.state_dir.display());
    Ok(())
}
