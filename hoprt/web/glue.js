// glue.js — the browser side of the hop runtime, ~60 lines.
//
// Runs the SAME hoprt.lua and app.lua the server runs, inside wasmoon
// (a prebuilt Lua VM compiled to WASM). Known spike caveat: the browser VM
// is Lua 5.4 while the server is Luau; hoprt.lua and hopc's output are
// written to the common subset.

import { LuaFactory } from 'https://cdn.jsdelivr.net/npm/wasmoon@1.16.0/+esm';

const status = (s) => { document.getElementById('status').textContent = s; };

const lua = await new LuaFactory().createEngine();

const ws = new WebSocket(`ws://${location.hostname}:9001`);
const session = await new Promise((resolve, reject) => {
  ws.addEventListener('error', () => reject(new Error('ws failed')));
  ws.addEventListener('message', function hello(ev) {
    const msg = JSON.parse(ev.data);
    if (msg.kind === 'hello') {
      ws.removeEventListener('message', hello);
      resolve(msg.session);
    }
  });
});

// identity + transport, then the runtime, then the app — same load order
// as every other VM in the cluster.
lua.global.set('SIDE', 'browser');
lua.global.set('SESSION', session);
lua.global.set('__send', (pkt) => ws.send(JSON.stringify(pkt)));
lua.global.set('__print', (s) => console.log(s));
lua.global.set('dom', {
  set: (sel, html) => { document.querySelector(sel).innerHTML = html; },
});

const [rtSrc, appSrc] = await Promise.all([
  fetch('/hoprt.lua').then((r) => r.text()),
  fetch('/app.lua').then((r) => r.text()),
]);
await lua.doString(rtSrc);
await lua.doString(appSrc);

ws.addEventListener('message', (ev) => {
  const pkt = JSON.parse(ev.data);
  if (pkt.kind === 'hello') return;
  lua.global.get('__receive')(pkt); // wasmoon marshals the object to a table
});
ws.addEventListener('close', () => status(`session ${session} — disconnected`));

// DOM events start flows. onclick= handlers in rendered HTML use this too.
window.hopFire = (name, arg) => lua.global.get('__fire')(name, arg);

document.getElementById('f').addEventListener('submit', (e) => {
  e.preventDefault();
  const t = document.getElementById('text');
  if (t.value.trim()) window.hopFire('add_todo', t.value.trim());
  t.value = '';
});

status(`session ${session} — connected`);
