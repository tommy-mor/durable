// glue.js — a dumb pipe between the WebSocket and the wasm interpreter.
//
// All protocol knowledge lives in hop-web (and hoprt): every binary frame
// goes to vm.receive(); everything the VM sends comes back through the
// callback. Rendered hui HTML calls window.__hopHandler(id, event).
// DOM writes go through window.__hopMorph: idiomorph merges the new tree
// into the live one (focus, scroll, and the input you're typing in
// survive a re-render), with innerHTML as the fallback.
//
// The socket is not the tab. Mobile browsers, laptop sleep, and idle
// proxies (Fly, Caddy) close a quiet WebSocket the moment the tab is
// backgrounded. A new connection gets a new session hello; BrowserVm
// rebuilds and on_connect paints. Do not ask the human to reload.

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
    // A view switch swaps in a structurally different tree (vote screen →
    // fullscreen log/prompt). Morphing across those let idiomorph hoist
    // old-tree nodes into new positions — a vote-screen `play` button once
    // surfaced as the prompt view's back button, handler id and all. When
    // the incoming root's first element differs from the current one,
    // replace wholesale instead of merging.
    const rootClass = (node) => {
      const first = node && node.firstElementChild;
      return first ? first.tagName + '.' + (first.className || '') : '';
    };
    const probe = document.createElement('div');
    probe.innerHTML = html;
    if (el.firstElementChild && probe.firstElementChild &&
        rootClass(el) !== rootClass(probe)) {
      el.innerHTML = html;
      return;
    }
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
  let ws = null;
  let everOpened = false;
  let reconnectTimer = null;
  let attempt = 0;
  let gen = 0;

  // hopd may be IPv4-only; `localhost` prefers ::1 and the handshake hangs
  const host = location.hostname === 'localhost' ? '127.0.0.1' : location.hostname;
  const wsUrl = wsPath
    ? `${location.protocol === 'https:' ? 'wss:' : 'ws:'}//${location.host}${wsPath.startsWith('/') ? wsPath : '/' + wsPath}`
    : `ws://${host}:${wsPort}`;

  const sendBytes = (bytes) => {
    if (ws && ws.readyState === WebSocket.OPEN) ws.send(bytes);
    // NEVER silently discard: a click sent while the socket reconnects is
    // the human's intent. Queue (bounded) and flush on open. The old code
    // dropped these — under a flood of megabyte paints the socket reconnected
    // constantly and ate every click in the gaps.
    else if (outbound.length < 64) outbound.push(bytes);
  };

  const openSocket = () => {
    if (ws && (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING)) {
      return;
    }
    const my = ++gen;
    inbound.length = 0;
    say(everOpened ? 'reconnecting…' : 'connecting…');
    const socket = new WebSocket(wsUrl);
    ws = socket;
    socket.binaryType = 'arraybuffer';
    socket.onerror = () => {
      if (my !== gen) return;
      if (!everOpened) say('ws error — is hopd listening on IPv6 localhost?');
    };
    socket.onopen = () => {
      if (my !== gen) return;
      attempt = 0;
      say('connected');
      // flush whatever queued during the gap — clicks are intent; a stale
      // queued paint is harmless (the next paint supersedes it)
      for (const b of outbound) socket.send(b);
      everOpened = true;
      outbound.length = 0;
    };
    socket.onclose = () => {
      if (my !== gen) return;
      if (reconnectTimer) return;
      if (document.visibilityState === 'hidden') {
        say('disconnected — waiting for this tab');
        return;
      }
      attempt = Math.min(attempt + 1, 6);
      const delay = Math.min(500 * 2 ** (attempt - 1), 8000);
      say(attempt === 1 ? 'disconnected — reconnecting…' : `disconnected — reconnecting… (${attempt})`);
      reconnectTimer = setTimeout(() => {
        reconnectTimer = null;
        openSocket();
      }, delay);
    };
    socket.onmessage = (e) => {
      if (my !== gen) return;
      deliver(new Uint8Array(e.data));
    };
  };

  const wake = () => {
    if (document.visibilityState === 'hidden') return;
    if (ws && ws.readyState === WebSocket.OPEN) return;
    if (reconnectTimer) {
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
    attempt = 0;
    openSocket();
  };
  document.addEventListener('visibilitychange', wake);
  window.addEventListener('pageshow', wake);
  window.addEventListener('online', wake);

  openSocket();

  const { default: init, BrowserVm } = await hopWeb;
  await init();
  const src = await srcP;
  const vm = new BrowserVm(src, sendBytes);
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
