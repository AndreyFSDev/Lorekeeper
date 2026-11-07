// Live check of the checkpoint showcase through the app IPC (CDP WebView2):
// prints the cards visible to the "Images" tab and, separately,
// how many of them are files not meant for image generation (audio/video).
// Run: node tools/lk-image-models.mjs [--port 9222]
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
const NOT_IMAGE = /audio|ace_step|ace-step|diffrhythm|wan2|wan_2|hunyuan/i;

const list = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
const page = list.find((t) => t.type === 'page' && t.url.startsWith('http://tauri.localhost'));
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

await send('Runtime.enable', {});
const res = await send('Runtime.evaluate', {
  expression: `window.__TAURI_INTERNALS__.invoke('get_image_models')`,
  awaitPromise: true,
  returnByValue: true,
});
if (res.result?.exceptionDetails) throw new Error(JSON.stringify(res.result.exceptionDetails));
const models = res.result.result.value;

for (const m of models) {
  const mark = m.is_installed ? '[x]' : '[ ]';
  console.log(`${mark} ${m.is_active ? '* ' : '  '}${m.filename}  (${m.size})`);
}
console.log(`всего карточек: ${models.length}`);
console.log(`не про картинки: ${models.filter((m) => NOT_IMAGE.test(m.filename)).length}`);
const active = models.find((m) => m.is_active);
console.log(`активная: ${active ? active.filename : '—'}`);
socket.close();
