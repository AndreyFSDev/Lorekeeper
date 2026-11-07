import { Channel, invoke } from '@tauri-apps/api/core';
import type { AppUpdateInfo, AppUpdateProgress, BootState, Settings, SetupState } from './types';

export const api = {
  bootState: () => invoke<BootState>('boot_state'),
  setLocale: (lang: string) => invoke<void>('set_locale', { lang }),
  retryBoot: () => invoke<void>('retry_boot'),
  setupState: () => invoke<SetupState>('setup_state'),
  modelStores: () => invoke<import('./types').ModelStore[]>('model_stores'),
  inspectModelStore: (path: string) =>
    invoke<import('./types').ModelStore>('inspect_model_store', { path }),
  setModelsRoot: (path: string) =>
    invoke<import('./types').ModelStore>('set_models_root', { path }),
  downloadRuntime: () => invoke<void>('download_runtime'),
  cancelDownload: () => invoke<void>('cancel_download'),
  cancelComfyInstall: () => invoke<void>('cancel_comfy_install'),
  startRuntime: () => invoke<void>('start_runtime'),
  downloadModel: (model: string) => invoke<void>('download_model', { model }),
  downloadsState: () => invoke<import('./types').DownloadsSnapshot>('downloads_state'),
  downloadsPause: () => invoke<void>('downloads_pause'),
  downloadsResume: () => invoke<void>('downloads_resume'),
  downloadsCancel: () => invoke<void>('downloads_cancel'),
  downloadsPauseTask: (id: string) => invoke<void>('downloads_pause_task', { id }),
  downloadsResumeTask: (id: string) => invoke<void>('downloads_resume_task', { id }),
  downloadsCancelTask: (id: string) => invoke<void>('downloads_cancel_task', { id }),
  installedModels: () => invoke<string[]>('installed_models'),
  backgroundVisionDownload: () => invoke<boolean>('background_vision_download'),
  downloadSillytavern: () => invoke<void>('download_sillytavern'),
  finalizeSetup: () => invoke<void>('finalize_setup'),
  getSettings: () => invoke<Settings>('get_settings'),
  saveSettings: (settings: Settings) => invoke<void>('save_settings', { settings }),
  openStFolder: () => invoke<void>('open_st_folder'),
  resetSetup: () => invoke<void>('reset_setup'),
  deleteModel: (model: string) => invoke<void>('delete_model', { model }),
  openModelsFolder: () => invoke<void>('open_models_folder'),
  openAppFolder: () => invoke<void>('open_app_folder'),
  openAboutLink: (target: 'repo' | 'license') =>
    invoke<void>('open_about_link', { target }),
  engine: {
    listCharacters: () => invoke<import('./types').EngineCharacter[]>('engine_list_characters'),
    importCharacter: (path: string) =>
      invoke<import('./types').EngineCharacter>('engine_import_character', { path }),
    deleteCharacter: (characterId: string) =>
      invoke<void>('engine_delete_character', { characterId }),
    avatar: (characterId: string) => invoke<string | null>('engine_avatar', { characterId }),
    background: () => invoke<string | null>('engine_background'),
    setBackground: (path: string | null) =>
      invoke<string | null>('engine_set_background', { path }),
    getLorebook: (characterId: string) =>
      invoke<import('./types').Lorebook>('engine_get_lorebook', { characterId }),
    saveLorebook: (characterId: string, lorebook: import('./types').Lorebook) =>
      invoke<void>('engine_save_lorebook', { characterId, lorebook }),
    importLorebook: (characterId: string, path: string) =>
      invoke<import('./types').Lorebook>('engine_import_lorebook', { characterId, path }),
    listChats: (characterId: string) =>
      invoke<import('./types').ChatSummary[]>('engine_list_chats', { characterId }),
    createChat: (characterId: string) =>
      invoke<{ id: string; messages: import('./types').ChatMessage[] }>('engine_create_chat', {
        characterId,
      }),
    loadChat: (characterId: string, chatId: string) =>
      invoke<{ messages: import('./types').ChatMessage[]; skipped_lines: number }>(
        'engine_load_chat',
        {
          characterId,
          chatId,
        },
      ),
    deleteChat: (characterId: string, chatId: string) =>
      invoke<void>('engine_delete_chat', { characterId, chatId }),
    cancel: () => invoke<void>('engine_cancel'),
    /** text = null — regenerate the last reply. */
    send: (
      characterId: string,
      chatId: string,
      text: string | null,
      onEvent: (event: import('./types').ChatEvent) => void,
    ) => {
      const channel = new Channel<import('./types').ChatEvent>();
      channel.onmessage = onEvent;
      return invoke<void>('engine_send', { characterId, chatId, text, onEvent: channel });
    },
  },
  isolationReport: () => invoke<import('./types').IsolationEntry[]>('isolation_report'),
  openSettingsWindow: () => invoke<void>('open_settings_window'),
  /** Settings window tab list from the gateway (gateway.rs::SETTINGS_TABS) —
   *  the only source of the id whitelist (V-45): no copies in the frontend, so
   *  on failure the caller shows an honest error instead of its own list. */
  settingsTabs: () => invoke<string[]>('settings_tabs'),
  guideData: () => invoke<import('./types').GuideSection[]>('guide_data'),
  modelsUpgradeState: () => invoke<import('./types').UpgradeState>('models_upgrade_state'),
  modelsUpgradeApply: (from: string, to: string) =>
    invoke<void>('models_upgrade_apply', { from, to }),
  modelsUpgradeDismiss: (from: string) => invoke<void>('models_upgrade_dismiss', { from }),
  markStOpened: () => invoke<void>('mark_st_opened'),
  isStOpened: () => invoke<boolean>('is_st_opened'),
  getImageModels: () => invoke<import('./types').ImageModelInfo[]>('get_image_models'),
  gpuUsage: () => invoke<import('./types').GpuUsage>('gpu_usage'),
  setImageModel: (model: string) => invoke<void>('set_image_model', { model }),
  recommendedSettings: () => invoke<import('./types').RecommendReport>('recommended_settings'),
  applyRecommendedSettings: (keys?: string[]) =>
    invoke<Settings>('apply_recommended_settings', { keys: keys ?? null }),
  systemInfo: () => invoke<import('./types').SystemInfo>('system_info'),
  recommendedDownloads: () =>
    invoke<import('./types').RecommendedDownload[]>('recommended_downloads'),
  downloadRecommended: () => invoke<string[]>('download_recommended'),
  traitsState: () => invoke<import('./types').TraitCharacter[]>('traits_state'),
  traitsSave: (
    avatar: string,
    tags: import('./types').TraitTag[],
    noRepeat: boolean,
    chaos: number,
    live: boolean,
    liveState: import('./types').LiveState | null,
  ) => invoke<string>('traits_save', { avatar, tags, noRepeat, chaos, live, liveState }),
  traitsResetState: (avatar: string) => invoke<string>('traits_reset_state', { avatar }),
  bondsSave: (avatar: string, bonds: import('./types').Bond[]) =>
    invoke<string>('bonds_save', { avatar, bonds }),
  getComfyStatus: () => invoke<import('./types').ComfyStatus>('get_comfy_status'),
  startComfyServer: () => invoke<void>('start_comfy_server'),
  installComfyui: () => invoke<void>('install_comfyui'),
  downloadImageModel: (filename: string) => invoke<void>('download_image_model', { filename }),
  deleteImageModel: (filename: string) => invoke<void>('delete_image_model', { filename }),
  checkAppUpdate: () => invoke<AppUpdateInfo>('check_app_update'),
  trainerStatus: () => invoke<import('./types').TrainerInfo>('trainer_status'),
  trainerInstall: () => invoke<void>('trainer_install'),
  trainerCancel: () => invoke<void>('trainer_cancel'),
  trainingPresets: () => invoke<import('./types').TrainPreset[]>('training_presets'),
  trainingEstimate: (presetId: string, maxSeqLength: number | null) =>
    invoke<number>('training_estimate', { presetId, maxSeqLength }),
  trainingImport: (path: string) =>
    invoke<import('./types').DatasetInfo>('training_import', { path }),
  trainingDatasets: () => invoke<import('./types').DatasetInfo[]>('training_datasets'),
  trainingStart: (presetId: string, dataset: string, epochs: number, maxSeqLength: number | null) =>
    invoke<string>('training_start', { presetId, dataset, epochs, maxSeqLength }),
  trainingState: () => invoke<import('./types').TrainingState>('training_state'),
  trainingCancel: () => invoke<void>('training_cancel'),
  trainingClear: () => invoke<void>('training_clear'),
  trainingExport: (presetId: string, adapterDir: string, name: string) =>
    invoke<string>('training_export', { presetId, adapterDir, name }),
  trainingOpenImportFolder: () => invoke<void>('training_open_import_folder'),
  imageTrainingPresets: () =>
    invoke<import('./types').ImageTrainPreset[]>('image_training_presets'),
  imageTrainingEstimate: (presetId: string, steps: number) =>
    invoke<number>('image_training_estimate', { presetId, steps }),
  imageTrainingDatasets: () =>
    invoke<import('./types').ImageDatasetInfo[]>('image_training_datasets'),
  imageTrainingImport: (path: string) =>
    invoke<import('./types').ImageDatasetInfo>('image_training_import', { path }),
  imageTrainingCaption: (dataset: string) => invoke<string>('image_training_caption', { dataset }),
  imageTrainingStart: (presetId: string, dataset: string, repeats: number, name: string) =>
    invoke<string>('image_training_start', { presetId, dataset, repeats, name }),
  imageTrainingState: () => invoke<import('./types').ImageTrainingState>('image_training_state'),
  imageTrainingCancel: () => invoke<void>('image_training_cancel'),
  imageTrainingClear: () => invoke<void>('image_training_clear'),
  imageTrainingOpenFolder: (folder: 'datasets' | 'loras') =>
    invoke<void>('image_training_open_folder', { folder }),
  createDesktopShortcut: () => invoke<string>('create_desktop_shortcut'),
  applyAppUpdate: (onProgress: (p: AppUpdateProgress) => void) => {
    const channel = new Channel<AppUpdateProgress>();
    channel.onmessage = onProgress;
    return invoke<void>('apply_app_update', { onProgress: channel });
  },
};
