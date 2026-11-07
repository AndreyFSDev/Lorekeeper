export interface Settings {
  chat_model: string;
  vision_model: string;
  embed_model: string;
  context_size: number;
  temperature: number;
  top_p: number;
  max_tokens: number;
  auto_memory: boolean;
  memory_max_chunks: number;
  ollama_port: number;
  use_system_ollama: boolean;
  setup_done: boolean;
  coder_removed: boolean;
  echo_migrated?: boolean;
  auto_launch_st?: boolean;
  image_model?: string;
  auto_launch_comfy?: boolean;
  tagger_enabled?: boolean;
  tagger_model?: string;
  tagger_max_tags?: number;
  tagger_language?: string;
  tagger_write_card_tags?: boolean;
  /** Automatically translate imported character cards into Russian. */
  auto_translate_imports?: boolean;
  tagger_prompt?: string;
  /** Hardware fingerprint the settings were auto-tuned for. */
  tuned_for?: string;
  /** Model for the background evaluator; empty — pick automatically by VRAM. */
  evaluator_model?: string;
  director_prompt?: string;
  rpg_evaluator_prompt?: string;
  rpg_combat_prompt?: string;
}

/** One hardware tuning suggestion. */
export interface Recommendation {
  key: string;
  title: string;
  current: string;
  recommended: string;
  reason: string;
}

export interface RecommendHardware {
  name: string;
  vram: string;
  backend: string;
  tier: string;
  notes: string[];
  fingerprint: string;
}

/** Whole-system specs: CPU, memory, disk space. */
export interface SystemInfo {
  cpu: string;
  cpu_cores: number;
  cpu_threads: number;
  ram_total_mb: number;
  ram_free_mb: number;
  /** Free space on the model store and in the app folder (the smaller of the two), GB. */
  disk_free_gb: number;
}

/** Which of the items recommended for this hardware are not downloaded yet. */
export interface RecommendedDownload {
  kind: DownloadKind;
  id: string;
  title: string;
  size: string;
}

export type DownloadKind = 'ollama' | 'image';

export type DownloadStatus = 'queued' | 'downloading' | 'paused' | 'done' | 'failed' | 'cancelled';

/** One download queue item: the backend drives it, so it survives the window. */
export interface DownloadTask {
  kind: DownloadKind;
  /** Ollama model tag or checkpoint file name. */
  id: string;
  title: string;
  status: DownloadStatus;
  completed: number;
  total: number;
  speed: number;
  eta: number;
  /** State in words from the transport: "downloading sha256-…", "preparing…". */
  note: string;
  error: string | null;
}

/** Download queue snapshot: what is downloading, what waits, and whether everything is paused. */
export interface DownloadsSnapshot {
  tasks: DownloadTask[];
  paused: boolean;
  running: boolean;
}

export interface RecommendReport {
  hardware: RecommendHardware;
  system: SystemInfo;
  /** Hardware the settings were tuned for last time (empty — never). */
  tuned_for: string;
  /** The settings are already tuned for the current hardware. */
  matches: boolean;
  items: Recommendation[];
}

/** Character tag with a configured display intensity (0–100). */
export interface TraitTag {
  tag: string;
  intensity: number;
}

export interface TraitCharacter {
  avatar: string;
  name: string;
  tags: TraitTag[];
  /** All character tags in SillyTavern — for the "take tags from SillyTavern" button. */
  st_tags: string[];
  /** Ask the model not to repeat the same actions. */
  no_repeat: boolean;
  /** Behavior chaos, 0–100. */
  chaos: number;
  /** Live state enabled: changes on its own as the conversation goes. */
  live: boolean;
  state: LiveState;
  /** Bonds with other characters — what he can gossip about behind their backs. */
  bonds: Bond[];
  /** Text that goes into the card as a depth prompt. */
  fragment: string;
}

/** Bond with another character: who is to whom, how strong it is, and whether they know. */
export interface Bond {
  /** Target card avatar (`Gawr_Gura.png`), not the display name. */
  target: string;
  /** Bond name in words: "younger sister", "debtor", "rival". */
  kind: string;
  /** How much the bond defines behavior, 0–100. */
  strength: number;
  /** What to gossip about: a fact, a grudge, shared history. */
  note: string;
  /** "He does not know" — the target is unaware of this attitude. */
  secret: boolean;
}

/** Character state in the conversation: changes from line to line. */
export interface LiveState {
  /** Tenderness, warmth — what a single rude line cuts off. */
  affection: number;
  trust: number;
  energy: number;
  irritation: number;
}

export type GpuVendor = 'nvidia' | 'amd' | 'intel' | 'unknown';
export type GpuBackend = 'cuda' | 'vulkan' | 'cpu';
export type HardwareTier = 'minimal' | 'low' | 'mid' | 'high';

export interface GpuInfo {
  has_gpu: boolean;
  vendor: GpuVendor;
  name: string | null;
  vram_mb: number | null;
  backend: GpuBackend;
  tier: HardwareTier;
  notes: string[];
}

export interface GpuUsage {
  loaded: boolean;
  model: string | null;
  size_vram_mb: number;
  size_total_mb: number;
  on_gpu: boolean;
  /** How many context tokens actually fit into VRAM (0 — unknown). */
  context_ceiling: number;
  /** Whether the model fits into VRAM entirely. */
  model_fits: boolean;
  /** Free on the card now, budget for our model, required minimum and total (MB). */
  vram_free_mb: number;
  vram_budget_mb: number;
  vram_needed_mb: number;
  vram_total_mb: number;
}

export interface CatalogModel {
  tag: string;
  name: string;
  category: 'chat' | 'embed' | 'vision';
  badge: string;
  vram: string;
  size: string;
  quant: string;
  description: string;
  roleplay_style: string;
  min_tier: HardwareTier;
  recommended: boolean;
}

export interface ImageModelInfo {
  filename: string;
  name: string;
  badge: string;
  size: string;
  description: string;
  roleplay_style: string;
  is_installed: boolean;
  is_active: boolean;
  recommended: boolean;
  vram_tier: string;
  min_tier: HardwareTier;
}

export interface ComfyStatus {
  running: boolean;
  port: number;
  path: string | null;
  installed: boolean;
  /** Partially downloaded installation archive: how much is downloaded and the full size. */
  partial_bytes: number;
  partial_total_bytes: number;
}

export interface IsolationEntry {
  label: string;
  path: string;
  exists: boolean;
  size_bytes: number;
  outside: boolean;
}

export interface SetupState {
  has_gpu: boolean;
  gpu_name?: string | null;
  vram_mb?: number | null;
  free_space_gb: number;
  runtime_installed: boolean;
  st_installed: boolean;
  setup_done: boolean;
  recommended_chat_model: string;
  hardware: GpuInfo;
  models: CatalogModel[];
  images_available: boolean;
  /** Model store that the installation will use. */
  models_root: string;
  settings: Settings;
}

/** Candidate folder for the model store (first-run wizard). */
export interface ModelStore {
  path: string;
  /** How many Ollama models are already in the folder. */
  models: number;
  /** Free on the volume, GB; -1 — unknown. */
  free_gb: number;
  current: boolean;
}

export interface DownloadProgress {
  completed: number;
  total: number;
  status: string;
}

export interface ModelDownloadProgress extends DownloadProgress {
  model: string;
  speed_bytes_per_sec?: number;
  eta_sec?: number;
}

export interface StSetupProgress extends DownloadProgress {
  step: string;
}

/** Result of the background vision model download. */
export interface VisionDownloadResult {
  ok: boolean;
  model: string;
  error: string | null;
}

export interface BootState {
  status: string;
  error: string | null;
  ready: boolean;
  port: number;
  /** Boot progress 0..100, if the current step computes it. */
  progress?: number | null;
}

export interface AppUpdateInfo {
  available: boolean;
  current_version: string;
  latest_version: string;
  notes: string | null;
}

export interface AppUpdateProgress {
  stage: string;
  downloaded_bytes: number;
  total_bytes: number;
  percent: number;
  speed_bps: number;
}

export interface PyruntimeStatus {
  installed: boolean;
  python: string | null;
  torch: string | null;
  llama_cpp: boolean;
  size_mb: number;
  installing: boolean;
  stage: string | null;
  progress: number | null;
  error: string | null;
}

export interface TrainerInfo {
  available: boolean;
  reason: string | null;
  runtime: PyruntimeStatus;
  /** Step divisor per pass: batch_size × grad_accum from trainer::tier_profile.
   *  The backend does not expose this field yet (the contract is code-review report V-50); when
   *  it is missing, the UI uses the fallback value checked against tier_profile. */
  step_divisor?: number;
}

export interface TrainPreset {
  id: string;
  name: string;
  description: string;
  base_model: string;
  ollama_tag: string;
  params_b: number;
  download_gb: number;
  min_tier: HardwareTier;
  recommended: boolean;
}

export interface DatasetInfo {
  name: string;
  path: string;
  mode: 'chat' | 'text';
  examples: number;
  chars: number;
  approx_tokens: number;
  preview: string[];
}

export interface TrainingState {
  running: boolean;
  run_id: string | null;
  preset_id: string | null;
  dataset: string | null;
  stage: string;
  step: number;
  total: number;
  loss: number | null;
  epoch: number | null;
  eta_seconds: number | null;
  elapsed_seconds: number;
  adapter_dir: string | null;
  log_tail: string[];
  error: string | null;
  finished: boolean;
}

/** Image checkpoint as a fine-tuning base: the family defines the kohya entry point. */
export interface ImageTrainPreset {
  id: string;
  name: string;
  file_name: string;
  path: string;
  family: 'sd15' | 'sdxl';
  family_label: string;
  description: string;
  resolution: number;
  size_gb: number;
  min_tier: HardwareTier;
  recommended: boolean;
}

/** Image dataset: "image + .txt" pairs in data/training/images/<slug>. */
export interface ImageDatasetInfo {
  slug: string;
  path: string;
  images: number;
  captions: number;
}

export interface ImageRuntime {
  cuda: boolean;
  venv: boolean;
  sd_scripts: boolean;
  checkpoints: number;
  tagger: boolean;
  reason: string | null;
}

export interface CaptionProgress {
  running: boolean;
  done: number;
  total: number;
  stage: string;
}

export interface ImageTrainingState {
  running: boolean;
  run_id: string | null;
  preset_id: string | null;
  dataset: string | null;
  stage: string;
  step: number;
  total: number;
  loss: number | null;
  lr: number | null;
  speed: number | null;
  eta_seconds: number | null;
  elapsed_seconds: number;
  images: number;
  captions: number;
  lora_path: string | null;
  log_tail: string[];
  error: string | null;
  finished: boolean;
  runtime: ImageRuntime;
  caption: CaptionProgress;
}

/** One user guide feature (see src-tauri/src/guide.rs). */
export interface GuideEntry {
  icon: string;
  title: string;
  since: string | null;
  location: string;
  usage: string;
  note: string | null;
}

/** Guide section: a version with its features or "Basics" (empty version). */
export interface GuideSection {
  version: string;
  date: string;
  entries: GuideEntry[];
}

/** Suggestion to upgrade a model of the previous series (see catalog::MODEL_UPGRADES). */
export interface UpgradeOffer {
  from: string;
  to: string;
  category: string;
  /** The new model is already on disk: the update reduces to switching roles. */
  to_installed: boolean;
}

/** Model update flow state. */
export interface UpgradeState {
  offers: UpgradeOffer[];
  dismissed: string[];
}

export interface EngineCharacter {
  id: string;
  name: string;
  description: string;
  personality: string;
  scenario: string;
  first_message: string;
  example_dialogue: string;
  system_prompt: string;
  tags: string[];
  has_avatar: boolean;
}

export interface LoreEntry {
  title: string;
  keys: string[];
  content: string;
  enabled: boolean;
  constant: boolean;
  priority: number;
  case_sensitive: boolean;
}

export interface Lorebook {
  name: string;
  scan_depth: number;
  token_budget: number;
  entries: LoreEntry[];
}

export interface ChatMessage {
  role: 'user' | 'assistant';
  content: string;
  at_ms: number;
}

export interface ChatSummary {
  id: string;
  updated_ms: number;
}

export type ChatEvent =
  | { kind: 'delta'; text: string }
  | { kind: 'done'; message: ChatMessage | null; cancelled: boolean };
