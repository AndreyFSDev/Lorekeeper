// App window screenshot via CDP WebView2 (for UI checks).
// Usage: node tools/app-shot.mjs --out data/tmp/shot.png [--clip .settings-header]

import { mkdirSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import path from 'node:path';
import { loadWs } from './ws-resolve.mjs';

const require = createRequire(import.meta.url);
const ROOT = path.resolve(import.meta.dirname, '..');
const ws = loadWs(ROOT);

const arg = (name, fallback) => {
  const i = process.argv.indexOf(`--${name}`);
  return i === -1 ? fallback : process.argv[i + 1];
};
const PORT = Number(arg('port', 9222));
const OUT = path.resolve(ROOT, arg('out', 'data/tmp/shot.png'));
const CLIP = arg('clip', '');
const MATCH = arg('match', '');

const list = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
const page = list.find((t) => t.type === 'page' && (!MATCH || t.url.includes(MATCH)));
if (!page) throw new Error('страница приложения не найдена');

const socket = new ws(page.webSocketDebuggerUrl);
await new Promise((resolve, reject) => {
  socket.once('open', resolve);
  socket.once('error', reject);
});
let id = 0;
const pending = new Map();
socket.on('message', (raw) => {
  const msg = JSON.parse(raw.toString());
  const entry = pending.get(msg.id);
  if (!entry) return;
  pending.delete(msg.id);
  entry(msg);
});
const send = (method, params) =>
  new Promise((resolve) => {
    const i = ++id;
    pending.set(i, resolve);
    socket.send(JSON.stringify({ id: i, method, params }));
  });

await send('Page.enable', {});
await send('Runtime.enable', {});

let clip;
if (CLIP) {
  const box = await send('Runtime.evaluate', {
    expression: `(() => { const el = document.querySelector(${JSON.stringify(CLIP)}); if (!el) return null; const r = el.getBoundingClientRect(); return { x: r.x, y: r.y, width: r.width, height: r.height, scale: 1 }; })()`,
    returnByValue: true,
  });
  clip = box.result?.result?.value ?? undefined;
  if (!clip) console.warn(`элемент ${CLIP} не найден — снимаю всё окно`);
}

const shot = await send('Page.captureScreenshot', { format: 'png', clip });
const data = shot.result?.data;
if (!data) {
  console.error('не удалось получить скриншот:', JSON.stringify(shot).slice(0, 300));
  process.exit(1);
}
mkdirSync(path.dirname(OUT), { recursive: true });
writeFileSync(OUT, Buffer.from(data, 'base64'));
console.log(`скриншот: ${OUT}${clip ? ` (область ${CLIP})` : ''}`);
socket.close();
