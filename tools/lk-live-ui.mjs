// Live visual UI check: launcher, SillyTavern page, settings window.
// Run: the application is already up with --remote-debugging-port=9223.
//   node tools/lk-live-ui.mjs [--port 9223] [--out stand-out/live-ui]
import fs from 'node:fs';
import path from 'node:path';
import { loadWs } from './ws-resolve.mjs';

const ROOT = path.resolve(import.meta.dirname, '..');
const ws = loadWs(ROOT);
const arg = (name, fallback) => {
  const i = process.argv.indexOf(`--${name}`);
  return i === -1 ? fallback : process.argv[i + 1];
};
const PORT = Number(arg('port', 9223));
const OUT = path.resolve(ROOT, arg('out', 'stand-out/live-ui'));
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

async function shot(send, name) {
  const res = await send('Page.captureScreenshot', { format: 'png' });
  const file = path.join(OUT, `${name}.png`);
  fs.writeFileSync(file, Buffer.from(res.result.data, 'base64'));
  return file;
}

async function evalJs(send, expression) {
  const res = await send('Runtime.evaluate', {
    expression,
    awaitPromise: true,
    returnByValue: true,
  });
  if (res.result?.exceptionDetails) throw new Error(JSON.stringify(res.result.exceptionDetails));
  return res.result?.result?.value;
}

// 1. Launcher.
let list = await targets();
const launcher = list.find((t) => t.url.startsWith('http://tauri.localhost'));
if (!launcher) throw new Error(`нет страницы лаунчера: ${list.map((t) => t.url).join(', ')}`);
const app = await connect(launcher);
await sleep(1500);
const launcherShot = await shot(app.send, 'launcher');
const launcherInfo = await evalJs(
  app.send,
  `({
  title: document.title,
  heading: document.querySelector('h1,h2')?.innerText ?? '',
  buttons: [...document.querySelectorAll('button')].map(b => b.innerText.trim().replace(/\\s+/g,' ')).filter(Boolean).slice(0, 10),
})`,
);
console.log('лаунчер:', JSON.stringify({ ...launcherInfo, shot: launcherShot }));

// 2. Open SillyTavern.
const opened = await evalJs(
  app.send,
  `(() => {
  const el = [...document.querySelectorAll('button')].find(b => /Открыть SillyTavern|Открыть чат|SillyTavern/i.test(b.innerText));
  if (!el) return false;
  el.click();
  return true;
})()`,
);
console.log('клик открытия ST:', opened);
await sleep(4000);
list = await targets();
const stPage = list.find((t) => /127\.0\.0\.1:800\d/.test(t.url));
if (stPage) {
  const st = await connect(stPage);
  await sleep(2500);
  const stShot = await shot(st.send, 'sillytavern');
  const stInfo = await evalJs(
    st.send,
    `({
    title: document.title,
    messages: document.querySelectorAll('#chat .mes').length,
    hasChat: !!document.querySelector('#chat'),
    menuItems: [...document.querySelectorAll('#extensionsMenu .list-group-item, #extensionsMenu [data-i18n]')].length,
  })`,
  );
  console.log(
    'ST:',
    JSON.stringify({ ...stInfo, shot: stShot, exceptions: st.exceptions.slice(0, 5) }),
  );
  st.socket.close();
} else {
  console.log('ST-страница не найдена, есть:', list.map((t) => t.url).join(', '));
}

// 3. Settings window through the gateway (the same path as the menu item in ST).
const status = await evalJs(
  app.send,
  `fetch(window.__LK_GATEWAY + '/api/cloufam/open-settings', {
  method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ tab: 'tags' })
}).then(r => r.status).catch(e => String(e))`,
);
console.log('open-settings:', status);
await sleep(4000);
list = await targets();
// A second window of the same origin: take a page different from the launcher by id.
const second = list.find((t) => t.url.startsWith('http://tauri.localhost') && t.id !== launcher.id);
if (second) {
  const st = await connect(second);
  await sleep(2000);
  const settingsShot = await shot(st.send, 'settings');
  const info = await evalJs(
    st.send,
    `({
    title: document.title,
    tabs: document.querySelectorAll('[class*=tab], .settings-tab, nav button').length,
    heading: document.querySelector('h1,h2')?.innerText ?? '',
    body: document.body.innerText.slice(0, 300),
  })`,
  );
  console.log(
    'настройки:',
    JSON.stringify({ ...info, shot: settingsShot, exceptions: st.exceptions.slice(0, 5) }),
  );
  st.socket.close();
} else {
  console.log('окно настроек не появилось, есть:', list.map((t) => `${t.id}:${t.url}`).join(', '));
}

console.log('исключения лаунчера:', JSON.stringify(app.exceptions.slice(0, 5)));
app.socket.close();
process.exit(0);

