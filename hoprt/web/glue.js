// glue.js — the browser side of the hop runtime.
//
// Runs the SAME hoprt.lua and app.lua the server runs, inside wasmoon
// (a prebuilt Lua VM compiled to WASM). Known spike caveat: the browser VM
// is Lua 5.4 while the server is Luau; hoprt.lua and hopc's output are
// written to the common subset.

import { LuaFactory } from 'https://cdn.jsdelivr.net/npm/wasmoon@1.16.0/+esm';

const status = (s) => { document.getElementById('status').textContent = s; };

const cfg = await fetch('/config.json').then((r) => r.json());
const lua = await new LuaFactory().createEngine();

const ws = new WebSocket(`ws://${location.hostname}:${cfg.wsPort}`);
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

lua.global.set('SIDE', 'browser');
lua.global.set('SESSION', session);
lua.global.set('__send', (pkt) => ws.send(JSON.stringify(pkt)));
lua.global.set('__print', (s) => console.log(s));
lua.global.set('dom', {
  set: (sel, html) => {
    const el = document.querySelector(sel);
    if (el) el.innerHTML = html;
  },
  get: (sel) => {
    const el = document.querySelector(sel);
    if (!el) return '';
    if (el.value !== undefined) return el.value;
    return el.textContent || '';
  },
  clear: (sel) => {
    const el = document.querySelector(sel);
    if (el && el.value !== undefined) el.value = '';
  },
});

const [rtSrc, huiSrc, appSrc] = await Promise.all([
  fetch('/hoprt.lua').then((r) => r.text()),
  fetch('/hui.lua').then((r) => r.text()),
  fetch('/app.lua').then((r) => r.text()),
]);
await lua.doString(rtSrc);
await lua.doString(huiSrc);
await lua.doString(appSrc);

ws.addEventListener('message', (ev) => {
  const pkt = JSON.parse(ev.data);
  if (pkt.kind === 'hello') return;
  lua.global.get('__receive')(pkt);
});
ws.addEventListener('close', () => status(`session ${session} — disconnected`));

window.hopFire = (name, arg) => lua.global.get('__fire')(name, arg);
window.__hopHandler = (id) => lua.global.get('__handler_fire')(id);

status(`session ${session} — connected`);
