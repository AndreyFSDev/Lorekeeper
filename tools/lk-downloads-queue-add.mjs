// Live check: a task that lands in a busy queue is visible to the window immediately.
// Previously, while paused (no progress ticks), an added item did not appear until
// the queue moved — the window showed the stale queue contents.
// Starting the app:
//   $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS='--remote-debugging-port=9223'; .\Cloufam.exe
// Running the probe:
//   node tools/lk-downloads-queue-add.mjs <downloading> <adding> [--port 9223]
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
const [RUNNING, ADDED] = process.argv.slice(2).filter((a) => !a.startsWith('--'));
if (!RUNNING || !ADDED) throw new Error('нужны два тега: <качается> <добавляется>');
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
const call = (command) => evalJs(`window.__TAURI_INTERNALS__.invoke(${JSON.stringify(command)})`);
const state = () => evalJs(`window.__TAURI_INTERNALS__.invoke('downloads_state')`);
const brief = (snapshot) =>
  snapshot.tasks.map((t) => `${t.id} [${t.status}]`).join(' | ') || 'пусто';

console.log(`--- старт ${RUNNING} ---`);
await evalJs(
  `window.__TAURI_INTERNALS__.invoke('download_model', { model: ${JSON.stringify(RUNNING)} }).catch(() => {}); true`,
);
await sleep(7000);
console.log(`идёт: ${brief(await state())}`);

console.log('--- пауза ---');
await call('downloads_pause');
await sleep(2000);
console.log(`на паузе: ${brief(await state())}`);

// Listen for events: an item that lands in a busy queue must arrive in a snapshot.
await evalJs(`(() => {
  window.__lkSnapshots = [];
  const internals = window.__TAURI_INTERNALS__;
  const handlerId = internals.transformCallback((message) => {
    // The event callback arrives both in the wrapper {event, id, payload} and as
    // the snapshot itself — depending on the IPC layer; handle both shapes.
    const payload = message && message.payload ? message.payload : message;
    const tasks = (payload && payload.tasks) || [];
    window.__lkSnapshots.push(tasks.map((t) => t.id + ' ' + t.status).join(' | '));
  });
  return internals.invoke('plugin:event|listen', { event: 'downloads-changed', target: { kind: 'Any' }, handler: handlerId });
})()`);

// Listener self-check: pausing again always sends a snapshot. Without it,
// "no events" looks the same whether the subscription broke or no event occurred.
await call('downloads_pause');
await sleep(800);
const selfCheck = await evalJs(`window.__lkSnapshots.length`);
console.log(`самопроверка слушателя (повторная пауза): событий ${selfCheck}`);

console.log(`--- добавляю ${ADDED} в очередь на паузе ---`);
await evalJs(
  `window.__TAURI_INTERNALS__.invoke('download_model', { model: ${JSON.stringify(ADDED)} }).catch(() => {}); true`,
);
await sleep(1500);
const snapshot = await state();
const added = snapshot.tasks.find((t) => t.id === ADDED);
const events = await evalJs(`window.__lkSnapshots`);
const seenInEvent = events.some((line) => line.includes(ADDED));
console.log(`в снимке: ${brief(snapshot)}`);
console.log(
  `событий после паузы: ${events.length}, последнее: ${events[events.length - 1] ?? '—'}`,
);

const problems = [];
if (selfCheck === 0) problems.push('подписка на события не работает — проба ничего не доказывает');
if (!added) problems.push('добавленный пункт не появился в снимке');
if (!seenInEvent) problems.push('о добавленном пункте окну не сообщили событием');

console.log('--- отмена ---');
await call('downloads_cancel');
await sleep(3000);
console.log(`после отмены: ${brief(await state())}`);
if ((await state()).tasks.some((t) => t.status === 'downloading' || t.status === 'queued')) {
  problems.push('после отмены остались активные пункты');
}
console.log(
  problems.length
    ? `ПРОБЛЕМЫ: ${problems.join('; ')}`
    : 'итог: пункт, вставший в занятую очередь, виден окну сразу',
);

socket.close();
await sleep(200);
process.exit(problems.length ? 1 : 0);

