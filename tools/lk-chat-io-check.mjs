// Stand: chat export/import and a SillyTavern update check against a live ST (CDP 9223).
// 1) "Chat export" — clicking the menu item; the file must appear in data/chat-backups
//    and parse as ST jsonl. 2) Import — the window.__lkImportChatFile hook with the same
//    file: ST accepts it through its regular mechanism; the chat file the stand created
//    is deleted, the user's state is untouched. 3) /api/cloufam/st/update-check
//    — the route responds from the ST page (which also exercises the CORS layer). The
//    update itself is not run by the stand: it replaces ST code.
//
// Run: the app with WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9223,
// LOREKEEPER_INSTALL=Z:\AImodels\Cloufam (install root + the ws module).
import fs from 'node:fs';
import path from 'node:path';
import { loadWs } from './ws-resolve.mjs';

const INSTALL = process.env.LOREKEEPER_INSTALL ?? 'Z:\\AImodels\\Cloufam';
process.env.LOREKEEPER_INSTALL = INSTALL;
const ws = loadWs(path.resolve(import.meta.dirname, '..'));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const list = await fetch('http://127.0.0.1:9223/json/list').then((r) => r.json());
const page =
  list.find((t) => t.type === 'page' && t.url.includes('8001')) ??
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
  const msg = JSON.parse(raw);
  if (msg.id && pending.has(msg.id)) {
    const { resolve, reject } = pending.get(msg.id);
    pending.delete(msg.id);
    if (msg.error) reject(new Error(msg.error.message));
    else resolve(msg.result);
  }
});

const evaljs = async (expression) => {
  const out = await send('Runtime.evaluate', {
    expression,
    returnByValue: true,
    awaitPromise: true,
  });
  return out.exceptionDetails
    ? { error: out.exceptionDetails.exception?.description ?? out.exceptionDetails.text }
    : out.result?.value;
};

function send(method, params = {}) {
  return new Promise((resolve, reject) => {
    const id = ++seq;
    pending.set(id, { resolve, reject });
    s.send(JSON.stringify({ id, method, params }));
  });
}

// 0. Open ST from the app launcher.
let state = await evaljs(`({ href: location.href, hasChat: !!document.querySelector('#chat') })`);
if (!state?.hasChat) {
  await evaljs(`(() => {
    const b = document.querySelector('button.btn-launch-st')
      || [...document.querySelectorAll('button')].find(el => (el.innerText || '').includes('Открыть SillyTavern'));
    if (!b) return 'no_button';
    b.click();
    return 'clicked';
  })()`);
  for (let i = 0; i < 30; i++) {
    await sleep(1000);
    state = await evaljs(`({ href: location.href, hasChat: !!document.querySelector('#chat') })`);
    if (state?.hasChat) break;
  }
}
if (!state?.hasChat) throw new Error('ST не открылся из лаунчера приложения');
await sleep(1500);

const report = {};

// 1. Export: click the wand menu item.
const backupDir = path.join(INSTALL, 'data', 'chat-backups');
const before = fs.existsSync(backupDir) ? fs.readdirSync(backupDir) : [];
const clicked = await evaljs(`(() => {
  const item = document.querySelector('#cloufam-chat-export-menu-item');
  if (!item) return 'no_item';
  item.click();
  return 'clicked';
})()`);
report.exportClicked = clicked;
for (let i = 0; i < 15; i++) {
  await sleep(1000);
  if (fs.existsSync(backupDir) && fs.readdirSync(backupDir).length > before.length) break;
}
const after = fs.existsSync(backupDir) ? fs.readdirSync(backupDir) : [];
const created = after.filter((f) => !before.includes(f));
report.exportFiles = created;
if (created.length > 0) {
  const file = path.join(backupDir, created[created.length - 1]);
  const text = fs.readFileSync(file, 'utf8');
  const lines = text.trim().split('\n');
  report.exportParsed = lines.map((l) => {
    try {
      return JSON.parse(l);
    } catch {
      return null;
    }
  });
  report.exportValidJsonl = lines.length > 1 && report.exportParsed.every(Boolean);

  // 2. Import the same file through ST's regular mechanism; delete the created chat file.
  const chatRoot = path.join(INSTALL, 'sillytavern', 'data', 'default-user', 'chats');
  const chatsBefore = listChats(chatRoot);
  report.importResult = await evaljs(`(async () => {
    try {
      const text = ${JSON.stringify(text)};
      const file = new File([text], ${JSON.stringify(created[created.length - 1])}, { type: 'application/jsonl' });
      window.__lkImportChatFile(file);
      await new Promise((r) => setTimeout(r, 4000));
      return 'sent';
    } catch (e) {
      return 'error:' + String(e);
    }
  })()`);
  await sleep(2000);
  const chatsAfter = listChats(chatRoot);
  report.importCreated = chatsAfter.filter((c) => !chatsBefore.includes(c));
  report.importTouched = chatsAfter
    .filter((c) => chatsBefore.includes(c))
    .filter((c) => fs.statSync(path.join(chatRoot, c)).mtimeMs > Date.now() - 60_000);
  // Cleanup: the stand must not leave files behind.
  for (const rel of report.importCreated) {
    fs.unlinkSync(path.join(chatRoot, rel));
  }
  report.importCleaned = report.importCreated.length;
}

// 3. The ST update-check route — from the ST page (CORS layer).
report.updateCheck = await evaljs(`(async () => {
  try {
    const response = await fetch(window.__LK_GATEWAY + '/api/cloufam/st/update-check');
    return await response.json();
  } catch (e) {
    return { error: String(e) };
  }
})()`);

console.log(JSON.stringify(report, null, 2));
s.close();

function listChats(root) {
  const out = [];
  if (!fs.existsSync(root)) return out;
  for (const dir of fs.readdirSync(root)) {
    const full = path.join(root, dir);
    if (!fs.statSync(full).isDirectory()) continue;
    for (const file of fs.readdirSync(full)) out.push(path.join(dir, file));
  }
  return out;
}

