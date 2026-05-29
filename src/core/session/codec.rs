use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::DownloadConfig;
use crate::core::model::{PackageId, PackageKey, SessionRunStatus, UrlId};

use super::{
    FileSnapshot, PackageSnapshot, SESSION_FILE_PREFIX, SESSION_POSTCARD_EXTENSION,
    SESSION_TOML_EXTENSION, SavedCredentials, SessionSnapshot, SessionUrlSnapshot,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionEncoding {
    Postcard,
    Toml,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PostcardSessionUrlSnapshot {
    url: UrlId,
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PostcardPackageSnapshot {
    id: PackageId,
    key: PackageKey,
    display_name: String,
    files: Vec<FileSnapshot>,
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PostcardSessionSnapshot {
    version: u32,
    id: String,
    created: DateTime<Utc>,
    status: SessionRunStatus,
    urls: Vec<PostcardSessionUrlSnapshot>,
    packages: Vec<PostcardPackageSnapshot>,
    config: DownloadConfig,
    credentials: SavedCredentials,
}

impl From<&SessionUrlSnapshot> for PostcardSessionUrlSnapshot {
    fn from(url: &SessionUrlSnapshot) -> Self {
        Self {
            url: url.url.clone(),
            error: url.error.clone(),
        }
    }
}

impl From<PostcardSessionUrlSnapshot> for SessionUrlSnapshot {
    fn from(url: PostcardSessionUrlSnapshot) -> Self {
        Self {
            url: url.url,
            error: url.error,
        }
    }
}

impl From<&PackageSnapshot> for PostcardPackageSnapshot {
    fn from(package: &PackageSnapshot) -> Self {
        Self {
            id: package.id,
            key: package.key.clone(),
            display_name: package.display_name.clone(),
            files: package.files.clone(),
            error: package.error.clone(),
        }
    }
}

impl From<PostcardPackageSnapshot> for PackageSnapshot {
    fn from(package: PostcardPackageSnapshot) -> Self {
        Self {
            id: package.id,
            key: package.key,
            display_name: package.display_name,
            files: package.files,
            error: package.error,
        }
    }
}

impl From<&SessionSnapshot> for PostcardSessionSnapshot {
    fn from(snapshot: &SessionSnapshot) -> Self {
        Self {
            version: snapshot.version,
            id: snapshot.id.clone(),
            created: snapshot.created,
            status: snapshot.status,
            urls: snapshot
                .urls
                .iter()
                .map(PostcardSessionUrlSnapshot::from)
                .collect(),
            packages: snapshot
                .packages
                .iter()
                .map(PostcardPackageSnapshot::from)
                .collect(),
            config: snapshot.config.clone(),
            credentials: snapshot.credentials.clone(),
        }
    }
}

impl From<PostcardSessionSnapshot> for SessionSnapshot {
    fn from(snapshot: PostcardSessionSnapshot) -> Self {
        Self {
            version: snapshot.version,
            id: snapshot.id,
            created: snapshot.created,
            status: snapshot.status,
            urls: snapshot
                .urls
                .into_iter()
                .map(SessionUrlSnapshot::from)
                .collect(),
            packages: snapshot
                .packages
                .into_iter()
                .map(PackageSnapshot::from)
                .collect(),
            config: snapshot.config,
            credentials: snapshot.credentials,
        }
    }
}

fn session_encoding_for_path(path: &Path) -> SessionEncoding {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some(SESSION_POSTCARD_EXTENSION) => SessionEncoding::Postcard,
        _ => SessionEncoding::Toml,
    }
}

pub(super) fn is_canonical_session_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some(SESSION_POSTCARD_EXTENSION | SESSION_TOML_EXTENSION)
    ) && path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(SESSION_FILE_PREFIX))
}

fn session_path_preference(path: &Path) -> u8 {
    match session_encoding_for_path(path) {
        SessionEncoding::Postcard => 1,
        SessionEncoding::Toml => 0,
    }
}

pub(super) fn should_replace_session_candidate(
    candidate_path: &Path,
    candidate_modified: Option<SystemTime>,
    existing_path: &Path,
    existing_modified: Option<SystemTime>,
) -> bool {
    let candidate_preference = session_path_preference(candidate_path);
    let existing_preference = session_path_preference(existing_path);
    if candidate_preference != existing_preference {
        return candidate_preference > existing_preference;
    }

    matches!(
        (candidate_modified, existing_modified),
        (Some(candidate), Some(existing)) if candidate > existing
    )
}

pub(super) fn temporary_save_path(path: &Path) -> PathBuf {
    path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or(SESSION_POSTCARD_EXTENSION)
    ))
}

pub(super) fn encode_snapshot(path: &Path, snapshot: &SessionSnapshot) -> std::io::Result<Vec<u8>> {
    match session_encoding_for_path(path) {
        SessionEncoding::Postcard => postcard::to_stdvec(&PostcardSessionSnapshot::from(snapshot))
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
        SessionEncoding::Toml => toml::to_string(snapshot)
            .map(|value| value.into_bytes())
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
    }
}

pub(super) fn decode_snapshot(path: &Path) -> std::io::Result<SessionSnapshot> {
    match session_encoding_for_path(path) {
        SessionEncoding::Postcard => {
            let contents = std::fs::read(path)?;
            postcard::from_bytes::<PostcardSessionSnapshot>(&contents)
                .map(SessionSnapshot::from)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
        }
        SessionEncoding::Toml => {
            let contents = std::fs::read_to_string(path)?;
            toml::from_str(&contents)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_replace_session_candidate_prefers_postcard_duplicates() {
        let candidate = Path::new("/tmp/session-v6-demo.postcard");
        let existing = Path::new("/tmp/session-v6-demo.toml");

        assert!(should_replace_session_candidate(
            candidate, None, existing, None
        ));
        assert!(!should_replace_session_candidate(
            existing, None, candidate, None
        ));
    }

    #[test]
    fn temporary_save_path_preserves_primary_extension() {
        assert_eq!(
            temporary_save_path(Path::new("/tmp/session-v6-demo.postcard")),
            PathBuf::from("/tmp/session-v6-demo.postcard.tmp")
        );
        assert_eq!(
            temporary_save_path(Path::new("/tmp/session-v6-demo.toml")),
            PathBuf::from("/tmp/session-v6-demo.toml.tmp")
        );
    }
}
