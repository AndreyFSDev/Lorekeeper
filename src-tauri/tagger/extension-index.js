// Cloufam character auto-tags: tags are assigned by the local Ollama model
// from the card description. Model requests go through the server plugin
// `plugins/cloufam`, which proxies them to the Tauri app gateway.

import { extension_prompt_types, getOneCharacter } from '../../../../script.js';
import { event_types, eventSource } from '../../../events.js';
import { SlashCommand } from '../../../slash-commands/SlashCommand.js';
import { ARGUMENT_TYPE } from '../../../slash-commands/SlashCommandArgument.js';
import { PARSER_FLAG } from '../../../slash-commands/SlashCommandParser.js';
import { getContext } from '../../../st-context.js';
import {
  loadWorldInfo,
  saveWorldInfo,
  selected_world_info,
  world_names,
} from '../../../world-info.js';

const MODULE_NAME = 'cloufam_tagger';
const AUTO_LIMIT_PER_RUN = 20;
const WAND_CONTAINER_ID = 'cloufam_wand_container';
const WAND_ITEM_ID = 'cloufam-tagger-menu-item';
const TRAITS_ITEM_ID = 'cloufam-traits-menu-item';
const RELATIONSHIPS_ITEM_ID = 'cloufam-relationships-menu-item';
const RPG_ITEM_ID = 'cloufam-rpg-menu-item';
const MD_IMPORT_ITEM_ID = 'cloufam-md-import-menu-item';
const CHAT_EXPORT_ITEM_ID = 'cloufam-chat-export-menu-item';
const CHAT_IMPORT_ITEM_ID = 'cloufam-chat-import-menu-item';
const ST_UPDATE_ITEM_ID = 'cloufam-st-update-menu-item';
const ST_UPDATE_MODAL_ID = 'cloufam-st-update-modal';
// id of the thoughts regex script — the same one the app seeds (sillytavern.rs).
const THOUGHT_REGEX_ID = 'cloufam-thought-highlight';
const THOUGHT_PROMPT_KEY = 'cloufam_thoughts';
const THOUGHT_PROMPT =
  'Мысли персонажа, которые он не произносит вслух, выделяй тильдами: ~вот так~.';

/**
 * fetch to the gateway with a timeout: without it a hung handler leaves the modal
 * on "Loading…" forever — neither an error nor a retry. A model that writes
 * for tens of seconds does not get here: these are short state routes.
 * @param {string} url
 * @param {RequestInit} [init]
 * @param {number} [timeoutMs]
 */
function gatewayFetch(url, init = {}, timeoutMs = 12000) {
  return fetch(url, { ...init, signal: AbortSignal.timeout(timeoutMs) });
}

const {
  getRequestHeaders,
  extensionSettings,
  saveSettingsDebounced,
  executeSlashCommandsWithOptions,
  SlashCommandParser,
} = getContext();

// ST reassigns characters/tags/tag_map on load — we take them at the moment
// of use, otherwise we hold a stale array reference.
function allCharacters() {
  return getContext().characters ?? [];
}

/** @type {{enabled?: boolean, model?: string, max_tags?: number, language?: string, write_card_tags?: boolean, auto_translate?: boolean, prompt?: string, models?: string[], seen?: string[]}} */
let config = {};
let running = false;
let cancelled = false;
let initialized = false;
/** One-off values from the modal: they do not touch the extension's saved settings. */
let runOverrides = null;

function settings() {
  if (!extensionSettings[MODULE_NAME]) {
    extensionSettings[MODULE_NAME] = { seen: [] };
  }
  return extensionSettings[MODULE_NAME];
}

function pluginUrl(path) {
  return `/api/plugins/cloufam/${path}`;
}

const TRANSLATABLE_CHARACTER_FIELDS = [
  'description',
  'personality',
  'scenario',
  'first_mes',
  'mes_example',
  'creator_notes',
  'system_prompt',
  'post_history_instructions',
  'alternate_greetings',
];

async function requestTranslation(kind, content) {
  const response = await fetch(pluginUrl('translate'), {
    method: 'POST',
    headers: getRequestHeaders(),
    body: JSON.stringify({ kind, content }),
  });
  if (!response.ok) {
    throw new Error((await response.text()) || `HTTP ${response.status}`);
  }
  return response.json();
}

function needsRussianTranslation(value) {
  const text = (Array.isArray(value) ? value.join('\n') : String(value ?? '')).replace(
    /\{\{[^}]+\}\}/g,
    '',
  );
  const latin = (text.match(/[A-Za-z]/g) ?? []).length;
  // Same range as translation.rs::is_cyrillic_letter: Ukrainian and Belarusian letters are Cyrillic too
  const cyrillic = (text.match(/[\u0400-\u052F]/g) ?? []).length;
  return latin >= 6 && latin > cyrillic;
}

async function translateCharacter(character) {
  const data = character?.data ?? character ?? {};
  if (data.extensions?.cloufam_translation?.language === 'ru') return false;
  const source = Object.fromEntries(
    TRANSLATABLE_CHARACTER_FIELDS.filter((field) => needsRussianTranslation(data[field])).map(
      (field) => [field, data[field]],
    ),
  );
  if (Object.keys(source).length === 0) return false;
  const translated = await requestTranslation('character', source);
  const response = await fetch('/api/characters/merge-attributes', {
    method: 'POST',
    headers: getRequestHeaders(),
    body: JSON.stringify({
      avatar: character.avatar,
      ...translated,
      data: {
        ...translated,
        extensions: {
          cloufam_translation: { language: 'ru', original: source },
        },
      },
    }),
  });
  if (!response.ok) throw new Error((await response.text()) || `HTTP ${response.status}`);
  await getOneCharacter(character.avatar);
  return true;
}

async function translateNewCharacters() {
  // Translation is a user option: we re-read the config before deciding, the toggle
  // is changed in the Cloufam window, not on this page (see __lkReloadTaggerConfig).
  await loadConfig();
  if (!config.auto_translate) return;
  const state = settings();
  const list = allCharacters();
  const avatars = list.map((character) => character.avatar);
  if (!Array.isArray(state.translation_seen)) {
    state.translation_seen = avatars;
    saveSettingsDebounced();
    return;
  }
  const known = new Set(state.translation_seen);
  const fresh = list.filter((character) => !known.has(character.avatar));
  if (fresh.length === 0) return;
  // Mark only a successfully translated card: a model failure must not
  // leave it English forever without a retry.
  const translated = [];
  for (const character of fresh) {
    try {
      const done = await translateCharacter(character);
      // false — "already Russian" or nothing to translate: this is a deliberate skip,
      // it does not need repeating; an error (catch) stays for the next run.
      translated.push(character.avatar);
      if (done) {
        toastr.success(`Карточка «${character.name}» переведена на русский`, 'Cloufam');
      }
    } catch (error) {
      console.warn('[cloufam-translate] character:', error);
      toastr.warning(`Не удалось перевести карточку «${character.name}»: ${error}`, 'Cloufam');
    }
  }
  if (translated.length > 0) {
    state.translation_seen = Array.from(new Set([...state.translation_seen, ...translated]));
    saveSettingsDebounced();
  }
}

async function translateImportedWorldInfo(name, { silent = false } = {}) {
  // Translation is a user option (the same toggle as for cards): without it
  // the lorebook import touches neither keys nor text. We re-read the config: the toggle
  // is changed in the Cloufam window, not on this page.
  await loadConfig();
  if (!config.auto_translate) return true;
  try {
    const data = await loadWorldInfo(name);
    // Only the activation (keys) is translated: content and comment stay authored —
    // machine translation distorts the text, and the lorebook works poorly.
    const entries = Object.values(data?.entries ?? {}).filter(
      (entry) =>
        !entry.extensions?.cloufam_translation &&
        [entry.key, entry.keysecondary].some(needsRussianTranslation),
    );
    if (entries.length === 0) return true;
    if (!silent) toastr.info(`Перевожу ключи лорбука «${name}» на русский…`, 'Cloufam');
    const pending = entries.map((entry) => ({
      uid: entry.uid,
      key: entry.key ?? [],
      keysecondary: entry.keysecondary ?? [],
      comment: entry.comment ?? '',
      content: entry.content ?? '',
    }));
    // One entry per request: a long lorebook ate the whole reply limit,
    // and a small model returned the original English JSON without translation.
    const batches = pending.map((entry) => [entry]);
    for (const batch of batches) {
      const translated = await requestTranslation('world_info', { entries: batch });
      for (const result of translated.entries ?? []) {
        const entry = data.entries[result.uid];
        if (!entry) continue;
        const original = batch.find((item) => item.uid === result.uid);
        const mergeKeys = (before, added) =>
          Array.from(
            new Map(
              [...before, ...added].map((key) => [String(key).toLocaleLowerCase(), String(key)]),
            ).values(),
          );
        entry.extensions = {
          ...(entry.extensions ?? {}),
          cloufam_translation: { language: 'ru', original },
        };
        entry.key = mergeKeys(entry.key ?? [], result.key ?? []);
        entry.keysecondary = mergeKeys(entry.keysecondary ?? [], result.keysecondary ?? []);
      }
    }
    await saveWorldInfo(name, data, true);
    if (!silent)
      toastr.success(
        `Лорбук «${name}»: русские ключи добавлены, текст записи не менялся`,
        'Cloufam',
      );
    return true;
  } catch (error) {
    console.warn('[cloufam-translate] world info:', error);
    if (!silent) toastr.warning(`Не удалось перевести лорбук «${name}»: ${error}`, 'Cloufam');
    return false;
  }
}

// For entries translated before the contract change (when the text was translated too),
// the content/comment source is saved in `cloufam_translation.original` —
// we return it: machine translation distorted the authored text of the entry.
function restoreWorldInfoEntryContent(entry) {
  const original = entry.extensions?.cloufam_translation?.original;
  if (!original) return false;
  let changed = false;
  if (typeof original.content === 'string' && entry.content !== original.content) {
    entry.content = original.content;
    changed = true;
  }
  if (typeof original.comment === 'string' && entry.comment !== original.comment) {
    entry.comment = original.comment;
    changed = true;
  }
  return changed;
}

async function restoreTranslatedWorldContent({ silent = false } = {}) {
  const state = settings();
  if (state.world_content_restored) return;
  let completed = true;
  for (const name of world_names ?? []) {
    try {
      const data = await loadWorldInfo(name);
      let changed = false;
      for (const entry of Object.values(data?.entries ?? {})) {
        changed = restoreWorldInfoEntryContent(entry) || changed;
      }
      if (changed) {
        await saveWorldInfo(name, data, true);
        if (!silent)
          toastr.info(`Лорбук «${name}»: исходный текст записей восстановлен`, 'Cloufam');
      }
    } catch (error) {
      completed = false;
      console.warn('[cloufam-translate] world info restore:', error);
    }
  }
  if (completed) {
    state.world_content_restored = true;
    saveSettingsDebounced();
  }
}

// Catching up translation of active books: the startup timer often fires before
// ST brings up `selected_world_info`, and without a retry the installation would forever
// stay with English keys — the lorebook did not match Russian lines.
const WORLD_TRANSLATION_RETRY_MS = 3 * 60 * 1000;
let worldTranslationLastAttempt = 0;

async function migrateActiveWorldInfoTranslation({ silent = false } = {}) {
  await loadConfig();
  // The toggle is off — we do not mark the catch-up as done: after enabling
  // translation the user expects the active books to catch up on their own.
  if (!config.auto_translate) return;
  const state = settings();
  if (state.world_translation_migrated || selected_world_info.length === 0) return;
  if (silent && Date.now() - worldTranslationLastAttempt < WORLD_TRANSLATION_RETRY_MS) return;
  worldTranslationLastAttempt = Date.now();
  let completed = true;
  for (const name of selected_world_info) {
    completed = (await translateImportedWorldInfo(name, { silent })) && completed;
  }
  if (completed) {
    state.world_translation_migrated = true;
    saveSettingsDebounced();
  }
}

async function loadConfig() {
  try {
    const response = await fetch(pluginUrl('config'), { headers: getRequestHeaders() });
    if (!response.ok) {
      throw new Error(await response.text());
    }
    config = await response.json();
    renderSettingsBlock();
    return true;
  } catch (error) {
    config = {};
    renderSettingsBlock(String(error));
    return false;
  }
}

async function saveConfig(patch) {
  const response = await fetch(pluginUrl('config'), {
    method: 'POST',
    headers: getRequestHeaders(),
    body: JSON.stringify(patch),
  });
  if (!response.ok) {
    throw new Error(await response.text());
  }
  config = await response.json();
  renderSettingsBlock();
}

function escapeHtml(value) {
  return String(value ?? '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

/** ST tag names already assigned to the character (not card tags). */
function assignedTagNames(avatar) {
  const context = getContext();
  const ids = context.tagMap?.[avatar] ?? [];
  return ids
    .map((id) => context.tags?.find((tag) => tag.id === id)?.name)
    .filter((name) => typeof name === 'string' && name.length > 0);
}

function cardTagNames(character) {
  const raw = character?.data?.tags ?? character?.tags ?? [];
  return Array.isArray(raw) ? raw.map((t) => String(t)) : [];
}

function descriptionOf(character) {
  const data = character?.data ?? {};
  return String(data.description ?? character?.description ?? '').trim();
}

function characterFields(character) {
  const data = character?.data ?? {};
  return {
    name: String(data.name ?? character?.name ?? ''),
    description: data.description ?? character?.description ?? '',
    personality: data.personality ?? character?.personality ?? '',
    scenario: data.scenario ?? character?.scenario ?? '',
    first_mes: data.first_mes ?? character?.first_mes ?? '',
    mes_example: data.mes_example ?? character?.mes_example ?? '',
  };
}

/** overrides — values from the modal (one-off run), otherwise the extension settings. */
async function requestTags(character, overrides = null) {
  const body = {
    ...characterFields(character),
    existing_tags: assignedTagNames(character.avatar),
    max_tags: Number(overrides?.max_tags ?? config.max_tags) || undefined,
    language: overrides?.language ?? config.language ?? undefined,
  };
  const response = await fetch(pluginUrl('tags'), {
    method: 'POST',
    headers: getRequestHeaders(),
    body: JSON.stringify(body),
  });
  if (!response.ok) {
    throw new Error((await response.text()) || `HTTP ${response.status}`);
  }
  const data = await response.json();
  return Array.isArray(data.tags) ? data.tags : [];
}

/**
 * Quote a value for a double-quoted slash-command argument.
 *
 * `applyTags` parses the command with PARSER_FLAG.STRICT_ESCAPING, where ST's
 * parser (`SlashCommandParser#testSymbol`) handles a run of backslashes in
 * front of a quote as a unit: `2k` backslashes produce `k` literal backslashes
 * with the quote staying a delimiter, `2k+1` produce `k` literal backslashes
 * and a literal quote. So a `"` is escaped as `\"`, and a backslash run is
 * doubled only when a quote follows it (or the closing quote does). The old
 * behavior stripped `"` and `\` instead of escaping: a tag like `a"b` silently
 * became `ab` and merged with an existing `ab` tag.
 */
function slashQuote(value) {
  let out = '';
  const text = String(value);
  let i = 0;
  while (i < text.length) {
    const char = text[i];
    if (char === '"') {
      out += '\\"';
      i += 1;
      continue;
    }
    if (char === '\\') {
      let run = 0;
      while (i + run < text.length && text[i + run] === '\\') run += 1;
      // Quote follows (value quote or the closing quote) — double the run.
      const quoteFollows = i + run >= text.length || text[i + run] === '"';
      out += '\\'.repeat(quoteFollows ? run * 2 : run);
      i += run;
      continue;
    }
    out += char;
    i += 1;
  }
  return out;
}

async function applyTags(character, tags) {
  if (tags.length === 0) {
    return 0;
  }
  for (const tag of tags) {
    await executeSlashCommandsWithOptions(
      // The avatar is the unique key; the character name may repeat.
      `/tag-add name="${slashQuote(character.avatar)}" "${slashQuote(tag)}"`,
      // Our own escaping rules only hold under strict parsing; the user's
      // Power User flag would otherwise change how `\"`/`\\` are read.
      { parserFlags: { [PARSER_FLAG.STRICT_ESCAPING]: true } },
    );
  }
  if (runOverrides?.write_card_tags ?? config.write_card_tags) {
    await writeCardTags(character, tags);
  }
  return tags.length;
}

/** Card tags: direct assignment, or merge if the field does not exist yet. */
async function writeCardTags(character, tags) {
  const merged = Array.from(new Set([...cardTagNames(character), ...tags]));
  const payload = {
    ch_name: character.name ?? '',
    avatar_url: character.avatar,
    field: 'tags',
    value: merged,
  };
  let written = false;
  try {
    const response = await fetch('/api/characters/edit-attribute', {
      method: 'POST',
      headers: getRequestHeaders(),
      body: JSON.stringify(payload),
    });
    written = response.ok;
  } catch (error) {
    console.warn('[cloufam-tagger] edit-attribute:', error);
  }
  if (!written) {
    try {
      const response = await fetch('/api/characters/merge-attributes', {
        method: 'POST',
        headers: getRequestHeaders(),
        body: JSON.stringify({ avatar: character.avatar, tags: merged, data: { tags: merged } }),
      });
      written = response.ok;
    } catch (error) {
      console.warn('[cloufam-tagger] merge-attributes:', error);
    }
  }
  if (written) {
    // Update the in-memory card snapshot: otherwise a second run would consider
    // the tags old and rewrite the card with only the new ones.
    character.data = character.data ?? {};
    character.data.tags = merged;
    if (Array.isArray(character.tags)) character.tags = merged;
  }
}

// ── Modal window ────────────────────────────────────────────────────────────

function modalElement() {
  return document.getElementById('cloufam-tagger-modal');
}

function logLine(text, kind = '') {
  const log = document.querySelector('#lk-tagger-log');
  if (!log) {
    return;
  }
  const line = document.createElement('div');
  line.className = `lk-tagger-log-line ${kind}`;
  line.textContent = text;
  log.appendChild(line);
  log.scrollTop = log.scrollHeight;
}

function setProgress(done, total, label) {
  const bar = document.querySelector('#lk-tagger-progress-fill');
  const meta = document.querySelector('#lk-tagger-progress-meta');
  if (bar) {
    bar.style.width = total > 0 ? `${Math.round((done / total) * 100)}%` : '0%';
  }
  if (meta) {
    meta.textContent = `${done} / ${total}${label ? ` — ${label}` : ''}`;
  }
}

function modalMarkup() {
  const languageOptions = [
    ['ru', 'русские'],
    ['en', 'английские'],
    ['both', 'русские и английские'],
  ]
    .map(([value, label]) => {
      const selected = (config.language ?? 'ru') === value ? ' selected' : '';
      return `<option value="${value}"${selected}>${escapeHtml(label)}</option>`;
    })
    .join('');

  return `
        <div class="lk-tagger-backdrop" data-lk-close="1"></div>
        <div class="lk-tagger-dialog">
            <div class="lk-tagger-header">
                <span>Авто-теги персонажей</span>
                <div class="lk-tagger-close" data-lk-close="1" title="Закрыть">✕</div>
            </div>
            <div class="lk-tagger-controls">
                <label>Тегов на персонажа
                    <input id="lk-tagger-max" type="number" min="3" max="20" value="${Number(config.max_tags) || 8}">
                </label>
                <label>Язык тегов
                    <select id="lk-tagger-language">${languageOptions}</select>
                </label>
                <label class="lk-tagger-check">
                    <input id="lk-tagger-write-card" type="checkbox" ${config.write_card_tags ? 'checked' : ''}>
                    писать теги в карточку персонажа
                </label>
                <label class="lk-tagger-check">
                    <input id="lk-tagger-only-empty" type="checkbox" checked>
                    только персонажи без тегов
                </label>
            </div>
            <div class="lk-tagger-model-line">Модель: <b>${escapeHtml(config.model ?? '—')}</b> (меняется в настройках расширения)</div>
            <div class="lk-tagger-list-header">
                <span id="lk-tagger-count"></span>
                <div class="lk-tagger-actions">
                    <div class="lk-tagger-mini" id="lk-tagger-select-empty">выбрать без тегов</div>
                    <div class="lk-tagger-mini" id="lk-tagger-select-none">снять выбор</div>
                </div>
            </div>
            <div class="lk-tagger-list" id="lk-tagger-list"></div>
            <div class="lk-tagger-progress">
                <div class="lk-tagger-progress-track"><div id="lk-tagger-progress-fill"></div></div>
                <div id="lk-tagger-progress-meta"></div>
            </div>
            <div class="lk-tagger-log" id="lk-tagger-log"></div>
            <div class="lk-tagger-footer">
                <div class="lk-tagger-button" id="lk-tagger-run">Проставить теги</div>
                <div class="lk-tagger-button secondary" id="lk-tagger-cancel">Отмена</div>
                <div class="lk-tagger-button secondary" data-lk-close="1">Закрыть</div>
            </div>
        </div>`;
}

function characterRows() {
  return allCharacters()
    .map((character) => {
      const assigned = assignedTagNames(character.avatar);
      const hasTags = assigned.length > 0 || cardTagNames(character).length > 0;
      const hasDescription = descriptionOf(character).length > 0;
      const badges =
        assigned.length > 0
          ? `<span class="lk-tagger-tags">${escapeHtml(assigned.slice(0, 6).join(', '))}</span>`
          : '<span class="lk-tagger-tags empty">без тегов</span>';
      const note = hasDescription ? '' : '<span class="lk-tagger-note">нет описания</span>';
      // data-avatar instead of data-index: the allCharacters() array is live, and by index
      // a row could point at a different character after the list refresh.
      return `<label class="lk-tagger-row" data-avatar="${escapeHtml(character.avatar)}">
                <input type="checkbox" class="lk-tagger-row-check" data-has-tags="${hasTags ? '1' : '0'}"
                    data-described="${hasDescription ? '1' : '0'}">
                <span class="lk-tagger-name">${escapeHtml(character.name ?? character.avatar)}</span>
                ${badges}${note}
            </label>`;
    })
    .join('');
}

function openModal() {
  if (modalElement()) {
    return;
  }
  const modal = document.createElement('div');
  modal.id = 'cloufam-tagger-modal';
  modal.innerHTML = modalMarkup();
  document.body.appendChild(modal);

  document.querySelector('#lk-tagger-list').innerHTML = characterRows();
  refreshCount();

  modal.addEventListener('click', (event) => {
    if (event.target.dataset?.lkClose) {
      modal.remove();
    }
  });
  document.querySelector('#lk-tagger-select-empty').addEventListener('click', () => {
    document.querySelectorAll('.lk-tagger-row-check').forEach((check) => {
      check.checked = check.dataset.hasTags === '0' || check.dataset.described === '0';
      if (check.dataset.hasTags === '1') {
        check.checked = false;
      }
    });
    refreshCount();
  });
  document.querySelector('#lk-tagger-select-none').addEventListener('click', () => {
    document.querySelectorAll('.lk-tagger-row-check').forEach((check) => {
      check.checked = false;
    });
    refreshCount();
  });
  document.querySelector('#lk-tagger-only-empty').addEventListener('change', (event) => {
    if (event.target.checked) {
      document.querySelector('#lk-tagger-select-empty').click();
    }
  });
  document.querySelector('#lk-tagger-list').addEventListener('change', refreshCount);
  document.querySelector('#lk-tagger-run').addEventListener('click', () => runSelected());
  document.querySelector('#lk-tagger-cancel').addEventListener('click', () => {
    cancelled = true;
    logLine('Отмена запрошена…', 'warn');
  });

  // Defaults: checkboxes on characters without tags.
  document.querySelector('#lk-tagger-select-empty').click();
}

function refreshCount() {
  const checks = Array.from(document.querySelectorAll('.lk-tagger-row-check'));
  const selected = checks.filter((check) => check.checked).length;
  const counter = document.querySelector('#lk-tagger-count');
  if (counter) {
    counter.textContent = `Выбрано: ${selected} из ${checks.length}`;
  }
}

function selectedCharacters() {
  const rows = Array.from(document.querySelectorAll('.lk-tagger-row'));
  return rows
    .filter((row) => row.querySelector('.lk-tagger-row-check')?.checked)
    .map((row) => allCharacters().find((character) => character.avatar === row.dataset.avatar))
    .filter((character) => character && descriptionOf(character).length > 0);
}

async function runSelected() {
  if (running) {
    // A silent exit looked like a broken button: the lock is also held by
    // the background auto-tagging run after import, which takes minutes.
    const message = 'Подождите: простановка тегов уже идёт (включая фоновую после импорта).';
    logLine(message, 'warn');
    toastr?.info?.(message, 'Теги');
    return;
  }
  const targets = selectedCharacters();
  if (targets.length === 0) {
    logLine('Нет выбранных персонажей с описанием', 'warn');
    return;
  }
  running = true;
  cancelled = false;
  runOverrides = {
    max_tags: Number(document.querySelector('#lk-tagger-max')?.value) || config.max_tags,
    language: document.querySelector('#lk-tagger-language')?.value || config.language,
    write_card_tags:
      document.querySelector('#lk-tagger-write-card')?.checked ?? config.write_card_tags,
  };
  const runButton = document.querySelector('#lk-tagger-run');
  if (runButton) {
    runButton.classList.add('disabled');
  }
  logLine(`Начинаю: ${targets.length} персонажей, модель ${config.model ?? '—'}`);
  let done = 0;
  let tagged = 0;
  setProgress(0, targets.length, '');
  for (const character of targets) {
    if (cancelled) {
      logLine('Остановлено пользователем', 'warn');
      break;
    }
    setProgress(done, targets.length, character.name ?? character.avatar);
    try {
      const tags = await requestTags(character, runOverrides);
      if (tags.length === 0) {
        logLine(`${character.name}: модель не предложила тегов`, 'warn');
      } else {
        await applyTags(character, tags);
        tagged += 1;
        logLine(`${character.name}: ${tags.join(', ')}`, 'ok');
      }
    } catch (error) {
      logLine(`${character.name}: ошибка — ${error}`, 'err');
    }
    done += 1;
    setProgress(done, targets.length, '');
  }
  logLine(`Готово: теги проставлены ${tagged} из ${targets.length}`, 'ok');
  running = false;
  runOverrides = null;
  if (runButton) {
    runButton.classList.remove('disabled');
  }
}

// ── Auto mode and panel ───────────────────────────────────────────────────────

// The check runs after every character list load, while saving settings is
// serializing the whole settings.json (~140 KB), a POST and a disk write:
// we save only if the character set really changed.
function rememberSeen(state, list) {
  const avatars = list.map((character) => character.avatar);
  const seen = new Set(Array.isArray(state.seen) ? state.seen : []);
  if (avatars.length === seen.size && avatars.every((avatar) => seen.has(avatar))) {
    return;
  }
  state.seen = avatars;
  saveSettingsDebounced();
}

/** Add processed ones to already seen without losing the previous ones (import tail). */
function rememberSeenUnion(state, avatars) {
  const seen = new Set(Array.isArray(state.seen) ? state.seen : []);
  const before = seen.size;
  for (const avatar of avatars) {
    if (avatar) seen.add(avatar);
  }
  if (seen.size === before) {
    return;
  }
  state.seen = Array.from(seen);
  saveSettingsDebounced();
}

async function autoTagNewCharacters() {
  // Shared lock with runSelected: otherwise two CHARACTER_PAGE_LOADED in a row run
  // two batches concurrently (both pass the check before the first await).
  if (running) {
    return;
  }
  running = true;
  try {
    const list = allCharacters();
    if (!Array.isArray(list) || list.length === 0) {
      return;
    }
    // Reload the config from the server: when settings change from the Cloufam window,
    // push_settings_to_client calls __lkReloadTaggerConfig asynchronously,
    // and if CHARACTER_PAGE_LOADED fires before the fetch completes,
    // config.enabled stays the old one (true). Without the reload the race
    // leads to tags being assigned while the setting is off.
    await loadConfig();
    // Always update seen, even with auto-tags off: otherwise
    // characters created with the setting off do not get into seen
    // and will get tags automatically when it is turned back on.
    const state = settings();
    const seen = Array.isArray(state.seen) ? state.seen : [];
    const known = new Set(seen);
    const fresh = list.filter((character) => !known.has(character.avatar));
    // Off or first run — remember the current set without tagging anything.
    if (!config.enabled || seen.length === 0) {
      rememberSeen(state, list);
      return;
    }
    if (fresh.length === 0) {
      return;
    }
    const candidates = fresh
      .filter(
        (character) =>
          descriptionOf(character).length > 0 &&
          // A character's own card tags are its data: we do not overwrite them with ours under
          // any settings; that would be "tag translation" against its will.
          cardTagNames(character).length === 0 &&
          assignedTagNames(character.avatar).length === 0,
      )
      .slice(0, AUTO_LIMIT_PER_RUN);
    // Mark only actually processed ones as seen: otherwise an import tail of more
    // than 20 cards would forever remain without auto-tags.
    const processed = [];
    if (candidates.length > 0) {
      toastr.info(`Авто-теги: обрабатываю ${candidates.length} новых персонажей`, 'Cloufam');
      for (const character of candidates) {
        try {
          const tags = await requestTags(character);
          if (tags.length > 0) {
            await applyTags(character, tags);
            // toastr escapes it itself (`escapeHtml: true` in ST); a second pass
            // would show `&amp;` instead of `&`.
            toastr.success(`${character.name}: ${tags.join(', ')}`, 'Авто-теги');
          }
          processed.push(character.avatar);
        } catch (error) {
          console.warn('[cloufam-tagger] авто-тег:', error);
        }
      }
    }
    rememberSeenUnion(state, processed);
  } finally {
    running = false;
  }
}

function renderSettingsBlock(errorText = '') {
  const container = document.getElementById('lk-tagger-settings');
  if (!container) {
    return;
  }
  const models = (config.models ?? [])
    .map(
      (model) =>
        `<option value="${escapeHtml(model)}"${model === config.tagger_model ? ' selected' : ''}>${escapeHtml(model)}</option>`,
    )
    .join('');
  const markup = `
        <div class="lk-tagger-settings-title">Авто-теги персонажей (Cloufam)</div>
        ${errorText ? `<div class="lk-tagger-settings-error">Шлюз недоступен: ${escapeHtml(errorText)}</div>` : ''}
        <label class="lk-tagger-check">
            <input id="lk-tagger-cfg-enabled" type="checkbox" ${config.enabled ? 'checked' : ''}>
            проставлять теги новым персонажам автоматически
        </label>
        <label class="lk-tagger-check">
            <input id="lk-tagger-cfg-card" type="checkbox" ${config.write_card_tags ? 'checked' : ''}>
            писать теги в карточку персонажа
        </label>
        <label class="lk-tagger-check">
            <input id="lk-tagger-cfg-translate" type="checkbox" ${config.auto_translate ? 'checked' : ''}>
            переводить новые карточки и ключи лорбуков на русский автоматически
        </label>
        <div class="lk-tagger-setting-row">
            <span>Модель</span>
            <select id="lk-tagger-cfg-model">
                <option value=""${!config.tagger_model ? ' selected' : ''}>как основная чат-модель</option>
                ${models}
            </select>
        </div>
        <div class="lk-tagger-setting-row">
            <span>Тегов на персонажа</span>
            <input id="lk-tagger-cfg-max" type="number" min="3" max="20" value="${Number(config.max_tags) || 8}">
        </div>
        <div class="lk-tagger-setting-row">
            <span>Язык тегов</span>
            <select id="lk-tagger-cfg-language">
                <option value="ru"${config.language === 'ru' ? ' selected' : ''}>русские</option>
                <option value="en"${config.language === 'en' ? ' selected' : ''}>английские</option>
                <option value="both"${config.language === 'both' ? ' selected' : ''}>русские и английские</option>
            </select>
        </div>
        <div class="lk-tagger-settings-hint">Теги ставятся пунктом «🏷 Теги персонажей» в меню расширений SillyTavern (жезл рядом с полем ввода).</div>`;
  // Timers and character list events call loadConfig: re-rendering the same
  // block or the block under the cursor recreates the fields and resets focus and input.
  if (container.lkMarkup === markup || container.contains(document.activeElement)) {
    return;
  }
  container.lkMarkup = markup;
  container.innerHTML = markup;

  const push = (patch) =>
    saveConfig(patch).catch((error) => toastr.error(String(error), 'Cloufam'));
  container
    .querySelector('#lk-tagger-cfg-enabled')
    ?.addEventListener('change', (event) => push({ enabled: event.target.checked }));
  container
    .querySelector('#lk-tagger-cfg-card')
    ?.addEventListener('change', (event) => push({ write_card_tags: event.target.checked }));
  container
    .querySelector('#lk-tagger-cfg-translate')
    ?.addEventListener('change', (event) => push({ auto_translate: event.target.checked }));
  container
    .querySelector('#lk-tagger-cfg-model')
    ?.addEventListener('change', (event) => push({ model: event.target.value }));
  container
    .querySelector('#lk-tagger-cfg-max')
    ?.addEventListener('change', (event) => push({ max_tags: Number(event.target.value) }));
  container
    .querySelector('#lk-tagger-cfg-language')
    ?.addEventListener('change', (event) => push({ language: event.target.value }));
}

function injectSettingsBlock() {
  if (document.getElementById('lk-tagger-settings')) {
    return;
  }
  const host =
    document.getElementById('extensions_settings2') ||
    document.getElementById('extensions_settings');
  if (!host) {
    return;
  }
  const block = document.createElement('div');
  block.id = 'lk-tagger-settings';
  block.className = 'extension_container';
  host.appendChild(block);
  renderSettingsBlock();
}

/** Item in the wand menu (`#extensionsMenu`) — ST's standard place for extension tools.
 *  The `#top-bar` trick gave overlap with `#top-settings-holder` (z-index 3005) and non-working clicks. */
function injectWandItem() {
  const menu = document.getElementById('extensionsMenu');
  if (!menu) {
    return;
  }
  let container = document.getElementById(WAND_CONTAINER_ID);
  if (!container) {
    container = document.createElement('div');
    container.id = WAND_CONTAINER_ID;
    container.className = 'extension_container';
    menu.prepend(container);
  }
  if (!container.querySelector(`#${WAND_ITEM_ID}`)) {
    const item = document.createElement('div');
    item.id = WAND_ITEM_ID;
    item.className = 'interactable';
    item.title = 'Авто-теги персонажей (Cloufam)';
    item.innerHTML =
      '<div class="fa-solid fa-tags extensionsMenuExtensionButton"></div><span>Теги персонажей</span>';
    item.addEventListener('click', () => openModal());
    container.append(item);
  }

  // Second item — straight to the character settings of the selected character: from the chat
  // the path to the trait and state sliders must be one click.
  if (!container.querySelector(`#${TRAITS_ITEM_ID}`)) {
    const traitsItem = document.createElement('div');
    traitsItem.id = TRAITS_ITEM_ID;
    traitsItem.className = 'interactable';
    traitsItem.title = 'Черты и состояние персонажа (Cloufam)';
    traitsItem.innerHTML =
      '<div class="fa-solid fa-heart-pulse extensionsMenuExtensionButton"></div><span>Черты и характер</span>';
    traitsItem.addEventListener('click', () => openTraitsSettings());
    container.append(traitsItem);
  }

  // Third item — character relationships: between characters and toward the companion.
  if (!container.querySelector(`#${RELATIONSHIPS_ITEM_ID}`)) {
    const relsItem = document.createElement('div');
    relsItem.id = RELATIONSHIPS_ITEM_ID;
    relsItem.className = 'interactable';
    relsItem.title = 'Отношения персонажа (Cloufam)';
    relsItem.innerHTML =
      '<div class="fa-solid fa-heart extensionsMenuExtensionButton"></div><span>Отношения</span>';
    relsItem.addEventListener('click', () => openRelationshipsModal());
    container.append(relsItem);
  }

  // Fourth item — RPG panel: world, stats, skills, quests, inventory, combat.
  if (!container.querySelector(`#${RPG_ITEM_ID}`)) {
    const rpgItem = document.createElement('div');
    rpgItem.id = RPG_ITEM_ID;
    rpgItem.className = 'interactable';
    rpgItem.title = 'RPG-панель (Cloufam)';
    rpgItem.innerHTML =
      '<div class="fa-solid fa-shield-halved extensionsMenuExtensionButton"></div><span>RPG</span>';
    rpgItem.addEventListener('click', () => openRpgPanel());
    container.append(rpgItem);
  }

  // Fifth item — import a draft text into the card fields.
  if (!container.querySelector(`#${MD_IMPORT_ITEM_ID}`)) {
    const importItem = document.createElement('div');
    importItem.id = MD_IMPORT_ITEM_ID;
    importItem.className = 'interactable';
    importItem.title = 'Импорт черновика в карточку персонажа (Cloufam)';
    importItem.innerHTML =
      '<div class="fa-solid fa-file-import extensionsMenuExtensionButton"></div><span>Импорт черновика</span>';
    importItem.addEventListener('click', () => openMdImportModal());
    container.append(importItem);
  }

  // Sixth item — "What's new": the update journal.
  const clId = 'cloufam-changelog-menu-item';
  if (!container.querySelector(`#${clId}`)) {
    const clItem = document.createElement('div');
    clItem.id = clId;
    clItem.className = 'interactable';
    clItem.title = 'Что нового (Cloufam)';
    clItem.innerHTML =
      '<div class="fa-solid fa-wand-magic-sparkles extensionsMenuExtensionButton"></div><span>Что нового</span>';
    clItem.addEventListener('click', () => openChangelogModal());
    container.append(clItem);
  }

  // Seventh — export the open chat to data/chat-backups.
  if (!container.querySelector(`#${CHAT_EXPORT_ITEM_ID}`)) {
    const exportItem = document.createElement('div');
    exportItem.id = CHAT_EXPORT_ITEM_ID;
    exportItem.className = 'interactable';
    exportItem.title = 'Сохранить чат в файл (Cloufam)';
    exportItem.innerHTML =
      '<div class="fa-solid fa-file-export extensionsMenuExtensionButton"></div><span>Экспорт чата</span>';
    exportItem.addEventListener('click', () => exportCurrentChat());
    container.append(exportItem);
  }

  // Eighth — import a chat from json/jsonl with ST's standard mechanism.
  if (!container.querySelector(`#${CHAT_IMPORT_ITEM_ID}`)) {
    const chatImportItem = document.createElement('div');
    chatImportItem.id = CHAT_IMPORT_ITEM_ID;
    chatImportItem.className = 'interactable';
    chatImportItem.title = 'Загрузить чат из файла (Cloufam)';
    chatImportItem.innerHTML =
      '<div class="fa-solid fa-file-import extensionsMenuExtensionButton"></div><span>Импорт чата</span>';
    chatImportItem.addEventListener('click', () => importChatFile());
    container.append(chatImportItem);
  }

  // Ninth — update the installed SillyTavern.
  if (!container.querySelector(`#${ST_UPDATE_ITEM_ID}`)) {
    const stItem = document.createElement('div');
    stItem.id = ST_UPDATE_ITEM_ID;
    stItem.className = 'interactable';
    stItem.title = 'Обновить SillyTavern (Cloufam)';
    stItem.innerHTML =
      '<div class="fa-solid fa-arrows-rotate extensionsMenuExtensionButton"></div><span>Обновить SillyTavern</span>';
    stItem.addEventListener('click', () => openStUpdateModal());
    container.append(stItem);
  }
}

// ── Chat export/import and SillyTavern update ─────────────────────────────

function esc(value) {
  return String(value ?? '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

/** Save the open chat to `data/chat-backups`: we take the content from ST's standard
 *  `/api/chats/export` — the chat file layout remains its concern. */
async function exportCurrentChat() {
  const gateway = window.__LK_GATEWAY;
  try {
    const context = getContext();
    // Group chat: characterId is empty in ST, while export supports is_group.
    const isGroup = context.groupId !== undefined && context.groupId !== null;
    const avatar = context.characters?.[context.characterId]?.avatar;
    if (!isGroup && !avatar) {
      toastr.warning('Откройте чат персонажа, чтобы сохранить его', 'Экспорт чата');
      return;
    }
    await context.saveChat();
    const base = String(
      context.chatId ?? (isGroup ? context.groups?.[context.groupId]?.chat_id : '') ?? '',
    );
    if (!base) {
      toastr.warning('Не удалось определить файл открытого чата', 'Экспорт чата');
      return;
    }
    const file = base.endsWith('.jsonl') ? base : `${base}.jsonl`;
    const exportBody = { is_group: isGroup, file, exportfilename: file, format: 'jsonl' };
    if (!isGroup) exportBody.avatar_url = avatar;
    const response = await fetch('/api/chats/export', {
      method: 'POST',
      headers: getRequestHeaders(),
      body: JSON.stringify(exportBody),
    });
    const data = await response.json();
    if (!response.ok) throw new Error(data?.message || `HTTP ${response.status}`);
    const saved = await fetch(`${gateway}/api/cloufam/chat/export`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ filename: file, content: data.result }),
    });
    const result = await saved.json();
    if (!result.ok) throw new Error(result.error || `HTTP ${saved.status}`);
    toastr.success(`Чат сохранён: ${result.path}`, 'Экспорт чата');
  } catch (error) {
    toastr.error(String(error?.message ?? error), 'Экспорт чата');
  }
}

/** Pass the selected file to ST's standard import (`#chat_import_file`), including
 *  its hint to open the imported chat. The `window.__lkImportChatFile` hook
 *  is called by the `lk-chat-io-check.mjs` stand. */
function forwardChatFile(file) {
  const context = getContext();
  const stImport = document.getElementById('chat_import_file');
  const form = document.getElementById('form_import_chat');
  if (!stImport || !form) {
    throw new Error('в интерфейсе ST не нашёлся штатный импорт чата');
  }
  if (!context.characters?.[context.characterId]?.avatar) {
    toastr.warning('Откройте чат персонажа, к которому относится импорт', 'Импорт чата');
    return;
  }
  // ST form fields say who gets the imported chat: without them
  // the import would go to the wrong character. This is the import form, not a card —
  // its fields have no autosave (see gotchas.md, pitfall 19.09).
  const nameField = form.querySelector('#chat_import_character_name');
  const avatarField = form.querySelector('#chat_import_avatar_url');
  if (nameField) nameField.value = context.name2 ?? '';
  if (avatarField) avatarField.value = context.characters[context.characterId].avatar;
  const transfer = new DataTransfer();
  transfer.items.add(file);
  stImport.files = transfer.files;
  stImport.dispatchEvent(new Event('change', { bubbles: true }));
}

/** Load a chat from json/jsonl with ST's standard mechanism. */
function importChatFile() {
  const input = document.createElement('input');
  input.type = 'file';
  input.accept = '.json,.jsonl';
  input.addEventListener('change', () => {
    const file = input.files?.[0];
    if (!file) return;
    try {
      forwardChatFile(file);
    } catch (error) {
      toastr.error(String(error?.message ?? error), 'Импорт чата');
    }
  });
  input.click();
}
window.__lkImportChatFile = forwardChatFile;

async function openStUpdateModal() {
  if (document.getElementById(ST_UPDATE_MODAL_ID)) return;
  const gateway = window.__LK_GATEWAY;
  let info;
  try {
    const response = await fetch(`${gateway}/api/cloufam/st/update-check`);
    info = await response.json();
  } catch (error) {
    toastr.error(
      `Не удалось проверить версию SillyTavern: ${String(error?.message ?? error)}`,
      'Обновление ST',
    );
    return;
  }
  const modal = document.createElement('div');
  modal.id = ST_UPDATE_MODAL_ID;
  modal.innerHTML = stUpdateModalMarkup(info);
  document.body.appendChild(modal);
  modal.addEventListener('click', (event) => {
    if (event.target.dataset?.lkClose) modal.remove();
  });
  document.querySelector('#lk-st-update-run')?.addEventListener('click', () => runStUpdate(info));
}

function stUpdateModalMarkup(info) {
  const latest = info.latest ? esc(info.latest) : '—';
  const upToDate = !info.latest || info.latest === info.installed;
  return `
        <div class="lk-tagger-backdrop" data-lk-close="1"></div>
        <div class="lk-tagger-dialog" style="max-width: 560px;">
            <div class="lk-tagger-header">
                <span>Обновление SillyTavern</span>
                <div class="lk-tagger-close" data-lk-close="1" title="Закрыть">✕</div>
            </div>
            <p class="lk-hint">
                Установлена версия <b>${esc(info.installed)}</b>, в апстриме доступна <b>${latest}</b>.
                Cloufam проверен с SillyTavern <b>${esc(info.pinned)}</b>: на других версиях часть
                функций (правки промпта, лорбуки, быстрый стрим) может отключиться до выхода их
                поддержки. Обновление скачает архив, переустановит зависимости и перезапустит
                SillyTavern; чаты и настройки не трогаются.
            </p>
            ${info.error ? `<div class="lk-tagger-settings-error">${esc(info.error)}</div>` : ''}
            <div class="lk-tagger-footer">
                <div class="lk-tagger-button${upToDate ? ' disabled' : ''}" id="lk-st-update-run">
                    ${upToDate ? 'Уже последняя версия' : `Обновить до ${latest}`}
                </div>
                <div class="lk-tagger-button secondary" data-lk-close="1">Отмена</div>
            </div>
            <div id="lk-st-update-result"></div>
        </div>`;
}

async function runStUpdate(info) {
  const gateway = window.__LK_GATEWAY;
  const button = document.querySelector('#lk-st-update-run');
  const result = document.querySelector('#lk-st-update-result');
  if (!info?.latest || info.latest === info.installed) return;
  if (button) button.classList.add('disabled');
  if (result) {
    result.innerHTML =
      '<div class="lk-relationships-loading">Обновляю: скачивание, распаковка, зависимости…</div>';
  }
  try {
    // ST update — downloading and unpacking tens of megabytes: the timeout
    // here is obviously larger than for other routes, but a hang is still visible.
    const response = await gatewayFetch(
      `${gateway}/api/cloufam/st/update`,
      {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ version: info.latest }),
      },
      600000,
    );
    const data = await response.json();
    if (!data.ok) throw new Error(data.error || `HTTP ${response.status}`);
    const warnings = Array.isArray(data.warnings) ? data.warnings : [];
    const restarted = data.restarted
      ? 'SillyTavern перезапущена.'
      : 'SillyTavern не запустилась — перезапустите Cloufam.';
    const warningsText = warnings.length
      ? ` Не легли патчи Cloufam: ${esc(warnings.join('; '))}.`
      : '';
    if (result) {
      result.innerHTML = `<div class="lk-hint">Обновлено ${esc(data.from)} → ${esc(data.to)}. ${restarted}${warningsText}</div>`;
    }
    if (warnings.length) {
      toastr.warning(`Обновлено, но не легли патчи: ${warnings.join('; ')}`, 'Обновление ST');
    } else {
      toastr.success(`SillyTavern обновлена до ${data.to}`, 'Обновление ST');
    }
  } catch (error) {
    const message = String(error?.message ?? error);
    if (result) result.innerHTML = `<div class="lk-tagger-settings-error">${esc(message)}</div>`;
    toastr.error(message, 'Обновление ST');
    if (button) button.classList.remove('disabled');
  }
}

// ── Character traits: tag intensity and no-repeat of actions ─────────────
//
// Settings live in the app, while the text for generation is delivered to the card:
// SillyTavern substitutes the character's `depth_prompt` into the prompt at a given depth
// as a separate message. The card is not rewritten because of this, and the description
// stays authored.

/** Tell the app which chat is ongoing: by this name the gateway mixes the character's
 * live state into the prompt. Chat change and card opening — both cases. */
async function reportActiveCharacter() {
  try {
    const context = getContext();
    const character = allCharacters()[context.characterId];
    const avatar = character?.avatar ?? '';
    // The person's name: a character has a separate attitude toward each interlocutor.
    const persona = String(context.name1 ?? '').trim();
    await fetch(pluginUrl('active-character'), {
      method: 'POST',
      headers: getRequestHeaders(),
      body: JSON.stringify({ avatar, persona }),
    });
  } catch (error) {
    console.warn('[cloufam-tagger] active-character:', error);
  }
}

/** Open Cloufam settings straight on the character traits tab. */
async function openTraitsSettings() {
  const gateway = window.__LK_GATEWAY;
  if (!gateway) {
    return;
  }
  try {
    await fetch(`${gateway}/api/cloufam/open-settings`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ tab: 'tags' }),
    });
  } catch (error) {
    console.warn('[cloufam-tagger] open-settings:', error);
  }
}

/** Apply the traits configured in Cloufam to the cards. Idempotent. */
async function applyTraits() {
  let map = {};
  try {
    const response = await fetch(pluginUrl('traits'), { headers: getRequestHeaders() });
    if (!response.ok) {
      return;
    }
    map = await response.json();
  } catch (error) {
    console.warn('[cloufam-tagger] traits:', error);
    return;
  }
  const characters = allCharacters();
  for (const [avatar, fragment] of Object.entries(map ?? {})) {
    const character = characters.find((c) => c.avatar === avatar);
    const current = character?.data?.extensions?.depth_prompt?.prompt ?? '';
    const next = withTraitsFragment(current, fragment);
    if (!character || current === next) {
      continue;
    }
    // Only the text under the marker is ours: the depth and role could have been set by the card author.
    // 4/system — ST defaults for a card without its own values.
    const previous = character.data?.extensions?.depth_prompt ?? {};
    const depthPrompt = {
      prompt: next,
      depth: previous.depth ?? 4,
      role: previous.role ?? 'system',
    };
    try {
      const response = await fetch('/api/characters/merge-attributes', {
        method: 'POST',
        headers: getRequestHeaders(),
        body: JSON.stringify({
          avatar,
          data: { extensions: { depth_prompt: depthPrompt } },
        }),
      });
      if (!response.ok) {
        // fetch also resolves on 4xx/5xx: without the check the note was considered
        // applied in memory and never rewritten again.
        console.warn('[cloufam-tagger] depth_prompt: HTTP', response.status);
        continue;
      }
      // The live page keeps cards in memory: without an in-memory edit
      // the changes would be visible only after a reload.
      character.data.extensions = character.data.extensions ?? {};
      character.data.extensions.depth_prompt = depthPrompt;
    } catch (error) {
      console.warn('[cloufam-tagger] depth_prompt:', error);
    }
  }
}

/** Marker of our part in Character's Note: below it the text is ours, above it is the author's. */
const TRAITS_MARKER = '<!-- cloufam: черты характера -->';

/**
 * Build Character's Note: the author's text on top, our fragment under the marker.
 * Earlier the fragment was written over the whole field, and a note written by a human
 * ("answer only in verse") disappeared on the first pass — characters that were
 * never opened in Cloufam were also overwritten.
 */
function withTraitsFragment(current, fragment) {
  const marker = current.indexOf(TRAITS_MARKER);
  const authored = (marker === -1 ? current : current.slice(0, marker)).trimEnd();
  if (!fragment) {
    return authored;
  }
  return authored
    ? `${authored}

${TRAITS_MARKER}
${fragment}`
    : `${TRAITS_MARKER}
${fragment}`;
}

// ── Character relationships ────────────────────────────────────────────────
//
// Modal for viewing inter-character relationships and the attitude toward the companion.
// Data comes from relationships.json and traits.json through the gateway API.

const RELATIONSHIPS_MODAL_ID = 'cloufam-relationships-modal';

/** Open the relationships modal for the current character from the chat. */
async function openRelationshipsModal() {
  if (document.getElementById(RELATIONSHIPS_MODAL_ID)) {
    return;
  }
  const context = getContext();
  const character = allCharacters()[context.characterId];
  const avatar = character?.avatar ?? '';
  if (!avatar) {
    toastr.warning('Нет активного персонажа в чате', 'Cloufam');
    return;
  }
  const modal = document.createElement('div');
  modal.id = RELATIONSHIPS_MODAL_ID;
  modal.innerHTML = relationshipsModalMarkup();
  document.body.appendChild(modal);

  // Closing by clicking the backdrop or the button
  modal.addEventListener('click', (event) => {
    if (event.target.dataset?.lkClose) {
      modal.remove();
    }
  });

  // Load the data
  await loadRelationshipsData(avatar);
}

/** HTML markup of the relationships modal. */
function relationshipsModalMarkup() {
  return `
        <div class="lk-tagger-backdrop" data-lk-close="1"></div>
        <div class="lk-tagger-dialog" style="max-width: 600px;">
            <div class="lk-tagger-header">
                <span>Отношения персонажа</span>
                <div class="lk-tagger-close" data-lk-close="1" title="Закрыть">✕</div>
            </div>
            <div class="lk-relationships-content">
                <div class="lk-relationships-loading">Загрузка…</div>
            </div>
            <div class="lk-tagger-footer">
                <div class="lk-tagger-button secondary" data-lk-close="1">Закрыть</div>
            </div>
        </div>`;
}

/** Load and display the relationship data. */
async function loadRelationshipsData(avatar) {
  const content = document.querySelector(`#${RELATIONSHIPS_MODAL_ID} .lk-relationships-content`);
  if (!content) {
    return;
  }
  try {
    const gateway = window.__LK_GATEWAY;
    if (!gateway) {
      content.innerHTML = '<div class="lk-relationships-error">Шлюз недоступен</div>';
      return;
    }
    const response = await gatewayFetch(
      `${gateway}/api/cloufam/character-relationships?avatar=${encodeURIComponent(avatar)}`,
    );
    if (!response.ok) {
      throw new Error(`HTTP ${response.status}`);
    }
    const data = await response.json();
    renderRelationships(content, data);
  } catch (error) {
    content.innerHTML = `<div class="lk-relationships-error">Ошибка загрузки: ${escapeHtml(String(error))}</div>`;
  }
}

/** Render the relationship data in the container. */
function renderRelationships(container, data) {
  const { character, inter_character, persona_relations, bonds } = data;
  const context = getContext();
  const currentPersona = String(context.name1 ?? '').trim();

  // Collect the HTML
  let html = `<div class="lk-relationships-title">${escapeHtml(character)}</div>`;

  // Companion (persona) selection
  const personas = Object.keys(persona_relations || {}).filter((p) => p !== '');
  if (personas.length > 0 || currentPersona) {
    html += `
            <div class="lk-relationships-section">
                <div class="lk-relationships-section-title">Спутник</div>
                <select id="lk-relationships-persona-select" class="lk-relationships-select">
                    ${currentPersona ? `<option value="${escapeHtml(currentPersona)}">${escapeHtml(currentPersona)} (текущий)</option>` : ''}
                    ${personas
                      .filter((p) => p !== currentPersona)
                      .map((p) => `<option value="${escapeHtml(p)}">${escapeHtml(p)}</option>`)
                      .join('')}
                    <option value="">Анонимный</option>
                </select>
            </div>`;
  }

  // Attitude toward the companion block
  html += `
        <div class="lk-relationships-section" id="lk-relationships-persona-block">
            <div class="lk-relationships-section-title">Отношение к спутнику</div>
            <div id="lk-relationships-persona-content"></div>
        </div>`;

  // Manual bonds block: what the character thinks of others.
  html += `
        <div class="lk-relationships-section">
            <div class="lk-relationships-section-title">Что он о них думает</div>
            <div id="lk-relationships-bonds-content"></div>
        </div>`;

  // Inter-character relationships block
  html += `
        <div class="lk-relationships-section">
            <div class="lk-relationships-section-title">Оценка по переписке (авто)</div>
            <div id="lk-relationships-inter-character-content"></div>
        </div>`;

  container.innerHTML = html;

  // Fill in the data
  updatePersonaRelations(persona_relations, currentPersona || '');
  renderBonds(bonds || []);
  renderInterCharacter(inter_character || []);

  // Companion change handler
  const select = document.getElementById('lk-relationships-persona-select');
  if (select) {
    select.addEventListener('change', () => {
      updatePersonaRelations(persona_relations, select.value);
    });
  }
}

/** Update the attitude toward the companion block. */
function updatePersonaRelations(personaRelations, persona) {
  const content = document.getElementById('lk-relationships-persona-content');
  if (!content) {
    return;
  }
  const state = personaRelations?.[persona] || {
    affection: 50,
    trust: 50,
    energy: 55,
    irritation: 10,
  };
  content.innerHTML = renderLiveState(state);
}

/** Bonds set manually in Cloufam. There are no input fields here: writing into
 * SillyTavern forms saves the card, while edits from this window still reach
 * the card only through saving in Cloufam. */
function renderBonds(bonds) {
  const content = document.getElementById('lk-relationships-bonds-content');
  if (!content) {
    return;
  }
  if (bonds.length === 0) {
    content.innerHTML =
      '<div class="lk-relationships-empty">Связей нет — о других персонажах модель говорит по своему усмотрению.</div>' +
      '<div class="lk-tagger-button secondary" id="lk-relationships-bonds-open">Задать в Cloufam</div>';
    const open = document.getElementById('lk-relationships-bonds-open');
    if (open) {
      open.addEventListener('click', () => openTraitsSettings());
    }
    return;
  }
  content.innerHTML = bonds.map((bond) => renderBondRow(bond)).join('');
}

/** Bond row: target name, strength bar, name and note. The order is the same
 * as in the card block — the modal and the prompt must not show different lists. */
function renderBondRow(bond) {
  const strength = Math.max(0, Math.min(Number(bond.strength) || 0, 100));
  // "Does not know" is part of the caption: there is no separate chip in the modal CSS, and touching
  // style.css and the extension version for one line is not needed.
  const kind = bond.secret ? `${bond.kind} · не знает` : bond.kind;
  return `
        <div class="lk-relationships-row">
            <div class="lk-relationships-name">${escapeHtml(bond.target)}</div>
            <div class="lk-relationships-bar-container">
                <div class="lk-relationships-bar" style="width: ${strength}%; background-color: #a78bfa;"></div>
            </div>
            <div class="lk-relationships-score">${strength}</div>
            <div class="lk-relationships-label" style="flex: 1; text-align: left;">
                ${escapeHtml(kind)}${bond.note ? `<div style="color: #64748b;">${escapeHtml(bond.note)}</div>` : ''}
            </div>
        </div>`;
}

/** Render inter-character relationships. */
function renderInterCharacter(interCharacter) {
  const content = document.getElementById('lk-relationships-inter-character-content');
  if (!content) {
    return;
  }
  if (interCharacter.length === 0) {
    content.innerHTML =
      '<div class="lk-relationships-empty">Нет данных о межперсонажных отношениях</div>';
    return;
  }
  // Sort by absolute value
  const sorted = [...interCharacter].sort((a, b) => Math.abs(b.score) - Math.abs(a.score));
  content.innerHTML = sorted.map((rel) => renderRelationshipRow(rel)).join('');
}

/** Render one inter-character relationship row. */
function renderRelationshipRow(rel) {
  const score = rel.score;
  const absScore = Math.abs(score);
  const isPositive = score >= 0;
  const color = isPositive ? '#4ade80' : '#f87171';
  const width = Math.min(absScore, 100);
  return `
        <div class="lk-relationships-row">
            <div class="lk-relationships-name">${escapeHtml(rel.target)}</div>
            <div class="lk-relationships-bar-container">
                <div class="lk-relationships-bar" style="width: ${width}%; background-color: ${color};"></div>
            </div>
            <div class="lk-relationships-score">${Math.round(score)}</div>
            <div class="lk-relationships-label">${escapeHtml(rel.label)}</div>
        </div>`;
}

/** Render the live state (affection, trust, energy, irritation). */
function renderLiveState(state) {
  const metrics = [
    { name: 'Нежность', value: state.affection, color: '#f472b6' },
    { name: 'Доверие', value: state.trust, color: '#60a5fa' },
    { name: 'Энергия', value: state.energy, color: '#fbbf24' },
    { name: 'Раздражение', value: state.irritation, color: '#ef4444' },
  ];
  return metrics
    .map((m) => {
      const val = Number(m.value) || 0;
      return `
        <div class="lk-relationships-state-row">
            <span class="lk-relationships-state-name">${m.name}</span>
            <div class="lk-relationships-state-bar-container">
                <div class="lk-relationships-state-bar" style="width: ${val}%; background-color: ${m.color};"></div>
            </div>
            <span class="lk-relationships-state-value">${val}</span>
        </div>`;
    })
    .join('');
}

/** Summary for the slash command: tags of all characters without tags. */
async function tagAllUntagged() {
  // The same lock as the button runs: otherwise /autotags in parallel with
  // a batch duplicated model calls and overwrote card tags.
  if (running) {
    return 'Подождите: простановка тегов уже идёт.';
  }
  running = true;
  try {
    const targets = allCharacters().filter(
      (character) =>
        assignedTagNames(character.avatar).length === 0 && descriptionOf(character).length > 0,
    );
    if (targets.length === 0) {
      return 'Все персонажи уже с тегами (или нет описаний).';
    }
    let tagged = 0;
    const errors = [];
    for (const character of targets.slice(0, AUTO_LIMIT_PER_RUN)) {
      try {
        const tags = await requestTags(character);
        if (tags.length > 0) {
          await applyTags(character, tags);
          tagged += 1;
        }
      } catch (error) {
        errors.push(`${character.name}: ${error}`);
      }
    }
    const tail = errors.length > 0 ? `\nОшибки: ${errors.join('; ')}` : '';
    return `Теги проставлены ${tagged} из ${targets.length}.${tail}`;
  } finally {
    running = false;
  }
}

// ── Importing a draft into the card fields ──────────────────────────────────
//
// The same loop as auto-tags: a local model through the plugin-index.js bridge.
// It saves nothing and creates no character by itself — it only lays out the text
// over the fields and, with a separate button, inserts them into an already open character
// creation/edit form for manual review.

const MD_IMPORT_MODAL_ID = 'cloufam-md-import-modal';

const MD_IMPORT_FIELD_MAP = {
  name: 'character_name_pole',
  description: 'description_textarea',
  personality: 'personality_textarea',
  scenario: 'scenario_pole',
  first_mes: 'firstmessage_textarea',
  mes_example: 'mes_example_textarea',
};

const MD_IMPORT_FIELD_LABELS = {
  name: 'Имя',
  description: 'Описание',
  personality: 'Характер',
  scenario: 'Сценарий',
  first_mes: 'Первое сообщение',
  mes_example: 'Пример диалога',
};

function openMdImportModal() {
  if (document.getElementById(MD_IMPORT_MODAL_ID)) {
    return;
  }
  const modal = document.createElement('div');
  modal.id = MD_IMPORT_MODAL_ID;
  modal.innerHTML = mdImportModalMarkup();
  document.body.appendChild(modal);

  modal.addEventListener('click', (event) => {
    if (event.target.dataset?.lkClose) {
      modal.remove();
    }
  });
  document.querySelector('#lk-md-import-run')?.addEventListener('click', runMdImport);
}

function mdImportModalMarkup() {
  return `
        <div class="lk-tagger-backdrop" data-lk-close="1"></div>
        <div class="lk-tagger-dialog" style="max-width: 640px;">
            <div class="lk-tagger-header">
                <span>Импорт черновика в карточку</span>
                <div class="lk-tagger-close" data-lk-close="1" title="Закрыть">✕</div>
            </div>
            <p class="lk-hint">
                Вставьте заметки, обрывки диалога или прозу — модель разложит их по полям карточки и
                покажет результат для проверки. Заполнить форму можно только при открытом
                «Создать персонажа»: в режиме редактирования SillyTavern сохранила бы черновик
                поверх открытой карточки. Сохранение — штатной кнопкой ST, само ничего не пишется.
            </p>
            <textarea id="lk-md-import-draft" rows="8"
                placeholder="Например: Эльфийка-архивариус, саркастичная, живёт в библиотеке киберпанк-города. «Опять пришёл за книгами, которые не умеешь читать?»"></textarea>
            <div class="lk-tagger-footer">
                <div class="lk-tagger-button" id="lk-md-import-run">Разобрать</div>
                <div class="lk-tagger-button secondary" data-lk-close="1">Закрыть</div>
            </div>
            <div id="lk-md-import-result"></div>
        </div>`;
}

async function runMdImport() {
  const button = document.querySelector('#lk-md-import-run');
  const result = document.querySelector('#lk-md-import-result');
  const draft = document.querySelector('#lk-md-import-draft')?.value?.trim();
  if (!draft) {
    if (result)
      result.innerHTML = '<div class="lk-tagger-settings-error">Вставьте текст черновика.</div>';
    return;
  }
  if (button) button.classList.add('disabled');
  if (result) result.innerHTML = '<div class="lk-relationships-loading">Разбираю…</div>';
  try {
    const response = await fetch(pluginUrl('md-import'), {
      method: 'POST',
      headers: getRequestHeaders(),
      body: JSON.stringify({ draft }),
    });
    if (!response.ok) {
      throw new Error((await response.text()) || `HTTP ${response.status}`);
    }
    renderMdImportResult(await response.json());
  } catch (error) {
    if (result) {
      result.innerHTML = `<div class="lk-tagger-settings-error">Ошибка: ${escapeHtml(String(error))}</div>`;
    }
  } finally {
    if (button) button.classList.remove('disabled');
  }
}

function renderMdImportResult(fields) {
  const result = document.querySelector('#lk-md-import-result');
  if (!result) {
    return;
  }
  const missing = [];
  const rows = Object.entries(MD_IMPORT_FIELD_LABELS)
    .map(([key, label]) => {
      const value = String(fields[key] ?? '').trim();
      if (!value) {
        missing.push(label);
        return '';
      }
      const preview = value.length > 200 ? `${value.slice(0, 200)}…` : value;
      return `<div class="lk-md-import-row">
                <div class="lk-md-import-label">${escapeHtml(label)}</div>
                <div class="lk-md-import-value">${escapeHtml(preview)}</div>
            </div>`;
    })
    .join('');
  const warn =
    missing.length > 0
      ? `<div class="lk-tagger-settings-error">Модель не нашла: ${escapeHtml(missing.join(', '))} — заполните вручную</div>`
      : '';
  result.innerHTML = `
        <div class="lk-md-import-rows">${rows || '<span class="lk-tagger-tags empty">Модель не нашла материала ни для одного поля</span>'}</div>
        ${warn}
        ${rows && getContext().menuType === 'create' ? '<div class="lk-tagger-button" id="lk-md-import-apply">Заполнить форму персонажа</div>' : ''}`;
  document
    .querySelector('#lk-md-import-apply')
    ?.addEventListener('click', () => applyMdImportFields(fields));
}

/** Insert the parsed fields into the character CREATION form.
 *
 * Only in creation mode, and this is not overcaution. ST form fields have a
 * shared `input` handler (script.js), and outside creation mode it calls
 * `saveCharacterDebounced()` → `#create_button` → `POST /api/characters/edit`,
 * i.e. the edit goes to disk over the existing card. And `menu_type`
 * becomes `character_edit` on a mere entry into a chat with a character, while the
 * `#rm_ch_create_block` form block is always in the DOM (just `display:none`) —
 * i.e. without this check, "Fill form" from the chat would silently overwrite
 * the description, personality, scenario, greeting and name of the character the
 * person is talking to at that moment. There is no undo for this. */
function applyMdImportFields(fields) {
  const menuType = getContext().menuType;
  if (menuType !== 'create') {
    toastr?.warning?.(
      'Поля заполняются только в форме создания персонажа: откройте «Создать персонажа» и повторите. ' +
        'В режиме редактирования SillyTavern сохранила бы черновик поверх открытой карточки.',
      'Импорт черновика',
    );
    return;
  }
  let applied = 0;
  for (const [key, elementId] of Object.entries(MD_IMPORT_FIELD_MAP)) {
    const value = String(fields[key] ?? '').trim();
    if (!value) {
      continue;
    }
    const el = document.getElementById(elementId);
    if (!el) {
      continue;
    }
    el.value = value;
    el.dispatchEvent(new Event('input', { bubbles: true }));
    applied += 1;
  }
  if (applied > 0) {
    toastr?.success?.(`Заполнено полей: ${applied}`, 'Импорт черновика');
    if (!String(fields.name ?? '').trim()) {
      toastr?.warning?.('Имя не заполнено — впишите его вручную', 'Импорт черновика');
    }
    document.getElementById(MD_IMPORT_MODAL_ID)?.remove();
  } else {
    toastr?.warning?.(
      'Не нашёл полей формы на странице — откройте «Создать персонажа»',
      'Импорт черновика',
    );
  }
}

// ── "What's new" ──────────────────────────────────────────────────────────
//
// Modal with the update journal. Shown once after an update.
// Also available from the wand menu.

const CHANGELOG_MODAL_ID = 'cloufam-changelog-modal';

/** Check whether the player has seen the current version. If not — show the modal. */
async function checkChangelog() {
  const gateway = window.__LK_GATEWAY;
  if (!gateway) {
    return;
  }
  try {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 5000);
    const resp = await fetch(`${gateway}/api/cloufam/changelog`, { signal: controller.signal });
    clearTimeout(timer);
    if (!resp.ok) return;
    const data = await resp.json();
    const latest = data.entries?.[0]?.version;
    // Not strict equality: after a rollback, seen is newer than the last entry, and the modal
    // "you have already seen everything" would pop up on every load.
    if (!latest || (data.seen_version && compareVersions(latest, data.seen_version) <= 0)) {
      return; // Already seen
    }
    // Show with a delay — ST must have time to load.
    setTimeout(() => showChangelogModal(data), 3000);
  } catch (_) {
    // Silent: the journal is not critical.
  }
}

/** Open the "What's new" modal manually (from the wand menu). */
async function openChangelogModal() {
  if (document.getElementById(CHANGELOG_MODAL_ID)) {
    return;
  }
  const gateway = window.__LK_GATEWAY;
  if (!gateway) return;
  try {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 5000);
    const resp = await fetch(`${gateway}/api/cloufam/changelog`, { signal: controller.signal });
    clearTimeout(timer);
    if (!resp.ok) {
      const message = `Не удалось загрузить «Что нового» (HTTP ${resp.status})`;
      if (typeof toastr !== 'undefined' && toastr?.error) toastr.error(message, 'Cloufam');
      else alert(message);
      return;
    }
    showChangelogModal(await resp.json(), { manual: true });
  } catch (error) {
    const message = `Не удалось загрузить «Что нового»: ${error}`;
    if (typeof toastr !== 'undefined' && toastr?.error) toastr.error(message, 'Cloufam');
    else alert(message);
  }
}

function showChangelogModal(data, { manual = false } = {}) {
  if (document.getElementById(CHANGELOG_MODAL_ID)) {
    return;
  }
  const modal = document.createElement('div');
  modal.id = CHANGELOG_MODAL_ID;
  modal.innerHTML = changelogMarkup(data, manual);
  document.body.appendChild(modal);

  modal.addEventListener('click', async (event) => {
    if (event.target.dataset?.lkClose) {
      modal.remove();
      // Mark the version as read.
      const gateway = window.__LK_GATEWAY;
      const latest = data.entries?.[0]?.version;
      if (gateway && latest) {
        try {
          await fetch(`${gateway}/api/cloufam/changelog/seen`, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ version: latest }),
          });
        } catch (_) {}
      }
    }
  });
}

/** Numeric comparison of "x.y.z" versions: 1.2.9 < 1.2.52 (string comparison does not). */
function compareVersions(a, b) {
  const pa = String(a).split('.').map(Number);
  const pb = String(b).split('.').map(Number);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const na = pa[i] || 0;
    const nb = pb[i] || 0;
    if (na !== nb) return na - nb;
  }
  return 0;
}

/** Filter entries: only those newer than seen_version. */
function filterChangelogEntries(entries, seenVersion) {
  if (!seenVersion) {
    // First run (or the gateway did not read the settings) — only the last version,
    // not the whole history (decision 21.09).
    return entries.slice(0, 1);
  }
  return entries.filter((e) => compareVersions(e.version, seenVersion) > 0);
}

function changelogMarkup(data, manual = false) {
  const allEntries = data.entries || [];
  const missed = filterChangelogEntries(allEntries, data.seen_version);
  // The popup journal itself shows only what was missed (decision 21.09), while
  // when opened from the menu with nothing missed it would show emptiness — there, the whole history.
  const entries = manual && missed.length === 0 ? allEntries : missed;
  if (entries.length === 0) {
    return `
        <div class="lk-tagger-backdrop" data-lk-close="1"></div>
        <div class="lk-tagger-dialog" style="max-width: 560px;">
            <div class="lk-tagger-header">
                <span>🎉 Что нового</span>
                <div class="lk-tagger-close" data-lk-close="1" title="Закрыть">✕</div>
            </div>
            <div class="lk-changelog-content"><div class="lk-changelog-item">Вы уже видели все обновления.</div></div>
            <div class="lk-tagger-footer">
                <div class="lk-tagger-button" data-lk-close="1">Отлично!</div>
            </div>
        </div>`;
  }
  const sections = entries
    .map((entry) => {
      const gameplay = (entry.gameplay || [])
        .map((g) => `<div class="lk-changelog-item">${escapeHtml(g)}</div>`)
        .join('');
      const fixes = (entry.fixes || [])
        .map((f) => `<div class="lk-changelog-item lk-changelog-fix">${escapeHtml(f)}</div>`)
        .join('');
      return `
            <div class="lk-changelog-version">
                <div class="lk-changelog-version-header">
                    <span class="lk-changelog-version-tag">v${escapeHtml(entry.version)}</span>
                    <span class="lk-changelog-date">${escapeHtml(entry.date)}</span>
                </div>
                ${gameplay}
                ${fixes}
            </div>`;
    })
    .join('');

  return `
        <div class="lk-tagger-backdrop" data-lk-close="1"></div>
        <div class="lk-tagger-dialog" style="max-width: 560px; max-height: 75vh; overflow-y: auto;">
            <div class="lk-tagger-header">
                <span>🎉 Что нового</span>
                <div class="lk-tagger-close" data-lk-close="1" title="Закрыть">✕</div>
            </div>
            <div class="lk-changelog-content">${sections}</div>
            <div class="lk-tagger-footer">
                <div class="lk-tagger-button" data-lk-close="1">Отлично!</div>
            </div>
        </div>`;
}

// ── RPG panel ─────────────────────────────────────────────────────────────
//
// Modal with tabs: World, Stats, Skills, Quests, Inventory, Combat.
// Data comes through the gateway API.

const RPG_MODAL_ID = 'cloufam-rpg-modal';

/** Open the RPG panel. */
async function openRpgPanel() {
  if (document.getElementById(RPG_MODAL_ID)) {
    return;
  }
  const context = getContext();
  const character = allCharacters()[context.characterId];
  const avatar = character?.avatar ?? '';
  if (!avatar) {
    toastr.warning('Нет активного персонажа', 'Cloufam');
    return;
  }
  const modal = document.createElement('div');
  modal.id = RPG_MODAL_ID;
  modal.dataset.lkAvatar = avatar;
  modal.innerHTML = rpgModalMarkup();
  document.body.appendChild(modal);

  modal.addEventListener('click', (event) => {
    if (event.target.dataset?.lkClose) {
      modal.remove();
      return;
    }
    const actionEl = event.target.closest?.('[data-lk-action]');
    if (actionEl) {
      runRpgAction(modal, actionEl);
      return;
    }
    // Tab switching
    const tab = event.target.dataset?.lkTab;
    if (tab) {
      modal.querySelectorAll('[data-lk-tab]').forEach((el) => {
        el.classList.toggle('active', el.dataset.lkTab === tab);
      });
      modal.querySelectorAll('.lk-rpg-tab-panel').forEach((el) => {
        el.style.display = el.dataset.lkPanel === tab ? 'block' : 'none';
      });
      if (tab === 'combat') {
        loadRpgCombat(modal);
      }
    }
  });
  modal.addEventListener('keydown', (event) => {
    if (event.key !== 'Enter' || event.isComposing || event.target.dataset?.lkField !== 'action') {
      return;
    }
    event.preventDefault();
    const button = modal.querySelector('[data-lk-action="combat-act"]');
    if (button) {
      runRpgAction(modal, button);
    }
  });

  await loadRpgData(avatar, modal);
}

function rpgModalMarkup() {
  return `
        <div class="lk-tagger-backdrop" data-lk-close="1"></div>
        <div class="lk-tagger-dialog" style="max-width: 650px; max-height: 80vh; overflow-y: auto;">
            <div class="lk-tagger-header">
                <span>⚔ RPG-панель</span>
                <div class="lk-tagger-close" data-lk-close="1" title="Закрыть">✕</div>
            </div>
            <div class="lk-rpg-tabs">
                <div class="lk-rpg-tab active" data-lk-tab="world">🌍 Мир</div>
                <div class="lk-rpg-tab" data-lk-tab="stats">📊 Характеристики</div>
                <div class="lk-rpg-tab" data-lk-tab="skills">🎯 Навыки</div>
                <div class="lk-rpg-tab" data-lk-tab="quests">📜 Квесты</div>
                <div class="lk-rpg-tab" data-lk-tab="inventory">🎒 Инвентарь</div>
                <div class="lk-rpg-tab" data-lk-tab="combat">⚔ Бой</div>
            </div>
            <div class="lk-rpg-tab-panel" data-lk-panel="world"><div class="lk-rpg-loading">Загрузка…</div></div>
            <div class="lk-rpg-tab-panel" data-lk-panel="stats" style="display:none"><div class="lk-rpg-loading">Загрузка…</div></div>
            <div class="lk-rpg-tab-panel" data-lk-panel="skills" style="display:none"><div class="lk-rpg-loading">Загрузка…</div></div>
            <div class="lk-rpg-tab-panel" data-lk-panel="quests" style="display:none"><div class="lk-rpg-loading">Загрузка…</div></div>
            <div class="lk-rpg-tab-panel" data-lk-panel="inventory" style="display:none"><div class="lk-rpg-loading">Загрузка…</div></div>
            <div class="lk-rpg-tab-panel" data-lk-panel="combat" style="display:none"><div class="lk-rpg-loading">Нет активного боя</div></div>
            <div class="lk-tagger-footer">
                <div class="lk-tagger-button secondary" data-lk-close="1">Закрыть</div>
            </div>
        </div>`;
}

async function loadRpgData(avatar, modal) {
  const gateway = window.__LK_GATEWAY;
  if (!gateway) {
    for (const panel of ['world', 'stats', 'skills', 'quests', 'inventory']) {
      const el = modal.querySelector(`.lk-rpg-tab-panel[data-lk-panel="${panel}"]`);
      if (el) el.innerHTML = '<div class="lk-rpg-error">Шлюз недоступен</div>';
    }
    return;
  }
  const results = await Promise.allSettled([
    gatewayFetch(`${gateway}/api/cloufam/rpg/world`),
    gatewayFetch(`${gateway}/api/cloufam/rpg/stats?avatar=${encodeURIComponent(avatar)}`),
    gatewayFetch(`${gateway}/api/cloufam/rpg/skills?avatar=${encodeURIComponent(avatar)}`),
    gatewayFetch(`${gateway}/api/cloufam/rpg/quests`),
    // avatar is needed for the price: the showcase counts friendship the same way as the deduction in /rpg/trade
    gatewayFetch(`${gateway}/api/cloufam/rpg/inventory?avatar=${encodeURIComponent(avatar)}`),
  ]);
  // None of the routes responded — this is not an "empty world": we show the reason, not
  // five panels full of zeros.
  if (results.every((r) => r.status === 'rejected')) {
    for (const panel of ['world', 'stats', 'skills', 'quests', 'inventory']) {
      const el = modal.querySelector(`.lk-rpg-tab-panel[data-lk-panel="${panel}"]`);
      if (el)
        el.innerHTML = '<div class="lk-rpg-error">Шлюз не ответил — откройте панель снова</div>';
    }
    return;
  }
  const [worldR, statsR, skillsR, questsR, invR] = results;
  const safeJson = async (r) => {
    try {
      return r.status === 'fulfilled' && r.value.ok ? await r.value.json() : {};
    } catch (_) {
      return {};
    }
  };

  renderRpgWorld(modal, await safeJson(worldR));
  renderRpgStats(modal, await safeJson(statsR));
  renderRpgSkills(modal, await safeJson(skillsR));
  renderRpgQuests(modal, await safeJson(questsR));
  renderRpgInventory(modal, await safeJson(invR));
}

function renderRpgWorld(modal, data) {
  const panel = modal.querySelector('[data-lk-panel="world"]');
  if (data.started === false) {
    panel.innerHTML = `
        <div class="lk-rpg-section">
            <div class="lk-rpg-desc">RPG-мир не начат. Пока его нет, в промпт не попадают ни локация, ни характеристики с навыками, ни бой.</div>
            <div class="lk-rpg-actions">
                <div class="lk-tagger-button" data-lk-action="world-start">Начать RPG-мир</div>
            </div>
        </div>`;
    return;
  }
  const loc = data.location;
  if (!loc) {
    panel.innerHTML = '<div class="lk-rpg-empty">Нет данных о мире</div>';
    return;
  }
  const exits = (data.exits || [])
    .map(
      (e) =>
        `<span class="lk-rpg-tag lk-rpg-tag-link" data-lk-action="world-move" data-lk-location="${escapeHtml(e.id)}" title="Перейти">${escapeHtml(e.name)}</span>`,
    )
    .join(' ');
  const tags = (loc.tags || [])
    .map((t) => `<span class="lk-rpg-tag">${escapeHtml(t)}</span>`)
    .join(' ');
  const metaHtml = loc.atmosphere
    ? `<div class="lk-rpg-meta">Атмосфера: ${escapeHtml(loc.atmosphere)}</div>`
    : '';
  panel.innerHTML = `
        <div class="lk-rpg-section">
            <div class="lk-rpg-label">Локация</div>
            <div class="lk-rpg-value">${escapeHtml(loc.name)}</div>
            <div class="lk-rpg-desc">${escapeHtml(loc.description)}</div>
            ${metaHtml}
            ${tags ? `<div class="lk-rpg-tags">${tags}</div>` : ''}
        </div>
        ${exits ? `<div class="lk-rpg-section"><div class="lk-rpg-label">Перейти</div><div class="lk-rpg-tags">${exits}</div></div>` : ''}
        ${loc.has_shop ? `<div class="lk-rpg-section"><div class="lk-rpg-label">Магазин</div><div class="lk-rpg-value">${escapeHtml(loc.shop_type || 'есть')}</div></div>` : ''}
        <div class="lk-rpg-actions">
            <div class="lk-tagger-button secondary" data-lk-action="npc-generate">✨ Описать NPC в локации</div>
            <div class="lk-tagger-button secondary" data-lk-action="world-reset">Сбросить мир</div>
        </div>`;
}

function renderRpgStats(modal, data) {
  const panel = modal.querySelector('[data-lk-panel="stats"]');
  const stats = data.stats || [];
  if (stats.length === 0) {
    panel.innerHTML = '<div class="lk-rpg-empty">Нет характеристик</div>';
    return;
  }
  panel.innerHTML =
    '<div class="lk-rpg-section">' +
    stats
      .map((s) => {
        const level = Number(s.level) || 0;
        const exp = Number(s.experience) || 0;
        const toNext = Number(s.exp_to_next) || 1;
        return `
        <div class="lk-rpg-stat-row">
            <span class="lk-rpg-stat-name">${escapeHtml(s.name)}</span>
            <div class="lk-rpg-bar-container">
                <div class="lk-rpg-bar" style="width: ${(level / 10) * 100}%"></div>
            </div>
            <span class="lk-rpg-stat-level">${level}/10</span>
            <span class="lk-rpg-stat-exp">${exp}/${toNext} XP</span>
        </div>`;
      })
      .join('') +
    '</div>';
}

function renderRpgSkills(modal, data) {
  const panel = modal.querySelector('[data-lk-panel="skills"]');
  const skills = data.skills || [];
  if (skills.length === 0) {
    panel.innerHTML = '<div class="lk-rpg-empty">Нет навыков</div>';
    return;
  }
  panel.innerHTML =
    '<div class="lk-rpg-section">' +
    skills
      .map((s) => {
        const level = Number(s.level) || 0;
        const exp = Number(s.experience) || 0;
        const toNext = Number(s.exp_to_next) || 1;
        return `
        <div class="lk-rpg-stat-row">
            <span class="lk-rpg-stat-name">${escapeHtml(s.name)}${s.is_custom ? ' ✦' : ''}</span>
            <div class="lk-rpg-bar-container">
                <div class="lk-rpg-bar lk-rpg-bar-skill" style="width: ${(level / 10) * 100}%"></div>
            </div>
            <span class="lk-rpg-stat-level">${level}/10</span>
            <span class="lk-rpg-stat-exp">${exp}/${toNext} XP</span>
            <span class="lk-rpg-stat-bind" title="Характеристика">(${escapeHtml(s.stat_binding)})</span>
        </div>`;
      })
      .join('') +
    '</div>';
}

function renderRpgQuests(modal, data) {
  const panel = modal.querySelector('[data-lk-panel="quests"]');
  const quests = data.quests || [];
  // Quests are created only by the model: without the button the tab would forever remain
  // empty, although generation and rewards exist in the backend.
  const generate =
    '<div class="lk-rpg-actions"><div class="lk-tagger-button secondary" data-lk-action="quest-generate">✨ Сгенерировать квест</div></div>';
  if (quests.length === 0) {
    panel.innerHTML = '<div class="lk-rpg-empty">Нет активных квестов</div>' + generate;
    return;
  }
  panel.innerHTML =
    quests
      .map((q) => {
        const stage = (q.current_stage ?? 0) + 1;
        const total = q.total_stages ?? 1;
        const pct = Math.round((stage / total) * 100);
        return `
        <div class="lk-rpg-quest">
            <div class="lk-rpg-quest-title">📜 ${escapeHtml(q.title)}</div>
            <div class="lk-rpg-quest-desc">${escapeHtml(q.description)}</div>
            <div class="lk-rpg-quest-stage">Стадия ${stage}/${total}: ${escapeHtml(q.objective || '—')}</div>
            <div class="lk-rpg-quest-progress">
                <div class="lk-rpg-quest-progress-track"><div class="lk-rpg-quest-progress-fill" style="width: ${pct}%"></div></div>
            </div>
            ${q.giver ? `<div class="lk-rpg-meta">Квестодатель: ${escapeHtml(q.giver)}</div>` : ''}
        </div>`;
      })
      .join('') + generate;
}

function renderRpgInventory(modal, data) {
  const panel = modal.querySelector('[data-lk-panel="inventory"]');
  const items = data.items || [];
  const shops = Array.isArray(data.shops) ? data.shops : [];
  const rarityIcons = { common: '⚪', uncommon: '🟢', rare: '🔵', epic: '🟣', legendary: '🟡' };
  const validRarities = new Set(Object.keys(rarityIcons));
  let html = '';
  if (items.length === 0) {
    html += '<div class="lk-rpg-empty">Инвентарь пуст</div>';
  } else {
    html +=
      '<div class="lk-rpg-section">' +
      items
        .map((i) => {
          const qty = (i.quantity ?? 0) < 0 ? '∞' : (i.quantity ?? 0);
          const rarity = validRarities.has(i.rarity) ? i.rarity : 'common';
          const icon = rarityIcons[rarity];
          return `<div class="lk-rpg-item">
            <span class="lk-rpg-item-name">${escapeHtml(i.name)} ×${qty}</span>
            <span class="lk-rpg-item-rarity lk-rpg-rarity-${rarity}">${icon} ${escapeHtml(i.rarity_name || rarity)}</span>
            <span class="lk-rpg-item-desc">${escapeHtml(i.description)}</span>
        </div>`;
        })
        .join('') +
      '</div>';
  }
  if (shops.length === 0) {
    html += '<div class="lk-rpg-empty">Рядом нет торговцев с прилавком</div>';
  } else {
    html += shops
      .map(
        (shop) => `
        <div class="lk-rpg-section">
            <div class="lk-rpg-quest-title">🏪 ${escapeHtml(shop.name)}</div>
            ${(shop.items || [])
              .map(
                (item) => `
              <div class="lk-rpg-item">
                <span class="lk-rpg-item-name">${escapeHtml(item.name)} — ${Number(item.price) || 0} 💰</span>
                <span class="lk-rpg-item-desc">${escapeHtml(item.description)}</span>
                <div class="lk-rpg-actions">
                  <div class="lk-tagger-button secondary" data-lk-action="trade-buy"
                       data-lk-npc="${escapeHtml(shop.npc)}" data-lk-item="${escapeHtml(item.id)}">Купить</div>
                </div>
              </div>`,
              )
              .join('')}
        </div>`,
      )
      .join('');
  }
  panel.innerHTML = html;
}

async function loadRpgCombat(modal) {
  const panel = modal.querySelector('[data-lk-panel="combat"]');
  const gateway = window.__LK_GATEWAY;
  if (!gateway) {
    panel.innerHTML = '<div class="lk-rpg-error">Шлюз недоступен</div>';
    return;
  }
  try {
    const resp = await gatewayFetch(`${gateway}/api/cloufam/rpg/combat`);
    if (!resp.ok) {
      panel.innerHTML = `<div class="lk-rpg-error">Ошибка ${resp.status}</div>`;
      return;
    }
    const data = await resp.json();
    panel.innerHTML = data.active ? rpgCombatMarkup(data) : rpgCombatStartMarkup(data);
  } catch (error) {
    panel.innerHTML = `<div class="lk-rpg-error">Ошибка: ${escapeHtml(String(error))}</div>`;
  }
}

const RPG_MODE_LABELS = {
  fatal: 'Насмерть',
  non_lethal: 'Бытовой (без увечий)',
  negotiation: 'Переговоры',
};
const RPG_RESULT_LABELS = {
  enemy_defeated: 'Победа: противник повержен',
  player_defeated: 'Поражение: вы повержены',
  non_lethal_win: 'Победа: противник отступил',
  non_lethal_loss: 'Поражение: вы отступили',
};
const RPG_ENEMY_STAT_LABELS = { Endurance: 'Выносливость', Strength: 'Сила', Morale: 'Мораль' };
const RPG_LOG_LINES = 12;

function rpgCombatStartMarkup(data) {
  if (data.world_started === false) {
    return '<div class="lk-rpg-empty">Боя нет. Бой начинается в начатом RPG-мире — вкладка «Мир».</div>';
  }
  const enemies = data.enemies || [];
  if (enemies.length === 0) {
    return '<div class="lk-rpg-empty">Нет активного боя</div>';
  }
  const enemyOptions = enemies
    .map((e) => {
      const traits = `HP ${Number(e.hp) || 0}, атака ${Number(e.attack) || 0}, защита ${Number(e.defense) || 0}`;
      const notes = `${e.is_boss ? ', босс' : ''}${e.negotiable ? '' : ', не договаривается'}`;
      return `<option value="${escapeHtml(e.id)}">${escapeHtml(e.name)} — ${traits}${notes}</option>`;
    })
    .join('');
  const modeOptions = Object.entries(RPG_MODE_LABELS)
    .map(([value, label]) => `<option value="${value}">${escapeHtml(label)}</option>`)
    .join('');
  return `
        <div class="lk-rpg-section">
            <div class="lk-rpg-label">Новый бой</div>
            <div class="lk-rpg-form">
                <label class="lk-rpg-field">Противник<select class="lk-rpg-input" data-lk-field="enemy">${enemyOptions}</select></label>
                <label class="lk-rpg-field">Режим<select class="lk-rpg-input" data-lk-field="mode">${modeOptions}</select></label>
            </div>
            <div class="lk-rpg-desc">Действия пишутся свободным текстом. Урон и исход считает игра, после каждого вашего хода противник отвечает.</div>
            <div class="lk-rpg-actions">
                <div class="lk-tagger-button" data-lk-action="combat-start">Начать бой</div>
            </div>
        </div>`;
}

function rpgCombatMarkup(data) {
  const e = data.enemy || {};
  const enemyHp = Number(e.hp) || 0;
  const maxHp = Number(e.max_hp) || 1;
  const hpPct = Math.max(0, Math.min(100, (enemyHp / maxHp) * 100));
  const php = Number(data.player_hp) || 0;
  const maxPhp = Math.max(1, Number(data.player_max_hp) || 100);
  const playerHpPct = Math.max(0, Math.min(100, (php / maxPhp) * 100));
  // In domestic combat the enemy's HP does not drop until surrender — progress is visible by stats.
  const enemyStats =
    data.mode === 'non_lethal' && data.enemy_stats
      ? `<div class="lk-rpg-meta">${Object.entries(data.enemy_stats)
          .map(
            ([name, value]) =>
              `${escapeHtml(RPG_ENEMY_STAT_LABELS[name] || name)}: ${Math.max(0, Number(value) || 0)}`,
          )
          .join(' · ')}</div>`
      : '';
  const log = (data.log || [])
    .slice(-RPG_LOG_LINES)
    .map((line) => `<div class="lk-rpg-log-line">${escapeHtml(line)}</div>`)
    .join('');
  const controls = data.over
    ? `<div class="lk-rpg-result">${escapeHtml(RPG_RESULT_LABELS[data.result] || 'Бой окончен')}</div>
            <div class="lk-rpg-actions">
                <div class="lk-tagger-button" data-lk-action="combat-end">Завершить</div>
            </div>`
    : `<input class="lk-rpg-input" type="text" maxlength="400" data-lk-field="action" placeholder="Что вы делаете? Например: бью мечом по ногам">
            <div class="lk-rpg-meta" data-lk-status></div>
            <div class="lk-rpg-actions">
                <div class="lk-tagger-button" data-lk-action="combat-act">Действовать</div>
                <div class="lk-tagger-button secondary" data-lk-action="combat-end">Прервать бой</div>
            </div>`;
  return `
        <div class="lk-rpg-section">
            <div class="lk-rpg-label">Бой с ${escapeHtml(e.name || '???')}</div>
            <div class="lk-rpg-meta">Режим: ${escapeHtml(RPG_MODE_LABELS[data.mode] || data.mode || '?')} · Раунд: ${Number(data.round) || 0}</div>
            <div class="lk-rpg-stat-row">
                <span class="lk-rpg-stat-name">HP противника</span>
                <div class="lk-rpg-bar-container"><div class="lk-rpg-bar lk-rpg-bar-enemy" style="width: ${hpPct}%"></div></div>
                <span class="lk-rpg-stat-exp">${enemyHp}/${maxHp}</span>
            </div>
            <div class="lk-rpg-stat-row">
                <span class="lk-rpg-stat-name">Ваше HP</span>
                <div class="lk-rpg-bar-container"><div class="lk-rpg-bar lk-rpg-bar-player" style="width: ${playerHpPct}%"></div></div>
                <span class="lk-rpg-stat-exp">${php}/${maxPhp}</span>
            </div>
            ${enemyStats}
        </div>
        <div class="lk-rpg-section">${controls}</div>
        ${log ? `<div class="lk-rpg-section"><div class="lk-rpg-label">Журнал боя</div><div class="lk-rpg-log">${log}</div></div>` : ''}`;
}

/** RPG panel buttons: world (start, reset, move) and combat (start, turn, finish). */
async function runRpgAction(modal, el) {
  // A combat turn waits for the model up to two minutes: a repeated click must not send a second turn.
  if (modal.dataset.lkBusy) {
    return;
  }
  const gateway = window.__LK_GATEWAY;
  if (!gateway) {
    toastr.error('Шлюз недоступен', 'RPG');
    return;
  }
  const field = (name) => modal.querySelector(`[data-lk-field="${name}"]`);
  let path;
  let body = null;
  switch (el.dataset.lkAction) {
    case 'world-start':
      path = 'world/start';
      break;
    case 'world-reset':
      if (
        !confirm(
          'Сбросить RPG-мир? Локация, магазины и текущий бой будут удалены. Характеристики и навыки персонажей останутся.',
        )
      ) {
        return;
      }
      path = 'world/reset';
      break;
    case 'world-move':
      path = 'world/move';
      body = { location: el.dataset.lkLocation };
      break;
    case 'combat-start':
      path = 'combat/start';
      body = { enemy: field('enemy')?.value, mode: field('mode')?.value };
      break;
    case 'combat-act': {
      const input = field('action');
      const text = String(input?.value ?? '').trim();
      if (!text) {
        input?.focus();
        return;
      }
      path = 'combat/action';
      body = { action: text };
      break;
    }
    case 'combat-end':
      // The turn field exists only for an ongoing fight: interrupting it — with confirmation.
      if (field('action') && !confirm('Прервать бой? Он будет удалён без итога.')) {
        return;
      }
      path = 'combat/end';
      break;
    case 'quest-generate':
      path = 'quests/generate';
      break;
    case 'npc-generate':
      path = 'npcs/generate';
      break;
    case 'trade-buy':
      path = 'trade';
      body = {
        npc: el.dataset.lkNpc,
        item: el.dataset.lkItem,
        quantity: 1,
      };
      if (modal.dataset.lkAvatar) body.avatar = modal.dataset.lkAvatar;
      break;
    default:
      return;
  }

  modal.dataset.lkBusy = '1';
  el.classList.add('disabled');
  const status = modal.querySelector('[data-lk-status]');
  const statusText =
    path === 'combat/action'
      ? 'Модель описывает ход…'
      : path === 'quests/generate'
        ? 'Модель придумывает квест…'
        : path === 'npcs/generate'
          ? 'Модель описывает NPC…'
          : path === 'trade'
            ? 'Торгуемся…'
            : '';
  if (status && statusText) {
    status.textContent = statusText;
  }
  try {
    // Quest/NPC generation and the merchant's line wait for the model: the timeout here is
    // longer than for usual state routes.
    const resp = await gatewayFetch(
      `${gateway}/api/cloufam/rpg/${path}`,
      {
        method: 'POST',
        headers: body ? { 'Content-Type': 'application/json' } : {},
        body: body ? JSON.stringify(body) : undefined,
      },
      120000,
    );
    if (!resp.ok) {
      toastr.error((await resp.text()) || `Ошибка ${resp.status}`, 'RPG');
      return;
    }
    let payload = null;
    try {
      payload = await resp.json();
    } catch {
      payload = null;
    }
    if (path === 'trade' && payload) {
      const price = Number(payload.paid) || 0;
      const line = payload.line ? ` ${payload.line}` : '';
      toastr.success(`Куплено за ${price} 💰.${line}`, 'RPG');
    } else if (path === 'quests/generate' && payload?.quest?.title) {
      toastr.success(`Новый квест: ${payload.quest.title}`, 'RPG');
    } else if (path === 'npcs/generate' && payload?.npc?.name) {
      toastr.success(`В локации появился ${payload.npc.name}`, 'RPG');
    }
  } catch (error) {
    toastr.error(String(error?.message ?? error), 'RPG');
    return;
  } finally {
    delete modal.dataset.lkBusy;
    el.classList.remove('disabled');
    if (status) {
      status.textContent = '';
    }
  }
  if (
    path.startsWith('world/') ||
    path === 'quests/generate' ||
    path === 'npcs/generate' ||
    path === 'trade'
  ) {
    await loadRpgData(modal.dataset.lkAvatar, modal);
  }
  await loadRpgCombat(modal);
}

export function init() {
  if (initialized) {
    return;
  }
  initialized = true;

  // The ST extensions panel may not be in the DOM yet — we finish the injection with a timer
  // (the same trick as the Cloufam settings button in index.html).
  setInterval(() => {
    injectWandItem();
    injectSettingsBlock();
  }, 2000);
  injectWandItem();
  injectSettingsBlock();
  loadConfig();

  // Character traits: the app asks to apply them right after saving
  // (`window.__lkApplyTraits`), while the timer catches the case where settings
  // were changed with the chat closed.
  window.__lkApplyTraits = applyTraits;
  window.__lkTranslateWorldInfo = translateImportedWorldInfo;
  setTimeout(
    () => restoreTranslatedWorldContent().then(() => migrateActiveWorldInfoTranslation()),
    6000,
  );
  eventSource.on(event_types.WORLDINFO_SETTINGS_UPDATED, () => migrateActiveWorldInfoTranslation());
  // The timer starts before `selected_world_info` comes up — without a silent retry on
  // chat change, the installation stayed with keys that do not match the lines.
  eventSource.on(event_types.CHAT_CHANGED, () =>
    setTimeout(() => {
      restoreTranslatedWorldContent({ silent: true }).then(() =>
        migrateActiveWorldInfoTranslation({ silent: true }),
      );
    }, 5000),
  );
  // Tag settings (including "Assign tags automatically") are changed in
  // a separate Cloufam window, not on this page: `config` in this module
  // is loaded once at startup and without this hook would not learn about a change
  // until the ST page is reloaded manually — and browser hotkeys are disabled
  // in WebView2, so there is nothing to reload with. `push_settings_to_client` fires this
  // hook on every app settings save.
  window.__lkReloadTaggerConfig = () => {
    loadConfig();
  };
  setInterval(applyTraits, 30000);
  checkChangelog();
  setInterval(reportActiveCharacter, 30000);
  reportActiveCharacter();
  eventSource.on(event_types.CHAT_CHANGED, () => {
    reportActiveCharacter();
    setTimeout(applyTraits, 1000);
  });
  // The regex script only colors ~text~ — telling the model to write thoughts in tildes needs to be
  // said. This is an extension prompt, not the main prompt: for cards with their own
  // system_prompt ST replaces the main one entirely. It is set only when the script is
  // enabled (otherwise bare tildes would remain in the chat) and does not go into quiet calls
  // like Summarize, otherwise tildes would get into the summary.
  eventSource.on(event_types.GENERATION_STARTED, (type) => {
    const script = (extensionSettings.regex ?? []).find((s) => s.id === THOUGHT_REGEX_ID);
    const on = Boolean(script && !script.disabled) && type !== 'quiet';
    getContext().setExtensionPrompt(
      THOUGHT_PROMPT_KEY,
      on ? THOUGHT_PROMPT : '',
      extension_prompt_types.IN_PROMPT,
      0,
    );
  });
  // The generation type, chat id and repetition penalty otherwise do not reach the gateway: for
  // a custom source ST forwards a fixed set of fields plus YAML from
  // custom_include_body. Without the type the gateway cannot tell Summarize and other quiet requests
  // from a line, without the chat id — the first turn of a new chat from a regeneration in the past;
  // `repetition_penalty` is sent only to providers with this field in the API, so the
  // "Repetition penalty" slider silently did nothing, and Ollama ran with the gateway's
  // weaker default (the model looped). We do not touch the user's body in flow style
  // or as a list (including the block "- key:"): appended lines
  // would break its parsing, and ST would discard the whole body.
  eventSource.on(event_types.CHAT_COMPLETION_SETTINGS_READY, (data) => {
    if (data?.chat_completion_source !== 'custom') return;
    const body = String(data.custom_include_body ?? '').trim();
    const firstLine = body.split('\n').find((l) => l.trim() && !l.trim().startsWith('#')) ?? '';
    if (/^\s*[-[{]/.test(firstLine)) return;
    const lines = [`lk_generation_type: ${JSON.stringify(String(data.type || 'normal'))}`];
    const chatId = getContext().getCurrentChatId?.();
    if (chatId) lines.push(`lk_chat_id: ${JSON.stringify(String(chatId))}`);
    // For the custom provider, generate_data has no penalty field — the slider lives in
    // oai_settings; earlier Number(undefined) gave NaN, and the penalty did not get through.
    const repPen = Number(
      data.repetition_penalty ?? getContext().chatCompletionSettings?.repetition_penalty_openai,
    );
    // 1.0 in ST means "no penalty": we do not pass such a value, the gateway keeps
    // its proven default, otherwise a disabled slider would kill the protection against
    // looping.
    if (Number.isFinite(repPen) && repPen > 1) lines.push(`repetition_penalty: ${repPen}`);
    data.custom_include_body = [body, ...lines].filter(Boolean).join('\n');
  });
  // ST sends CHARACTER_PAGE_LOADED on every printCharacters: search, filter and
  // every `/tag-add`. Without coalescing, a burst of events drove parallel card
  // writes and, for each, the config (with the Ollama model list) and a re-render.
  let traitsTimer = null;
  let autoTagTimer = null;
  let translationTimer = null;
  eventSource.on(event_types.CHARACTER_PAGE_LOADED, () => {
    clearTimeout(traitsTimer);
    traitsTimer = setTimeout(() => {
      reportActiveCharacter();
      applyTraits();
    }, 1500);
  });

  eventSource.on(event_types.CHARACTER_PAGE_LOADED, () => {
    clearTimeout(translationTimer);
    translationTimer = setTimeout(() => translateNewCharacters(), 3500);
  });

  eventSource.on(event_types.CHARACTER_PAGE_LOADED, () => {
    clearTimeout(autoTagTimer);
    autoTagTimer = setTimeout(() => autoTagNewCharacters(), 3000);
  });

  SlashCommandParser.addCommandObject(
    SlashCommand.fromProps({
      name: 'автотеги',
      aliases: ['autotags', 'теги'],
      callback: async () => {
        if (!config.enabled && Object.keys(config).length === 0) {
          await loadConfig();
        }
        // ST discards the command's return value from the input field — we show the result
        // ourselves and keep the line for pipes.
        const summary = await tagAllUntagged();
        toastr.info(summary, 'Авто-теги');
        return summary;
      },
      helpString: 'Проставить теги всем персонажам без тегов (локальная модель Cloufam).',
      returns: ARGUMENT_TYPE.STRING,
    }),
  );
}

