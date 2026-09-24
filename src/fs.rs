//! File system abstraction for testability.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DurableTempFileMode {
    CreateNew,
    Truncate,
}

pub(crate) fn write_durable_temp_file(
    path: &Path,
    contents: &[u8],
    description: &str,
    mode: DurableTempFileMode,
) -> std::io::Result<()> {
    use std::io::Write as _;

    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    match mode {
        DurableTempFileMode::CreateNew => {
            options.create_new(true);
        }
        DurableTempFileMode::Truncate => {
            options.create(true).truncate(true);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("write {description} {}: {error}", path.display()),
        )
    })?;
    file.write_all(contents).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("write {description} {}: {error}", path.display()),
        )
    })?;
    file.flush().map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("flush {description} {}: {error}", path.display()),
        )
    })?;
    file.sync_all().map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("sync {description} {}: {error}", path.display()),
        )
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(
            |error| {
                std::io::Error::new(
                    error.kind(),
                    format!("set {description} permissions {}: {error}", path.display()),
                )
            },
        )?;
    }

    Ok(())
}

/// Syncs directory-entry changes where the platform supports directory syncing.
///
/// Returns [`std::io::ErrorKind::Unsupported`] on non-Unix platforms rather
/// than claiming durability without performing a directory sync.
pub(crate) fn sync_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let directory = std::fs::File::open(path)?;
        directory.sync_all()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "directory syncing is not supported on this platform",
        ))
    }
}

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

    /// Opens a resumable part file without truncating or resizing it.
    async fn open_part_file_for_resume(&self, path: &Path) -> std::io::Result<tokio::fs::File> {
        tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .await
    }

    /// Reads from the already-open part file, so path replacement cannot
    /// redirect resume validation to a different file.
    async fn read_exact_at_open_file(
        &self,
        file: &tokio::fs::File,
        offset: u64,
        buf: &mut [u8],
    ) -> std::io::Result<()> {
        let mut file = file.try_clone().await?;
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        file.read_exact(buf).await?;
        Ok(())
    }

    /// Returns metadata for the open part file rather than its current path.
    async fn fingerprint_open_file(&self, file: &tokio::fs::File) -> Option<FileFingerprint> {
        file.metadata()
            .await
            .ok()
            .map(|metadata| FileFingerprint::from_metadata(&metadata))
    }

    /// Reads exactly `buf.len()` bytes at `offset` without trusting file cursor state.
    async fn read_exact_at(&self, path: &Path, offset: u64, buf: &mut [u8]) -> std::io::Result<()>;

    /// Makes a file visible at another path by renaming it.
    async fn rename_file(&self, from: &Path, to: &Path) -> std::io::Result<()>;

    /// Acknowledges durability of directory-entry changes in this directory.
    async fn sync_directory(&self, path: &Path) -> std::io::Result<()> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || crate::fs::sync_directory(&path))
            .await
            .map_err(std::io::Error::other)?
    }

    /// Flushes a file's contents and metadata to stable storage.
    async fn sync_file(&self, path: &Path) -> std::io::Result<()>;

    /// Removes a file at the given path. Ignores `NotFound` errors.
    async fn remove_file(&self, path: &Path) -> std::io::Result<()>;
}

/// Default file system implementation using `tokio::fs`.
#[derive(Debug, Clone, Default)]
pub struct TokioFileSystem {
    download_root: Option<PathBuf>,
}

pub(crate) fn canonicalize_allow_missing(path: &Path) -> std::io::Result<PathBuf> {
    let absolute;
    let path = if path.is_absolute() {
        path
    } else {
        absolute = std::env::current_dir()?.join(path);
        &absolute
    };
    let mut current = path;
    let mut missing = Vec::new();
    loop {
        match std::fs::symlink_metadata(current) {
            Ok(_) => {
                let mut resolved = std::fs::canonicalize(current)?;
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = current.file_name() else {
                    return Err(error);
                };
                missing.push(name.to_os_string());
                let Some(parent) = current.parent() else {
                    return Err(error);
                };
                current = parent;
            }
            Err(error) => return Err(error),
        }
    }
}

pub(crate) fn resolve_within_root(root: &Path, candidate: &Path) -> std::io::Result<PathBuf> {
    let root = canonicalize_allow_missing(root)?;
    let resolved = canonicalize_allow_missing(candidate)?;
    if !resolved.starts_with(root) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "path resolves outside configured download root",
        ));
    }
    Ok(resolved)
}

impl TokioFileSystem {
    /// Creates a new `TokioFileSystem` instance.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            download_root: None,
        }
    }

    pub(crate) fn with_download_root(mut self, root: Option<PathBuf>) -> Self {
        self.download_root = root;
        self
    }

    fn resolve_download_path(&self, path: &Path) -> std::io::Result<PathBuf> {
        let Some(root) = &self.download_root else {
            return Ok(path.to_path_buf());
        };
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        resolve_within_root(root, &candidate).map_err(|error| {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!(
                        "download path resolves outside configured root: {}",
                        path.display()
                    ),
                )
            } else {
                error
            }
        })
    }
}

fn sync_file_blocking(path: &Path) -> std::io::Result<()> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    file.sync_all()
}

#[async_trait]
impl FileSystem for TokioFileSystem {
    async fn file_exists(&self, path: &Path) -> bool {
        let Ok(path) = self.resolve_download_path(path) else {
            return false;
        };
        tokio::fs::metadata(path).await.is_ok()
    }

    async fn file_size(&self, path: &Path) -> Option<u64> {
        let path = self.resolve_download_path(path).ok()?;
        tokio::fs::metadata(path).await.ok().map(|m| m.len())
    }

    async fn file_fingerprint(&self, path: &Path) -> Option<FileFingerprint> {
        let path = self.resolve_download_path(path).ok()?;
        tokio::fs::metadata(path)
            .await
            .ok()
            .map(|metadata| FileFingerprint::from_metadata(&metadata))
    }

    async fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
        let path = self.resolve_download_path(path)?;
        tokio::fs::create_dir_all(path).await
    }

    async fn create_file(&self, path: &Path, size: u64) -> std::io::Result<tokio::fs::File> {
        let path = self.resolve_download_path(path)?;
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
        if let Ok(metadata) = tokio::fs::symlink_metadata(path).await
            && metadata.file_type().is_symlink()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("refusing to open symlink as part file: {}", path.display()),
            ));
        }
        let path = self.resolve_download_path(path)?;
        let file = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(!preserve_existing)
            .open(&path)
            .await?;
        file.set_len(size).await?;
        Ok(file)
    }

    async fn open_part_file_for_resume(&self, path: &Path) -> std::io::Result<tokio::fs::File> {
        if let Ok(metadata) = tokio::fs::symlink_metadata(path).await
            && metadata.file_type().is_symlink()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("refusing to open symlink as part file: {}", path.display()),
            ));
        }
        let path = self.resolve_download_path(path)?;
        tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .await
    }

    async fn read_exact_at(&self, path: &Path, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        let path = self.resolve_download_path(path)?;
        let mut file = tokio::fs::File::open(path).await?;
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        file.read_exact(buf).await?;
        Ok(())
    }

    async fn rename_file(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        let from = self.resolve_download_path(from)?;
        let to = self.resolve_download_path(to)?;
        tokio::fs::rename(from, to).await
    }

    async fn sync_directory(&self, path: &Path) -> std::io::Result<()> {
        let path = self.resolve_download_path(path)?;
        tokio::task::spawn_blocking(move || crate::fs::sync_directory(&path))
            .await
            .map_err(std::io::Error::other)?
    }

    async fn sync_file(&self, path: &Path) -> std::io::Result<()> {
        let path = self.resolve_download_path(path)?;
        tokio::task::spawn_blocking({
            let path = path.clone();
            move || sync_file_blocking(&path)
        })
        .await
        .unwrap_or_else(|_| sync_file_blocking(&path))
    }

    async fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        let path = self.resolve_download_path(path)?;
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

    #[cfg(not(unix))]
    #[test]
    fn sync_directory_fails_closed_when_platform_sync_is_unsupported() {
        let error = sync_directory(Path::new("."))
            .expect_err("unsupported platforms must not acknowledge directory durability");

        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    }

    #[test]
    fn resolve_within_root_accepts_missing_descendants_and_rejects_sibling_prefixes() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("downloads");
        std::fs::create_dir_all(&root).unwrap();

        let inside = resolve_within_root(&root, &root.join("nested/file.bin")).unwrap();
        assert!(inside.starts_with(std::fs::canonicalize(&root).unwrap()));

        let outside = resolve_within_root(&root, &dir.path().join("downloads-other/file.bin"))
            .expect_err("a sibling prefix must not satisfy containment");
        assert_eq!(outside.kind(), std::io::ErrorKind::PermissionDenied);
    }

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

    #[cfg(unix)]
    #[tokio::test]
    async fn tokio_fs_open_part_file_rejects_preexisting_symlink() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target.part");
        let link = dir.path().join("download.part");
        std::fs::write(&target, b"keep target").unwrap();
        symlink(&target, &link).unwrap();

        let error = TokioFileSystem::new()
            .open_part_file(&link, 32, false)
            .await
            .expect_err("a pre-existing part symlink must be rejected");

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(std::fs::read(&target).unwrap(), b"keep target");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rooted_tokio_fs_rejects_symlinked_ancestor_at_write() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new().unwrap();
        let root = dir.path().join("downloads");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join("linked")).unwrap();
        let path = root.join("linked/payload.part");

        let error = TokioFileSystem::new()
            .with_download_root(Some(root))
            .open_part_file(&path, 32, false)
            .await
            .expect_err("write through symlinked parent must remain inside download root");

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(!outside.join("payload.part").exists());
    }

    #[tokio::test]
    async fn tokio_fs_sync_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(b"hello").unwrap();
        file.flush().unwrap();
        drop(file);

        let fs = TokioFileSystem::new();
        fs.sync_file(&path).await.unwrap();
    }

    #[tokio::test]
    async fn rooted_tokio_fs_rejects_sync_outside_download_root() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("downloads");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        let fs = TokioFileSystem::new().with_download_root(Some(root.clone()));
        fs.sync_directory(&root).await.unwrap();
        let error = fs
            .sync_directory(&outside)
            .await
            .expect_err("directory sync must remain inside the configured root");

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    mod property_tests {
        use super::*;
        use proptest::prelude::*;

        fn block_on_test<T>(future: impl std::future::Future<Output = T>) -> T {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(future)
        }

        fn read_window_case() -> impl Strategy<Value = (Vec<u8>, usize, usize)> {
            proptest::collection::vec(any::<u8>(), 0..4097).prop_flat_map(|data| {
                let len = data.len();
                (Just(data), 0usize..=len, 0usize..=len).prop_filter(
                    "offset + read_len must stay within the file",
                    move |(_, offset, read_len)| offset.saturating_add(*read_len) <= len,
                )
            })
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(24))]

            #[test]
            fn create_file_sets_requested_length(
                size in 0u64..131_073,
            ) {
                let actual_len = block_on_test(async {
                    let dir = tempfile::tempdir().unwrap();
                    let path = dir.path().join("create.bin");
                    let fs = TokioFileSystem::new();
                    let file = fs.create_file(&path, size).await.unwrap();
                    drop(file);
                    tokio::fs::metadata(&path).await.unwrap().len()
                });

                prop_assert_eq!(actual_len, size);
            }

            #[test]
            fn read_exact_at_returns_requested_window(
                (data, offset, read_len) in read_window_case(),
            ) {
                let actual = block_on_test(async {
                    let dir = tempfile::tempdir().unwrap();
                    let path = dir.path().join("window.bin");
                    tokio::fs::write(&path, &data).await.unwrap();
                    let fs = TokioFileSystem::new();
                    let mut buf = vec![0u8; read_len];
                    fs.read_exact_at(&path, offset as u64, &mut buf).await.unwrap();
                    buf
                });

                prop_assert_eq!(actual, data[offset..offset + read_len].to_vec());
            }

            #[test]
            fn open_part_file_preserve_existing_matches_contract(
                original in proptest::collection::vec(any::<u8>(), 0..4097),
                target_size in 0usize..4097,
                preserve_existing in any::<bool>(),
            ) {
                let actual = block_on_test(async {
                    let dir = tempfile::tempdir().unwrap();
                    let path = dir.path().join("part.bin");
                    tokio::fs::write(&path, &original).await.unwrap();
                    let fs = TokioFileSystem::new();
                    let file = fs
                        .open_part_file(&path, target_size as u64, preserve_existing)
                        .await
                        .unwrap();
                    drop(file);
                    tokio::fs::read(&path).await.unwrap()
                });

                let expected = if preserve_existing {
                    let mut expected = original[..original.len().min(target_size)].to_vec();
                    expected.resize(target_size, 0);
                    expected
                } else {
                    vec![0u8; target_size]
                };
                prop_assert_eq!(actual, expected);
            }
        }
    }
}
