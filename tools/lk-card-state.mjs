// Small helper for checks: reads the model card state on the "Chat" tab
// (install badge, buttons) — used to confirm the list refreshed after a
// background download. Run: node tools/lk-card-state.mjs <tag> [--port 9223]
import { createRequire } from 'node:module';
import path from 'node:path';
import { loadWs } from './ws-resolve.mjs';

const require = createRequire(import.meta.url);
const ROOT = path.resolve(import.meta.dirname, '..');
const ws = loadWs(ROOT);

const TAG = process.argv[2];
const portArg = process.argv.indexOf('--port');
const PORT = portArg === -1 ? 9223 : Number(process.argv[portArg + 1]);

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
const evalJs = async (expression) => {
  const res = await send('Runtime.evaluate', {
    expression,
    awaitPromise: true,
    returnByValue: true,
  });
  const details = res.result?.exceptionDetails;
  if (details) throw new Error(details.exception?.description || JSON.stringify(details));
  return res.result?.result?.value;
};
await send('Runtime.enable', {});

const tabs = await evalJs(`(() => {
  const button = [...document.querySelectorAll('button')].find((b) => b.textContent.includes('Чат') && b.className.includes('tab'));
  if (button) button.click();
  return Boolean(button);
})()`);
await new Promise((r) => setTimeout(r, 1500));

const report = await evalJs(`(() => {
  const card = [...document.querySelectorAll('.model-card')].find((el) => el.textContent.includes(${JSON.stringify(TAG)}));
  if (!card) return null;
  return {
    badges: [...card.querySelectorAll('.badge-active, .badge-installed, .badge-not-installed')].map((b) => b.textContent.trim()),
    buttons: [...card.querySelectorAll('button')].map((b) => b.textContent.trim()),
    progress: card.querySelector('.progress-bar-fill')?.getAttribute('style') ?? null,
  };
})()`);
console.log(`вкладка «Чат» переключена: ${tabs}`);
console.log(`карточка ${TAG}: ${JSON.stringify(report)}`);
socket.close();
await new Promise((r) => setTimeout(r, 200));
process.exit(0);
