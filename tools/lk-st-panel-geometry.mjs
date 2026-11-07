// Diagnostics of the models menu geometry inside SillyTavern: we inspect the geometry
// of the overlay and panel and the ancestor styles (transform/filter on an ancestor breaks position:fixed).
// Run: node tools/lk-st-panel-geometry.mjs [--port 9223]
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

const targets = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
const page = targets.find((t) => t.type === 'page' && t.url.startsWith('http://127.0.0.1:8000'));
if (!page) throw new Error('страница SillyTavern не найдена');
console.log(`страница: ${page.url}`);

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

const report = await evalJs(`(() => {
  const overlay = document.getElementById('lk-models-overlay');
  const panel = document.getElementById('lk-models-panel');
  const header = panel?.querySelector('header');
  const list = document.getElementById('lk-models-list');
  const rect = (el) => {
    if (!el) return null;
    const r = el.getBoundingClientRect();
    return { top: Math.round(r.top), bottom: Math.round(r.bottom), height: Math.round(r.height) };
  };
  // Ищем предка, который создаёт containing block для position:fixed.
  const breakers = [];
  for (let el = overlay?.parentElement; el; el = el.parentElement) {
    const style = getComputedStyle(el);
    const bad = ['transform', 'filter', 'backdropFilter', 'perspective', 'contain', 'willChange']
      .filter((prop) => {
        const value = style[prop];
        return value && value !== 'none' && value !== 'auto' && value !== 'normal';
      });
    if (bad.length) {
      breakers.push({
        tag: el.tagName + (el.id ? '#' + el.id : '') + (el.className && typeof el.className === 'string' ? '.' + el.className.split(' ').slice(0, 2).join('.') : ''),
        props: bad.map((prop) => prop + '=' + style[prop]),
      });
    }
  }
  return {
    open: overlay ? !overlay.classList.contains('lk-hidden') : false,
    overlayPosition: overlay ? getComputedStyle(overlay).position : null,
    overlayRect: rect(overlay),
    panelRect: rect(panel),
    headerRect: rect(header),
    listScrollTop: list?.scrollTop ?? null,
    listScrollHeight: list?.scrollHeight ?? null,
    listClientHeight: list?.clientHeight ?? null,
    viewport: { width: innerWidth, height: innerHeight },
    scroll: { y: window.scrollY, docHeight: document.documentElement.scrollHeight },
    bodyOverflow: getComputedStyle(document.body).overflow,
    htmlOverflow: getComputedStyle(document.documentElement).overflow,
    breakers,
  };
})()`);

console.log(`панель открыта: ${report.open}`);
console.log(
  `оверлей: position=${report.overlayPosition} rect=${JSON.stringify(report.overlayRect)}`,
);
console.log(`панель: ${JSON.stringify(report.panelRect)}`);
console.log(`шапка панели: ${JSON.stringify(report.headerRect)}`);
console.log(
  `список: scrollTop=${report.listScrollTop} scrollHeight=${report.listScrollHeight} clientHeight=${report.listClientHeight}`,
);
console.log(
  `окно: ${JSON.stringify(report.viewport)} скролл страницы: ${JSON.stringify(report.scroll)}`,
);
console.log(`overflow body/html: ${report.bodyOverflow} / ${report.htmlOverflow}`);
console.log(`предки, ломающие position:fixed: ${report.breakers.length}`);
for (const breaker of report.breakers) console.log(`  ${breaker.tag}: ${breaker.props.join(', ')}`);

const shotArg = arg('shot', '');
if (shotArg) {
  const { mkdirSync, writeFileSync } = await import('node:fs');
  await send('Page.enable', {});
  const shot = await send('Page.captureScreenshot', { format: 'png' });
  if (shot.result?.data) {
    const out = path.resolve(ROOT, shotArg);
    mkdirSync(path.dirname(out), { recursive: true });
    writeFileSync(out, Buffer.from(shot.result.data, 'base64'));
    console.log(`скриншот: ${out}`);
  }
}

socket.close();
await new Promise((r) => setTimeout(r, 200));
process.exit(0);
