//! Web UI frontend assets served inline.
//!
//! All HTML, CSS, JS, manifest, and service worker content is generated
//! as strings so no external static files are needed.

/// Returns the main web UI HTML page.
pub fn index_html(_host: &str, _port: u16) -> String {
    format!(
        r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, maximum-scale=1, user-scalable=no">
<meta name="theme-color" content="#1a1a2e">
<meta name="apple-mobile-web-app-capable" content="yes">
<meta name="apple-mobile-web-app-status-bar-style" content="black-translucent">
<meta name="apple-mobile-web-app-title" content="octo-dl">
<link rel="manifest" href="/manifest.json">
<link rel="icon" href="/icon-192.svg" type="image/svg+xml">
<link rel="apple-touch-icon" href="/icon-192.svg">
<title>octo-dl</title>
<style>
:root {{
  --bg: #1a1a2e;
  --bg2: #16213e;
  --bg3: #0f3460;
  --fg: #e0e0e0;
  --fg2: #a0a0b0;
  --accent: #e94560;
  --green: #4caf50;
  --yellow: #ffc107;
  --red: #ef5350;
  --cyan: #00bcd4;
  --radius: 8px;
}}
* {{ margin: 0; padding: 0; box-sizing: border-box; }}
body {{
  font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', system-ui, sans-serif;
  background: var(--bg);
  color: var(--fg);
  min-height: 100vh;
  min-height: 100dvh;
  display: flex;
  flex-direction: column;
  -webkit-tap-highlight-color: transparent;
}}
/* Header */
.header {{
  background: var(--bg2);
  padding: 12px 16px;
  display: flex;
  align-items: center;
  justify-content: space-between;
  border-bottom: 1px solid var(--bg3);
  position: sticky;
  top: 0;
  z-index: 10;
}}
.header h1 {{
  font-size: 1.1rem;
  color: var(--cyan);
  font-weight: 700;
}}
.header .stats {{
  font-size: 0.75rem;
  color: var(--fg2);
  text-align: right;
}}
/* URL input */
.url-bar {{
  padding: 12px 16px;
  background: var(--bg2);
  border-bottom: 1px solid var(--bg3);
}}
.url-bar form {{
  display: flex;
  gap: 8px;
}}
.url-bar input {{
  flex: 1;
  background: var(--bg);
  color: var(--fg);
  border: 1px solid var(--bg3);
  border-radius: var(--radius);
  padding: 12px;
  font-size: 16px; /* prevents iOS zoom */
  outline: none;
}}
.url-bar input:focus {{
  border-color: var(--cyan);
}}
.url-bar button {{
  background: var(--accent);
  color: #fff;
  border: none;
  border-radius: var(--radius);
  padding: 12px 20px;
  font-size: 0.9rem;
  font-weight: 600;
  cursor: pointer;
  min-width: 44px;
  min-height: 44px;
}}
/* Progress */
.progress-section {{
  padding: 12px 16px;
}}
.progress-bar-outer {{
  background: var(--bg3);
  border-radius: var(--radius);
  height: 28px;
  overflow: hidden;
  position: relative;
}}
.progress-bar-inner {{
  background: var(--green);
  height: 100%;
  transition: width 0.3s ease;
  border-radius: var(--radius);
}}
.progress-label {{
  position: absolute;
  top: 0; left: 0; right: 0; bottom: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 0.8rem;
  font-weight: 600;
  color: #fff;
  text-shadow: 0 1px 2px rgba(0,0,0,0.5);
}}
/* File list */
.file-list {{
  flex: 1;
  overflow-y: auto;
  padding: 8px 16px;
  -webkit-overflow-scrolling: touch;
}}
.file-item {{
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 12px;
  background: var(--bg2);
  border-radius: var(--radius);
  margin-bottom: 6px;
  position: relative;
  overflow: hidden;
  touch-action: pan-y;
  min-height: 56px;
}}
.file-item .file-progress-bg {{
  position: absolute;
  top: 0; left: 0; bottom: 0;
  background: rgba(76, 175, 80, 0.15);
  transition: width 0.3s ease;
  pointer-events: none;
}}
.file-icon {{
  font-size: 1.1rem;
  flex-shrink: 0;
  width: 24px;
  text-align: center;
}}
.file-info {{
  flex: 1;
  min-width: 0;
  z-index: 1;
}}
.file-name {{
  font-size: 0.85rem;
  font-weight: 500;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}}
.file-detail {{
  font-size: 0.75rem;
  color: var(--fg2);
  margin-top: 2px;
}}
.file-actions {{
  display: flex;
  gap: 6px;
  z-index: 1;
}}
.file-actions button {{
  background: transparent;
  border: 1px solid var(--fg2);
  color: var(--fg2);
  border-radius: 4px;
  padding: 6px 10px;
  font-size: 0.7rem;
  cursor: pointer;
  min-width: 44px;
  min-height: 36px;
}}
.file-actions button:hover {{
  border-color: var(--accent);
  color: var(--accent);
}}
.file-actions button.retry {{ border-color: var(--yellow); color: var(--yellow); }}
.file-actions button.delete {{ border-color: var(--red); color: var(--red); }}
/* Status + controls */
.status-bar {{
  padding: 8px 16px;
  font-size: 0.8rem;
  color: var(--fg2);
  background: var(--bg2);
  border-top: 1px solid var(--bg3);
  display: flex;
  align-items: center;
  gap: 8px;
}}
.status-bar .dot {{
  width: 8px;
  height: 8px;
  border-radius: 50%;
  flex-shrink: 0;
}}
.dot.green {{ background: var(--green); }}
.dot.yellow {{ background: var(--yellow); }}
.dot.gray {{ background: var(--fg2); }}
.controls {{
  padding: 8px 16px;
  padding-bottom: max(8px, env(safe-area-inset-bottom));
  display: flex;
  gap: 8px;
  background: var(--bg2);
  border-top: 1px solid var(--bg3);
}}
.controls button {{
  flex: 1;
  background: var(--bg3);
  color: var(--fg);
  border: none;
  border-radius: var(--radius);
  padding: 12px;
  font-size: 0.85rem;
  cursor: pointer;
  min-height: 44px;
  font-weight: 500;
}}
.controls button:active {{
  opacity: 0.7;
}}
.controls button.pause {{ background: var(--yellow); color: #000; }}
.controls button.resume {{ background: var(--green); color: #fff; }}
/* Login popup */
.overlay {{
  position: fixed;
  top: 0; left: 0; right: 0; bottom: 0;
  background: rgba(0,0,0,0.6);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 100;
  padding: 16px;
}}
.popup {{
  background: var(--bg2);
  border: 1px solid var(--cyan);
  border-radius: 12px;
  padding: 24px;
  width: 100%;
  max-width: 400px;
}}
.popup h2 {{
  color: var(--cyan);
  margin-bottom: 16px;
  font-size: 1.1rem;
}}
.popup label {{
  display: block;
  font-size: 0.8rem;
  color: var(--fg2);
  margin-bottom: 4px;
  margin-top: 12px;
}}
.popup input {{
  width: 100%;
  background: var(--bg);
  color: var(--fg);
  border: 1px solid var(--bg3);
  border-radius: var(--radius);
  padding: 12px;
  font-size: 16px;
  outline: none;
}}
.popup input:focus {{
  border-color: var(--cyan);
}}
.popup .btn-row {{
  margin-top: 20px;
  display: flex;
  gap: 8px;
}}
.popup button {{
  flex: 1;
  padding: 12px;
  border: none;
  border-radius: var(--radius);
  font-size: 0.9rem;
  font-weight: 600;
  cursor: pointer;
  min-height: 44px;
}}
.popup button.primary {{
  background: var(--cyan);
  color: #000;
}}
.popup .error {{
  color: var(--red);
  font-size: 0.8rem;
  margin-top: 8px;
}}
/* Config popup */
.config-row {{
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 10px 0;
  border-bottom: 1px solid var(--bg3);
}}
.config-row:last-child {{ border-bottom: none; }}
.config-row label {{ margin: 0; flex: 1; }}
.config-row .config-control {{
  display: flex;
  align-items: center;
  gap: 8px;
}}
.config-row button {{
  width: 36px;
  height: 36px;
  background: var(--bg3);
  color: var(--fg);
  border: none;
  border-radius: 4px;
  font-size: 1rem;
  cursor: pointer;
}}
.config-row .val {{
  min-width: 32px;
  text-align: center;
  font-weight: 600;
}}
/* Empty state */
.empty {{
  flex: 1;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  color: var(--fg2);
  gap: 8px;
  padding: 40px 16px;
}}
.empty .icon {{ font-size: 3rem; opacity: 0.5; }}
/* Connection status */
.conn-badge {{
  position: fixed;
  top: 50%;
  left: 50%;
  transform: translate(-50%, -50%);
  background: var(--red);
  color: #fff;
  padding: 12px 24px;
  border-radius: var(--radius);
  font-weight: 600;
  z-index: 200;
  display: none;
}}
.conn-badge.show {{ display: block; }}
</style>
</head>
<body>

<div id="app">
  <div class="header">
    <h1>octo-dl</h1>
    <div class="stats" id="stats"></div>
  </div>

  <div class="url-bar">
    <form id="url-form">
      <input type="text" id="url-input" placeholder="Paste MEGA URL(s)..." autocomplete="off" autocapitalize="off" spellcheck="false">
      <button type="submit">Add</button>
    </form>
  </div>

  <div class="progress-section">
    <div class="progress-bar-outer">
      <div class="progress-bar-inner" id="progress-bar" style="width:0%"></div>
      <div class="progress-label" id="progress-label">0%</div>
    </div>
  </div>

  <div class="file-list" id="file-list">
    <div class="empty" id="empty-state">
      <div class="icon">&#128194;</div>
      <div>No files yet</div>
      <div style="font-size:0.75rem">Add MEGA URLs above or share from another app</div>
    </div>
  </div>

  <div class="status-bar" id="status-bar">
    <span class="dot gray" id="status-dot"></span>
    <span id="status-text">Connecting...</span>
  </div>

  <div class="controls" id="controls">
    <button id="btn-pause" onclick="togglePause()">Pause</button>
    <button id="btn-config" onclick="showConfig()">Config</button>
    <button id="btn-login" onclick="showLogin()">Login</button>
  </div>
</div>

<!-- Login popup -->
<div class="overlay" id="login-overlay" style="display:none">
  <div class="popup">
    <h2>Login to MEGA</h2>
    <label for="login-email">Email</label>
    <input type="email" id="login-email" autocomplete="email">
    <label for="login-pass">Password</label>
    <input type="password" id="login-pass" autocomplete="current-password">
    <label for="login-mfa">MFA (optional)</label>
    <input type="text" id="login-mfa" inputmode="numeric" autocomplete="one-time-code">
    <div class="error" id="login-error"></div>
    <div class="btn-row">
      <button onclick="hideLogin()">Cancel</button>
      <button class="primary" onclick="doLogin()">Login</button>
    </div>
  </div>
</div>

<!-- Config popup -->
<div class="overlay" id="config-overlay" style="display:none">
  <div class="popup">
    <h2>Configuration</h2>
    <div id="config-body"></div>
    <div class="btn-row">
      <button class="primary" onclick="hideConfig()">Close</button>
    </div>
  </div>
</div>

<div class="conn-badge" id="conn-badge">Disconnected</div>

<script>
(function() {{
  'use strict';

  const API = '';  // same origin
  let state = null;
  let evtSource = null;
  let reconnectTimer = null;

  // ---- SSE connection ----
  function connect() {{
    if (evtSource) evtSource.close();
    evtSource = new EventSource(API + '/api/events');
    evtSource.onmessage = function(e) {{
      try {{
        state = JSON.parse(e.data);
        render(state);
        hideDisconnected();
      }} catch(err) {{ console.error('SSE parse error', err); }}
    }};
    evtSource.onerror = function() {{
      showDisconnected();
      evtSource.close();
      clearTimeout(reconnectTimer);
      reconnectTimer = setTimeout(connect, 3000);
    }};
    evtSource.onopen = function() {{
      hideDisconnected();
      // Fetch initial state
      fetch(API + '/api/state').then(r => r.json()).then(s => {{ state = s; render(s); }}).catch(() => {{}});
    }};
  }}

  function showDisconnected() {{ document.getElementById('conn-badge').classList.add('show'); }}
  function hideDisconnected() {{ document.getElementById('conn-badge').classList.remove('show'); }}

  // ---- Rendering ----
  function formatBytes(b) {{
    if (b === 0) return '0 B';
    const units = ['B','KB','MB','GB','TB'];
    const i = Math.min(Math.floor(Math.log(b)/Math.log(1024)), units.length-1);
    const v = b/Math.pow(1024,i);
    return v.toFixed(i>0?1:0)+' '+units[i];
  }}

  function render(s) {{
    // Stats
    const cpu = Math.min(Math.round(s.cpu_usage), 999);
    document.getElementById('stats').textContent =
      cpu + '% CPU | ' + formatBytes(s.memory_rss) + ' RAM' + (s.paused ? ' | PAUSED' : '');

    // Progress
    const ratio = s.total_size > 0 ? s.total_downloaded / s.total_size : 0;
    const pct = Math.min(Math.round(ratio * 100), 100);
    document.getElementById('progress-bar').style.width = pct + '%';
    document.getElementById('progress-label').textContent =
      pct + '%  ' + s.files_completed + '/' + s.files_total + ' files  ' + formatBytes(s.current_speed) + '/s';

    // Pause button
    const btnPause = document.getElementById('btn-pause');
    if (s.paused) {{
      btnPause.textContent = 'Resume';
      btnPause.className = 'resume';
    }} else {{
      btnPause.textContent = 'Pause';
      btnPause.className = 'pause';
    }}

    // Login button visibility
    document.getElementById('btn-login').style.display = s.authenticated ? 'none' : '';

    // Status
    const dot = document.getElementById('status-dot');
    const statusText = document.getElementById('status-text');
    if (s.authenticated) {{
      dot.className = 'dot green';
      statusText.textContent = 'Logged in \u2713';
    }} else if (s.logging_in) {{
      dot.className = 'dot yellow';
      statusText.textContent = 'Logging in...';
    }} else {{
      dot.className = 'dot gray';
      statusText.textContent = 'Not logged in';
    }}
    if (s.status) {{
      statusText.textContent += ' | ' + s.status;
    }}
    const errCount = s.files.filter(f => f.status === 'error').length;
    if (errCount > 0) {{
      statusText.textContent += ' | ' + errCount + ' failed';
    }}

    // File list
    renderFiles(s.files);
  }}

  // Sort order matching TUI
  function statusOrder(s) {{
    switch(s) {{
      case 'downloading': return 0;
      case 'queued': return 1;
      case 'complete': return 2;
      case 'error': return 3;
      default: return 4;
    }}
  }}

  function renderFiles(files) {{
    const list = document.getElementById('file-list');
    const empty = document.getElementById('empty-state');

    if (files.length === 0) {{
      empty.style.display = '';
      // Remove all file items
      list.querySelectorAll('.file-item').forEach(el => el.remove());
      return;
    }}
    empty.style.display = 'none';

    // Sort files
    const sorted = files.slice().sort((a,b) => statusOrder(a.status) - statusOrder(b.status));

    // Build a map of existing DOM elements by name
    const existing = {{}};
    list.querySelectorAll('.file-item').forEach(el => {{
      existing[el.dataset.name] = el;
    }});

    // Track which names are in the new list
    const currentNames = new Set(sorted.map(f => f.name));

    // Remove items no longer in the list
    for (const name of Object.keys(existing)) {{
      if (!currentNames.has(name)) {{
        existing[name].remove();
        delete existing[name];
      }}
    }}

    // Update or create items in order
    let prevEl = null;
    for (const f of sorted) {{
      let el = existing[f.name];
      if (!el) {{
        el = createFileItem(f);
        existing[f.name] = el;
      }} else {{
        updateFileItem(el, f);
      }}
      // Ensure correct order
      if (prevEl) {{
        if (prevEl.nextElementSibling !== el) {{
          prevEl.after(el);
        }}
      }} else {{
        if (list.firstElementChild !== el || list.firstElementChild === empty) {{
          list.insertBefore(el, list.firstElementChild);
        }}
      }}
      prevEl = el;
    }}
  }}

  function createFileItem(f) {{
    const el = document.createElement('div');
    el.className = 'file-item';
    el.dataset.name = f.name;
    updateFileItem(el, f);
    return el;
  }}

  function updateFileItem(el, f) {{
    const icon = f.status === 'downloading' ? '\u25cf' :
                 f.status === 'queued' ? '\u25cb' :
                 f.status === 'complete' ? '\u2713' : '\u2717';
    const color = f.status === 'downloading' ? 'var(--yellow)' :
                  f.status === 'queued' ? 'var(--fg2)' :
                  f.status === 'complete' ? 'var(--green)' : 'var(--red)';

    let detail = '';
    let bgWidth = '0%';
    if (f.status === 'downloading') {{
      const pct = f.size > 0 ? Math.min(Math.round(f.downloaded/f.size*100),100) : 0;
      detail = pct + '%  ' + formatBytes(f.speed) + '/s';
      bgWidth = pct + '%';
    }} else if (f.status === 'queued') {{
      detail = 'queued';
    }} else if (f.status === 'complete') {{
      detail = formatBytes(f.size) + '  done';
    }} else {{
      detail = f.error || 'error';
    }}

    let actions = '';
    if (f.status === 'error') {{
      actions += '<button class="retry" onclick="retryFile(\'' + escHtml(f.name) + '\')">Retry</button>';
    }}
    if (f.status !== 'complete') {{
      actions += '<button class="delete" onclick="deleteFile(\'' + escHtml(f.name) + '\')">\u2717</button>';
    }}

    el.innerHTML =
      '<div class="file-progress-bg" style="width:' + bgWidth + '"></div>' +
      '<span class="file-icon" style="color:' + color + '">' + icon + '</span>' +
      '<div class="file-info">' +
        '<div class="file-name">' + escHtml(f.name) + '</div>' +
        '<div class="file-detail">' + detail + '</div>' +
      '</div>' +
      '<div class="file-actions">' + actions + '</div>';
  }}

  function escHtml(s) {{
    return s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/'/g,'&#39;').replace(/"/g,'&quot;');
  }}

  // ---- Actions ----
  function post(path, body) {{
    return fetch(API + path, {{
      method: 'POST',
      headers: {{'Content-Type': 'application/json'}},
      body: JSON.stringify(body || {{}})
    }});
  }}

  window.togglePause = function() {{ post('/api/pause'); }};

  window.deleteFile = function(name) {{ post('/api/delete', {{name: name}}); }};

  window.retryFile = function(name) {{ post('/api/retry', {{name: name}}); }};

  window.showLogin = function() {{ document.getElementById('login-overlay').style.display = ''; }};
  window.hideLogin = function() {{ document.getElementById('login-overlay').style.display = 'none'; }};

  window.doLogin = function() {{
    const email = document.getElementById('login-email').value;
    const password = document.getElementById('login-pass').value;
    const mfa = document.getElementById('login-mfa').value;
    if (!email || !password) {{
      document.getElementById('login-error').textContent = 'Email and password are required';
      return;
    }}
    document.getElementById('login-error').textContent = '';
    post('/api/login', {{email: email, password: password, mfa: mfa}});
    hideLogin();
  }};

  window.showConfig = function() {{
    if (!state) return;
    const c = state.config;
    const body = document.getElementById('config-body');
    body.innerHTML =
      configRow('Chunks per file', 'chunks_per_file', c.chunks_per_file, 'number') +
      configRow('Concurrent files', 'concurrent_files', c.concurrent_files, 'number') +
      configRow('Force overwrite', 'force_overwrite', c.force_overwrite, 'bool') +
      configRow('Cleanup on error', 'cleanup_on_error', c.cleanup_on_error, 'bool');
    document.getElementById('config-overlay').style.display = '';
  }};
  window.hideConfig = function() {{ document.getElementById('config-overlay').style.display = 'none'; }};

  function configRow(label, key, val, type) {{
    if (type === 'number') {{
      return '<div class="config-row"><label>' + label + '</label><div class="config-control">' +
        '<button onclick="cfgDec(\'' + key + '\')">-</button>' +
        '<span class="val" id="cfg-' + key + '">' + val + '</span>' +
        '<button onclick="cfgInc(\'' + key + '\')">+</button></div></div>';
    }} else {{
      return '<div class="config-row"><label>' + label + '</label><div class="config-control">' +
        '<button onclick="cfgToggle(\'' + key + '\')" id="cfg-' + key + '">' + (val ? 'Yes' : 'No') + '</button></div></div>';
    }}
  }}

  window.cfgInc = function(key) {{
    const update = {{}}; update[key] = (state.config[key] || 1) + 1;
    post('/api/config', update);
  }};
  window.cfgDec = function(key) {{
    const update = {{}}; update[key] = Math.max(1, (state.config[key] || 2) - 1);
    post('/api/config', update);
  }};
  window.cfgToggle = function(key) {{
    const update = {{}}; update[key] = !state.config[key];
    post('/api/config', update);
  }};

  // URL submission
  document.getElementById('url-form').addEventListener('submit', function(e) {{
    e.preventDefault();
    const input = document.getElementById('url-input');
    const text = input.value.trim();
    if (text) {{
      post('/api/urls', {{text: text}});
      input.value = '';
    }}
  }});

  // Register service worker (PWA)
  if ('serviceWorker' in navigator) {{
    navigator.serviceWorker.register('/sw.js').catch(function(err) {{
      console.log('SW registration failed:', err);
    }});
  }}

  // Start SSE connection
  connect();

}})();
</script>
</body>
</html>"##
    )
}

/// Returns the PWA manifest JSON.
pub fn manifest_json(_host: &str, _port: u16) -> String {
    format!(
        r##"{{
  "name": "octo-dl",
  "short_name": "octo-dl",
  "description": "MEGA file downloader",
  "start_url": "/",
  "display": "standalone",
  "background_color": "#1a1a2e",
  "theme_color": "#1a1a2e",
  "orientation": "any",
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
}}"##
    )
}

/// Returns the service worker JavaScript for PWA offline support and share target.
pub fn service_worker_js() -> &'static str {
    r##"// octo-dl Service Worker
const CACHE_NAME = 'octo-dl-v1';
const PRECACHE = ['/', '/manifest.json', '/icon-192.svg'];

self.addEventListener('install', function(event) {
  event.waitUntil(
    caches.open(CACHE_NAME).then(function(cache) {
      return cache.addAll(PRECACHE);
    })
  );
  self.skipWaiting();
});

self.addEventListener('activate', function(event) {
  event.waitUntil(
    caches.keys().then(function(names) {
      return Promise.all(
        names.filter(function(n) { return n !== CACHE_NAME; })
             .map(function(n) { return caches.delete(n); })
      );
    })
  );
  self.clients.claim();
});

self.addEventListener('fetch', function(event) {
  const url = new URL(event.request.url);

  // Handle share target — forward to the app and let the server process it
  if (url.pathname === '/share') {
    event.respondWith(
      fetch(event.request).then(function(response) {
        // If the server redirects to /, follow it
        return response;
      }).catch(function() {
        // Offline fallback: redirect to cached index
        return caches.match('/').then(function(cached) {
          return cached || new Response('Offline — could not process shared URLs', {
            status: 503,
            headers: { 'Content-Type': 'text/plain' }
          });
        });
      })
    );
    return;
  }

  // API and SSE requests should always go to network
  if (url.pathname.startsWith('/api/')) {
    return;
  }

  // Cache-first for static assets, network-first for the SPA shell
  event.respondWith(
    caches.match(event.request).then(function(cached) {
      if (cached) {
        // Update cache in background
        fetch(event.request).then(function(response) {
          if (response.ok) {
            caches.open(CACHE_NAME).then(function(cache) {
              cache.put(event.request, response);
            });
          }
        }).catch(function() {});
        return cached;
      }
      return fetch(event.request).then(function(response) {
        if (response.ok) {
          var clone = response.clone();
          caches.open(CACHE_NAME).then(function(cache) {
            cache.put(event.request, clone);
          });
        }
        return response;
      }).catch(function() {
        return new Response('Offline', { status: 503 });
      });
    })
  );
});
"##
}

/// Returns an SVG icon for the PWA.
pub fn icon_svg() -> &'static str {
    r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 192 192">
  <rect width="192" height="192" rx="32" fill="#1a1a2e"/>
  <g transform="translate(96,96)">
    <circle r="60" fill="none" stroke="#00bcd4" stroke-width="6"/>
    <path d="M-20,-15 L0,15 L20,-15" fill="none" stroke="#e94560" stroke-width="6" stroke-linecap="round" stroke-linejoin="round"/>
    <line x1="0" y1="15" x2="0" y2="35" stroke="#e94560" stroke-width="6" stroke-linecap="round"/>
    <line x1="-25" y1="35" x2="25" y2="35" stroke="#e94560" stroke-width="6" stroke-linecap="round"/>
  </g>
</svg>"##
}
