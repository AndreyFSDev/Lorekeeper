import { defaultSettings, ensureSettingsStructure } from '../config/default-settings.js';
import {
  getSettings as getExtensionSettings,
  saveSettings as saveExtensionSettings,
  settingsKey,
} from '../services/settings-service.js';
import { registerDomReadyHandler } from './lifecycle-hooks.js';

/**
 * Initialize the Moonlit Echoes extension context and schedule UI setup.
 * Handles settings bootstrapping, CSS toggling, and defers UI initialization
 * until the DOM is ready.
 *
 * @param {{ initExtensionUI: Function, toggleCss: Function }} dependencies - Startup dependencies provided by index.js.
 */
export function initExtension({ initExtensionUI, toggleCss }) {
  const context = SillyTavern.getContext();

  let extensionSettings = getExtensionSettings(context);
  if (!extensionSettings) {
    context.extensionSettings[settingsKey] = structuredClone(defaultSettings);
    extensionSettings = getExtensionSettings(context);
  }

  ensureSettingsStructure(extensionSettings);

  for (const key of Object.keys(defaultSettings)) {
    if (key !== 'presets' && key !== 'activePreset' && extensionSettings[key] === undefined) {
      extensionSettings[key] = defaultSettings[key];
    }
  }

  saveExtensionSettings(context);

  toggleCss(extensionSettings.enabled);

  registerDomReadyHandler(initExtensionUI);
}
