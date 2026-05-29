use std::io;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use super::downloader::CURRENT_RESUME_SIDECAR_VERSION;
use super::sidecar_writer::sidecar_tmp_path;
use crate::fs::FileFingerprint;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::download) struct VerifiedChunkRecord {
    pub(in crate::download) index: u32,
    pub(in crate::download) mac: [u8; 16],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(in crate::download) struct ResumeSidecar {
    pub(in crate::download) version: u32,
    pub(in crate::download) file_size: u64,
    pub(in crate::download) expected_condensed_mac: [u8; 8],
    pub(in crate::download) verified_chunks: Vec<VerifiedChunkRecord>,
    #[serde(default)]
    pub(in crate::download) part_fingerprint: Option<FileFingerprint>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct LegacyJsonVerifiedChunkRecord {
    pub(super) index: u32,
    pub(super) mac_b64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct LegacyJsonResumeSidecar {
    pub(super) version: u32,
    pub(super) file_size: u64,
    pub(super) expected_condensed_mac_b64: String,
    pub(super) verified_chunks: Vec<LegacyJsonVerifiedChunkRecord>,
    #[serde(default)]
    pub(super) part_fingerprint: Option<FileFingerprint>,
}

impl TryFrom<LegacyJsonResumeSidecar> for ResumeSidecar {
    type Error = ();

    fn try_from(legacy: LegacyJsonResumeSidecar) -> std::result::Result<Self, Self::Error> {
        let expected_condensed_mac = decode_array::<8>(&legacy.expected_condensed_mac_b64)?;
        let mut verified_chunks = Vec::with_capacity(legacy.verified_chunks.len());
        for record in legacy.verified_chunks {
            verified_chunks.push(VerifiedChunkRecord {
                index: record.index,
                mac: decode_array::<16>(&record.mac_b64)?,
            });
        }
        Ok(Self {
            version: legacy.version,
            file_size: legacy.file_size,
            expected_condensed_mac,
            verified_chunks,
            part_fingerprint: legacy.part_fingerprint,
        })
    }
}

fn decode_array<const N: usize>(encoded: &str) -> std::result::Result<[u8; N], ()> {
    let mut output = [0_u8; N];
    let written = STANDARD
        .decode_slice(encoded.as_bytes(), &mut output)
        .map_err(|_| ())?;
    (written == N).then_some(output).ok_or(())
}

pub(super) fn load_sidecar_sync(
    path: &Path,
    legacy_binary_path: &Path,
    legacy_json_path: &Path,
) -> Option<ResumeSidecar> {
    std::fs::read(path)
        .ok()
        .and_then(|data| deserialize_postcard_sidecar(&data))
        .or_else(|| {
            std::fs::read(legacy_binary_path)
                .ok()
                .and_then(|data| deserialize_legacy_binary_sidecar(&data))
        })
        .or_else(|| {
            std::fs::read(legacy_json_path)
                .ok()
                .and_then(|data| deserialize_legacy_json_sidecar(&data))
        })
}

pub(in crate::download) async fn load_sidecar(path: &Path) -> Option<ResumeSidecar> {
    let legacy_binary_path = legacy_binary_path_for_sidecar(path);
    let legacy_json_path = legacy_json_path_for_sidecar(path);
    match tokio::fs::read(path).await {
        Ok(data) => {
            if let Some(sidecar) = deserialize_postcard_sidecar(&data) {
                return Some(sidecar);
            }
            if legacy_binary_path == path {
                if let Some(sidecar) = deserialize_legacy_binary_sidecar(&data) {
                    return Some(sidecar);
                }
            }
            if legacy_json_path == path {
                return deserialize_legacy_json_sidecar(&data);
            }
            if let Ok(legacy_binary_data) = tokio::fs::read(&legacy_binary_path).await
                && let Some(sidecar) = deserialize_legacy_binary_sidecar(&legacy_binary_data)
            {
                return Some(sidecar);
            }
            let legacy_json_data = tokio::fs::read(&legacy_json_path).await.ok()?;
            deserialize_legacy_json_sidecar(&legacy_json_data)
        }
        Err(_) => {
            if let Ok(legacy_binary_data) = tokio::fs::read(&legacy_binary_path).await
                && let Some(sidecar) = deserialize_legacy_binary_sidecar(&legacy_binary_data)
            {
                return Some(sidecar);
            }
            let legacy_json_data = tokio::fs::read(&legacy_json_path).await.ok()?;
            deserialize_legacy_json_sidecar(&legacy_json_data)
        }
    }
}

fn deserialize_postcard_sidecar(data: &[u8]) -> Option<ResumeSidecar> {
    postcard::from_bytes(data).ok()
}

fn deserialize_legacy_binary_sidecar(data: &[u8]) -> Option<ResumeSidecar> {
    bincode::deserialize(data).ok()
}

fn deserialize_legacy_json_sidecar(data: &[u8]) -> Option<ResumeSidecar> {
    serde_json::from_slice::<LegacyJsonResumeSidecar>(data)
        .ok()
        .and_then(|sidecar| sidecar.try_into().ok())
}

pub(super) async fn save_sidecar_atomic(path: &Path, sidecar: &ResumeSidecar) -> io::Result<()> {
    let tmp = sidecar_tmp_path(path);
    let data = postcard::to_stdvec(sidecar).map_err(io::Error::other)?;
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

pub(super) fn legacy_binary_path_for_sidecar(path: &Path) -> PathBuf {
    let path = path.as_os_str().to_string_lossy();
    if let Some(stem) = path.strip_suffix(".part.postcard") {
        let mut legacy = String::with_capacity(stem.len() + ".part.meta.bin".len());
        legacy.push_str(stem);
        legacy.push_str(".part.meta.bin");
        return PathBuf::from(legacy);
    }
    if let Some(stem) = path.strip_suffix(".part.meta.json") {
        let mut legacy = String::with_capacity(stem.len() + ".part.meta.bin".len());
        legacy.push_str(stem);
        legacy.push_str(".part.meta.bin");
        return PathBuf::from(legacy);
    }
    PathBuf::from(path.as_ref())
}

pub(super) fn legacy_json_path_for_sidecar(path: &Path) -> PathBuf {
    let path = path.as_os_str().to_string_lossy();
    if let Some(stem) = path
        .strip_suffix(".part.postcard")
        .or_else(|| path.strip_suffix(".part.meta.bin"))
    {
        let mut json = String::with_capacity(stem.len() + ".part.meta.json".len());
        json.push_str(stem);
        json.push_str(".part.meta.json");
        return PathBuf::from(json);
    }
    PathBuf::from(path.as_ref())
}

pub(super) fn postcard_path_for_sidecar(path: &Path) -> PathBuf {
    let path = path.as_os_str().to_string_lossy();
    if let Some(stem) = path
        .strip_suffix(".part.meta.bin")
        .or_else(|| path.strip_suffix(".part.meta.json"))
    {
        let mut postcard = String::with_capacity(stem.len() + ".part.postcard".len());
        postcard.push_str(stem);
        postcard.push_str(".part.postcard");
        return PathBuf::from(postcard);
    }
    PathBuf::from(path.as_ref())
}

#[cfg(test)]
mod tests;
