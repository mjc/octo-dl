//! CLI mode for octo - downloads files from provided URLs.

use crate::config::AppConfig;
use crate::Result;

/// Runs the CLI download mode with the given URLs.
///
/// # Errors
///
/// Returns an error if the download fails.
pub async fn run_download(
    _config: AppConfig,
    _urls: Vec<String>,
    _resume: bool,
) -> Result<()> {
    // TODO: Implement CLI download mode
    log::info!("CLI mode not yet fully implemented");
    Ok(())
}
