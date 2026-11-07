<script lang="ts">
  import { onMount, tick } from 'svelte';
  import { open as openDialog } from '@tauri-apps/plugin-dialog';
  import { api } from './lib/api';
  import { i18n, t } from './lib/i18n.svelte';
  import Icon from './lib/Icon.svelte';
  import { renderMarkup } from './lib/markup';
  // Custom default background (generated locally, ZavyChroma XL): without a
  // wallpaper the glass transparency is not visible.
  import defaultBackground from './assets/chat-bg.webp';
  import type { ChatMessage, ChatSummary, EngineCharacter, Lorebook, LoreEntry } from './lib/types';

  let { onExit }: { onExit: () => void } = $props();

  // Chat screen of our own engine (engine/*.rs). All handlers run on the
  // WebView UI thread; the model reply arrives in chunks through a Channel.

  let characters = $state<EngineCharacter[]>([]);
  let avatars = $state<Record<string, string>>({});
  let current = $state<EngineCharacter | null>(null);
  let chats = $state<ChatSummary[]>([]);
  let chatId = $state<string | null>(null);
  let messages = $state<ChatMessage[]>([]);
  let streaming = $state<string | null>(null);
  let draft = $state('');
  let error = $state('');
  let notice = $state('');
  let loreOpen = $state(false);
  let lore = $state<Lorebook | null>(null);
  let loreSaved = $state(false);
  let moreOpen = $state(false);
  let background = $state<string | null>(null);
  let rosterOpen = $state(false);
  let log: HTMLDivElement | undefined = $state();

  const busy = $derived(streaming !== null);
  const canRegenerate = $derived(!busy && messages.length > 1 && messages[messages.length - 1].role === 'assistant');

  function fail(e: unknown) {
    error = String(e);
  }

  async function scrollDown() {
    await tick();
    log?.scrollTo({ top: log.scrollHeight });
  }

  async function loadCharacters(selectId?: string) {
    characters = await api.engine.listCharacters();
    for (const c of characters) {
      if (c.has_avatar && !avatars[c.id]) {
        const url = await api.engine.avatar(c.id).catch(() => null);
        if (url) avatars[c.id] = url;
      }
    }
    const next = characters.find((c) => c.id === selectId) ?? (current ? characters.find((c) => c.id === current?.id) : null) ?? characters[0];
    if (next && next.id !== current?.id) await selectCharacter(next);
    if (!next) {
      current = null;
      rosterOpen = true;
    }
  }

  async function selectCharacter(character: EngineCharacter) {
    if (busy) return;
    current = character;
    error = '';
    loreOpen = false;
    chats = await api.engine.listChats(character.id);
    if (chats.length === 0) await newChat();
    else await openChat(chats[0].id);
  }

  async function openChat(id: string) {
    if (!current || busy) return;
    const loaded = await api.engine.loadChat(current.id, id);
    chatId = id;
    messages = loaded.messages;
    notice = loaded.skipped_lines > 0 ? t('chat.skipped', { count: loaded.skipped_lines }) : '';
    await scrollDown();
  }

  async function newChat() {
    if (!current || busy) return;
    const created = await api.engine.createChat(current.id);
    chatId = created.id;
    messages = created.messages;
    notice = '';
    chats = await api.engine.listChats(current.id);
    await scrollDown();
  }

  async function deleteChat() {
    if (!current || !chatId || busy || !confirm(t('chat.deleteChatConfirm'))) return;
    await api.engine.deleteChat(current.id, chatId);
    chats = await api.engine.listChats(current.id);
    if (chats.length === 0) await newChat();
    else await openChat(chats[0].id);
  }

  async function importCharacter() {
    const picked = await openDialog({ multiple: false, title: t('chat.importTitle'), filters: [{ name: 'PNG', extensions: ['png'] }] });
    if (typeof picked !== 'string') return;
    const character = await api.engine.importCharacter(picked);
    await loadCharacters(character.id);
  }

  async function deleteCharacter() {
    if (!current || busy || !confirm(t('chat.deleteCharacterConfirm', { name: current.name }))) return;
    await api.engine.deleteCharacter(current.id);
    delete avatars[current.id];
    current = null;
    messages = [];
    chatId = null;
    await loadCharacters();
  }

  /** text = null — regenerate the last reply. */
  async function generate(text: string | null) {
    if (!current || !chatId || busy) return;
    const characterId = current.id;
    const id = chatId;
    error = '';
    if (text === null) {
      messages = messages.slice(0, -1);
    } else {
      messages = [...messages, { role: 'user', content: text, at_ms: Date.now() }];
      draft = '';
    }
    streaming = '';
    await scrollDown();
    try {
      await api.engine.send(characterId, id, text, (event) => {
        if (event.kind === 'delta') {
          streaming = (streaming ?? '') + event.text;
          void scrollDown();
        } else if (event.message && chatId === id) {
          messages = [...messages, event.message];
        }
      });
    } catch (e) {
      fail(e);
      // The history on disk is the source of truth: we show exactly that.
      if (chatId === id) messages = (await api.engine.loadChat(characterId, id).catch(() => ({ messages }))).messages;
    } finally {
      streaming = null;
      await scrollDown();
    }
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
      e.preventDefault();
      const text = draft.trim();
      if (text) void generate(text).catch(fail);
    }
  }

  async function toggleLore() {
    if (!current) return;
    loreOpen = !loreOpen;
    loreSaved = false;
    if (loreOpen) lore = await api.engine.getLorebook(current.id);
  }

  function addEntry() {
    if (!lore) return;
    const entry: LoreEntry = { title: '', keys: [], content: '', enabled: true, constant: false, priority: 100, case_sensitive: false };
    lore.entries = [...lore.entries, entry];
    loreSaved = false;
  }

  async function importLore() {
    if (!current) return;
    const picked = await openDialog({ multiple: false, title: t('chat.lore.importTitle'), filters: [{ name: 'JSON', extensions: ['json'] }] });
    if (typeof picked !== 'string') return;
    lore = await api.engine.importLorebook(current.id, picked);
    loreSaved = true;
  }

  async function saveLore() {
    if (!current || !lore) return;
    await api.engine.saveLorebook(current.id, $state.snapshot(lore));
    loreSaved = true;
  }

  async function pickBackground() {
    const picked = await openDialog({
      multiple: false,
      title: t('chat.backgroundTitle'),
      filters: [{ name: 'Image', extensions: ['png', 'jpg', 'jpeg', 'webp'] }],
    });
    if (typeof picked !== 'string') return;
    background = await api.engine.setBackground(picked);
  }

  async function clearBackground() {
    background = await api.engine.setBackground(null);
  }

  const locale = () => (i18n.locale === 'ru' ? 'ru-RU' : 'en-US');
  const stamp = (ms: number) => new Date(ms).toLocaleString(locale(), { dateStyle: 'medium', timeStyle: 'short' });

  const chatLabel = (chat: ChatSummary) =>
    t('chat.chatFrom', { date: new Date(chat.updated_ms).toLocaleString(locale()) });

  onMount(() => {
    loadCharacters().catch(fail);
    api.engine
      .background()
      .then((url) => (background = url))
      .catch(fail);
  });
</script>

<svelte:window
  onclick={(e) => {
    if (!(e.target instanceof Element) || !e.target.closest('.menu-anchor')) moreOpen = false;
  }}
/>

<div class="chat-shell" style:--chat-bg={`url("${background ?? defaultBackground}")`}>
  <nav class="topbar" data-tauri-drag-region>
    {@render tool('home', t('chat.home'), onExit)}
    {@render tool('users', t('chat.characters'), () => (rosterOpen = !rosterOpen), rosterOpen)}
    {@render tool('plus', t('chat.newChat'), () => newChat().catch(fail), false, busy || !current)}
    {@render tool('book', t('chat.lorebook'), () => toggleLore().catch(fail), loreOpen, !current)}
    {@render tool('image', t('chat.background'), () => pickBackground().catch(fail))}
    <div class="menu-anchor">
      <button
        class="tool"
        title={t('chat.more')}
        aria-label={t('chat.more')}
        aria-haspopup="menu"
        aria-expanded={moreOpen}
        disabled={busy}
        onclick={() => (moreOpen = !moreOpen)}
      >
        <Icon name={'dots'} size={20} />
      </button>
      {#if moreOpen}
        <div class="menu more-menu" role="menu">
          {#if background}
            <button class="menu-item" role="menuitem" onclick={() => ((moreOpen = false), clearBackground().catch(fail))}>
              <span class="menu-icon"><Icon name={'image'} /></span>{t('chat.backgroundClear')}
            </button>
          {/if}
          {#if current}
            <button class="menu-item danger" role="menuitem" onclick={() => ((moreOpen = false), deleteChat().catch(fail))}>
              <span class="menu-icon"><Icon name={'trash'} /></span>{t('chat.deleteChat')}
            </button>
            <button class="menu-item danger" role="menuitem" onclick={() => ((moreOpen = false), deleteCharacter().catch(fail))}>
              <span class="menu-icon"></span>{t('chat.deleteCharacter')}
            </button>
          {/if}
        </div>
      {/if}
    </div>
  </nav>

  <section class="conversation">
    <div class="column">
      {#if !current}
        <p class="hint center">{characters.length ? t('chat.pickCharacter') : t('chat.noCharacters')}</p>
      {:else}
        <header class="conv-head glass">
          {#if avatars[current.id]}<img class="head-avatar" src={avatars[current.id]} alt="" />{/if}
          <h2>{current.name}</h2>
          <select value={chatId} disabled={busy} onchange={(e) => openChat(e.currentTarget.value).catch(fail)}>
            {#each chats as chat (chat.id)}
              <option value={chat.id}>{chatLabel(chat)}</option>
            {/each}
          </select>
        </header>

        {#if loreOpen && lore}
          <div class="lore glass">
            <div class="lore-bar">
              <label>{t('chat.lore.scanDepth')} <input type="number" min="1" max="100" bind:value={lore.scan_depth} /></label>
              <label>{t('chat.lore.budget')} <input type="number" min="64" max="16384" step="64" bind:value={lore.token_budget} /></label>
              <span class="spacer"></span>
              <button class="glass-btn" onclick={addEntry}>{t('chat.lore.add')}</button>
              <button class="glass-btn" onclick={() => importLore().catch(fail)}>{t('chat.lore.import')}</button>
              <button class="primary" onclick={() => saveLore().catch(fail)}>{loreSaved ? t('chat.lore.saved') : t('chat.lore.save')}</button>
            </div>
            {#if lore.entries.length === 0}
              <p class="hint">{t('chat.lore.empty')}</p>
            {/if}
            {#each lore.entries as entry, i (i)}
              <div class="lore-entry">
                <div class="lore-row">
                  <input placeholder={t('chat.lore.title')} bind:value={entry.title} oninput={() => (loreSaved = false)} />
                  <input
                    class="keys"
                    placeholder={t('chat.lore.keys')}
                    value={entry.keys.join(', ')}
                    oninput={(e) => {
                      entry.keys = e.currentTarget.value.split(',').map((k) => k.trim()).filter(Boolean);
                      loreSaved = false;
                    }}
                  />
                  <label><input type="checkbox" bind:checked={entry.enabled} onchange={() => (loreSaved = false)} /> {t('chat.lore.enabled')}</label>
                  <label><input type="checkbox" bind:checked={entry.constant} onchange={() => (loreSaved = false)} /> {t('chat.lore.constant')}</label>
                  <label>{t('chat.lore.priority')} <input class="num" type="number" bind:value={entry.priority} oninput={() => (loreSaved = false)} /></label>
                  <button
                    class="ghost icon"
                    title={t('chat.lore.remove')}
                    aria-label={t('chat.lore.remove')}
                    onclick={() => {
                      if (lore) lore.entries = lore.entries.filter((_, j) => j !== i);
                      loreSaved = false;
                    }}><Icon name={'trash'} /></button>
                </div>
                <textarea rows="3" placeholder={t('chat.lore.content')} bind:value={entry.content} oninput={() => (loreSaved = false)}></textarea>
              </div>
            {/each}
          </div>
        {/if}

        <div class="log" bind:this={log}>
          {#each messages as m, i (i)}
            {@render bubble(m.role, m.content, m.at_ms, i)}
          {/each}
          {#if streaming !== null}
            {@render bubble('assistant', streaming, null, messages.length)}
          {/if}
        </div>

        {#if notice}<p class="notice">{notice}</p>{/if}
        {#if error}<p class="error">{error}</p>{/if}

        <div class="composer glass">
          <textarea rows="1" placeholder={t('chat.placeholder')} title={t('chat.keysHint')} bind:value={draft} onkeydown={onKey} disabled={busy}></textarea>
          {#if busy}
            <button class="ghost icon" title={t('chat.stop')} aria-label={t('chat.stop')} onclick={() => api.engine.cancel().catch(fail)}>
              <Icon name={'stop'} />
            </button>
          {:else}
            <button class="ghost icon" disabled={!canRegenerate} title={t('chat.regenerate')} aria-label={t('chat.regenerate')} onclick={() => generate(null).catch(fail)}>
              <Icon name={'retry'} />
            </button>
            <button class="ghost icon" disabled={!draft.trim()} title={t('chat.send')} aria-label={t('chat.send')} onclick={() => generate(draft.trim()).catch(fail)}>
              <Icon name={'send'} />
            </button>
          {/if}
        </div>
      {/if}
    </div>
  </section>

  {#if rosterOpen}
    <aside class="roster glass">
      <div class="roster-head">
        <span>{t('chat.characters')}</span>
        <button class="ghost icon" title={t('chat.import')} aria-label={t('chat.import')} onclick={() => importCharacter().catch(fail)}>
          <Icon name={'plus'} />
        </button>
      </div>
      <ul>
        {#each characters as c (c.id)}
          <li>
            <button
              class="who"
              class:active={current?.id === c.id}
              disabled={busy}
              onclick={() => {
                rosterOpen = false;
                selectCharacter(c).catch(fail);
              }}
            >
              {#if avatars[c.id]}
                <img src={avatars[c.id]} alt="" />
              {:else}
                <span class="avatar-blank">{c.name.slice(0, 1)}</span>
              {/if}
              <span class="who-name">{c.name}</span>
            </button>
          </li>
        {/each}
      </ul>
    </aside>
  {/if}
</div>

{#snippet tool(icon: string, label: string, onclick: () => void, active = false, disabled = false)}
  <button class="tool" class:active title={label} aria-label={label} {disabled} {onclick}>
    <Icon name={icon} size={20} />
  </button>
{/snippet}

{#snippet bubble(role: string, content: string, at: number | null, index: number)}
  <article class="msg {role}">
    <div class="msg-side">
      {#if role === 'assistant' && current && avatars[current.id]}
        <img src={avatars[current.id]} alt="" />
      {:else}
        <span class="avatar-blank">{role === 'user' ? t('chat.you').slice(0, 1) : (current?.name.slice(0, 1) ?? '')}</span>
      {/if}
      <span class="msg-index">#{index}</span>
    </div>
    <div class="msg-body">
      <div class="msg-head">
        <span class="msg-author">{role === 'user' ? t('chat.you') : current?.name}</span>
        {#if at !== null}<time class="msg-time">{stamp(at)}</time>{/if}
      </div>
      <div class="msg-text">{@html renderMarkup(content)}{#if content === ''}<span class="spinner small"></span>{/if}</div>
    </div>
  </article>
{/snippet}

<style>
  /* Look of the owner's SillyTavern: full-screen wallpaper, a tinted column in
     the center, liquid-glass messages. Gradients and text colors come from the
     own Cloufam theme (src-tauri/theme/glass.css, Cloufam.json). */
  .chat-shell {
    --lk-text: rgba(226, 234, 248, 1);
    --lk-italic: rgba(186, 200, 246, 1);
    --lk-dim: rgba(226, 234, 248, 0.62);
    --lk-glass: linear-gradient(135deg, rgba(140, 175, 255, 0.28) 0%, rgba(72, 64, 180, 0.38) 48%, rgba(12, 17, 48, 0.5) 100%);
    --lk-glass-shadow: inset 0 1px 0 rgba(255, 255, 255, 0.24), inset 0 -1px 0 rgba(255, 255, 255, 0.06), 0 18px 44px rgba(3, 6, 22, 0.48);
    position: relative;
    display: grid;
    grid-template-rows: 44px 1fr;
    height: 100%;
    min-height: 0;
    color: var(--lk-text);
    background-color: #070b1c;
    background-image: var(--chat-bg);
    background-position: center;
    background-size: cover;
  }

  .glass {
    background-image: var(--lk-glass);
    backdrop-filter: blur(26px) saturate(185%) brightness(1.06);
    box-shadow: var(--lk-glass-shadow);
    border-radius: 14px;
  }

  /* Icon strip like the ST top menu: centered, full width, wallpaper behind. */
  .topbar {
    position: relative;
    z-index: 6;
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 6px;
    background: rgba(8, 12, 32, 0.55);
    backdrop-filter: blur(12px) saturate(150%);
    box-shadow: 0 1px 0 rgba(255, 255, 255, 0.06);
  }

  .tool {
    display: grid;
    place-items: center;
    width: 40px;
    height: 34px;
    padding: 0;
    border: 0;
    border-radius: 8px;
    background: transparent;
    box-shadow: none;
    color: rgba(226, 234, 248, 0.78);
  }

  .tool:hover:not(:disabled),
  .tool.active {
    background: rgba(140, 175, 255, 0.18);
    color: var(--lk-text);
  }

  .tool:active {
    transform: none;
  }

  .glass-btn {
    background: rgba(255, 255, 255, 0.08);
    border-color: rgba(255, 255, 255, 0.14);
    box-shadow: inset 0 1px 0 rgba(255, 255, 255, 0.12);
    color: var(--lk-text);
  }

  .glass-btn:hover,
  .glass-btn.active {
    background: rgba(255, 255, 255, 0.16);
    border-color: rgba(255, 255, 255, 0.22);
  }

  /* Character drawer, like the character management panel in ST. */
  .roster {
    position: absolute;
    z-index: 7;
    top: 52px;
    right: 12px;
    bottom: 12px;
    display: flex;
    flex-direction: column;
    width: min(320px, calc(100% - 24px));
    padding: 12px 10px;
    overflow-y: auto;
  }

  .roster-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 0 6px 8px;
    color: var(--lk-dim);
  }

  .roster ul {
    margin: 0;
    padding: 0;
    list-style: none;
  }

  .who {
    display: flex;
    align-items: center;
    gap: 10px;
    width: 100%;
    padding: 6px 8px;
    border: 0;
    border-radius: 10px;
    background: transparent;
    box-shadow: none;
    color: var(--lk-text);
    text-align: left;
  }

  .who:hover,
  .who.active {
    background: rgba(140, 175, 255, 0.16);
  }

  .who img,
  .avatar-blank {
    flex: none;
    width: 32px;
    height: 32px;
    border-radius: 50%;
    object-fit: cover;
  }

  .avatar-blank {
    display: grid;
    place-items: center;
    background: rgba(140, 175, 255, 0.2);
    color: var(--lk-text);
  }

  .who-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .conversation {
    display: flex;
    justify-content: center;
    min-width: 0;
    min-height: 0;
  }

  /* Chat column: a tint over the wallpaper, like the theme's blur_tint_color;
     on the sides the wallpaper stays sharp (chat_width: 50 in the ST theme). */
  .column {
    display: flex;
    flex-direction: column;
    gap: 10px;
    width: min(100%, max(620px, 60%));
    min-height: 0;
    padding: 10px 14px 14px;
    background: rgba(12, 18, 46, 0.36);
    backdrop-filter: blur(8px);
  }

  /* backdrop-filter on messages creates its own layers: without z-index the "⋯"
     menu goes under the feed. */
  .conv-head {
    position: relative;
    z-index: 5;
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 8px 10px;
  }

  .head-avatar {
    flex: none;
    width: 34px;
    height: 34px;
    border-radius: 50%;
    object-fit: cover;
  }

  .conv-head h2 {
    flex: 0 0 auto;
    max-width: 30%;
    min-width: 0;
    margin: 0 6px 0 0;
    overflow: hidden;
    font-size: 18px;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .conv-head select {
    flex: 0 1 240px;
    min-width: 0;
    background: rgba(8, 12, 32, 0.45);
    border-color: rgba(255, 255, 255, 0.14);
    color: var(--lk-text);
  }

  .menu-anchor {
    position: relative;
    flex: none;
  }

  .more-menu {
    top: calc(100% + 6px);
    left: 50%;
    transform: translateX(-50%);
    min-width: 220px;
  }

  .menu-item.danger:hover {
    color: var(--err);
  }

  .log {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding-right: 4px;
  }

  .msg {
    display: grid;
    grid-template-columns: 52px 1fr;
    gap: 12px;
    margin-bottom: 10px;
    padding: 12px 16px 14px 10px;
    border-radius: 14px;
    background-image: linear-gradient(135deg, rgba(140, 175, 255, 0.34) 0%, rgba(80, 72, 195, 0.44) 46%, rgba(14, 20, 54, 0.56) 100%);
    backdrop-filter: blur(26px) saturate(185%) brightness(1.06);
    box-shadow: inset 0 1px 0 rgba(255, 255, 255, 0.28), inset 0 -1px 0 rgba(255, 255, 255, 0.08), 0 16px 38px rgba(3, 6, 22, 0.46);
  }

  .msg.user {
    background-image: linear-gradient(135deg, rgba(160, 196, 255, 0.4) 0%, rgba(96, 88, 210, 0.48) 46%, rgba(20, 28, 64, 0.58) 100%);
  }

  .msg-side {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 6px;
  }

  .msg-side img,
  .msg-side .avatar-blank {
    width: 48px;
    height: 48px;
    border: 2px solid rgba(255, 255, 255, 0.22);
    border-radius: 50%;
    object-fit: cover;
  }

  .msg-index {
    color: var(--lk-dim);
    font-size: 12px;
  }

  .msg-body {
    min-width: 0;
  }

  .msg-head {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: 8px;
    margin-bottom: 6px;
  }

  .msg-author {
    font-weight: 600;
  }

  .msg-time {
    color: var(--lk-dim);
    font-size: 11.5px;
  }

  .msg-text {
    line-height: 1.6;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }

  .msg-text :global(em) {
    color: var(--lk-italic);
  }

  .composer {
    display: flex;
    align-items: flex-end;
    gap: 4px;
    padding: 6px 8px 6px 14px;
    border-radius: 18px;
  }

  .composer textarea {
    flex: 1;
    min-height: 36px;
    max-height: 200px;
    padding: 8px 0;
    border: 0;
    background: transparent;
    color: var(--lk-text);
    resize: none;
    field-sizing: content;
  }

  .composer textarea::placeholder {
    color: var(--lk-dim);
  }

  .composer .icon {
    color: var(--lk-text);
  }

  .composer .icon:hover:not(:disabled) {
    background: rgba(255, 255, 255, 0.12);
  }

  .lore {
    max-height: 45%;
    overflow-y: auto;
    padding: 12px;
  }

  .lore input,
  .lore textarea {
    background: rgba(8, 12, 32, 0.45);
    border-color: rgba(255, 255, 255, 0.14);
    color: var(--lk-text);
  }

  .lore-bar,
  .lore-row {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px;
  }

  .lore-bar {
    margin-bottom: 10px;
  }

  .lore-bar input {
    width: 80px;
  }

  .spacer {
    flex: 1;
  }

  .lore-entry {
    display: grid;
    gap: 6px;
    margin-bottom: 12px;
    padding: 10px;
    border-radius: 10px;
    background: rgba(8, 12, 32, 0.28);
  }

  .lore-row .keys {
    flex: 1;
    min-width: 180px;
  }

  .lore-row .num {
    width: 70px;
  }

  .hint {
    color: var(--lk-dim);
    padding: 0 6px;
  }

  .center {
    margin: auto;
  }

  .notice {
    color: var(--warn);
    margin: 0;
  }

  .error {
    color: var(--err);
    margin: 0;
    white-space: pre-wrap;
  }
</style>

