/// Web assets for the xterm.js terminal UI and bookmarklet helpers.

/// Returns the main web UI HTML page (the xterm.js terminal).
pub fn index_html(host: &str, scheme: &str) -> String {
    let template = r##"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>octo-dl terminal</title>
  <link rel="manifest" href="/manifest.json">
  <link rel="icon" href="/icon-192.svg">
  <link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/xterm@5.3.0/css/xterm.css">
  <style>
    html, body { height: 100%; margin: 0; background: #05060a; }
    #terminal { width: 100%; height: 100%; }
    #notice {
      position: absolute;
      left: 16px;
      top: 16px;
      padding: 4px 10px;
      border-radius: 6px;
      background: rgba(0, 0, 0, 0.6);
      color: #c5c5c5;
      font-family: system-ui, sans-serif;
      font-size: 0.8rem;
      z-index: 5;
    }
  </style>
</head>
<body>
  <div id="terminal"></div>
  <div id="notice">Connecting…</div>
  <script src="https://cdn.jsdelivr.net/npm/xterm@5.3.0/lib/xterm.js"></script>
  <script src="https://cdn.jsdelivr.net/npm/xterm-addon-fit@0.7.0/lib/xterm-addon-fit.js"></script>
  <script>
    const term = new Terminal({ cursorBlink: true, fontFamily: "JetBrains Mono, ui-monospace, monospace" });
    const fitAddon = new FitAddon.FitAddon();
    term.loadAddon(fitAddon);
    term.open(document.getElementById("terminal"));
    term.focus();
    fitAddon.fit();

    let socket;
    const notice = document.getElementById("notice");
    notice.style.transition = "opacity 0.25s ease";
    let hideAfterMessage = false;
    let reconnectAttempt = 0;

    const showNotice = (text) => {
      notice.textContent = text;
      notice.style.opacity = "1";
    };

    const hideNotice = () => {
      notice.style.opacity = "0";
    };

    const connect = () => {
      socket = new WebSocket("${WS_SCHEME}://${WS_HOST}/ws");
      socket.binaryType = "arraybuffer";
      socket.addEventListener("open", () => {
        reconnectAttempt = 0;
        showNotice("Connected");
        hideAfterMessage = true;
      });
      socket.addEventListener("close", () => {
        reconnectAttempt += 1;
        showNotice(
          `Connection closed, reconnecting… (attempt ${reconnectAttempt})`
        );
        hideAfterMessage = false;
        setTimeout(connect, 800);
      });
      socket.addEventListener("message", (event) => {
        term.write(new Uint8Array(event.data));
        if (hideAfterMessage) {
          hideAfterMessage = false;
          setTimeout(hideNotice, 250);
        }
      });
    };
    connect();

    term.onData((data) => {
      if (socket && socket.readyState === WebSocket.OPEN) {
        socket.send(data);
      }
    });

    window.addEventListener("resize", () => fitAddon.fit());

    const sendText = (text) => {
      fetch("/api/urls", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ text }),
      }).catch((err) => console.error("Failed to send URLs:", err));
    };

    const sendDlcFile = (content, filename) => {
      fetch("/api/dlc", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ content, filename }),
      }).catch((err) => console.error("Failed to parse DLC file:", err));
    };

    const handleDrop = async (dataTransfer) => {
      const items = Array.from(dataTransfer.items || []);
      for (const item of items) {
        if (item.kind === "string") {
          item.getAsString(sendText);
        } else if (item.kind === "file") {
          const file = item.getAsFile();
          if (file) {
            const contents = await file.text();
            sendDlcFile(contents, file.name);
          }
        }
      }
    };

    document.addEventListener("dragover", (event) => event.preventDefault());
    document.addEventListener("drop", (event) => {
      event.preventDefault();
      handleDrop(event.dataTransfer);
    });
  </script>
</body>
</html>"##;
    template
        .replace("${WS_SCHEME}", scheme)
        .replace("${WS_HOST}", host)
}

/// Returns the bookmarklet helper page.
pub fn dashboard_html() -> &'static str {
    r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>octo-dl</title>
  <link rel="manifest" href="/manifest.json">
  <link rel="icon" href="/icon-192.svg">
  <style>
    :root {
      color-scheme: dark;
      --bg: #0b1220;
      --panel: #101b2f;
      --panel-alt: #14233d;
      --accent: #4dd0e1;
      --danger: #ff6b6b;
      --ok: #61d095;
      --text: #e9eef7;
      --muted: #92a2bd;
      --border: #223456;
      font-family: "IBM Plex Sans", system-ui, sans-serif;
    }
    * { box-sizing: border-box; }
    body { margin: 0; background: radial-gradient(circle at top, #172946, var(--bg) 60%); color: var(--text); }
    main { max-width: 980px; margin: 0 auto; padding: 24px; display: grid; gap: 16px; }
    section { background: rgba(16, 27, 47, 0.92); border: 1px solid var(--border); border-radius: 16px; padding: 16px; }
    h1, h2 { margin: 0 0 12px; }
    .muted { color: var(--muted); }
    .row { display: flex; gap: 12px; flex-wrap: wrap; align-items: center; }
    .row > * { min-width: 0; }
    input { width: 100%; padding: 10px 12px; border-radius: 10px; border: 1px solid var(--border); background: var(--panel-alt); color: var(--text); }
    button { padding: 10px 14px; border-radius: 10px; border: 1px solid var(--border); background: var(--panel-alt); color: var(--text); cursor: pointer; }
    button.primary { background: var(--accent); color: #09111f; border-color: transparent; }
    button.danger { background: rgba(255, 107, 107, 0.16); color: #ffd7d7; }
    button:disabled { opacity: 0.55; cursor: default; }
    .stats { display: grid; grid-template-columns: repeat(auto-fit, minmax(150px, 1fr)); gap: 12px; }
    .stat { padding: 12px; border-radius: 12px; background: var(--panel-alt); }
    .files { display: grid; gap: 10px; }
    .file { padding: 12px; border: 1px solid var(--border); border-radius: 12px; background: rgba(20, 35, 61, 0.9); }
    .file-header { display: flex; justify-content: space-between; gap: 12px; align-items: center; }
    .file-name { overflow-wrap: anywhere; font-weight: 600; }
    .badge { padding: 4px 8px; border-radius: 999px; font-size: 0.8rem; border: 1px solid var(--border); }
    .queued { color: var(--muted); }
    .downloading { color: var(--accent); }
    .complete { color: var(--ok); }
    .error { color: #ffc4c4; background: rgba(255, 107, 107, 0.12); }
    .hidden { display: none; }
    code { color: var(--accent); }
  </style>
</head>
<body>
  <main>
    <section>
      <div class="row" style="justify-content: space-between;">
        <div>
          <h1>octo-dl</h1>
          <div id="status" class="muted">Connecting…</div>
        </div>
        <div class="row">
          <button id="pauseBtn">Pause</button>
        </div>
      </div>
    </section>

    <section id="loginSection">
      <h2>Login</h2>
      <div class="row">
        <input id="email" type="email" placeholder="MEGA email">
        <input id="password" type="password" placeholder="Password">
        <input id="mfa" type="text" placeholder="MFA (optional)">
      </div>
      <div class="row" style="margin-top: 12px;">
        <button id="loginBtn" class="primary">Login</button>
        <span id="loginError" class="muted"></span>
      </div>
    </section>

    <section>
      <h2>Overview</h2>
      <div class="stats">
        <div class="stat"><div class="muted">Files</div><div id="filesStat">0 / 0</div></div>
        <div class="stat"><div class="muted">Downloaded</div><div id="bytesStat">0 / 0</div></div>
        <div class="stat"><div class="muted">Speed</div><div id="speedStat">0/s</div></div>
        <div class="stat"><div class="muted">API Port</div><div id="portStat">-</div></div>
      </div>
    </section>

    <section>
      <h2>Files</h2>
      <div id="files" class="files"></div>
    </section>
  </main>

  <script>
    const fmt = (n) => {
      const units = ["B", "KB", "MB", "GB", "TB"];
      let value = Number(n || 0);
      let unit = 0;
      while (value >= 1024 && unit < units.length - 1) {
        value /= 1024;
        unit += 1;
      }
      const digits = unit === 0 ? 0 : value >= 10 ? 1 : 2;
      return `${value.toFixed(digits)} ${units[unit]}`;
    };

    const state = { files: [] };
    const filesEl = document.getElementById("files");
    const statusEl = document.getElementById("status");
    const loginSection = document.getElementById("loginSection");
    const loginError = document.getElementById("loginError");
    const pauseBtn = document.getElementById("pauseBtn");

    const postJson = async (url, body) => {
      const response = await fetch(url, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      });
      if (!response.ok) {
        const payload = await response.text();
        throw new Error(payload || `Request failed: ${response.status}`);
      }
      return response;
    };

    const renderFiles = () => {
      filesEl.innerHTML = "";
      for (const file of state.files) {
        const item = document.createElement("div");
        item.className = "file";
        const retryDisabled = !(file.status && file.status.error);
        const badgeClass = file.status?.downloading ? "downloading" :
          file.status?.complete ? "complete" :
          file.status?.error ? "error" : "queued";
        const badgeText = file.status?.error ?? (file.status?.downloading ? "downloading" :
          file.status?.complete ? "complete" : "queued");
        item.innerHTML = `
          <div class="file-header">
            <div class="file-name">${file.name}</div>
            <span class="badge ${badgeClass}">${badgeText}</span>
          </div>
          <div class="muted">${fmt(file.downloaded)} / ${fmt(file.size)} at ${fmt(file.speed)}/s</div>
          <div class="row" style="margin-top: 10px;">
            <button data-id="${file.id}" data-action="retry" ${retryDisabled ? "disabled" : ""}>Retry</button>
            <button data-id="${file.id}" data-action="delete" class="danger">Delete</button>
          </div>`;
        filesEl.appendChild(item);
      }
    };

    const normalizeStatus = (status) => {
      if (typeof status === "string") return { [status]: true };
      if (status && typeof status === "object") {
        const [key, value] = Object.entries(status)[0] || [];
        return key ? { [key]: value ?? true } : {};
      }
      return {};
    };

    const render = (snapshot) => {
      state.files = (snapshot.files || []).map((file) => ({
        ...file,
        status: normalizeStatus(file.status),
      }));
      statusEl.textContent = snapshot.login_error || (snapshot.paused ? "Paused" : "Ready");
      document.getElementById("filesStat").textContent = `${snapshot.files_completed} / ${snapshot.files_total}`;
      document.getElementById("bytesStat").textContent = `${fmt(snapshot.total_downloaded)} / ${fmt(snapshot.total_size)}`;
      document.getElementById("speedStat").textContent = `${fmt(snapshot.current_speed)}/s`;
      document.getElementById("portStat").textContent = snapshot.api_port;
      pauseBtn.textContent = snapshot.paused ? "Resume" : "Pause";
      loginSection.classList.toggle("hidden", snapshot.authenticated || snapshot.logging_in);
      loginError.textContent = snapshot.login_error || "";
      renderFiles();
    };

    document.getElementById("loginBtn").addEventListener("click", async () => {
      try {
        loginError.textContent = "";
        await postJson("/api/login", {
          email: document.getElementById("email").value,
          password: document.getElementById("password").value,
          mfa: document.getElementById("mfa").value,
        });
      } catch (error) {
        loginError.textContent = error.message;
      }
    });

    pauseBtn.addEventListener("click", async () => {
      try {
        await postJson("/api/pause", {});
      } catch (error) {
        statusEl.textContent = error.message;
      }
    });

    filesEl.addEventListener("click", async (event) => {
      const button = event.target.closest("button[data-action]");
      if (!button) return;
      const id = button.dataset.id;
      const action = button.dataset.action;
      try {
        await postJson(`/api/${action}`, { id });
      } catch (error) {
        statusEl.textContent = error.message;
      }
    });

    fetch("/api/state")
      .then((response) => response.json())
      .then(render)
      .catch((error) => {
        statusEl.textContent = `State load failed: ${error}`;
      });

    const events = new EventSource("/api/events");
    events.onmessage = (event) => {
      render(JSON.parse(event.data));
    };
    events.onerror = () => {
      statusEl.textContent = "Connection lost; retrying…";
    };
  </script>
</body>
</html>"#
}

/// Returns the bookmarklet helper page.
pub fn bookmarklet_html(fallback_host: &str) -> String {
    format!(
        r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>octo-dl bookmarklet</title>
<style>
  body {{ font-family: system-ui, sans-serif; max-width: 480px; margin: 60px auto; color: #e0e0e0; background: #1a1a2e; }}
  h1 {{ font-size: 1.4rem; }}
  p {{ line-height: 1.5; }}
  a.bookmarklet {{
    display: inline-block; padding: 10px 20px; margin: 20px 0;
    background: #0f3460; color: #e94560; border-radius: 6px;
    text-decoration: none; font-weight: bold; font-size: 1.1rem;
    border: 2px solid #e94560; cursor: grab;
  }}
  a.bookmarklet:hover {{ background: #16213e; }}
  code {{ background: #16213e; padding: 2px 6px; border-radius: 3px; }}
</style>
</head>
<body>
<h1>octo-dl bookmarklet</h1>
<p>Drag this link to your bookmarks bar:</p>
<a class="bookmarklet" href="javascript:void(function(){{
var page=document.documentElement.outerHTML;
var selected=window.getSelection().toString();
var proto=window.location.protocol;
var h=proto+'//{fallback_host}';
fetch(h+'/api/parse',{{method:'POST',headers:{{'Content-Type':'application/json'}},body:JSON.stringify({{page:page,fallback:selected}})}}).then(function(r){{return r.json()}}).then(function(d){{if(d.count>0){{alert('Sent '+d.count+' URL(s) to octo-dl')}}else{{alert('No URLs found on this page')}}}}).catch(function(e){{alert('Error: '+e)}})}})()">Send to octo-dl</a>
<p>Click it on any page to send the page HTML (with selected text as fallback) to octo-dl for download.</p>
<p>Configured to use <code>{fallback_host}</code></p>
</body>
</html>"##,
    )
}

/// Returns the PWA manifest JSON.
pub fn manifest_json(host: &str, _port: u16) -> String {
    let start_url = if host != "127.0.0.1" && host != "0.0.0.0" && !host.is_empty() {
        "/"
    } else {
        "/"
    };
    format!(
        r##"{{
  "name": "octo-dl",
  "short_name": "octo",
  "description": "MEGA file download manager",
  "start_url": "{start_url}",
  "scope": "/",
  "display": "standalone",
  "background_color": "#1a1a2e",
  "theme_color": "#1a1a2e",
  "orientation": "portrait-primary",
  "prefer_related_applications": false,
  "icons": [
    {{
      "src": "/icon-192.svg",
      "sizes": "192x192",
      "type": "image/svg+xml",
      "purpose": "any maskable"
    }},
    {{
      "src": "/icon-512.svg",
      "sizes": "512x512",
      "type": "image/svg+xml",
      "purpose": "any maskable"
    }}
  ],
  "share_target": {{
    "action": "/share",
    "method": "GET",
    "params": {{
      "title": "title",
      "text": "text",
      "url": "url"
    }}
  }}
}}"##,
    )
}

/// Returns the service worker JavaScript.
pub fn service_worker_js() -> &'static str {
    r##"// octo-dl Service Worker
const CACHE_NAME = 'octo-dl-v2';
const PRECACHE = ['/', '/manifest.json', '/icon-192.svg', '/icon-512.svg'];

self.addEventListener('install', function(event) {
  event.waitUntil(
    caches.open(CACHE_NAME).then(function(cache) {
      return cache.addAll(PRECACHE).catch(function() {
        // silently ignore failures during install
      });
    })
  );
});

self.addEventListener('activate', function(event) {
  event.waitUntil(
    caches.keys().then(function(keys) {
      return Promise.all(
        keys.filter(function(key) { return key !== CACHE_NAME; }).map(function(key) {
          return caches.delete(key);
        })
      );
    })
  );
});

self.addEventListener('fetch', function(event) {
  if (event.request.method !== 'GET') return;
  event.respondWith(
    caches.match(event.request).then(function(response) {
      return response || fetch(event.request);
    })
  );
});

self.addEventListener('message', function(event) {
  if (event.data && event.data.type === 'SKIP_WAITING') {
    self.skipWaiting();
  }
});
"##
}

/// Returns an SVG icon for the PWA.
pub fn icon_svg() -> &'static str {
    r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 192 192">
  <defs>
    <style>
      .icon-bg { fill: #1a1a2e; }
      .icon-ring { stroke: #00bcd4; stroke-width: 6; fill: none; }
      .icon-arrow { stroke: #e94560; fill: none; stroke-width: 6; stroke-linecap: round; stroke-linejoin: round; }
      .icon-stem { stroke: #e94560; stroke-width: 6; stroke-linecap: round; }
    </style>
  </defs>
  <rect class="icon-bg" width="192" height="192" rx="32"/>
  <g transform="translate(96,96)">
    <circle class="icon-ring" r="60"/>
    <path class="icon-arrow" d="M-20,-15 L0,15 L20,-15"/>
    <line class="icon-stem" x1="0" y1="15" x2="0" y2="40"/>
    <line class="icon-stem" x1="-30" y1="45" x2="30" y2="45"/>
  </g>
</svg>"##
}

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
    if let Some(colon_pos) = host.rfind(':') {
        if host.starts_with('[') {
            host.rfind(']').map_or(false, |idx| colon_pos > idx)
        } else {
            host[colon_pos + 1..].parse::<u16>().is_ok()
        }
    } else {
        false
    }
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
    fn bookmarklet_mentions_fallback_host() {
        let html = bookmarklet_html("proxy.host");
        assert!(html.contains("proxy.host"));
        assert!(html.contains("bookmarklet"));
    }

    #[test]
    fn dashboard_html_uses_state_and_id_controls() {
        let html = dashboard_html();
        assert!(html.contains("/api/events"));
        assert!(html.contains("data-id"));
        assert!(html.contains("/api/${action}"));
    }

    #[test]
    fn manifest_json_contains_name_and_share_target() {
        let manifest = manifest_json("example.com", 9723);
        assert!(manifest.contains("\"name\": \"octo-dl\""));
        assert!(manifest.contains("\"share_target\""));
    }
}
