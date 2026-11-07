// One-off live check of the chain of thought on the working copy.
import path from 'node:path';
import { loadWs } from './ws-resolve.mjs';

const ROOT = path.resolve(import.meta.dirname, '..');
const ws = loadWs(ROOT);
const CDP_PORT = 9223;

const list = await (await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`)).json();
const page = list.find((t) => t.type === 'page' && t.url.startsWith('http://tauri.localhost'));
if (!page) throw new Error('страница приложения не найдена');
const socket = new ws(page.webSocketDebuggerUrl);
await new Promise((res, rej) => {
  socket.once('open', res);
  socket.once('error', rej);
});
let id = 0;
const pending = new Map();
socket.on('message', (raw) => {
  const m = JSON.parse(raw.toString());
  const e = pending.get(m.id);
  if (e) {
    pending.delete(m.id);
    e(m);
  }
});
const send = (method, params) =>
  new Promise((resolve) => {
    const i = ++id;
    pending.set(i, resolve);
    socket.send(JSON.stringify({ id: i, method, params }));
  });
const evalJs = async (expression) => {
  const r = await send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
  return r.result?.result?.value;
};

await evalJs(
  `window.__TAURI_INTERNALS__.invoke('mark_st_opened').then(() => { location.href = 'http://127.0.0.1:8001/'; return true; })`,
);
await new Promise((r) => setTimeout(r, 12000));

const list2 = await (await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`)).json();
const stPage = list2.find((t) => t.type === 'page' && t.url.startsWith('http://127.0.0.1:8001'));
if (!stPage) throw new Error('страница ST не открылась: ' + list2.map((t) => t.url).join(', '));
const socket2 = new ws(stPage.webSocketDebuggerUrl);
await new Promise((res, rej) => {
  socket2.once('open', res);
  socket2.once('error', rej);
});
let id2 = 0;
const pending2 = new Map();
socket2.on('message', (raw) => {
  const m = JSON.parse(raw.toString());
  const e = pending2.get(m.id);
  if (e) {
    pending2.delete(m.id);
    e(m);
  }
});
const send2 = (method, params) =>
  new Promise((resolve) => {
    const i = ++id2;
    pending2.set(i, resolve);
    socket2.send(JSON.stringify({ id: i, method, params }));
  });
const evalSt = async (expression) => {
  const r = await send2('Runtime.evaluate', {
    expression,
    awaitPromise: true,
    returnByValue: true,
  });
  if (r.result?.exceptionDetails)
    return (
      'ERR ' +
      JSON.stringify(r.result.exceptionDetails.exception?.description ?? r.result.exceptionDetails)
    );
  return r.result?.result?.value;
};

const checks = await evalSt(`(async () => {
  const ext = (await import('/scripts/extensions.js')).extension_settings;
  const script = (ext.regex ?? []).find((s) => s.id === 'cloufam-thought-highlight');
  const css = document.getElementById('cloufam-theme-style')?.textContent ?? '';
  return {
    scriptPresent: Boolean(script),
    scriptDisabled: script ? Boolean(script.disabled) : null,
    placement: script?.placement,
    markdownOnly: script?.markdownOnly,
    cssHasThoughtRule: css.includes('.custom-lk-thought'),
  };
})()`);
console.log(JSON.stringify(checks, null, 2));
socket.close();
socket2.close();
process.exit(0);

