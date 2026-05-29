use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use tokio_util::sync::CancellationToken;

use crate::config::DownloadConfig;
use crate::core::ProgressDelta;
use crate::fs::{FileFingerprint, FileSystem, TokioFileSystem};

use super::*;

pub(super) static TEST_AES_KEY: [u8; 16] = [7u8; 16];
pub(super) static TEST_AES_IV: [u8; 8] = [3u8; 8];

pub(super) struct MockFileSystem {
    files: Mutex<HashMap<PathBuf, u64>>,
    fingerprints: Mutex<HashMap<PathBuf, FileFingerprint>>,
}

impl MockFileSystem {
    pub(super) fn new() -> Self {
        Self {
            files: Mutex::new(HashMap::new()),
            fingerprints: Mutex::new(HashMap::new()),
        }
    }

    pub(super) fn add_file(&self, path: impl Into<PathBuf>, size: u64) {
        self.files.lock().unwrap().insert(path.into(), size);
    }

    pub(super) fn add_fingerprint(&self, path: impl Into<PathBuf>, fingerprint: FileFingerprint) {
        self.fingerprints
            .lock()
            .unwrap()
            .insert(path.into(), fingerprint);
    }
}

#[async_trait::async_trait]
impl FileSystem for MockFileSystem {
    async fn file_exists(&self, path: &Path) -> bool {
        self.files.lock().unwrap().contains_key(path)
    }

    async fn file_size(&self, path: &Path) -> Option<u64> {
        self.files.lock().unwrap().get(path).copied()
    }

    async fn file_fingerprint(&self, path: &Path) -> Option<FileFingerprint> {
        self.fingerprints.lock().unwrap().get(path).copied()
    }

    async fn create_dir_all(&self, _path: &Path) -> io::Result<()> {
        Ok(())
    }

    async fn create_file(&self, _path: &Path, _size: u64) -> io::Result<tokio::fs::File> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "mock"))
    }

    async fn open_part_file(
        &self,
        _path: &Path,
        _size: u64,
        _preserve_existing: bool,
    ) -> io::Result<tokio::fs::File> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "mock"))
    }

    async fn read_exact_at(&self, _path: &Path, _offset: u64, _buf: &mut [u8]) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "mock"))
    }

    async fn rename_file(&self, _from: &Path, _to: &Path) -> io::Result<()> {
        Ok(())
    }

    async fn sync_file(&self, _path: &Path) -> io::Result<()> {
        Ok(())
    }

    async fn remove_file(&self, _path: &Path) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn mock_downloader(fs: MockFileSystem) -> Downloader<MockFileSystem> {
    let http = reqwest::Client::new();
    let client = mega::Client::builder().build(http).unwrap();
    Downloader::with_fs(client, DownloadConfig::default(), fs)
}

pub(super) fn mock_downloader_force(fs: MockFileSystem) -> Downloader<MockFileSystem> {
    let http = reqwest::Client::new();
    let client = mega::Client::builder().build(http).unwrap();
    let config = DownloadConfig {
        force_overwrite: true,
        ..DownloadConfig::default()
    };
    Downloader::with_fs(client, config, fs)
}

pub(super) fn tokio_downloader() -> Downloader<TokioFileSystem> {
    let http = reqwest::Client::new();
    let client = mega::Client::builder().build(http).unwrap();
    Downloader::new(client, DownloadConfig::default())
}

pub(super) fn run_with_large_stack_current_thread_runtime<F, Fut>(name: &str, run: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + 'static,
{
    std::thread::Builder::new()
        .name(name.to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(run());
        })
        .unwrap()
        .join()
        .unwrap();
}

pub(super) fn test_plaintext(size: usize) -> Vec<u8> {
    (0..size).map(|i| u8::try_from(i % 251).unwrap()).collect()
}

pub(super) fn test_incompressible_plaintext(size: usize) -> Vec<u8> {
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

pub(super) fn usize_from_u64(value: u64) -> usize {
    usize::try_from(value).unwrap()
}

pub(super) fn usize_from_u32(value: u32) -> usize {
    usize::try_from(value).unwrap()
}

pub(super) fn chunk_data<'a>(data: &'a [u8], chunk: &mega::MegaChunk) -> &'a [u8] {
    &data[usize_from_u64(chunk.offset)..usize_from_u64(chunk.offset + chunk.length)]
}

pub(super) fn fingerprint_with_allocated_bytes(
    len: u64,
    allocated_bytes: Option<u64>,
) -> FileFingerprint {
    FileFingerprint {
        len,
        modified_ns: 42,
        allocated_bytes,
        dev: Some(7),
        ino: Some(9),
    }
}

pub(super) fn sidecar_for_chunk(
    file_size: u64,
    expected_condensed_mac: [u8; 8],
    index: u32,
    mac: [u8; 16],
) -> ResumeSidecar {
    ResumeSidecar {
        version: CURRENT_RESUME_SIDECAR_VERSION,
        file_size,
        expected_condensed_mac,
        verified_chunks: vec![VerifiedChunkRecord { index, mac }],
        part_fingerprint: None,
    }
}

pub(super) fn legacy_json_sidecar_for_chunk(
    file_size: u64,
    expected_condensed_mac: [u8; 8],
    index: u32,
    mac: [u8; 16],
) -> LegacyJsonResumeSidecar {
    LegacyJsonResumeSidecar {
        version: CURRENT_RESUME_SIDECAR_VERSION,
        file_size,
        expected_condensed_mac_b64: STANDARD.encode(expected_condensed_mac),
        verified_chunks: vec![LegacyJsonVerifiedChunkRecord {
            index,
            mac_b64: STANDARD.encode(mac),
        }],
        part_fingerprint: None,
    }
}

pub(super) async fn write_legacy_json_sidecar(
    path: &Path,
    sidecar: &LegacyJsonResumeSidecar,
) -> io::Result<()> {
    let data = serde_json::to_vec(sidecar)?;
    tokio::fs::write(path, data).await
}

#[derive(Default)]
pub(super) struct RecordingProgress {
    pub(super) total: std::sync::atomic::AtomicU64,
    pub(super) network: std::sync::atomic::AtomicU64,
    pub(super) max_delta: std::sync::atomic::AtomicU64,
    pub(super) calls: std::sync::atomic::AtomicUsize,
    pub(super) validation_starts: std::sync::atomic::AtomicUsize,
    pub(super) validation_calls: std::sync::atomic::AtomicUsize,
    pub(super) validation_checked: std::sync::atomic::AtomicU64,
    pub(super) validation_total: std::sync::atomic::AtomicU64,
}

impl DownloadProgress for RecordingProgress {
    fn on_resume_validation_start(&self, _name: &str) {
        self.validation_starts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    fn on_resume_validation_progress(&self, _name: &str, checked_bytes: u64, total_bytes: u64) {
        self.validation_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.validation_checked
            .store(checked_bytes, std::sync::atomic::Ordering::SeqCst);
        self.validation_total
            .store(total_bytes, std::sync::atomic::Ordering::SeqCst);
    }

    fn on_progress(&self, _name: &str, delta: ProgressDelta) {
        self.total
            .fetch_add(delta.total_bytes_delta, std::sync::atomic::Ordering::SeqCst);
        self.network.fetch_add(
            delta.network_bytes_delta,
            std::sync::atomic::Ordering::SeqCst,
        );
        self.max_delta
            .fetch_max(delta.total_bytes_delta, std::sync::atomic::Ordering::SeqCst);
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

pub(super) struct CancelOnProgress {
    pub(super) token: CancellationToken,
    pub(super) calls: std::sync::atomic::AtomicUsize,
}

impl DownloadProgress for CancelOnProgress {
    fn on_progress(&self, _name: &str, _delta: ProgressDelta) {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.token.cancel();
    }
}
