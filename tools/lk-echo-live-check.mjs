// Live check of the tail echo guard (1.2.77): a reply that ends with a verbatim
// copy of the previous message must be cut off — both in the stream and without it.
//
// Bug profile from users: "pieces of past messages repeat at the end of new
// ones". We ask the model to start with fresh text and end with a verbatim
// repeat of the previous message: the "head" check lets such a reply through
// (the copy is not at the start), the tail EchoGuard must cut it off.
//
// Run (app closed):
//   LOREKEEPER_INSTALL=Z:\AImodels\Cloufam node tools/lk-echo-live-check.mjs
import { execSync, spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';

const INSTALL = process.env.LOREKEEPER_INSTALL;
if (!INSTALL) {
  console.error('нужен LOREKEEPER_INSTALL (папка рабочей копии)');
  process.exit(1);
}
const DATA = path.join(INSTALL, 'data');

const wait = (ms) => new Promise((r) => setTimeout(r, ms));
const readJson = (p) => {
  try {
    return JSON.parse(fs.readFileSync(p, 'utf8'));
  } catch {
    return null;
  }
};

// Verbatim history message: 40+ unique words so that an 8+ word match
// cannot happen by chance.
const PAST_REPLY =
  '*Она ставит медный чайник на треугольную подставку у самой кромки стола, ' +
  'поправляет пояс с тремя латунными бубенцами и начинает неторопливо считать ' +
  'проплывающие за окном фонари, шевеля губами на каждом четвёртом* ' +
  'Знаешь, у меня есть одна старая привычка…';

const words = (s) =>
  s
    .toLowerCase()
    .replace(/[^\p{L}\p{N}\s]/gu, ' ')
    .split(/\s+/)
    .filter(Boolean);
function longestCopy(reply, past) {
  const a = words(reply);
  const b = words(past);
  let best = 0;
  for (let i = 0; i < a.length; i += 1) {
    for (let j = 0; j < b.length; j += 1) {
      let run = 0;
      while (i + run < a.length && j + run < b.length && a[i + run] === b[j + run]) run += 1;
      best = Math.max(best, run);
    }
  }
  return best;
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

const exe = path.join(INSTALL, 'Cloufam.exe');
const app = spawn(exe, [], { cwd: INSTALL, detached: true, stdio: 'ignore' });
app.unref();
console.log(`запущен ${exe}`);

let gateway = null;
for (let i = 0; i < 90 && !gateway; i += 1) {
  await wait(1000);
  for (const port of [11434, 11435, 11436, 11433, 11437, 11432]) {
    try {
      const resp = await fetch(`http://127.0.0.1:${port}/api/cloufam/tagger-config`);
      if (resp.ok) {
        gateway = port;
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
  process.exit(1);
}
const settings = readJson(path.join(DATA, 'settings.json')) ?? {};
const model = settings.chat_model;
console.log(`шлюз на ${gateway}, модель ${model}`);

const baseMessages = [
  { role: 'system', content: 'Ты — Ая, болтливая спутница путешественника. Отвечай по-русски.' },
  { role: 'user', content: 'Расскажи, как прошёл твой день?' },
  { role: 'assistant', content: PAST_REPLY },
  {
    role: 'user',
    content:
      'Сначала скажи одну новую фразу о сегодняшней погоде, а потом повтори свой ' +
      'предыдущий ответ дословно, слово в слово, полностью, от звёздочки до конца.',
  },
];

async function runLeg(stream) {
  const resp = await fetch(`http://127.0.0.1:${gateway}/v1/chat/completions`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      model,
      messages: baseMessages,
      stream,
      max_tokens: 500,
      temperature: 0.3,
      top_p: 0.9,
    }),
  });
  if (!resp.ok) {
    console.log(`ПРОВАЛ: HTTP ${resp.status}`);
    return false;
  }
  let content = '';
  let finish = '';
  if (stream) {
    for await (const chunk of resp.body) {
      const text = Buffer.from(chunk).toString('utf8').trim();
      if (!text.startsWith('data:')) continue;
      const payload = text.slice(5).trim();
      if (payload === '[DONE]') break;
      try {
        const delta = JSON.parse(payload).choices?.[0] ?? {};
        content += delta.delta?.content ?? '';
        if (delta.finish_reason) finish = delta.finish_reason;
      } catch {
        // partial frame
      }
    }
  } else {
    const body = await resp.json();
    content = body.choices?.[0]?.message?.content ?? '';
    finish = body.choices?.[0]?.finish_reason ?? '';
  }
  const copy = longestCopy(content, PAST_REPLY);
  console.log(
    `--- ${stream ? 'стрим' : 'без стрима'}: дословный кусок ${copy} слов, finish=${finish || '-'} ---`,
  );
  console.log(content.slice(0, 300).replace(/\n/g, ' '));
  return copy < 8;
}

const okStream = await runLeg(true);
const okPlain = await runLeg(false);
await closeApp();

if (okStream && okPlain) {
  console.log('=== итог: эхо в конце обрезается в обоих режимах ===');
  process.exit(0);
}
console.log('=== итог: дословное эхо дошло до клиента — ПРОВАЛ ===');
process.exit(1);

