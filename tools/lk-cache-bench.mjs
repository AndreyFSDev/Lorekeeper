// End-to-end measurement of the Ollama prompt cache: how many times the model reloads and
// how many tokens are recomputed on each chat turn with the state evaluator
// and the director note between turns.
//
// The install is isolated — src-tauri/target/lk-bench/install: copies of the runtime
// (Ollama, Node) and the SillyTavern code from the working copy, its own data and chats, its own
// Ollama port. Models come from the shared store. The owner's working copy and its chats are
// not touched; the stand closes the app by its own PID (WM_CLOSE).
//
// Preparing the install (once) — see log.md 25.09.2026; after that:
//   node tools/lk-cache-bench.mjs --exe <path to exe> --label before [--fresh]
//        [--model huihui_ai/qwen3-abliterated:4b] [--turns 5] [--cdp-port 9231] [--main-prompt]
// --fresh — clean app and SillyTavern data (seed from scratch); without it
// whatever the previous run left is taken (catching up the settings of an existing copy).
// Result — src-tauri/target/lk-bench/cache-<label>.json and a summary in the console.
import { execSync, spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { loadWs } from './ws-resolve.mjs';

const ROOT = path.resolve(import.meta.dirname, '..');
const BENCH = path.join(ROOT, 'src-tauri', 'target', 'lk-bench');
const INSTALL = path.join(BENCH, 'install');
process.env.LOREKEEPER_INSTALL ??= INSTALL;
const ws = loadWs(ROOT);

const arg = (name, fallback) => {
  const i = process.argv.indexOf(`--${name}`);
  return i === -1 ? fallback : process.argv[i + 1];
};
const EXE_SRC = arg('exe');
const LABEL = arg('label', 'run');
const MODEL = arg('model', 'huihui_ai/qwen3-abliterated:4b');
const TURNS = Number(arg('turns', 5));
const CDP_PORT = Number(arg('cdp-port', 9231));
const OLLAMA_PORT = Number(arg('ollama-port', 11460));
const CHARACTER = process.env.LK_CHARACTER ?? 'default_Seraphina.png';
if (!EXE_SRC) throw new Error('--exe <путь к собранному Cloufam.exe>');

const DATA = path.join(INSTALL, 'data');
const ST_DATA = path.join(INSTALL, 'sillytavern', 'data');
const SERVE_LOG = path.join(DATA, 'ollama-serve.log');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ── install ─────────────────────────────────────────────────────────────────
for (const need of [
  'runtime/ollama/ollama.exe',
  'runtime/node/node.exe',
  'sillytavern/server.js',
  'characters',
]) {
  if (!fs.existsSync(path.join(INSTALL, need))) throw new Error(`в ${INSTALL} нет ${need}`);
}
if (process.argv.includes('--fresh')) {
  fs.rmSync(DATA, { recursive: true, force: true });
  fs.rmSync(ST_DATA, { recursive: true, force: true });
}
fs.mkdirSync(DATA, { recursive: true });
fs.copyFileSync(EXE_SRC, path.join(INSTALL, 'Cloufam.exe'));
const settingsFile = path.join(DATA, 'settings.json');
const settings = fs.existsSync(settingsFile)
  ? JSON.parse(fs.readFileSync(settingsFile, 'utf8'))
  : {};
Object.assign(settings, {
  chat_model: MODEL,
  embed_model: 'bge-m3:latest',
  vision_model: 'huihui_ai/qwen3-vl-abliterated:8b-instruct-q4_K_M',
  context_size: 32768,
  max_tokens: 400,
  auto_memory: true,
  memory_max_chunks: 8,
  ollama_port: OLLAMA_PORT,
  use_system_ollama: false,
  setup_done: true,
  coder_removed: true,
  echo_migrated: true,
  echo_removed: true,
  models_root: 'Z:\\AImodels',
  models_moved: true,
  auto_launch_st: false,
  auto_launch_comfy: false,
  tagger_enabled: false,
  evaluator_model: '',
  changelog_version_seen: '1.2.52',
  // Hardware fingerprint of this machine: otherwise auto-tuning at startup would
  // change the chat model and reply length itself.
  tuned_for: '3|Cuda',
});
fs.writeFileSync(settingsFile, JSON.stringify(settings, null, 2));

// ── CDP ─────────────────────────────────────────────────────────────────────
async function attach(predicate, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  let page = null;
  while (!page && Date.now() < deadline) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`)).json();
      page = list.find((t) => t.type === 'page' && predicate(t.url));
    } catch {
      // no window yet
    }
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
    pending.get(msg.id)?.(msg);
    pending.delete(msg.id);
  });
  const send = (method, params) =>
    new Promise((resolve) => {
      const i = ++id;
      pending.set(i, resolve);
      socket.send(JSON.stringify({ id: i, method, params }));
    });
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
  return { socket, evalJs };
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

// ── ollama serve log ────────────────────────────────────────────────────────
const blobNames = (() => {
  const map = new Map();
  const root = 'Z:\\AImodels\\ollama\\manifests\\registry.ollama.ai';
  for (const ns of fs.readdirSync(root)) {
    for (const name of fs.readdirSync(path.join(root, ns))) {
      for (const tag of fs.readdirSync(path.join(root, ns, name))) {
        const manifest = JSON.parse(fs.readFileSync(path.join(root, ns, name, tag), 'utf8'));
        const layer = manifest.layers.find(
          (l) => l.mediaType === 'application/vnd.ollama.image.model',
        );
        if (layer) map.set(layer.digest.replace(':', '-'), `${name}:${tag}`);
      }
    }
  }
  return map;
})();

const logSize = () => (fs.existsSync(SERVE_LOG) ? fs.statSync(SERVE_LOG).size : 0);
function logSince(offset) {
  const fd = fs.openSync(SERVE_LOG, 'r');
  const size = fs.fstatSync(fd).size;
  const buf = Buffer.alloc(Math.max(0, size - offset));
  fs.readSync(fd, buf, 0, buf.length, offset);
  fs.closeSync(fd);
  return buf.toString('utf8');
}

/** Events in a log segment in order: loads, computed prompts, requests. */
function events(text) {
  const out = [];
  for (const line of text.split(/\r?\n/)) {
    let m = line.match(/starting llama-server.*?--model \S*?(sha256-[0-9a-f]+)\S*.*? -c (\d+)/);
    if (m) {
      out.push({
        kind: 'load',
        model: blobNames.get(m[1]) ?? m[1].slice(0, 18),
        ctx: Number(m[2]),
      });
      continue;
    }
    m = line.match(/prompt eval time =\s+([\d.]+) ms \/\s+(\d+) tokens/);
    if (m) {
      out.push({ kind: 'prefill', tokens: Number(m[2]), ms: Math.round(Number(m[1])) });
      continue;
    }
    m = line.match(
      /\[GIN\].*\|\s+(\d+)\s+\|\s+([\d.]+)(µs|ms|s)\s+\|.*"(\/api\/(?:chat|embed|embeddings|generate))"/,
    );
    if (m) {
      const seconds = Number(m[2]) / { µs: 1e6, ms: 1e3, s: 1 }[m[3]];
      out.push({
        kind: 'request',
        path: m[4],
        status: Number(m[1]),
        ms: Math.round(seconds * 1000),
      });
    }
  }
  return out;
}

// ── scenario ────────────────────────────────────────────────────────────────
// A custom 20-message history: ST's vector memory turns on from the sixth
// message, so it shows whether memory touches the middle of the history.
const HISTORY = [
  [
    'Привет, Фубуки! Не ожидал встретить тебя в библиотеке так поздно.',
    'Кон-кон! Ой, ты меня напугал. Я тут ищу старую книгу про лисьи праздники, говорят, её кто-то спрятал на верхней полке.',
  ],
  [
    'Давай помогу, я выше достаю. Какая обложка?',
    'Синяя, с серебряным тиснением. И пахнет она, говорят, как осенний лес. Звучит глупо, но я правда хочу её найти.',
  ],
  [
    'Вот, кажется, эта? Пыльная, но тиснение серебряное.',
    'Она! Ты гений! Смотри, тут даже закладка осталась — засушенный кленовый лист. Кто-то читал её до нас.',
  ],
  [
    'Интересно, кто. Может, прочитаем вместе, что там на закладке?',
    'На первой странице кто-то написал карандашом: «Тому, кто найдёт — праздник начинается у старого моста». Вот это загадка!',
  ],
  [
    'Старый мост за рекой? Туда полчаса идти.',
    'Полчаса — это ерунда для лисы. Но уже темно... Ты же проводишь меня? Одной как-то не очень хочется.',
  ],
  [
    'Конечно провожу. Возьму фонарик.',
    'Спасибо. Знаешь, с тобой почему-то спокойно. Даже когда по дороге ухает сова — а она ухает, я слышу.',
  ],
  [
    'Мы у моста. Видишь огоньки внизу у воды?',
    'Вижу! Это бумажные фонарики. Их кто-то пускает по течению... Смотри, на каждом нарисован маленький лисий хвост.',
  ],
  [
    'Похоже, книга не врала. Праздник правда здесь.',
    'Я столько лет искала этот праздник, а он всё это время был в получасе от дома. Даже обидно немного. И радостно.',
  ],
  [
    'Хочешь, пустим свой фонарик?',
    'Хочу! Только я не умею их складывать. Покажешь? У тебя пальцы ловкие, я видела, как ты книгу доставал.',
  ],
  [
    'Смотри: сгибаешь пополам, потом уголки внутрь. Вот так.',
    'Получилось! Кривоватый, но мой. Я нарисую на нём хвост... и маленькую звёздочку. Для удачи.',
  ],
];
const TURN_LINES = [
  'Отпускай его, пока течение сильное. Что ты загадала?',
  'А мне расскажешь потом, сбылось ли?',
  'Становится холодно. Пойдём греться — я знаю кафе рядом с мостом.',
  'В кафе пахнет корицей. Что тебе заказать?',
  'Слушай, а почему ты так долго искала этот праздник?',
  'Спасибо, что показала мне всё это. Это был лучший вечер за долгое время.',
];
const DIRECTOR_NOTES = [
  'Внезапно над рекой начинается тихий снег.',
  'В кафе гаснет свет, остаются только свечи.',
];

/** Wait until requests stop coming to Ollama (the evaluator runs after the reply,
 * vectorization — in the ST background); GET polls do not count. */
const posts = () => (logSince(0).match(/\| POST /g) ?? []).length;
const settle = () =>
  waitFor(
    async () => {
      const before = posts();
      await sleep(6000);
      return posts() === before;
    },
    180000,
    0,
  );

let app = null;
async function closeApp() {
  if (!app) return;
  try {
    execSync(`taskkill /PID ${app.pid}`, { stdio: 'ignore' });
  } catch {
    // already closed
  }
  for (let i = 0; i < 30; i += 1) {
    await sleep(1000);
    try {
      process.kill(app.pid, 0);
    } catch {
      return;
    }
  }
  execSync(`taskkill /PID ${app.pid} /T /F`, { stdio: 'ignore' });
}

const result = { label: LABEL, model: MODEL, exe: EXE_SRC, free_vram_mb: null, segments: [] };
try {
  result.free_vram_mb = Number(
    execSync('nvidia-smi --query-gpu=memory.free --format=csv,noheader,nounits').toString().trim(),
  );
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
  let gateway = null;
  for (const delta of [1, -1, 2, -2, 3, -3]) {
    try {
      if (
        (await fetch(`http://127.0.0.1:${OLLAMA_PORT + delta}/api/cloufam/tagger-config`)).ok
      ) {
        gateway = OLLAMA_PORT + delta;
        break;
      }
    } catch {
      // not this port
    }
  }
  if (!gateway) throw new Error('шлюз не найден');
  await main.evalJs(
    `window.__TAURI_INTERNALS__.invoke('mark_st_opened').then(() => { location.href = 'http://127.0.0.1:${boot.port}/'; return true; })`,
  );
  const st = await attach((url) => url.startsWith(`http://127.0.0.1:${boot.port}`), 90000);
  if (!st) throw new Error('страница SillyTavern не открылась');
  await waitFor(
    () =>
      st.evalJs(
        `!!(window.SillyTavern?.getContext && document.getElementById('cloufam-director-nav-btn'))`,
      ),
    120000,
    1000,
  );
  await sleep(3000);

  // On a fresh install ST picks the first model in the list (the embedder) and may
  // end up without a connection — sending then silently goes nowhere.
  const online = await waitFor(
    async () => {
      await st.evalJs(`(async () => {
      const o = (await import('/scripts/openai.js')).oai_settings;
      o.custom_model = ${JSON.stringify(MODEL)};
      const select = document.getElementById('model_custom_select');
      if (select) { select.value = o.custom_model; select.dispatchEvent(new Event('change', { bubbles: true })); }
      document.getElementById('api_button_openai').click();
      return true;
    })()`);
      await sleep(3000);
      return st.evalJs(`(async () => {
      const s = await import('/script.js');
      const o = (await import('/scripts/openai.js')).oai_settings;
      return s.online_status !== 'no_connection' && o.custom_model === ${JSON.stringify(MODEL)};
    })()`);
    },
    60000,
    0,
  );
  if (!online) throw new Error('ST не подключилась к шлюзу');

  result.vectors = await st.evalJs(
    `(() => { const v = SillyTavern.getContext().extensionSettings.vectors; return { position: v.position, depth: v.depth, protect: v.protect, lk_protect: v.lk_protect, enabled_chats: v.enabled_chats }; })()`,
  );
  result.max_context = await st.evalJs(
    `(async () => (await import('/scripts/openai.js')).oai_settings.openai_max_context)()`,
  );

  // All 63 cards in the set have their own system_prompt without {{original}}: with
  // the default prefer_character_prompt it replaces the main Cloufam prompt.
  // Before 25.09.2026 the gateway then did not see the roleplay generation marker and the evaluator did not
  // run; to compare with the old build the override can be turned off
  // (--main-prompt), by default — as the user has it.
  if (process.argv.includes('--main-prompt')) {
    await st.evalJs(`(async () => {
      (await import('/scripts/power-user.js')).power_user.prefer_character_prompt = false;
      return true;
    })()`);
  }
  const opened = await st.evalJs(`(async () => {
    const ctx = SillyTavern.getContext();
    const index = ctx.characters.findIndex((c) => c.avatar === '${CHARACTER}');
    if (index < 0) return false;
    await ctx.selectCharacterById(index);
    await (await import('/script.js')).doNewChat();
    const chat = SillyTavern.getContext().chat;
    const now = new Date().toISOString();
    for (const [user, reply] of ${JSON.stringify(HISTORY)}) {
      chat.push({ name: ctx.name1, is_user: true, is_system: false, send_date: now, mes: user, extra: {} });
      chat.push({ name: ctx.name2, is_user: false, is_system: false, send_date: now, mes: reply, extra: {} });
    }
    await ctx.saveChat();
    await ctx.reloadCurrentChat();
    return SillyTavern.getContext().chat.length;
  })()`);
  if (!opened) throw new Error(`персонаж ${CHARACTER} не найден`);
  result.history_messages = opened;
  // History vectorization runs in the ST module background; wait until the embeddings settle.
  await settle();

  const idle = () =>
    waitFor(
      () =>
        st.evalJs(
          `!document.getElementById('mes_stop') || getComputedStyle(document.getElementById('mes_stop')).display === 'none'`,
        ),
      300000,
      1000,
    );

  for (let turn = 0; turn < TURNS; turn += 1) {
    const offset = logSize();
    let started = Date.now();
    const lenBefore = await st.evalJs(`SillyTavern.getContext().chat.length`);
    const line = TURN_LINES[turn % TURN_LINES.length];
    // While quiet generation is running (Summarize on a long history), ST silently
    // ignores the send — retry until the reply lands in the chat.
    let sent = false;
    for (let attempt = 0; attempt < 5 && !sent; attempt += 1) {
      await settle();
      started = Date.now();
      await st.evalJs(
        `(() => { const a = document.getElementById('send_textarea'); a.value = ${JSON.stringify(line)}; a.dispatchEvent(new Event('input', { bubbles: true })); document.getElementById('send_but').click(); return true; })()`,
      );
      sent = await waitFor(
        async () => (await st.evalJs(`SillyTavern.getContext().chat.length`)) > lenBefore,
        15000,
        500,
      );
    }
    if (!sent) throw new Error(`ход ${turn + 1}: реплика не ушла`);
    await waitFor(
      async () => (await st.evalJs(`SillyTavern.getContext().chat.length`)) >= lenBefore + 2,
      300000,
      500,
    );
    await idle();
    const replied = Date.now() - started;
    await settle();
    result.segments.push({
      what: `ход ${turn + 1}`,
      reply_wall_ms: replied,
      events: events(logSince(offset)),
    });

    if (turn === 1 || turn === 3) {
      const offsetDirector = logSize();
      const note = DIRECTOR_NOTES[turn === 1 ? 0 : 1];
      const resp = await fetch(`http://127.0.0.1:${gateway}/api/cloufam/director`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ text: note, expand: true }),
      });
      await settle();
      result.segments.push({
        what: `режиссёр (${resp.status})`,
        events: events(logSince(offsetDirector)),
      });
    }
  }

  const traces = path.join(DATA, 'traces');
  result.traces = fs.existsSync(traces)
    ? fs
        .readdirSync(traces)
        .flatMap((day) =>
          fs.readdirSync(path.join(traces, day)).map((f) => path.join(traces, day, f)),
        )
        .sort()
        .slice(-TURNS * 2)
        .map((f) => {
          const t = JSON.parse(fs.readFileSync(f, 'utf8'));
          const last = t.request_messages?.at(-1);
          return {
            num_ctx: t.options?.num_ctx,
            messages: t.request_messages?.length,
            tail: last ? `${last.role}: ${String(last.content).slice(0, 60)}` : null,
            metrics: t.ollama_metrics ?? null,
          };
        })
    : [];
} finally {
  await closeApp();
  fs.mkdirSync(BENCH, { recursive: true });
  fs.writeFileSync(path.join(BENCH, `cache-${LABEL}.json`), JSON.stringify(result, null, 2));
}

// ── summary ──────────────────────────────────────────────────────────────────
console.log(
  `${LABEL}: ${MODEL}, свободно VRAM ${result.free_vram_mb} МБ, max_context ST ${result.max_context}, память ${JSON.stringify(result.vectors)}`,
);
for (const seg of result.segments) {
  const parts = seg.events.map((e) =>
    e.kind === 'load'
      ? `ЗАГРУЗКА ${e.model}@${e.ctx}`
      : e.kind === 'prefill'
        ? `префилл ${e.tokens} ток/${e.ms} мс`
        : `${e.path.replace('/api/', '')} ${e.ms} мс`,
  );
  console.log(
    `  ${seg.what}${seg.reply_wall_ms ? ` (ответ ${seg.reply_wall_ms} мс)` : ''}: ${parts.join(' · ')}`,
  );
}

