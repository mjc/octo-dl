use std::io;
use std::path::{Path, PathBuf};

use super::{
    CURRENT_RESUME_SIDECAR_VERSION, legacy_binary_path_for_sidecar, legacy_json_path_for_sidecar,
    load_sidecar_sync, postcard_path_for_sidecar,
};

pub(crate) fn part_path(path: &str) -> PathBuf {
    let mut part = String::with_capacity(path.len() + ".part".len());
    part.push_str(path);
    part.push_str(".part");
    PathBuf::from(part)
}

pub(crate) fn sidecar_path(path: &str) -> PathBuf {
    let mut sidecar = String::with_capacity(path.len() + ".part.postcard".len());
    sidecar.push_str(path);
    sidecar.push_str(".part.postcard");
    PathBuf::from(sidecar)
}

pub(crate) fn legacy_binary_sidecar_path(path: &str) -> PathBuf {
    let mut sidecar = String::with_capacity(path.len() + ".part.meta.bin".len());
    sidecar.push_str(path);
    sidecar.push_str(".part.meta.bin");
    PathBuf::from(sidecar)
}

pub(crate) fn legacy_json_sidecar_path(path: &str) -> PathBuf {
    let mut sidecar = String::with_capacity(path.len() + ".part.meta.json".len());
    sidecar.push_str(path);
    sidecar.push_str(".part.meta.json");
    PathBuf::from(sidecar)
}

pub(crate) fn has_resume_sidecar(path: &str) -> bool {
    sidecar_path(path).exists()
        || legacy_binary_sidecar_path(path).exists()
        || legacy_json_sidecar_path(path).exists()
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
    remove_file_if_exists(&super::sidecar_writer::sidecar_tmp_path(&postcard_path)).await?;
    Ok(())
}

pub(crate) fn resume_sidecar_verified_bytes(path: &str) -> Option<u64> {
    let sidecar = load_sidecar_sync(
        &sidecar_path(path),
        &legacy_binary_sidecar_path(path),
        &legacy_json_sidecar_path(path),
    )?;
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
    remove_file_if_exists(&part_path(path)).await?;
    delete_sidecar_pair(
        &sidecar_path(path),
        &legacy_binary_sidecar_path(path),
        &legacy_json_sidecar_path(path),
    )
    .await
}

pub(super) async fn delete_download_artifacts_for_path(path: &str) -> io::Result<()> {
    remove_file_if_exists(Path::new(path)).await?;
    delete_resume_artifacts_for_path(path).await
}

#[cfg(test)]
mod tests;
