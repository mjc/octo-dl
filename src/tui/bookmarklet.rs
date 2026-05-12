//! Bookmarklet helper and retained icon asset.

/// Returns the bookmarklet helper page.
#[must_use]
pub fn bookmarklet_html(
    fallback_origin: &str,
    fallback_host: &str,
    api_key_header: &str,
) -> String {
    r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>octo-dl bookmarklet</title>
<style>
  body { font-family: system-ui, sans-serif; max-width: 480px; margin: 60px auto; color: #e0e0e0; background: #1a1a2e; }
  h1 { font-size: 1.4rem; }
  p { line-height: 1.5; }
  a.bookmarklet {
    display: inline-block; padding: 10px 20px; margin: 20px 0;
    background: #0f3460; color: #e94560; border-radius: 6px;
    text-decoration: none; font-weight: bold; font-size: 1.1rem;
    border: 2px solid #e94560; cursor: grab;
  }
  a.bookmarklet:hover { background: #16213e; }
  code { background: #16213e; padding: 2px 6px; border-radius: 3px; }
</style>
</head>
<body>
<h1>octo-dl bookmarklet</h1>
<p>Drag this link to your bookmarks bar:</p>
<a class="bookmarklet" href="javascript:void(function(){var page=document.documentElement.outerHTML;var selected=window.getSelection().toString();var h='__FALLBACK_ORIGIN__';var headers=Object.assign({'Content-Type':'application/json'},__API_KEY_HEADER__);fetch(h+'/api/parse',{method:'POST',headers:headers,body:JSON.stringify({page:page,fallback:selected})}).then(function(r){if(!r.ok){return r.text().then(function(t){throw new Error('HTTP '+r.status+(t?': '+t:''))})}return r.json()}).then(function(d){if(d.count>0){alert('Sent '+d.count+' URL(s) to octo-dl')}else{alert('No URLs found on this page')}}).catch(function(e){alert('Error: '+e)})})()">Send to octo-dl</a>
<p>Click it on any page to send the page HTML (with selected text as fallback) to octo-dl for download.</p>
<p>Configured to use <code>__FALLBACK_HOST__</code></p>
</body>
</html>"#
        .replace("__FALLBACK_ORIGIN__", fallback_origin)
        .replace("__FALLBACK_HOST__", fallback_host)
        .replace("__API_KEY_HEADER__", api_key_header)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ICON_SVG: &str = include_str!("assets/icon.svg");

    #[test]
    fn bookmarklet_mentions_fallback_host_and_api_key_header() {
        let html = bookmarklet_html(
            "https://proxy.host",
            "proxy.host",
            r#"{"x-api-key":"secret"}"#,
        );
        assert!(html.contains("proxy.host"));
        assert!(html.contains("bookmarklet"));
        assert!(html.contains(r#""x-api-key":"secret""#));
        assert!(html.contains("https://proxy.host"));
        assert!(html.contains("HTTP '+r.status"));
        assert!(!html.contains("__FALLBACK_HOST__"));
        assert!(!html.contains("__FALLBACK_ORIGIN__"));
        assert!(!html.contains("__API_KEY_HEADER__"));
    }

    #[test]
    fn icon_svg_is_nonempty() {
        assert!(ICON_SVG.starts_with("<svg "));
    }
}
