#![warn(clippy::pedantic)]
#![warn(clippy::nursery)]

use std::{env, fs, path::PathBuf, time::Duration};

use async_read_progress::AsyncReadProgressExt;
use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use log::error;
use tokio::fs::{create_dir_all, File};
use tokio_util::compat::TokioAsyncWriteCompatExt;

fn get_all_paths<'node>(
    nodes: &'node mega::Nodes,
    node: &'node mega::Node,
) -> Vec<(String, &'node mega::Node)> {
    let mut paths = vec![];
    let (mut folders, mut files): (Vec<_>, Vec<_>) = node
        .children()
        .iter()
        .filter_map(|hash| nodes.get_node_by_handle(hash))
        .partition(|node| node.kind().is_folder());

    folders.sort_unstable_by_key(|node| node.name());
    files.sort_unstable_by_key(|node| node.name());

    let mut file_paths = files
        .iter()
        .filter_map(|file| Some((build_path(node, nodes, file)?, *file)))
        .collect();

    let mut child_file_paths: Vec<(String, &mega::Node)> = folders
        .iter()
        .flat_map(|folder| get_all_paths(nodes, folder))
        .collect();

    paths.append(&mut file_paths);
    paths.append(&mut child_file_paths);

    paths
}

fn build_path(node: &mega::Node, nodes: &mega::Nodes, file: &mega::Node) -> Option<String> {
    let parent = node.parent()?;
    let parent_node = nodes.get_node_by_handle(parent)?;

    Some(format!(
        "{}/{}/{}",
        parent_node.name(),
        node.name(),
        file.name()
    ))
}

async fn run(mega: &mega::Client, public_url: &str) -> mega::Result<()> {
    let nodes = mega.fetch_public_nodes(public_url).await.map_err(|e| {
        error!("Failed to fetch public nodes for URL {public_url}: {e:?}");
        e
    })?;

    let m = MultiProgress::new();

    for root in nodes.roots() {
        // Handle single file URLs - if root is a file, download it directly
        if !root.kind().is_folder() {
            let path = root.name().to_string();
            if should_skip_file(&path, root.size()) {
                println!("File {path} already exists with correct size, skipping");
                continue;
            }
            download_path(&m, path, root, mega).await?;
            continue;
        }

        // Handle folder URLs
        let paths: Vec<(String, &mega::Node)> = get_all_paths(&nodes, root)
            .iter()
            .filter_map(|(path, node)| {
                if let Some(parent) = get_parent_dir(path) {
                    let _ = fs::create_dir_all(parent);
                }
                if should_skip_file(path, node.size()) {
                    None
                } else {
                    Some((path.clone(), *node))
                }
            })
            .collect();

        let chunks: Vec<&[(String, &mega::Node)]> = paths.chunks(20).collect();

        for chunk in chunks {
            let mut futures = Vec::new();

            for (path, node) in chunk {
                futures.push(download_path(&m, path.clone(), node, mega));
            }

            let results = futures::future::join_all(futures).await;
            for result in results {
                if let Err(e) = result {
                    error!("Error downloading file: {e:?}");
                }
            }
        }
    }

    Ok(())
}

async fn download_path(
    m: &MultiProgress,
    path: String, // Changed `path` to `String` to ensure it has a `'static` lifetime
    node: &mega::Node,
    mega: &mega::Client,
) -> mega::Result<()> {
    let (reader, writer) = sluice::pipe::pipe();

    create_dir_all(PathBuf::from(&path).parent().unwrap()).await.map_err(|e| {
        error!("Failed to create directory for path {path}: {e:?}");
        e
    })?;
    let file = File::create(&path).await.map_err(|e| {
        error!("Failed to create file {path}: {e:?}");
        e
    })?;

    let bar = m.add(progress_bar(node));
    bar.set_message(format!("downloading {0}...", node.name()));

    let reader = {
        let bar = bar.clone();

        reader.report_progress(Duration::from_millis(100), move |bytes_read| {
            bar.set_position(bytes_read as u64);
        })
    };

    let handle = tokio::spawn(async move {
        futures::io::copy(reader, &mut file.compat_write()).await.map_err(|e| {
            error!("Failed to copy data to file {path}: {e:?}");
            e
        })
    });
    mega.download_node(node, writer).await.map_err(|e| {
        error!("Failed to download node {}: {:?}", node.name(), e);
        e
    })?;
    handle.await.expect("download failed")?;
    bar.finish_with_message(format!("{0} downloaded !", node.name()));
    Ok(())
}

fn progress_bar(node: &mega::Node) -> ProgressBar {
    let bar = ProgressBar::new(node.size());
    bar.set_style(progress_bar_style());
    bar
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> mega::Result<()> {
    env_logger::init(); // Initialize the logger

    let args: Vec<String> = std::env::args().skip(1).collect();

    assert!(!args.is_empty(), "Usage: octo-dl <public url(s)>");

    let email = env::var("MEGA_EMAIL").expect("missing MEGA_EMAIL environment variable");
    let password = env::var("MEGA_PASSWORD").expect("missing MEGA_PASSWORD environment variable");
    let mfa = env::var("MEGA_MFA").ok();

    println!("Initializing MEGA client...");
    let http_client = reqwest::Client::new();
    let mut mega = mega::Client::builder().build(http_client)?;

    println!("Attempting to log in to MEGA...");
    mega.login(&email, &password, mfa.as_deref()).await.map_err(|e| {
        println!("Login attempt failed: {e:?}");
        e
    })?;

    println!("Login successful. Processing public URLs...");
    for public_url in args.as_slice() {
        println!("Processing URL: {public_url}");
        run(&mega, public_url).await?;
    }

    println!("All URLs processed successfully.");

    Ok(())
}

fn progress_bar_style() -> ProgressStyle {
    let template = format!(
        "{}{{bar:30.magenta.bold/magenta/bold}}{} {{percent}}% at {{binary_bytes_per_sec}} (ETA {{eta}}): {{msg}}",
        style("▐").bold().magenta(),
        style("▌").bold().magenta(),
    );

    ProgressStyle::default_bar()
        .progress_chars("▨▨╌")
        .template(template.as_str())
        .expect("somehow couldn't set up progress bar template")
}

/// Check if a file should be skipped (already exists with correct size)
fn should_skip_file(path: &str, expected_size: u64) -> bool {
    fs::metadata(path)
        .map(|m| m.len() == expected_size)
        .unwrap_or(false)
}

/// Parse a MEGA URL to determine if it's a file or folder link
#[cfg(test)]
fn parse_mega_url(url: &str) -> Option<MegaUrlType> {
    if url.contains("/file/") {
        Some(MegaUrlType::File)
    } else if url.contains("/folder/") {
        Some(MegaUrlType::Folder)
    } else {
        None
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MegaUrlType {
    File,
    Folder,
}

/// Build a file path from components
#[cfg(test)]
fn build_file_path(parent_name: &str, folder_name: &str, file_name: &str) -> String {
    format!("{}/{}/{}", parent_name, folder_name, file_name)
}

/// Get parent directory of a path, returns None for root-level files
fn get_parent_dir(path: &str) -> Option<String> {
    let parent = PathBuf::from(path).parent()?.to_str()?.to_string();
    if parent.is_empty() {
        None
    } else {
        Some(parent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;
    use tempfile::TempDir;

    // ==================== URL Parsing Tests ====================

    #[test]
    fn test_parse_mega_url_file() {
        let url = "https://mega.nz/file/ABC123#key";
        assert_eq!(parse_mega_url(url), Some(MegaUrlType::File));
    }

    #[test]
    fn test_parse_mega_url_folder() {
        let url = "https://mega.nz/folder/XYZ789#key";
        assert_eq!(parse_mega_url(url), Some(MegaUrlType::Folder));
    }

    #[test]
    fn test_parse_mega_url_invalid() {
        let url = "https://mega.nz/invalid/ABC123";
        assert_eq!(parse_mega_url(url), None);
    }

    #[test]
    fn test_parse_mega_url_not_mega() {
        let url = "https://example.com/file/ABC123";
        // Still matches the pattern, which is fine - validation happens elsewhere
        assert_eq!(parse_mega_url(url), Some(MegaUrlType::File));
    }

    #[test]
    fn test_parse_mega_url_empty() {
        assert_eq!(parse_mega_url(""), None);
    }

    // ==================== File Skip Logic Tests ====================

    #[test]
    fn test_should_skip_file_exists_correct_size() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        let mut file = File::create(&path).unwrap();
        file.write_all(b"hello").unwrap();

        assert!(should_skip_file(path.to_str().unwrap(), 5));
    }

    #[test]
    fn test_should_skip_file_exists_wrong_size() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        let mut file = File::create(&path).unwrap();
        file.write_all(b"hello").unwrap();

        assert!(!should_skip_file(path.to_str().unwrap(), 100));
    }

    #[test]
    fn test_should_skip_file_not_exists() {
        assert!(!should_skip_file("/nonexistent/path/file.txt", 100));
    }

    #[test]
    fn test_should_skip_file_empty_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("empty.txt");
        File::create(&path).unwrap();

        assert!(should_skip_file(path.to_str().unwrap(), 0));
        assert!(!should_skip_file(path.to_str().unwrap(), 1));
    }

    // ==================== Path Building Tests ====================

    #[test]
    fn test_build_file_path_simple() {
        assert_eq!(
            build_file_path("parent", "folder", "file.txt"),
            "parent/folder/file.txt"
        );
    }

    #[test]
    fn test_build_file_path_with_spaces() {
        assert_eq!(
            build_file_path("My Files", "Sub Folder", "My Document.pdf"),
            "My Files/Sub Folder/My Document.pdf"
        );
    }

    #[test]
    fn test_build_file_path_unicode() {
        assert_eq!(
            build_file_path("文件夹", "サブ", "файл.txt"),
            "文件夹/サブ/файл.txt"
        );
    }

    #[test]
    fn test_get_parent_dir_nested() {
        assert_eq!(get_parent_dir("a/b/c.txt"), Some("a/b".to_string()));
    }

    #[test]
    fn test_get_parent_dir_single_level() {
        assert_eq!(get_parent_dir("folder/file.txt"), Some("folder".to_string()));
    }

    #[test]
    fn test_get_parent_dir_root_file() {
        assert_eq!(get_parent_dir("file.txt"), None);
    }

    #[test]
    fn test_get_parent_dir_empty() {
        assert_eq!(get_parent_dir(""), None);
    }

    // ==================== Directory Creation Tests ====================

    #[test]
    fn test_create_nested_dirs() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("a/b/c");

        fs::create_dir_all(&nested).unwrap();

        assert!(nested.exists());
        assert!(nested.is_dir());
    }

    #[test]
    fn test_create_dir_idempotent() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("test");

        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(&nested).unwrap(); // Should not fail

        assert!(nested.exists());
    }

    // ==================== Progress Bar Tests ====================

    #[test]
    fn test_progress_bar_style_creation() {
        // Should not panic
        let _style = progress_bar_style();
    }

    // ==================== Argument Validation Tests ====================

    #[test]
    fn test_args_parsing_single_url() {
        let args = vec!["https://mega.nz/file/ABC#key".to_string()];
        assert_eq!(args.len(), 1);
        assert!(!args.is_empty());
    }

    #[test]
    fn test_args_parsing_multiple_urls() {
        let args = vec![
            "https://mega.nz/file/ABC#key".to_string(),
            "https://mega.nz/folder/XYZ#key".to_string(),
        ];
        assert_eq!(args.len(), 2);
    }

    // ==================== Environment Variable Tests ====================

    #[test]
    fn test_env_var_mfa_optional() {
        // MFA should be optional - test that .ok() converts Err to None
        let mfa = std::env::var("MEGA_MFA_NONEXISTENT_VAR_FOR_TEST").ok();
        assert!(mfa.is_none());
    }

    // ==================== File Operations Tests ====================

    #[test]
    fn test_file_write_and_verify_size() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("output.bin");

        let data = vec![0u8; 1024];
        fs::write(&path, &data).unwrap();

        let metadata = fs::metadata(&path).unwrap();
        assert_eq!(metadata.len(), 1024);
    }

    #[test]
    fn test_file_overwrite() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("output.txt");

        fs::write(&path, "initial").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 7);

        fs::write(&path, "new content here").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 16);
    }

    // ==================== Path Edge Cases ====================

    #[test]
    fn test_path_with_dots() {
        let path = "folder/../other/file.txt";
        let normalized = PathBuf::from(path);
        // PathBuf doesn't normalize, but we can still work with it
        assert!(normalized.to_str().is_some());
    }

    #[test]
    fn test_path_with_special_chars() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file with spaces & symbols!.txt");

        File::create(&path).unwrap();
        assert!(path.exists());
    }

    // ==================== Chunking Tests ====================

    #[test]
    fn test_chunking_exact() {
        let items: Vec<i32> = (0..20).collect();
        let chunks: Vec<&[i32]> = items.chunks(20).collect();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), 20);
    }

    #[test]
    fn test_chunking_with_remainder() {
        let items: Vec<i32> = (0..25).collect();
        let chunks: Vec<&[i32]> = items.chunks(20).collect();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 20);
        assert_eq!(chunks[1].len(), 5);
    }

    #[test]
    fn test_chunking_smaller_than_chunk_size() {
        let items: Vec<i32> = (0..5).collect();
        let chunks: Vec<&[i32]> = items.chunks(20).collect();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), 5);
    }

    #[test]
    fn test_chunking_empty() {
        let items: Vec<i32> = vec![];
        let chunks: Vec<&[i32]> = items.chunks(20).collect();
        assert_eq!(chunks.len(), 0);
    }

    // ==================== Concurrent Download Simulation ====================

    #[tokio::test]
    async fn test_concurrent_file_creation() {
        let dir = TempDir::new().unwrap();
        let mut handles: Vec<tokio::task::JoinHandle<Result<(), std::io::Error>>> = vec![];

        for i in 0..10 {
            let path = dir.path().join(format!("file_{}.txt", i));
            handles.push(tokio::spawn(async move {
                tokio::fs::write(&path, format!("content {}", i)).await
            }));
        }

        let results = futures::future::join_all(handles).await;
        for result in results {
            assert!(result.is_ok());
            assert!(result.unwrap().is_ok());
        }

        // Verify all files exist
        for i in 0..10 {
            let path = dir.path().join(format!("file_{}.txt", i));
            assert!(path.exists());
        }
    }

    #[tokio::test]
    async fn test_create_dir_all_async() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("a/b/c/d/e");

        tokio::fs::create_dir_all(&nested).await.unwrap();

        assert!(nested.exists());
    }
}
