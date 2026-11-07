<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import { t } from './i18n.svelte';

  // WebView UI thread. There is no system frame (decorations: false), so we
  // draw the window buttons ourselves over the top-right corner, like in GitPro.
  const win = getCurrentWindow();
  let maximized = $state(false);
  let unlisten: (() => void) | null = null;

  async function syncMaximized() {
    try {
      maximized = await win.isMaximized();
    } catch {
      // The window is already closing — the button state does not matter.
    }
  }

  onMount(async () => {
    await syncMaximized();
    unlisten = await win.onResized(() => void syncMaximized());
  });
  onDestroy(() => unlisten?.());
</script>

<div class="window-controls">
  <button type="button" title={t('window.minimize')} aria-label={t('window.minimize')} onclick={() => void win.minimize()}>
    <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true"><path d="M1 5h8" /></svg>
  </button>
  <button
    type="button"
    title={maximized ? t('window.restore') : t('window.maximize')}
    aria-label={maximized ? t('window.restore') : t('window.maximize')}
    onclick={() => void win.toggleMaximize()}
  >
    {#if maximized}
      <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true"><path d="M3 1h6v6M1 3h6v6H1z" /></svg>
    {:else}
      <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true"><path d="M1 1h8v8H1z" /></svg>
    {/if}
  </button>
  <button type="button" class="close" title={t('common.close')} aria-label={t('common.close')} onclick={() => void win.close()}>
    <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true"><path d="M1 1l8 8M9 1 1 9" /></svg>
  </button>
</div>

<style>
  /* Same as in Video Downloader: the top gap equals the right one. */
  .window-controls {
    position: fixed;
    top: 8px;
    right: 8px;
    z-index: 1000;
    display: flex;
    gap: 2px;
  }

  button {
    display: grid;
    width: 30px;
    height: 28px;
    place-items: center;
    padding: 0;
    border: 0;
    border-radius: 6px;
    background: transparent;
    box-shadow: none;
    color: var(--text-dim);
  }

  /* The button keeps focus after a click and a return from the tray, while the global
     button:focus-visible rule draws a ring. */
  button:focus,
  button:focus-visible {
    outline: none;
    box-shadow: none;
  }

  button:hover {
    background: #2d2d2d;
    color: var(--text);
  }

  button.close:hover {
    background: #c42b1c;
    color: #fff;
  }

  button:active {
    transform: none;
  }

  svg {
    fill: none;
    stroke: currentColor;
    stroke-width: 1;
  }
</style>
