// Live check of what the gateway mixes into the prompt: the character's live state,
// the item-based repetition ban and the penalty window — and that quiet generations (Summarize and
// other ST service requests) go without notes and without state evaluation.
//
// What went to the model, the bench reads from the Prompt Trace (`data/traces/<day>/*.json` —
// messages and options after the gateway substitutions). The model is real, from the working copy:
// since 25.09.2026 the app does not mistake a third-party service on the Ollama port for its own, and
// the release build bypasses the former echo server on 11434 by starting its own Ollama.
//
// The trackers (traits/relationships/stats/skills) the bench restores from a copy after
// the run; the app is closed cleanly.
//
// Run: node tools/lk-prompt-check.mjs [--avatar default_Seraphina.png] [--persona Алекс]
import { execSync, spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';

const arg = (name, fallback) => {
  const i = process.argv.indexOf(`--${name}`);
  return i === -1 ? fallback : process.argv[i + 1];
};

const AVATAR = arg('avatar', process.env.LK_CHARACTER ?? 'default_Seraphina.png');
const PERSONA = arg('persona', 'Алекс');
const INSTALL = process.env.LOREKEEPER_INSTALL ?? 'Z:\\AImodels\\Cloufam';
const DATA = path.join(INSTALL, 'data');
const TRACES = path.join(DATA, 'traces');
const TRACKERS = [
  'traits.json',
  'relationships.json',
  'characters_stats.json',
  'characters_skills.json',
  'bonds.json',
];

const wait = (ms) => new Promise((r) => setTimeout(r, ms));
const readJson = (file) => {
  try {
    return JSON.parse(fs.readFileSync(file, 'utf8'));
  } catch {
    return null;
  }
};

const saved = new Map(
  TRACKERS.map((f) => [
    f,
    fs.existsSync(path.join(DATA, f)) ? fs.readFileSync(path.join(DATA, f)) : null,
  ]),
);
function restoreTrackers() {
  for (const [f, content] of saved) {
    const live = path.join(DATA, f);
    if (content) fs.writeFileSync(live, content);
    else fs.rmSync(live, { force: true });
  }
}

// A clean exit (WM_CLOSE → RunEvent::Exit kills node/ollama/ComfyUI); /F only
// if the window did not close: a hard kill leaves orphans on the ports.
async function closeApp() {
  try {
    execSync('taskkill /IM Cloufam.exe', { stdio: 'ignore' });
  } catch {
    return;
  }
  for (let i = 0; i < 30; i += 1) {
    await wait(1000);
    const list = execSync('tasklist /FI "IMAGENAME eq Cloufam.exe" /NH', { encoding: 'utf8' });
    if (!list.includes('Cloufam.exe')) return;
  }
  execSync('taskkill /F /IM Cloufam.exe', { stdio: 'ignore' });
}

function traceFiles() {
  if (!fs.existsSync(TRACES)) return [];
  return fs
    .readdirSync(TRACES)
    .flatMap((day) => fs.readdirSync(path.join(TRACES, day)).map((f) => path.join(TRACES, day, f)));
}

async function newTrace(before) {
  for (let i = 0; i < 20; i += 1) {
    const fresh = traceFiles()
      .filter((f) => !before.has(f))
      .sort();
    if (fresh.length) return readJson(fresh[fresh.length - 1]);
    await wait(500);
  }
  return null;
}

const settings = readJson(path.join(DATA, 'settings.json')) ?? {};
const exe = path.join(INSTALL, 'Cloufam.exe');
const app = spawn(exe, [], { cwd: INSTALL, detached: true, stdio: 'ignore' });
app.unref();
console.log(`запущен ${exe}`);

// The gateway starts next to the Ollama port (+1, −1, +2, …): we look for it via /v1/models and
// by the sign of its own response, not at one fixed port.
let gateway = null;
for (let i = 0; i < 90 && !gateway; i += 1) {
  await wait(1000);
  const base = settings.ollama_port ?? 11434;
  for (const delta of [1, -1, 2, -2, 3, -3]) {
    try {
      const resp = await fetch(`http://127.0.0.1:${base + delta}/api/cloufam/tagger-config`);
      if (resp.ok) {
        gateway = base + delta;
        break;
      }
    } catch {
      // not up yet
    }
  }
}
if (!gateway) {
  console.log('ПРОВАЛ: шлюз не ответил');
  await closeApp();
  restoreTrackers();
  process.exit(1);
}
console.log(`шлюз на ${gateway}`);

// The gateway closes the connection after a stream itself, but undici keeps keep-alive and
// sends the next request into a dead socket («other side closed», 0 bytes).
// We ask to close the connection after every response and retry once if
// the race still happens.
const post = async (url, body) => {
  for (let attempt = 0; ; attempt += 1) {
    try {
      return await fetch(`http://127.0.0.1:${gateway}${url}`, {
        method: 'POST',
        headers: { 'content-type': 'application/json', connection: 'close' },
        body: JSON.stringify(body),
      });
    } catch (e) {
      if (attempt > 0) throw e;
      console.log(`  соединение сброшено (${e.cause?.code ?? e.message}), повторяю запрос`);
      await wait(700);
    }
  }
};

const getJson = async (url) => {
  for (let attempt = 0; ; attempt += 1) {
    try {
      return await (
        await fetch(`http://127.0.0.1:${gateway}${url}`, {
          headers: { connection: 'close' },
        })
      ).json();
    } catch (e) {
      if (attempt > 0) throw e;
      console.log(`  ${url}: соединение сброшено (${e.cause?.code ?? e.message}), повторяю`);
      await wait(700);
    }
  }
};

/** Read the stream to the end, but do not crash if the connection closes before
 *  the terminal chunk: `resp.text()` on a break throws «terminated» and the bench
 *  died on the first step, never reaching the traces. The bench judges the trace, not whether
 *  the last SSE byte arrived — the break is printed on a separate line. */
async function drainStream(resp) {
  let text = '';
  let broken = null;
  try {
    const reader = resp.body?.getReader();
    if (!reader) return { text: await resp.text(), broken: null };
    const decoder = new TextDecoder();
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      text += decoder.decode(value, { stream: true });
    }
  } catch (e) {
    broken = `${e.name}: ${e.message}${e.cause?.code ? ` (${e.cause.code})` : ''}`;
  }
  return { text, broken };
}

await post('/api/cloufam/active-character', { avatar: AVATAR, persona: PERSONA });

// The system prompt carries the main Cloufam prompt marker — by it the gateway
// tells a roleplay request apart. History with a repeat: the same gesture twice.
const messages = [
  {
    role: 'system',
    content: 'Ты — Усада Пэкора, а не шаблонный вежливый ИИ-помощник. Отыгрывай сцену коротко.',
  },
  { role: 'user', content: 'Привет, чем занята?' },
  { role: 'assistant', content: '*Пэкора хихикает и машет рукой* Ничем особенным, пэко!' },
  { role: 'user', content: 'А если серьёзно?' },
  { role: 'assistant', content: '*Пэкора хихикает и машет рукой* Строю ловушку, пэко!' },
  { role: 'user', content: 'Покажешь ловушку?' },
];
const model = settings.chat_model;
const checks = [];
const traitsBefore = JSON.stringify(
  readJson(path.join(DATA, 'traits.json'))?.[AVATAR]?.relations?.[PERSONA] ?? null,
);

// 0. Bonds: the gateway does not insert the character card's note into the prompt (it travels
// via the depth_prompt PNG that the extension appends), so we judge what
// /api/cloufam/traits returns — the block must be exactly one.
const BONDS_HEADER = 'Отношения с другими персонажами';
const bondsFile = path.join(DATA, 'bonds.json');
const bondsBefore = readJson(bondsFile) ?? {};
const traitNotes = (await getJson('/api/cloufam/traits')) ?? {};
const partnerCard = Object.keys(traitNotes).find((a) => a !== AVATAR);
if (!partnerCard) {
  checks.push([
    'связи: в каталоге нет второй карточки',
    false,
    `заметок: ${Object.keys(traitNotes).length}`,
  ]);
} else {
  fs.writeFileSync(
    bondsFile,
    JSON.stringify({
      ...bondsBefore,
      [AVATAR]: [
        {
          target: partnerCard,
          kind: 'младшая сестра',
          strength: 70,
          note: 'Ревнует.',
          secret: true,
        },
      ],
    }),
  );
  const note = String(((await getJson('/api/cloufam/traits')) ?? {})[AVATAR] ?? '');
  checks.push([
    'связи доехали в заметку карточки',
    note.includes(BONDS_HEADER),
    note.slice(-60).replace(/\n/g, ' '),
  ]);
  checks.push([
    'блок связей в заметке ровно один',
    note.split(BONDS_HEADER).length - 1 === 1,
    `вхождений: ${note.split(BONDS_HEADER).length - 1}`,
  ]);
  checks.push(['тайна помечена отдельной строкой', /не догадывается/.test(note), partnerCard]);
}

// 1. Quiet generation: neither notes nor evaluation (Summarize and other service requests).
// This step runs FIRST among generations: no evaluator has been started before it, so any
// state movement is attributable only to it. State evaluation is asynchronous and
// is appended tens of seconds after the reply, so a snapshot taken right
// after the stream catches a value that has not settled yet — quiet went red on someone else's write.
const BOOT_LOG = path.join(DATA, 'boot.log');
const relationOf = () =>
  JSON.stringify(readJson(path.join(DATA, 'traits.json'))?.[AVATAR]?.relations?.[PERSONA] ?? null);
let before = new Set(traceFiles());
const stateBeforeQuiet = relationOf();
let resp = await post('/v1/chat/completions', {
  model,
  messages: [
    ...messages,
    { role: 'system', content: 'Кратко перескажи сцену в одном предложении.' },
  ],
  stream: false,
  max_tokens: 40,
  lk_generation_type: 'quiet',
});
await resp.text();
const quiet = await newTrace(before);
const quietSent = (quiet?.request_messages ?? []).map((m) => String(m.content)).join('\n');
checks.push([
  'quiet-запрос без заметок',
  quiet &&
    !/уже было в последних ответах/i.test(quietSent) &&
    !new RegExp(`Сейчас .+ с ${PERSONA}:`).test(quietSent) &&
    !/Язык ответа/.test(quietSent),
]);
await wait(20000);
const stateAfterQuiet = relationOf();
checks.push([
  'quiet-запрос не двигает состояние',
  stateAfterQuiet === stateBeforeQuiet,
  `${stateBeforeQuiet} ↔ ${stateAfterQuiet}`,
]);

// 2. A regular reply in a stream: notes, the penalty window, state evaluation.
before = new Set(traceFiles());
resp = await post('/v1/chat/completions', {
  model,
  messages,
  stream: true,
  max_tokens: 60,
  lk_generation_type: 'normal',
});
const streamed = await drainStream(resp);
if (streamed.broken) {
  console.log(`стрим оборван до терминального чанка: ${streamed.broken}`);
}
console.log(`реплика: HTTP ${resp.status}`);
const chat = await newTrace(before);
const sent = (chat?.request_messages ?? [])
  .filter((m) => m.role === 'system')
  .map((m) => String(m.content))
  .join('\n');
checks.push(['живое состояние в промпте', new RegExp(`Сейчас .+ с ${PERSONA}:`).test(sent)]);
checks.push(['язык ответа закреплён в промпте', /Язык ответа/.test(sent)]);
checks.push(['запрет повторов в промпте', /уже было в последних ответах/i.test(sent)]);
checks.push(['повтор назван прямо', /хихикает и машет рукой/.test(sent)]);
checks.push(['окно штрафа 512', chat?.options?.repeat_last_n === 512]);

// The evaluator is considered done by either of two traces: a state write
// (success) or a line in boot.log (model refusal, stale turn, write error).
// Judging by state alone is not enough: it would stay silent both on an honest evaluator
// refusal and when it was never started at all.
const stateBeforeReply = relationOf();
const logBefore = fs.existsSync(BOOT_LOG) ? fs.statSync(BOOT_LOG).size : 0;
let evaluated = '';
for (let i = 0; i < 60 && !evaluated; i += 1) {
  await wait(2000);
  if (relationOf() !== stateBeforeReply) {
    evaluated = `${stateBeforeReply} → ${relationOf()}`;
    break;
  }
  const grown = fs.existsSync(BOOT_LOG) ? fs.readFileSync(BOOT_LOG, 'utf8').slice(logBefore) : '';
  const line = grown
    .split('\n')
    .find((l) => /Оценка состояния|Состояние персонажа|Отношения:/.test(l));
  if (line) evaluated = `отказ: ${line.trim()}`;
}
checks.push([
  'оценщик отработал на обычную реплику',
  Boolean(evaluated),
  evaluated || 'нет ни записи, ни строки за 120 с',
]);

// 3. A regular reply without a stream (streaming is off in ST): notes come too.
before = new Set(traceFiles());
resp = await post('/v1/chat/completions', {
  model,
  messages,
  stream: false,
  max_tokens: 40,
  lk_generation_type: 'normal',
});
await resp.text();
const plain = await newTrace(before);
const plainSent = (plain?.request_messages ?? []).map((m) => String(m.content)).join('\n');
checks.push([
  'реплика без стрима получает заметки',
  new RegExp(`Сейчас .+ с ${PERSONA}:`).test(plainSent),
]);

// 4. An English character card: the reply language follows the interlocutor, not the
// card text — a Russian reply gets a Russian answer.
before = new Set(traceFiles());
resp = await post('/v1/chat/completions', {
  model,
  messages: [
    {
      role: 'system',
      content: 'You are Seraphina, a cheerful mage with a playful streak. Stay in character.',
    },
    { role: 'user', content: 'Привет! Расскажи, что ты умеешь.' },
  ],
  stream: false,
  max_tokens: 150,
  lk_generation_type: 'normal',
});
await resp.text();
const englishCard = await newTrace(before);
const reply = String(englishCard?.reply ?? '');
const cyrillic = (reply.match(/[А-Яа-яЁё]/g) ?? []).length;
const latin = (reply.match(/[A-Za-z]/g) ?? []).length;
checks.push([
  'английская карточка отвечает на языке собеседника',
  reply.length > 0 && cyrillic > latin,
  `кириллицы ${cyrillic}, латиницы ${latin}`,
]);

console.log(
  `состояние до: ${traitsBefore}, после quiet: ${stateAfterQuiet}, после реплики: ${relationOf()}`,
);
console.log('\n=== итог ===');
for (const [name, ok, detail] of checks)
  console.log(`${ok ? 'ПРОШЛО' : 'ПРОВАЛ'}: ${name}${detail ? ` — ${detail}` : ''}`);

await closeApp();
restoreTrackers();
process.exit(checks.every(([, ok]) => ok) ? 0 : 1);

