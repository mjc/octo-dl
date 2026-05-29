use super::*;
use crate::download::{legacy_binary_sidecar_path, legacy_json_sidecar_path, sidecar_path};

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
async fn load_sidecar_falls_back_to_legacy_binary() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("file.bin").to_string_lossy().into_owned();
    let postcard_path = sidecar_path(&base);
    let legacy_binary_path = legacy_binary_sidecar_path(&base);
    let legacy = ResumeSidecar {
        version: CURRENT_RESUME_SIDECAR_VERSION,
        file_size: 42,
        expected_condensed_mac: [7u8; 8],
        verified_chunks: vec![VerifiedChunkRecord {
            index: 3,
            mac: [4u8; 16],
        }],
        part_fingerprint: None,
    };

    tokio::fs::write(&legacy_binary_path, bincode::serialize(&legacy).unwrap())
        .await
        .unwrap();
    let loaded = load_sidecar(&postcard_path).await.unwrap();

    assert_eq!(loaded.expected_condensed_mac, [7u8; 8]);
    assert_eq!(loaded.verified_chunks[0].index, 3);
    assert_eq!(loaded.verified_chunks[0].mac, [4u8; 16]);
}

#[tokio::test]
async fn load_sidecar_falls_back_to_legacy_json() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("file.bin").to_string_lossy().into_owned();
    let binary_path = sidecar_path(&base);
    let json_path = legacy_json_sidecar_path(&base);
    let legacy = legacy_json_sidecar_for_chunk(42, [9u8; 8], 7, [1u8; 16]);

    write_legacy_json_sidecar(&json_path, &legacy)
        .await
        .unwrap();
    let loaded = load_sidecar(&binary_path).await.unwrap();

    assert_eq!(loaded.expected_condensed_mac, [9u8; 8]);
    assert_eq!(loaded.verified_chunks[0].index, 7);
    assert_eq!(loaded.verified_chunks[0].mac, [1u8; 16]);
}

#[tokio::test]
async fn load_sidecar_prefers_binary_over_legacy_json() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("file.bin").to_string_lossy().into_owned();
    let binary_path = sidecar_path(&base);
    let json_path = legacy_json_sidecar_path(&base);
    let legacy = legacy_json_sidecar_for_chunk(42, [1u8; 8], 0, [1u8; 16]);
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

    write_legacy_json_sidecar(&json_path, &legacy)
        .await
        .unwrap();
    save_sidecar_atomic(&binary_path, &binary).await.unwrap();
    let loaded = load_sidecar(&binary_path).await.unwrap();

    assert_eq!(loaded.expected_condensed_mac, [2u8; 8]);
    assert_eq!(loaded.verified_chunks[0].index, 1);
    assert_eq!(loaded.verified_chunks[0].mac, [2u8; 16]);
}

#[tokio::test]
async fn load_sidecar_falls_back_to_legacy_json_when_binary_is_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("file.bin").to_string_lossy().into_owned();
    let binary_path = sidecar_path(&base);
    let json_path = legacy_json_sidecar_path(&base);
    let legacy = legacy_json_sidecar_for_chunk(42, [9u8; 8], 7, [1u8; 16]);

    tokio::fs::write(&binary_path, b"not-postcard")
        .await
        .unwrap();
    write_legacy_json_sidecar(&json_path, &legacy)
        .await
        .unwrap();

    let loaded = load_sidecar(&binary_path).await.unwrap();
    assert_eq!(loaded.expected_condensed_mac, [9u8; 8]);
    assert_eq!(loaded.verified_chunks[0].index, 7);
    assert_eq!(loaded.verified_chunks[0].mac, [1u8; 16]);
}

#[tokio::test]
async fn load_sidecar_falls_back_to_legacy_binary_when_postcard_is_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("file.bin").to_string_lossy().into_owned();
    let postcard_path = sidecar_path(&base);
    let legacy_binary_path = legacy_binary_sidecar_path(&base);
    let legacy = ResumeSidecar {
        version: CURRENT_RESUME_SIDECAR_VERSION,
        file_size: 42,
        expected_condensed_mac: [5u8; 8],
        verified_chunks: vec![VerifiedChunkRecord {
            index: 6,
            mac: [7u8; 16],
        }],
        part_fingerprint: None,
    };

    tokio::fs::write(&postcard_path, b"not-postcard")
        .await
        .unwrap();
    tokio::fs::write(&legacy_binary_path, bincode::serialize(&legacy).unwrap())
        .await
        .unwrap();

    let loaded = load_sidecar(&postcard_path).await.unwrap();
    assert_eq!(loaded.expected_condensed_mac, [5u8; 8]);
    assert_eq!(loaded.verified_chunks[0].index, 6);
    assert_eq!(loaded.verified_chunks[0].mac, [7u8; 16]);
}

#[tokio::test]
async fn load_sidecar_rejects_bad_legacy_json_base64_without_allocating_vec_decode() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("file.bin").to_string_lossy().into_owned();
    let binary_path = sidecar_path(&base);
    let json_path = legacy_json_sidecar_path(&base);
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

    write_legacy_json_sidecar(&json_path, &legacy)
        .await
        .unwrap();

    assert!(load_sidecar(&binary_path).await.is_none());
}

#[tokio::test]
async fn sidecar_save_writes_binary_not_json() {
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
    let data = tokio::fs::read(&sidecar_path).await.unwrap();

    assert!(postcard::from_bytes::<ResumeSidecar>(&data).is_ok());
    assert!(bincode::deserialize::<ResumeSidecar>(&data).is_err());
    assert!(serde_json::from_slice::<LegacyJsonResumeSidecar>(&data).is_err());
}
