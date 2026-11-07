// Live "two windows" check: the main window starts a download, and a separate settings
// window must show the same queue and be able to control it (pause, cancel),
// and a model already on disk must not enter the queue.
// Run the app:
//   $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS='--remote-debugging-port=9223'; .\Cloufam.exe
// Run the probe: node tools/lk-downloads-two-windows.mjs <tag> <installed-tag> [--port 9223]
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
const [MODEL, INSTALLED] = process.argv.slice(2).filter((a) => !a.startsWith('--'));
if (!MODEL || !INSTALLED) throw new Error('нужны два тега: <качается> <уже установленный>');
const PORT = Number(arg('port', 9223));
const GATEWAY = arg('gateway', 'http://127.0.0.1:11435');

async function connect(match) {
  const targets = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
  // The windows share the same page title, so we tell them apart by address:
  // the settings window always has the `#settings` hash, the main one has none.
  const page = targets.find(
    (t) =>
      t.type === 'page' &&
      t.url.startsWith('http://tauri.localhost') &&
      (match === 'main' ? !t.url.includes('#') : t.url.includes(match)),
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

const main = await connect('main');
if (!main) throw new Error('главное окно не найдено');
const state = () => main.evalJs(`window.__TAURI_INTERNALS__.invoke('downloads_state')`);
const brief = (snapshot) =>
  snapshot.tasks.map((t) => `${t.id} [${t.status}]`).join(' | ') || 'пусто';

// An already installed model: the queue must not even put it into work.
console.log(`--- загрузка установленной модели ${INSTALLED} ---`);
const started = Date.now();
const outcome = await main.evalJs(
  `window.__TAURI_INTERNALS__.invoke('download_model', { model: ${JSON.stringify(INSTALLED)} })
     .then(() => ({ ok: true })).catch((e) => ({ ok: false, error: String(e) }))`,
);
console.log(`ответ: ${JSON.stringify(outcome)} за ${((Date.now() - started) / 1000).toFixed(1)} с`);
if (!outcome?.ok) problems.push('установленная модель не подтвердилась как готовая');
if (Date.now() - started > 20000)
  problems.push('проверка установленной модели заняла слишком долго');

console.log(`--- запускаю ${MODEL} в главном окне, открываю окно настроек ---`);
await main.evalJs(
  `window.__TAURI_INTERNALS__.invoke('download_model', { model: ${JSON.stringify(MODEL)} }).catch(() => {}); true`,
);
await sleep(5000);
await main.evalJs(`window.__TAURI_INTERNALS__.invoke('mark_st_opened')`);
await fetch(`${GATEWAY}/api/cloufam/open-settings`, {
  method: 'POST',
  headers: { 'Content-Type': 'application/json' },
  body: JSON.stringify({}),
}).catch(() => {});

let settings = null;
for (let i = 0; i < 12 && !settings; i++) {
  await sleep(1000);
  settings = await connect('#settings');
}
if (!settings) {
  problems.push('окно настроек не открылось');
} else {
  await sleep(2000);
  const dom = await settings.evalJs(`(async () => {
    const snapshot = await window.__TAURI_INTERNALS__.invoke('downloads_state');
    return {
      url: location.href,
      hasSettingsPage: Boolean(document.querySelector('.settings-page')),
      cards: document.querySelectorAll('.model-card').length,
      pill: document.querySelector('.download-pill')?.textContent?.replace(/\\s+/g, ' ').trim() ?? null,
      blockButtons: [...document.querySelectorAll('.tune-optional .action-row button')].map((b) => b.textContent.trim()),
      tasks: (snapshot.tasks ?? []).map((t) => t.id + ' [' + t.status + ']'),
      running: snapshot.running,
    };
  })()`);
  console.log(
    `второе окно ${dom.url}: страница настроек ${dom.hasSettingsPage}, карточек ${dom.cards}`,
  );
  console.log(
    `второе окно: плашка «${dom.pill ?? '—'}», кнопки блока: ${dom.blockButtons.join(' | ') || '—'}`,
  );
  console.log(
    `второе окно видит очередь: running=${dom.running} ${dom.tasks.join(' | ') || 'пусто'}`,
  );
  if (!dom.pill && !dom.blockButtons.length) problems.push('второе окно не видит идущую загрузку');

  // Did the event reach the second window: a pause from the main window must show
  // in its pill, even if at open time it did not fetch the snapshot with a command.
  await main.evalJs(`window.__TAURI_INTERNALS__.invoke('downloads_pause')`);
  await sleep(2500);
  const pillAfterPause = await settings.evalJs(
    `document.querySelector('.download-pill')?.textContent?.replace(/\\s+/g, ' ').trim() ?? null`,
  );
  console.log(`после паузы из главного окна плашка второго окна: «${pillAfterPause ?? '—'}»`);
  if (!pillAfterPause) problems.push('второе окно не получает событий очереди');
  await main.evalJs(`window.__TAURI_INTERNALS__.invoke('downloads_resume')`);
  await sleep(2000);

  console.log('--- пауза из второго окна (клик по плашке) ---');
  const paused = await settings.evalJs(`(() => {
    // Плашка в шапке — то же управление, что кнопки в блоке: клик ставит паузу.
    const pill = document.querySelector('.download-pill');
    if (pill) pill.click();
    return Boolean(pill);
  })()`);
  if (!paused) problems.push('во втором окне не нашлось плашки загрузки');
  await sleep(3000);
  const afterPause = await state();
  const task = afterPause.tasks.find((t) => t.id === MODEL);
  console.log(`состояние после паузы из второго окна: ${brief(afterPause)}`);
  if (!afterPause.paused && task?.status !== 'paused') {
    problems.push('пауза из второго окна не дошла до очереди');
  }

  console.log('--- закрытие второго окна своей кнопкой ---');
  const closed = await settings.evalJs(`(() => {
    const button = document.querySelector('.close-btn');
    if (button) button.click();
    return Boolean(button);
  })()`);
  await sleep(3000);
  const stillThere = await connect('#settings');
  console.log(`кнопка закрытия нажата: ${closed}, окно на месте: ${Boolean(stillThere)}`);
  if (!closed) problems.push('в окне настроек нет кнопки закрытия');
  if (stillThere) {
    problems.push('кнопка закрытия не закрыла окно настроек');
    stillThere.socket.close();
  }
  settings.socket.close();
}

console.log('--- отмена ---');
await main.evalJs(`window.__TAURI_INTERNALS__.invoke('downloads_cancel')`);
await sleep(4000);
console.log(`после отмены: ${brief(await state())}`);
if ((await state()).tasks.some((t) => t.status === 'downloading' || t.status === 'queued')) {
  problems.push('после отмены остались активные пункты');
}

console.log(
  problems.length
    ? `ПРОБЛЕМЫ: ${problems.join('; ')}`
    : 'итог: второе окно видит очередь и управляет ею, установленная модель не качается',
);
main.socket.close();
await sleep(200);
process.exit(problems.length ? 1 : 0);

