#![warn(clippy::pedantic)]
#![warn(clippy::nursery)]

use std::{env, fs, path::PathBuf, time::Duration};

use async_read_progress::AsyncReadProgressExt;
use console::style;
use env_logger;
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
        error!("Failed to fetch public nodes for URL {}: {:?}", public_url, e);
        e
    })?;

    for root in nodes.roots() {
        let paths: Vec<(String, &mega::Node)> = get_all_paths(&nodes, root)
            .iter()
            .filter_map(|(path, node)| {
                let _dir = fs::create_dir_all(PathBuf::from(&path).parent().unwrap()).ok();
                if let Ok(len) = fs::metadata(path)
                    && len.len() == node.size()
                {
                    None
                } else {
                    Some((path.clone(), *node))
                }
            })
            .collect();

        let chunks: Vec<&[(String, &mega::Node)]> = paths.chunks(20).collect();
        let m = MultiProgress::new();

        for chunk in chunks {
            let mut futures = Vec::new();

            for (path, node) in chunk {
                futures.push(download_path(&m, path.to_string(), node, mega));
            }

            let results = futures::future::join_all(futures).await;
            for result in results {
                if let Err(e) = result {
                    error!("Error downloading file: {:?}", e);
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
        error!("Failed to create directory for path {}: {:?}", path, e);
        e
    })?;
    let file = File::create(&path).await.map_err(|e| {
        error!("Failed to create file {}: {:?}", path, e);
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
            error!("Failed to copy data to file {}: {:?}", path, e);
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
        println!("Login attempt failed: {:?}", e);
        e
    })?;

    println!("Login successful. Processing public URLs...");
    for public_url in args.as_slice() {
        println!("Processing URL: {}", public_url);
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
