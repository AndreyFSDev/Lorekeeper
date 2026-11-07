// Live World Info check via CDP: import should immediately enable the lorebook,
// the key should activate the entry, and the editor should show the available fields.
// Run: node tools/lk-lorebook-check.mjs [--port 9223] [--probe-import]

import path from 'node:path';
import { loadWs } from './ws-resolve.mjs';

const arg = (name, fallback) => {
  const index = process.argv.indexOf(`--${name}`);
  return index === -1 ? fallback : process.argv[index + 1];
};

const port = Number(arg('port', 9223));
const probeImport = process.argv.includes('--probe-import');
const ws = loadWs(path.resolve(import.meta.dirname, '..'));
const pages = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
const page = pages.find((item) => item.type === 'page' && /127\.0\.0\.1:8\d+/.test(item.url));
if (!page) throw new Error('в окне Cloufam не открыт SillyTavern');

const socket = new ws(page.webSocketDebuggerUrl);
await new Promise((resolve, reject) => {
  socket.once('open', resolve);
  socket.once('error', reject);
});

let id = 0;
const pending = new Map();
socket.on('message', (raw) => {
  const message = JSON.parse(raw.toString());
  pending.get(message.id)?.(message);
  pending.delete(message.id);
});

const send = (method, params) =>
  new Promise((resolve) => {
    const requestId = ++id;
    pending.set(requestId, resolve);
    socket.send(JSON.stringify({ id: requestId, method, params }));
  });

async function evaluate(expression) {
  const response = await send('Runtime.evaluate', {
    expression,
    awaitPromise: true,
    returnByValue: true,
  });
  if (response.result?.exceptionDetails) {
    throw new Error(
      response.result.exceptionDetails.exception?.description ?? 'ошибка Runtime.evaluate',
    );
  }
  return response.result?.result?.value;
}

await send('Runtime.enable', {});
const state = await evaluate(`(async () => {
  const wi = await import('/scripts/world-info.js');
  const visible = (selector) => {
    const element = document.querySelector(selector);
    if (!element) return false;
    const rect = element.getBoundingClientRect();
    const style = getComputedStyle(element);
    return rect.width > 0 && rect.height > 0 && style.display !== 'none' && style.visibility !== 'hidden';
  };
  return {
    names: wi.world_names,
    active: wi.selected_world_info,
    createVisible: visible('#world_create_button'),
    newEntryVisible: visible('#world_popup_new'),
    editorVisible: visible('#world_editor_select'),
  };
})()`);
console.log(JSON.stringify(state, null, 2));

if (probeImport) {
  const probe = `__lk_lorebook_probe_${Date.now()}`;
  const result = await evaluate(`(async () => {
    const wi = await import('/scripts/world-info.js');
    const name = ${JSON.stringify(probe)};
    const book = {
      entries: {
        0: {
          uid: 0,
          key: ['cloufam_probe_key'],
          keysecondary: [],
          comment: 'Cloufam probe',
          content: 'LOREKEEPER_LOREBOOK_ACTIVE',
          constant: false,
          selective: false,
          selectiveLogic: 0,
          addMemo: true,
          order: 100,
          position: 0,
          disable: false,
          probability: 100,
          useProbability: true,
          depth: 4,
        },
      },
    };
    try {
      await wi.importWorldInfo(new File([JSON.stringify(book)], name + '.json', { type: 'application/json' }));
      await new Promise((resolve) => setTimeout(resolve, 800));
      const prompt = await wi.getWorldInfoPrompt(
        ['cloufam_probe_key'],
        8192,
        true,
        { personaDescription: '', characterDescription: '', characterPersonality: '', characterDepthPrompt: '', scenario: '', creatorNotes: '', trigger: 'normal' },
      );
      const entries = document.querySelectorAll('#world_popup_entries_list .world_entry').length;
      const editable = [...document.querySelectorAll('#world_popup_entries_list textarea, #world_popup_entries_list input')]
        .some((element) => !element.disabled);
      return {
        imported: wi.world_names.includes(name),
        active: wi.selected_world_info.includes(name),
        keyActivated: prompt.worldInfoString.includes('LOREKEEPER_LOREBOOK_ACTIVE'),
        entries,
        editable,
      };
    } finally {
      if (wi.world_names.includes(name)) await wi.deleteWorldInfo(name);
    }
  })()`);
  console.log(JSON.stringify(result, null, 2));
  if (
    !result.imported ||
    !result.active ||
    !result.keyActivated ||
    result.entries < 1 ||
    !result.editable
  ) {
    process.exitCode = 1;
  }
}

socket.close();

