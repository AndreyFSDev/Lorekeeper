// End-to-end live run after the audit of 25.09.2026: the gateway, SillyTavern seeds,
// the director, the state estimator, the RPG panel and the settings window — on a working copy.
//
// The harness starts the app with CDP itself, works in the bundled Assistant chat and
// at the end restores everything it touched: the Assistant chats folder and the tracker files
// (traits/relationships/stats/skills). The RPG world is reset by the panel button.
// The app closes gracefully (WM_CLOSE → RunEvent::Exit), after which the harness
// looks for orphan node/ollama from the installation folder.
//
// Usage (the app must be closed, port 11434 — free or taken by
// this installation's Ollama):
//   node tools/lk-audit-smoke.mjs [--port 9223] [--keep-open]
import { execSync, spawn } from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { loadWs } from './ws-resolve.mjs';

const ROOT = path.resolve(import.meta.dirname, '..');
const INSTALL = process.env.LOREKEEPER_INSTALL ?? 'Z:\\AImodels\\Cloufam';
process.env.LOREKEEPER_INSTALL = INSTALL;
const ws = loadWs(ROOT);

const arg = (name, fallback) => {
  const i = process.argv.indexOf(`--${name}`);
  return i === -1 ? fallback : process.argv[i + 1];
};
const CDP_PORT = Number(arg('port', 9223));
const KEEP_OPEN = process.argv.includes('--keep-open');

const DATA = path.join(INSTALL, 'data');
const ST_USER = path.join(INSTALL, 'sillytavern', 'data', 'default-user');
const ASSISTANT_CHATS = path.join(ST_USER, 'chats', 'default_Assistant');
const BACKUP = path.join(ROOT, 'src-tauri', 'target', 'lk-audit', 'smoke-backup');
const TRACKERS = [
  'traits.json',
  'relationships.json',
  'characters_stats.json',
  'characters_skills.json',
];

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const results = [];
function check(name, ok, detail = '') {
  results.push({ name, ok: Boolean(ok), detail });
  console.log(`${ok ? 'ПРОШЛО' : 'ПРОВАЛ'}: ${name}${detail ? ` — ${detail}` : ''}`);
}

// ── backup ────────────────────────────────────────────────────────────────────
fs.rmSync(BACKUP, { recursive: true, force: true });
fs.mkdirSync(BACKUP, { recursive: true });
for (const f of TRACKERS) {
  const src = path.join(DATA, f);
  if (fs.existsSync(src)) fs.copyFileSync(src, path.join(BACKUP, f));
}
if (fs.existsSync(ASSISTANT_CHATS)) {
  fs.cpSync(ASSISTANT_CHATS, path.join(BACKUP, 'default_Assistant'), { recursive: true });
}
const hadWorld = fs.existsSync(path.join(DATA, 'world.json'));
console.log(`бэкап: ${BACKUP}`);

function restore() {
  for (const f of TRACKERS) {
    const saved = path.join(BACKUP, f);
    const live = path.join(DATA, f);
    if (fs.existsSync(saved)) fs.copyFileSync(saved, live);
    else fs.rmSync(live, { force: true });
  }
  const savedChats = path.join(BACKUP, 'default_Assistant');
  if (fs.existsSync(savedChats)) {
    fs.rmSync(ASSISTANT_CHATS, { recursive: true, force: true });
    fs.cpSync(savedChats, ASSISTANT_CHATS, { recursive: true });
  }
  if (!hadWorld) {
    for (const f of ['world.json', 'active_combat.json'])
      fs.rmSync(path.join(DATA, f), { force: true });
  }
}

// ── CDP ──────────────────────────────────────────────────────────────────────
async function targets() {
  try {
    return await (await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`)).json();
  } catch {
    return [];
  }
}

async function attach(predicate, timeoutMs = 60000) {
  const deadline = Date.now() + timeoutMs;
  let page = null;
  while (!page && Date.now() < deadline) {
    page = (await targets()).find((t) => t.type === 'page' && predicate(t.url));
    if (!page) await sleep(500);
  }
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
    if (entry) {
      pending.delete(msg.id);
      entry(msg);
    }
  });
  socket.on('close', () => {
    for (const resolve of pending.values()) resolve({ result: { result: { value: undefined } } });
    pending.clear();
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

function rawRequest(port, { method = 'GET', urlPath, headers = {}, body }) {
  return new Promise((resolve) => {
    const req = http.request({ host: '127.0.0.1', port, method, path: urlPath, headers }, (res) => {
      let data = '';
      res.on('data', (c) => (data += c));
      res.on('end', () => resolve({ status: res.statusCode, body: data }));
    });
    req.on('error', (e) => resolve({ status: 0, body: String(e) }));
    if (body) req.write(body);
    req.end();
  });
}

const readJson = (file) => {
  try {
    return JSON.parse(fs.readFileSync(file, 'utf8'));
  } catch {
    return null;
  }
};

// ── launch ───────────────────────────────────────────────────────────────────
const exe = path.join(INSTALL, 'Cloufam.exe');
const app = spawn(exe, [], {
  cwd: INSTALL,
  detached: true,
  stdio: 'ignore',
  env: {
    ...process.env,
    WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${CDP_PORT}`,
  },
});
app.unref();
console.log(`запущен ${exe}`);

let exitCode = 1;
try {
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
    240000,
    1000,
  );
  check(
    'подъём Ollama и SillyTavern',
    boot?.ready,
    boot
      ? `порт ST ${boot.port}${boot.error ? `, ошибка: ${boot.error}` : ''}`
      : 'нет ответа за 240 с',
  );
  if (!boot?.ready) throw new Error('подъём не завершился');
  const stPort = boot.port;
  const settings = await main.evalJs(`window.__TAURI_INTERNALS__.invoke('get_settings')`);
  const gatewayPort = await waitFor(async () => {
    for (let p = settings.ollama_port + 1; p <= settings.ollama_port + 15; p += 1) {
      const r = await rawRequest(p, { urlPath: '/v1/models' });
      if (r.status === 200) return p;
    }
    for (let p = settings.ollama_port - 1; p >= settings.ollama_port - 15; p -= 1) {
      const r = await rawRequest(p, { urlPath: '/v1/models' });
      if (r.status === 200) return p;
    }
    return null;
  }, 20000);
  check('шлюз отвечает', gatewayPort, `порт ${gatewayPort}`);

  // ── gateway: source policy ─────────────────────────────────────────────
  const evil = await rawRequest(gatewayPort, {
    urlPath: '/api/cloufam/models',
    headers: { Origin: 'https://evil.example' },
  });
  check('чужой Origin получает 403', evil.status === 403, `статус ${evil.status}`);
  const stOrigin = await rawRequest(gatewayPort, {
    urlPath: '/api/cloufam/models',
    headers: { Origin: `http://127.0.0.1:${stPort}` },
  });
  check('Origin страницы ST пропускается', stOrigin.status === 200, `статус ${stOrigin.status}`);
  const noOrigin = await rawRequest(gatewayPort, { urlPath: '/api/cloufam/models' });
  check(
    'запрос без Origin (плагин ST) пропускается',
    noOrigin.status === 200,
    `статус ${noOrigin.status}`,
  );
  const rebinding = await rawRequest(gatewayPort, {
    urlPath: '/api/cloufam/models',
    headers: { Host: `evil.example:${gatewayPort}` },
  });
  check(
    'чужой Host (DNS rebinding) получает 403',
    rebinding.status === 403,
    `статус ${rebinding.status}`,
  );
  const preflight = await rawRequest(gatewayPort, {
    method: 'OPTIONS',
    urlPath: '/api/cloufam/rpg/world/start',
    headers: { Origin: `http://127.0.0.1:${stPort}`, 'Access-Control-Request-Method': 'POST' },
  });
  check(
    'preflight со страницы ST проходит',
    preflight.status >= 200 && preflight.status < 300,
    `статус ${preflight.status}`,
  );
  const worldBefore = JSON.parse(
    (await rawRequest(gatewayPort, { urlPath: '/api/cloufam/rpg/world' })).body || '{}',
  );
  check(
    'RPG-мир до старта не выдаёт дефолтный город',
    worldBefore.started === false || hadWorld,
    JSON.stringify(worldBefore).slice(0, 120),
  );

  // ── SillyTavern ──────────────────────────────────────────────────────────
  await main.evalJs(
    `window.__TAURI_INTERNALS__.invoke('mark_st_opened').then(() => { location.href = 'http://127.0.0.1:${stPort}/'; return true; })`,
  );
  const st = await attach((url) => url.startsWith(`http://127.0.0.1:${stPort}`), 60000);
  if (!st) throw new Error('страница SillyTavern не открылась');
  const ready = await waitFor(
    () =>
      st.evalJs(
        `!!(window.SillyTavern?.getContext && document.getElementById('cloufam-rpg-menu-item') && document.getElementById('cloufam-director-nav-btn'))`,
      ),
    120000,
    1000,
  );
  check('расширения Cloufam загрузились в ST', ready);
  await sleep(4000); // the "What's new" auto-show waits 3 s after load

  const menu = await st.evalJs(
    `[...document.querySelectorAll('#extensionsMenu [id^="cloufam-"]')].map((e) => e.id)`,
  );
  const expectedMenu = [
    'cloufam-tagger-menu-item',
    'cloufam-traits-menu-item',
    'cloufam-relationships-menu-item',
    'cloufam-rpg-menu-item',
    'cloufam-md-import-menu-item',
    'cloufam-models-menu-item',
  ];
  const missing = expectedMenu.filter((id) => !menu.includes(id));
  check(
    'пункты меню жезла на месте',
    missing.length === 0,
    missing.length ? `нет: ${missing.join(', ')}` : `${menu.length} пунктов`,
  );
  const changelogIcon = await st.evalJs(
    `(() => { const i = [...document.querySelectorAll('#extensionsMenu .extensionsMenuExtensionButton')].find((e) => e.parentElement?.textContent.includes('Что нового')); return i ? getComputedStyle(i, '::before').content : null; })()`,
  );
  check(
    'у «Что нового» видимая иконка',
    changelogIcon && changelogIcon !== 'none' && changelogIcon !== '""',
    `content ${changelogIcon}`,
  );
  const changelogOpen = await st.evalJs(`!!document.getElementById('cloufam-changelog-modal')`);
  // The "What's new" auto-show is honest: on a version newer than the viewed one the modal pops up
  // and must pop up. The "does not pop up without new versions" check applies only
  // when there are no new versions — otherwise it would catch correct behaviour on every release.
  const appVersion = JSON.parse(fs.readFileSync(path.join(ROOT, 'package.json'), 'utf8')).version;
  const seenVersion = String(
    readJson(path.join(DATA, 'settings.json'))?.changelog_version_seen ?? '',
  );
  const isNewer = (a, b) => {
    const pa = String(a).split('.').map(Number);
    const pb = String(b).split('.').map(Number);
    for (let i = 0; i < Math.max(pa.length, pb.length); i += 1) {
      const d = (pa[i] || 0) - (pb[i] || 0);
      if (d) return d > 0;
    }
    return false;
  };
  check(
    '«Что нового» не всплывает без новых версий',
    isNewer(appVersion, seenVersion) || !changelogOpen,
    `app ${appVersion}, просмотрено ${seenVersion || '—'}`,
  );

  const thought = await st.evalJs(`(() => {
    const host = document.createElement('div');
    host.className = 'mes_text';
    host.innerHTML = '<span class="custom-lk-thought">мысль</span>';
    document.getElementById('chat').appendChild(host);
    const cs = getComputedStyle(host.firstChild);
    const out = { color: cs.color, fontStyle: cs.fontStyle, background: cs.backgroundColor };
    host.remove();
    return out;
  })()`);
  check(
    'подсветка мыслей применяется',
    thought.color === 'rgb(249, 168, 212)' &&
      thought.fontStyle === 'normal' &&
      thought.background === 'rgba(249, 168, 212, 0.12)',
    JSON.stringify(thought),
  );

  const ext = await st.evalJs(
    `(() => { const e = SillyTavern.getContext().extensionSettings; return { caption: e.caption, director: e.cloufam_director, sd: e.sd?.source }; })()`,
  );
  check(
    'засев caption указывает на Ollama',
    ext.caption?.source === 'multimodal' &&
      ext.caption?.multimodal_api === 'ollama' &&
      ext.caption?.multimodal_model === 'ollama_custom' &&
      !!ext.caption?.ollama_custom_model,
    JSON.stringify({
      source: ext.caption?.source,
      api: ext.caption?.multimodal_api,
      model: ext.caption?.multimodal_model,
      vision: ext.caption?.ollama_custom_model,
    }),
  );
  check(
    'ремарки режиссёра идут рассказчиком после миграции',
    ext.director?.delivery === 'system' && ext.director?.delivery_migrated === true,
    JSON.stringify(ext.director),
  );
  const sdRegex = await st.evalJs(
    `fetch('/scripts/extensions/stable-diffusion/index.js').then((r) => r.text()).then((t) => t.includes('(?<![a-zа-яё])(send|mail'))`,
  );
  check('русские триггеры картинок обновлены', sdRegex);
  const gatewayFromPage = await st.evalJs(
    `fetch(window.__LK_GATEWAY + '/api/cloufam/models').then((r) => r.status)`,
  );
  check('страница ST ходит в шлюз', gatewayFromPage === 200, `статус ${gatewayFromPage}`);

  // ── Assistant chat ────────────────────────────────────────────────────────
  const selected = await st.evalJs(`(async () => {
    const ctx = SillyTavern.getContext();
    const index = ctx.characters.findIndex((c) => c.avatar === 'default_Assistant.png');
    if (index < 0) return null;
    await ctx.selectCharacterById(index);
    return index;
  })()`);
  const inChat = await waitFor(
    () =>
      st.evalJs(
        `SillyTavern.getContext().characters[SillyTavern.getContext().characterId]?.avatar === 'default_Assistant.png'`,
      ),
    20000,
  );
  check('открыт чат Assistant', selected !== null && inChat);
  await sleep(2500); // reportActiveCharacter goes to the gateway on CHAT_CHANGED
  const persona = await st.evalJs(`SillyTavern.getContext().name1`);
  const traitsFile = path.join(DATA, 'traits.json');
  const stateOf = () =>
    JSON.stringify(readJson(traitsFile)?.['default_Assistant.png']?.relations?.[persona] ?? null);
  const chatLen = () => st.evalJs(`SillyTavern.getContext().chat.length`);
  const idle = () =>
    waitFor(
      () =>
        st.evalJs(
          `!document.getElementById('mes_stop') || getComputedStyle(document.getElementById('mes_stop')).display === 'none'`,
        ),
      180000,
      1000,
    );

  // a quiet generation must not move live state
  const beforeQuiet = stateOf();
  const quiet = await st.evalJs(
    `SillyTavern.getContext().generateQuietPrompt({ quietPrompt: 'Ответь одним словом: да или нет. Сейчас день?' }).then((t) => String(t).slice(0, 60)).catch((e) => 'ERR ' + e)`,
  );
  await sleep(8000);
  check('quiet-генерация не запускает оценщик', stateOf() === beforeQuiet, `ответ: ${quiet}`);

  // director: enable, a narrator remark, /roll passes to ST, shop without a world — a toast
  // The toggle state survives restarts (extension_settings.json), so
  // first bring it to off, then check the TOGGLE itself: the click
  // must flip the flag (by polling, without reading immediately after the click).
  const directorState = () =>
    st.evalJs(`!!SillyTavern.getContext().extensionSettings.cloufam_director?.enabled`);
  const clickDirector = () =>
    st.evalJs(`document.getElementById('cloufam-director-nav-btn').click(); true`);
  if (await directorState()) {
    await clickDirector();
    await waitFor(async () => !(await directorState()), 10000, 250);
  }
  await clickDirector();
  const directorOn = await waitFor(async () => await directorState(), 10000, 250);
  check('режим режиссёра включается кнопкой', directorOn === true);

  const lenBeforeRoll = await chatLen();
  await st.evalJs(
    `(() => { const a = document.getElementById('send_textarea'); a.value = '/roll 1d6'; a.dispatchEvent(new Event('input', { bubbles: true })); document.getElementById('send_but').click(); return true; })()`,
  );
  await sleep(3000);
  const rollMsg = await st.evalJs(
    `(() => { const c = SillyTavern.getContext().chat; const m = c[c.length - 1]; return { len: c.length, text: m?.mes ?? '', user: !!m?.is_user }; })()`,
  );
  check(
    '/roll при режиме режиссёра исполняется командой',
    rollMsg.len === lenBeforeRoll + 1 && /\d/.test(rollMsg.text) && !rollMsg.text.includes('/roll'),
    rollMsg.text.slice(0, 80),
  );

  const lenBeforeShop = await chatLen();
  await st.evalJs(
    `(() => { const a = document.getElementById('send_textarea'); a.value = '// shop зелья'; a.dispatchEvent(new Event('input', { bubbles: true })); document.getElementById('send_but').click(); return true; })()`,
  );
  await sleep(4000);
  check('shop-команда без мира не пишет в чат', (await chatLen()) === lenBeforeShop);

  const beforeReply = stateOf();
  const lenBeforeRemark = await chatLen();
  await st.evalJs(
    `(() => { const a = document.getElementById('send_textarea'); a.value = 'За окном начинается гроза, свет мигает.'; a.dispatchEvent(new Event('input', { bubbles: true })); document.getElementById('send_but').click(); return true; })()`,
  );
  const remarkPosted = await waitFor(async () => (await chatLen()) > lenBeforeRemark, 120000, 1000);
  const remark = await st.evalJs(
    `(() => { const c = SillyTavern.getContext().chat; const m = c[${lenBeforeRemark}]; return m ? { name: m.name, user: !!m.is_user, system: !!m.is_system, type: m.extra?.type ?? null, mes: m.mes.slice(0, 80) } : null; })()`,
  );
  check(
    'ремарка режиссёра легла рассказчиком',
    remarkPosted && remark && !remark.user && remark.type === 'narrator',
    JSON.stringify(remark),
  );
  const replied = await waitFor(async () => (await chatLen()) > lenBeforeRemark + 1, 180000, 1000);
  await idle();
  const reply = await st.evalJs(
    `(() => { const c = SillyTavern.getContext().chat; const m = c[c.length - 1]; return { name: m?.name, user: !!m?.is_user, len: (m?.mes || '').length }; })()`,
  );
  check(
    'после ремарки ответил персонаж',
    replied && !reply.user && reply.len > 0,
    JSON.stringify(reply),
  );
  const evaluated = await waitFor(() => stateOf() !== beforeReply, 90000, 2000);
  check(
    'оценщик сдвинул живое состояние после реплики',
    evaluated,
    `было ${beforeReply}, стало ${stateOf()}`,
  );

  await clickDirector();
  const directorOff = await waitFor(async () => !(await directorState()), 10000, 250);
  check('режим режиссёра выключается кнопкой', directorOff === true);

  // ── RPG panel ───────────────────────────────────────────────────────────
  await st.evalJs(
    `window.confirm = () => true; document.getElementById('cloufam-rpg-menu-item').click(); true`,
  );
  const rpgOpen = await waitFor(
    () =>
      st.evalJs(
        `!!document.querySelector('#cloufam-rpg-modal [data-lk-action="world-start"], #cloufam-rpg-modal [data-lk-action="world-reset"]')`,
      ),
    20000,
  );
  check('RPG-панель открывается', rpgOpen);
  await st.evalJs(
    `document.querySelector('#cloufam-rpg-modal [data-lk-action="world-start"]')?.click(); true`,
  );
  const worldStarted = await waitFor(
    () =>
      st.evalJs(`!!document.querySelector('#cloufam-rpg-modal [data-lk-action="world-reset"]')`),
    20000,
  );
  const worldAfter = JSON.parse(
    (await rawRequest(gatewayPort, { urlPath: '/api/cloufam/rpg/world' })).body || '{}',
  );
  check(
    'RPG-мир начинается из панели',
    worldStarted && worldAfter.started !== false,
    JSON.stringify(worldAfter).slice(0, 120),
  );

  await st.evalJs(
    `document.querySelector('#cloufam-rpg-modal [data-lk-tab="combat"]').click(); true`,
  );
  const startBtn = await waitFor(
    () =>
      st.evalJs(
        `!!document.querySelector('#cloufam-rpg-modal [data-lk-action="combat-start"]')`,
      ),
    20000,
  );
  check('во вкладке «Бой» есть выбор противника', startBtn);
  await st.evalJs(
    `(() => { const m = document.querySelector('#cloufam-rpg-modal [data-lk-field="mode"]'); if (m) m.value = 'fatal'; document.querySelector('#cloufam-rpg-modal [data-lk-action="combat-start"]').click(); return true; })()`,
  );
  const inCombat = await waitFor(
    () =>
      st.evalJs(`!!document.querySelector('#cloufam-rpg-modal [data-lk-action="combat-act"]')`),
    20000,
  );
  check('бой начинается', inCombat);
  await st.evalJs(
    `(() => { const i = document.querySelector('#cloufam-rpg-modal [data-lk-field="action"]'); i.value = 'Бью мечом по ногам противника'; document.querySelector('#cloufam-rpg-modal [data-lk-action="combat-act"]').click(); return true; })()`,
  );
  const combat = await waitFor(
    async () => {
      const c = JSON.parse(
        (await rawRequest(gatewayPort, { urlPath: '/api/cloufam/rpg/combat' })).body || '{}',
      );
      return c.round >= 1 ? c : null;
    },
    180000,
    2000,
  );
  check(
    'ход в бою: раунд засчитан, противник ответил',
    combat && (combat.player_hp < 100 || combat.over) && (combat.log || []).length >= 1,
    combat
      ? `раунд ${combat.round}, HP игрока ${combat.player_hp}, HP врага ${combat.enemy?.hp}, итог ${combat.result ?? '—'}`
      : 'нет раунда за 180 с',
  );
  const note = await rawRequest(gatewayPort, { urlPath: '/api/cloufam/rpg/combat' });
  check('состояние боя читается снова', note.status === 200);
  await st.evalJs(
    `document.querySelector('#cloufam-rpg-modal [data-lk-action="combat-end"]')?.click(); true`,
  );
  const combatGone = await waitFor(
    () => !fs.existsSync(path.join(DATA, 'active_combat.json')),
    20000,
  );
  check('бой завершается и удаляется', combatGone);
  await st.evalJs(
    `document.querySelector('#cloufam-rpg-modal [data-lk-tab="world"]').click(); true`,
  );
  await sleep(1000);
  await st.evalJs(
    `document.querySelector('#cloufam-rpg-modal [data-lk-action="world-reset"]')?.click(); true`,
  );
  const worldGone = await waitFor(() => !fs.existsSync(path.join(DATA, 'world.json')), 20000);
  check('RPG-мир сбрасывается', worldGone);
  await st.evalJs(`document.querySelector('#cloufam-rpg-modal [data-lk-close]')?.click(); true`);

  // ── settings window ────────────────────────────────────────────────────────
  const stOrigin2 = await st.evalJs(
    `fetch(window.__LK_GATEWAY + '/api/cloufam/open-settings', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ tab: 'tags' }) }).then((r) => r.status)`,
  );
  const win = await attach(
    (url) => url.startsWith('http://tauri.localhost') && url.includes('#settings'),
    30000,
  );
  check('окно настроек открывается из ST', stOrigin2 === 200 && win, `статус ${stOrigin2}`);
  if (win) {
    const tabs = await waitFor(
      () => win.evalJs(`document.querySelectorAll('.tab-btn').length`),
      20000,
    );
    check('в окне настроек есть вкладки', tabs > 0, `${tabs}`);
    const evaluatorOptions = await win.evalJs(
      `[...document.querySelectorAll('select')].flatMap((s) => [...s.options].map((o) => o.value)).filter((v) => /bge|nomic|embed/i.test(v)).length`,
    );
    check(
      'эмбеддеры не предлагаются оценщиком и теггером',
      evaluatorOptions === 0,
      `${evaluatorOptions} найдено`,
    );
    await win
      .evalJs(
        `window.__TAURI_INTERNALS__.invoke('plugin:window|close', { label: 'settings' }).catch(() => window.close()); true`,
      )
      .catch(() => {});
  }
  exitCode = results.every((r) => r.ok) ? 0 : 1;
} catch (error) {
  check('стенд дошёл до конца', false, String(error?.message ?? error));
  exitCode = 1;
} finally {
  if (!KEEP_OPEN) {
    try {
      execSync('taskkill /IM Cloufam.exe', { stdio: 'ignore' });
    } catch {
      // already closed
    }
    const closed = await waitFor(
      () => {
        try {
          return !execSync('tasklist /FI "IMAGENAME eq Cloufam.exe" /NH', {
            encoding: 'utf8',
          }).includes('Cloufam.exe');
        } catch {
          return true;
        }
      },
      30000,
      1000,
    );
    check('приложение закрылось штатно', closed);
    await sleep(3000);
    const orphans = execSync(
      `powershell -NoProfile -Command "Get-CimInstance Win32_Process | Where-Object { $_.ExecutablePath -like '${INSTALL.replace(/'/g, "''")}\\*' } | ForEach-Object { $_.Name + ' ' + $_.ProcessId }"`,
      { encoding: 'utf8' },
    ).trim();
    check(
      'после выхода нет сирот из папки установки',
      orphans === '',
      orphans.replace(/\s+/g, ' '),
    );
    restore();
    console.log('данные трекеров и чат Assistant возвращены из бэкапа');
  }
  const failed = results.filter((r) => !r.ok);
  console.log(`\n=== итог: ${results.length - failed.length}/${results.length} ===`);
  process.exit(failed.length === 0 ? 0 : exitCode || 1);
}

