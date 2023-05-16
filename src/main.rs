use std::{sync::Arc, time::Duration};

use async_read_progress::AsyncReadProgressExt;
use console::style;
use indicatif::{ProgressBar, ProgressStyle};
use tokio::fs::File;
use tokio_util::compat::TokioAsyncWriteCompatExt;

fn get_all_paths(nodes: &mega::Nodes, node: &mega::Node) -> Vec<String> {
    let mut paths = vec![];
    let (mut folders, mut files): (Vec<_>, Vec<_>) = node
        .children()
        .iter()
        .filter_map(|hash| nodes.get_node_by_hash(hash))
        .partition(|node| node.kind().is_folder());

    folders.sort_unstable_by_key(|node| node.name());
    files.sort_unstable_by_key(|node| node.name());

    let mut file_paths = files
        .iter()
        .filter_map(|file| build_path(node, nodes, file))
        .collect();

    let mut child_file_paths: Vec<String> = folders
        .iter()
        .flat_map(|folder| get_all_paths(nodes, folder))
        .collect();

    paths.append(&mut file_paths);
    paths.append(&mut child_file_paths);

    paths
}

fn build_path(node: &mega::Node, nodes: &mega::Nodes, file: &&mega::Node) -> Option<String> {
    let parent_node = nodes.get_node_by_hash(node.parent()?)?;
    let parent_name = parent_node.name();
    Some(format!("{}/{}/{}", parent_name, node.name(), file.name()))
}

async fn run(mega: &mut mega::Client, public_url: &str) -> mega::Result<()> {
    let nodes = mega.fetch_public_nodes(public_url).await?;

    for root in nodes.roots() {
        let paths = get_all_paths(&nodes, root);

        for path in paths {
            let node = nodes
                .get_node_by_path(&path)
                .ok_or(mega::Error::NodeNotFound)?;
            download_path(node, mega).await?;
        }
    }

    Ok(())
}

async fn download_path(node: &mega::Node, mega: &mut mega::Client) -> Result<(), mega::Error> {
    let file = File::create(node.name()).await?;
    let (reader, writer) = sluice::pipe::pipe();

    let bar = progress_bar(node);

    let reader = {
        let bar = bar.clone();

        reader.report_progress(Duration::from_millis(100), move |bytes_read| {
            bar.set_position(bytes_read as u64);
        })
    };

    let handle =
        tokio::spawn(async move { futures::io::copy(reader, &mut file.compat_write()).await });
    mega.download_node(node, writer).await?;
    handle.await.expect("download failed")?;
    bar.finish_with_message(format!("{0} downloaded !", node.name()));
    Ok(())
}

fn progress_bar(node: &mega::Node) -> Arc<ProgressBar> {
    let bar = ProgressBar::new(node.size());
    bar.set_style(progress_bar_style());
    bar.set_message(format!("downloading {0}...", node.name()));
    let bar = Arc::new(bar);
    bar.set_style(progress_bar_style());
    bar
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> mega::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let public_url = match args.as_slice() {
        [public_url] => public_url.as_str(),
        _ => {
            panic!("expected 1 command-line argument: {{public_url}}");
        }
    };

    let http_client = reqwest::Client::new();
    let mut mega = mega::Client::builder().build(http_client)?;

    run(&mut mega, public_url).await
}

pub fn progress_bar_style() -> ProgressStyle {
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
