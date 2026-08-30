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

try {
  say('loading hop…');
  const { default: init, BrowserVm } = await import('/pkg/hop_web.js');
  const [, src, config, { Idiomorph }] = await Promise.all([
    init(),
    fetch('/app.hop').then(r => r.text()),
    fetch('/config.json').then(r => r.json()),
    import('/idiomorph.esm.js'),
  ]);

  window.__hopMorph = (sel, html) => {
    const el = document.querySelector(sel);
    if (!el) return;
    try {
      Idiomorph.morph(el, html, { morphStyle: 'innerHTML', ignoreActiveValue: true });
    } catch (e) {
      console.warn('morph failed, falling back to innerHTML', e);
      el.innerHTML = html;
    }
  };

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
