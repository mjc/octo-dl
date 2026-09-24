use std::io;
use std::path::{Path, PathBuf};

use super::path::{artifact_path, has_reserved_artifact_name};
use super::resume_state::CURRENT_RESUME_SIDECAR_VERSION;
use super::sidecar_store::{
    legacy_binary_path_for_sidecar, legacy_json_path_for_sidecar, load_sidecar_sync,
    postcard_path_for_sidecar,
};
use super::sidecar_writer::sidecar_tmp_path;

pub fn part_path(path: &str) -> PathBuf {
    let mut part = artifact_path(path).into_os_string();
    part.push(".part");
    PathBuf::from(part)
}

pub(super) fn legacy_part_path(path: &str) -> PathBuf {
    PathBuf::from(format!("{path}.part"))
}

pub(super) fn legacy_postcard_sidecar_path(path: &str) -> PathBuf {
    PathBuf::from(format!("{path}.part.postcard"))
}

pub fn sidecar_path(path: &str) -> PathBuf {
    let mut sidecar = part_path(path).into_os_string();
    sidecar.push(".postcard");
    PathBuf::from(sidecar)
}

pub fn legacy_binary_sidecar_path(path: &str) -> PathBuf {
    let mut sidecar = String::with_capacity(path.len() + ".part.meta.bin".len());
    sidecar.push_str(path);
    sidecar.push_str(".part.meta.bin");
    PathBuf::from(sidecar)
}

pub fn legacy_json_sidecar_path(path: &str) -> PathBuf {
    let mut sidecar = String::with_capacity(path.len() + ".part.meta.json".len());
    sidecar.push_str(path);
    sidecar.push_str(".part.meta.json");
    PathBuf::from(sidecar)
}

#[derive(Debug, Clone)]
pub(super) struct ResumeArtifactCandidate {
    pub(super) part: PathBuf,
    pub(super) sidecars: [PathBuf; 3],
}

pub(super) fn resume_artifact_candidates(path: &str) -> [ResumeArtifactCandidate; 2] {
    let legacy_binary = legacy_binary_sidecar_path(path);
    let legacy_json = legacy_json_sidecar_path(path);
    [
        ResumeArtifactCandidate {
            part: part_path(path),
            sidecars: [
                sidecar_path(path),
                legacy_binary.clone(),
                legacy_json.clone(),
            ],
        },
        ResumeArtifactCandidate {
            part: legacy_part_path(path),
            sidecars: [
                legacy_postcard_sidecar_path(path),
                legacy_binary,
                legacy_json,
            ],
        },
    ]
}

pub fn has_resume_sidecar(path: &str) -> bool {
    resume_artifact_candidates(path)
        .iter()
        .any(|candidate| candidate.sidecars.iter().any(|path| path.exists()))
}

pub(super) async fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

pub(super) async fn delete_sidecar(path: &Path) -> io::Result<()> {
    let legacy_binary_path = legacy_binary_path_for_sidecar(path);
    let legacy_json_path = legacy_json_path_for_sidecar(path);
    delete_sidecar_pair(path, &legacy_binary_path, &legacy_json_path).await
}

pub(super) async fn delete_sidecar_pair(
    path: &Path,
    legacy_binary_path: &Path,
    legacy_json_path: &Path,
) -> io::Result<()> {
    let postcard_path = postcard_path_for_sidecar(path);
    remove_file_if_exists(path).await?;
    if legacy_binary_path != path {
        remove_file_if_exists(legacy_binary_path).await?;
    }
    if legacy_json_path != path {
        remove_file_if_exists(legacy_json_path).await?;
    }
    remove_file_if_exists(&sidecar_tmp_path(&postcard_path)).await?;
    let parent = postcard_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    tokio::task::spawn_blocking(move || crate::fs::sync_directory(&parent))
        .await
        .map_err(io::Error::other)??;
    Ok(())
}

pub fn resume_sidecar_verified_bytes(path: &str) -> Option<u64> {
    let sidecar = resume_artifact_candidates(path)
        .iter()
        .find_map(|candidate| {
            load_sidecar_sync(
                &candidate.sidecars[0],
                &candidate.sidecars[1],
                &candidate.sidecars[2],
            )
        })?;
    if sidecar.version != CURRENT_RESUME_SIDECAR_VERSION {
        return None;
    }
    let boundaries = mega::mega_chunk_boundaries(sidecar.file_size);
    let mut seen = vec![false; boundaries.len()];
    Some(
        sidecar
            .verified_chunks
            .iter()
            .filter_map(|record| {
                let index = usize::try_from(record.index).ok()?;
                let boundary = boundaries.get(index)?;
                let seen_slot = seen.get_mut(index)?;
                if *seen_slot {
                    return None;
                }
                *seen_slot = true;
                Some(boundary)
            })
            .fold(0u64, |sum, chunk| sum.saturating_add(chunk.length)),
    )
}

pub(super) async fn delete_resume_artifacts_for_path(path: &str) -> io::Result<()> {
    if has_reserved_artifact_name(Path::new(path)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reserved download artifact names cannot be output paths",
        ));
    }
    remove_file_if_exists(&part_path(path)).await?;
    delete_sidecar(&sidecar_path(path)).await
}

pub(super) async fn delete_download_artifacts_for_path(path: &str) -> io::Result<()> {
    if has_reserved_artifact_name(Path::new(path)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reserved download artifact names cannot be output paths",
        ));
    }
    remove_file_if_exists(Path::new(path)).await?;
    delete_resume_artifacts_for_path(path).await
}

#[cfg(test)]
mod tests;
