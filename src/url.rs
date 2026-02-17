//! URL extraction and DLC path detection utilities.

use std::collections::HashSet;
use std::path::Path;
use std::sync::LazyLock;

use base64::Engine;
use regex::Regex;

static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"https?://mega\.nz/[^\s"'<>\[\](){}]+"#).expect("valid regex"));

static LEGACY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"https?://mega\.nz/#[F!][^\s"'<>\[\](){}]+"#).expect("valid regex"));

/// Extracts MEGA URLs and DLC file paths from raw input text.
///
/// Scans for `https://mega.nz/...` URLs and `.dlc` file paths. If a
/// whitespace-separated token doesn't look like a URL or path, it is
/// base64-decoded (both STANDARD and `URL_SAFE` alphabets) and the result
/// is scanned again, up to 3 decode rounds.
///
/// # Panics
///
/// Panics if the internal URL regex fails to compile (this is a compile-time
/// constant and will not happen in practice).
#[must_use]
pub fn extract_urls(input: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut result: Vec<String> = Vec::new();

    // Match legacy URLs first so they get normalized before the modern regex
    // would capture them as raw (unusable) strings.
    for m in LEGACY_RE.find_iter(input) {
        let url = normalize_mega_url(m.as_str());
        if seen.insert(url.clone()) {
            result.push(url);
        }
        // Also mark the raw match as seen so url_re doesn't duplicate it.
        seen.insert(m.as_str().to_string());
    }

    // Pull modern-format URLs out of the entire input.
    for m in URL_RE.find_iter(input) {
        let url = m.as_str().to_string();
        if seen.insert(url.clone()) {
            result.push(url);
        }
    }

    // Then inspect each whitespace-separated token individually.
    for token in input.split_whitespace() {
        if is_dlc_path(token) {
            let s = token.to_string();
            if seen.insert(s.clone()) {
                result.push(s);
            }
            continue;
        }

        // If the token already matched a URL above, skip decode attempts
        if URL_RE.is_match(token) {
            continue;
        }

        // Try base64 decoding up to 3 times
        try_decode_base64(token, 3, &mut seen, &mut result);
    }

    result
}

/// Attempts to base64-decode `token` up to `max_rounds` times, collecting
/// any discovered MEGA URLs or DLC paths into `result`.
fn try_decode_base64(
    token: &str,
    max_rounds: usize,
    seen: &mut HashSet<String>,
    result: &mut Vec<String>,
) {
    let mut decoded = token.to_string();
    for _ in 0..max_rounds {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(decoded.trim())
            .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(decoded.trim()));
        let Ok(bytes) = bytes else { break };
        let Ok(s) = String::from_utf8(bytes) else {
            break;
        };
        decoded = s;

        // Check for legacy URLs first and normalize them
        for m in LEGACY_RE.find_iter(&decoded) {
            let url = normalize_mega_url(m.as_str());
            if seen.insert(url.clone()) {
                result.push(url);
            }
        }

        // Check for modern URLs in decoded result
        for m in URL_RE.find_iter(&decoded) {
            let url = m.as_str().to_string();
            if seen.insert(url.clone()) {
                result.push(url);
            }
        }
        if is_dlc_path(&decoded) && seen.insert(decoded.clone()) {
            result.push(decoded.clone());
        }
    }
}

/// Converts a legacy MEGA URL to the modern format.
///
/// Legacy formats:
/// - `https://mega.nz/#F!{id}!{key}` → `https://mega.nz/folder/{id}#{key}`
/// - `https://mega.nz/#!{id}!{key}`  → `https://mega.nz/file/{id}#{key}`
///
/// Modern URLs are returned unchanged.
#[must_use]
pub fn normalize_mega_url(url: &str) -> String {
    if let Some(rest) = url
        .strip_prefix("https://mega.nz/#F!")
        .or_else(|| url.strip_prefix("http://mega.nz/#F!"))
        && let Some((id, key)) = rest.split_once('!')
    {
        return format!("https://mega.nz/folder/{id}#{key}");
    }
    if let Some(rest) = url
        .strip_prefix("https://mega.nz/#!")
        .or_else(|| url.strip_prefix("http://mega.nz/#!"))
        && let Some((id, key)) = rest.split_once('!')
    {
        return format!("https://mega.nz/file/{id}#{key}");
    }
    url.to_string()
}

/// Returns `true` if `s` looks like a path to a `.dlc` file.
#[must_use]
pub fn is_dlc_path(s: &str) -> bool {
    Path::new(s)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("dlc"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::{STANDARD, URL_SAFE};

    // --- extract_urls: plain URLs ---

    #[test]
    fn extract_single_mega_url() {
        let urls = extract_urls("https://mega.nz/folder/abc123");
        assert_eq!(urls, vec!["https://mega.nz/folder/abc123"]);
    }

    #[test]
    fn extract_multiple_space_separated_urls() {
        let input = "https://mega.nz/folder/aaa https://mega.nz/file/bbb";
        let urls = extract_urls(input);
        assert_eq!(
            urls,
            vec!["https://mega.nz/folder/aaa", "https://mega.nz/file/bbb",]
        );
    }

    #[test]
    fn extract_multiple_newline_separated_urls() {
        let input =
            "https://mega.nz/folder/aaa\nhttps://mega.nz/file/bbb\nhttps://mega.nz/file/ccc";
        let urls = extract_urls(input);
        assert_eq!(
            urls,
            vec![
                "https://mega.nz/folder/aaa",
                "https://mega.nz/file/bbb",
                "https://mega.nz/file/ccc",
            ]
        );
    }

    #[test]
    fn extract_deduplicates_urls() {
        let input = "https://mega.nz/file/aaa https://mega.nz/file/aaa";
        let urls = extract_urls(input);
        assert_eq!(urls, vec!["https://mega.nz/file/aaa"]);
    }

    #[test]
    fn extract_http_url() {
        let urls = extract_urls("http://mega.nz/file/aaa");
        assert_eq!(urls, vec!["http://mega.nz/file/aaa"]);
    }

    #[test]
    fn extract_url_embedded_in_text() {
        let input = "check this out: https://mega.nz/folder/xyz#key123 and more text";
        let urls = extract_urls(input);
        assert_eq!(urls, vec!["https://mega.nz/folder/xyz#key123"]);
    }

    // --- extract_urls: DLC paths ---

    #[test]
    fn extract_dlc_path() {
        let urls = extract_urls("/home/user/links.dlc");
        assert_eq!(urls, vec!["/home/user/links.dlc"]);
    }

    #[test]
    fn extract_dlc_case_insensitive() {
        let urls = extract_urls("links.DLC");
        assert_eq!(urls, vec!["links.DLC"]);
    }

    #[test]
    fn extract_dlc_and_url_together() {
        let input = "https://mega.nz/file/aaa /tmp/links.dlc";
        let urls = extract_urls(input);
        assert_eq!(urls, vec!["https://mega.nz/file/aaa", "/tmp/links.dlc"]);
    }

    // --- extract_urls: base64 decoding ---

    #[test]
    fn extract_single_base64_encoded_url() {
        let url = "https://mega.nz/file/test123";
        let encoded = STANDARD.encode(url);
        let urls = extract_urls(&encoded);
        assert_eq!(urls, vec![url]);
    }

    #[test]
    fn extract_url_safe_base64_encoded_url() {
        let url = "https://mega.nz/file/test123";
        let encoded = URL_SAFE.encode(url);
        let urls = extract_urls(&encoded);
        assert_eq!(urls, vec![url]);
    }

    #[test]
    fn extract_double_base64_encoded_url() {
        let url = "https://mega.nz/file/deep";
        let once = STANDARD.encode(url);
        let twice = STANDARD.encode(&once);
        let urls = extract_urls(&twice);
        assert_eq!(urls, vec![url]);
    }

    #[test]
    fn extract_triple_base64_encoded_url() {
        let url = "https://mega.nz/file/verydeep";
        let once = STANDARD.encode(url);
        let twice = STANDARD.encode(&once);
        let thrice = STANDARD.encode(&twice);
        let urls = extract_urls(&thrice);
        assert_eq!(urls, vec![url]);
    }

    #[test]
    fn extract_quadruple_base64_exceeds_limit() {
        let url = "https://mega.nz/file/toomuch";
        let once = STANDARD.encode(url);
        let twice = STANDARD.encode(&once);
        let thrice = STANDARD.encode(&twice);
        let quad = STANDARD.encode(&thrice);
        let urls = extract_urls(&quad);
        assert!(!urls.contains(&url.to_string()));
    }

    #[test]
    fn extract_base64_encoded_dlc_path() {
        let path = "/tmp/links.dlc";
        let encoded = STANDARD.encode(path);
        let urls = extract_urls(&encoded);
        assert_eq!(urls, vec![path]);
    }

    #[test]
    fn extract_mix_of_plain_and_base64() {
        let plain = "https://mega.nz/file/plain";
        let secret = "https://mega.nz/file/secret";
        let encoded = STANDARD.encode(secret);
        let input = format!("{plain} {encoded}");
        let urls = extract_urls(&input);
        assert_eq!(urls, vec![plain, secret]);
    }

    // --- extract_urls: empty / garbage input ---

    #[test]
    fn extract_empty_input() {
        let urls = extract_urls("");
        assert!(urls.is_empty());
    }

    #[test]
    fn extract_whitespace_only() {
        let urls = extract_urls("   \n\t  ");
        assert!(urls.is_empty());
    }

    #[test]
    fn extract_garbage_returns_nothing() {
        let urls = extract_urls("not a url at all");
        assert!(urls.is_empty());
    }

    #[test]
    fn extract_non_mega_url_ignored() {
        let urls = extract_urls("https://example.com/file");
        assert!(urls.is_empty());
    }

    #[test]
    fn extract_invalid_base64_ignored() {
        let urls = extract_urls("!!!not-base64!!!");
        assert!(urls.is_empty());
    }

    // --- extract_urls: new tests ---

    #[test]
    fn extract_urls_trailing_punctuation() {
        let input = "See https://mega.nz/file/abc.";
        let urls = extract_urls(input);
        // The regex will capture "https://mega.nz/file/abc." including the trailing dot
        // which is expected behavior for \S+ matching
        assert_eq!(urls.len(), 1);
        assert!(urls[0].starts_with("https://mega.nz/file/abc"));
    }

    // --- is_dlc_path ---

    #[test]
    fn dlc_path_detected() {
        assert!(is_dlc_path("foo.dlc"));
        assert!(is_dlc_path("/absolute/path.DLC"));
        assert!(is_dlc_path("relative/path.Dlc"));
    }

    #[test]
    fn non_dlc_path_rejected() {
        assert!(!is_dlc_path("foo.txt"));
        assert!(!is_dlc_path("dlc"));
        assert!(!is_dlc_path(""));
        assert!(!is_dlc_path("https://mega.nz/file/abc"));
    }

    #[test]
    fn is_dlc_path_edge_cases() {
        // Directory path — no extension
        assert!(!is_dlc_path("/some/directory/"));
        // No extension
        assert!(!is_dlc_path("noextension"));
    }

    // --- normalize_mega_url ---

    #[test]
    fn normalize_legacy_folder_url() {
        assert_eq!(
            normalize_mega_url("https://mega.nz/#F!3RYjXIAK!6cjk7zs42McdRTT4C-J-sg"),
            "https://mega.nz/folder/3RYjXIAK#6cjk7zs42McdRTT4C-J-sg"
        );
    }

    #[test]
    fn normalize_legacy_file_url() {
        assert_eq!(
            normalize_mega_url("https://mega.nz/#!abc123!keydata"),
            "https://mega.nz/file/abc123#keydata"
        );
    }

    #[test]
    fn normalize_modern_url_unchanged() {
        let url = "https://mega.nz/folder/abc123#key456";
        assert_eq!(normalize_mega_url(url), url);
    }

    #[test]
    fn normalize_http_legacy_folder() {
        assert_eq!(
            normalize_mega_url("http://mega.nz/#F!id!key"),
            "https://mega.nz/folder/id#key"
        );
    }

    #[test]
    fn normalize_http_legacy_file() {
        assert_eq!(
            normalize_mega_url("http://mega.nz/#!id!key"),
            "https://mega.nz/file/id#key"
        );
    }

    // --- extract_urls: legacy URL conversion ---

    #[test]
    fn extract_converts_legacy_folder_url() {
        let urls = extract_urls("https://mega.nz/#F!abc!key123");
        assert_eq!(urls, vec!["https://mega.nz/folder/abc#key123"]);
    }

    #[test]
    fn extract_converts_legacy_file_url() {
        let urls = extract_urls("https://mega.nz/#!abc!key123");
        assert_eq!(urls, vec!["https://mega.nz/file/abc#key123"]);
    }
}
