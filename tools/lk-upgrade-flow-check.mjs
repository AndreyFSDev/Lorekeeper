// Live run of the model update flow (1.2.63): the "old → new" map,
// the offer and the "Update and delete the old one" scenario through the application IPC.
//
// Running (the application is started with CDP 9223):
//   node stand-out/with-install.mjs tools/lk-upgrade-flow-check.mjs --port 9223
//   node stand-out/with-install.mjs tools/lk-upgrade-flow-check.mjs --port 9223 --apply [--from <old tag> --to <new tag>]
//
// Without --apply the stand only prints the offers. With --apply it updates the first
// pair (or the one given by --from/--to) and verifies: the old one is deleted, the new one is in place,
// the roles moved over.
import path from 'node:path';
import { loadWs } from './ws-resolve.mjs';

const ROOT = path.resolve(import.meta.dirname, '..');
const ws = loadWs(ROOT);

const arg = (name) => {
  const i = process.argv.indexOf(`--${name}`);
  return i === -1 ? null : process.argv[i + 1];
};
const PORT = Number(arg('port') ?? 9223);
const APPLY = process.argv.includes('--apply');
const FROM = arg('from');
const TO = arg('to');

const list = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
const page = list.find((t) => t.type === 'page' && t.url.startsWith('http://tauri.localhost'));
if (!page) {
  throw new Error(
    `страница приложения (http://tauri.localhost) не найдена, есть: ${list.map((t) => t.url).join(', ')}`,
  );
}

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

async function evalJs(expression) {
  const res = await send('Runtime.evaluate', {
    expression,
    awaitPromise: true,
    returnByValue: true,
  });
  const details = res.result?.exceptionDetails;
  if (details) throw new Error(details.exception?.description || JSON.stringify(details));
  return res.result?.result?.value;
}

await send('Runtime.enable', {});
console.log(`страница: ${page.title} ${page.url}`);

const invoke = (cmd, args = {}) =>
  evalJs(`window.__TAURI_INTERNALS__.invoke(${JSON.stringify(cmd)}, ${JSON.stringify(args)})
     .then((v) => ({ ok: true, value: v ?? null }))
     .catch((e) => ({ ok: false, error: String(e) }))`);

const results = [];
function check(name, ok, detail = '') {
  results.push({ name, ok: Boolean(ok), detail });
  console.log(`${ok ? 'ПРОШЛО' : 'ПРОВАЛ'}: ${name}${detail ? ` — ${detail}` : ''}`);
}

const before = await invoke('models_upgrade_state');
console.log(`предложения: ${JSON.stringify(before.value?.offers ?? before)}`);

if (!APPLY) {
  socket.close();
  process.exit(0);
}

const offer = (before.value?.offers ?? []).find(
  (o) => (!FROM || o.from === FROM) && (!TO || o.to === TO),
);
if (!offer) {
  check('нашлось предложение для обновления', false, `from=${FROM ?? 'любой'} to=${TO ?? 'любой'}`);
  socket.close();
  process.exit(1);
}
check('нашлось предложение для обновления', true, `${offer.from} → ${offer.to}`);

console.log(`--- обновляю ${offer.from} → ${offer.to} (загрузка может быть долгой) ---`);
const applied = await invoke('models_upgrade_apply', { from: offer.from, to: offer.to });
check('обновление завершилось без ошибки', applied.ok, applied.ok ? '' : applied.error);

const installedAfter = (await invoke('installed_models')).value ?? [];
const settings = (await invoke('get_settings')).value ?? {};
check('новая модель на месте', installedAfter.includes(offer.to), offer.to);
check('старая модель удалена', !installedAfter.includes(offer.from), offer.from);
const roles = ['chat_model', 'vision_model', 'evaluator_model', 'tagger_model'];
const stuck = roles.filter((r) => settings[r] === offer.from);
check('роли переехали на новую модель', stuck.length === 0, stuck.join(', '));

const after = await invoke('models_upgrade_state');
const stillOffered = (after.value?.offers ?? []).some((o) => o.from === offer.from);
check('предложение пропало после обновления', !stillOffered);

console.log(`\n=== итог: ${results.filter((r) => r.ok).length}/${results.length} ===`);
socket.close();
process.exit(results.every((r) => r.ok) ? 0 : 1);
