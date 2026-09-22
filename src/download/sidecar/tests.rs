use super::super::sidecar_store::{
    LegacyJsonResumeSidecar, LegacyJsonVerifiedChunkRecord, ResumeSidecar, VerifiedChunkRecord,
    legacy_binary_bytes, load_sidecar, save_sidecar_atomic,
};
use super::*;
use crate::download::legacy_json_sidecar_path;
use crate::download::test_support::TestDownloadPaths;
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
    let paths = TestDownloadPaths::new("file.bin");

    assert!(paths.sidecar.ends_with("file.bin.part.postcard"));
    assert!(paths.legacy_binary.ends_with("file.bin.part.meta.bin"));
    assert!(paths.legacy_json.ends_with("file.bin.part.meta.json"));
}

#[tokio::test]
async fn delete_sidecar_removes_postcard_legacy_binary_and_legacy_json() {
    let paths = TestDownloadPaths::new("file.bin");
    let binary = binary_sidecar_for_indices(42, CURRENT_RESUME_SIDECAR_VERSION, &[1]);
    let legacy = legacy_json_sidecar_for_chunk(42, [1u8; 8], 0, [1u8; 16]);

    save_sidecar_atomic(&paths.sidecar, &binary).await.unwrap();
    tokio::fs::write(&paths.legacy_binary, legacy_binary_bytes(&binary))
        .await
        .unwrap();
    write_legacy_json_sidecar(&paths.legacy_json, &legacy)
        .await
        .unwrap();
    delete_sidecar(&paths.sidecar).await.unwrap();

    assert!(!paths.sidecar.exists());
    assert!(!paths.legacy_binary.exists());
    assert!(!paths.legacy_json.exists());
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
            let paths = TestDownloadPaths::new("file.bin");
            let file_path = paths.file_string;
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
            let paths = TestDownloadPaths::new("file.bin");
            let file_path = paths.file_string;
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
            let paths = TestDownloadPaths::new("file.bin");
            let file_path = paths.file_string;
            let sidecar = binary_sidecar_for_indices(file_size, version, &indices);
            write_postcard_sidecar_sync(&sidecar_path(&file_path), &sidecar).unwrap();

            prop_assert_eq!(resume_sidecar_verified_bytes(&file_path), None);
        }
    }
}

#[tokio::test]
async fn delete_resume_artifacts_removes_part_and_all_sidecars() {
    let paths = TestDownloadPaths::new("file.bin");
    tokio::fs::write(&paths.part, b"partial").await.unwrap();
    tokio::fs::write(&paths.sidecar, b"{}").await.unwrap();
    tokio::fs::write(&paths.legacy_binary, b"{}").await.unwrap();
    tokio::fs::write(&paths.legacy_json, b"{}").await.unwrap();

    delete_resume_artifacts_for_path(&paths.file_string)
        .await
        .unwrap();

    assert!(!paths.part.exists());
    assert!(!paths.sidecar.exists());
    assert!(!paths.legacy_binary.exists());
    assert!(!paths.legacy_json.exists());
}

#[tokio::test]
async fn delete_resume_artifacts_removes_postcard_tmp_leftovers() {
    let paths = TestDownloadPaths::new("file.bin");
    let tmp_path = super::super::sidecar_writer::sidecar_tmp_path(&paths.sidecar);
    tokio::fs::write(&tmp_path, b"tmp").await.unwrap();

    delete_resume_artifacts_for_path(&paths.file_string)
        .await
        .unwrap();

    assert!(!tmp_path.exists());
}

#[tokio::test]
async fn delete_download_artifacts_removes_postcard_tmp_leftovers() {
    let paths = TestDownloadPaths::new("file.bin");
    let tmp_path = super::super::sidecar_writer::sidecar_tmp_path(&paths.sidecar);
    tokio::fs::write(&paths.file, b"final").await.unwrap();
    tokio::fs::write(&tmp_path, b"tmp").await.unwrap();

    delete_download_artifacts_for_path(&paths.file_string)
        .await
        .unwrap();

    assert!(!paths.file.exists());
    assert!(!tmp_path.exists());
}

#[tokio::test]
async fn delete_sidecar_removes_postcard_tmp_leftovers() {
    let paths = TestDownloadPaths::new("file.bin");
    let tmp_path = super::super::sidecar_writer::sidecar_tmp_path(&paths.sidecar);
    tokio::fs::write(&paths.sidecar, b"{}").await.unwrap();
    tokio::fs::write(&tmp_path, b"tmp").await.unwrap();

    delete_sidecar(&paths.sidecar).await.unwrap();

    assert!(!paths.sidecar.exists());
    assert!(!tmp_path.exists());
}

#[tokio::test]
async fn sidecar_save_and_delete_round_trip() {
    let paths = TestDownloadPaths::new("file.bin");
    let sidecar = binary_sidecar_for_indices(42, CURRENT_RESUME_SIDECAR_VERSION, &[0]);

    save_sidecar_atomic(&paths.sidecar, &sidecar).await.unwrap();
    let loaded = load_sidecar(&paths.sidecar).await.unwrap();
    assert_eq!(loaded.file_size, sidecar.file_size);
    assert_eq!(loaded.verified_chunks.len(), 1);

    delete_sidecar(&paths.sidecar).await.unwrap();
    assert!(load_sidecar(&paths.sidecar).await.is_none());
}
