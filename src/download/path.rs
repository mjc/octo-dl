use sha2::Digest as _;
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};

use crate::fs::canonicalize_allow_missing;

const ARTIFACT_PREFIX: &str = ".octo-dl-artifact-";

pub(super) fn has_reserved_artifact_name(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        name.to_string_lossy()
            .get(..ARTIFACT_PREFIX.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(ARTIFACT_PREFIX))
    })
}

/// A trusted base directory for downloaded output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DownloadRoot(PathBuf);

impl DownloadRoot {
    pub(super) fn new(path: impl Into<PathBuf>) -> Result<Self, &'static str> {
        let path = path.into();
        if path.as_os_str().is_empty() {
            return Err("download root must not be empty");
        }
        Ok(Self(path))
    }

    pub(super) fn resolve(&self, output: &RelativeOutputPath) -> PathBuf {
        self.0.join(output.as_path())
    }

    pub(super) fn validate_existing_ancestors(
        &self,
        output: &RelativeOutputPath,
    ) -> std::io::Result<()> {
        let root_path = if self.0.is_absolute() {
            self.0.clone()
        } else {
            std::env::current_dir()?.join(&self.0)
        };
        let root = canonicalize_allow_missing(&root_path)?;
        let candidate = self.resolve(output);
        let resolved = canonicalize_allow_missing(&candidate)?;
        if !resolved.starts_with(root) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "output path resolves outside the configured download root",
            ));
        }
        Ok(())
    }
}

/// A download output path that cannot escape its configured [`DownloadRoot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RelativeOutputPath(PathBuf);

impl RelativeOutputPath {
    pub(super) fn new(path: impl AsRef<Path>) -> Result<Self, &'static str> {
        let path = path.as_ref();
        if path.as_os_str().is_empty() {
            return Err("relative output path must not be empty");
        }
        if path.is_absolute() {
            return Err("relative output path must not be absolute");
        }

        // A backslash is a separator on Windows but a valid filename character
        // on Unix. Rejecting it at this boundary keeps the persisted/downloaded
        // path contract portable and prevents platform-dependent traversal.
        if path.to_string_lossy().contains('\\') {
            return Err("relative output path must not contain backslashes");
        }

        for component in path.components() {
            if matches!(
                component,
                Component::CurDir
                    | Component::ParentDir
                    | Component::RootDir
                    | Component::Prefix(_)
            ) {
                return Err("relative output path contains an unsafe component");
            }
        }

        if has_reserved_artifact_name(path) {
            return Err("relative output path uses a reserved download artifact name");
        }

        Ok(Self(path.to_path_buf()))
    }

    pub(super) fn as_path(&self) -> &Path {
        &self.0
    }
}

pub(super) fn artifact_path(path: &str) -> PathBuf {
    let digest = sha2::Sha256::digest(path.as_bytes());
    let mut name = String::with_capacity(ARTIFACT_PREFIX.len() + digest.len() * 2);
    name.push_str(ARTIFACT_PREFIX);
    for byte in digest {
        let _ = write!(name, "{byte:02x}");
    }
    Path::new(path)
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_output_path_rejects_root_and_traversal() {
        for path in [
            "/outside/file.bin",
            "../outside/file.bin",
            "nested/../../file.bin",
        ] {
            assert!(RelativeOutputPath::new(path).is_err(), "accepted {path:?}");
        }
    }

    #[test]
    fn download_root_resolves_only_valid_relative_output() {
        let root = DownloadRoot::new("/downloads").unwrap();
        let output = RelativeOutputPath::new("package/file.bin").unwrap();

        assert_eq!(
            root.resolve(&output),
            PathBuf::from("/downloads/package/file.bin")
        );
    }

    #[test]
    fn empty_roots_and_outputs_are_rejected() {
        assert!(DownloadRoot::new("").is_err());
        assert!(RelativeOutputPath::new("").is_err());
    }

    #[test]
    fn output_rejects_reserved_artifact_filename_prefix() {
        assert!(RelativeOutputPath::new(".octo-dl-artifact-abc").is_err());
        assert!(RelativeOutputPath::new(".OCTO-DL-ARTIFACT-abc").is_err());
    }
}
