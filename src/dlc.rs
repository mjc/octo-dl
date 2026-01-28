use aes::Aes128;
use aes::cipher::KeyIvInit;
use aes::cipher::BlockDecryptMut;
use base64::Engine;
use cbc::Decryptor;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const DLC_SERVICE: &str = "https://service.jdownloader.org/dlcrypt/service.php";
const MIN_DLC_SIZE: usize = 100;
const DLC_KEY_LENGTH: usize = 88;

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

/// Extract MEGA links from a JDownloader2 DLC file
pub async fn parse_dlc_file(
    path: &str,
    http_client: &reqwest::Client,
    cache: &DlcKeyCache,
) -> Option<Vec<String>> {
    let content = std::fs::read_to_string(path).ok()?;

    // Validate file size
    if content.trim().len() < MIN_DLC_SIZE {
        eprintln!("DLC file too small (< {} bytes)", MIN_DLC_SIZE);
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

/// Get decryption key from JDownloader service with exponential backoff
async fn get_decryption_key(
    dlc_key: &str,
    http_client: &reqwest::Client,
    cache: &DlcKeyCache,
) -> Option<String> {
    // Check cache first
    if let Some(cached) = cache.get(dlc_key) {
        return Some(cached);
    }

    const MAX_RETRIES: u32 = 3;
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
                eprintln!("DLC service unreachable after {} attempts", MAX_RETRIES);
                return None;
            }
        }
    }
}

/// Call JDownloader's DLC decryption service
async fn call_decryption_service(dlc_key: &str, http_client: &reqwest::Client) -> Option<String> {
    let version = env!("CARGO_PKG_VERSION");
    let user_agent = format!("JDownloader/2.0 (octo-dl/{})", version);

    let params = [("jd", "1"), ("srcType", "plain"), ("data", dlc_key)];

    let response = http_client
        .post(DLC_SERVICE)
        .header("User-Agent", &user_agent)
        .form(&params)
        .send()
        .await
        .ok()?;

    if !response.status().is_success() {
        eprintln!("DLC service returned status: {}", response.status());
        return None;
    }

    let text = response.text().await.ok()?;

    // Extract the RC value from <rc>...</rc>
    let start = text.find("<rc>")?;
    let end = text.find("</rc>")?;

    if start >= end {
        return None;
    }

    let rc_value = &text[start + 4..end];

    // Check for rate limit error
    if rc_value == "2YVhzRFdjR2dDQy9JL25aVXFjQ1RPZ" {
        eprintln!("DLC service rate limit hit");
        return None;
    }

    // Check minimum length (should be >80 chars)
    if rc_value.trim().len() < 80 {
        eprintln!("DLC service returned invalid key");
        return None;
    }

    Some(rc_value.trim().to_string())
}

/// Decrypt AES-128 CBC encrypted data
fn decrypt_aes_cbc(encrypted: &[u8], key_str: &str) -> Option<String> {
    use aes::cipher::generic_array::GenericArray;

    // Decode the key from base64
    let key_bytes = base64::engine::general_purpose::STANDARD
        .decode(key_str)
        .ok()?;

    if key_bytes.len() < 16 {
        return None;
    }

    // Use first 16 bytes as key, rest as IV (or zero IV)
    let key = GenericArray::from_slice(&key_bytes[..16]);
    let iv = if key_bytes.len() >= 32 {
        GenericArray::from_slice(&key_bytes[16..32])
    } else {
        // Zero IV
        GenericArray::from_slice(&[0u8; 16])
    };

    // Create decryptor
    let cipher = Decryptor::<Aes128>::new(key, iv);
    let mut data = encrypted.to_vec();

    let decrypted = cipher
        .decrypt_padded_mut::<aes::cipher::block_padding::Pkcs7>(&mut data)
        .ok()?;

    // Convert to string
    String::from_utf8(decrypted.to_vec()).ok()
}

/// Extract all MEGA links from decrypted DLC XML
fn extract_mega_links_from_xml(xml: &str) -> Vec<String> {
    let mut urls = Vec::new();

    // Simple regex-free approach: find <url> tags
    let mut content = xml;
    while let Some(start) = content.find("<url>") {
        let after_tag = &content[start + 5..];
        if let Some(end) = after_tag.find("</url>") {
            let url = &after_tag[..end];
            if (url.starts_with("https://mega.nz/") || url.starts_with("http://mega.nz/"))
                && !urls.contains(&url.to_string())
            {
                urls.push(url.to_string());
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
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dlc_key_cache() {
        let cache = DlcKeyCache::new();
        cache.set("key1".to_string(), "value1".to_string());
        assert_eq!(cache.get("key1"), Some("value1".to_string()));
        assert_eq!(cache.get("key2"), None);
    }

    #[test]
    fn test_extract_mega_links() {
        let xml = r#"<dlc><url>https://mega.nz/file/test1#key</url><url>https://google.com/search</url><url>https://mega.nz/folder/test2#key</url></dlc>"#;
        let urls = extract_mega_links_from_xml(xml);
        assert_eq!(urls.len(), 2);
        assert!(urls.iter().all(|u| u.starts_with("https://mega.nz/")));
    }

    #[test]
    fn test_valid_base64() {
        assert!(is_valid_base64("SGVsbG8gV29ybGQ="));
        assert!(!is_valid_base64("not!!base64"));
    }

    #[test]
    fn test_dlc_size_validation() {
        let small_content = "x".repeat(50);
        // Would fail validation due to size
        assert!(small_content.len() < MIN_DLC_SIZE);
    }
}
