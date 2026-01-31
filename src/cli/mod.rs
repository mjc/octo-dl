//! octo-dl CLI - Command-line interface for downloading MEGA files.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use dirs;
use futures::{StreamExt, stream};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tokio_util::compat::TokioAsyncWriteCompatExt;

use crate::{
    DlcKeyCache, DownloadConfig, DownloadItem, DownloadStatsTracker, FileEntry, FileEntryStatus,
    FileStats, NoProgress, SavedCredentials, SessionState, SessionStats, SessionStatsBuilder,
    SessionStatus, UrlEntry, UrlStatus, format_bytes, format_duration, is_dlc_path,
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

fn dummy_downloader(config: &DownloadConfig) -> crate::Downloader {
    let http = reqwest::Client::new();
    let client = mega::Client::builder()
        .build(http)
        .expect("client builder");
    crate::Downloader::new(client, config.clone())
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

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
async fn download_file(
    client: &mega::Client,
    progress: &MultiProgress,
    total_bar: &ProgressBar,
    item: &DownloadItem<'_>,
    chunks: usize,
) -> crate::Result<FileStats> {
    let DownloadItem { path, node } = item;

    // Ensure parent directory exists
    if let Some(parent) = std::path::Path::new(path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        let _ = std::fs::create_dir_all(parent);
    }

    let part_file = format!("{path}.part");
    let stats = Arc::new(DownloadStatsTracker::new(node.size()));
    let bar = progress.insert_before(total_bar, make_progress_bar(node.size(), node.name()));
    bar.enable_steady_tick(std::time::Duration::from_millis(250));

    let bar_clone = bar.clone();
    let total_bar_clone = total_bar.clone();
    let stats_clone = Arc::clone(&stats);

    // Open .part file for parallel chunk download with MAC verification
    let file = tokio::fs::File::create(&part_file).await?;
    file.set_len(node.size()).await?;
    let file = file.compat_write();

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
        let file_stats = FileStats {
            size: node.size(),
            elapsed: stats.elapsed(),
            average_speed: stats.average_speed(),
            peak_speed: stats.peak_speed(),
            ramp_up_time: stats.time_to_80pct(),
        };
        let ramp_up = file_stats.ramp_up_time.map_or_else(
            || "ramp <1s".to_string(),
            |d| format!("ramp {}", format_duration(d)),
        );
        let _ = progress.println(format!(
            "  {} - {} in {} ({}/s avg, {}/s peak, {})",
            node.name(),
            format_bytes(file_stats.size),
            format_duration(file_stats.elapsed),
            format_bytes(file_stats.average_speed),
            format_bytes(file_stats.peak_speed),
            ramp_up,
        ));
        Ok(file_stats)
    } else {
        // Clean up .part file on error
        let _ = tokio::fs::remove_file(&part_file).await;
        bar.abandon();
        result
            .map(|()| unreachable!())
            .map_err(crate::Error::from)
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
        println!("  {partial} file(s) with partial downloads (will re-download)");
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

#[allow(clippy::similar_names)]
async fn download_all(
    client: &mega::Client,
    progress: &MultiProgress,
    total_bar: &ProgressBar,
    files: &[DownloadItem<'_>],
    config: &DownloadConfig,
    builder: &mut SessionStatsBuilder,
    mut session_state: Option<&mut SessionState>,
) -> crate::Result<()> {
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
    builder.set_peak_speed(session_peak.load(Ordering::Relaxed));

    for (path, result) in results {
        match result {
            Ok(file_stats) => {
                builder.add_download(&file_stats);
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
            "--api-host" => {
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
            let expanded_path = if dlc_path.starts_with('~') {
                match dirs::home_dir() {
                    Some(home) => dlc_path.replacen('~', home.to_string_lossy().as_ref(), 1),
                    None => {
                        eprintln!("Error: Could not determine home directory");
                        std::process::exit(1);
                    }
                }
            } else {
                dlc_path.to_string()
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

    let mut client = mega::Client::builder().build(http)?;

    println!("Logging in...");
    client.login(&email, &password, mfa.as_deref()).await?;
    println!("Logged in successfully.");

    // Create downloader for file collection
    let downloader = dummy_downloader(&config.download_config);
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
        match client.fetch_public_nodes(url).await {
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

    let mut builder = SessionStatsBuilder::new();
    builder.set_skipped(total_skipped);

    download_all(
        &client,
        &progress,
        &total_bar,
        &all_files,
        &config.download_config,
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
    // Decrypt credentials
    let (email, password, mfa) = session
        .credentials
        .decrypt()
        .expect("Failed to decrypt session credentials");

    let http = build_http_client()?;

    let mut client = mega::Client::builder().build(http)?;

    println!("Logging in...");
    client.login(&email, &password, mfa.as_deref()).await?;
    println!("Logged in successfully.");

    let downloader = dummy_downloader(&config.download_config);
    let no_progress: Arc<dyn crate::DownloadProgress> = Arc::new(NoProgress);

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
                let collected_tmp = downloader.collect_files(&nodes, &no_progress).await;
                let file_count = collected_tmp.to_download.len() + collected_tmp.skipped;
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
        let collected = downloader.collect_files(nodes, &no_progress).await;
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

    let mut builder = SessionStatsBuilder::new();
    builder.set_skipped(total_skipped);

    download_all(
        &client,
        &progress,
        &total_bar,
        &all_files,
        &config.download_config,
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
}
