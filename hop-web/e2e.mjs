// e2e.mjs — headless end-to-end check of the wasm browser backend.
//
//   wasm-pack build hop-web --target web
//   cargo build -p hoprt --bin hopd
//   node hop-web/e2e.mjs
//
// This is a browser stand-in with zero dependencies: a stub DOM that
// satisfies web-sys (globalThis becomes the Window, elements live in a
// map) and a minimal raw WebSocket client (node 18 has no global
// WebSocket). Everything in between — compiling todo.hop in wasm, the
// hello, on_connect renders, hui handler ids, hops to the server and
// casts back — is the real thing.

import { readFile } from 'node:fs/promises';
import { spawn } from 'node:child_process';
import { connect } from 'node:net';
import { randomBytes } from 'node:crypto';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const HTTP = 9500, WS = 9501;

// ── stub DOM ────────────────────────────────────────────────────────────
class Window {}
class Document {}
class Element { constructor() { this.innerHTML = ''; this.textContent = ''; } }
class HTMLInputElement extends Element { constructor() { super(); this.value = ''; } }
class HTMLTextAreaElement extends HTMLInputElement {}
Object.assign(globalThis, { Window, Document, Element, HTMLInputElement, HTMLTextAreaElement });
Object.setPrototypeOf(globalThis, Window.prototype); // globalThis instanceof Window

const els = new Map([
  ['#app', new Element()],
  ['#status', new Element()],
  ['#draft', new HTMLInputElement()],
]);
globalThis.document = Object.setPrototypeOf(
  { querySelector: (sel) => els.get(sel) ?? null },
  Document.prototype,
);

// ── minimal WebSocket client (binary frames only) ───────────────────────
function wsConnect(port, onFrame) {
  return new Promise((resolve, reject) => {
    const sock = connect(port, '127.0.0.1');
    const key = randomBytes(16).toString('base64');
    let buf = Buffer.alloc(0);
    let shook = false;
    sock.on('connect', () => {
      sock.write(
        `GET / HTTP/1.1\r\nHost: 127.0.0.1:${port}\r\nUpgrade: websocket\r\n` +
        `Connection: Upgrade\r\nSec-WebSocket-Key: ${key}\r\nSec-WebSocket-Version: 13\r\n\r\n`,
      );
    });
    sock.on('error', reject);
    sock.on('data', (chunk) => {
      buf = Buffer.concat([buf, chunk]);
      if (!shook) {
        const end = buf.indexOf('\r\n\r\n');
        if (end < 0) return;
        if (!buf.subarray(0, end).toString().includes(' 101 ')) {
          return reject(new Error('handshake refused'));
        }
        buf = buf.subarray(end + 4);
        shook = true;
        resolve({
          send(bytes) {
            const payload = Buffer.from(bytes);
            const mask = randomBytes(4);
            const head =
              payload.length < 126
                ? Buffer.from([0x82, 0x80 | payload.length])
                : Buffer.concat([Buffer.from([0x82, 0x80 | 126]),
                    (() => { const b = Buffer.alloc(2); b.writeUInt16BE(payload.length); return b; })()]);
            const masked = payload.map((byte, i) => byte ^ mask[i % 4]);
            sock.write(Buffer.concat([head, mask, Buffer.from(masked)]));
          },
          close() { sock.destroy(); },
        });
      }
      // server → client frames are unmasked
      for (;;) {
        if (buf.length < 2) return;
        const op = buf[0] & 0x0f;
        let len = buf[1] & 0x7f, off = 2;
        if (len === 126) { if (buf.length < 4) return; len = buf.readUInt16BE(2); off = 4; }
        else if (len === 127) { if (buf.length < 10) return; len = Number(buf.readBigUInt64BE(2)); off = 10; }
        if (buf.length < off + len) return;
        const payload = buf.subarray(off, off + len);
        buf = buf.subarray(off + len);
        if (op === 0x2) onFrame(new Uint8Array(payload));
        if (op === 0x8) return; // close
      }
    });
  });
}

// ── helpers ─────────────────────────────────────────────────────────────
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function until(what, pred) {
  for (let i = 0; i < 100; i++) {
    if (pred()) return;
    await sleep(50);
  }
  throw new Error(`timed out waiting for: ${what}\n#app is now: ${els.get('#app').innerHTML}`);
}

// ── run ─────────────────────────────────────────────────────────────────
const data = mkdtempSync(join(tmpdir(), 'hop-e2e-'));
const hopd = spawn(join(root, 'target/debug/hopd'),
  ['hoprt/hop/todo.hop', String(HTTP), String(WS), '--data', data],
  { cwd: root, stdio: ['ignore', 'pipe', 'inherit'] });
hopd.stdout.on('data', (d) => process.stdout.write(`  [hopd] ${d}`));

for (let i = 0; ; i++) {
  try { await fetch(`http://127.0.0.1:${HTTP}/config.json`); break; }
  catch { if (i > 100) throw new Error('hopd never came up'); await sleep(100); }
}

const { default: init, BrowserVm } = await import(join(root, 'hop-web/pkg/hop_web.js'));
await init({ module_or_path: await readFile(join(root, 'hop-web/pkg/hop_web_bg.wasm')) });
const src = await (await fetch(`http://127.0.0.1:${HTTP}/app.hop`)).text();

let vm;
const ws = await wsConnect(WS, (bytes) => vm.receive(bytes));
vm = new BrowserVm(src, (bytes) => ws.send(bytes));

const app = els.get('#app');

// on_connect renders the empty board
await until('initial render', () => app.innerHTML.includes('what needs doing?'));
console.log('✓ connected; on_connect rendered the board');

// type into the draft input and click "add"
els.get('#draft').value = 'buy milk';
const addId = Number(app.innerHTML.match(/<button[^>]*__hopHandler\((\d+)\)/)[1]);
vm.fire_handler(addId);
await until('todo appears', () => app.innerHTML.includes('buy milk'));
await until('stats update', () => app.innerHTML.includes('0 done of 1'));
console.log('✓ add: hopped to the server, cast re-rendered the board');

// click the todo to toggle it
const liId = Number(app.innerHTML.match(/<li[^>]*__hopHandler\((\d+)\)/)[1]);
vm.fire_handler(liId);
await until('todo done', () =>
  app.innerHTML.includes('class="done"') && app.innerHTML.includes('1 done of 1'));
console.log('✓ toggle: marked lambda hopped, stats and class updated');

console.log('PASS — the wasm interpreter drives the full protocol');
ws.close();
hopd.kill();
process.exit(0);
