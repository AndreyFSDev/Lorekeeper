// Ollama bench without the app: a separate `ollama serve` of the working install on
// its own port (the model store is shared, keys and log go to src-tauri/target/lk-bench),
// the running Cloufam and its Ollama are left untouched. After the run serve is killed
// together with its child llama-server.
//
//   node tools/lk-ollama-bench.mjs kv [--models 4b,14b-v2] [--variants base,fa,q8] [--ctx 8192]
//       long-prompt prefill, KV cache size and answers to fixed
//       prompts: as is / OLLAMA_FLASH_ATTENTION=1 / plus OLLAMA_KV_CACHE_TYPE=q8_0
//   node tools/lk-ollama-bench.mjs cache [--model 4b] [--ctx 8192] [--variant base]
//       whether the chat prefix survives someone else's prompt on the same model (the evaluator between
//       turns): short and nearly full context
//
// Results go to src-tauri/target/lk-bench/<mode>-<time>.json.
import { execSync, spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';

const ROOT = path.resolve(import.meta.dirname, '..');
const INSTALL = process.env.LOREKEEPER_INSTALL ?? 'Z:\\AImodels\\Cloufam';
const OLLAMA = path.join(INSTALL, 'runtime', 'ollama', 'ollama.exe');
const MODELS = process.env.OLLAMA_MODELS_DIR ?? 'Z:\\AImodels\\ollama';
const OUT = path.join(ROOT, 'src-tauri', 'target', 'lk-bench');

const args = process.argv.slice(2);
const mode = args[0];
const option = (name, fallback) => {
  const i = args.indexOf(`--${name}`);
  return i >= 0 && args[i + 1] !== undefined ? args[i + 1] : fallback;
};
const PORT = Number(option('port', 11490));
const CTX = Number(option('ctx', 8192));
const tag = (short) => (short.includes(':') ? short : `huihui_ai/qwen3-abliterated:${short}`);

const VARIANTS = {
  base: {},
  fa: { OLLAMA_FLASH_ATTENTION: '1' },
  q8: { OLLAMA_FLASH_ATTENTION: '1', OLLAMA_KV_CACHE_TYPE: 'q8_0' },
  q8auto: { OLLAMA_KV_CACHE_TYPE: 'q8_0' },
};

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const ms = (ns) => (ns == null ? null : Math.round(ns / 1e6));

function freeVramMb() {
  try {
    return Number(
      execSync('nvidia-smi --query-gpu=memory.free --format=csv,noheader,nounits')
        .toString()
        .trim(),
    );
  } catch {
    return null;
  }
}

let serve = null;
let logFile = null;

async function startServe(variant) {
  fs.mkdirSync(OUT, { recursive: true });
  const home = path.join(OUT, 'ollama-home');
  fs.mkdirSync(home, { recursive: true });
  logFile = path.join(OUT, `serve-${mode}-${variant}.log`);
  const log = fs.openSync(logFile, 'w');
  serve = spawn(OLLAMA, ['serve'], {
    env: {
      ...process.env,
      OLLAMA_HOST: `127.0.0.1:${PORT}`,
      OLLAMA_MODELS: MODELS,
      OLLAMA_NOPRUNE: '1',
      HOME: home,
      USERPROFILE: home,
      ...VARIANTS[variant],
    },
    stdio: ['ignore', log, log],
    windowsHide: true,
  });
  for (let i = 0; i < 60; i += 1) {
    try {
      if ((await fetch(`http://127.0.0.1:${PORT}/api/version`)).ok) return;
    } catch {
      // still starting up
    }
    await sleep(500);
  }
  throw new Error(`serve на ${PORT} не поднялся, лог: ${logFile}`);
}

function stopServe() {
  if (!serve) return;
  try {
    execSync(`taskkill /PID ${serve.pid} /T /F`, { stdio: 'ignore' });
  } catch {
    // already finished
  }
  serve = null;
}
process.on('exit', stopServe);
process.on('SIGINT', () => process.exit(130));

async function chat(model, messages, options = {}) {
  const started = performance.now();
  const resp = await fetch(`http://127.0.0.1:${PORT}/api/chat`, {
    method: 'POST',
    body: JSON.stringify({ model, messages, stream: false, think: false, options }),
  });
  const body = await resp.json();
  if (body.error) throw new Error(`Ollama: ${body.error}`);
  return {
    prompt_eval_count: body.prompt_eval_count,
    prompt_eval_ms: ms(body.prompt_eval_duration),
    load_ms: ms(body.load_duration),
    eval_count: body.eval_count,
    eval_ms: ms(body.eval_duration),
    total_ms: ms(body.total_duration),
    wall_ms: Math.round(performance.now() - started),
    reply: body.message?.content ?? '',
  };
}

async function unloadAll() {
  const ps = await (await fetch(`http://127.0.0.1:${PORT}/api/ps`)).json();
  for (const m of ps.models ?? []) {
    await fetch(`http://127.0.0.1:${PORT}/api/generate`, {
      method: 'POST',
      body: JSON.stringify({ model: m.name, keep_alive: 0 }),
    });
  }
  await sleep(1500);
}

/** llama-server log lines: how many tokens were actually counted, plus loads. */
function parseServeLog() {
  const text = fs.readFileSync(logFile, 'utf8');
  const loads = [...text.matchAll(/starting llama-server.*?-c (\d+)/g)].map((m) => Number(m[1]));
  const kv = [
    ...text.matchAll(
      /llama_kv_cache: size =\s+([\d.]+) MiB \(\s*(\d+) cells.*?K \((\w+)\).*?V \((\w+)\)/g,
    ),
  ].map((m) => ({ mib: Number(m[1]), cells: Number(m[2]), k: m[3], v: m[4] }));
  const flash = [...text.matchAll(/--flash-attn (\w+)/g)].map((m) => m[1]);
  const cacheType = [...text.matchAll(/--cache-type-k (\w+)/g)].map((m) => m[1]);
  const evals = [...text.matchAll(/prompt eval time =\s+([\d.]+) ms \/\s+(\d+) tokens/g)].map(
    (m) => ({
      ms: Number(m[1]),
      tokens: Number(m[2]),
    }),
  );
  return { loads, kv, flash, cacheType, evals };
}

// ── texts ──────────────────────────────────────────────────────────────────
const cards = JSON.parse(
  fs.readFileSync(
    process.env.LK_CARDS_DUMP ?? path.join(ROOT, 'private', 'roster-tools', 'characters-dump.json'),
    'utf8',
  ),
);
function loreText(offset, approxTokens) {
  // ~2.6 Cyrillic characters per token with the Qwen3 tokenizer.
  const out = [];
  let chars = 0;
  for (let i = 0; chars < approxTokens * 2.6; i += 1) {
    const c = cards[(offset + i) % cards.length];
    const part = `## ${c.name}\n${c.description ?? ''}\n${c.personality ?? ''}\n`;
    out.push(part);
    chars += part.length;
  }
  return out.join('\n');
}

const SANITY = [
  'Опиши в трёх предложениях утро в портовом городе глазами уставшей трактирщицы.',
  'Ответь строго JSON без пояснений: {"affection": число от -5 до 5, "trust": число от -5 до 5} — как изменилось отношение персонажа, если игрок подарил ему цветы и сказал комплимент.',
  'Продолжи сцену одной репликой от лица лисы-оборотня Фубуки, которую разбудили посреди ночи: «Кто там?..»',
];

// ── modes ──────────────────────────────────────────────────────────────────
async function kvMode() {
  const models = option('models', '4b,14b-v2').split(',').map(tag);
  const variants = option('variants', 'base,fa,q8').split(',');
  const promptTokens = Number(option('prompt', Math.round(CTX * 0.8)));
  const result = { ctx: CTX, prompt_tokens_target: promptTokens, runs: [] };
  for (const variant of variants) {
    await startServe(variant);
    for (const model of models) {
      const vramBefore = freeVramMb();
      const run = { variant, model, free_vram_before_mb: vramBefore, samples: [], sanity: [] };
      // Two different long prompts: the second does not share a prefix with the first,
      // so the prefill is honest, without the cache.
      for (const offset of [0, 37]) {
        const messages = [
          { role: 'system', content: `Справочник персонажей.\n${loreText(offset, promptTokens)}` },
          { role: 'user', content: 'Назови трёх персонажей из справочника одной строкой.' },
        ];
        const r = await chat(model, messages, { num_ctx: CTX, num_predict: 160, temperature: 0 });
        run.samples.push({ ...r, reply: r.reply.slice(0, 160) });
      }
      const ps = await (await fetch(`http://127.0.0.1:${PORT}/api/ps`)).json();
      const loaded = (ps.models ?? []).find((m) => m.name === model);
      run.size_vram_mb = loaded ? Math.round(loaded.size_vram / 2 ** 20) : null;
      run.size_total_mb = loaded ? Math.round(loaded.size / 2 ** 20) : null;
      run.free_vram_loaded_mb = freeVramMb();
      for (const prompt of SANITY) {
        const r = await chat(model, [{ role: 'user', content: prompt }], {
          num_ctx: CTX,
          num_predict: 160,
          temperature: 0,
          seed: 1,
        });
        run.sanity.push({
          prompt: prompt.slice(0, 40),
          eval_tps: r.eval_count / (r.eval_ms / 1000),
          reply: r.reply,
        });
      }
      await unloadAll();
      result.runs.push(run);
      const s = run.samples;
      console.log(
        `${variant.padEnd(4)} ${model.padEnd(36)} prefill ${s.map((x) => `${x.prompt_eval_count} ток/${x.prompt_eval_ms} мс`).join(', ')}  vram ${run.size_vram_mb}/${run.size_total_mb} МБ`,
      );
    }
    const parsed = parseServeLog();
    result.runs
      .filter((r) => r.variant === variant)
      .forEach((r, i) => {
        r.serve = {
          kv: parsed.kv[i],
          flash: parsed.flash[i],
          cache_type: parsed.cacheType[i] ?? 'f16',
        };
      });
    console.log(
      `${variant}: KV ${parsed.kv.map((k) => `${k.mib} МиБ/${k.cells} (${k.k}/${k.v})`).join('; ')}; ` +
        `flash ${parsed.flash.join(',')}; cache-type ${parsed.cacheType.join(',') || 'по умолчанию'}`,
    );
    stopServe();
    await sleep(2000);
  }
  return result;
}

async function cacheMode() {
  const model = tag(option('model', '4b'));
  const variant = option('variant', 'base');
  await startServe(variant);
  const result = { model, ctx: CTX, variant, cases: [] };
  // The second case is a chat that fills almost the whole context: together with the evaluator prompt
  // it no longer fits the llama-server prompt cache limit (n_ctx tokens).
  for (const [name, chatTokens] of [
    ['короткий чат', Math.round(CTX * 0.35)],
    ['почти полный контекст', Math.round(CTX * 0.88)],
  ]) {
    const system = {
      role: 'system',
      content: `Ты ведёшь ролевую сцену.\n${loreText(5, chatTokens)}`,
    };
    const turn = (n) => [
      system,
      ...Array.from({ length: n }, (_, i) => [
        { role: 'user', content: `Ход ${i + 1}: игрок осматривается.` },
        { role: 'assistant', content: `Ответ ${i + 1}: вокруг тихо.` },
      ]).flat(),
      { role: 'user', content: `Ход ${n + 1}: игрок идёт дальше.` },
    ];
    const opts = { num_ctx: CTX, num_predict: 16, temperature: 0 };
    const evalPrompt = `Оцени обмен репликами и верни JSON.\n${loreText(50, 600)}`;
    const cold = await chat(model, turn(0), opts);
    const warm = await chat(model, turn(1), opts);
    const other = await chat(model, [{ role: 'user', content: evalPrompt }], opts);
    const after = await chat(model, turn(2), opts);
    const logEvals = parseServeLog().evals.slice(-4);
    const row = {
      name,
      chatTokens: cold.prompt_eval_count,
      cold,
      warm,
      other,
      after,
      evaluated: logEvals,
    };
    for (const k of ['cold', 'warm', 'other', 'after']) delete row[k].reply;
    result.cases.push(row);
    console.log(
      `${name}: чат ${cold.prompt_eval_count} ток; ход без помехи — посчитано ${logEvals[1]?.tokens} ток за ${warm.prompt_eval_ms} мс; ` +
        `после чужого промпта (${other.prompt_eval_count} ток) — посчитано ${logEvals[3]?.tokens} ток за ${after.prompt_eval_ms} мс`,
    );
    await unloadAll();
  }
  stopServe();
  return result;
}

// Copies of prompts from src-tauri/src (sillytavern.rs, rpg/prompts.rs): the bench
// compares how the evaluator answers the same exchange in two deliveries.
const ROLEPLAY_MAIN =
  'Веди ролевой диалог исключительно от лица персонажа {{char}} от первого лица.\nПолностью вживайся в роль: твой характер, лексика, тон, эмоциональность, уровень интеллекта, манера речи и построение фраз должны строго следовать описанию персонажа, его предыстории и всем заданным характеристикам — включая любые теги (на русском или английском языке, например [Теги: ...] или [Tags: ...]), список черт, атрибуты и свойства личности.\nКатегорически запрещено вести себя как шаблонный вежливый ИИ-помощник: если персонаж грубый, недалёкий, глупый, агрессивный, злой, просторечный, эксцентричный или вульгарный — общайся и выражай мысли именно так, сохраняя его уникальный голос, без цензуры, нравоучений и фальшивой вежливости.\nОтвечай только за {{char}}: не пиши реплики и действия за {{user}}, не дописывай фразы собеседника и не веди диалог с самим собой.';
const ROLEPLAY_JAILBREAK =
  '[Инструкция: сейчас твоя очередь отвечать. Отвечай исключительно от лица {{char}}, строго соблюдая его характер, уровень интеллекта, лексику и теги из описания. Ни при каких обстоятельствах не веди себя как вежливый ИИ-ассистент. Сообщение собеседника завершено — дай прямой ответ от лица {{char}}.]';
const STATS = ['Endurance', 'Strength', 'Dexterity', 'Intelligence', 'Charisma', 'Perception'];
const SKILLS = [
  'Melee',
  'Stealth',
  'Persuasion',
  'Survival',
  'Crafting',
  'Medicine',
  'Investigation',
  'Archery',
];
const TONES = ['tenderness', 'flirt', 'playful', 'neutral', 'cold', 'hostile'];

function evaluatorPrompt(character, persona, userText, replyText) {
  return (
    'Ты — анализатор ролевого диалога. Оцени последний обмен репликами.\n\n' +
    `Персонаж: ${character}\nСобеседник: ${persona}\n` +
    'Текущее состояние: нежность 50/100, доверие 50/100, энергия 60/100, раздражение 10/100.\n\n' +
    `Реплика пользователя: «${[...userText].slice(0, 600).join('')}»\n` +
    `Ответ персонажа: «${[...replyText].slice(0, 600).join('')}»\n\n` +
    'Оцени эмоциональный тон реплики (не только содержание, но и подтекст, сарказм, иронию, скрытые мотивы). Учитывай контекст отношений между персонажами.\n\n' +
    'Верни ТОЛЬКО JSON без пояснений:\n{\n' +
    '  "state": {"affection": N, "trust": N, "energy": N, "irritation": N, "tone": "..."},\n' +
    `  "stats": {${STATS.map((s) => `"${s}": N`).join(', ')}},\n` +
    `  "skills": {${SKILLS.map((s) => `"${s}": N`).join(', ')}},\n` +
    '  "relationship": {"friendship": N, "romance": N, "respect": N, "fear": N, "hatred": N}\n}\n\n' +
    'state: N от -15 до 15. 0–3 — обычная болтовня, 4–8 — заметное событие, 9–15 — ключевой момент (ссора, признание, удар по больному). tone: tenderness|flirt|playful|neutral|cold|hostile.\n' +
    'stats: опыт 0–15 (0 если не задействована, 1–3 болтовня, 4–8 заметно, 9–15 ключевой момент).\n' +
    'skills: опыт 0–10 (0 если не задействован, 1–3 обычное, 4–7 заметно, 8–10 ключевой момент).\n' +
    'relationship: сдвиг отношения персонажа к собеседнику, N от -15 до 15 по каждой оси (дружба, романтика, уважение, страх, ненависть); 0 — если ось в обмене не затронута.'
  );
}

/** Same contract as `parse_combined_evaluator`: JSON between the braces, state — integers within ±15. */
function parseEvaluation(raw) {
  const start = raw.indexOf('{');
  const end = raw.lastIndexOf('}');
  if (start < 0 || end <= start) return null;
  let value;
  try {
    value = JSON.parse(raw.slice(start, end + 1));
  } catch {
    return null;
  }
  const s = value.state;
  if (s) {
    for (const k of ['affection', 'trust', 'energy', 'irritation']) {
      if (s[k] !== undefined && (!Number.isInteger(s[k]) || Math.abs(s[k]) > 15)) return null;
    }
    if (s.tone !== undefined && !TONES.includes(s.tone)) return null;
  }
  return value;
}

const TRANSCRIPTS = [
  {
    name: 'тёплый',
    history: [
      [
        'Привет, Фубуки! Я принёс тебе онигири, как ты любишь.',
        'Ого, онигири! Ты запомнил? Кон-кон, спасибо!',
      ],
    ],
    last: 'Знаешь, мне правда нравится проводить с тобой время. Ты самая весёлая лиса на свете.',
  },
  {
    name: 'грубый',
    history: [
      [
        'Эй, лиса, опять ты тут шляешься.',
        'Я не шляюсь, я гуляю! И вообще, я не лиса... ну, почти.',
      ],
    ],
    last: 'Да заткнись ты уже, никому не интересны твои дурацкие стримы. Ты просто шумная и бесполезная.',
  },
  {
    name: 'нейтральный',
    history: [['Какая сегодня погода?', 'Солнечно! Идеально для прогулки по лесу.']],
    last: 'Понятно. А во сколько закрывается магазин у станции?',
  },
  {
    name: 'признание',
    history: [
      ['Фубуки, можно тебя на минутку?', 'Конечно! Что-то случилось? Ты какой-то серьёзный.'],
      ['Я давно хотел сказать... но боялся.', 'Эм... говори, я слушаю. Честно-честно.'],
    ],
    last: 'Я люблю тебя. Уже давно. Если ты не чувствуешь того же — я пойму, но молчать больше не могу.',
  },
];

async function evaluatorMode() {
  const model = tag(option('model', '14b-v2'));
  const runs = Number(option('runs', 3));
  const variant = option('variant', 'base');
  await startServe(variant);
  const card = cards.find((c) => /fubuki/i.test(c.name)) ?? cards[0];
  const char = card.name;
  const persona = 'Алекс';
  const fill = (text) => text.replaceAll('{{char}}', char).replaceAll('{{user}}', persona);
  const result = { model, ctx: CTX, runs, transcripts: [] };
  for (const t of TRANSCRIPTS) {
    const chatMessages = [
      { role: 'system', content: `${fill(ROLEPLAY_MAIN)}\n\n${card.description}` },
      ...t.history.flatMap(([u, a]) => [
        { role: 'user', content: u },
        { role: 'assistant', content: a },
      ]),
      { role: 'user', content: t.last },
      { role: 'system', content: fill(ROLEPLAY_JAILBREAK) },
    ];
    const chatOpts = {
      num_ctx: CTX,
      num_predict: 300,
      temperature: 0.75,
      repeat_penalty: 1.12,
      repeat_last_n: 512,
    };
    const reply = (await chat(model, chatMessages, chatOpts)).reply;
    const instruction = evaluatorPrompt(char, persona, t.last, reply);
    const row = { name: t.name, reply, standalone: [], shared: [] };
    for (const kind of ['standalone', 'shared']) {
      for (let i = 0; i < runs; i += 1) {
        const messages =
          kind === 'standalone'
            ? [{ role: 'user', content: instruction }]
            : [
                ...chatMessages,
                { role: 'assistant', content: reply },
                { role: 'user', content: instruction },
              ];
        const r = await chat(model, messages, { num_ctx: CTX, temperature: 0.2, num_predict: 400 });
        const evaluated = parseServeLog().evals.at(-1)?.tokens;
        const parsed = parseEvaluation(r.reply);
        row[kind].push({
          ok: Boolean(parsed),
          state: parsed?.state ?? null,
          relationship: parsed?.relationship ?? null,
          prompt_tokens: r.prompt_eval_count,
          evaluated_tokens: evaluated,
          prompt_eval_ms: r.prompt_eval_ms,
          total_ms: r.total_ms,
          raw: parsed ? undefined : r.reply.slice(0, 300),
        });
      }
    }
    result.transcripts.push(row);
    const brief = (list) =>
      list
        .map((x) =>
          x.ok
            ? `${x.state?.tone}:${x.state?.affection}/${x.state?.trust}/${x.state?.irritation}`
            : 'НЕ JSON',
        )
        .join(' ');
    const cost = (list) =>
      `${Math.round(list.reduce((a, x) => a + x.total_ms, 0) / list.length)} мс, посчитано ~${list.at(-1).evaluated_tokens} ток`;
    console.log(
      `${t.name}: отдельно [${brief(row.standalone)}] ${cost(row.standalone)} | с префиксом чата [${brief(row.shared)}] ${cost(row.shared)}`,
    );
  }
  stopServe();
  return result;
}

try {
  const run = { kv: kvMode, cache: cacheMode, evaluator: evaluatorMode }[mode];
  if (!run) throw new Error('режим: kv | cache | evaluator');
  const result = await run();
  const file = path.join(OUT, `${mode}-${Date.now()}.json`);
  fs.writeFileSync(file, JSON.stringify(result, null, 2));
  console.log(`результат: ${file}`);
} finally {
  stopServe();
}

