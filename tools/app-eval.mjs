// Run JS on the application page via CDP WebView2 (for UI checks).
// Run: node tools/app-eval.mjs --port 9223 --js "location.hash = '#guide'"
//   --match <url substring> selects the page (by default the first tauri.localhost)
import path from 'node:path';
import { loadWs } from './ws-resolve.mjs';

const ROOT = path.resolve(import.meta.dirname, '..');
const ws = loadWs(ROOT);

const arg = (name, fallback) => {
  const i = process.argv.indexOf(`--${name}`);
  return i === -1 ? fallback : process.argv[i + 1];
};
const PORT = Number(arg('port', 9223));
const JS = arg('js', '');
const MATCH = arg('match', '');
if (!JS) {
  console.error('нужен --js "<выражение>"');
  process.exit(2);
}

const list = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
const page = list.find((t) => t.type === 'page' && (!MATCH || t.url.includes(MATCH)));
if (!page) throw new Error(`страница не найдена: ${list.map((t) => t.url).join(', ')}`);

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
  expression: JS,
  awaitPromise: true,
  returnByValue: true,
});
const details = res.result?.exceptionDetails;
if (details) {
  console.error(details.exception?.description || JSON.stringify(details));
  socket.close();
  process.exit(1);
}
console.log(JSON.stringify(res.result?.result?.value ?? null));
socket.close();
