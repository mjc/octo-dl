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
    DlcKeyCache, DownloadConfig, DownloadItem, DownloadProgress, FileEntry, FileEntryStatus,
    FileStats, NoProgress, SavedCredentials, SessionState, SessionStats, SessionStatsBuilder,
    SessionStatus, UrlEntry, UrlStatus,
    core::{ProgressDelta, RestartSnapshot, reconcile_restart, scan_filesystem},
    file_key, format_bytes, format_duration, is_dlc_path,
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

fn print_file_list(files: &[DownloadItem], skipped: usize, partial: usize) {
    if files.is_empty() && skipped == 0 {
        println!("No files found.");
        return;
    }

    let total_size: u64 = files.iter().map(|i| i.node.size()).sum();

    println!("\n{SEPARATOR}");
    println!("Files to download:");
    println!("{SEPARATOR}");

    for item in files {
        println!("  {} ({})", item.path, format_bytes(item.node.size()));
    }

    println!("{SEPARATOR}");
    println!(
        "  {} file(s), {} total",
        files.len(),
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

fn build_restart_snapshot(session: &SessionState) -> RestartSnapshot {
    reconcile_restart(
        Some(session.to_v3()),
        scan_filesystem(session.files.iter().map(|file| file.path.clone())),
        session.urls.iter().map(|entry| entry.url.clone()).collect(),
    )
}

#[cfg(test)]
fn resumable_urls(session: &SessionState) -> Vec<(usize, String)> {
    let restart = build_restart_snapshot(session);
    restart
        .resumable_urls()
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
    mut session_state: Option<&mut SessionState>,
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
        if let Some(session) = SessionState::latest() {
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
        if let Some(session) = SessionState::latest() {
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

    // Create session state for persistence
    let url_entries: Vec<UrlEntry> = config
        .urls
        .iter()
        .map(|url| UrlEntry {
            url: url.clone(),
            status: UrlStatus::Pending,
        })
        .collect();

    let mut session_state = SessionState::new(
        SavedCredentials::encrypt(&email, &password, mfa.as_deref()),
        config.download_config.clone(),
        url_entries,
    );

    // Phase 1: Fetch all URLs and collect files
    println!("Fetching file lists from {} URL(s)...\n", config.urls.len());
    let mut all_nodes: Vec<(String, mega::Nodes)> = Vec::new();
    for (idx, url) in config.urls.iter().enumerate() {
        print!("  {url} ... ");
        match crate::fetch_public_nodes(&http, url).await {
            Ok(nodes) => {
                let collected_tmp = downloader.collect_files(&nodes, &no_progress).await;
                let file_count = collected_tmp.to_download.len() + collected_tmp.skipped;
                println!("{file_count} file(s)");
                session_state.urls[idx].status = UrlStatus::Fetched;
                all_nodes.push((url.clone(), nodes));
            }
            Err(e) => {
                println!("ERROR: {e:?}");
                session_state.urls[idx].status = UrlStatus::Error(e.to_string());
            }
        }
    }

    // Collect files from all fetched nodes
    let mut all_files: Vec<DownloadItem> = Vec::new();
    let mut total_skipped = 0;
    let mut total_partial = 0;
    for (url_idx, (_url, nodes)) in all_nodes.iter().enumerate() {
        let collected = downloader.collect_files(nodes, &no_progress).await;
        // Record files in session state
        for item in &collected.to_download {
            session_state.files.push(FileEntry {
                key: Some(file_key(url_idx, &item.path)),
                url_index: url_idx,
                path: item.path.clone(),
                size: item.node.size(),
                status: FileEntryStatus::Pending,
            });
        }
        all_files.extend(collected.to_download.into_iter().map(|mut item| {
            item.key = Some(file_key(url_idx, &item.path));
            item
        }));
        total_skipped += collected.skipped;
        total_partial += collected.partial;
    }

    // Save initial session state
    let _ = session_state.save();

    // Phase 2: Print what we found
    print_file_list(&all_files, total_skipped, total_partial);

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
async fn resume_session(mut session: SessionState, config: &CliConfig) -> crate::Result<()> {
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
                .urls
                .iter()
                .position(|entry| entry.url == url)
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
                if let Some(entry) = session.urls.get_mut(*url_idx) {
                    entry.status = UrlStatus::Fetched;
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
    let mut all_files: Vec<DownloadItem> = Vec::new();
    let mut total_skipped = 0;
    let mut total_partial = 0;
    for (url_idx, _url, nodes) in &all_nodes {
        let collected = downloader.collect_files(nodes, &no_progress).await;
        for mut item in collected.to_download {
            let key = file_key(*url_idx, &item.path);
            if !resumable_file_ids.is_empty()
                && !resumable_file_ids.contains(&item.path)
                && (ignored_paths.contains(&key) || ignored_paths.contains(&item.path))
            {
                total_skipped += 1;
            } else {
                item.key = Some(key);
                all_files.push(item);
            }
        }
        total_skipped += collected.skipped;
        total_partial += collected.partial;
    }

    print_file_list(&all_files, total_skipped, total_partial);

    if all_files.is_empty() {
        println!("All files already downloaded.");
        let _ = session.mark_completed();
        return Ok(());
    }

    session.status = SessionStatus::InProgress;
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

    #[test]
    fn progress_bar_creation() {
        let bar = make_progress_bar(1000, "test.txt");
        assert_eq!(bar.length(), Some(1000));
    }

    #[test]
    fn resume_url_selection_includes_pending_and_fetched() {
        let session = SessionState::new(
            SavedCredentials::encrypt("test@example.com", "password", None),
            DownloadConfig::default(),
            vec![
                UrlEntry {
                    url: "https://mega.nz/file/pending".to_string(),
                    status: UrlStatus::Pending,
                },
                UrlEntry {
                    url: "https://mega.nz/file/fetched".to_string(),
                    status: UrlStatus::Fetched,
                },
                UrlEntry {
                    url: "https://mega.nz/file/error".to_string(),
                    status: UrlStatus::Error("nope".to_string()),
                },
            ],
        );

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
        let mut session = SessionState::new(
            SavedCredentials::encrypt("test@example.com", "password", None),
            DownloadConfig::default(),
            vec![UrlEntry {
                url: "https://mega.nz/file/skipped".to_string(),
                status: UrlStatus::Fetched,
            }],
        );
        session.files = vec![FileEntry {
            key: Some("0:skip.bin".to_string()),
            url_index: 0,
            path: "skip.bin".to_string(),
            size: 123,
            status: FileEntryStatus::Skipped,
        }];

        let urls = resumable_urls(&session);
        assert!(urls.is_empty());
    }
}
