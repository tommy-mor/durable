// glue.js — a dumb pipe between the WebSocket and the wasm interpreter.
//
// All protocol knowledge lives in hop-web (and hoprt): every binary frame
// goes to vm.receive(); everything the VM sends comes back through the
// callback. Rendered hui HTML calls window.__hopHandler(id, event).
// DOM writes go through window.__hopMorph: idiomorph merges the new tree
// into the live one (focus, scroll, and the input you're typing in
// survive a re-render), with innerHTML as the fallback.

const status = document.getElementById('status');
const say = (t) => { if (status) status.textContent = t; };

// Unhandled hop errors already unwind to this tab (no try/catch needed).
// Show them in the page — a binary WS error frame is not a UI.
window.__hopError = (msg) => {
  const el = document.getElementById('hop-err');
  if (el) {
    el.hidden = false;
    el.textContent = msg;
  }
  console.error(msg);
};

try {
  say('loading hop…');

  // Idiomorph is not on the critical path. Awaiting it used to deadlock
  // with /boot.css: the stylesheet holds its HTTP/1.1 connection until
  // a ws session connects, and we used to refuse to open the socket
  // until this import finished — so both sides waited on each other.
  let Idiomorph = null;
  import('/idiomorph.esm.js')
    .then((m) => { Idiomorph = m.Idiomorph; })
    .catch((e) => console.warn('idiomorph unavailable', e));

  window.__hopMorph = (sel, html) => {
    const el = document.querySelector(sel);
    if (!el) return;
    if (Idiomorph) {
      try {
        Idiomorph.morph(el, html, { morphStyle: 'innerHTML', ignoreActiveValue: true });
        return;
      } catch (e) {
        console.warn('morph failed, falling back to innerHTML', e);
      }
    }
    el.innerHTML = html;
  };

  // Port is inlined in the HTML. Do not fetch /config.json (or anything
  // else) before opening the socket: /boot.css used to hold its HTTP/1.1
  // connection until a session connected, so a request on that socket
  // for config/idiomorph deadlocked the boot.
  const hopWeb = import('/pkg/hop_web.js');
  const srcP = fetch('/app.hop').then((r) => r.text());
  const wsPort = window.__hopWsPort;
  const wsPath = (window.__hopWsPath || '').trim();
  if (!wsPath && !wsPort) throw new Error('missing __hopWsPort');

  const inbound = [];
  const outbound = [];
  let deliver = (bytes) => { inbound.push(bytes); };

  say('connecting…');
  // hopd may be IPv4-only; `localhost` prefers ::1 and the handshake hangs
  const host = location.hostname === 'localhost' ? '127.0.0.1' : location.hostname;
  const wsUrl = wsPath
    ? `${location.protocol === 'https:' ? 'wss:' : 'ws:'}//${location.host}${wsPath.startsWith('/') ? wsPath : '/' + wsPath}`
    : `ws://${host}:${wsPort}`;
  const ws = new WebSocket(wsUrl);
  ws.binaryType = 'arraybuffer';
  ws.onerror = () => say('ws error — is hopd listening on IPv6 localhost?');
  ws.onopen = () => {
    say('connected');
    for (const b of outbound) ws.send(b);
    outbound.length = 0;
  };
  ws.onclose = () => say('disconnected — restart hopd and reload');
  ws.onmessage = (e) => deliver(new Uint8Array(e.data));

  const { default: init, BrowserVm } = await hopWeb;
  await init();
  const src = await srcP;
  const vm = new BrowserVm(src, (bytes) => {
    if (ws.readyState === WebSocket.OPEN) ws.send(bytes);
    else outbound.push(bytes);
  });
  window.__hopHandler = (id, ev) => vm.fire_handler(id, ev);
  deliver = (bytes) => {
    try {
      vm.receive(bytes);
    } catch (err) {
      say('receive failed: ' + err);
      throw err;
    }
  };
  for (const f of inbound) deliver(f);
} catch (e) {
  say('boot failed: ' + e);
  throw e;
}
