// Live check of three ST extension items that until 1.2.74 were verified
// by code only (degraded in the journal):
// 1) a repeated auto-tagging run does not lose the card's existing tags (`writeCardTags`
//    and the in-memory card snapshot);
// 2) card translation is retried after a failure: `translation_seen` is filled only
//    on success or a deliberate skip;
// 3) "Export chat" works in a group chat (`is_group`).
//
// The stand uses its own test card (a duplicate of a real one) and its own group, deletes
// them at the end, and restores the tagger settings to the originals. Before the run, take a snapshot of
// `sillytavern/data/default-user` — see the journal, recovery after the run.
//
// Run: app with WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9223,
//   LOREKEEPER_INSTALL=Z:\AImodels\Cloufam node tools/lk-live-extensions.mjs [--stage a|b|c|all]
import fs from 'node:fs';
import path from 'node:path';
import { loadWs } from './ws-resolve.mjs';

const INSTALL = process.env.LOREKEEPER_INSTALL ?? 'Z:\\AImodels\\Cloufam';
const ROOT = path.resolve(import.meta.dirname, '..');
const ws = loadWs(ROOT);
const arg = (name, fallback) => {
  const i = process.argv.indexOf(`--${name}`);
  return i === -1 ? fallback : process.argv[i + 1];
};
const PORT = Number(arg('port', 9223));
const STAGE = arg('stage', 'all');
const OUT = path.resolve(ROOT, 'stand-out', 'live-ui');
const ST_SETTINGS = path.join(INSTALL, 'sillytavern', 'data', 'default-user', 'settings.json');
const BACKUP_DIR = path.join(INSTALL, 'data', 'chat-backups');
const MARKER = `lk-проверка-${Date.now()}`;
const ENGLISH =
  'A silver-haired archives keeper who sorts water-damaged maps. She speaks softly, ' +
  'hates the smell of mildew, and keeps a lantern lit even at noon because the stacks ' +
  'have no windows.';

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const list = await fetch(`http://127.0.0.1:${PORT}/json/list`).then((r) => r.json());
const page =
  list.find((t) => t.type === 'page' && t.url.includes(':8001')) ??
  list.find((t) => t.type === 'page');
if (!page) throw new Error('нет page-таргетов в CDP');

const s = new ws(page.webSocketDebuggerUrl);
await new Promise((resolve, reject) => {
  s.once('open', resolve);
  s.once('error', reject);
});

let seq = 0;
const pending = new Map();
s.on('message', (raw) => {
  const msg = JSON.parse(raw.toString());
  if (msg.id && pending.has(msg.id)) {
    const { resolve, reject } = pending.get(msg.id);
    pending.delete(msg.id);
    if (msg.error) reject(new Error(msg.error.message));
    else resolve(msg.result);
  }
});

function send(method, params = {}) {
  return new Promise((resolve, reject) => {
    const id = ++seq;
    pending.set(id, { resolve, reject });
    s.send(JSON.stringify({ id, method, params }));
  });
}

async function evaljs(expression) {
  const out = await send('Runtime.evaluate', {
    expression,
    returnByValue: true,
    awaitPromise: true,
  });
  if (out.exceptionDetails) {
    throw new Error(out.exceptionDetails.exception?.description ?? out.exceptionDetails.text);
  }
  return out.result?.value;
}

const checks = [];
function check(name, ok, detail = '') {
  checks.push({ name, ok: !!ok, detail: String(detail).slice(0, 400) });
  console.log(
    `${ok ? 'PASS' : 'FAIL'} ${name}${detail ? ` — ${String(detail).slice(0, 200)}` : ''}`,
  );
}

/** Dynamic import of ST modules: the same paths that injected extensions see. */
const HELPERS = `(async () => {
  const { getContext } = await import('/scripts/st-context.js');
  const { getRequestHeaders } = await import('/script.js');
  return { getContext, getRequestHeaders };
})()`;

async function api(pathname, body) {
  const expr = `(async () => {
    const { getRequestHeaders } = await ${HELPERS};
    const r = await fetch(${JSON.stringify(pathname)}, {
      method: 'POST', headers: getRequestHeaders(), body: JSON.stringify(${JSON.stringify(body)}),
    });
    const j = await r.json().catch(() => null);
    return { status: r.status, j };
  })()`;
  const out = await evaljs(expr);
  if (out?.status >= 400) throw new Error(`${pathname}: ${JSON.stringify(out?.j)}`);
  return out?.j;
}

/** Tagger settings: the same path as the extension's — the ST server plugin
 *  proxies it to the gateway (a direct GET/POST with JSON to the gateway does
 *  not work for these routes: they have no preflight handler). */
async function taggerConfig(body) {
  const method = body === undefined ? 'GET' : 'POST';
  return evaljs(`(async () => {
    const { getRequestHeaders } = await ${HELPERS};
    const r = await fetch('/api/plugins/cloufam/config', {
      method: ${JSON.stringify(method)},
      headers: getRequestHeaders(),
      ${method === 'POST' ? `body: JSON.stringify(${JSON.stringify(body)}),` : ''}
    });
    if (!r.ok) throw new Error('tagger-config: HTTP ' + r.status);
    return await r.json();
  })()`);
}

async function waitForSt(timeoutMs = 90000) {
  for (let i = 0; i < timeoutMs / 1000; i++) {
    const ready = await evaljs(`!!(document.querySelector('#chat') && window.__LK_GATEWAY)`).catch(
      () => false,
    );
    if (ready) return true;
    await sleep(1000);
  }
  return false;
}

async function reload() {
  await send('Page.reload', { ignoreCache: true });
  if (!(await waitForSt())) throw new Error('ST не поднялся после перезагрузки');
  for (let i = 0; i < 30; i++) {
    const ready = await evaljs(
      `document.querySelectorAll('#rm_print_characters_block .character').length`,
    );
    if (ready > 0) break;
    await sleep(1000);
  }
  await sleep(2500);
}

async function openSt() {
  await send('Page.enable');
  await send('Runtime.enable');
  if (await waitForSt(15000)) return;
  await evaljs(`(() => {
    const b = document.querySelector('button.btn-launch-st')
      || [...document.querySelectorAll('button')].find(el => (el.innerText || '').includes('Открыть SillyTavern'));
    if (!b) return 'no_button';
    b.click();
    return 'clicked';
  })()`);
  if (!(await waitForSt(120000))) throw new Error('ST не открылся из лаунчера');
  await sleep(2500);
}

const readSettings = () => JSON.parse(fs.readFileSync(ST_SETTINGS, 'utf8'));
const taggerState = () => readSettings()?.extension_settings?.cloufam_tagger ?? {};
const cardTags = async (avatar) => {
  const j = await api('/api/characters/get', { avatar_url: avatar });
  const data = j?.data ?? j ?? {};
  return {
    tags: Array.isArray(data.tags) ? data.tags.map(String) : [],
    description: String(data.description ?? ''),
    translated: data?.extensions?.cloufam_translation?.language ?? null,
  };
};

async function runTaggerModal(avatar, timeoutMs = 300000) {
  const opened = await evaljs(`(() => {
    const stale = document.querySelector('#cloufam-tagger-modal');
    if (stale) stale.remove();
    const item = document.querySelector('#cloufam-tagger-menu-item');
    if (!item) return 'no_item';
    item.click();
    const modal = document.querySelector('#cloufam-tagger-modal');
    const row = [...document.querySelectorAll('.lk-tagger-row')]
      .find(r => r.dataset.avatar === ${JSON.stringify(avatar)});
    if (!row) {
      const rows = [...document.querySelectorAll('.lk-tagger-row')];
      return 'no_row: modal=' + !!modal + ' rows=' + rows.length +
        ' первые=' + rows.slice(0, 3).map(r => r.dataset.avatar).join(',') +
        ' в контексте=' + !!rows.length;
    }
    document.querySelectorAll('.lk-tagger-row-check').forEach(c => { c.checked = false; });
    const box = row.querySelector('.lk-tagger-row-check');
    box.checked = true;
    box.dispatchEvent(new Event('change', { bubbles: true }));
    document.querySelector('#lk-tagger-write-card').checked = true;
    document.querySelector('#lk-tagger-max').value = '4';
    return 'ready';
  })()`);
  if (opened !== 'ready') throw new Error(`модалка тегов: ${opened}`);
  const before = await evaljs(`document.querySelector('#lk-tagger-log').innerText`);
  await evaljs(`document.querySelector('#lk-tagger-run').click()`);
  for (let i = 0; i < timeoutMs / 2000; i++) {
    await sleep(2000);
    const log = await evaljs(`document.querySelector('#lk-tagger-log').innerText`);
    if (log.slice(before.length).includes('Готово')) return log.slice(before.length);
  }
  throw new Error('прогон тегов не завершился за отведённое время');
}

await openSt();

const all = await api('/api/characters/all', {});
const cards = Array.isArray(all) ? all : (all?.characters ?? []);
const source = cards.find((c) => String(c?.data?.description ?? '').length > 120);
if (!source) throw new Error('нет карточки с описанием — нечего дублировать');

const configOriginal = await taggerConfig();
console.log(
  `исходный конфиг теггера: enabled=${configOriginal.enabled} auto_translate=${configOriginal.auto_translate}`,
);

// ── D. Gateway preflight: ST pages can send JSON directly ───────────────
// Until 1.2.75 these five routes had no OPTIONS handler, and the browser
// killed the page request before it got there (405 from the router instead of 200 on the preflight check).
if (STAGE === 'all' || STAGE === 'd') {
  const routes = [
    '/api/cloufam/tags',
    '/api/cloufam/tagger-config',
    '/api/cloufam/director',
    '/api/cloufam/md-import',
    '/api/cloufam/translate',
  ];
  const codes = await evaljs(`(async () => {
    const out = {};
    for (const p of ${JSON.stringify(routes)}) {
      try {
        out[p] = (await fetch(window.__LK_GATEWAY + p, { method: 'OPTIONS' })).status;
      } catch (e) {
        out[p] = String(e.message ?? e);
      }
    }
    return out;
  })()`);
  for (const p of routes) {
    check(`D. preflight ${p}`, codes?.[p] === 200, `ответ=${codes?.[p]}`);
  }
}

let testAvatar = null;
let groupId = null;
let cardName = '';
try {
  // ── Preparing the test card ────────────────────────────────────────────
  const dupe = await api('/api/characters/duplicate', { avatar_url: source.avatar });
  testAvatar = dupe?.path;
  if (!testAvatar) throw new Error('дублирование карточки не вернуло avatar');
  // The duplicate name is deterministic (`<base>_1.png`), and after the card is deleted
  // the next run takes it: translation_seen, seen and tagMap remember the avatar, so
  // the second run would get "already translated" and "tags already exist". We rename
  // the test card to a unique one — the measure does not depend on traces of the previous run.
  const renamed = await api('/api/characters/rename', {
    avatar_url: testAvatar,
    new_name: `lk-proverka-${Date.now()}`,
  });
  testAvatar = renamed?.avatar ?? testAvatar;
  // ST requires the ch_name field in edit-attribute — without it, it answers 400.
  const dupeCard = await api('/api/characters/get', { avatar_url: testAvatar });
  cardName = dupeCard?.name ?? dupeCard?.data?.name ?? '';
  // Someone else's "already translated" flag (the source card could have been translated earlier)
  // would lead to a deliberate skip rather than translation — we clear it.
  const extensions = { ...(dupeCard?.data?.extensions ?? dupeCard?.extensions ?? {}) };
  delete extensions.cloufam_translation;
  await api('/api/characters/edit-attribute', {
    ch_name: cardName,
    avatar_url: testAvatar,
    field: 'extensions',
    value: extensions,
  });
  await api('/api/characters/edit-attribute', {
    ch_name: cardName,
    avatar_url: testAvatar,
    field: 'description',
    value: ENGLISH,
  });
  await api('/api/characters/edit-attribute', {
    ch_name: cardName,
    avatar_url: testAvatar,
    field: 'tags',
    value: [MARKER],
  });
  await taggerConfig({ enabled: false, write_card_tags: true });
  await reload();

  // ── A. Repeated auto-tagging run ──────────────────────────────────────────
  if (STAGE === 'all' || STAGE === 'a') {
    const log1 = await runTaggerModal(testAvatar);
    const run1 = await cardTags(testAvatar);
    check(
      'A1. первый прогон добавил теги',
      run1.tags.length >= 2,
      `${run1.tags.length}: ${run1.tags.join(', ')} | ${log1.replace(/\n/g, ' ')}`,
    );
    const log2 = await runTaggerModal(testAvatar);
    const run2 = await cardTags(testAvatar);
    const lost = run1.tags.filter((t) => !run2.tags.includes(t));
    check(
      'A2. второй прогон не потерял теги первого',
      lost.length === 0,
      lost.length
        ? `потеряно: ${lost.join(', ')}`
        : `${run2.tags.join(', ')} | ${log2.replace(/\n/g, ' ')}`,
    );
    check('A3. маркер жив после второго прогона', run2.tags.includes(MARKER), run2.tags.join(', '));
    check(
      'A4. дублей в тегах карточки нет',
      new Set(run2.tags).size === run2.tags.length,
      run2.tags.join(', '),
    );
  }

  // ── B. Translation is retried after a failure ───────────────────────────
  if (STAGE === 'all' || STAGE === 'b') {
    const injecting = await send('Page.addScriptToEvaluateOnNewDocument', {
      source: `(() => {
        const real = window.fetch.bind(window);
        window.fetch = (...a) => {
          const url = String(a[0]?.url ?? a[0] ?? '');
          if (url.includes('/api/plugins/cloufam/translate')) {
            return Promise.resolve(new Response('injected failure', { status: 500 }));
          }
          return real(...a);
        };
      })()`,
    });
    await taggerConfig({ auto_translate: true });
    await reload();
    await sleep(15000);
    const seenAfterFail = taggerState().translation_seen ?? [];
    const cardAfterFail = await cardTags(testAvatar);
    check(
      'B1. сбой не помечает карточку переведённой',
      !seenAfterFail.includes(testAvatar),
      `translation_seen=${seenAfterFail.length}`,
    );
    check(
      'B2. после сбоя описание осталось английским',
      cardAfterFail.translated !== 'ru',
      cardAfterFail.description.slice(0, 60),
    );
    await send('Page.removeScriptToEvaluateOnNewDocument', { identifier: injecting.identifier });
    await reload();
    let marked = false;
    for (let i = 0; i < 150 && !marked; i++) {
      await sleep(2000);
      marked = (taggerState().translation_seen ?? []).includes(testAvatar);
    }
    const cardAfterRetry = await cardTags(testAvatar);
    check(
      'B3. после восстановления карточка переведена и помечена',
      marked,
      `seen=${(taggerState().translation_seen ?? []).length}`,
    );
    check(
      'B4. описание стало русским',
      /[А-Яа-я]{4}/.test(cardAfterRetry.description) && cardAfterRetry.translated === 'ru',
      cardAfterRetry.description.slice(0, 80),
    );
  }

  // ── C. Group chat export ──────────────────────────────────────────────
  if (STAGE === 'all' || STAGE === 'c') {
    const group = await api('/api/groups/create', {
      name: `LK live ${Date.now()}`,
      members: [testAvatar, source.avatar],
      activation_strategy: 0,
    });
    groupId = String(group?.id ?? '');
    if (!groupId) throw new Error('группа не создана');
    let selected = null;
    for (let i = 0; i < 20 && selected !== groupId; i++) {
      selected = await evaljs(`(async () => {
        const g = await import('/scripts/group-chats.js');
        await g.getGroups();
        await g.openGroupById(${JSON.stringify(groupId)});
        return g.selected_group;
      })()`);
      await sleep(1500);
    }
    check('C1. групповой чат открыт', selected === groupId, `selected=${selected}`);
    const before = fs.existsSync(BACKUP_DIR) ? fs.readdirSync(BACKUP_DIR) : [];
    const clicked = await evaljs(`(() => {
      const item = document.querySelector('#cloufam-chat-export-menu-item');
      if (!item) return 'no_item';
      item.click();
      return 'clicked';
    })()`);
    check('C2. пункт «Экспорт чата» найден и нажат', clicked === 'clicked', clicked);
    let created = [];
    for (let i = 0; i < 30; i++) {
      await sleep(1000);
      created = (fs.existsSync(BACKUP_DIR) ? fs.readdirSync(BACKUP_DIR) : []).filter(
        (f) => !before.includes(f),
      );
      if (created.length) break;
    }
    check('C3. экспорт из группы создал файл', created.length > 0, created.join(', '));
    if (created.length > 0) {
      const file = path.join(BACKUP_DIR, created[0]);
      const parsed = [];
      let broken = 0;
      for (const line of fs.readFileSync(file, 'utf8').trim().split('\n')) {
        try {
          parsed.push(JSON.parse(line));
        } catch {
          broken += 1;
        }
      }
      const header = parsed[0] ?? {};
      const messages = parsed.slice(1);
      // The group participants are the source card and the renamed duplicate.
      const memberNames = new Set([source.name ?? source.data?.name, cardName]);
      check(
        'C4. файл — валидный jsonl группового чата',
        broken === 0 &&
          created[0].startsWith(`${groupId}-`) &&
          !!header?.chat_metadata &&
          messages.length > 0 &&
          messages.every((m) => memberNames.has(m?.name)),
        `файл=${created[0]} строк=${parsed.length} битых=${broken} реплики=${messages.map((m) => m?.name).join(',')}`,
      );
      const toast = await evaljs(
        `[...document.querySelectorAll('#toast-container .toast-message')].map(t => t.innerText).join(' | ')`,
      );
      check(
        'C5. шлюз принял экспорт (тост о сохранении)',
        /сохран[её]н/i.test(toast),
        toast.slice(0, 120),
      );
      // The evidence snapshot goes into the project; we do not leave files in someone else's backup folder.
      fs.mkdirSync(OUT, { recursive: true });
      fs.copyFileSync(file, path.join(OUT, 'group-export-sample.jsonl'));
      fs.rmSync(file);
    }
  }
} finally {
  if (groupId) {
    await api('/api/groups/delete', { id: groupId }).catch((e) =>
      console.warn(`группа не удалена: ${e}`),
    );
  }
  if (testAvatar) {
    await api('/api/characters/delete', { avatar_url: testAvatar }).catch((e) =>
      console.warn(`карточка не удалена: ${e}`),
    );
  }
  await taggerConfig({
    enabled: configOriginal.enabled,
    auto_translate: configOriginal.auto_translate,
    write_card_tags: configOriginal.write_card_tags,
    max_tags: configOriginal.max_tags,
    language: configOriginal.language,
  }).catch((e) => console.warn(`конфиг не восстановлен: ${e}`));
}

fs.mkdirSync(OUT, { recursive: true });
const failed = checks.filter((c) => !c.ok);
fs.writeFileSync(
  path.join(OUT, 'live-extensions.json'),
  JSON.stringify({ at: new Date().toISOString(), checks }, null, 2),
);
console.log(`=== итог: ${checks.length - failed.length}/${checks.length} ===`);
for (const c of failed) console.log(`FAIL ${c.name} — ${c.detail}`);
process.exit(failed.length === 0 && checks.length > 0 ? 0 : 1);

