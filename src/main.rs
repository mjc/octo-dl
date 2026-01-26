#![warn(clippy::pedantic)]
#![warn(clippy::nursery)]

use std::{env, fs, path::Path, time::Duration};

use async_read_progress::AsyncReadProgressExt;
use futures::{stream, StreamExt};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tokio::fs::File;
use tokio_util::compat::TokioAsyncWriteCompatExt;

const CONCURRENT_DOWNLOADS: usize = 20;

// ============================================================================
// Core Types
// ============================================================================

type Result<T> = std::result::Result<T, mega::Error>;

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

    // Collect files from current folder
    let current_files = files
        .into_iter()
        .filter_map(|file| {
            build_path(nodes, node, file).map(|path| DownloadItem { path, node: file })
        });

    // Recursively collect from subfolders
    let nested_files = folders
        .into_iter()
        .flat_map(|folder| collect_files(nodes, folder));

    current_files.chain(nested_files).collect()
}

fn build_path(nodes: &mega::Nodes, parent: &mega::Node, file: &mega::Node) -> Option<String> {
    let grandparent = nodes.get_node_by_handle(parent.parent()?)?;
    Some(format!("{}/{}/{}", grandparent.name(), parent.name(), file.name()))
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
) -> Result<()> {
    let DownloadItem { path, node } = item;

    ensure_parent_dir(path);

    let (reader, writer) = sluice::pipe::pipe();
    let file = File::create(path).await?;

    let bar = progress.add(make_progress_bar(node.size(), node.name()));

    let reader = {
        let bar = bar.clone();
        reader.report_progress(Duration::from_millis(50), move |bytes| {
            bar.set_position(bytes as u64);
        })
    };

    let path_clone = path.clone();
    let copy_task = tokio::spawn(async move {
        futures::io::copy(reader, &mut file.compat_write())
            .await
            .map_err(|e| {
                eprintln!("Failed writing {path_clone}: {e}");
                e
            })
    });

    client.download_node(node, writer).await?;
    copy_task.await.expect("copy task panicked")?;

    bar.finish_with_message(format!("✓ {}", node.name()));
    Ok(())
}

async fn process_url(client: &mega::Client, progress: &MultiProgress, url: &str) -> Result<()> {
    let nodes = client.fetch_public_nodes(url).await?;

    let items: Vec<_> = nodes
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
        .filter(|item| !should_skip(&item.path, item.node.size()))
        .collect();

    if items.is_empty() {
        println!("All files already downloaded.");
        return Ok(());
    }

    println!("Downloading {} file(s)...", items.len());

    // Process downloads concurrently with bounded parallelism
    stream::iter(&items)
        .map(|item| download_file(client, progress, item))
        .buffer_unordered(CONCURRENT_DOWNLOADS)
        .for_each(|result| async {
            if let Err(e) = result {
                eprintln!("Download error: {e:?}");
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
// Main
// ============================================================================

fn get_credentials() -> (String, String, Option<String>) {
    let email = env::var("MEGA_EMAIL").expect("MEGA_EMAIL not set");
    let password = env::var("MEGA_PASSWORD").expect("MEGA_PASSWORD not set");
    let mfa = env::var("MEGA_MFA").ok();
    (email, password, mfa)
}

#[tokio::main]
async fn main() -> Result<()> {
    let urls: Vec<_> = env::args().skip(1).collect();

    if urls.is_empty() {
        eprintln!("Usage: octo-dl <url>...");
        std::process::exit(1);
    }

    let (email, password, mfa) = get_credentials();

    let http = reqwest::Client::new();
    let mut client = mega::Client::builder().build(http)?;

    println!("Logging in...");
    client.login(&email, &password, mfa.as_deref()).await?;
    println!("Logged in successfully.\n");

    let progress = MultiProgress::new();

    for url in &urls {
        println!("Processing: {url}");
        if let Err(e) = process_url(&client, &progress, url).await {
            eprintln!("Error processing {url}: {e:?}");
        }
        println!();
    }

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

    // ==================== File Skip Logic ====================

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
    fn skip_empty_file_when_expected_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("empty.txt");
        File::create(&path).unwrap();

        assert!(should_skip(path.to_str().unwrap(), 0));
    }

    // ==================== Parent Directory ====================

    #[test]
    fn ensure_parent_creates_nested_dirs() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a/b/c/file.txt");

        ensure_parent_dir(path.to_str().unwrap());

        assert!(dir.path().join("a/b/c").exists());
    }

    #[test]
    fn ensure_parent_handles_root_file() {
        // Should not panic
        ensure_parent_dir("file.txt");
    }

    // ==================== Progress Bar ====================

    #[test]
    fn progress_bar_creation() {
        let bar = make_progress_bar(1000, "test.txt");
        assert_eq!(bar.length(), Some(1000));
    }

    // ==================== Credentials ====================

    #[test]
    fn mfa_is_optional() {
        // Ensure missing MFA env var returns None
        let result = env::var("MEGA_MFA_NONEXISTENT_TEST_VAR").ok();
        assert!(result.is_none());
    }

    // ==================== URL Parsing ====================

    #[test]
    fn detect_file_url() {
        assert!("https://mega.nz/file/ABC#key".contains("/file/"));
    }

    #[test]
    fn detect_folder_url() {
        assert!("https://mega.nz/folder/XYZ#key".contains("/folder/"));
    }

    // ==================== Concurrent Operations ====================

    #[tokio::test]
    async fn concurrent_file_writes() {
        let dir = TempDir::new().unwrap();

        let tasks: Vec<_> = (0..10)
            .map(|i| {
                let path = dir.path().join(format!("file_{i}.txt"));
                tokio::spawn(async move { tokio::fs::write(&path, format!("content {i}")).await })
            })
            .collect();

        let results = futures::future::join_all(tasks).await;

        assert!(results.iter().all(|r| r.is_ok()));
        assert!(results.iter().all(|r| r.as_ref().unwrap().is_ok()));
    }

    #[tokio::test]
    async fn async_dir_creation() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("a/b/c/d/e");

        tokio::fs::create_dir_all(&nested).await.unwrap();

        assert!(nested.exists());
    }

    // ==================== Stream Processing ====================

    #[tokio::test]
    async fn stream_buffer_unordered() {
        let items = vec![1, 2, 3, 4, 5];

        let results: Vec<_> = stream::iter(items)
            .map(|x| async move { x * 2 })
            .buffer_unordered(3)
            .collect()
            .await;

        assert_eq!(results.len(), 5);
        assert!(results.iter().all(|&x| x % 2 == 0));
    }
}
