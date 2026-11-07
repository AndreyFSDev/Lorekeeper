// Probing the app UI through CDP WebView2: dumps the available elements
// to figure out how to reach the screen you need.
// Run: node tools/app-ui-probe.mjs [--port 9222] [--expr "<js>"]
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
const EXPR = arg('expr', '');
const SHOT = arg('shot', '');
const MATCH = arg('match', '');

const list = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
const page = list.find((t) => t.type === 'page' && (!MATCH || t.url.includes(MATCH)));
if (!page) throw new Error('страница приложения не найдена');

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
const expression =
  EXPR ||
  `(() => {
        const text = (el) => (el.textContent || '').trim().replace(/\\s+/g, ' ').slice(0, 70);
        return {
            title: document.title,
            url: location.href,
            buttons: [...document.querySelectorAll('button')].slice(0, 60).map(text),
            tabs: [...document.querySelectorAll('.tab-btn')].map(text),
            inputs: [...document.querySelectorAll('input')].slice(0, 20).map((el) => ({ type: el.type, placeholder: el.placeholder })),
            headings: [...document.querySelectorAll('h1,h2,h3')].slice(0, 20).map(text),
            bodyStart: (document.body.innerText || '').trim().replace(/\\s+/g, ' ').slice(0, 400),
        };
    })()`;
const result = await send('Runtime.evaluate', {
  expression,
  awaitPromise: true,
  returnByValue: true,
});
if (result.result?.exceptionDetails) {
  console.error('ошибка в странице:', result.result.exceptionDetails.exception?.description);
  process.exitCode = 1;
} else {
  console.log(JSON.stringify(result.result?.result?.value ?? result.result, null, 2));
}
socket.close();
