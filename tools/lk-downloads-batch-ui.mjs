// Live check of the "Optional" block through the interface itself: click "Download
// recommended", wait for the queue, cancel it with the button in the block, and look at
// the final caption and rows — this checks starting, cancellation, and the outcome text.
// Starting the application:
//   $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS='--remote-debugging-port=9223'; .\Cloufam.exe
// Running the probe: node tools/lk-downloads-batch-ui.mjs [--port 9223]

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
const OUT = path.resolve(ROOT, arg('out', 'data/tmp/tune-batch-cancelled.png'));

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
await send('Page.enable', {});

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const clickByText = (text) =>
  evalJs(`(() => {
    const button = [...document.querySelectorAll('button')].find((b) => b.textContent.includes(${JSON.stringify(text)}));
    if (button) button.click();
    return Boolean(button);
  })()`);
const readBlock = () =>
  evalJs(`(() => {
    const block = document.querySelector('.tune-optional');
    if (!block) return null;
    return {
      rows: [...block.querySelectorAll('.tune-download-row')].map((row) => ({
        title: row.querySelector('.tdr-title')?.textContent?.trim() ?? '',
        status: row.querySelector('.tdr-status')?.textContent?.trim() ?? '',
        error: row.querySelector('.tdr-error')?.textContent?.trim() ?? '',
      })),
      buttons: [...block.querySelectorAll('.action-row button')].map((b) => b.textContent.trim()),
      notes: [...block.querySelectorAll('.hint')].map((h) => h.textContent.replace(/\\s+/g, ' ').trim()),
    };
  })()`);

console.log(`окно: ${page.url}`);
await clickByText('Рекомендуемые');
await sleep(1500);

const before = await readBlock();
if (!before) throw new Error('блока «Опционально» нет — нечего проверять');
console.log(`до запуска: ${before.rows.length} строк(и), кнопки: ${before.buttons.join(' | ')}`);

console.log('--- жму «Скачать рекомендуемые» ---');
const pressed = await clickByText('Скачать рекомендуемые');
if (!pressed) throw new Error('кнопка запуска не найдена');
await sleep(6000);
const running = await readBlock();
console.log(`идёт: кнопки ${running.buttons.join(' | ')}`);
for (const row of running.rows) console.log(`  ${row.title} → ${row.status}`);

console.log('--- жму «Отмена» в блоке ---');
await clickByText('Отмена');
await sleep(6000);
const after = await readBlock();
console.log(`после отмены: кнопки ${after.buttons.join(' | ')}`);
for (const row of after.rows) console.log(`  ${row.title} → ${row.status}`);
console.log(`подписи блока: ${after.notes.join(' / ')}`);

const shot = await send('Page.captureScreenshot', {
  format: 'png',
  clip: await evalJs(`(() => {
    const el = document.querySelector('.tune-optional');
    if (!el) return undefined;
    el.scrollIntoView({ block: 'center' });
    const r = el.getBoundingClientRect();
    return { x: Math.max(0, r.x), y: Math.max(0, r.y), width: r.width, height: r.height, scale: 1 };
  })()`),
});
if (shot.result?.data) {
  mkdirSync(path.dirname(OUT), { recursive: true });
  writeFileSync(OUT, Buffer.from(shot.result.data, 'base64'));
  console.log(`скриншот: ${OUT}`);
}

const problems = [];
if (!running.buttons.some((b) => b.includes('Пауза') || b.includes('Продолжить'))) {
  problems.push('во время загрузки в блоке нет кнопки паузы');
}
if (!after.rows.every((row) => row.status.includes('Отменено'))) {
  problems.push('после отмены строки не помечены отменёнными');
}
if (!after.buttons.some((b) => b.includes('Скачать рекомендуемые'))) {
  problems.push('после отмены нельзя запустить набор заново');
}
console.log(
  problems.length
    ? `ПРОБЛЕМЫ: ${problems.join('; ')}`
    : 'итог: блок запускает набор, отменяет его и показывает итог',
);

socket.close();
await sleep(200);
process.exit(problems.length ? 1 : 0);

