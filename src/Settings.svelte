<script lang="ts">
  import { onMount, onDestroy, untrack } from 'svelte';
  import { listen } from '@tauri-apps/api/event';
  import { api } from './lib/api';
  import { i18n, t, type I18nKey } from './lib/i18n.svelte';
  import AppLogo from './lib/AppLogo.svelte';
  import Icon from './lib/Icon.svelte';
  import LanguageMenu from './lib/LanguageMenu.svelte';
  import { isModelInstalled, isLikelyChatModel, sameTag } from './lib/models';
  import { clampNumber } from './lib/numbers';
  import Training from './Training.svelte';
  import ImageTraining from './ImageTraining.svelte';
  import type { Bond, CatalogModel, ComfyStatus, DownloadTask, DownloadsSnapshot, ImageModelInfo, RecommendReport, RecommendedDownload, Settings, SetupState, TraitCharacter, VisionDownloadResult } from './lib/types';


  const numberLocale = $derived(i18n.locale === 'ru' ? 'ru-RU' : 'en-US');

  // The canonical tab list lives in the gateway (gateway.rs::SETTINGS_TABS) and
  // arrives ONLY through the IPC command `settings_tabs`: it also picks the tab
  // with which SillyTavern asks to open the window. There is no copy of the list here —
  // uniqueness is guarded by the test `settings_tabs_live_only_in_the_gateway`
  // (gateway.rs).
  type Tab = string;
  /** Allowlist of ids from the gateway; empty until the command replies. */
  let gatewayTabs: string[] = $state([]);
  /** Honest error: the tab list was not received — do not validate tabs. */
  let tabsError: string | null = $state(null);
  let activeTab: Tab = $state('chat');

  // Embedded mode is a section in the main window: the tab is chosen by the launcher's
  // sidebar, there is no tab column or language switcher of its own.
  let {
    embedded = false,
    tab = undefined,
    onTabChange = undefined,
    hidden = false,
  }: { embedded?: boolean; tab?: Tab; onTabChange?: (tab: Tab) => void; hidden?: boolean } = $props();

  $effect(() => {
    const wanted = tab;
    if (!embedded || !wanted) return;
    // Read activeTab without subscribing: otherwise a tab change via an event from ST
    // would roll back to the old prop value before the launcher updates it.
    untrack(() => {
      if (wanted === activeTab) return;
      activeTab = wanted;
      if (wanted === 'tune') loadRecommendations();
    });
  });

  $effect(() => {
    onTabChange?.(activeTab);
  });

  /** The tab came from the gateway — this screen knows how to render it. */
  function isRenderableTab(value: string | undefined): value is Tab {
    return !!value && gatewayTabs.includes(value);
  }

  /** Tab list from the gateway — the single source of the id allowlist.
   *  Accept the reply only when valid (a non-empty list of strings); on rejection or
   *  garbage — an honest error without a silent fallback. */
  async function loadGatewayTabs() {
    try {
      const fromGateway = await api.settingsTabs();
      if (
        Array.isArray(fromGateway) &&
        fromGateway.length > 0 &&
        fromGateway.every((t) => typeof t === 'string')
      ) {
        gatewayTabs = fromGateway;
        tabsError = null;
        return;
      }
      tabsError = t('st.tabsInvalid');
      console.error('[settings] settings_tabs вернул невалидный список', fromGateway);
    } catch (error) {
      tabsError = t('st.tabsFailed');
      console.error('[settings] settings_tabs не ответил', error);
    }
  }

  /** Tab from the URL: from SillyTavern the "Traits and personality" item opens
   *  the window directly on the right tab (`index.html#settings/tags`). */
  function tabFromHash(): Tab | null {
    const hash = window.location.hash.replace(/^#/, '');
    const requested = hash.split('/')[1];
    return isRenderableTab(requested) ? requested : null;
  }

  let settings: Settings | null = $state(null);
  // Snapshot of settings after load/save: loadData and deleteModel must not
  // overwrite unsaved form edits with a fresh server reply.
  let settingsSnapshot = $state('');
  let settingsSaveTail: Promise<void> = Promise.resolve();
  let setup = $state<SetupState | null>(null);
  let installedList = $state<string[]>([]);
  // The installed model list comes from Ollama: until it is up, the list is
  // unknown, and cards must not lie with "Not downloaded".
  let installedKnown = $state(true);
  let installedRetry: ReturnType<typeof setTimeout> | null = null;
  // VRAM changes before your eyes (someone closed another model, a game was started):
  // keep the indicators live, not a snapshot of when the screen opened.
  let usageTimer: ReturnType<typeof setInterval> | null = null;
  let imageModels = $state<ImageModelInfo[]>([]);
  let comfyStatus = $state<ComfyStatus>({ running: false, port: 8188, path: null, installed: false, partial_bytes: 0, partial_total_bytes: 0 });
  // Do not show the "Install" button before the first reply: the placeholder above looks
  // like "not installed" even for an installed ComfyUI.
  let comfyChecked = $state(false);
  let startingComfy = $state(false);
  let installingComfy = $state(false);
  let comfyCancelRequested = $state(false);
  let settingsContent: HTMLElement | undefined = $state();
  let comfyInstallProgress = $state<{ completed: number; total: number; status: string }>({
    completed: 0,
    total: 0,
    status: '',
  });

  let loading = $state(true);
let loadedOnce = false;
  let error = $state<string | null>(null);
  let saveSuccess = $state(false);
  // Hardware-based selection: the backend computes it, this is just display and apply.
  let recommend = $state<RecommendReport | null>(null);
  let applyingRecommend = $state(false);
  // Optional download of recommended models: nothing downloads by itself.
  let recommendedDownloads = $state<RecommendedDownload[]>([]);
  let downloadingRecommended = $state(false);
  let recommendedNote = $state('');
  // Personality traits: tag intensity and the ban on repeated actions.
  let traitCharacters = $state<TraitCharacter[]>([]);
  let selectedTraitAvatar = $state('');
  let savingTraits = $state(false);
  let traitsPreview = $state('');
  let traitsSnapshots = $state<Record<string, string>>({});
  // Live state is driven by the evaluator in the chat while the window is open: send it
  // only after a manual edit, otherwise saving a tag slider would roll
  // the state back to the load-time snapshot. The key is avatar: an edit is not lost
  // when switching to another character and back.
  let loadedTraitStates: Record<string, string> = {};

  function rememberTraitStates(list: TraitCharacter[]) {
    loadedTraitStates = Object.fromEntries(list.map((c) => [c.avatar, JSON.stringify(c.state)]));
  }

  function rememberTraitSnapshots(list: TraitCharacter[]) {
    traitsSnapshots = Object.fromEntries(list.map((c) => [c.avatar, traitsKey(c)]));
    bondsSnapshots = Object.fromEntries(list.map((c) => [c.avatar, bondsKey(c.bonds)]));
  }
  let newTraitTag = $state('');
  let unlistenTab: (() => void) | null = null;
  let checkingUpdate = $state(false);
  let updateNotice = $state<{
    message: string;
    type: 'info' | 'success' | 'error';
    closable: boolean;
  } | null>(null);

  // Download queue: the backend runs it, so the window shows what is downloading
  // now — even if the download was started by the wizard, the model menu in SillyTavern or
  // a previous settings window.
  let downloads = $state<DownloadsSnapshot>({ tasks: [], paused: false, running: false });

  /** What to show for a model or checkpoint: its own queue item, if any. */
  function downloadTask(kind: 'ollama' | 'image', id: string): DownloadTask | null {
    return (
      downloads.tasks.find((task) =>
        task.kind === kind && (kind === 'ollama' ? sameTag(task.id, id) : task.id === id),
      ) ?? null
    );
  }

  /** The item is still active: downloading, queued or paused. */
  function isActiveTask(task: DownloadTask | null): boolean {
    return (
      task !== null &&
      (task.status === 'queued' || task.status === 'downloading' || task.status === 'paused')
    );
  }

  /** The item failed: the reason must be shown where the model is listed.
   *  Cancel is not included: that is the user's decision, nothing to explain in the card. */
  function isFailedTask(task: DownloadTask | null): boolean {
    return task !== null && task.status === 'failed';
  }

  const activeDownloads = $derived(downloads.tasks.filter(isActiveTask));

  let customModelTag = $state('');
  let extraModelTag = $state('');
  let deletingModel = $state<string | null>(null);
  let deletingImageModel = $state<string | null>(null);
  // Previous-series model update flow: "Update and delete the old one".
  let upgradeOffers = $state<import('./lib/types').UpgradeOffer[]>([]);
  let upgradingModel = $state<string | null>(null);

  let unlistenDownloads: (() => void) | null = null;
  let unlistenComfy: (() => void) | null = null;
  let unlistenVision: (() => void) | null = null;
  let isolationEntries = $state<import('./lib/types').IsolationEntry[] | null>(null);
  let isolationChecking = $state(false);
  let shortcutBusy = $state(false);
  let shortcutMessage = $state('');
  let gpuUsage = $state<import('./lib/types').GpuUsage | null>(null);

  // Context beyond what fits in VRAM is moved to swap by Ollama:
  // generation slows hundreds of times. Show the real ceiling and warn.
  const contextCeiling = $derived(gpuUsage?.context_ceiling ?? 0);
  const contextTooBig = $derived.by(() => {
    const current = settings;
    if (!current) return false;
    return contextCeiling > 0 && current.context_size > contextCeiling;
  });

  // The model catalog comes from the backend already filtered for the hardware
  // (src-tauri/src/catalog.rs): the user sees only what the machine can run.
  let catalogModels: CatalogModel[] = $derived(setup?.models ?? []);

  // Models absent from the catalog (downloaded manually or fine-tuned here): without
  // this list a selected model could not be returned to the chat from the UI.
  let extraChatModels = $derived(
    installedList
      .filter((tag) => isLikelyChatModel(tag))
      .filter((tag) => !catalogModels.some((m) => isModelInstalled([tag], m.tag)))
      .sort((a, b) => a.localeCompare(b))
  );
  // Same for the embedder and vision: a model chosen outside this hardware's catalog is otherwise
  // invisible and cannot be selected back — only by editing settings.json.
  let extraRoleModels = $derived(
    installedList
      .filter((tag) =>
        activeTab === 'embed'
          ? /(embed|bge|nomic)/i.test(tag)
          : activeTab === 'vision'
            ? /(minicpm|llava|moondream|vision|(^|[/:_-])vl([:_-]|$))/i.test(tag)
            : false,
      )
      .filter((tag) => !catalogModels.some((m) => isModelInstalled([tag], m.tag)))
      .sort((a, b) => a.localeCompare(b))
  );


  function isInstalled(tag: string): boolean {
    return isModelInstalled(installedList, tag);
  }

  /** The evaluator and tagger need a chat model: an embedder or vision model will not
   *  do there. Keep an already saved choice in the list so it stays visible. */
  function chatModelOptions(current: string | undefined): string[] {
    return installedList.filter((tag) => isLikelyChatModel(tag) || sameTag(tag, current ?? ''));
  }

  function isActive(tag: string, cat: 'chat' | 'embed' | 'vision'): boolean {
    if (!settings) return false;
    if (cat === 'chat') return sameTag(settings.chat_model, tag);
    if (cat === 'embed') return sameTag(settings.embed_model, tag);
    if (cat === 'vision') return sameTag(settings.vision_model, tag);
    return false;
  }

  /** Repeat the model list poll until Ollama replies. */
  async function retryInstalled() {
    installedRetry = null;
    try {
      installedList = await api.installedModels();
      installedKnown = true;
    } catch {
      installedRetry = setTimeout(retryInstalled, 3000);
    }
  }

  async function loadData() {
    if (!loadedOnce) loading = true;
    error = null;
    try {
      const [s, st, inst, imgModels, usage, rec, recDownloads, traits, upState] = await Promise.all([
        api.getSettings(),
        api.setupState(),
        api.installedModels().then(
          (list) => ({ list }),
          () => null
        ),
        api.getImageModels().catch(() => []),
        api.gpuUsage().catch(() => null),
        api.recommendedSettings().catch(() => null),
        api.recommendedDownloads().catch(() => [] as RecommendedDownload[]),
        api.traitsState().catch(() => [] as TraitCharacter[]),
        api.modelsUpgradeState().catch(() => null),
      ]);
      // Do not overwrite settings while there are unsaved edits — otherwise
      // deleteModel/"Recheck" erase what the person typed into the form.
      if (!settingsDirty) {
        settings = s;
        markSettingsSaved();
      }
      setup = st;
      installedList = inst?.list ?? [];
      installedKnown = inst !== null;
      imageModels = imgModels;
      gpuUsage = usage;
      recommend = rec;
      recommendedDownloads = recDownloads;
      upgradeOffers = upState?.offers ?? [];
      // Do not overwrite unsaved trait and bond edits: "Refresh",
      // model download completion and startup readiness called loadData and
      // silently rolled sliders and bond rows back to the on-disk state.
      if (!anyTraitsDirty && !savingTraits && !savingBonds) {
        traitCharacters = traits;
        rememberTraitStates(traits);
        rememberTraitSnapshots(traits);
        if (!traits.some((c) => c.avatar === selectedTraitAvatar)) {
          selectedTraitAvatar = traits[0]?.avatar ?? '';
        }
        updateTraitsPreview();
      }
      // Ollama takes longer to start than the page: while the list is missing, ask again.
      // Re-ask only for the model list: a full screen reload
      // pulled nine requests every three seconds the whole time Ollama was down.
      if (!installedKnown && !installedRetry) {
        installedRetry = setTimeout(retryInstalled, 3000);
      }
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
      loadedOnce = true;
    }
  }

  function markSettingsSaved() {
    settingsSnapshot = settings ? JSON.stringify(settings) : '';
  }

  const settingsDirty = $derived(
    settings ? JSON.stringify(settings) !== settingsSnapshot : false,
  );

  // Tabs live in one scrollable container: without a reset a long
  // model list left a new tab open in the middle of the page.
  $effect(() => {
    activeTab;
    settingsContent?.scrollTo({ top: 0 });
  });

  /** The selected character and its traits (tag intensity). */
  const selectedTraits = $derived(
    traitCharacters.find((c) => c.avatar === selectedTraitAvatar) ?? null,
  );

  function traitsKey(character: TraitCharacter): string {
    return JSON.stringify({
      tags: character.tags,
      no_repeat: character.no_repeat,
      chaos: character.chaos,
      live: character.live,
      state: character.state,
    });
  }
  /** Sliders were moved after load or save. */
  const traitsDirty = $derived(
    selectedTraits
      ? traitsKey(selectedTraits) !== traitsSnapshots[selectedTraits.avatar]
      : false,
  );

  const anyTraitsDirty = $derived(traitCharacters.some((character) =>
    traitsKey(character) !== traitsSnapshots[character.avatar] ||
    bondsKey(character.bonds) !== bondsSnapshots[character.avatar],
  ));

  async function loadTraits() {
    try {
      const characters = await api.traitsState();
      if (anyTraitsDirty || savingTraits || savingBonds) return;
      traitCharacters = characters;
      rememberTraitStates(traitCharacters);
      rememberTraitSnapshots(traitCharacters);
      if (!traitCharacters.some((c) => c.avatar === selectedTraitAvatar)) {
        selectedTraitAvatar = traitCharacters[0]?.avatar ?? '';
      }
      updateTraitsPreview();
    } catch (e) {
      error = String(e);
    }
  }

  /** "Refresh" on the traits tab: while there are unsaved edits, rewriting
   *  would only lose them. */
  function refreshTraits() {
    if (anyTraitsDirty || savingTraits || savingBonds) {
      updateNotice = {
        message: t('st.saveTraitsFirst'),
        type: 'error',
        closable: true,
      };
      return;
    }
    loadTraits();
  }

  function updateTraitsPreview() {
    const current = traitCharacters.find((c) => c.avatar === selectedTraitAvatar);
    traitsPreview = current?.fragment ?? '';
  }

  function onTraitCharacterChange() {
    updateTraitsPreview();
  }

  function addTraitTag() {
    const tag = newTraitTag.trim();
    if (!tag || !selectedTraits) return;
    if (selectedTraits.tags.some((t) => t.tag.toLowerCase() === tag.toLowerCase())) {
      newTraitTag = '';
      return;
    }
    selectedTraits.tags = [...selectedTraits.tags, { tag, intensity: 60 }];
    newTraitTag = '';
  }

  function removeTraitTag(tag: string) {
    if (!selectedTraits) return;
    selectedTraits.tags = selectedTraits.tags.filter((t) => t.tag !== tag);
  }

  /** Pull tags set in SillyTavern (auto-tagger or manually). */
  function pullStTags() {
    if (!selectedTraits) return;
    const known = new Set(selectedTraits.tags.map((t) => t.tag.toLowerCase()));
    const added = selectedTraits.st_tags
      .filter((tag) => !known.has(tag.toLowerCase()))
      .map((tag) => ({ tag, intensity: 50 }));
    if (added.length > 0) {
      selectedTraits.tags = [...selectedTraits.tags, ...added];
    }
  }

  /** Return the state to a level baseline after a quarrel or a strange scene. */
  async function resetTraitState() {
    // One mutation at a time; reconciliation uses the sent avatar.
    if (!selectedTraits || savingTraits || savingBonds) return;
    savingTraits = true;
    try {
      const avatar = selectedTraits.avatar;
      const previousState = JSON.stringify(selectedTraits.state);
      const cleanState = { affection: 50, trust: 50, energy: 55, irritation: 10 };
      const fragment = await api.traitsResetState(avatar);
      loadedTraitStates[avatar] = JSON.stringify(cleanState);
      const baseline = (traitsSnapshots[avatar] ? JSON.parse(traitsSnapshots[avatar]) : {}) as Record<string, unknown>;
      traitsSnapshots[avatar] = JSON.stringify({ ...baseline, state: cleanState });
      const saved = traitCharacters.find((c) => c.avatar === avatar);
      if (saved) {
        if (JSON.stringify(saved.state) === previousState) saved.state = cleanState;
        saved.fragment = fragment;
      }
      if (selectedTraitAvatar === avatar) {
        traitsPreview = fragment;
      }
      updateNotice = { message: t('st.stateReset'), type: 'success', closable: true };
    } catch (e) {
      error = String(e);
    } finally {
      savingTraits = false;
    }
  }

  // The card text is built by the backend (traits.rs) — here it is only displayed,
  // so intensity rules do not diverge in two places.
  async function saveTraits() {
    // Field copies: edits during await do not land in the saved snapshot.
    if (!selectedTraits || savingTraits || savingBonds) return;
    savingTraits = true;
    error = null;
    try {
      // Saving is bound to the avatar, not to the current selection: while the
      // request ran, the user could switch the card, and the reply landed on it.
      const avatar = selectedTraits.avatar;
      const stateValues = { ...selectedTraits.state };
      const stateJson = JSON.stringify(stateValues);
      const stateEdited = stateJson !== loadedTraitStates[avatar];
      // Tags and flags are also captured before await: after saving bonds
      // the selected card may already be another one, and its fields would go to the first.
      const tags = selectedTraits.tags.map((tag) => ({ ...tag }));
      const noRepeat = selectedTraits.no_repeat;
      const chaos = selectedTraits.chaos;
      const live = selectedTraits.live;
      // Unsaved bonds: `traits_save` builds the note from the bond store,
      // so without this write new rows did not reach the text, and
      // `markTraitsSaved` immediately marked them saved — the edits disappeared.
      const pendingBonds = bondsDirty ? selectedTraits.bonds.map((bond) => ({ ...bond })) : null;
      const submittedKey = traitsKey({ ...selectedTraits, tags, state: stateValues });
      if (pendingBonds) {
        await api.bondsSave(avatar, pendingBonds);
        bondsSnapshots[avatar] = bondsKey(pendingBonds);
      }
      const fragment = await api.traitsSave(
        avatar,
        tags,
        noRepeat,
        chaos,
        live,
        stateEdited ? stateValues : null,
      );
      if (stateEdited) loadedTraitStates[avatar] = stateJson;
      traitsSnapshots[avatar] = submittedKey;
      const saved = traitCharacters.find((c) => c.avatar === avatar);
      if (saved) {
        saved.fragment = fragment;
      }
      if (selectedTraitAvatar === avatar) {
        traitsPreview = fragment;
      }
      updateNotice = {
        message: fragment
          ? t(pendingBonds ? 'st.traitsBondsSaved' : 'st.traitsSaved')
          : t('st.traitsCleared'),
        type: 'success',
        closable: true,
      };
    } catch (e) {
      error = String(e);
    } finally {
      savingTraits = false;
    }
  }

  /** Reset means zeros and repeat ban off: the note is removed from the card. */
  async function resetTraits() {
    if (!selectedTraits || savingTraits || savingBonds) return;
    for (const tag of selectedTraits.tags) {
      tag.intensity = 0;
    }
    selectedTraits.no_repeat = false;
    await saveTraits();
  }

  // ── Bonds with other characters ───────────────────────────────────────────
  // Separate "Save bonds" and separate dirty flag: editing bonds must not
  // make the "Save traits" button require a write for someone else's field.
  let bondsSnapshots = $state<Record<string, string>>({});
  let savingBonds = $state(false);
  /** The same limit as in `bonds.rs`: beyond it the backend refuses to write. */
  const MAX_BONDS = 12;

  /** Bond write key: the list or a row of another bond changed. */
  function bondsKey(list: Bond[]): string {
    return JSON.stringify(
      list.map((bond) => ({
        target: bond.target,
        kind: bond.kind,
        strength: bond.strength,
        note: bond.note,
        secret: bond.secret,
      })),
    );
  }

  const selectedBonds = $derived(selectedTraits?.bonds ?? []);
  const bondsDirty = $derived(
    selectedTraits ? bondsKey(selectedTraits.bonds) !== bondsSnapshots[selectedTraits.avatar] : false,
  );

  /** Who can be chosen as a target: all cards except the character itself. */
  const bondCandidates = $derived(
    traitCharacters
      .filter((c) => c.avatar !== selectedTraitAvatar)
      .map((c) => ({ avatar: c.avatar, name: c.name })),
  );

  /** Targets already used in the list: a second row about the same character is not needed. */
  const usedTargets = $derived(new Set(selectedBonds.map((bond) => bond.target)));

  function addBond() {
    if (!selectedTraits || selectedTraits.bonds.length >= MAX_BONDS) return;
    selectedTraits.bonds = [
      ...selectedTraits.bonds,
      { target: '', kind: '', strength: 50, note: '', secret: false },
    ];
  }

  function removeBond(index: number) {
    if (!selectedTraits) return;
    selectedTraits.bonds = selectedTraits.bonds.filter((_, i) => i !== index);
  }

  async function clearBonds() {
    if (!selectedTraits || savingTraits || savingBonds) return;
    selectedTraits.bonds = [];
    await saveBonds();
  }

  /** The card text is built by the backend: here we show what it returned. */
  async function saveBonds() {
    // One mutation at a time; the snapshot matches the sent list.
    if (!selectedTraits || savingTraits || savingBonds) return;
    savingBonds = true;
    error = null;
    try {
      // Like traits: the reply belongs to the saved avatar, not to the current selection.
      const avatar = selectedTraits.avatar;
      const list = selectedTraits.bonds.map((bond) => ({ ...bond }));
      const fragment = await api.bondsSave(avatar, list);
      bondsSnapshots[avatar] = bondsKey(list);
      const saved = traitCharacters.find((c) => c.avatar === avatar);
      if (saved) {
        saved.fragment = fragment;
      }
      if (selectedTraitAvatar === avatar) {
        traitsPreview = fragment;
      }
      updateNotice = {
        message: list.length
          ? t('st.bondsSaved', { count: list.length })
          : t('st.bondsCleared'),
        type: 'success',
        closable: true,
      };
    } catch (e) {
      error = String(e);
    } finally {
      savingBonds = false;
    }
  }

  /** Refresh only the model list: a model could have finished downloading in the background. */
  async function refreshInstalled() {
    try {
      installedList = await api.installedModels();
      installedKnown = true;
    } catch {
      // Ollama did not reply — the list will refresh on the next screen open.
    }
  }

  /** Re-read the download queue: it lives in the backend, not in this window. */
  async function refreshDownloads() {
    try {
      downloads = await api.downloadsState();
    } catch {
      // The backend is not ready yet — the state will arrive via events.
    }
  }

  /** What of the recommended set is not downloaded yet — list for the optional download. */
  async function loadRecommendedDownloads() {
    try {
      recommendedDownloads = await api.recommendedDownloads();
    } catch {
      recommendedDownloads = [];
    }
  }

  async function downloadRecommended() {
    if (downloadingRecommended) return;
    downloadingRecommended = true;
    recommendedNote = t('st.recQueue');
    try {
      const failed = await api.downloadRecommended();
      recommendedNote = failed.length
        ? t('st.recPartial', { list: failed.join('; ') })
        : t('st.recDone');
      await loadRecommendedDownloads();
      await refreshInstalled();
    } catch (e) {
      error = String(e);
    } finally {
      downloadingRecommended = false;
    }
  }

  /** Recompute hardware suggestions (both on tab open and on button press). */
  async function loadRecommendations() {
    try {
      recommend = await api.recommendedSettings();
    } catch (e) {
      error = String(e);
    }
  }

  // Apply settings on the backend side: it also recomputes the context
  // ceiling and syncs SillyTavern, otherwise ST would keep the old
  // values until the next save.
  async function applyRecommended(keys?: string[]) {
    if (applyingRecommend) return;
    applyingRecommend = true;
    error = null;
    try {
      // Unsaved form edits stay on top of the applied values:
      // otherwise "Apply" silently erased what was typed on other tabs.
      await queueSettingsChange(() => api.applyRecommendedSettings(keys), true);
      await loadRecommendations();
      updateNotice = { message: t(keys?.length ? 'st.tuneOne' : 'st.tuneAll'), type: 'success', closable: true };
    } catch (e) {
      error = String(e);
    } finally {
      applyingRecommend = false;
    }
  }

  async function handleRefreshAndUpdate() {
    await loadData();
    await checkForAppUpdate();
  }

  async function checkForAppUpdate() {
    if (checkingUpdate) return;
    checkingUpdate = true;
    updateNotice = {
      message: t('st.upd.checking'),
      type: 'info',
      closable: false,
    };

    try {
      const update = await api.checkAppUpdate();
      if (update.available) {
        // Installation kills SillyTavern, Ollama, ComfyUI, downloads and training and
        // restarts the app, while "Refresh" is also pressed just for model status:
        // install only with consent, like the update card in App.
        const agreed = confirm(
          t('st.upd.confirm', { version: update.latest_version }),
        );
        if (!agreed) {
          updateNotice = {
            message: t('st.upd.later', { version: update.latest_version }),
            type: 'info',
            closable: true,
          };
          return;
        }
        updateNotice = {
          message: t('st.upd.found', { version: update.latest_version }),
          type: 'info',
          closable: false,
        };

        await api.applyAppUpdate((p) => {
          const currentMb = (p.downloaded_bytes / 1048576).toFixed(1);
          const totalMb = (p.total_bytes / 1048576).toFixed(1);
          updateNotice = {
            message: t('st.upd.progress', { version: update.latest_version, percent: p.percent, current: currentMb, total: totalMb }),
            type: 'info',
            closable: false,
          };
          if (p.percent >= 100) {
            updateNotice = {
              message: t('st.upd.ready', { version: update.latest_version }),
              type: 'success',
              closable: false,
            };
          }
        });
      } else {
        updateNotice = {
          message: t('st.upd.latest', { version: update.current_version }),
          type: 'success',
          closable: true,
        };
        setTimeout(() => {
          if (updateNotice?.message === t('st.upd.latest', { version: update.current_version })) {
            updateNotice = null;
          }
        }, 4000);
      }
    } catch (e) {
      updateNotice = {
        message: t('st.upd.error', { error: e instanceof Error ? e.message : String(e) }),
        type: 'error',
        closable: true,
      };
      setTimeout(() => {
        if (updateNotice?.type === 'error') {
          updateNotice = null;
        }
      }, 6000);
    } finally {
      checkingUpdate = false;
    }
  }

  /** Last saved settings plus one field: selecting a model must neither
   *  send unsaved form edits nor mark them saved. */
  function settingsChanges(base: Partial<Settings>, current: Settings | null): Partial<Settings> {
    if (!current) return {};
    const before = base as Record<string, unknown>;
    const edits: Record<string, unknown> = {};
    for (const [key, value] of Object.entries(current)) {
      if (JSON.stringify(value) !== JSON.stringify(before[key])) edits[key] = value;
    }
    return edits as Partial<Settings>;
  }

  /** Form fields the user changed since the last load. */
  function formEdits(): Partial<Settings> {
    return settingsChanges(settingsSnapshot ? JSON.parse(settingsSnapshot) : {}, settings);
  }

  function queueSettingsChange(
    mutate: (edits: Partial<Settings>) => Promise<Settings>,
    preserveEdits: boolean,
    patch: Partial<Settings> = {},
  ): Promise<void> {
    // Writes in order; edits made during IPC are preserved.
    const edits = formEdits();
    const draft = settings ? { ...settings } : {};
    const operation = settingsSaveTail.then(async () => {
      const saved = await mutate(edits);
      const newerEdits = settingsChanges(draft, settings);
      settingsSnapshot = JSON.stringify(saved);
      settings = { ...saved, ...(preserveEdits ? edits : {}), ...newerEdits, ...patch };
    });
    settingsSaveTail = operation.catch(() => {});
    return operation;
  }

  /** While the window is open, settings are changed by the ST model menu, the selection and the gateway:
   *  the window snapshot goes stale. Save fresh backend values plus edits,
   *  and the form keeps unsaved edits on top of what was written. */
  async function saveOverFresh(patch: Partial<Settings>, keepEdits: boolean): Promise<void> {
    await queueSettingsChange(async (edits) => {
      const saved = { ...(await api.getSettings()), ...(keepEdits ? edits : {}), ...patch };
      await api.saveSettings(saved);
      return saved;
    }, !keepEdits, patch);
  }

  async function selectModel(tag: string, cat: 'chat' | 'embed' | 'vision') {
    if (!settings) return;
    try {
      const patch: Partial<Settings> =
        cat === 'chat' ? { chat_model: tag } : cat === 'embed' ? { embed_model: tag } : { vision_model: tag };
      await saveOverFresh(patch, false);
      saveSuccess = true;
      setTimeout(() => (saveSuccess = false), 2500);
    } catch (e) {
      error = t('st.err.saveModel', { error: String(e) });
    }
  }

  async function selectImageModel(filename: string) {
    if (!settings) return;
    try {
      await queueSettingsChange(async () => {
        await api.setImageModel(filename);
        return api.getSettings();
      }, true, { image_model: filename });
      imageModels = await api.getImageModels().catch(() => imageModels);
      saveSuccess = true;
      setTimeout(() => (saveSuccess = false), 2500);
    } catch (e) {
      error = t('st.err.saveImageModel', { error: String(e) });
      // The radio marked itself on click before the reply: restore the backend state,
      // otherwise a failed choice would stay marked in the DOM.
      imageModels = await api.getImageModels().catch(() => imageModels);
    }
  }

  // ComfyUI status is needed only by the images tab, and the port probe with ComfyUI off
  // costs 600 ms — the general screen load does not wait for it.
  async function refreshComfyStatus() {
    try {
      comfyStatus = await api.getComfyStatus();
      comfyChecked = true;
    } catch {
      // status is not critical — the previous one stays
    }
  }

  $effect(() => {
    if (activeTab === 'image') refreshComfyStatus();
  });

  async function handleStartComfy() {
    startingComfy = true;
    try {
      await api.startComfyServer();
      // Wait until the server actually responds: a fixed three seconds
      // brought the button back too early, and a second click started
      // a second ComfyUI process.
      for (let attempt = 0; attempt < 40; attempt += 1) {
        comfyStatus = await api.getComfyStatus();
        if (comfyStatus.running) break;
        await new Promise((resolve) => setTimeout(resolve, 1500));
      }
      if (!comfyStatus.running) {
        error =
          t('st.err.comfyTimeout');
      }
      imageModels = await api.getImageModels().catch(() => imageModels);
    } catch (e) {
      error = t('st.err.comfyStart', { error: String(e) });
    } finally {
      startingComfy = false;
    }
  }

  async function handleInstallComfy() {
    installingComfy = true;
    comfyCancelRequested = false;
    error = null;
    comfyInstallProgress = { completed: 0, total: 0, status: t('st.comfyPreparing') };
    try {
      await api.installComfyui();
      comfyStatus = await api.getComfyStatus();
      imageModels = await api.getImageModels().catch(() => imageModels);
    } catch (e) {
      if (comfyCancelRequested) {
        // A deliberate cancel is not an install error — the same policy as in the wizard.
        comfyCancelRequested = false;
        comfyInstallProgress = { completed: 0, total: 0, status: '' };
        // The resumable download stays on disk: without re-reading the status, the badge
        // keeps the size captured before cancel.
        try {
          comfyStatus = await api.getComfyStatus();
        } catch {
          // status is not critical — keep the previous one
        }
      } else {
        error = t('st.err.comfyInstall', { error: String(e) });
      }
    } finally {
      installingComfy = false;
    }
  }

  // Cancel the ComfyUI install: clear the download flag and remove "Installing…",
  // otherwise the cancel button canceled nothing and the state hung until the install ended.
  async function cancelComfyInstall() {
    comfyCancelRequested = true;
    try {
      await api.cancelComfyInstall();
    } catch (e) {
      error = String(e);
      return;
    }
    // The button is not re-enabled here: the install has not finished, and a repeated
    // "Resume install" click would hit the busy backend slot, and its
    // rejection would later look like a real error. handleInstallComfy will clear the state.
    comfyInstallProgress = { completed: 0, total: 0, status: t('st.comfyCancelling') };
  }

  async function handleDownloadImageModel(filename: string) {
    error = null;
    try {
      await api.downloadImageModel(filename);
      imageModels = await api.getImageModels().catch(() => imageModels);
    } catch (e) {
      const text = String(e);
      if (/отмен|приостанов/i.test(text)) {
        updateNotice = { message: text, type: 'info', closable: true };
      } else {
        error = t('st.err.download', { error: text });
      }
    }
  }

  async function startDownload(tag: string) {
    error = null;
    try {
      await api.downloadModel(tag);
      await loadData();
    } catch (e) {
      const text = String(e);
      // Canceling with the button is not a download failure: the red "Download
      // error" banner hung until the window reloaded (the same policy as in the wizard).
      if (/отмен/i.test(text)) {
        updateNotice = { message: t('st.dl.cancelled'), type: 'info', closable: true };
      } else if (/приостанов/i.test(text)) {
        updateNotice = {
          message: t('st.dl.paused'),
          type: 'info',
          closable: true,
        };
      } else {
        error = t('st.err.downloadTag', { tag, error: text });
      }
    }
  }

  /** Cancel the whole queue: from the header banner, where one row covers all downloads. */
  async function cancelDownloads() {
    try {
      await api.downloadsCancel();
    } catch (e) {
      error = String(e);
    }
  }

  async function pauseDownloads() {
    try {
      await api.downloadsPause();
    } catch (e) {
      error = String(e);
    }
  }

  async function resumeDownloads() {
    error = null;
    try {
      await api.downloadsResume();
    } catch (e) {
      error = String(e);
    }
  }

  // The buttons in a download row address exactly it: the queue is sequential, and
  // "Resume" on a waiting model means "download this one, not the one started
  // first" — previously all three buttons controlled the whole queue.
  async function pauseTask(id: string) {
    try {
      await api.downloadsPauseTask(id);
    } catch (e) {
      error = String(e);
    }
  }

  async function resumeTask(id: string) {
    error = null;
    try {
      await api.downloadsResumeTask(id);
    } catch (e) {
      error = String(e);
    }
  }

  async function cancelTask(id: string) {
    try {
      await api.downloadsCancelTask(id);
    } catch (e) {
      error = String(e);
    }
  }

  async function deleteModel(tag: string) {
    if (!confirm(t('st.confirmDeleteModel', { tag }))) {
      return;
    }
    deletingModel = tag;
    error = null;
    try {
      await api.deleteModel(tag);
      const updated = await api.getSettings().catch(() => null);
      if (updated && !settingsDirty) {
        settings = updated;
        markSettingsSaved();
      }
      await loadData();
    } catch (e) {
      error = t('st.err.deleteModel', { tag, error: String(e) });
    } finally {
      deletingModel = null;
    }
  }

  /** Update a model per the map rule: download the new one, move roles from the old
   *  to it and delete the old one. Download progress is in the model cards below. */
  async function upgradeModel(offer: import('./lib/types').UpgradeOffer) {
    upgradingModel = offer.from;
    error = null;
    try {
      await api.modelsUpgradeApply(offer.from, offer.to);
    } catch (e) {
      error = t('st.err.upgradeModel', { tag: offer.from, error: String(e) });
    } finally {
      upgradingModel = null;
      await loadData();
    }
  }

  /** "Later": the pair is hidden from suggestions; new map rules will show up again. */
  async function dismissUpgrade(offer: import('./lib/types').UpgradeOffer) {
    upgradeOffers = upgradeOffers.filter((o) => o.from !== offer.from);
    try {
      await api.modelsUpgradeDismiss(offer.from);
    } catch {
      // Not saved — the suggestion will return on the next screen open.
    }
  }

  async function handleDeleteImageModel(filename: string) {
    const isCurActive = !!(settings && settings.image_model?.toLowerCase() === filename.toLowerCase());
    const confirmMsg = isCurActive
      ? t('st.confirmDeleteActiveImage', { file: filename })
      : t('st.confirmDeleteImage', { file: filename });
    if (!confirm(confirmMsg)) {
      return;
    }
    deletingImageModel = filename;
    error = null;
    try {
      await api.deleteImageModel(filename);
      imageModels = await api.getImageModels().catch(() => imageModels);
      const updatedSettings = await api.getSettings().catch(() => null);
      if (updatedSettings && !settingsDirty) {
        settings = updatedSettings;
        markSettingsSaved();
      }
    } catch (e) {
      error = t('st.err.deleteImage', { file: filename, error: String(e) });
    } finally {
      deletingImageModel = null;
    }
  }

  async function checkIsolation() {
    isolationChecking = true;
    error = null;
    try {
      isolationEntries = await api.isolationReport();
    } catch (e) {
      error = t('st.err.isolation', { error: String(e) });
    } finally {
      isolationChecking = false;
    }
  }

  async function createShortcut() {
    shortcutBusy = true;
    shortcutMessage = '';
    try {
      const path = await api.createDesktopShortcut();
      shortcutMessage = t('st.shortcutCreated', { path });
    } catch (e) {
      shortcutMessage = t('st.err.shortcut', { error: String(e) });
    } finally {
      shortcutBusy = false;
    }
  }

  async function saveInferenceParams() {
    if (!settings) return;
    // Range and default value as in Settings::load (settings.rs).
    settings.tagger_max_tags = Math.round(clampNumber(settings.tagger_max_tags, 3, 20, 8));
    try {
      await saveOverFresh({}, true);
      saveSuccess = true;
      setTimeout(() => (saveSuccess = false), 2500);
    } catch (e) {
      error = t('st.err.saveParams', { error: String(e) });
    }
  }

  /** Size in bytes (size_bytes of the isolation check, download queue counters):
   *  up to a gigabyte — megabytes, beyond — gigabytes with a fraction, as in backend logs
   *  (`{:.1} GB`): the model store runs into hundreds of gigabytes. */
  function formatSize(bytes: number): string {
    if (!bytes) return `0 ${t('unit.mb')}`;
    if (bytes >= 1073741824) return `${(bytes / 1073741824).toFixed(1)} ${t('unit.gb')}`;
    return `${(bytes / 1048576).toFixed(0)} ${t('unit.mb')}`;
  }

  function formatEta(seconds: number): string {
    if (seconds < 60) return `${Math.max(1, Math.round(seconds))} ${t('unit.s')}`;
    if (seconds < 3600) return `${Math.round(seconds / 60)} ${t('unit.min')}`;
    return `${(seconds / 3600).toFixed(1)} ${t('unit.h')}`;
  }

  /** Download percentage. Zero — no data yet: Ollama counts by layers. */
  function downloadPercent(task: DownloadTask): number {
    if (task.total > 0) {
      return Math.min(100, Math.round((task.completed / task.total) * 100));
    }
    return task.status === 'done' ? 100 : 0;
  }

  function downloadNote(task: DownloadTask): string {
    if (task.status === 'paused') return t('st.task.paused');
    if (task.status === 'queued') return t('st.task.queued');
    if (task.status === 'cancelled') return t('st.task.cancelled');
    if (task.status === 'failed') return t('st.task.failed');
    if (task.status === 'done') return t('st.task.done');
    return task.note || t('st.task.preparing');
  }

  /** How much is downloaded, at what speed and how much is left. Speed and ETA
   *  are shown only for an active download: for pause and failure those are stale numbers. */
  function downloadDetails(task: DownloadTask): string {
    const parts: string[] = [];
    if (task.total > 0) {
      parts.push(`${formatSize(task.completed)} / ${formatSize(task.total)}`);
    } else if (task.completed > 0) {
      parts.push(formatSize(task.completed));
    }
    if (task.status === 'downloading') {
      if (task.speed > 0) {
        parts.push(`${(task.speed / 1048576).toFixed(1)} ${t('wizard.mbps')}`);
      }
      if (task.eta > 0) {
        parts.push(t('st.task.left', { eta: formatEta(task.eta) }));
      }
    }
    return parts.join(' · ');
  }

  onMount(async () => {
    // The tab allowlist — before reading the URL: the window could have been opened
    // by SillyTavern on a specific tab, and the check must use the gateway list.
    await loadGatewayTabs();
    // The tab from the URL matters before data load: the window could have been opened
    // by the "Traits and personality" item from SillyTavern.
    const fromHash = tabFromHash();
    if (fromHash) {
      activeTab = fromHash;
    }
    await loadData();
    // Same for an already open window: the app sends the tab via an event.
    unlistenTab = await listen<string>('cloufam-tab', (e) => {
      // The gateway takes the tab from the ST page request as is: an unknown string
      // would leave an empty content area.
      const tab = e.payload;
      if (!isRenderableTab(tab)) return;
      activeTab = tab;
      // While the window was open, live state changed in the chat and
      // cards appeared: the open-time snapshot would roll the state back on save.
      if (tab === 'tags' && !anyTraitsDirty && !savingTraits && !savingBonds) loadTraits();
    });
    usageTimer = setInterval(() => {
      // The embedded screen is hidden by a class rather than unmounted: on "Home" the poll is not needed
      if (document.visibilityState !== 'visible' || hidden) return;
      api.gpuUsage().then((usage) => (gpuUsage = usage)).catch(() => {});
    }, 10000);
    // The queue snapshot arrives both via events and via a request on window open: so
    // a download started before settings opened is visible immediately, not from the first tick.
    unlistenDownloads = await listen<DownloadsSnapshot>('downloads-changed', (e) => {
      const next = e.payload;
      const finished = next.tasks.some((task) => {
        if (task.status !== 'done') return false;
        const before = downloads.tasks.find((t) => t.kind === task.kind && t.id === task.id);
        return before?.status !== 'done';
      });
      downloads = next;
      if (finished) {
        refreshInstalled();
        loadRecommendedDownloads();
      }
    });
    await refreshDownloads();

    unlistenVision = await listen<VisionDownloadResult>('vision-download-done', () => {
      refreshInstalled();
    });

    unlistenComfy = await listen<{ completed: number; total: number; status: string }>(
      'comfy-setup-progress',
      (e) => {
        comfyInstallProgress = e.payload;
      }
    );
  });

  onDestroy(() => {
    unlistenDownloads?.();
    unlistenComfy?.();
    unlistenVision?.();
    unlistenTab?.();
    if (installedRetry) clearTimeout(installedRetry);
    if (usageTimer) clearInterval(usageTimer);
  });
</script>

{#snippet downloadBox(task: DownloadTask)}
  <div class="download-box">
    <div class="progress-bar-wrap">
      <div
        class="progress-bar-fill {task.status === 'paused' ? 'is-paused' : ''}"
        style:width="{downloadPercent(task)}%"
      ></div>
    </div>
    <div class="progress-meta">
      <span class="p-status">{downloadNote(task)}</span>
      <span class="p-details">{downloadDetails(task)}</span>
    </div>
    {#if task.error}
      <div class="p-error">{task.error}</div>
    {/if}
    {#if !isFailedTask(task)}
      <div class="download-actions">
        {#if task.status === 'queued'}
          <button class="btn-tool" onclick={() => resumeTask(task.id)} title={t('st.dl.next')}>
            {t('st.dl.thisOne')}
          </button>
        {:else if task.status === 'paused' || downloads.paused}
          <button class="btn-tool" onclick={() => resumeTask(task.id)}>{t('st.dl.resume')}</button>
        {:else}
          <button
            class="btn-tool"
            onclick={() => pauseTask(task.id)}
            disabled={task.status !== 'downloading'}
          >
            {t('st.dl.pause')}
          </button>
        {/if}
        <button class="btn-cancel" onclick={() => cancelTask(task.id)}>{t('st.dl.cancel')}</button>
      </div>
    {/if}
  </div>
{/snippet}

<div class="settings-page" class:embedded>
  <!-- Top header -->
  <header class="settings-header" data-tauri-drag-region>
    {#if embedded}
      <h1 class="settings-title" data-tauri-drag-region>{t(`settings.tab.${activeTab}` as I18nKey)}</h1>
    {:else}
    <div class="header-left" data-tauri-drag-region>
      <div class="crystal-icon"><AppLogo size={40} /></div>
      <div>
        <h1 class="settings-title">
          {t('settings.title')}
        </h1>
        <p class="settings-sub">
          {t('settings.sub')}
        </p>
      </div>
    </div>

    {/if}

    <div class="header-right">
      {#if activeDownloads.length > 0}
        {@const current = activeDownloads.find((task) => task.status !== 'queued') ?? activeDownloads[0]}
        <button
          class="stat-pill download-pill {downloads.paused || current.status === 'paused' ? 'paused' : ''}"
          onclick={downloads.paused || current.status === 'paused' ? resumeDownloads : pauseDownloads}
          title={t('st.dl.background')}
        >
          {#if downloads.paused || current.status === 'paused'}
            <span>{t('st.dl.resumeNamed', { title: current.title })}</span>
          {:else}
            <span class="btn-spinner"></span>
            <span>
              {current.title}{current.status === 'queued' || current.total === 0
                ? ''
                : ` — ${downloadPercent(current)}%`}{activeDownloads.length > 1
                ? t('st.dl.queuedMore', { count: activeDownloads.length - 1 })
                : ''}
            </span>
            <span class="pill-action">⏸</span>
          {/if}
        </button>
      {/if}

      <!-- In the main window the GPU and disk are already in the "System" block of the
           sidebar, and on one line with the section name the badges slid under the window buttons. -->
      {#if setup && !embedded}
        <div class="stat-pill gpu-pill" title={t('st.gpuTitle')}>
          <span class="dot-live"></span>
          <b>{setup.gpu_name || t('st.cpuOnly')}</b>
          {#if setup.vram_mb}
            <span class="stat-dim">{t('st.vramGb', { gb: (setup.vram_mb / 1024).toFixed(1) })}</span>
          {/if}
        </div>
        {#if setup.free_space_gb >= 0}
          <div class="stat-pill disk-pill" title={t('st.diskTitle')}>
            <Icon name="disk" size={15} />
            <span>{t('st.freeGb', { gb: setup.free_space_gb.toFixed(1) })}</span>
          </div>
        {/if}
      {/if}
      {#if embedded}
        <button class="btn-tool" onclick={handleRefreshAndUpdate} disabled={checkingUpdate} title={t('st.refreshTitle')}>
          {#if checkingUpdate}
            <span class="btn-spinner"></span> {t('settings.checking')}
          {:else}
            <Icon name="retry" /> {t('settings.refresh')}
          {/if}
        </button>
      {:else}
        <LanguageMenu />
      {/if}
    </div>
  </header>

  {#if tabsError}
    <!-- Honest error V-45: the tab list was not received, there is no fallback copy. -->
    <div class="tabs-error" role="alert">⚠ {tabsError}</div>
  {/if}

  <div class="settings-body">
  <!-- Quick utilities panel -->
  {#if !embedded}
  <div class="quick-toolbar">
    <div class="nav-tabs">
      <button class="tab-btn {activeTab === 'chat' ? 'active' : ''}" onclick={() => activeTab = 'chat'}>
        <Icon name="chat" /> {t('settings.tab.chat')}
      </button>
      <button class="tab-btn {activeTab === 'embed' ? 'active' : ''}" onclick={() => activeTab = 'embed'}>
        <Icon name="memory" /> {t('settings.tab.embed')}
      </button>
      <button class="tab-btn {activeTab === 'vision' ? 'active' : ''}" onclick={() => activeTab = 'vision'}>
        <Icon name="eye" /> {t('settings.tab.vision')}
      </button>
      <button class="tab-btn {activeTab === 'image' ? 'active' : ''}" onclick={() => activeTab = 'image'}>
        <Icon name="image" /> {t('settings.tab.image')}
      </button>
      <button
        class="tab-btn {activeTab === 'tune' ? 'active' : ''}"
        onclick={() => { activeTab = 'tune'; loadRecommendations(); }}
        title={t('st.tuneTitle')}
      >
        <Icon name="wand" /> {t('settings.tab.tune')}
        {#if recommend && recommend.items.length > 0}
          <span class="tab-badge">{recommend.items.length}</span>
        {/if}
      </button>
      <button class="tab-btn {activeTab === 'params' ? 'active' : ''}" onclick={() => activeTab = 'params'}>
        <Icon name="sliders" /> {t('settings.tab.params')}
      </button>
      <button class="tab-btn {activeTab === 'custom' ? 'active' : ''}" onclick={() => activeTab = 'custom'}>
        <Icon name="plus" /> {t('settings.tab.custom')}
      </button>
      <button class="tab-btn {activeTab === 'tags' ? 'active' : ''}" onclick={() => activeTab = 'tags'}>
        <Icon name="tag" /> {t('settings.tab.tags')}
      </button>
      <button class="tab-btn {activeTab === 'train' ? 'active' : ''}" onclick={() => activeTab = 'train'}>
        <Icon name="cap" /> {t('settings.tab.train')}
      </button>
      <button class="tab-btn {activeTab === 'imgtrain' ? 'active' : ''}" onclick={() => activeTab = 'imgtrain'}>
        <Icon name="image" /> {t('settings.tab.imgtrain')}
      </button>
    </div>

    <div class="toolbar-actions">
      <button class="btn-tool" onclick={() => api.openModelsFolder()} title={t('st.openModelsTitle')}>
        <Icon name="models" /> {t('launcher.modelsFolder')}
      </button>
      <button class="btn-tool" onclick={() => api.openStFolder()} title={t('st.openStTitle')}>
        <Icon name="folder" /> SillyTavern
      </button>
      <button class="btn-tool" onclick={handleRefreshAndUpdate} disabled={checkingUpdate} title={t('st.refreshTitle')}>
        {#if checkingUpdate}
          <span class="btn-spinner"></span> {t('settings.checking')}
        {:else}
          <Icon name="retry" /> {t('settings.refresh')}
        {/if}
      </button>
    </div>
  </div>
  {/if}

  <div class="settings-main">
  <!-- Error or success notifications -->
  {#if updateNotice}
    <div class="toast toast-{updateNotice.type}">
      <span>{updateNotice.message}</span>
      {#if updateNotice.closable}
        <button class="toast-close" onclick={() => (updateNotice = null)}>✕</button>
      {/if}
    </div>
  {/if}
  {#if error}
    <div class="toast toast-error">
      <span>⚠️ {error}</span>
      <button class="toast-close" onclick={() => (error = null)}>✕</button>
    </div>
  {/if}
  {#if saveSuccess}
    <div class="toast toast-success">
      <span>{t('st.saved')}</span>
    </div>
  {/if}

  <!-- Tab contents -->
  <main class="settings-content" bind:this={settingsContent}>
    {#if loading}
      <div class="loading-box">
        <div class="spinner"></div>
        <p>{t('st.loadingModels')}</p>
      </div>
    {:else if activeTab === 'chat' || activeTab === 'embed' || activeTab === 'vision'}
      {#if setup}
        <div class="hardware-note">
          <b>{setup.hardware.name ?? t('st.noGpu')}</b>
          {#if setup.hardware.vram_mb}
            · {t('st.vramGb', { gb: (setup.hardware.vram_mb / 1024).toFixed(0) })}
          {/if}
          {t('st.fitsOnly')}
          <!-- The memory and context banners concern the chat model: on the vision
               and memory tabs they are about a different role and only confuse. -->
          {#if activeTab === 'chat'}
            {#if gpuUsage?.loaded}
              <div class="hw-warn">
                {t('st.inMemory', { model: gpuUsage.model ?? '' })}
                {gpuUsage.on_gpu
                  ? t('st.gpuAccel', { gb: (gpuUsage.size_vram_mb / 1024).toFixed(1) })
                  : t('st.cpuSlow')}
              </div>
            {/if}
            {#if gpuUsage && !gpuUsage.model_fits && gpuUsage.vram_needed_mb > 0}
              <div class="hw-warn hw-bad">
                {t('st.notFit', { model: settings?.chat_model ?? '', free: (gpuUsage.vram_free_mb / 1024).toFixed(1), total: (gpuUsage.vram_total_mb / 1024).toFixed(1), need: (gpuUsage.vram_needed_mb / 1024).toFixed(1) })}
                {#if gpuUsage.loaded && sameTag(gpuUsage.model ?? '', settings?.chat_model ?? '')}
                  {t('st.notFitOffload')}
                {:else}
                  {t('st.notFitOther')}
                {/if}
              </div>
            {/if}
            {#if contextCeiling > 0}
              <div class="hw-warn">
                {t('st.ctxFree', { free: (gpuUsage?.vram_free_mb ?? 0) / 1024 > 0.1 ? `${((gpuUsage?.vram_free_mb ?? 0) / 1024).toFixed(1)} ${t('unit.gb')}` : t('st.lessThan'), total: ((gpuUsage?.vram_total_mb ?? 0) / 1024).toFixed(1) })}
                {t('st.ctxFits', { model: gpuUsage?.model ?? settings?.chat_model ?? '', tokens: contextCeiling.toLocaleString(numberLocale) })}
                {#if contextTooBig}
                  {t('st.ctxTooBig', { tokens: (settings?.context_size ?? 0).toLocaleString(numberLocale) })}
                {/if}
              </div>
            {/if}
          {/if}
          {#each setup.hardware.notes as note}
            <div class="hw-warn">{note}</div>
          {/each}
        </div>
      {/if}
      <!-- Previous-series model update flow: the suggestion before the catalog. -->
      {#each upgradeOffers.filter((o) => o.category === activeTab) as offer (offer.from)}
        <div class="upgrade-card">
          <div class="upgrade-title">{t('st.upg.title')}</div>
          <p class="upgrade-text">
            <b>{offer.from}</b> → <b>{offer.to}</b> —
            {#if offer.to_installed}
              {t('st.upg.installed')}
            {:else}
              {t('st.upg.better')}
            {/if}
          </p>
          <div class="upgrade-actions">
            <button
              class="btn-tool"
              disabled={upgradingModel !== null || deletingModel !== null}
              onclick={() => upgradeModel(offer)}
            >
              {#if upgradingModel === offer.from}
                {t('st.upg.updating')}
              {:else if offer.to_installed}
                {t('st.upg.switch')}
              {:else}
                {t('st.upg.update')}
              {/if}
            </button>
            <button
              class="btn-tool"
              disabled={upgradingModel !== null || deletingModel !== null}
              onclick={() => deleteModel(offer.from)}
            >
              {t('st.upg.deleteOnly')}
            </button>
            <button class="btn-tool" disabled={upgradingModel !== null} onclick={() => dismissUpgrade(offer)}>
              {t('common.later')}
            </button>
          </div>
        </div>
      {/each}
      <div class="models-grid">
        {#each catalogModels.filter((m) => m.category === activeTab) as m}
          {@const installed = isInstalled(m.tag)}
          {@const active = isActive(m.tag, m.category)}
          {@const task = downloadTask('ollama', m.tag)}
          {@const isDown = isActiveTask(task)}
          {@const isDel = deletingModel === m.tag}

          <div class="model-card {active ? 'card-active' : ''} {installed ? 'card-installed' : ''}">
            <div class="card-header">
              <div class="title-group">
                <div class="title-row">
                  <h3 class="model-name">{m.name}</h3>
                  {#if active}
                    <span class="badge-active">{t('st.badge.active')}</span>
                  {:else if installed}
                    <span class="badge-installed">{t('st.badge.installed')}</span>
                  {:else if !installedKnown}
                    <span class="badge-not-installed">{t('st.badge.unknown')}</span>
                  {:else}
                    <span class="badge-not-installed">{t('st.badge.notDownloaded')}</span>
                  {/if}
                  {#if m.recommended}
                    <span class="badge-active">{t('st.badge.recommended')}</span>
                  {/if}
                </div>
                <span class="model-tag-sub">{m.tag}</span>
              </div>
              <span class="badge-vram">{m.badge}</span>
            </div>

            <p class="model-desc">{m.description}</p>

            <div class="roleplay-note">
              <span class="rp-label">{t('st.roleplay')}</span> {m.roleplay_style}
            </div>

            <div class="card-metrics">
              <div class="metric"><span class="m-label">VRAM:</span> <b>{m.vram}</b></div>
              <div class="metric"><span class="m-label">{t('st.weights')}</span> <b>{m.size}</b></div>
              <div class="metric"><span class="m-label">{t('st.quant')}</span> <b>{m.quant}</b></div>
            </div>

            <!-- Download progress section: for this model, including failure -->
            {#if task && (isDown || isFailedTask(task))}
              {@render downloadBox(task)}
            {/if}

            <div class="card-footer">
              <div class="footer-left">
                {#if active}
                  <button class="btn-selected" disabled>{t('st.selected')}</button>
                {:else if installed}
                  <button class="btn-select" onclick={() => selectModel(m.tag, m.category)}>
                    {t('st.selectModel')}
                  </button>
                {:else if !installedKnown}
                  <button class="btn-download" disabled title={t('st.waitOllamaTitle')}>
                    {t('st.waitOllama')}
                  </button>
                {:else}
                  <button
                    class="btn-download"
                    disabled={isDown}
                    onclick={() => startDownload(m.tag)}
                    title={isDown ? t('st.inQueueTitle') : ''}
                  >
                    {t('st.downloadSize', { size: m.size })}
                  </button>
                {/if}
              </div>

              <div class="footer-right">
                {#if installed && !active}
                  <button
                    class="btn-delete"
                    disabled={isDel || isDown}
                    onclick={() => deleteModel(m.tag)}
                    title={t('st.deleteWeightsTitle')}
                  >
                    {isDel ? t('st.deleting') : t('st.delete')}
                  </button>
                {/if}
              </div>
            </div>
          </div>
        {/each}
      </div>

      {#if !installedKnown}
        <p class="hint" style="margin-top: 12px;">
          {t('st.ollamaStarting')}
        </p>
      {/if}

      {#if (activeTab === 'embed' || activeTab === 'vision') && extraRoleModels.length > 0}
        <div class="param-card extra-models">
          <h3>{t('st.extra.title')}</h3>
          <p class="param-sub">
            {t('st.extra.text')}
          </p>
          <div class="custom-input-row">
            <select bind:value={extraModelTag}>
              <option value="">{t('st.extra.placeholder')}</option>
              {#each extraRoleModels as tag}
                <option value={tag}>{tag}{isActive(tag, activeTab) ? ' ✓' : ''}</option>
              {/each}
            </select>
            <button
              class="btn-primary"
              disabled={!extraModelTag}
              onclick={() => selectModel(extraModelTag, activeTab === 'embed' ? 'embed' : 'vision')}
            >
              {t('st.extra.select')}
            </button>
          </div>
        </div>
      {/if}

      {#if activeTab === 'chat' && extraChatModels.length > 0}
        <div class="param-card extra-models">
          <h3>{t('st.extra.title')}</h3>
          <p class="param-sub">
            {t('st.extra.text')}
          </p>
          <div class="custom-input-row">
            <select bind:value={extraModelTag}>
              <option value="">{t('st.extra.placeholder')}</option>
              {#each extraChatModels as tag}
                <option value={tag}>{tag}</option>
              {/each}
            </select>
            <button
              class="btn-primary"
              disabled={!extraModelTag}
              onclick={() => selectModel(extraModelTag, 'chat')}
            >
              {t('st.extra.select')}
            </button>
          </div>
        </div>
      {/if}

    {:else if activeTab === 'image'}
      {#if setup && !setup.images_available}
        <div class="param-card">
          <h3>{t('st.img.unavailableTitle')}</h3>
          <p class="param-sub">
            {t('st.img.unavailableText')}
          </p>
          {#each setup.hardware.notes as note}
            <p class="hint">{note}</p>
          {/each}
        </div>
      {:else}
      <div class="models-grid">
        <!-- ComfyUI status banner -->
        <div class="comfy-status-card">
          <div class="comfy-status-info">
            <div class="comfy-status-badge {comfyStatus.running ? 'online' : comfyStatus.installed || !comfyChecked ? 'offline' : 'not-installed'}">
              <span class="status-dot"></span>
              <b>
                {#if !comfyChecked}
                  {t('st.comfy.checking')}
                {:else if comfyStatus.running}
                  {t('st.comfy.running')}
                {:else if comfyStatus.installed}
                  {t('st.comfy.stopped')}
                {:else if comfyStatus.partial_bytes > 0}
                  {t('st.comfy.partial', { size: formatSize(comfyStatus.partial_bytes) + (comfyStatus.partial_total_bytes > 0 ? ` / ${formatSize(comfyStatus.partial_total_bytes)}` : '') })}
                {:else}
                  {t('st.comfy.missing')}
                {/if}
              </b>
            </div>
            <p class="comfy-desc">
              {t('st.comfy.desc')} <code>/imagine</code>.
            </p>

            {#if installingComfy}
              <div class="download-box" style="margin-top: 14px;">
                <div class="progress-bar-wrap">
                  <div
                    class="progress-bar-fill"
                    style:width="{comfyInstallProgress.total > 0 ? (comfyInstallProgress.completed / comfyInstallProgress.total) * 100 : 25}%"
                  ></div>
                </div>
                <div class="progress-meta">
                  <span class="p-status">{comfyInstallProgress.status}</span>
                  {#if comfyInstallProgress.total > 0}
                    <span class="p-details">
                      {formatSize(comfyInstallProgress.completed)} / {formatSize(comfyInstallProgress.total)} · {((comfyInstallProgress.completed / comfyInstallProgress.total) * 100).toFixed(0)}%
                    </span>
                  {/if}
                </div>
                <button class="btn-cancel" onclick={cancelComfyInstall}>{t('st.dl.cancel')}</button>
              </div>
            {/if}
          </div>
          <div class="comfy-status-actions">
            {#if !comfyChecked}
              <button class="btn-start-comfy" disabled>
                <span class="btn-spinner"></span> {t('st.checking')}
              </button>
            {:else if !comfyStatus.installed}
              <button class="btn-start-comfy" onclick={handleInstallComfy} disabled={installingComfy}>
                {#if installingComfy}
                  <span class="btn-spinner"></span> {t('st.installing')}
                {:else if comfyStatus.partial_bytes > 0}
                  {t('st.comfy.resume')}
                {:else}
                  {t('st.comfy.install')}
                {/if}
              </button>
            {:else if !comfyStatus.running}
              <button class="btn-start-comfy" onclick={handleStartComfy} disabled={startingComfy}>
                {#if startingComfy}
                  <span class="btn-spinner"></span> {t('st.starting')}
                {:else}
                  {t('st.comfy.start')}
                {/if}
              </button>
            {:else}
              <span class="comfy-ready-tag">{t('st.comfy.ready')}</span>
            {/if}
          </div>
        </div>

        {#each imageModels as m}
          {@const task = downloadTask('image', m.filename)}
          {@const isDown = isActiveTask(task)}
          <!-- The card is a container, not a button: role="button" with nested
               <button> produced nested interactive elements, and Space on the
               focused card scrolled the page, while another button's key
               bubbled up as a model selection. Selection is the radio button below. -->
          <div
            class="model-card {m.is_active ? 'card-active' : ''} {m.is_installed ? 'card-installed' : ''}"
          >
            <div class="card-header">
              <div class="title-group">
                <div class="title-row">
                  {#if m.is_installed}
                    <!-- A real selection control: Tab + Space/Enter work
                         as usual without scrolling the page. -->
                    <input
                      type="radio"
                      name="active_image_model"
                      checked={m.is_active}
                      onchange={() => selectImageModel(m.filename)}
                      aria-label={t('st.selectModelNamed', { name: m.name })}
                      style="margin-right: 6px; accent-color: var(--text); cursor: pointer;"
                    />
                  {/if}
                  <h3 class="model-name">{m.name}</h3>
                  {#if m.is_active}
                    <span class="badge-active">{t('st.badge.active')}</span>
                  {:else if m.is_installed}
                    <span class="badge-installed">{t('st.badge.installed')}</span>
                  {:else}
                    <span class="badge-not-installed">{t('st.badge.notDownloaded')}</span>
                  {/if}
                </div>
                <span class="model-tag-sub">{m.filename}</span>
              </div>
              <span class="badge-vram">{m.badge}</span>
            </div>

            <p class="model-desc">{m.description}</p>

            <div class="roleplay-note">
              <span class="rp-label">{t('st.genStyle')}</span> {m.roleplay_style}
            </div>

            <div class="card-metrics">
              <div class="metric"><span class="m-label">{t('st.weights')}</span> <b>{m.size}</b></div>
              <div class="metric"><span class="m-label">{t('st.arch')}</span> <b>{m.vram_tier === 'sd15' ? 'SD 1.5 Checkpoint' : m.vram_tier === 'sdxl' ? 'SDXL Checkpoint' : 'Checkpoint'}</b></div>
              <div class="metric"><span class="m-label">{t('st.censor')}</span> <b style="color: var(--ok);">{t('st.noLimits')}</b></div>
              <div class="metric"><span class="m-label">VRAM:</span> <b>{m.vram_tier === 'sd15' ? `8 ${t('unit.gb')}+` : m.vram_tier === 'sdxl' ? `12 ${t('unit.gb')}+` : t('st.auto')}</b></div>
            </div>

            {#if task && (isDown || isFailedTask(task))}
              {@render downloadBox(task)}
            {/if}

            <div class="card-actions">
              {#if m.is_active}
                <button class="btn-use active" disabled>{t('st.activeModel')}</button>
              {:else if m.is_installed}
                <button class="btn-use" onclick={() => selectImageModel(m.filename)}>
                  {t('st.selectModel')}
                </button>
              {:else}
                <button
                  class="btn-download"
                  disabled={isDown}
                  onclick={() => handleDownloadImageModel(m.filename)}
                >
                  {t('st.downloadSize', { size: m.size })}
                </button>
              {/if}
              {#if m.is_installed}
                <button
                  class="btn-delete"
                  disabled={deletingImageModel === m.filename}
                  onclick={() => handleDeleteImageModel(m.filename)}
                  title={t('st.deleteCkptTitle')}
                >
                  {deletingImageModel === m.filename ? '…' : t('st.delete')}
                </button>
              {/if}
            </div>
          </div>
        {/each}

        <!-- Informational banner for the chat -->
        <div class="image-tips-card">
          <h4>{t('st.tips.title')}</h4>
          <div class="tips-grid">
            <div class="tip-item">
              <span class="tip-num">1</span>
              <div>
                <b>{t('st.tips.1.title')}</b>
                <p>{t('st.tips.1.text')}</p>
              </div>
            </div>
            <div class="tip-item">
              <span class="tip-num">2</span>
              <div>
                <b>{t('st.tips.2.title')}</b>
                <p>{t('st.tips.2.text')}</p>
              </div>
            </div>
            <div class="tip-item">
              <span class="tip-num">3</span>
              <div>
                <b>{t('st.tips.3.title')} <code>/imagine</code></b>
                <p>{t('st.tips.3.text', { cmd: `/imagine <${t('st.tips.3.prompt')}>` })}</p>
              </div>
            </div>
          </div>
        </div>
      </div>
      {/if}

    {:else if activeTab === 'tune'}
      <div class="params-container">
        <div class="param-card">
          <h3>{t('st.tune.title')}</h3>
          <p class="param-sub">
            {t('st.tune.text')}
          </p>

          {#if recommend}
            <div class="sys-grid">
              <div class="sys-row">
                <span class="sys-label">{t('st.tune.gpu')}</span>
                <b>{recommend.hardware.name}</b>
                <span class="sys-extra">{recommend.hardware.vram} · {recommend.hardware.backend} · {t('st.tune.tier')}: {recommend.hardware.tier}</span>
              </div>
              <div class="sys-row">
                <span class="sys-label">{t('st.tune.cpu')}</span>
                <b>{recommend.system.cpu}</b>
              </div>
              <div class="sys-row">
                <span class="sys-label">{t('st.tune.ram')}</span>
                <b>{(recommend.system.ram_total_mb / 1024).toFixed(1)} {t('unit.gb')}</b>
                <span class="sys-extra">
                  {t('st.tune.ramFree', { gb: (recommend.system.ram_free_mb / 1024).toFixed(1) })}
                </span>
              </div>
              {#if recommend.system.disk_free_gb >= 0}
                <div class="sys-row">
                  <span class="sys-label">{t('st.tune.disk')}</span>
                  <b>{t('st.tune.diskFree', { gb: recommend.system.disk_free_gb.toFixed(1) })}</b>
                  <span class="sys-extra">{t('st.tune.diskNote')}</span>
                </div>
              {/if}
              {#each recommend.hardware.notes as note}
                <div class="hint" style="margin-top: 2px;">{note}</div>
              {/each}
            </div>

            {#if recommend.items.length === 0}
              <div class="tune-ok">
                ✓ {recommend.matches
                  ? t('st.tune.matches')
                  : t('st.tune.nothing')}
              </div>
            {:else}
              {#if recommend.tuned_for && !recommend.matches}
                <div class="tune-warn">
                  {t('st.tune.otherHw')}
                </div>
              {/if}

              <div class="tune-list">
                {#each recommend.items as item (item.key)}
                  <div class="tune-item">
                    <div class="tune-head">
                      <b>{item.title}</b>
                      <span class="tune-diff">
                        {item.current} <span class="tune-arrow">→</span> <b>{item.recommended}</b>
                      </span>
                      <button
                        class="btn-use"
                        onclick={() => applyRecommended([item.key])}
                        disabled={applyingRecommend}
                      >
                        {t('st.tune.apply')}
                      </button>
                    </div>
                    <p class="tune-why">{item.reason}</p>
                  </div>
                {/each}
              </div>

              <div class="action-row">
                <button class="btn-primary" onclick={() => applyRecommended()} disabled={applyingRecommend}>
                  {#if applyingRecommend}
                    <span class="btn-spinner"></span> {t('st.tune.applying')}
                  {:else}
                    {t('st.tune.applyAll', { count: recommend.items.length })}
                  {/if}
                </button>
                <button class="btn-tool" onclick={() => { loadRecommendations(); loadRecommendedDownloads(); }} disabled={applyingRecommend}>
                  <Icon name="retry" size={15} /> {t('st.tune.recalc')}
                </button>
              </div>
            {/if}

            {#if recommendedDownloads.length > 0 || downloads.running}
              <div class="tune-optional">
                <div class="hint" style="margin-bottom: 6px;">
                  <b>{t('st.tune.optional')}</b> {t('st.tune.optionalText')}
                </div>
                {#if recommendedDownloads.length > 0}
                  <ul class="tune-downloads">
                  {#each recommendedDownloads as item (item.id)}
                    {@const task = downloadTask(item.kind, item.id)}
                    <li class="tune-download-row">
                      <div class="tdr-head">
                        <span class="tdr-title">{item.title} — {item.size}</span>
                        <span class="tdr-status {task?.status ?? ''}">
                          {task ? downloadNote(task) : t('st.tune.notDownloaded')}
                        </span>
                      </div>
                      {#if task?.error}
                        <div class="tdr-error">{task.error}</div>
                      {/if}
                      {#if task && (isActiveTask(task) || isFailedTask(task))}
                        <div class="progress-bar-wrap thin">
                          <div
                            class="progress-bar-fill {task.status === 'paused' ? 'is-paused' : ''}"
                            style:width="{downloadPercent(task)}%"
                          ></div>
                        </div>
                        <div class="progress-meta">
                          <span class="p-details">{downloadDetails(task)}</span>
                        </div>
                      {/if}
                    </li>
                  {/each}
                  </ul>
                {/if}
                <div class="action-row">
                  {#if recommendedDownloads.length > 0}
                    <button class="btn-tool" onclick={downloadRecommended} disabled={downloadingRecommended}>
                      {#if downloadingRecommended}
                        <span class="btn-spinner"></span> {t('st.tune.downloading')}
                      {:else}
                        {t('st.tune.downloadRec', { count: recommendedDownloads.length })}
                      {/if}
                    </button>
                  {/if}
                  {#if downloads.running}
                    {#if downloads.paused || activeDownloads.some((task) => task.status === 'paused')}
                      <button class="btn-tool" onclick={resumeDownloads}>{t('st.tune.resumeAll')}</button>
                    {:else}
                      <button class="btn-tool" onclick={pauseDownloads}>{t('st.dl.pause')}</button>
                    {/if}
                    <button class="btn-cancel" onclick={cancelDownloads}>{t('st.dl.cancel')}</button>
                  {/if}
                </div>
                {#if recommendedNote}
                  <div class="hint" style="margin-top: 6px;">{recommendedNote}</div>
                {/if}
              </div>
            {/if}
          {:else}
            <div class="loading-box">
              <div class="spinner"></div>
              <p>{t('st.tune.loading')}</p>
            </div>
          {/if}
        </div>
      </div>

    {:else if activeTab === 'params'}
      <div class="params-container">
        <div class="param-card">
          <h3>{t('st.par.title')}</h3>
          <p class="param-sub">
            {t('st.par.text')}
          </p>

          {#if settings}
            <div class="form-group">
              <label for="ctx-size">
                <b>{t('st.par.ctx')}</b>
                <span class="hint">{t('st.par.ctxHint')}</span>
              </label>
              <select id="ctx-size" bind:value={settings.context_size}>
                <option value={8192}>{t('st.par.ctx8')}</option>
                <option value={16384}>{t('st.par.ctx16')}</option>
                <option value={32768}>{t('st.par.ctx32')}</option>
              </select>
              {#if contextTooBig}
                <span class="hint" style="color: var(--warn);">
                  {t('st.par.ctxCap', { model: gpuUsage?.model ?? settings.chat_model, tokens: contextCeiling.toLocaleString(numberLocale) })}
                </span>
              {/if}
            </div>

            <div class="form-group">
              <label for="max-tokens">
                <b>{t('st.par.maxTokens')}</b>
                <span class="hint">
                  {t('st.par.maxTokensHint')}
                </span>
              </label>
              <select id="max-tokens" bind:value={settings.max_tokens}>
                <option value={400}>{t('st.par.mt400')}</option>
                <option value={600}>{t('st.par.mt600')}</option>
                <option value={800}>800</option>
                <option value={1000}>{t('st.par.mt1000')}</option>
                <option value={1500}>{t('st.par.mt1500')}</option>
              </select>
            </div>

            <div class="form-group">
              <label for="temp">
                <b>{t('st.par.temp', { value: settings.temperature })}</b>
                <span class="hint">{t('st.par.tempHint')}</span>
              </label>
              <input
                id="temp"
                type="range"
                min="0.4"
                max="1.2"
                step="0.05"
                bind:value={settings.temperature}
              />
            </div>

            <div class="form-group">
              <label for="top-p">
                <b>{t('st.par.topP', { value: settings.top_p })}</b>
                <span class="hint">{t('st.par.topPHint')}</span>
              </label>
              <input
                id="top-p"
                type="range"
                min="0.5"
                max="1.0"
                step="0.05"
                bind:value={settings.top_p}
              />
            </div>

            <div class="form-group" style="margin-top: 16px;">
              <label for="auto-launch-cb" class="tagger-toggle">
                <input
                  id="auto-launch-cb"
                  type="checkbox"
                  class="custom-check"
                  bind:checked={settings.auto_launch_st}
                />
                <div>
                  <b>{t('st.par.autoLaunch')}</b>
                  <span class="hint" style="display: block; margin-top: 2px;">
                    {t('st.par.autoLaunchHint')}
                  </span>
                </div>
              </label>
            </div>

            <div class="action-row">
              <button class="btn-primary" onclick={saveInferenceParams}>
                {t('st.par.save')}
              </button>
            </div>
          {/if}
        </div>
      </div>

    {:else if activeTab === 'tags'}
      <div class="params-container">
        <div class="param-card">
          <h3>{t('st.tag.title')}</h3>
          <p class="param-sub">
            {t('st.tag.text')}
          </p>

          {#if settings}
            <div class="form-group tagger-field">
              <label for="tagger-enabled" class="tagger-toggle">
                <input
                  id="tagger-enabled"
                  type="checkbox"
                  class="custom-check"
                  bind:checked={settings.tagger_enabled}
                />
                <div>
                  <b>{t('st.tag.auto')}</b>
                  <span class="hint" style="display: block; margin-top: 2px;">
                    {t('st.tag.autoHint')}
                  </span>
                </div>
              </label>
            </div>

            <div class="form-group">
              <label for="tagger-model">
                <b>{t('st.tag.model')}</b>
                <span class="hint">{t('st.tag.modelHint')}</span>
              </label>
              <select id="tagger-model" bind:value={settings.tagger_model}>
                <option value="">{t('st.tag.sameAsChat', { model: settings.chat_model })}</option>
                {#each chatModelOptions(settings.tagger_model) as m}
                  <option value={m}>{m}</option>
                {/each}
              </select>
            </div>

            <div class="form-group">
              <label for="tagger-max">
                <b>{t('st.tag.max')}</b>
                <span class="hint">{t('st.tag.maxHint')}</span>
              </label>
              <input id="tagger-max" type="number" min="3" max="20" bind:value={settings.tagger_max_tags} />
            </div>

            <div class="form-group">
              <label for="tagger-language">
                <b>{t('st.tag.lang')}</b>
                <span class="hint">{t('st.tag.langHint')}</span>
              </label>
              <select id="tagger-language" bind:value={settings.tagger_language}>
                <option value="ru">{t('st.tag.ru')}</option>
                <option value="en">{t('st.tag.en')}</option>
                <option value="both">{t('st.tag.both')}</option>
              </select>
            </div>

            <div class="form-group tagger-field">
              <label for="tagger-card-tags" class="tagger-toggle">
                <input
                  id="tagger-card-tags"
                  type="checkbox"
                  class="custom-check"
                  bind:checked={settings.tagger_write_card_tags}
                />
                <div>
                  <b>{t('st.tag.card')}</b>
                  <span class="hint" style="display: block; margin-top: 2px;">
                    {t('st.tag.cardHint')}
                  </span>
                </div>
              </label>
            </div>

            <div class="form-group tagger-field">
              <label for="tagger-auto-translate" class="tagger-toggle">
                <input
                  id="tagger-auto-translate"
                  type="checkbox"
                  class="custom-check"
                  bind:checked={settings.auto_translate_imports}
                />
                <div>
                  <b>{t('st.tag.translate')}</b>
                  <span class="hint" style="display: block; margin-top: 2px;">
                    {t('st.tag.translateHint')}
                  </span>
                </div>
              </label>
            </div>

            <div class="form-group">
              <label for="tagger-prompt">
                <b>{t('st.tag.prompt')}</b>
                <span class="hint">{t('st.tag.promptHint')}</span>
              </label>
              <textarea id="tagger-prompt" rows="3" bind:value={settings.tagger_prompt}></textarea>
            </div>

            <div class="action-row">
              <button class="btn-primary" onclick={saveInferenceParams}>
                {t('st.tag.save')}
              </button>
            </div>
          {/if}
        </div>

        <div class="param-card">
          <h3>{t('st.eval.title')}</h3>
          <p class="param-sub">
            {t('st.eval.text')}
          </p>

          {#if settings}
            <div class="form-group">
              <label for="evaluator-model">
                <b>{t('st.eval.model')}</b>
                <span class="hint">
                  {t('st.eval.modelHint')}
                </span>
              </label>
              <select id="evaluator-model" bind:value={settings.evaluator_model}>
                <option value="">{t('st.eval.auto')}</option>
                {#each chatModelOptions(settings.evaluator_model) as m}
                  <option value={m}>{m}</option>
                {/each}
              </select>
            </div>

            <div class="form-group">
              <label for="director-prompt">
                <b>{t('st.eval.director')}</b>
                <span class="hint">{t('st.eval.directorHint')}</span>
              </label>
              <textarea id="director-prompt" rows="3" bind:value={settings.director_prompt}></textarea>
            </div>

            <div class="form-group">
              <label for="rpg-evaluator-prompt">
                <b>{t('st.eval.rpg')}</b>
                <span class="hint">{t('st.eval.rpgHint')}</span>
              </label>
              <textarea id="rpg-evaluator-prompt" rows="3" bind:value={settings.rpg_evaluator_prompt}></textarea>
            </div>

            <div class="form-group">
              <label for="rpg-combat-prompt">
                <b>{t('st.eval.combat')}</b>
                <span class="hint">{t('st.eval.combatHint')}</span>
              </label>
              <textarea id="rpg-combat-prompt" rows="3" bind:value={settings.rpg_combat_prompt}></textarea>
            </div>

            <div class="action-row">
              <button class="btn-primary" onclick={saveInferenceParams}>
                {t('st.eval.save')}
              </button>
            </div>
          {/if}
        </div>

        <div class="param-card">
          <h3>{t('st.tr.title')}</h3>
          <p class="param-sub">
            {t('st.tr.text')}
          </p>

          {#if traitCharacters.length === 0}
            <div class="tune-ok">{t('st.tr.empty')}</div>
          {:else}
            <div class="form-group">
              <label for="traits-character">
                <b>{t('st.tr.character')}</b>
                <span class="hint">{t('st.tr.characterHint')}</span>
              </label>
              <select id="traits-character" bind:value={selectedTraitAvatar} onchange={onTraitCharacterChange}>
                {#each traitCharacters as c}
                  <option value={c.avatar}>{c.name} ({c.avatar})</option>
                {/each}
              </select>
            </div>

            {#if selectedTraits}
              {#if selectedTraits.tags.length === 0}
                <div class="tune-warn">
                  {t('st.tr.noTags')}
                </div>
              {/if}

              {#if selectedTraits.tags.length > 0}
                <div class="traits-list">
                  {#each selectedTraits.tags as tag (tag.tag)}
                    <div class="trait-row">
                      <span class="trait-name" title={tag.tag}>{tag.tag}</span>
                      <input
                        type="range"
                        min="0"
                        max="100"
                        step="5"
                        bind:value={tag.intensity}
                        aria-label={t('st.tr.intensity', { tag: tag.tag })}
                      />
                      <span class="trait-value {tag.intensity === 0 ? 'trait-off' : ''}">{tag.intensity}%</span>
                      <button
                        class="trait-remove"
                        title={t('st.tr.removeTag')}
                        onclick={() => removeTraitTag(tag.tag)}
                      >✕</button>
                    </div>
                  {/each}
                </div>
              {/if}

              <div class="trait-add">
                <input
                  type="text"
                  placeholder={t('st.tr.placeholder')}
                  bind:value={newTraitTag}
                  onkeydown={(e) => { if (e.key === 'Enter') addTraitTag(); }}
                />
                <button class="btn-tool" onclick={addTraitTag}>{t('st.tr.add')}</button>
                {#if selectedTraits.st_tags.length > 0}
                  <button
                    class="btn-tool"
                    onclick={pullStTags}
                    title={t('st.tr.pullTitle')}
                  >{t('st.tr.pull')}</button>
                {/if}
              </div>

              <div class="form-group">
                <label for="traits-chaos">
                  <b>{t('st.tr.chaos', { value: selectedTraits.chaos })}</b>
                  <span class="hint">
                    {t('st.tr.chaosHint')}
                  </span>
                </label>
                <input id="traits-chaos" type="range" min="0" max="100" step="5" bind:value={selectedTraits.chaos} />
              </div>

              <div class="form-group tagger-field">
                <label for="traits-live" class="tagger-toggle">
                  <input
                    id="traits-live"
                    type="checkbox"
                    class="custom-check"
                    bind:checked={selectedTraits.live}
                  />
                  <div>
                    <b>{t('st.tr.live')}</b>
                    <span class="hint" style="display: block; margin-top: 2px;">
                      {t('st.tr.liveHint')}
                    </span>
                  </div>
                </label>
              </div>

              {#if selectedTraits.live}
                <div class="traits-state">
                  <div class="hint" style="margin-bottom: 6px;">
                    {t('st.tr.stateHint')}
                  </div>
                  <div class="state-row">
                    <span class="state-name">{t('st.tr.affection')}</span>
                    <input type="range" min="0" max="100" step="5" bind:value={selectedTraits.state.affection} />
                    <span class="state-value">{selectedTraits.state.affection}</span>
                  </div>
                  <div class="state-row">
                    <span class="state-name">{t('st.tr.trust')}</span>
                    <input type="range" min="0" max="100" step="5" bind:value={selectedTraits.state.trust} />
                    <span class="state-value">{selectedTraits.state.trust}</span>
                  </div>
                  <div class="state-row">
                    <span class="state-name">{t('st.tr.energy')}</span>
                    <input type="range" min="0" max="100" step="5" bind:value={selectedTraits.state.energy} />
                    <span class="state-value">{selectedTraits.state.energy}</span>
                  </div>
                  <div class="state-row">
                    <span class="state-name">{t('st.tr.irritation')}</span>
                    <input type="range" min="0" max="100" step="5" bind:value={selectedTraits.state.irritation} />
                    <span class="state-value">{selectedTraits.state.irritation}</span>
                  </div>
                  <div class="action-row" style="margin-top: 8px;">
                    <button class="btn-tool" onclick={resetTraitState} disabled={savingTraits}>
                      {t('st.tr.resetState')}
                    </button>
                    <button class="btn-tool" onclick={refreshTraits} disabled={savingTraits}>
                      <Icon name="retry" /> {t('settings.refresh')}
                    </button>
                  </div>
                </div>
              {/if}

              <div class="form-group tagger-field">
                <label for="traits-no-repeat" class="tagger-toggle">
                  <input
                    id="traits-no-repeat"
                    type="checkbox"
                    class="custom-check"
                    bind:checked={selectedTraits.no_repeat}
                  />
                  <div>
                    <b>{t('st.tr.noRepeat')}</b>
                    <span class="hint" style="display: block; margin-top: 2px;">
                      {t('st.tr.noRepeatHint')}
                    </span>
                  </div>
                </label>
              </div>

              <div class="form-group tagger-field">
                <div>
                  <b>{t('st.bond.title')}</b>
                  <span class="hint" style="display: block; margin-top: 2px;">
                    {t('st.bond.text')}
                  </span>
                </div>

                {#if selectedBonds.length > 0}
                  <div class="traits-list">
                    {#each selectedBonds as bond, i}
                      <div class="bond-item">
                        <div class="bond-row">
                          <select class="bond-target" bind:value={bond.target} aria-label={t('st.bond.character')}>
                            <option value="" disabled>{t('st.bond.choose')}</option>
                            {#each bondCandidates as candidate (candidate.avatar)}
                              <option
                                value={candidate.avatar}
                                disabled={usedTargets.has(candidate.avatar) && candidate.avatar !== bond.target}
                              >
                                {candidate.name}
                              </option>
                            {/each}
                          </select>
                          <input
                            type="text"
                            class="bond-kind"
                            placeholder={t('st.bond.kindPlaceholder')}
                            maxlength="40"
                            bind:value={bond.kind}
                            aria-label={t('st.bond.kind')}
                          />
                          <input
                            type="range"
                            min="0"
                            max="100"
                            step="5"
                            bind:value={bond.strength}
                            aria-label={t('st.bond.strength')}
                          />
                          <span class="trait-value {bond.strength === 0 ? 'trait-off' : ''}">
                            {bond.strength}%
                          </span>
                          <button
                            class="trait-remove"
                            title={t('st.bond.remove')}
                            onclick={() => removeBond(i)}
                          >✕</button>
                        </div>
                        <label class="bond-note">
                          <span class="hint">{t('st.bond.note')}</span>
                          <input
                            type="text"
                            maxlength="200"
                            placeholder={t('st.bond.notePlaceholder')}
                            bind:value={bond.note}
                          />
                        </label>
                        <label for="bond-{i}-secret" class="tagger-toggle bond-secret">
                          <input
                            id="bond-{i}-secret"
                            type="checkbox"
                            class="custom-check"
                            bind:checked={bond.secret}
                          />
                          <div>
                            <b>{t('st.bond.secret')}</b>
                            <span class="hint" style="display: block; margin-top: 2px;">
                              {t('st.bond.secretHint')}
                            </span>
                          </div>
                        </label>
                      </div>
                    {/each}
                  </div>
                {:else}
                  <div class="tune-ok">
                    {t('st.bond.none')}
                  </div>
                {/if}

                <div class="action-row">
                  <button
                    class="btn-tool"
                    onclick={addBond}
                    disabled={selectedBonds.length >= MAX_BONDS || savingBonds}
                  >{t('st.bond.add')}</button>
                  {#if selectedBonds.length > 0}
                    <button class="btn-tool" onclick={clearBonds} disabled={savingBonds}>
                      {t('st.bond.clear')}
                    </button>
                  {/if}
                  <button class="btn-primary" onclick={saveBonds} disabled={savingBonds}>
                    {#if savingBonds}
                      <span class="btn-spinner"></span> {t('st.saving')}
                    {:else}
                      {t('st.bond.save')}
                    {/if}
                  </button>
                </div>
              </div>

              {#if traitsPreview}
                <div class="traits-preview">
                  <div class="hint" style="margin-bottom: 6px;">
                    {t(traitsDirty || bondsDirty ? 'st.tr.previewAfterSave' : 'st.tr.preview')} {t('st.tr.previewLive')}
                  </div>
                  <pre>{traitsPreview}</pre>
                </div>
              {:else}
                <div class="tune-ok">
                  {t('st.tr.notSet')}
                </div>
              {/if}

              <div class="action-row">
                <button class="btn-primary" onclick={saveTraits} disabled={savingTraits}>
                  {#if savingTraits}
                    <span class="btn-spinner"></span> {t('st.saving')}
                  {:else}
                    {t('st.tr.save')}
                  {/if}
                </button>
                <button class="btn-tool" onclick={resetTraits} disabled={savingTraits}>
                  {t('st.tr.reset')}
                </button>
              </div>
            {/if}
          {/if}
        </div>
      </div>

    {:else if activeTab === 'train'}
    <Training hardwareTier={setup?.hardware.tier ?? 'minimal'} />
  {:else if activeTab === 'imgtrain'}
    <ImageTraining hardwareTier={setup?.hardware.tier ?? 'minimal'} />
  {:else if activeTab === 'custom'}
      <div class="custom-container">
        <div class="param-card">
          <h3>{t('st.custom.title')}</h3>
          <p class="param-sub">
            {t('st.custom.text', { examples: 'gemma2:9b, mistral:7b, dolphin-mixtral', folder: 'models/' })}
          </p>

          <div class="custom-input-row">
            <input
              type="text"
              placeholder={t('st.custom.placeholder')}
              bind:value={customModelTag}
            />
            <button
              class="btn-primary"
              disabled={!customModelTag.trim()}
              onclick={() => startDownload(customModelTag.trim())}
            >
              {t('st.custom.start')}
            </button>
          </div>

          {#if activeDownloads.length > 0}
            <div class="download-box" style="margin-top: 20px;">
              {#each activeDownloads as task (task.kind + task.id)}
                <div class="progress-bar-wrap">
                  <div
                    class="progress-bar-fill {task.status === 'paused' ? 'is-paused' : ''}"
                    style:width="{downloadPercent(task)}%"
                  ></div>
                </div>
                <div class="progress-meta">
                  <span class="p-status">{task.title}: {downloadNote(task)}</span>
                  <span class="p-details">{downloadDetails(task)}</span>
                </div>
              {/each}
              <div class="download-actions">
                {#if downloads.paused || activeDownloads.some((task) => task.status === 'paused')}
                  <button class="btn-tool" onclick={resumeDownloads}>{t('st.dl.resume')}</button>
                {:else}
                  <button class="btn-tool" onclick={pauseDownloads}>{t('st.dl.pause')}</button>
                {/if}
                <button class="btn-cancel" onclick={cancelDownloads}>{t('st.dl.cancel')}</button>
              </div>
            </div>
          {/if}

          <!-- List of already downloaded models -->
          <div class="installed-list-box">
            <h4>{t('st.custom.installed')}</h4>
            {#if installedList.length === 0}
              <p class="dim-text">{t('st.custom.none')}</p>
            {:else}
              <div class="tags-cloud">
                {#each installedList as tag}
                  <div class="tag-badge">
                    <span>{tag}</span>
                    <button
                      class="tag-del-btn"
                      disabled={deletingModel === tag}
                      onclick={() => deleteModel(tag)}
                      title={t('st.custom.deleteTitle')}
                    >
                      ✕
                    </button>
                  </div>
                {/each}
              </div>
            {/if}
          </div>
        </div>

        <div class="param-card" style="margin-top: 16px;">
          <h3>{t('st.iso.title')}</h3>
          <p class="param-sub">
            {t('st.iso.text')}
          </p>
          <div class="action-row">
            <button class="btn-primary" onclick={checkIsolation} disabled={isolationChecking}>
              {isolationChecking ? t('st.iso.checking') : t('st.iso.check')}
            </button>
            <button class="btn-utility" onclick={() => api.openAppFolder()}>
              {t('st.iso.openFolder')}
            </button>
            <button class="btn-utility" onclick={createShortcut} disabled={shortcutBusy}>
              {shortcutBusy ? t('st.iso.creating') : t('st.iso.shortcut')}
            </button>
          </div>
          {#if shortcutMessage}
            <p class="param-sub" style="margin-top: 8px;">{shortcutMessage}</p>
          {/if}

          {#if isolationEntries}
            <div class="isolation-list">
              {#each isolationEntries as entry}
                <div
                  class="isolation-row"
                  class:isolation-bad={entry.outside && entry.exists}
                  class:isolation-good={entry.outside && !entry.exists}
                >
                  <span class="iso-state">{entry.outside ? (entry.exists ? '⚠' : '✓') : '•'}</span>
                  <span class="iso-label">{entry.label}</span>
                  <span class="iso-path" title={entry.path}>{entry.path}</span>
                  <span class="iso-size">{entry.exists ? formatSize(entry.size_bytes) : t('st.iso.none')}</span>
                </div>
              {/each}
            </div>
            {#if isolationEntries.some((e) => e.outside && e.exists)}
              <p class="hint">
                {t('st.iso.stale')}
              </p>
            {/if}
          {/if}
        </div>
      </div>
    {/if}
  </main>
  </div>
  </div>
</div>

<style>
  .settings-page {
    display: flex;
    flex-direction: column;
    height: 100vh;
    background: var(--bg);
    color: var(--text);
    font-family: var(--font);
    user-select: none;
    overflow: hidden;
  }

  /* The launcher is the only surface where this background is removed: there,
     under the panel, must be the window's shared aurora (body), not a second, more
     saturated instance of the same pattern. */
  .settings-page.embedded {
    height: 100%;
  }

  /* In the main window the header is one line with the section name: the launcher
     column already shows the logo and language. */
  .embedded .settings-header {
    flex-wrap: nowrap;
    min-height: 56px;
    padding-top: 10px;
    padding-bottom: 10px;
  }

  .embedded .settings-title {
    font-size: 1.1rem;
    white-space: nowrap;
  }

  .embedded .header-right {
    flex-wrap: nowrap;
    gap: 8px;
    min-width: 0;
  }

  .embedded .header-right .btn-tool {
    width: auto;
    gap: 8px;
    padding: 7px 14px;
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-pill);
    background: var(--btn-fill);
    box-shadow: var(--btn-shadow);
    font-size: 13px;
  }

  .embedded .header-right .btn-tool:hover {
    background: var(--btn-fill-hover);
  }

  .settings-page.launcher-view {
    background: none;
  }

  .settings-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    flex-wrap: wrap;
    gap: 12px 18px;
    /* On the right, room for the window buttons (WindowControls: 8 + 3 × 30 + 2 × 2 px). */
    padding: 16px 112px 16px 24px;
    /* The header sits right on the aurora: its bright highlights dropped white text to
       ~2.9:1. An inner pad over the glass dampens highlights without changing
       the material or layout. */
    box-shadow: none;
  }

  .header-left {
    display: flex;
    align-items: center;
    flex: 0 1 auto;
    min-width: 0;
    gap: 14px;
  }

  .crystal-icon {
    display: flex;
  }

  .settings-title {
    margin: 0;
    font-size: 1.28rem;
    font-weight: 700;
    letter-spacing: -0.02em;
    /* A text gradient looks nicer, but over glass highlights it dropped
       contrast in places: solid white passes on the dark header pad. */
    color: #ffffff;
  }

  .version-chip {
    display: inline-block;
    margin-left: 8px;
    padding: 2px 8px;
    border-radius: 999px;
    font-size: 11px;
    font-weight: 650;
    letter-spacing: 0.04em;
    vertical-align: 3px;
    color: var(--text);
    background: var(--surface-2);
    border: 1px solid var(--border-strong);
  }

  .settings-sub {
    margin: 2px 0 0;
    font-size: 0.8rem;
    /* #96989d held 2.1:1 over glass highlights; the lighter one is above the norm. */
    color: var(--text);
  }

  .header-right {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 12px;
  }

  .stat-pill {
    display: inline-flex;
    align-items: center;
    flex: 0 0 auto;
    gap: 8px;
    padding: 6px 12px;
    font-size: 0.8rem;
    /* Pure white: #f1f5f9 held 4.1–4.3 over glass highlights. */
    color: #ffffff;
  }

  .dot-live {
    flex: 0 0 auto;
    width: 7px;
    height: 7px;
    aspect-ratio: 1;
    border-radius: 50%;
    background: var(--ok);
    box-shadow: none;
  }

  .stat-dim {
    /* Contrast over glass highlights: #96989d — 3.9:1. */
    color: var(--text);
  }

  /* Status banners — tone modeled on the shared app.css badges: 40% ring,
     14% fill, color from the token. The downloads banner is visible from any tab,
     a click on it pauses. There is no blue token, so its tone is a single
     literal. */
  .download-pill {
    cursor: pointer;
    font-family: inherit;
    border-color: var(--border-strong);
    background: rgba(255, 255, 255, 0.06);
    color: var(--text);
    font-variant-numeric: tabular-nums;
  }

  .download-pill.paused {
    border-color: rgba(251, 191, 36, 0.4);
    background: rgba(251, 191, 36, 0.14);
    color: var(--warn);
  }

  .pill-action {
    margin-left: 4px;
    opacity: 0.75;
  }

  .boot-pill.ready {
    /* Transparent green on the dark header gave black text 2.32:1
       (measured in 1.2.78, "Ready to launch"). A solid dark fill does not
       let the highlight through, and white text on it is above the norm. */
    border-color: rgba(110, 231, 183, 0.65);
    background: #0c2e20;
    color: #ffffff;
  }

  .boot-pill.error {
    border-color: rgba(252, 165, 165, 0.7);
    background: #4a1212;
    color: #ffffff;
  }

  .boot-pill.booting {
    border-color: rgba(252, 211, 77, 0.7);
    background: #3a2a08;
    color: #ffffff;
  }

  /* Startup bar — markup of the shared progress bars (app.css
     .progress-track/.progress-fill); only dimensions change here so
     the bar fits into the status banner. */
  .boot-progress {
    width: 120px;
    height: 5px;
    flex: 0 0 auto;
  }

  .dot-green {
    background: var(--ok);
    box-shadow: none;
  }

  .dot-red {
    background: var(--err);
    box-shadow: none;
  }

  /* Launch button — the same glass as the panels: states differ
     by glow, not by repainting the fill. */
  .btn-launch-st {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    padding: 7px 16px;
    border-radius: var(--radius-sm);
    font-size: 0.85rem;
    font-weight: 600;
    cursor: pointer;
    border: none;
    outline: none;
    user-select: none;
    -webkit-user-select: none;
    -webkit-tap-highlight-color: transparent;
    transition: transform 0.2s, box-shadow 0.2s;
  }

  .btn-launch-st:focus,
  .btn-launch-st:focus-visible {
    outline: none;
  }

  .btn-launch-st.ready {
    /* --lg-fill-strong is a bit light: white text held 4.4:1 over highlights; a dark
       green fill gives headroom to the norm, the "green button" semantics are the same. */
    background: var(--surface);
    color: #ffffff;
    padding: 10px 22px;
    border-radius: 999px;
    font-size: 0.95rem;
    letter-spacing: 0.01em;
    animation: launch-pulse 3s infinite;
  }

  .btn-launch-st.ready:hover:not(:disabled) {
    transform: translateY(-1px) scale(1.02);
    box-shadow: none;
  }

  .btn-launch-st.ready:active:not(:disabled) {
    transform: translateY(0) scale(0.99);
  }

  .btn-launch-st.ready.opening {
    opacity: 0.9;
    cursor: wait;
    transform: none !important;
    animation: none;
    box-shadow: none;
  }

  .btn-launch-st.booting {
    background: var(--lg-fill-soft);
    color: var(--text);
    border: 1px solid var(--lg-edge-dim);
    cursor: wait;
  }

  .btn-launch-st.error {
    background: var(--lg-fill-strong);
    color: #ffffff;
    box-shadow: none;
  }

  .btn-spinner {
    width: 13px;
    height: 13px;
    border: 2px solid var(--border);
    border-top-color: currentColor;
    border-radius: 50%;
    animation: spin 0.8s linear infinite;
  }

  @keyframes launch-pulse {
    0%, 100% {
      box-shadow: none;
    }
    50% {
      box-shadow: none;
    }
  }

  /* The global prefers-reduced-motion in app.css only covers background
     motion under the glass — it does not stop this pulse. */
  @media (prefers-reduced-motion: reduce) {
    .btn-launch-st,
    .btn-launch-st.ready {
      animation: none;
      transition: none;
    }

    .btn-spinner {
      animation: none;
    }
  }

  .lang-switch {
    display: inline-flex;
    padding: 3px;
    border-radius: 999px;
    border: 1px solid var(--border);
    background: var(--surface-2);
  }

  .lang-btn {
    padding: 3px 8px;
    border: none;
    background: transparent;
    color: var(--text-dim);
    font-size: 10.5px;
    font-weight: 700;
    letter-spacing: 0.05em;
    border-radius: 999px;
    box-shadow: none;
  }

  .lang-btn.active {
    color: var(--text);
    background: rgba(255, 255, 255, 0.08);
  }

  .quick-toolbar {
    display: flex;
    flex-direction: column;
    justify-content: space-between;
    gap: 16px;
    flex: none;
    width: var(--sidebar-width);
    padding: 12px 10px;
    background: var(--bg-sidebar);
    border-right: 1px solid var(--border);
    overflow-y: auto;
  }

  .nav-tabs {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  /* Honest tab list load error (V-45): a visible banner instead of
     a silent fallback to its own copy. */
  .tabs-error {
    margin: 10px 14px 0;
    padding: 8px 12px;
    border-radius: 10px;
    border: 1px solid rgba(248, 113, 113, 0.45);
    background: rgba(239, 68, 68, 0.12);
    color: #fca5a5;
    font-size: 0.83rem;
  }

  .tab-btn {
    box-shadow: none;
    transition: none;
    display: flex;
    align-items: center;
    gap: 12px;
    width: 100%;
    padding: 9px 10px;
    border-radius: 10px;
    border: none;
    background: transparent;
    color: var(--text);
    font-size: 14px;
    font-weight: 400;
    text-align: left;
    cursor: pointer;
  }

  .tab-btn:hover {
    background: var(--surface-2);
  }

  .tab-btn.active {
    background: var(--surface-2);
    border: none;
  }

  .toolbar-actions {
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding-top: 8px;
    border-top: 1px solid var(--border);
  }

  .btn-tool {
    box-shadow: none;
    transition: none;
    display: flex;
    align-items: center;
    gap: 12px;
    width: 100%;
    padding: 9px 10px;
    border-radius: 10px;
    border: none;
    background: transparent;
    white-space: nowrap;
    font-size: 14px;
    color: var(--text);
    text-align: left;
    cursor: pointer;
  }

  .btn-tool:hover {
    background: var(--surface-2);
  }

  .tagger-field {
    margin-bottom: 16px;
  }

  .tagger-toggle {
    display: flex;
    align-items: flex-start;
    gap: 12px;
    cursor: pointer;
    user-select: none;
  }

  .custom-check {
    flex: none;
    margin-top: 2px;
    width: 18px;
    height: 18px;
    accent-color: var(--text);
    cursor: pointer;
  }

  .settings-body {
    flex: 1;
    display: flex;
    min-height: 0;
  }

  .settings-main {
    flex: 1;
    display: flex;
    flex-direction: column;
    min-width: 0;
  }

  .tab-btn :global(svg),
  .btn-tool :global(svg) {
    flex: none;
    color: var(--text-dim);
  }

  .settings-content {
    flex: 1;
    overflow-y: auto;
    padding: 24px;
  }

  .models-grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(420px, 1fr));
    gap: 16px;
  }

  .model-card {
    padding: 20px;
    display: flex;
    flex-direction: column;
    gap: 12px;
    transition: all 0.2s;
    overflow: hidden;
    box-shadow: none;
  }

  .card-active {
    background-image: var(--lg-fill);
    box-shadow: none;
  }

  .card-header {
    display: flex;
    justify-content: space-between;
    align-items: flex-start;
    gap: 12px;
    min-width: 0;
  }

  .title-group {
    flex: 1 1 auto;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .title-row {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
    min-width: 0;
  }

  .model-name {
    margin: 0;
    font-size: 1.05rem;
    font-weight: 600;
    color: var(--text);
    word-break: break-word;
  }

  .model-tag-sub {
    font-size: 0.75rem;
    font-family: monospace;
    /* On glass the background under the text is lighter than on the old dark card:
       the old #64748b blended into the backing. */
    color: var(--text-dim);
    word-break: break-all;
  }

  .badge-active {
    font-size: 0.7rem;
    font-weight: 600;
    padding: 2px 8px;
    border-radius: 6px;
    background: rgba(255, 255, 255, 0.11);
    color: var(--text);
    border: 1px solid var(--border-strong);
  }

  .badge-installed {
    font-size: 0.7rem;
    font-weight: 600;
    padding: 2px 8px;
    border-radius: 6px;
    background: rgba(16, 185, 129, 0.2);
    color: #539e55;
    border: 1px solid rgba(52, 211, 153, 0.3);
  }

  .badge-not-installed {
    font-size: 0.7rem;
    font-weight: 500;
    padding: 2px 8px;
    border-radius: 6px;
    background: rgba(255, 255, 255, 0.05);
    color: var(--text-dim);
  }

  .badge-vram {
    font-size: 0.72rem;
    font-weight: 500;
    padding: 3px 8px;
    border-radius: 6px;
    background: rgba(255, 255, 255, 0.05);
    color: var(--text);
    border: 1px solid var(--border);
    white-space: normal;
    text-align: right;
    line-height: 1.35;
    word-break: normal;
    overflow-wrap: break-word;
    flex-shrink: 1;
    max-width: 48%;
    align-self: flex-start;
  }

  .model-desc {
    margin: 0;
    font-size: 0.84rem;
    color: var(--text);
    line-height: 1.45;
    display: -webkit-box;
    -webkit-line-clamp: 3;
    -webkit-box-orient: vertical;
    line-clamp: 3;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .roleplay-note {
    font-size: 0.8rem;
    background: var(--surface-2);
    border-left: 3px solid var(--border-strong);
    padding: 7px 10px;
    border-radius: 0 6px 6px 0;
    color: var(--text);
  }

  .rp-label {
    font-weight: 600;
    color: var(--text);
  }

  .card-metrics {
    display: flex;
    gap: 16px;
    padding-top: 4px;
    border-top: 1px solid var(--border);
    font-size: 0.78rem;
  }

  .m-label {
    color: var(--text-faint);
  }

  .download-box {
    background: var(--surface-2);
    border: 1px solid var(--border-strong);
    border-radius: 8px;
    padding: 10px;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .progress-bar-wrap {
    height: 6px;
    background: rgba(255, 255, 255, 0.1);
    border-radius: 3px;
    overflow: hidden;
  }

  .progress-bar-fill {
    height: 100%;
    background: var(--surface-3);
    border-radius: 3px;
    transition: width 0.2s;
  }

  /* Paused: the bar dims so the state reads without text. */
  .progress-bar-fill.is-paused {
    background: var(--surface-3);
  }

  .progress-bar-wrap.thin {
    height: 4px;
  }

  .progress-meta {
    display: flex;
    justify-content: space-between;
    font-size: 0.75rem;
    color: var(--text-dim);
  }

  .download-actions {
    display: flex;
    gap: 8px;
    justify-content: flex-end;
  }

  .download-actions .btn-cancel {
    align-self: auto;
  }

  /* The failure reason on its own line: there is no room for it in the status label. */
  .p-error {
    font-size: 0.74rem;
    line-height: 1.4;
    color: #dc6973;
    overflow-wrap: anywhere;
  }

  .btn-cancel {
    align-self: flex-end;
    background: transparent;
    border: 1px solid rgba(239, 68, 68, 0.3);
    color: #dc6973;
    border-radius: 4px;
    padding: 2px 8px;
    font-size: 0.72rem;
    cursor: pointer;
  }

  .card-footer {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-top: auto;
    padding-top: 10px;
  }

  .btn-select {
    padding: 8px 16px;
    border-radius: 8px;
    background: var(--btn-fill);
    color: #ffffff;
    border: 1px solid var(--border);
    font-size: 0.82rem;
    font-weight: 500;
    cursor: pointer;
    transition: all 0.15s;
    box-shadow: var(--btn-shadow);
  }

  .btn-select:hover {
    background: var(--btn-fill-hover);
  }

  .btn-selected {
    padding: 8px 16px;
    border-radius: 8px;
    background: rgba(255, 255, 255, 0.11);
    color: var(--text);
    border: 1px solid var(--border-strong);
    font-size: 0.82rem;
    font-weight: 600;
    cursor: default;
  }

  .btn-download {
    padding: 8px 18px;
    border-radius: 8px;
    background: var(--btn-fill);
    color: #ffffff;
    border: none;
    font-size: 0.82rem;
    font-weight: 600;
    cursor: pointer;
    transition: all 0.15s;
    box-shadow: var(--btn-shadow);
  }

  .btn-download:hover:not(:disabled) {
    opacity: 0.92;
    transform: translateY(-1px);
  }

  .btn-download:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  .btn-delete {
    background: transparent;
    border: 1px solid rgba(239, 68, 68, 0.2);
    color: #dc6973;
    border-radius: 6px;
    padding: 6px 12px;
    font-size: 0.76rem;
    cursor: pointer;
    transition: all 0.15s;
  }

  .btn-delete:hover:not(:disabled) {
    background: rgba(239, 68, 68, 0.15);
    border-color: #dc6973;
  }

  /* Non-destructive actions (open folder, create shortcut): btn-delete
     is kept only for delete — the red "danger" signal lied on those buttons. */
  .btn-utility {
    background: var(--btn-fill);
    border: 1px solid var(--border-strong);
    color: var(--text);
    border-radius: 6px;
    padding: 6px 12px;
    font-size: 0.76rem;
    cursor: pointer;
    transition: all 0.15s;
    box-shadow: var(--btn-shadow);
  }

  .btn-utility:hover:not(:disabled) {
    background: var(--btn-fill-hover);
    border-color: var(--border-strong);
  }

  .btn-utility:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  .params-container,
  .custom-container {
    max-width: 680px;
    margin: 0 auto;
  }

  .param-card {
    padding: 24px;
    /* Aurora highlights shone through the glass and dropped white headings and
       labels to 2.9–3.8:1 (measurement 31.10.2026): a pad over the glass. */
    box-shadow: none;
  }

  .param-card h3 {
    margin: 0 0 6px;
    font-size: 1.1rem;
    color: #fff;
  }

  .param-sub {
    margin: 0 0 20px;
    font-size: 0.83rem;
    /* #96989d on glass did not hold 4.5:1 (measurement 31.10.2026). */
    color: var(--text);
    line-height: 1.4;
  }

  .form-group {
    margin-bottom: 20px;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .form-group label {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  /* The same selector weight as the rule above: without it the checkbox sat above the label. */
  .form-group .tagger-toggle {
    flex-direction: row;
    gap: 12px;
  }

  .form-group label b {
    font-size: 0.88rem;
    color: var(--text);
  }

  .hint {
    font-size: 0.76rem;
    color: var(--text-dim);
  }

  select,
  input[type='text'] {
    background: var(--surface);
    border: 1px solid var(--border);
    color: #fff;
    padding: 9px 12px;
    border-radius: 8px;
    font-size: 0.88rem;
    outline: none;
  }


  input[type='range'] {
    accent-color: var(--text);
    cursor: pointer;
  }

  .action-row {
    margin-top: 24px;
  }

  .btn-primary {
    padding: 10px 20px;
    background: var(--btn-fill);
    color: #fff;
    border: none;
    border-radius: 8px;
    font-weight: 600;
    font-size: 0.88rem;
    cursor: pointer;
    transition: all 0.15s;
    box-shadow: var(--btn-shadow);
  }

  .btn-primary:hover:not(:disabled) {
    opacity: 0.9;
    transform: translateY(-1px);
  }

  .btn-primary:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  .custom-input-row {
    display: flex;
    gap: 10px;
  }

  .custom-input-row input,
  .custom-input-row select {
    flex: 1;
    min-width: 0;
  }

  .extra-models {
    margin-top: 16px;
  }

  .installed-list-box {
    margin-top: 30px;
    padding-top: 20px;
    border-top: 1px solid var(--border);
  }

  .installed-list-box h4 {
    margin: 0 0 12px;
    font-size: 0.9rem;
    color: var(--text);
  }

  .tags-cloud {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
  }

  .tag-badge {
    background: var(--surface-2);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 5px 10px;
    display: inline-flex;
    align-items: center;
    gap: 8px;
    font-size: 0.82rem;
    font-family: monospace;
    color: var(--text);
  }

  .tag-del-btn {
    background: transparent;
    border: none;
    color: #dc6973;
    cursor: pointer;
    font-size: 0.8rem;
    padding: 0 2px;
  }

  .dim-text {
    font-size: 0.82rem;
    color: var(--text-faint);
  }

  .toast {
    margin: 0 24px 12px;
    padding: 10px 16px;
    border-radius: 8px;
    display: flex;
    justify-content: space-between;
    align-items: center;
    font-size: 0.85rem;
  }

  .toast-error {
    background: rgba(239, 68, 68, 0.15);
    border: 1px solid rgba(239, 68, 68, 0.4);
    color: #fca5a5;
  }

  .toast-success {
    background: rgba(16, 185, 129, 0.15);
    border: 1px solid rgba(16, 185, 129, 0.4);
    color: #6ee7b7;
  }

  .toast-info {
    background: rgba(255, 255, 255, 0.07);
    border: 1px solid var(--border-strong);
    color: var(--text);
  }

  .toast-close {
    background: transparent;
    border: none;
    color: inherit;
    cursor: pointer;
  }

  .loading-box {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    padding: 60px;
    gap: 16px;
    color: var(--text-dim);
  }

  .spinner {
    width: 32px;
    height: 32px;
    border: 3px solid var(--border);
    border-top-color: var(--border-strong);
    border-radius: 50%;
    animation: spin 0.8s linear infinite;
  }

  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }

  /* Image tab styles */
  .comfy-status-card {
    grid-column: 1 / -1;
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 14px;
    padding: 18px 24px;
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 20px;
    flex-wrap: wrap;
  }

  .comfy-status-info {
    flex: 1;
    min-width: 280px;
  }

  .comfy-status-badge {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    padding: 4px 12px;
    border-radius: 20px;
    font-size: 0.82rem;
    margin-bottom: 8px;
  }

  .comfy-status-badge.online {
    background: rgba(34, 197, 94, 0.15);
    border: 1px solid rgba(34, 197, 94, 0.35);
    color: #539e55;
  }

  .comfy-status-badge.offline {
    background: rgba(245, 158, 11, 0.15);
    border: 1px solid rgba(245, 158, 11, 0.35);
    color: #d57141;
  }

  .comfy-status-badge.not-installed {
    background: rgba(255, 255, 255, 0.05);
    border: 1px solid var(--border);
    color: var(--text-dim);
  }

  .status-dot {
    flex: 0 0 auto;
    width: 8px;
    height: 8px;
    aspect-ratio: 1;
    border-radius: 50%;
    background: currentColor;
    box-shadow: none;
  }

  .comfy-desc {
    margin: 0;
    font-size: 0.85rem;
    color: var(--text-dim);
    line-height: 1.45;
  }

  .comfy-desc code {
    background: var(--surface-2);
    padding: 2px 6px;
    border-radius: 4px;
    font-size: 0.82rem;
    color: var(--text);
  }

  .btn-start-comfy {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    padding: 9px 18px;
    border-radius: 9px;
    font-size: 0.88rem;
    font-weight: 600;
    background: var(--btn-fill);
    color: #fff;
    border: none;
    cursor: pointer;
    transition: all 0.2s;
    box-shadow: var(--btn-shadow);
  }

  .btn-start-comfy:hover:not(:disabled) {
    transform: translateY(-1px);
  }

  .comfy-ready-tag {
    font-size: 0.85rem;
    font-weight: 600;
    color: #539e55;
    background: rgba(34, 197, 94, 0.1);
    padding: 6px 14px;
    border-radius: 20px;
    border: 1px solid rgba(34, 197, 94, 0.25);
  }

  .image-tips-card {
    grid-column: 1 / -1;
    background: var(--surface-2);
    border: 1px solid var(--border);
    border-radius: 14px;
    padding: 20px 24px;
  }

  .image-tips-card h4 {
    margin: 0 0 16px;
    font-size: 0.95rem;
    color: var(--text);
  }

  .tips-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(280px, 1fr));
    gap: 16px;
  }

  .tip-item {
    display: flex;
    gap: 12px;
    align-items: flex-start;
  }

  .tip-num {
    display: flex;
    align-items: center;
    justify-content: center;
    width: 26px;
    height: 26px;
    border-radius: 50%;
    background: rgba(255, 255, 255, 0.09);
    border: 1px solid var(--border-strong);
    color: var(--text);
    font-weight: 700;
    font-size: 0.8rem;
    flex-shrink: 0;
  }

  .tip-item b {
    display: block;
    font-size: 0.86rem;
    color: var(--text);
    margin-bottom: 4px;
  }

  .tip-item p {
    margin: 0;
    font-size: 0.8rem;
    color: var(--text-dim);
    line-height: 1.4;
  }

  .hardware-note {
    grid-column: 1 / -1;
    padding: 10px 14px;
    margin-bottom: 4px;
    border-radius: 10px;
    background: rgba(255, 255, 255, 0.05);
    border: 1px solid var(--border-strong);
    font-size: 0.82rem;
    color: var(--text);
  }

  .hardware-note .hw-warn {
    margin-top: 4px;
    color: #d57141;
  }

  .hardware-note .hw-bad {
    color: #dc6973;
  }

  .tune-optional {
    margin-top: 14px;
    padding: 12px 14px;
    border-radius: 10px;
    background: rgba(255, 255, 255, 0.04);
    border: 1px solid var(--border);
  }

  .tune-downloads {
    margin: 0 0 10px;
    padding-left: 18px;
    font-size: 0.8rem;
    color: var(--text);
  }

  /* Queue row: model name, its state and a bar if a download is running. */
  .tune-download-row {
    margin-bottom: 8px;
  }

  .tdr-head {
    display: flex;
    justify-content: space-between;
    gap: 10px;
    align-items: baseline;
  }

  .tdr-status {
    font-size: 0.74rem;
    color: var(--text-dim);
    text-align: right;
    /* A long failure reason otherwise stretches the row: on a shrunk flex item
       without min-width the width is computed from the longest word. */
    min-width: 0;
    overflow-wrap: anywhere;
  }

  .tdr-error {
    margin-top: 2px;
    font-size: 0.72rem;
    line-height: 1.4;
    color: #dc6973;
    overflow-wrap: anywhere;
  }

  .tdr-status.downloading {
    color: var(--text);
  }

  .tdr-status.paused,
  .tdr-status.queued {
    color: #d57141;
  }

  .tdr-status.done {
    color: #539e55;
  }

  .tdr-status.failed {
    color: #dc6973;
  }

  /* Counter of unapplied suggestions on the "Recommended settings" tab. */
  .tab-badge {
    display: inline-block;
    margin-left: 6px;
    padding: 0 6px;
    border-radius: 8px;
    background: rgba(255, 255, 255, 0.14);
    border: 1px solid var(--border-strong);
    font-size: 0.72rem;
    font-weight: 600;
    color: var(--text);
  }

  /* Whole-system parameters on the recommendations tab. */
  .sys-grid {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 12px 14px;
    margin-bottom: 14px;
    border-radius: 10px;
    background: rgba(255, 255, 255, 0.05);
    border: 1px solid var(--border-strong);
    font-size: 0.82rem;
    color: var(--text);
  }

  .sys-row {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: 8px;
  }

  .sys-label {
    min-width: 150px;
    color: var(--text-dim);
    font-size: 0.78rem;
  }

  .sys-extra {
    color: var(--text-dim);
    font-size: 0.76rem;
  }

  /* Personality traits: tag list with intensity sliders. */
  .traits-list {
    display: flex;
    flex-direction: column;
    gap: 6px;
    margin: 12px 0;
    padding: 12px 14px;
    border-radius: 10px;
    background: var(--surface-2);
    border: 1px solid var(--border);
  }

  .trait-row {
    display: grid;
    grid-template-columns: minmax(110px, 1fr) minmax(110px, 2fr) 48px 24px;
    align-items: center;
    gap: 10px;
    font-size: 0.82rem;
    color: var(--text);
  }

  .trait-remove {
    background: transparent;
    border: none;
    color: var(--text-faint);
    cursor: pointer;
    font-size: 0.8rem;
    padding: 2px 4px;
    border-radius: 6px;
  }

  .trait-remove:hover {
    color: #dc6973;
    background: rgba(248, 113, 113, 0.12);
  }

  .trait-add {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    margin-bottom: 14px;
  }

  .trait-add input {
    flex: 1 1 260px;
  }

  /* Live state: values move by themselves, but can be corrected by hand. */
  .traits-state {
    margin: 12px 0;
    padding: 12px 14px;
    border-radius: 10px;
    background: var(--surface-2);
    border: 1px solid var(--border);
  }

  .state-row {
    display: grid;
    grid-template-columns: 120px minmax(120px, 1fr) 40px;
    align-items: center;
    gap: 10px;
    font-size: 0.82rem;
    color: var(--text);
    margin-bottom: 6px;
  }

  .state-name {
    color: var(--text-dim);
  }

  .state-value {
    text-align: right;
    color: var(--text);
    font-weight: 600;
    font-variant-numeric: tabular-nums;
  }

  .trait-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .trait-value {
    text-align: right;
    color: var(--text);
    font-weight: 600;
    font-variant-numeric: tabular-nums;
  }

  .trait-value.trait-off {
    color: var(--text-faint);
    font-weight: 500;
  }

  /* Bonds: container, row and buttons are shared with traits — only the columns
     (target, bond name, strength) and the note placement with the secret flag are its own. */
  .bond-item + .bond-item {
    margin-top: 12px;
  }

  .bond-row {
    display: grid;
    grid-template-columns: minmax(120px, 1.1fr) minmax(110px, 1fr) minmax(90px, 1.2fr) 46px 24px;
    align-items: center;
    gap: 10px;
    font-size: 0.82rem;
    color: var(--text);
  }

  .bond-note {
    display: flex;
    flex-direction: column;
    gap: 4px;
    font-size: 0.78rem;
  }

  .bond-secret {
    margin-top: 2px;
  }

  .traits-preview {
    margin: 12px 0;
    padding: 12px 14px;
    border-radius: 10px;
    background: rgba(255, 255, 255, 0.04);
    border: 1px solid var(--border);
  }

  .traits-preview pre {
    margin: 0;
    white-space: pre-wrap;
    font-size: 0.78rem;
    line-height: 1.5;
    color: var(--text);
  }

  .tune-ok {
    padding: 12px 14px;
    border-radius: 10px;
    background: rgba(34, 197, 94, 0.1);
    border: 1px solid rgba(74, 222, 128, 0.3);
    color: #bbf7d0;
    font-size: 0.85rem;
  }

  .tune-warn {
    padding: 10px 14px;
    margin-bottom: 12px;
    border-radius: 10px;
    background: rgba(251, 191, 36, 0.1);
    border: 1px solid rgba(251, 191, 36, 0.35);
    color: #fde68a;
    font-size: 0.82rem;
  }

  .tune-list {
    display: flex;
    flex-direction: column;
    gap: 10px;
    margin: 14px 0;
  }

  .tune-item {
    padding: 12px 14px;
    border-radius: 10px;
    background: var(--surface-2);
    border: 1px solid var(--border);
  }

  .tune-head {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 10px;
    font-size: 0.88rem;
    color: var(--text);
  }

  .tune-diff {
    color: var(--text-dim);
    font-size: 0.82rem;
  }

  .tune-arrow {
    color: var(--text);
    font-weight: 700;
  }

  .tune-why {
    margin: 8px 0 0;
    font-size: 0.8rem;
    line-height: 1.5;
    color: var(--text-dim);
  }

  .isolation-list {
    margin-top: 16px;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .isolation-row {
    display: grid;
    grid-template-columns: 18px 200px 1fr 90px;
    align-items: center;
    gap: 10px;
    padding: 8px 10px;
    border-radius: 8px;
    background: var(--surface);
    border: 1px solid var(--border-strong);
    font-size: 0.82rem;
  }

  .isolation-row.isolation-bad {
    border-color: rgba(248, 113, 113, 0.6);
  }

  .isolation-row.isolation-good .iso-state {
    color: #539e55;
  }

  .isolation-row.isolation-bad .iso-state {
    color: #dc6973;
  }

  .iso-label {
    color: var(--text);
  }

  .iso-path {
    color: var(--text-faint);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .iso-size {
    text-align: right;
    color: var(--text-dim);
  }
  /* Card of the previous-series model update suggestion. */
  .upgrade-card {
    display: flex;
    flex-direction: column;
    gap: 8px;
    margin-bottom: 14px;
    padding: 14px 16px;
    border: 1px solid var(--border-strong);
    border-radius: var(--lg-radius-inner);
    background: var(--lg-fill-soft);
  }

  .upgrade-title {
    font-weight: 700;
  }

  .upgrade-text {
    margin: 0;
    font-size: 0.86rem;
    line-height: 1.5;
  }

  .upgrade-actions {
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
  }
</style>








