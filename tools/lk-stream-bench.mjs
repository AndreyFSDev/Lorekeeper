// Synthetic stream benchmark for SillyTavern: how much main-thread time one
// frame of streaming rendering of a long Russian reply costs and who is
// superfluous in it (extension observers, forced layouts).
//
// The server is a separate ST instance from the working installation's code,
// but with its own dataRoot/config/port inside `src-tauri/target/stream-bench`:
// the owner's chats and characters are untouched, and a running app does not interfere.
// Cloufam extensions and the Moonlit theme are taken from the current repository tree.
//
// A frame follows the `StreamingProcessor.onProgressStreaming` path (ST 1.19):
// cleanUpMessage → markup balancing → messageFormatting → innerHTML →
// scrollChatToBottom, with streaming_fps = 30.
//
// Usage:
//   LOREKEEPER_INSTALL=Z:\AImodels\Cloufam node tools/lk-stream-bench.mjs --label before
//   ... --director-on          director mode enabled (writes a placeholder)
//   ... --tail                 script.js with the tail-render patch (lines from sillytavern.rs)
//   ... --validate --no-trace  compare the DOM against the stock render after every frame
//   ... --edge-cases           text from markup that showdown glues with a blank line
//   ... --e2e [--tail]         a real generation through a stub model; the resulting DOM
//                              is written to e2e-<label>.html to compare stock and patched
//   ... --invalidations        reasons for restyling (heavy trace, small --chars only)
//   ... --timer-restyle        cost of writing the message timer: textContent vs node edit
//   ... --startup 3            ST server start only: without and with the compile cache
// Results — src-tauri/target/stream-bench/results-<label>.json.
import { spawn, spawnSync } from 'node:child_process';
import { cpSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { loadWs } from './ws-resolve.mjs';

const ROOT = path.resolve(import.meta.dirname, '..');
const ws = loadWs(ROOT);

const args = process.argv.slice(2);
const flag = (name) => args.includes(name);
const option = (name, fallback) => {
  const i = args.indexOf(name);
  return i >= 0 && args[i + 1] !== undefined ? args[i + 1] : fallback;
};

const INSTALL = process.env.LOREKEEPER_INSTALL;
if (!INSTALL) throw new Error('задайте LOREKEEPER_INSTALL (папка рабочей установки)');
const ST_CODE = path.join(INSTALL, 'sillytavern');
const NODE =
  [path.join(INSTALL, 'runtime', 'node', 'node.exe')].find((p) => existsSync(p)) ?? 'node';
const BENCH = path.join(ROOT, 'src-tauri', 'target', 'stream-bench');
const DATA = path.join(BENCH, 'st-data');
const USER_DIR = path.join(DATA, 'default-user');
const ST_PORT = Number(option('--port', 8130));
const CDP_PORT = Number(option('--cdp-port', 9230));
const ST_URL = `http://127.0.0.1:${ST_PORT}`;
const LABEL = option('--label', 'run');
const REPLY_CHARS = Number(option('--chars', 11000));
const CHARS_PER_FRAME = Number(option('--step', 8));
const FRAME_MS = Number(option('--interval', 33));
const HISTORY = Number(option('--history', 20));
const CHARACTER = process.env.LK_CHARACTER ?? 'default_Seraphina.png';
const EDGE = [
  'C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe',
  'C:\\Program Files\\Microsoft\\Edge\\Application\\msedge.exe',
].find((p) => existsSync(p));

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ── Data of the isolated ST ────────────────────────────────────────────────

function deployExtension(name, files) {
  const dir = path.join(USER_DIR, 'extensions', name);
  mkdirSync(dir, { recursive: true });
  for (const [from, to] of files) cpSync(path.join(ROOT, 'src-tauri', from), path.join(dir, to));
}

function prepareData() {
  rmSync(DATA, { recursive: true, force: true });
  mkdirSync(path.join(USER_DIR, 'characters'), { recursive: true });
  const live = path.join(ST_CODE, 'data', 'default-user');
  // A copy, not a link: the ST client saves settings when the page loads.
  const settings = JSON.parse(readFileSync(path.join(live, 'settings.json'), 'utf8'));
  settings.active_character = CHARACTER;
  if (flag('--director-on')) {
    settings.extension_settings.cloufam_director = {
      ...(settings.extension_settings.cloufam_director ?? {}),
      enabled: true,
    };
  }
  writeFileSync(path.join(USER_DIR, 'settings.json'), JSON.stringify(settings, null, 4));
  cpSync(path.join(live, 'themes'), path.join(USER_DIR, 'themes'), { recursive: true });
  cpSync(path.join(ROOT, 'characters', CHARACTER), path.join(USER_DIR, 'characters', CHARACTER));

  const ext = (dir) => [
    [`${dir}/extension-manifest.json`, 'manifest.json'],
    [`${dir}/extension-index.js`, 'index.js'],
    [`${dir}/extension-style.css`, 'style.css'],
  ];
  deployExtension('cloufam-director', ext('director'));
  deployExtension('cloufam-tagger', ext('tagger'));
  cpSync(
    path.join(ROOT, 'src-tauri', 'moonlit'),
    path.join(USER_DIR, 'extensions', 'SillyTavern-MoonlitEchoesTheme'),
    { recursive: true },
  );

  // Cloufam server plugins call a gateway that does not exist here.
  const config = readFileSync(path.join(ST_CODE, 'config.yaml'), 'utf8').replace(
    /^enableServerPlugins:.*$/m,
    'enableServerPlugins: false',
  );
  writeFileSync(path.join(BENCH, 'config.yaml'), config);
}

async function startSt({ compileCache } = {}) {
  const env = { ...process.env, HOME: BENCH, USERPROFILE: BENCH };
  delete env.NODE_COMPILE_CACHE;
  if (compileCache) env.NODE_COMPILE_CACHE = compileCache;
  const started = performance.now();
  const child = spawn(
    NODE,
    [
      'server.js',
      '--configPath',
      path.join(BENCH, 'config.yaml'),
      '--dataRoot',
      DATA,
      '--port',
      String(ST_PORT),
      '--listen',
      'false',
      '--browserLaunchEnabled',
      'false',
    ],
    { cwd: ST_CODE, env, stdio: ['ignore', 'pipe', 'pipe'] },
  );
  const log = [];
  child.stdout.on('data', (d) => log.push(String(d)));
  child.stderr.on('data', (d) => log.push(String(d)));
  for (let i = 0; i < 1200; i++) {
    if (child.exitCode !== null) throw new Error(`ST завершилась: ${log.join('').slice(-2000)}`);
    try {
      const res = await fetch(`${ST_URL}/`);
      if (res.ok) return { child, ms: performance.now() - started, log };
    } catch {
      // the server does not listen on the port yet
    }
    await sleep(50);
  }
  child.kill();
  throw new Error('ST не ответила за 60 секунд');
}

async function stopSt(child) {
  if (child.exitCode !== null) return;
  child.kill();
  for (let i = 0; i < 100 && child.exitCode === null; i++) await sleep(50);
}

// ── CDP ─────────────────────────────────────────────────────────────────────

class Cdp {
  constructor(socket) {
    this.socket = socket;
    this.seq = 0;
    this.pending = new Map();
    this.listeners = new Map();
    socket.on('message', (raw) => {
      const msg = JSON.parse(raw.toString());
      if (msg.method) {
        for (const fn of this.listeners.get(msg.method) ?? []) fn(msg.params);
        return;
      }
      const entry = this.pending.get(msg.id);
      if (!entry) return;
      this.pending.delete(msg.id);
      if (msg.error)
        entry.reject(new Error(`${msg.error.message} (${JSON.stringify(msg.error.data ?? {})})`));
      else entry.resolve(msg.result);
    });
  }

  on(method, fn) {
    if (!this.listeners.has(method)) this.listeners.set(method, []);
    this.listeners.get(method).push(fn);
  }

  once(method) {
    return new Promise((resolve) => {
      const fn = (params) => {
        this.listeners.set(
          method,
          (this.listeners.get(method) ?? []).filter((f) => f !== fn),
        );
        resolve(params);
      };
      this.on(method, fn);
    });
  }

  send(method, params = {}, timeoutMs = 60000) {
    const id = ++this.seq;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.socket.send(JSON.stringify({ id, method, params }));
      // unref: an armed reply timer would keep the process alive until its deadline.
      setTimeout(() => {
        if (this.pending.delete(id)) reject(new Error(`CDP ${method}: таймаут`));
      }, timeoutMs).unref();
    });
  }

  async evaluate(expression, timeoutMs = 60000) {
    const res = await this.send(
      'Runtime.evaluate',
      { expression, awaitPromise: true, returnByValue: true },
      timeoutMs,
    );
    if (res.exceptionDetails) {
      const text = res.exceptionDetails.exception?.description ?? res.exceptionDetails.text;
      throw new Error(`ошибка в странице: ${text}`);
    }
    return res.result.value;
  }
}

async function openEdge() {
  const profile = path.join(BENCH, 'edge-profile');
  rmSync(profile, { recursive: true, force: true });
  mkdirSync(profile, { recursive: true });
  const edge = spawn(
    EDGE,
    [
      '--headless=new',
      `--remote-debugging-port=${CDP_PORT}`,
      `--user-data-dir=${profile}`,
      '--no-first-run',
      '--no-default-browser-check',
      '--disable-extensions',
      '--window-size=1440,900',
      'about:blank',
    ],
    { stdio: 'ignore' },
  );
  for (let i = 0; i < 60; i++) {
    try {
      const targets = await (await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`)).json();
      const page = targets.find((t) => t.type === 'page');
      if (page) {
        const socket = new ws(page.webSocketDebuggerUrl, { maxPayload: 512 * 1024 * 1024 });
        await new Promise((resolve, reject) => {
          socket.once('open', resolve);
          socket.once('error', reject);
        });
        return { edge, cdp: new Cdp(socket), socket };
      }
    } catch {
      // the debug port is not up yet
    }
    await sleep(500);
  }
  throw new Error('Edge не поднял порт отладки');
}

function closeEdge(edge) {
  // Only our Edge's tree: it has child renderer and GPU processes.
  spawnSync('taskkill', ['/PID', String(edge.pid), '/T', '/F'], { stdio: 'ignore' });
}

// Injected into the page before its scripts: otherwise the extension observers created
// on load would stay uncounted.
const INSTRUMENTATION = `(() => {
    const site = (stack) => (stack || '').split('\\n').slice(1).map((l) => l.trim())
        .find((l) => !l.includes('lk-instr.js')) || '?';
    const mo = [];
    const Native = window.MutationObserver;
    window.MutationObserver = class extends Native {
        constructor(callback) {
            const stat = { site: site(new Error().stack), targets: [], calls: 0, records: 0, ms: 0 };
            mo.push(stat);
            super(function (records, observer) {
                stat.calls++;
                stat.records += records.length;
                const t = performance.now();
                try {
                    return callback.call(this, records, observer);
                } finally {
                    stat.ms += performance.now() - t;
                }
            });
            this.__lkStat = stat;
        }
        observe(target, options) {
            const name = target === document.body ? 'body' : target.id ? '#' + target.id : target.nodeName;
            this.__lkStat.targets.push(name + ' ' + JSON.stringify(options));
            return super.observe(target, options);
        }
    };
    const rect = { calls: 0, ms: 0, slow: {} };
    const nativeRect = Element.prototype.getBoundingClientRect;
    Element.prototype.getBoundingClientRect = function () {
        const t = performance.now();
        const result = nativeRect.call(this);
        const d = performance.now() - t;
        rect.calls++;
        rect.ms += d;
        // Longer than 0.2 ms is already a layout recalculation, not reading a ready value.
        if (d > 0.2) {
            const key = site(new Error().stack) + ' @' + (this.id ? '#' + this.id : this.nodeName);
            const slot = rect.slow[key] || (rect.slow[key] = { n: 0, ms: 0 });
            slot.n++;
            slot.ms += d;
        }
        return result;
    };
    window.__lkInstr = {
        mo,
        rect,
        reset() {
            for (const s of mo) { s.calls = 0; s.records = 0; s.ms = 0; }
            rect.calls = 0; rect.ms = 0; rect.slow = {};
        },
    };
})();
//# sourceURL=lk-instr.js`;

// ── Reply text ────────────────────────────────────────────────────────────

const SENTENCES = [
  'Фубуки поправляет прядь белых волос и оглядывает поляну, на которой ещё тлеют угли вчерашнего костра.',
  'Ветер приносит запах мокрой хвои, а где-то за холмом глухо перекликаются вороны.',
  '"Ну что, пойдём дальше или ты опять собрался спорить с картой?" — спрашивает она, прищурившись.',
  '*Она легко перепрыгивает через поваленное бревно и оборачивается, дожидаясь тебя.*',
  'Тропа уходит вниз, к реке, и по камням видно, что недавно здесь прошёл кто-то тяжёлый.',
  '"Следы свежие, — замечает Фубуки, присев на корточки. — **Час, может, два**, не больше."',
  'Солнце пробивается сквозь листву пятнами, и тени ложатся на землю неровной сеткой.',
  '*Лисий хвост нервно подрагивает: она явно что-то слышит, чего не слышишь ты.*',
  'В кармане шуршит сложенная вчетверо записка, которую вам передал староста деревни.',
  '"Если это опять кабан, я беру его на себя, а ты держи фонарь и не геройствуй", — смеётся она.',
];
const LIST_ITEMS = [
  '**Верёвка** — метров двадцать, крепкая, но уже потёртая на концах',
  '**Фонарь** — масла осталось на пару часов, не больше',
  '**Карта** — старая, с пометками чужой рукой на полях',
  '*Сушёные яблоки* — на случай, если поход затянется',
  '**Нож** — затуплен, но для хвороста сойдёт',
];

// Markup that showdown glues with a blank line or that ST
// completes across the whole text: on it the tail render must match the stock one.
const EDGE_CASES = [
  '"Первая часть длинной реплики, которая не закрывается в этом абзаце.\n\nИ продолжение реплики во втором абзаце."',
  '**Жирный текст, который тянется\n\nчерез пустую строку**, и дальше.',
  '<div class="status">\n\nСтатус сцены: **ночь**, туман.\n\n</div>',
  '<details><summary>Секрет</summary>\n\nСпрятанный текст.\n\n</details>',
  '<style>\n.mes_text .x { color: red; }\n\n.mes_text .y { color: blue; }\n</style>',
  '- пункт первый\n\n- пункт второй через пустую строку\n\n- третий',
  '1. раз\n\n2. два\n\n3. три',
  '    отступ — блок кода\n\n    вторая строка кода после пустой',
  '> цитата первая\n\n> цитата вторая',
  'Слово_с_подчёркиванием и _курсив_ через _подчёркивание\n\nв двух абзацах_.',
  '~~~\nблок тильдами\n\nс пустой строкой внутри\n~~~',
  '# Заголовок\n\nАбзац после заголовка.\n\n---\n\nАбзац после черты.',
  '| a | b |\n|---|---|\n| 1 | 2 |',
  '<!-- комментарий\n\nна два абзаца -->',
  'Строка с <b>незакрытым тегом\n\nи продолжением</b> после.',
  '*Действие, начатое в одном абзаце\n\nи законченное в другом.*',
];

function buildEdgeCases(target) {
  const blocks = [];
  for (let i = 0; blocks.join('\n\n').length < target; i++) {
    blocks.push(EDGE_CASES[i % EDGE_CASES.length], SENTENCES[i % SENTENCES.length]);
  }
  return blocks.join('\n\n');
}

function buildReply(target, seed = 0) {
  const blocks = [];
  let length = 0;
  let n = seed;
  let fenced = false;
  while (length < target) {
    let block;
    if (n % 7 === 3) {
      block = LIST_ITEMS.map((item) => `- ${item}`).join('\n');
    } else if (n % 7 === 5) {
      block = LIST_ITEMS.slice(0, 3)
        .map((item, i) => `${i + 1}. ${item}`)
        .join('\n');
    } else if (n % 11 === 6 && !fenced) {
      fenced = true;
      block =
        '```\nЗаписка старосты:\n  — у старой мельницы не ночевать;\n  — к реке идти до заката;\n  — если увидите огни, возвращайтесь.\n```';
    } else if (n % 13 === 9) {
      block = `> ${SENTENCES[n % SENTENCES.length]}`;
    } else {
      block = [0, 1, 2, 3].map((k) => SENTENCES[(n * 3 + k) % SENTENCES.length]).join(' ');
    }
    blocks.push(block);
    length += block.length + 2;
    n++;
  }
  return blocks.join('\n\n');
}

// ── In-page benchmark ──────────────────────────────────────────────────────

const PAGE_SETUP = (history) => `(async () => {
    const s = await import('/script.js');
    const ctx = SillyTavern.getContext();
    const id = ctx.characters.findIndex((c) => c.avatar === ${JSON.stringify(CHARACTER)});
    if (id < 0) throw new Error('нет персонажа для замера');
    await s.selectCharacterById(id);
    for (let i = 0; i < 100 && s.chat.length === 0; i++) await new Promise((r) => setTimeout(r, 100));
    const name = ctx.characters[id].name;
    const history = ${JSON.stringify(history)};
    history.forEach((mes, i) => {
        const message = { name: i % 2 ? name : ctx.name1, is_user: i % 2 === 0, is_system: false,
            send_date: new Date().toISOString(), mes, extra: {} };
        s.chat.push(message);
        s.addOneMessage(message, { scroll: false });
    });
    const target = { name, is_user: false, is_system: false, send_date: new Date().toISOString(),
        mes: '', extra: {}, swipes: [''], swipe_id: 0 };
    s.chat.push(target);
    s.addOneMessage(target);
    return { messages: document.querySelectorAll('#chat .mes').length, name };
})()
//# sourceURL=lk-bench-setup.js`;

// tail — a frame through Cloufam's tail render (script.js patch) instead of
// the full innerHTML; validate — compare the DOM against the stock render after every frame.
const PAGE_RUN = (text, step, interval, { tail, validate }) => `(async () => {
    const s = await import('/script.js');
    const u = await import('/scripts/utils.js');
    const tailRender = ${tail} ? (await import('/scripts/lk-tail-render.js')).renderStreamingText : null;
    const mesId = s.chat.length - 1;
    const message = s.chat[mesId];
    const block = document.querySelector('#chat .mes[mesid="' + mesId + '"]');
    const mesText = block.querySelector('.mes_text');
    const timer = block.querySelector('.mes_timer');
    const text = ${JSON.stringify(text)};
    const format = (t) => s.messageFormatting(t, message.name, false, false, mesId, {}, false);
    const frames = [];
    const mismatches = [];
    let strictMismatches = 0;
    window.__lkInstr.reset();
    const started = performance.now();
    for (let i = 0, n = ${step}; ; n += ${step}, i++) {
        const isFinal = n >= text.length;
        performance.mark('lkf:' + i);
        const t0 = performance.now();
        let processed = s.cleanUpMessage({ getMessage: text.slice(0, n), isImpersonate: false,
            isContinue: false, displayIncompleteSentences: !isFinal, stoppingStrings: null });
        for (const char of ['*', '"', '\`\`\`', '~~~']) {
            if (!isFinal && u.isOdd(u.countOccurrences(processed, char))) {
                processed = processed.trimEnd() + (char.length > 1 ? '\\n' : '') + char;
            }
        }
        message.mes = processed;
        const t1 = performance.now();
        let t2;
        if (tailRender) {
            tailRender(mesText, processed, format, isFinal);
            t2 = performance.now();
        } else {
            const html = format(processed);
            t2 = performance.now();
            mesText.innerHTML = html;
        }
        const t3 = performance.now();
        if (${validate}) {
            const stock = document.createElement('div');
            stock.innerHTML = format(processed);
            // Whitespace nodes between top-level blocks (showdown puts either
            // \\n or \\n\\n) are not painted under white-space: normal — compare without them.
            // showdown numbers heading ids across the whole document (repeat → "-1"),
            // while the tail is a separate document; it does not affect the look.
            const blocksOnly = (root) => {
                for (const node of [...root.childNodes]) {
                    if (node.nodeType === Node.TEXT_NODE && !node.data.trim()) node.remove();
                }
                for (const heading of root.querySelectorAll('h1, h2, h3, h4, h5, h6')) heading.removeAttribute('id');
                return root.innerHTML;
            };
            if (mesText.innerHTML !== stock.innerHTML) strictMismatches++;
            const got = blocksOnly(mesText.cloneNode(true));
            const want = blocksOnly(stock);
            if (got !== want) {
                let at = 0;
                while (at < got.length && got[at] === want[at]) at++;
                mismatches.push({ frame: i, length: processed.length, isFinal, at,
                    got: got.slice(Math.max(0, at - 80), at + 120), want: want.slice(Math.max(0, at - 80), at + 120) });
            }
        }
        if (timer) timer.textContent = ((t3 - started) / 1000).toFixed(1) + 's';
        s.scrollChatToBottom({ waitForFrame: true });
        const t4 = performance.now();
        await null;
        const t5 = performance.now();
        frames.push([processed.length, t1 - t0, t2 - t1, t3 - t2, t4 - t3, t5 - t4]);
        if (isFinal) break;
        await new Promise((r) => setTimeout(r, ${interval}));
    }
    // The tail after the last frame: deferred timers and painting.
    await new Promise((r) => setTimeout(r, 300));
    performance.mark('lkf:end');
    const instr = window.__lkInstr;
    return {
        frames,
        mismatches,
        strictMismatches,
        mo: instr.mo.map((m) => ({ ...m })),
        rect: { calls: instr.rect.calls, ms: instr.rect.ms, slow: instr.rect.slow },
        htmlLength: mesText.innerHTML.length,
        nodes: mesText.querySelectorAll('*').length,
    };
})()
//# sourceURL=lk-bench-run.js`;

// ── End-to-end run: a real generation through a stub model ─────────────────

const FAKE_LLM_PORT = Number(option('--llm-port', 8131));

/// OpenAI-compatible stream: the reply is cut into CHARS_PER_FRAME characters every
/// 16 ms — the stream goes through the ST server and the real onProgressStreaming.
function startFakeLlm(reply) {
  const server = http.createServer((req, res) => {
    req.resume();
    if (!req.url.endsWith('/chat/completions')) {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ data: [{ id: 'bench' }] }));
      return;
    }
    res.writeHead(200, { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-cache' });
    let at = 0;
    const timer = setInterval(() => {
      if (at >= reply.length) {
        clearInterval(timer);
        res.end('data: [DONE]\n\n');
        return;
      }
      const content = reply.slice(at, at + CHARS_PER_FRAME);
      at += CHARS_PER_FRAME;
      res.write(
        `data: ${JSON.stringify({ choices: [{ index: 0, delta: { content }, finish_reason: null }] })}\n\n`,
      );
    }, 16);
  });
  return new Promise((resolve) => server.listen(FAKE_LLM_PORT, '127.0.0.1', () => resolve(server)));
}

const PAGE_E2E = `(async () => {
    const s = await import('/script.js');
    const oai = await import('/scripts/openai.js');
    oai.oai_settings.chat_completion_source = 'custom';
    oai.oai_settings.custom_url = 'http://127.0.0.1:${FAKE_LLM_PORT}/v1';
    oai.oai_settings.custom_model = 'bench';
    oai.oai_settings.stream_openai = true;
    s.setOnlineStatus('bench');
    const before = s.chat.length;
    await s.Generate('normal');
    for (let i = 0; i < 600 && (s.streamingProcessor || s.chat.length === before); i++) {
        await new Promise((r) => setTimeout(r, 100));
    }
    const mesId = s.chat.length - 1;
    const message = s.chat[mesId];
    const mesText = document.querySelector('#chat .mes[mesid="' + mesId + '"] .mes_text');
    const stock = document.createElement('div');
    stock.innerHTML = s.messageFormatting(message.mes, message.name, false, false, mesId, {}, false);
    return {
        messages: s.chat.length - before,
        length: message.mes.length,
        finalMatchesStock: mesText.innerHTML === stock.innerHTML,
        finalHtml: mesText.innerHTML,
        tailModuleLoaded: performance.getEntriesByType('resource').some((e) => e.name.endsWith('/scripts/lk-tail-render.js')),
    };
})()
//# sourceURL=lk-bench-e2e.js`;

// ── Trace ──────────────────────────────────────────────────────────────────

/// The trace is parsed as a stream, one event at a time: it does not fit into a
/// string whole (V8's string length limit is ~512 MB, a trace with invalidations is bigger).
async function readTrace(cdp, stream) {
  const events = [];
  let pending = '';
  let depth = 0;
  let inString = false;
  let escaped = false;
  let start = -1;
  // {"traceEvents":[...]} — events at depth 2, a bare array — at depth 1.
  let eventDepth = 0;
  for (;;) {
    const chunk = await cdp.send('IO.read', { handle: stream, size: 8 * 1024 * 1024 });
    const text =
      pending +
      (chunk.base64Encoded ? Buffer.from(chunk.data, 'base64').toString('utf8') : chunk.data);
    for (let i = pending.length; i < text.length; i++) {
      const c = text[i];
      if (eventDepth === 0) {
        if (c === '{') eventDepth = 2;
        else if (c === '[') eventDepth = 1;
        else continue;
      }
      if (inString) {
        if (escaped) escaped = false;
        else if (c === '\\') escaped = true;
        else if (c === '"') inString = false;
        continue;
      }
      if (c === '"') inString = true;
      else if (c === '{') {
        if (++depth === eventDepth) start = i;
      } else if (c === '}') {
        if (depth-- === eventDepth) {
          events.push(JSON.parse(text.slice(start, i + 1)));
          start = -1;
        }
      }
    }
    pending = start >= 0 ? text.slice(start) : '';
    if (start >= 0) start = 0;
    if (chunk.eof) break;
  }
  await cdp.send('IO.close', { handle: stream });
  return events;
}

const SCRIPT_EVENTS = new Set([
  'FunctionCall',
  'TimerFire',
  'FireAnimationFrame',
  'EvaluateScript',
  'EventDispatch',
  'v8.callFunction',
]);

/// Bucket the page's main-thread trace into frames between the `lkf:N` marks.
function analyzeTrace(events) {
  const marks = events.filter(
    (e) =>
      typeof e.name === 'string' &&
      e.name.startsWith('lkf:') &&
      e.cat?.includes('blink.user_timing'),
  );
  if (marks.length === 0) throw new Error('в трассе нет отметок кадров');
  const { pid, tid } = marks[0];
  const own = events.filter((e) => e.pid === pid && e.tid === tid);
  const bounds = marks
    .filter((e) => e.pid === pid && e.tid === tid)
    .map((e) => ({ name: e.name, ts: e.ts }))
    .sort((a, b) => a.ts - b.ts);

  // Fold B/E pairs into intervals, take X as is.
  const spans = [];
  const open = new Map();
  for (const e of own.sort((a, b) => a.ts - b.ts)) {
    if (e.ph === 'X')
      spans.push({ name: e.name, cat: e.cat, ts: e.ts, end: e.ts + (e.dur ?? 0), args: e.args });
    else if (e.ph === 'B') {
      const stack = open.get(e.name) ?? [];
      stack.push(e);
      open.set(e.name, stack);
    } else if (e.ph === 'E') {
      const begin = open.get(e.name)?.pop();
      if (begin)
        spans.push({
          name: e.name,
          cat: begin.cat,
          ts: begin.ts,
          end: e.ts,
          args: { ...begin.args, ...e.args },
        });
    }
  }
  // Tasks are nested (ThreadControllerImpl::RunTask around RunTask): take the
  // union of intervals, otherwise main-thread time is counted twice.
  const tasks = [];
  for (const s of spans.filter((x) => x.cat?.includes('toplevel')).sort((a, b) => a.ts - b.ts)) {
    const last = tasks.at(-1);
    if (last && s.ts <= last.end) last.end = Math.max(last.end, s.end);
    else tasks.push({ ts: s.ts, end: s.end });
  }
  const layouts = spans.filter((s) => s.name === 'Layout');
  const styles = spans.filter((s) => s.name === 'UpdateLayoutTree');
  const scripts = spans.filter((s) => SCRIPT_EVENTS.has(s.name));

  const sourceOf = (layout) => {
    const frame = layout.args?.beginData?.stackTrace?.[0];
    if (frame) return `${path.basename(frame.url || '?')}:${frame.functionName || '?'}`;
    const holder = scripts
      .filter((s) => s.ts <= layout.ts && s.end >= layout.end)
      .sort((a, b) => b.ts - a.ts)[0];
    if (!holder) return null;
    const data = holder.args?.data ?? {};
    return `${holder.name}:${path.basename(data.url || '')}:${data.functionName || ''}`;
  };

  const frames = [];
  for (let i = 0; i + 1 < bounds.length; i++) {
    const from = bounds[i].ts;
    const to = bounds[i + 1].ts;
    const clip = (s) => Math.max(0, Math.min(s.end, to) - Math.max(s.ts, from));
    const inside = (s) => s.ts >= from && s.ts < to;
    const frameLayouts = layouts.filter(inside);
    const forced = [];
    for (const layout of frameLayouts) {
      const source = sourceOf(layout);
      if (source) forced.push({ source, ms: (layout.end - layout.ts) / 1000 });
    }
    // Durations by event name (nested ones are counted in the parent too):
    // shows where a frame goes — style, layout, paint, scripts.
    const byName = {};
    for (const s of spans) {
      if (!inside(s) || s.cat?.includes('toplevel')) continue;
      byName[s.name] = (byName[s.name] ?? 0) + (s.end - s.ts) / 1000;
    }
    const frameStyles = styles.filter(inside);
    const invalidations = {};
    for (const e of own) {
      if (!e.cat?.includes('invalidationTracking') || e.ts < from || e.ts >= to) continue;
      const d = e.args?.data ?? {};
      const what = d.changedClass
        ? `.${d.changedClass}`
        : d.changedId
          ? `#${d.changedId}`
          : d.changedAttribute
            ? `[${d.changedAttribute}]`
            : d.changedPseudo
              ? `:${d.changedPseudo}`
              : '';
      const key = `${e.name} ${d.reason ?? ''} ${what} @${d.nodeName ?? ''}${d.subtree ? ' subtree' : ''}`;
      invalidations[key] = (invalidations[key] ?? 0) + 1;
    }
    frames.push({
      invalidations,
      mainMs: tasks.reduce((sum, t) => sum + clip(t), 0) / 1000,
      layouts: frameLayouts.length,
      layoutMs: frameLayouts.reduce((sum, l) => sum + (l.end - l.ts), 0) / 1000,
      styles: frameStyles.length,
      styleMs: frameStyles.reduce((sum, l) => sum + (l.end - l.ts), 0) / 1000,
      styleElements: frameStyles.reduce(
        (sum, l) => sum + (l.args?.elementCount ?? l.args?.endData?.elementCount ?? 0),
        0,
      ),
      forced,
      byName,
    });
  }
  return frames;
}

// ── Summary ────────────────────────────────────────────────────────────────

function stats(values) {
  if (values.length === 0) return null;
  const sorted = [...values].sort((a, b) => a - b);
  const at = (q) => sorted[Math.min(sorted.length - 1, Math.floor(q * sorted.length))];
  const round = (v) => Math.round(v * 100) / 100;
  return {
    n: values.length,
    mean: round(values.reduce((a, b) => a + b, 0) / values.length),
    p50: round(at(0.5)),
    p95: round(at(0.95)),
    max: round(sorted[sorted.length - 1]),
  };
}

function summarize(page, traceFrames) {
  const frames = page.frames.map(([len, clean, format, inner, rest, micro]) => ({
    len,
    clean,
    format,
    inner,
    rest,
    micro,
  }));
  const finalLength = frames.at(-1)?.len ?? 0;
  const tail = (list) => list.filter((_, i) => frames[i] && frames[i].len >= (finalLength * 2) / 3);
  const sync = frames.map((f) => f.clean + f.format + f.inner + f.rest);
  const summary = {
    frames: frames.length,
    finalChars: finalLength,
    htmlLength: page.htmlLength,
    nodes: page.nodes,
    syncMs: stats(sync),
    syncTailMs: stats(tail(sync)),
    formatTailMs: stats(tail(frames.map((f) => f.format))),
    innerHtmlTailMs: stats(tail(frames.map((f) => f.inner))),
    renderTailMs: stats(tail(frames.map((f) => f.format + f.inner))),
    mismatches: page.mismatches.length,
    strictMismatches: page.strictMismatches,
    mismatchSamples: page.mismatches.slice(0, 5),
    microtasksMs: stats(frames.map((f) => f.micro)),
    observers: page.mo
      .filter((m) => m.calls > 0)
      .map((m) => ({
        site: m.site,
        targets: m.targets,
        calls: m.calls,
        callsPerFrame: Math.round((m.calls / frames.length) * 100) / 100,
        ms: Math.round(m.ms * 100) / 100,
      }))
      .sort((a, b) => b.calls - a.calls),
    getBoundingClientRect: {
      calls: page.rect.calls,
      ms: Math.round(page.rect.ms * 100) / 100,
      slow: page.rect.slow,
    },
  };
  if (traceFrames) {
    const main = traceFrames.map((f) => f.mainMs);
    const forcedBySource = {};
    for (const f of traceFrames) {
      for (const { source, ms } of f.forced) {
        forcedBySource[source] ??= { n: 0, ms: 0 };
        const slot = forcedBySource[source];
        slot.n++;
        slot.ms = Math.round((slot.ms + ms) * 100) / 100;
      }
    }
    const tailFrames = tail(traceFrames);
    const byName = {};
    for (const f of tailFrames) {
      for (const [name, ms] of Object.entries(f.byName)) byName[name] = (byName[name] ?? 0) + ms;
    }
    const tailByNameMs = Object.fromEntries(
      Object.entries(byName)
        .map(([name, ms]) => [name, Math.round((ms / tailFrames.length) * 100) / 100])
        .sort((a, b) => b[1] - a[1])
        .slice(0, 15),
    );
    const invalidations = {};
    for (const f of tailFrames) {
      for (const [key, n] of Object.entries(f.invalidations))
        invalidations[key] = (invalidations[key] ?? 0) + n;
    }
    if (Object.keys(invalidations).length > 0) {
      summary.tailInvalidationsPerFrame = Object.fromEntries(
        Object.entries(invalidations)
          .sort((a, b) => b[1] - a[1])
          .slice(0, 25)
          .map(([key, n]) => [key, Math.round((n / tailFrames.length) * 100) / 100]),
      );
    }
    Object.assign(summary, {
      mainThreadMs: stats(main),
      mainThreadTailMs: stats(tail(main)),
      tailByNameMs,
      layoutsPerFrame: stats(traceFrames.map((f) => f.layouts)),
      layoutTailMs: stats(tail(traceFrames.map((f) => f.layoutMs))),
      styleTailMs: stats(tail(traceFrames.map((f) => f.styleMs))),
      styleElementsTail: stats(tail(traceFrames.map((f) => f.styleElements))),
      forcedLayoutsPerFrame: stats(traceFrames.map((f) => f.forced.length)),
      forcedBySource,
    });
  }
  return summary;
}

// ── Modes ──────────────────────────────────────────────────────────────────

async function benchStartup(runs) {
  const cache = path.join(BENCH, 'node-compile-cache');
  const result = { cold: [], warm: [] };
  // The first start on a fresh dataRoot builds the frontend libraries' webpack cache —
  // for a live installation it was built long ago and does not go into the benchmark.
  await stopSt((await startSt()).child);
  for (const [mode, withCache] of [
    ['cold', false],
    ['warm', true],
  ]) {
    if (withCache) {
      rmSync(cache, { recursive: true, force: true });
      // The first start fills the cache, it does not go into the benchmark.
      const { child } = await startSt({ compileCache: cache });
      await stopSt(child);
    }
    for (let i = 0; i < runs; i++) {
      const { child, ms } = await startSt({ compileCache: withCache ? cache : undefined });
      result[mode].push(Math.round(ms));
      await stopSt(child);
    }
  }
  console.log(JSON.stringify(result));
  writeFileSync(path.join(BENCH, `startup-${LABEL}.json`), JSON.stringify(result, null, 2));
}

/// script.js with the tail-render patch — the same lines that
/// `sillytavern::patch_streaming_tail` lays down, so the benchmark runs the code that will
/// ship to the user, not a copy of it.
function patchedStScript() {
  const rust = readFileSync(path.join(ROOT, 'src-tauri', 'src', 'sillytavern.rs'), 'utf8');
  const constant = (name) => {
    const match = rust.match(new RegExp(`const ${name}: &str = r#"([\\s\\S]*?)"#;`));
    if (!match) throw new Error(`в sillytavern.rs нет ${name}`);
    return match[1];
  };
  let script = readFileSync(path.join(ST_CODE, 'public', 'script.js'), 'utf8');
  for (const part of ['IMPORT', 'RENDER']) {
    const find = constant(`STREAM_TAIL_${part}_FIND`);
    if (script.split(find).length !== 2)
      throw new Error(`место патча ${part} в script.js не найдено`);
    script = script.replace(find, () => constant(`STREAM_TAIL_${part}_REPLACE`));
  }
  return script;
}

async function servePatchedSt(cdp) {
  const bodies = {
    '/script.js': patchedStScript(),
    '/scripts/lk-tail-render.js': readFileSync(
      path.join(ROOT, 'src-tauri', 'streaming', 'tail-render.js'),
      'utf8',
    ),
  };
  cdp.on('Fetch.requestPaused', ({ requestId, request }) => {
    cdp.send('Fetch.fulfillRequest', {
      requestId,
      responseCode: 200,
      responseHeaders: [{ name: 'Content-Type', value: 'text/javascript; charset=utf-8' }],
      body: Buffer.from(bodies[new URL(request.url).pathname], 'utf8').toString('base64'),
    });
  });
  await cdp.send('Fetch.enable', {
    patterns: Object.keys(bodies).map((p) => ({ urlPattern: `${ST_URL}${p}*` })),
  });
}

async function benchStreaming() {
  const { child } = await startSt();
  const { edge, cdp, socket } = await openEdge();
  try {
    await cdp.send('Page.enable');
    await cdp.send('Runtime.enable');
    if (flag('--tail')) await servePatchedSt(cdp);
    await cdp.send('Page.addScriptToEvaluateOnNewDocument', { source: INSTRUMENTATION });
    await cdp.send('Page.navigate', { url: `${ST_URL}/` });
    for (let i = 0; ; i++) {
      const ready = await cdp
        .evaluate(`!!(window.SillyTavern?.getContext?.()?.characters?.length
                    && document.getElementById('cloufam-director-nav-btn')
                    && window.formSheldHeightController)`)
        .catch(() => false);
      if (ready) break;
      if (i > 120) throw new Error('ST с расширениями не догрузилась');
      await sleep(500);
    }
    // Connecting to the gateway (absent here) is retried for ~10 s — wait until it settles.
    await sleep(15000);
    const history = Array.from({ length: HISTORY }, (_, i) => buildReply(i % 2 ? 900 : 200, i + 1));
    const setup = await cdp.evaluate(PAGE_SETUP(history));
    await sleep(2000);

    if (flag('--timer-restyle')) {
      // Cost of the message timer: textContent replaces the text node, an insertion
      // wakes :has() on body and restyles the whole chat; a node edit does not.
      console.log(
        JSON.stringify(
          await cdp.evaluate(`(() => {
                const timer = [...document.querySelectorAll('#chat .mes_timer')].at(-1);
                const probe = document.querySelector('#chat .mes .mes_block');
                const measure = (write) => {
                    write(-1);
                    getComputedStyle(probe).color;
                    const t = performance.now();
                    for (let i = 0; i < 20; i++) {
                        write(i);
                        getComputedStyle(probe).color;
                    }
                    return Math.round(((performance.now() - t) / 20) * 100) / 100;
                };
                return {
                    textContentMs: measure((i) => { timer.textContent = i + 's'; }),
                    nodeValueMs: measure((i) => { timer.firstChild.nodeValue = i + 's'; }),
                };
            })()`),
        ),
      );
    }
    const text = flag('--edge-cases') ? buildEdgeCases(REPLY_CHARS) : buildReply(REPLY_CHARS);
    if (flag('--e2e')) {
      const warnings = [];
      cdp.on('Runtime.consoleAPICalled', ({ type, args: values }) => {
        const line = values.map((v) => v.value ?? v.description ?? '').join(' ');
        if (
          (type === 'warning' || type === 'error') &&
          /cloufam|tail-render|renderStreamingText/i.test(line)
        )
          warnings.push(line);
      });
      cdp.on('Runtime.exceptionThrown', ({ exceptionDetails }) =>
        warnings.push(exceptionDetails.exception?.description ?? exceptionDetails.text),
      );
      const llm = await startFakeLlm(text);
      try {
        const result = await cdp.evaluate(PAGE_E2E, 300000);
        const { finalHtml, ...rest } = result;
        // The resulting DOM is compared between runs with stock and patched script.js.
        writeFileSync(path.join(BENCH, `e2e-${LABEL}.html`), finalHtml);
        console.log(
          JSON.stringify(
            { label: LABEL, tail: flag('--tail'), sent: text.length, ...rest, warnings },
            null,
            1,
          ),
        );
      } finally {
        llm.close();
      }
      return;
    }
    const traced = !flag('--no-trace');
    if (traced) {
      await cdp.send('Tracing.start', {
        transferMode: 'ReturnAsStream',
        traceConfig: {
          // An overflowing buffer silently cuts the trace tail — and frames with long text.
          traceBufferSizeInKb: 1024 * 1024,
          includedCategories: [
            'toplevel',
            'devtools.timeline',
            'disabled-by-default-devtools.timeline.stack',
            'blink.user_timing',
            // Who invalidates style: heavy, only for a short analysis.
            ...(flag('--invalidations')
              ? ['disabled-by-default-devtools.timeline.invalidationTracking']
              : []),
          ],
        },
      });
    }
    const page = await cdp.evaluate(
      PAGE_RUN(text, CHARS_PER_FRAME, FRAME_MS, {
        tail: flag('--tail'),
        validate: flag('--validate'),
      }),
      600000,
    );
    let traceFrames = null;
    if (traced) {
      const done = cdp.once('Tracing.tracingComplete');
      await cdp.send('Tracing.end');
      const { stream } = await done;
      traceFrames = analyzeTrace(await readTrace(cdp, stream));
      if (traceFrames.length !== page.frames.length) {
        throw new Error(
          `в трассе ${traceFrames.length} кадров из ${page.frames.length}: буфер переполнен`,
        );
      }
    }
    const summary = {
      label: LABEL,
      directorOn: flag('--director-on'),
      tail: flag('--tail'),
      setup,
      ...summarize(page, traceFrames),
    };
    writeFileSync(
      path.join(BENCH, `results-${LABEL}.json`),
      JSON.stringify({ summary, frames: page.frames, traceFrames }, null, 1),
    );
    console.log(JSON.stringify(summary, null, 2));
  } finally {
    socket.close();
    closeEdge(edge);
    await stopSt(child);
  }
}

mkdirSync(BENCH, { recursive: true });
prepareData();
if (option('--startup', null)) await benchStartup(Number(option('--startup', 3)));
else await benchStreaming();

