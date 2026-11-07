// Visual review of glass: screenshots of all surfaces, text contrast over
// the real background, and the cost of glass in a long chat.
//
// The install is isolated, like lk-cache-bench: src-tauri/target/lk-bench/install
// (own data and chats, Ollama on 11460, ST on a free port). The owner's working copy
// is not touched; the app is closed by its own PID via WM_CLOSE.
//
//   node tools/lk-glass-review.mjs --exe <path to exe> --label before|after
//        [--cdp-port 9241] [--skip app,st,perf] [--perf-runs 3]
//
// The result is in src-tauri/target/glass-review/<label>/: PNGs with identical names
// for comparison, contrast.json (text below 4.5:1 over the background from a text-free frame),
// perf.json (layers, number of backdrop-filters, scroll and stream frame, GPU thread
// occupancy from the trace).
//
// Screens absent in the normal state are shown by the bench via response substitution:
// splash — eternal `setup_state`, wizard — `setup_done: false`, the update
// window — `check_app_update`, ST top bar — responses from `update-status` and
// `update-progress`. Download, install, and update commands are blocked in these modes
// so that a click on the screenshot starts nothing.
import { execSync, spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { loadWs } from './ws-resolve.mjs';

const ROOT = path.resolve(import.meta.dirname, '..');
const INSTALL = path.join(ROOT, 'src-tauri', 'target', 'lk-bench', 'install');
process.env.LOREKEEPER_INSTALL ??= INSTALL;
const ws = loadWs(ROOT);

const arg = (name, fallback) => {
  const i = process.argv.indexOf(`--${name}`);
  return i === -1 ? fallback : process.argv[i + 1];
};
const EXE_SRC = arg('exe');
const LABEL = arg('label', 'run');
const CDP_PORT = Number(arg('cdp-port', 9241));
const SKIP = new Set(String(arg('skip', '')).split(',').filter(Boolean));
const PERF_RUNS = Number(arg('perf-runs', 3));
const OUT = path.join(ROOT, 'src-tauri', 'target', 'glass-review', LABEL);
const CHARACTER = process.env.LK_CHARACTER ?? 'default_Seraphina.png';
if (!EXE_SRC) throw new Error('--exe <путь к собранному Cloufam.exe>');
fs.mkdirSync(OUT, { recursive: true });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const log = (...a) => console.log(`[${LABEL}]`, ...a);

// ── install ───────────────────────────────────────────────────────────────
const DATA = path.join(INSTALL, 'data');
fs.copyFileSync(EXE_SRC, path.join(INSTALL, 'Cloufam.exe'));
const settingsFile = path.join(DATA, 'settings.json');
const settings = JSON.parse(fs.readFileSync(settingsFile, 'utf8'));
Object.assign(settings, {
  setup_done: true,
  auto_launch_st: false,
  auto_launch_comfy: false,
  tagger_enabled: false,
  // Vectorizing a long chat would load the GPU with the embedder during measurement.
  auto_memory: false,
  changelog_version_seen: JSON.parse(fs.readFileSync(path.join(ROOT, 'package.json'), 'utf8'))
    .version,
});
fs.writeFileSync(settingsFile, JSON.stringify(settings, null, 2));
// Same window geometry — otherwise before and after frames are not comparable.
fs.writeFileSync(
  path.join(DATA, 'window.json'),
  JSON.stringify({ width: 1375, height: 925, x: 320, y: 320, maximized: false }),
);

// ── CDP ─────────────────────────────────────────────────────────────────────
async function attach(predicate, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  let target = null;
  while (!target && Date.now() < deadline) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`)).json();
      target = list.find((t) => t.type === 'page' && predicate(t.url));
    } catch {
      // window not there yet
    }
    if (!target) await sleep(500);
  }
  if (!target) return null;
  const socket = new ws(target.webSocketDebuggerUrl, { maxPayload: 512 * 1024 * 1024 });
  await new Promise((resolve, reject) => {
    socket.once('open', resolve);
    socket.once('error', reject);
  });
  let id = 0;
  const pending = new Map();
  const listeners = new Map();
  socket.on('message', (raw) => {
    const msg = JSON.parse(raw.toString());
    if (msg.id) {
      pending.get(msg.id)?.(msg);
      pending.delete(msg.id);
    } else {
      for (const cb of listeners.get(msg.method) ?? []) cb(msg.params);
    }
  });
  const send = (method, params = {}) =>
    new Promise((resolve, reject) => {
      const i = ++id;
      pending.set(i, (msg) =>
        msg.error ? reject(new Error(`${method}: ${msg.error.message}`)) : resolve(msg.result),
      );
      socket.send(JSON.stringify({ id: i, method, params }));
    });
  const on = (method, cb) => {
    listeners.set(method, [...(listeners.get(method) ?? []), cb]);
    return () =>
      listeners.set(
        method,
        (listeners.get(method) ?? []).filter((x) => x !== cb),
      );
  };
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
  return { socket, send, on, evalJs };
}

async function waitFor(fn, timeoutMs, stepMs = 500) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const v = await fn();
      if (v) return v;
    } catch {
      // not ready yet
    }
    await sleep(stepMs);
  }
  return null;
}

// ── screenshot and contrast ───────────────────────────────────────────────────
const contrastReport = {};

async function shot(page, name, { clip, audit = true } = {}) {
  await sleep(350);
  const params = { format: 'png' };
  if (clip) params.clip = { ...clip, scale: 1 };
  const { data } = await page.send('Page.captureScreenshot', params);
  fs.writeFileSync(path.join(OUT, `${name}.png`), Buffer.from(data, 'base64'));
  if (audit && !clip) contrastReport[name] = await auditContrast(page);
  log('снимок', name);
}

// Text is checked against the background from a frame where all text is transparent: this way the background
// includes what actually lies under the letters (wallpaper through glass, fill,
// highlights), not the color from CSS. For light text, the light edge of the background is taken
// (90th percentile of brightness), for dark — the dark one.
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

async function auditContrast(page) {
  const probe = await page.evalJs(TEXT_PROBE);
  if (!probe?.items?.length) return { checked: 0, failures: [] };
  // A constructed table, not <style>: the page CSP doesn't cut it.
  await page.evalJs(
    `(() => { const s = new CSSStyleSheet(); s.replaceSync(${JSON.stringify(HIDE_TEXT_CSS)}); window.__lkHide = s; document.adoptedStyleSheets = [...document.adoptedStyleSheets, s]; return true; })()`,
  );
  await sleep(120);
  const { data } = await page.send('Page.captureScreenshot', { format: 'png' });
  await page.evalJs(
    `(document.adoptedStyleSheets = document.adoptedStyleSheets.filter((s) => s !== window.__lkHide)), true`,
  );
  // PNG parsing — in the page itself: the decoder is already there.
  const result = await page.evalJs(`(async () => {
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
      const r = ratio(fgL, worstBg);
      out.push({ text: it.text, sel: it.sel, ratio: Math.round(r * 100) / 100, need: large ? 3 : 4.5, color: it.color, size: it.size, alpha: +a.toFixed(2) });
    }
    return out;
  })()`);
  const failures = result.filter((r) => r.ratio < r.need).sort((a, b) => a.ratio - b.ratio);
  return {
    checked: result.length,
    min: result.reduce((m, r) => Math.min(m, r.ratio), Infinity),
    failures,
  };
}

// ── response substitutions for rare screens ──────────────────────────────────
const BLOCKED =
  '^(download|install|apply|delete|reset|finalize|start_|trainer_install|training_start|image_training_start|create_desktop)';
// `__TAURI_INTERNALS__.invoke` cannot be overwritten (writable: false), so
// the substitution sits on the transport: on Windows the command goes as a fetch request to
// http://ipc.localhost/<command>, the response is a Tauri-Response header and JSON.
const APP_HOOK = `(() => {
  const mode = sessionStorage.getItem('lk-review-mode') || '';
  if (!mode || location.hostname !== 'tauri.localhost') return;
  const real = window.fetch.bind(window);
  const reply = (value, ok = true) => new Response(ok ? JSON.stringify(value) : String(value), {
    status: 200,
    headers: { 'Content-Type': ok ? 'application/json' : 'text/plain', 'Tauri-Response': ok ? 'ok' : 'error' },
  });
  window.fetch = async (input, init) => {
    const url = String(typeof input === 'string' ? input : input.url);
    if (!url.startsWith('http://ipc.localhost/')) return real(input, init);
    const cmd = decodeURIComponent(new URL(url).pathname.slice(1));
    if (new RegExp(${JSON.stringify(BLOCKED)}).test(cmd)) return reply('lk-review: команда заблокирована стендом', false);
    if (mode === 'boot' && cmd === 'setup_state') return new Promise(() => {});
    if (mode === 'update' && cmd === 'check_app_update') {
      return reply({ available: true, current_version: '0', latest_version: '9.9.9', notes: 'Жидкое стекло во всех окнах: один материал, читаемый текст, лёгкий чат.' });
    }
    const res = await real(input, init);
    if (mode === 'wizard' && cmd === 'setup_state' && res.headers.get('Tauri-Response') === 'ok') {
      const state = await res.json();
      state.settings.setup_done = false;
      return reply(state);
    }
    return res;
  };
})()`;

const ST_HOOK = `(() => {
  const mode = sessionStorage.getItem('lk-review-st') || '';
  if (!mode || location.hostname !== '127.0.0.1') return;
  const real = window.fetch.bind(window);
  const json = (v) => new Response(JSON.stringify(v), { status: 200, headers: { 'Content-Type': 'application/json' } });
  let progress = 0;
  window.fetch = (input, init) => {
    const url = String(typeof input === 'string' ? input : input.url);
    if (url.includes('/api/cloufam/update-status')) return Promise.resolve(json({ available: true, latest_version: '9.9.9', current_version: '1.2.53' }));
    if (url.includes('/api/cloufam/apply-update')) return Promise.resolve(json({ ok: true }));
    if (url.includes('/api/cloufam/versions/rollback')) return Promise.resolve(new Response('заблокировано стендом', { status: 403 }));
    if (url.includes('/api/cloufam/update-progress')) {
      progress = Math.min(62, progress + 31);
      return Promise.resolve(json({ updating: true, done: false, error: null, percent: progress, downloaded_bytes: progress * 209715, total_bytes: 20971520, stage: 'Загрузка' }));
    }
    return real(input, init);
  };
})()`;

// ── application ──────────────────────────────────────────────────────────────
let app = null;
async function closeApp() {
  if (!app) return;
  try {
    execSync(`taskkill /PID ${app.pid}`, { stdio: 'ignore' });
  } catch {
    // already closed
  }
  for (let i = 0; i < 60; i += 1) {
    await sleep(1000);
    try {
      process.kill(app.pid, 0);
    } catch {
      log('приложение закрыто');
      return;
    }
  }
  log(`ВНИМАНИЕ: PID ${app.pid} не закрылся по WM_CLOSE за 60 с — закрыть руками`);
}

const TABS = [
  'chat',
  'embed',
  'vision',
  'image',
  'tune',
  'params',
  'custom',
  'tags',
  'train',
  'imgtrain',
];

async function reviewApp(main) {
  await main.send('Page.enable');
  await main.send('Page.addScriptToEvaluateOnNewDocument', { source: APP_HOOK });
  const setMode = async (mode) => {
    await main.evalJs(`sessionStorage.setItem('lk-review-mode', ${JSON.stringify(mode)}), true`);
    await main.send('Page.reload', { ignoreCache: false });
    await sleep(2500);
  };

  await setMode('boot');
  await waitFor(() => main.evalJs(`!!document.querySelector('.boot-card')`), 15000);
  await sleep(800);
  await shot(main, 'app-boot');

  await setMode('wizard');
  await waitFor(() => main.evalJs(`!!document.querySelector('.wizard .panel')`), 20000);
  await sleep(800);
  await shot(main, 'app-wizard');

  await setMode('update');
  await waitFor(() => main.evalJs(`!!document.querySelector('.update-card')`), 30000);
  await sleep(800);
  await shot(main, 'app-update-modal');

  await setMode('');
  await waitFor(() => main.evalJs(`document.querySelectorAll('.tab-btn').length >= 10`), 30000);
  await sleep(2500);
  for (const [i, tab] of TABS.entries()) {
    await main.evalJs(`document.querySelectorAll('.tab-btn')[${i}].click(), true`);
    await sleep(tab === 'tune' || tab === 'tags' ? 3000 : 1500);
    await shot(main, `app-settings-${String(i + 1).padStart(2, '0')}-${tab}`);
  }
  await main.evalJs(`document.querySelectorAll('.tab-btn')[0].click(), true`);
}

// ── SillyTavern ─────────────────────────────────────────────────────────────
const USER_LINES = [
  'Привет, Фубуки! Не ожидал встретить тебя в библиотеке так поздно.',
  '*Протягиваю руку к верхней полке.* Какая обложка?',
  'Вот, кажется, эта? Пыльная, но тиснение серебряное.',
  'Интересно, кто её читал до нас. "Может, прочитаем вместе?"',
  'Старый мост за рекой? Туда полчаса идти.',
];
const BOT_LINES = [
  '*Фубуки вздрагивает и прижимает книгу к груди.* "Кон-кон! Ты меня напугал." ~Хорошо, что это он, а не библиотекарь.~',
  'Синяя, с серебряным тиснением. И пахнет она, говорят, как осенний лес. *Она смущённо улыбается.* Звучит глупо, но я правда хочу её найти.',
  '"Она!" *Лиса хлопает в ладоши, хвост мелькает за спиной.* Смотри, тут даже закладка осталась — засушенный кленовый лист. ~Кто-то ждал, что её найдут.~',
  'На первой странице кто-то написал карандашом: «Тому, кто найдёт — праздник начинается у старого моста». *Она поднимает глаза.* "Вот это загадка!"',
  'Полчаса — это ерунда для лисы. Но уже темно... *Фубуки теребит край рукава.* "Ты же проводишь меня?" ~Одной правда страшно, но признаваться не хочу.~',
];
const LAST_BOT =
  '*Фубуки прячет улыбку за раскрытой книгой и косится на тебя поверх страниц.* ~Он правда пришёл сюда ради меня? Нет, глупости, просто совпадение...~ "Спасибо, что проводил. С тобой почему-то совсем не страшно."';

async function openChat(st) {
  return st.evalJs(`(async () => {
    const ctx = SillyTavern.getContext();
    const index = ctx.characters.findIndex((c) => c.avatar === '${CHARACTER}');
    if (index < 0) return 0;
    await ctx.selectCharacterById(index);
    await (await import('/script.js')).doNewChat();
    const chat = SillyTavern.getContext().chat;
    const now = new Date().toISOString();
    // name2 после selectCharacterById обновляется не сразу — имя берём из карточки.
    const bot = ctx.characters[index].name;
    const users = ${JSON.stringify(USER_LINES)};
    const bots = ${JSON.stringify(BOT_LINES)};
    for (let i = 0; i < 60; i++) {
      chat.push({ name: ctx.name1, is_user: true, is_system: false, send_date: now, mes: users[i % users.length], extra: {} });
      chat.push({ name: bot, is_user: false, is_system: false, send_date: now, mes: bots[i % bots.length], extra: {} });
    }
    chat.push({ name: ctx.name1, is_user: true, is_system: false, send_date: now, mes: 'Ну вот и мост. Видишь огоньки у воды?', extra: {} });
    chat.push({ name: bot, is_user: false, is_system: false, send_date: now, mes: ${JSON.stringify(LAST_BOT)}, extra: {} });
    await ctx.saveChat();
    await ctx.reloadCurrentChat();
    return SillyTavern.getContext().chat.length;
  })()`);
}

const toBottom = `(() => { const c = document.getElementById('chat'); c.scrollTop = c.scrollHeight; return true; })()`;

async function toggleDrawer(st, selector) {
  await st.evalJs(
    `(() => { const t = document.querySelector(${JSON.stringify(`${selector} .drawer-toggle`)}); t && t.click(); return !!t; })()`,
  );
  await sleep(900);
}

async function reviewSt(st) {
  await st.evalJs(toBottom);
  await sleep(1200);
  await shot(st, 'st-chat');

  const thought = await st.evalJs(`(() => {
    const t = [...document.querySelectorAll('#chat .mes .custom-lk-thought')].pop();
    if (!t) return null;
    const mes = t.closest('.mes');
    mes.scrollIntoView({ block: 'center' });
    const r = mes.getBoundingClientRect();
    return { x: Math.max(0, r.left - 24), y: Math.max(0, r.top - 24), width: r.width + 48, height: r.height + 48 };
  })()`);
  if (thought) await shot(st, 'st-message-thought', { clip: thought });
  else log('ВНИМАНИЕ: мысль (.custom-lk-thought) в чате не найдена');

  await st.evalJs(
    `(() => { const c = document.getElementById('chat'); c.scrollTop = c.scrollHeight * 0.45; return true; })()`,
  );
  await sleep(800);
  await shot(st, 'st-chat-scrolled');
  await st.evalJs(toBottom);

  for (const [name, sel] of [
    ['st-drawer-ai', '#ai-config-button'],
    ['st-drawer-api', '#sys-settings-button'],
    ['st-drawer-user', '#user-settings-button'],
    ['st-drawer-extensions', '#extensions-settings-button'],
    ['st-drawer-characters', '#rightNavHolder'],
  ]) {
    await toggleDrawer(st, sel);
    await shot(st, name);
    await toggleDrawer(st, sel);
  }

  await st.evalJs(`document.getElementById('extensionsMenuButton')?.click(), true`);
  await sleep(700);
  await shot(st, 'st-wand-menu');
  await st.evalJs(`document.body.click(), true`);
  await sleep(400);

  await st.evalJs(`window.__lkOpenModels && window.__lkOpenModels(), true`);
  await sleep(2500);
  await shot(st, 'st-models-menu');
  await st.evalJs(`document.getElementById('lk-models-close')?.click(), true`);
  await sleep(400);

  await st.evalJs(
    `document.getElementById('cloufam-director-nav-btn')?.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true })), true`,
  );
  await sleep(800);
  await shot(st, 'st-director-modal');
  await st.evalJs(`document.getElementById('lk-director-modal')?.remove(), true`);

  // Mode badge above the input: turn the mode on and immediately put it back.
  await st.evalJs(`document.getElementById('cloufam-director-nav-btn')?.click(), true`);
  await sleep(700);
  await shot(st, 'st-director-badge');
  await st.evalJs(`document.getElementById('cloufam-director-nav-btn')?.click(), true`);
  await sleep(400);

  for (const [name, item, modal] of [
    ['st-tagger-modal', 'cloufam-tagger-menu-item', 'cloufam-tagger-modal'],
    [
      'st-relationships-modal',
      'cloufam-relationships-menu-item',
      'cloufam-relationships-modal',
    ],
    ['st-rpg-modal', 'cloufam-rpg-menu-item', 'cloufam-rpg-modal'],
    ['st-mdimport-modal', 'cloufam-md-import-menu-item', 'cloufam-md-import-modal'],
    ['st-changelog-modal', 'cloufam-changelog-menu-item', 'cloufam-changelog-modal'],
  ]) {
    await st.evalJs(`document.getElementById(${JSON.stringify(item)})?.click(), true`);
    const opened = await waitFor(
      () => st.evalJs(`!!document.getElementById(${JSON.stringify(modal)})`),
      8000,
    );
    await sleep(1500);
    if (opened) await shot(st, name);
    else log(`ВНИМАНИЕ: ${modal} не открылся`);
    await st.evalJs(`document.getElementById(${JSON.stringify(modal)})?.remove(), true`);
    await sleep(300);
  }
}

async function reviewUpdateBar(st, stPort) {
  await st.send('Page.enable');
  await st.send('Page.addScriptToEvaluateOnNewDocument', { source: ST_HOOK });
  await st.evalJs(`sessionStorage.setItem('lk-review-st', '1'), true`);
  await st.send('Page.reload', {});
  const bar = await waitFor(
    () => st.evalJs(`!!document.getElementById('lk-bar-update-btn')`),
    40000,
    1000,
  );
  if (!bar) {
    log('ВНИМАНИЕ: топ-бар обновления не показался');
  } else {
    await sleep(3500);
    await shot(st, 'st-update-bar');
    await st.evalJs(`document.getElementById('lk-bar-versions-btn')?.click(), true`);
    await waitFor(() => st.evalJs(`!!document.getElementById('lk-versions-modal')`), 5000);
    await sleep(4000);
    await shot(st, 'st-versions-modal');
    await st.evalJs(`document.getElementById('lk-versions-modal')?.remove(), true`);
    // The update request is intercepted by the fetch substitution: it doesn't reach the gateway.
    const hooked = await st.evalJs(`String(window.fetch).includes('update-progress')`);
    if (hooked) {
      await st.evalJs(`document.getElementById('lk-bar-update-btn')?.click(), true`);
      await sleep(1500);
      await shot(st, 'st-update-progress');
    }
  }
  await st.evalJs(`sessionStorage.removeItem('lk-review-st'), true`);
  await st.send('Page.reload', {});
  void stPort;
}

// ── performance ──────────────────────────────────────────────────────
const stats = (arr) => {
  const s = [...arr].sort((a, b) => a - b);
  const q = (p) => s[Math.min(s.length - 1, Math.floor(s.length * p))];
  return {
    frames: s.length,
    p50: +q(0.5).toFixed(2),
    p95: +q(0.95).toFixed(2),
    max: +s[s.length - 1].toFixed(2),
    over20ms: s.filter((v) => v > 20).length,
  };
};

async function traced(page, fn) {
  const events = [];
  const off = page.on('Tracing.dataCollected', (p) => events.push(...p.value));
  const done = new Promise((resolve) => {
    const offDone = page.on('Tracing.tracingComplete', () => {
      offDone();
      resolve();
    });
  });
  await page.send('Tracing.start', {
    categories:
      'devtools.timeline,disabled-by-default-devtools.timeline,disabled-by-default-devtools.timeline.frame,viz,gpu,cc,benchmark,toplevel',
    transferMode: 'ReportEvents',
  });
  const frames = await fn();
  await page.send('Tracing.end');
  await done;
  off();
  // Thread occupancy — merging intervals so that nested events are not
  // counted twice.
  const names = new Map();
  for (const e of events) {
    if (e.ph === 'M' && e.name === 'thread_name') names.set(`${e.pid}:${e.tid}`, e.args.name);
  }
  const spans = new Map();
  const open = new Map();
  for (const e of events) {
    const key = `${e.pid}:${e.tid}`;
    let span = null;
    if (e.ph === 'X' && e.dur) span = [e.ts, e.ts + e.dur];
    else if (e.ph === 'B') {
      open.set(key, [...(open.get(key) ?? []), e.ts]);
    } else if (e.ph === 'E') {
      const stack = open.get(key) ?? [];
      const start = stack.pop();
      if (start !== undefined) span = [start, e.ts];
    }
    if (span) spans.set(key, [...(spans.get(key) ?? []), span]);
  }
  const busy = {};
  for (const [key, list] of spans) {
    const thread = names.get(key) ?? '?';
    if (!/^(CrGpuMain|VizCompositorThread|CrRendererMain|Compositor)$/.test(thread)) continue;
    list.sort((a, b) => a[0] - b[0]);
    let total = 0;
    let [cs, ce] = list[0];
    for (const [s, e] of list.slice(1)) {
      if (s > ce) {
        total += ce - cs;
        [cs, ce] = [s, e];
      } else ce = Math.max(ce, e);
    }
    total += ce - cs;
    busy[thread] = (busy[thread] ?? 0) + total / 1000;
  }
  const gpuTasks = events
    .filter((e) => e.name === 'GPUTask' && e.dur)
    .reduce((sum, e) => sum + e.dur / 1000, 0);
  const n = frames.length || 1;
  const perFrame = Object.fromEntries(
    Object.entries(busy).map(([k, v]) => [k, +(v / n).toFixed(3)]),
  );
  return {
    frames: stats(frames),
    busy_ms_per_frame: perFrame,
    gpu_task_ms_per_frame: +(gpuTasks / n).toFixed(3),
  };
}

const SCROLL_TEST = `new Promise((resolve) => {
  const chat = document.getElementById('chat');
  chat.scrollTop = chat.scrollHeight;
  const total = chat.scrollHeight - chat.clientHeight;
  const frames = 240;
  const deltas = [];
  let i = 0, last = 0;
  const tick = (now) => {
    if (last) deltas.push(now - last);
    last = now;
    chat.scrollTop = total - (total * i) / frames;
    i++;
    if (i <= frames) requestAnimationFrame(tick); else resolve(deltas);
  };
  requestAnimationFrame(tick);
})`;

// Stream: appending text to the last reply on every frame with scrolling
// down, as ST does during streaming generation.
const STREAM_TEST = `new Promise((resolve) => {
  const chat = document.getElementById('chat');
  const text = [...chat.querySelectorAll('.mes .mes_text')].pop();
  const node = text.appendChild(document.createTextNode(''));
  const words = 'снег падает на воду и тихо гаснет в тёмной реке '.split(' ');
  const deltas = [];
  let i = 0, last = 0;
  const tick = (now) => {
    if (last) deltas.push(now - last);
    last = now;
    node.appendData(' ' + words[i % words.length]);
    chat.scrollTop = chat.scrollHeight;
    i++;
    if (i <= 180) requestAnimationFrame(tick); else { node.remove(); resolve(deltas); }
  };
  requestAnimationFrame(tick);
})`;

async function reviewPerf(st) {
  await st.evalJs(toBottom);
  await sleep(1500);
  const census = await st.evalJs(`(() => {
    const all = [...document.querySelectorAll('*')];
    const has = (cs) => cs.backdropFilter && cs.backdropFilter !== 'none';
    let total = 0, messages = 0, pseudo = 0;
    for (const el of all) {
      if (has(getComputedStyle(el))) { total++; if (el.matches('#chat .mes, #chat .mes *')) messages++; }
      for (const p of ['::before', '::after']) if (has(getComputedStyle(el, p))) pseudo++;
    }
    return { elements_with_backdrop: total, in_messages: messages, pseudo_with_backdrop: pseudo, rendered_messages: document.querySelectorAll('#chat .mes').length };
  })()`);
  await st.send('LayerTree.enable');
  let layers = null;
  const offLayers = st.on('LayerTree.layerTreeDidChange', (p) => {
    if (p.layers) layers = p.layers.length;
  });
  await st.evalJs(
    `(() => { const c = document.getElementById('chat'); c.scrollTop -= 400; return true; })()`,
  );
  await sleep(1500);
  await st.evalJs(toBottom);
  await sleep(1500);
  offLayers();
  await st.send('LayerTree.disable');

  const runs = { scroll: [], stream: [] };
  for (let r = 0; r < PERF_RUNS; r += 1) {
    await st.evalJs(toBottom);
    await sleep(800);
    runs.scroll.push(await traced(st, () => st.evalJs(SCROLL_TEST)));
    await st.evalJs(toBottom);
    await sleep(800);
    runs.stream.push(await traced(st, () => st.evalJs(STREAM_TEST)));
  }
  const median = (list, pick) => {
    const v = list.map(pick).sort((a, b) => a - b);
    return v[Math.floor(v.length / 2)];
  };
  const summary = {};
  for (const kind of ['scroll', 'stream']) {
    const list = runs[kind];
    summary[kind] = {
      frame_p50: median(list, (x) => x.frames.p50),
      frame_p95: median(list, (x) => x.frames.p95),
      over20ms: median(list, (x) => x.frames.over20ms),
      gpu_main_ms: median(list, (x) => x.busy_ms_per_frame.CrGpuMain ?? 0),
      viz_ms: median(list, (x) => x.busy_ms_per_frame.VizCompositorThread ?? 0),
      renderer_main_ms: median(list, (x) => x.busy_ms_per_frame.CrRendererMain ?? 0),
      renderer_compositor_ms: median(list, (x) => x.busy_ms_per_frame.Compositor ?? 0),
      gpu_task_ms: median(list, (x) => x.gpu_task_ms_per_frame),
    };
  }
  const perf = { census, layers, summary, runs };
  fs.writeFileSync(path.join(OUT, 'perf.json'), JSON.stringify(perf, null, 2));
  log('perf', JSON.stringify({ census, layers, summary }));
}

// ── run ──────────────────────────────────────────────────────────────────
try {
  app = spawn(path.join(INSTALL, 'Cloufam.exe'), [], {
    cwd: INSTALL,
    detached: true,
    stdio: 'ignore',
    env: {
      ...process.env,
      WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${CDP_PORT}`,
    },
  });
  app.unref();
  const main = await attach(
    (url) => url.startsWith('http://tauri.localhost') && !url.includes('#'),
    90000,
  );
  if (!main) throw new Error('главное окно не появилось в CDP');
  const boot = await waitFor(
    () =>
      main.evalJs(
        `window.__TAURI_INTERNALS__.invoke('boot_state').then((s) => (s.ready || s.error) ? s : null)`,
      ),
    300000,
    1000,
  );
  if (!boot?.ready) throw new Error(`подъём не завершился: ${JSON.stringify(boot)}`);
  log('подъём готов, ST на', boot.port);
  if (!SKIP.has('app')) await reviewApp(main);

  await main.evalJs(
    `window.__TAURI_INTERNALS__.invoke('mark_st_opened').then(() => { location.href = 'http://127.0.0.1:${boot.port}/'; return true; })`,
  );
  main.socket.close();
  const st = await attach((url) => url.startsWith(`http://127.0.0.1:${boot.port}`), 90000);
  if (!st) throw new Error('страница SillyTavern не открылась');
  const ready = () =>
    waitFor(
      () =>
        st.evalJs(
          `!!(window.SillyTavern?.getContext && document.getElementById('cloufam-director-nav-btn') && document.getElementById('cloufam-glass'))`,
        ),
      120000,
      1000,
    );
  if (!(await ready())) throw new Error('ST не догрузилась (нет режиссёра или стекла)');
  await sleep(2000);
  const messages = await openChat(st);
  if (!messages) throw new Error(`персонаж ${CHARACTER} не найден`);
  log('чат', messages, 'сообщений');
  await sleep(3000);

  if (!SKIP.has('st')) await reviewSt(st);
  if (!SKIP.has('perf')) await reviewPerf(st);
  if (!SKIP.has('st')) {
    await reviewUpdateBar(st, boot.port);
    await ready();
  }
  fs.writeFileSync(path.join(OUT, 'contrast.json'), JSON.stringify(contrastReport, null, 2));
  const worst = Object.entries(contrastReport)
    .map(
      ([name, r]) =>
        `${name}: ${r.failures.length}/${r.checked} ниже нормы, min ${r.min?.toFixed?.(2)}`,
    )
    .join('\n');
  log('контраст\n' + worst);
} catch (e) {
  console.error(e);
  process.exitCode = 1;
} finally {
  await closeApp();
}

