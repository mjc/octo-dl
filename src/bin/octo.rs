use env_logger::Target;
use std::env;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::net::TcpStream;
#[cfg(unix)]
use std::os::unix::io::{FromRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{FromRawHandle, RawHandle};
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

fn init_logger(args: &[String]) {
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("octo_dl=info"));
    if let Some(writer) = log_writer(args) {
        builder.target(Target::Pipe(writer));
    }
    builder.init();
}

fn log_writer(args: &[String]) -> Option<Box<dyn Write + Send>> {
    let primary_writer = log_pipe_writer()
        .or_else(|| native_tui_log_detachment_required(args).then(native_tui_log_writer));
    let debug_file_writer = debug_logging_enabled()
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

fn log_pipe_writer() -> Option<Box<dyn Write + Send>> {
    if let Ok(addr) = env::var("OCTO_TUI_LOG_ADDR")
        && let Some(writer) = log_pipe_writer_from_addr(&addr)
    {
        return Some(writer);
    }
    let raw_fd = env::var_os("OCTO_TUI_LOG_FD")?;
    let fd = raw_fd.to_string_lossy().parse::<usize>().ok()?;
    log_pipe_writer_from_raw(fd)
}

fn log_pipe_writer_from_addr(addr: &str) -> Option<Box<dyn Write + Send>> {
    TcpStream::connect(addr)
        .ok()
        .map(|stream| Box::new(stream) as Box<dyn Write + Send>)
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

fn native_tui_log_detachment_required(args: &[String]) -> bool {
    (args.iter().any(|arg| arg == "--tui")
        || args
            .windows(2)
            .any(|pair| pair[0] == "--ui" && pair[1] == "tui"))
        && env::var_os("OCTO_TUI_LOG_ADDR").is_none()
        && env::var_os("OCTO_TUI_LOG_FD").is_none()
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
    let mut path = octo_dl::SessionSnapshotV3::state_dir();
    path.pop();
    path.push("native-tui.log");
    path
}

#[cfg(unix)]
fn log_pipe_writer_from_raw(fd: usize) -> Option<Box<dyn Write + Send>> {
    if fd > i32::MAX as usize {
        return None;
    }
    unsafe {
        let file = File::from_raw_fd(fd as RawFd);
        Some(Box::new(file))
    }
}

#[cfg(windows)]
fn log_pipe_writer_from_raw(fd: usize) -> Option<Box<dyn Write + Send>> {
    unsafe {
        let file = File::from_raw_handle(fd as RawHandle);
        Some(Box::new(file))
    }
}

#[tokio::main]
async fn main() -> octo_dl::Result<()> {
    // Scan for global flags without consuming — sub-modules re-parse for their own flags
    let args: Vec<String> = env::args().skip(1).collect();
    init_logger(&args);

    let options = parse_runtime_options(&args).unwrap_or_else(|error| {
        eprintln!("Error: {error}");
        std::process::exit(1);
    });

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
        let listen = options.tui_listen.as_deref().map(|value| {
            octo_dl::tui::parse_loopback_addr(value).unwrap_or_else(|error| {
                eprintln!("Error: {error}");
                std::process::exit(1);
            })
        });
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
            eprintln!("TUI support not compiled in");
            std::process::exit(1);
        }
    } else if options.ui == Some(UiMode::Headless) {
        let listen = options.tui_listen.as_deref().map(|value| {
            octo_dl::tui::parse_loopback_addr(value).unwrap_or_else(|error| {
                eprintln!("Error: {error}");
                std::process::exit(1);
            })
        });
        #[cfg(feature = "tui")]
        {
            octo_dl::tui::run_api_only(options.config_path.as_deref(), listen)
                .await
                .map_err(octo_dl::Error::Io)
        }
        #[cfg(not(feature = "tui"))]
        {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    fn log_env_guard() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .expect("log env test mutex should not be poisoned")
    }

    #[test]
    fn log_pipe_writer_handles_missing_and_invalid_values() {
        let _guard = log_env_guard();
        unsafe { env::remove_var("OCTO_TUI_LOG_FD") };
        assert!(log_pipe_writer().is_none());

        unsafe { env::set_var("OCTO_TUI_LOG_FD", "invalid") };
        assert!(log_pipe_writer().is_none());

        unsafe { env::remove_var("OCTO_TUI_LOG_FD") };
    }

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
    fn native_tui_log_detachment_only_applies_to_unforwarded_tui() {
        let _guard = log_env_guard();
        unsafe {
            env::remove_var("OCTO_TUI_LOG_ADDR");
            env::remove_var("OCTO_TUI_LOG_FD");
        }

        assert!(native_tui_log_detachment_required(&[
            "--ui".to_string(),
            "tui".to_string()
        ]));
        assert!(native_tui_log_detachment_required(&["--tui".to_string()]));
        assert!(!native_tui_log_detachment_required(&[
            "--ui".to_string(),
            "headless".to_string()
        ]));
        assert!(!native_tui_log_detachment_required(&[
            "--headless".to_string()
        ]));

        unsafe { env::set_var("OCTO_TUI_LOG_ADDR", "127.0.0.1:1") };
        assert!(!native_tui_log_detachment_required(&[
            "--ui".to_string(),
            "tui".to_string()
        ]));
        unsafe { env::remove_var("OCTO_TUI_LOG_ADDR") };

        unsafe { env::set_var("OCTO_TUI_LOG_FD", "9") };
        assert!(!native_tui_log_detachment_required(&[
            "--ui".to_string(),
            "tui".to_string()
        ]));
        unsafe { env::remove_var("OCTO_TUI_LOG_FD") };
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
