<script lang="ts">
  import { listen } from '@tauri-apps/api/event';
  import { onDestroy, onMount } from 'svelte';
  import { api } from './lib/api';
  import { isModelInstalled, sameTag } from './lib/models';
  import { i18n, t } from './lib/i18n.svelte';
  import AppLogo from './lib/AppLogo.svelte';
  import Icon from './lib/Icon.svelte';
  import LanguageMenu from './lib/LanguageMenu.svelte';
  import { open as openDialog } from '@tauri-apps/plugin-dialog';
  import type { DownloadProgress, ModelDownloadProgress, ModelStore, SetupState, StSetupProgress, VisionDownloadResult } from './lib/types';

  let { setup, onDone }: { setup: SetupState; onDone: () => void } = $props();

  type Step = 'intro' | 'runtime' | 'models' | 'sillytavern' | 'done';

  let step: Step = $state('intro');
  let busy = $state(false);
  let error: string | null = $state(null);

  let rtProgress: DownloadProgress = $state({ completed: 0, total: 0, status: '' });
  let modelProgress = $state<Record<string, ModelDownloadProgress>>({});
  let completedModels = $state<Record<string, boolean>>({});
  let failedModels = $state<Record<string, string>>({});
  let currentModel = $state('');
  let visionState = $state<'idle' | 'downloading' | 'done' | 'error' | 'cancelled'>('idle');
  let visionError = $state('');
  let stProgress = $state<StSetupProgress>({ step: '', status: '', completed: 0, total: 0 });
  // Cancelling long downloads with a button: the model queue is driven by the backend
  // (items with ids), the runtime — by the shared cancel_download flag. cancelRequested
  // distinguishes "cancelled by the user" from a failure in the error handlers below.
  let cancelling = $state(false);
  // The runtime downloads inside startSetup: the cancel button is visible only in this
  // window, otherwise a click on it outside a download would leave a false cancel mark.
  let runtimeDownloading = $state(false);
  let cancelRequested = false;
  let visionCancelRequested = false;

  /**
   * Model readiness — by the terminal status only. Ollama tracks progress counters
   * per layer and resets them on each new one, so "completed >=
   * total" is true already on the template and the licence, while the model weighs gigabytes more.
   */
  function isTerminalStatus(status: string): boolean {
    return status === 'готово' || status === 'уже установлено' || status === 'success';
  }

  // The model showcase comes from the backend already filtered for the hardware:
  // the user is offered only the options their machine can handle.
  const chatOptions = $derived(setup.models.filter((m) => m.category === 'chat'));

  let appSettings = $state<SetupState['settings']>(
    (() => {
      const s = { ...setup.settings };
      if (setup.recommended_chat_model) {
        s.chat_model = setup.recommended_chat_model;
      } else if (chatOptions.length > 0) {
        s.chat_model = chatOptions[chatOptions.length - 1].tag;
      }
      return s;
    })(),
  );

  let unlisten: (() => void) | null = null;

  function checkInstalled(list: string[], tag: string): boolean {
    return isModelInstalled(list, tag);
  }

  function isDone(tag: string): boolean {
    if (completedModels[tag]) return true;
    const p = modelProgress[tag];
    return !!p && isTerminalStatus(p.status);
  }

  function statusLabel(status: string): string {
    switch (status) {
      case 'готово':
      case 'success':
        return t('wizard.statusReady');
      case 'уже установлено':
        return t('wizard.alreadyInstalled');
      case 'ошибка':
        return t('wizard.statusError');
      case 'отменено':
        return t('wizard.statusCancelled');
      case 'ожидание…':
        return t('wizard.waiting');
      default:
        // Rust sends the runtime and ST steps in Russian; Ollama statuses are in English.
        return i18n.locale !== 'ru' && /[а-яё]/i.test(status) ? t('wizard.downloading') : status;
    }
  }

  function formatModelStatus(p: ModelDownloadProgress | undefined, done: boolean): string {
    if (!p) return '';
    if (done) return p.status === 'уже установлено' ? t('wizard.alreadyInstalled') : t('wizard.ready');
    let text = statusLabel(p.status);
    if (p.total > 0) {
      text += ` · ${mb(p.completed)} / ${mb(p.total)}`;
    }
    if (p.speed_bytes_per_sec && p.speed_bytes_per_sec > 0) {
      text += ` · ${(p.speed_bytes_per_sec / 1048576).toFixed(1)} ${t('wizard.mbps')}`;
    }
    if (p.eta_sec && p.eta_sec > 0) {
      const mins = Math.ceil(p.eta_sec / 60);
      text += ` · ~${mins} ${t('wizard.min')}`;
    }
    return text;
  }

  const MODELS = $derived<{ tag: string; label: string; note: string; icon: string; required: boolean }[]>([
    {
      tag: appSettings.embed_model,
      label: t('wizard.memory'),
      note: sizeOf(appSettings.embed_model),
      icon: 'memory',
      required: true,
    },
    {
      tag: appSettings.chat_model,
      label: t('wizard.chat'),
      note: sizeOf(appSettings.chat_model),
      icon: 'chat',
      required: true,
    },
    {
      tag: appSettings.vision_model,
      label: t('wizard.vision'),
      note: `${sizeOf(appSettings.vision_model)} · ${t('wizard.visionBackground')}`,
      icon: 'eye',
      required: false,
    },
  ]);

  /** The caption with the size comes from the showcase: the catalog knows each model's weight. */
  function sizeOf(tag: string): string {
    const model = setup.models.find((m) => sameTag(m.tag, tag));
    return model ? `${model.size}, ${model.quant}` : '';
  }

  // The model warehouse is chosen before the runtime download: a running Ollama keeps
  // the previous path (see `set_models_root` in lib.rs).
  let stores = $state<ModelStore[]>([]);
  let storeBusy = $state(false);
  let storeError: string | null = $state(null);
  const currentStore = $derived(stores.find((s) => s.current));
  const foundStores = $derived(stores.filter((s) => !s.current && s.models > 0));

  async function loadStores() {
    try {
      stores = await api.modelStores();
    } catch (e) {
      storeError = String(e);
    }
  }

  async function useStore(path: string) {
    storeBusy = true;
    storeError = null;
    try {
      await api.setModelsRoot(path);
      await loadStores();
    } catch (e) {
      storeError = String(e);
    } finally {
      storeBusy = false;
    }
  }

  async function pickStore() {
    const picked = await openDialog({
      directory: true,
      multiple: false,
      title: t('wizard.store.dialog'),
      defaultPath: currentStore?.path ?? setup.models_root,
    }).catch((e) => {
      storeError = String(e);
      return null;
    });
    if (typeof picked === 'string') await useStore(picked);
  }

  let mounted = false;

  onMount(async () => {
    mounted = true;
    void loadStores();
    const [un1, un2, un3, un4] = await Promise.all([
      listen<DownloadProgress>('runtime-download-progress', (e) => {
        if (!mounted) return;
        rtProgress = e.payload;
      }),
      listen<ModelDownloadProgress>('model-download-progress', (e) => {
        if (!mounted) return;
        const p = e.payload;
        currentModel = p.model;
        modelProgress = { ...modelProgress, [p.model]: p };
        if (isTerminalStatus(p.status)) {
          completedModels = { ...completedModels, [p.model]: true };
        }
      }),
      listen<StSetupProgress>('st-setup-progress', (e) => {
        if (!mounted) return;
        stProgress = e.payload;
      }),
      listen<VisionDownloadResult>('vision-download-done', (e) => {
        if (!mounted) return;
        // A button cancel — the "cancelled" state, not "error": it was initiated by the
        // user, a red banner would lie about a breakdown. The second sign is the
        // cancel text from the queue (downloads::CANCELLED), if the download was stopped
        // from another window.
        const cancelled =
          visionCancelRequested || e.payload?.error === 'отменено пользователем';
        visionState = e.payload?.ok ? 'done' : cancelled ? 'cancelled' : 'error';
        visionError = cancelled ? '' : e.payload?.error ?? '';
        visionCancelRequested = false;
      }),
    ]);
    unlisten = () => {
      un1();
      un2();
      un3();
      un4();
    };

    if (!setup.models.some((m) => m.category === 'vision')) {
      visionState = 'error';
      visionError = t('wizard.visionUnavailable');
    }

    api.installedModels().then((list) => {
      const completed: Record<string, boolean> = {};
      const prog: Record<string, ModelDownloadProgress> = {};
      for (const m of [appSettings.embed_model, appSettings.chat_model, appSettings.vision_model]) {
        if (checkInstalled(list, m)) {
          completed[m] = true;
          prog[m] = { model: m, completed: 1, total: 1, status: 'уже установлено' };
        }
      }
      completedModels = { ...completedModels, ...completed };
      modelProgress = { ...modelProgress, ...prog };
    }).catch(() => {});
  });

  onDestroy(() => {
    mounted = false;
    unlisten?.();
  });

  async function startSetup() {
    busy = true;
    error = null;
    cancelRequested = false;
    try {
      // The selected model must reach the settings before downloading and ST seeding.
      await api.saveSettings(appSettings);
      if (!setup.runtime_installed) {
        step = 'runtime';
        runtimeDownloading = true;
        try {
          await api.downloadRuntime();
        } finally {
          runtimeDownloading = false;
        }
      }
      await api.startRuntime();
      const installed = await api.installedModels().catch(() => [] as string[]);
      const queue = [appSettings.embed_model, appSettings.chat_model];
      const allInstalled = queue.every((m) => completedModels[m] || checkInstalled(installed, m));
      if (allInstalled) {
        step = 'sillytavern';
        // The chat and memory models are already on disk — the model step is skipped, but
        // vision downloads only from here. Without this call it never starts.
        visionState = 'downloading';
        api.backgroundVisionDownload().catch(() => {});
        await installSillytavern();
      } else {
        step = 'models';
        await downloadModels();
      }
    } catch (e) {
      // A button cancel is not a failure: instead of the error text we show a calm
      // line, "Retry" continues from the same step.
      error = cancelRequested ? t('wizard.cancelled') : String(e);
      cancelRequested = false;
      busy = false;
    }
  }

  /** Cancelling the wizard's current downloads. The runtime downloads under the shared
   *  cancel_download flag; models — backend queue items, each with its own id
   *  (tag): a pending download_model will return the "cancelled" error. */
  async function cancelDownloads() {
    if (cancelling) return;
    cancelling = true;
    try {
      if (step === 'runtime') {
        cancelRequested = true;
        await api.cancelDownload();
        return;
      }
      const modelIds = [currentModel, visionState === 'downloading' ? appSettings.vision_model : ''];
      const ids = modelIds.filter((id) => id !== '');
      // Nothing to stop: the window between models — a cancel would interrupt nothing,
      // while a false mark would turn the next failure into "cancelled".
      if (ids.length === 0) return;
      cancelRequested = true;
      if (ids.includes(appSettings.vision_model)) {
        visionCancelRequested = true;
      }
      await Promise.all(ids.map((id) => api.downloadsCancelTask(id)));
    } catch (e) {
      cancelRequested = false;
      visionCancelRequested = false;
      error = String(e);
    } finally {
      cancelling = false;
    }
  }

  async function downloadModels() {
    busy = true;
    error = null;
    cancelRequested = false;
    try {
      // The settings were already saved by startSetup (the model cannot be chosen at this step).
      // Re-writing the snapshot after startRuntime rolled back ollama_port,
      // if ensure_runtime moved Ollama to a free port.
      const installed = await api.installedModels().catch(() => [] as string[]);
      const queue = [appSettings.embed_model, appSettings.chat_model];
      for (const model of queue) {
        if (completedModels[model] || checkInstalled(installed, model)) {
          completedModels = { ...completedModels, [model]: true };
          modelProgress = {
            ...modelProgress,
            [model]: { model, completed: 1, total: 1, status: 'уже установлено' },
          };
          continue;
        }

        currentModel = model;
        // Retry after an error: the old red badge and error text must not
        // hang over a running download and the green "ready".
        if (failedModels[model] !== undefined) {
          const next = { ...failedModels };
          delete next[model];
          failedModels = next;
        }
        modelProgress = {
          ...modelProgress,
          [model]: { model, completed: 0, total: 0, status: 'ожидание…' },
        };
        await api.downloadModel(model);
        completedModels = { ...completedModels, [model]: true };
        modelProgress = {
          ...modelProgress,
          [model]: {
            model,
            completed: modelProgress[model]?.total || 1,
            total: modelProgress[model]?.total || 1,
            status: 'готово',
          },
        };
      }
      currentModel = '';
      visionState = 'downloading';
      api.backgroundVisionDownload().catch(() => {});
      step = 'sillytavern';
      await installSillytavern();
    } catch (e) {
      // The line where everything failed must not keep the "in progress" spinner:
      // the process is already gone, while the user sees two contradictory captions.
      const failed = currentModel;
      currentModel = '';
      if (cancelRequested) {
        // A button cancel — the line gets "cancelled", not a red "error".
        cancelRequested = false;
        if (failed) {
          modelProgress = {
            ...modelProgress,
            [failed]: { model: failed, completed: 0, total: 0, status: 'отменено' },
          };
        }
        error = t('wizard.cancelled');
        busy = false;
        return;
      }
      if (failed) {
        failedModels = { ...failedModels, [failed]: String(e) };
        modelProgress = {
          ...modelProgress,
          [failed]: { model: failed, completed: 0, total: 0, status: 'ошибка' },
        };
      }
      error = String(e);
      busy = false;
    }
  }

  async function installSillytavern() {
    busy = true;
    error = null;
    try {
      await api.downloadSillytavern();
      await api.finalizeSetup();
      step = 'done';
      unlisten?.();
      onDone();
    } catch (e) {
      error = String(e);
      busy = false;
    }
  }

  function pct(d: DownloadProgress): number {
    if (!d.total) return 0;
    return Math.min(100, (d.completed / d.total) * 100);
  }

  function mb(n: number): string {
    return `${(n / 1048576).toFixed(0)} ${t('wizard.mb')}`;
  }

  const stepIndex = $derived(
    step === 'intro' ? 0 : step === 'runtime' ? 1 : step === 'models' ? 2 : step === 'sillytavern' ? 3 : 4,
  );

  // The recommended model's name — from this hardware's catalog. The hardcoded
  // "8B/14B" outlived the move to Qwen 3.5 and contradicted the badge below.
  const recommendedName = $derived(
    setup.models.find((m) => sameTag(m.tag, setup.recommended_chat_model))?.name ??
      setup.recommended_chat_model,
  );
</script>

<div class="wizard" data-tauri-drag-region>
  <div class="wizard-top"><LanguageMenu /></div>
  <div class="logo"><AppLogo size={64} /></div>
  <h1>Cloufam <span class="version-tag">v{__APP_VERSION__}</span></h1>
  <p class="tagline">{t('wizard.tagline')}</p>

  <div class="steps">
    {#each [t('wizard.step.start'), t('wizard.step.runtime'), t('wizard.step.models'), t('wizard.step.st'), t('wizard.step.done')] as label, i}
      <div class="step-item {i < stepIndex ? 'done' : i === stepIndex ? 'now' : ''}">
        <div class="step-dot"></div>
        <span>{label}</span>
      </div>
    {/each}
  </div>

  {#if step === 'intro'}
    <section class="card panel">
      <h2>{t('wizard.introReady')}</h2>
      <div class="facts">
        <div class="fact">
          <span class="fact-icon"><Icon name="gpu" size={20} /></span>
          <div>
            <b>
              {#if setup.gpu_name}
                {setup.gpu_name}
              {:else if setup.has_gpu}
                {t('wizard.gpuNvidia')}
              {:else}
                {t('wizard.noGpuTitle')}
              {/if}
            </b>
            <span class="dim">
              {#if setup.vram_mb}
                {t('wizard.vramRecommended', {
                  vram: (setup.vram_mb / 1024).toFixed(0),
                  model: recommendedName,
                })}
              {:else if setup.has_gpu}
                {t('wizard.chooseVram')}
              {:else}
                {t('wizard.cpuRecommended', { model: recommendedName })}
              {/if}
            </span>
          </div>
        </div>
        <div class="fact">
          <span class="fact-icon"><Icon name="disk" size={20} /></span>
          <div class="store-info">
            <b>{t('wizard.store.title')} · {t('wizard.needSpace')}</b>
            <span class="dim store-path" title={currentStore?.path ?? setup.models_root}>
              {currentStore?.path ?? setup.models_root}
            </span>
            <span class="dim">
              {t('wizard.freeGb', {
                gb: (currentStore?.free_gb ?? setup.free_space_gb) >= 0
                  ? (currentStore?.free_gb ?? setup.free_space_gb).toFixed(0)
                  : '…',
              })}{#if currentStore && currentStore.models > 0}{' · '}{t('wizard.store.models', {
                  count: currentStore.models,
                })}{/if}
            </span>
          </div>
          <button class="store-change" onclick={pickStore} disabled={storeBusy || busy}>
            {storeBusy ? t('wizard.store.busy') : t('wizard.store.change')}
          </button>
        </div>
        {#if foundStores.length > 0}
          <div class="found-stores">
            <p class="dim small">{t('wizard.store.found')}</p>
            {#each foundStores as store (store.path)}
              <div class="found-store">
                <span class="store-path" title={store.path}>{store.path}</span>
                <span class="dim small">{t('wizard.store.models', { count: store.models })}</span>
                <button onclick={() => useStore(store.path)} disabled={storeBusy || busy}>
                  {t('wizard.store.use')}
                </button>
              </div>
            {/each}
          </div>
        {/if}
        {#if storeError}
          <p class="err small">{storeError}</p>
        {/if}
        <div class="fact">
          <span class="fact-icon"><Icon name="sparkle" size={20} /></span>
          <div>
            <b>{t('wizard.featuresTitle')}</b>
            <span class="dim">{t('wizard.featuresText')}</span>
          </div>
        </div>
      </div>
      <div class="presets">
        <p class="dim">{t('wizard.chatForGpu')}</p>
        <div class="preset-grid">
          {#each chatOptions as option}
            <button
              class="preset {sameTag(appSettings.chat_model, option.tag) ? 'sel' : ''}"
              onclick={() => (appSettings.chat_model = option.tag)}
            >
              <div class="preset-title">
                <b>{option.name}</b>
                {#if sameTag(option.tag, setup.recommended_chat_model)}
                  <span class="badge-rec">{t('wizard.recommended')}</span>
                {/if}
              </div>
              <span class="dim">{option.vram} · {option.size}</span>
            </button>
          {/each}
        </div>
      </div>
      {#if error}
        <p class="err">{error}</p>
      {/if}
      <button class="primary big" onclick={startSetup} disabled={busy}>
        {busy ? t('wizard.starting') : t('wizard.start')}
      </button>
    </section>
  {:else if step === 'runtime'}
    <section class="card panel">
      <h2>{t('wizard.runtimeTitle')}</h2>
      <p class="dim">{t('wizard.runtimeText')}</p>
      <div class="progress-track"><div class="progress-fill" style:width="{pct(rtProgress)}%"></div></div>
      <p class="dim">{statusLabel(rtProgress.status)}{rtProgress.total ? ` · ${mb(rtProgress.completed)} / ${mb(rtProgress.total)}` : ''}</p>
      {#if busy && !error && runtimeDownloading}
        <div class="row">
          <button class="ghost" onclick={cancelDownloads} disabled={cancelling}>
            {cancelling ? t('wizard.cancelling') : t('wizard.cancelDownload')}
          </button>
        </div>
      {/if}
      {#if error}
        <p class="err">{error}</p>
        <div class="row">
          <button class="primary" onclick={startSetup} disabled={busy}>{t('wizard.retry')}</button>
          <button class="ghost" onclick={() => { error = null; step = 'intro'; }}>{t('wizard.back')}</button>
        </div>
      {/if}
    </section>
  {:else if step === 'models'}
    <section class="card panel">
      <h2>{t('wizard.modelsTitle')}</h2>
      <p class="dim">{t('wizard.modelsText')}</p>
      {#each MODELS as m (m.tag)}
        <div class="model-row">
          <div class="model-head">
            <span class="model-icon"><Icon name={m.icon} size={18} /></span>
            <div class="model-info">
              <b>{m.label}</b>
              <span class="dim">{m.note}</span>
            </div>
            {#if !m.required}
              {#if visionState === 'done'}
                <span class="badge ok"><span class="dot"></span>{t('wizard.statusReady')}</span>
              {:else if visionState === 'error'}
                <span class="badge err">{t('wizard.statusError')}</span>
              {:else if visionState === 'cancelled'}
                <span class="badge">{t('wizard.statusCancelled')}</span>
              {:else if visionState === 'downloading'}
                <span class="badge warn"><span class="spinner"></span>{t('wizard.statusRunning')}</span>
              {:else}
                <span class="badge">{t('wizard.statusBackground')}</span>
              {/if}
            {:else if isDone(m.tag)}
              <span class="badge ok"><span class="dot"></span>{t('wizard.statusReady')}</span>
            {:else if failedModels[m.tag] !== undefined}
              <span class="badge err">{t('wizard.statusError')}</span>
            {:else if modelProgress[m.tag]?.status === 'отменено'}
              <span class="badge">{t('wizard.statusCancelled')}</span>
            {:else if currentModel === m.tag}
              <span class="badge warn"><span class="spinner"></span>{t('wizard.statusRunning')}</span>
            {:else}
              <span class="badge">{t('wizard.statusQueued')}</span>
            {/if}
          </div>
          {#if modelProgress[m.tag]}
            <div class="progress-track">
              <div class="progress-fill" style:width="{isDone(m.tag) ? 100 : pct(modelProgress[m.tag])}%"></div>
            </div>
            <p class="dim small">{formatModelStatus(modelProgress[m.tag], isDone(m.tag))}</p>
          {/if}
          {#if failedModels[m.tag]}
            <p class="err small">{failedModels[m.tag]}</p>
          {:else if !m.required && visionState === 'error' && visionError}
            <p class="err small">{visionError}</p>
          {/if}
        </div>
      {/each}
      {#if busy && !error && currentModel}
        <div class="row">
          <button class="ghost" onclick={cancelDownloads} disabled={cancelling}>
            {cancelling ? t('wizard.cancelling') : t('wizard.cancelDownload')}
          </button>
        </div>
      {/if}
      {#if error}
        <p class="err">{error}</p>
        <div class="row">
          <button class="primary" onclick={downloadModels} disabled={busy}>{t('wizard.retry')}</button>
          <button class="ghost" onclick={() => { error = null; step = 'intro'; }}>{t('wizard.back')}</button>
        </div>
      {/if}
    </section>
  {:else if step === 'sillytavern'}
    <section class="card panel">
      <h2>{t('wizard.stTitle')}</h2>
      <p class="dim">
        {statusLabel(stProgress.status) || t('wizard.stText')}
        {stProgress.total ? ` · ${mb(stProgress.completed)} / ${mb(stProgress.total)}` : ''}
      </p>
      {#if stProgress.total}
        <div class="progress-track"><div class="progress-fill" style:width="{pct(stProgress)}%"></div></div>
      {/if}
      {#if error}
        <p class="err">{error}</p>
        <button class="primary" onclick={installSillytavern} disabled={busy}>{t('wizard.retry')}</button>
      {/if}
    </section>
  {:else if step === 'done'}
    <section class="card panel">
      <h2>{t('wizard.doneTitle2')}</h2>
      <p class="dim">
        {appSettings.auto_launch_st ? t('wizard.doneAuto') : t('wizard.doneManual')}
      </p>
    </section>
  {/if}
</div>

<style>
  /* Scrolling — on the whole window, otherwise the bar hangs mid-screen next to the column. */
  .wizard {
    position: relative;
    padding: 40px max(24px, calc((100% - 620px) / 2));
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 18px;
    height: 100vh;
    overflow-y: auto;
  }

  .wizard-top {
    position: fixed;
    top: 44px;
    right: 16px;
    z-index: 10;
  }

  .logo {
    color: var(--text);
    display: flex;
  }

  h1 {
    margin: 0;
    font-size: 26px;
    font-weight: 500;
    letter-spacing: -0.2px;
    display: flex;
    align-items: center;
    gap: 10px;
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

  .tagline {
    margin: -8px 0 0;
    color: var(--text-dim);
    font-size: 14px;
    text-align: center;
  }

  .steps {
    display: flex;
    align-items: flex-start;
    gap: 6px;
    width: 100%;
    max-width: 500px;
    margin: 6px 0 4px;
  }

  .step-item {
    flex: 1;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 7px;
    color: var(--text-faint);
    font-size: 12px;
    position: relative;
  }

  .step-item::before {
    content: '';
    position: absolute;
    top: 4px;
    left: calc(-50% + 10px);
    right: calc(50% + 10px);
    height: 1px;
    background: var(--border-strong);
  }

  .step-item:first-child::before {
    display: none;
  }

  .step-item.done::before {
    background: var(--text-dim);
  }

  .step-item.done {
    color: var(--text-dim);
  }

  .step-item.now {
    color: var(--text);
  }

  .step-dot {
    width: 9px;
    height: 9px;
    border-radius: 50%;
    background: var(--surface-3);
    z-index: 1;
    transition: background 0.2s, box-shadow 0.2s;
  }

  .step-item.done .step-dot {
    background: var(--text-dim);
  }

  .step-item.now .step-dot {
    background: var(--primary);
    box-shadow: 0 0 0 4px var(--focus-ring);
  }

  /* Layout; the material is set by the shared .card in app.css. */
  .panel {
    width: 100%;
    padding: 24px;
    display: flex;
    flex-direction: column;
    gap: 16px;
    animation: fadeUp 0.25s ease;
  }

  h2 {
    margin: 0;
    font-size: 17px;
    font-weight: 500;
  }

  .facts {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .fact,
  .model-row {
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 12px;
    padding: 12px 14px;
  }

  .fact {
    display: flex;
    gap: 14px;
    align-items: center;
  }

  .fact-icon,
  .model-icon {
    width: 36px;
    height: 36px;
    flex: none;
    display: flex;
    align-items: center;
    justify-content: center;
    color: var(--text-dim);
    background: var(--surface-2);
    border-radius: 10px;
  }

  .fact div {
    display: flex;
    flex-direction: column;
    gap: 3px;
  }

  .store-info {
    flex: 1;
    min-width: 0;
  }

  .store-path {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .store-change {
    flex: none;
  }

  .found-stores {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 10px 14px;
    border: 1px dashed var(--border-strong);
    border-radius: 12px;
  }

  .found-store {
    display: flex;
    align-items: center;
    gap: 10px;
  }

  .found-store .store-path {
    flex: 1;
    min-width: 0;
  }

  .presets {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .preset-grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 8px;
  }

  .preset {
    display: flex;
    flex-direction: column;
    gap: 3px;
    align-items: flex-start;
    text-align: left;
    padding: 11px 14px;
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 12px;
    color: var(--text);
  }

  .preset:hover {
    background: var(--surface-2);
  }

  .preset.sel {
    background: var(--surface-3);
    border-color: var(--border-strong);
    box-shadow: var(--btn-bevel);
  }

  .preset-title {
    display: flex;
    align-items: center;
    justify-content: space-between;
    width: 100%;
    gap: 8px;
  }

  .badge-rec {
    font-size: 11px;
    font-weight: 500;
    color: var(--ok);
    background: var(--ok-soft);
    padding: 1px 8px;
    border-radius: var(--radius-pill);
  }

  .big {
    padding: 12px;
    font-size: 15px;
  }

  .model-row {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }

  .model-head {
    display: flex;
    gap: 12px;
    align-items: center;
  }

  .model-info {
    flex: 1;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .row {
    display: flex;
    gap: 10px;
    flex-wrap: wrap;
  }

  .dim {
    color: var(--text-dim);
    margin: 0;
  }

  .small {
    font-size: 12px;
  }

  .err {
    color: var(--err);
    margin: 0;
  }
</style>

