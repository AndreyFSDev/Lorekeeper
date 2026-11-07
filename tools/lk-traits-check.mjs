// Live check of character traits through the application IPC (CDP WebView2):
// shows the characters and their tags, saves the intensity and reads what
// actually landed in the card (depth prompt inside the PNG).
// Running: node tools/lk-traits-check.mjs [--port 9222] [--avatar X.png] [--apply]
//        [--bonds]  — the bonds branch: writes a bond, checks the block in the card and removes it

import fs from 'node:fs';
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
const APPLY = process.argv.includes('--apply');
const BONDS = process.argv.includes('--bonds');
const OPEN_ST = process.argv.includes('--open-st');
const AVATAR = arg('avatar', null);
const INSTALL = process.env.LOREKEEPER_INSTALL ?? 'Z:\\AImodels\\Cloufam';

// Check counter for the --bonds branch: without it "nothing checked" would be
// printed as a successful run.
let checks = 0;
let failures = 0;
const check = (label, ok, detail = '') => {
  checks += 1;
  if (!ok) failures += 1;
  console.log(`  ${ok ? 'ок' : 'ПРОВАЛ'}: ${label}${detail ? ` — ${detail}` : ''}`);
};

/** depth_prompt from the PNG card: a tEXt chunk with the `chara` key and base64 JSON. */
function cardDepthPrompt(avatar) {
  const file = path.join(INSTALL, 'sillytavern', 'data', 'default-user', 'characters', avatar);
  if (!fs.existsSync(file)) return `карточка не найдена: ${file}`;
  const buf = fs.readFileSync(file);
  let offset = 8;
  while (offset + 12 <= buf.length) {
    const length = buf.readUInt32BE(offset);
    const type = buf.toString('ascii', offset + 4, offset + 8);
    const data = buf.subarray(offset + 8, offset + 8 + length);
    if (type === 'tEXt') {
      const zero = data.indexOf(0);
      const key = data.toString('latin1', 0, zero);
      if (key === 'chara') {
        try {
          const json = JSON.parse(
            Buffer.from(data.toString('latin1', zero + 1), 'base64').toString('utf8'),
          );
          return json?.data?.extensions?.depth_prompt?.prompt ?? '(пусто)';
        } catch (e) {
          return `не разобран JSON карточки: ${e}`;
        }
      }
    }
    if (type === 'IEND') break;
    offset += 12 + length;
  }
  return 'чанк chara не найден';
}

const list = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
// The main window shows either the application settings or the SillyTavern chat:
// in the second case we skip the IPC part and work with the extension in ST.
const page = list.find((t) => t.type === 'page' && t.url.startsWith('http://tauri.localhost'));
if (!page) {
  const stPage = list.find((t) => t.type === 'page' && /127\.0\.0\.1:8\d+/.test(t.url));
  if (!stPage) throw new Error('ни настроек приложения, ни SillyTavern в окне нет');
  console.log(`окно занято SillyTavern: ${stPage.url} — проверяю только доставку черт`);
  await diagStPage(stPage);
  process.exit(0);
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

const APP_URL = page.url;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Reload the window and wait for a page with the needed URL. After navigating to
 *  SillyTavern the same window is remote content already, and Tauri rejects its own commands
 *  ("not allowed by ACL"), so bonds are written/removed only from the application UI,
 *  and delivery to the PNG — from ST. */
async function navigate(url, match, what) {
  if (!url) return false;
  await send('Page.navigate', { url });
  for (let i = 0; i < 40; i++) {
    await sleep(1500);
    const targets = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
    if (targets.some((t) => t.type === 'page' && match.test(t.url))) return true;
  }
  console.log(`  окно не вернулось к ${what} за 60 с`);
  return false;
}

/** Wait until IPC answers in the current context again: navigation kills the
 *  execution context, the first evaluate fails because of CDP, not the application. */
async function waitIpc() {
  for (let i = 0; i < 20; i++) {
    try {
      await evalJs(`window.__TAURI_INTERNALS__.invoke('traits_state').then(() => 'ok')`);
      return true;
    } catch {
      await sleep(1500);
    }
  }
  return false;
}

await send('Runtime.enable', {});

const characters = await evalJs(`window.__TAURI_INTERNALS__.invoke('traits_state')`);
const withTags = characters.filter((c) => c.tags.length > 0);
console.log(`персонажей: ${characters.length}, с тегами: ${withTags.length}`);
for (const c of characters.slice(0, 8)) {
  console.log(
    `  • ${c.name} [${c.avatar}] — тегов ${c.tags.length}, связей ${c.bonds?.length ?? 0}, повтор: ${c.no_repeat ? 'выкл' : 'как обычно'}`,
  );
}

// Without --bonds the target is the first character with tags: with no traits there is nothing to check.
// The bonds branch also accepts a card without traits, so there the first one is taken.
const target = AVATAR
  ? characters.find((c) => c.avatar === AVATAR)
  : (withTags[0] ?? (BONDS ? characters[0] : undefined));
if (!target) {
  console.log('нет персонажа с тегами — настраивать нечего');
  socket.close();
  process.exit(BONDS ? 1 : 0);
}

console.log(`\nцель: ${target.name} [${target.avatar}]`);
console.log(`теги: ${target.tags.map((t) => `${t.tag}=${t.intensity}`).join(', ') || '—'}`);
const diskBefore = cardDepthPrompt(target.avatar);
console.log(`в карточке сейчас: ${JSON.stringify(diskBefore)}`);

if (APPLY) {
  // Lay out the intensity in ascending order, mute the first tag to zero:
  // this way both "manifest sparingly" and an explicit zero are checked.
  const tags = target.tags.map((t, i) => ({
    tag: t.tag,
    intensity: i === 0 ? 0 : Math.min(100, 20 + i * 20),
  }));
  const fragment = await evalJs(
    `window.__TAURI_INTERNALS__.invoke('traits_save', ${JSON.stringify({ avatar: target.avatar, tags, noRepeat: true })})
       .then((v) => ({ ok: true, v }))
       .catch((e) => ({ ok: false, error: String(e) }))`,
  );
  if (!fragment.ok) {
    console.log(`сохранение не прошло: ${fragment.error}`);
  } else {
    console.log(`\nсохранённый текст:\n${fragment.v}`);
    console.log(
      `\nчто видит приложение: команда traits_state → ${JSON.stringify(
        (await evalJs(`window.__TAURI_INTERNALS__.invoke('traits_state')`)).find(
          (c) => c.avatar === target.avatar,
        )?.fragment,
      )}`,
    );
    console.log(
      'страница SillyTavern откроется — расширение допишет карточку само (проверка файла ниже).',
    );
  }
}

console.log(`\nв карточке на диске: ${JSON.stringify(cardDepthPrompt(target.avatar))}`);

// ── bonds branch ────────────────────────────────────────────────────────────
const BONDS_HEADER = 'Отношения с другими персонажами';

/** Writing bonds through IPC — the same call as the editor in the "Traits" tab. */
async function saveBonds(avatar, list) {
  return evalJs(
    `window.__TAURI_INTERNALS__.invoke('bonds_save', ${JSON.stringify({ avatar, bonds: list })})
       .then((v) => ({ ok: true, v }))
       .catch((e) => ({ ok: false, error: String(e) }))`,
  );
}

/** The same string that saving returns: the preview and the card must not diverge. */
async function fragmentOf(avatar) {
  const list = await evalJs(`window.__TAURI_INTERNALS__.invoke('traits_state')`);
  return list.find((c) => c.avatar === avatar)?.fragment;
}

let bondPartner = null;
let stUrl = null;
if (BONDS) {
  bondPartner = characters.find((c) => c.avatar !== target.avatar) ?? null;
  if (!bondPartner) {
    checks += 1;
    failures += 1;
    console.log('  ПРОВАЛ: связей не проверить — в каталоге нет второй карточки');
  } else if (!APPLY) {
    console.log(
      `\nсвязи ${target.name}: ${target.bonds?.map((b) => `${b.kind}→${b.target}=${b.strength}%`).join(', ') || 'нет'} (без --apply ничего не записываю)`,
    );
  } else {
    console.log(`\nсвязи: пишу одну, на ${bondPartner.name} [${bondPartner.avatar}]`);
    const bond = {
      target: bondPartner.avatar,
      kind: 'младшая сестра',
      strength: 70,
      note: 'Ревнует к вниманию, но скрывает.',
      secret: true,
    };
    const saved = await saveBonds(target.avatar, [bond]);
    if (!saved.ok) {
      checks += 1;
      failures += 1;
      console.log(`  ПРОВАЛ: связь не сохранена — ${saved.error}`);
      bondPartner = null;
    } else {
      console.log(`сохранённый текст:\n${saved.v}`);
      check('блок есть в тексте карты', saved.v.includes(BONDS_HEADER));
      check(
        'партёр назван именем, а не файлом',
        saved.v.includes(bondPartner.name) && !saved.v.includes(bondPartner.avatar),
        `${bondPartner.name} / ${bondPartner.avatar}`,
      );
      check('тайна помечена отдельной строкой', saved.v.includes('не догадывается'));
      const seen = await fragmentOf(target.avatar);
      check(
        'приложение отдаёт тот же текст, что сохранение',
        seen === saved.v,
        String(seen).slice(0, 60),
      );
    }
  }
}

// The extension in SillyTavern lives on the chat page: to check delivery,
// we open it with the application button and wait for the extension to finish writing the card.
if (OPEN_ST) {
  const opened = await evalJs(`(() => {
    const button = [...document.querySelectorAll('button')].find((b) => b.textContent.includes('SillyTavern'));
    if (!button) return 'кнопка не найдена';
    button.click();
    return 'нажата';
  })()`);
  console.log(`\nоткрываю SillyTavern: ${opened}`);
  let stPage = null;
  for (let i = 0; i < 30 && !stPage; i++) {
    await new Promise((r) => setTimeout(r, 2000));
    const targets = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
    stPage = targets.find((t) => t.type === 'page' && /127\.0\.0\.1:8\d+/.test(t.url));
  }
  if (!stPage) {
    console.log('страница SillyTavern не открылась');
  } else {
    stUrl = stPage.url;
    console.log(`страница ST: ${stPage.url}`);
    if (process.argv.includes('--diag')) {
      await diagStPage(stPage);
    }
    let applied = '(пусто)';
    for (let i = 0; i < 20; i++) {
      await new Promise((r) => setTimeout(r, 3000));
      applied = cardDepthPrompt(target.avatar);
      if (applied && applied !== '(пусто)' && !applied.startsWith('карточка не найдена')) break;
    }
    console.log(`карточка после загрузки ST: ${JSON.stringify(applied)}`);
  }
}

// Proof of delivery is in the PNG card, not in the command reply: the file
// is rewritten by the extension in SillyTavern, so we judge only after it
// loads. The card's author text is everything before the marker of our block.
const MARKER = '<!-- cloufam: черты характера -->';
const authorOf = (text) => {
  const i = text.indexOf(MARKER);
  return i === -1 ? text : text.slice(0, i);
};

if (bondPartner) {
  if (!OPEN_ST) {
    await saveBonds(target.avatar, []);
    console.log(
      '  пропуск: доставка в PNG не судится без --open-st — пробную связь снял, не оставляя её в стейте',
    );
  } else {
    const withBond = cardDepthPrompt(target.avatar);
    check('блок доехал в PNG карточки', withBond.includes(BONDS_HEADER), withBond.slice(0, 80));
    // The bond must be removed from the application UI: in a window that went to ST, the same call
    // is treated as remote content and Tauri rejects it. Otherwise the probe would be removed as
    // a tool "failure", and the bond would stay in the user's card.
    if (
      !(await navigate(APP_URL, /^http:\/\/tauri\.localhost/, 'UI приложения')) ||
      !(await waitIpc())
    ) {
      check('снятие: окно вернулось к UI приложения', false, 'IPC не отвечает — снятие не судится');
    } else {
      const cleared = await saveBonds(target.avatar, []);
      check(
        'снятие: текст команды без блока',
        cleared.ok && !cleared.v.includes(BONDS_HEADER),
        String(cleared.error ?? cleared.v).slice(0, 80),
      );
      let offDisk = withBond;
      if (!(await navigate(stUrl, /127\.0\.0\.1:8\d+/, 'SillyTavern'))) {
        check('снятие: блок убран из PNG', false, 'ST не открылась повторно — снятие не судится');
      } else {
        for (let i = 0; i < 10 && offDisk.includes(BONDS_HEADER); i++) {
          await new Promise((r) => setTimeout(r, 3000));
          offDisk = cardDepthPrompt(target.avatar);
        }
        check('снятие: блок убран из PNG', !offDisk.includes(BONDS_HEADER), offDisk.slice(0, 80));
        check(
          'снятие: авторский текст карточки цел',
          authorOf(diskBefore) === authorOf(offDisk),
          `${authorOf(diskBefore).slice(0, 40)} ↔ ${authorOf(offDisk).slice(0, 40)}`,
        );
      }
    }
  }
}

console.log(`\nпроверок: ${checks}, провалов: ${failures}`);
process.exitCode = failures > 0 ? 1 : 0;
socket.close();

/** What is visible from the SillyTavern page side: the extension, characters, route. */
async function diagStPage(stPage) {
  const st = new ws(stPage.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => {
    st.once('open', resolve);
    st.once('error', reject);
  });
  let stId = 0;
  const stPending = new Map();
  st.on('message', (raw) => {
    const msg = JSON.parse(raw.toString());
    const entry = stPending.get(msg.id);
    if (!entry) return;
    stPending.delete(msg.id);
    entry(msg);
  });
  const stEval = (expression) =>
    new Promise((resolve) => {
      const i = ++stId;
      stPending.set(i, (msg) =>
        resolve(
          msg.result?.exceptionDetails
            ? `ошибка: ${JSON.stringify(msg.result.exceptionDetails).slice(0, 300)}`
            : msg.result?.result?.value,
        ),
      );
      st.send(
        JSON.stringify({
          id: i,
          method: 'Runtime.evaluate',
          params: { expression, awaitPromise: true, returnByValue: true },
        }),
      );
    });
  await stEval('1');
  console.log(`  __lkApplyTraits: ${await stEval('typeof window.__lkApplyTraits')}`);
  console.log(
    `  роут плагина: ${await stEval("fetch('/api/plugins/cloufam/traits').then(async (r) => r.status + ' ' + (await r.text()).slice(0, 200))")}`,
  );
  console.log(`  персонажей на странице: ${await stEval('(window.characters ?? []).length')}`);
  console.log(
    `  вызов применения: ${await stEval('window.__lkApplyTraits ? window.__lkApplyTraits().then(() => "ок") : "нет функции"')}`,
  );
  st.close();
}

