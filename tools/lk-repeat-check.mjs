// "Repetition" bench: measures whether a new reply repeats past messages verbatim.
//
// Symptom from the user: the character repeats past messages and appends ~20% new
// text to them. The bench runs the scenario through the gateway with a pinned seed
// (identical generation on every run) and for each reply computes the longest
// verbatim chunk that matched any past message in the scene.
//
// Scenarios (--scenario):
//   short — 4 lines, as in a young conversation;
//   long  — 30 lines: ST vector memory turns on only in long chats
//           (`chat.length >= protect`, the owner has protect=256);
//   group — group chat: participant lines with "Name: " prefixes, the reply
//           is written for one character with someone else's line last in history;
//   xlong — 150 pairs (~300 messages, ~13K tokens): the user's long-chat
//           mode, where the q8_0 KV cache and context truncation work live;
//   mid / mid-unique — 75 pairs (~150 messages, ~6.5K tokens) — the real product
//           prompt size (ST trims history by openai_max_context);
//           mid-unique — same, but every message is unique in content.
//
// Variants (--variant):
//   base       — as is: the "This already happened…" note is on (card no_repeat);
//   nonote     — same, but no_repeat is off (the note doesn't go into the prompt);
//   memory     — a vector memory block like in the 25.09 layout: old messages
//                are REMOVED from history and move as a "Past events:" block to
//                the tail (position IN_CHAT, depth 2 — before the last two);
//   memory-top — same, but the block right after the main prompt (layout before 25.09).
//
// Samplers (--samplers):
//   default — soft (temp 0.8, no repetition_penalty: the gateway sets its own 1.12);
//   real    — as it leaves SillyTavern: the "Repetition penalty" slider 1.18
//             (custom_include_body → repeat_penalty), frequency/presence 0.2/0.1,
//             top_k 40 and num_predict 1000 — the real reply length.
//
// Run (application closed):
//   node tools/lk-repeat-check.mjs [--scenario long] [--variant memory]
//   [--samplers real] [--seed 42] [--turns 2] [--model <tag>]
import { execSync, spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';

const arg = (name, fallback) => {
  const i = process.argv.indexOf(`--${name}`);
  return i === -1 ? fallback : process.argv[i + 1];
};
const VARIANT = arg('variant', 'base');
const SCENARIO = arg('scenario', 'short');
const SAMPLERS = arg('samplers', 'default');
const REP_PEN = arg('rep-pen', null);
const REP_WINDOW = arg('rep-window', null);
const ANTI_COPY = process.argv.includes('--anti-copy');
const ANTI_COPY_TAIL = process.argv.includes('--anti-copy-tail');
const USER_TEXT = arg('user', '…');
const CTX = arg('ctx', null);
const COUNT = arg('count', null);
const TEMP = arg('temp', null);
const REAL_TAIL = process.argv.includes('--real-tail');
const NO_SYSTEM = process.argv.includes('--no-system');
const USER_SUFFIX = process.argv.includes('--user-suffix');
const DROP_LAST = process.argv.includes('--drop-last');
// Tail of the real product prompt: the jailbreak instruction (ROLEPLAY_JAILBREAK_PROMPT
// in sillytavern.rs) and the repetition-ban line from the traits fragment (traits::fragment).
const REAL_TAIL_RULES = [
  '[Инструкция: сейчас твоя очередь отвечать. Отвечай исключительно от лица Пэкора, строго соблюдая её характер, уровень интеллекта, лексику и теги из описания. Ни при каких обстоятельствах не веди себя как вежливый ИИ-ассистент. Сообщение собеседника завершено — дай прямой ответ от лица Пэкора.]',
  'Не повторяй жесты, позы и действия из прошлых реплик: каждый ответ приносит новое.',
];
const ANTI_COPY_RULE =
  'Следующее сообщение — новый эпизод сцены: новые события, новые действия, новые предметы и новые слова. Не повторяй и не пересказывай ни одно из прошлых сообщений этой беседы.';
const SEED = Number(arg('seed', 42));
const TURNS = Number(arg('turns', 2));
const INSTALL = process.env.LOREKEEPER_INSTALL ?? 'Z:\\AImodels\\Cloufam';
const DATA = path.join(INSTALL, 'data');
const TRAITS = path.join(DATA, 'traits.json');
const AVATAR = arg('avatar', process.env.LK_CHARACTER ?? 'default_Seraphina.png');
const PERSONA = arg('persona', 'Алекс');

const wait = (ms) => new Promise((r) => setTimeout(r, ms));
const readJson = (file) => {
  try {
    return JSON.parse(fs.readFileSync(file, 'utf8'));
  } catch {
    return null;
  }
};

// ── Scenario ─────────────────────────────────────────────────────────────────
// Character lines are deliberately distinctive and varied: there is no reason to copy them except
// by the repetition mechanism itself.
const SHORT_SCRIPT = [
  ['user', 'Доброе утро! Ты чем занимаешься?'],
  [
    'assistant',
    '*выворачивает карман рюкзака и вытряхивает на стол моток медной проволоки* Собираю антенну для радиоприёмника, чтобы ловить передачи с далёкого маяка. Катушка уже почти готова.',
  ],
  ['user', 'А зачем тебе маяк?'],
  [
    'assistant',
    '*подносит латунную трубку к глазу, как подзорную* Мне сказали, что каждую полночь оттуда передают прогноз погоды для рыбаков. Я записываю все сигналы в блокнот и сверяю с тем, что видно на небе.',
  ],
  ['user', 'И что у тебя в блокноте?'],
  [
    'assistant',
    '*разворачивает потрёпанный блокнот в клетку, исписанный мелким почерком* За неделю — семнадцать записей. В одиннадцати случаях ветер менялся ровно через час после сигнала. Это не совпадение, а закономерность.',
  ],
];

// A long conversation: 30 pairs from different templates, so that the model has material and
// its own templates don't coincide with the messages being checked.
const LONG_TURNS = [
  [
    'ремонтирует старый фонарь у окна',
    'Мне нравится, как трещит стекло, когда его протирают сухой тряпкой.',
  ],
  [
    'раскладывает по столу семена лекарственных трав',
    'Каждый пакетик подписала сама, чтобы не перепутать с ядовитыми.',
  ],
  ['точит нож на точильном камне', 'Острый нож — это не угроза, а уважение к продукту.'],
  [
    'перебирает стопку старых писем',
    'Почерк в этих письмах меняется с каждым годом, будто человек растёт вместе с бумагой.',
  ],
  [
    'собирает из палок модель мельницы',
    'Лопасти должны крутиться от малейшего дуновения, иначе это не мельница, а забор.',
  ],
  ['варит варенье из рябины', 'Горечь уходит, только если варить медленно и не спешить с сахаром.'],
  [
    'рисует карту окрестностей на куске бересты',
    'Дороги на карте я рисую те, которыми хожу сама, а не те, что нарисованы в казённых атласах.',
  ],
  [
    'чинит сломанный стул',
    'Ножка держалась на одном гвозде, и это было оскорбительно для столярного дела.',
  ],
  [
    'сушит на верёвке полоски ткани',
    'Цвет этих полосок зависит от того, в какой воде их полоскали.',
  ],
  [
    'разбирает коробку с инструментами',
    'У каждого инструмента есть своё место, и когда он не на месте — это крик о помощи.',
  ],
  [
    'сшивает страницы старой книги',
    'Переплёт — это скелет книги, и от него зависит, доживёт ли она до внуков.',
  ],
  [
    'набирает воду из колодца',
    'Вода в этом колодце холодная даже в июле, потому что идёт из-под известняка.',
  ],
  [
    'раскладывает шашки на доске',
    'Я играю только в долгую партию, где каждая ошибка возвращается через десять ходов.',
  ],
  [
    'чистит рыбу на крыльце',
    'Нож веду от хвоста к голове, иначе мясо рвётся и блюдо выходит сухим.',
  ],
  [
    'перебирает бусины в жестяной банке',
    'Эти бусины я собирала по одной на разных ярмарках, и каждая помнит свой город.',
  ],
  [
    'затачивает карандаш ножом',
    'Точилка рвёт древесину, а нож оставляет гладкий срез, и грифель не ломается.',
  ],
  [
    'раскладывает по полкам банки с крупой',
    'Порядок на полке экономит время лучше любого расписания.',
  ],
  [
    'пылесосит ковёр в гостиной',
    'Ковёр собрал в себя столько песка, что мог бы основать собственную пустыню.',
  ],
  [
    'вяжет шерстяной шарф',
    'Спицы у меня тяжёлые, костяные, и петли на них выходят ровнее, чем на металлических.',
  ],
  [
    'разводит костёр во дворе',
    'Огонь начинаю с тонкой сухой коры, и только потом кладу толстые ветки.',
  ],
  [
    'считает монеты в кошельке',
    'Монеты старого чекана звучат иначе, чем новые, и это слышно без лупы.',
  ],
  ['моет посуду после ужина', 'Чашки мою в холодной воде, чтобы на них не оставалось разводов.'],
  [
    'раскладывает карты пасьянсом',
    'Пасьянс не предсказывает будущее, он показывает, как ты обращаешься с неопределённостью.',
  ],
  ['проверяет петли на двери', 'Скрип двери — это не поломка, это предупреждение.'],
  ['пропалывает грядку с мятой', 'Мята захватывает землю, если её не останавливать каждую неделю.'],
  [
    'перебирает старые фотографии',
    'На этих фотографиях люди не улыбаются, потому что улыбаться тогда было не принято.',
  ],
  ['готовит тесто для пирога', 'Тесто любит тёплые руки и не прощает спешки.'],
  ['чинит крышу сарая', 'Кровля держится на четырёх гвоздях и уважении к гравитации.'],
  ['переписывает рецепт в тетрадь', 'Рецепт без точных пропорций — не рецепт, а воспоминание.'],
  ['упаковывает вещи в дорогу', 'Дорога начинается с того, что ты завязываешь узел на верёвке.'],
];

// Group chat: ST sends participant lines with name prefixes, the reply
// is written for one character, and someone else's line stands last in history.
const GROUP_SCRIPT = [
  [
    'user',
    'Мария: *раскладывает на столе карты и приподнимает бровь* Ну и погодка сегодня — хоть домой не ходи.',
  ],
  [
    'assistant',
    'Алиса: *подходит к окну и раздвигает занавеску* Дождь идёт третий час. Если он не устанет, то устану я.',
  ],
  [
    'user',
    'Мария: *переворачивает верхнюю карту и щурится* А мне нравится. В такой день никто не спешит, и все говорят правду.',
  ],
  [
    'assistant',
    'Алиса: *садится на подоконник и качает ногой* Правду? В такую погоду люди врут даже сами себе. Но тебе я верю.',
  ],
  [
    'user',
    'Мария: *собирает карты в колоду и постукивает по ней пальцем* Тогда загадай желание, пока дождь не кончился.',
  ],
  [
    'assistant',
    'Алиса: *закрывает глаза и слушает капли по стеклу* Пусть этот вечер продлится ещё немного. Вот моё желание.',
  ],
  [
    'user',
    'Мария: *встаёт из-за стола и смотрит в окно* Ладно, я пошла заваривать чай. Оставайся здесь и не грусти без меня.',
  ],
];

// A very long conversation: 150 pairs. Reply forms alternate, and every record
// is numbered — otherwise the model repeats the stimulus template, and the metric catches
// a scenario artifact, not copying.
const XLONG_PATTERNS = [
  (action, line, i) => `*${action}* ${line} Запись ${i} готова.`,
  (action, line, i) => `${line} Об этом я подумала, когда ${action}.`,
  (action, line, i) => `*ставит точку в тетради* ${line} А ещё сегодня я ${action}.`,
  (action, line, i) => `Запись под номером ${i}: ${line} И ещё я ${action}.`,
  (action, line, i) => `*${action}* На этом запись ${i} заканчивается, и я думаю: ${line}`,
  (action, line, i) => `Сегодня вышло так: ${line} Потом я решила, что ${action}.`,
];

function xlongScript(count = 150) {
  const out = [];
  for (let i = 0; i < count; i += 1) {
    const [action, line] = LONG_TURNS[i % LONG_TURNS.length];
    out.push(['user', `Вопрос номер ${i + 1}: чем закончилась твоя сегодняшняя затея?`]);
    out.push(['assistant', XLONG_PATTERNS[i % XLONG_PATTERNS.length](action, line, i + 1)]);
  }
  return out;
}

function xlongUniqueScript(count) {
  const out = [];
  for (let i = 0; i < count; i += 1) {
    const [action, line] = LONG_TURNS[i % LONG_TURNS.length];
    out.push(['user', `Вопрос номер ${i + 1}: чем закончилась твоя сегодняшняя затея?`]);
    out.push([
      'assistant',
      `*${action} — и это была ${i + 1}-я попытка в этом сезоне* ${line} Запись ${i + 1} готова, и ни одна другая запись не повторяет её.`,
    ]);
  }
  return out;
}

// The same length but without rigid frames (record numbers and repeated questions):
// a control for whether the model itself provokes the repetition, not the stimulus template.
const FREE_QUESTIONS = [
  'А что было дальше?',
  'Расскажи подробнее.',
  'И как на это посмотрели другие?',
  'Что ты почувствовала?',
  'А если подумать, в чём тут дело?',
  'И что из этого вышло?',
  'Продолжай, интересно.',
  'А потом?',
];

function midFreeScript(count) {
  const out = [];
  for (let i = 0; i < count; i += 1) {
    const [action, line] = LONG_TURNS[i % LONG_TURNS.length];
    const shape = i % 6;
    const reply =
      shape === 0
        ? `*${action}* ${line}`
        : shape === 1
          ? `${line} Об этом я подумала, когда ${action}.`
          : shape === 2
            ? `*ставит точку в тетради* ${line} А ещё сегодня я ${action}.`
            : shape === 3
              ? `Скажу прямо: ${line} И ещё я ${action}.`
              : shape === 4
                ? `*${action}* И думаю: ${line}`
                : `Сегодня вышло так: ${line} Потом я решила, что ${action}.`;
    out.push(['user', FREE_QUESTIONS[i % FREE_QUESTIONS.length]]);
    out.push(['assistant', reply]);
  }
  return out;
}

function longScript() {
  const out = [];
  LONG_TURNS.forEach(([action, line], i) => {
    out.push(['user', `${i + 1}-й вопрос дня: и как продвигается твоя затея?`]);
    out.push(['assistant', `*${action}* ${line}`]);
  });
  return out;
}

const SCRIPT =
  SCENARIO === 'long'
    ? longScript()
    : SCENARIO === 'group'
      ? GROUP_SCRIPT
      : SCENARIO === 'mid'
        ? xlongScript(75)
        : SCENARIO === 'mid-unique'
          ? xlongUniqueScript(COUNT === null ? 75 : Number(COUNT))
          : SCENARIO === 'mid-free'
            ? midFreeScript(75)
            : SCENARIO === 'xlong'
              ? xlongScript()
              : SHORT_SCRIPT;

// Metric: the longest verbatim chunk of the reply that occurs in past
// scene messages (in words). 8+ words in a row is no longer "the same style" but
// verbatim copying of someone else's message.
function longestCopy(reply, past) {
  const words = (s) =>
    s
      .toLowerCase()
      .replace(/[^\p{L}\p{N}\s]/gu, ' ')
      .split(/\s+/)
      .filter(Boolean);
  const replyWords = words(reply);
  let best = 0;
  for (const message of past) {
    const pastWords = words(message);
    const pastSet = new Set();
    for (let i = 0; i < pastWords.length; i += 1) {
      for (let j = i + 1; j <= Math.min(pastWords.length, i + 40); j += 1) {
        pastSet.add(pastWords.slice(i, j).join(' '));
      }
    }
    let run = 0;
    for (let i = 0; i < replyWords.length; i += 1) {
      for (let j = i + 1; j <= Math.min(replyWords.length, i + 40); j += 1) {
        if (pastSet.has(replyWords.slice(i, j).join(' '))) run = Math.max(run, j - i);
      }
    }
    best = Math.max(best, run);
  }
  return best;
}

// ── Application startup ────────────────────────────────────────────────────────
const settings = readJson(path.join(DATA, 'settings.json')) ?? {};
const model = arg('model', settings.chat_model);
const traitsBackup = fs.existsSync(TRAITS) ? fs.readFileSync(TRAITS) : null;
const SETTINGS = path.join(DATA, 'settings.json');
const settingsBackup = fs.existsSync(SETTINGS) ? fs.readFileSync(SETTINGS) : null;

// The gateway takes the context size from app settings (chat_num_ctx) — for
// the experiment we change it before the app starts.
if (CTX !== null) {
  const current = readJson(SETTINGS) ?? {};
  current.context_size = Number(CTX);
  fs.writeFileSync(SETTINGS, JSON.stringify(current, null, 2));
  console.log(`context_size принудительно ${CTX}`);
}

function restoreSettings() {
  if (settingsBackup) fs.writeFileSync(SETTINGS, settingsBackup);
}

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

function restoreTraits() {
  if (traitsBackup) fs.writeFileSync(TRAITS, traitsBackup);
  else fs.rmSync(TRAITS, { force: true });
}

const exe = path.join(INSTALL, 'Cloufam.exe');
const app = spawn(exe, [], { cwd: INSTALL, detached: true, stdio: 'ignore' });
app.unref();
console.log(`запущен ${exe}`);

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
      // not started yet
    }
  }
}
if (!gateway) {
  console.log('ПРОВАЛ: шлюз не ответил');
  await closeApp();
  restoreTraits();
  restoreSettings();
  process.exit(1);
}
console.log(`шлюз на ${gateway}, модель ${model}`);
console.log(
  `сценарий ${SCENARIO} (${SCRIPT.length} сообщений в истории), вариант ${VARIANT}, seed ${SEED}`,
);

const post = (url, body) =>
  fetch(`http://127.0.0.1:${gateway}${url}`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });

await post('/api/cloufam/active-character', { avatar: AVATAR, persona: PERSONA });

// ── Variants ─────────────────────────────────────────────────────────────────
if (VARIANT === 'nonote') {
  const traits = readJson(TRAITS) ?? {};
  traits[AVATAR] = { ...(traits[AVATAR] ?? {}), no_repeat: false };
  fs.writeFileSync(TRAITS, JSON.stringify(traits, null, 2));
  console.log('no_repeat выключен — заметка «Это уже было…» не уйдёт в промпт');
}

// ST vector memory removes old messages from history and inserts them
// as a "Past events:" block (vectors/index.js: getPromptText + chat.splice).
// We take lines from the beginning of the conversation — like queryCollection by relevance.
const MEMORY_TAKEN = SCENARIO === 'long' ? [2, 5] : [1];

function buildMessages() {
  const system =
    SCENARIO === 'group'
      ? 'Сейчас очередь говорить только у Алиса. Отвечай строго от лица Алиса от первого лица. Категорически запрещено писать реплики, диалоги, слова или мысли за других персонажей (Мария) или за Алекс. Пиши только свою реплику и свои действия. Не продолжай диалог за собеседников. Начинай сразу с текста реплики без префикса «Имя:».'
      : 'Веди ролевой диалог от лица персонажа Пэкора от первого лица. Строго следуй её характеру и манере речи.';
  const list = [
    {
      role: 'system',
      // Emulating prompt truncation from the head (Ollama trims to num_ctx): without
      // the system prompt the model loses its role and instructions.
      content: NO_SYSTEM ? ' ' : system + (ANTI_COPY ? ` ${ANTI_COPY_RULE}` : ''),
    },
    ...SCRIPT.map(([role, content]) => ({ role, content })),
  ];
  if (VARIANT === 'memory' || VARIANT === 'memory-top') {
    // Pull messages out of history (like chat.splice in vectors/index.js).
    const taken = [];
    let seenAssistant = 0;
    for (const message of list) {
      if (message.role !== 'assistant') continue;
      seenAssistant += 1;
      if (MEMORY_TAKEN.includes(seenAssistant)) {
        message.moved = true;
        taken.push(message);
      }
    }
    const kept = list.filter((m) => !m.moved);
    for (const m of kept) delete m.moved;
    const block = {
      role: 'system',
      content: `Past events:\n${taken.map((m) => `Пэкора: ${m.content}`).join('\n\n')}`,
    };
    if (VARIANT === 'memory') {
      // Cloufam layout since 25.09: the block before the last two messages.
      kept.splice(kept.length - 2, 0, block);
    } else {
      // Layout before 25.09: the block right after the main prompt.
      kept.splice(1, 0, block);
    }
    return kept;
  }
  if (DROP_LAST) {
    // Testing the idea of regenerating without the copied message: remove
    // the last character line — there is nothing left to copy.
    for (let i = list.length - 1; i >= 0; i -= 1) {
      if (list[i].role === 'assistant') {
        list.splice(i, 1);
        break;
      }
    }
  }
  return list;
}

// ── Run ───────────────────────────────────────────────────────────────────
const SAMPLER_PROFILES = {
  default: { max_tokens: 220, temperature: 0.8, top_p: 0.9 },
  // As it leaves ST: preset "Cloufam — balanced" + the penalty slider.
  real: {
    max_tokens: 1000,
    temperature: 0.75,
    top_p: 0.9,
    top_k: 40,
    frequency_penalty: 0.2,
    presence_penalty: 0.1,
    repetition_penalty: 1.18,
  },
};
const samplers = SAMPLER_PROFILES[SAMPLERS] ?? SAMPLER_PROFILES.default;
// Penalty override: in ST the slider sends 1.18, and with a repeat_last_n window of 512
// such a penalty mows down fresh vocabulary and pushes the model toward old messages.
if (REP_PEN !== null) samplers.repetition_penalty = Number(REP_PEN);
if (REP_WINDOW !== null) samplers.repeat_last_n = Number(REP_WINDOW);
if (TEMP !== null) samplers.temperature = Number(TEMP);
console.log(`сэмплеры: ${SAMPLERS} — ${JSON.stringify(samplers)}`);

// We count a match with any past scene message as copying — both with
// our own lines and with others' (in group chat it's others' lines that get copied).
const past = SCRIPT.map(([, text]) => text);
const messages = buildMessages();
const results = [];
for (let turn = 0; turn < TURNS; turn += 1) {
  const userLine = turn === 0 ? USER_TEXT : 'И что дальше?';
  messages.push({
    role: 'user',
    content: USER_SUFFIX
      ? `${userLine} (новый эпизод сцены: новые события, новые действия и новые слова — не повторяй прошлое сообщение)`
      : userLine,
  });
  if (ANTI_COPY_TAIL) messages.push({ role: 'system', content: ANTI_COPY_RULE });
  if (REAL_TAIL) {
    for (const rule of REAL_TAIL_RULES) messages.push({ role: 'system', content: rule });
  }
  const resp = await post('/v1/chat/completions', {
    model,
    messages,
    stream: false,
    ...samplers,
    seed: SEED + turn,
    lk_generation_type: 'normal',
    lk_chat_id: 'lk-repeat-check',
  });
  const data = await resp.json().catch(() => ({}));
  const reply = String(data.choices?.[0]?.message?.content ?? '');
  const copy = longestCopy(reply, past);
  const shown = reply.replace(/\s+/g, ' ').slice(0, 90);
  console.log(`ход ${turn + 1}: дословный кусок ${copy} слов | ${shown}…`);
  results.push({ turn: turn + 1, copy, reply });
  past.push(reply);
  messages.push({ role: 'assistant', content: reply });
}

const worst = Math.max(...results.map((r) => r.copy));
console.log(
  `\n=== итог: ${SCENARIO}/${VARIANT}/${SAMPLERS}, худший дословный кусок ${worst} слов ===`,
);
// 8+ words in a row from a past message — the symptom of "repeats past messages".
const red = worst >= 8;
console.log(
  red ? 'ПРОВАЛ: ответ копирует прошлые сообщения' : 'ПРОШЛО: дословного копирования нет',
);

await closeApp();
restoreTraits();
restoreSettings();
process.exit(red ? 1 : 0);

