//! File system abstraction for testability.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::UNIX_EPOCH;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFingerprint {
    pub len: u64,
    pub modified_ns: u128,
    #[serde(default)]
    pub allocated_bytes: Option<u64>,
    #[serde(default)]
    pub dev: Option<u64>,
    #[serde(default)]
    pub ino: Option<u64>,
}

impl FileFingerprint {
    #[must_use]
    pub fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        let modified_ns = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_nanos());

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self {
                len: metadata.len(),
                modified_ns,
                allocated_bytes: Some(metadata.blocks().saturating_mul(512)),
                dev: Some(metadata.dev()),
                ino: Some(metadata.ino()),
            }
        }

        #[cfg(not(unix))]
        {
            Self {
                len: metadata.len(),
                modified_ns,
                allocated_bytes: None,
                dev: None,
                ino: None,
            }
        }
    }
}

/// Abstraction over file system operations for testability.
#[async_trait]
pub trait FileSystem: Send + Sync {
    /// Checks if a file exists at the given path.
    async fn file_exists(&self, path: &Path) -> bool;

    /// Returns the size of a file if it exists.
    async fn file_size(&self, path: &Path) -> Option<u64>;

    /// Returns a stable-enough fingerprint for detecting unchanged files.
    async fn file_fingerprint(&self, path: &Path) -> Option<FileFingerprint>;

    /// Creates all directories in the given path.
    async fn create_dir_all(&self, path: &Path) -> std::io::Result<()>;

    /// Creates a file at the given path and pre-allocates the specified size.
    async fn create_file(&self, path: &Path, size: u64) -> std::io::Result<tokio::fs::File>;

    /// Opens a `.part` file and pre-allocates it to `size`.
    async fn open_part_file(
        &self,
        path: &Path,
        size: u64,
        preserve_existing: bool,
    ) -> std::io::Result<tokio::fs::File>;

    /// Reads exactly `buf.len()` bytes at `offset` without trusting file cursor state.
    async fn read_exact_at(&self, path: &Path, offset: u64, buf: &mut [u8]) -> std::io::Result<()>;

    /// Renames a file from one path to another.
    async fn rename_file(&self, from: &Path, to: &Path) -> std::io::Result<()>;

    /// Flushes a file's contents and metadata to stable storage.
    async fn sync_file(&self, path: &Path) -> std::io::Result<()>;

    /// Removes a file at the given path. Ignores `NotFound` errors.
    async fn remove_file(&self, path: &Path) -> std::io::Result<()>;
}

/// Default file system implementation using `tokio::fs`.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioFileSystem;

impl TokioFileSystem {
    /// Creates a new `TokioFileSystem` instance.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl FileSystem for TokioFileSystem {
    async fn file_exists(&self, path: &Path) -> bool {
        tokio::fs::metadata(path).await.is_ok()
    }

    async fn file_size(&self, path: &Path) -> Option<u64> {
        tokio::fs::metadata(path).await.ok().map(|m| m.len())
    }

    async fn file_fingerprint(&self, path: &Path) -> Option<FileFingerprint> {
        tokio::fs::metadata(path)
            .await
            .ok()
            .map(|metadata| FileFingerprint::from_metadata(&metadata))
    }

    async fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
        tokio::fs::create_dir_all(path).await
    }

    async fn create_file(&self, path: &Path, size: u64) -> std::io::Result<tokio::fs::File> {
        let file = tokio::fs::File::create(path).await?;
        file.set_len(size).await?;
        Ok(file)
    }

    async fn open_part_file(
        &self,
        path: &Path,
        size: u64,
        preserve_existing: bool,
    ) -> std::io::Result<tokio::fs::File> {
        let file = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(!preserve_existing)
            .open(path)
            .await?;
        file.set_len(size).await?;
        Ok(file)
    }

    async fn read_exact_at(&self, path: &Path, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        let mut file = tokio::fs::File::open(path).await?;
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        file.read_exact(buf).await?;
        Ok(())
    }

    async fn rename_file(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        tokio::fs::rename(from, to).await
    }

    async fn sync_file(&self, path: &Path) -> std::io::Result<()> {
        let file = tokio::fs::OpenOptions::new().read(true).open(path).await?;
        file.sync_data().await
    }

    async fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        match tokio::fs::remove_file(path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[tokio::test]
    async fn tokio_fs_file_exists() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::File::create(&path).unwrap();

        let fs = TokioFileSystem::new();
        assert!(fs.file_exists(&path).await);
        assert!(!fs.file_exists(&dir.path().join("nonexistent.txt")).await);
    }

    #[tokio::test]
    async fn tokio_fs_file_size() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(b"hello").unwrap();

        let fs = TokioFileSystem::new();
        assert_eq!(fs.file_size(&path).await, Some(5));
        assert_eq!(
            fs.file_size(&dir.path().join("nonexistent.txt")).await,
            None
        );
    }

    #[tokio::test]
    async fn tokio_fs_create_dir_all() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("a/b/c");

        let fs = TokioFileSystem::new();
        fs.create_dir_all(&nested).await.unwrap();
        assert!(nested.exists());
    }

    #[tokio::test]
    async fn tokio_fs_create_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");

        let fs = TokioFileSystem::new();
        let _file = fs.create_file(&path, 1024).await.unwrap();

        // File should exist with pre-allocated size
        let metadata = std::fs::metadata(&path).unwrap();
        assert_eq!(metadata.len(), 1024);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tokio_fs_fingerprint_reports_sparse_allocation() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sparse.bin");
        let file = tokio::fs::File::create(&path).await.unwrap();
        file.set_len(1024 * 1024).await.unwrap();
        drop(file);

        let fs = TokioFileSystem::new();
        let fingerprint = fs.file_fingerprint(&path).await.unwrap();

        assert_eq!(fingerprint.len, 1024 * 1024);
        assert!(fingerprint.allocated_bytes.unwrap() < fingerprint.len);
    }

    #[tokio::test]
    async fn tokio_fs_rename_file() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("source.txt");
        let dst = dir.path().join("dest.txt");
        std::fs::File::create(&src).unwrap();

        let fs = TokioFileSystem::new();
        fs.rename_file(&src, &dst).await.unwrap();
        assert!(!src.exists());
        assert!(dst.exists());
    }

    #[tokio::test]
    async fn tokio_fs_remove_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::File::create(&path).unwrap();

        let fs = TokioFileSystem::new();
        fs.remove_file(&path).await.unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn tokio_fs_remove_file_not_found_is_ok() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent.txt");

        let fs = TokioFileSystem::new();
        // Should not error on missing file
        fs.remove_file(&path).await.unwrap();
    }
}
