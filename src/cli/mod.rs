//! octo-dl CLI - Command-line interface for downloading MEGA files.

#![allow(clippy::too_many_lines)]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use dirs;
use futures::{StreamExt, stream};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::{
    DlcKeyCache, DownloadConfig, DownloadItem, DownloadProgress, FileStats, NoProgress,
    ServiceConfig, SessionStats, SessionStatsBuilder,
    config::CredentialKey,
    core::{
        PackageId, PackageKey, PackageSnapshot, ProgressDelta, SavedCredentials, SessionRunStatus,
        SessionSnapshot, SessionUrlSnapshot, build_restart_snapshot,
    },
    download::{build_http_client, infer_package_display_name, infer_package_id},
    format_bytes, format_duration,
    url::DownloadSource,
};

const SEPARATOR: &str = "────────────────────────────────────────────────────────────";

// ============================================================================
// CLI Configuration
// ============================================================================

struct CliConfig {
    urls: Vec<String>,
    dlc_files: Vec<String>,
    download_config: DownloadConfig,
    download_overrides: CliDownloadOverrides,
    config_path: Option<PathBuf>,
    resume: bool,
}

#[derive(Default)]
struct CliDownloadOverrides {
    chunks_per_file: Option<usize>,
    concurrent_files: Option<usize>,
    force_overwrite: Option<bool>,
}

struct CliPackageFiles<'a> {
    id: PackageId,
    display_name: String,
    files: Vec<DownloadItem<'a>>,
    output_owners: Vec<(String, String)>,
    skipped: usize,
    partial: usize,
}

impl CliPackageFiles<'_> {
    fn total_size(&self) -> u64 {
        self.files.iter().map(|item| item.node.size()).sum()
    }
}

fn append_cli_package_files<'a>(
    package_files: &mut Vec<CliPackageFiles<'a>>,
    package: CliPackageFiles<'a>,
) -> Result<(), String> {
    validate_cli_output_owners(
        package_files
            .iter()
            .flat_map(|entry| {
                entry
                    .output_owners
                    .iter()
                    .map(|(path, handle)| (path, handle))
            })
            .chain(
                package
                    .output_owners
                    .iter()
                    .map(|(path, handle)| (path, handle)),
            ),
    )?;

    if let Some(existing) = package_files
        .iter_mut()
        .find(|entry| entry.id == package.id)
    {
        let mut known_paths = existing
            .files
            .iter()
            .map(|item| item.path.clone())
            .collect::<std::collections::HashSet<_>>();
        existing.files.extend(
            package
                .files
                .into_iter()
                .filter(|item| known_paths.insert(item.path.clone())),
        );
        existing.skipped += package.skipped;
        existing.partial += package.partial;
        existing.output_owners.extend(package.output_owners);
        return Ok(());
    }
    package_files.push(package);
    Ok(())
}

fn validate_cli_output_owners<I, P, H>(owners: I) -> Result<(), String>
where
    I: IntoIterator<Item = (P, H)>,
    P: AsRef<str>,
    H: AsRef<str>,
{
    let mut paths = HashMap::<String, String>::new();
    for (path, handle) in owners {
        let path = path.as_ref();
        let handle = handle.as_ref();
        if let Some(existing) = paths.get(path) {
            if existing != handle {
                return Err(format!(
                    "different MEGA files ({existing} and {handle}) resolve to the same output path {path:?}"
                ));
            }
        } else {
            paths.insert(path.to_string(), handle.to_string());
        }
    }
    Ok(())
}

const fn cli_run_can_complete(had_fetch_failures: bool, had_download_failures: bool) -> bool {
    !had_fetch_failures && !had_download_failures
}

fn deduplicate_source_urls(urls: &mut Vec<String>) {
    let mut seen = HashSet::with_capacity(urls.len());
    urls.retain_mut(|url| {
        if let Ok(DownloadSource::Mega(normalized)) = DownloadSource::parse(url) {
            *url = normalized.into_string();
        }
        seen.insert(url.clone())
    });
}

fn effective_resume_config(saved: &DownloadConfig, cli: &CliConfig) -> DownloadConfig {
    let mut effective = saved.clone();
    if let Some(chunks) = cli.download_overrides.chunks_per_file {
        effective.chunks_per_file = chunks;
    }
    if let Some(concurrent) = cli.download_overrides.concurrent_files {
        effective.concurrent_files = concurrent;
    }
    if let Some(force) = cli.download_overrides.force_overwrite {
        effective.force_overwrite = force;
    }
    effective
}

fn default_cli_config_path() -> crate::Result<PathBuf> {
    let local = std::env::current_dir()?.join("config.toml");
    let mut state = SessionSnapshot::state_dir();
    state.pop();
    state.push("config.toml");
    Ok(if local.exists() || !state.exists() {
        local
    } else {
        state
    })
}

fn load_cli_credential_key(path: Option<&Path>) -> crate::Result<CredentialKey> {
    let path = match path {
        Some(path) => path.to_path_buf(),
        None => default_cli_config_path()?,
    };
    Ok(ServiceConfig::load_or_create_credential_key(&path)?)
}

fn load_cli_resume_key(
    credentials: &SavedCredentials,
    path: Option<&Path>,
) -> crate::Result<CredentialKey> {
    Ok(credentials.resume_key(|| {
        let path = match path {
            Some(path) => path.to_path_buf(),
            None => default_cli_config_path().map_err(std::io::Error::other)?,
        };
        let config = ServiceConfig::load(&path)?;
        config.require_credential_key()
    })?)
}

fn resume_mfa(saved: Option<String>, fresh: Option<String>) -> Result<Option<String>, String> {
    match (saved, fresh) {
        (_, Some(fresh)) if !fresh.trim().is_empty() => Ok(Some(fresh)),
        (Some(_), _) => Err("MEGA_MFA must contain a current code to resume this session".into()),
        (None, Some(_) | None) => Ok(None),
    }
}

fn prepare_cli_download_root(config: &mut DownloadConfig) -> crate::Result<()> {
    let Some(root) = config.path.as_deref() else {
        return Ok(());
    };
    let requested = PathBuf::from(root);
    let display_path = if requested.is_absolute() {
        requested.clone()
    } else {
        std::env::current_dir()?.join(&requested)
    };
    let absolute_root =
        crate::config::prepare_download_root(Some(&requested)).map_err(|error| {
            crate::Error::Download(format!(
                "cannot create download root {}: {error}",
                display_path.display()
            ))
        })?;
    config.path = absolute_root.map(|path| path.to_string_lossy().into_owned());
    Ok(())
}

// ============================================================================
// Progress Bar Implementation
// ============================================================================

fn make_progress_bar(size: u64, name: &str) -> ProgressBar {
    let bar = ProgressBar::new(size);
    bar.set_style(
        ProgressStyle::with_template(
            "{spinner:.cyan} [{bar:40.cyan/blue}] {bytes}/{total_bytes} @ {bytes_per_sec} - {msg}",
        )
        .expect("progress template is valid")
        .progress_chars("━━╌"),
    );
    bar.set_message(name.to_string());
    bar
}

fn make_total_progress_bar(size: u64) -> ProgressBar {
    let bar = ProgressBar::new(size);
    bar.set_style(
        ProgressStyle::with_template(
            "Total [{bar:40.green/white}] {bytes}/{total_bytes} @ {bytes_per_sec}",
        )
        .expect("template valid")
        .progress_chars("━━╌"),
    );
    bar
}

// ============================================================================
// Download Functions
// ============================================================================

struct CliDownloadProgress {
    progress: MultiProgress,
    total_bar: ProgressBar,
    bars: Mutex<HashMap<String, ProgressBar>>,
    session_peak: AtomicU64,
}

impl CliDownloadProgress {
    fn new(progress: MultiProgress, total_bar: ProgressBar) -> Self {
        Self {
            progress,
            total_bar,
            bars: Mutex::new(HashMap::new()),
            session_peak: AtomicU64::new(0),
        }
    }

    fn peak_speed(&self) -> u64 {
        self.session_peak.load(Ordering::Relaxed)
    }
}

impl DownloadProgress for CliDownloadProgress {
    fn on_file_start(&self, name: &str, size: u64) {
        let bar = self
            .progress
            .insert_before(&self.total_bar, make_progress_bar(size, name));
        bar.enable_steady_tick(Duration::from_millis(250));
        self.bars.lock().unwrap().insert(name.to_string(), bar);
    }

    fn on_resume_validation_start(&self, name: &str) {
        let _ = self
            .progress
            .println(format!("  checking local partial: {name}"));
    }

    fn on_resume_validation_progress(&self, name: &str, checked_bytes: u64, total_bytes: u64) {
        let pct = crate::download::resume_validation_percent(checked_bytes, total_bytes);
        let _ = self
            .progress
            .println(format!("  checking local partial: {name} ({pct}%)"));
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn on_progress(&self, name: &str, delta: ProgressDelta) {
        self.total_bar.inc(delta.total_bytes_delta);
        let current_speed = self.total_bar.per_sec() as u64;
        self.session_peak
            .fetch_max(current_speed, Ordering::Relaxed);
        if let Some(bar) = self.bars.lock().unwrap().get(name) {
            bar.inc(delta.total_bytes_delta);
        }
    }

    fn on_file_complete(&self, name: &str, stats: &FileStats) {
        let bar = self.bars.lock().unwrap().remove(name);
        if let Some(bar) = bar {
            bar.finish_and_clear();
        }
        let ramp_up = stats.ramp_up_time.map_or_else(
            || "ramp <1s".to_string(),
            |d| format!("ramp {}", format_duration(d)),
        );
        let _ = self.progress.println(format!(
            "  {} - {} in {} ({}/s avg, {}/s peak, {}, {} reused)",
            name,
            format_bytes(stats.size),
            format_duration(stats.elapsed),
            format_bytes(stats.average_speed),
            format_bytes(stats.peak_speed),
            ramp_up,
            format_bytes(stats.reused_bytes),
        ));
    }

    fn on_error(&self, name: &str, _error: &str) {
        let bar = self.bars.lock().unwrap().remove(name);
        if let Some(bar) = bar {
            bar.abandon();
        }
    }

    fn on_partial_detected(&self, name: &str, existing_size: u64, expected_size: u64) {
        let _ = self.progress.println(format!(
            "  partial: {name} ({}/{})",
            format_bytes(existing_size),
            format_bytes(expected_size),
        ));
    }

    fn on_resume_reused(&self, name: &str, chunks: usize, bytes: u64) {
        let _ = self.progress.println(format!(
            "  resuming: {name} reusing {chunks} verified chunk(s), {}",
            format_bytes(bytes),
        ));
    }
}

fn print_file_list(packages: &[CliPackageFiles<'_>]) {
    let queued_files: usize = packages.iter().map(|package| package.files.len()).sum();
    let skipped: usize = packages.iter().map(|package| package.skipped).sum();
    let partial: usize = packages.iter().map(|package| package.partial).sum();

    if queued_files == 0 && skipped == 0 {
        println!("No files found.");
        return;
    }

    let total_size: u64 = packages.iter().map(CliPackageFiles::total_size).sum();

    println!("\n{SEPARATOR}");
    println!("Packages to download:");
    println!("{SEPARATOR}");

    for package in packages {
        println!("Package: {}", package.display_name);

        for item in &package.files {
            println!("  {} ({})", item.path, format_bytes(item.node.size()));
        }

        println!(
            "  queued: {} file(s), {}",
            package.files.len(),
            format_bytes(package.total_size())
        );
        if package.skipped > 0 {
            println!("  skipped: {} file(s) already complete", package.skipped);
        }
        if package.partial > 0 {
            println!(
                "  partial: {} file(s) with verified resumable data",
                package.partial
            );
        }
        println!("{SEPARATOR}");
    }

    println!(
        "  {} queued file(s), {} total",
        queued_files,
        format_bytes(total_size)
    );
    if skipped > 0 {
        println!("  {skipped} file(s) skipped (already exist)");
    }
    if partial > 0 {
        println!("  {partial} file(s) with partial downloads (verified chunks will be reused)");
    }
    println!("{SEPARATOR}\n");
}

fn print_summary(stats: &SessionStats) {
    if stats.files_downloaded == 0 && stats.files_skipped == 0 {
        return;
    }

    println!("\n{SEPARATOR}");
    println!("Download Summary");
    println!("{SEPARATOR}");

    if stats.files_downloaded > 0 {
        println!("  Files downloaded:  {}", stats.files_downloaded);
        println!("  Total size:        {}", format_bytes(stats.total_bytes));
        println!("  Network this run:  {}", format_bytes(stats.network_bytes));
        println!("  Reused partials:   {}", format_bytes(stats.reused_bytes));
        println!("  Total time:        {}", format_duration(stats.elapsed));
        println!(
            "  Average speed:     {}/s",
            format_bytes(stats.average_speed())
        );
        println!("  Peak speed:        {}/s", format_bytes(stats.peak_speed));
        if let Some(ramp) = stats.average_ramp_up {
            println!(
                "  Avg ramp-up:       {} to 80% of peak",
                format_duration(ramp)
            );
        }
    }

    if stats.files_skipped > 0 {
        println!("  Files skipped:     {}", stats.files_skipped);
    }

    println!("{SEPARATOR}");
}

fn ensure_session_url<'a>(
    session: &'a mut SessionSnapshot,
    url: &str,
) -> &'a mut SessionUrlSnapshot {
    if let Some(index) = session.urls.iter().position(|entry| entry.url == url) {
        return &mut session.urls[index];
    }
    session.urls.push(SessionUrlSnapshot {
        url: url.to_string(),
        error: None,
    });
    session.urls.last_mut().expect("url was just pushed")
}

fn mark_session_file_complete(session: &mut SessionSnapshot, file_id: &str) {
    session.mark_file_complete(file_id);
}

fn mark_session_file_error(session: &mut SessionSnapshot, file_id: &str, error: &str) {
    session.mark_file_error(file_id, error);
}

#[must_use]
fn session_completed_count(session: &SessionSnapshot) -> usize {
    session.completed_count()
}

#[must_use]
fn session_remaining_count(session: &SessionSnapshot) -> usize {
    session.remaining_count()
}

fn persist_session(session: &mut SessionSnapshot) -> crate::Result<()> {
    session.save()?;
    *session = SessionSnapshot::load(&session.state_path())?;
    Ok(())
}

async fn collect_cli_package_files<'a>(
    downloader: &crate::Downloader,
    progress: &Arc<dyn DownloadProgress>,
    nodes: &'a mega::Nodes,
    mut keep_file: impl FnMut(&DownloadItem<'a>) -> bool,
) -> CliPackageFiles<'a> {
    let collected = downloader.collect_files(nodes, progress).await;
    let id = infer_package_id(nodes, &collected);
    let display_name = infer_package_display_name(nodes, &collected);
    let partial = collected.partial;
    let output_owners = collected
        .to_download
        .iter()
        .chain(&collected.completed)
        .map(|item| (item.path.clone(), item.node.handle().to_string()))
        .collect();
    let mut files = Vec::new();
    let mut skipped = collected.skipped;

    for item in collected.to_download {
        if keep_file(&item) {
            files.push(item);
        } else {
            skipped += 1;
        }
    }

    CliPackageFiles {
        id,
        display_name,
        files,
        output_owners,
        skipped,
        partial,
    }
}

fn register_cli_package_in_session(
    session: &mut SessionSnapshot,
    source_url: &str,
    package: &CliPackageFiles<'_>,
) -> Result<(), String> {
    if !session.packages.iter().any(|entry| entry.id == package.id) {
        session.packages.push(PackageSnapshot {
            id: package.id,
            key: PackageKey::new(package.display_name.clone()),
            display_name: package.display_name.clone(),
            files: Vec::new(),
            error: None,
        });
    }

    let Some(package_entry) = session
        .packages
        .iter_mut()
        .find(|entry| entry.id == package.id)
    else {
        return Err("CLI package disappeared during session registration".into());
    };
    let mut known_file_ids = package_entry
        .files
        .iter()
        .map(|file| file.id.clone())
        .collect::<std::collections::HashSet<_>>();
    for item in &package.files {
        if known_file_ids.insert(item.path.clone().into()) {
            package_entry.files.push(crate::core::queued_file_snapshot(
                item.path.clone(),
                package.id,
                source_url.to_string(),
                item.path.clone(),
                item.node.size(),
            ));
        }
    }
    session.prune_empty_packages();
    crate::core::validate_snapshot(session)
}

fn resumable_urls<I>(session: &SessionSnapshot, selected_urls: I) -> Vec<(usize, String)>
where
    I: IntoIterator<Item = String>,
{
    selected_urls
        .into_iter()
        .filter_map(|url| {
            session
                .urls
                .iter()
                .position(|entry| entry.url == url)
                .map(|idx| (idx, url))
        })
        .collect()
}

#[allow(clippy::similar_names)]
async fn download_all(
    downloader: &crate::Downloader,
    files: &[DownloadItem<'_>],
    progress: &Arc<CliDownloadProgress>,
    builder: &mut SessionStatsBuilder,
    mut session_state: Option<&mut SessionSnapshot>,
) -> crate::Result<bool> {
    if files.is_empty() {
        return Ok(false);
    }

    let progress_trait: Arc<dyn DownloadProgress> = progress.clone();
    let known_session_file_ids = session_state
        .as_ref()
        .map(|session| {
            session
                .iter_files()
                .map(|file| file.id.clone())
                .collect::<std::collections::HashSet<_>>()
        })
        .unwrap_or_default();

    let results: Vec<_> = stream::iter(files)
        .map(|item| {
            let progress = Arc::clone(&progress_trait);
            let trust_resume_state = known_session_file_ids.contains(item.path.as_str());
            async move {
                let result = Box::pin(downloader.download_file(
                    item.node,
                    &item.path,
                    &progress,
                    trust_resume_state,
                    None,
                ))
                .await;
                (item.path.clone(), result)
            }
        })
        .buffer_unordered(downloader.config().concurrent_files)
        .collect()
        .await;

    // Use aggregate peak, not per-file peak
    builder.set_peak_speed(progress.peak_speed());

    let mut had_failures = false;
    for (path, result) in results {
        match result {
            Ok(file_stats) => {
                builder.add_download(&file_stats);
                if let Some(ref mut state) = session_state.as_deref_mut() {
                    mark_session_file_complete(state, &path);
                }
            }
            Err(e) => {
                had_failures = true;
                let _ = progress.progress.println(format!("Download error: {e:?}"));
                if let Some(ref mut state) = session_state.as_deref_mut() {
                    mark_session_file_error(state, &path, &e.to_string());
                }
            }
        }
    }

    Ok(had_failures)
}

// ============================================================================
// CLI Parsing
// ============================================================================

fn parse_args<I>(args: I) -> Result<CliConfig, String>
where
    I: IntoIterator<Item = String>,
{
    let mut urls = Vec::new();
    let mut dlc_files = Vec::new();
    let mut chunks_per_file = None;
    let mut concurrent_files = None;
    let mut force_overwrite = None;
    let mut resume = false;
    let mut config_path = None;

    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-j" | "--chunks" => {
                chunks_per_file = Some(parse_positive_number(&mut args, &arg)?);
            }
            "-p" | "--parallel" => {
                concurrent_files = Some(parse_positive_number(&mut args, &arg)?);
            }
            "-f" | "--force" => {
                force_overwrite = Some(true);
            }
            "-r" | "--resume" => {
                resume = true;
            }
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            // Skip global flags handled by the unified binary
            "--config" => {
                config_path = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| "--config requires a path".to_string())?,
                ));
            }
            "--host" | "--ui" | "--tui-listen" | "--tui-attach" | "--api-key" => {
                let _ = args.next(); // consume the value
            }
            "--tui" | "--headless" => {}
            _ if !arg.starts_with('-') => match DownloadSource::parse(&arg) {
                Ok(DownloadSource::Mega(url)) => urls.push(url.into_string()),
                Ok(DownloadSource::Dlc(path)) => dlc_files.push(path.into_string()),
                Err(error) => {
                    return Err(format!("invalid download source {arg:?}: {error}"));
                }
            },
            _ => {
                return Err(format!("unknown option: {arg}"));
            }
        }
    }

    deduplicate_source_urls(&mut urls);

    let mut download_config = DownloadConfig::default();
    if let Some(chunks) = chunks_per_file {
        download_config.chunks_per_file = chunks;
    }
    if let Some(concurrent) = concurrent_files {
        download_config.concurrent_files = concurrent;
    }
    if let Some(force) = force_overwrite {
        download_config.force_overwrite = force;
    }

    Ok(CliConfig {
        urls,
        dlc_files,
        download_config,
        download_overrides: CliDownloadOverrides {
            chunks_per_file,
            concurrent_files,
            force_overwrite,
        },
        config_path,
        resume,
    })
}

fn parse_positive_number<I>(args: &mut I, flag: &str) -> Result<usize, String>
where
    I: Iterator<Item = String>,
{
    let value = args
        .next()
        .ok_or_else(|| format!("{flag} requires a positive integer"))?;
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("{flag} requires a positive integer, got {value:?}"))?;
    (parsed > 0)
        .then_some(parsed)
        .ok_or_else(|| format!("{flag} requires a positive integer, got {value:?}"))
}

fn print_usage() {
    let defaults = DownloadConfig::default();
    eprintln!("Usage: octo [OPTIONS] <url|dlc>...");
    eprintln!();
    eprintln!("Arguments:");
    eprintln!("  <url|dlc>           MEGA URL or JDownloader2 .dlc file (MEGA links only)");
    eprintln!();
    eprintln!("Options:");
    eprintln!(
        "  -j, --chunks <N>    Chunks per file for parallel download (default: {})",
        defaults.chunks_per_file
    );
    eprintln!(
        "  -p, --parallel <N>  Concurrent file downloads (default: {})",
        defaults.concurrent_files
    );
    eprintln!("  -f, --force         Overwrite existing files");
    eprintln!("  -r, --resume        Resume a previous incomplete session");
    eprintln!("  --tui               Launch interactive TUI mode");
    eprintln!("  --ui tui            Equivalent explicit form");
    eprintln!("  -h, --help          Show this help");
    eprintln!();
    eprintln!("Environment:");
    eprintln!("  MEGA_EMAIL          MEGA account email");
    eprintln!("  MEGA_PASSWORD       MEGA account password");
    eprintln!("  MEGA_MFA            MEGA MFA code (optional)");
}

fn get_credentials() -> crate::Result<(String, String, Option<String>)> {
    let email = std::env::var("MEGA_EMAIL")
        .map_err(|_| crate::Error::Download("MEGA_EMAIL not set".to_string()))?;
    let password = std::env::var("MEGA_PASSWORD")
        .map_err(|_| crate::Error::Download("MEGA_PASSWORD not set".to_string()))?;
    let mfa = std::env::var("MEGA_MFA").ok();
    Ok((email, password, mfa))
}

// ============================================================================
// Entry point
// ============================================================================

/// Run the CLI application.
///
/// # Errors
/// Returns an error if download operations fail or configuration loading fails.
#[allow(clippy::too_many_lines, clippy::similar_names)]
pub async fn run() -> crate::Result<()> {
    let mut config = parse_args(std::env::args().skip(1)).map_err(crate::Error::Download)?;

    // Check for resumable session
    if config.resume {
        if let Some(session) = SessionSnapshot::latest() {
            println!(
                "Resuming session {} ({} files, {} completed)",
                session.id,
                session.file_count(),
                session_completed_count(&session)
            );
            let credential_key =
                load_cli_resume_key(&session.credentials, config.config_path.as_deref())?;
            return resume_session(session, &config, &credential_key).await;
        }
        println!("No resumable session found, starting fresh.");
    } else if config.urls.is_empty() && config.dlc_files.is_empty() {
        // Check if there's a session to resume
        if let Some(session) = SessionSnapshot::latest() {
            println!(
                "Found incomplete session: {} ({} remaining files)",
                session.id,
                session_remaining_count(&session)
            );
            println!("Use --resume to continue, or provide URLs to start a new session.");
            std::process::exit(0);
        }
        print_usage();
        std::process::exit(1);
    }

    let credential_key = load_cli_credential_key(config.config_path.as_deref())?;

    let (email, password, mfa) = get_credentials()?;

    // Create HTTP client with custom user agent for DLC service
    let http = build_http_client()?;

    // Process DLC files before logging in
    if !config.dlc_files.is_empty() {
        println!("Processing DLC files...\n");
        let dlc_cache = DlcKeyCache::new();
        for dlc_path in &config.dlc_files {
            print!("  {dlc_path} ... ");
            // Expand ~ to home directory for local DLC files
            #[allow(clippy::option_if_let_else)]
            let expanded_path = if dlc_path.starts_with('~') {
                if let Some(home) = dirs::home_dir() {
                    dlc_path.replacen('~', home.to_string_lossy().as_ref(), 1)
                } else {
                    eprintln!("Error: Could not determine home directory");
                    std::process::exit(1);
                }
            } else {
                dlc_path.clone()
            };
            match crate::parse_dlc_file(&expanded_path, &http, &dlc_cache).await {
                Ok(urls) => {
                    println!("{} MEGA link(s)", urls.len());
                    config.urls.extend(urls);
                    deduplicate_source_urls(&mut config.urls);
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            }
        }
        println!();
    }

    let mut client = mega::Client::builder().build(http.clone())?;

    println!("Logging in...");
    client.login(&email, &password, mfa.as_deref()).await?;
    println!("Logged in successfully.");

    // Shared downloader owns collection and all payload writes.
    let mut fresh_download_config = effective_resume_config(&config.download_config, &config);
    prepare_cli_download_root(&mut fresh_download_config)?;
    let downloader = crate::Downloader::new(client, fresh_download_config.clone());
    let no_progress: Arc<dyn crate::DownloadProgress> = Arc::new(NoProgress);

    let mut session_state = SessionSnapshot::new(
        fresh_download_config,
        SavedCredentials::encrypt_with_key(&email, &password, None, &credential_key),
    );
    session_state.urls = config
        .urls
        .iter()
        .map(|url| SessionUrlSnapshot {
            url: url.clone(),
            error: None,
        })
        .collect();

    // Phase 1: Fetch all URLs and collect files
    println!("Fetching file lists from {} URL(s)...\n", config.urls.len());
    let sources = config.urls.iter().cloned().enumerate().collect::<Vec<_>>();
    let fetched_sources = fetch_source_nodes(&http, &sources).await;
    let mut all_nodes: Vec<(usize, String, mega::Nodes)> = Vec::new();
    let mut had_fetch_failures = false;
    for (idx, url, result) in fetched_sources {
        match result {
            Ok(nodes) => {
                ensure_session_url(&mut session_state, &url).error = None;
                all_nodes.push((idx, url, nodes));
            }
            Err(error) => {
                println!("  {url} ... ERROR: {error}");
                had_fetch_failures = true;
                ensure_session_url(&mut session_state, &url).error = Some(error);
            }
        }
    }

    // Collect files from all fetched nodes, preserving the original URL index
    let mut package_files: Vec<CliPackageFiles<'_>> = Vec::new();
    for (_url_idx, url, nodes) in &all_nodes {
        let package = collect_cli_package_files(&downloader, &no_progress, nodes, |_| true).await;
        println!(
            "  {url} ... {} file(s)",
            package.files.len() + package.skipped
        );
        let package_id = package.id;
        append_cli_package_files(&mut package_files, package).map_err(crate::Error::Download)?;
        let Some(registered) = package_files.iter().find(|entry| entry.id == package_id) else {
            return Err(crate::Error::Download(
                "CLI package disappeared after being appended".to_string(),
            ));
        };
        register_cli_package_in_session(&mut session_state, url, registered)
            .map_err(crate::Error::Download)?;
    }

    // Save initial session state
    persist_session(&mut session_state)?;

    // Phase 2: Print what we found
    print_file_list(&package_files);

    let total_skipped: usize = package_files.iter().map(|package| package.skipped).sum();
    let all_files: Vec<DownloadItem> = package_files
        .into_iter()
        .flat_map(|package| package.files)
        .collect();

    if all_files.is_empty() {
        if had_fetch_failures {
            session_state.status = SessionRunStatus::InProgress;
            persist_session(&mut session_state)?;
            return Err(crate::Error::Download(
                "Failed to fetch one or more URLs".to_string(),
            ));
        }
        if total_skipped > 0 {
            println!("All files already downloaded.");
        }
        session_state.status = SessionRunStatus::Completed;
        persist_session(&mut session_state)?;
        return Ok(());
    }

    // Phase 3: Download all files
    let progress = MultiProgress::new();
    let total_size: u64 = all_files.iter().map(|i| i.node.size()).sum();
    let total_bar = progress.add(make_total_progress_bar(total_size));
    total_bar.enable_steady_tick(Duration::from_millis(250));
    let cli_progress = Arc::new(CliDownloadProgress::new(
        progress.clone(),
        total_bar.clone(),
    ));

    let mut builder = SessionStatsBuilder::new();
    builder.set_skipped(total_skipped);

    let had_download_failures = download_all(
        &downloader,
        &all_files,
        &cli_progress,
        &mut builder,
        Some(&mut session_state),
    )
    .await?;

    total_bar.finish_and_clear();
    progress.clear().ok();
    let session_stats = builder.build();
    print_summary(&session_stats);

    if !cli_run_can_complete(had_fetch_failures, had_download_failures) {
        session_state.status = SessionRunStatus::InProgress;
        persist_session(&mut session_state)?;
        if had_fetch_failures {
            return Err(crate::Error::Download(
                "Failed to fetch one or more URLs".to_string(),
            ));
        }
        return Err(crate::Error::Download(
            "One or more downloads failed".to_string(),
        ));
    }

    // Mark session as completed
    session_state.status = SessionRunStatus::Completed;
    persist_session(&mut session_state)?;

    Ok(())
}

/// Resume a previous incomplete session.
async fn resume_session(
    mut session: SessionSnapshot,
    config: &CliConfig,
    credential_key: &CredentialKey,
) -> crate::Result<()> {
    let mut resume_config = effective_resume_config(&session.config, config);
    prepare_cli_download_root(&mut resume_config)?;
    session.config.clone_from(&resume_config);
    let restart = build_restart_snapshot(&session);
    // Decrypt credentials
    let (email, password, saved_mfa) = session
        .credentials
        .decrypt_with_key(credential_key)
        .or_else(|| session.credentials.decrypt_legacy())
        .ok_or_else(|| {
            crate::Error::Download("Failed to decrypt session credentials".to_string())
        })?;
    let fresh_mfa = std::env::var("MEGA_MFA")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let mfa = resume_mfa(saved_mfa, fresh_mfa).map_err(crate::Error::Download)?;

    let http = build_http_client()?;

    let mut client = mega::Client::builder().build(http.clone())?;

    println!("Logging in...");
    client.login(&email, &password, mfa.as_deref()).await?;
    println!("Logged in successfully.");
    session.credentials =
        SavedCredentials::encrypt_with_key(&email, &password, None, credential_key);

    let downloader = crate::Downloader::new(client, resume_config.clone());
    let no_progress: Arc<dyn crate::DownloadProgress> = Arc::new(NoProgress);

    // Re-fetch URLs and collect remaining files
    let remaining_urls = resumable_urls(&session, restart.resumable_urls());

    println!(
        "Fetching file lists from {} URL(s)...\n",
        remaining_urls.len()
    );
    let fetched_sources = fetch_source_nodes(&http, &remaining_urls).await;
    let mut all_nodes = Vec::new();
    let mut had_fetch_failures = false;
    for (url_idx, url, result) in fetched_sources {
        match result {
            Ok(nodes) => {
                if let Some(entry) = session.urls.get_mut(url_idx) {
                    entry.error = None;
                }
                all_nodes.push((url_idx, url, nodes));
            }
            Err(error) => {
                had_fetch_failures = true;
                println!("  {url} ... ERROR: {error}");
                if let Some(entry) = session.urls.get_mut(url_idx) {
                    entry.error = Some(error);
                }
            }
        }
    }

    // Completed file paths from session state
    let resumable_file_ids: std::collections::HashSet<_> =
        restart.resume_file_ids.iter().cloned().collect();
    let ignored_paths: std::collections::HashSet<String> = restart
        .state
        .files
        .values()
        .filter(|file| !resumable_file_ids.contains(&file.id))
        .map(|file| file.path.clone())
        .collect();

    // Collect files, skipping already-completed ones
    let mut package_files: Vec<CliPackageFiles<'_>> = Vec::new();
    for (_url_idx, url, nodes) in &all_nodes {
        let package = collect_cli_package_files(&downloader, &no_progress, nodes, |item| {
            resumable_file_ids.is_empty()
                || resumable_file_ids.contains(item.path.as_str())
                || !ignored_paths.contains(&item.path)
        })
        .await;
        println!(
            "  {url} ... {} file(s)",
            package.files.len() + package.skipped
        );
        let package_id = package.id;
        append_cli_package_files(&mut package_files, package).map_err(crate::Error::Download)?;
        let registered = package_files
            .iter()
            .find(|entry| entry.id == package_id)
            .expect("the package was just appended");
        register_cli_package_in_session(&mut session, url, registered)
            .map_err(crate::Error::Download)?;
    }

    print_file_list(&package_files);

    let total_skipped: usize = package_files.iter().map(|package| package.skipped).sum();
    let all_files: Vec<DownloadItem> = package_files
        .into_iter()
        .flat_map(|package| package.files)
        .collect();

    if all_files.is_empty() {
        if had_fetch_failures {
            session.status = SessionRunStatus::InProgress;
            persist_session(&mut session)?;
            return Err(crate::Error::Download(
                "Failed to fetch one or more URLs".to_string(),
            ));
        }
        println!("All files already downloaded.");
        session.status = SessionRunStatus::Completed;
        persist_session(&mut session)?;
        return Ok(());
    }

    session.status = SessionRunStatus::InProgress;
    persist_session(&mut session)?;

    let progress = MultiProgress::new();
    let total_size: u64 = all_files.iter().map(|i| i.node.size()).sum();
    let total_bar = progress.add(make_total_progress_bar(total_size));
    total_bar.enable_steady_tick(Duration::from_millis(250));
    let cli_progress = Arc::new(CliDownloadProgress::new(
        progress.clone(),
        total_bar.clone(),
    ));

    let mut builder = SessionStatsBuilder::new();
    builder.set_skipped(total_skipped);

    let had_download_failures = download_all(
        &downloader,
        &all_files,
        &cli_progress,
        &mut builder,
        Some(&mut session),
    )
    .await?;

    total_bar.finish_and_clear();
    progress.clear().ok();
    let session_stats = builder.build();
    print_summary(&session_stats);

    if !cli_run_can_complete(had_fetch_failures, had_download_failures) {
        session.status = SessionRunStatus::InProgress;
        persist_session(&mut session)?;
        if had_fetch_failures {
            return Err(crate::Error::Download(
                "Failed to fetch one or more URLs".to_string(),
            ));
        }
        return Err(crate::Error::Download(
            "One or more downloads failed".to_string(),
        ));
    }

    session.status = SessionRunStatus::Completed;
    persist_session(&mut session)?;

    Ok(())
}

async fn fetch_source_nodes(
    http: &reqwest::Client,
    sources: &[(usize, String)],
) -> Vec<(usize, String, Result<mega::Nodes, String>)> {
    let mut fetched = Vec::with_capacity(sources.len());
    for (index, url) in sources {
        let result = crate::fetch_public_nodes(http, url)
            .await
            .map_err(|error| error.to_string());
        fetched.push((*index, url.clone(), result));
    }
    fetched
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        CurrentDirGuard, FileFixtureStatus, UrlFixtureStatus, push_file, session_snapshot,
    };

    #[test]
    fn progress_bar_creation() {
        let bar = make_progress_bar(1000, "test.txt");
        assert_eq!(bar.length(), Some(1000));
    }

    #[test]
    fn malformed_numeric_cli_values_are_rejected() {
        let error = parse_args(["--parallel", "not-a-number"].map(str::to_string))
            .err()
            .expect("malformed parallel value should be rejected");
        assert!(error.contains("--parallel requires a positive integer"));

        let error = parse_args(["--chunks", "0"].map(str::to_string))
            .err()
            .expect("zero chunks value should be rejected");
        assert!(error.contains("--chunks requires a positive integer"));
    }

    #[test]
    fn cli_source_boundary_normalizes_and_types_inputs() {
        let config =
            parse_args(["https://mega.nz/#F!folder!key", "./links.DLC"].map(str::to_string))
                .expect("supported sources should parse");

        assert_eq!(config.urls, ["https://mega.nz/folder/folder#key"]);
        assert_eq!(config.dlc_files, ["./links.DLC"]);
    }

    #[test]
    fn cli_preserves_explicit_service_config_path_for_session_credentials() {
        let config = parse_args(["--config", "/tmp/octo-config.toml"].map(str::to_string))
            .expect("global config path should be accepted by the CLI parser");
        assert_eq!(
            config.config_path.as_deref(),
            Some(Path::new("/tmp/octo-config.toml"))
        );
    }

    #[test]
    fn duplicate_direct_sources_are_admitted_once() {
        let config = parse_args(
            [
                "https://mega.nz/file/abc#key",
                "https://mega.nz/file/abc#key",
            ]
            .map(str::to_string),
        )
        .expect("duplicate source inputs should be accepted");

        assert_eq!(config.urls, ["https://mega.nz/file/abc#key"]);
    }

    #[test]
    fn source_deduplication_preserves_first_seen_order_across_expansions() {
        let mut urls = vec![
            "https://mega.nz/file/one#key".to_string(),
            "https://mega.nz/file/two#key".to_string(),
            "https://mega.nz/file/one#key".to_string(),
        ];
        deduplicate_source_urls(&mut urls);
        assert_eq!(
            urls,
            [
                "https://mega.nz/file/one#key",
                "https://mega.nz/file/two#key"
            ]
        );
    }

    #[test]
    fn completion_requires_successful_fetches_and_downloads() {
        assert!(cli_run_can_complete(false, false));
        assert!(!cli_run_can_complete(true, false));
        assert!(!cli_run_can_complete(false, true));
        assert!(!cli_run_can_complete(true, true));
    }

    #[test]
    fn distinct_remote_handles_cannot_claim_the_same_output_path() {
        assert!(validate_cli_output_owners([("payload.bin", "same-handle"); 2]).is_ok());
        assert!(
            validate_cli_output_owners([("payload.bin", "handle-a"), ("payload.bin", "handle-b"),])
                .is_err()
        );
        assert!(
            validate_cli_output_owners([("one.bin", "handle-a"), ("two.bin", "handle-b"),]).is_ok()
        );
    }

    #[test]
    fn resume_config_uses_saved_values_unless_cli_overrides_them() {
        let saved = DownloadConfig {
            path: Some("/saved/root".to_string()),
            chunks_per_file: 6,
            mega_chunks_per_request: 3,
            concurrent_files: 9,
            force_overwrite: true,
            cleanup_on_error: true,
        };
        let defaults = parse_args(std::iter::empty()).expect("defaults should parse");
        let resumed = effective_resume_config(&saved, &defaults);
        assert_eq!(resumed, saved);

        let overrides =
            parse_args(["--chunks", "8", "--parallel", "2", "--force"].map(str::to_string))
                .expect("explicit options should parse");
        let resumed = effective_resume_config(&saved, &overrides);
        assert_eq!(resumed.path.as_deref(), Some("/saved/root"));
        assert_eq!(resumed.chunks_per_file, 8);
        assert_eq!(resumed.concurrent_files, 2);
        assert!(resumed.force_overwrite);
        assert!(resumed.cleanup_on_error);
    }

    #[test]
    fn no_option_cli_download_config_matches_download_defaults() {
        let cli = parse_args(std::iter::empty()).expect("defaults should parse");

        assert_eq!(cli.download_config, DownloadConfig::default());
    }

    #[test]
    fn resume_prefers_current_mfa_and_rejects_replaying_saved_code() {
        assert_eq!(
            resume_mfa(Some("111111".into()), Some("222222".into())).unwrap(),
            Some("222222".into())
        );
        assert!(
            resume_mfa(Some("111111".into()), None)
                .unwrap_err()
                .contains("MEGA_MFA")
        );
        assert_eq!(resume_mfa(None, None).unwrap(), None);
    }

    #[test]
    fn cli_credentials_use_the_persisted_random_config_key() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        let first = load_cli_credential_key(Some(&config_path)).unwrap();
        let second = load_cli_credential_key(Some(&config_path)).unwrap();
        assert_eq!(first, second);

        let saved = SavedCredentials::encrypt_with_key("user", "password", None, &first);
        assert_eq!(
            saved.decrypt_with_key(&second),
            Some(("user".into(), "password".into(), None))
        );
    }

    #[test]
    fn cli_resume_keeps_original_key_across_directories_and_configs() {
        let state = tempfile::tempdir().unwrap();
        let _state = crate::test_support::StateDirectoryGuard::set(state.path());
        let first_dir = tempfile::tempdir().unwrap();
        let second_dir = tempfile::tempdir().unwrap();
        let key;
        let path;
        {
            let _cwd = CurrentDirGuard::set(first_dir.path());
            key = load_cli_credential_key(None).unwrap();
            let mut session = session_snapshot(vec![(
                "https://mega.nz/file/pending",
                UrlFixtureStatus::Pending,
            )]);
            session.credentials =
                SavedCredentials::encrypt_with_key("original", "password", None, &key);
            session.save().unwrap();
            path = session.state_path();
        }
        let _cwd = CurrentDirGuard::set(second_dir.path());
        let other_key = load_cli_credential_key(None).unwrap();
        assert_ne!(other_key, key);
        // Neither changing config nor losing the original config loses the archived key.
        std::fs::remove_file(first_dir.path().join("config.toml.key")).unwrap();
        std::fs::remove_file(first_dir.path().join("config.toml")).unwrap();
        let restored = SessionSnapshot::load(&path).unwrap();
        assert_eq!(
            load_cli_resume_key(&restored.credentials, None).unwrap(),
            key
        );
        assert_eq!(
            load_cli_resume_key(&restored.credentials, Some(Path::new("config.toml"))).unwrap(),
            key
        );
        let absent = second_dir.path().join("absent.toml");
        assert_eq!(
            load_cli_resume_key(&restored.credentials, Some(&absent)).unwrap(),
            key
        );
        assert!(!absent.exists());
        assert_eq!(
            restored.credentials.decrypt_with_key(&key),
            Some(("original".into(), "password".into(), None))
        );
    }

    #[test]
    fn cli_refuses_to_create_session_credentials_when_key_archive_cannot_be_written() {
        let state = tempfile::tempdir().unwrap();
        let _state = crate::test_support::StateDirectoryGuard::set(state.path());
        std::fs::create_dir_all(SessionSnapshot::state_dir()).unwrap();
        std::fs::write(
            SessionSnapshot::state_dir().join("credential-keys"),
            b"blocked",
        )
        .unwrap();
        assert!(load_cli_credential_key(Some(&state.path().join("config.toml"))).is_err());
        assert!(SessionSnapshot::latest().is_none());
    }

    #[test]
    fn configured_download_root_is_absolute_without_changing_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let _cwd = CurrentDirGuard::set(temp.path());
        let root = temp.path().join("saved-root");
        let mut config = DownloadConfig {
            path: Some("saved-root".into()),
            ..DownloadConfig::default()
        };
        prepare_cli_download_root(&mut config).unwrap();
        assert_eq!(
            std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap(),
            std::fs::canonicalize(temp.path()).unwrap()
        );
        assert_eq!(
            std::fs::canonicalize(config.path.as_deref().unwrap()).unwrap(),
            std::fs::canonicalize(&root).unwrap()
        );
    }

    #[tokio::test]
    async fn newly_resolved_resume_files_are_registered_before_download_scheduling() {
        let temp = tempfile::tempdir().unwrap();
        let _cwd = CurrentDirGuard::set(temp.path());
        let fixture =
            crate::fake_mega::create_fake_mega_fixture(temp.path(), "payload.bin", 64, 19)
                .await
                .unwrap();
        let server = crate::fake_mega::FakeMegaServer::spawn(fixture.clone(), 1).unwrap();
        let http = mega::http_client_builder().unwrap().build().unwrap();
        let client = mega::Client::builder()
            .origin(server.origin().clone())
            .build(http)
            .unwrap();
        let nodes = client
            .fetch_public_nodes(&fixture.public_url())
            .await
            .unwrap();
        let downloader = crate::Downloader::new(client, DownloadConfig::default());
        let progress: Arc<dyn DownloadProgress> = Arc::new(NoProgress);
        let mut session = session_snapshot(vec![(
            &fixture.public_url(),
            UrlFixtureStatus::Error("unavailable during first run".into()),
        )]);

        let package = collect_cli_package_files(&downloader, &progress, &nodes, |_| true).await;
        let source_url = fixture.public_url();
        register_cli_package_in_session(&mut session, &source_url, &package).unwrap();

        let file = session
            .iter_files()
            .next()
            .expect("newly collected file must be tracked before it is scheduled");
        assert_eq!(file.path, package.files[0].path);
        assert_eq!(file.source_url, source_url);
        assert!(matches!(
            &file.lifecycle,
            crate::core::FileLifecycle::Queued
        ));
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn colliding_output_paths_from_distinct_remote_handles_fail_collection() {
        let temp = tempfile::tempdir().unwrap();
        let _cwd = CurrentDirGuard::set(temp.path());
        let first = crate::fake_mega::create_fake_mega_fixture(
            &temp.path().join("first"),
            "payload.bin",
            64,
            19,
        )
        .await
        .unwrap();
        let second = crate::fake_mega::create_fake_mega_fixture(
            &temp.path().join("second"),
            "payload.bin",
            64,
            29,
        )
        .await
        .unwrap();
        let first_server = crate::fake_mega::FakeMegaServer::spawn(first.clone(), 1).unwrap();
        let second_server = crate::fake_mega::FakeMegaServer::spawn(second.clone(), 1).unwrap();
        let http = mega::http_client_builder().unwrap().build().unwrap();
        let first_client = mega::Client::builder()
            .origin(first_server.origin().clone())
            .build(http.clone())
            .unwrap();
        let second_client = mega::Client::builder()
            .origin(second_server.origin().clone())
            .build(http)
            .unwrap();
        let first_nodes = first_client
            .fetch_public_nodes(&first.public_url())
            .await
            .unwrap();
        let second_nodes = second_client
            .fetch_public_nodes(&second.public_url())
            .await
            .unwrap();
        let first_downloader = crate::Downloader::new(first_client, DownloadConfig::default());
        let second_downloader = crate::Downloader::new(second_client, DownloadConfig::default());
        let progress: Arc<dyn DownloadProgress> = Arc::new(NoProgress);
        let first_package =
            collect_cli_package_files(&first_downloader, &progress, &first_nodes, |_| true).await;
        let second_package =
            collect_cli_package_files(&second_downloader, &progress, &second_nodes, |_| true).await;
        let mut packages = vec![first_package];

        let error = append_cli_package_files(&mut packages, second_package)
            .expect_err("different remote handles must not silently share a destination");
        assert!(error.contains("same output path"));
        first_server.shutdown().await.unwrap();
        second_server.shutdown().await.unwrap();
    }

    #[test]
    fn cli_source_boundary_rejects_unsupported_inputs() {
        let Err(error) = parse_args(["https://example.com/file/id"].map(str::to_string)) else {
            panic!("non-MEGA URLs should be rejected at submission");
        };
        assert!(error.contains("invalid download source"));
    }

    #[test]
    fn persist_session_reloads_canonical_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = crate::test_support::StateDirectoryGuard::set(dir.path());
        let mut session = session_snapshot(vec![(
            "https://mega.nz/file/root",
            UrlFixtureStatus::Fetched,
        )]);
        push_file(
            &mut session,
            0,
            "episode-1.mkv",
            128,
            FileFixtureStatus::Pending,
        );
        persist_session(&mut session).unwrap();

        assert_eq!(
            session
                .iter_files()
                .map(|file| file.id.clone())
                .collect::<Vec<_>>(),
            vec![crate::core::FileId::from("episode-1.mkv")]
        );
    }

    #[test]
    fn resume_url_selection_includes_pending_and_fetched() {
        let session = session_snapshot(vec![
            ("https://mega.nz/file/pending", UrlFixtureStatus::Pending),
            ("https://mega.nz/file/fetched", UrlFixtureStatus::Fetched),
            (
                "https://mega.nz/file/error",
                UrlFixtureStatus::Error("nope".to_string()),
            ),
        ]);

        let restart = build_restart_snapshot(&session);
        let urls = resumable_urls(&session, restart.resumable_urls());
        assert_eq!(
            urls,
            vec![
                (0, "https://mega.nz/file/pending".to_string()),
                (1, "https://mega.nz/file/fetched".to_string()),
                (2, "https://mega.nz/file/error".to_string()),
            ]
        );
    }

    #[test]
    fn resume_url_selection_excludes_fetched_urls_with_only_terminal_files() {
        let dir = tempfile::tempdir().unwrap();
        let _cwd = CurrentDirGuard::set(dir.path());
        let mut session = session_snapshot(vec![(
            "https://mega.nz/file/complete",
            UrlFixtureStatus::Fetched,
        )]);
        push_file(
            &mut session,
            0,
            "complete.bin",
            123,
            FileFixtureStatus::Completed,
        );
        std::fs::write("complete.bin", vec![0_u8; 123]).unwrap();

        let restart = build_restart_snapshot(&session);
        let urls = resumable_urls(&session, restart.resumable_urls());
        assert!(urls.is_empty());
    }

    #[test]
    fn resumable_url_indices_preserve_original_positions_around_terminal_sources() {
        let dir = tempfile::tempdir().unwrap();
        let _cwd = CurrentDirGuard::set(dir.path());
        let mut session = session_snapshot(vec![
            ("https://mega.nz/file/pending", UrlFixtureStatus::Pending),
            ("https://mega.nz/file/complete", UrlFixtureStatus::Fetched),
            (
                "https://mega.nz/file/error",
                UrlFixtureStatus::Error("nope".into()),
            ),
        ]);
        push_file(
            &mut session,
            1,
            "complete.bin",
            123,
            FileFixtureStatus::Completed,
        );
        std::fs::write("complete.bin", vec![0_u8; 123]).unwrap();

        let restart = build_restart_snapshot(&session);
        let urls = resumable_urls(&session, restart.resumable_urls());

        assert_eq!(
            urls,
            vec![
                (0, "https://mega.nz/file/pending".to_string()),
                (2, "https://mega.nz/file/error".to_string()),
            ]
        );
    }

    #[test]
    fn duplicate_session_package_registration_is_deduplicated() {
        let mut session = session_snapshot(vec![(
            "https://mega.nz/file/root",
            UrlFixtureStatus::Fetched,
        )]);
        let package_id = crate::test_support::package_id("pkg", "pkg");
        session.packages.push(PackageSnapshot {
            id: package_id,
            key: PackageKey::new("pkg"),
            display_name: "pkg".to_string(),
            files: vec![crate::core::queued_file_snapshot(
                "episode-1.mkv".to_string(),
                package_id,
                "https://mega.nz/file/root".to_string(),
                "episode-1.mkv".to_string(),
                128,
            )],
            error: None,
        });

        let package_entry = session
            .packages
            .iter_mut()
            .find(|entry| entry.id == package_id)
            .expect("package exists");
        let mut known_file_ids = package_entry
            .files
            .iter()
            .map(|file| file.id.clone())
            .collect::<std::collections::HashSet<_>>();
        for path in ["episode-1.mkv", "episode-1.mkv", "episode-2.mkv"] {
            if known_file_ids.insert(path.to_string().into()) {
                package_entry.files.push(crate::core::queued_file_snapshot(
                    path.to_string(),
                    package_id,
                    "https://mega.nz/file/root".to_string(),
                    path.to_string(),
                    128,
                ));
            }
        }
        session.prune_empty_packages();
        crate::core::validate_snapshot(&session).unwrap();

        assert_eq!(
            session
                .iter_files()
                .map(|file| file.id.clone())
                .collect::<Vec<_>>(),
            vec![
                crate::core::FileId::from("episode-1.mkv"),
                crate::core::FileId::from("episode-2.mkv")
            ]
        );
    }
}
