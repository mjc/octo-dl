use super::package_identity::single_file_package_path;
/// A file to be downloaded with its destination path.
pub struct DownloadItem<'a> {
    /// Local file path where the file will be saved.
    pub path: String,
    /// Reference to the MEGA node to download.
    pub node: &'a mega::Node,
    /// Whether this item already has a partial local `.part` file.
    pub was_partial: bool,
}

/// Result of collecting files from nodes.
pub struct CollectedFiles<'a> {
    /// Files that need to be downloaded.
    pub to_download: Vec<DownloadItem<'a>>,
    /// Files already complete on disk.
    pub completed: Vec<DownloadItem<'a>>,
    /// Number of files skipped (already exist with correct size).
    pub skipped: usize,
    /// Number of files with partial `.part` downloads detected.
    pub partial: usize,
}

impl CollectedFiles<'_> {
    /// Returns the total size of files to download in bytes.
    #[must_use]
    pub fn total_size(&self) -> u64 {
        self.to_download.iter().map(|i| i.node.size()).sum()
    }

    /// Returns true if there are no files to download.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.to_download.is_empty()
    }

    /// Converts borrowed download items into owned items by cloning the nodes.
    ///
    /// This is useful when the items need to be sent to a `tokio::spawn`'d task,
    /// which requires `'static` data.
    #[must_use]
    pub fn into_owned(self) -> Vec<OwnedDownloadItem> {
        self.to_download
            .into_iter()
            .map(|item| OwnedDownloadItem {
                path: item.path,
                node: item.node.clone(),
                was_partial: item.was_partial,
            })
            .collect()
    }

    /// Converts both download and already-complete items into owned items.
    #[must_use]
    pub fn into_owned_parts(self) -> (Vec<OwnedDownloadItem>, Vec<OwnedDownloadItem>) {
        let to_download = self
            .to_download
            .into_iter()
            .map(|item| OwnedDownloadItem {
                path: item.path,
                node: item.node.clone(),
                was_partial: item.was_partial,
            })
            .collect();
        let completed = self
            .completed
            .into_iter()
            .map(|item| OwnedDownloadItem {
                path: item.path,
                node: item.node.clone(),
                was_partial: item.was_partial,
            })
            .collect();
        (to_download, completed)
    }
}

/// A file to be downloaded with an owned node (no lifetime parameter).
///
/// Use this instead of [`DownloadItem`] when the items need to cross
/// `tokio::spawn` boundaries (which require `'static` data).
#[derive(Clone)]
pub struct OwnedDownloadItem {
    /// Local file path where the file will be saved.
    pub path: String,
    /// Owned copy of the MEGA node to download.
    pub node: mega::Node,
    /// Whether this item already has a partial local `.part` file.
    pub was_partial: bool,
}

pub(crate) fn collect_download_items<'a>(nodes: &'a mega::Nodes) -> Vec<DownloadItem<'a>> {
    let roots = nodes.roots().collect::<Vec<_>>();
    let single_root_file = roots.len() == 1 && roots[0].kind().is_file();
    roots
        .into_iter()
        .flat_map(|root| {
            if root.kind().is_folder() {
                collect_files_recursive(nodes, root)
            } else {
                let path = if single_root_file {
                    single_file_package_path(root.name())
                } else {
                    root.name().to_string()
                };
                vec![DownloadItem {
                    path,
                    node: root,
                    was_partial: false,
                }]
            }
        })
        .collect()
}

/// Recursively collects files from a folder node.
fn collect_files_recursive<'a>(
    nodes: &'a mega::Nodes,
    node: &'a mega::Node,
) -> Vec<DownloadItem<'a>> {
    let (folders, files): (Vec<_>, Vec<_>) = node
        .children()
        .iter()
        .filter_map(|hash| nodes.get_node_by_handle(hash))
        .partition(|n| n.kind().is_folder());

    let current_files = files.into_iter().map(|file| DownloadItem {
        path: build_path(nodes, file),
        node: file,
        was_partial: false,
    });

    let nested_files = folders
        .into_iter()
        .flat_map(|folder| collect_files_recursive(nodes, folder));

    current_files.chain(nested_files).collect()
}

/// Builds the full path for a file within a folder structure.
fn build_path(nodes: &mega::Nodes, file: &mega::Node) -> String {
    build_relative_path(file.name(), file.parent(), |handle| {
        let node = nodes.get_node_by_handle(handle)?;
        Some((node.name().to_string(), node.parent().map(str::to_string)))
    })
}

fn build_relative_path<F>(file_name: &str, parent: Option<&str>, mut lookup: F) -> String
where
    F: FnMut(&str) -> Option<(String, Option<String>)>,
{
    let mut components = vec![file_name.to_string()];
    let mut parent = parent.map(str::to_string);

    while let Some(handle) = parent.as_deref() {
        let Some((name, next_parent)) = lookup(handle) else {
            break;
        };
        if next_parent.is_none() {
            break;
        }
        components.push(name);
        parent = next_parent;
    }

    components.reverse();
    components.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collected_files_total_size() {
        let collected = CollectedFiles {
            to_download: vec![],
            completed: vec![],
            skipped: 5,
            partial: 0,
        };
        assert_eq!(collected.total_size(), 0);
        assert!(collected.is_empty());
    }

    #[test]
    fn build_relative_path_handles_deep_nesting() {
        let mut parents = std::collections::HashMap::new();
        parents.insert(
            "folder-b".to_string(),
            ("b".to_string(), Some("folder-a".to_string())),
        );
        parents.insert(
            "folder-a".to_string(),
            ("a".to_string(), Some("root".to_string())),
        );
        parents.insert("root".to_string(), ("ignored-root".to_string(), None));

        let path = build_relative_path("file.bin", Some("folder-b"), |handle| {
            parents.get(handle).cloned()
        });

        assert_eq!(path, "a/b/file.bin");
    }
}
