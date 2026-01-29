//! octo-dl CLI - Command-line interface for downloading MEGA files.

#![warn(clippy::pedantic)]
#![warn(clippy::nursery)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::{env, path::Path};

use futures::{StreamExt, stream};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use octo_dl::{
    CollectedFiles, DlcKeyCache, DownloadConfig, DownloadItem, FileEntry, FileEntryStatus,
    SavedCredentials, SessionState, SessionStatus, UrlEntry, UrlStatus,
};

const DEFAULT_CONCURRENT_FILES: usize = 4;
const DEFAULT_CHUNKS_PER_FILE: usize = 2;

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
// Formatting Helpers
// ============================================================================

#[allow(clippy::cast_precision_loss)]
fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        format!(
            "{}h {:02}m {:02}s",
            secs / 3600,
            (secs % 3600) / 60,
            secs % 60
        )
    } else if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}.{:01}s", secs, d.subsec_millis() / 100)
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
// Indicatif Progress Implementation
// ============================================================================

/// Per-file download statistics tracker using indicatif's speed calculation.
struct DownloadStats {
    start_time: Instant,
    total_bytes: u64,
    peak_speed: AtomicU64,
    time_to_80pct_ms: AtomicU64,
}

impl DownloadStats {
    fn new(total_bytes: u64) -> Self {
        Self {
            start_time: Instant::now(),
            total_bytes,
            peak_speed: AtomicU64::new(0),
            time_to_80pct_ms: AtomicU64::new(0),
        }
    }

    /// Called with indicatif's `per_sec()` value to track peak and ramp-up.
    fn update_speed(&self, speed: u64) {
        let prev_peak = self.peak_speed.fetch_max(speed, Ordering::Relaxed);
        let peak = prev_peak.max(speed);

        // Track time to reach 80% of peak
        if self.time_to_80pct_ms.load(Ordering::Relaxed) == 0 && speed >= peak * 4 / 5 {
            let ms = self
                .start_time
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX);
            self.time_to_80pct_ms.store(ms, Ordering::Relaxed);
        }
    }

    fn elapsed(&self) -> Duration {
        self.start_time.elapsed()
    }

    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn average_speed(&self) -> u64 {
        let secs = self.elapsed().as_secs_f64();
        if secs > 0.0 {
            (self.total_bytes as f64 / secs) as u64
        } else {
            0
        }
    }

    fn peak_speed(&self) -> u64 {
        self.peak_speed.load(Ordering::Relaxed)
    }

    fn time_to_80pct(&self) -> Option<Duration> {
        let ms = self.time_to_80pct_ms.load(Ordering::Relaxed);
        if ms > 0 {
            Some(Duration::from_millis(ms))
        } else {
            None
        }
    }
}

/// Session statistics builder for CLI.
struct CliSessionStats {
    files_downloaded: usize,
    files_skipped: usize,
    total_bytes: u64,
    start_time: Instant,
    peak_speed: u64,
    total_ramp_up_ms: u64,
    ramp_up_count: u64,
}

impl CliSessionStats {
    fn new() -> Self {
        Self {
            files_downloaded: 0,
            files_skipped: 0,
            total_bytes: 0,
            start_time: Instant::now(),
            peak_speed: 0,
            total_ramp_up_ms: 0,
            ramp_up_count: 0,
        }
    }

    fn add_download(&mut self, bytes: u64, ramp_up: Option<Duration>) {
        self.files_downloaded += 1;
        self.total_bytes += bytes;
        if let Some(ramp) = ramp_up {
            self.total_ramp_up_ms += ramp.as_millis().try_into().unwrap_or(u64::MAX);
            self.ramp_up_count += 1;
        }
    }

    fn elapsed(&self) -> Duration {
        self.start_time.elapsed()
    }

    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn average_speed(&self) -> u64 {
        let secs = self.elapsed().as_secs_f64();
        if secs > 0.0 {
            (self.total_bytes as f64 / secs) as u64
        } else {
            0
        }
    }

    const fn average_ramp_up(&self) -> Option<Duration> {
        if self.ramp_up_count > 0 {
            Some(Duration::from_millis(
                self.total_ramp_up_ms / self.ramp_up_count,
            ))
        } else {
            None
        }
    }

    fn print_summary(&self) {
        if self.files_downloaded == 0 && self.files_skipped == 0 {
            return;
        }

        println!("\n{}", "─".repeat(60));
        println!("Download Summary");
        println!("{}", "─".repeat(60));

        if self.files_downloaded > 0 {
            println!("  Files downloaded:  {}", self.files_downloaded);
            println!("  Total size:        {}", format_bytes(self.total_bytes));
            println!("  Total time:        {}", format_duration(self.elapsed()));
            println!(
                "  Average speed:     {}/s",
                format_bytes(self.average_speed())
            );
            println!("  Peak speed:        {}/s", format_bytes(self.peak_speed));
            if let Some(ramp) = self.average_ramp_up() {
                println!(
                    "  Avg ramp-up:       {} to 80% of peak",
                    format_duration(ramp)
                );
            }
        }

        if self.files_skipped > 0 {
            println!("  Files skipped:     {}", self.files_skipped);
        }

        println!("{}", "─".repeat(60));
    }
}

// ============================================================================
// Download Functions
// ============================================================================

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
async fn download_file(
    client: &mega::Client,
    progress: &MultiProgress,
    total_bar: &ProgressBar,
    item: &DownloadItem<'_>,
    chunks: usize,
) -> octo_dl::Result<(u64, Option<Duration>)> {
    let DownloadItem { path, node } = item;

    ensure_parent_dir(path);

    let part_file = format!("{path}.part");
    let stats = Arc::new(DownloadStats::new(node.size()));
    let bar = progress.insert_before(total_bar, make_progress_bar(node.size(), node.name()));
    bar.enable_steady_tick(std::time::Duration::from_millis(250));

    let bar_clone = bar.clone();
    let total_bar_clone = total_bar.clone();
    let stats_clone = Arc::clone(&stats);

    // Open .part file for parallel chunk download with MAC verification
    let file = tokio::fs::File::create(&part_file).await?;
    file.set_len(node.size()).await?;

    let name_for_progress = node.name().to_string();
    let result = client
        .download_node_parallel(
            node,
            file,
            chunks,
            Some(move |delta| {
                bar_clone.inc(delta);
                total_bar_clone.inc(delta);
                // per_sec() returns f64; as u64 saturates (Rust 1.45+)
                stats_clone.update_speed(bar_clone.per_sec() as u64);
                bar_clone.set_message(name_for_progress.clone());
            }),
        )
        .await;

    if result.is_ok() {
        // Rename .part → final
        tokio::fs::rename(&part_file, path).await?;
        bar.finish_and_clear();
        let ramp_up = stats.time_to_80pct().map_or_else(
            || "ramp <1s".to_string(),
            |d| format!("ramp {}", format_duration(d)),
        );
        let _ = progress.println(format!(
            "  {} - {} in {} ({}/s avg, {}/s peak, {})",
            node.name(),
            format_bytes(node.size()),
            format_duration(stats.elapsed()),
            format_bytes(stats.average_speed()),
            format_bytes(stats.peak_speed()),
            ramp_up,
        ));
    } else {
        // Clean up .part file on error
        let _ = tokio::fs::remove_file(&part_file).await;
        bar.abandon();
    }

    result
        .map(|()| (node.size(), stats.time_to_80pct()))
        .map_err(octo_dl::Error::from)
}

fn ensure_parent_dir(path: &str) {
    if let Some(parent) = Path::new(path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        let _ = std::fs::create_dir_all(parent);
    }
}

fn print_file_list(files: &[DownloadItem], skipped: usize, partial: usize) {
    if files.is_empty() && skipped == 0 {
        println!("No files found.");
        return;
    }

    let total_size: u64 = files.iter().map(|i| i.node.size()).sum();

    println!("\n{}", "─".repeat(60));
    println!("Files to download:");
    println!("{}", "─".repeat(60));

    for item in files {
        println!("  {} ({})", item.path, format_bytes(item.node.size()));
    }

    println!("{}", "─".repeat(60));
    println!(
        "  {} file(s), {} total",
        files.len(),
        format_bytes(total_size)
    );
    if skipped > 0 {
        println!("  {skipped} file(s) skipped (already exist)");
    }
    if partial > 0 {
        println!("  {partial} file(s) with partial downloads (will re-download)");
    }
    println!("{}\n", "─".repeat(60));
}

#[allow(clippy::similar_names)]
async fn download_all(
    client: &mega::Client,
    progress: &MultiProgress,
    total_bar: &ProgressBar,
    files: &[DownloadItem<'_>],
    config: &DownloadConfig,
    session_stats: &mut CliSessionStats,
    mut session_state: Option<&mut SessionState>,
) -> octo_dl::Result<()> {
    if files.is_empty() {
        return Ok(());
    }

    // Track aggregate peak speed from total_bar
    let session_peak = Arc::new(AtomicU64::new(0));
    let session_peak_clone = Arc::clone(&session_peak);

    let results: Vec<_> = stream::iter(files)
        .map(|item| {
            let peak_tracker = Arc::clone(&session_peak_clone);
            async move {
                let result =
                    download_file(client, progress, total_bar, item, config.chunks_per_file).await;
                // Update session peak from total_bar's aggregate speed
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let current_speed = total_bar.per_sec() as u64;
                peak_tracker.fetch_max(current_speed, Ordering::Relaxed);
                (item.path.clone(), result)
            }
        })
        .buffer_unordered(config.concurrent_files)
        .collect()
        .await;

    // Use aggregate peak, not per-file peak
    session_stats.peak_speed = session_peak.load(Ordering::Relaxed);

    for (path, result) in results {
        match result {
            Ok((bytes, ramp_up)) => {
                session_stats.add_download(bytes, ramp_up);
                if let Some(ref mut state) = session_state.as_deref_mut() {
                    let _ = state.mark_file_complete(&path);
                }
            }
            Err(e) => {
                let _ = progress.println(format!("Download error: {e:?}"));
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
    let args: Vec<_> = env::args().skip(1).collect();

    let mut urls = Vec::new();
    let mut dlc_files = Vec::new();
    let mut chunks_per_file = DEFAULT_CHUNKS_PER_FILE;
    let mut concurrent_files = DEFAULT_CONCURRENT_FILES;
    let mut force = false;
    let mut resume = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-j" | "--chunks" => {
                i += 1;
                if i < args.len() {
                    chunks_per_file = args[i].parse().unwrap_or(DEFAULT_CHUNKS_PER_FILE);
                }
            }
            "-p" | "--parallel" => {
                i += 1;
                if i < args.len() {
                    concurrent_files = args[i].parse().unwrap_or(DEFAULT_CONCURRENT_FILES);
                }
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
            arg if !arg.starts_with('-') => {
                // Check if it's a DLC file (case-insensitive)
                if Path::new(arg)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("dlc"))
                {
                    dlc_files.push(arg.to_string());
                } else {
                    urls.push(arg.to_string());
                }
            }
            _ => {
                eprintln!("Unknown option: {}", args[i]);
                std::process::exit(1);
            }
        }
        i += 1;
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
    eprintln!("Usage: octo-dl [OPTIONS] <url|dlc>...");
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
    eprintln!("  -h, --help          Show this help");
    eprintln!();
    eprintln!("Environment:");
    eprintln!("  MEGA_EMAIL          MEGA account email");
    eprintln!("  MEGA_PASSWORD       MEGA account password");
    eprintln!("  MEGA_MFA            MEGA MFA code (optional)");
}

fn get_credentials() -> (String, String, Option<String>) {
    let email = env::var("MEGA_EMAIL").expect("MEGA_EMAIL not set");
    let password = env::var("MEGA_PASSWORD").expect("MEGA_PASSWORD not set");
    let mfa = env::var("MEGA_MFA").ok();
    (email, password, mfa)
}

// ============================================================================
// Node Collection (using library types)
// ============================================================================

fn collect_files<'a>(nodes: &'a mega::Nodes, node: &'a mega::Node) -> Vec<DownloadItem<'a>> {
    let (folders, files): (Vec<_>, Vec<_>) = node
        .children()
        .iter()
        .filter_map(|hash| nodes.get_node_by_handle(hash))
        .partition(|n| n.kind().is_folder());

    let current_files = files.into_iter().map(|file| DownloadItem {
        path: build_path(nodes, node, file),
        node: file,
    });

    let nested_files = folders
        .into_iter()
        .flat_map(|folder| collect_files(nodes, folder));

    current_files.chain(nested_files).collect()
}

fn build_path(nodes: &mega::Nodes, parent: &mega::Node, file: &mega::Node) -> String {
    if let Some(gp_handle) = parent.parent()
        && let Some(grandparent) = nodes.get_node_by_handle(gp_handle)
    {
        return format!("{}/{}/{}", grandparent.name(), parent.name(), file.name());
    }
    format!("{}/{}", parent.name(), file.name())
}

fn should_skip(path: &str, expected_size: u64, force: bool) -> bool {
    !force && std::fs::metadata(path).is_ok_and(|m| m.len() == expected_size)
}

fn has_part_file(path: &str) -> bool {
    std::fs::metadata(format!("{path}.part")).is_ok()
}

fn collect_from_nodes(nodes: &mega::Nodes, force: bool) -> CollectedFiles<'_> {
    let all_items: Vec<_> = nodes
        .roots()
        .flat_map(|root| {
            if root.kind().is_folder() {
                collect_files(nodes, root)
            } else {
                vec![DownloadItem {
                    path: root.name().to_string(),
                    node: root,
                }]
            }
        })
        .collect();

    let mut to_download = Vec::new();
    let mut skipped = 0;
    let mut partial = 0;

    for item in all_items {
        if should_skip(&item.path, item.node.size(), force) {
            skipped += 1;
        } else {
            if has_part_file(&item.path) {
                partial += 1;
            }
            to_download.push(item);
        }
    }

    CollectedFiles {
        to_download,
        skipped,
        partial,
    }
}

// ============================================================================
// Main
// ============================================================================

#[allow(clippy::too_many_lines, clippy::similar_names)]
#[tokio::main]
async fn main() -> octo_dl::Result<()> {
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
    let http = reqwest::Client::builder()
        .pool_idle_timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(8)
        .tcp_keepalive(Duration::from_secs(30))
        .build()
        .expect("Failed to build HTTP client");

    // Process DLC files before logging in
    if !config.dlc_files.is_empty() {
        println!("Processing DLC files...\n");
        let dlc_cache = DlcKeyCache::new();
        for dlc_path in &config.dlc_files {
            print!("  {dlc_path} ... ");
            match octo_dl::parse_dlc_file(dlc_path, &http, &dlc_cache).await {
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

    let mut client = mega::Client::builder().build(http)?;

    println!("Logging in...");
    client.login(&email, &password, mfa.as_deref()).await?;
    println!("Logged in successfully.");

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
        match client.fetch_public_nodes(url).await {
            Ok(nodes) => {
                let file_count: usize = nodes
                    .roots()
                    .map(|r| {
                        if r.kind().is_folder() {
                            collect_files(&nodes, r).len()
                        } else {
                            1
                        }
                    })
                    .sum();
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
        let collected = collect_from_nodes(nodes, config.download_config.force_overwrite);
        // Record files in session state
        for item in &collected.to_download {
            session_state.files.push(FileEntry {
                url_index: url_idx,
                path: item.path.clone(),
                size: item.node.size(),
                status: FileEntryStatus::Pending,
            });
        }
        all_files.extend(collected.to_download);
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

    let mut session_stats = CliSessionStats::new();
    session_stats.files_skipped = total_skipped;

    download_all(
        &client,
        &progress,
        &total_bar,
        &all_files,
        &config.download_config,
        &mut session_stats,
        Some(&mut session_state),
    )
    .await?;

    total_bar.finish_and_clear();
    progress.clear().ok();
    session_stats.print_summary();

    // Mark session as completed
    let _ = session_state.mark_completed();

    Ok(())
}

/// Resume a previous incomplete session.
async fn resume_session(mut session: SessionState, config: &CliConfig) -> octo_dl::Result<()> {
    // Decrypt credentials
    let (email, password, mfa) = session
        .credentials
        .decrypt()
        .expect("Failed to decrypt session credentials");

    let http = reqwest::Client::builder()
        .pool_idle_timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(8)
        .tcp_keepalive(Duration::from_secs(30))
        .build()
        .expect("Failed to build HTTP client");

    let mut client = mega::Client::builder().build(http)?;

    println!("Logging in...");
    client.login(&email, &password, mfa.as_deref()).await?;
    println!("Logged in successfully.");

    // Re-fetch URLs and collect remaining files
    let remaining_urls: Vec<_> = session
        .urls
        .iter()
        .filter(|u| u.status == UrlStatus::Fetched)
        .map(|u| u.url.clone())
        .collect();

    println!(
        "Fetching file lists from {} URL(s)...\n",
        remaining_urls.len()
    );
    let mut all_nodes: Vec<(String, mega::Nodes)> = Vec::new();
    for url in &remaining_urls {
        print!("  {url} ... ");
        match client.fetch_public_nodes(url).await {
            Ok(nodes) => {
                let file_count: usize = nodes
                    .roots()
                    .map(|r| {
                        if r.kind().is_folder() {
                            collect_files(&nodes, r).len()
                        } else {
                            1
                        }
                    })
                    .sum();
                println!("{file_count} file(s)");
                all_nodes.push((url.clone(), nodes));
            }
            Err(e) => println!("ERROR: {e:?}"),
        }
    }

    // Completed file paths from session state
    let completed_paths: std::collections::HashSet<String> = session
        .files
        .iter()
        .filter(|f| f.status == FileEntryStatus::Completed)
        .map(|f| f.path.clone())
        .collect();

    // Collect files, skipping already-completed ones
    let mut all_files: Vec<DownloadItem> = Vec::new();
    let mut total_skipped = 0;
    let mut total_partial = 0;
    for (_url, nodes) in &all_nodes {
        let collected = collect_from_nodes(nodes, config.download_config.force_overwrite);
        for item in collected.to_download {
            if completed_paths.contains(&item.path) {
                total_skipped += 1;
            } else {
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

    let mut session_stats = CliSessionStats::new();
    session_stats.files_skipped = total_skipped;

    download_all(
        &client,
        &progress,
        &total_bar,
        &all_files,
        &config.download_config,
        &mut session_stats,
        Some(&mut session),
    )
    .await?;

    total_bar.finish_and_clear();
    progress.clear().ok();
    session_stats.print_summary();

    let _ = session.mark_completed();

    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn skip_existing_file_with_correct_size() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        File::create(&path).unwrap().write_all(b"hello").unwrap();
        assert!(should_skip(path.to_str().unwrap(), 5, false));
    }

    #[test]
    fn dont_skip_file_with_wrong_size() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        File::create(&path).unwrap().write_all(b"hello").unwrap();
        assert!(!should_skip(path.to_str().unwrap(), 100, false));
    }

    #[test]
    fn dont_skip_missing_file() {
        assert!(!should_skip("/nonexistent/file.txt", 100, false));
    }

    #[test]
    fn force_overwrite_existing_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        File::create(&path).unwrap().write_all(b"hello").unwrap();
        assert!(!should_skip(path.to_str().unwrap(), 5, true));
    }

    #[test]
    fn detect_part_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        let part = dir.path().join("test.txt.part");
        File::create(&part).unwrap();
        assert!(has_part_file(path.to_str().unwrap()));
    }

    #[test]
    fn no_part_file() {
        assert!(!has_part_file("/nonexistent/file.txt"));
    }

    #[test]
    fn ensure_parent_creates_nested_dirs() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a/b/c/file.txt");
        ensure_parent_dir(path.to_str().unwrap());
        assert!(dir.path().join("a/b/c").exists());
    }

    #[test]
    fn ensure_parent_handles_root_file() {
        ensure_parent_dir("file.txt");
    }

    #[test]
    fn progress_bar_creation() {
        let bar = make_progress_bar(1000, "test.txt");
        assert_eq!(bar.length(), Some(1000));
    }

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1024), "1.00 KB");
        assert_eq!(format_bytes(1536), "1.50 KB");
        assert_eq!(format_bytes(1048576), "1.00 MB");
        assert_eq!(format_bytes(1073741824), "1.00 GB");
    }

    #[test]
    fn format_duration_units() {
        assert_eq!(format_duration(Duration::from_secs(5)), "5.0s");
        assert_eq!(format_duration(Duration::from_secs(65)), "1m 05s");
        assert_eq!(format_duration(Duration::from_secs(3665)), "1h 01m 05s");
    }

    mod property_tests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn format_bytes_never_panics(bytes in 0u64..u64::MAX) {
                let _ = format_bytes(bytes);
            }

            #[test]
            fn format_bytes_monotonic(a in 0u64..1_000_000_000, b in 1_000_000_000u64..u64::MAX) {
                let _ = (format_bytes(a), format_bytes(b));
            }

            #[test]
            fn format_duration_never_panics(secs in 0u64..1_000_000) {
                let _ = format_duration(Duration::from_secs(secs));
            }

            #[test]
            fn format_duration_millis_never_panics(millis in 0u64..1_000_000_000) {
                let _ = format_duration(Duration::from_millis(millis));
            }

            #[test]
            fn download_stats_speed_never_panics(bytes in 0u64..u64::MAX, speed in 0u64..1_000_000_000) {
                let stats = DownloadStats::new(bytes);
                stats.update_speed(speed);
                let _ = stats.average_speed();
                let _ = stats.peak_speed();
            }

            #[test]
            fn session_stats_never_panics(
                files in 0usize..1000,
                bytes in 0u64..1_000_000_000_000,
                ramp_up_ms in proptest::option::of(0u64..60_000)
            ) {
                let mut stats = CliSessionStats::new();
                for _ in 0..files {
                    let ramp_up = ramp_up_ms.map(Duration::from_millis);
                    stats.add_download(bytes / (files.max(1) as u64), ramp_up);
                }
                let _ = stats.elapsed();
                let _ = stats.average_speed();
                let _ = stats.average_ramp_up();
            }
        }
    }
}
