// Live check of trading, quest generation and NPCs on a running app.
// Run: the app is already open with CDP 9223, the ST page is reachable.
//   LOREKEEPER_INSTALL=Z:\AImodels\Cloufam node tools/lk-rpg-features-check.mjs
// Data: the world is started and reset at the end; purchases use native payment,
// so back up data/*.json before the run (the stand does it: see BACKUP).
import fs from 'node:fs';
import path from 'node:path';
import { loadWs } from './ws-resolve.mjs';

const ROOT = path.resolve(import.meta.dirname, '..');
const ws = loadWs(ROOT);
const PORT = 9223;
const OUT = path.join(ROOT, 'stand-out', 'live-ui');
fs.mkdirSync(OUT, { recursive: true });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function targets() {
  return (await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json()).filter(
    (t) => t.type === 'page',
  );
}

async function connect(page) {
  const socket = new ws(page.webSocketDebuggerUrl);
  await new Promise((res, rej) => {
    socket.once('open', res);
    socket.once('error', rej);
  });
  let id = 0;
  const pending = new Map();
  const exceptions = [];
  socket.on('message', (raw) => {
    const msg = JSON.parse(raw.toString());
    if (msg.method === 'Runtime.exceptionThrown') {
      exceptions.push(msg.params?.exceptionDetails?.exception?.description ?? 'exception');
    }
    const entry = pending.get(msg.id);
    if (entry) {
      pending.delete(msg.id);
      entry(msg);
    }
  });
  const send = (method, params = {}) =>
    new Promise((resolve) => {
      const i = ++id;
      pending.set(i, resolve);
      socket.send(JSON.stringify({ id: i, method, params }));
    });
  await send('Runtime.enable');
  await send('Page.enable');
  return { socket, send, exceptions };
}

async function evalJs(send, expression, awaitPromise = true) {
  const res = await send('Runtime.evaluate', {
    expression,
    awaitPromise,
    returnByValue: true,
  });
  if (res.result?.exceptionDetails) throw new Error(JSON.stringify(res.result.exceptionDetails));
  return res.result?.result?.value;
}

async function shot(send, name) {
  const res = await send('Page.captureScreenshot', { format: 'png' });
  const file = path.join(OUT, `${name}.png`);
  fs.writeFileSync(file, Buffer.from(res.result.data, 'base64'));
  return file;
}

const list = await targets();
const stPage = list.find((t) => /127\.0\.0\.1:800\d/.test(t.url));
if (!stPage) throw new Error(`нет страницы ST: ${list.map((t) => t.url).join(', ')}`);
const st = await connect(stPage);
await sleep(1500);

const api = (method, url, body) =>
  `fetch(window.__LK_GATEWAY + '${url}', { method: '${method}', headers: ${body ? "{ 'Content-Type': 'application/json' }" : '{}'}, ${body ? `body: JSON.stringify(${JSON.stringify(body)})` : ''} }).then(async (r) => ({ status: r.status, text: await r.text() }))`;

async function call(method, url, body) {
  const raw = await evalJs(st.send, `(${api(method, url, body)})`);
  let json = null;
  try {
    json = JSON.parse(raw.text);
  } catch {}
  return { status: raw.status, json, text: raw.text };
}

const results = [];
const check = (name, ok, details) => {
  results.push({ name, ok: !!ok, details });
  console.log(
    `${ok ? 'ПРОШЛО' : 'ПРОВАЛ'}: ${name} — ${typeof details === 'string' ? details : JSON.stringify(details)}`,
  );
};

// 1. World and moving to the tavern (the shop is there).
const start = await call('POST', '/api/cloufam/rpg/world/start');
check('мир начинается', start.status === 200, start.status);
const move = await call('POST', '/api/cloufam/rpg/world/move', { location: 'tavern' });
check('переход в таверну', move.status === 200, move.status);

// 2. The counter shows up in the inventory.
const inv = await call('GET', '/api/cloufam/rpg/inventory');
const shops = inv.json?.shops ?? [];
check(
  'прилавок торговца виден',
  inv.status === 200 && shops.some((s) => s.npc === 'tavern_keeper' && (s.items ?? []).length > 0),
  shops.map((s) => `${s.npc}:${s.items?.length ?? 0}`).join(', ') || 'пусто',
);
const shelf = shops.find((s) => s.npc === 'tavern_keeper');
const item = shelf?.items?.find((i) => i.id === 'health_potion') ?? shelf?.items?.[0];
check(
  'у товара есть цена',
  Number(item?.price) > 0,
  item ? `${item.name} — ${item.price}` : 'нет товара',
);

// 3. Purchase: either goes through or honestly refuses over values.
const trade = await call('POST', '/api/cloufam/rpg/trade', {
  npc: shelf?.npc,
  item: item?.id,
  quantity: 1,
});
check(
  'покупка отвечает осмысленно',
  trade.status === 200 || /ценност|не хватает|нет в наличии/i.test(trade.text),
  `${trade.status} ${trade.text.slice(0, 140)}`,
);

// 4. Quest generation (model, up to 2 minutes).
const quest = await call('POST', '/api/cloufam/rpg/quests/generate');
check(
  'квест генерируется',
  quest.status === 200 && !!quest.json?.quest?.title,
  `${quest.status} ${JSON.stringify(quest.json?.quest?.title ?? quest.text.slice(0, 120))}`,
);

// 5. NPC generation.
const npc = await call('POST', '/api/cloufam/rpg/npcs/generate');
check(
  'NPC генерируется',
  npc.status === 200 && !!npc.json?.npc?.name,
  `${npc.status} ${npc.json?.npc?.name ?? npc.text.slice(0, 120)}`,
);

// 6. Panel: an active character first — without one the panel refuses with a toast.
await sleep(1200);
const selected = await evalJs(
  st.send,
  `(async () => {
    const ctx = SillyTavern.getContext();
    const index = ctx.characters.findIndex((c) => c.avatar === 'default_Assistant.png');
    if (index < 0) return null;
    await ctx.selectCharacterById(index);
    return index;
  })()`,
);
check('активный персонаж выбран', selected !== null, selected);
await sleep(1500);
await evalJs(
  st.send,
  `(() => { window.confirm = () => true; document.getElementById('cloufam-rpg-menu-item')?.click(); return true; })()`,
);
let modalOpen = false;
for (let i = 0; i < 40 && !modalOpen; i += 1) {
  modalOpen = await evalJs(st.send, `!!document.getElementById('cloufam-rpg-modal')`);
  if (!modalOpen) await sleep(500);
}
check('панель RPG открыта', modalOpen, modalOpen ? 'модалка есть' : 'модалки нет');

if (modalOpen) {
  await evalJs(st.send, `document.querySelector('[data-lk-tab="inventory"]')?.click(); true`);
  await sleep(900);
  const invText = await evalJs(
    st.send,
    `document.querySelector('.lk-rpg-tab-panel[data-lk-panel="inventory"]')?.innerText ?? ''`,
  );
  check(
    'в инвентаре виден прилавок с кнопкой покупки',
    /🏪/.test(invText) && /Купить/.test(invText),
    invText.replace(/\s+/g, ' ').slice(0, 120),
  );
  await shot(st.send, 'rpg-inventory');
  await evalJs(st.send, `document.querySelector('[data-lk-tab="quests"]')?.click(); true`);
  await sleep(900);
  const questText = await evalJs(
    st.send,
    `document.querySelector('.lk-rpg-tab-panel[data-lk-panel="quests"]')?.innerText ?? ''`,
  );
  const questTitle = String(quest.json?.quest?.title ?? '');
  check(
    'во вкладке квестов есть генерация и созданный квест',
    /Сгенерировать квест/.test(questText) &&
      questTitle.length > 0 &&
      questText.includes(questTitle),
    `«${questTitle}» в панели: ${questText.includes(questTitle)}`,
  );
  await shot(st.send, 'rpg-quests');
  await evalJs(
    st.send,
    `document.querySelector('#cloufam-rpg-modal [data-lk-close="1"]')?.click(); true`,
  );
}

// 7. Thought style: current and a variant without italics with a background.
const injected = await evalJs(
  st.send,
  `(() => {
    const last = document.querySelector('#chat .mes:last-of-type');
    if (!last) return 'нет сообщения';
    const clone = last.cloneNode(true);
    clone.id = 'lk-style-probe';
    clone.dataset.lkProbe = '1';
    const text = clone.querySelector('.mes_text');
    if (text) {
      text.innerHTML = 'Она вздыхает и отводит взгляд. <span class="custom-lk-thought">Он опять забыл про обещание…</span> <em>*опускает плечи*</em> «Ладно. Рассказывай».';
    }
    last.parentElement.appendChild(clone);
    return 'ок';
  })()`,
);
check('проба стиля вставлена', injected === 'ок', injected);
await sleep(400);
const currentShot = await shot(st.send, 'thoughts-current');
await evalJs(
  st.send,
  `(() => {
    const style = document.createElement('style');
    style.id = 'lk-style-probe-css';
    style.textContent = '.custom-lk-thought, .lk-thought { font-style: normal !important; background: rgba(249,168,212,0.12); border-radius: 4px; padding: 0 2px; }';
    document.head.appendChild(style);
    return true;
  })()`,
);
await sleep(300);
const variantShot = await shot(st.send, 'thoughts-variant');
await evalJs(
  st.send,
  `(() => {
    document.getElementById('lk-style-probe')?.remove();
    document.getElementById('lk-style-probe-css')?.remove();
    return true;
  })()`,
);

// 8. The world is reset: no foreign data is left behind.
const reset = await call('POST', '/api/cloufam/rpg/world/reset');
check('мир сброшен', reset.status === 200, reset.status);

const failed = results.filter((r) => !r.ok).length;
console.log(`\n=== итог: ${results.length - failed}/${results.length} ===`);
console.log(
  `мысли: ${path.basename(currentShot)} (текущий), ${path.basename(variantShot)} (вариант)`,
);
console.log(`исключения ST: ${st.exceptions.length}`);
st.socket.close();
process.exit(failed ? 1 : 0);

