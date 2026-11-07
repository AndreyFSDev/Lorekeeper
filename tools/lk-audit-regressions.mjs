import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import vm from 'node:vm';
import ts from 'typescript';

const root = path.resolve(import.meta.dirname, '..');
const read = (file) => fs.readFileSync(path.join(root, file), 'utf8');
const stripImports = (source) =>
  source.replace(/^import\s[\s\S]*?from\s+['"][^'"]+['"];\r?\n/gm, '');
const deferred = () => {
  let resolve;
  const promise = new Promise((done) => {
    resolve = done;
  });
  return { promise, resolve };
};

// The VM runs the embedded extension itself; only ST, IPC and the DOM are replaced.
function extensionHarness(kind) {
  const handlers = new Map();
  const eventSource = {
    on(name, handler) {
      handlers.set(name, [...(handlers.get(name) ?? []), handler]);
    },
  };
  const event_types = {
    CHAT_CHANGED: 'chat',
    GENERATION_ENDED: 'end',
    GROUP_WRAPPER_FINISHED: 'group-end',
  };
  const response = deferred();
  const narrationDone = deferred();
  const writes = [];
  const prompts = [];
  const replies = [];
  let chat = 'A';
  let callback;
  let holdNarration = false;
  const area = { value: '// scene for A', dispatchEvent() {} };
  const context = {
    characterId: 0,
    groupId: null,
    extensionSettings: {
      cloufam_director: { after: 'characters', delivery: 'system', delivery_migrated: true },
    },
    getCurrentChatId: () => chat,
    getRequestHeaders: () => ({}),
    saveSettingsDebounced() {},
    setExtensionPrompt(_key, text) {
      prompts.push({ chat, text });
    },
    generate: async () => {
      replies.push(chat);
    },
    eventSource,
    event_types,
    SlashCommandParser: {
      addCommandObject(command) {
        callback = command.callback;
      },
    },
  };
  const sandbox = {
    console,
    getContext: () => context,
    eventSource,
    event_types,
    isGenerating: () => false,
    extension_prompt_types: { IN_CHAT: 1 },
    extension_prompt_roles: { SYSTEM: 1 },
    SlashCommand: { fromProps: (props) => props },
    SlashCommandArgument: class {},
    ARGUMENT_TYPE: { STRING: 'string' },
    fetch: () => response.promise,
    toastr: { warning() {}, error() {}, info() {} },
    document: { getElementById: (id) => (id === 'send_textarea' ? area : null) },
    Event: class {},
    sendNarratorMessage: async (_args, text) => {
      writes.push({ chat, text });
      if (holdNarration) await narrationDone.promise;
    },
    sendMessageAsUser: async (text) => {
      writes.push({ chat, text });
    },
  };
  vm.createContext(sandbox);
  const file = `src-tauri/${kind}/extension-index.js`;
  const source = stripImports(read(file)).replace(/^export\s+/gm, '');
  const exports =
    kind === 'director'
      ? ';globalThis.audit = { directScene, bindCleanup, submitDirectorInput };'
      : '';
  vm.runInContext(source + exports, sandbox, { filename: file });
  if (kind === 'director') sandbox.audit.bindCleanup();
  return {
    writes,
    prompts,
    replies,
    area,
    direct: (...args) => sandbox.audit.directScene(...args),
    submit: () => sandbox.audit.submitDirectorInput(),
    search: () => callback({}, 'query for A'),
    change(next) {
      chat = next;
      for (const handler of handlers.get(event_types.CHAT_CHANGED) ?? []) handler();
    },
    holdNarration() {
      holdNarration = true;
    },
    finishNarration() {
      narrationDone.resolve();
    },
    finish(payload) {
      response.resolve({
        ok: true,
        text: async () => JSON.stringify(payload),
        json: async () => payload,
      });
    },
  };
}

for (const roundTrip of [false, true]) {
  test(`director discards a response after chat ${roundTrip ? 'A → B → A' : 'A → B'}`, async () => {
    const h = extensionHarness('director');
    const pending = h.direct('scene for A', { hiddenNote: 'secret for A' });
    h.change('B');
    if (roundTrip) h.change('A');
    h.finish({ narration: 'scene for A', hidden: 'secret for A' });
    await pending.catch(() => {});
    assert.equal(h.writes.length, 0, 'a stale scene must not be appended');
    assert.equal(
      h.prompts.some((entry) => entry.text.includes('secret for A')),
      false,
    );
  });

  test(`web search discards a response after chat ${roundTrip ? 'A → B → A' : 'A → B'}`, async () => {
    const h = extensionHarness('websearch');
    const pending = h.search();
    h.change('B');
    if (roundTrip) h.change('A');
    h.finish({ results: [{ title: 'A', url: 'https://example.test', snippet: 'A context' }] });
    await pending;
    assert.equal(h.writes.length, 0, 'stale search results must not enter a chat');
  });
}

test('director leaves the new chat draft intact on a stale response', async () => {
  const h = extensionHarness('director');
  const pending = h.submit();
  h.change('B');
  h.area.value = 'draft for B';
  h.finish({ narration: 'scene for A' });
  await pending;
  assert.equal(h.writes.length, 0);
  assert.equal(h.area.value, 'draft for B');
});

test('director does not inject a secret or trigger a reply after narration changes chat', async () => {
  const h = extensionHarness('director');
  h.holdNarration();
  const pending = h.direct('scene for A', { hiddenNote: 'secret for A' });
  h.finish({ narration: 'scene for A', hidden: 'secret for A' });
  while (!h.writes.length) await new Promise((resolve) => setImmediate(resolve));
  h.change('B');
  h.finishNarration();
  await pending;
  assert.equal(
    h.prompts.some((entry) => entry.chat === 'B' && entry.text.includes('secret for A')),
    false,
  );
  assert.equal(h.replies.length, 0);
});

const settingsSource = read('src/Settings.svelte').match(
  /<script lang="ts">([\s\S]*?)<\/script>/,
)[1];
const settingsAst = ts.createSourceFile(
  'Settings.ts',
  settingsSource,
  ts.ScriptTarget.Latest,
  true,
  ts.ScriptKind.TS,
);
const compile = (source) =>
  ts.transpileModule(source, {
    compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ES2022 },
  }).outputText;
const functions = new Map(
  settingsAst.statements
    .filter(ts.isFunctionDeclaration)
    .map((node) => [node.name.text, node.getText(settingsAst)]),
);
const derived = new Map();
for (const statement of settingsAst.statements) {
  if (!ts.isVariableStatement(statement)) continue;
  for (const declaration of statement.declarationList.declarations) {
    if (
      ts.isCallExpression(declaration.initializer ?? {}) &&
      declaration.initializer.expression.getText(settingsAst) === '$derived'
    ) {
      derived.set(
        declaration.name.getText(settingsAst),
        declaration.initializer.arguments[0].getText(settingsAst),
      );
    }
  }
}

function settingsHarness(api = {}) {
  const makeCharacter = (avatar) => ({
    avatar,
    name: avatar,
    tags: [{ tag: 'kind', intensity: 50 }],
    no_repeat: true,
    chaos: 0,
    live: true,
    state: { affection: 50, trust: 50, energy: 55, irritation: 10 },
    bonds: [{ target: 'other', kind: 'friend', strength: 50, note: '', secret: false }],
    fragment: avatar,
  });
  const box = {
    settings: { temperature: 0.5, chat_model: 'old' },
    settingsSnapshot: JSON.stringify({ temperature: 0.5, chat_model: 'old' }),
    settingsSaveTail: Promise.resolve(),
    settingsSaving: false,
    traitCharacters: [makeCharacter('A'), makeCharacter('B')],
    selectedTraitAvatar: 'A',
    traitsPreview: '',
    traitsSnapshot: '',
    bondsSnapshot: '',
    traitsSnapshots: {},
    bondsSnapshots: {},
    loadedTraitStates: {},
    savingTraits: false,
    savingBonds: false,
    error: null,
    updateNotice: null,
    api: {
      getSettings: async () => ({ temperature: 0.5, chat_model: 'old' }),
      saveSettings: async () => {},
      ...api,
    },
  };
  Object.defineProperty(box, 'selectedTraits', {
    get: () => box.traitCharacters.find((c) => c.avatar === box.selectedTraitAvatar),
  });
  vm.createContext(box);
  const names = [
    'settingsChanges',
    'queueSettingsChange',
    'formEdits',
    'saveOverFresh',
    'traitsKey',
    'bondsKey',
    'rememberTraitStates',
    'rememberTraitSnapshots',
    'markTraitsSaved',
    'markBondsSaved',
    'updateTraitsPreview',
    'onTraitCharacterChange',
    'saveTraits',
    'saveBonds',
    'resetTraitState',
  ];
  vm.runInContext(
    compile(
      names
        .filter((name) => functions.has(name))
        .map((name) => functions.get(name))
        .join('\n'),
    ),
    box,
  );
  for (const name of ['traitsDirty', 'bondsDirty', 'anyTraitsDirty']) {
    if (!derived.has(name)) continue;
    const getter = vm.runInContext(compile(`() => (${derived.get(name)})`), box);
    Object.defineProperty(box, name, { get: getter });
  }
  box.rememberTraitStates(box.traitCharacters);
  if (box.rememberTraitSnapshots) box.rememberTraitSnapshots(box.traitCharacters);
  box.markTraitsSaved?.();
  return box;
}

test('saving settings preserves edits made while fetching the current settings', async () => {
  const current = deferred();
  const box = settingsHarness({ getSettings: () => current.promise });
  box.settings.temperature = 0.6;
  const pending = box.saveOverFresh({}, true);
  box.settings.temperature = 0.9;
  current.resolve({ temperature: 0.5, chat_model: 'old' });
  await pending;
  assert.equal(box.settings.temperature, 0.9);
  assert.equal(JSON.parse(box.settingsSnapshot).temperature, 0.6);
});

test('changing model preserves edits made while saving settings', async () => {
  const saved = deferred();
  const box = settingsHarness({ saveSettings: () => saved.promise });
  const pending = box.saveOverFresh({ chat_model: 'new' }, false);
  await Promise.resolve();
  await Promise.resolve();
  box.settings.temperature = 0.9;
  saved.resolve();
  await pending;
  assert.equal(box.settings.temperature, 0.9);
  assert.equal(box.settings.chat_model, 'new');
});

test('unsaved traits and bonds remain dirty after selecting another avatar', () => {
  const box = settingsHarness();
  box.traitCharacters[0].tags[0].intensity = 80;
  box.traitCharacters[0].bonds[0].note = 'unsaved';
  assert.equal(box.traitsDirty, true);
  box.selectedTraitAvatar = 'B';
  box.onTraitCharacterChange();
  if ('anyTraitsDirty' in box) assert.equal(box.anyTraitsDirty, true);
  box.selectedTraitAvatar = 'A';
  box.onTraitCharacterChange();
  assert.equal(box.traitsDirty, true);
  assert.equal(box.bondsDirty, true);
});

test('resetting live state does not mark unsaved traits and bonds saved', async () => {
  const box = settingsHarness({ traitsResetState: async () => 'reset' });
  box.selectedTraits.tags[0].intensity = 80;
  box.selectedTraits.bonds[0].note = 'unsaved';
  await box.resetTraitState();
  assert.equal(box.traitsDirty, true);
  assert.equal(box.bondsDirty, true);
});

test('saving traits preserves edits made while the request is pending', async () => {
  const done = deferred();
  const box = settingsHarness({ traitsSave: () => done.promise });
  box.selectedTraits.state.affection = 60;
  const pending = box.saveTraits();
  box.selectedTraits.tags[0].intensity = 80;
  box.selectedTraits.state.affection = 90;
  done.resolve('saved');
  await pending;
  assert.equal(box.selectedTraits.state.affection, 90);
  assert.equal(box.traitsDirty, true);
});

test('saving bonds preserves edits made while the request is pending', async () => {
  const done = deferred();
  const box = settingsHarness({ bondsSave: () => done.promise });
  box.selectedTraits.bonds[0].note = 'submitted';
  const pending = box.saveBonds();
  box.selectedTraits.bonds[0].note = 'new draft';
  done.resolve('saved');
  await pending;
  assert.equal(box.selectedTraits.bonds[0].note, 'new draft');
  assert.equal(box.bondsDirty, true);
});

