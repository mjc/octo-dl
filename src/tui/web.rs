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
html {{ font-size: clamp(14px, 2vw, 16px); }}
body {{
  font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', system-ui, sans-serif;
  background: var(--bg);
  color: var(--fg);
  min-height: 100vh;
  min-height: 100dvh;
  display: flex;
  flex-direction: column;
  -webkit-tap-highlight-color: transparent;
  -webkit-user-select: none;
  user-select: none;
}}
/* Header */
.header {{
  background: var(--bg2);
  padding: clamp(8px, 2vw, 12px) clamp(12px, 3vw, 16px);
  display: flex;
  align-items: center;
  justify-content: space-between;
  border-bottom: 1px solid var(--bg3);
  position: sticky;
  top: 0;
  z-index: 10;
  flex-shrink: 0;
}}
.header h1 {{
  font-size: clamp(1rem, 5vw, 1.2rem);
  color: var(--cyan);
  font-weight: 700;
  letter-spacing: -0.5px;
}}
.header .stats {{
  font-size: clamp(0.65rem, 1.5vw, 0.8rem);
  color: var(--fg2);
  text-align: right;
  line-height: 1.3;
}}
/* URL input */
.url-bar {{
  padding: clamp(8px, 2vw, 12px) clamp(12px, 3vw, 16px);
  background: var(--bg2);
  border-bottom: 1px solid var(--bg3);
  flex-shrink: 0;
}}
.url-bar form {{
  display: flex;
  gap: clamp(6px, 2vw, 8px);
}}
.url-bar input {{
  flex: 1;
  background: var(--bg);
  color: var(--fg);
  border: 1px solid var(--bg3);
  border-radius: var(--radius);
  padding: clamp(10px, 2vw, 12px);
  font-size: 16px;
  outline: none;
  -webkit-appearance: none;
  appearance: none;
}}
@media (max-width: 480px) {{
  .url-bar input {{ font-size: 18px; }}
}}
.url-bar input:focus {{
  border-color: var(--cyan);
  box-shadow: 0 0 0 2px rgba(0,188,212,0.1);
}}
.url-bar button {{
  background: var(--accent);
  color: #fff;
  border: none;
  border-radius: var(--radius);
  padding: clamp(10px, 2vw, 12px) clamp(16px, 3vw, 20px);
  font-size: clamp(0.8rem, 2vw, 0.9rem);
  font-weight: 600;
  cursor: pointer;
  min-height: 44px;
  min-width: 44px;
  white-space: nowrap;
  transition: background 0.2s;
}}
.url-bar button:active {{
  opacity: 0.8;
}}
/* Progress bar */
.progress-section {{
  padding: clamp(6px, 1.5vw, 10px) clamp(12px, 3vw, 16px);
  background: var(--bg2);
  border-bottom: 1px solid var(--bg3);
  flex-shrink: 0;
}}
.progress-bar-outer {{
  position: relative;
  background: var(--bg3);
  border-radius: 4px;
  height: 24px;
  overflow: hidden;
}}
.progress-bar-inner {{
  background: linear-gradient(90deg, var(--cyan), var(--green));
  height: 100%;
  width: 0%;
  transition: width 0.3s ease;
}}
.progress-label {{
  position: absolute;
  top: 50%;
  left: 50%;
  transform: translate(-50%, -50%);
  font-size: 0.75rem;
  font-weight: 600;
  color: #fff;
  text-shadow: 0 1px 2px rgba(0,0,0,0.5);
}}
/* File list */
.file-list {{
  flex: 1;
  overflow-y: auto;
  overflow-x: hidden;
  -webkit-overflow-scrolling: touch;
}}
.file-item {{
  display: flex;
  align-items: center;
  gap: clamp(8px, 2vw, 12px);
  padding: clamp(10px, 2vw, 12px) clamp(12px, 3vw, 16px);
  border-bottom: 1px solid var(--bg3);
  position: relative;
  overflow: hidden;
  background: var(--bg);
}}
.file-item.error {{ background: rgba(239, 83, 80, 0.05); }}
.file-progress-bg {{
  position: absolute;
  top: 0; left: 0; bottom: 0;
  background: rgba(0,188,212,0.1);
  transition: width 0.3s ease;
}}
.file-icon {{
  font-size: clamp(1rem, 3vw, 1.2rem);
  min-width: 1.2em;
  text-align: center;
  flex-shrink: 0;
  position: relative;
  z-index: 1;
}}
.file-info {{
  flex: 1;
  min-width: 0;
  position: relative;
  z-index: 1;
}}
.file-name {{
  font-weight: 600;
  font-size: clamp(0.9rem, 2vw, 1rem);
  color: var(--fg);
  word-break: break-word;
  margin-bottom: 2px;
}}
.file-detail {{
  font-size: clamp(0.7rem, 1.5vw, 0.8rem);
  color: var(--fg2);
}}
.file-item.error .file-detail {{
  color: var(--red);
  font-weight: 500;
}}
.file-actions {{
  display: flex;
  gap: 4px;
  flex-shrink: 0;
  position: relative;
  z-index: 1;
}}
.file-actions button {{
  width: 32px;
  height: 32px;
  min-height: 32px;
  padding: 0;
  background: transparent;
  border: 1px solid var(--fg2);
  border-radius: 4px;
  color: var(--fg2);
  font-size: 1rem;
  cursor: pointer;
  display: flex;
  align-items: center;
  justify-content: center;
  transition: all 0.2s;
}}
@media (max-width: 480px) {{
  .file-actions button {{
    width: 40px;
    height: 40px;
    min-height: 40px;
  }}
}}
.file-actions button:active {{
  transform: scale(0.95);
}}
.file-actions button.retry {{ border-color: var(--yellow); color: var(--yellow); }}
.file-actions button.delete {{ border-color: var(--red); color: var(--red); }}
/* Status bar */
.status-bar {{
  padding: clamp(8px, 1.5vw, 10px) clamp(12px, 3vw, 16px);
  font-size: clamp(0.7rem, 1.5vw, 0.8rem);
  color: var(--fg2);
  background: var(--bg2);
  border-top: 1px solid var(--bg3);
  display: flex;
  align-items: center;
  gap: 6px;
  flex-shrink: 0;
}}
.status-bar .dot {{
  width: 8px;
  height: 8px;
  border-radius: 50%;
  flex-shrink: 0;
  animation: pulse 2s infinite;
}}
@keyframes pulse {{
  0%, 100% {{ opacity: 1; }}
  50% {{ opacity: 0.5; }}
}}
.dot.green {{ background: var(--green); animation: none; }}
.dot.yellow {{ background: var(--yellow); animation: none; }}
.dot.gray {{ background: var(--fg2); animation: none; }}
.dot.red {{ background: var(--red); animation: none; }}
/* Controls */
.controls {{
  padding: clamp(8px, 2vw, 10px) clamp(12px, 3vw, 16px);
  padding-bottom: max(clamp(8px, 2vw, 10px), env(safe-area-inset-bottom));
  display: flex;
  gap: clamp(6px, 2vw, 8px);
  background: var(--bg2);
  border-top: 1px solid var(--bg3);
  flex-shrink: 0;
}}
.controls button {{
  flex: 1;
  background: var(--bg3);
  color: var(--fg);
  border: none;
  border-radius: var(--radius);
  padding: clamp(10px, 2vw, 12px);
  font-size: clamp(0.8rem, 1.5vw, 0.85rem);
  cursor: pointer;
  min-height: 44px;
  font-weight: 500;
  transition: background 0.2s, transform 0.1s;
  -webkit-appearance: none;
  appearance: none;
}}
.controls button:active {{
  transform: scale(0.98);
  opacity: 0.8;
}}
.controls button.pause {{ background: var(--yellow); color: #000; }}
.controls button.resume {{ background: var(--green); color: #fff; }}
.controls button:disabled {{
  opacity: 0.5;
  cursor: not-allowed;
}}
/* Overlays */
.overlay {{
  position: fixed;
  top: 0; left: 0; right: 0; bottom: 0;
  background: rgba(0,0,0,0.7);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 100;
  padding: clamp(12px, 3vw, 20px);
}}
.popup {{
  background: var(--bg2);
  border: 1px solid var(--cyan);
  border-radius: 12px;
  padding: clamp(16px, 4vw, 24px);
  width: 100%;
  max-width: 420px;
  max-height: 90vh;
  overflow-y: auto;
  -webkit-overflow-scrolling: touch;
  box-shadow: 0 10px 40px rgba(0,0,0,0.3);
}}
.popup h2 {{
  color: var(--cyan);
  margin-bottom: clamp(12px, 2vw, 16px);
  font-size: clamp(1rem, 3vw, 1.2rem);
}}
.popup label {{
  display: block;
  font-size: clamp(0.75rem, 1.5vw, 0.8rem);
  color: var(--fg2);
  margin-bottom: 4px;
  margin-top: clamp(10px, 2vw, 12px);
  font-weight: 500;
}}
.popup input, .popup button {{
  -webkit-appearance: none;
  appearance: none;
}}
.popup input {{
  width: 100%;
  background: var(--bg);
  color: var(--fg);
  border: 1px solid var(--bg3);
  border-radius: var(--radius);
  padding: clamp(10px, 2vw, 12px);
  font-size: 16px;
  outline: none;
  transition: border-color 0.2s;
}}
.popup input:focus {{
  border-color: var(--cyan);
  box-shadow: 0 0 0 2px rgba(0,188,212,0.1);
}}
.popup .error {{
  background: rgba(239, 83, 80, 0.1);
  color: var(--red);
  border: 1px solid rgba(239, 83, 80, 0.3);
  border-radius: var(--radius);
  padding: clamp(8px, 1.5vw, 12px);
  font-size: clamp(0.75rem, 1.5vw, 0.85rem);
  margin-top: clamp(8px, 1.5vw, 12px);
  margin-bottom: clamp(8px, 1.5vw, 12px);
}}
.popup .btn-row {{
  margin-top: clamp(16px, 3vw, 20px);
  display: flex;
  gap: clamp(6px, 2vw, 8px);
}}
.popup button {{
  flex: 1;
  padding: clamp(10px, 2vw, 12px);
  border: none;
  border-radius: var(--radius);
  font-size: clamp(0.85rem, 2vw, 0.9rem);
  font-weight: 600;
  cursor: pointer;
  min-height: 44px;
  transition: background 0.2s, opacity 0.2s;
}}
.popup button:not(.primary) {{
  background: var(--bg3);
  color: var(--fg);
}}
.popup button.primary {{
  background: var(--cyan);
  color: #000;
}}
.popup button:active {{
  opacity: 0.8;
}}
.popup button:disabled {{
  opacity: 0.5;
  cursor: not-allowed;
}}
/* Config popup */
.config-row {{
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: clamp(8px, 1.5vw, 10px) 0;
  border-bottom: 1px solid var(--bg3);
}}
.config-row:last-child {{ border-bottom: none; }}
.config-row label {{
  margin: 0;
  flex: 1;
  font-size: clamp(0.8rem, 1.5vw, 0.9rem);
}}
.config-row .config-control {{
  display: flex;
  align-items: center;
  gap: 6px;
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
  display: flex;
  align-items: center;
  justify-content: center;
  transition: background 0.2s;
  -webkit-appearance: none;
  appearance: none;
}}
.config-row button:active {{ background: var(--cyan); color: #000; }}
.config-row .val {{
  min-width: 32px;
  text-align: center;
  font-weight: 600;
  font-size: clamp(0.9rem, 2vw, 1rem);
}}
/* Empty state */
.empty {{
  flex: 1;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  color: var(--fg2);
  gap: clamp(6px, 1.5vw, 12px);
  padding: clamp(30px, 5vw, 50px) clamp(16px, 3vw, 24px);
  text-align: center;
}}
.empty .icon {{ font-size: clamp(2rem, 10vw, 4rem); opacity: 0.3; }}
.empty > div:nth-child(2) {{ font-size: clamp(0.9rem, 2vw, 1rem); color: var(--fg); font-weight: 500; }}
.empty > div:nth-child(3) {{ font-size: clamp(0.75rem, 1.5vw, 0.85rem); }}
/* Disconnected badge */
.conn-badge {{
  position: fixed;
  top: 50%;
  left: 50%;
  transform: translate(-50%, -50%);
  background: var(--red);
  color: #fff;
  padding: clamp(12px, 2vw, 16px) clamp(20px, 3vw, 28px);
  border-radius: var(--radius);
  font-weight: 600;
  font-size: clamp(0.85rem, 2vw, 1rem);
  z-index: 200;
  display: none;
  box-shadow: 0 4px 12px rgba(0,0,0,0.3);
  animation: slideIn 0.3s ease;
}}
@keyframes slideIn {{
  from {{ transform: translate(-50%, -60%); opacity: 0; }}
  to {{ transform: translate(-50%, -50%); opacity: 1; }}
}}
.conn-badge.show {{ display: block; }}
/* Loading spinner */
.spinner {{
  display: inline-block;
  width: 1rem;
  height: 1rem;
  border: 2px solid rgba(255,255,255,0.3);
  border-top-color: #fff;
  border-radius: 50%;
  animation: spin 1s linear infinite;
}}
@keyframes spin {{
  to {{ transform: rotate(360deg); }}
}}
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
      <div>No files in queue</div>
      <div>Add MEGA URLs above or share from another app</div>
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
    <input type="email" id="login-email" autocomplete="email" placeholder="your@email.com">
    <label for="login-pass">Password</label>
    <input type="password" id="login-pass" autocomplete="current-password" placeholder="••••••••">
    <label for="login-mfa">MFA Code <span style="color:var(--fg2);font-weight:normal">(optional)</span></label>
    <input type="text" id="login-mfa" inputmode="numeric" autocomplete="one-time-code" placeholder="123456">
    <div class="error" id="login-error" style="display:none"></div>
    <div class="btn-row">
      <button onclick="hideLogin()">Cancel</button>
      <button class="primary" id="login-btn" onclick="doLogin()">Login</button>
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

  const API = '';
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
    let pct = 0;
    if (s.total_size > 0) {{
      pct = Math.min(Math.round(s.total_downloaded / s.total_size * 100), 100);
    }}
    document.getElementById('progress-bar').style.width = pct + '%';
    document.getElementById('progress-label').textContent = pct + '%';

    // Status dot and text
    const dot = document.getElementById('status-dot');
    const statusText = document.getElementById('status-text');
    if (!s.authenticated) {{
      dot.className = 'dot red';
      statusText.textContent = 'Not logged in';
    }} else if (s.logging_in) {{
      dot.className = 'dot yellow';
      statusText.textContent = 'Logging in...';
    }} else if (s.paused) {{
      dot.className = 'dot yellow';
      statusText.textContent = 'Paused';
    }} else if (s.current_speed > 0) {{
      dot.className = 'dot green';
      statusText.textContent = formatBytes(s.current_speed) + '/s';
    }} else {{
      dot.className = 'dot gray';
      statusText.textContent = s.files_total > 0 ? 'Idle' : 'Ready';
    }}

    // Update login button text
    document.getElementById('btn-login').textContent = s.authenticated ? 'Account' : 'Login';

    // File list
    renderFiles(s);

    // Login error display (update the popup)
    const loginError = document.getElementById('login-error');
    if (s.login_error) {{
      loginError.textContent = s.login_error;
      loginError.style.display = 'block';
    }} else {{
      loginError.style.display = 'none';
    }}
  }}

  function renderFiles(s) {{
    const list = document.getElementById('file-list');
    const empty = document.getElementById('empty-state');

    if (s.files.length === 0) {{
      if (!list.contains(empty)) {{
        list.innerHTML = '';
        list.appendChild(empty);
      }}
      return;
    }}

    if (list.contains(empty)) list.removeChild(empty);

    let prevEl = null;
    for (const f of s.files) {{
      let el = list.querySelector('[data-name="' + f.name.replace(/"/g, '&quot;') + '"]');
      if (!el) {{
        el = createFileItem(f);
      }} else {{
        updateFileItem(el, f);
      }}

      if (prevEl && prevEl.nextElementSibling !== el) {{
        prevEl.after(el);
      }} else if (!prevEl && list.firstElementChild !== el) {{
        list.insertBefore(el, list.firstElementChild);
      }}
      prevEl = el;
    }}

    // Remove deleted files
    const fileNames = new Set(s.files.map(f => f.name));
    for (const el of list.querySelectorAll('[data-name]')) {{
      if (!fileNames.has(el.dataset.name)) {{
        el.remove();
      }}
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
      detail = 'queued  •  ' + formatBytes(f.size);
    }} else if (f.status === 'complete') {{
      detail = formatBytes(f.size) + '  •  done';
    }} else {{
      detail = f.error ? f.error : 'error';
    }}

    let actions = '';
    if (f.status === 'error') {{
      actions += '<button class="retry" onclick="retryFile(\'' + escHtml(f.name) + '\')">Retry</button>';
    }}
    if (f.status !== 'complete') {{
      actions += '<button class="delete" onclick="deleteFile(\'' + escHtml(f.name) + '\')">\u2717</button>';
    }}

    if (f.status === 'error') el.classList.add('error');
    else el.classList.remove('error');

    el.innerHTML =
      '<div class="file-progress-bg" style="width:' + bgWidth + '"></div>' +
      '<span class="file-icon" style="color:' + color + '">' + icon + '</span>' +
      '<div class="file-info">' +
        '<div class="file-name">' + escHtml(f.name) + '</div>' +
        '<div class="file-detail">' + escHtml(detail) + '</div>' +
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
    const email = document.getElementById('login-email').value.trim();
    const password = document.getElementById('login-pass').value;
    const mfa = document.getElementById('login-mfa').value.trim();
    if (!email || !password) {{
      document.getElementById('login-error').textContent = 'Email and password are required';
      document.getElementById('login-error').style.display = 'block';
      return;
    }}
    document.getElementById('login-error').style.display = 'none';
    const btn = document.getElementById('login-btn');
    const oldText = btn.textContent;
    btn.textContent = '';
    btn.innerHTML = '<span class="spinner"></span>';
    btn.disabled = true;
    post('/api/login', {{email: email, password: password, mfa: mfa}}).finally(() => {{
      btn.textContent = oldText;
      btn.disabled = false;
      hideLogin();
    }});
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
        '<button onclick="cfgDec(\'' + key + '\')">−</button>' +
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
      input.focus();
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

/// Returns the PWA manifest JSON with reverse proxy support.
pub fn manifest_json(host: &str, _port: u16) -> String {
    let start_url = if host != "127.0.0.1" && host != "0.0.0.0" && !host.is_empty() {
        // When using a custom public host, the app is likely behind a reverse proxy
        // serving at a root path, so use "/" relative to that
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
}}"##
    )
}

/// Returns the service worker JavaScript for PWA offline support and share target.
pub fn service_worker_js() -> &'static str {
    r##"// octo-dl Service Worker
const CACHE_NAME = 'octo-dl-v2';
const PRECACHE = ['/', '/manifest.json', '/icon-192.svg', '/icon-512.svg'];

self.addEventListener('install', function(event) {
  event.waitUntil(
    caches.open(CACHE_NAME).then(function(cache) {
      return cache.addAll(PRECACHE).catch(function() {
        // Graceful failure if some assets aren't available on install
        return cache.addAll(['/', '/manifest.json']);
      });
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

  // Handle share target — forward to the app
  if (url.pathname === '/share') {
    event.respondWith(
      fetch(event.request).catch(function() {
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

  // API and SSE requests: network-first, always skip cache
  if (url.pathname.startsWith('/api/')) {
    event.respondWith(
      fetch(event.request).catch(function() {
        return new Response(
          JSON.stringify({ error: 'Offline' }),
          { status: 503, headers: { 'Content-Type': 'application/json' } }
        );
      })
    );
    return;
  }

  // Static assets: cache-first with network update
  event.respondWith(
    caches.match(event.request).then(function(cached) {
      if (cached) {
        // Update cache in background
        fetch(event.request).then(function(response) {
          if (response && response.status === 200) {
            caches.open(CACHE_NAME).then(function(cache) {
              cache.put(event.request, response);
            });
          }
        }).catch(function() {});
        return cached;
      }
      return fetch(event.request).then(function(response) {
        if (response && response.status === 200) {
          var clone = response.clone();
          caches.open(CACHE_NAME).then(function(cache) {
            cache.put(event.request, clone);
          });
        }
        return response;
      }).catch(function() {
        // Fallback for missing resources
        return caches.match('/').then(function(index) {
          return index || new Response('Offline', { status: 503 });
        });
      });
    })
  );
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
