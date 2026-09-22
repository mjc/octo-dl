//! URL extraction and DLC path detection utilities.

use std::collections::HashSet;
use std::fmt;
use std::path::Path;
use std::str::FromStr;
use std::sync::LazyLock;

use base64::Engine;
use regex::Regex;

static URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"https?://mega\.nz/(?:(?:file|folder)/[^\s"'<>\[\](){}]+|#(?:!|F!)[^\s"'<>\[\](){}]+)"#,
    )
    .expect("valid regex")
});

/// A validated MEGA public link.
///
/// The string representation is always the canonical `/file/...` or
/// `/folder/...` form. Legacy `#!...` and `#F!...` links are accepted and
/// normalized while parsing.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct MegaUrl(String);

impl MegaUrl {
    /// Parses and canonicalizes a MEGA public link.
    ///
    /// Both `http` and `https` are preserved. The public-link fragment is
    /// preserved verbatim because MEGA keys are opaque to this crate.
    ///
    /// # Errors
    ///
    /// Returns an error when the input is empty or is not a supported MEGA
    /// public-link form.
    pub fn parse(input: &str) -> Result<Self, SourceParseError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(SourceParseError::Empty);
        }

        let Some((scheme, payload)) = split_mega_url(input) else {
            return Err(SourceParseError::UnsupportedMegaUrl(input.to_string()));
        };

        let canonical = if let Some(rest) = payload.strip_prefix("#!") {
            canonicalize_legacy_link(scheme, rest, "file")?
        } else if let Some(rest) = payload.strip_prefix("#F!") {
            canonicalize_legacy_link(scheme, rest, "folder")?
        } else {
            canonicalize_modern_link(scheme, payload)?
        };

        Ok(Self(canonical))
    }

    /// Returns the canonical URL without exposing internal representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Converts this validated URL into its canonical string form.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl AsRef<str> for MegaUrl {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for MegaUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for MegaUrl {
    type Err = SourceParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        Self::parse(input)
    }
}

/// A validated path to a `JDownloader` `.dlc` file.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DlcPath(String);

impl DlcPath {
    /// Parses a local path whose extension is `.dlc`, case-insensitively.
    ///
    /// # Errors
    ///
    /// Returns an error when the input is empty, contains a URL scheme or NUL
    /// byte, or does not have a `.dlc` extension.
    pub fn parse(input: &str) -> Result<Self, SourceParseError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(SourceParseError::Empty);
        }
        if input.contains("://") {
            return Err(SourceParseError::UnsupportedDlcPath(input.to_string()));
        }
        if input.as_bytes().contains(&0) {
            return Err(SourceParseError::UnsupportedDlcPath(input.to_string()));
        }
        if !is_dlc_path(input) {
            return Err(SourceParseError::NotDlcPath(input.to_string()));
        }

        Ok(Self(input.to_string()))
    }

    /// Returns the original path spelling supplied by the caller.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Converts this validated path into its original string form.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl AsRef<str> for DlcPath {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for DlcPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for DlcPath {
    type Err = SourceParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        Self::parse(input)
    }
}

/// A supported download submission source.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum DownloadSource {
    Mega(MegaUrl),
    Dlc(DlcPath),
}

impl DownloadSource {
    /// Parses one supported CLI/TUI submission source.
    ///
    /// # Errors
    ///
    /// Returns an error when the input is empty or is neither a supported MEGA
    /// public link nor a `.dlc` path.
    pub fn parse(input: &str) -> Result<Self, SourceParseError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(SourceParseError::Empty);
        }

        if input.starts_with("http://") || input.starts_with("https://") {
            return MegaUrl::parse(input).map(Self::Mega);
        }
        if is_dlc_path(input) {
            return DlcPath::parse(input).map(Self::Dlc);
        }

        Err(SourceParseError::UnsupportedSource(input.to_string()))
    }

    /// Returns the source in the canonical string form accepted downstream.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Mega(url) => url.as_str(),
            Self::Dlc(path) => path.as_str(),
        }
    }

    /// Converts the source into the string representation used by legacy
    /// persistence and download request fields.
    #[must_use]
    pub fn into_string(self) -> String {
        match self {
            Self::Mega(url) => url.into_string(),
            Self::Dlc(path) => path.into_string(),
        }
    }
}

impl fmt::Display for DownloadSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for DownloadSource {
    type Err = SourceParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        Self::parse(input)
    }
}

/// Errors returned when a submission is not a supported MEGA or DLC source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceParseError {
    Empty,
    UnsupportedSource(String),
    UnsupportedMegaUrl(String),
    UnsupportedDlcPath(String),
    NotDlcPath(String),
}

impl fmt::Display for SourceParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("download source is empty"),
            Self::UnsupportedSource(source) => write!(
                formatter,
                "unsupported download source {source:?}; expected a mega.nz URL or .dlc path"
            ),
            Self::UnsupportedMegaUrl(url) => write!(
                formatter,
                "unsupported MEGA URL {url:?}; expected /file/<id>, /folder/<id>, or a legacy #!/#F! link"
            ),
            Self::UnsupportedDlcPath(path) => {
                write!(formatter, "unsupported DLC path {path:?}")
            }
            Self::NotDlcPath(path) => {
                write!(formatter, "DLC source must have a .dlc extension: {path:?}")
            }
        }
    }
}

impl std::error::Error for SourceParseError {}

fn split_mega_url(input: &str) -> Option<(&str, &str)> {
    input
        .strip_prefix("https://mega.nz/")
        .map(|payload| ("https", payload))
        .or_else(|| {
            input
                .strip_prefix("http://mega.nz/")
                .map(|payload| ("http", payload))
        })
}

fn canonicalize_modern_link(scheme: &str, payload: &str) -> Result<String, SourceParseError> {
    let (kind, rest) = payload.split_once('/').ok_or_else(|| {
        SourceParseError::UnsupportedMegaUrl(format!("{scheme}://mega.nz/{payload}"))
    })?;
    if kind != "file" && kind != "folder" {
        return Err(SourceParseError::UnsupportedMegaUrl(format!(
            "{scheme}://mega.nz/{payload}"
        )));
    }

    let (node_id, key) = rest
        .split_once('#')
        .map_or((rest, None), |(id, key)| (id, Some(key)));
    if node_id.is_empty()
        || node_id.contains(['/', '?', '#', ' ', '\t', '\r', '\n'])
        || key.is_some_and(|key| key.is_empty() || key.contains(['?', ' ', '\t', '\r', '\n']))
    {
        return Err(SourceParseError::UnsupportedMegaUrl(format!(
            "{scheme}://mega.nz/{payload}"
        )));
    }

    let mut canonical = format!("{scheme}://mega.nz/{kind}/{node_id}");
    if let Some(key) = key {
        canonical.push('#');
        canonical.push_str(key);
    }
    Ok(canonical)
}

fn canonicalize_legacy_link(
    scheme: &str,
    rest: &str,
    kind: &str,
) -> Result<String, SourceParseError> {
    let (node_id, key) = rest.split_once('!').ok_or_else(|| {
        SourceParseError::UnsupportedMegaUrl(format!("{scheme}://mega.nz/#!{rest}"))
    })?;
    if node_id.is_empty()
        || key.is_empty()
        || node_id.contains(['/', '?', '#', ' ', '\t', '\r', '\n'])
        || key.contains(['?', ' ', '\t', '\r', '\n'])
    {
        return Err(SourceParseError::UnsupportedMegaUrl(format!(
            "{scheme}://mega.nz/#{kind}!{rest}"
        )));
    }

    Ok(format!("{scheme}://mega.nz/{kind}/{node_id}#{key}"))
}

fn normalize_extracted_url(raw_url: &str) -> String {
    let trimmed = raw_url.trim_end_matches(['.', ',', '!', '?', ';', ':']);
    normalize_mega_url(trimmed).unwrap_or_else(|| trimmed.to_string())
}

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

    // Pull MEGA URLs out of the entire input.
    for m in URL_RE.find_iter(input) {
        let url = normalize_extracted_url(m.as_str());
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
    if !looks_like_base64_token(token) {
        return;
    }
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

        // Check for MEGA URLs in decoded result.
        for m in URL_RE.find_iter(&decoded) {
            let url = normalize_extracted_url(m.as_str());
            if seen.insert(url.clone()) {
                result.push(url);
            }
        }
        if is_dlc_path(&decoded) && seen.insert(decoded.clone()) {
            result.push(decoded.clone());
        }
    }
}

fn looks_like_base64_token(token: &str) -> bool {
    let token = token.trim();
    if token.len() < 8 || token.len() % 4 == 1 {
        return false;
    }
    token.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'-' | b'_' | b'=')
    })
}

#[must_use]
pub(crate) fn normalize_mega_url(url: &str) -> Option<String> {
    MegaUrl::parse(url).ok().map(MegaUrl::into_string)
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

    #[test]
    fn base64_probe_skips_plain_markup_tokens() {
        assert!(!looks_like_base64_token("<span>"));
        assert!(!looks_like_base64_token("class=\"link\""));
        assert!(!looks_like_base64_token("word"));
        assert!(looks_like_base64_token(
            &STANDARD.encode("https://mega.nz/file/abc#key")
        ));
    }

    // --- extract_urls: new tests ---

    #[test]
    fn extract_urls_trailing_punctuation() {
        let input = "See https://mega.nz/file/abc.";
        let urls = extract_urls(input);
        assert_eq!(urls, vec!["https://mega.nz/file/abc"]);
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

    // --- extract_urls: legacy URLs ---

    #[test]
    fn extract_normalizes_legacy_folder_url() {
        let urls = extract_urls("https://mega.nz/#F!abc!key123");
        assert_eq!(urls, vec!["https://mega.nz/folder/abc#key123"]);
    }

    #[test]
    fn extract_normalizes_legacy_file_url() {
        let urls =
            extract_urls("https://mega.nz/#!x3JwHYgK!7l9e_rV1yVFCxd63F4zipoQyCjwyRT0pckkpvAdxm2M");
        assert_eq!(
            urls,
            vec!["https://mega.nz/file/x3JwHYgK#7l9e_rV1yVFCxd63F4zipoQyCjwyRT0pckkpvAdxm2M"]
        );
    }

    #[test]
    fn extract_normalizes_short_legacy_file_url() {
        let urls = extract_urls("https://mega.nz/#!abc!key123");
        assert_eq!(urls, vec!["https://mega.nz/file/abc#key123"]);
    }

    #[test]
    fn extract_normalizes_base64_encoded_legacy_url() {
        let encoded = STANDARD.encode("https://mega.nz/#!abc!key123");
        let urls = extract_urls(&encoded);
        assert_eq!(urls, vec!["https://mega.nz/file/abc#key123"]);
    }

    #[test]
    fn mega_url_parsing_preserves_scheme_and_fragment_key() {
        let url = MegaUrl::parse("http://mega.nz/file/node#key!@#$%^&*()")
            .expect("canonical MEGA URL should parse");
        assert_eq!(url.as_str(), "http://mega.nz/file/node#key!@#$%^&*()");
    }

    #[test]
    fn mega_url_parsing_normalizes_legacy_file_and_folder_links() {
        assert_eq!(
            MegaUrl::parse("https://mega.nz/#!file-id!file-key")
                .unwrap()
                .as_str(),
            "https://mega.nz/file/file-id#file-key"
        );
        assert_eq!(
            MegaUrl::parse("http://mega.nz/#F!folder-id!folder-key")
                .unwrap()
                .as_str(),
            "http://mega.nz/folder/folder-id#folder-key"
        );
    }

    #[test]
    fn mega_url_parsing_rejects_unsupported_forms() {
        for input in [
            "https://example.com/file/id#key",
            "https://mega.nz/unknown/id#key",
            "https://mega.nz/file/",
            "https://mega.nz/file/id?download=1",
            "https://mega.nz/#!id",
        ] {
            assert!(MegaUrl::parse(input).is_err(), "{input} should be rejected");
        }
    }

    #[test]
    fn download_source_distinguishes_mega_urls_and_dlc_paths() {
        assert!(matches!(
            DownloadSource::parse("https://mega.nz/#!id!key"),
            Ok(DownloadSource::Mega(url)) if url.as_str() == "https://mega.nz/file/id#key"
        ));
        assert!(matches!(
            DownloadSource::parse("./links.DLC"),
            Ok(DownloadSource::Dlc(path)) if path.as_str() == "./links.DLC"
        ));
    }

    #[test]
    fn dlc_path_parsing_rejects_urls_and_non_dlc_paths() {
        assert!(DlcPath::parse("https://example.com/links.dlc").is_err());
        assert!(DlcPath::parse("links.zip").is_err());
        assert!(DlcPath::parse("").is_err());
    }

    mod property_tests {
        use super::*;
        use proptest::{prelude::*, string::string_regex};

        fn scheme() -> impl Strategy<Value = &'static str> {
            prop_oneof![Just("https"), Just("http")]
        }

        fn fragment() -> impl Strategy<Value = String> {
            string_regex("[A-Za-z0-9_-]{1,16}").expect("valid fragment regex")
        }

        fn canonical_file_url(scheme: &str, node_id: &str, node_key: &str) -> String {
            format!("{scheme}://mega.nz/file/{node_id}#{node_key}")
        }

        fn canonical_folder_url(scheme: &str, node_id: &str, node_key: &str) -> String {
            format!("{scheme}://mega.nz/folder/{node_id}#{node_key}")
        }

        proptest! {
            #[test]
            fn normalize_mega_url_normalizes_legacy_file_urls(
                scheme in scheme(),
                node_id in fragment(),
                node_key in fragment(),
            ) {
                let legacy = format!("{scheme}://mega.nz/#!{node_id}!{node_key}");
                prop_assert_eq!(
                    normalize_mega_url(&legacy),
                    Some(canonical_file_url(scheme, &node_id, &node_key)),
                );
            }

            #[test]
            fn normalize_mega_url_normalizes_legacy_folder_urls(
                scheme in scheme(),
                node_id in fragment(),
                node_key in fragment(),
            ) {
                let legacy = format!("{scheme}://mega.nz/#F!{node_id}!{node_key}");
                prop_assert_eq!(
                    normalize_mega_url(&legacy),
                    Some(canonical_folder_url(scheme, &node_id, &node_key)),
                );
            }

            #[test]
            fn extract_urls_deduplicates_plain_and_encoded_forms(
                scheme in scheme(),
                node_id in fragment(),
                node_key in fragment(),
            ) {
                let legacy = format!("{scheme}://mega.nz/#!{node_id}!{node_key}");
                let canonical = canonical_file_url(scheme, &node_id, &node_key);
                let encoded = STANDARD.encode(&legacy);
                let input = format!("{legacy}\n{canonical}\n{encoded}");

                prop_assert_eq!(extract_urls(&input), vec![canonical]);
            }

            #[test]
            fn extract_urls_decodes_up_to_three_rounds(
                scheme in scheme(),
                node_id in fragment(),
                node_key in fragment(),
            ) {
                let legacy = format!("{scheme}://mega.nz/#!{node_id}!{node_key}");
                let expected = canonical_file_url(scheme, &node_id, &node_key);
                let once = STANDARD.encode(&legacy);
                let twice = STANDARD.encode(&once);
                let thrice = STANDARD.encode(&twice);
                let quadruple = STANDARD.encode(&thrice);

                prop_assert_eq!(extract_urls(&thrice), vec![expected.clone()]);
                prop_assert!(!extract_urls(&quadruple).contains(&expected));
            }

            #[test]
            fn is_dlc_path_accepts_case_insensitive_suffix(
                stem in fragment(),
                extension in prop_oneof![Just("dlc"), Just("DLC"), Just("DlC"), Just("dLc")],
            ) {
                let path = format!("{stem}.{extension}");
                prop_assert!(is_dlc_path(&path));
            }

            #[test]
            fn is_dlc_path_rejects_other_extensions(
                stem in fragment(),
                extension in string_regex("[A-Za-z0-9]{1,6}")
                    .expect("valid extension regex")
                    .prop_filter("extension must not be dlc", |ext| !ext.eq_ignore_ascii_case("dlc")),
            ) {
                let path = format!("{stem}.{extension}");
                prop_assert!(!is_dlc_path(&path));
            }
        }
    }
}
