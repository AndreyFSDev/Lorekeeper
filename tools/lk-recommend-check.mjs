// Live check of hardware-tuned settings via the app IPC (CDP WebView2):
// prints a report (hardware, fingerprint, suggestions) and applies them with a flag.
// Run: node tools/lk-recommend-check.mjs [--port 9222] [--apply] [--key <field>]
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
const APPLY = process.argv.includes('--apply');
const KEY = arg('key', null);

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

const report = await evalJs(`window.__TAURI_INTERNALS__.invoke('recommended_settings')`);
console.log(
  `железо: ${report.hardware.name} · ${report.hardware.vram} · ${report.hardware.backend} · тир ${report.hardware.tier}`,
);
console.log(`отпечаток: ${report.hardware.fingerprint}`);
console.log(`подобрано под: ${report.tuned_for || '—'} (совпадает: ${report.matches})`);
for (const item of report.items) {
  console.log(`  • ${item.title}: ${item.current} → ${item.recommended}`);
  console.log(`      ${item.reason}`);
}
console.log(`предложений: ${report.items.length}`);

if (APPLY) {
  const keys = KEY ? [KEY] : null;
  const applied = await evalJs(
    `window.__TAURI_INTERNALS__.invoke('apply_recommended_settings', { keys: ${JSON.stringify(keys)} })
       .then((s) => ({ ok: true, s }))
       .catch((e) => ({ ok: false, error: String(e) }))`,
  );
  if (!applied.ok) {
    console.log(`применение не прошло: ${applied.error}`);
  } else {
    const s = applied.s;
    console.log(
      `после применения: chat=${s.chat_model} ctx=${s.context_size} max_tokens=${s.max_tokens} chunks=${s.memory_max_chunks} img=${s.image_model} tuned_for=${s.tuned_for}`,
    );
  }
}
socket.close();
