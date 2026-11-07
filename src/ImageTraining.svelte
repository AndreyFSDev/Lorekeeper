<script lang="ts">
  import { listen } from '@tauri-apps/api/event';
  import { untrack } from 'svelte';
  import { api } from './lib/api';
  import { clampNumber } from './lib/numbers';
  import { t } from './lib/i18n.svelte';
  import type {
    HardwareTier,
    ImageDatasetInfo,
    ImageTrainPreset,
    ImageTrainingState,
  } from './lib/types';

  let { hardwareTier = 'high' }: { hardwareTier?: HardwareTier } = $props();

  const tierOrder: HardwareTier[] = ['minimal', 'low', 'mid', 'high'];

  let presets = $state<ImageTrainPreset[]>([]);
  let datasets = $state<ImageDatasetInfo[]>([]);
  let train = $state<ImageTrainingState | null>(null);
  let busy = $state(false);
  let message = $state('');
  let error = $state('');

  let importPath = $state('');
  let presetId = $state('');
  let datasetSlug = $state('');
  let repeats = $state(10);
  let loraName = $state('');
  let estimatedSeconds = $state(0);
  let presetsLoaded = $state(false);

  let poll: ReturnType<typeof setInterval> | null = null;

  const available = $derived(
    presets.filter((p: ImageTrainPreset) => tierOrder.indexOf(p.min_tier) <= tierOrder.indexOf(hardwareTier)),
  );
  const selected = $derived(available.find((p: ImageTrainPreset) => p.id === presetId) ?? null);
  const dataset = $derived(datasets.find((d: ImageDatasetInfo) => d.slug === datasetSlug) ?? null);
  // An empty field is null while image_training_start expects u32: the summary and the command
  // use the clamped value (the range is REPEAT_RANGE in image_trainer.rs).
  const repeatsValue = $derived(Math.round(clampNumber(repeats, 1, 100, 10)));
  const steps = $derived(dataset ? dataset.images * repeatsValue : 0);

  const estimate = $derived(estimatedSeconds > 0 ? formatSeconds(estimatedSeconds) : '');

  // The estimate formula lives in Rust (image_trainer.rs) — here it is only formatted into text.
  // The estimate re-reads headers of every checkpoint in the store, so it is not called on
  // every keystroke in the "repeats" field, but after an input pause and only with the clamped
  // valid value (repeatsValue). The generation token keeps a stale .then
  // from overwriting a fresh estimate.
  let estimateGen = 0;
  $effect(() => {
    const id = presetId;
    const count = steps;
    if (!id || !Number.isInteger(count) || count <= 0) {
      estimatedSeconds = 0;
      return;
    }
    const gen = ++estimateGen;
    const timer = setTimeout(() => {
      api
        .imageTrainingEstimate(id, count)
        .then((value) => {
          if (gen === estimateGen) estimatedSeconds = value;
        })
        .catch(() => {
          if (gen === estimateGen) estimatedSeconds = 0;
        });
    }, 400);
    return () => clearTimeout(timer);
  });

  // Presets arrive asynchronously, the tier is a prop: we set the default choice
  // reactively, otherwise the start button stays dead until the base is picked manually.
  $effect(() => {
    if (!presetId && available.length > 0) {
      const recommended = available.filter((p: ImageTrainPreset) => p.recommended);
      presetId = (recommended[0] ?? available[available.length - 1]).id;
    }
  });

  function formatSeconds(value: number | null): string {
    if (value === null) return '—';
    if (value < 90) return `${Math.round(value)} ${t('unit.s')}`;
    if (value < 5400) return `${Math.round(value / 60)} ${t('unit.min')}`;
    return `${(value / 3600).toFixed(1)} ${t('unit.h')}`;
  }

  async function refresh() {
    // The base list comes before state: state recomputes environment readiness.
    if (!presetsLoaded) {
      presets = await api.imageTrainingPresets();
      presetsLoaded = true;
    }
    datasets = await api.imageTrainingDatasets();
    train = await api.imageTrainingState();
    if (!datasetSlug && datasets.length > 0) datasetSlug = datasets[0].slug;
  }

  $effect(() => {
    // refresh() reads presetsLoaded before the first await: without untrack the effect
    // re-ran after presets loaded and recreated the poll and subscription.
    untrack(refresh).catch((e) => (error = String(e)));
    poll = setInterval(() => {
      api.imageTrainingState().then((s) => (train = s)).catch(() => {});
    }, 2000);
    const unlisten = listen('image-training-progress', () => {
      api.imageTrainingState().then((s) => (train = s)).catch(() => {});
    });
    return () => {
      if (poll) clearInterval(poll);
      unlisten.then((off) => off()).catch(() => {});
    };
  });

  async function importDataset() {
    busy = true;
    error = '';
    try {
      const result = await api.imageTrainingImport(importPath.trim());
      message = t('it.imported', { images: result.images, captions: result.captions });
      importPath = '';
      datasets = await api.imageTrainingDatasets();
      datasetSlug = result.slug;
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  async function captionDataset() {
    if (!datasetSlug) return;
    busy = true;
    error = '';
    message = t('it.captioning');
    try {
      message = await api.imageTrainingCaption(datasetSlug);
      datasets = await api.imageTrainingDatasets();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
      api.imageTrainingState().then((s) => (train = s)).catch(() => {});
    }
  }

  async function startTraining() {
    busy = true;
    error = '';
    message = '';
    try {
      await api.imageTrainingStart(presetId, datasetSlug, repeatsValue, loraName || t('it.defaultName'));
      message = t('tr.started');
      train = await api.imageTrainingState();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  async function cancelTraining() {
    try {
      await api.imageTrainingCancel();
    } catch (e) {
      error = String(e);
    } finally {
      train = await api.imageTrainingState().catch(() => train);
    }
  }

  async function clearFinished() {
    try {
      await api.imageTrainingClear();
      train = await api.imageTrainingState();
    } catch (e) {
      error = String(e);
    }
  }
</script>

<div class="train">
  <section class="block">
    <h3>{t('it.s1')}</h3>
    {#if train?.runtime.reason}
      <div class="unavailable">
        <b>{t('it.unavailable')}</b>
        <p>{train.runtime.reason}</p>
      </div>
    {/if}
    <p class="hint">
      {t('it.stackHint', { venv: 'runtime\\trainer\\venv-sd', scripts: 'runtime\\trainer\\sd-scripts' })}
    </p>
    <ul class="checks">
      <li class:ok={train?.runtime.cuda}>{train?.runtime.cuda ? '✅' : '⛔'} {t('it.cuda')}</li>
      <li class:ok={train?.runtime.venv}>{train?.runtime.venv ? '✅' : '⛔'} {t('it.venv')}</li>
      <li class:ok={train?.runtime.sd_scripts}>{train?.runtime.sd_scripts ? '✅' : '⛔'} {t('it.sdScripts')}</li>
      <li class:ok={(train?.runtime.checkpoints ?? 0) > 0}>
        {train?.runtime.checkpoints ? '✅' : '⛔'} {t('it.checkpoints', { count: train?.runtime.checkpoints ?? 0 })}
      </li>
      <li class:ok={train?.runtime.tagger}>
        {train?.runtime.tagger ? '✅' : '⚠️'} {t('it.tagger')}
      </li>
    </ul>
  </section>

  <section class="block">
    <h3>{t('it.s2')}</h3>
    <p class="hint">
      {t('it.importHint', { txt: '.txt' })}
    </p>
    <div class="row">
      <input type="text" placeholder={t('it.pathPlaceholder')} bind:value={importPath} />
      <button class="btn" disabled={busy || !importPath.trim()} onclick={importDataset}>{t('tr.import')}</button>
      <button class="btn ghost" onclick={() => api.imageTrainingOpenFolder('datasets')}>{t('it.openDatasets')}</button>
    </div>

    {#if datasets.length > 0}
      <table>
        <thead><tr><th>{t('tr.colDataset')}</th><th>{t('it.colImages')}</th><th>{t('it.colCaptioned')}</th><th>{t('it.colSteps')}</th></tr></thead>
        <tbody>
          {#each datasets as d}
            <tr class:selected={d.slug === datasetSlug} onclick={() => (datasetSlug = d.slug)}>
              <td>{d.slug}</td>
              <td>{d.images}</td>
              <td class:warn={d.captions < d.images}>{d.captions}</td>
              <td>{d.images * repeatsValue}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    {/if}

    {#if dataset && dataset.captions < dataset.images}
      <p class="hint">
        {t('it.uncaptioned', { count: dataset.images - dataset.captions, txt: '.txt' })}
      </p>
    {/if}

    <div class="row">
      <button
        class="btn"
        disabled={busy || !datasetSlug || !train?.runtime.tagger || train?.caption.running}
        onclick={captionDataset}
      >
        {t('it.caption')}
      </button>
      {#if !train?.runtime.tagger}
        <span class="hint">
          {t('it.noOnnx', { txt: '.txt' })}
        </span>
      {/if}
    </div>

    {#if train?.caption.running}
      <p>
        {train.caption.stage} · {t('it.of', { done: train.caption.done, total: train.caption.total })}
      </p>
      <div class="bar">
        <div
          class="bar-fill"
          style="width: {Math.min(100, Math.round((train.caption.done / Math.max(1, train.caption.total)) * 100))}%"
        ></div>
      </div>
    {/if}
  </section>

  <section class="block">
    <h3>{t('it.s3')}</h3>
    {#if available.length > 0}
      <div class="presets">
        {#each available as p}
          <label class="preset" class:active={p.id === presetId}>
            <input type="radio" name="image-preset" value={p.id} bind:group={presetId} />
            <div>
              <b>{p.name}</b>
              <span class="tag">{p.family_label}</span>
              {#if p.recommended}<span class="tag">{t('it.forGen')}</span>{/if}
              <div class="params">
                {p.resolution}px · {p.size_gb.toFixed(1)} {t('unit.gb')} · {t('it.unetOnly')} · {p.file_name}
              </div>
              <div class="hint">{p.description}</div>
            </div>
          </label>
        {/each}
      </div>
    {:else}
      <p class="hint">
        {presetsLoaded ? t('it.noCkpt') : t('it.loadingCkpt')}
      </p>
    {/if}

    <div class="row">
      <label>{t('it.repeats')} <input type="number" min="1" max="100" bind:value={repeats} /></label>
      <label>{t('it.loraName')} <input type="text" placeholder={t('it.defaultName')} bind:value={loraName} /></label>
    </div>
    {#if selected && dataset}
      <p class="summary">
        {t('it.summary', { images: dataset.images, repeats: repeatsValue, steps, estimate })}
      </p>
      <p class="hint">
        {t('it.resHint')}
      </p>
    {/if}

    <div class="row">
      {#if train?.running}
        <button class="btn danger" onclick={cancelTraining}>{t('tr.stop')}</button>
      {:else}
        <button
          class="btn primary"
          disabled={busy || !selected || !dataset || !!train?.runtime.reason}
          onclick={startTraining}
        >
          {t('tr.start')}
        </button>
      {/if}
      {#if train && !train.running && (train.finished || train.error || train.step > 0)}
        <button class="btn ghost" onclick={clearFinished}>{t('tr.reset')}</button>
      {/if}
    </div>
  </section>

  {#if train && (train.running || train.step > 0 || train.error)}
    <section class="block">
      <h3>{t('tr.s4')}</h3>
      <p>
        {train.stage}{#if train.running}{t('tr.step', { step: train.step, total: train.total })}{/if}
        {#if train.loss !== null}· loss {train.loss.toFixed(3)}{/if}
        {#if train.lr !== null}· lr {train.lr}{/if}
        {#if train.speed !== null}· {train.speed.toFixed(2)} {t('it.secPerStep')}{/if}
        {#if train.running}{t('tr.left', { time: formatSeconds(train.eta_seconds) })}{/if}
        {t('tr.elapsed', { time: formatSeconds(train.elapsed_seconds) })}
      </p>
      {#if train.total > 0}
        <div class="bar">
          <div class="bar-fill" style="width: {Math.min(100, Math.round((train.step / train.total) * 100))}%"></div>
        </div>
      {/if}
      {#if train.images > 0}
        <p class="hint">
          {t('it.datasetInfo', { images: train.images, captions: train.captions })}
        </p>
      {/if}      {#if train.error}<p class="err">{train.error}</p>{/if}
      {#if train.log_tail.length > 0}
        <pre>{train.log_tail.slice(-12).join('\n')}</pre>
      {/if}
    </section>
  {/if}

  {#if train && !train.running && train.finished && train.lora_path}
    <section class="block">
      <h3>{t('it.s5')}</h3>
      <p class="ok"><code>{train.lora_path}</code></p>
      <p class="hint">
        {t('it.doneHint', { node: 'LoraLoader' })}
      </p>
      <div class="row">
        <button class="btn" onclick={() => api.imageTrainingOpenFolder('loras')}>{t('it.openLoras')}</button>
      </div>
    </section>
  {/if}

  {#if message}<p class="ok">{message}</p>{/if}
  {#if error}<p class="err">{error}</p>{/if}
</div>

<style>
  .train {
    display: flex;
    flex-direction: column;
    gap: 18px;
  }
  .block {
    background: var(--bg-panel);
    backdrop-filter: none;
    border: 1px solid var(--border);
    border-radius: var(--radius);
    padding: 18px 20px;
    box-shadow: none;
    transition: border-color 0.2s, box-shadow 0.2s;
  }
  .block:hover {
    border-color: var(--border-strong);
  }
  .block h3 {
    margin: 0 0 10px;
    font-size: 15px;
    font-weight: 700;
    color: var(--text);
    letter-spacing: 0.2px;
  }
  .row {
    display: flex;
    flex-wrap: wrap;
    gap: 10px;
    align-items: center;
    margin: 10px 0;
  }
  input[type='text'],
  input[type='number'] {
    background: var(--bg-input);
    backdrop-filter: none;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    color: var(--text);
    padding: 8px 12px;
    font: inherit;
    font-size: 13.5px;
    transition: border-color 0.2s, box-shadow 0.2s;
  }
  input[type='text']:focus,
  input[type='number']:focus {
    outline: none;
    box-shadow: none;
  }
  input[type='text'] {
    min-width: 300px;
    flex: 1;
  }
  input[type='number'] {
    width: 90px;
  }
  .btn {
    background: var(--btn-fill);
    box-shadow: var(--btn-shadow);
    backdrop-filter: none;
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-sm);
    color: var(--text);
    padding: 8px 16px;
    cursor: pointer;
    font: inherit;
    font-size: 13.5px;
    font-weight: 600;
    user-select: none;
    -webkit-user-select: none;
    transition: background 0.2s, border-color 0.2s, transform 0.08s, box-shadow 0.2s;
  }
  .btn:hover:not(:disabled) {
    background: var(--btn-fill-hover);
    border-color: var(--border-strong);
  }
  .btn:active:not(:disabled) {
    transform: scale(0.98);
  }
  .btn:disabled {
    opacity: 0.45;
    cursor: not-allowed;
    transform: none;
    box-shadow: none;
  }
  .btn.primary {
    background: var(--primary-fill);
    border-color: transparent;
    color: var(--on-primary);
    box-shadow: var(--primary-shadow);
  }
  .btn.primary:hover:not(:disabled) {
    background: var(--primary-hover);
    box-shadow: var(--primary-shadow);
  }
  .btn.primary:disabled {
    opacity: 1;
    background: var(--surface-3);
    border-color: var(--border);
    color: var(--text-faint);
    box-shadow: none;
  }
  .btn.danger {
    background: #dc6973;
    border-color: transparent;
    color: #ffffff;
    box-shadow: none;
  }
  .btn.ghost {
    background: transparent;
    border-color: var(--border);
    color: var(--text-dim);
  }
  .btn.ghost:hover:not(:disabled) {
    color: var(--text);
    background: rgba(255, 255, 255, 0.05);
    border-color: var(--border-strong);
  }
  .checks {
    list-style: none;
    margin: 8px 0 0;
    padding: 0;
    display: flex;
    flex-wrap: wrap;
    gap: 6px 18px;
    font-size: 13px;
    color: var(--text-dim);
  }
  .checks li.ok {
    color: var(--text);
  }
  .presets {
    display: flex;
    flex-direction: column;
    gap: 8px;
    margin: 12px 0;
  }
  .preset {
    display: flex;
    gap: 12px;
    align-items: flex-start;
    background: var(--bg-input);
    backdrop-filter: none;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    padding: 12px 14px;
    cursor: pointer;
    transition: border-color 0.2s, background 0.2s, box-shadow 0.2s, transform 0.1s;
  }
  .preset:hover {
    border-color: var(--border-strong);
    background: var(--surface-2);
  }
  .preset.active {
    border-color: var(--border-strong);
    background: var(--surface-3);
    box-shadow: var(--btn-bevel);
  }
  .params {
    font-size: 12px;
    color: var(--text-dim);
    margin-top: 3px;
    word-break: break-word;
  }
  .tag {
    margin-left: 8px;
    font-size: 11px;
    font-weight: 600;
    padding: 2px 8px;
    border-radius: 999px;
    background: rgba(255, 255, 255, 0.07);
    border: 1px solid var(--border-strong);
    color: var(--accent2);
    letter-spacing: 0.2px;
  }
  .hint {
    font-size: 12.5px;
    color: var(--text-dim);
    margin: 4px 0;
    line-height: 1.45;
  }
  .ok {
    color: var(--ok);
    font-weight: 500;
  }
  .err {
    color: var(--err);
    font-weight: 500;
  }
  .warn {
    color: #d57141;
  }
  .summary {
    font-size: 12.5px;
    color: var(--text-dim);
  }
  .bar {
    height: 8px;
    border-radius: 999px;
    background: var(--surface-2);
    border: 1px solid var(--border);
    overflow: hidden;
    margin: 10px 0;
  }
  .bar-fill {
    height: 100%;
    background: linear-gradient(90deg, var(--accent), var(--accent2), var(--accent));
    background-size: 200% 100%;
    border-radius: 999px;
    transition: width 0.3s ease;
    animation: shimmer 2s linear infinite;
  }
  @keyframes shimmer {
    0% { background-position: 200% 0; }
    100% { background-position: -200% 0; }
  }
  table {
    width: 100%;
    border-collapse: collapse;
    font-size: 13px;
    margin-top: 10px;
    border-radius: var(--radius-sm);
    overflow: hidden;
    border: 1px solid var(--border);
  }
  th,
  td {
    text-align: left;
    padding: 8px 12px;
    border-bottom: 1px solid var(--border-strong);
  }
  th {
    background: var(--surface-2);
    color: var(--text-dim);
    font-size: 11.5px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.4px;
  }
  tbody tr {
    cursor: pointer;
    transition: background 0.15s;
  }
  tbody tr:hover:not(.selected) {
    background: rgba(255, 255, 255, 0.04);
  }
  tr.selected {
    background: var(--accent-soft);
    color: var(--text);
  }
  code {
    font-family: 'Cascadia Code', Consolas, monospace;
    font-size: 12px;
    word-break: break-all;
  }
  pre {
    max-height: 190px;
    overflow: auto;
    font-size: 11.5px;
    line-height: 1.45;
    background: var(--surface-2);
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    padding: 10px 12px;
    margin: 10px 0 0;
    color: var(--text);
    font-family: 'Cascadia Code', Consolas, monospace;
    box-shadow: inset 0 2px 8px rgba(0, 0, 0, 0.4);
  }
  .unavailable {
    border: 1px solid rgba(251, 191, 36, 0.35);
    background: rgba(251, 191, 36, 0.08);
    backdrop-filter: none;
    border-radius: var(--radius);
    padding: 14px 16px;
    margin-bottom: 10px;
    box-shadow: none;
  }
</style>
