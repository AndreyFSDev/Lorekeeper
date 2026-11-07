// Live check of the download queue through the application IPC (CDP WebView2):
// download start, pause (the file on disk stops growing), resume from the break
// point and cancel. Works for an Ollama model, for an image checkpoint, and
// for the whole recommendations queue.
// Launching the application:
//   $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS='--remote-debugging-port=9223'; .\Cloufam.exe
// Running the probe:
//   node tools/lk-downloads-check.mjs <tag> [--port 9223] [--pause-after 8]
//   node tools/lk-downloads-check.mjs <file>.safetensors --image
//   node tools/lk-downloads-check.mjs --recommended

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
const RECOMMENDED = process.argv.includes('--recommended');
const IMAGE = process.argv.includes('--image');
const ID = RECOMMENDED ? '' : process.argv[2];
if (!RECOMMENDED && (!ID || ID.startsWith('--'))) {
  throw new Error(
    'укажите тег модели или имя чекпоинта: node tools/lk-downloads-check.mjs <id> [--image]',
  );
}
const PORT = Number(arg('port', 9223));
const PAUSE_AFTER = Number(arg('pause-after', 8));
const BLOBS = process.env.LOREKEEPER_BLOBS ?? 'Z:\\AImodels\\ollama\\blobs';
const CHECKPOINTS = process.env.LOREKEEPER_CHECKPOINTS ?? 'Z:\\AImodels\\comfyui\\checkpoints';

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

const call = (command, args) =>
  evalJs(
    `window.__TAURI_INTERNALS__.invoke(${JSON.stringify(command)}${args ? `, ${JSON.stringify(args)}` : ''})`,
  );
const state = () => call('downloads_state');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const mb = (bytes) => (bytes / 1048576).toFixed(1);

// Leftovers that existed before the probe: their size does not belong to this download.
// Selection by modification time, not by name: a pull may continue someone else's
// unfinished layer, and then no "new" files would appear at all.
const startedAt = Date.now();
const baseline = new Set(
  fs.existsSync(BLOBS) ? fs.readdirSync(BLOBS).filter((name) => name.includes('-partial')) : [],
);

/** The partially downloaded file on disk: it shows whether traffic is flowing right now. */
function partialBytes() {
  if (IMAGE) {
    const file = path.join(CHECKPOINTS, `${ID}.download`);
    return fs.existsSync(file) ? fs.statSync(file).size : 0;
  }
  if (!fs.existsSync(BLOBS)) return 0;
  return fs
    .readdirSync(BLOBS)
    .filter((name) => {
      if (!name.includes('-partial')) return false;
      if (!baseline.has(name)) return true;
      const stat = fs.statSync(path.join(BLOBS, name));
      return stat.mtimeMs >= startedAt;
    })
    .reduce((sum, name) => sum + fs.statSync(path.join(BLOBS, name)).size, 0);
}

/** The queue item we watch: the named one or the first one in progress. */
function target(snapshot) {
  if (ID) return snapshot.tasks.find((task) => task.id === ID) ?? null;
  return (
    snapshot.tasks.find((task) => task.status === 'downloading') ??
    snapshot.tasks.find((task) => task.status === 'paused') ??
    snapshot.tasks.find((task) => task.status === 'queued') ??
    null
  );
}

function line(task) {
  if (!task) return 'пункта нет';
  const speed = task.speed > 0 ? ` · ${(task.speed / 1048576).toFixed(1)} МБ/с` : '';
  const eta = task.eta > 0 ? ` · осталось ~${Math.round(task.eta)} с` : '';
  return `${task.id}: ${task.status} ${mb(task.completed)}/${mb(task.total)} МБ${speed}${eta} · ${task.note}`;
}

const command = RECOMMENDED
  ? 'download_recommended'
  : IMAGE
    ? 'download_image_model'
    : 'download_model';
const args = RECOMMENDED ? null : IMAGE ? { filename: ID } : { model: ID };

console.log(`команда: ${command} ${args ? JSON.stringify(args) : '(рекомендованные)'}`);
console.log(`недокачано на диске до старта: ${mb(partialBytes())} МБ`);

await evalJs(
  `window.__TAURI_INTERNALS__.invoke(${JSON.stringify(command)}${args ? `, ${JSON.stringify(args)}` : ''})
     .then((v) => { window.__lkDownloadResult = { ok: true, value: v ?? null }; })
     .catch((e) => { window.__lkDownloadResult = { ok: false, error: String(e) }; }); true`,
);

let started = false;
let isPaused = false;
let pausedAt = 0;
let resumed = false;
let resumedAt = 0;
let cancelled = false;
let watched = ID;

for (let tick = 0; tick < 30; tick++) {
  await sleep(2000);
  const snapshot = await state();
  const task = target(snapshot);
  const size = partialBytes();
  if (task && !watched) watched = task.id;
  console.log(
    `t+${(tick + 1) * 2}с: running=${snapshot.running} paused=${snapshot.paused} · ` +
      `очередь [${snapshot.tasks.map((t) => `${t.status}`).join(', ')}] · ${line(task)} · файл ${mb(size)} МБ`,
  );
  if (!task) continue;
  if (task.status === 'downloading') started = true;
  if (!started) continue;

  if (!isPaused && (tick + 1) * 2 >= PAUSE_AFTER) {
    console.log('--- пауза ---');
    await call('downloads_pause');
    isPaused = true;
    pausedAt = size;
    continue;
  }

  if (isPaused && !resumed) {
    if (size > pausedAt + 4 * 1048576) {
      throw new Error(`пауза не остановила загрузку: ${mb(pausedAt)} → ${mb(size)} МБ`);
    }
    const paused = target(await state());
    if (paused?.status !== 'paused') {
      throw new Error(`ожидался статус paused, получено ${paused?.status}`);
    }
    if (tick * 2 > PAUSE_AFTER + 2) {
      console.log('--- продолжение ---');
      await call('downloads_resume');
      resumed = true;
      resumedAt = size;
    }
    continue;
  }

  if (resumed && !cancelled && size > resumedAt + 2 * 1048576) {
    console.log('--- отмена ---');
    await call('downloads_cancel');
    cancelled = true;
    await sleep(4000);
    const after = await state();
    const cancelledTask = after.tasks.find((task) => task.id === watched);
    const stopSize = partialBytes();
    await sleep(3000);
    console.log(`после отмены: ${line(cancelledTask)} · файл ${mb(stopSize)} МБ`);
    if (cancelledTask?.status !== 'cancelled') {
      throw new Error(`ожидался статус cancelled, получено ${cancelledTask?.status}`);
    }
    if (after.tasks.some((task) => task.status === 'downloading' || task.status === 'queued')) {
      throw new Error('после отмены в очереди остались активные пункты');
    }
    if (partialBytes() > stopSize + 4 * 1048576) {
      throw new Error('после отмены загрузка продолжилась');
    }
    const result = await evalJs(`window.__lkDownloadResult ?? null`);
    const error = typeof result?.error === 'string' ? result.error.slice(0, 120) : null;
    console.log(`ответ IPC: ${JSON.stringify({ ok: result?.ok ?? null, error })}`);
    console.log(
      `итог: пауза встала на ${mb(pausedAt)} МБ (файл не рос), продолжение докачало ` +
        `до ${mb(stopSize)} МБ, отмена остановила всю очередь; недокачанное осталось на диске`,
    );
    socket.close();
    await sleep(200);
    process.exit(0);
  }
}

throw new Error('проба не дошла до конца за отведённое время');

