//! File system abstraction for testability.

use async_trait::async_trait;
use std::path::Path;

/// Abstraction over file system operations for testability.
#[async_trait]
pub trait FileSystem: Send + Sync {
    /// Checks if a file exists at the given path.
    async fn file_exists(&self, path: &Path) -> bool;

    /// Returns the size of a file if it exists.
    async fn file_size(&self, path: &Path) -> Option<u64>;

    /// Creates all directories in the given path.
    async fn create_dir_all(&self, path: &Path) -> std::io::Result<()>;

    /// Creates a file at the given path and pre-allocates the specified size.
    async fn create_file(&self, path: &Path, size: u64) -> std::io::Result<tokio::fs::File>;
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

    async fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
        tokio::fs::create_dir_all(path).await
    }

    async fn create_file(&self, path: &Path, size: u64) -> std::io::Result<tokio::fs::File> {
        let file = tokio::fs::File::create(path).await?;
        file.set_len(size).await?;
        Ok(file)
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
}
