//! Unified octo binary supporting CLI, TUI, and API modes.

use std::path::PathBuf;
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "octo",
    version,
    about = "MEGA file downloader with CLI, TUI, and API modes"
)]
struct Cli {
    /// Launch TUI mode
    #[arg(long)]
    tui: bool,

    /// Enable API server
    #[arg(long)]
    api: bool,

    /// MEGA URLs or .dlc files (if not in TUI mode)
    urls: Vec<String>,

    /// Download directory
    #[arg(long, env = "OCTO_DOWNLOAD_DIR")]
    download_dir: Option<PathBuf>,

    /// Config directory
    #[arg(long, env = "OCTO_CONFIG_DIR")]
    config_dir: Option<PathBuf>,

    /// State directory (for session files)
    #[arg(long, env = "OCTO_STATE_DIR")]
    state_dir: Option<PathBuf>,

    /// API server host
    #[arg(long, env = "OCTO_API_HOST", default_value = "127.0.0.1")]
    api_host: String,

    /// API server port
    #[arg(long, env = "OCTO_API_PORT", default_value = "9723")]
    api_port: u16,

    /// Chunks per file for parallel download
    #[arg(short = 'j', long, default_value = "2")]
    chunks: usize,

    /// Concurrent file downloads
    #[arg(short = 'p', long, default_value = "4")]
    parallel: usize,

    /// Overwrite existing files
    #[arg(short, long)]
    force: bool,

    /// Resume previous session
    #[arg(short, long)]
    resume: bool,
}

#[tokio::main]
async fn main() -> octo::Result<()> {
    env_logger::init();

    let cli = Cli::parse();

    // Load base config
    let mut config = octo::AppConfig::load()?;

    // Apply CLI overrides
    if let Some(dir) = cli.download_dir {
        config.paths.download_dir = dir;
    }
    if let Some(dir) = cli.config_dir {
        config.paths.config_dir = dir;
    }
    if let Some(dir) = cli.state_dir {
        config.paths.state_dir = dir;
    }

    config.download.chunks_per_file = cli.chunks;
    config.download.concurrent_files = cli.parallel;
    config.download.force_overwrite = cli.force;
    config.api.enabled = cli.api;
    config.api.host = cli.api_host;
    config.api.port = cli.api_port;

    // Determine mode based on flags
    match (cli.tui, cli.api, cli.urls.is_empty(), cli.resume) {
        // TUI mode (with or without API)
        #[cfg(feature = "tui")]
        (true, _, _, _) => {
            octo::tui::run(config).await?;
            Ok(())
        }
        #[cfg(not(feature = "tui"))]
        (true, _, _, _) => {
            eprintln!("Error: TUI mode not available (compiled without 'tui' feature)");
            std::process::exit(1);
        }

        // API-only mode (no TUI, API enabled, no URLs, not resuming)
        #[cfg(feature = "api")]
        (false, true, true, false) => {
            octo::api::run_standalone(config).await
        }
        #[cfg(not(feature = "api"))]
        (false, true, true, false) => {
            eprintln!("Error: API mode not available (compiled without 'api' feature)");
            std::process::exit(1);
        }

        // CLI mode (with or without API, may have URLs or be resuming)
        #[cfg(feature = "cli")]
        (false, _, _, _) => {
            octo::cli::run_download(config, cli.urls, cli.resume).await
        }
        #[cfg(not(feature = "cli"))]
        (false, _, _, _) => {
            eprintln!("Error: CLI mode not available (compiled without 'cli' feature)");
            std::process::exit(1);
        }
    }
}
