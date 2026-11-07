<script lang="ts">
  import { listen } from '@tauri-apps/api/event';
  import { untrack } from 'svelte';
  import { api } from './lib/api';
  import { clampNumber } from './lib/numbers';
  import { i18n, t } from './lib/i18n.svelte';
  import type { DatasetInfo, HardwareTier, TrainPreset, TrainerInfo, TrainingState } from './lib/types';

  let { hardwareTier = 'high' }: { hardwareTier?: HardwareTier } = $props();

  const tierOrder: HardwareTier[] = ['minimal', 'low', 'mid', 'high'];

  let info = $state<TrainerInfo | null>(null);
  let presets = $state<TrainPreset[]>([]);
  let datasets = $state<DatasetInfo[]>([]);
  let train = $state<TrainingState | null>(null);
  let busy = $state(false);
  let message = $state('');
  let error = $state('');

  let importPath = $state('');
  let presetId = $state('');
  let datasetName = $state('');
  let epochs = $state(3);
  let maxSeq = $state(0);
  let exportName = $state('');
  let createdModel = $state('');

  let poll: ReturnType<typeof setInterval> | null = null;
  let secondsPerStep = $state(0);
  let presetsLoaded = $state(false);

  function defaultSeq(tier: HardwareTier): number {
    if (tier === 'high') return 2048;
    if (tier === 'mid') return 1024;
    return 512;
  }

  const available = $derived(
    presets.filter((p: TrainPreset) => tierOrder.indexOf(p.min_tier) <= tierOrder.indexOf(hardwareTier)),
  );
  const selected = $derived(available.find((p: TrainPreset) => p.id === presetId) ?? null);
  const dataset = $derived(datasets.find((d: DatasetInfo) => d.name === datasetName) ?? null);

  // The commands receive clamped field values: the ranges match the field and
  // training_start (epochs 1..20); the length is an integer for Option<u32>.
  const epochsValue = $derived(clampNumber(epochs, 1, 20, 3));
  const seqValue = $derived(maxSeq > 0 ? Math.round(clampNumber(maxSeq, 256, 4096, 256)) : null);

  // Step divisor per pass — batch × grad_accum from trainer::tier_profile
  // (currently 1×8 on all tiers). The backend does not expose it yet: when
  // trainer_status starts returning step_divisor (the contract is code-review report
  // V-50), the value will come from there, and the steps and the "≈time" estimate will stop
  // drifting when the profile is edited. Verify the fallback number against tier_profile.
  const STEP_DIVISOR_FALLBACK = 8;
  const stepDivisor = $derived(
    info && typeof info.step_divisor === 'number' && info.step_divisor > 0
      ? info.step_divisor
      : STEP_DIVISOR_FALLBACK,
  );

  const steps = $derived.by((): number => {
    if (!dataset || !selected) return 0;
    const perEpoch = Math.ceil(dataset.examples / stepDivisor);
    return Math.max(1, Math.round(perEpoch * epochsValue));
  });

  const estimate = $derived.by((): string => {
    if (!selected || secondsPerStep <= 0) return '';
    const total = secondsPerStep * Math.max(1, steps);
    if (total < 90) return `≈ ${Math.round(total)} ${t('unit.s')}`;
    if (total < 5400) return `≈ ${Math.round(total / 60)} ${t('unit.min')}`;
    return `≈ ${(total / 3600).toFixed(1)} ${t('unit.h')}`;
  });

  // The estimate formula lives in Rust (trainer.rs): here it is only formatted into text.
  // The generation token keeps a stale .then from overwriting a fresh estimate.
  let estimateGen = 0;
  $effect(() => {
    const id = presetId;
    const seq = seqValue;
    if (!id) return;
    const gen = ++estimateGen;
    api
      .trainingEstimate(id, seq)
      .then((value) => {
        if (gen === estimateGen) secondsPerStep = value;
      })
      .catch(() => {
        if (gen === estimateGen) secondsPerStep = 0;
      });
  });

  function formatSeconds(value: number | null): string {
    if (value === null) return '—';
    if (value < 90) return `${Math.round(value)} ${t('unit.s')}`;
    if (value < 5400) return `${Math.round(value / 60)} ${t('unit.min')}`;
    return `${(value / 3600).toFixed(1)} ${t('unit.h')}`;
  }

  async function refresh() {
    // Presets before the runtime status: the latter computes torch versions and directory size
    // and answers in seconds on a cold start, while the base list is needed immediately.
    if (!presetsLoaded) {
      presets = await api.trainingPresets();
      presetsLoaded = true;
    }
    datasets = await api.trainingDatasets();
    train = await api.trainingState();
    info = await api.trainerStatus();
    if (!datasetName && datasets.length > 0) datasetName = datasets[0].name;
  }

  // The preset catalog arrives asynchronously, while the hardware tier is a prop that
  // changes after setup loads. We set the default choice reactively: a one-off
  // check inside refresh() fired before the data arrived, and the start button
  // stayed disabled until the base was picked manually. We take the largest
  // of the recommended ones, as in the model showcase.
  $effect(() => {
    if (!presetId && available.length > 0) {
      const recommended = available.filter((p: TrainPreset) => p.recommended);
      const pick = recommended.length > 0 ? recommended[recommended.length - 1] : available[available.length - 1];
      presetId = pick.id;
    }
  });

  $effect(() => {
    // refresh() reads presetsLoaded before the first await: without untrack
    // the effect re-ran after presets loaded and recreated the poll and subscription.
    untrack(refresh).catch((e) => (error = String(e)));
    poll = setInterval(() => {
      api.trainingState().then((s) => (train = s)).catch(() => {});
      api.trainerStatus().then((i) => (info = i)).catch(() => {});
    }, 2000);
    const unlisten = listen('trainer-progress', () => {
      api.trainerStatus().then((i) => (info = i)).catch(() => {});
      api.trainingState().then((s) => (train = s)).catch(() => {});
    });
    return () => {
      if (poll) clearInterval(poll);
      unlisten.then((off) => off()).catch(() => {});
    };
  });

  async function installRuntime() {
    busy = true;
    error = '';
    message = t('tr.installStarted');
    try {
      await api.trainerInstall();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  async function importDataset() {
    busy = true;
    error = '';
    try {
      const result = await api.trainingImport(importPath.trim());
      message = t('tr.imported', { examples: result.examples, mode: result.mode, tokens: result.approx_tokens.toLocaleString(i18n.locale === 'ru' ? 'ru-RU' : 'en-US') });
      importPath = '';
      datasets = await api.trainingDatasets();
      datasetName = result.name;
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  async function startTraining() {
    busy = true;
    error = '';
    message = '';
    createdModel = '';
    try {
      await api.trainingStart(presetId, datasetName, epochsValue, seqValue);
      message = t('tr.started');
      train = await api.trainingState();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  async function cancelTraining() {
    try {
      await api.trainingCancel();
    } catch (e) {
      error = String(e);
    } finally {
      train = await api.trainingState().catch(() => train);
    }
  }

  async function clearFinished() {
    try {
      await api.trainingClear();
      train = await api.trainingState();
    } catch (e) {
      error = String(e);
    }
  }

  async function exportModel() {
    if (!train?.adapter_dir || !train.preset_id) return;
    busy = true;
    error = '';
    try {
      createdModel = await api.trainingExport(train.preset_id, train.adapter_dir, exportName || t('tr.defaultName'));
      message = t('tr.created', { model: createdModel });
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }
</script>

<div class="train">
  {#if !info}
    <div class="hint">{t('tr.checking')}</div>
  {:else if info && !info.available}
    <div class="unavailable">
      <b>{t('tr.unavailable')}</b>
      <p>{info.reason}</p>
      <p class="hint">{t('tr.unavailableHint')}</p>
    </div>
  {:else}
    <section class="block">
      <h3>{t('tr.s1')}</h3>
      {#if info?.runtime.installed}
        <p class="ok">
          {t('tr.ready', { python: info.runtime.python ?? '', torch: info.runtime.torch ?? '', llama: info.runtime.llama_cpp ? t('tr.llamaPresent') : t('tr.llamaMissing'), size: info.runtime.size_mb })}
        </p>
      {:else if info?.runtime.installing}
        <p>
          {info.runtime.stage ?? t('tr.installing')}
          {#if info.runtime.progress !== null}· {info.runtime.progress}%{/if}
        </p>
        {#if info.runtime.progress !== null}
          <div class="bar"><div class="bar-fill" style="width: {info.runtime.progress}%"></div></div>
        {/if}
        <button class="btn" onclick={() => api.trainerCancel()}>{t('tr.cancelInstall')}</button>
      {:else}
        <p class="hint">
          {t('tr.installHint')}
        </p>
        {#if info?.runtime.error}<p class="err">{info.runtime.error}</p>{/if}
        <button class="btn primary" disabled={busy} onclick={installRuntime}>{t('tr.install')}</button>
      {/if}
    </section>

    <section class="block">
      <h3>{t('tr.s2')}</h3>
      <p class="hint">
        {t('tr.textsHint')}
      </p>
      <div class="row">
        <input type="text" placeholder={t('tr.pathPlaceholder')} bind:value={importPath} />
        <button class="btn" disabled={busy || !importPath.trim()} onclick={importDataset}>{t('tr.import')}</button>
        <button class="btn ghost" onclick={() => api.trainingOpenImportFolder()}>{t('tr.openImport')}</button>
      </div>
      {#if datasets.length > 0}
        <table>
          <thead><tr><th>{t('tr.colDataset')}</th><th>{t('tr.colExamples')}</th><th>{t('tr.colMode')}</th><th>{t('tr.colTokens')}</th></tr></thead>
          <tbody>
            {#each datasets as d}
              <tr class:selected={d.name === datasetName} onclick={() => (datasetName = d.name)}>
                <td>{d.name}</td>
                <td>{d.examples}</td>
                <td>{d.mode === 'chat' ? t('tr.modeChat') : t('tr.modeText')}</td>
                <td>{d.approx_tokens.toLocaleString('ru-RU')}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/if}
    </section>

    <section class="block">
      <h3>{t('tr.s3')}</h3>
      <div class="presets">
        {#each available as p}
          <label class="preset" class:active={p.id === presetId}>
            <input type="radio" name="preset" value={p.id} bind:group={presetId} />
            <div>
              <b>{p.name}</b>{#if p.recommended}<span class="tag">{t('tr.recommended')}</span>{/if}
              <div class="params">
                {t('tr.presetParams', { params: p.params_b, base: p.base_model.split('/')[1], gb: p.download_gb.toFixed(1), tag: p.ollama_tag })}
              </div>
              <div class="hint">{p.description}</div>
            </div>
          </label>
        {/each}
      </div>
      {#if available.length === 0}
        <p class="hint">{presetsLoaded ? t('tr.noBases') : t('tr.loadingBases')}</p>
      {/if}

      <div class="row">
        <label>{t('tr.epochs')} <input type="number" min="1" max="20" bind:value={epochs} /></label>
        <label>
          {t('tr.seqLen')}
          <input type="number" min="256" max="4096" step="256" bind:value={maxSeq} placeholder={String(defaultSeq(hardwareTier))} />
        </label>
        {#if selected && dataset}
          <span class="summary">
            {t('tr.summary', { steps, estimate, gb: (selected.download_gb + 3).toFixed(0) })}
          </span>
        {/if}
      </div>

      <div class="row">
        {#if train?.running}
          <button class="btn danger" onclick={cancelTraining}>{t('tr.stop')}</button>
        {:else}
          <button class="btn primary" disabled={busy || !selected || !dataset || !info?.runtime.installed} onclick={startTraining}>
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
          {#if train.running}{t('tr.left', { time: formatSeconds(train.eta_seconds) })}{/if}
          {t('tr.elapsed', { time: formatSeconds(train.elapsed_seconds) })}
        </p>
        {#if train.total > 0}
          <div class="bar"><div class="bar-fill" style="width: {Math.min(100, Math.round((train.step / train.total) * 100))}%"></div></div>
        {/if}
        {#if train.error}<p class="err">{train.error}</p>{/if}
        {#if train.log_tail.length > 0}
          <pre>{train.log_tail.slice(-12).join('\n')}</pre>
        {/if}
      </section>
    {/if}

    {#if train && !train.running && train.finished && train.adapter_dir}
      <section class="block">
        <h3>{t('tr.s5')}</h3>
        <p class="hint">
          {t('tr.exportHint')}
        </p>
        <div class="row">
          <input type="text" placeholder={t('tr.namePlaceholder')} bind:value={exportName} />
          <button class="btn primary" disabled={busy} onclick={exportModel}>{t('tr.create')}</button>
        </div>
        {#if createdModel}
          <p class="ok">{t('tr.done')} <code>{createdModel}</code> {t('tr.doneHint')}</p>
        {/if}
      </section>
    {/if}

    {#if message}<p class="ok">{message}</p>{/if}
    {#if error}<p class="err">{error}</p>{/if}
  {/if}
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
    min-width: 320px;
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
  .btn.danger:hover:not(:disabled) {
    background: #c95560;
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
    padding: 18px 20px;
    box-shadow: none;
  }
</style>

