#![warn(clippy::pedantic)]
#![warn(clippy::nursery)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{env, fs, path::Path};

use futures::{stream, StreamExt};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

const DEFAULT_CONCURRENT_FILES: usize = 4;
const DEFAULT_CHUNKS_PER_FILE: usize = 8;

// ============================================================================
// Core Types
// ============================================================================

type Result<T> = std::result::Result<T, mega::Error>;

struct Config {
    urls: Vec<String>,
    chunks_per_file: usize,
    concurrent_files: usize,
    force: bool,
}

struct DownloadItem<'a> {
    path: String,
    node: &'a mega::Node,
}

// ============================================================================
// Download Statistics
// ============================================================================

struct DownloadStats {
    start_time: Instant,
    total_bytes: u64,
    last_bytes: AtomicU64,
    last_time: std::sync::Mutex<Instant>,
    peak_speed: AtomicU64,
}

impl DownloadStats {
    fn new(total_bytes: u64) -> Self {
        let now = Instant::now();
        Self {
            start_time: now,
            total_bytes,
            last_bytes: AtomicU64::new(0),
            last_time: std::sync::Mutex::new(now),
            peak_speed: AtomicU64::new(0),
        }
    }

    fn update(&self, current_bytes: u64) {
        let now = Instant::now();
        let mut last_time = self.last_time.lock().unwrap();
        let elapsed = now.duration_since(*last_time);

        // Update speed calculation every 100ms minimum
        if elapsed >= Duration::from_millis(100) {
            let last = self.last_bytes.swap(current_bytes, Ordering::Relaxed);
            let bytes_delta = current_bytes.saturating_sub(last);
            let speed = (bytes_delta as f64 / elapsed.as_secs_f64()) as u64;

            // Update peak speed
            self.peak_speed.fetch_max(speed, Ordering::Relaxed);
            *last_time = now;
        }
    }

    fn elapsed(&self) -> Duration {
        self.start_time.elapsed()
    }

    fn average_speed(&self) -> u64 {
        let elapsed = self.elapsed().as_secs_f64();
        if elapsed > 0.0 {
            (self.total_bytes as f64 / elapsed) as u64
        } else {
            0
        }
    }

    fn peak_speed(&self) -> u64 {
        self.peak_speed.load(Ordering::Relaxed)
    }
}

struct SessionStats {
    files_downloaded: usize,
    files_skipped: usize,
    total_bytes: u64,
    start_time: Instant,
    peak_speed: u64,
}

impl SessionStats {
    fn new() -> Self {
        Self {
            files_downloaded: 0,
            files_skipped: 0,
            total_bytes: 0,
            start_time: Instant::now(),
            peak_speed: 0,
        }
    }

    fn add_download(&mut self, bytes: u64, peak: u64) {
        self.files_downloaded += 1;
        self.total_bytes += bytes;
        self.peak_speed = self.peak_speed.max(peak);
    }

    fn elapsed(&self) -> Duration {
        self.start_time.elapsed()
    }

    fn average_speed(&self) -> u64 {
        let elapsed = self.elapsed().as_secs_f64();
        if elapsed > 0.0 {
            (self.total_bytes as f64 / elapsed) as u64
        } else {
            0
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
            println!("  Average speed:     {}/s", format_bytes(self.average_speed()));
            println!("  Peak speed:        {}/s", format_bytes(self.peak_speed));
        }

        if self.files_skipped > 0 {
            println!("  Files skipped:     {}", self.files_skipped);
        }

        println!("{}", "─".repeat(60));
    }
}

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
        format!("{}h {:02}m {:02}s", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}.{:01}s", secs, d.subsec_millis() / 100)
    }
}

// ============================================================================
// Node Traversal (Functional Style)
// ============================================================================

fn collect_files<'a>(nodes: &'a mega::Nodes, node: &'a mega::Node) -> Vec<DownloadItem<'a>> {
    let (folders, files): (Vec<_>, Vec<_>) = node
        .children()
        .iter()
        .filter_map(|hash| nodes.get_node_by_handle(hash))
        .partition(|n| n.kind().is_folder());

    let current_files = files
        .into_iter()
        .filter_map(|file| {
            build_path(nodes, node, file).map(|path| DownloadItem { path, node: file })
        });

    let nested_files = folders
        .into_iter()
        .flat_map(|folder| collect_files(nodes, folder));

    current_files.chain(nested_files).collect()
}

fn build_path(nodes: &mega::Nodes, parent: &mega::Node, file: &mega::Node) -> Option<String> {
    // Try to build full path with grandparent, fallback to parent/file if no grandparent
    if let Some(gp_handle) = parent.parent() {
        if let Some(grandparent) = nodes.get_node_by_handle(gp_handle) {
            return Some(format!("{}/{}/{}", grandparent.name(), parent.name(), file.name()));
        }
    }
    // Parent is at root level, just use parent/file
    Some(format!("{}/{}", parent.name(), file.name()))
}

// ============================================================================
// File System Helpers
// ============================================================================

fn should_skip(path: &str, expected_size: u64, force: bool) -> bool {
    !force && fs::metadata(path).is_ok_and(|m| m.len() == expected_size)
}

fn ensure_parent_dir(path: &str) {
    if let Some(parent) = Path::new(path).parent().filter(|p| !p.as_os_str().is_empty()) {
        let _ = fs::create_dir_all(parent);
    }
}

// ============================================================================
// Download Logic
// ============================================================================

async fn download_file(
    client: &mega::Client,
    progress: &MultiProgress,
    item: &DownloadItem<'_>,
    chunks: usize,
) -> Result<(u64, u64)> {
    let DownloadItem { path, node } = item;

    ensure_parent_dir(path);

    let stats = Arc::new(DownloadStats::new(node.size()));
    let bar = progress.add(make_progress_bar(node.size(), node.name()));
    bar.enable_steady_tick(std::time::Duration::from_millis(100));

    let bar_clone = bar.clone();
    let stats_clone = Arc::clone(&stats);

    // Open file for parallel chunk download with MAC verification
    let file = tokio::fs::File::create(path).await?;
    file.set_len(node.size()).await?;

    let result = client
        .download_node_parallel(node, file, chunks, Some(move |bytes| {
            bar_clone.set_position(bytes);
            stats_clone.update(bytes);
        }))
        .await;

    match &result {
        Ok(()) => {
            bar.finish_and_clear();
            let _ = progress.println(format!(
                "  {} - {} in {} ({}/s avg, {}/s peak)",
                node.name(),
                format_bytes(node.size()),
                format_duration(stats.elapsed()),
                format_bytes(stats.average_speed()),
                format_bytes(stats.peak_speed()),
            ));
        }
        Err(_) => bar.abandon(),
    }

    result.map(|()| (node.size(), stats.peak_speed()))
}

async fn process_url(
    client: &mega::Client,
    progress: &MultiProgress,
    url: &str,
    config: &Config,
    session_stats: &mut SessionStats,
) -> Result<()> {
    let nodes = client.fetch_public_nodes(url).await?;

    let all_items: Vec<_> = nodes
        .roots()
        .flat_map(|root| {
            if root.kind().is_folder() {
                collect_files(&nodes, root)
            } else {
                vec![DownloadItem {
                    path: root.name().to_string(),
                    node: root,
                }]
            }
        })
        .collect();

    let found_any = !all_items.is_empty();

    let (to_download, to_skip): (Vec<_>, Vec<_>) = all_items
        .into_iter()
        .partition(|item| !should_skip(&item.path, item.node.size(), config.force));

    session_stats.files_skipped += to_skip.len();

    if to_download.is_empty() {
        if !found_any {
            let _ = progress.println("No files found in the shared folder.");
        } else {
            let _ = progress.println(format!(
                "All {} file(s) already downloaded.",
                to_skip.len()
            ));
        }
        return Ok(());
    }

    let total_size: u64 = to_download.iter().map(|i| i.node.size()).sum();
    let _ = progress.println(format!(
        "Downloading {} file(s) ({}) with {} chunks each...\n",
        to_download.len(),
        format_bytes(total_size),
        config.chunks_per_file
    ));

    let results: Vec<_> = stream::iter(&to_download)
        .map(|item| download_file(client, progress, item, config.chunks_per_file))
        .buffer_unordered(config.concurrent_files)
        .collect()
        .await;

    for result in results {
        match result {
            Ok((bytes, peak)) => session_stats.add_download(bytes, peak),
            Err(e) => {
                let _ = progress.println(format!("Download error: {e:?}"));
            }
        }
    }

    Ok(())
}

// ============================================================================
// Progress Bar
// ============================================================================

fn make_progress_bar(size: u64, name: &str) -> ProgressBar {
    let bar = ProgressBar::new(size);
    bar.set_style(
        ProgressStyle::with_template(
            "{spinner:.cyan} [{bar:40.cyan/blue}] {bytes}/{total_bytes} @ {bytes_per_sec} - {msg}",
        )
        .unwrap()
        .progress_chars("━━╌"),
    );
    bar.set_message(name.to_string());
    bar
}

// ============================================================================
// CLI Parsing
// ============================================================================

fn parse_args() -> Config {
    let args: Vec<_> = env::args().skip(1).collect();

    let mut urls = Vec::new();
    let mut chunks_per_file = DEFAULT_CHUNKS_PER_FILE;
    let mut concurrent_files = DEFAULT_CONCURRENT_FILES;
    let mut force = false;

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
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            arg if !arg.starts_with('-') => {
                urls.push(arg.to_string());
            }
            _ => {
                eprintln!("Unknown option: {}", args[i]);
                std::process::exit(1);
            }
        }
        i += 1;
    }

    Config {
        urls,
        chunks_per_file,
        concurrent_files,
        force,
    }
}

fn print_usage() {
    eprintln!("Usage: octo-dl [OPTIONS] <url>...");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  -j, --chunks <N>    Chunks per file for parallel download (default: {})", DEFAULT_CHUNKS_PER_FILE);
    eprintln!("  -p, --parallel <N>  Concurrent file downloads (default: {})", DEFAULT_CONCURRENT_FILES);
    eprintln!("  -f, --force         Overwrite existing files");
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
// Main
// ============================================================================

#[tokio::main]
async fn main() -> Result<()> {
    let config = parse_args();

    if config.urls.is_empty() {
        print_usage();
        std::process::exit(1);
    }

    let (email, password, mfa) = get_credentials();

    let http = reqwest::Client::new();
    let mut client = mega::Client::builder().build(http)?;

    println!("Logging in...");
    client.login(&email, &password, mfa.as_deref()).await?;
    println!("Logged in successfully.\n");

    let progress = MultiProgress::new();
    let mut session_stats = SessionStats::new();

    for url in &config.urls {
        let _ = progress.println(format!("Processing: {url}"));
        if let Err(e) = process_url(&client, &progress, url, &config, &mut session_stats).await {
            let _ = progress.println(format!("Error processing {url}: {e:?}"));
        }
    }

    progress.clear().ok();
    session_stats.print_summary();

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
}
