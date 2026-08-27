import { spawn, type ChildProcess } from 'node:child_process';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { type Page } from '@playwright/test';

export const WORKSPACE = path.resolve(__dirname, '..', '..');
export const HOPD = path.join(WORKSPACE, 'target', 'debug', 'hopd');

export type Hopd = {
  http: number;
  ws: number;
  dataDir: string;
  proc: ChildProcess;
  url: string;
};

function waitFor(proc: ChildProcess, needle: string, timeoutMs: number): Promise<void> {
  return new Promise((resolve, reject) => {
    let buf = '';
    const timer = setTimeout(() => {
      reject(new Error(`hopd did not print ${JSON.stringify(needle)} in ${timeoutMs}ms\n${buf}`));
    }, timeoutMs);
    const onData = (chunk: Buffer) => {
      buf += chunk.toString();
      if (buf.includes(needle)) {
        clearTimeout(timer);
        proc.stdout?.off('data', onData);
        proc.stderr?.off('data', onData);
        resolve();
      }
    };
    proc.stdout?.on('data', onData);
    proc.stderr?.on('data', onData);
  });
}

export async function startHopd(
  hopRel: string,
  http: number,
  ws: number,
  dataDir?: string,
): Promise<Hopd> {
  if (!fs.existsSync(HOPD)) {
    throw new Error(`hopd binary missing at ${HOPD}; build it first`);
  }
  const dir = dataDir ?? fs.mkdtempSync(path.join(os.tmpdir(), 'hop-e2e-'));
  const hopFile = path.join(WORKSPACE, hopRel);
  const proc = spawn(
    HOPD,
    [hopFile, String(http), String(ws), '--data', dir],
    {
      cwd: WORKSPACE,
      stdio: ['ignore', 'pipe', 'pipe'],
    },
  );
  proc.stdout?.setEncoding('utf8');
  proc.stderr?.setEncoding('utf8');
  await waitFor(proc, 'serving http', 20_000);
  return { http, ws, dataDir: dir, proc, url: `http://127.0.0.1:${http}` };
}

export async function stopHopd(h: Hopd): Promise<void> {
  h.proc.kill('SIGTERM');
  await new Promise((r) => setTimeout(r, 200));
  try {
    h.proc.kill('SIGKILL');
  } catch {
    /* already gone */
  }
}

export async function waitConnected(page: Page): Promise<string> {
  await page.waitForFunction(
    () => (document.getElementById('status')?.textContent || '').includes('connected'),
    { timeout: 60_000 },
  );
  const text = await page.locator('#status').textContent();
  const m = /session (\S+)/.exec(text || '');
  return m?.[1] || '';
}

export async function openTab(page: Page, url: string): Promise<string> {
  await page.goto(url, { waitUntil: 'domcontentloaded' });
  return waitConnected(page);
}
