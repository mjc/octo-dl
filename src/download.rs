//! Core download logic and abstractions.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::DownloadConfig;
use crate::core::{PackageId, PackageKey, ProgressDelta};
use crate::error::{Error, Result};
use crate::fs::{FileSystem, TokioFileSystem};
use crate::progress::CumulativeProgress;
use crate::stats::{DownloadStatsTracker, FileStats, SessionStats, SessionStatsBuilder};

/// Fetches public-link metadata with a fresh anonymous MEGA client.
///
/// Public-link browsing should not depend on the caller's authenticated client
/// state. Using a fresh client avoids cross-talk between account session state
/// and public-link metadata fetches.
///
/// # Errors
///
/// Returns an error if the MEGA client cannot be created or the public link
/// metadata fetch fails.
pub async fn fetch_public_nodes(http: &reqwest::Client, url: &str) -> Result<mega::Nodes> {
    let client = mega::Client::builder().build(http.clone())?;
    client.fetch_public_nodes(url).await.map_err(Error::Mega)
}

/// Classification of a file's current state on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    /// File exists with the expected size — fully downloaded.
    Complete,
    /// A `.part` file exists (partial download from a previous run).
    Partial,
    /// Neither the final file nor a `.part` file exists.
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ObservedLocalFile {
    pub final_size: Option<u64>,
    pub part_size: Option<u64>,
    pub has_sidecar: bool,
    pub verified_resume_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InspectedLocalFile {
    pub status: FileStatus,
    pub existing_partial_bytes: u64,
    pub has_resume_sidecar: bool,
    pub verified_resume_bytes: u64,
}

impl Default for InspectedLocalFile {
    fn default() -> Self {
        Self {
            status: FileStatus::Missing,
            existing_partial_bytes: 0,
            has_resume_sidecar: false,
            verified_resume_bytes: 0,
        }
    }
}

pub(crate) fn classify_observed_local_file(
    observed: ObservedLocalFile,
    expected_size: u64,
    force_overwrite: bool,
) -> InspectedLocalFile {
    if force_overwrite {
        return InspectedLocalFile {
            status: FileStatus::Missing,
            ..InspectedLocalFile::default()
        };
    }

    if observed.final_size == Some(expected_size) {
        return InspectedLocalFile {
            status: FileStatus::Complete,
            ..InspectedLocalFile::default()
        };
    }

    if let Some(part_size) = observed.part_size {
        return InspectedLocalFile {
            status: FileStatus::Partial,
            existing_partial_bytes: part_size,
            has_resume_sidecar: observed.has_sidecar,
            verified_resume_bytes: observed.verified_resume_bytes.min(expected_size),
        };
    }

    InspectedLocalFile {
        status: FileStatus::Missing,
        ..InspectedLocalFile::default()
    }
}

/// Trait for receiving download progress updates.
///
/// Implement this trait to receive callbacks during download operations.
/// All methods have default no-op implementations for convenience.
pub trait DownloadProgress: Send + Sync {
    /// Called when a file download starts.
    fn on_file_start(&self, _name: &str, _size: u64) {}

    /// Called periodically with the number of bytes advanced since the last call.
    ///
    /// `total_bytes_delta` includes any locally reused bytes revealed by
    /// resume revalidation, while `network_bytes_delta` counts only fresh
    /// bytes received from the network during this callback interval.
    fn on_progress(&self, _name: &str, _delta: ProgressDelta) {}

    /// Called when a file download completes successfully.
    fn on_file_complete(&self, _name: &str, _stats: &FileStats) {}

    /// Called when a file download fails.
    fn on_error(&self, _name: &str, _error: &str) {}

    /// Called when a partial `.part` file is detected from a previous run.
    fn on_partial_detected(&self, _name: &str, _existing_size: u64, _expected_size: u64) {}

    /// Called when previously verified chunks will be reused.
    fn on_resume_reused(&self, _name: &str, _chunks: usize, _bytes: u64) {}
}

/// A null progress implementation that ignores all events.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoProgress;

impl DownloadProgress for NoProgress {}

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

/// Bytes and chunks reused from resumable state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeReuse {
    pub chunks: usize,
    pub bytes: u64,
    pub source: ResumeReuseSource,
}

/// Source of reused chunk state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeReuseSource {
    Sidecar,
}

const CURRENT_RESUME_SIDECAR_VERSION: u32 = 2;

/// Returns the `.part` file path for a given final path.
pub(crate) fn part_path(path: &str) -> PathBuf {
    PathBuf::from(format!("{path}.part"))
}

pub(crate) fn sidecar_path(path: &str) -> PathBuf {
    PathBuf::from(format!("{path}.part.meta.json"))
}

pub(crate) fn resume_sidecar_verified_bytes(path: &str) -> Option<u64> {
    let sidecar = std::fs::read(sidecar_path(path)).ok()?;
    let sidecar: ResumeSidecar = serde_json::from_slice(&sidecar).ok()?;
    if sidecar.version != CURRENT_RESUME_SIDECAR_VERSION {
        return Some(0);
    }
    let boundaries = mega::mega_chunk_boundaries(sidecar.file_size);
    Some(
        sidecar
            .verified_chunks
            .iter()
            .filter_map(|record| boundaries.get(record.index as usize))
            .fold(0u64, |sum, chunk| sum.saturating_add(chunk.length)),
    )
}

/// Deletes resumable download artifacts for a final output path.
pub async fn delete_resume_artifacts(path: &str) -> io::Result<()> {
    remove_file_if_exists(&part_path(path)).await?;
    remove_file_if_exists(&sidecar_path(path)).await
}

/// Deletes the final output and resumable download artifacts for a path.
pub async fn delete_download_artifacts(path: &str) -> io::Result<()> {
    remove_file_if_exists(Path::new(path)).await?;
    delete_resume_artifacts(path).await
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct VerifiedChunkRecord {
    index: u32,
    mac_b64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ResumeSidecar {
    version: u32,
    file_size: u64,
    expected_condensed_mac_b64: String,
    verified_chunks: Vec<VerifiedChunkRecord>,
}

#[derive(Debug)]
struct ResumeTracker {
    file_size: u64,
    expected_condensed_mac_b64: String,
    chunk_macs: Vec<Option<[u8; 16]>>,
    dirty_chunks: usize,
}

impl ResumeTracker {
    const fn new(
        file_size: u64,
        expected_condensed_mac_b64: String,
        chunk_macs: Vec<Option<[u8; 16]>>,
    ) -> Self {
        Self {
            file_size,
            expected_condensed_mac_b64,
            chunk_macs,
            dirty_chunks: 0,
        }
    }

    fn mark_verified(&mut self, index: u32, mac: [u8; 16]) {
        let Some(slot) = self.chunk_macs.get_mut(index as usize) else {
            return;
        };
        let was_unverified = slot.is_none();
        *slot = Some(mac);
        if was_unverified {
            self.dirty_chunks = self.dirty_chunks.saturating_add(1);
        }
    }

    fn snapshot(&mut self) -> ResumeSidecar {
        self.dirty_chunks = 0;
        ResumeSidecar {
            version: CURRENT_RESUME_SIDECAR_VERSION,
            file_size: self.file_size,
            expected_condensed_mac_b64: self.expected_condensed_mac_b64.clone(),
            verified_chunks: self
                .chunk_macs
                .iter()
                .enumerate()
                .filter_map(|(index, mac)| {
                    mac.and_then(|mac| {
                        Some(VerifiedChunkRecord {
                            index: u32::try_from(index).ok()?,
                            mac_b64: STANDARD.encode(mac),
                        })
                    })
                })
                .collect(),
        }
    }

    fn trusted_chunks(&self) -> Vec<Option<[u8; 16]>> {
        self.chunk_macs.clone()
    }
}

#[derive(Debug)]
struct ResumeValidation {
    trusted_chunks: Vec<Option<[u8; 16]>>,
    trusted_count: usize,
    trusted_bytes: u64,
    sidecar_loaded: bool,
    source: Option<ResumeReuseSource>,
}

impl ResumeValidation {
    fn empty(chunk_count: usize) -> Self {
        Self {
            trusted_chunks: vec![None; chunk_count],
            trusted_count: 0,
            trusted_bytes: 0,
            sidecar_loaded: false,
            source: None,
        }
    }
}

struct SidecarValidationInput<'a> {
    boundaries: &'a [mega::MegaChunk],
    part_path: &'a Path,
    sidecar: &'a ResumeSidecar,
    file_size: u64,
    aes_key: &'a [u8; 16],
    aes_iv: &'a [u8; 8],
    expected_condensed_mac_b64: &'a str,
}

async fn load_sidecar(path: &Path) -> Option<ResumeSidecar> {
    let data = tokio::fs::read(path).await.ok()?;
    serde_json::from_slice(&data).ok()
}

async fn save_sidecar_atomic(path: &Path, sidecar: &ResumeSidecar) -> io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_vec(sidecar)?;
    let mut file = tokio::fs::File::create(&tmp).await?;
    file.write_all(&data).await?;
    file.flush().await?;
    file.sync_data().await?;
    drop(file);
    tokio::fs::rename(&tmp, path).await?;

    #[cfg(unix)]
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        let parent = parent.to_path_buf();
        let _ = tokio::task::spawn_blocking(move || {
            std::fs::File::open(parent).and_then(|dir| dir.sync_all())
        })
        .await;
    }

    Ok(())
}

async fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

async fn delete_sidecar(path: &Path) -> io::Result<()> {
    remove_file_if_exists(path).await
}

fn spawn_sidecar_writer(
    path: PathBuf,
) -> (
    tokio::sync::mpsc::UnboundedSender<ResumeSidecar>,
    JoinHandle<()>,
) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ResumeSidecar>();
    let handle = tokio::spawn(async move {
        while let Some(snapshot) = rx.recv().await {
            if let Err(err) = save_sidecar_atomic(&path, &snapshot).await {
                log::warn!(
                    "Failed to persist resume sidecar {} after verified chunk sync: {err}",
                    path.display()
                );
            }
        }
    });
    (tx, handle)
}

async fn finish_sidecar_writer(path: &Path, handle: JoinHandle<()>) {
    if let Err(err) = handle.await {
        log::warn!(
            "Resume sidecar writer task failed for {}: {err}",
            path.display()
        );
    }
}

fn encode_expected_mac(node: &mega::Node) -> Result<String> {
    let mac = node
        .condensed_mac()
        .ok_or(mega::Error::MissingCondensedMac)?;
    Ok(STANDARD.encode(mac))
}

const fn should_reuse_resume_state(force_overwrite: bool, trust_resume_state: bool) -> bool {
    !force_overwrite && trust_resume_state
}

const fn is_condensed_mac_mismatch(error: &Error) -> bool {
    matches!(error, Error::Mega(mega::Error::CondensedMacMismatch))
}

const fn should_delete_resume_state_on_error(config: &DownloadConfig, error: &Error) -> bool {
    is_condensed_mac_mismatch(error)
        || (config.cleanup_on_error && !matches!(error, Error::Cancelled))
}

fn consume_reused_bytes(remaining: &AtomicU64, delta: u64) -> u64 {
    let mut current = remaining.load(Ordering::Relaxed);
    loop {
        if current == 0 || delta == 0 {
            return 0;
        }
        let consumed = delta.min(current);
        match remaining.compare_exchange_weak(
            current,
            current - consumed,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return consumed,
            Err(next) => current = next,
        }
    }
}

struct DownloadFinishContext<'a> {
    node: &'a mega::Node,
    path: &'a str,
    part_path: &'a Path,
    sidecar_path: &'a Path,
    reused_bytes: u64,
    stats: &'a DownloadStatsTracker,
    tracker: &'a Mutex<ResumeTracker>,
    progress: &'a Arc<dyn DownloadProgress>,
    name: &'a str,
}

/// Core downloader that handles MEGA file downloads.
pub struct Downloader<F: FileSystem = TokioFileSystem> {
    client: mega::Client,
    config: DownloadConfig,
    fs: F,
}

impl Downloader<TokioFileSystem> {
    /// Creates a new downloader with the default file system.
    #[must_use]
    pub const fn new(client: mega::Client, config: DownloadConfig) -> Self {
        Self {
            client,
            config,
            fs: TokioFileSystem,
        }
    }
}

impl<F: FileSystem> Downloader<F> {
    /// Creates a new downloader with a custom file system implementation.
    #[must_use]
    pub const fn with_fs(client: mega::Client, config: DownloadConfig, fs: F) -> Self {
        Self { client, config, fs }
    }

    /// Returns a reference to the underlying MEGA client.
    #[must_use]
    pub const fn client(&self) -> &mega::Client {
        &self.client
    }

    /// Returns a mutable reference to the underlying MEGA client.
    pub const fn client_mut(&mut self) -> &mut mega::Client {
        &mut self.client
    }

    /// Returns a reference to the download configuration.
    #[must_use]
    pub const fn config(&self) -> &DownloadConfig {
        &self.config
    }

    /// Classifies a file's current status on disk.
    async fn inspect_local_file(&self, path: &str, expected_size: u64) -> InspectedLocalFile {
        let part_path = part_path(path);
        let observed = ObservedLocalFile {
            final_size: self.fs.file_size(Path::new(path)).await,
            part_size: self.fs.file_size(&part_path).await,
            has_sidecar: self.fs.file_exists(&sidecar_path(path)).await,
            verified_resume_bytes: resume_sidecar_verified_bytes(path).unwrap_or(0),
        };
        classify_observed_local_file(observed, expected_size, self.config.force_overwrite)
    }

    /// Collects files from nodes, checking which need to be downloaded.
    pub async fn collect_files<'a>(
        &self,
        nodes: &'a mega::Nodes,
        progress: &Arc<dyn DownloadProgress>,
    ) -> CollectedFiles<'a> {
        let roots = nodes.roots().collect::<Vec<_>>();
        let single_root_file = roots.len() == 1 && roots[0].kind().is_file();
        let all_items: Vec<_> = roots
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
            .collect();

        let mut to_download = Vec::new();
        let mut completed = Vec::new();
        let mut skipped = 0;
        let mut partial = 0;

        for item in all_items {
            let local = self.inspect_local_file(&item.path, item.node.size()).await;
            match local.status {
                FileStatus::Complete => {
                    skipped += 1;
                    completed.push(item);
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

    /// Ensures the parent directory exists for a file path.
    async fn ensure_parent_dir(&self, path: &str) -> Result<()> {
        if let Some(parent) = Path::new(path)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
        {
            self.fs.create_dir_all(parent).await?;
        }
        Ok(())
    }

    async fn complete_existing_file(
        &self,
        node: &mega::Node,
        path: &str,
        progress: &Arc<dyn DownloadProgress>,
    ) -> Option<FileStats> {
        if self.config.force_overwrite
            || self
                .fs
                .file_size(Path::new(path))
                .await
                .is_none_or(|size| size != node.size())
        {
            return None;
        }
        let stats = FileStats {
            size: node.size(),
            network_bytes: 0,
            reused_bytes: 0,
            elapsed: std::time::Duration::ZERO,
            average_speed: 0,
            peak_speed: 0,
            ramp_up_time: None,
        };
        progress.on_file_complete(path, &stats);
        Some(stats)
    }

    async fn revalidate_resume_chunks(
        &self,
        node: &mega::Node,
        boundaries: &[mega::MegaChunk],
        part_path: &Path,
        sidecar_path: &Path,
        expected_condensed_mac_b64: &str,
    ) -> Result<ResumeValidation> {
        let Some(sidecar) = load_sidecar(sidecar_path).await else {
            return Ok(ResumeValidation::empty(boundaries.len()));
        };

        let Some(aes_iv) = node.aes_iv() else {
            return Ok(ResumeValidation {
                sidecar_loaded: true,
                ..ResumeValidation::empty(boundaries.len())
            });
        };

        Ok(self
            .revalidate_sidecar_chunks(SidecarValidationInput {
                boundaries,
                part_path,
                sidecar: &sidecar,
                file_size: node.size(),
                aes_key: node.aes_key(),
                aes_iv,
                expected_condensed_mac_b64,
            })
            .await)
    }

    async fn revalidate_sidecar_chunks(
        &self,
        input: SidecarValidationInput<'_>,
    ) -> ResumeValidation {
        let mut validation = ResumeValidation {
            sidecar_loaded: true,
            ..ResumeValidation::empty(input.boundaries.len())
        };

        if input.sidecar.version != CURRENT_RESUME_SIDECAR_VERSION
            || input.sidecar.file_size != input.file_size
            || input.sidecar.expected_condensed_mac_b64 != input.expected_condensed_mac_b64
        {
            return validation;
        }

        let part_size = self.fs.file_size(input.part_path).await.unwrap_or(0);
        let max_chunk_len = input
            .boundaries
            .iter()
            .filter_map(|chunk| usize::try_from(chunk.length).ok())
            .max()
            .unwrap_or(0);
        let mut scratch = vec![0u8; max_chunk_len];

        for record in &input.sidecar.verified_chunks {
            let Some(boundary) = input.boundaries.get(record.index as usize) else {
                continue;
            };
            if validation.trusted_chunks[record.index as usize].is_some() {
                continue;
            }
            if boundary.offset.saturating_add(boundary.length) > part_size {
                continue;
            }
            let Ok(decoded) = STANDARD.decode(record.mac_b64.as_bytes()) else {
                continue;
            };
            let Ok(expected_mac) = <[u8; 16]>::try_from(decoded.as_slice()) else {
                continue;
            };

            let Ok(chunk_len) = usize::try_from(boundary.length) else {
                continue;
            };
            let buf = &mut scratch[..chunk_len];
            if self
                .fs
                .read_exact_at(input.part_path, boundary.offset, buf)
                .await
                .is_err()
            {
                continue;
            }
            let actual_mac = mega::compute_mega_chunk_mac(buf, input.aes_key, input.aes_iv);
            if actual_mac == expected_mac {
                validation.trusted_chunks[record.index as usize] = Some(actual_mac);
                validation.trusted_count = validation.trusted_count.saturating_add(1);
                validation.trusted_bytes = validation.trusted_bytes.saturating_add(boundary.length);
            }
        }

        if validation.trusted_count > 0 {
            validation.source = Some(ResumeReuseSource::Sidecar);
        }

        validation
    }

    /// Downloads a single file using atomic `.part` file semantics.
    ///
    /// Writes to `{path}.part` during download, then renames to `{path}` on success.
    /// On recoverable errors, keeps the `.part` file for resume unless
    /// `cleanup_on_error` is enabled. Final MAC mismatches always discard
    /// resumable state because the assembled plaintext failed verification.
    /// If a `cancellation_token` is provided, the download can be cancelled.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be created or the download fails.
    ///
    /// # Panics
    ///
    /// Panics if the internal resume tracker mutex is poisoned.
    #[allow(clippy::too_many_lines)]
    pub async fn download_file(
        &self,
        node: &mega::Node,
        path: &str,
        progress: &Arc<dyn DownloadProgress>,
        trust_resume_state: bool,
        cancellation_token: Option<CancellationToken>,
    ) -> Result<FileStats> {
        if let Some(stats) = self.complete_existing_file(node, path, progress).await {
            return Ok(stats);
        }

        self.ensure_parent_dir(path).await?;
        let name = path.to_string();
        progress.on_file_start(&name, node.size());

        let pp = part_path(path);
        let sp = sidecar_path(path);
        let expected_condensed_mac_b64 = encode_expected_mac(node)?;
        let boundaries = mega::mega_chunk_boundaries(node.size());
        let resume_validation =
            if !should_reuse_resume_state(self.config.force_overwrite, trust_resume_state) {
                ResumeValidation::empty(boundaries.len())
            } else {
                self.revalidate_resume_chunks(
                    node,
                    &boundaries,
                    &pp,
                    &sp,
                    &expected_condensed_mac_b64,
                )
                .await?
            };
        let preserve_existing = resume_validation.trusted_count > 0;
        if !preserve_existing {
            let _ = delete_sidecar(&sp).await;
        }
        if resume_validation.sidecar_loaded && resume_validation.trusted_count == 0 {
            log::debug!("Resume sidecar found for {path}, but no chunks were reusable");
        }
        let trusted_bytes = resume_validation.trusted_bytes;

        let stats = Arc::new(DownloadStatsTracker::new(
            node.size().saturating_sub(trusted_bytes),
        ));

        if trusted_bytes > 0 {
            progress.on_resume_reused(&name, resume_validation.trusted_count, trusted_bytes);
        }

        // Open the plaintext .part file, preserving only locally revalidated chunks.
        let file = self
            .fs
            .open_part_file(&pp, node.size(), preserve_existing)
            .await?;

        let name_clone = name.clone();
        let tracker = Arc::new(Mutex::new(ResumeTracker::new(
            node.size(),
            expected_condensed_mac_b64,
            resume_validation.trusted_chunks,
        )));
        let trusted_for_download = tracker.lock().unwrap().trusted_chunks();
        let (sidecar_updates_tx, sidecar_writer) = spawn_sidecar_writer(sp.clone());
        // The mega library calls the progress callback with the *cumulative*
        // total bytes downloaded so far, NOT a delta.  We use fetch_max (not
        // swap) so that out-of-order callbacks from parallel workers never
        // regress the high-water mark.
        let cumulative = Arc::new(CumulativeProgress::new());
        let reused_remaining = Arc::new(AtomicU64::new(trusted_bytes));
        let stats_clone = Arc::clone(&stats);
        let progress_clone = Arc::clone(progress);
        let name_for_cb = name.clone();
        let progress_cb = move |cumulative_bytes: u64| {
            let delta = cumulative.delta(cumulative_bytes);
            if delta > 0 {
                let reused_delta = consume_reused_bytes(&reused_remaining, delta);
                let fresh_delta = delta.saturating_sub(reused_delta);
                if fresh_delta > 0 {
                    let _ = stats_clone.record_bytes(fresh_delta);
                }
                progress_clone.on_progress(
                    &name_for_cb,
                    ProgressDelta {
                        total_bytes_delta: delta,
                        network_bytes_delta: fresh_delta,
                    },
                );
            }
        };
        let verify_tracker = Arc::clone(&tracker);
        let sidecar_updates_tx_for_cb = sidecar_updates_tx.clone();
        let chunk_verified_cb = move |index: u32, mac: [u8; 16]| {
            let snapshot = {
                let mut guard = verify_tracker.lock().unwrap();
                guard.mark_verified(index, mac);
                guard.snapshot()
            };
            let _ = sidecar_updates_tx_for_cb.send(snapshot);
        };

        // Download with progress callback, optionally with cancellation support
        let download_result = if let Some(token) = cancellation_token {
            let download_fut = self.client.download_node_parallel_resumable_to_file_with_progress(
                node,
                file,
                self.config.chunks_per_file,
                &trusted_for_download,
                Some(progress_cb),
                Some(chunk_verified_cb),
            );
            tokio::select! {
                res = download_fut => res.map_err(Error::Mega),
                () = token.cancelled() => {
                    Err(Error::Cancelled)
                }
            }
        } else {
            self.client
                .download_node_parallel_resumable_to_file_with_progress(
                    node,
                    file,
                    self.config.chunks_per_file,
                    &trusted_for_download,
                    Some(progress_cb),
                    Some(chunk_verified_cb),
                )
                .await
                .map_err(Error::Mega)
        };
        drop(sidecar_updates_tx);
        finish_sidecar_writer(&sp, sidecar_writer).await;
        self.finish_download_result(
            DownloadFinishContext {
                node,
                path,
                part_path: &pp,
                sidecar_path: &sp,
                reused_bytes: trusted_bytes,
                stats: &stats,
                tracker: &tracker,
                progress,
                name: &name_clone,
            },
            download_result,
        )
        .await
    }

    async fn finish_download_result(
        &self,
        ctx: DownloadFinishContext<'_>,
        download_result: Result<()>,
    ) -> Result<FileStats> {
        match download_result {
            Ok(()) => {
                if self.config.force_overwrite {
                    let _ = self.fs.remove_file(Path::new(ctx.path)).await;
                }
                // Rename .part → final
                self.fs
                    .rename_file(ctx.part_path, Path::new(ctx.path))
                    .await?;
                delete_sidecar(ctx.sidecar_path).await?;

                let file_stats = FileStats {
                    size: ctx.node.size(),
                    network_bytes: ctx.stats.downloaded_bytes(),
                    reused_bytes: ctx.reused_bytes,
                    elapsed: ctx.stats.elapsed(),
                    average_speed: ctx.stats.average_speed(),
                    peak_speed: ctx.stats.peak_speed(),
                    ramp_up_time: ctx.stats.time_to_80pct(),
                };
                ctx.progress.on_file_complete(ctx.name, &file_stats);
                Ok(file_stats)
            }
            Err(e) => {
                // Keep .part files for future resume support; only clean up if configured
                if should_delete_resume_state_on_error(&self.config, &e) {
                    let _ = self.fs.remove_file(ctx.part_path).await;
                    let _ = delete_sidecar(ctx.sidecar_path).await;
                } else {
                    match self.fs.sync_file(ctx.part_path).await {
                        Ok(()) => {
                            let snapshot = ctx.tracker.lock().unwrap().snapshot();
                            if let Err(save_err) =
                                save_sidecar_atomic(ctx.sidecar_path, &snapshot).await
                            {
                                log::warn!(
                                    "Failed to save final resume sidecar {}: {save_err}",
                                    ctx.sidecar_path.display()
                                );
                            }
                        }
                        Err(sync_err) => {
                            log::warn!(
                                "Failed to sync partial file {} before saving resume sidecar: {sync_err}",
                                ctx.part_path.display()
                            );
                        }
                    }
                }
                if !matches!(e, Error::Cancelled) {
                    ctx.progress.on_error(ctx.name, &e.to_string());
                }
                Err(e)
            }
        }
    }

    /// Downloads all collected files with concurrent downloads.
    ///
    /// Returns session statistics on completion.
    ///
    /// # Errors
    ///
    /// Individual file download errors are logged but do not cause the
    /// entire operation to fail. The returned stats will reflect which
    /// files succeeded.
    pub async fn download_all(
        &self,
        files: &[DownloadItem<'_>],
        progress: &Arc<dyn DownloadProgress>,
        skipped_count: usize,
    ) -> Result<SessionStats> {
        let mut builder = SessionStatsBuilder::new();
        builder.set_skipped(skipped_count);

        if files.is_empty() {
            return Ok(builder.build());
        }

        let peak_speed = Arc::new(AtomicU64::new(0));

        let results: Vec<_> = stream::iter(files)
            .map(|item| {
                let peak_tracker = Arc::clone(&peak_speed);
                async move {
                    let result = self
                        .download_file(item.node, &item.path, progress, false, None)
                        .await;
                    if let Ok(ref stats) = result {
                        peak_tracker.fetch_max(stats.peak_speed, Ordering::Relaxed);
                    }
                    result
                }
            })
            .buffer_unordered(self.config.concurrent_files)
            .collect()
            .await;

        builder.set_peak_speed(peak_speed.load(Ordering::Relaxed));

        for result in results {
            match result {
                Ok(file_stats) => builder.add_download(&file_stats),
                Err(e) => {
                    log::error!("Download failed: {e}");
                }
            }
        }

        Ok(builder.build())
    }

    /// Downloads all owned items with concurrent downloads.
    ///
    /// This is the same as [`download_all`](Self::download_all) but takes
    /// [`OwnedDownloadItem`] values, making it safe to call from inside
    /// `tokio::spawn` (which requires `'static` futures).
    ///
    /// # Errors
    ///
    /// Individual file download errors are logged but do not cause the
    /// entire operation to fail. The returned stats will reflect which
    /// files succeeded.
    pub async fn download_all_owned(
        &self,
        files: &[OwnedDownloadItem],
        progress: &Arc<dyn DownloadProgress>,
        skipped_count: usize,
    ) -> Result<SessionStats> {
        let mut builder = SessionStatsBuilder::new();
        builder.set_skipped(skipped_count);

        if files.is_empty() {
            return Ok(builder.build());
        }

        let peak_speed = Arc::new(AtomicU64::new(0));

        let results: Vec<_> = stream::iter(files)
            .map(|item| {
                let peak_tracker = Arc::clone(&peak_speed);
                async move {
                    let result = self
                        .download_file(&item.node, &item.path, progress, false, None)
                        .await;
                    if let Ok(ref stats) = result {
                        peak_tracker.fetch_max(stats.peak_speed, Ordering::Relaxed);
                    }
                    result
                }
            })
            .buffer_unordered(self.config.concurrent_files)
            .collect()
            .await;

        builder.set_peak_speed(peak_speed.load(Ordering::Relaxed));

        for result in results {
            match result {
                Ok(file_stats) => builder.add_download(&file_stats),
                Err(e) => {
                    log::error!("Download failed: {e}");
                }
            }
        }

        Ok(builder.build())
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

#[must_use]
pub fn infer_package_display_name(nodes: &mega::Nodes, collected: &CollectedFiles<'_>) -> String {
    let roots = nodes.roots().collect::<Vec<_>>();
    roots.iter()
        .find(|root| root.kind().is_folder())
        .map(|root| root.name().to_string())
        .or_else(|| {
            (roots.len() == 1 && roots[0].kind().is_file())
                .then(|| stemmed_package_name(roots[0].name()))
        })
        .or_else(|| common_collected_root(collected))
        .unwrap_or_else(|| "root".to_string())
}

#[must_use]
pub fn infer_package_id(nodes: &mega::Nodes, collected: &CollectedFiles<'_>) -> PackageId {
    let display_name = infer_package_display_name(nodes, collected);
    PackageId::for_package_key(&PackageKey::new(display_name))
}

fn common_collected_root(collected: &CollectedFiles<'_>) -> Option<String> {
    let mut roots = collected
        .to_download
        .iter()
        .chain(collected.completed.iter())
        .filter_map(|item| item.path.split('/').next())
        .filter(|root| !root.is_empty())
        .map(str::to_string);
    let first = roots.next()?;
    roots.all(|root| root == first).then_some(first)
}

fn single_file_package_path(file_name: &str) -> String {
    format!("{}/{}", stemmed_package_name(file_name), file_name)
}

fn stemmed_package_name(file_name: &str) -> String {
    Path::new(file_name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or(file_name)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_progress_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<NoProgress>();
    }

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
    fn part_path_appends_extension() {
        assert_eq!(part_path("foo/bar.zip"), PathBuf::from("foo/bar.zip.part"));
        assert_eq!(part_path("file.txt"), PathBuf::from("file.txt.part"));
    }

    #[test]
    fn resume_state_is_reused_only_for_session_tracked_files() {
        assert!(should_reuse_resume_state(false, true));
        assert!(!should_reuse_resume_state(false, false));
        assert!(!should_reuse_resume_state(true, true));
    }

    #[test]
    fn file_status_variants() {
        assert_ne!(FileStatus::Complete, FileStatus::Partial);
        assert_ne!(FileStatus::Partial, FileStatus::Missing);
        assert_ne!(FileStatus::Complete, FileStatus::Missing);
    }

    // =========================================================================
    // Mock-based classify_file tests
    // =========================================================================

    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A mock file system for testing `classify_file` behavior.
    struct MockFileSystem {
        /// Maps path → file size (if the file exists).
        files: Mutex<HashMap<PathBuf, u64>>,
    }

    impl MockFileSystem {
        fn new() -> Self {
            Self {
                files: Mutex::new(HashMap::new()),
            }
        }

        fn add_file(&self, path: impl Into<PathBuf>, size: u64) {
            self.files.lock().unwrap().insert(path.into(), size);
        }
    }

    #[async_trait::async_trait]
    impl crate::fs::FileSystem for MockFileSystem {
        async fn file_exists(&self, path: &Path) -> bool {
            self.files.lock().unwrap().contains_key(path)
        }

        async fn file_size(&self, path: &Path) -> Option<u64> {
            self.files.lock().unwrap().get(path).copied()
        }

        async fn create_dir_all(&self, _path: &Path) -> std::io::Result<()> {
            Ok(())
        }

        async fn create_file(&self, _path: &Path, _size: u64) -> std::io::Result<tokio::fs::File> {
            // Not needed for classify_file tests
            Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "mock"))
        }

        async fn open_part_file(
            &self,
            _path: &Path,
            _size: u64,
            _preserve_existing: bool,
        ) -> std::io::Result<tokio::fs::File> {
            Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "mock"))
        }

        async fn read_exact_at(
            &self,
            _path: &Path,
            _offset: u64,
            _buf: &mut [u8],
        ) -> std::io::Result<()> {
            Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "mock"))
        }

        async fn rename_file(&self, _from: &Path, _to: &Path) -> std::io::Result<()> {
            Ok(())
        }

        async fn sync_file(&self, _path: &Path) -> std::io::Result<()> {
            Ok(())
        }

        async fn remove_file(&self, _path: &Path) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn mock_downloader(fs: MockFileSystem) -> Downloader<MockFileSystem> {
        let http = reqwest::Client::new();
        let client = mega::Client::builder().build(http).unwrap();
        Downloader::with_fs(client, DownloadConfig::default(), fs)
    }

    fn mock_downloader_force(fs: MockFileSystem) -> Downloader<MockFileSystem> {
        let http = reqwest::Client::new();
        let client = mega::Client::builder().build(http).unwrap();
        let config = DownloadConfig {
            force_overwrite: true,
            ..DownloadConfig::default()
        };
        Downloader::with_fs(client, config, fs)
    }

    fn tokio_downloader() -> Downloader<TokioFileSystem> {
        let http = reqwest::Client::new();
        let client = mega::Client::builder().build(http).unwrap();
        Downloader::new(client, DownloadConfig::default())
    }

    fn test_plaintext(size: usize) -> Vec<u8> {
        (0..size).map(|i| u8::try_from(i % 251).unwrap()).collect()
    }

    fn usize_from_u64(value: u64) -> usize {
        usize::try_from(value).unwrap()
    }

    fn chunk_data<'a>(data: &'a [u8], chunk: &mega::MegaChunk) -> &'a [u8] {
        &data[usize_from_u64(chunk.offset)..usize_from_u64(chunk.offset + chunk.length)]
    }

    fn sidecar_for_chunk(
        file_size: u64,
        expected_condensed_mac_b64: &str,
        index: u32,
        mac: [u8; 16],
    ) -> ResumeSidecar {
        ResumeSidecar {
            version: CURRENT_RESUME_SIDECAR_VERSION,
            file_size,
            expected_condensed_mac_b64: expected_condensed_mac_b64.to_string(),
            verified_chunks: vec![VerifiedChunkRecord {
                index,
                mac_b64: STANDARD.encode(mac),
            }],
        }
    }

    #[test]
    fn consume_reused_bytes_caps_at_remaining() {
        let remaining = AtomicU64::new(100);

        assert_eq!(consume_reused_bytes(&remaining, 60), 60);
        assert_eq!(remaining.load(Ordering::Relaxed), 40);
        assert_eq!(consume_reused_bytes(&remaining, 60), 40);
        assert_eq!(remaining.load(Ordering::Relaxed), 0);
        assert_eq!(consume_reused_bytes(&remaining, 60), 0);
    }

    #[test]
    fn cleanup_policy_preserves_recoverable_errors_by_default() {
        let config = DownloadConfig::default();

        assert!(!should_delete_resume_state_on_error(
            &config,
            &Error::Download("temporary network failure".to_string()),
        ));
        assert!(!should_delete_resume_state_on_error(
            &config,
            &Error::Cancelled,
        ));
        assert!(should_delete_resume_state_on_error(
            &config,
            &Error::Mega(mega::Error::CondensedMacMismatch),
        ));
    }

    #[test]
    fn cleanup_policy_honors_explicit_cleanup_except_cancel() {
        let config = DownloadConfig {
            cleanup_on_error: true,
            ..DownloadConfig::default()
        };

        assert!(should_delete_resume_state_on_error(
            &config,
            &Error::Download("temporary network failure".to_string()),
        ));
        assert!(!should_delete_resume_state_on_error(
            &config,
            &Error::Cancelled,
        ));
    }

    #[tokio::test]
    async fn revalidate_sidecar_accepts_matching_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("file.bin.part");
        let file_size = 300_000_u64;
        let data = test_plaintext(usize_from_u64(file_size));
        tokio::fs::write(&part, &data).await.unwrap();

        let aes_key = [7u8; 16];
        let aes_iv = [3u8; 8];
        let expected = STANDARD.encode([9u8; 8]);
        let boundaries = mega::mega_chunk_boundaries(file_size);
        let first = &boundaries[0];
        let first_data = chunk_data(&data, first);
        let mac = mega::compute_mega_chunk_mac(first_data, &aes_key, &aes_iv);
        let sidecar = sidecar_for_chunk(file_size, &expected, first.index, mac);

        let validation = tokio_downloader()
            .revalidate_sidecar_chunks(SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                aes_key: &aes_key,
                aes_iv: &aes_iv,
                expected_condensed_mac_b64: &expected,
            })
            .await;

        assert!(validation.sidecar_loaded);
        assert_eq!(validation.trusted_count, 1);
        assert_eq!(validation.trusted_bytes, first.length);
        assert_eq!(validation.trusted_chunks[0], Some(mac));
        assert!(validation.trusted_chunks[1].is_none());
    }

    #[tokio::test]
    async fn revalidate_sidecar_rejects_bad_chunk_mac() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("file.bin.part");
        let file_size = 300_000_u64;
        let data = test_plaintext(usize_from_u64(file_size));
        tokio::fs::write(&part, &data).await.unwrap();

        let aes_key = [7u8; 16];
        let aes_iv = [3u8; 8];
        let expected = STANDARD.encode([9u8; 8]);
        let boundaries = mega::mega_chunk_boundaries(file_size);
        let sidecar = sidecar_for_chunk(file_size, &expected, 0, [1u8; 16]);

        let validation = tokio_downloader()
            .revalidate_sidecar_chunks(SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                aes_key: &aes_key,
                aes_iv: &aes_iv,
                expected_condensed_mac_b64: &expected,
            })
            .await;

        assert!(validation.sidecar_loaded);
        assert_eq!(validation.trusted_count, 0);
        assert_eq!(validation.trusted_bytes, 0);
        assert!(validation.trusted_chunks.iter().all(Option::is_none));
    }

    #[tokio::test]
    async fn revalidate_sidecar_rejects_short_part_file() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("file.bin.part");
        let file_size = 300_000_u64;
        let data = test_plaintext(usize_from_u64(file_size));

        let aes_key = [7u8; 16];
        let aes_iv = [3u8; 8];
        let expected = STANDARD.encode([9u8; 8]);
        let boundaries = mega::mega_chunk_boundaries(file_size);
        let second = &boundaries[1];
        let second_data = chunk_data(&data, second);
        let mac = mega::compute_mega_chunk_mac(second_data, &aes_key, &aes_iv);
        let sidecar = sidecar_for_chunk(file_size, &expected, second.index, mac);

        tokio::fs::write(&part, &data[..usize_from_u64(second.offset)])
            .await
            .unwrap();

        let validation = tokio_downloader()
            .revalidate_sidecar_chunks(SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                aes_key: &aes_key,
                aes_iv: &aes_iv,
                expected_condensed_mac_b64: &expected,
            })
            .await;

        assert_eq!(validation.trusted_count, 0);
        assert_eq!(validation.trusted_bytes, 0);
        assert!(validation.trusted_chunks.iter().all(Option::is_none));
    }

    #[tokio::test]
    async fn revalidate_sidecar_rejects_stale_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("file.bin.part");
        let file_size = 300_000_u64;
        let data = test_plaintext(usize_from_u64(file_size));
        tokio::fs::write(&part, &data).await.unwrap();

        let aes_key = [7u8; 16];
        let aes_iv = [3u8; 8];
        let expected = STANDARD.encode([9u8; 8]);
        let boundaries = mega::mega_chunk_boundaries(file_size);
        let sidecar = sidecar_for_chunk(file_size, "stale", 0, [1u8; 16]);

        let validation = tokio_downloader()
            .revalidate_sidecar_chunks(SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                aes_key: &aes_key,
                aes_iv: &aes_iv,
                expected_condensed_mac_b64: &expected,
            })
            .await;

        assert!(validation.sidecar_loaded);
        assert_eq!(validation.trusted_count, 0);
        assert_eq!(validation.trusted_bytes, 0);
        assert!(validation.trusted_chunks.iter().all(Option::is_none));
    }

    #[tokio::test]
    async fn stale_sidecar_without_matching_metadata_trusts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("file.bin.part");
        let sidecar = dir.path().join("file.bin.part.meta.json");
        let file_size = 300_000_u64;
        let data = test_plaintext(200_000);
        tokio::fs::write(&part, &data).await.unwrap();

        let expected = STANDARD.encode([9u8; 8]);
        let aes_key = [7u8; 16];
        let aes_iv = [3u8; 8];
        let boundaries = mega::mega_chunk_boundaries(file_size);
        save_sidecar_atomic(
            &sidecar,
            &ResumeSidecar {
                version: CURRENT_RESUME_SIDECAR_VERSION,
                file_size,
                expected_condensed_mac_b64: "wrong".to_string(),
                verified_chunks: vec![VerifiedChunkRecord {
                    index: 0,
                    mac_b64: STANDARD.encode([1u8; 16]),
                }],
            },
        )
        .await
        .unwrap();
        let loaded_sidecar = load_sidecar(&sidecar).await.unwrap();
        let validation = tokio_downloader()
            .revalidate_sidecar_chunks(SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &loaded_sidecar,
                file_size,
                aes_key: &aes_key,
                aes_iv: &aes_iv,
                expected_condensed_mac_b64: &expected,
            })
            .await;

        assert!(validation.sidecar_loaded);
        assert_eq!(validation.trusted_count, 0);
        assert_eq!(validation.trusted_bytes, 0);
        assert!(validation.trusted_chunks.iter().all(Option::is_none));
    }

    #[tokio::test]
    async fn legacy_v1_sidecar_trusts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("file.bin.part");
        let sidecar = dir.path().join("file.bin.part.meta.json");
        let file_size = 300_000_u64;
        let data = test_plaintext(usize_from_u64(file_size));
        tokio::fs::write(&part, &data).await.unwrap();

        let expected = STANDARD.encode([9u8; 8]);
        let aes_key = [7u8; 16];
        let aes_iv = [3u8; 8];
        let boundaries = mega::mega_chunk_boundaries(file_size);
        let first = &boundaries[0];
        let first_data = chunk_data(&data, first);
        let mac = mega::compute_mega_chunk_mac(first_data, &aes_key, &aes_iv);
        save_sidecar_atomic(
            &sidecar,
            &ResumeSidecar {
                version: 1,
                file_size,
                expected_condensed_mac_b64: expected.clone(),
                verified_chunks: vec![VerifiedChunkRecord {
                    index: first.index,
                    mac_b64: STANDARD.encode(mac),
                }],
            },
        )
        .await
        .unwrap();

        let loaded_sidecar = load_sidecar(&sidecar).await.unwrap();
        let validation = tokio_downloader()
            .revalidate_sidecar_chunks(SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &loaded_sidecar,
                file_size,
                aes_key: &aes_key,
                aes_iv: &aes_iv,
                expected_condensed_mac_b64: &expected,
            })
            .await;

        assert!(validation.sidecar_loaded);
        assert_eq!(validation.trusted_count, 0);
        assert_eq!(validation.trusted_bytes, 0);
        assert!(validation.trusted_chunks.iter().all(Option::is_none));
    }

    #[tokio::test]
    async fn sidecar_save_and_delete_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let sidecar_path = dir.path().join("file.bin.part.meta.json");
        let sidecar = ResumeSidecar {
            version: CURRENT_RESUME_SIDECAR_VERSION,
            file_size: 42,
            expected_condensed_mac_b64: STANDARD.encode([9u8; 8]),
            verified_chunks: vec![VerifiedChunkRecord {
                index: 0,
                mac_b64: STANDARD.encode([1u8; 16]),
            }],
        };

        save_sidecar_atomic(&sidecar_path, &sidecar).await.unwrap();
        let loaded = load_sidecar(&sidecar_path).await.unwrap();
        assert_eq!(loaded.file_size, sidecar.file_size);
        assert_eq!(loaded.verified_chunks.len(), 1);

        delete_sidecar(&sidecar_path).await.unwrap();
        assert!(load_sidecar(&sidecar_path).await.is_none());
    }

    #[tokio::test]
    async fn resume_sidecar_verified_bytes_sums_verified_chunk_lengths() {
        let dir = tempfile::tempdir().unwrap();
        let base_path = dir.path().join("file.bin");
        let file_path = base_path.to_string_lossy().into_owned();
        let sidecar_path = sidecar_path(&file_path);
        let file_size = 300_000_u64;
        let boundaries = mega::mega_chunk_boundaries(file_size);

        save_sidecar_atomic(
            &sidecar_path,
            &ResumeSidecar {
                version: CURRENT_RESUME_SIDECAR_VERSION,
                file_size,
                expected_condensed_mac_b64: STANDARD.encode([9u8; 8]),
                verified_chunks: vec![
                    VerifiedChunkRecord {
                        index: boundaries[0].index,
                        mac_b64: STANDARD.encode([1u8; 16]),
                    },
                    VerifiedChunkRecord {
                        index: boundaries[1].index,
                        mac_b64: STANDARD.encode([2u8; 16]),
                    },
                ],
            },
        )
        .await
        .unwrap();

        assert_eq!(
            resume_sidecar_verified_bytes(&file_path),
            Some(boundaries[0].length + boundaries[1].length)
        );
    }

    #[tokio::test]
    async fn resume_sidecar_verified_bytes_ignores_legacy_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let base_path = dir.path().join("file.bin");
        let file_path = base_path.to_string_lossy().into_owned();
        let sidecar_path = sidecar_path(&file_path);

        save_sidecar_atomic(
            &sidecar_path,
            &ResumeSidecar {
                version: 1,
                file_size: 300_000,
                expected_condensed_mac_b64: STANDARD.encode([9u8; 8]),
                verified_chunks: vec![VerifiedChunkRecord {
                    index: 0,
                    mac_b64: STANDARD.encode([1u8; 16]),
                }],
            },
        )
        .await
        .unwrap();

        assert_eq!(resume_sidecar_verified_bytes(&file_path), Some(0));
    }

    #[tokio::test]
    async fn sidecar_writer_persists_verified_snapshots_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let sidecar_path = dir.path().join("file.bin.part.meta.json");
        let first = ResumeSidecar {
            version: CURRENT_RESUME_SIDECAR_VERSION,
            file_size: 42,
            expected_condensed_mac_b64: STANDARD.encode([9u8; 8]),
            verified_chunks: vec![VerifiedChunkRecord {
                index: 0,
                mac_b64: STANDARD.encode([1u8; 16]),
            }],
        };
        let second = ResumeSidecar {
            verified_chunks: vec![
                VerifiedChunkRecord {
                    index: 0,
                    mac_b64: STANDARD.encode([1u8; 16]),
                },
                VerifiedChunkRecord {
                    index: 1,
                    mac_b64: STANDARD.encode([2u8; 16]),
                },
            ],
            ..first.clone()
        };

        let (tx, handle) = spawn_sidecar_writer(sidecar_path.clone());
        tx.send(first).unwrap();
        tx.send(second.clone()).unwrap();
        drop(tx);
        finish_sidecar_writer(&sidecar_path, handle).await;

        let loaded = load_sidecar(&sidecar_path).await.unwrap();
        assert_eq!(loaded.verified_chunks, second.verified_chunks);
    }

    #[tokio::test]
    async fn delete_resume_artifacts_removes_part_and_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.bin");
        let path_str = path.to_string_lossy().to_string();
        tokio::fs::write(part_path(&path_str), b"partial")
            .await
            .unwrap();
        tokio::fs::write(sidecar_path(&path_str), b"{}")
            .await
            .unwrap();

        delete_resume_artifacts(&path_str).await.unwrap();

        assert!(!part_path(&path_str).exists());
        assert!(!sidecar_path(&path_str).exists());
    }

    #[tokio::test]
    async fn classify_file_complete() {
        let fs = MockFileSystem::new();
        fs.add_file("movie.mkv", 1_000_000);
        let dl = mock_downloader(fs);
        assert_eq!(
            dl.inspect_local_file("movie.mkv", 1_000_000).await.status,
            FileStatus::Complete
        );
    }

    #[tokio::test]
    async fn classify_file_size_mismatch_checks_part() {
        let fs = MockFileSystem::new();
        // File exists but wrong size, no .part file
        fs.add_file("movie.mkv", 500);
        let dl = mock_downloader(fs);
        assert_eq!(
            dl.inspect_local_file("movie.mkv", 1_000_000).await.status,
            FileStatus::Missing
        );
    }

    #[tokio::test]
    async fn classify_file_partial() {
        let fs = MockFileSystem::new();
        // No final file, but .part file exists
        fs.add_file("movie.mkv.part", 500_000);
        let dl = mock_downloader(fs);
        assert_eq!(
            dl.inspect_local_file("movie.mkv", 1_000_000).await.status,
            FileStatus::Partial
        );
    }

    #[tokio::test]
    async fn classify_file_missing() {
        let fs = MockFileSystem::new();
        let dl = mock_downloader(fs);
        assert_eq!(
            dl.inspect_local_file("movie.mkv", 1_000_000).await.status,
            FileStatus::Missing
        );
    }

    #[tokio::test]
    async fn classify_file_force_overwrite() {
        let fs = MockFileSystem::new();
        // File exists with correct size, but force_overwrite is on
        fs.add_file("movie.mkv", 1_000_000);
        let dl = mock_downloader_force(fs);
        assert_eq!(
            dl.inspect_local_file("movie.mkv", 1_000_000).await.status,
            FileStatus::Missing
        );
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
    fn single_file_package_path_wraps_file_in_stemmed_folder() {
        assert_eq!(single_file_package_path("file.bin"), "file/file.bin");
        assert_eq!(single_file_package_path("archive.tar.gz"), "archive.tar/archive.tar.gz");
    }

    #[test]
    fn stemmed_package_name_uses_file_name_without_extension() {
        assert_eq!(stemmed_package_name("file.bin"), "file");
        assert_eq!(stemmed_package_name("archive.tar.gz"), "archive.tar");
        assert_eq!(stemmed_package_name(".env"), ".env");
    }
}
