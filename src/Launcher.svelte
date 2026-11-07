<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { api } from './lib/api';
  import { i18n, t, type I18nKey } from './lib/i18n.svelte';
  import LanguageMenu from './lib/LanguageMenu.svelte';
  import AppLogo from './lib/AppLogo.svelte';
  import Icon from './lib/Icon.svelte';
  import Settings from './Settings.svelte';
  import { sameTag } from './lib/models';
  import type { BootState, SetupState } from './lib/types';

  let {
    setup,
    boot,
    onOpenSillyTavern,
    onRetryBoot,
  }: {
    setup: SetupState;
    boot: BootState;
    onOpenSillyTavern: () => void;
    onRetryBoot: () => void;
  } = $props();

  type MenuId = 'model' | 'more';
  const FEATURES = ['characters', 'memory', 'vision', 'images', 'tts', 'rpg'] as const;
  const featureKey = (f: (typeof FEATURES)[number], part: 'title' | 'text') =>
    `launcher.features.${f}.${part}` as I18nKey;
  let openMenu = $state<MenuId | null>(null);
  let collapsed = $state(false);
  let aboutOpen = $state(false);

  // The settings section in the right part of the window; null — home. The list of sections
  // comes from the gateway (gateway.rs::SETTINGS_TABS); only the icons by id live here.
  let sections = $state<string[]>([]);
  let section = $state<string | null>(null);
  // Settings are mounted once on the first visit and after that only
  // hidden: otherwise every transition would reload the model catalog.
  let settingsMounted = $state(false);
  const SECTION_ICONS: Record<string, string> = {
    chat: 'chat',
    embed: 'memory',
    vision: 'eye',
    image: 'image',
    tune: 'wand',
    params: 'sliders',
    custom: 'plus',
    tags: 'tag',
    train: 'cap',
    imgtrain: 'image',
  };
  // Section placement: group and order within it. The list of ids is still only
  // from the gateway; an id missing from the map goes to "Other" so it is not lost.
  type GroupId = 'models' | 'tuning' | 'training' | 'other';
  const GROUP_ORDER: GroupId[] = ['models', 'tuning', 'training', 'other'];
  const SECTION_PLACE: Record<string, [GroupId, number]> = {
    chat: ['models', 0],
    embed: ['models', 1],
    vision: ['models', 2],
    image: ['models', 3],
    custom: ['models', 4],
    tune: ['tuning', 0],
    params: ['tuning', 1],
    tags: ['tuning', 2],
    train: ['training', 0],
    imgtrain: ['training', 1],
  };
  const groups = $derived(
    GROUP_ORDER.map((group) => ({
      group,
      items: sections
        .filter((id) => (SECTION_PLACE[id]?.[0] ?? 'other') === group)
        .sort((a, b) => (SECTION_PLACE[a]?.[1] ?? 99) - (SECTION_PLACE[b]?.[1] ?? 99)),
    })).filter((g) => g.items.length > 0),
  );

  function openSection(id: string | null) {
    openMenu = null;
    if (id) settingsMounted = true;
    section = id;
  }

  const gpu = $derived(setup.gpu_name?.trim() || t('wizard.noGpu'));
  const vram = $derived(setup.vram_mb ? `${(setup.vram_mb / 1024).toFixed(1)} GB` : '—');
  const free = $derived(setup.free_space_gb >= 0 ? `${setup.free_space_gb.toFixed(1)} GB` : '—');
  const chatTag = $derived(setup.settings.chat_model || '');
  const chatName = $derived(
    setup.models.find((m) => m.category === 'chat' && sameTag(m.tag, chatTag))?.name || chatTag || '—',
  );
  const chatSize = $derived(setup.models.find((m) => sameTag(m.tag, chatTag))?.size ?? '');

  const phase = $derived(boot.ready ? 'ready' : boot.error ? 'error' : 'booting');
  // Rust sends the boot status and error already in the UI language (see l10n).
  const statusDetail = $derived(boot.error ? boot.error : boot.status);

  function toggle(id: MenuId) {
    openMenu = openMenu === id ? null : id;
  }

  function run(action: () => Promise<void>) {
    openMenu = null;
    void action().catch(() => {});
  }

  async function resetToWizard() {
    if (!confirm(t('launcher.resetWizardConfirm'))) return;
    await api.resetSetup();
    location.reload();
  }

  function primaryAction() {
    if (boot.ready) onOpenSillyTavern();
    else if (boot.error) onRetryBoot();
  }

  // The handlers are attached to the main window's window and run in the WebView UI thread.
  // We close on pointerdown outside the menu anchor: click with stopPropagation does not help —
  // Svelte 5 delegates events, and window received the same click that opened the menu.
  function closeMenus(e: PointerEvent) {
    if (!(e.target instanceof Element) || !e.target.closest('.menu-anchor')) openMenu = null;
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      openMenu = null;
      aboutOpen = false;
    }
  }

  onMount(() => {
    window.addEventListener('pointerdown', closeMenus);
    window.addEventListener('keydown', onKey);
    api
      .settingsTabs()
      .then((tabs) => (sections = Array.isArray(tabs) ? tabs.filter((id) => typeof id === 'string') : []))
      .catch(() => {
        // Without the section list, home and the separate settings window from the "…" menu remain.
      });
  });

  onDestroy(() => {
    window.removeEventListener('pointerdown', closeMenus);
    window.removeEventListener('keydown', onKey);
  });
</script>


<div class="shell" class:collapsed>
  <aside class="sidebar">
    <div class="side-top" data-tauri-drag-region>
      <span class="brand"><AppLogo size={28} /><span class="brand-name" data-tauri-drag-region>Cloufam</span></span>
      <button
        class="ghost icon side-toggle"
        title={collapsed ? t('launcher.expand') : t('launcher.collapse')}
        aria-label={collapsed ? t('launcher.expand') : t('launcher.collapse')}
        onclick={() => (collapsed = !collapsed)}
      >
        <Icon name={'panel'} />
      </button>
    </div>

    <div class="side-scroll">
    <nav class="nav">
      <button class="nav-item" class:active={section === null} onclick={() => openSection(null)}>
        <Icon name={'home'} /><span>{t('launcher.home')}</span>
      </button>
    </nav>

    {#each groups as { group, items } (group)}
      <div class="nav-group" class:hidden-label={collapsed}>{t(`launcher.group.${group}` as I18nKey)}</div>
      <nav class="nav">
        {#each items as id (id)}
          <button
            class="nav-item"
            class:active={section === id}
            title={t(`settings.tab.${id}` as I18nKey)}
            onclick={() => openSection(id)}
          >
            <Icon name={SECTION_ICONS[id] ?? 'settings'} /><span>{t(`settings.tab.${id}` as I18nKey)}</span>
          </button>
        {/each}
      </nav>
    {/each}

    <nav class="nav nav-tools">
      <button class="nav-item" onclick={() => run(api.openModelsFolder)}>
        <Icon name={'models'} /><span>{t('launcher.modelsFolder')}</span>
      </button>
      <button class="nav-item" onclick={() => run(api.openStFolder)}>
        <Icon name={'folder'} /><span>{t('launcher.stFolder')}</span>
      </button>
      <button class="nav-item" title={t('about.title')} onclick={() => (aboutOpen = true)}>
        <Icon name={'sparkle'} /><span>{t('about.title')}</span>
      </button>
    </nav>
    </div>

    <div class="side-bottom">
      <LanguageMenu variant="nav" up />
      <div class="version">Cloufam v{__APP_VERSION__}</div>
    </div>
  </aside>

  <main class="main" class:gone={section !== null} data-tauri-drag-region>
    <div class="hero">
      <span class="hero-logo"><AppLogo size={56} /></span>
      <h1>{t('launcher.heading')}</h1>

      <div class="composer">
        <p class="composer-text" class:err={phase === 'error'} title={boot.status}>
          {#if phase === 'ready'}
            {t('launcher.composerReady')}
          {:else if phase === 'error'}
            {t('launcher.composerError')}
          {:else}
            {statusDetail}
          {/if}
        </p>
        {#if phase === 'booting' && boot.progress != null}
          <div class="progress-track" role="progressbar" aria-valuemin="0" aria-valuemax="100" aria-valuenow={boot.progress}>
            <div class="progress-fill" style:width="{boot.progress}%"></div>
          </div>
        {/if}
        {#if phase === 'error'}
          <p class="composer-error">{boot.error}</p>
        {/if}

        <div class="composer-bar">
          <div class="pill-group">
            <span class="pill status {phase}">
              {#if phase === 'booting'}<span class="spinner small"></span>{:else}<span class="dot"></span>{/if}
              {phase === 'ready' ? t('launcher.ready') : phase === 'error' ? t('launcher.error') : t('launcher.booting')}
            </span>
            <div class="menu-anchor">
              <button class="pill" onclick={() => toggle('model')} aria-haspopup="menu" aria-expanded={openMenu === 'model'}>
                <Icon name={'chip'} /><span class="pill-label">{chatName}</span><Icon name={'chevron'} />
              </button>
              {#if openMenu === 'model'}
                <div class="menu" role="menu">
                  <div class="menu-item static">
                    <span class="menu-icon"><Icon name={'chat'} /></span>
                    <span class="menu-label">{chatName}</span>
                    <span class="menu-hint">{chatSize}</span>
                    <span class="menu-check"><Icon name={'check'} /></span>
                  </div>
                  <div class="menu-sep"></div>
                  <button class="menu-item" onclick={() => run(api.openSettingsWindow)}>
                    <span class="menu-icon"><Icon name={'settings'} /></span>
                    <span class="menu-label">{t('launcher.manageModels')}</span>
                    <span class="menu-hint">Ctrl+,</span>
                    <span class="menu-check"></span>
                  </button>
                </div>
              {/if}
            </div>
          </div>

          <div class="bar-right">
            <div class="menu-anchor">
              <button class="ghost icon" title={t('launcher.more')} aria-label={t('launcher.more')} onclick={() => toggle('more')}>
                <Icon name={'dots'} />
              </button>
              {#if openMenu === 'more'}
                <div class="menu menu-right" role="menu">
                  <button class="menu-item" onclick={() => run(api.openModelsFolder)}>
                    <span class="menu-icon"><Icon name={'models'} /></span>
                    <span class="menu-label">{t('launcher.modelsFolder')}</span>
                    <span class="menu-check"></span>
                  </button>
                  <button class="menu-item" onclick={() => run(api.openStFolder)}>
                    <span class="menu-icon"><Icon name={'folder'} /></span>
                    <span class="menu-label">{t('launcher.stFolder')}</span>
                    <span class="menu-check"></span>
                  </button>
                  <div class="menu-sep"></div>
                  <button class="menu-item" onclick={() => run(api.openSettingsWindow)}>
                    <span class="menu-icon"><Icon name={'settings'} /></span>
                    <span class="menu-label">{t('launcher.settings')}</span>
                    <span class="menu-hint">Ctrl+,</span>
                    <span class="menu-check"></span>
                  </button>
                  <button class="menu-item" onclick={() => run(resetToWizard)}>
                    <span class="menu-icon"><Icon name={'retry'} /></span>
                    <span class="menu-label">{t('launcher.resetWizard')}</span>
                    <span class="menu-check"></span>
                  </button>
                </div>
              {/if}
            </div>
            <button
              class="launch"
              class:retry={phase === 'error'}
              disabled={phase === 'booting'}
              title={phase === 'error' ? t('launcher.retry') : t('launcher.open')}
              aria-label={phase === 'error' ? t('launcher.retry') : t('launcher.open')}
              onclick={primaryAction}
            >
              <Icon name={phase === 'error' ? 'retry' : 'play'} />
            </button>
          </div>
        </div>
      </div>

      <ul class="features">
        {#each FEATURES as f (f)}
          <li title={t(featureKey(f, 'text'))}>{t(featureKey(f, 'title'))}</li>
        {/each}
      </ul>
      <p class="note">{t('launcher.note')}</p>
      <!-- Machine details live on the home screen: in the sidebar they lacked
           room next to the settings sections. The chat model is visible in the pill above. -->
      <dl class="facts">
        <div><dt>{t('launcher.gpu')}</dt><dd title={gpu}>{gpu}</dd></div>
        <div><dt>{t('launcher.vram')}</dt><dd>{vram}</dd></div>
        <div><dt>{t('launcher.freeSpace')}</dt><dd>{free}</dd></div>
      </dl>
    </div>
  </main>

  {#if settingsMounted}
    <section class="section-view" class:gone={section === null}>
      <Settings
        embedded
        hidden={section === null}
        tab={section ?? undefined}
        onTabChange={(id) => {
          if (section !== null && id !== section) section = id;
        }}
      />
    </section>
  {/if}
</div>

{#if aboutOpen}
  <div class="about-backdrop" role="presentation" onclick={(e) => e.target === e.currentTarget && (aboutOpen = false)}>
    <div class="about card" role="dialog" aria-modal="true" aria-labelledby="about-title">
      <span class="hero-logo"><AppLogo size={48} /></span>
      <h2 id="about-title">Cloufam <span class="about-version">v{__APP_VERSION__}</span></h2>
      <p class="about-tagline">{t('about.tagline')}</p>
      <dl class="about-facts">
        <dt>{t('about.repo')}</dt>
        <dd><button class="link" onclick={() => run(() => api.openAboutLink('repo'))}>github.com/AndreyFSDev/Cloufam</button></dd>
        <dt>{t('about.license')}</dt>
        <dd>
          <button class="link" onclick={() => run(() => api.openAboutLink('license'))}>{t('about.licenseText')}</button>
        </dd>
      </dl>
      <button class="about-close" onclick={() => (aboutOpen = false)}>{t('common.close')}</button>
    </div>
  </div>
{/if}

<style>
  .about-backdrop {
    position: fixed;
    inset: 0;
    z-index: 900;
    display: grid;
    place-items: center;
    background: rgba(0, 0, 0, 0.55);
  }

  .about {
    display: grid;
    justify-items: center;
    gap: 10px;
    width: min(460px, calc(100vw - 48px));
    padding: 28px;
    text-align: center;
  }

  .about h2 {
    margin: 0;
  }

  .about-version,
  .about-tagline {
    color: var(--text-dim);
  }

  .about-tagline {
    margin: 0;
  }

  .about-facts {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 6px 14px;
    margin: 8px 0;
    text-align: left;
  }

  .about-facts dt {
    color: var(--text-dim);
  }

  .about-facts dd {
    margin: 0;
  }

  .link {
    padding: 0;
    border: 0;
    background: none;
    box-shadow: none;
    color: var(--accent2);
    text-align: left;
    cursor: pointer;
  }

  .link:hover {
    text-decoration: underline;
  }

  .shell {
    display: grid;
    grid-template-columns: var(--sidebar-width) 1fr;
    /* The row is exactly the window height: otherwise a long settings section stretched it and the whole window scrolled. */
    grid-template-rows: minmax(0, 1fr);
    height: 100vh;
    background: var(--bg);
    transition: grid-template-columns 0.2s ease;
  }

  .shell.collapsed {
    grid-template-columns: 60px 1fr;
  }

  .sidebar {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 12px 10px;
    background: var(--bg-sidebar);
    border-right: 1px solid var(--border);
    min-width: 0;
  }

  .side-top {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 2px 4px 10px;
  }

  .brand {
    color: var(--text);
    display: inline-flex;
    align-items: center;
    gap: 10px;
    padding: 4px;
  }

  .brand-name {
    font-size: 15px;
    font-weight: 600;
    white-space: nowrap;
  }

  .collapsed .side-top {
    flex-direction: column;
    gap: 6px;
  }

  .side-toggle {
    width: 30px;
    height: 30px;
    color: var(--text-dim);
  }

  .nav {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .nav-item {
    box-shadow: none;
    transition: none;
    display: flex;
    align-items: center;
    gap: 12px;
    width: 100%;
    padding: 7px 10px;
    background: transparent;
    border: none;
    border-radius: 10px;
    color: var(--text);
    font-size: 13.5px;
    text-align: left;
    white-space: nowrap;
  }

  .nav-item span {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .nav-item :global(svg) {
    flex: none;
    color: var(--text-dim);
  }

  .nav-item:hover,
  .nav-item.active {
    background: var(--surface-2);
  }

  .nav-item:active {
    transform: none;
  }

  .nav-hint {
    margin-left: auto;
    color: var(--text-faint);
    font-size: 12px;
  }

  .side-scroll {
    flex: 1;
    min-height: 0;
    overflow-x: hidden;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .nav-group {
    padding: 10px 10px 4px;
    color: var(--text-faint);
    font-size: 12px;
  }

  .nav-group.hidden-label {
    visibility: hidden;
    padding: 6px 0 0;
    height: 0;
  }

  .nav-tools {
    margin-top: 10px;
  }

  .section-view {
    grid-column: 2;
    grid-row: 1;
    min-width: 0;
    min-height: 0;
    overflow: hidden;
  }

  .gone {
    display: none !important;
  }

  .facts {
    margin: 0;
    display: flex;
    flex-wrap: wrap;
    justify-content: center;
    gap: 4px 18px;
  }

  .facts div {
    display: flex;
    align-items: baseline;
    gap: 6px;
    min-width: 0;
  }

  .facts dt {
    color: var(--text-faint);
    font-size: 12px;
  }

  .facts dd {
    margin: 0;
    color: var(--text-dim);
    font-size: 12px;
    white-space: nowrap;
  }

  .collapsed .nav-item span,
  .collapsed .brand-name,
  .collapsed .version {
    display: none;
  }

  .collapsed .nav-item {
    justify-content: center;
    padding: 9px 0;
  }

  .side-bottom {
    margin-top: auto;
    border-top: 1px solid var(--border);
    padding-top: 8px;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .version {
    padding: 2px 10px 4px;
    color: var(--text-faint);
    font-size: 12px;
  }

  .main {
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 24px;
    overflow-y: auto;
    min-width: 0;
  }

  .hero {
    width: 100%;
    max-width: 640px;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 18px;
    margin-top: -6vh;
    animation: fadeUp 0.25s ease;
  }

  .hero-logo {
    color: var(--text);
    display: inline-flex;
  }

  h1 {
    margin: 0 0 10px;
    font-size: 26px;
    font-weight: 500;
    letter-spacing: -0.2px;
    text-align: center;
  }

  .composer {
    width: 100%;
    display: flex;
    flex-direction: column;
    gap: 12px;
    padding: 16px 14px 12px 18px;
    background: var(--surface);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius);
    box-shadow: inset 0 1px 0 rgba(255, 255, 255, 0.05), 0 6px 18px rgba(0, 0, 0, 0.28);
  }

  .composer-text {
    margin: 0;
    min-height: 40px;
    color: var(--text-dim);
    font-size: 15px;
    line-height: 1.45;
  }

  .composer-text.err {
    color: var(--text);
  }

  .composer-error {
    margin: -4px 0 0;
    color: var(--err);
    font-size: 12.5px;
    word-break: break-word;
  }

  .composer-bar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 10px;
  }

  .pill-group {
    display: flex;
    align-items: center;
    gap: 2px;
    padding: 2px;
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: var(--radius-pill);
    min-width: 0;
  }

  .pill {
    box-shadow: none;
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 5px 12px;
    background: transparent;
    border: 1px solid transparent;
    border-radius: var(--radius-pill);
    color: var(--text-dim);
    font-size: 13.5px;
    white-space: nowrap;
    max-width: 260px;
  }

  button.pill:hover {
    background: var(--surface-2);
    color: var(--text);
  }

  .pill.status {
    background: var(--btn-fill);
    box-shadow: var(--btn-shadow);
    border-color: var(--border-strong);
    color: var(--text);
  }

  .pill.status.ready .dot {
    color: var(--ok);
  }

  .pill.status.error .dot {
    color: var(--err);
  }

  .pill-label {
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .spinner.small {
    width: 11px;
    height: 11px;
    border-width: 1.5px;
  }

  .bar-right {
    display: flex;
    align-items: center;
    gap: 6px;
  }

  .bar-right .ghost.icon {
    width: 32px;
    height: 32px;
  }

  .launch {
    width: 34px;
    height: 34px;
    padding: 0;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border-radius: 50%;
    border: none;
    background: var(--primary-fill);
    box-shadow: var(--primary-shadow);
    color: var(--on-primary);
  }

  .launch:hover {
    background: var(--primary-hover);
  }

  .launch:disabled {
    opacity: 1;
    box-shadow: var(--btn-bevel);
    background: var(--surface-3);
    color: var(--text-faint);
  }

  .menu-anchor {
    position: relative;
  }

  .menu {
    top: calc(100% + 6px);
    left: 0;
  }

  .menu-up {
    top: auto;
    bottom: calc(100% + 6px);
  }

  .menu-right {
    left: auto;
    right: 0;
  }

  .menu-item.static {
    cursor: default;
  }

  .menu-item.static:hover {
    background: transparent;
  }

  .features {
    list-style: none;
    margin: 6px 0 0;
    padding: 0;
    display: flex;
    flex-wrap: wrap;
    justify-content: center;
    gap: 8px;
  }

  .features li {
    padding: 5px 12px;
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-pill);
    background: var(--btn-fill);
    box-shadow: var(--btn-shadow);
    color: var(--text-dim);
    font-size: 12.5px;
  }

  .note {
    margin: 0;
    color: var(--text-faint);
    font-size: 12.5px;
    text-align: center;
  }
</style>

