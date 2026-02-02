use std::env;
use std::path::PathBuf;

/// Flags that consume the next argument as a value (not a positional arg).
const FLAGS_WITH_VALUES: &[&str] = &["--api-host", "--config"];

/// Returns true if `args` contains positional arguments (URLs, DLC paths, etc.)
/// as opposed to just flags and their values.
fn has_positional_args(args: &[String]) -> bool {
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if FLAGS_WITH_VALUES.contains(&arg) {
            i += 2; // skip flag + its value
        } else if arg.starts_with('-') {
            i += 1; // skip bare flag
        } else {
            return true; // positional arg found
        }
    }
    false
}

fn print_usage() {
    eprintln!("Usage: octo [MODE] [OPTIONS] [url|dlc]...");
    eprintln!();
    eprintln!("Modes:");
    eprintln!("  --tui               Launch interactive TUI");
    eprintln!("  --api               Start HTTP API server (combinable with --tui or standalone)");
    eprintln!("  (default)           CLI download mode when URLs/DLC files are provided");
    eprintln!();
    eprintln!("Global options:");
    eprintln!("  --api-host <HOST>   API server bind address (default: 127.0.0.1)");
    eprintln!("  --config <PATH>     Config file for headless/service mode");
    eprintln!("  -h, --help          Show this help");
    eprintln!();
    eprintln!("Run 'octo --tui --help' or 'octo --help' for mode-specific options.");
}

#[tokio::main]
async fn main() -> octo_dl::Result<()> {
    env_logger::init();

    let mut tui = false;
    let mut api = false;
    let mut api_host = "127.0.0.1".to_string();
    let mut config_path: Option<PathBuf> = None;

    // Scan for global flags without consuming — sub-modules re-parse for their own flags
    let args: Vec<String> = env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--tui" => tui = true,
            "--api" => api = true,
            "--api-host" => {
                i += 1;
                if i < args.len() {
                    api_host = args[i].clone();
                } else {
                    eprintln!("Error: --api-host requires a value");
                    std::process::exit(1);
                }
            }
            "--config" => {
                i += 1;
                if i < args.len() {
                    config_path = Some(PathBuf::from(&args[i]));
                } else {
                    eprintln!("Error: --config requires a path");
                    std::process::exit(1);
                }
            }
            _ => {} // sub-module flags, URLs, etc.
        }
        i += 1;
    }

    let api_host = if api { Some(api_host) } else { None };

    if tui {
        #[cfg(feature = "tui")]
        {
            octo_dl::tui::run(api_host)
                .await
                .map_err(octo_dl::Error::Io)
        }
        #[cfg(not(feature = "tui"))]
        {
            let _ = api_host;
            eprintln!("TUI support not compiled in");
            std::process::exit(1);
        }
    } else if api && !has_positional_args(&args) {
        // --api with no URLs/DLC = API-only mode (headless)
        let config = config_path.unwrap_or_else(|| {
            eprintln!("Error: --api mode requires --config <PATH>");
            std::process::exit(1);
        });
        #[cfg(feature = "tui")]
        {
            octo_dl::tui::run_api_only(&config)
                .await
                .map_err(octo_dl::Error::Io)
        }
        #[cfg(not(feature = "tui"))]
        {
            let _ = config;
            eprintln!("API support requires the 'tui' feature");
            std::process::exit(1);
        }
    } else {
        // CLI mode — check if there are any positional args (URLs/DLC)
        let has_positional = has_positional_args(&args);
        if !has_positional && !args.iter().any(|a| a == "-r" || a == "--resume") {
            // No URLs, no --resume, and not TUI/API — show help
            if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
                print_usage();
                std::process::exit(0);
            }
        }

        #[cfg(feature = "cli")]
        {
            octo_dl::cli::run().await
        }
        #[cfg(not(feature = "cli"))]
        {
            eprintln!("CLI support not compiled in");
            std::process::exit(1);
        }
    }
}
