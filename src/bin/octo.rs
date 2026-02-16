use std::env;
use std::path::PathBuf;

/// Flags that consume the next argument as a value (not a positional arg).
const FLAGS_WITH_VALUES: &[&str] = &["--api-host", "--config", "--web-host"];

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
    eprintln!("  --tui               Launch interactive terminal TUI");
    eprintln!("  --web               Launch web UI in browser (PWA with mobile share support)");
    eprintln!("  --api               Start headless API server (requires --config)");
    eprintln!("  (default)           CLI download mode when URLs/DLC files are provided");
    eprintln!();
    eprintln!("Combinable:");
    eprintln!("  --tui --api         Terminal TUI with API server");
    eprintln!("  --tui --web         Terminal TUI with web UI alongside");
    eprintln!();
    eprintln!("Global options:");
    eprintln!("  --api-host <HOST>   Bind address (default: 127.0.0.1)");
    eprintln!("  --web-host <HOST>   Public host for PWA manifest (default: same as --api-host)");
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
    let mut web = false;
    let mut api_host = "127.0.0.1".to_string();
    let mut web_host: Option<String> = None;
    let mut config_path: Option<PathBuf> = None;

    // Scan for global flags without consuming — sub-modules re-parse for their own flags
    let args: Vec<String> = env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--tui" => tui = true,
            "--api" => api = true,
            "--web" => web = true,
            "--api-host" => {
                i += 1;
                if i < args.len() {
                    api_host = args[i].clone();
                } else {
                    eprintln!("Error: --api-host requires a value");
                    std::process::exit(1);
                }
            }
            "--web-host" => {
                i += 1;
                if i < args.len() {
                    web_host = Some(args[i].clone());
                } else {
                    eprintln!("Error: --web-host requires a value");
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

    let api_host_str = api_host.clone();
    let web_opts = if web {
        Some(octo_dl::tui::WebOptions {
            public_host: web_host.unwrap_or_else(|| api_host_str.clone()),
        })
    } else {
        None
    };

    if tui {
        // Terminal TUI mode, optionally with --api and/or --web alongside
        let api_host = if api { Some(api_host) } else { None };
        #[cfg(feature = "tui")]
        {
            octo_dl::tui::run(api_host, web_opts)
                .await
                .map_err(octo_dl::Error::Io)
        }
        #[cfg(not(feature = "tui"))]
        {
            let _ = api_host;
            eprintln!("TUI support not compiled in");
            std::process::exit(1);
        }
    } else if web && !has_positional_args(&args) {
        // --web without --tui = web UI as the primary interface
        #[cfg(feature = "tui")]
        {
            octo_dl::tui::run_web(
                &api_host,
                web_opts.expect("web_opts set when web=true"),
                config_path.as_deref(),
            )
            .await
            .map_err(octo_dl::Error::Io)
        }
        #[cfg(not(feature = "tui"))]
        {
            eprintln!("Web UI requires the 'tui' feature");
            std::process::exit(1);
        }
    } else if api && !has_positional_args(&args) {
        // --api without --tui = headless API-only mode, requires --config
        let config = config_path.unwrap_or_else(|| {
            eprintln!("Error: --api mode requires --config <PATH> (or use --web for browser UI)");
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
