use super::super::{
    LegacyJsonResumeSidecar, LegacyJsonVerifiedChunkRecord, ResumeSidecar, VerifiedChunkRecord,
    load_sidecar, save_sidecar_atomic,
};
use super::*;
use crate::download::{legacy_binary_sidecar_path, legacy_json_sidecar_path};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;

fn legacy_json_sidecar_for_chunk(
    file_size: u64,
    expected_condensed_mac: [u8; 8],
    index: u32,
    mac: [u8; 16],
) -> LegacyJsonResumeSidecar {
    LegacyJsonResumeSidecar {
        version: CURRENT_RESUME_SIDECAR_VERSION,
        file_size,
        expected_condensed_mac_b64: STANDARD.encode(expected_condensed_mac),
        verified_chunks: vec![LegacyJsonVerifiedChunkRecord {
            index,
            mac_b64: STANDARD.encode(mac),
        }],
        part_fingerprint: None,
    }
}

fn legacy_json_sidecar_for_indices(file_size: u64, indices: &[u32]) -> LegacyJsonResumeSidecar {
    LegacyJsonResumeSidecar {
        version: CURRENT_RESUME_SIDECAR_VERSION,
        file_size,
        expected_condensed_mac_b64: STANDARD.encode([9u8; 8]),
        verified_chunks: indices
            .iter()
            .enumerate()
            .map(|(offset, &index)| LegacyJsonVerifiedChunkRecord {
                index,
                mac_b64: STANDARD.encode([offset as u8; 16]),
            })
            .collect(),
        part_fingerprint: None,
    }
}

fn binary_sidecar_for_indices(file_size: u64, version: u32, indices: &[u32]) -> ResumeSidecar {
    ResumeSidecar {
        version,
        file_size,
        expected_condensed_mac: [9u8; 8],
        verified_chunks: indices
            .iter()
            .enumerate()
            .map(|(offset, &index)| VerifiedChunkRecord {
                index,
                mac: [offset as u8; 16],
            })
            .collect(),
        part_fingerprint: None,
    }
}

async fn write_legacy_json_sidecar(
    path: &Path,
    sidecar: &LegacyJsonResumeSidecar,
) -> io::Result<()> {
    let data = serde_json::to_vec(sidecar)?;
    tokio::fs::write(path, data).await
}

fn write_postcard_sidecar_sync(path: &Path, sidecar: &ResumeSidecar) -> io::Result<()> {
    let data = postcard::to_stdvec(sidecar).map_err(io::Error::other)?;
    std::fs::write(path, data)
}

fn write_legacy_json_sidecar_sync(
    path: &Path,
    sidecar: &LegacyJsonResumeSidecar,
) -> io::Result<()> {
    let data = serde_json::to_vec(sidecar)?;
    std::fs::write(path, data)
}

fn expected_verified_bytes(file_size: u64, indices: &[u32]) -> u64 {
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let mut seen = vec![false; boundaries.len()];
    indices
        .iter()
        .filter_map(|&index| {
            let index = usize::try_from(index).ok()?;
            let boundary = boundaries.get(index)?;
            let seen_slot = seen.get_mut(index)?;
            if *seen_slot {
                return None;
            }
            *seen_slot = true;
            Some(boundary.length)
        })
        .fold(0u64, |sum, chunk| sum.saturating_add(chunk))
}

#[tokio::test]
async fn part_path_appends_extension() {
    assert_eq!(part_path("foo/bar.zip"), PathBuf::from("foo/bar.zip.part"));
    assert_eq!(part_path("file.txt"), PathBuf::from("file.txt.part"));
}

#[tokio::test]
async fn sidecar_path_uses_postcard_extension_and_legacy_paths_remain_available() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("file.bin").to_string_lossy().into_owned();

    assert!(sidecar_path(&base).ends_with("file.bin.part.postcard"));
    assert!(legacy_binary_sidecar_path(&base).ends_with("file.bin.part.meta.bin"));
    assert!(legacy_json_sidecar_path(&base).ends_with("file.bin.part.meta.json"));
}

#[tokio::test]
async fn delete_sidecar_removes_postcard_legacy_binary_and_legacy_json() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("file.bin").to_string_lossy().into_owned();
    let binary_path = sidecar_path(&base);
    let legacy_binary_path = legacy_binary_sidecar_path(&base);
    let json_path = legacy_json_sidecar_path(&base);
    let binary = binary_sidecar_for_indices(42, CURRENT_RESUME_SIDECAR_VERSION, &[1]);
    let legacy = legacy_json_sidecar_for_chunk(42, [1u8; 8], 0, [1u8; 16]);

    save_sidecar_atomic(&binary_path, &binary).await.unwrap();
    tokio::fs::write(&legacy_binary_path, bincode::serialize(&binary).unwrap())
        .await
        .unwrap();
    write_legacy_json_sidecar(&json_path, &legacy)
        .await
        .unwrap();
    delete_sidecar(&binary_path).await.unwrap();

    assert!(!binary_path.exists());
    assert!(!legacy_binary_path.exists());
    assert!(!json_path.exists());
}

mod property_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn resume_sidecar_verified_bytes_matches_binary_chunk_oracle(
            file_size in 1u64..3_000_001,
            indices in proptest::collection::vec(0u32..32, 0..16),
        ) {
            let dir = tempfile::tempdir().unwrap();
            let file_path = dir.path().join("file.bin").to_string_lossy().into_owned();
            let sidecar = binary_sidecar_for_indices(file_size, CURRENT_RESUME_SIDECAR_VERSION, &indices);
            write_postcard_sidecar_sync(&sidecar_path(&file_path), &sidecar).unwrap();

            prop_assert_eq!(
                resume_sidecar_verified_bytes(&file_path),
                Some(expected_verified_bytes(file_size, &indices))
            );
        }

        #[test]
        fn resume_sidecar_verified_bytes_matches_legacy_json_chunk_oracle(
            file_size in 1u64..3_000_001,
            indices in proptest::collection::vec(0u32..32, 0..16),
        ) {
            let dir = tempfile::tempdir().unwrap();
            let file_path = dir.path().join("file.bin").to_string_lossy().into_owned();
            let sidecar = legacy_json_sidecar_for_indices(file_size, &indices);
            write_legacy_json_sidecar_sync(&legacy_json_sidecar_path(&file_path), &sidecar).unwrap();

            prop_assert_eq!(
                resume_sidecar_verified_bytes(&file_path),
                Some(expected_verified_bytes(file_size, &indices))
            );
        }

        #[test]
        fn resume_sidecar_verified_bytes_returns_none_for_non_current_versions(
            version in any::<u32>(),
            file_size in 1u64..3_000_001,
            indices in proptest::collection::vec(0u32..32, 0..16),
        ) {
            prop_assume!(version != CURRENT_RESUME_SIDECAR_VERSION);
            let dir = tempfile::tempdir().unwrap();
            let file_path = dir.path().join("file.bin").to_string_lossy().into_owned();
            let sidecar = binary_sidecar_for_indices(file_size, version, &indices);
            write_postcard_sidecar_sync(&sidecar_path(&file_path), &sidecar).unwrap();

            prop_assert_eq!(resume_sidecar_verified_bytes(&file_path), None);
        }
    }
}

#[tokio::test]
async fn delete_resume_artifacts_removes_part_and_all_sidecars() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file.bin");
    let path_str = path.to_string_lossy().to_string();
    tokio::fs::write(part_path(&path_str), b"partial")
        .await
        .unwrap();
    tokio::fs::write(sidecar_path(&path_str), b"{}")
        .await
        .unwrap();
    tokio::fs::write(legacy_binary_sidecar_path(&path_str), b"{}")
        .await
        .unwrap();
    tokio::fs::write(legacy_json_sidecar_path(&path_str), b"{}")
        .await
        .unwrap();

    delete_resume_artifacts_for_path(&path_str).await.unwrap();

    assert!(!part_path(&path_str).exists());
    assert!(!sidecar_path(&path_str).exists());
    assert!(!legacy_binary_sidecar_path(&path_str).exists());
    assert!(!legacy_json_sidecar_path(&path_str).exists());
}

#[tokio::test]
async fn delete_resume_artifacts_removes_postcard_tmp_leftovers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file.bin");
    let path_str = path.to_string_lossy().to_string();
    let tmp_path = super::super::sidecar_writer::sidecar_tmp_path(&sidecar_path(&path_str));
    tokio::fs::write(&tmp_path, b"tmp").await.unwrap();

    delete_resume_artifacts_for_path(&path_str).await.unwrap();

    assert!(!tmp_path.exists());
}

#[tokio::test]
async fn delete_download_artifacts_removes_postcard_tmp_leftovers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file.bin");
    let path_str = path.to_string_lossy().to_string();
    let tmp_path = super::super::sidecar_writer::sidecar_tmp_path(&sidecar_path(&path_str));
    tokio::fs::write(&path, b"final").await.unwrap();
    tokio::fs::write(&tmp_path, b"tmp").await.unwrap();

    delete_download_artifacts_for_path(&path_str).await.unwrap();

    assert!(!path.exists());
    assert!(!tmp_path.exists());
}

#[tokio::test]
async fn delete_sidecar_removes_postcard_tmp_leftovers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file.bin");
    let path_str = path.to_string_lossy().to_string();
    let sidecar = sidecar_path(&path_str);
    let tmp_path = super::super::sidecar_writer::sidecar_tmp_path(&sidecar);
    tokio::fs::write(&sidecar, b"{}").await.unwrap();
    tokio::fs::write(&tmp_path, b"tmp").await.unwrap();

    delete_sidecar(&sidecar).await.unwrap();

    assert!(!sidecar.exists());
    assert!(!tmp_path.exists());
}

#[tokio::test]
async fn sidecar_save_and_delete_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let sidecar_path = dir.path().join("file.bin.part.postcard");
    let sidecar = binary_sidecar_for_indices(42, CURRENT_RESUME_SIDECAR_VERSION, &[0]);

    save_sidecar_atomic(&sidecar_path, &sidecar).await.unwrap();
    let loaded = load_sidecar(&sidecar_path).await.unwrap();
    assert_eq!(loaded.file_size, sidecar.file_size);
    assert_eq!(loaded.verified_chunks.len(), 1);

    delete_sidecar(&sidecar_path).await.unwrap();
    assert!(load_sidecar(&sidecar_path).await.is_none());
}
