use super::*;
use crate::download::resume_state::CURRENT_RESUME_SIDECAR_VERSION;
use crate::download::test_support::{
    TestDownloadPaths, legacy_json_sidecar_for_chunk, sidecar_for_chunk, write_legacy_json_sidecar,
};

#[tokio::test]
async fn load_sidecar_falls_back_to_legacy_binary() {
    let paths = TestDownloadPaths::new("file.bin");
    let legacy = sidecar_for_chunk(42, [7u8; 8], 3, [4u8; 16]);

    tokio::fs::write(&paths.legacy_binary, legacy_binary_bytes(&legacy))
        .await
        .unwrap();
    let loaded = load_sidecar(&paths.sidecar).await.unwrap();

    assert_eq!(loaded.expected_condensed_mac, [7u8; 8]);
    assert_eq!(loaded.verified_chunks[0].index, 3);
    assert_eq!(loaded.verified_chunks[0].mac, [4u8; 16]);
    assert!(paths.sidecar.exists());
    assert!(!paths.legacy_binary.exists());
}

#[tokio::test]
async fn load_sidecar_falls_back_to_legacy_json() {
    let paths = TestDownloadPaths::new("file.bin");
    let legacy = legacy_json_sidecar_for_chunk(42, [9u8; 8], 7, [1u8; 16]);

    write_legacy_json_sidecar(&paths.legacy_json, &legacy)
        .await
        .unwrap();
    let loaded = load_sidecar(&paths.sidecar).await.unwrap();

    assert_eq!(loaded.expected_condensed_mac, [9u8; 8]);
    assert_eq!(loaded.verified_chunks[0].index, 7);
    assert_eq!(loaded.verified_chunks[0].mac, [1u8; 16]);
}

#[tokio::test]
async fn load_sidecar_prefers_binary_over_legacy_json() {
    let paths = TestDownloadPaths::new("file.bin");
    let legacy = legacy_json_sidecar_for_chunk(42, [1u8; 8], 0, [1u8; 16]);
    let binary = sidecar_for_chunk(42, [2u8; 8], 1, [2u8; 16]);

    write_legacy_json_sidecar(&paths.legacy_json, &legacy)
        .await
        .unwrap();
    save_sidecar_atomic(&paths.sidecar, &binary).await.unwrap();
    let loaded = load_sidecar(&paths.sidecar).await.unwrap();

    assert_eq!(loaded.expected_condensed_mac, [2u8; 8]);
    assert_eq!(loaded.verified_chunks[0].index, 1);
    assert_eq!(loaded.verified_chunks[0].mac, [2u8; 16]);
}

#[tokio::test]
async fn load_sidecar_falls_back_to_legacy_json_when_binary_is_corrupt() {
    let paths = TestDownloadPaths::new("file.bin");
    let legacy = legacy_json_sidecar_for_chunk(42, [9u8; 8], 7, [1u8; 16]);

    tokio::fs::write(&paths.sidecar, b"not-postcard")
        .await
        .unwrap();
    write_legacy_json_sidecar(&paths.legacy_json, &legacy)
        .await
        .unwrap();

    let loaded = load_sidecar(&paths.sidecar).await.unwrap();
    assert_eq!(loaded.expected_condensed_mac, [9u8; 8]);
    assert_eq!(loaded.verified_chunks[0].index, 7);
    assert_eq!(loaded.verified_chunks[0].mac, [1u8; 16]);
}

#[tokio::test]
async fn load_sidecar_falls_back_to_legacy_binary_when_postcard_is_corrupt() {
    let paths = TestDownloadPaths::new("file.bin");
    let legacy = sidecar_for_chunk(42, [5u8; 8], 6, [7u8; 16]);

    tokio::fs::write(&paths.sidecar, b"not-postcard")
        .await
        .unwrap();
    tokio::fs::write(&paths.legacy_binary, legacy_binary_bytes(&legacy))
        .await
        .unwrap();

    let loaded = load_sidecar(&paths.sidecar).await.unwrap();
    assert_eq!(loaded.expected_condensed_mac, [5u8; 8]);
    assert_eq!(loaded.verified_chunks[0].index, 6);
    assert_eq!(loaded.verified_chunks[0].mac, [7u8; 16]);
}

#[tokio::test]
async fn load_sidecar_rejects_bad_legacy_json_base64_without_allocating_vec_decode() {
    let paths = TestDownloadPaths::new("file.bin");
    let legacy = LegacyJsonResumeSidecar {
        version: CURRENT_RESUME_SIDECAR_VERSION,
        file_size: 42,
        expected_condensed_mac_b64: STANDARD.encode([9u8; 8]),
        verified_chunks: vec![LegacyJsonVerifiedChunkRecord {
            index: 0,
            mac_b64: "not-base64".to_string(),
        }],
        part_fingerprint: None,
    };

    write_legacy_json_sidecar(&paths.legacy_json, &legacy)
        .await
        .unwrap();

    assert!(load_sidecar(&paths.sidecar).await.is_none());
}

#[tokio::test]
async fn sidecar_save_writes_postcard_not_legacy_formats() {
    let paths = TestDownloadPaths::new("file.bin");
    let sidecar = sidecar_for_chunk(42, [9u8; 8], 0, [1u8; 16]);

    save_sidecar_atomic(&paths.sidecar, &sidecar).await.unwrap();
    let data = tokio::fs::read(&paths.sidecar).await.unwrap();

    assert!(postcard::from_bytes::<ResumeSidecar>(&data).is_ok());
    assert!(serde_json::from_slice::<LegacyJsonResumeSidecar>(&data).is_err());
}

#[test]
fn serialize_sidecar_uses_the_persisted_postcard_format() {
    let sidecar = sidecar_for_chunk(42, [9u8; 8], 0, [1u8; 16]);

    let data = serialize_sidecar(&sidecar).expect("sidecar should serialize");

    assert_eq!(data, postcard::to_stdvec(&sidecar).unwrap());
}

#[cfg(unix)]
#[tokio::test]
async fn sidecar_save_rejects_preexisting_temp_symlink() {
    use std::os::unix::fs::symlink;

    let paths = TestDownloadPaths::new("file.bin");
    let temp_path = sidecar_tmp_path(&paths.sidecar);
    let target_path = paths.file.with_file_name("target");
    let sidecar = sidecar_for_chunk(42, [9u8; 8], 0, [1u8; 16]);
    tokio::fs::write(&target_path, b"keep target")
        .await
        .unwrap();
    symlink(&target_path, &temp_path).unwrap();

    let error = save_sidecar_atomic(&paths.sidecar, &sidecar)
        .await
        .expect_err("a pre-existing sidecar temp symlink must be rejected");

    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(tokio::fs::read(&target_path).await.unwrap(), b"keep target");
}
