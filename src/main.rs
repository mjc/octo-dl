use std::{env, sync::Arc, time::Duration};

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

    for file in files {
        paths.push(format!(
            "{}/{}/{}",
            nodes
                .get_node_by_hash(node.parent().unwrap())
                .unwrap()
                .name()
                .to_string(),
            node.name().to_string(),
            file.name().to_string()
        ));
    }

    for folder in folders {
        let mut child_files = get_all_paths(nodes, folder);
        paths.append(&mut child_files);
    }

    paths
}

async fn run(mega: &mut mega::Client, public_url: &str) -> mega::Result<()> {
    let nodes = mega.fetch_public_nodes(public_url).await?;

    for root in nodes.roots() {
        let paths = get_all_paths(&nodes, root);

        for path in paths {
            download_path(&nodes, path, mega).await?;
        }
    }

    Ok(())
}

async fn download_path(
    nodes: &mega::Nodes,
    path: String,
    mega: &mut mega::Client,
) -> Result<(), mega::Error> {
    let (reader, writer) = sluice::pipe::pipe();
    let node = nodes.get_node_by_path(&path).unwrap();
    let bar = ProgressBar::new(node.size());
    bar.set_style(progress_bar_style());
    bar.set_message(format!("downloading {0}...", node.name()));
    let file = File::create(node.name()).await?;
    let bar = Arc::new(bar);
    bar.set_style(progress_bar_style());
    let reader = {
        let bar = bar.clone();

        reader.report_progress(Duration::from_millis(100), move |bytes_read| {
            bar.set_position(bytes_read as u64);
        })
    };
    let handle =
        tokio::spawn(async move { futures::io::copy(reader, &mut file.compat_write()).await });
    mega.download_node(node, writer).await?;
    handle.await.unwrap()?;
    bar.finish_with_message(format!("{0} downloaded !", node.name()));
    Ok(())
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
        .unwrap()
}
