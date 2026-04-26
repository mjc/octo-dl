//! octo-dl CLI - Command-line interface for downloading MEGA files.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use dirs;
use futures::{StreamExt, stream};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::{
    DlcKeyCache, DownloadConfig, DownloadItem, DownloadProgress, FileStats, NoProgress,
    SessionStats, SessionStatsBuilder,
    core::{
        DesiredState, FileLifecycle, FileProgressState, FileSnapshot, PackageSnapshot,
        ProgressDelta, RestartSnapshot, RuntimeState, SavedCredentials, SessionRunStatus,
        SessionSnapshotV3, reconcile_restart, scan_filesystem,
    },
    format_bytes, format_duration, is_dlc_path,
};

const DEFAULT_CONCURRENT_FILES: usize = 4;
const DEFAULT_CHUNKS_PER_FILE: usize = 2;
const SEPARATOR: &str = "────────────────────────────────────────────────────────────";

fn build_http_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .pool_idle_timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(8)
        .tcp_keepalive(Duration::from_secs(30))
        .build()
}

// ============================================================================
// CLI Configuration
// ============================================================================

struct CliConfig {
    urls: Vec<String>,
    dlc_files: Vec<String>,
    download_config: DownloadConfig,
    resume: bool,
}

struct CliPackageFiles<'a> {
    source_url: String,
    files: Vec<DownloadItem<'a>>,
    skipped: usize,
    partial: usize,
}

impl CliPackageFiles<'_> {
    fn total_size(&self) -> u64 {
        self.files.iter().map(|item| item.node.size()).sum()
    }
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
        println!("Package: {}", package.source_url);

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

fn build_restart_snapshot(session: &SessionSnapshotV3) -> RestartSnapshot {
    reconcile_restart(
        Some(session.clone()),
        scan_filesystem(session.files.iter().map(|file| file.path.clone())),
        session
            .packages
            .iter()
            .map(|entry| entry.source_url.clone())
            .collect(),
    )
}

#[cfg(test)]
fn resumable_urls(session: &SessionSnapshotV3) -> Vec<(usize, String)> {
    let restart = build_restart_snapshot(session);
    restart
        .resumable_urls()
        .into_iter()
        .filter_map(|url| {
            session
                .packages
                .iter()
                .position(|entry| entry.source_url == url)
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
    mut session_state: Option<&mut SessionSnapshotV3>,
) -> crate::Result<()> {
    if files.is_empty() {
        return Ok(());
    }

    let progress_trait: Arc<dyn DownloadProgress> = progress.clone();

    let results: Vec<_> = stream::iter(files)
        .map(|item| {
            let progress = Arc::clone(&progress_trait);
            async move {
                let result = downloader
                    .download_file(item.node, &item.path, &progress, None)
                    .await;
                (
                    item.key.clone().unwrap_or_else(|| item.path.clone()),
                    result,
                )
            }
        })
        .buffer_unordered(downloader.config().concurrent_files)
        .collect()
        .await;

    // Use aggregate peak, not per-file peak
    builder.set_peak_speed(progress.peak_speed());

    for (path, result) in results {
        match result {
            Ok(file_stats) => {
                builder.add_download(&file_stats);
                if let Some(ref mut state) = session_state.as_deref_mut() {
                    let _ = state.mark_file_complete(&path);
                }
            }
            Err(e) => {
                let _ = progress.progress.println(format!("Download error: {e:?}"));
                if let Some(ref mut state) = session_state.as_deref_mut() {
                    let _ = state.mark_file_error(&path, &e.to_string());
                }
            }
        }
    }

    Ok(())
}

// ============================================================================
// CLI Parsing
// ============================================================================

fn parse_args() -> CliConfig {
    let mut urls = Vec::new();
    let mut dlc_files = Vec::new();
    let mut chunks_per_file = DEFAULT_CHUNKS_PER_FILE;
    let mut concurrent_files = DEFAULT_CONCURRENT_FILES;
    let mut force = false;
    let mut resume = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-j" | "--chunks" => {
                chunks_per_file = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(DEFAULT_CHUNKS_PER_FILE);
            }
            "-p" | "--parallel" => {
                concurrent_files = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(DEFAULT_CONCURRENT_FILES);
            }
            "-f" | "--force" => {
                force = true;
            }
            "-r" | "--resume" => {
                resume = true;
            }
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            // Skip global flags handled by the unified binary
            "--tui" | "--api" => {}
            "--host" | "--config" => {
                let _ = args.next(); // consume the value
            }
            _ if !arg.starts_with('-') => {
                if is_dlc_path(&arg) {
                    dlc_files.push(arg);
                } else {
                    urls.push(arg);
                }
            }
            _ => {
                eprintln!("Unknown option: {arg}");
                std::process::exit(1);
            }
        }
    }

    CliConfig {
        urls,
        dlc_files,
        download_config: DownloadConfig::new()
            .with_chunks_per_file(chunks_per_file)
            .with_concurrent_files(concurrent_files)
            .with_force_overwrite(force),
        resume,
    }
}

fn print_usage() {
    eprintln!("Usage: octo [OPTIONS] <url|dlc>...");
    eprintln!();
    eprintln!("Arguments:");
    eprintln!("  <url|dlc>           MEGA URL or JDownloader2 .dlc file (MEGA links only)");
    eprintln!();
    eprintln!("Options:");
    eprintln!(
        "  -j, --chunks <N>    Chunks per file for parallel download (default: {DEFAULT_CHUNKS_PER_FILE})"
    );
    eprintln!(
        "  -p, --parallel <N>  Concurrent file downloads (default: {DEFAULT_CONCURRENT_FILES})"
    );
    eprintln!("  -f, --force         Overwrite existing files");
    eprintln!("  -r, --resume        Resume a previous incomplete session");
    eprintln!("  --tui               Launch interactive TUI mode");
    eprintln!("  -h, --help          Show this help");
    eprintln!();
    eprintln!("Environment:");
    eprintln!("  MEGA_EMAIL          MEGA account email");
    eprintln!("  MEGA_PASSWORD       MEGA account password");
    eprintln!("  MEGA_MFA            MEGA MFA code (optional)");
}

fn get_credentials() -> (String, String, Option<String>) {
    let email = std::env::var("MEGA_EMAIL").expect("MEGA_EMAIL not set");
    let password = std::env::var("MEGA_PASSWORD").expect("MEGA_PASSWORD not set");
    let mfa = std::env::var("MEGA_MFA").ok();
    (email, password, mfa)
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
    let mut config = parse_args();

    // Check for resumable session
    if config.resume {
        if let Some(session) = SessionSnapshotV3::latest() {
            println!(
                "Resuming session {} ({} files, {} completed)",
                session.id,
                session.files.len(),
                session.completed_count()
            );
            return resume_session(session, &config).await;
        }
        println!("No resumable session found, starting fresh.");
    } else if config.urls.is_empty() && config.dlc_files.is_empty() {
        // Check if there's a session to resume
        if let Some(session) = SessionSnapshotV3::latest() {
            println!(
                "Found incomplete session: {} ({} remaining files)",
                session.id,
                session.remaining_count()
            );
            println!("Use --resume to continue, or provide URLs to start a new session.");
            std::process::exit(0);
        }
        print_usage();
        std::process::exit(1);
    }

    let (email, password, mfa) = get_credentials();

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
    let downloader = crate::Downloader::new(client, config.download_config.clone());
    let no_progress: Arc<dyn crate::DownloadProgress> = Arc::new(NoProgress);

    let mut session_state = SessionSnapshotV3::new(
        config.download_config.clone(),
        SavedCredentials::encrypt(&email, &password, mfa.as_deref()),
    );
    session_state.packages = config
        .urls
        .iter()
        .map(|url| PackageSnapshot {
            id: url.clone(),
            source_url: url.clone(),
            display_name: url.clone(),
            file_ids: Vec::new(),
            error: None,
        })
        .collect();

    // Phase 1: Fetch all URLs and collect files
    println!("Fetching file lists from {} URL(s)...\n", config.urls.len());
    let mut all_nodes: Vec<(usize, String, mega::Nodes)> = Vec::new();
    for (idx, url) in config.urls.iter().enumerate() {
        print!("  {url} ... ");
        match crate::fetch_public_nodes(&http, url).await {
            Ok(nodes) => {
                let collected_tmp = downloader.collect_files(&nodes, &no_progress).await;
                let file_count = collected_tmp.to_download.len() + collected_tmp.skipped;
                println!("{file_count} file(s)");
                if let Some(package) = session_state.packages.get_mut(idx) {
                    package.error = None;
                }
                all_nodes.push((idx, url.clone(), nodes));
            }
            Err(e) => {
                println!("ERROR: {e:?}");
                if let Some(package) = session_state.packages.get_mut(idx) {
                    package.error = Some(e.to_string());
                }
            }
        }
    }

    // Collect files from all fetched nodes, preserving the original URL index
    let mut package_files = Vec::new();
    for (url_idx, url, nodes) in &all_nodes {
        let collected = downloader.collect_files(nodes, &no_progress).await;

        let mut files = Vec::new();
        if let Some(package) = session_state.packages.get_mut(*url_idx) {
            for item in &collected.to_download {
                if !package.file_ids.contains(&item.path) {
                    package.file_ids.push(item.path.clone());
                }
                session_state.files.push(FileSnapshot {
                    id: item.path.clone(),
                    package_id: package.id.clone(),
                    path: item.path.clone(),
                    size: item.node.size(),
                    lifecycle: FileLifecycle::Queued,
                    progress: FileProgressState::default(),
                    desired: DesiredState::Present,
                    runtime: RuntimeState {
                        counts_in_run_totals: true,
                        active: false,
                        preexisting_complete: false,
                        reused_chunks: 0,
                    },
                    message: None,
                });
            }
        }
        for mut item in collected.to_download {
            item.key = Some(item.path.clone());
            files.push(item);
        }

        package_files.push(CliPackageFiles {
            source_url: url.clone(),
            files,
            skipped: collected.skipped,
            partial: collected.partial,
        });
    }

    // Save initial session state
    let _ = session_state.save();

    // Phase 2: Print what we found
    print_file_list(&package_files);

    let total_skipped: usize = package_files.iter().map(|package| package.skipped).sum();
    let all_files: Vec<DownloadItem> = package_files
        .into_iter()
        .flat_map(|package| package.files)
        .collect();

    if all_files.is_empty() {
        if total_skipped > 0 {
            println!("All files already downloaded.");
        }
        let _ = session_state.mark_completed();
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

    download_all(
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

    // Mark session as completed
    let _ = session_state.mark_completed();

    Ok(())
}

/// Resume a previous incomplete session.
async fn resume_session(mut session: SessionSnapshotV3, config: &CliConfig) -> crate::Result<()> {
    let restart = build_restart_snapshot(&session);
    // Decrypt credentials
    let (email, password, mfa) = session
        .credentials
        .decrypt()
        .expect("Failed to decrypt session credentials");

    let http = build_http_client()?;

    let mut client = mega::Client::builder().build(http.clone())?;

    println!("Logging in...");
    client.login(&email, &password, mfa.as_deref()).await?;
    println!("Logged in successfully.");

    let downloader = crate::Downloader::new(client, config.download_config.clone());
    let no_progress: Arc<dyn crate::DownloadProgress> = Arc::new(NoProgress);

    // Re-fetch URLs and collect remaining files
    let remaining_urls = restart
        .resumable_urls()
        .into_iter()
        .filter_map(|url| {
            session
                .packages
                .iter()
                .position(|entry| entry.source_url == url)
                .map(|idx| (idx, url))
        })
        .collect::<Vec<_>>();

    println!(
        "Fetching file lists from {} URL(s)...\n",
        remaining_urls.len()
    );
    let mut all_nodes: Vec<(usize, String, mega::Nodes)> = Vec::new();
    for (url_idx, url) in &remaining_urls {
        print!("  {url} ... ");
        match crate::fetch_public_nodes(&http, url).await {
            Ok(nodes) => {
                let collected_tmp = downloader.collect_files(&nodes, &no_progress).await;
                let file_count = collected_tmp.to_download.len() + collected_tmp.skipped;
                println!("{file_count} file(s)");
                if let Some(package) = session.packages.get_mut(*url_idx) {
                    package.error = None;
                }
                all_nodes.push((*url_idx, url.clone(), nodes));
            }
            Err(e) => println!("ERROR: {e:?}"),
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
    let mut package_files = Vec::new();
    for (url_idx, _url, nodes) in &all_nodes {
        let collected = downloader.collect_files(nodes, &no_progress).await;
        let mut files = Vec::new();
        let mut skipped = collected.skipped;
        for mut item in collected.to_download {
            let key = item.path.clone();
            if !resumable_file_ids.is_empty()
                && !resumable_file_ids.contains(&item.path)
                && ignored_paths.contains(&item.path)
            {
                skipped += 1;
            } else {
                item.key = Some(key);
                files.push(item);
            }
        }

        package_files.push(CliPackageFiles {
            source_url: session
                .packages
                .get(*url_idx)
                .map(|entry| entry.source_url.clone())
                .unwrap_or_else(|| format!("url:{url_idx}")),
            files,
            skipped,
            partial: collected.partial,
        });
    }

    print_file_list(&package_files);

    let total_skipped: usize = package_files.iter().map(|package| package.skipped).sum();
    let all_files: Vec<DownloadItem> = package_files
        .into_iter()
        .flat_map(|package| package.files)
        .collect();

    if all_files.is_empty() {
        println!("All files already downloaded.");
        let _ = session.mark_completed();
        return Ok(());
    }

    session.status = SessionRunStatus::InProgress;
    let _ = session.save();

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

    download_all(
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

    let _ = session.mark_completed();

    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{FileFixtureStatus, UrlFixtureStatus, push_file, session_snapshot};

    #[test]
    fn progress_bar_creation() {
        let bar = make_progress_bar(1000, "test.txt");
        assert_eq!(bar.length(), Some(1000));
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

        let urls = resumable_urls(&session);
        assert_eq!(
            urls,
            vec![
                (0, "https://mega.nz/file/pending".to_string()),
                (1, "https://mega.nz/file/fetched".to_string()),
            ]
        );
    }

    #[test]
    fn resume_url_selection_excludes_fetched_urls_with_only_terminal_files() {
        let mut session = session_snapshot(vec![(
            "https://mega.nz/file/skipped",
            UrlFixtureStatus::Fetched,
        )]);
        push_file(&mut session, 0, "skip.bin", 123, FileFixtureStatus::Skipped);

        let urls = resumable_urls(&session);
        assert!(urls.is_empty());
    }
}
