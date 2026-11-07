// Model comparison stand for choosing the catalog flagship (research 12.10.2026):
// the same RP prompts into both models, output — the reply, tokens/s and Ollama
// metrics. The vision probe is behind the --image flag (the same model answers from a picture).
//
// Running (Ollama serve is already up, models downloaded):
//   node tools/lk-model-bench.mjs huihui_ai/qwen3.5-abliterated:9b
//   node tools/lk-model-bench.mjs huihui_ai/qwen3.5-abliterated:4B --image path\to\pic.png
//   node tools/lk-model-bench.mjs a:tag --compare b:tag
import fs from 'node:fs';
import path from 'node:path';

const OLLAMA = process.env.OLLAMA_URL ?? 'http://127.0.0.1:11434';

const args = process.argv.slice(2);
const flag = (name) => {
  const i = args.indexOf(`--${name}`);
  return i === -1 ? null : args[i + 1];
};
const models = args.filter(
  (a) => !a.startsWith('--') && a !== flag('image') && a !== flag('compare'),
);
const compare = flag('compare');
const image = flag('image');

if (models.length === 0) {
  console.error(
    'нужен тег модели: node tools/lk-model-bench.mjs <tag> [--compare <tag>] [--image <png>]',
  );
  process.exit(2);
}

// RP scene: character, live state, the interlocutor's line in Russian.
const roleplay = [
  {
    role: 'system',
    content:
      'Ты — Серафина, следопыт из туманного леса: сдержанная, с сухим юмором, говорит короткими ' +
      'фразами и часто описывает действия звёздочками. Пишешь на русском. Не пишешь от имени ' +
      'пользователя и не объясняешь правила.',
  },
  {
    role: 'user',
    content:
      '*подхожу к костру и ставлю рядом походный котёл* Ты опять не спала? В лесу сегодня тихо, ' +
      'можно бы и отдохнуть.',
  },
];

// Long scene: the same character + long backstory (~2.5K words) — to see whether
// the model holds the intonation and whether the long reply falls apart.
const longScene = [
  roleplay[0],
  {
    role: 'user',
    content:
      '*наливаю чай и протягиваю кружку* Расскажи, что было в лесу за эти дни. ' +
      'Начни с дороги к старой мельнице и не сокращай.',
  },
];

async function chat(model, messages, extra = {}) {
  const started = Date.now();
  const resp = await fetch(`${OLLAMA}/api/chat`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    // think:false — same as the product gateway: without it qwen3.5 "thinking" goes into
    // reasoning and burns the token budget without answering the user.
    body: JSON.stringify({
      model,
      messages,
      stream: false,
      think: false,
      options: { temperature: 0.75 },
      ...extra,
    }),
  });
  if (!resp.ok) {
    throw new Error(`${model}: HTTP ${resp.status} ${await resp.text()}`);
  }
  const data = await resp.json();
  const wall = (Date.now() - started) / 1000;
  const evalTps =
    data.eval_count && data.eval_duration
      ? (data.eval_count / (data.eval_duration / 1e9)).toFixed(1)
      : '?';
  return {
    model,
    text: data.message?.content ?? '',
    wall: wall.toFixed(1),
    evalTps,
    promptTokens: data.prompt_eval_count ?? 0,
    evalTokens: data.eval_count ?? 0,
  };
}

function report(label, r) {
  console.log(`\n=== ${label} · ${r.model} ===`);
  console.log(
    `скорость: ${r.evalTps} ток/с, стена ${r.wall} с, промпт ${r.promptTokens} ток, ответ ${r.evalTokens} ток`,
  );
  console.log(r.text.slice(0, 1200));
}

const targets = compare ? [models[0], compare] : models;

for (const model of targets) {
  report('реплика', await chat(model, roleplay));
  report('длинная сцена', await chat(model, longScene));
  if (image) {
    const b64 = fs.readFileSync(path.resolve(image)).toString('base64');
    report(
      'зрение',
      await chat(model, [
        {
          role: 'user',
          content: 'Опиши изображение одним абзацем: кто на нём, во что одет, что происходит.',
          images: [b64],
        },
      ]),
    );
  }
}
