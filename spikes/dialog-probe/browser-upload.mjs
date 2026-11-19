// Launches a browser with a throwaway profile, clicks an <input type=file> via CDP (counts as a user
// gesture, so the native Open dialog appears), runs the probe, then closes everything.
// Usage: node browser-upload.mjs <browser.exe> <tempdir>
import { spawn, execFileSync } from 'node:child_process';
import { writeFileSync, mkdirSync, rmSync } from 'node:fs';
import { join } from 'node:path';

const [browser, tmp] = process.argv.slice(2);
const probe = 'C:\\Users\\adame\\AppData\\Local\\cargo-target\\file-manager\\debug\\dialog-probe.exe';
const port = 9300 + Math.floor(process.pid % 500);
mkdirSync(tmp, { recursive: true });
const page = join(tmp, 'upload.html');
writeFileSync(page, '<input id=f type=file style="position:absolute;left:20px;top:20px;width:300px;height:60px">');
const profile = join(tmp, 'profile');
const proc = spawn(browser, [`--user-data-dir=${profile}`, `--remote-debugging-port=${port}`, '--no-first-run',
  '--no-default-browser-check', '--disable-sync', `file:///${page.replace(/\\/g, '/')}`], { stdio: 'ignore' });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let ws;
try {
  let targets;
  for (let i = 0; i < 50 && !targets; i++) {
    await sleep(300);
    try { targets = await (await fetch(`http://127.0.0.1:${port}/json`)).json(); } catch {}
  }
  const t = targets.find((x) => x.type === 'page' && x.url.includes('upload.html'));
  ws = new WebSocket(t.webSocketDebuggerUrl);
  await new Promise((r) => (ws.onopen = r));
  let id = 0;
  const send = (method, params = {}) => ws.send(JSON.stringify({ id: ++id, method, params }));
  await sleep(800);
  for (const type of ['mousePressed', 'mouseReleased'])
    send('Input.dispatchMouseEvent', { type, x: 100, y: 40, button: 'left', clickCount: 1 });
  await sleep(3500);
  console.log(execFileSync(probe, { encoding: 'utf8' }));
} catch (e) {
  console.log('ERROR', e.message);
} finally {
  ws?.close();
  try { execFileSync('taskkill', ['/PID', String(proc.pid), '/T', '/F'], { stdio: 'ignore' }); } catch {}
  await sleep(1500);
  try { rmSync(tmp, { recursive: true, force: true }); } catch {}
}
