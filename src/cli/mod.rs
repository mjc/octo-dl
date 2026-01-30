//! CLI mode for octo - command-line interface for downloading MEGA files.

mod progress;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::collections::HashSet;

use futures::{StreamExt, stream};
use indicatif::MultiProgress;

use crate::{
    AppConfig, DlcKeyCache, DownloadConfig, DownloadItem, FileEntry, FileEntryStatus,
    NoProgress, SavedCredentials, SessionState, SessionStatsBuilder,
    SessionStatus, UrlEntry, UrlStatus, format_bytes, format_duration, is_dlc_path,
    parse_dlc_file, DownloadProgress,
};

use progress::{make_progress_bar, make_total_progress_bar, print_file_list, print_summary};

/// Builds a configured HTTP client for MEGA requests.
fn build_http_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .pool_idle_timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(8)
        .tcp_keepalive(Duration::from_secs(30))
        .build()
}

/// Creates a dummy downloader for file collection without download progress.
fn dummy_downloader(config: &DownloadConfig) -> crate::Downloader {
    let http = reqwest::Client::new();
    let client = mega::Client::builder()
        .build(http)
        .expect("client builder");
    crate::Downloader::new(client, config.clone())
}

/// Downloads a single file with progress reporting.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
async fn download_file(
    client: &mega::Client,
    progress: &MultiProgress,
    total_bar: &indicatif::ProgressBar,
    item: &DownloadItem<'_>,
    chunks: usize,
) -> crate::Result<crate::FileStats> {
    let DownloadItem { path, node } = item;

    // Ensure parent directory exists
    if let Some(parent) = std::path::Path::new(path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        let _ = std::fs::create_dir_all(parent);
    }

    let part_file = format!("{path}.part");
    let stats = Arc::new(crate::DownloadStatsTracker::new(node.size()));
    let bar = progress.insert_before(total_bar, make_progress_bar(node.size(), node.name()));
    bar.enable_steady_tick(Duration::from_millis(250));

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
        let file_stats = crate::FileStats {
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

/// Orchestrates downloading all files with progress reporting.
#[allow(clippy::similar_names)]
async fn download_all(
    client: &mega::Client,
    progress: &MultiProgress,
    total_bar: &indicatif::ProgressBar,
    files: &[DownloadItem<'_>],
    config: &DownloadConfig,
    builder: &mut SessionStatsBuilder,
    mut session_state: Option<&mut SessionState>,
    state_dir: &std::path::Path,
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
                    let _ = state.mark_file_complete(&path, state_dir);
                }
            }
            Err(e) => {
                let _ = progress.println(format!("Download error: {e:?}"));
                if let Some(ref mut state) = session_state.as_deref_mut() {
                    let _ = state.mark_file_error(&path, &e.to_string(), state_dir);
                }
            }
        }
    }

    Ok(())
}

/// Gets MEGA credentials from environment variables.
fn get_credentials() -> crate::Result<(String, String, Option<String>)> {
    let email = std::env::var("MEGA_EMAIL")
        .map_err(|_| crate::Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "MEGA_EMAIL environment variable not set"
        )))?;
    let password = std::env::var("MEGA_PASSWORD")
        .map_err(|_| crate::Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "MEGA_PASSWORD environment variable not set"
        )))?;
    let mfa = std::env::var("MEGA_MFA").ok();
    Ok((email, password, mfa))
}

/// Runs the CLI download mode with the given URLs.
///
/// # Errors
///
/// Returns an error if the download fails.
pub async fn run_download(config: AppConfig, urls: Vec<String>, resume: bool) -> crate::Result<()> {
    // Check for resumable session
    if resume {
        if let Some(session) = SessionState::latest(&config.paths.state_dir) {
            println!(
                "Resuming session {} ({} files, {} completed)",
                session.id,
                session.files.len(),
                session.completed_count()
            );
            return resume_session(session, &config).await;
        }
        println!("No resumable session found, starting fresh.");
    } else if urls.is_empty() {
        // Check if there's a session to resume
        if let Some(session) = SessionState::latest(&config.paths.state_dir) {
            println!(
                "Found incomplete session: {} ({} remaining files)",
                session.id,
                session.remaining_count()
            );
            println!("Use --resume to continue, or provide URLs to start a new session.");
            return Ok(());
        }
        log::info!("No URLs provided and no resumable session found");
        return Ok(());
    }

    let (email, password, mfa) = get_credentials()?;

    // Create HTTP client with custom user agent for DLC service
    let http = build_http_client()?;

    // Process any DLC files in the URLs
    let mut all_urls = Vec::new();
    let dlc_cache = DlcKeyCache::new();

    for url in urls {
        if is_dlc_path(&url) {
            log::info!("Processing DLC file: {}", url);
            // Expand ~ to home directory for local DLC files
            let expanded_path = if url.starts_with('~') {
                match dirs::home_dir() {
                    Some(home) => url.replacen('~', home.to_string_lossy().as_ref(), 1),
                    None => {
                        return Err(crate::Error::Io(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "Could not determine home directory"
                        )));
                    }
                }
            } else {
                url.clone()
            };
            match parse_dlc_file(&expanded_path, &http, &dlc_cache).await {
                Ok(dlc_urls) => {
                    log::info!("Extracted {} URLs from DLC file", dlc_urls.len());
                    all_urls.extend(dlc_urls);
                }
                Err(e) => {
                    log::error!("Error parsing DLC file: {}", e);
                    return Err(e);
                }
            }
        } else {
            all_urls.push(url);
        }
    }

    if all_urls.is_empty() {
        log::info!("No URLs to download");
        return Ok(());
    }

    let mut client = mega::Client::builder().build(http)?;

    println!("Logging in...");
    client.login(&email, &password, mfa.as_deref()).await?;
    println!("Logged in successfully.");

    // Create downloader for file collection
    let downloader = dummy_downloader(&config.download);
    let no_progress: Arc<dyn DownloadProgress> = Arc::new(NoProgress);

    // Create session state for persistence
    let url_entries: Vec<UrlEntry> = all_urls
        .iter()
        .map(|url| UrlEntry {
            url: url.clone(),
            status: UrlStatus::Pending,
        })
        .collect();

    let mut session_state = SessionState::new(
        SavedCredentials::encrypt(&email, &password, mfa.as_deref()),
        config.download.clone(),
        url_entries,
    );

    // Phase 1: Fetch all URLs and collect files
    println!("Fetching file lists from {} URL(s)...\n", all_urls.len());
    let mut all_nodes: Vec<(String, mega::Nodes)> = Vec::new();
    for (idx, url) in all_urls.iter().enumerate() {
        print!("  {url} ... ");
        match client.fetch_public_nodes(url).await {
            Ok(nodes) => {
                let collected_tmp = downloader.collect_files(&nodes, &no_progress, &config.paths.download_dir).await;
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
        let collected = downloader.collect_files(nodes, &no_progress, &config.paths.download_dir).await;
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
    let _ = session_state.save(&config.paths.state_dir);

    // Phase 2: Print what we found
    print_file_list(&all_files, total_skipped, total_partial);

    if all_files.is_empty() {
        if total_skipped > 0 {
            println!("All files already downloaded.");
        }
        let _ = session_state.mark_completed(&config.paths.state_dir);
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
        &config.download,
        &mut builder,
        Some(&mut session_state),
        &config.paths.state_dir,
    )
    .await?;

    total_bar.finish_and_clear();
    progress.clear().ok();
    let session_stats = builder.build();
    print_summary(&session_stats);

    // Mark session as completed
    let _ = session_state.mark_completed(&config.paths.state_dir);

    Ok(())
}

/// Resumes a previous incomplete session.
async fn resume_session(mut session: SessionState, config: &AppConfig) -> crate::Result<()> {
    // Decrypt credentials
    let (email, password, mfa) = session
        .credentials
        .decrypt()
        .ok_or_else(|| crate::Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Failed to decrypt session credentials"
        )))?;

    let http = build_http_client()?;

    let mut client = mega::Client::builder().build(http)?;

    println!("Logging in...");
    client.login(&email, &password, mfa.as_deref()).await?;
    println!("Logged in successfully.");

    let downloader = dummy_downloader(&config.download);
    let no_progress: Arc<dyn DownloadProgress> = Arc::new(NoProgress);

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
                let collected_tmp = downloader.collect_files(&nodes, &no_progress, &config.paths.download_dir).await;
                let file_count = collected_tmp.to_download.len() + collected_tmp.skipped;
                println!("{file_count} file(s)");
                all_nodes.push((url.clone(), nodes));
            }
            Err(e) => println!("ERROR: {e:?}"),
        }
    }

    // Completed file paths from session state
    let completed_paths: HashSet<String> = session
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
        let collected = downloader.collect_files(nodes, &no_progress, &config.paths.download_dir).await;
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
        let _ = session.mark_completed(&config.paths.state_dir);
        return Ok(());
    }

    session.status = SessionStatus::InProgress;
    let _ = session.save(&config.paths.state_dir);

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
        &config.download,
        &mut builder,
        Some(&mut session),
        &config.paths.state_dir,
    )
    .await?;

    total_bar.finish_and_clear();
    progress.clear().ok();
    let session_stats = builder.build();
    print_summary(&session_stats);

    let _ = session.mark_completed(&config.paths.state_dir);

    Ok(())
}
