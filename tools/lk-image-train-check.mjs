// Live check of image fine-tuning through the app's IPC (CDP WebView2):
// presets from checkpoints on disk, datasets, time estimate; behind flags —
// WD14 captioning and a short training run that waits for the state.
// Usage: node tools/lk-image-train-check.mjs [--import <folder>] [--caption <slug>] [--train <preset> <slug> <repeats> <name>]
import { createRequire } from 'node:module';
import path from 'node:path';
import { loadWs } from './ws-resolve.mjs';

const require = createRequire(import.meta.url);
const ROOT = path.resolve(import.meta.dirname, '..');
const ws = loadWs(ROOT);

const arg = (name, fallback) => {
  const i = process.argv.indexOf(`--${name}`);
  return i === -1 ? fallback : process.argv[i + 1];
};
const PORT = Number(arg('port', 9222));
const IMPORT = arg('import', null);
const CAPTION = arg('caption', null);
const TRAIN = process.argv.includes('--train')
  ? process.argv.slice(process.argv.indexOf('--train') + 1)
  : null;

const list = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
const pages = list.filter((t) => t.type === 'page' && t.url.startsWith('http://tauri.localhost'));
const page = pages.find((t) => t.url.includes('settings')) ?? pages[0];
if (!page) throw new Error('страница приложения не найдена');
console.log(`страница: ${page.url}`);

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
  if (!entry) return;
  pending.delete(msg.id);
  entry(msg);
});
const send = (method, params) =>
  new Promise((resolve) => {
    const i = ++id;
    pending.set(i, resolve);
    socket.send(JSON.stringify({ id: i, method, params }));
  });

await send('Runtime.enable', {});
async function invoke(command, args = {}) {
  const res = await send('Runtime.evaluate', {
    expression: `window.__TAURI_INTERNALS__.invoke(${JSON.stringify(command)}, ${JSON.stringify(args)})`,
    awaitPromise: true,
    returnByValue: true,
  });
  const details = res.result?.exceptionDetails;
  if (details) throw new Error(details.exception?.description || JSON.stringify(details));
  return res.result?.result?.value;
}

if (IMPORT) {
  console.log(`\nимпорт папки ${IMPORT}…`);
  const info = await invoke('image_training_import', { path: IMPORT });
  console.log(
    `  датасет ${info.slug}: картинок ${info.images}, подписей ${info.captions} → ${info.path}`,
  );
}

const presets = await invoke('image_training_presets');
console.log(`\nчекпоинтов-баз: ${presets.length}`);
for (const p of presets) {
  console.log(
    `  ${p.recommended ? '*' : ' '} ${p.name} [${p.family_label}, ${p.resolution}px, тир ${p.min_tier}, ${p.size_gb.toFixed(1)} ГБ] id=${p.id}`,
  );
}
if (presets.length > 0) {
  const perStep = await invoke('image_training_estimate', { presetId: presets[0].id });
  console.log(`оценка для ${presets[0].id}: ${perStep.toFixed(2)} с/шаг`);
}

const datasets = await invoke('image_training_datasets');
console.log(`\nдатасетов: ${datasets.length}`);
for (const d of datasets) {
  console.log(`  • ${d.slug}: картинок ${d.images}, подписей ${d.captions}`);
}

if (CAPTION) {
  console.log(`\nподпись WD14 для датасета ${CAPTION}…`);
  const summary = await invoke('image_training_caption', { dataset: CAPTION });
  console.log(`  ${summary}`);
}

if (TRAIN) {
  const [presetId, dataset, repeats, name] = TRAIN;
  console.log(`\nпрогон: ${presetId} / ${dataset} / повторов ${repeats} / имя ${name}`);
  const runId = await invoke('image_training_start', {
    presetId,
    dataset,
    repeats: Number(repeats),
    name,
  });
  console.log(`  прогон ${runId}`);
  let last = '';
  for (let i = 0; i < 400; i++) {
    await new Promise((r) => setTimeout(r, 3000));
    const state = await invoke('image_training_state');
    const line = `${state.stage} ${state.step}/${state.total} loss=${state.loss ?? '—'} ${state.speed ? state.speed.toFixed(2) + ' с/шаг' : ''} eta=${state.eta_seconds ?? '—'}с`;
    if (line !== last) {
      console.log(`  ${line}`);
      last = line;
    }
    if (!state.running) {
      console.log(
        `\nитог: ${state.stage}, LoRA=${state.lora_path ?? '—'}, ошибка=${state.error ?? '—'}`,
      );
      if (state.log_tail.length > 0) {
        console.log('  хвост лога:');
        for (const row of state.log_tail.slice(-6)) console.log(`    ${row}`);
      }
      break;
    }
  }
}
socket.close();
