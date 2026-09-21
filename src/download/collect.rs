use std::sync::Arc;

use crate::fs::FileSystem;

use super::callbacks::DownloadProgress;
use super::downloader::Downloader;
use super::inspect::FileStatus;
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
                    single_file_package_path(&safe_path_component(root.name()))
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

pub(super) async fn collect_files_with_downloader<'a, F: FileSystem>(
    downloader: &Downloader<F>,
    nodes: &'a mega::Nodes,
    progress: &Arc<dyn DownloadProgress>,
) -> CollectedFiles<'a> {
    let all_items = collect_download_items(nodes);

    let mut to_download = Vec::new();
    let mut completed = Vec::new();
    let mut skipped = 0;
    let mut partial = 0;

    for item in all_items {
        let local = downloader
            .inspect_local_file(&item.path, item.node.size())
            .await;
        match local.status {
            FileStatus::Complete => {
                match downloader
                    .complete_existing_file(item.node, &item.path, progress)
                    .await
                {
                    Ok(Some(_)) => {
                        skipped += 1;
                        completed.push(item);
                    }
                    Ok(None) => to_download.push(item),
                    Err(error) => {
                        log::warn!(
                            "Existing completed file {} could not be verified; redownloading: {error}",
                            item.path
                        );
                        to_download.push(item);
                    }
                }
            }
            FileStatus::Partial => {
                progress.on_partial_detected(
                    &item.path,
                    local.existing_partial_bytes,
                    item.node.size(),
                );
                partial += 1;
                to_download.push(DownloadItem {
                    was_partial: true,
                    ..item
                });
            }
            FileStatus::Missing => {
                to_download.push(item);
            }
        }
    }

    CollectedFiles {
        to_download,
        completed,
        skipped,
        partial,
    }
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
    let mut components = vec![safe_path_component(file_name)];
    let mut parent = parent.map(str::to_string);

    while let Some(handle) = parent.as_deref() {
        let Some((name, next_parent)) = lookup(handle) else {
            break;
        };
        if next_parent.is_none() {
            break;
        }
        components.push(safe_path_component(&name));
        parent = next_parent;
    }

    components.reverse();
    components.join("/")
}

fn safe_path_component(component: &str) -> String {
    let sanitized = component.replace(['/', '\\'], "_");
    if sanitized.is_empty() || sanitized == "." || sanitized == ".." {
        "_".to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use super::super::callbacks::NoProgress;
    use super::super::sidecar::part_path;
    use super::super::test_support::*;
    use super::*;
    use crate::fake_mega::{FakeMegaFixture, FakeMegaServer, create_fake_mega_fixture};
    use crate::test_support::CurrentDirGuard;

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

    #[test]
    fn build_relative_path_sanitizes_traversal_components() {
        let path = build_relative_path("../outside", Some("folder"), |handle| {
            Some((handle.to_string(), None))
        });

        assert!(!path.split('/').any(|component| component == ".."));
        assert!(!path.starts_with('/'));
    }

    #[test]
    fn single_root_file_path_sanitizes_the_filename_before_wrapping_it() {
        let path = single_file_package_path(&safe_path_component("../outside\\file.bin"));

        assert_eq!(path, ".._outside_file/.._outside_file.bin");
        assert!(!path.starts_with('/'));
        assert!(
            !path
                .split('/')
                .any(|component| component == "." || component == "..")
        );
    }

    #[derive(Default)]
    struct PartialRecordingProgress {
        detected: Mutex<Vec<(String, u64, u64)>>,
    }

    impl DownloadProgress for PartialRecordingProgress {
        fn on_partial_detected(&self, name: &str, existing_size: u64, expected_size: u64) {
            self.detected
                .lock()
                .unwrap()
                .push((name.to_string(), existing_size, expected_size));
        }
    }

    async fn single_file_nodes(
        seed: u64,
    ) -> (
        tempfile::TempDir,
        FakeMegaFixture,
        FakeMegaServer,
        mega::Nodes,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let fixture_dir = temp.path().join("fixture");
        let fixture = create_fake_mega_fixture(&fixture_dir, "payload.bin", 262_219, seed)
            .await
            .unwrap();
        let server = FakeMegaServer::spawn(fixture.clone(), 1).unwrap();
        let client = mega::Client::builder()
            .origin(server.origin().clone())
            .build(mega::http_client_builder().unwrap().build().unwrap())
            .unwrap();
        let nodes = client
            .fetch_public_nodes(&fixture.public_url())
            .await
            .unwrap();
        (temp, fixture, server, nodes)
    }

    #[tokio::test]
    async fn collect_files_marks_existing_output_complete() {
        let (temp, fixture, server, nodes) = single_file_nodes(51).await;
        let _cwd = CurrentDirGuard::set(temp.path());
        let items = collect_download_items(&nodes);
        let path = items[0].path.clone();
        let size = items[0].node.size();
        let output_path = Path::new(&path);
        tokio::fs::create_dir_all(output_path.parent().unwrap())
            .await
            .unwrap();
        let mut bytes = vec![0u8; usize::try_from(size).unwrap()];
        fixture.fill_plaintext(0, &mut bytes);
        tokio::fs::write(output_path, bytes.clone()).await.unwrap();
        let fs = MockFileSystem::new();
        fs.add_bytes(&path, bytes);
        let downloader = mock_downloader(fs);
        let progress: Arc<dyn DownloadProgress> = Arc::new(NoProgress);

        let collected = collect_files_with_downloader(&downloader, &nodes, &progress).await;

        assert_eq!(collected.skipped, 1);
        assert_eq!(collected.partial, 0);
        assert!(collected.to_download.is_empty());
        assert_eq!(collected.completed.len(), 1);
        assert_eq!(collected.completed[0].path, path);

        tokio::fs::remove_dir_all(output_path.parent().unwrap())
            .await
            .unwrap();
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn collect_files_redownloads_same_size_corrupt_output() {
        let (temp, fixture, server, nodes) = single_file_nodes(57).await;
        let _cwd = CurrentDirGuard::set(temp.path());
        let items = collect_download_items(&nodes);
        let path = items[0].path.clone();
        let size = items[0].node.size();
        let output_path = Path::new(&path);
        tokio::fs::create_dir_all(output_path.parent().unwrap())
            .await
            .unwrap();
        let mut bytes = vec![0u8; usize::try_from(size).unwrap()];
        fixture.fill_plaintext(0, &mut bytes);
        bytes[0] ^= 0xff;
        tokio::fs::write(output_path, bytes.clone()).await.unwrap();
        let fs = MockFileSystem::new();
        fs.add_bytes(&path, bytes);
        let downloader = mock_downloader(fs);
        let progress: Arc<dyn DownloadProgress> = Arc::new(NoProgress);

        let collected = collect_files_with_downloader(&downloader, &nodes, &progress).await;

        assert_eq!(collected.skipped, 0);
        assert_eq!(collected.partial, 0);
        assert_eq!(collected.to_download.len(), 1);
        assert!(collected.completed.is_empty());

        tokio::fs::remove_dir_all(output_path.parent().unwrap())
            .await
            .unwrap();
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn collect_files_marks_existing_partials_and_reports_detected_bytes() {
        let (_temp, _fixture, server, nodes) = single_file_nodes(53).await;
        let items = collect_download_items(&nodes);
        let path = items[0].path.clone();
        let size = items[0].node.size();
        let partial_bytes = size / 3;
        let fs = MockFileSystem::new();
        fs.add_file(part_path(&path), partial_bytes);
        let downloader = mock_downloader(fs);
        let progress_impl = Arc::new(PartialRecordingProgress::default());
        let progress: Arc<dyn DownloadProgress> = progress_impl.clone();

        let collected = collect_files_with_downloader(&downloader, &nodes, &progress).await;

        assert_eq!(collected.skipped, 0);
        assert_eq!(collected.partial, 1);
        assert_eq!(collected.to_download.len(), 1);
        assert!(collected.to_download[0].was_partial);
        assert_eq!(collected.to_download[0].path, path);
        assert!(collected.completed.is_empty());
        assert_eq!(
            progress_impl.detected.lock().unwrap().as_slice(),
            &[(path, partial_bytes, size)]
        );

        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn collect_files_force_overwrite_treats_existing_output_as_missing() {
        let (_temp, _fixture, server, nodes) = single_file_nodes(59).await;
        let items = collect_download_items(&nodes);
        let path = items[0].path.clone();
        let size = items[0].node.size();
        let fs = MockFileSystem::new();
        fs.add_file(&path, size);
        let downloader = mock_downloader_force(fs);
        let progress: Arc<dyn DownloadProgress> = Arc::new(NoProgress);

        let collected = collect_files_with_downloader(&downloader, &nodes, &progress).await;

        assert_eq!(collected.skipped, 0);
        assert_eq!(collected.partial, 0);
        assert_eq!(collected.to_download.len(), 1);
        assert!(!collected.to_download[0].was_partial);
        assert_eq!(collected.to_download[0].path, path);
        assert!(collected.completed.is_empty());

        server.shutdown().await.unwrap();
    }
}
