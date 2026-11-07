// Live check of two things that broke silently:
//  1) cancellation must not cover a download started right after it;
//  2) the settings window opened from SillyTavern on a specific tab
//     (`#settings/tags`) must be a settings window, not a launcher.
// Starting the application:
//   $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS='--remote-debugging-port=9223'; .\Cloufam.exe
// Running the probe: node tools/lk-downloads-cancel-race.mjs <tag> [--port 9223]
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
const SECOND = arg('second', '');
if (!MODEL || MODEL.startsWith('--')) throw new Error('укажите тег модели для загрузки');
if (!SECOND)
  throw new Error('укажите вторую модель: --second <тег> (она стартует сразу после отмены)');
const PORT = Number(arg('port', 9223));
const GATEWAY = arg('gateway', 'http://127.0.0.1:11435');

async function connect(match) {
  const targets = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
  const page = targets.find(
    (t) => t.type === 'page' && t.url.startsWith('http://tauri.localhost') && t.url.includes(match),
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
  return { page, socket, send, evalJs };
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const problems = [];

const launcher = (await connect('tauri.localhost/')) ?? (await connect(''));
if (!launcher) throw new Error('страница приложения не найдена');
console.log(`окно: ${launcher.page.url}`);

const call = (command, args) =>
  launcher.evalJs(
    `window.__TAURI_INTERNALS__.invoke(${JSON.stringify(command)}${args ? `, ${JSON.stringify(args)}` : ''})`,
  );
const state = () => call('downloads_state');

// --- 1. cancellation and a new download immediately ---------------------------
console.log(`--- старт ${MODEL}, затем отмена и сразу запуск ${SECOND} ---`);
await launcher.evalJs(
  `window.__TAURI_INTERNALS__.invoke('download_model', { model: ${JSON.stringify(MODEL)} })
     .then(() => { window.__lkFirst = { ok: true }; })
     .catch((e) => { window.__lkFirst = { ok: false, error: String(e) }; }); true`,
);
await sleep(8000);

const before = await state();
const wasDownloading = before.tasks.some((t) => t.status === 'downloading');
console.log(`перед отменой: ${before.tasks.map((t) => `${t.id} [${t.status}]`).join(' | ')}`);
if (!wasDownloading) problems.push('загрузка не началась — проверять отмену не на чем');

await call('downloads_cancel');
// The second model starts immediately: previously cancellation covered it with its flag.
await launcher.evalJs(
  `window.__TAURI_INTERNALS__.invoke('download_model', { model: ${JSON.stringify(SECOND)} })
     .then(() => { window.__lkSecond = { ok: true }; })
     .catch((e) => { window.__lkSecond = { ok: false, error: String(e) }; }); true`,
);

let secondRan = false;
for (let tick = 0; tick < 8; tick++) {
  await sleep(2000);
  const snapshot = await state();
  const task = snapshot.tasks.find((t) => t.id === SECOND);
  console.log(
    `t+${(tick + 1) * 2}с: running=${snapshot.running} · ${snapshot.tasks.map((t) => `${t.id} [${t.status}]`).join(' | ')}`,
  );
  if (task?.status === 'downloading' && task.completed > 0) {
    secondRan = true;
    break;
  }
  if (task?.status === 'cancelled') {
    problems.push('загрузка, начатая после отмены, тоже отменена');
    break;
  }
}
if (!secondRan && !problems.length) problems.push('загрузка, начатая после отмены, не пошла');

await call('downloads_cancel');
await sleep(3000);
console.log(
  `после финальной отмены: ${(await state()).tasks.map((t) => `${t.id} [${t.status}]`).join(' | ')}`,
);
console.log(
  `ответы IPC: первая ${JSON.stringify(await launcher.evalJs('window.__lkFirst ?? null'))}, вторая ${JSON.stringify(await launcher.evalJs('window.__lkSecond ?? null'))}`,
);

// --- 2. settings window on the tab ------------------------------------------
console.log('--- окно настроек из SillyTavern на вкладке «Персонажи и теги» ---');
await call('mark_st_opened');
await sleep(500);
const opened = await fetch(`${GATEWAY}/api/cloufam/open-settings`, {
  method: 'POST',
  headers: { 'Content-Type': 'application/json' },
  body: JSON.stringify({ tab: 'tags' }),
})
  .then((r) => r.status)
  .catch((e) => `ошибка: ${e.message}`);
console.log(`запрос open-settings: ${opened}`);
await sleep(6000);

const settings = await connect('#settings');
if (!settings) {
  problems.push('окно настроек с вкладкой не открылось');
} else {
  const dom = await settings.evalJs(`(() => ({
    url: location.href,
    close: Boolean(document.querySelector('.close-btn')),
    launcherButton: [...document.querySelectorAll('button')].some((b) => b.textContent.includes('Открыть SillyTavern')),
    tab: [...document.querySelectorAll('.tab-btn, nav button')].filter((b) => b.className.includes('active')).map((b) => b.textContent.trim())[0] ?? null,
  }))()`);
  console.log(`окно: ${dom.url}`);
  console.log(
    `кнопка закрытия: ${dom.close ? 'есть' : 'нет'} · кнопка «Открыть SillyTavern»: ${dom.launcherButton ? 'есть' : 'нет'} · активная вкладка: ${dom.tab ?? '—'}`,
  );
  if (!dom.close) problems.push('окно с вкладкой нарисовано как лаунчер: нет кнопки закрытия');
  if (dom.launcherButton) problems.push('в окне с вкладкой висит кнопка «Открыть SillyTavern»');
  settings.socket.close();
}

console.log(
  problems.length
    ? `ПРОБЛЕМЫ: ${problems.join('; ')}`
    : 'итог: отмена не накрывает новую загрузку, окно с вкладкой — настоящие настройки',
);
launcher.socket.close();
await sleep(200);
process.exit(problems.length ? 1 : 0);

