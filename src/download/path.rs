use std::path::{Component, Path, PathBuf};

/// A trusted base directory for downloaded output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DownloadRoot(PathBuf);

impl DownloadRoot {
    pub(crate) fn new(path: impl Into<PathBuf>) -> Result<Self, &'static str> {
        let path = path.into();
        if path.as_os_str().is_empty() {
            return Err("download root must not be empty");
        }
        Ok(Self(path))
    }

    pub(crate) fn resolve(&self, output: &RelativeOutputPath) -> PathBuf {
        self.0.join(output.as_path())
    }
}

/// A download output path that cannot escape its configured [`DownloadRoot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelativeOutputPath(PathBuf);

impl RelativeOutputPath {
    pub(crate) fn new(path: impl AsRef<Path>) -> Result<Self, &'static str> {
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

        Ok(Self(path.to_path_buf()))
    }

    pub(crate) fn as_path(&self) -> &Path {
        &self.0
    }
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
}
