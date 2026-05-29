// octo-dl Bookmarklet
//
// The authoritative bookmarklet is served by octo-dl itself at /bookmarklet so
// it can embed the active API origin and API key header. This standalone file
// is only a readable example for the default local API listener on
// http://localhost:9723.
//
// Example bookmarklet source:
javascript:void(function() {
  var t = window.getSelection().toString();
  if (!t) {
    t = window.location.href;
  }
  fetch('http://localhost:9723/api/urls', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ text: t })
  })
  .then(function(r) { return r.json(); })
  .then(function(d) { alert('Sent ' + d.count + ' URL(s) to octo-dl'); })
  .catch(function(e) { alert('octo-dl not running: ' + e); });
})()
