// Live queue check on a pair of "failing item + normal model": the item with
// a typo in the name fails immediately, and the queue must continue with the next
// model, while the failure reason stays in the snapshot.
// Starting the application:
//   $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS='--remote-debugging-port=9223'; .\Cloufam.exe
// Running the probe:
//   node tools/lk-downloads-mixed.mjs <bad-tag> <good-tag> [--port 9223]
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
const [BAD, GOOD] = process.argv.slice(2).filter((a) => !a.startsWith('--'));
if (!BAD || !GOOD) throw new Error('нужны два тега: <падает> <качается>');
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

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const state = () => evalJs(`window.__TAURI_INTERNALS__.invoke('downloads_state')`);
const start = (tag, slot) =>
  evalJs(`window.__TAURI_INTERNALS__.invoke('download_model', { model: ${JSON.stringify(tag)} })
    .then((v) => { window.${slot} = { ok: true, value: v ?? null }; })
    .catch((e) => { window.${slot} = { ok: false, error: String(e) }; }); true`);
const brief = (snapshot) =>
  snapshot.tasks
    .map((t) => `${t.id} [${t.status}]${t.error ? ` — ${t.error.slice(0, 90)}` : ''}`)
    .join(' | ') || 'пусто';

// Both runs in a row, in one tick: the second item must enter the same queue
// while the first is still failing (otherwise it would just start a new run).
console.log(`--- ${BAD} падает, ${GOOD} встаёт следом ---`);
await start(BAD, '__lkBad');
await start(GOOD, '__lkGood');

let sawFailed = false;
let goodRan = false;
for (let tick = 0; tick < 10; tick++) {
  await sleep(2000);
  const snapshot = await state();
  console.log(`t+${(tick + 1) * 2}с: running=${snapshot.running} · ${brief(snapshot)}`);
  const bad = snapshot.tasks.find((t) => t.id === BAD);
  const good = snapshot.tasks.find((t) => t.id === GOOD);
  if (bad?.status === 'failed') sawFailed = true;
  if (sawFailed && (good?.status === 'downloading' || good?.status === 'done')) {
    goodRan = true;
    break;
  }
}

const badResult = await evalJs(`window.__lkBad ?? null`);
console.log(`ответ по падающему пункту: ${JSON.stringify(badResult)}`);

console.log('--- отмена ---');
await evalJs(`window.__TAURI_INTERNALS__.invoke('downloads_cancel')`);
await sleep(4000);
const after = await state();
console.log(`после отмены: ${brief(after)}`);

const problems = [];
if (!sawFailed) problems.push('провал пункта не отразился в снимке');
if (!goodRan) problems.push('очередь не продолжилась после провала пункта');
if (after.tasks.some((t) => t.status === 'downloading' || t.status === 'queued')) {
  problems.push('после отмены остались активные пункты');
}
console.log(
  problems.length
    ? `ПРОБЛЕМЫ: ${problems.join('; ')}`
    : 'итог: провал одного пункта не рушит очередь, причина видна, отмена гасит остаток',
);

socket.close();
await sleep(200);
process.exit(problems.length ? 1 : 0);

