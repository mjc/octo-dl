use std::io;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use super::sidecar_writer::sidecar_tmp_path;
use crate::fs::FileFingerprint;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::download) struct VerifiedChunkRecord {
    pub(in crate::download) index: u32,
    pub(in crate::download) mac: [u8; 16],
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(in crate::download) struct VerifiedChunks(Vec<VerifiedChunkRecord>);

impl VerifiedChunks {
    pub(in crate::download) fn with_capacity(capacity: usize) -> Self {
        Self(Vec::with_capacity(capacity))
    }

    pub(in crate::download) fn insert(&mut self, index: usize, record: VerifiedChunkRecord) {
        self.0.insert(index, record);
    }

    pub(in crate::download) fn push(&mut self, record: VerifiedChunkRecord) {
        self.0.push(record);
    }
}

impl std::ops::Deref for VerifiedChunks {
    type Target = [VerifiedChunkRecord];

    fn deref(&self) -> &Self::Target {
        self.0.as_slice()
    }
}

impl std::ops::DerefMut for VerifiedChunks {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut_slice()
    }
}

impl From<Vec<VerifiedChunkRecord>> for VerifiedChunks {
    fn from(value: Vec<VerifiedChunkRecord>) -> Self {
        Self(value)
    }
}

impl FromIterator<VerifiedChunkRecord> for VerifiedChunks {
    fn from_iter<T: IntoIterator<Item = VerifiedChunkRecord>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl IntoIterator for VerifiedChunks {
    type Item = VerifiedChunkRecord;
    type IntoIter = std::vec::IntoIter<VerifiedChunkRecord>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a VerifiedChunks {
    type Item = &'a VerifiedChunkRecord;
    type IntoIter = std::slice::Iter<'a, VerifiedChunkRecord>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(in crate::download) struct ResumeSidecar {
    pub(in crate::download) version: u32,
    pub(in crate::download) file_size: u64,
    pub(in crate::download) expected_condensed_mac: [u8; 8],
    pub(in crate::download) verified_chunks: VerifiedChunks,
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
        let mut verified_chunks = VerifiedChunks::with_capacity(legacy.verified_chunks.len());
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
                migrate_legacy_binary_sidecar(path, &legacy_binary_path, &sidecar).await;
                return Some(sidecar);
            }
            let legacy_json_data = tokio::fs::read(&legacy_json_path).await.ok()?;
            deserialize_legacy_json_sidecar(&legacy_json_data)
        }
        Err(_) => {
            if let Ok(legacy_binary_data) = tokio::fs::read(&legacy_binary_path).await
                && let Some(sidecar) = deserialize_legacy_binary_sidecar(&legacy_binary_data)
            {
                migrate_legacy_binary_sidecar(path, &legacy_binary_path, &sidecar).await;
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
    let mut reader = LegacyBinaryReader::new(data);
    let version = reader.read_u32()?;
    let file_size = reader.read_u64()?;
    let expected_condensed_mac = reader.read_array::<8>()?;
    let verified_chunks_len = usize::try_from(reader.read_u64()?).ok()?;

    // Reject impossible lengths before allocating so a corrupt legacy file
    // cannot request an unbounded Vec.
    if verified_chunks_len > reader.remaining() / 20 {
        return None;
    }
    let mut verified_chunks = VerifiedChunks::with_capacity(verified_chunks_len);
    for _ in 0..verified_chunks_len {
        verified_chunks.push(VerifiedChunkRecord {
            index: reader.read_u32()?,
            mac: reader.read_array::<16>()?,
        });
    }

    let part_fingerprint = if reader.remaining() == 0 {
        None
    } else {
        match reader.read_u8()? {
            0 => None,
            1 => Some(FileFingerprint {
                len: reader.read_u64()?,
                modified_ns: reader.read_u128()?,
                allocated_bytes: reader.read_option_u64()?,
                dev: reader.read_option_u64()?,
                ino: reader.read_option_u64()?,
            }),
            _ => return None,
        }
    };

    reader.is_empty().then_some(ResumeSidecar {
        version,
        file_size,
        expected_condensed_mac,
        verified_chunks,
        part_fingerprint,
    })
}

struct LegacyBinaryReader<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> LegacyBinaryReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, offset: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.offset)
    }

    fn is_empty(&self) -> bool {
        self.offset == self.data.len()
    }

    fn read_exact<const N: usize>(&mut self) -> Option<[u8; N]> {
        let end = self.offset.checked_add(N)?;
        let bytes = self.data.get(self.offset..end)?;
        self.offset = end;
        bytes.try_into().ok()
    }

    fn read_u8(&mut self) -> Option<u8> {
        Some(self.read_exact::<1>()?[0])
    }

    fn read_u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.read_exact()?))
    }

    fn read_u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.read_exact()?))
    }

    fn read_u128(&mut self) -> Option<u128> {
        Some(u128::from_le_bytes(self.read_exact()?))
    }

    fn read_array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.read_exact()
    }

    fn read_option_u64(&mut self) -> Option<Option<u64>> {
        match self.read_u8()? {
            0 => Some(None),
            1 => Some(Some(self.read_u64()?)),
            _ => None,
        }
    }
}

async fn migrate_legacy_binary_sidecar(
    path: &Path,
    legacy_binary_path: &Path,
    sidecar: &ResumeSidecar,
) {
    if path == legacy_binary_path || save_sidecar_atomic(path, sidecar).await.is_err() {
        return;
    }
    let _ = tokio::fs::remove_file(legacy_binary_path).await;
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

#[cfg(test)]
pub(super) fn legacy_binary_bytes(sidecar: &ResumeSidecar) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(&sidecar.version.to_le_bytes());
    data.extend_from_slice(&sidecar.file_size.to_le_bytes());
    data.extend_from_slice(&sidecar.expected_condensed_mac);
    data.extend_from_slice(&(sidecar.verified_chunks.len() as u64).to_le_bytes());
    for record in &sidecar.verified_chunks {
        data.extend_from_slice(&record.index.to_le_bytes());
        data.extend_from_slice(&record.mac);
    }
    match sidecar.part_fingerprint {
        None => data.push(0),
        Some(fingerprint) => {
            data.push(1);
            data.extend_from_slice(&fingerprint.len.to_le_bytes());
            data.extend_from_slice(&fingerprint.modified_ns.to_le_bytes());
            for value in [
                fingerprint.allocated_bytes,
                fingerprint.dev,
                fingerprint.ino,
            ] {
                match value {
                    None => data.push(0),
                    Some(value) => {
                        data.push(1);
                        data.extend_from_slice(&value.to_le_bytes());
                    }
                }
            }
        }
    }
    data
}
