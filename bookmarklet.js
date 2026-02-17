// octo-dl Bookmarklet
//
// To use: create a new bookmark in your browser and set the URL to the
// minified version below. When clicked, it sends any selected text (or the
// current page URL if nothing is selected) to the octo-tui API server,
// which extracts MEGA URLs and queues them for download.
//
// Minified (copy this as the bookmark URL):
// javascript:void(function(){var t=window.getSelection().toString();if(!t){t=window.location.href}fetch('http://localhost:9723/api/urls',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({text:t})}).then(function(r){return r.json()}).then(function(d){alert('Sent '+d.count+' URL(s) to octo-dl')}).catch(function(e){alert('octo-dl not running: '+e)})})()
//
// Readable version:
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
