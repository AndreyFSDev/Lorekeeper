// Smoke check of the SillyTavern group chat over CDP.
//
// Launches Edge with remote debugging, enters the live ST (port 8000 of the
// running app), creates a room with two characters, sends one
// user message and captures the resulting chat: who replied, whether there are empty
// messages or truncation at the length limit.
//
// Usage: node tools/st-group-check.mjs   (the app must be running)
import { spawn } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import path from 'node:path';
import { loadWs } from './ws-resolve.mjs';

const require = createRequire(import.meta.url);
const ROOT = path.resolve(import.meta.dirname, '..');
const ws = loadWs(ROOT);

const ST_URL = process.env.ST_URL ?? 'http://127.0.0.1:8000';
const CDP_PORT = Number(process.env.CDP_PORT ?? 9223);
// Keep the browser profile OUTSIDE the project folder: the dev-mode Vite watcher tries
// to watch the whole tree and fails with EBUSY on Edge temp files.
const PROFILE = path.resolve(ROOT, '..', '..', 'research', 'cloufam-ui-check', 'cdp-profile');
const EDGE_CANDIDATES = [
  'C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe',
  'C:\\Program Files\\Microsoft\\Edge\\Application\\msedge.exe',
];
const MEMBERS = (process.env.MEMBERS ?? 'default_Seraphina.png,default_Assistant.png').split(',');
const GROUP_NAME = process.env.GROUP_NAME ?? 'Проверка комнаты';
const USER_MESSAGE =
  process.env.MESSAGE ??
  'Фубуки, Крони — расскажите по очереди, чем вы заняты сегодня. Каждая отвечает один раз, одним абзацем.';
const GENERATION_TIMEOUT_MS = Number(process.env.TIMEOUT_MS ?? 900000);
const PROXY_URL = process.env.PROXY_URL ?? ''; // for example http://127.0.0.1:8010/v1

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

class Cdp {
  constructor(socket) {
    this.socket = socket;
    this.seq = 0;
    this.pending = new Map();
    this.events = [];
    socket.on('message', (raw) => {
      const msg = JSON.parse(raw.toString());
      if (msg.method) {
        this.events.push(msg);
        return;
      }
      const entry = this.pending.get(msg.id);
      if (!entry) return;
      this.pending.delete(msg.id);
      if (msg.error)
        entry.reject(new Error(`${msg.error.message} (${JSON.stringify(msg.error.data ?? {})})`));
      else entry.resolve(msg.result);
    });
  }

  /// Page errors and warnings accumulated since the previous call.
  drainEvents() {
    const lines = [];
    for (const event of this.events.splice(0)) {
      if (event.method === 'Runtime.consoleAPICalled') {
        const type = event.params.type;
        if (type !== 'error' && type !== 'warning') continue;
        const text = event.params.args.map((a) => a.value ?? a.description ?? a.type).join(' ');
        lines.push(`[${type}] ${text}`);
      } else if (event.method === 'Runtime.exceptionThrown') {
        const d = event.params.exceptionDetails;
        lines.push(`[исключение] ${d.exception?.description ?? d.text}`);
      }
    }
    return lines;
  }

  send(method, params = {}) {
    const id = ++this.seq;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.socket.send(JSON.stringify({ id, method, params }));
      setTimeout(() => {
        if (this.pending.delete(id)) reject(new Error(`CDP ${method}: таймаут`));
      }, 60000);
    });
  }

  /// Evaluates an expression in the page; page errors become an exception.
  async evaluate(expression) {
    const res = await this.send('Runtime.evaluate', {
      expression,
      awaitPromise: true,
      returnByValue: true,
    });
    if (res.exceptionDetails) {
      const text = res.exceptionDetails.exception?.description ?? res.exceptionDetails.text;
      throw new Error(`ошибка в странице: ${text}`);
    }
    return res.result.value;
  }
}

async function findPageTarget() {
  for (let i = 0; i < 120; i++) {
    try {
      const res = await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`);
      const targets = await res.json();
      const page = targets.find((t) => t.type === 'page' && t.url.startsWith(ST_URL));
      if (page) return page;
    } catch {
      // Edge has not opened the debug port yet
    }
    await sleep(1000);
  }
  throw new Error('страница ST в Edge не появилась');
}

async function waitReady(cdp) {
  for (let i = 0; i < 120; i++) {
    const ready = await cdp.evaluate(`(() => {
            const ctx = window.SillyTavern?.getContext?.();
            const ta = document.querySelector('#send_textarea');
            const loading = document.querySelector('#loading_mes');
            return !!(
                ta && ctx && Array.isArray(ctx.characters) && ctx.characters.length > 0 &&
                (!loading || getComputedStyle(loading).display === 'none')
            );
        })()`);
    if (ready) {
      // ST modules finish initializing after the context appears:
      // a dynamic import at that moment fails on circular dependencies.
      await sleep(4000);
      return;
    }
    await sleep(1000);
  }
  throw new Error('интерфейс ST не догрузился');
}

async function createGroup(cdp, members) {
  const payload = JSON.stringify({ name: GROUP_NAME, members, activation_strategy: 0 });
  return cdp.evaluate(`(async () => {
        const tok = await (await fetch('/csrf-token')).json();
        const res = await fetch('/api/groups/create', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json', 'X-CSRF-Token': tok.token },
            body: ${JSON.stringify(payload)},
        });
        return await res.json();
    })()`);
}

async function openGroup(cdp, id) {
  // openGroupById silently returns false if the chat is being saved at that moment
  // or a generation is running, so we retry persistently and verify the group selection:
  // otherwise the test silently lands in a previously opened single chat and "checks" the wrong thing.
  let lastState = null;
  for (let attempt = 0; attempt < 20; attempt++) {
    lastState = await cdp.evaluate(`(async () => {
            const g = await import('/scripts/group-chats.js');
            await g.getGroups();
            const opened = await g.openGroupById(${JSON.stringify(id)});
            return {
                opened,
                selected: g.selected_group,
                known: g.groups.map((x) => x.id),
                messages: document.querySelectorAll('#chat .mes').length,
            };
        })()`);
    if (lastState.selected === id) return lastState;
    await sleep(2000);
  }
  throw new Error(`группа ${id} не выбралась в интерфейсе: ${JSON.stringify(lastState)}`);
}

async function sendMessage(cdp, text) {
  if (process.env.DISABLE_GROUP_TRIM === '1') {
    // ST truncates a group reply at the first foreign name (cleanGroupMessage):
    // if the model started with another participant's name, the message becomes empty.
    await cdp.evaluate(`(async () => {
            const s = await import('/script.js');
            s.power_user.disable_group_trimming = true;
            return s.power_user.disable_group_trimming;
        })()`);
  }
  // ST's send button lives under a mutex and silently ignores a click if
  // the UI is not ready yet, so we retry several times and verify the effect.
  for (let attempt = 1; attempt <= 6; attempt++) {
    const diag = await cdp.evaluate(`(async () => {
            const info = { attempt: ${attempt}, before: document.querySelectorAll('#chat .mes').length };
            const ta = document.querySelector('#send_textarea');
            ta.value = ${JSON.stringify(text)};
            ta.dispatchEvent(new Event('input', { bubbles: true }));
            ta.dispatchEvent(new Event('change', { bubbles: true }));
            const scriptModule = await import('/script.js');
            await scriptModule.sendTextareaMessage();
            await new Promise((r) => setTimeout(r, 3000));
            info.after = document.querySelectorAll('#chat .mes').length;
            info.valueAfter = document.querySelector('#send_textarea')?.value?.length ?? -1;
            info.toasts = [...document.querySelectorAll('.toast')].map((t) => t.textContent.trim()).slice(0, 2);
            return info;
        })()`);
    const sent = diag.after !== diag.before || diag.valueAfter === 0;
    console.log(
      `  отправка (попытка ${attempt}): ${JSON.stringify(diag)}${sent ? '' : ' — не ушла'}`,
    );
    if (sent) return diag;
    const events = cdp.drainEvents();
    if (events.length)
      console.log(`  сообщения страницы:\n    ${events.slice(0, 12).join('\n    ')}`);
    const state = await cdp.evaluate(`(() => {
            const ctx = window.SillyTavern?.getContext?.() ?? {};
            return {
                chat: ctx.chat?.length ?? null,
                characters: ctx.characters?.length ?? null,
                selectedGroup: ctx.selectedGroup?.id ?? null,
                mainApi: ctx.mainApi ?? null,
                onlineStatus: ctx.onlineStatus ?? null,
                chatCompletionSource: ctx.chatCompletionSettings?.chat_completion_source ?? null,
                customUrl: ctx.chatCompletionSettings?.custom_url ?? null,
            };
        })()`);
    console.log(`  состояние ST: ${JSON.stringify(state)}`);
    await sleep(3000);
  }
  throw new Error('реплика пользователя не уходит из интерфейса');
}

async function cleanupGroups(cdp, name) {
  return cdp.evaluate(`(async () => {
        const g = await import('/scripts/group-chats.js');
        await g.getGroups();
        const doomed = g.groups.filter((x) => x.name === ${JSON.stringify(name)});
        let deleted = 0;
        for (const grp of doomed) {
            try {
                await g.deleteGroup(grp.id);
                deleted++;
            } catch (e) {
                console.warn('не удалилась группа', grp.id, e);
            }
        }
        return { found: doomed.length, deleted };
    })()`);
}

const ST_SETTINGS = path.join(ROOT, 'sillytavern', 'data', 'default-user', 'settings.json');

/// Switches the LLM address directly in the ST client's memory.
/// Reloading the page is not suitable for this: on unload ST saves
/// settings from memory, and if the client has not finished loading — writes defaults
/// (main_api: kobold, context 2048) over the working ones, after which generation
/// silently stops working.
async function pointStAtProxy(cdp, url) {
  if (!url) return null;
  const state = await cdp.evaluate(`(async () => {
        const oai = await import('/scripts/openai.js');
        const previous = oai.oai_settings.custom_url;
        oai.oai_settings.custom_url = ${JSON.stringify(url)};
        return { previous, current: oai.oai_settings.custom_url };
    })()`);
  return state;
}

async function restoreStUrl(cdp, previous) {
  if (!previous) return;
  await cdp.evaluate(`(async () => {
        const oai = await import('/scripts/openai.js');
        oai.oai_settings.custom_url = ${JSON.stringify(previous)};
        return oai.oai_settings.custom_url;
    })()`);
}

async function snapshot(cdp) {
  return cdp.evaluate(`(async () => {
        const g = await import('/scripts/group-chats.js');
        const stop = document.querySelector('#mes_stop');
        const stopVisible = !!(stop && getComputedStyle(stop).display !== 'none');
        const messages = [...document.querySelectorAll('#chat .mes')].map((el) => ({
            name: el.getAttribute('ch_name') ?? el.querySelector('.name_text')?.textContent?.trim() ?? '',
            isUser: el.getAttribute('is_user') === 'true',
            isSystem: el.getAttribute('is_system') === 'true',
            text: (el.querySelector('.mes_text')?.innerText ?? '').trim(),
        }));
        return { generating: !!g.is_group_generating || stopVisible, messages };
    })()`);
}

async function waitIdle(cdp, { minMessages = 0, quietPolls = 3 } = {}) {
  const deadline = Date.now() + GENERATION_TIMEOUT_MS;
  let lastCount = -1;
  let stable = 0;
  while (Date.now() < deadline) {
    const { generating, messages } = await snapshot(cdp);
    if (messages.length === lastCount && !generating && messages.length >= minMessages) {
      if (++stable >= quietPolls) return messages;
    } else {
      stable = 0;
    }
    lastCount = messages.length;
    await sleep(3000);
  }
  throw new Error('интерфейс не успокоился за отведённое время');
}

async function waitForUserMessage(cdp, text) {
  for (let i = 0; i < 30; i++) {
    const found = await cdp.evaluate(
      `[...document.querySelectorAll('#chat .mes')].some((el) =>
                el.getAttribute('is_user') === 'true' &&
                (el.querySelector('.mes_text')?.innerText ?? '').includes(${JSON.stringify(text.slice(0, 40))}))`,
    );
    if (found) return true;
    await sleep(1000);
  }
  throw new Error('реплика пользователя не появилась в чате');
}

async function waitForGroupGeneration(cdp) {
  const deadline = Date.now() + GENERATION_TIMEOUT_MS;
  let lastCount = -1;
  let stable = 0;
  let sawReply = false;
  while (Date.now() < deadline) {
    await sleep(3000);
    const { generating, messages } = await snapshot(cdp);
    const replies = messages.filter((m) => !m.isUser).length;
    if (replies > 0) sawReply = true;
    if (messages.length === lastCount && !generating && sawReply) {
      stable++;
      if (stable >= 3) return messages;
    } else {
      stable = 0;
    }
    lastCount = messages.length;
    process.stdout.write(
      `\r  реплик: ${messages.length} (ответов персонажей: ${replies}), генерация: ${generating ? 'идёт' : 'нет'}   `,
    );
  }
  throw new Error('групповая генерация не завершилась за отведённое время');
}

function report(messages, members) {
  const names = new Set(members.map((m) => m.replace(/\.png$/i, '').replace(/_/g, ' ')));
  const replies = messages.filter((m) => !m.isUser && !m.isSystem);
  const empty = replies.filter((m) => m.text.length === 0);
  const speakers = replies.map((m) => m.name);

  console.log('\n\n=== ЧАТ ===');
  for (const m of messages) {
    const who = m.isUser ? 'ПОЛЬЗОВАТЕЛЬ' : m.isSystem ? 'СИСТЕМА' : m.name;
    const body =
      m.text.length > 600 ? `${m.text.slice(0, 600)}…[всего ${m.text.length} символов]` : m.text;
    console.log(`\n[${who}] (${m.text.length} символов)\n${body}`);
  }

  console.log('\n=== ПРОВЕРКИ ===');
  console.log(`участников в комнате: ${members.length}, ответов персонажей: ${replies.length}`);
  console.log(`говорили: ${[...new Set(speakers)].join(', ') || '—'}`);
  console.log(`пустых реплик: ${empty.length}`);
  const outsiders = replies.filter((m) => m.name && !names.has(m.name));
  console.log(
    `ответов от посторонних имён: ${outsiders.length}${outsiders.length ? ` (${outsiders.map((o) => o.name).join(', ')})` : ''}`,
  );
  const cut = replies.filter((m) => m.text.length > 0 && !/[.!?…»"*)\]]$/.test(m.text));
  console.log(
    `оборванных реплик (не заканчиваются знаком/разметкой): ${cut.length}${cut.length ? ` → ${cut.map((c) => c.name).join(', ')}` : ''}`,
  );

  // Cross-speaking sign: inside a character's message a line with
  // another participant's name starts ("Name: ...") — so the model wrote for them.
  const crossTalk = [];
  for (const m of replies) {
    for (const other of names) {
      if (other === m.name) continue;
      const re = new RegExp(`(^|\\n)\\s*${other.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}\\s*:`, 'i');
      if (re.test(m.text)) crossTalk.push(`${m.name} → писал за ${other}`);
    }
  }
  console.log(
    `перекрёстные реплики (писал за другого): ${crossTalk.length}${crossTalk.length ? ` → ${crossTalk.join('; ')}` : ''}`,
  );
  return {
    replies: replies.length,
    empty: empty.length,
    cut: cut.length,
    crossTalk: crossTalk.length,
  };
}

async function main() {
  const edge = EDGE_CANDIDATES.find((p) => existsSync(p));
  if (!edge) throw new Error('Edge не найден');
  if (existsSync(PROFILE)) rmSync(PROFILE, { recursive: true, force: true });
  mkdirSync(PROFILE, { recursive: true });

  const child = spawn(
    edge,
    [
      '--headless=new',
      `--remote-debugging-port=${CDP_PORT}`,
      `--user-data-dir=${PROFILE}`,
      '--no-first-run',
      '--no-default-browser-check',
      '--disable-extensions',
      '--window-size=1440,900',
      ST_URL,
    ],
    { stdio: 'ignore', detached: false },
  );

  let cdp;
  try {
    const target = await findPageTarget();
    const socket = new ws(target.webSocketDebuggerUrl, { maxPayload: 64 * 1024 * 1024 });
    await new Promise((resolve, reject) => {
      socket.once('open', resolve);
      socket.once('error', reject);
    });
    cdp = new Cdp(socket);
    await cdp.send('Runtime.enable');
    await cdp.send('Page.enable');

    await waitReady(cdp);
    if (PROXY_URL) {
      const switched = await pointStAtProxy(cdp, PROXY_URL);
      console.log(`ST переключён на прокси (было: ${switched?.previous}): ${PROXY_URL}`);
    }
    console.log('ST загрузился, чищу прошлые тестовые комнаты…');
    console.log(`  удалено: ${JSON.stringify(await cleanupGroups(cdp, GROUP_NAME))}`);
    const group = await createGroup(cdp, MEMBERS);
    console.log(`комната ${group.id} (${group.name}), участники: ${MEMBERS.join(', ')}`);
    const opened = await openGroup(cdp, group.id);
    // A fresh room generates participants' greetings itself — wait until
    // it settles, otherwise the user message would land during generation.
    const greeting = await waitIdle(cdp, { minMessages: opened.messages });
    console.log(
      `комната открыта: стартовых реплик ${greeting.length}, отправляю реплику пользователя…`,
    );
    await sendMessage(cdp, USER_MESSAGE);
    await waitForUserMessage(cdp, USER_MESSAGE);
    const messages = await waitForGroupGeneration(cdp);
    const summary = report(messages, MEMBERS);
    console.log(`\nИТОГ: ${JSON.stringify(summary)}`);
    console.log(`GROUP_ID=${group.id}`);
    if (process.env.KEEP === '1') {
      console.log('KEEP=1: тестовая комната оставлена для разбора');
    } else {
      const cleaned = await cleanupGroups(cdp, GROUP_NAME);
      console.log(`тестовые комнаты удалены: ${JSON.stringify(cleaned)}`);
    }
  } finally {
    try {
      cdp?.socket.close();
    } catch {
      // socket already closed
    }
    child.kill();
    await sleep(1500);
    try {
      spawn('taskkill', ['/PID', String(child.pid), '/T', '/F'], { stdio: 'ignore' });
    } catch {
      // process already dead
    }
  }
}

main().catch((err) => {
  console.error(`\nОШИБКА: ${err.message}`);
  process.exit(1);
});

