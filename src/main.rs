fn get_all_files(nodes: &mega::Nodes, node: &mega::Node) -> Vec<String> {
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
            "{}/{}",
            node.name().to_string(),
            file.name().to_string()
        ));
    }

    for folder in folders {
        let mut child_files = get_all_files(nodes, folder);
        paths.append(&mut child_files);
    }

    paths
}

async fn run(mega: &mut mega::Client, public_url: &str) -> mega::Result<()> {
    let nodes = mega.fetch_public_nodes(public_url).await?;

    println!();
    for root in nodes.roots() {
        let files = get_all_files(&nodes, root);
        println!("{:?}", files);
    }

    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let public_url = match args.as_slice() {
        [public_url] => public_url.as_str(),
        _ => {
            panic!("expected 1 command-line argument: {{public_url}}");
        }
    };

    let http_client = reqwest::Client::new();
    let mut mega = mega::Client::builder().build(http_client).unwrap();

    run(&mut mega, public_url).await.unwrap();
}
