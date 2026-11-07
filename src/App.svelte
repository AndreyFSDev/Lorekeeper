<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import { listen } from '@tauri-apps/api/event';
  import { api } from './lib/api';
  import type { BootState, SetupState } from './lib/types';
  import Wizard from './Wizard.svelte';
  import Settings from './Settings.svelte';
  import Guide from './Guide.svelte';
  import Launcher from './Launcher.svelte';
  import AppLogo from './lib/AppLogo.svelte';
  import WindowControls from './lib/WindowControls.svelte';
  import { i18n, t } from './lib/i18n.svelte';

  // The settings window opens with a tab in the address too (`#settings/tags`), hence
  // a prefix check: strict equality rendered such a window as the
  // launcher — without a close button and with an "Open SillyTavern" button.
  let isSettingsWindow = $state(window.location.hash.startsWith('#settings'));
  // The guide from the tray is its own window (`#guide`): it needs neither
  // an update check, nor boot polling, nor auto-navigation to SillyTavern.
  let isGuideWindow = $state(window.location.hash.startsWith('#guide'));

  let setup: SetupState | null = $state(null);
  let error: string | null = $state(null);
  $effect(() => {
    document.documentElement.lang = i18n.locale;
  });

  // Initial status before the first Rust poll — in the UI language, like the
  // statuses that will come from the backend (see l10n::t in src-tauri).
  let boot: BootState = $state({ status: t('boot.starting'), error: null, ready: false, port: 0 });
  let autoLaunchChecked = $state(false);

  /** Auto-navigation is decided from fresh settings: the `setup` snapshot may be stale
   *  if the checkbox was cleared in the settings window during boot. */
  async function maybeAutoLaunch() {
    if (
      autoLaunchChecked ||
      isSettingsWindow ||
      isGuideWindow ||
      updateInfo ||
      !boot.ready ||
      !boot.port
    ) {
      return;
    }
    autoLaunchChecked = true;
    try {
      const fresh = await api.getSettings();
      if (fresh.auto_launch_st) await openSillyTavern();
    } catch {
      // Settings are not available yet — stay on the settings screen.
    }
  }
  let updateInfo = $state<{ version: string; notes: string | null } | null>(null);
  let updateChecked = $state(false);
  let updating = $state(false);
  let updateProgressPct = $state(0);
  let updateError = $state<string | null>(null);
  let unlistenBoot: (() => void) | null = null;

  let navigated = false;
  const mountTime = Date.now();

  // Update check on the splash: after navigating to ST our UI is unreachable.
  async function checkUpdate() {
    try {
      const update = await api.checkAppUpdate();
      if (update.available && !navigated) {
        updateInfo = { version: update.latest_version, notes: update.notes };
      }
    } catch {
      // Offline, socket timeout or server unreachable — silently, the update is optional.
    } finally {
      updateChecked = true;
    }
  }

  async function applyUpdate() {
    updating = true;
    updateProgressPct = 0;
    updateError = null;
    try {
      await api.applyAppUpdate((p) => {
        updateProgressPct = p.percent;
      });
      // On success the process restarts (process::exit + spawn),
      // but if the restart did not happen — clear the lock.
      updating = false;
    } catch (e) {
      updateError = String(e);
      updating = false;
    }
  }

  let pollTimer: ReturnType<typeof setInterval> | null = null;

  async function openSillyTavern() {
    if (!boot.ready || !boot.port || navigated) return;
    stopPolling();
    navigated = true;
    try {
      await api.markStOpened();
    } catch {
      // Ignore
    }
    // Window buttons on the ST page are drawn by the init script (src-tauri/shell/st-window-controls.js).
    window.location.href = `http://127.0.0.1:${boot.port}/`;
  }

  async function pollBoot() {
    if (navigated) return;
    try {
      const b = await api.bootState();
      boot = b;

      // Navigation to the SillyTavern web server: only if the user explicitly enabled auto_launch_st in the settings.
      // Default false: on start the user sees the settings and opens SillyTavern with the button themselves.
      const bypassUpdateWait = updateChecked || Date.now() - mountTime > 3500;
      // The separate settings window never navigates away: auto-navigation to SillyTavern
      // concerns the main window, while here it would simply close the settings.
      if (bypassUpdateWait) {
        await maybeAutoLaunch();
      }
    } catch {
      // The command is not available yet — next tick.
    }
  }

  async function dismissUpdate() {
    updateInfo = null;
    if (isSettingsWindow || isGuideWindow) return;
    // Auto-navigation decision from fresh settings: the `setup` snapshot showed
    // the checkbox as of start time, and one cleared in the settings window was ignored.
    await maybeAutoLaunch();
  }

  function stopPolling() {
    if (pollTimer) {
      clearInterval(pollTimer);
      pollTimer = null;
    }
  }

  // The launcher (Settings) shows the error: here `error` is rendered only on
  // the splash, and in the launcher the message disappeared silently.
  async function retry() {
    await api.retryBoot();
    await pollBoot();
  }

  // The separate settings window is Settings only: the update check (a request to
  // GitHub on every open) and the endless boot poll are needed by the main window.
  // The guide window is a static page and needs neither.
  // It catches up on hash change too: a window mounted as #guide/#settings
  // would otherwise stay on the "Initializing environment…" splash forever.
  let mainModeStarted = false;
  async function startMainMode() {
    if (mainModeStarted || isSettingsWindow || isGuideWindow) return;
    mainModeStarted = true;

    // 1. Run the update check in parallel in the background, without blocking UI and service start
    checkUpdate().then(() => {
      maybeAutoLaunch();
    });

    // 2. Initialize local state and process polling without artificial delays
    try {
      const state = await api.setupState();
      if (state.settings.setup_done) {
        // The launcher mounts with a real boot state already: with a stub
        // ready=false it re-read all data a second time a moment after start.
        boot = await api.bootState().catch(() => boot);
      }
      setup = state;
      if (state.settings.setup_done) {
        unlistenBoot = await listen<string>('boot-status', () => pollBoot());
        pollTimer = setInterval(pollBoot, 800);
        await pollBoot();
      }
    } catch (e) {
      error = String(e);
      updateChecked = true;
    }
  }

  onMount(async () => {
    window.addEventListener('keydown', handleKeyDown);
    window.addEventListener('hashchange', handleHashChange);
    await startMainMode();
  });

  function handleKeyDown(e: KeyboardEvent) {
    if ((e.ctrlKey && e.key === ',') || (e.altKey && (e.key === 's' || e.key === 'ы'))) {
      e.preventDefault();
      api.openSettingsWindow().catch(() => {});
    }
  }

  function handleHashChange() {
    isSettingsWindow = window.location.hash.startsWith('#settings');
    isGuideWindow = window.location.hash.startsWith('#guide');
    startMainMode().catch(() => {});
  }

  onDestroy(() => {
    stopPolling();
    unlistenBoot?.();
    window.removeEventListener('keydown', handleKeyDown);
    window.removeEventListener('hashchange', handleHashChange);
  });

  function onSetupDone() {
    location.reload();
  }
</script>

{#snippet updateCard()}
  {#if updateInfo}
    <div class="update-modal-backdrop">
      <div class="update-card">
        <div class="update-badge">{t('update.badge')}</div>
        <b class="update-title">{t('update.available', { version: updateInfo.version })}</b>
        {#if updateInfo.notes}
          <p class="update-notes">{updateInfo.notes}</p>
        {/if}
        {#if updateError}
          <p class="err">{updateError}</p>
        {/if}
        <div class="row">
          <button class="primary" onclick={applyUpdate} disabled={updating}>
            {updating
              ? updateProgressPct > 0
                ? t('update.updating', { percent: updateProgressPct })
                : t('update.updatingNoPercent')
              : t('update.update')}
          </button>
          <button class="ghost" onclick={dismissUpdate} disabled={updating}>{t('update.later')}</button>
        </div>
      </div>
    </div>
  {/if}
{/snippet}

{#snippet cloufamCrystal(size = 64)}
  <span class="boot-logo"><AppLogo {size} /></span>
{/snippet}

<WindowControls />

{#if isGuideWindow}
  <Guide />
{:else if isSettingsWindow}
  <Settings />
{:else if !setup}
  <main class="boot-container" data-tauri-drag-region>
    {#if error}
      <div class="card boot-card error-card">
        <div class="logo error-mark">
          <svg width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true">
            <path d="M7 7l10 10M17 7 7 17" />
          </svg>
        </div>
        <h2>{t('boot.errorTitle')}</h2>
        <p class="err">{error}</p>
        <button class="primary" onclick={() => location.reload()}>{t('common.retry')}</button>
      </div>
      {@render updateCard()}
    {:else}
      <div class="card boot-card init-card">
        {@render cloufamCrystal()}
        <h2>Cloufam <span class="version-tag">v{__APP_VERSION__}</span></h2>
        <p class="dim">{t('boot.init')}</p>
        <div class="init-spinner"></div>
        {#if boot.progress != null}
          <div
            class="progress-track init-progress"
            role="progressbar"
            aria-valuemin="0"
            aria-valuemax="100"
            aria-valuenow={boot.progress}
          >
            <div class="progress-fill" style={`width: ${boot.progress}%`}></div>
          </div>
        {/if}
      </div>
      {@render updateCard()}
    {/if}
  </main>
{:else if !setup.settings.setup_done}
  <Wizard {setup} onDone={onSetupDone} />
{:else}
  {@render updateCard()}
  <Launcher
    {setup}
    {boot}
    onOpenSillyTavern={() => void openSillyTavern()}
    onRetryBoot={() => void retry()}
  />
{/if}

<style>
  .boot-container {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    min-height: 100vh;
    padding: 24px;
    background: var(--bg);
  }

  .boot-card {
    width: 100%;
    max-width: 460px;
    padding: 32px 30px;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 16px;
    text-align: center;
    animation: fadeUp 0.25s ease;
  }

  .boot-card h2 {
    margin: 0;
    font-size: 20px;
    font-weight: 500;
    display: flex;
    align-items: center;
    gap: 10px;
  }

  .init-card {
    padding: 40px 32px;
  }

  .boot-logo {
    color: var(--text);
    display: inline-flex;
    animation: breathe 2.4s ease-in-out infinite;
  }

  @keyframes breathe {
    0%, 100% { opacity: 1; }
    50% { opacity: 0.55; }
  }

  @media (prefers-reduced-motion: reduce) {
    .boot-logo {
      animation: none;
    }
  }

  .dim {
    margin: 0;
    color: var(--text-dim);
  }

  .version-tag {
    font-size: 11.5px;
    font-weight: 500;
    color: var(--text-dim);
    background: var(--surface-2);
    border: 1px solid var(--border-strong);
    padding: 2px 9px;
    border-radius: var(--radius-pill);
  }

  .init-spinner {
    width: 18px;
    height: 18px;
    border: 2px solid var(--surface-3);
    border-top-color: var(--text);
    border-radius: 50%;
    animation: spin 0.8s linear infinite;
  }

  /* Boot progress appears only where the step computes it — the bar should not
     leave an empty groove "just in case" in the card. */
  .init-progress {
    width: min(260px, 100%);
  }

  .logo {
    width: 48px;
    height: 48px;
    border-radius: 50%;
    display: flex;
    align-items: center;
    justify-content: center;
  }

  .logo.error-mark {
    background: var(--err-soft);
    color: var(--err);
  }

  .err {
    color: var(--err);
    max-width: 440px;
    margin: 0;
    line-height: 1.5;
    font-size: 13px;
    word-break: break-word;
  }

  .row {
    display: flex;
    gap: 10px;
    flex-wrap: wrap;
    justify-content: center;
    width: 100%;
  }

  .update-modal-backdrop {
    position: fixed;
    inset: 0;
    z-index: 10000;
    display: flex;
    align-items: center;
    justify-content: center;
    background: var(--overlay);
    padding: 20px;
    animation: backdropFade 0.15s ease-out;
  }

  @keyframes backdropFade {
    from { opacity: 0; }
    to { opacity: 1; }
  }

  .update-card {
    display: flex;
    flex-direction: column;
    gap: 12px;
    align-items: center;
    text-align: center;
    width: 100%;
    max-width: 460px;
    padding: 24px 28px;
    background: var(--surface);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius);
    box-shadow: var(--menu-shadow);
    animation: fadeUp 0.2s ease;
  }

  .update-title {
    font-size: 16px;
    font-weight: 500;
    color: var(--text);
  }

  .update-badge {
    font-size: 12px;
    color: var(--text-dim);
    background: var(--surface-2);
    border: 1px solid var(--border-strong);
    padding: 2px 10px;
    border-radius: var(--radius-pill);
  }

  .update-notes {
    margin: 0;
    font-size: 13.5px;
    color: var(--text-dim);
    line-height: 1.5;
    max-width: 420px;
  }
</style>

