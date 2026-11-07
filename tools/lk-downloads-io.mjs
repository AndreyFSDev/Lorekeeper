// Live check that "pause actually stops the transfer": we watch the ollama
// process I/O counters (network traffic is included) during the download,
// while paused and after resuming. Controlled through the app IPC (CDP).
// Start the app:
//   $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS='--remote-debugging-port=9223'; .\Cloufam.exe
// Run the probe:
//   node tools/lk-downloads-io.mjs <tag> [--port 9223]

import { execFileSync } from 'node:child_process';
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
if (!MODEL || MODEL.startsWith('--')) throw new Error('укажите тег модели');
const PORT = Number(arg('port', 9223));

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

const call = (command, args) =>
  evalJs(
    `window.__TAURI_INTERNALS__.invoke(${JSON.stringify(command)}${args ? `, ${JSON.stringify(args)}` : ''})`,
  );
const state = () => call('downloads_state');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Bytes written by the ollama process according to the performance counters. */
function ollamaBytesWritten() {
  const raw = execFileSync('powershell.exe', [
    '-NoProfile',
    '-Command',
    "(Get-CimInstance Win32_PerfRawData_PerfProc_Process | Where-Object { $_.Name -like 'ollama*' } | Measure-Object -Property IOWriteBytesPersec -Sum).Sum",
  ])
    .toString()
    .trim();
  return Number(raw);
}

/** How much the process added over the observation window, MB/s. */
async function rate(seconds) {
  const before = ollamaBytesWritten();
  await sleep(seconds * 1000);
  const after = ollamaBytesWritten();
  return (after - before) / seconds / 1048576;
}

console.log(`--- старт загрузки ${MODEL} ---`);
await evalJs(`window.__TAURI_INTERNALS__.invoke('download_model', { model: ${JSON.stringify(MODEL)} })
  .then(() => { window.__lkIoResult = { ok: true }; })
  .catch((e) => { window.__lkIoResult = { ok: false, error: String(e) }; }); true`);

await sleep(12000);
const downloading = await rate(6);
const snapshot = await state();
console.log(
  `идёт загрузка: ${downloading.toFixed(1)} МБ/с · ${JSON.stringify(snapshot.tasks.map((t) => `${t.id} ${t.status}`))}`,
);

console.log('--- пауза ---');
await call('downloads_pause');
await sleep(3000);
const paused = await rate(6);
console.log(`на паузе: ${paused.toFixed(1)} МБ/с`);

console.log('--- продолжение ---');
await call('downloads_resume');
await sleep(3000);
const resumed = await rate(6);
console.log(`после продолжения: ${resumed.toFixed(1)} МБ/с`);

console.log('--- отмена ---');
await call('downloads_cancel');
await sleep(4000);
const cancelled = await rate(6);
const after = await state();
console.log(
  `после отмены: ${cancelled.toFixed(1)} МБ/с · ${JSON.stringify(after.tasks.map((t) => `${t.id} ${t.status}`))}`,
);
const result = await evalJs(`window.__lkIoResult ?? null`);
console.log(`ответ IPC: ${JSON.stringify(result)}`);

const problems = [];
if (downloading < 1) problems.push('загрузка не создаёт трафик');
if (paused > Math.max(1, downloading * 0.1))
  problems.push(`пауза не остановила передачу (${paused.toFixed(1)} МБ/с)`);
if (resumed < 1) problems.push('продолжение не возобновило передачу');
if (cancelled > Math.max(1, downloading * 0.1))
  problems.push(`отмена не остановила передачу (${cancelled.toFixed(1)} МБ/с)`);
if (after.tasks.some((task) => task.status === 'downloading' || task.status === 'queued')) {
  problems.push('после отмены в очереди остались активные пункты');
}
console.log(
  problems.length
    ? `ПРОБЛЕМЫ: ${problems.join('; ')}`
    : 'итог: пауза и отмена останавливают передачу, продолжение возвращает её',
);

socket.close();
await sleep(200);
process.exit(problems.length ? 1 : 0);

