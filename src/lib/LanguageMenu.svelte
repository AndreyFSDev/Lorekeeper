<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import Icon from './Icon.svelte';
  import { i18n, systemLocale, t, type LocaleMode } from './i18n.svelte';

  // `nav` — sidebar entry, `pill` — pill in the header; `up` opens the menu upwards.
  let { variant = 'pill', up = false }: { variant?: 'pill' | 'nav'; up?: boolean } = $props();

  let open = $state(false);

  const options: { mode: LocaleMode; label: () => string; hint: () => string }[] = [
    { mode: 'auto', label: () => t('launcher.langAuto'), hint: () => systemLocale().toUpperCase() },
    { mode: 'ru', label: () => t('launcher.langRu'), hint: () => 'RU' },
    { mode: 'en', label: () => t('launcher.langEn'), hint: () => 'EN' },
  ];

  function choose(mode: LocaleMode) {
    i18n.setMode(mode);
    open = false;
  }

  let anchor: HTMLDivElement;

  // Handlers on the owner window's `window`, WebView UI thread. pointerdown outside
  // the anchor, not click: the delegated Svelte click reached window and immediately
  // closed the menu that had just opened.
  const close = (e: PointerEvent) => {
    if (!(e.target instanceof Node) || !anchor.contains(e.target)) open = false;
  };
  const onKey = (e: KeyboardEvent) => {
    if (e.key === 'Escape') open = false;
  };

  onMount(() => {
    window.addEventListener('pointerdown', close);
    window.addEventListener('keydown', onKey);
  });

  onDestroy(() => {
    window.removeEventListener('pointerdown', close);
    window.removeEventListener('keydown', onKey);
  });
</script>

<div class="anchor" bind:this={anchor}>
  <button
    class={variant === 'nav' ? 'nav-trigger' : 'pill-trigger'}
    aria-haspopup="menu"
    aria-expanded={open}
    title={t('launcher.language')}
    onclick={() => (open = !open)}
  >
    <Icon name="globe" />
    {#if variant === 'nav'}<span class="label">{t('launcher.language')}</span>{/if}
    <span class="hint">{i18n.locale.toUpperCase()}</span>
  </button>
  {#if open}
    <div class="menu" class:up class:align-right={variant === 'pill'} role="menu">
      {#each options as option (option.mode)}
        <button
          class="menu-item"
          role="menuitemradio"
          aria-checked={i18n.mode === option.mode}
          onclick={() => choose(option.mode)}
        >
          <span class="menu-label">{option.label()}</span>
          <span class="menu-hint">{option.hint()}</span>
          <span class="menu-check">{#if i18n.mode === option.mode}<Icon name="check" size={16} />{/if}</span>
        </button>
      {/each}
    </div>
  {/if}
</div>

<style>
  .anchor {
    position: relative;
  }

  .menu {
    top: calc(100% + 6px);
    left: 0;
  }

  .menu.up {
    top: auto;
    bottom: calc(100% + 6px);
  }

  .menu.align-right {
    left: auto;
    right: 0;
  }

  .nav-trigger {
    box-shadow: none;
    transition: none;
    display: flex;
    align-items: center;
    gap: 12px;
    width: 100%;
    padding: 9px 10px;
    background: transparent;
    border: none;
    border-radius: 10px;
    color: var(--text);
    text-align: left;
    white-space: nowrap;
  }

  .nav-trigger:hover {
    background: var(--surface-2);
  }

  .nav-trigger:active {
    transform: none;
  }

  .nav-trigger :global(svg) {
    flex: none;
    color: var(--text-dim);
  }

  .nav-trigger .hint {
    margin-left: auto;
  }

  .pill-trigger {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 5px 12px;
    font-size: 13px;
    color: var(--text-dim);
  }

  .pill-trigger:hover {
    color: var(--text);
  }

  .hint {
    color: var(--text-faint);
    font-size: 12px;
  }

  :global(.collapsed) .nav-trigger .label,
  :global(.collapsed) .nav-trigger .hint {
    display: none;
  }

  :global(.collapsed) .nav-trigger {
    justify-content: center;
    padding: 9px 0;
  }
</style>
