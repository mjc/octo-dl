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

async fn write_legacy_json_sidecar(
    path: &Path,
    sidecar: &LegacyJsonResumeSidecar,
) -> io::Result<()> {
    let data = serde_json::to_vec(sidecar)?;
    tokio::fs::write(path, data).await
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
    let binary = ResumeSidecar {
        version: CURRENT_RESUME_SIDECAR_VERSION,
        file_size: 42,
        expected_condensed_mac: [2u8; 8],
        verified_chunks: vec![VerifiedChunkRecord {
            index: 1,
            mac: [2u8; 16],
        }],
        part_fingerprint: None,
    };
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

#[tokio::test]
async fn resume_sidecar_verified_bytes_reads_legacy_json_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let base_path = dir.path().join("file.bin");
    let file_path = base_path.to_string_lossy().into_owned();
    let file_size = 300_000_u64;
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let legacy = LegacyJsonResumeSidecar {
        verified_chunks: vec![
            LegacyJsonVerifiedChunkRecord {
                index: boundaries[0].index,
                mac_b64: STANDARD.encode([1u8; 16]),
            },
            LegacyJsonVerifiedChunkRecord {
                index: boundaries[1].index,
                mac_b64: STANDARD.encode([2u8; 16]),
            },
        ],
        ..legacy_json_sidecar_for_chunk(file_size, [9u8; 8], boundaries[0].index, [1u8; 16])
    };

    write_legacy_json_sidecar(&legacy_json_sidecar_path(&file_path), &legacy)
        .await
        .unwrap();

    assert_eq!(
        resume_sidecar_verified_bytes(&file_path),
        Some(boundaries[0].length + boundaries[1].length)
    );
}

#[tokio::test]
async fn resume_sidecar_verified_bytes_dedupes_duplicate_binary_chunk_records() {
    let dir = tempfile::tempdir().unwrap();
    let base_path = dir.path().join("file.bin");
    let file_path = base_path.to_string_lossy().into_owned();
    let sidecar_path = sidecar_path(&file_path);
    let file_size = 300_000_u64;
    let boundaries = mega::mega_chunk_boundaries(file_size);

    save_sidecar_atomic(
        &sidecar_path,
        &ResumeSidecar {
            version: CURRENT_RESUME_SIDECAR_VERSION,
            file_size,
            expected_condensed_mac: [9u8; 8],
            verified_chunks: vec![
                VerifiedChunkRecord {
                    index: boundaries[0].index,
                    mac: [1u8; 16],
                },
                VerifiedChunkRecord {
                    index: boundaries[0].index,
                    mac: [2u8; 16],
                },
                VerifiedChunkRecord {
                    index: boundaries[1].index,
                    mac: [3u8; 16],
                },
            ],
            part_fingerprint: None,
        },
    )
    .await
    .unwrap();

    assert_eq!(
        resume_sidecar_verified_bytes(&file_path),
        Some(boundaries[0].length + boundaries[1].length)
    );
}

#[tokio::test]
async fn resume_sidecar_verified_bytes_dedupes_duplicate_legacy_chunk_records() {
    let dir = tempfile::tempdir().unwrap();
    let base_path = dir.path().join("file.bin");
    let file_path = base_path.to_string_lossy().into_owned();
    let file_size = 300_000_u64;
    let boundaries = mega::mega_chunk_boundaries(file_size);
    let legacy = LegacyJsonResumeSidecar {
        verified_chunks: vec![
            LegacyJsonVerifiedChunkRecord {
                index: boundaries[0].index,
                mac_b64: STANDARD.encode([1u8; 16]),
            },
            LegacyJsonVerifiedChunkRecord {
                index: boundaries[0].index,
                mac_b64: STANDARD.encode([2u8; 16]),
            },
            LegacyJsonVerifiedChunkRecord {
                index: boundaries[1].index,
                mac_b64: STANDARD.encode([3u8; 16]),
            },
        ],
        ..legacy_json_sidecar_for_chunk(file_size, [9u8; 8], boundaries[0].index, [1u8; 16])
    };

    write_legacy_json_sidecar(&legacy_json_sidecar_path(&file_path), &legacy)
        .await
        .unwrap();

    assert_eq!(
        resume_sidecar_verified_bytes(&file_path),
        Some(boundaries[0].length + boundaries[1].length)
    );
}

#[tokio::test]
async fn resume_sidecar_verified_bytes_sums_verified_chunk_lengths() {
    let dir = tempfile::tempdir().unwrap();
    let base_path = dir.path().join("file.bin");
    let file_path = base_path.to_string_lossy().into_owned();
    let sidecar_path = sidecar_path(&file_path);
    let file_size = 300_000_u64;
    let boundaries = mega::mega_chunk_boundaries(file_size);

    save_sidecar_atomic(
        &sidecar_path,
        &ResumeSidecar {
            version: CURRENT_RESUME_SIDECAR_VERSION,
            file_size,
            expected_condensed_mac: [9u8; 8],
            verified_chunks: vec![
                VerifiedChunkRecord {
                    index: boundaries[0].index,
                    mac: [1u8; 16],
                },
                VerifiedChunkRecord {
                    index: boundaries[1].index,
                    mac: [2u8; 16],
                },
            ],
            part_fingerprint: None,
        },
    )
    .await
    .unwrap();

    assert_eq!(
        resume_sidecar_verified_bytes(&file_path),
        Some(boundaries[0].length + boundaries[1].length)
    );
}

#[tokio::test]
async fn resume_sidecar_verified_bytes_returns_none_for_legacy_sidecars() {
    let dir = tempfile::tempdir().unwrap();
    let base_path = dir.path().join("file.bin");
    let file_path = base_path.to_string_lossy().into_owned();
    let sidecar_path = sidecar_path(&file_path);

    save_sidecar_atomic(
        &sidecar_path,
        &ResumeSidecar {
            version: 1,
            file_size: 300_000,
            expected_condensed_mac: [9u8; 8],
            verified_chunks: vec![VerifiedChunkRecord {
                index: 0,
                mac: [1u8; 16],
            }],
            part_fingerprint: None,
        },
    )
    .await
    .unwrap();

    assert_eq!(resume_sidecar_verified_bytes(&file_path), None);
}

#[tokio::test]
async fn resume_sidecar_verified_bytes_treats_unknown_version_as_missing() {
    let dir = tempfile::tempdir().unwrap();
    let base_path = dir.path().join("file.bin");
    let file_path = base_path.to_string_lossy().into_owned();
    let sidecar_path = sidecar_path(&file_path);

    save_sidecar_atomic(
        &sidecar_path,
        &ResumeSidecar {
            version: 1,
            file_size: 300_000,
            expected_condensed_mac: [9u8; 8],
            verified_chunks: vec![VerifiedChunkRecord {
                index: 0,
                mac: [1u8; 16],
            }],
            part_fingerprint: None,
        },
    )
    .await
    .unwrap();

    assert_eq!(resume_sidecar_verified_bytes(&file_path), None);
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
    let sidecar = ResumeSidecar {
        version: CURRENT_RESUME_SIDECAR_VERSION,
        file_size: 42,
        expected_condensed_mac: [9u8; 8],
        verified_chunks: vec![VerifiedChunkRecord {
            index: 0,
            mac: [1u8; 16],
        }],
        part_fingerprint: None,
    };

    save_sidecar_atomic(&sidecar_path, &sidecar).await.unwrap();
    let loaded = load_sidecar(&sidecar_path).await.unwrap();
    assert_eq!(loaded.file_size, sidecar.file_size);
    assert_eq!(loaded.verified_chunks.len(), 1);

    delete_sidecar(&sidecar_path).await.unwrap();
    assert!(load_sidecar(&sidecar_path).await.is_none());
}
