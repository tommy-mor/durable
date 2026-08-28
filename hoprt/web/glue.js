// glue.js — a dumb pipe between the WebSocket and the wasm interpreter.
//
// All protocol knowledge lives in hop-web (and hoprt): every binary frame
// goes to vm.receive(); everything the VM sends comes back through the
// callback. Rendered hui HTML calls window.__hopHandler(id, event).

const status = document.getElementById('status');
const say = (t) => { if (status) status.textContent = t; };

try {
  say('loading hop…');
  const { default: init, BrowserVm } = await import('/pkg/hop_web.js');
  const [, src, config] = await Promise.all([
    init(),
    fetch('/app.hop').then(r => r.text()),
    fetch('/config.json').then(r => r.json()),
  ]);

  const ws = new WebSocket(`ws://${location.hostname}:${config.wsPort}`);
  ws.binaryType = 'arraybuffer';

  const vm = new BrowserVm(src, (bytes) => ws.send(bytes));
  window.__hopHandler = (id, ev) => vm.fire_handler(id, ev);

  ws.onopen = () => say('connected');
  ws.onclose = () => say('disconnected — restart hopd and reload');
  ws.onmessage = (e) => vm.receive(new Uint8Array(e.data));
} catch (e) {
  say('boot failed: ' + e);
  throw e;
}
