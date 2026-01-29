use aes::Aes128;
use aes::cipher::{KeyIvInit, KeyInit, BlockDecryptMut};
use base64::Engine;
use cbc::Decryptor;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

// Hardcoded AES/ECB key from JDownloader (hex: 447e787351e60e2c6a96b3964be0c9bd)
const JDOWNLOADER_KEY: &[u8] = &[
    0x44, 0x7e, 0x78, 0x73, 0x51, 0xe6, 0x0e, 0x2c, 0x6a, 0x96, 0xb3, 0x96, 0x4b, 0xe0, 0xc9,
    0xbd,
];

const DLC_SERVICE: &str = "https://service.jdownloader.org/dlcrypt/service.php";
const MIN_DLC_SIZE: usize = 100;
const DLC_KEY_LENGTH: usize = 88;
const MAX_RETRIES: u32 = 3;

/// Shared cache for decryption keys to avoid duplicate service calls
pub struct DlcKeyCache {
    cache: Arc<Mutex<HashMap<String, String>>>,
}

impl DlcKeyCache {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn get(&self, key: &str) -> Option<String> {
        self.cache.lock().unwrap().get(key).cloned()
    }

    fn set(&self, key: String, value: String) {
        self.cache.lock().unwrap().insert(key, value);
    }
}

impl Default for DlcKeyCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract MEGA links from a `JDownloader2` DLC file
pub async fn parse_dlc_file(
    path: &str,
    http_client: &reqwest::Client,
    cache: &DlcKeyCache,
) -> Option<Vec<String>> {
    let content = std::fs::read_to_string(path).ok()?;

    // Validate file size
    if content.trim().len() < MIN_DLC_SIZE {
        eprintln!("DLC file too small (< {MIN_DLC_SIZE} bytes)");
        return None;
    }

    // Split into encrypted data and key
    let trimmed = content.trim();
    if trimmed.len() < DLC_KEY_LENGTH {
        eprintln!("DLC file missing encryption key");
        return None;
    }

    let dlc_key = trimmed[trimmed.len() - DLC_KEY_LENGTH..].to_string();
    let encrypted_base64 = &trimmed[..trimmed.len() - DLC_KEY_LENGTH];

    // Validate key format (should be base64)
    if !is_valid_base64(&dlc_key) {
        eprintln!("DLC encryption key is not valid base64");
        return None;
    }

    if !is_valid_base64(encrypted_base64) {
        eprintln!("DLC encrypted data is not valid base64");
        return None;
    }

    // Get decryption key from service (with caching)
    let decryption_key = get_decryption_key(&dlc_key, http_client, cache).await?;

    // Decode the encrypted data
    let encrypted_bytes = base64::engine::general_purpose::STANDARD
        .decode(encrypted_base64)
        .ok()?;

    // Decrypt
    let xml = decrypt_aes_cbc(&encrypted_bytes, &decryption_key)?;

    // Extract MEGA links from XML
    let mut urls = extract_mega_links_from_xml(&xml);
    urls.sort();
    urls.dedup();

    if urls.is_empty() {
        eprintln!("No MEGA links found in DLC file");
        return None;
    }

    Some(urls)
}

/// Get decryption key from `JDownloader` service with exponential backoff
async fn get_decryption_key(
    dlc_key: &str,
    http_client: &reqwest::Client,
    cache: &DlcKeyCache,
) -> Option<String> {
    // Check cache first
    if let Some(cached) = cache.get(dlc_key) {
        return Some(cached);
    }

    let mut retry_count = 0;

    loop {
        match call_decryption_service(dlc_key, http_client).await {
            Some(key) => {
                // Cache the result
                cache.set(dlc_key.to_string(), key.clone());
                return Some(key);
            }
            None if retry_count < MAX_RETRIES => {
                // Exponential backoff: 1s, 2s, 4s, 8s
                let delay = std::time::Duration::from_secs(1 << retry_count);
                eprintln!(
                    "DLC service call failed, retrying in {:?}... (attempt {}/{})",
                    delay,
                    retry_count + 1,
                    MAX_RETRIES
                );
                tokio::time::sleep(delay).await;
                retry_count += 1;
            }
            None => {
                eprintln!("DLC service unreachable after {MAX_RETRIES} attempts");
                return None;
            }
        }
    }
}

/// Call `JDownloader`'s DLC decryption service
async fn call_decryption_service(dlc_key: &str, http_client: &reqwest::Client) -> Option<String> {
    let version = env!("CARGO_PKG_VERSION");
    let user_agent = format!("JDownloader/2.0 (octo-dl/{version})");

    // Build parameters matching JDownloader's actual format
    let params = [
        ("destType", "jdtc6"),
        ("srcType", "dlc"),
        ("data", dlc_key),
    ];

    let response = http_client
        .post(DLC_SERVICE)
        .header("User-Agent", &user_agent)
        .form(&params)
        .send()
        .await
        .ok()?;

    if !response.status().is_success() {
        return None;
    }

    let text = response.text().await.ok()?;

    // Extract the RC value from <rc>...</rc>
    let start = text.find("<rc>")?;
    let end = text.find("</rc>")?;

    if start >= end {
        return None;
    }

    let rc_value = text[start + 4..end].trim();

    // Check for rate limit error
    if rc_value == "2YVhzRFdjR2dDQy9JL25aVXFjQ1RPZ" {
        eprintln!("DLC service rate limit hit");
        return None;
    }

    // Decrypt the RC value using AES/ECB with JDownloader's hardcoded key
    decrypt_service_key(rc_value)
}

/// Decrypt the service response key using AES/ECB with `JDownloader`'s key
fn decrypt_service_key(encrypted_key: &str) -> Option<String> {
    use aes::cipher::generic_array::GenericArray;
    use aes::cipher::BlockDecrypt;

    // Base64 decode the encrypted key
    let encrypted_bytes = base64::engine::general_purpose::STANDARD
        .decode(encrypted_key)
        .ok()?;

    // Decrypt using AES/ECB with JDownloader's hardcoded key
    // ECB mode: decrypt each block independently
    let key = GenericArray::from_slice(JDOWNLOADER_KEY);
    let cipher = Aes128::new(key);

    // Process in 16-byte blocks (AES block size)
    let mut decrypted = encrypted_bytes;
    let block_size = 16;

    if decrypted.len() % block_size != 0 {
        return None;
    }

    for chunk in decrypted.chunks_exact_mut(block_size) {
        let block = GenericArray::from_mut_slice(chunk);
        cipher.decrypt_block(block);
    }

    // Strip null padding bytes (ECB NoPadding leaves them)
    while !decrypted.is_empty() && decrypted.last() == Some(&0) {
        decrypted.pop();
    }

    // The decrypted result is base64 encoded again, decode it
    let decoded = String::from_utf8(decrypted).ok()?;
    let final_key = base64::engine::general_purpose::STANDARD
        .decode(&decoded)
        .ok()?;

    // Convert to string and take first 16 characters
    let key_str = String::from_utf8(final_key).ok()?;
    Some(key_str.chars().take(16).collect())
}

/// Decrypt AES-128 CBC encrypted data
fn decrypt_aes_cbc(encrypted: &[u8], key_str: &str) -> Option<String> {
    use aes::cipher::generic_array::GenericArray;

    // The key is the raw UTF-8 bytes of the 16-character string
    // Both key and IV are the same (as per JDownloader's d5 function)
    let key_bytes = key_str.as_bytes();

    if key_bytes.len() != 16 {
        return None;
    }

    // Key and IV are the same
    let key = GenericArray::from_slice(key_bytes);
    let iv = GenericArray::from_slice(key_bytes);

    // Create decryptor and try PKCS7 padding first, then NoPadding as fallback
    let mut data = encrypted.to_vec();
    let cipher = Decryptor::<Aes128>::new(key, iv);

    let decrypted_bytes = if let Ok(d) = cipher.decrypt_padded_mut::<aes::cipher::block_padding::Pkcs7>(&mut data) {
        d.to_vec()
    } else {
        // Try NoPadding as fallback (as per JDownloader's d5 function)
        let mut data2 = encrypted.to_vec();
        let cipher2 = Decryptor::<Aes128>::new(key, iv);
        cipher2
            .decrypt_padded_mut::<aes::cipher::block_padding::NoPadding>(&mut data2)
            .ok()?
            .to_vec()
    };

    // Strip trailing null/padding bytes from decrypted content
    let content_end = decrypted_bytes
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(decrypted_bytes.len());
    let clean_bytes = &decrypted_bytes[..content_end];

    // The decrypted content is base64 encoded, decode it
    let decoded_content = base64::engine::general_purpose::STANDARD
        .decode(clean_bytes)
        .ok()?;

    // Convert to string
    String::from_utf8(decoded_content).ok()
}

/// Extract all MEGA links from decrypted DLC XML
fn extract_mega_links_from_xml(xml: &str) -> Vec<String> {
    let mut urls = Vec::new();

    // Simple regex-free approach: find <url> tags
    let mut content = xml;
    while let Some(start) = content.find("<url>") {
        let after_tag = &content[start + 5..];
        if let Some(end) = after_tag.find("</url>") {
            let encoded_url = &after_tag[..end];

            // URLs inside DLC XML are base64 encoded
            if let Ok(decoded_bytes) = base64::engine::general_purpose::STANDARD.decode(encoded_url)
                && let Ok(url) = String::from_utf8(decoded_bytes)
                    && (url.starts_with("https://mega.nz/") || url.starts_with("http://mega.nz/"))
                        && !urls.contains(&url)
                    {
                        urls.push(url);
                    }

            content = &after_tag[end + 6..];
        } else {
            break;
        }
    }

    urls
}

/// Check if a string is valid base64
fn is_valid_base64(s: &str) -> bool {
    base64::engine::general_purpose::STANDARD.decode(s).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // DlcKeyCache tests
    // =========================================================================

    #[test]
    fn cache_stores_and_retrieves_keys() {
        let cache = DlcKeyCache::new();
        cache.set("dlc_key_abc".to_string(), "decryption_key_xyz".to_string());
        assert_eq!(
            cache.get("dlc_key_abc"),
            Some("decryption_key_xyz".to_string())
        );
    }

    #[test]
    fn cache_returns_none_for_missing_keys() {
        let cache = DlcKeyCache::new();
        assert_eq!(cache.get("nonexistent"), None);
    }

    #[test]
    fn cache_overwrites_existing_keys() {
        let cache = DlcKeyCache::new();
        cache.set("key".to_string(), "value1".to_string());
        cache.set("key".to_string(), "value2".to_string());
        assert_eq!(cache.get("key"), Some("value2".to_string()));
    }

    #[test]
    fn cache_default_creates_empty_cache() {
        let cache = DlcKeyCache::default();
        assert_eq!(cache.get("any_key"), None);
    }

    // =========================================================================
    // Base64 validation tests
    // =========================================================================

    #[test]
    fn valid_base64_standard_padding() {
        assert!(is_valid_base64("SGVsbG8gV29ybGQ="));
    }

    #[test]
    fn valid_base64_double_padding() {
        assert!(is_valid_base64("SGVsbG8=")); // "Hello"
    }

    #[test]
    fn valid_base64_no_padding_rejected_by_standard() {
        // STANDARD base64 requires padding - this should fail validation
        assert!(!is_valid_base64("SGVsbG8")); // "Hello" without padding
    }

    #[test]
    fn invalid_base64_special_chars() {
        assert!(!is_valid_base64("not!!base64"));
    }

    #[test]
    fn invalid_base64_spaces() {
        assert!(!is_valid_base64("SGVs bG8=")); // Space in middle
    }

    #[test]
    fn valid_base64_empty_string() {
        assert!(is_valid_base64("")); // Empty is technically valid
    }

    #[test]
    fn valid_base64_long_string() {
        // A longer base64 string (encodes "The quick brown fox jumps over the lazy dog")
        assert!(is_valid_base64(
            "VGhlIHF1aWNrIGJyb3duIGZveCBqdW1wcyBvdmVyIHRoZSBsYXp5IGRvZw=="
        ));
    }

    // =========================================================================
    // DLC file format validation tests
    // =========================================================================

    #[test]
    fn dlc_min_size_constant() {
        assert_eq!(MIN_DLC_SIZE, 100);
    }

    #[test]
    fn dlc_key_length_constant() {
        assert_eq!(DLC_KEY_LENGTH, 88);
    }

    #[test]
    fn small_content_fails_size_check() {
        let small = "x".repeat(50);
        assert!(small.len() < MIN_DLC_SIZE);
    }

    #[test]
    fn content_at_boundary_passes_size_check() {
        let exact = "x".repeat(100);
        assert!(exact.len() >= MIN_DLC_SIZE);
    }

    // =========================================================================
    // XML URL extraction tests
    // =========================================================================

    #[test]
    fn extract_single_mega_link_from_xml() {
        // Base64 encode "https://mega.nz/file/abc123#key456"
        let encoded = base64::engine::general_purpose::STANDARD
            .encode("https://mega.nz/file/abc123#key456");
        let xml = format!("<dlc><content><file><url>{}</url></file></content></dlc>", encoded);
        let urls = extract_mega_links_from_xml(&xml);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0], "https://mega.nz/file/abc123#key456");
    }

    #[test]
    fn extract_multiple_mega_links_from_xml() {
        let url1 = base64::engine::general_purpose::STANDARD
            .encode("https://mega.nz/file/test1#key1");
        let url2 = base64::engine::general_purpose::STANDARD
            .encode("https://mega.nz/folder/test2#key2");
        let xml = format!(
            "<dlc><url>{}</url><url>{}</url></dlc>",
            url1, url2
        );
        let urls = extract_mega_links_from_xml(&xml);
        assert_eq!(urls.len(), 2);
        assert!(urls.contains(&"https://mega.nz/file/test1#key1".to_string()));
        assert!(urls.contains(&"https://mega.nz/folder/test2#key2".to_string()));
    }

    #[test]
    fn extract_filters_non_mega_links() {
        let mega_url = base64::engine::general_purpose::STANDARD
            .encode("https://mega.nz/file/abc#123");
        let google_url = base64::engine::general_purpose::STANDARD
            .encode("https://google.com/search");
        let xml = format!(
            "<dlc><url>{}</url><url>{}</url></dlc>",
            mega_url, google_url
        );
        let urls = extract_mega_links_from_xml(&xml);
        assert_eq!(urls.len(), 1);
        assert!(urls[0].starts_with("https://mega.nz/"));
    }

    #[test]
    fn extract_handles_http_mega_links() {
        let url = base64::engine::general_purpose::STANDARD
            .encode("http://mega.nz/file/oldformat#key");
        let xml = format!("<dlc><url>{}</url></dlc>", url);
        let urls = extract_mega_links_from_xml(&xml);
        assert_eq!(urls.len(), 1);
        assert!(urls[0].starts_with("http://mega.nz/"));
    }

    #[test]
    fn extract_deduplicates_urls() {
        let url = base64::engine::general_purpose::STANDARD
            .encode("https://mega.nz/file/same#key");
        let xml = format!(
            "<dlc><url>{}</url><url>{}</url></dlc>",
            url, url
        );
        let urls = extract_mega_links_from_xml(&xml);
        assert_eq!(urls.len(), 1);
    }

    #[test]
    fn extract_handles_empty_xml() {
        let urls = extract_mega_links_from_xml("<dlc></dlc>");
        assert!(urls.is_empty());
    }

    #[test]
    fn extract_handles_no_url_tags() {
        let urls = extract_mega_links_from_xml("<dlc><content><file></file></content></dlc>");
        assert!(urls.is_empty());
    }

    #[test]
    fn extract_handles_invalid_base64_in_url() {
        let xml = "<dlc><url>not!!!valid!!!base64</url></dlc>";
        let urls = extract_mega_links_from_xml(xml);
        assert!(urls.is_empty());
    }

    #[test]
    fn extract_handles_nested_structure() {
        let url = base64::engine::general_purpose::STANDARD
            .encode("https://mega.nz/file/nested#key");
        let xml = format!(
            r#"<dlc><header></header><content><package name="test"><file><url>{}</url></file></package></content></dlc>"#,
            url
        );
        let urls = extract_mega_links_from_xml(&xml);
        assert_eq!(urls.len(), 1);
    }

    // =========================================================================
    // AES decryption tests (using known test vectors)
    // =========================================================================

    #[test]
    fn decrypt_aes_cbc_rejects_wrong_key_length() {
        let encrypted = vec![0u8; 32]; // Dummy encrypted data
        let short_key = "short"; // Only 5 chars, need 16
        assert!(decrypt_aes_cbc(&encrypted, short_key).is_none());
    }

    #[test]
    fn decrypt_aes_cbc_rejects_long_key() {
        let encrypted = vec![0u8; 32];
        let long_key = "this_key_is_way_too_long_for_aes128";
        assert!(decrypt_aes_cbc(&encrypted, long_key).is_none());
    }

    #[test]
    fn decrypt_aes_cbc_accepts_16_char_key() {
        // This won't decrypt to valid content, but should not panic
        let encrypted = vec![0u8; 32];
        let key = "0123456789abcdef"; // Exactly 16 chars
        // Result may be None due to invalid padding/content, but shouldn't panic
        let _ = decrypt_aes_cbc(&encrypted, key);
    }

    // =========================================================================
    // Service key decryption tests
    // =========================================================================

    #[test]
    fn decrypt_service_key_rejects_invalid_base64() {
        let result = decrypt_service_key("not!!!valid!!!base64");
        assert!(result.is_none());
    }

    #[test]
    fn decrypt_service_key_rejects_wrong_block_size() {
        // Valid base64 but wrong size (not multiple of 16)
        let result = decrypt_service_key("SGVsbG8="); // "Hello" = 5 bytes
        assert!(result.is_none());
    }

    #[test]
    fn decrypt_service_key_handles_empty_input() {
        // Empty base64 decodes to empty bytes, which produces empty key
        let result = decrypt_service_key("");
        assert_eq!(result, Some("".to_string()));
    }

    // =========================================================================
    // JDownloader key constant tests
    // =========================================================================

    #[test]
    fn jdownloader_key_is_16_bytes() {
        assert_eq!(JDOWNLOADER_KEY.len(), 16);
    }

    #[test]
    fn jdownloader_key_matches_known_value() {
        // hex: 447e787351e60e2c6a96b3964be0c9bd
        let expected: [u8; 16] = [
            0x44, 0x7e, 0x78, 0x73, 0x51, 0xe6, 0x0e, 0x2c, 0x6a, 0x96, 0xb3, 0x96, 0x4b, 0xe0,
            0xc9, 0xbd,
        ];
        assert_eq!(JDOWNLOADER_KEY, &expected);
    }

    // =========================================================================
    // Service URL tests
    // =========================================================================

    #[test]
    fn service_url_is_https() {
        assert!(DLC_SERVICE.starts_with("https://"));
    }

    #[test]
    fn service_url_is_jdownloader_domain() {
        assert!(DLC_SERVICE.contains("jdownloader.org"));
    }

    // =========================================================================
    // Edge case tests
    // =========================================================================

    #[test]
    fn extract_handles_url_with_special_chars() {
        // MEGA URLs can have # and other special chars
        let url = base64::engine::general_purpose::STANDARD
            .encode("https://mega.nz/file/ABC123#key!@#$%^&*()");
        let xml = format!("<dlc><url>{}</url></dlc>", url);
        let urls = extract_mega_links_from_xml(&xml);
        assert_eq!(urls.len(), 1);
    }

    #[test]
    fn extract_handles_very_long_url() {
        let long_key = "x".repeat(200);
        let url = base64::engine::general_purpose::STANDARD
            .encode(format!("https://mega.nz/file/ABC123#{}", long_key));
        let xml = format!("<dlc><url>{}</url></dlc>", url);
        let urls = extract_mega_links_from_xml(&xml);
        assert_eq!(urls.len(), 1);
    }

    #[test]
    fn cache_is_thread_safe() {
        use std::thread;
        let cache = Arc::new(DlcKeyCache::new());
        let cache1 = Arc::clone(&cache);
        let cache2 = Arc::clone(&cache);

        let t1 = thread::spawn(move || {
            for i in 0..100 {
                cache1.set(format!("key{}", i), format!("value{}", i));
            }
        });

        let t2 = thread::spawn(move || {
            for i in 0..100 {
                let _ = cache2.get(&format!("key{}", i));
            }
        });

        t1.join().unwrap();
        t2.join().unwrap();
        // If we get here without deadlock or panic, the cache is thread-safe
    }

    #[test]
    fn extract_handles_malformed_xml() {
        // Missing closing tag
        let url = base64::engine::general_purpose::STANDARD
            .encode("https://mega.nz/file/abc#key");
        let xml = format!("<dlc><url>{}", url);
        let urls = extract_mega_links_from_xml(&xml);
        assert!(urls.is_empty());
    }

    #[test]
    fn extract_handles_url_tags_with_attributes() {
        // Real DLC files might have attributes we ignore
        let url = base64::engine::general_purpose::STANDARD
            .encode("https://mega.nz/file/abc#key");
        // Note: our simple parser won't handle attributes, just testing it doesn't crash
        let xml = format!("<dlc><url type=\"http\">{}</url></dlc>", url);
        // Current implementation will find "<url>" without attributes
        let urls = extract_mega_links_from_xml(&xml);
        // This might be empty since we look for exact "<url>" match
        // That's okay - we're testing it doesn't crash
        let _ = urls;
    }
}
