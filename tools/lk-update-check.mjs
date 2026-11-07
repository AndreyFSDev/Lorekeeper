// Live check of the auto-update chain through the application IPC (CDP WebView2):
// verifies that an update is available and applies it the same way the UI does.
// Launching a copy of the previous version:
//   $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS='--remote-debugging-port=9223'; .\Cloufam.exe
// Running the probe: node tools/lk-update-check.mjs [--port 9223] [--apply]
import path from 'node:path';
import { loadWs } from './ws-resolve.mjs';

const ROOT = path.resolve(import.meta.dirname, '..');
const ws = loadWs(ROOT);

const arg = (name, fallback) => {
  const i = process.argv.indexOf(`--${name}`);
  return i === -1 ? fallback : process.argv[i + 1];
};
const PORT = Number(arg('port', 9223));
const APPLY = process.argv.includes('--apply');

const list = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
// IPC exists only on application pages: the SillyTavern page (127.0.0.1:<ST port>)
// is not covered by capabilities, so invoke is rejected there.
const page = list.find((t) => t.type === 'page' && t.url.startsWith('http://tauri.localhost'));
if (!page) {
  throw new Error(
    `страница приложения (http://tauri.localhost) не найдена, есть: ${list.map((t) => t.url).join(', ')}`,
  );
}

const socket = new ws(page.webSocketDebuggerUrl);
await new Promise((resolve, reject) => {
  socket.once('open', resolve);
  socket.once('error', reject);
});
let id = 0;
const pending = new Map();
let socketClosed = false;
socket.on('message', (raw) => {
  const msg = JSON.parse(raw.toString());
  const entry = pending.get(msg.id);
  if (!entry) return;
  pending.delete(msg.id);
  entry(msg);
});
// A successful install terminates the application process: there will be no evaluate reply,
// and without this handler the stand silently exited without printing the result.
socket.on('close', () => {
  socketClosed = true;
  for (const entry of pending.values()) entry({ closed: true });
  pending.clear();
});
const send = (method, params) =>
  new Promise((resolve) => {
    if (socketClosed) return resolve({ closed: true });
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
  if (res.closed) throw new Error('соединение с приложением закрыто');
  const details = res.result?.exceptionDetails;
  if (details) throw new Error(details.exception?.description || JSON.stringify(details));
  return res.result?.result?.value;
}

await send('Runtime.enable', {});
console.log(`страница: ${page.title} ${page.url}`);

const info = await evalJs(`window.__TAURI_INTERNALS__.invoke('check_app_update')`);
console.log(`проверка обновления: ${JSON.stringify(info)}`);
if (!info?.available) {
  console.log('обновление не предлагается — применять нечего');
  socket.close();
  process.exit(0);
}
if (!APPLY) {
  socket.close();
  process.exit(0);
}

// Progress channel: the UI creates it via @tauri-apps/api, here the same object
// is built manually (the `__TAURI_TO_IPC_KEY__` method returns the callback id),
// to call the command directly.
await evalJs(`(() => {
  window.__lkUpdateEvents = [];
  const id = window.__TAURI_INTERNALS__.transformCallback((message) => { window.__lkUpdateEvents.push(message); });
  window.__lkUpdateChannel = { ['__TAURI_TO_IPC_KEY__']() { return '__CHANNEL__:' + id; } };
  return true;
})()`);

console.log('--- применяю обновление ---');
let result;
try {
  result = await evalJs(
    `window.__TAURI_INTERNALS__.invoke('apply_app_update', { onProgress: window.__lkUpdateChannel })
       .then((v) => ({ ok: true, value: v ?? null }))
       .catch((e) => ({ ok: false, error: String(e) }))`,
  );
} catch (e) {
  if (!socketClosed) throw e;
  console.log(
    'результат IPC: соединение закрыто во время установки — приложение вышло для подмены exe ' +
      '(так выглядит успех). Проверь, что оно само вернулось новой версией.',
  );
  process.exit(0);
}
console.log(`результат IPC: ${JSON.stringify(result)}`);
const events = await evalJs(`window.__lkUpdateEvents`);
console.log(`событий прогресса: ${events.length}`);
if (events.length) {
  console.log(`последнее: ${JSON.stringify(events[events.length - 1])}`);
}
socket.close();

