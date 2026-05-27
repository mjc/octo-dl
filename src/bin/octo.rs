use env_logger::Target;
use std::env;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::PathBuf;

/// Flags that consume the next argument as a value (not a positional arg).
const FLAGS_WITH_VALUES: &[&str] = &[
    "--host",
    "--config",
    "--ui",
    "--tui-listen",
    "--tui-attach",
    "-j",
    "--chunks",
    "-p",
    "--parallel",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiMode {
    Headless,
    Tui,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeOptions {
    ui: Option<UiMode>,
    tui_listen: Option<String>,
    tui_attach: Option<String>,
    host: String,
    host_explicit: bool,
    config_path: Option<PathBuf>,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            ui: None,
            tui_listen: None,
            tui_attach: None,
            host: "127.0.0.1".to_string(),
            host_explicit: false,
            config_path: None,
        }
    }
}

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
    eprintln!("  --headless          Start headless API service");
    eprintln!("  --tui --tui-attach ADDR");
    eprintln!("                      Attach a read-only terminal UI to ADDR");
    eprintln!("  --ui tui|headless   Equivalent explicit form for mode selection");
    eprintln!("  (default)           CLI download mode when URLs/DLC files are provided");
    eprintln!();
    eprintln!("Global options:");
    eprintln!("  --tui-listen ADDR   Publish remote TUI attach stream on loopback ADDR");
    eprintln!("  --host <HOST>       Bind address for API server when enabled");
    eprintln!("  --config <PATH>     Config file override for TUI/headless mode");
    eprintln!("                      (default: ./config.toml when present)");
    eprintln!("  -h, --help          Show this help");
    eprintln!();
    eprintln!("Run 'octo --tui --help' or 'octo --help' for mode-specific options.");
}

fn print_tui_usage() {
    eprintln!("Usage: octo --tui [OPTIONS]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --tui               Launch interactive terminal TUI");
    eprintln!("  --host <HOST>       Bind address for the API server when enabled");
    eprintln!("  --tui-listen ADDR   Publish remote TUI attach stream on loopback ADDR");
    eprintln!("  --config <PATH>     Config file override");
    eprintln!("  -h, --help          Show this help");
}

fn print_headless_usage() {
    eprintln!("Usage: octo --headless [OPTIONS]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --headless          Start headless API service");
    eprintln!("  --host <HOST>       Bind address for the API server");
    eprintln!("  --tui-listen ADDR   Publish remote TUI attach stream on loopback ADDR");
    eprintln!("  --config <PATH>     Config file override");
    eprintln!("  -h, --help          Show this help");
}

fn help_requested(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "-h" || arg == "--help")
}

fn startup_log_mode(options: &RuntimeOptions) -> Option<&'static str> {
    match options.ui {
        Some(UiMode::Tui) => None,
        Some(UiMode::Headless) => Some("headless"),
        None => Some("cli"),
    }
}

#[cfg(feature = "tui")]
fn parse_tui_listen(value: &str) -> std::net::SocketAddr {
    octo_dl::tui::parse_loopback_addr(value).unwrap_or_else(|error| {
        eprintln!("Error: {error}");
        std::process::exit(1);
    })
}

#[cfg(not(feature = "tui"))]
fn parse_tui_listen(_value: &str) -> std::net::SocketAddr {
    eprintln!("TUI support not compiled in");
    std::process::exit(1);
}

fn init_logger(options: &RuntimeOptions) {
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("octo_dl=info"));
    if let Some(writer) = log_writer(options) {
        builder.target(Target::Pipe(writer));
    }
    builder.init();
}

fn log_writer(options: &RuntimeOptions) -> Option<Box<dyn Write + Send>> {
    let primary_writer = match options.ui {
        Some(UiMode::Headless) => Some(headless_log_writer()),
        Some(UiMode::Tui) if native_tui_log_detachment_required(options) => {
            Some(native_tui_log_writer())
        }
        _ => None,
    };
    let debug_file_writer = (debug_logging_enabled()
        && native_tui_log_detachment_required(options))
    .then(debug_log_file_writer)
    .flatten();

    match (primary_writer, debug_file_writer) {
        (None, None) => None,
        (Some(writer), None) | (None, Some(writer)) => Some(writer),
        (Some(primary), Some(debug_file)) => Some(Box::new(TeeWriter::new(primary, debug_file))),
    }
}

fn debug_logging_enabled() -> bool {
    env::var("RUST_LOG").is_ok_and(|value| {
        let value = value.to_ascii_lowercase();
        value.contains("debug") || value.contains("trace")
    })
}

fn debug_log_file_writer() -> Option<Box<dyn Write + Send>> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open("debug.log")
        .ok()
        .map(|file| Box::new(file) as Box<dyn Write + Send>)
}

fn headless_log_writer() -> Box<dyn Write + Send> {
    Box::new(io::LineWriter::new(io::stdout()))
}

struct TeeWriter {
    primary: Box<dyn Write + Send>,
    secondary: Box<dyn Write + Send>,
}

impl TeeWriter {
    fn new(primary: Box<dyn Write + Send>, secondary: Box<dyn Write + Send>) -> Self {
        Self { primary, secondary }
    }
}

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.primary.write_all(buf)?;
        self.secondary.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.primary.flush()?;
        self.secondary.flush()
    }
}

fn native_tui_log_detachment_required(options: &RuntimeOptions) -> bool {
    options.ui == Some(UiMode::Tui) && options.tui_attach.is_none()
}

fn set_ui_mode(options: &mut RuntimeOptions, mode: UiMode, flag: &str) -> Result<(), String> {
    match options.ui {
        Some(existing) if existing != mode => Err(format!(
            "{flag} conflicts with previously selected mode {}; choose one of --tui, --headless, or --ui",
            match existing {
                UiMode::Headless => "headless",
                UiMode::Tui => "tui",
            }
        )),
        _ => {
            options.ui = Some(mode);
            Ok(())
        }
    }
}

fn parse_runtime_options(args: &[String]) -> Result<RuntimeOptions, String> {
    let mut options = RuntimeOptions::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--tui" => set_ui_mode(&mut options, UiMode::Tui, "--tui")?,
            "--headless" => set_ui_mode(&mut options, UiMode::Headless, "--headless")?,
            "--api" | "--web" => {
                return Err(format!(
                    "{} has been removed; use --headless/--tui or --ui headless|tui, plus --tui-listen/--tui-attach",
                    args[i]
                ));
            }
            "--ui" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    return Err("--ui requires headless or tui".to_string());
                };
                let mode = match value.as_str() {
                    "headless" => UiMode::Headless,
                    "tui" => UiMode::Tui,
                    _ => return Err(format!("invalid --ui value {value:?}; use headless or tui")),
                };
                set_ui_mode(&mut options, mode, "--ui")?;
            }
            "--tui-listen" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    return Err("--tui-listen requires IP:PORT".to_string());
                };
                options.tui_listen = Some(value.clone());
            }
            "--tui-attach" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    return Err("--tui-attach requires IP:PORT".to_string());
                };
                options.tui_attach = Some(value.clone());
            }
            "--host" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    return Err("--host requires a value".to_string());
                };
                options.host = value.clone();
                options.host_explicit = true;
            }
            "--config" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    return Err("--config requires a path".to_string());
                };
                options.config_path = Some(PathBuf::from(value));
            }
            _ => {}
        }
        i += 1;
    }

    if options.tui_attach.is_some() {
        if options.tui_listen.is_some() {
            return Err("--tui-attach cannot be combined with --tui-listen".to_string());
        }
        if options.ui == Some(UiMode::Headless) {
            return Err("--tui-attach requires TUI mode (--tui or --ui tui)".to_string());
        }
        if options.ui.is_none() {
            options.ui = Some(UiMode::Tui);
        }
        return Ok(options);
    }

    if options.ui.is_none() && options.tui_listen.is_some() {
        options.ui = Some(UiMode::Headless);
    }

    Ok(options)
}

fn native_tui_log_writer() -> Box<dyn Write + Send> {
    let path = native_tui_log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    match File::options().create(true).append(true).open(path) {
        Ok(file) => Box::new(file),
        Err(_) => Box::new(io::sink()),
    }
}

fn native_tui_log_path() -> PathBuf {
    let mut path = octo_dl::SessionSnapshot::state_dir();
    path.pop();
    path.push("native-tui.log");
    path
}

#[tokio::main]
async fn main() -> octo_dl::Result<()> {
    // Scan for global flags without consuming — sub-modules re-parse for their own flags
    let args: Vec<String> = env::args().skip(1).collect();
    let options = parse_runtime_options(&args).unwrap_or_else(|error| {
        eprintln!("Error: {error}");
        std::process::exit(1);
    });
    init_logger(&options);

    if help_requested(&args) {
        match options.ui {
            Some(UiMode::Tui) => {
                print_tui_usage();
                std::process::exit(0);
            }
            Some(UiMode::Headless) => {
                print_headless_usage();
                std::process::exit(0);
            }
            None => {
                if args.iter().all(|arg| arg == "-h" || arg == "--help") {
                    print_usage();
                    std::process::exit(0);
                }
            }
        }
    }

    if let Some(mode) = startup_log_mode(&options) {
        log::info!("Starting octo in {mode} mode");
    }

    if options.ui == Some(UiMode::Tui)
        && let Some(addr) = options.tui_attach.as_deref()
    {
        #[cfg(feature = "tui")]
        {
            let addr = octo_dl::tui::parse_loopback_addr(addr).unwrap_or_else(|error| {
                eprintln!("Error: {error}");
                std::process::exit(1);
            });
            return octo_dl::tui::run_attach(addr)
                .await
                .map_err(octo_dl::Error::Io);
        }
        #[cfg(not(feature = "tui"))]
        {
            let _ = addr;
            eprintln!("TUI support not compiled in");
            std::process::exit(1);
        }
    }

    if options.ui == Some(UiMode::Tui) {
        let listen = options.tui_listen.as_deref().map(parse_tui_listen);
        let host_param = options.host_explicit.then_some(Some(options.host.clone()));
        #[cfg(feature = "tui")]
        {
            octo_dl::tui::run(host_param, options.config_path.as_deref(), listen)
                .await
                .map_err(octo_dl::Error::Io)
        }
        #[cfg(not(feature = "tui"))]
        {
            let _ = host_param;
            let _ = listen;
            eprintln!("TUI support not compiled in");
            std::process::exit(1);
        }
    } else if options.ui == Some(UiMode::Headless) {
        let listen = options.tui_listen.as_deref().map(parse_tui_listen);
        let host_param = options.host_explicit.then_some(Some(options.host.clone()));
        #[cfg(feature = "tui")]
        {
            octo_dl::tui::run_api_only(host_param, options.config_path.as_deref(), listen)
                .await
                .map_err(octo_dl::Error::Io)
        }
        #[cfg(not(feature = "tui"))]
        {
            let _ = host_param;
            let _ = listen;
            eprintln!("API support requires the 'tui' feature");
            std::process::exit(1);
        }
    } else {
        // CLI mode — check if there are any positional args (URLs/DLC)
        let has_positional = has_positional_args(&args);
        if !has_positional && !args.iter().any(|a| a == "-r" || a == "--resume") {
            // No URLs, no --resume, and not TUI/API — show help
            if args.is_empty() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positional_args_ignore_cli_flag_values() {
        let args = vec![
            "--ui".to_string(),
            "headless".to_string(),
            "--chunks".to_string(),
            "4".to_string(),
        ];
        assert!(!has_positional_args(&args));
    }

    #[test]
    fn positional_args_detect_url_after_global_flag_value() {
        let args = vec![
            "--host".to_string(),
            "0.0.0.0".to_string(),
            "https://mega.nz/file/test".to_string(),
        ];
        assert!(has_positional_args(&args));
    }

    #[test]
    fn help_requested_matches_global_help_flags() {
        assert!(help_requested(&["--tui".to_string(), "--help".to_string()]));
        assert!(help_requested(&["-h".to_string()]));
        assert!(!help_requested(&["--tui".to_string()]));
    }

    #[test]
    fn native_tui_log_detachment_only_applies_to_local_tui() {
        assert!(native_tui_log_detachment_required(&RuntimeOptions {
            ui: Some(UiMode::Tui),
            ..RuntimeOptions::default()
        }));
        assert!(!native_tui_log_detachment_required(&RuntimeOptions {
            ui: Some(UiMode::Headless),
            ..RuntimeOptions::default()
        }));
        assert!(!native_tui_log_detachment_required(&RuntimeOptions {
            ui: Some(UiMode::Tui),
            tui_attach: Some("127.0.0.1:9724".to_string()),
            ..RuntimeOptions::default()
        }));
    }

    #[test]
    fn startup_log_mode_skips_tui_and_labels_non_tui_modes() {
        assert_eq!(startup_log_mode(&RuntimeOptions::default()), Some("cli"));
        assert_eq!(
            startup_log_mode(&RuntimeOptions {
                ui: Some(UiMode::Headless),
                ..RuntimeOptions::default()
            }),
            Some("headless")
        );
        assert_eq!(
            startup_log_mode(&RuntimeOptions {
                ui: Some(UiMode::Tui),
                ..RuntimeOptions::default()
            }),
            None
        );
    }

    #[test]
    fn runtime_options_reject_old_mode_flags() {
        assert!(parse_runtime_options(&["--api".to_string()]).is_err());
        assert!(parse_runtime_options(&["--web".to_string()]).is_err());
    }

    #[test]
    fn runtime_options_parse_new_ui_modes() {
        let options =
            parse_runtime_options(&["--tui".to_string()]).expect("--tui should select TUI mode");
        assert_eq!(options.ui, Some(UiMode::Tui));

        let options = parse_runtime_options(&["--headless".to_string()])
            .expect("--headless should select headless mode");
        assert_eq!(options.ui, Some(UiMode::Headless));

        let options = parse_runtime_options(&[
            "--ui".to_string(),
            "tui".to_string(),
            "--tui-listen".to_string(),
            "127.0.0.1:9724".to_string(),
        ])
        .expect("new mode flags should parse");
        assert_eq!(options.ui, Some(UiMode::Tui));
        assert_eq!(options.tui_listen.as_deref(), Some("127.0.0.1:9724"));

        let options =
            parse_runtime_options(&["--tui-attach".to_string(), "127.0.0.1:9724".to_string()])
                .expect("attach should parse");
        assert_eq!(options.ui, Some(UiMode::Tui));
        assert_eq!(options.tui_attach.as_deref(), Some("127.0.0.1:9724"));

        let options = parse_runtime_options(&[
            "--headless".to_string(),
            "--host".to_string(),
            "0.0.0.0".to_string(),
        ])
        .expect("headless host override should parse");
        assert_eq!(options.ui, Some(UiMode::Headless));
        assert!(options.host_explicit);
        assert_eq!(options.host, "0.0.0.0");
    }

    #[test]
    fn runtime_options_reject_conflicting_mode_flags() {
        let error = parse_runtime_options(&["--tui".to_string(), "--headless".to_string()])
            .expect_err("conflicting aliases should reject");
        assert!(error.contains("conflicts"));

        let error = parse_runtime_options(&[
            "--headless".to_string(),
            "--ui".to_string(),
            "tui".to_string(),
        ])
        .expect_err("mixed explicit/conflicting modes should reject");
        assert!(error.contains("conflicts"));
    }

    #[test]
    fn runtime_options_reject_attach_combinations() {
        let error = parse_runtime_options(&[
            "--tui-attach".to_string(),
            "127.0.0.1:9724".to_string(),
            "--ui".to_string(),
            "headless".to_string(),
        ])
        .expect_err("attach cannot run in headless mode");
        assert!(error.contains("requires TUI mode"));

        let error = parse_runtime_options(&[
            "--tui-attach".to_string(),
            "127.0.0.1:9724".to_string(),
            "--tui-listen".to_string(),
            "127.0.0.1:9725".to_string(),
        ])
        .expect_err("attach cannot listen");
        assert!(error.contains("cannot be combined"));
    }
}
