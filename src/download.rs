//! Core download logic and abstractions.

use std::fmt::Write as _;
use std::io::{self, Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::task::JoinHandle;
use tokio_util::compat::TokioAsyncReadCompatExt;
use tokio_util::sync::CancellationToken;

use crate::config::DownloadConfig;
use crate::core::{PackageId, PackageKey, ProgressDelta};
use crate::error::{Error, Result};
use crate::fs::{FileFingerprint, FileSystem, TokioFileSystem};
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

/// Result of manually checking resumable state without starting a download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeReverify {
    pub sidecar_loaded: bool,
    pub chunks: usize,
    pub bytes: u64,
}

/// Result of manually checking a completed final file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompletedFileVerify {
    pub bytes: u64,
}

/// Source of reused chunk state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeReuseSource {
    Sidecar,
}

const CURRENT_RESUME_SIDECAR_VERSION: u32 = 2;
const SIDECAR_CHECKPOINT_CHUNK_INTERVAL: usize = 32;

/// Returns the `.part` file path for a given final path.
pub(crate) fn part_path(path: &str) -> PathBuf {
    let mut part = String::with_capacity(path.len() + ".part".len());
    part.push_str(path);
    part.push_str(".part");
    PathBuf::from(part)
}

pub(crate) fn sidecar_path(path: &str) -> PathBuf {
    let mut sidecar = String::with_capacity(path.len() + ".part.meta.json".len());
    sidecar.push_str(path);
    sidecar.push_str(".part.meta.json");
    PathBuf::from(sidecar)
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
    #[serde(default)]
    part_fingerprint: Option<FileFingerprint>,
}

#[derive(Debug)]
struct ResumeTracker {
    file_size: u64,
    expected_condensed_mac: [u8; 8],
    chunk_macs: Vec<Option<[u8; 16]>>,
    dirty_chunks: usize,
}

impl ResumeTracker {
    const fn new(
        file_size: u64,
        expected_condensed_mac: [u8; 8],
        chunk_macs: Vec<Option<[u8; 16]>>,
    ) -> Self {
        Self {
            file_size,
            expected_condensed_mac,
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
            expected_condensed_mac_b64: STANDARD.encode(self.expected_condensed_mac),
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
            part_fingerprint: None,
        }
    }

    fn checkpoint_snapshot(&mut self) -> Option<ResumeSidecar> {
        (self.dirty_chunks >= SIDECAR_CHECKPOINT_CHUNK_INTERVAL).then(|| self.snapshot())
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
    expected_condensed_mac_b64: &'a str,
    aes_key: &'a [u8; 16],
    aes_iv: &'a [u8; 8],
    progress: Option<(&'a str, &'a dyn DownloadProgress)>,
}

#[derive(Debug, Clone, Copy)]
struct TrustedResumeChunkCandidate {
    index: usize,
    length: u64,
    expected_mac: [u8; 16],
}

fn trust_resume_candidate(
    validation: &mut ResumeValidation,
    candidate: TrustedResumeChunkCandidate,
) -> bool {
    if validation.trusted_chunks[candidate.index].is_some() {
        return false;
    }
    validation.trusted_chunks[candidate.index] = Some(candidate.expected_mac);
    validation.trusted_count = validation.trusted_count.saturating_add(1);
    validation.trusted_bytes = validation.trusted_bytes.saturating_add(candidate.length);
    true
}

async fn load_sidecar(path: &Path) -> Option<ResumeSidecar> {
    let data = tokio::fs::read(path).await.ok()?;
    serde_json::from_slice(&data).ok()
}

async fn save_sidecar_atomic(path: &Path, sidecar: &ResumeSidecar) -> io::Result<()> {
    let tmp = sidecar_tmp_path(path);
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

fn sidecar_tmp_path(path: &Path) -> PathBuf {
    path.with_extension("json.tmp")
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

async fn sync_and_fingerprint_part(path: &Path) -> Option<FileFingerprint> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let file = std::fs::OpenOptions::new().read(true).open(&path).ok()?;
        file.sync_all().ok()?;
        let metadata = file.metadata().ok()?;
        Some(FileFingerprint::from_metadata(&metadata))
    })
    .await
    .ok()
    .flatten()
}

fn spawn_sidecar_writer(
    path: PathBuf,
    part_path: PathBuf,
) -> (
    tokio::sync::mpsc::UnboundedSender<ResumeSidecar>,
    JoinHandle<()>,
) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ResumeSidecar>();
    let handle = tokio::spawn(async move {
        while let Some(mut snapshot) = rx.recv().await {
            // Resume validation publishes a full snapshot after every newly
            // verified chunk. Persist only the latest queued snapshot so the
            // completion path does not stall flushing obsolete sidecar writes.
            while let Ok(newer_snapshot) = rx.try_recv() {
                snapshot = newer_snapshot;
            }
            snapshot.part_fingerprint = sync_and_fingerprint_part(&part_path).await;
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

struct RunningSidecarWriter {
    tx: tokio::sync::mpsc::UnboundedSender<ResumeSidecar>,
    handle: JoinHandle<()>,
}

struct LazySidecarWriter {
    path: PathBuf,
    part_path: PathBuf,
    running: Mutex<Option<RunningSidecarWriter>>,
}

impl LazySidecarWriter {
    const fn new(path: PathBuf, part_path: PathBuf) -> Self {
        Self {
            path,
            part_path,
            running: Mutex::new(None),
        }
    }

    fn send(&self, snapshot: ResumeSidecar) {
        let mut running = self.running.lock().unwrap();
        if running.is_none() {
            let (tx, handle) = spawn_sidecar_writer(self.path.clone(), self.part_path.clone());
            *running = Some(RunningSidecarWriter { tx, handle });
        }
        if let Some(writer) = running.as_ref() {
            let _ = writer.tx.send(snapshot);
        }
    }

    async fn finish(&self, shutdown: SidecarWriterShutdown) {
        let running = self.running.lock().unwrap().take();
        if let Some(RunningSidecarWriter { tx, handle }) = running {
            drop(tx);
            finish_sidecar_writer(&self.path, handle, shutdown).await;
        }
    }
}

enum SidecarWriterShutdown {
    Flush,
    Abort,
}

async fn finish_sidecar_writer(
    path: &Path,
    handle: JoinHandle<()>,
    shutdown: SidecarWriterShutdown,
) {
    match shutdown {
        SidecarWriterShutdown::Flush => {
            if let Err(err) = handle.await {
                log::warn!(
                    "Resume sidecar writer task failed for {}: {err}",
                    path.display()
                );
            }
        }
        SidecarWriterShutdown::Abort => {
            handle.abort();
            match handle.await {
                Ok(()) => {}
                Err(err) if err.is_cancelled() => {}
                Err(err) => {
                    log::warn!(
                        "Resume sidecar writer abort failed for {}: {err}",
                        path.display()
                    );
                }
            }
            let _ = remove_file_if_exists(&sidecar_tmp_path(path)).await;
        }
    }
}

fn expected_mac(node: &mega::Node) -> Result<[u8; 8]> {
    let mac = node
        .condensed_mac()
        .ok_or(mega::Error::MissingCondensedMac)?;
    Ok(*mac)
}

fn encode_expected_mac(node: &mega::Node) -> Result<String> {
    Ok(STANDARD.encode(expected_mac(node)?))
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

const REVALIDATION_BUFFER_BYTES: usize = 128 * 1024;

fn revalidation_buffer_len(remaining: u64) -> usize {
    usize::try_from(remaining.min(REVALIDATION_BUFFER_BYTES as u64))
        .expect("bounded revalidation read length fits usize")
}

fn resume_fingerprint_matches(expected: FileFingerprint, actual: FileFingerprint) -> bool {
    expected.len == actual.len
        && expected.modified_ns == actual.modified_ns
        && expected.dev == actual.dev
        && expected.ino == actual.ino
        && expected
            .allocated_bytes
            .is_none_or(|allocated| actual.allocated_bytes == Some(allocated))
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

struct ProgressCallbackState {
    name: String,
    stats: DownloadStatsTracker,
    cumulative: CumulativeProgress,
    progress: Arc<dyn DownloadProgress>,
}

impl ProgressCallbackState {
    fn new(
        name: String,
        expected_network_bytes: u64,
        trusted_bytes: u64,
        progress: Arc<dyn DownloadProgress>,
    ) -> Self {
        Self {
            name,
            stats: DownloadStatsTracker::new(expected_network_bytes),
            cumulative: CumulativeProgress::with_high_water(trusted_bytes),
            progress,
        }
    }

    fn record_cumulative(&self, cumulative_bytes: u64) {
        let delta = self.cumulative.delta(cumulative_bytes);
        if delta == 0 {
            return;
        }
        let _ = self.stats.record_bytes(delta);
        self.progress.on_progress(
            &self.name,
            ProgressDelta {
                total_bytes_delta: delta,
                network_bytes_delta: delta,
            },
        );
    }
}

struct ChunkVerifiedState {
    tracker: Mutex<ResumeTracker>,
    sidecar_writer: LazySidecarWriter,
}

impl ChunkVerifiedState {
    const fn new(tracker: ResumeTracker, sidecar_writer: LazySidecarWriter) -> Self {
        Self {
            tracker: Mutex::new(tracker),
            sidecar_writer,
        }
    }

    fn mark_verified(&self, index: u32, mac: [u8; 16]) {
        let snapshot = {
            let mut guard = self.tracker.lock().unwrap();
            guard.mark_verified(index, mac);
            guard.checkpoint_snapshot()
        };
        if let Some(snapshot) = snapshot {
            self.sidecar_writer.send(snapshot);
        }
    }

    async fn finish_sidecar_writer(&self, shutdown: SidecarWriterShutdown) {
        self.sidecar_writer.finish(shutdown).await;
    }
}

struct DownloadCallbackState {
    progress: ProgressCallbackState,
    chunk_verified: ChunkVerifiedState,
}

impl DownloadCallbackState {
    const fn new(progress: ProgressCallbackState, chunk_verified: ChunkVerifiedState) -> Self {
        Self {
            progress,
            chunk_verified,
        }
    }
}

impl mega::ParallelDownloadCallbacks for DownloadCallbackState {
    fn progress(&self, cumulative_bytes: u64) {
        self.progress.record_cumulative(cumulative_bytes);
    }

    fn chunk_verified(&self, index: u32, mac: [u8; 16]) {
        self.chunk_verified.mark_verified(index, mac);
    }
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
        progress: Option<(&str, &dyn DownloadProgress)>,
    ) -> Result<ResumeValidation> {
        let Some(sidecar) = load_sidecar(sidecar_path).await else {
            return Ok(ResumeValidation::empty(boundaries.len()));
        };
        let aes_iv = node.aes_iv().ok_or(mega::Error::MissingNodeAesIv)?;

        Ok(self
            .revalidate_sidecar_chunks(SidecarValidationInput {
                boundaries,
                part_path,
                sidecar: &sidecar,
                file_size: node.size(),
                expected_condensed_mac_b64,
                aes_key: node.aes_key(),
                aes_iv,
                progress,
            })
            .await)
    }

    async fn revalidate_candidate_from_part(
        &self,
        input: &SidecarValidationInput<'_>,
        candidate: TrustedResumeChunkCandidate,
        buffer: &mut [u8; REVALIDATION_BUFFER_BYTES],
    ) -> bool {
        let Some(boundary) = input.boundaries.get(candidate.index) else {
            return false;
        };
        let mut mac = mega::MegaChunkMac::new(input.aes_key, input.aes_iv);
        let mut offset = boundary.offset;
        let end = boundary.offset.saturating_add(boundary.length);
        let Ok(mut file) = std::fs::File::open(input.part_path) else {
            return false;
        };
        if file
            .seek(std::io::SeekFrom::Start(boundary.offset))
            .is_err()
        {
            return false;
        }

        while offset < end {
            let read_len = revalidation_buffer_len(end - offset);
            let read_buffer = &mut buffer[..read_len];
            if file.read_exact(read_buffer).is_err() {
                return false;
            }
            mac.update(read_buffer);
            if let Some((name, progress)) = input.progress {
                progress.on_progress(
                    name,
                    ProgressDelta {
                        total_bytes_delta: u64::try_from(read_len).unwrap_or(0),
                        network_bytes_delta: 0,
                    },
                );
            }
            offset = offset.saturating_add(u64::try_from(read_len).unwrap_or(0));
        }

        mac.finalize() == candidate.expected_mac
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
            log::debug!(
                "Resume sidecar rejected for {}: metadata mismatch version={} file_size={} expected_file_size={}",
                input.part_path.display(),
                input.sidecar.version,
                input.sidecar.file_size,
                input.file_size
            );
            return validation;
        }

        let part_size = self.fs.file_size(input.part_path).await.unwrap_or(0);
        let mut candidates = Vec::with_capacity(input.sidecar.verified_chunks.len());

        for record in &input.sidecar.verified_chunks {
            let Ok(index) = usize::try_from(record.index) else {
                continue;
            };
            let Some(boundary) = input.boundaries.get(index).copied() else {
                continue;
            };
            if boundary.offset.saturating_add(boundary.length) > part_size {
                continue;
            }
            let Ok(decoded) = STANDARD.decode(record.mac_b64.as_bytes()) else {
                continue;
            };
            let Ok(expected_mac) = <[u8; 16]>::try_from(decoded.as_slice()) else {
                continue;
            };

            candidates.push(TrustedResumeChunkCandidate {
                index,
                length: boundary.length,
                expected_mac,
            });
        }

        log::debug!(
            "Resume sidecar loaded for {}: records={} candidates={} part_size={} file_size={} fingerprint_present={}",
            input.part_path.display(),
            input.sidecar.verified_chunks.len(),
            candidates.len(),
            part_size,
            input.file_size,
            input.sidecar.part_fingerprint.is_some()
        );

        let mut seen_candidate_indexes = vec![false; input.boundaries.len()];
        let trusted_candidate_bytes = candidates
            .iter()
            .filter_map(|candidate| {
                let seen = seen_candidate_indexes.get_mut(candidate.index)?;
                if *seen {
                    return None;
                }
                *seen = true;
                Some(candidate.length)
            })
            .sum::<u64>();
        let part_fingerprint = input.sidecar.part_fingerprint;
        let Some(expected_fingerprint) = part_fingerprint else {
            log::debug!(
                "Resume sidecar for {} has no part fingerprint; falling back to disk revalidation",
                input.part_path.display()
            );
            return self
                .revalidate_candidates_from_part(input, candidates, trusted_candidate_bytes, None)
                .await;
        };
        let Some(actual_fingerprint) = self.fs.file_fingerprint(input.part_path).await else {
            log::debug!(
                "Resume sidecar for {} could not fingerprint part file; falling back to disk revalidation",
                input.part_path.display()
            );
            return self
                .revalidate_candidates_from_part(
                    input,
                    candidates,
                    trusted_candidate_bytes,
                    expected_fingerprint.allocated_bytes,
                )
                .await;
        };
        let fingerprint_matches =
            resume_fingerprint_matches(expected_fingerprint, actual_fingerprint);
        if !fingerprint_matches {
            log::debug!(
                "Resume sidecar for {} has stale part fingerprint; falling back to disk revalidation: expected={expected_fingerprint:?} actual={actual_fingerprint:?}",
                input.part_path.display()
            );
        }

        let allocation_covers_candidates = expected_fingerprint
            .allocated_bytes
            .is_some_and(|allocated| allocated >= trusted_candidate_bytes);

        if fingerprint_matches && allocation_covers_candidates {
            for candidate in candidates {
                trust_resume_candidate(&mut validation, candidate);
            }
            if validation.trusted_bytes > 0
                && let Some((name, progress)) = input.progress
            {
                progress.on_progress(
                    name,
                    ProgressDelta {
                        total_bytes_delta: validation.trusted_bytes,
                        network_bytes_delta: 0,
                    },
                );
            }
            log::debug!(
                "Resume sidecar fast-trusted for {}: chunks={} bytes={}",
                input.part_path.display(),
                validation.trusted_count,
                validation.trusted_bytes
            );
        } else {
            validation = self
                .revalidate_candidates_from_part(
                    input,
                    candidates,
                    trusted_candidate_bytes,
                    expected_fingerprint.allocated_bytes,
                )
                .await;
        }

        if validation.trusted_count > 0 {
            validation.source = Some(ResumeReuseSource::Sidecar);
        }

        validation
    }

    async fn revalidate_candidates_from_part(
        &self,
        input: SidecarValidationInput<'_>,
        candidates: Vec<TrustedResumeChunkCandidate>,
        trusted_candidate_bytes: u64,
        allocated_bytes: Option<u64>,
    ) -> ResumeValidation {
        let mut validation = ResumeValidation {
            sidecar_loaded: true,
            ..ResumeValidation::empty(input.boundaries.len())
        };
        log::debug!(
            "Resume sidecar for {} needs disk revalidation: candidate_bytes={} allocated_bytes={allocated_bytes:?}",
            input.part_path.display(),
            trusted_candidate_bytes
        );
        let mut buffer = [0; REVALIDATION_BUFFER_BYTES];
        let mut revalidated = 0usize;
        let mut rejected = 0usize;
        for candidate in candidates {
            if validation.trusted_chunks[candidate.index].is_some() {
                continue;
            }
            if self
                .revalidate_candidate_from_part(&input, candidate, &mut buffer)
                .await
            {
                revalidated = revalidated.saturating_add(1);
                trust_resume_candidate(&mut validation, candidate);
            } else {
                rejected = rejected.saturating_add(1);
            }
        }
        log::debug!(
            "Resume sidecar disk revalidation finished for {}: trusted={} rejected={} bytes={}",
            input.part_path.display(),
            revalidated,
            rejected,
            validation.trusted_bytes
        );

        if validation.trusted_count > 0 {
            validation.source = Some(ResumeReuseSource::Sidecar);
        }

        validation
    }

    /// Revalidates resumable chunk state for a file without downloading new data.
    ///
    /// # Errors
    ///
    /// Returns an error when the remote node is missing required MAC metadata.
    pub async fn reverify_resume_file(
        &self,
        node: &mega::Node,
        path: &str,
    ) -> Result<ResumeReverify> {
        self.reverify_resume_file_with_progress(node, path, None)
            .await
    }

    pub async fn reverify_resume_file_with_progress(
        &self,
        node: &mega::Node,
        path: &str,
        progress: Option<&dyn DownloadProgress>,
    ) -> Result<ResumeReverify> {
        let pp = part_path(path);
        let sp = sidecar_path(path);
        let expected_condensed_mac_b64 = encode_expected_mac(node)?;
        let boundaries = mega::mega_chunk_boundaries(node.size());
        let validation = self
            .revalidate_resume_chunks(
                node,
                &boundaries,
                &pp,
                &sp,
                &expected_condensed_mac_b64,
                progress.map(|progress| (path, progress)),
            )
            .await?;
        Ok(ResumeReverify {
            sidecar_loaded: validation.sidecar_loaded,
            chunks: validation.trusted_count,
            bytes: validation.trusted_bytes,
        })
    }

    /// Verifies the completed destination file against the remote node MAC.
    ///
    /// # Errors
    ///
    /// Returns an error when the file is missing, has the wrong size, or its
    /// computed MEGA condensed MAC does not match the node.
    pub async fn verify_completed_file(
        &self,
        node: &mega::Node,
        path: &str,
    ) -> Result<CompletedFileVerify> {
        self.verify_completed_file_with_progress(node, path, None)
            .await
    }

    pub async fn verify_completed_file_with_progress(
        &self,
        node: &mega::Node,
        path: &str,
        progress: Option<&dyn DownloadProgress>,
    ) -> Result<CompletedFileVerify> {
        let final_path = Path::new(path);
        let size = self.fs.file_size(final_path).await.ok_or_else(|| {
            let mut message =
                String::with_capacity("Completed file is missing: ".len() + path.len());
            message.push_str("Completed file is missing: ");
            message.push_str(path);
            Error::Download(message)
        })?;
        if size != node.size() {
            let mut message = String::with_capacity(
                "Completed file size mismatch for : local= remote=".len() + path.len() + 40,
            );
            let _ = write!(
                message,
                "Completed file size mismatch for {path}: local={size} remote={}",
                node.size()
            );
            return Err(Error::Download(message));
        }

        let aes_iv = node.aes_iv().ok_or(mega::Error::MissingNodeAesIv)?;
        let expected_mac = *node
            .condensed_mac()
            .ok_or(mega::Error::MissingCondensedMac)?;
        let actual_mac = self
            .compute_completed_file_mac(final_path, node, *aes_iv, progress.map(|p| (path, p)))
            .await?;
        if actual_mac != expected_mac {
            return Err(Error::Mega(mega::Error::CondensedMacMismatch));
        }

        Ok(CompletedFileVerify { bytes: size })
    }

    async fn compute_completed_file_mac(
        &self,
        final_path: &Path,
        node: &mega::Node,
        aes_iv: [u8; 8],
        progress: Option<(&str, &dyn DownloadProgress)>,
    ) -> Result<[u8; 8]> {
        if node.size() == 0 {
            let file = tokio::fs::File::open(final_path).await?;
            return Ok(mega::compute_condensed_mac(
                file.compat(),
                node.size(),
                node.aes_key(),
                &aes_iv,
            )
            .await?);
        }

        let processor = mega::ParallelMacProcessor::new(node.size(), node.aes_key(), &aes_iv);
        let boundaries = mega::mega_chunk_boundaries(node.size());
        let mut file = std::fs::File::open(final_path)?;
        let mut buffer = [0; REVALIDATION_BUFFER_BYTES];
        for boundary in boundaries {
            let mut mac = mega::MegaChunkMac::new(node.aes_key(), &aes_iv);
            let mut offset = boundary.offset;
            let end = boundary.offset.saturating_add(boundary.length);
            file.seek(std::io::SeekFrom::Start(boundary.offset))?;
            while offset < end {
                let read_len = revalidation_buffer_len(end - offset);
                let read_buffer = &mut buffer[..read_len];
                file.read_exact(read_buffer)?;
                mac.update(read_buffer);
                if let Some((name, progress)) = progress {
                    progress.on_progress(
                        name,
                        ProgressDelta {
                            total_bytes_delta: u64::try_from(read_len).unwrap_or(0),
                            network_bytes_delta: 0,
                        },
                    );
                }
                offset = offset.saturating_add(u64::try_from(read_len).unwrap_or(0));
            }
            let index = usize::try_from(boundary.index)
                .map_err(|_| Error::Download("MEGA chunk index overflow".to_string()))?;
            if !processor.set_chunk_mac(index, mac.finalize()) {
                let mut message = String::with_capacity(64);
                let _ = write!(
                    message,
                    "MEGA chunk index {index} outside completed file MAC processor"
                );
                return Err(Error::Download(message));
            }
        }
        processor
            .finalize()
            .ok_or_else(|| Error::Download("Completed file MAC missing chunk data".to_string()))
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
        progress.on_file_start(path, node.size());

        let pp = part_path(path);
        let sp = sidecar_path(path);
        let expected_condensed_mac = expected_mac(node)?;
        let boundaries = mega::mega_chunk_boundaries(node.size());
        log::debug!(
            "Download resume setup for {path}: size={} trust_resume_state={} force_overwrite={} part={} sidecar={} chunks={}",
            node.size(),
            trust_resume_state,
            self.config.force_overwrite,
            pp.display(),
            sp.display(),
            boundaries.len()
        );
        let reuse_resume_state =
            should_reuse_resume_state(self.config.force_overwrite, trust_resume_state);
        let resume_validation = if reuse_resume_state {
            let expected_condensed_mac_b64 = STANDARD.encode(expected_condensed_mac);
            self.revalidate_resume_chunks(
                node,
                &boundaries,
                &pp,
                &sp,
                &expected_condensed_mac_b64,
                None,
            )
            .await?
        } else {
            ResumeValidation::empty(boundaries.len())
        };
        log::debug!(
            "Download resume validation for {path}: sidecar_loaded={} trusted_chunks={} trusted_bytes={} source={:?}",
            resume_validation.sidecar_loaded,
            resume_validation.trusted_count,
            resume_validation.trusted_bytes,
            resume_validation.source
        );
        let preserve_existing = resume_validation.trusted_count > 0;
        if !preserve_existing {
            let _ = delete_sidecar(&sp).await;
        }
        if resume_validation.sidecar_loaded && resume_validation.trusted_count == 0 {
            log::debug!("Resume sidecar found for {path}, but no chunks were reusable");
        }
        let trusted_bytes = resume_validation.trusted_bytes;

        if trusted_bytes > 0 {
            progress.on_resume_reused(path, resume_validation.trusted_count, trusted_bytes);
        }

        // Open the plaintext .part file, preserving only locally revalidated chunks.
        let file = self
            .fs
            .open_part_file(&pp, node.size(), preserve_existing)
            .await?;

        let trusted_for_download: Arc<[Option<[u8; 16]>]> =
            resume_validation.trusted_chunks.clone().into();
        let callback_state = Arc::new(DownloadCallbackState::new(
            ProgressCallbackState::new(
                path.to_string(),
                node.size().saturating_sub(trusted_bytes),
                trusted_bytes,
                Arc::clone(progress),
            ),
            ChunkVerifiedState::new(
                ResumeTracker::new(
                    node.size(),
                    expected_condensed_mac,
                    resume_validation.trusted_chunks,
                ),
                LazySidecarWriter::new(sp.clone(), pp.clone()),
            ),
        ));
        let callbacks: Arc<dyn mega::ParallelDownloadCallbacks> = callback_state.clone();

        // Download with progress callback, optionally with cancellation support
        let download_result = if let Some(token) = cancellation_token {
            let download_fut = self
                .client
                .download_node_parallel_resumable_to_file_with_callbacks(
                    node,
                    file,
                    self.config.chunks_per_file,
                    Arc::clone(&trusted_for_download),
                    Some(callbacks),
                );
            tokio::select! {
                res = download_fut => res.map_err(Error::Mega),
                () = token.cancelled() => {
                    Err(Error::Cancelled)
                }
            }
        } else {
            self.client
                .download_node_parallel_resumable_to_file_with_callbacks(
                    node,
                    file,
                    self.config.chunks_per_file,
                    trusted_for_download,
                    Some(callbacks),
                )
                .await
                .map_err(Error::Mega)
        };
        let sidecar_shutdown = if download_result.is_ok() {
            SidecarWriterShutdown::Abort
        } else {
            SidecarWriterShutdown::Flush
        };
        callback_state
            .chunk_verified
            .finish_sidecar_writer(sidecar_shutdown)
            .await;
        self.finish_download_result(
            DownloadFinishContext {
                node,
                path,
                part_path: &pp,
                sidecar_path: &sp,
                reused_bytes: trusted_bytes,
                stats: &callback_state.progress.stats,
                tracker: &callback_state.chunk_verified.tracker,
                progress,
                name: path,
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
                            let mut snapshot = ctx.tracker.lock().unwrap().snapshot();
                            snapshot.part_fingerprint =
                                self.fs.file_fingerprint(ctx.part_path).await;
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

        let mut peak_speed = 0;
        let mut downloads = stream::iter(files)
            .map(|item| async move {
                self.download_file(item.node, &item.path, progress, false, None)
                    .await
            })
            .buffer_unordered(self.config.concurrent_files);

        while let Some(result) = downloads.next().await {
            match result {
                Ok(file_stats) => {
                    peak_speed = peak_speed.max(file_stats.peak_speed);
                    builder.add_download(&file_stats);
                }
                Err(e) => {
                    log::error!("Download failed: {e}");
                }
            }
        }
        builder.set_peak_speed(peak_speed);

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

        let mut peak_speed = 0;
        let mut downloads = stream::iter(files)
            .map(|item| async move {
                self.download_file(&item.node, &item.path, progress, false, None)
                    .await
            })
            .buffer_unordered(self.config.concurrent_files);

        while let Some(result) = downloads.next().await {
            match result {
                Ok(file_stats) => {
                    peak_speed = peak_speed.max(file_stats.peak_speed);
                    builder.add_download(&file_stats);
                }
                Err(e) => {
                    log::error!("Download failed: {e}");
                }
            }
        }
        builder.set_peak_speed(peak_speed);

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
    roots
        .iter()
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
    let package_name = stemmed_package_name(file_name);
    let mut path = String::with_capacity(package_name.len() + file_name.len() + 1);
    path.push_str(&package_name);
    path.push('/');
    path.push_str(file_name);
    path
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

    static TEST_AES_KEY: [u8; 16] = [7u8; 16];
    static TEST_AES_IV: [u8; 8] = [3u8; 8];

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
        /// Maps path → file fingerprint.
        fingerprints: Mutex<HashMap<PathBuf, FileFingerprint>>,
    }

    impl MockFileSystem {
        fn new() -> Self {
            Self {
                files: Mutex::new(HashMap::new()),
                fingerprints: Mutex::new(HashMap::new()),
            }
        }

        fn add_file(&self, path: impl Into<PathBuf>, size: u64) {
            self.files.lock().unwrap().insert(path.into(), size);
        }

        fn add_fingerprint(&self, path: impl Into<PathBuf>, fingerprint: FileFingerprint) {
            self.fingerprints
                .lock()
                .unwrap()
                .insert(path.into(), fingerprint);
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

        async fn file_fingerprint(&self, path: &Path) -> Option<FileFingerprint> {
            self.fingerprints.lock().unwrap().get(path).copied()
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

    fn test_incompressible_plaintext(size: usize) -> Vec<u8> {
        let mut state = 0x1234_5678_9abc_def0_u64;
        (0..size)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    fn usize_from_u64(value: u64) -> usize {
        usize::try_from(value).unwrap()
    }

    fn usize_from_u32(value: u32) -> usize {
        usize::try_from(value).unwrap()
    }

    fn chunk_data<'a>(data: &'a [u8], chunk: &mega::MegaChunk) -> &'a [u8] {
        &data[usize_from_u64(chunk.offset)..usize_from_u64(chunk.offset + chunk.length)]
    }

    fn fingerprint_with_allocated_bytes(len: u64, allocated_bytes: Option<u64>) -> FileFingerprint {
        FileFingerprint {
            len,
            modified_ns: 42,
            allocated_bytes,
            dev: Some(7),
            ino: Some(9),
        }
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
            part_fingerprint: None,
        }
    }

    #[derive(Default)]
    struct RecordingProgress {
        total: std::sync::atomic::AtomicU64,
        network: std::sync::atomic::AtomicU64,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl DownloadProgress for RecordingProgress {
        fn on_progress(&self, _name: &str, delta: ProgressDelta) {
            self.total
                .fetch_add(delta.total_bytes_delta, std::sync::atomic::Ordering::SeqCst);
            self.network.fetch_add(
                delta.network_bytes_delta,
                std::sync::atomic::Ordering::SeqCst,
            );
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[test]
    fn resume_progress_high_water_ignores_initial_trusted_callback() {
        let trusted_bytes = 1_024;
        let cumulative = CumulativeProgress::with_high_water(trusted_bytes);

        assert_eq!(cumulative.delta(trusted_bytes), 0);
    }

    #[test]
    fn resume_progress_after_high_water_counts_fresh_bytes_as_network() {
        let trusted_bytes = 1_024;
        let cumulative = CumulativeProgress::with_high_water(trusted_bytes);

        let delta = cumulative.delta(trusted_bytes + 512);

        assert_eq!(delta, 512);
    }

    #[test]
    fn resume_progress_ignores_duplicate_or_out_of_order_totals() {
        let trusted_bytes = 1_024;
        let cumulative = CumulativeProgress::with_high_water(trusted_bytes);

        assert_eq!(cumulative.delta(trusted_bytes), 0);
        assert_eq!(cumulative.delta(trusted_bytes - 1), 0);
        assert_eq!(cumulative.delta(trusted_bytes + 256), 256);
        assert_eq!(cumulative.delta(trusted_bytes + 128), 0);
        assert_eq!(cumulative.delta(trusted_bytes + 512), 256);
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
    async fn revalidate_sidecar_without_part_fingerprint_recomputes_from_part() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("file.bin.part");
        let file_size = 300_000_u64;
        let data = test_incompressible_plaintext(usize_from_u64(file_size));
        tokio::fs::write(&part, &data).await.unwrap();

        let expected = STANDARD.encode([9u8; 8]);
        let boundaries = mega::mega_chunk_boundaries(file_size);
        let first = &boundaries[0];
        let mac =
            mega::compute_mega_chunk_mac(chunk_data(&data, first), &TEST_AES_KEY, &TEST_AES_IV);
        let sidecar = sidecar_for_chunk(file_size, &expected, first.index, mac);

        let validation = tokio_downloader()
            .revalidate_sidecar_chunks(SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac_b64: &expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            })
            .await;

        assert!(validation.sidecar_loaded);
        assert_eq!(validation.trusted_count, 1);
        assert_eq!(validation.trusted_bytes, first.length);
        assert_eq!(validation.trusted_chunks[0], Some(mac));
        assert!(validation.trusted_chunks[1].is_none());
    }

    #[tokio::test]
    async fn revalidate_sidecar_trusts_matching_part_fingerprint_without_reread() {
        let part = PathBuf::from("file.bin.part");
        let file_size = 300_000_u64;

        let expected = STANDARD.encode([9u8; 8]);
        let boundaries = mega::mega_chunk_boundaries(file_size);
        let first = &boundaries[0];
        let mut sidecar = sidecar_for_chunk(file_size, &expected, first.index, [4u8; 16]);
        let fingerprint = fingerprint_with_allocated_bytes(file_size, Some(first.length));
        sidecar.part_fingerprint = Some(fingerprint);
        let fs = MockFileSystem::new();
        fs.add_file(&part, file_size);
        fs.add_fingerprint(&part, fingerprint);

        let validation = mock_downloader(fs)
            .revalidate_sidecar_chunks(SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac_b64: &expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            })
            .await;

        assert_eq!(validation.trusted_count, 1);
        assert_eq!(validation.trusted_bytes, first.length);
        assert_eq!(validation.trusted_chunks[0], Some([4u8; 16]));
    }

    #[tokio::test]
    async fn revalidate_sidecar_rejects_matching_fingerprint_without_allocated_bytes() {
        let part = PathBuf::from("file.bin.part");
        let file_size = 300_000_u64;
        let expected = STANDARD.encode([9u8; 8]);
        let boundaries = mega::mega_chunk_boundaries(file_size);
        let first = &boundaries[0];
        let mut sidecar = sidecar_for_chunk(file_size, &expected, first.index, [4u8; 16]);
        let fingerprint = fingerprint_with_allocated_bytes(file_size, None);
        sidecar.part_fingerprint = Some(fingerprint);
        let fs = MockFileSystem::new();
        fs.add_file(&part, file_size);
        fs.add_fingerprint(&part, fingerprint);

        let validation = mock_downloader(fs)
            .revalidate_sidecar_chunks(SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac_b64: &expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            })
            .await;

        assert_eq!(validation.trusted_count, 0);
        assert_eq!(validation.trusted_bytes, 0);
        assert!(validation.trusted_chunks.iter().all(Option::is_none));
    }

    #[tokio::test]
    async fn revalidate_sidecar_recomputes_old_fingerprint_chunk_from_part() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("file.bin.part");
        let file_size = 300_000_u64;
        let data = test_incompressible_plaintext(usize_from_u64(file_size));
        tokio::fs::write(&part, &data).await.unwrap();

        let expected = STANDARD.encode([9u8; 8]);
        let boundaries = mega::mega_chunk_boundaries(file_size);
        let first = &boundaries[0];
        let mac =
            mega::compute_mega_chunk_mac(chunk_data(&data, first), &TEST_AES_KEY, &TEST_AES_IV);
        let mut sidecar = sidecar_for_chunk(file_size, &expected, first.index, mac);
        sidecar.part_fingerprint = TokioFileSystem::new().file_fingerprint(&part).await;
        if let Some(fingerprint) = sidecar.part_fingerprint.as_mut() {
            fingerprint.allocated_bytes = None;
        }

        let validation = tokio_downloader()
            .revalidate_sidecar_chunks(SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac_b64: &expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            })
            .await;

        assert_eq!(validation.trusted_count, 1);
        assert_eq!(validation.trusted_bytes, first.length);
        assert_eq!(
            validation.trusted_chunks[usize_from_u32(first.index)],
            Some(mac)
        );
        assert_eq!(validation.source, Some(ResumeReuseSource::Sidecar));
    }

    #[tokio::test]
    async fn revalidate_sidecar_recomputes_chunk_index_at_its_offset() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("file.bin.part");
        let file_size = 300_000_u64;
        let mut data = test_incompressible_plaintext(usize_from_u64(file_size));
        data[..32].fill(0);
        tokio::fs::write(&part, &data).await.unwrap();

        let expected = STANDARD.encode([9u8; 8]);
        let boundaries = mega::mega_chunk_boundaries(file_size);
        let second = &boundaries[1];
        let mac =
            mega::compute_mega_chunk_mac(chunk_data(&data, second), &TEST_AES_KEY, &TEST_AES_IV);
        let mut sidecar = sidecar_for_chunk(file_size, &expected, second.index, mac);
        sidecar.part_fingerprint = TokioFileSystem::new().file_fingerprint(&part).await;
        if let Some(fingerprint) = sidecar.part_fingerprint.as_mut() {
            fingerprint.allocated_bytes = None;
        }

        let validation = tokio_downloader()
            .revalidate_sidecar_chunks(SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac_b64: &expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            })
            .await;

        assert_eq!(validation.trusted_count, 1);
        assert_eq!(validation.trusted_bytes, second.length);
        assert_eq!(validation.trusted_chunks[0], None);
        assert_eq!(
            validation.trusted_chunks[usize_from_u32(second.index)],
            Some(mac)
        );
    }

    #[tokio::test]
    async fn revalidate_sidecar_rejects_matching_fingerprint_with_insufficient_allocation() {
        let part = PathBuf::from("file.bin.part");
        let file_size = 300_000_u64;
        let expected = STANDARD.encode([9u8; 8]);
        let boundaries = mega::mega_chunk_boundaries(file_size);
        let first = &boundaries[0];
        let second = &boundaries[1];
        let mut sidecar = ResumeSidecar {
            version: CURRENT_RESUME_SIDECAR_VERSION,
            file_size,
            expected_condensed_mac_b64: expected.clone(),
            verified_chunks: vec![
                VerifiedChunkRecord {
                    index: first.index,
                    mac_b64: STANDARD.encode([4u8; 16]),
                },
                VerifiedChunkRecord {
                    index: second.index,
                    mac_b64: STANDARD.encode([5u8; 16]),
                },
            ],
            part_fingerprint: None,
        };
        let allocated = first.length.saturating_add(second.length).saturating_sub(1);
        let fingerprint = fingerprint_with_allocated_bytes(file_size, Some(allocated));
        sidecar.part_fingerprint = Some(fingerprint);
        let fs = MockFileSystem::new();
        fs.add_file(&part, file_size);
        fs.add_fingerprint(&part, fingerprint);

        let validation = mock_downloader(fs)
            .revalidate_sidecar_chunks(SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac_b64: &expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            })
            .await;

        assert_eq!(validation.trusted_count, 0);
        assert_eq!(validation.trusted_bytes, 0);
        assert!(validation.trusted_chunks.iter().all(Option::is_none));
    }

    #[tokio::test]
    async fn revalidate_sidecar_trusts_matching_fingerprint_with_sufficient_allocation() {
        let part = PathBuf::from("file.bin.part");
        let file_size = 300_000_u64;
        let expected = STANDARD.encode([9u8; 8]);
        let boundaries = mega::mega_chunk_boundaries(file_size);
        let first = &boundaries[0];
        let mut sidecar = sidecar_for_chunk(file_size, &expected, first.index, [4u8; 16]);
        let fingerprint = fingerprint_with_allocated_bytes(file_size, Some(first.length));
        sidecar.part_fingerprint = Some(fingerprint);
        let fs = MockFileSystem::new();
        fs.add_file(&part, file_size);
        fs.add_fingerprint(&part, fingerprint);

        let validation = mock_downloader(fs)
            .revalidate_sidecar_chunks(SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac_b64: &expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            })
            .await;

        assert_eq!(validation.trusted_count, 1);
        assert_eq!(validation.trusted_bytes, first.length);
        assert_eq!(validation.trusted_chunks[0], Some([4u8; 16]));
    }

    #[tokio::test]
    async fn revalidate_sidecar_reports_progress_for_fast_trusted_bytes() {
        let part = PathBuf::from("file.bin.part");
        let file_size = 300_000_u64;
        let expected = STANDARD.encode([9u8; 8]);
        let boundaries = mega::mega_chunk_boundaries(file_size);
        let first = &boundaries[0];
        let mut sidecar = sidecar_for_chunk(file_size, &expected, first.index, [4u8; 16]);
        let fingerprint = fingerprint_with_allocated_bytes(file_size, Some(first.length));
        sidecar.part_fingerprint = Some(fingerprint);
        let fs = MockFileSystem::new();
        fs.add_file(&part, file_size);
        fs.add_fingerprint(&part, fingerprint);
        let progress = RecordingProgress::default();

        let validation = mock_downloader(fs)
            .revalidate_sidecar_chunks(SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac_b64: &expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: Some(("file.bin", &progress)),
            })
            .await;

        assert_eq!(validation.trusted_count, 1);
        assert_eq!(validation.trusted_bytes, first.length);
        assert_eq!(
            progress.total.load(std::sync::atomic::Ordering::SeqCst),
            first.length
        );
        assert_eq!(
            progress.network.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(progress.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn revalidate_sidecar_reports_progress_for_disk_revalidation_reads() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("file.bin.part");
        let file_size = 131_072;
        let data = test_incompressible_plaintext(usize_from_u64(file_size));
        tokio::fs::write(&part, &data).await.unwrap();

        let expected = STANDARD.encode([9u8; 8]);
        let boundaries = mega::mega_chunk_boundaries(file_size);
        let first = &boundaries[0];
        let mac =
            mega::compute_mega_chunk_mac(chunk_data(&data, first), &TEST_AES_KEY, &TEST_AES_IV);
        let sidecar = sidecar_for_chunk(file_size, &expected, first.index, mac);
        let progress = RecordingProgress::default();

        let validation = tokio_downloader()
            .revalidate_sidecar_chunks(SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac_b64: &expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: Some(("file.bin", &progress)),
            })
            .await;

        assert_eq!(validation.trusted_count, 1);
        assert_eq!(validation.trusted_bytes, first.length);
        assert_eq!(
            progress.total.load(std::sync::atomic::Ordering::SeqCst),
            first.length
        );
        assert_eq!(
            progress.network.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert!(progress.calls.load(std::sync::atomic::Ordering::SeqCst) > 0);
    }

    #[tokio::test]
    async fn revalidate_sidecar_rejects_full_length_sparse_part_without_allocation_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("file.bin.part");
        let file_size = 300_000_u64;
        let data = test_plaintext(usize_from_u64(file_size));
        let file = tokio::fs::File::create(&part).await.unwrap();
        file.set_len(file_size).await.unwrap();
        drop(file);
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&part)
            .await
            .unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut file, &data[..1024])
            .await
            .unwrap();
        drop(file);

        let expected = STANDARD.encode([9u8; 8]);
        let boundaries = mega::mega_chunk_boundaries(file_size);
        let first = &boundaries[0];
        let mut sidecar = sidecar_for_chunk(file_size, &expected, first.index, [4u8; 16]);
        sidecar.part_fingerprint = TokioFileSystem::new().file_fingerprint(&part).await;
        if let Some(fingerprint) = sidecar.part_fingerprint.as_mut() {
            fingerprint.allocated_bytes = None;
        }

        let validation = tokio_downloader()
            .revalidate_sidecar_chunks(SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac_b64: &expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            })
            .await;

        assert!(validation.sidecar_loaded);
        assert_eq!(validation.trusted_count, 0);
        assert_eq!(validation.trusted_bytes, 0);
        assert!(validation.trusted_chunks.iter().all(Option::is_none));
    }

    #[tokio::test]
    async fn revalidate_sidecar_trusts_nothing_when_part_fingerprint_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("file.bin.part");
        let file_size = 300_000_u64;
        let data = test_plaintext(usize_from_u64(file_size));
        tokio::fs::write(&part, &data).await.unwrap();
        let mut stale_fingerprint = TokioFileSystem::new().file_fingerprint(&part).await;
        if let Some(fingerprint) = stale_fingerprint.as_mut() {
            fingerprint.len = fingerprint.len.saturating_add(1);
        }
        let mut changed = data.clone();
        changed[0] ^= 0xff;
        tokio::fs::write(&part, &changed).await.unwrap();

        let aes_key = [7u8; 16];
        let aes_iv = [3u8; 8];
        let expected = STANDARD.encode([9u8; 8]);
        let boundaries = mega::mega_chunk_boundaries(file_size);
        let first = &boundaries[0];
        let first_data = chunk_data(&data, first);
        let mac = mega::compute_mega_chunk_mac(first_data, &aes_key, &aes_iv);
        let mut sidecar = sidecar_for_chunk(file_size, &expected, first.index, mac);
        sidecar.part_fingerprint = stale_fingerprint;

        let validation = tokio_downloader()
            .revalidate_sidecar_chunks(SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac_b64: &expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            })
            .await;

        assert_eq!(validation.trusted_count, 0);
        assert_eq!(validation.trusted_bytes, 0);
        assert!(validation.trusted_chunks.iter().all(Option::is_none));
    }

    #[tokio::test]
    async fn revalidate_sidecar_recomputes_when_later_writes_stale_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("file.bin.part");
        let file_size = 300_000_u64;
        let mut data = test_incompressible_plaintext(usize_from_u64(file_size));
        tokio::fs::write(&part, &data).await.unwrap();
        let stale_fingerprint = TokioFileSystem::new().file_fingerprint(&part).await;

        let expected = STANDARD.encode([9u8; 8]);
        let boundaries = mega::mega_chunk_boundaries(file_size);
        let first = &boundaries[0];
        let second = &boundaries[1];
        let mac =
            mega::compute_mega_chunk_mac(chunk_data(&data, first), &TEST_AES_KEY, &TEST_AES_IV);
        let mut sidecar = sidecar_for_chunk(file_size, &expected, first.index, mac);
        sidecar.part_fingerprint = stale_fingerprint;

        let second_start = usize_from_u64(second.offset);
        data[second_start] ^= 0xff;
        tokio::fs::write(&part, &data).await.unwrap();

        let validation = tokio_downloader()
            .revalidate_sidecar_chunks(SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac_b64: &expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            })
            .await;

        assert_eq!(validation.trusted_count, 1);
        assert_eq!(validation.trusted_bytes, first.length);
        assert_eq!(validation.trusted_chunks[0], Some(mac));
    }

    #[tokio::test]
    async fn revalidate_sidecar_with_matching_fingerprint_keeps_first_duplicate_chunk() {
        let part = PathBuf::from("file.bin.part");
        let file_size = 300_000_u64;
        let data = test_plaintext(usize_from_u64(file_size));

        let aes_key = [7u8; 16];
        let aes_iv = [3u8; 8];
        let expected = STANDARD.encode([9u8; 8]);
        let boundaries = mega::mega_chunk_boundaries(file_size);
        let first = &boundaries[0];
        let first_data = chunk_data(&data, first);
        let mac = mega::compute_mega_chunk_mac(first_data, &aes_key, &aes_iv);
        let mut sidecar = ResumeSidecar {
            version: CURRENT_RESUME_SIDECAR_VERSION,
            file_size,
            expected_condensed_mac_b64: expected.clone(),
            verified_chunks: vec![
                VerifiedChunkRecord {
                    index: first.index,
                    mac_b64: STANDARD.encode([1u8; 16]),
                },
                VerifiedChunkRecord {
                    index: first.index,
                    mac_b64: STANDARD.encode(mac),
                },
            ],
            part_fingerprint: None,
        };
        let fingerprint = fingerprint_with_allocated_bytes(file_size, Some(first.length));
        sidecar.part_fingerprint = Some(fingerprint);
        let fs = MockFileSystem::new();
        fs.add_file(&part, file_size);
        fs.add_fingerprint(&part, fingerprint);

        let validation = mock_downloader(fs)
            .revalidate_sidecar_chunks(SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac_b64: &expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
            })
            .await;

        assert_eq!(validation.trusted_count, 1);
        assert_eq!(validation.trusted_bytes, first.length);
        assert_eq!(validation.trusted_chunks[0], Some([1u8; 16]));
    }

    #[tokio::test]
    async fn revalidate_sidecar_rejects_bad_chunk_mac() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("file.bin.part");
        let file_size = 300_000_u64;
        let data = test_plaintext(usize_from_u64(file_size));
        tokio::fs::write(&part, &data).await.unwrap();

        let expected = STANDARD.encode([9u8; 8]);
        let boundaries = mega::mega_chunk_boundaries(file_size);
        let sidecar = sidecar_for_chunk(file_size, &expected, 0, [1u8; 16]);

        let validation = tokio_downloader()
            .revalidate_sidecar_chunks(SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac_b64: &expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
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
                expected_condensed_mac_b64: &expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
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

        let expected = STANDARD.encode([9u8; 8]);
        let boundaries = mega::mega_chunk_boundaries(file_size);
        let sidecar = sidecar_for_chunk(file_size, "stale", 0, [1u8; 16]);

        let validation = tokio_downloader()
            .revalidate_sidecar_chunks(SidecarValidationInput {
                boundaries: &boundaries,
                part_path: &part,
                sidecar: &sidecar,
                file_size,
                expected_condensed_mac_b64: &expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
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
                part_fingerprint: None,
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
                expected_condensed_mac_b64: &expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
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
                part_fingerprint: None,
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
                expected_condensed_mac_b64: &expected,
                aes_key: &TEST_AES_KEY,
                aes_iv: &TEST_AES_IV,
                progress: None,
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
            part_fingerprint: None,
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
                part_fingerprint: None,
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
                part_fingerprint: None,
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
        let part_path = dir.path().join("file.bin.part");
        tokio::fs::write(&part_path, b"partial").await.unwrap();
        let first = ResumeSidecar {
            version: CURRENT_RESUME_SIDECAR_VERSION,
            file_size: 42,
            expected_condensed_mac_b64: STANDARD.encode([9u8; 8]),
            verified_chunks: vec![VerifiedChunkRecord {
                index: 0,
                mac_b64: STANDARD.encode([1u8; 16]),
            }],
            part_fingerprint: None,
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

        let (tx, handle) = spawn_sidecar_writer(sidecar_path.clone(), part_path.clone());
        tx.send(first).unwrap();
        tx.send(second.clone()).unwrap();
        drop(tx);
        finish_sidecar_writer(&sidecar_path, handle, SidecarWriterShutdown::Flush).await;

        let loaded = load_sidecar(&sidecar_path).await.unwrap();
        assert_eq!(loaded.verified_chunks, second.verified_chunks);
        assert_eq!(
            loaded.part_fingerprint,
            TokioFileSystem::new().file_fingerprint(&part_path).await
        );
    }

    #[tokio::test]
    async fn sidecar_writer_saves_snapshot_without_fingerprint_when_part_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let sidecar_path = dir.path().join("file.bin.part.meta.json");
        let part_path = dir.path().join("file.bin.part");
        let snapshot = ResumeSidecar {
            version: CURRENT_RESUME_SIDECAR_VERSION,
            file_size: 42,
            expected_condensed_mac_b64: STANDARD.encode([9u8; 8]),
            verified_chunks: vec![VerifiedChunkRecord {
                index: 0,
                mac_b64: STANDARD.encode([1u8; 16]),
            }],
            part_fingerprint: Some(FileFingerprint {
                len: 999,
                modified_ns: 999,
                allocated_bytes: Some(999),
                dev: None,
                ino: None,
            }),
        };

        let (tx, handle) = spawn_sidecar_writer(sidecar_path.clone(), part_path);
        tx.send(snapshot).unwrap();
        drop(tx);
        finish_sidecar_writer(&sidecar_path, handle, SidecarWriterShutdown::Flush).await;

        let loaded = load_sidecar(&sidecar_path).await.unwrap();
        assert_eq!(loaded.verified_chunks.len(), 1);
        assert_eq!(loaded.part_fingerprint, None);
    }

    #[tokio::test]
    async fn sync_and_fingerprint_part_reports_missing_files_as_untrusted() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.part");

        assert_eq!(sync_and_fingerprint_part(&missing).await, None);
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
        assert_eq!(
            single_file_package_path("archive.tar.gz"),
            "archive.tar/archive.tar.gz"
        );
    }

    #[test]
    fn stemmed_package_name_uses_file_name_without_extension() {
        assert_eq!(stemmed_package_name("file.bin"), "file");
        assert_eq!(stemmed_package_name("archive.tar.gz"), "archive.tar");
        assert_eq!(stemmed_package_name(".env"), ".env");
    }
}
