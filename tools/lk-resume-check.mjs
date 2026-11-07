// Live check of resuming a ComfyUI install through the app's IPC (CDP WebView2):
// starts a download, interrupts it and prints the state of the partial archive —
// this verifies that after cancel/restart the download resumes rather than
// starting from zero. Starting the app:
//   $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS='--remote-debugging-port=9222'; .\Cloufam.exe
// Running the probe:
//   node tools/lk-resume-check.mjs status [--port 9222]
//   node tools/lk-resume-check.mjs start --cancel-after 20 [--port 9222]

import fs from 'node:fs';
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
const MODE = process.argv[2];
const PORT = Number(arg('port', 9222));
const CANCEL_AFTER = Number(arg('cancel-after', 0));
const INSTALL = process.env.LOREKEEPER_INSTALL ?? 'Z:\\AImodels\\Cloufam';
const PARTIAL = path.join(INSTALL, 'comfyui_portable.7z');

const partialInfo = (label) => {
  const size = fs.existsSync(PARTIAL) ? fs.statSync(PARTIAL).size : 0;
  const meta = `${PARTIAL}.total`;
  const total = fs.existsSync(meta) ? Number(fs.readFileSync(meta, 'utf8').trim()) : 0;
  console.log(
    `${label}: ${(size / 1048576).toFixed(1)} МБ${total ? ` из ${(total / 1048576).toFixed(1)} МБ` : ''}`,
  );
  return size;
};

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

async function evalJs(expression) {
  const res = await send('Runtime.evaluate', {
    expression,
    awaitPromise: true,
    returnByValue: true,
  });
  const details = res.result?.exceptionDetails;
  if (details) throw new Error(details.exception?.description || JSON.stringify(details));
  return res.result?.result?.value;
}

await send('Runtime.enable', {});
console.log(`страница: ${page.title} ${page.url}`);

const status = await evalJs(`window.__TAURI_INTERNALS__.invoke('get_comfy_status')`);
console.log(
  `статус: installed=${status.installed} partial=${((status.partial_bytes ?? 0) / 1048576).toFixed(1)} МБ` +
    `/${((status.partial_total_bytes ?? 0) / 1048576).toFixed(1)} МБ`,
);
partialInfo('файл на диске');

if (MODE === 'status') {
  socket.close();
  process.exit(0);
}
if (MODE !== 'start') throw new Error('режим: status | start');

await evalJs(`(() => {
  window.__lkResumeEvents = [];
  const internals = window.__TAURI_INTERNALS__;
  const handlerId = internals.transformCallback((payload) => { window.__lkResumeEvents.push(payload); });
  return internals.invoke('plugin:event|listen', { event: 'comfy-setup-progress', target: { kind: 'Any' }, handler: handlerId });
})()`);

// No waiting for the download: the start runs in the background, watch the file grow.
await evalJs(
  `window.__TAURI_INTERNALS__.invoke('install_comfyui').catch((e) => { window.__lkResumeError = String(e); }); true`,
);

let previous = 0;
for (let i = 0; i < 60; i++) {
  await new Promise((r) => setTimeout(r, 2000));
  const size = fs.existsSync(PARTIAL) ? fs.statSync(PARTIAL).size : 0;
  const events = await evalJs(`window.__lkResumeEvents`);
  const last = events[events.length - 1];
  console.log(
    `t+${(i + 1) * 2}с: файл ${(size / 1048576).toFixed(1)} МБ, событие ${last ? JSON.stringify(last) : '—'}`,
  );
  if (CANCEL_AFTER > 0 && (i + 1) * 2 >= CANCEL_AFTER) {
    await evalJs(`window.__TAURI_INTERNALS__.invoke('cancel_download')`);
    console.log('--- отмена ---');
    break;
  }
  if (size > 0 && size === previous) break;
  previous = size;
}

await new Promise((r) => setTimeout(r, 5000));
partialInfo('после обрыва');
const after = await evalJs(`window.__TAURI_INTERNALS__.invoke('get_comfy_status')`);
console.log(
  `статус после: installed=${after.installed} partial=${((after.partial_bytes ?? 0) / 1048576).toFixed(1)} МБ`,
);
const error = await evalJs(`window.__lkResumeError ?? null`);
if (error) console.log(`ошибка IPC: ${error}`);
socket.close();

