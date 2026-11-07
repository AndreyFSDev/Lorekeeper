// Live UI check for downloads: opens the settings window, the
// "Recommended settings" tab and reads the queue rows (progress, pause, statuses),
// taking a screenshot of the block. Optionally it starts and cancels a download itself.
// Start the app:
//   $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS='--remote-debugging-port=9223'; .\Cloufam.exe
// Run the probe:
//   node tools/lk-downloads-ui.mjs [--start <tag>] [--cancel] [--out data/tmp/tune-downloads.png]

import { mkdirSync, writeFileSync } from 'node:fs';
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
const PORT = Number(arg('port', 9223));
const START = arg('start', '');
const CANCEL = process.argv.includes('--cancel');
const OUT = path.resolve(ROOT, arg('out', 'data/tmp/tune-downloads.png'));

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
  await send('Page.enable', {});
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

const main = await connect('#settings');
const launcher = main ?? (await connect('tauri.localhost/'));
if (!launcher) throw new Error('страница приложения не найдена');
console.log(`окно: ${launcher.page.title} ${launcher.page.url}`);

if (START) {
  const image = START.endsWith('.safetensors');
  const command = image ? 'download_image_model' : 'download_model';
  const args = image ? { filename: START } : { model: START };
  console.log(`--- запускаю загрузку ${START} (${command}) ---`);
  await launcher.evalJs(
    `window.__TAURI_INTERNALS__.invoke(${JSON.stringify(command)}, ${JSON.stringify(args)})
       .catch((e) => { window.__lkUiError = String(e); }); true`,
  );
  await launcher.evalJs(`window.__TAURI_INTERNALS__.invoke('open_settings_window')`);
  await sleep(6000);
}

const settings = launcher.page.url.includes('#settings') ? launcher : await openSettings();
console.log(
  `настройки: ${settings.page.url}${settings === launcher ? ' (главное окно: отдельное окно настроек появляется после входа в SillyTavern)' : ''}`,
);

if (process.argv.includes('--reload')) {
  // Page reload: the queue state must come from the backend,
  // not from window memory — this is how "closed settings and returned" is checked.
  console.log('--- перезагружаю страницу настроек ---');
  await settings.send('Page.reload', { ignoreCache: false });
  await sleep(7000);
}

async function openSettings() {
  let window = await connect('#settings');
  if (window) return window;
  await launcher.evalJs(`window.__TAURI_INTERNALS__.invoke('open_settings_window')`);
  for (let i = 0; i < 6; i++) {
    await sleep(1000);
    window = await connect('#settings');
    if (window) return window;
  }
  // Before the first SillyTavern login there is no separate window: the main window
  // shows the settings itself.
  return launcher;
}

// The "Recommended settings" tab — the one that hosts the "Optional" block.
await settings.evalJs(`(() => {
  const button = [...document.querySelectorAll('button')].find((b) => b.textContent.includes('Рекомендуемые'));
  if (button) button.click();
  return Boolean(button);
})()`);
await sleep(2500);

const report = await settings.evalJs(`(() => {
  const pulse = document.querySelector('.download-pill');
  const rows = [...document.querySelectorAll('.tune-download-row')].map((row) => ({
    title: row.querySelector('.tdr-title')?.textContent?.trim() ?? '',
    status: row.querySelector('.tdr-status')?.textContent?.trim() ?? '',
    bar: row.querySelector('.progress-bar-fill')?.getAttribute('style') ?? '',
    details: row.querySelector('.p-details')?.textContent?.trim() ?? '',
  }));
  const actions = [...document.querySelectorAll('.tune-optional .action-row button')].map((b) => b.textContent.trim());
  return {
    headerPill: pulse ? pulse.textContent.replace(/\\s+/g, ' ').trim() : null,
    note: document.querySelector('.tune-optional .hint')?.textContent?.replace(/\\s+/g, ' ').trim() ?? null,
    rows,
    actions,
  };
})()`);

console.log(`плашка в шапке: ${report.headerPill ?? '—'}`);
console.log(`подпись блока: ${report.note ?? '—'}`);
console.log(`кнопки блока: ${report.actions.join(' | ') || '—'}`);
for (const row of report.rows) {
  console.log(
    `строка: ${row.title} → ${row.status} ${row.bar}${row.details ? ` (${row.details})` : ''}`,
  );
}

// Scroll the block into view: the settings page scrolls inside a container,
// so element coordinates outside the viewport produce an empty screenshot.
await settings.evalJs(`(() => {
  const el = document.querySelector('.tune-optional');
  if (el) el.scrollIntoView({ block: 'center' });
  return Boolean(el);
})()`);
await sleep(500);

const shot = await settings.send('Page.captureScreenshot', {
  format: 'png',
  clip: await settings.evalJs(`(() => {
    const el = document.querySelector('.tune-optional');
    if (!el) return undefined;
    const r = el.getBoundingClientRect();
    return { x: Math.max(0, r.x), y: Math.max(0, r.y), width: r.width, height: r.height, scale: 1 };
  })()`),
});
if (shot.result?.data) {
  mkdirSync(path.dirname(OUT), { recursive: true });
  writeFileSync(OUT, Buffer.from(shot.result.data, 'base64'));
  console.log(`скриншот: ${OUT}`);
}

if (CANCEL) {
  await launcher.evalJs(`window.__TAURI_INTERNALS__.invoke('downloads_cancel')`);
  const failed = await launcher.evalJs(`window.__lkUiError ?? null`);
  console.log(`отмена отправлена${failed ? `, ошибка запуска: ${failed}` : ''}`);
}

launcher.socket.close();
if (settings !== launcher) settings.socket.close();
await sleep(300);
process.exit(0);

