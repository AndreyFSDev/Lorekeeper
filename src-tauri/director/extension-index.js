// Cloufam director mode: narrator remarks that actually
// happen in the scene.
//
// A remark is added as a `narrator` message — SillyTavern sends such
// messages to the model with the `system` role, so what is written becomes scene canon,
// and the characters must react to it. The model that expands the note lives in the
// app gateway: the extension talks to the `cloufam` server plugin.

import {
  extension_prompt_roles,
  extension_prompt_types,
  isGenerating,
  sendMessageAsUser,
} from '../../../../script.js';
import { event_types, eventSource } from '../../../events.js';
import { SlashCommand } from '../../../slash-commands/SlashCommand.js';
import {
  ARGUMENT_TYPE,
  SlashCommandArgument,
} from '../../../slash-commands/SlashCommandArgument.js';
import { sendNarratorMessage } from '../../../slash-commands.js';
import { getContext } from '../../../st-context.js';

const MODULE_NAME = 'cloufam_director';
const BUTTON_ID = 'cloufam-director-nav-btn';
const HIDDEN_KEY = 'LOREKEEPER_DIRECTOR';
const INPUT_CLASS = 'lk-director-input';

let initialized = false;
let busy = false;
let chatEpoch = 0;

function currentChatKey() {
  const context = getContext();
  return JSON.stringify([
    context.groupId ?? null,
    context.characterId ?? null,
    context.getCurrentChatId?.() ?? context.chatId ?? null,
  ]);
}

function isCurrentScene(key, epoch) {
  return epoch === chatEpoch && key === currentChatKey();
}

function assertCurrentScene(key, epoch) {
  if (!isCurrentScene(key, epoch)) {
    throw new Error('Чат изменился — подготовленная сцена не добавлена в другой разговор.');
  }
}

function settings() {
  const { extensionSettings, saveSettingsDebounced } = getContext();
  if (!extensionSettings[MODULE_NAME]) {
    extensionSettings[MODULE_NAME] = {
      enabled: false,
      expand: true,
      signature: 'Режиссёр',
      after: 'characters',
    };
  }
  const store = extensionSettings[MODULE_NAME];
  // 1.2.52 changed only the delivery default: where `user` was already saved, remarks
  // still went out as a player line. Migrate once, then respect the modal choice.
  if (!store.delivery_migrated) {
    if (store.delivery === 'user') {
      store.delivery = 'system';
    }
    store.delivery_migrated = true;
    saveSettingsDebounced();
  }
  return {
    get enabled() {
      return !!store.enabled;
    },
    get expand() {
      return store.expand !== false;
    },
    get signature() {
      return String(store.signature ?? 'Режиссёр');
    },
    get after() {
      return String(store.after ?? 'characters');
    },
    get delivery() {
      return String(store.delivery ?? 'system');
    },
    set(patch) {
      Object.assign(store, patch);
      saveSettingsDebounced();
    },
  };
}

function pluginUrl(path) {
  return `/api/plugins/cloufam/${path}`;
}

/** A remark through the gateway: the model expands the note while preserving all facts. */
async function expandScene(note, { hiddenText, after }) {
  const { getRequestHeaders } = getContext();
  const response = await fetch(pluginUrl('director'), {
    method: 'POST',
    headers: { ...getRequestHeaders(), 'Content-Type': 'application/json' },
    body: JSON.stringify({
      text: note,
      expand: settings().expand,
      hidden_text: hiddenText,
      after,
    }),
  });
  const text = await response.text();
  if (!response.ok) {
    throw new Error(text || `HTTP ${response.status}`);
  }
  return JSON.parse(text);
}

/**
 * A remark enters the chat as the narrator.
 *
 * `system` (default) — a narrator remark: the model sees it with the system role,
 * understands this is a scene (not a player line) and plays out the characters' reaction.
 * `user` — an event from the player's side: the model sees it as user input,
 * may confuse it with a line and start generating as the player.
 */
async function appendNarration(narration, mode = 'user') {
  if (mode === 'system') {
    try {
      await sendNarratorMessage({ name: settings().signature }, narration);
      return;
    } catch (err) {
      console.warn('sendNarratorMessage fallback:', err);
    }
  }
  await sendMessageAsUser(narration, null, null, false, settings().signature);
}

function setHiddenPrompt(value) {
  getContext().setExtensionPrompt(
    HIDDEN_KEY,
    value ?? '',
    extension_prompt_types.IN_CHAT,
    0,
    false,
    extension_prompt_roles.SYSTEM,
  );
}

/**
 * Main action: director note → chat remark → scene reaction.
 * @returns {Promise<string>} the remark text
 */
async function directScene(note, options = {}) {
  // The epoch catches A → B → A too while the model is thinking.
  const chatKey = currentChatKey();
  const epoch = chatEpoch;
  const text = String(note ?? '').trim();
  if (!text) {
    throw new Error('Напишите, что должно произойти.');
  }
  // Common check for the input field, the modal and the slash command: in the middle of a stream
  // the remark would land inside the reply and start a second generate.
  if (isGenerating()) {
    throw new Error('Дождитесь конца ответа — сцена сейчас сбила бы генерацию.');
  }
  const current = settings();
  let payload;
  try {
    payload = await expandScene(text, {
      hiddenText: String(options.hiddenNote ?? '').trim(),
      after: current.after,
    });
  } catch (err) {
    console.warn('Director expand fallback to raw text:', err);
    const secretLost = String(options.hiddenNote ?? '').trim()
      ? ' Тайное указание не применено.'
      : '';
    toastr?.warning?.(
      `Ремарка ушла как написана — шлюз не ответил: ${err?.message ?? err}.${secretLost}`,
      'Режиссёр',
    );
    payload = { narration: text };
  }
  assertCurrentScene(chatKey, epoch);
  // Service reply of the shop command (the gateway marks it model: 'director'):
  // show the result as a toast — in the chat it would be a scene and would trigger a reply.
  if (payload.model === 'director') {
    toastr?.info?.(payload.narration, 'Режиссёр');
    // The command does not start generation — a secret instruction has nowhere to go; silently losing it is not allowed
    if (String(options.hiddenNote ?? '').trim()) {
      toastr?.warning?.(
        'Тайное указание к команде не применяется — отправьте его с обычной ремаркой.',
        'Режиссёр',
      );
    }
    return payload.narration;
  }
  const narrationText = payload.narration || text;
  // Cloufam fix (A2-06): re-check after the multi-second
  // expandScene — another generate could have started while the scene was assembled, and the remark
  // would land inside someone else's stream (TOCTOU: the isGenerating check was only
  // at the start of directScene).
  if (isGenerating()) {
    throw new Error('Дождитесь конца ответа — во время сборки сцены началась новая генерация.');
  }
  await appendNarration(narrationText, current.delivery);

  // Everything after inserting the remark must not return control to the caller's
  // catch: it would put the text back into the input field, and the next send would insert
  // the same remark into the chat a second time.
  try {
    assertCurrentScene(chatKey, epoch);
    const willReply = current.after === 'characters' || current.after === 'narrator';
    if (willReply) {
      // The secret and the "who replies after the remark" hint travel as one block:
      // setExtensionPrompt with the same key would overwrite the previous call.
      setHiddenPrompt([payload.hidden, payload.hint].filter(Boolean).join('\n'));
    } else {
      // "After the remark — do nothing": there will be no generation, and we would have
      // nothing to clear the directive with — it would stay in the prompt until the chat changes and
      // apply to the next normal line.
      setHiddenPrompt('');
    }

    if (willReply) {
      // Cloufam fix (A2-06): third check — right before
      // starting the reply: appendNarration takes time too, and a second
      // generate on top of a started one breaks the stream.
      if (isGenerating()) {
        toastr?.info?.('Ответ уже генерируется — следующий не запускаем.', 'Режиссёр');
      } else {
        await triggerReply();
      }
    }
  } catch (err) {
    console.warn('Director: ремарка в чате, ответ не запущен:', err);
    // The reply did not start — the one-shot directive must not wait for the next
    // normal line and apply to it.
    if (isCurrentScene(chatKey, epoch)) setHiddenPrompt('');
    toastr?.warning?.(
      'Ремарка ушла в чат, но ответ не запустился — жмите регенерацию.',
      'Режиссёр',
    );
  }
  return narrationText;
}

/// Start of a normal generation: the extensions context exposes it as `generate`.
async function triggerReply() {
  const ctx = getContext();
  if (typeof ctx.generate === 'function') {
    return ctx.generate('normal');
  }
  if (typeof ctx.executeSlashCommandsWithOptions === 'function') {
    return ctx.executeSlashCommandsWithOptions('/trigger');
  }
}

/**
 * One-shot hints are cleared right after generation.
 *
 * Earlier the guard read `getContext().extension_prompts`, while the context exposes it under
 * the name `extensionPrompts` (`st-context.js`) — the check was false, and the block
 * "secret + who replies" stayed injected until the page reloaded:
 * the character received a stale directive in every subsequent reply, including
 * in another chat. We clear unconditionally by our own key.
 */
function bindCleanup() {
  const { event_types: types, eventSource: source } = getContext();
  source.on(types.GENERATION_ENDED, () => {
    // In a group, generation ends per participant, while the remark must
    // reach everyone — there the round is closed by GROUP_WRAPPER_FINISHED. The ST context
    // exposes the selected group as `groupId`, not `selected_group`.
    if (getContext().groupId) {
      return;
    }
    setHiddenPrompt('');
  });
  source.on(types.GROUP_WRAPPER_FINISHED, () => setHiddenPrompt(''));
  // Chat change — guarantees the instruction is not carried any further.
  source.on(types.CHAT_CHANGED, () => {
    chatEpoch += 1;
    setHiddenPrompt('');
  });
}

// ── UI ────────────────────────────────────────────────────────────────

function status(text, isError = false) {
  const line = document.getElementById('lk-director-status');
  if (!line) {
    return;
  }
  line.textContent = text;
  line.classList.toggle('lk-error', isError);
}

function renderModal() {
  return `
    <div id="lk-director-modal" class="lk-director-modal">
      <div class="lk-director-card">
        <div class="lk-director-head">
          <b>🎬 Режим режиссёра</b>
          <span class="lk-close" title="Закрыть">✕</span>
        </div>
        <p class="lk-hint">
          Напишите, что происходит в сцене. Ремарка уходит в чат от лица рассказчика и становится частью сцены:
          персонажи реагируют на неё как на произошедшее событие.
        </p>
        <label class="lk-switch">
          <input type="checkbox" id="lk-director-enabled" />
          <span>Писать в поле ввода как рассказчик</span>
        </label>
        <textarea id="lk-director-note" rows="4"
          placeholder="Например: в дверь стучат, на пороге незнакомец в плаще и требует Каталог."></textarea>
        <label class="lk-line">
          <input type="checkbox" id="lk-director-expand" />
          <span>Развернуть моделью (сохраняя все факты)</span>
        </label>
        <div class="lk-grid">
          <label class="lk-line">Подпись ремарки
            <input type="text" id="lk-director-signature" />
          </label>
          <label class="lk-line">После ремарки
            <select id="lk-director-after">
              <option value="characters">отвечают персонажи</option>
              <option value="narrator">продолжает рассказчик</option>
              <option value="none">ничего не делать</option>
            </select>
          </label>
        </div>
        <label class="lk-line">Подача
          <select id="lk-director-delivery">
            <option value="system">ремарка рассказчика (персонажи реагируют)</option>
            <option value="user">событие от лица игрока</option>
          </select>
        </label>
        <details class="lk-secret">
          <summary>Тайное указание модели (в чат не попадёт)</summary>
          <textarea id="lk-director-hidden" rows="2"
            placeholder="Например: Фубуки влюблена, но скрывает это и злится на себя."></textarea>
        </details>
        <div class="lk-actions">
          <button id="lk-director-run" class="menu_button">Сцена!</button>
          <span id="lk-director-status" class="lk-status"></span>
        </div>
      </div>
    </div>`;
}

function syncControls() {
  const store = settings();
  const enabled = document.getElementById('lk-director-enabled');
  const expand = document.getElementById('lk-director-expand');
  const signature = document.getElementById('lk-director-signature');
  const after = document.getElementById('lk-director-after');
  const delivery = document.getElementById('lk-director-delivery');
  if (enabled) enabled.checked = store.enabled;
  if (expand) expand.checked = store.expand;
  if (signature) signature.value = store.signature;
  if (after) after.value = store.after;
  if (delivery) delivery.value = store.delivery;
  updateInputHint();
}

function toggleDirectorMode() {
  const store = settings();
  const nextState = !store.enabled;
  store.set({ enabled: nextState });
  updateInputHint();
  const modalCheckbox = document.getElementById('lk-director-enabled');
  if (modalCheckbox) modalCheckbox.checked = nextState;
  if (nextState) {
    toastr?.info?.('Режим режиссёра включён: ремарка в сцену (или Ctrl+Enter)', 'Режиссёр');
  } else {
    toastr?.info?.('Режим режиссёра выключен: сообщения от своего лица', 'Режиссёр');
  }
}

// Writing the same value to an attribute is still a mutation: the Moonlit theme watches
// attributes in `#form_sheld` and responds to each one with a synchronous layout.
function setIfChanged(element, property, value) {
  if (element[property] !== value) {
    element[property] = value;
  }
}

function updateInputHint() {
  const area = document.getElementById('send_textarea');
  if (!area) {
    return;
  }
  const on = settings().enabled;
  area.classList.toggle(INPUT_CLASS, on);
  // We do not overwrite the ST hint: its text depends on the connection state and changes
  // on connect/disconnect, so we restore the one saved when entering the mode.
  if (on) {
    if (area.dataset.lkPlaceholder === undefined) {
      area.dataset.lkPlaceholder = area.placeholder ?? '';
    }
    setIfChanged(
      area,
      'placeholder',
      '🎬 Режиссёр: что происходит в сцене? (Ctrl+Enter в любой момент)',
    );
  } else if (area.dataset.lkPlaceholder !== undefined) {
    area.placeholder = area.dataset.lkPlaceholder;
    delete area.dataset.lkPlaceholder;
  }
  const button = document.getElementById(BUTTON_ID);
  if (button) {
    button.classList.toggle('lk-active', on);
    setIfChanged(
      button,
      'title',
      on
        ? 'Режим режиссёра: ВКЛ (клик — писать от себя, Shift+клик — настройки)'
        : 'Режим режиссёра: ВЫКЛ (клик — писать от режиссёра, Shift+клик — настройки, Ctrl+Enter — ремарка)',
    );
  }

  // Active mode indicator above the input row
  let badge = document.getElementById('lk-director-badge');
  if (on) {
    if (!badge) {
      badge = document.createElement('div');
      badge.id = 'lk-director-badge';
      badge.className = 'lk-director-badge';
      // With "Send on Enter" off, Enter inserts a newline, and the remark is
      // sent with Ctrl+Enter or the button.
      const hint =
        getContext().shouldSendOnEnter?.() === false
          ? 'Ctrl+Enter или кнопка отправки — ремарка сцены'
          : 'Enter — ремарка сцены';
      badge.innerHTML = `
                <span class="lk-director-badge-dot"></span>
                <span class="lk-director-badge-text">🎬 Режим режиссёра активен (${hint})</span>
                <span class="lk-director-badge-close" title="Выключить (сообщения от своего лица)">✕</span>
            `;
      badge.querySelector('.lk-director-badge-close')?.addEventListener('click', (e) => {
        e.stopPropagation();
        toggleDirectorMode();
      });
      const form = document.getElementById('send_form') || area.parentElement;
      if (form) {
        form.insertBefore(badge, form.firstChild);
      }
    }
  } else if (badge) {
    badge.remove();
  }
}

function closeModal() {
  document.getElementById('lk-director-modal')?.remove();
}

function openModal() {
  if (document.getElementById('lk-director-modal')) {
    return;
  }
  document.body.insertAdjacentHTML('beforeend', renderModal());
  syncControls();
  document.querySelector('#lk-director-modal .lk-close')?.addEventListener('click', closeModal);
  document.getElementById('lk-director-modal')?.addEventListener('click', (event) => {
    if (event.target?.id === 'lk-director-modal') {
      closeModal();
    }
  });
  document.getElementById('lk-director-enabled')?.addEventListener('change', (event) => {
    settings().set({ enabled: event.target.checked });
    updateInputHint();
    status(
      event.target.checked
        ? 'Режим включён: текст из поля ввода станет ремаркой.'
        : 'Режим выключен.',
    );
  });
  document.getElementById('lk-director-expand')?.addEventListener('change', (event) => {
    settings().set({ expand: event.target.checked });
  });
  document.getElementById('lk-director-signature')?.addEventListener('input', (event) => {
    settings().set({ signature: event.target.value });
  });
  document.getElementById('lk-director-after')?.addEventListener('change', (event) => {
    settings().set({ after: event.target.value });
  });
  document.getElementById('lk-director-delivery')?.addEventListener('change', (event) => {
    settings().set({ delivery: event.target.value });
  });
  document.getElementById('lk-director-run')?.addEventListener('click', runFromModal);
  document.getElementById('lk-director-note')?.focus();
}

async function runFromModal() {
  if (busy) {
    toastr?.info?.('Сцена уже собирается', 'Режиссёр');
    return;
  }
  const note = document.getElementById('lk-director-note')?.value ?? '';
  const hiddenNote = document.getElementById('lk-director-hidden')?.value ?? '';
  busy = true;
  status('Собираю сцену…');
  try {
    const narration = await directScene(note, { hiddenNote });
    const area = document.getElementById('lk-director-note');
    if (area) {
      area.value = '';
    }
    const secret = document.getElementById('lk-director-hidden');
    if (secret) {
      secret.value = '';
    }
    status(`Сцена задана: ${narration.slice(0, 90)}${narration.length > 90 ? '…' : ''}`);
  } catch (error) {
    status(String(error?.message ?? error), true);
  } finally {
    busy = false;
  }
}

function injectButton() {
  // The button lives in the send form next to the input field: `#top-bar` is an empty
  // strip under the ST icons (`#top-settings-holder` covers it and eats
  // clicks), while the form itself is a free slot that ST keeps for its buttons.
  const form = document.getElementById('leftSendForm');
  if (!form) {
    return;
  }
  let button = document.getElementById(BUTTON_ID);
  if (button && button.parentElement === form) {
    updateInputHint();
    return;
  }
  if (!button) {
    button = document.createElement('div');
    button.id = BUTTON_ID;
    button.className = 'fa-solid fa-clapperboard interactable lk-nav-icon';
    button.title = 'Режим режиссёра (клик: вкл/выкл | Shift+клик: настройки | Ctrl+Enter: ремарка)';
    button.addEventListener('click', (event) => {
      if (event.shiftKey) {
        openModal();
      } else {
        toggleDirectorMode();
      }
    });
    button.addEventListener('contextmenu', (event) => {
      event.preventDefault();
      openModal();
    });
  }
  // After the ST "burger"; the extensions wand stays last (`order: 4`).
  const options = document.getElementById('options_button');
  form.insertBefore(button, options ? options.nextSibling : form.firstChild);
  updateInputHint();
}

// ── Input mode: text goes out as a remark ───────────────────────────────────

function isDirectorInputActive() {
  return settings().enabled;
}

// `//` is a mode marker, not part of the remark, so director commands start with it
// and it is stripped before sending. Except for the commands parsed by
// `src-tauri/src/director.rs` (`shop`, `shop_remove`, `skill`, `location`): it
// expects the prefix as a whole, and without it `// shop potions` went into the scene as a normal
// note — the shop did not appear. The list keeps the same set as the command
// parsing in director.rs (Cloufam fix: `skill`/`location` were added after it).
const PREFIXED_COMMAND = /^\/\/\s*(shop(_remove)?|skill|location)\b/i;

function stripModePrefix(text) {
  return PREFIXED_COMMAND.test(text) ? text : text.replace(/^(\/\/|\/d\s+|🎬\s*)/, '').trim();
}

// ST commands (`/roll 2d6`, `/search …`, `/autotags`) are executed by ST itself — as a remark
// they went to the model as a scene note. `//` and `/d ` are director prefixes.
function isSlashCommand(text) {
  return text.startsWith('/') && !text.startsWith('//') && !text.startsWith('/d ');
}

async function submitDirectorInput(explicitText = null) {
  // Return the failed text only into the same chat and an empty field.
  const chatKey = currentChatKey();
  const epoch = chatEpoch;
  // Check on entry, not only in `isDirectorInputActive`: Ctrl+Enter and
  // the prefixed branch called the function directly, and fast repeated presses
  // started N parallel scenes — each with its own `generate`, while the stop button
  // then cancelled only the last one.
  if (busy) {
    toastr?.info?.('Сцена уже собирается', 'Режиссёр');
    return;
  }
  // In the middle of a reply the remark would land in the chat inside the stream and start a second
  // generate. ST itself does not send Enter at that time — the text stays in the field.
  if (isGenerating()) {
    toastr?.info?.('Дождитесь конца ответа — ремарка останется в поле ввода.', 'Режиссёр');
    return;
  }
  const area = document.getElementById('send_textarea');
  const raw = explicitText !== null ? explicitText : String(area?.value ?? '').trim();
  const text = stripModePrefix(raw);
  if (!text) {
    return;
  }
  busy = true;
  try {
    if (area) {
      area.value = '';
      area.dispatchEvent(new Event('input', { bubbles: true }));
    }
    await directScene(text, {});
  } catch (error) {
    if (area && isCurrentScene(chatKey, epoch) && !area.value) {
      area.value = raw;
    }
    toastr?.error?.(String(error?.message ?? error), 'Режиссёр');
  } finally {
    busy = false;
  }
}

function bindInputInterception() {
  // Intercept the send in the capture phase: ST handlers must not get a chance
  // to turn the remark into a normal user message.
  document.addEventListener(
    'click',
    (event) => {
      const target = event.target;
      if (!(target instanceof Element) || !target.closest('#send_but')) {
        return;
      }
      const area = document.getElementById('send_textarea');
      const text = String(area?.value ?? '').trim();
      if (isSlashCommand(text)) {
        return;
      }
      const hasDirectorPrefix =
        text.startsWith('//') || text.startsWith('/d ') || text.startsWith('🎬');

      if (hasDirectorPrefix) {
        event.preventDefault();
        event.stopImmediatePropagation();
        submitDirectorInput();
        return;
      }

      if (!isDirectorInputActive()) {
        return;
      }
      event.preventDefault();
      event.stopImmediatePropagation();
      submitDirectorInput();
    },
    true,
  );

  document.addEventListener(
    'keydown',
    (event) => {
      const target = event.target;
      if (!(target instanceof Element) || target.id !== 'send_textarea') {
        return;
      }
      // Enter confirming an IME candidate is not a send (ST checks
      // `isComposing` itself, while our interception runs before its handlers).
      if (event.isComposing) {
        return;
      }

      if (event.key !== 'Enter' || event.shiftKey) {
        return;
      }

      const area = target;
      const text = String(area.value ?? '').trim();
      if (isSlashCommand(text)) {
        return;
      }

      // Ctrl+Enter or Alt+Enter with text — a remark even with the mode off.
      // On an empty field these are ST hotkeys: regenerate/accept edit and continue.
      if (event.ctrlKey || event.altKey) {
        if (!text) {
          return;
        }
        event.preventDefault();
        event.stopImmediatePropagation();
        submitDirectorInput();
        return;
      }

      // Where Enter is a newline in ST ("Send on Enter" setting, phone),
      // we do not send the remark with it either.
      if (!getContext().shouldSendOnEnter()) {
        return;
      }

      const hasDirectorPrefix =
        text.startsWith('//') || text.startsWith('/d ') || text.startsWith('🎬');

      // Prefix // or /d: sends as the director in normal mode
      if (hasDirectorPrefix) {
        event.preventDefault();
        event.stopImmediatePropagation();
        submitDirectorInput();
        return;
      }

      // Plain Enter with director mode active
      if (!isDirectorInputActive()) {
        return;
      }
      event.preventDefault();
      event.stopImmediatePropagation();
      submitDirectorInput();
    },
    true,
  );
}

// ── Dice: /roll NdM[+K] ─────────────────────────────────────────────────────
//
// A light narrative feature, not server adjudication: the result does not affect
// anything mechanically (unlike RPG combat, where the server computes damage); this is
// just a roll into the chat as a player remark. No cache needed — the command sends a ready
// message once and is not mixed into the prompt on every generation, so there is nothing to
// recompute when regenerating the model reply after a roll.

const DICE_NOTATION = /^(\d*)d(\d+)([+-]\d+)?$/i;

/** "2d6+3" → {count, faces, modifier}; null — the notation is not recognized. */
function parseDiceNotation(input) {
  const match = String(input ?? '')
    .trim()
    .match(DICE_NOTATION);
  if (!match) {
    return null;
  }
  const count = match[1] ? parseInt(match[1], 10) : 1;
  const faces = parseInt(match[2], 10);
  const modifier = match[3] ? parseInt(match[3], 10) : 0;
  if (count < 1 || count > 100 || faces < 2 || faces > 1000) {
    return null;
  }
  return { count, faces, modifier };
}

function rollDice({ count, faces, modifier }) {
  const rolls = Array.from({ length: count }, () => 1 + Math.floor(Math.random() * faces));
  const total = rolls.reduce((sum, r) => sum + r, 0) + modifier;
  return { rolls, total };
}

function formatRoll(notation, { rolls, total }) {
  const rollsText = rolls.length > 1 ? ` [${rolls.join(', ')}]` : '';
  return `🎲 ${notation}${rollsText} → ${total}`;
}

function registerRollCommand() {
  const { SlashCommandParser } = getContext();
  SlashCommandParser.addCommandObject(
    SlashCommand.fromProps({
      name: 'roll',
      aliases: ['бросок', 'кости'],
      callback: async (_args, value) => {
        const notation = String(value ?? '').trim() || '1d20';
        const parsed = parseDiceNotation(notation);
        if (!parsed) {
          const message = `Не понял нотацию «${notation}» — ожидается вида 1d20 или 2d6+3.`;
          toastr?.error?.(message, 'Кости');
          return message;
        }
        const text = formatRoll(notation, rollDice(parsed));
        await appendNarration(text, 'user');
        return text;
      },
      unnamedArgumentList: [
        SlashCommandArgument.fromProps({
          description: 'нотация броска, например 1d20+3',
          typeList: [ARGUMENT_TYPE.STRING],
        }),
      ],
      helpString: 'Бросить кости (например /roll 2d6+3) — результат уходит в чат ремаркой игрока.',
    }),
  );
}

// ── Slash command ────────────────────────────────────────────────────────────

function registerCommand() {
  const { SlashCommandParser } = getContext();
  SlashCommandParser.addCommandObject(
    SlashCommand.fromProps({
      name: 'режиссёр',
      aliases: ['director', 'сцена', 'scene'],
      callback: async (_args, value) => {
        const note = String(value ?? '').trim();
        if (!note) {
          openModal();
          return 'Открыл панель режиссёра.';
        }
        const narration = await directScene(note, {});
        return narration;
      },
      unnamedArgumentList: [
        SlashCommandArgument.fromProps({
          description: 'что происходит в сцене',
          typeList: [ARGUMENT_TYPE.STRING],
        }),
      ],
      helpString:
        'Задать сцену от лица рассказчика: написанное становится каноном и персонажи на это реагируют.',
    }),
  );
}

export async function init() {
  if (initialized) {
    return;
  }
  initialized = true;
  try {
    registerCommand();
  } catch (error) {
    console.error('[cloufam] не удалось зарегистрировать команду режиссёра:', error);
  }
  try {
    registerRollCommand();
  } catch (error) {
    console.error('[cloufam] не удалось зарегистрировать команду /roll:', error);
  }
  injectButton();
  bindInputInterception();
  bindCleanup();
  eventSource.on(event_types.CHAT_CHANGED, () => {
    injectButton();
    updateInputHint();
  });
  // The button can disappear only when the input row is rebuilt — that is what we watch.
  // The observer on the whole `body` was left over from the button in `#top-bar` and
  // fired on every stream frame: ST rewrites the message entirely.
  const sendRow = document.getElementById('leftSendForm')?.parentElement;
  if (sendRow) {
    new MutationObserver(() => injectButton()).observe(sendRow, { childList: true, subtree: true });
  }
}

