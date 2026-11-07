// Contrast audit of the live app window's text: the same measurement as in
// lk-glass-review.mjs (text is checked against the background from a frame where all text
// is transparent), but for an already running copy and the current screen — without an isolated
// install. This is how screens that were not in the glass revision get checked.
//
// Run (the app is started with CDP):
//   node stand-out/with-install.mjs tools/lk-contrast-check.mjs --port 9223
//   ... --match '#guide' --scroll 900   -- repeat with scrolling down
//
// Failures — text below 4.5:1 (3:1 for large text), on the same scale as
// contrast.json in glass-review.
import path from 'node:path';
import { loadWs } from './ws-resolve.mjs';

const ROOT = path.resolve(import.meta.dirname, '..');
const ws = loadWs(ROOT);

const arg = (name, fallback) => {
  const i = process.argv.indexOf(`--${name}`);
  return i === -1 ? fallback : process.argv[i + 1];
};
const PORT = Number(arg('port', 9223));
const MATCH = arg('match', '');
const SCROLL = Number(arg('scroll', 0));

const list = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
const page = list.find((t) => t.type === 'page' && (!MATCH || t.url.includes(MATCH)));
if (!page) throw new Error(`страница не найдена: ${list.map((t) => t.url).join(', ')}`);

const socket = new ws(page.webSocketDebuggerUrl, { maxPayload: 512 * 1024 * 1024 });
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
const send = (method, params = {}) =>
  new Promise((resolve, reject) => {
    const i = ++id;
    pending.set(i, (msg) =>
      msg.error ? reject(new Error(msg.error.message)) : resolve(msg.result),
    );
    socket.send(JSON.stringify({ id: i, method, params }));
  });
const evalJs = async (expression) => {
  const res = await send('Runtime.evaluate', {
    expression,
    awaitPromise: true,
    returnByValue: true,
  });
  if (res.exceptionDetails) {
    throw new Error(
      res.exceptionDetails.exception?.description || JSON.stringify(res.exceptionDetails),
    );
  }
  return res.result?.value;
};

await send('Page.enable', {});
await send('Runtime.enable', {});
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Scroll if requested: the first scrollport found (for the guide —
// .guide-content, for the showcase — the window).
if (SCROLL > 0) {
  await evalJs(`(() => {
    const el = document.querySelector('.guide-content') || document.scrollingElement;
    el.scrollTop = ${SCROLL};
    return el.scrollTop;
  })()`);
  await sleep(300);
}

const TEXT_PROBE = `(() => {
  const out = [];
  const seen = new Set();
  const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
  while (walker.nextNode()) {
    const node = walker.currentNode;
    const text = node.nodeValue.trim();
    if (!/[\\p{L}\\p{N}]/u.test(text)) continue;
    const el = node.parentElement;
    if (!el || seen.has(el)) continue;
    seen.add(el);
    const cs = getComputedStyle(el);
    if (cs.visibility !== 'visible' || cs.display === 'none') continue;
    const fill = cs.webkitTextFillColor;
    if (fill && /rgba\\(.*,\\s*0\\)$/.test(fill)) continue;
    const range = document.createRange();
    range.selectNodeContents(node);
    const r = range.getBoundingClientRect();
    if (r.width < 4 || r.height < 6) continue;
    const cx = Math.min(innerWidth - 1, Math.max(0, r.left + r.width / 2));
    const cy = Math.min(innerHeight - 1, Math.max(0, r.top + r.height / 2));
    if (r.bottom <= 0 || r.top >= innerHeight || r.right <= 0 || r.left >= innerWidth) continue;
    const top = document.elementFromPoint(cx, cy);
    if (!top || !(el.contains(top) || top.contains(el))) continue;
    let opacity = 1;
    for (let e = el; e; e = e.parentElement) opacity *= Number(getComputedStyle(e).opacity);
    if (opacity < 0.05) continue;
    out.push({
      text: text.slice(0, 48),
      sel: el.tagName.toLowerCase() + (el.id ? '#' + el.id : '') + (el.className && typeof el.className === 'string' ? '.' + el.className.trim().split(/\\s+/).slice(0, 2).join('.') : ''),
      color: cs.color,
      opacity,
      size: parseFloat(cs.fontSize),
      weight: Number(cs.fontWeight) || 400,
      rect: [Math.max(0, r.left), Math.max(0, r.top), Math.min(innerWidth, r.right), Math.min(innerHeight, r.bottom)],
    });
  }
  return { items: out, dpr: devicePixelRatio };
})()`;

const HIDE_TEXT_CSS =
  '*,*::before,*::after,::placeholder{color:transparent!important;-webkit-text-fill-color:transparent!important;text-shadow:none!important;caret-color:transparent!important;transition:none!important}';

const probe = await evalJs(TEXT_PROBE);
if (!probe?.items?.length) {
  console.log('видимого текста нет — проверять нечего');
  socket.close();
  process.exit(0);
}
await evalJs(
  `(() => { const s = new CSSStyleSheet(); s.replaceSync(${JSON.stringify(HIDE_TEXT_CSS)}); window.__lkHide = s; document.adoptedStyleSheets = [...document.adoptedStyleSheets, s]; return true; })()`,
);
await sleep(120);
const { data } = await send('Page.captureScreenshot', { format: 'png' });
await evalJs(
  `(document.adoptedStyleSheets = document.adoptedStyleSheets.filter((s) => s !== window.__lkHide)), true`,
);

const result = await evalJs(`(async () => {
  const probe = ${JSON.stringify(probe)};
  const img = new Image();
  img.src = 'data:image/png;base64,${data}';
  await img.decode();
  const canvas = document.createElement('canvas');
  canvas.width = img.width; canvas.height = img.height;
  const ctx = canvas.getContext('2d', { willReadFrequently: true });
  ctx.drawImage(img, 0, 0);
  const lin = (c) => { c /= 255; return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4; };
  const lum = (r, g, b) => 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
  const ratio = (a, b) => (Math.max(a, b) + 0.05) / (Math.min(a, b) + 0.05);
  const out = [];
  for (const it of probe.items) {
    const [x0, y0, x1, y1] = it.rect.map((v) => Math.round(v * probe.dpr));
    const w = Math.max(1, x1 - x0), h = Math.max(1, y1 - y0);
    const px = ctx.getImageData(x0, y0, w, h).data;
    const lums = [];
    let sr = 0, sg = 0, sb = 0, n = 0;
    const step = Math.max(1, Math.floor(Math.sqrt((w * h) / 4000)));
    for (let y = 0; y < h; y += step) for (let x = 0; x < w; x += step) {
      const i = (y * w + x) * 4;
      lums.push(lum(px[i], px[i + 1], px[i + 2]));
      sr += px[i]; sg += px[i + 1]; sb += px[i + 2]; n++;
    }
    lums.sort((a, b) => a - b);
    const m = it.color.match(/[\\d.]+/g).map(Number);
    const a = (m[3] ?? 1) * it.opacity;
    const bg = [sr / n, sg / n, sb / n];
    const fg = [0, 1, 2].map((k) => m[k] * a + bg[k] * (1 - a));
    const fgL = lum(...fg);
    const bgMid = lum(...bg);
    const worstBg = fgL >= bgMid ? lums[Math.floor(lums.length * 0.9)] : lums[Math.floor(lums.length * 0.1)];
    const large = it.size >= 24 || (it.size >= 18.66 && it.weight >= 700);
    out.push({
      text: it.text,
      sel: it.sel,
      ratio: Math.round(ratio(fgL, worstBg) * 100) / 100,
      need: large ? 3 : 4.5,
      color: it.color,
      size: it.size,
      alpha: +a.toFixed(2),
      bg: [0.1, 0.5, 0.9].map((p) => Math.round(lums[Math.floor(lums.length * p)] * 255)),
    });
  }
  return out;
})()`);

const failures = result.filter((r) => r.ratio < r.need).sort((a, b) => a.ratio - b.ratio);
console.log(`страница: ${page.url} (прокрутка ${SCROLL})`);
console.log(`пар текст/фон: ${result.length}, провалов: ${failures.length}`);
for (const f of failures.slice(0, 15)) {
  console.log(
    `  ${f.ratio} < ${f.need}  [${f.sel}] «${f.text}» ${f.color} ${f.size}px фон(p10/50/90)=${f.bg.join('/')}`,
  );
}
socket.close();
process.exit(failures.length ? 1 : 0);
