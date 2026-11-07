// Live check of model downloads over the app IPC (CDP WebView2):
// installs a progress-event listener, calls download_model for the given
// tag and prints what actually arrived. Starting the app:
//   $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS='--remote-debugging-port=9222'; .\Cloufam.exe
// Running the probe: node tools/lk-download-check.mjs <tag> [--port 9222]
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
const MODEL = process.argv[2];
if (!MODEL) throw new Error('укажите тег модели: node tools/lk-download-check.mjs <тег>');
const PORT = Number(arg('port', 9222));

const list = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
const page = list.find((t) => t.type === 'page' && t.url.startsWith('http://tauri.localhost'));
if (!page) throw new Error('страница приложения не найдена');
console.log(`страница: ${page.title} ${page.url}`);

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

async function evalJs(expression, awaitPromise = true) {
  const res = await send('Runtime.evaluate', { expression, awaitPromise, returnByValue: true });
  const details = res.result?.exceptionDetails;
  if (details) throw new Error(details.exception?.description || JSON.stringify(details));
  return res.result?.result?.value;
}

await send('Runtime.enable', {});

// The listener goes on the app itself: import('@tauri-apps/api/event') is not
// available in the bundle, so we subscribe through the internal invoke — same as the ST event feed.
await evalJs(`(async () => {
  window.__lkProbe = [];
  const internals = window.__TAURI_INTERNALS__;
  const handlerId = internals.transformCallback((payload) => { window.__lkProbe.push(payload); });
  await internals.invoke('plugin:event|listen', { event: 'model-download-progress', target: { kind: 'Any' }, handler: handlerId });
  return true;
})()`);

const installed = await evalJs(`window.__TAURI_INTERNALS__.invoke('installed_models')`);
console.log(`установлено моделей: ${installed.length}`);
console.log(
  `тег в списке установленных: ${installed.some((m) => m.toLowerCase().includes(MODEL.toLowerCase()))}`,
);

const started = Date.now();
console.log(`--- вызываю download_model ${MODEL} ---`);
const result = await evalJs(
  `window.__TAURI_INTERNALS__.invoke('download_model', { model: ${JSON.stringify(MODEL)} })
     .then((v) => ({ ok: true, value: v ?? null }))
     .catch((e) => ({ ok: false, error: String(e) }))`,
);
console.log(`результат IPC: ${JSON.stringify(result)}`);
console.log(`время: ${((Date.now() - started) / 1000).toFixed(1)} с`);

const events = await evalJs(`window.__lkProbe`);
console.log(`событий прогресса: ${events.length}`);
const first = events.slice(0, 3).map((e) => `${e.status} ${e.completed}/${e.total}`);
const last = events.slice(-3).map((e) => `${e.status} ${e.completed}/${e.total}`);
console.log(`первые: ${first.join(' | ')}`);
console.log(`последние: ${last.join(' | ')}`);

const after = await evalJs(`window.__TAURI_INTERNALS__.invoke('installed_models')`);
console.log(
  `после: моделей ${after.length}, тег установлен: ${after.some((m) => m.toLowerCase().includes(MODEL.toLowerCase()))}`,
);
socket.close();

