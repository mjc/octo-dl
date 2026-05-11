//! Web assets for the xterm.js terminal UI and bookmarklet helper.

const INDEX_HTML_TEMPLATE: &str = include_str!("assets/terminal.html");
const SERVICE_WORKER_JS: &str = include_str!("assets/sw.js");
const ICON_SVG: &str = include_str!("assets/icon.svg");

/// Returns the main web UI HTML page (the xterm.js terminal).
#[must_use]
pub fn index_html(host: &str, scheme: &str) -> String {
    INDEX_HTML_TEMPLATE
        .replace("${WS_SCHEME}", scheme)
        .replace("${WS_HOST}", host)
}

/// Returns the bookmarklet helper page.
#[must_use]
pub fn bookmarklet_html(fallback_host: &str) -> String {
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
<a class="bookmarklet" href="javascript:void(function(){
var page=document.documentElement.outerHTML;
var selected=window.getSelection().toString();
var proto=window.location.protocol;
var h=proto+'//__FALLBACK_HOST__';
fetch(h+'/api/parse',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({page:page,fallback:selected})}).then(function(r){return r.json()}).then(function(d){if(d.count>0){alert('Sent '+d.count+' URL(s) to octo-dl')}else{alert('No URLs found on this page')}}).catch(function(e){alert('Error: '+e)})})()">Send to octo-dl</a>
<p>Click it on any page to send the page HTML (with selected text as fallback) to octo-dl for download.</p>
<p>Configured to use <code>__FALLBACK_HOST__</code></p>
</body>
</html>"#
    .replace("__FALLBACK_HOST__", fallback_host)
}

/// Returns the PWA manifest JSON.
#[must_use]
pub fn manifest_json(_host: &str, _port: u16) -> String {
    r##"{
  "name": "octo-dl",
  "short_name": "octo",
  "description": "MEGA file download manager",
  "start_url": "/",
  "scope": "/",
  "display": "standalone",
  "background_color": "#1a1a2e",
  "theme_color": "#1a1a2e",
  "orientation": "portrait-primary",
  "prefer_related_applications": false,
  "icons": [
    {
      "src": "/icon-192.svg",
      "sizes": "192x192",
      "type": "image/svg+xml",
      "purpose": "any maskable"
    },
    {
      "src": "/icon-512.svg",
      "sizes": "512x512",
      "type": "image/svg+xml",
      "purpose": "any maskable"
    }
  ],
  "share_target": {
    "action": "/share",
    "method": "GET",
    "params": {
      "title": "title",
      "text": "text",
      "url": "url"
    }
  }
}"##
    .to_string()
}

/// Returns the service worker JavaScript.
#[must_use]
pub const fn service_worker_js() -> &'static str {
    SERVICE_WORKER_JS
}

/// Returns an SVG icon for the PWA.
#[must_use]
pub const fn icon_svg() -> &'static str {
    ICON_SVG
}

#[must_use]
pub fn format_script_host(host: &str, port: u16, scheme: &str) -> String {
    let default_port = match scheme {
        "https" => 443,
        _ => 80,
    };
    if port == default_port || host_already_has_port(host) {
        return host.to_string();
    }

    let needs_brackets = host.contains(':') && !host.starts_with('[');
    let host_part = if needs_brackets {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    format!("{host_part}:{port}")
}

fn host_already_has_port(host: &str) -> bool {
    host.rfind(':').is_some_and(|colon_pos| {
        if host.starts_with('[') {
            host.rfind(']').is_some_and(|idx| colon_pos > idx)
        } else {
            host[..colon_pos].find(':').is_none() && host[colon_pos + 1..].parse::<u16>().is_ok()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_html_references_ws_endpoint() {
        let html = index_html("example.com:9723", "wss");
        assert!(html.contains("wss://example.com:9723/ws"));
        assert!(html.contains("xterm-addon-fit"));
    }

    #[test]
    fn index_html_has_local_xterm_row_layout_fallback() {
        let html = index_html("example.com:9723", "wss");
        assert!(html.contains("#terminal .xterm-rows > div"));
        assert!(html.contains("white-space: pre"));
        assert!(html.contains("display: block"));
    }

    #[test]
    fn index_html_supports_dlc_file_drop() {
        let html = index_html("example.com:9723", "wss");
        assert!(html.contains("handleDrop"));
        assert!(html.contains("dataTransfer.files"));
        assert!(html.contains("file.text()"));
        assert!(html.contains("\"/api/dlc\""));
        assert!(html.find("dataTransfer.files") < html.find("dataTransfer.items"));
        assert!(html.contains("return;\n      }\n\n      const items"));
    }

    #[test]
    fn bookmarklet_mentions_fallback_host() {
        let html = bookmarklet_html("proxy.host");
        assert!(html.contains("proxy.host"));
        assert!(html.contains("bookmarklet"));
        assert!(!html.contains("__FALLBACK_HOST__"));
        assert!(html.contains("proto+'//proxy.host'"));
    }

    #[test]
    fn manifest_json_contains_name_and_share_target() {
        let manifest = manifest_json("example.com", 9723);
        assert!(manifest.contains("\"name\": \"octo-dl\""));
        assert!(manifest.contains("\"share_target\""));
    }

    #[test]
    fn manifest_json_is_valid_and_uses_relative_start_url() {
        let manifest: serde_json::Value =
            serde_json::from_str(&manifest_json("0.0.0.0", 9723)).unwrap();
        assert_eq!(manifest["start_url"], "/");
        assert_eq!(manifest["icons"][0]["src"], "/icon-192.svg");
        assert_eq!(manifest["share_target"]["action"], "/share");
    }

    #[test]
    fn static_assets_are_nonempty_and_well_known() {
        assert!(service_worker_js().contains("CACHE_NAME"));
        assert!(icon_svg().starts_with("<svg "));
    }

    #[test]
    fn format_script_host_handles_default_ports_and_ipv6() {
        assert_eq!(format_script_host("example.com", 80, "http"), "example.com");
        assert_eq!(
            format_script_host("example.com", 9723, "http"),
            "example.com:9723"
        );
        assert_eq!(format_script_host("[::1]:9723", 80, "http"), "[::1]:9723");
        assert_eq!(format_script_host("::1", 9723, "http"), "[::1]:9723");
    }
}
