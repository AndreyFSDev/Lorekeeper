// Harness: the message edit button (pencil) on live ST over CDP 9223.
// On the open chat's messages it measures button visibility and hit-tests its center,
// performs a real mouse click and checks whether the editor opened (#curEditTextarea);
// on the last message it runs the full save cycle (marker -> "Done" ->
// check chat[].mes -> restore the original text). Chat data is not modified.
//
// Start: the app with WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9223,
// LOREKEEPER_INSTALL=Z:\AImodels\Cloufam (the ws module is taken from there).
// Flags: --welcome — do not open a character (stay in the welcome chat);
//        --char=Name — open the character with this name/avatar (no scanning:
//        switching to a character without a chat CREATES a new chat on disk).
import path from 'node:path';
import { loadWs } from './ws-resolve.mjs';

process.env.LOREKEEPER_INSTALL ??= 'Z:\\AImodels\\Cloufam';
const ws = loadWs(path.resolve(import.meta.dirname, '..'));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const charArg = (process.argv.find((a) => a.startsWith('--char=')) ?? '').slice('--char='.length);

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

const send = (method, params = {}) =>
  new Promise((resolve, reject) => {
    const id = ++seq;
    pending.set(id, { resolve, reject });
    s.send(JSON.stringify({ id, method, params }));
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

const clickAt = async (x, y) => {
  await send('Input.dispatchMouseEvent', { type: 'mouseMoved', x, y });
  await send('Input.dispatchMouseEvent', {
    type: 'mousePressed',
    x,
    y,
    button: 'left',
    clickCount: 1,
  });
  await send('Input.dispatchMouseEvent', {
    type: 'mouseReleased',
    x,
    y,
    button: 'left',
    clickCount: 1,
  });
};

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

// 0b. Open a character by name — exactly one: any other switch
// creates a new chat with a greeting on disk.
if (!process.argv.includes('--welcome') && charArg) {
  const opened = await evaljs(`(async () => {
    try {
      const ctx = await import('/script.js');
      const st = window.SillyTavern?.getContext?.();
      const chars = st?.characters ?? [];
      const wanted = ${JSON.stringify(charArg)};
      const i = chars.findIndex((c) => (c.name || '').includes(wanted) || (c.avatar || '').includes(wanted));
      if (i < 0) return 'not_found:' + chars.map((c) => c.name).join('|');
      await ctx.selectCharacterById(i, { switchMenu: false });
      await new Promise((r) => setTimeout(r, 1200));
      return 'opened:' + i;
    } catch (e) {
      return 'error:' + String(e);
    }
  })()`);
  console.log('=== ПЕРСОНАЖ ===', JSON.stringify(opened));
  await sleep(1000);
}

// 1. Snapshot: class, button, geometry, hit chain at the pencil center.
const snapshot = await evaljs(`(() => {
  const chain = (el) => {
    const out = [];
    for (let e = el; e && out.length < 4; e = e.parentElement) {
      out.push(e.tagName + (e.id ? '#' + e.id : '') + (e.className ? '.' + String(e.className).split(' ').slice(0, 3).join('.') : ''));
    }
    return out;
  };
  const messages = [];
  for (const mes of document.querySelectorAll('#chat .mes')) {
    const btn = mes.querySelector('.mes_buttons .mes_edit');
    const r = btn?.getBoundingClientRect();
    let hit = null;
    if (r && r.width > 0) {
      const el = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
      hit = el ? { chain: chain(el), isButton: el === btn || el.closest('.mes_edit') === btn } : null;
    }
    const cs = btn ? getComputedStyle(btn) : null;
    messages.push({
      mesid: mes.getAttribute('mesid'),
      type: mes.getAttribute('type') || '(none)',
      isUser: mes.getAttribute('is_user'),
      classes: String(mes.className),
      display: cs?.display ?? null,
      visibility: cs?.visibility ?? null,
      opacity: cs?.opacity ?? null,
      rect: r ? { x: Math.round(r.x), y: Math.round(r.y), w: Math.round(r.width), h: Math.round(r.height) } : null,
      hit,
      text: (mes.querySelector('.mes_text')?.innerText || '').slice(0, 40),
    });
  }
  return { mesCount: messages.length, messages };
})()`);
console.log('=== СНИМОК ===');
console.log(JSON.stringify(snapshot, null, 2));

// 2. Click each message's pencil (max 6): does the editor open?
const messages = (snapshot?.messages ?? []).filter((m) => m.rect && m.rect.w > 0).slice(0, 6);
await evaljs(`(() => {
  window.__lkEditErrors = [];
  const orig = console.error;
  console.error = (...a) => { window.__lkEditErrors.push(a.map(String).join(' ')); orig(...a); };
  return 'ok';
})()`);

const results = [];
for (const target of messages) {
  const at = await evaljs(`(() => {
    const btn = document.querySelector('#chat .mes[mesid="${target.mesid}"] .mes_buttons .mes_edit');
    const r = btn.getBoundingClientRect();
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
  })()`);
  await clickAt(at.x, at.y);
  await sleep(600);
  const afterClick = await evaljs(`(() => {
    const mes = document.querySelector('#chat .mes[mesid="${target.mesid}"]');
    const ta = document.querySelector('#curEditTextarea');
    return {
      editorFound: !!ta,
      editorValueLen: ta ? ta.value.length : 0,
      editButtonsDisplay: mes ? getComputedStyle(mes.querySelector('.mes_edit_buttons')).display : null,
      errors: [...(window.__lkEditErrors || [])].slice(-3),
    };
  })()`);
  const cancel = await evaljs(`(() => {
    const btn = document.querySelector('#chat .mes[mesid="${target.mesid}"] .mes_edit_cancel');
    if (!btn) return { clicked: false };
    const r = btn.getBoundingClientRect();
    return { clicked: true, x: r.left + r.width / 2, y: r.top + r.height / 2 };
  })()`);
  if (cancel.clicked) {
    await clickAt(cancel.x, cancel.y);
    await sleep(400);
  }
  results.push({
    mesid: target.mesid,
    label: `${target.type} ${target.classes} user=${target.isUser}`,
    hitIsButton: target.hit?.isButton ?? null,
    hit: target.hit?.chain ?? null,
    afterClick,
    cancelClicked: cancel.clicked,
    editorGone: !(await evaljs(`!!document.querySelector('#curEditTextarea')`)),
  });
  if (!afterClick.editorFound) {
    await evaljs(`document.querySelectorAll('.mes_edit_cancel').forEach((b) => b.click())`);
    await sleep(200);
  }
}
console.log('=== КЛИКИ ===');
console.log(JSON.stringify(results, null, 2));

// 3. Save cycle on the last message: marker -> "Done" -> check -> restore.
const last = messages[messages.length - 1];
if (last) {
  const mid = last.mesid;
  const openEditor = async () => {
    const at = await evaljs(`(() => {
      const btn = document.querySelector('#chat .mes[mesid="${mid}"] .mes_buttons .mes_edit');
      const r = btn.getBoundingClientRect();
      return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
    })()`);
    await clickAt(at.x, at.y);
    await sleep(500);
    return evaljs(`!!document.querySelector('#curEditTextarea')`);
  };
  const clickDone = async () => {
    const at = await evaljs(`(() => {
      const btn = document.querySelector('#chat .mes[mesid="${mid}"] .mes_edit_done');
      if (!btn) return null;
      const r = btn.getBoundingClientRect();
      return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
    })()`);
    if (at) await clickAt(at.x, at.y);
    await sleep(600);
  };

  const save = { editorOpened: await openEditor() };
  save.original = await evaljs(`document.querySelector('#curEditTextarea')?.value ?? null`);
  if (save.editorOpened && typeof save.original === 'string') {
    await evaljs(`(() => {
      const ta = document.querySelector('#curEditTextarea');
      ta.value = ta.value + '\\n[тест стенда lk-edit-check]';
      ta.dispatchEvent(new Event('input', { bubbles: true }));
      return ta.value.length;
    })()`);
    await clickDone();
    save.afterMarker = await evaljs(`(() => {
      const st = window.SillyTavern?.getContext?.();
      const mes = st?.chat?.[Number(${JSON.stringify(mid)})];
      return {
        chatMes: mes ? String(mes.mes).slice(-40) : null,
        domText: (document.querySelector('#chat .mes[mesid="${mid}"] .mes_text')?.textContent || '').slice(-40),
      };
    })()`);

    save.reopened = await openEditor();
    await evaljs(`(() => {
      const ta = document.querySelector('#curEditTextarea');
      ta.value = ${JSON.stringify(save.original ?? '')};
      ta.dispatchEvent(new Event('input', { bubbles: true }));
      return ta.value.length;
    })()`);
    await clickDone();
    save.afterRestore = await evaljs(`(() => {
      const st = window.SillyTavern?.getContext?.();
      const mes = st?.chat?.[Number(${JSON.stringify(mid)})];
      return {
        chatMes: mes ? String(mes.mes).slice(-40) : null,
        domText: (document.querySelector('#chat .mes[mesid="${mid}"] .mes_text')?.textContent || '').slice(-40),
      };
    })()`);
    save.restoredOk =
      String(save.afterRestore?.chatMes ?? '') === String(save.original ?? '').slice(-40);
  }
  console.log(`=== СОХРАНЕНИЕ (последнее сообщение mesid=${mid}) ===`);
  console.log(JSON.stringify(save, null, 2));
}

s.close();

