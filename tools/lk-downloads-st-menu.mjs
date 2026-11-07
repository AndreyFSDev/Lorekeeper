// End-to-end check of the models menu inside a live SillyTavern: a download starts from
// the app, the window switches to ST, and the panel must show its progress and clear
// «идёт загрузка» after cancellation. It checks both the panel fix (polling on open)
// and the gateway watching the queue.
// Run the app:
//   $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS='--remote-debugging-port=9223'; .\Cloufam.exe
// Run the probe: node tools/lk-downloads-st-menu.mjs <tag> [--port 9223] [--st http://127.0.0.1:8000]
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
const ST = arg('st', 'http://127.0.0.1:8000');

async function attach(match) {
  const targets = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
  const page = targets.find(
    (t) =>
      t.type === 'page' &&
      (match === 'main'
        ? t.url.startsWith('http://tauri.localhost') && !t.url.includes('#')
        : t.url.includes(match)),
  );
  if (!page) return null;
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
  return { page, socket, evalJs };
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const problems = [];

const main = await attach('main');
if (!main) throw new Error('главное окно не найдено');
const settings = await attach('#settings');
console.log(
  `главное окно: ${main.page.url}${settings ? `, окно настроек: ${settings.page.url}` : ''}`,
);

console.log(`--- запускаю ${MODEL} и ухожу в SillyTavern ---`);
await main.evalJs(
  `window.__TAURI_INTERNALS__.invoke('download_model', { model: ${JSON.stringify(MODEL)} }).catch(() => {}); true`,
);
await sleep(5000);
await main.evalJs(`window.__TAURI_INTERNALS__.invoke('mark_st_opened')`);
await main.evalJs(`window.location.href = ${JSON.stringify(`${ST}/`)}; true`);

let st = null;
for (let i = 0; i < 20 && !st; i++) {
  await sleep(2000);
  st = await attach(ST);
}
if (!st) throw new Error('страница SillyTavern не открылась');
console.log(`страница ST: ${st.page.url}`);

// The panel is loaded by the extension after ST starts: wait for its open function.
let ready = false;
for (let i = 0; i < 20 && !ready; i++) {
  await sleep(1500);
  ready = await st.evalJs(`typeof window.__lkOpenModels === 'function'`);
}
console.log(`панель моделей готова: ${ready}`);
if (!ready) throw new Error('панель моделей в ST не загрузилась');

await st.evalJs(`window.__lkOpenModels(); true`);
await sleep(4000);

const readPanel = () =>
  st.evalJs(`(() => {
    const progress = document.querySelector('#lk-models-list .lk-model-progress');
    return {
      open: !document.getElementById('lk-models-overlay')?.classList.contains('lk-hidden'),
      tab: document.querySelector('#lk-models-tabs button.lk-active')?.textContent?.trim() ?? null,
      progress: progress ? progress.textContent.replace(/\\s+/g, ' ').trim() : null,
    };
  })()`);

// Progress is drawn in the downloading model's card, so we look at every panel
// tab: the model may not be a chat one but a vision or memory one.
let panel = await readPanel();
console.log(`панель открыта: ${panel.open}, вкладка: ${panel.tab}`);
for (const tab of ['chat', 'embed', 'vision']) {
  await st.evalJs(`(() => {
    const button = document.querySelector('#lk-models-tabs button[data-tab="${tab}"]');
    if (button) button.click();
    return true;
  })()`);
  await sleep(1500);
  panel = await readPanel();
  console.log(`  вкладка ${tab}: прогресс ${panel.progress ? `«${panel.progress}»` : '—'}`);
  if (panel.progress) break;
}
if (!panel.progress) problems.push('панель не показывает идущую загрузку');

console.log('--- отмена из настроек приложения ---');
// The settings window opens from ST the same way as the menu item: through the gateway.
await fetch(`http://127.0.0.1:11435/api/cloufam/open-settings`, {
  method: 'POST',
  headers: { 'Content-Type': 'application/json' },
  body: JSON.stringify({}),
}).catch(() => {});
let cancelWindow = settings;
for (let i = 0; i < 10 && !cancelWindow; i++) {
  await sleep(1500);
  cancelWindow = await attach('#settings');
}
if (!cancelWindow) {
  problems.push('окно настроек не открылось — отменять нечем');
} else {
  await cancelWindow.evalJs(`window.__TAURI_INTERNALS__.invoke('downloads_cancel')`);
  await sleep(6000);
  panel = await readPanel();
  console.log(`после отмены: прогресс ${panel.progress ? `«${panel.progress}»` : '—'}`);
  if (panel.progress) problems.push('панель осталась с «идёт загрузка» после отмены');
  cancelWindow.socket.close();
}

console.log(
  problems.length
    ? `ПРОБЛЕМЫ: ${problems.join('; ')}`
    : 'итог: меню моделей в ST видит загрузку из приложения и снимает её после отмены',
);
main.socket.close();
if (settings) settings.socket.close();
st.socket.close();
await sleep(200);
process.exit(problems.length ? 1 : 0);

