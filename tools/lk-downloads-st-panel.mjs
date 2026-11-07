// Live check of the model menu in SillyTavern: the gateway must show the state
// of the shared queue — a download started outside the menu, pause and cancel —
// otherwise the panel sticks on "downloading" and refuses new starts.
// Start the app:
//   $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS='--remote-debugging-port=9223'; .\Cloufam.exe
// Run the probe: node tools/lk-downloads-st-panel.mjs <tag> [--port 9223] [--gateway http://127.0.0.1:11435]
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
const GATEWAY = arg('gateway', 'http://127.0.0.1:11435');

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

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const call = (command, args) =>
  evalJs(
    `window.__TAURI_INTERNALS__.invoke(${JSON.stringify(command)}${args ? `, ${JSON.stringify(args)}` : ''})`,
  );
const progress = () => fetch(`${GATEWAY}/api/cloufam/models/progress`).then((r) => r.json());
const line = (p) =>
  `running=${p.running} model=${p.model || '—'} status=${p.status || '—'} ${((p.completed ?? 0) / 1048576).toFixed(0)}/${((p.total ?? 0) / 1048576).toFixed(0)} МБ${p.error ? ` ошибка=${String(p.error).slice(0, 60)}` : ''}`;

const problems = [];
console.log(`--- запуск через меню моделей в ST: POST /models/download ---`);
const started = await fetch(`${GATEWAY}/api/cloufam/models/download`, {
  method: 'POST',
  headers: { 'Content-Type': 'application/json' },
  body: JSON.stringify({ model: MODEL }),
}).then((r) => r.json());
console.log(`ответ меню: ${JSON.stringify(started)}`);

await sleep(6000);
let snapshot = await progress();
console.log(`идёт: ${line(snapshot)}`);
if (!snapshot.running) problems.push('шлюз не видит идущую загрузку');
if (snapshot.model !== MODEL) problems.push(`шлюз показывает чужую модель: ${snapshot.model}`);

// Pause is triggered from the app settings — the menu must keep showing "downloading", not "error".
console.log('--- пауза из настроек ---');
await call('downloads_pause');
await sleep(2500);
snapshot = await progress();
console.log(`на паузе: ${line(snapshot)}`);
if (!snapshot.running) problems.push('пауза выглядит для меню как конец загрузки');
if (snapshot.error) problems.push(`пауза показана ошибкой: ${snapshot.error}`);

console.log('--- отмена из настроек ---');
await call('downloads_cancel');
await sleep(4000);
snapshot = await progress();
console.log(`после отмены: ${line(snapshot)}`);
if (snapshot.running) problems.push('после отмены меню считает, что загрузка идёт');

// An item cancelled BEFORE it starts: it used to produce no events at all, and the menu
// was left with a permanent "downloading".
console.log('--- отмена пункта, который не начинался ---');
await evalJs(
  `window.__TAURI_INTERNALS__.invoke('download_model', { model: ${JSON.stringify(MODEL)} }).catch(() => {}); true`,
);
await sleep(4000);
await call('downloads_cancel');
await sleep(4000);
snapshot = await progress();
console.log(`после отмены: ${line(snapshot)}`);
if (snapshot.running) problems.push('меню залипло на «идёт» после отмены неначатого пункта');

console.log('--- новая загрузка после отмены принимается меню ---');
const again = await fetch(`${GATEWAY}/api/cloufam/models/download`, {
  method: 'POST',
  headers: { 'Content-Type': 'application/json' },
  body: JSON.stringify({ model: MODEL }),
});
console.log(`ответ меню: HTTP ${again.status} ${JSON.stringify(await again.json())}`);
if (again.status !== 200) problems.push(`меню отказало в новой загрузке (HTTP ${again.status})`);
await sleep(3000);
snapshot = await progress();
console.log(`новая загрузка: ${line(snapshot)}`);
if (!snapshot.running) problems.push('новая загрузка не видна меню');
await call('downloads_cancel');

console.log(
  problems.length
    ? `ПРОБЛЕМЫ: ${problems.join('; ')}`
    : 'итог: меню моделей в ST видит очередь, паузу и отмену правильно',
);
socket.close();
await sleep(200);
process.exit(problems.length ? 1 : 0);

