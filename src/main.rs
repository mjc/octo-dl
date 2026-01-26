#![warn(clippy::pedantic)]
#![warn(clippy::nursery)]

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
}

struct DownloadItem<'a> {
    path: String,
    node: &'a mega::Node,
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

fn should_skip(path: &str, expected_size: u64) -> bool {
    fs::metadata(path).is_ok_and(|m| m.len() == expected_size)
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
) -> Result<()> {
    let DownloadItem { path, node } = item;

    ensure_parent_dir(path);

    let bar = progress.add(make_progress_bar(node.size(), node.name()));
    bar.enable_steady_tick(std::time::Duration::from_millis(100));
    let bar_clone = bar.clone();

    // Open file for parallel chunk download with MAC verification
    let file = tokio::fs::File::create(path).await?;
    file.set_len(node.size()).await?;

    let result = client
        .download_node_parallel(node, file, chunks, Some(move |bytes| {
            bar_clone.set_position(bytes);
        }))
        .await;

    match &result {
        Ok(()) => bar.finish_and_clear(),
        Err(_) => bar.abandon(),
    }

    result
}

async fn process_url(
    client: &mega::Client,
    progress: &MultiProgress,
    url: &str,
    config: &Config,
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

    let items: Vec<_> = all_items
        .into_iter()
        .filter(|item| !should_skip(&item.path, item.node.size()))
        .collect();

    if items.is_empty() {
        if !found_any {
            let _ = progress.println("No files found in the shared folder.");
        } else {
            let _ = progress.println("All files already downloaded.");
        }
        return Ok(());
    }

    let _ = progress.println(format!("Downloading {} file(s) with {} chunks each...", items.len(), config.chunks_per_file));

    stream::iter(&items)
        .map(|item| download_file(client, progress, item, config.chunks_per_file))
        .buffer_unordered(config.concurrent_files)
        .for_each(|result| async {
            if let Err(e) = result {
                let _ = progress.println(format!("Download error: {e:?}"));
            }
        })
        .await;

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
    }
}

fn print_usage() {
    eprintln!("Usage: octo-dl [OPTIONS] <url>...");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  -j, --chunks <N>    Chunks per file for parallel download (default: {})", DEFAULT_CHUNKS_PER_FILE);
    eprintln!("  -p, --parallel <N>  Concurrent file downloads (default: {})", DEFAULT_CONCURRENT_FILES);
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

    for url in &config.urls {
        let _ = progress.println(format!("Processing: {url}"));
        if let Err(e) = process_url(&client, &progress, url, &config).await {
            let _ = progress.println(format!("Error processing {url}: {e:?}"));
        }
        let _ = progress.println("");
    }

    progress.clear().ok();
    println!("Done.");
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
        assert!(should_skip(path.to_str().unwrap(), 5));
    }

    #[test]
    fn dont_skip_file_with_wrong_size() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        File::create(&path).unwrap().write_all(b"hello").unwrap();
        assert!(!should_skip(path.to_str().unwrap(), 100));
    }

    #[test]
    fn dont_skip_missing_file() {
        assert!(!should_skip("/nonexistent/file.txt", 100));
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
}
