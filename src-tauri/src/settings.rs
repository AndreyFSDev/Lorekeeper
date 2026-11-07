use serde::{Deserialize, Serialize};

use crate::paths;

/// Application settings. Stored in `data/settings.json` in the install folder;
/// defaults are in `Defaults`, missing keys are filled in on load.
///
/// No `deny_unknown_fields` on purpose: real settings.json files carry legacy
/// keys from older versions (for example `top_k`, dropped in 1.2.66) and must
/// keep loading. Unknown-key rejection belongs to the recommend apply layer,
/// not to this file format.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub chat_model: String,
    pub vision_model: String,
    pub embed_model: String,
    pub context_size: u32,
    pub temperature: f32,
    pub top_p: f32,
    pub max_tokens: u32,
    pub auto_memory: bool,
    pub memory_max_chunks: usize,
    pub ollama_port: u16,
    /// Dev mode: use the system Ollama on 11434 instead of the portable one.
    pub use_system_ollama: bool,
    /// First-run wizard completed (runtime and models are in place).
    pub setup_done: bool,
    /// One-shot migration that resets the active character.
    pub coder_removed: bool,
    /// One-shot migration to the Echo character (historical, kept for compatibility).
    pub echo_migrated: bool,
    /// One-shot removal of the Echo demo character.
    pub echo_removed: bool,
    /// Model storage: Ollama, ComfyUI and the Hugging Face cache. Empty — the program folder.
    pub models_root: String,
    /// One-shot move of models to the storage.
    pub models_moved: bool,
    /// Version of the applied Cloufam theme. Below THEME_VERSION the theme
    /// is laid out again (a design update reaches installed copies); the user's
    /// choice of another theme is left alone.
    pub theme_version: u32,
    /// Version of the applied built-in character markup. Below TAGS_VERSION the
    /// set's tags are laid out again, keeping the user's own tags.
    pub tags_version: u32,
    /// Automatically open SillyTavern after the services come up (default false).
    pub auto_launch_st: bool,
    /// Active image generation model (ComfyUI checkpoint).
    pub image_model: String,
    /// Automatically start ComfyUI on launch.
    pub auto_launch_comfy: bool,
    /// Auto-tag SillyTavern characters when new cards appear.
    pub tagger_enabled: bool,
    /// One-shot migration of auto-tags to opt-in: on existing installs the
    /// default was "enabled", and tags were written into cards without asking.
    pub tagger_opt_in_migrated: bool,
    /// Automatically translate imported character cards to Russian.
    /// Default false: translation is a user option, not coercion.
    pub auto_translate_imports: bool,
    /// Model for tagging; empty — chat_model is used.
    pub tagger_model: String,
    /// Maximum tags per character (3..20).
    pub tagger_max_tags: u8,
    /// Tag language: ru | en | both.
    pub tagger_language: String,
    /// Write tags into the character card itself (PNG), not only into ST tags.
    pub tagger_write_card_tags: bool,
    /// Extra model instructions; empty — the built-in prompt.
    pub tagger_prompt: String,
    /// Hardware fingerprint the settings were tuned for automatically
    /// (see `recommend`). Empty — tuning has never been applied.
    pub tuned_for: String,
    /// Version of the update whose changelog the user has already seen.
    /// Empty — show "What's new" on the next launch.
    pub changelog_version_seen: String,
    /// Model for the background evaluator (live state + RPG). Empty —
    /// auto-selection by VRAM (`gateway::evaluator_model`): the same model
    /// that runs the chat, and if it does not fit entirely — the lightest installed
    /// one from the catalog. Auto-selection was wrong once (15.09.2026, MIN_EVALUATOR_MODEL) —
    /// an explicit user choice does not depend on free-VRAM heuristics.
    pub evaluator_model: String,
    /// Extra model instructions for director mode; empty — the built-in
    /// prompt (`director::build_prompt`) with no additions.
    pub director_prompt: String,
    /// Extra instructions for the state/stats/skills/relationships evaluator;
    /// empty — the built-in prompt (`rpg::prompts::combined_evaluator_prompt`).
    pub rpg_evaluator_prompt: String,
    /// Extra instructions for the combat round evaluator; empty — the built-in
    /// prompt (`rpg::prompts::combat_action_prompt`).
    pub rpg_combat_prompt: String,
    /// Rejected model upgrade suggestions (from-tags separated by commas):
    /// "Later" hides a pair forever, but new rules of the upgrade map
    /// (`catalog::MODEL_UPGRADES`) are shown again.
    pub models_upgrade_dismissed: String,
}

impl Default for Settings {
    fn default() -> Self {
        let gpu = crate::gpu::gpu_info();

        Self {
            // From this hardware's showcase: on the minimal tier 8B is not in it at all,
            // while higher tiers recommend 14b-v2, not 14b.
            chat_model: crate::catalog::recommended_chat_model(&gpu),
            vision_model: default_vision_model(&gpu).into(),
            embed_model: crate::catalog::recommended_embed_model(&gpu),
            context_size: 8192,
            temperature: 0.75,
            top_p: 0.9,
            max_tokens: 600,
            auto_memory: true,
            memory_max_chunks: 5,
            ollama_port: 11434,
            use_system_ollama: false,
            setup_done: false,
            coder_removed: false,
            echo_migrated: false,
            echo_removed: false,
            models_root: String::new(),
            models_moved: false,
            theme_version: 0,
            tags_version: 0,
            auto_launch_st: false,
            image_model: crate::comfyui::DEFAULT_IMAGE_CHECKPOINT.into(),
            auto_launch_comfy: true,
            tagger_enabled: false,
            tagger_opt_in_migrated: false,
            auto_translate_imports: false,
            tagger_model: String::new(),
            tagger_max_tags: 8,
            tagger_language: "ru".into(),
            tagger_write_card_tags: false,
            tagger_prompt: String::new(),
            tuned_for: String::new(),
            changelog_version_seen: String::new(),
            evaluator_model: String::new(),
            director_prompt: String::new(),
            rpg_evaluator_prompt: String::new(),
            rpg_combat_prompt: String::new(),
            models_upgrade_dismissed: String::new(),
        }
    }
}

/// Default vision model — from the showcase for this hardware: 6 GB vision models
/// will not run on a weak machine, downloading them is pointless. The qwen3.5
/// line is multimodal, so the same tags drive both chat and vision.
fn default_vision_model(gpu: &crate::gpu::GpuInfo) -> &'static str {
    use crate::gpu::HardwareTier;
    match crate::catalog::chat_ceiling(gpu) {
        HardwareTier::Minimal => "huihui_ai/qwen3.5-abliterated:2B",
        HardwareTier::Low => "huihui_ai/qwen3.5-abliterated:4B",
        HardwareTier::Mid | HardwareTier::High => "huihui_ai/qwen3.5-abliterated:9b",
    }
}

impl Settings {
    /// Load `data/settings.json`. Blocking IO on the caller's thread (boot and
    /// sync Tauri commands run it on the main thread, trainer probes go through
    /// `spawn_blocking`). A missing, unreadable or corrupt file falls back to
    /// defaults — but a corrupt one is preserved as `settings.json.corrupt`,
    /// and `save` refuses to overwrite either until it is repaired (V-09).
    pub fn load() -> Self {
        normalize(Self::load_from(&paths::settings_path()))
    }

    /// Persist settings atomically (tmp + fsync + rename via `json_store`).
    pub fn save(&self) -> Result<(), String> {
        self.save_to(&paths::settings_path())
    }

    fn load_from(path: &std::path::Path) -> Settings {
        match crate::json_store::read_state(path) {
            Ok(Some(settings)) => settings,
            // read_state has already logged and backed up a corrupt file.
            _ => Settings::default(),
        }
    }

    /// V-09: refuse to overwrite an existing settings.json that could not be
    /// read or parsed — its content is unknown (or already preserved as
    /// `settings.json.corrupt`), and writing would destroy the only copy.
    fn save_to(&self, path: &std::path::Path) -> Result<(), String> {
        let _on_disk = crate::json_store::read_state::<Settings>(path)
            .map_err(|e| format!("настройки не сохранены: {e}"))?;
        crate::json_store::save_value(self, path)
    }
}

/// One-time migration of auto-tags to opt-in: the default was "enabled", and
/// tags were written into user cards without asking. Returns `true` if the
/// settings changed and need to be saved.
pub fn migrate_tagger_opt_in(s: &mut Settings) -> bool {
    if s.tagger_opt_in_migrated {
        return false;
    }
    s.tagger_enabled = false;
    s.tagger_opt_in_migrated = true;
    true
}

/// Brings loaded settings to valid values and migrates old defaults.
fn normalize(mut s: Settings) -> Settings {
    // The port could have been moved because of a conflict — do not let it drop below the base.
    // The base is 11434: the portable serve takes exactly that one, so Ollama URLs
    // in ST settings (seeded for 11434) stay valid without edits.
    if s.ollama_port < 11434 {
        s.ollama_port = 11434;
    }
    // A context below 8192 leads to history truncation and empty replies in group chats.
    if s.context_size < 8192 {
        s.context_size = 8192;
    }
    // 0.3.x had qwen3:8b as the default. Migrate only the old default —
    // to the model the showcase recommends for this hardware.
    if s.chat_model == "qwen3:8b" {
        s.chat_model = crate::catalog::recommended_chat_model(&crate::gpu::gpu_info());
    }
    if s.image_model.is_empty() {
        s.image_model = crate::comfyui::DEFAULT_IMAGE_CHECKPOINT.into();
    }
    // Old vision defaults carried developer filters — switch to
    // the abliterated model (uncensored catalog, see decisions.md).
    if matches!(s.vision_model.as_str(), "qwen2.5vl:7b" | "minicpm-v:latest" | "minicpm-v") {
        s.vision_model = default_vision_model(&crate::gpu::gpu_info()).into();
    }
    s.tagger_max_tags = s.tagger_max_tags.clamp(3, 20);
    if !matches!(s.tagger_language.as_str(), "ru" | "en" | "both") {
        s.tagger_language = "ru".into();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Chat, memory and vision defaults must be in this machine's showcase: a tag
    /// outside the catalog is not highlighted as active in settings and has no
    /// size label in the wizard.
    #[test]
    fn test_default_models_are_in_visible_catalog() {
        let s = Settings::default();
        let visible = crate::catalog::visible(&crate::gpu::gpu_info());
        assert!(
            visible.iter().any(|m| m.category == "chat" && m.tag == s.chat_model),
            "модель чата {} не из витрины",
            s.chat_model
        );
        assert!(
            visible.iter().any(|m| m.category == "embed" && m.tag == s.embed_model),
            "модель памяти {} не из витрины",
            s.embed_model
        );
        assert!(
            visible.iter().any(|m| m.category == "vision" && m.tag == s.vision_model),
            "модель зрения {} не из витрины",
            s.vision_model
        );
    }

    #[test]
    fn test_migrate_qwen3_8b_model() {
        let s = normalize(Settings { chat_model: "qwen3:8b".into(), ..Settings::default() });
        assert_eq!(
            s.chat_model,
            crate::catalog::recommended_chat_model(&crate::gpu::gpu_info())
        );
        // The migration does not touch the user's choice.
        let s = normalize(Settings { chat_model: "huihui_ai/qwen3-abliterated:8b".into(), ..Settings::default() });
        assert_eq!(s.chat_model, "huihui_ai/qwen3-abliterated:8b");
    }

    /// Auto-translation and auto-tags are options, not coercion: a fresh install
    /// rewrites nothing in cards by itself.
    #[test]
    fn card_rewrite_features_are_opt_in() {
        let s = Settings::default();
        assert!(!s.auto_translate_imports);
        assert!(!s.tagger_enabled);
        assert!(!s.tagger_opt_in_migrated);
    }

    /// The migration disables auto-tags once; a manual enable after it is not
    /// switched back.
    #[test]
    fn tagger_opt_in_migration_runs_once() {
        let mut s = Settings::default();
        assert!(migrate_tagger_opt_in(&mut s));
        assert!(!s.tagger_enabled);
        assert!(s.tagger_opt_in_migrated);

        s.tagger_enabled = true;
        assert!(!migrate_tagger_opt_in(&mut s));
        assert!(s.tagger_enabled, "включённые пользователем авто-теги не гасим");
    }

    /// V-09/V-31 delegation: a corrupt settings.json is preserved as
    /// settings.json.corrupt before the load falls back to defaults.
    #[test]
    fn corrupt_settings_are_backed_up_before_default() {
        let dir = std::env::temp_dir().join("cloufam_test_settings_corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("json.corrupt"));
        std::fs::write(&path, "oops {{{").unwrap();
        let s = Settings::load_from(&path);
        assert_eq!(s.chat_model, Settings::default().chat_model);
        let backup = path.with_extension("json.corrupt");
        assert!(backup.exists(), "копия повреждённого файла не создана");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "oops {{{");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&backup);
        let _ = std::fs::remove_dir(&dir);
    }

    /// V-09: save must not write over an existing file it cannot read or parse
    /// — that state is an error, not an empty settings set. Non-UTF-8 bytes
    /// take the same branch as a Windows sharing violation (os error 32) or
    /// AccessDenied.
    #[test]
    fn save_refuses_over_corrupt_or_unreadable_settings() {
        let dir = std::env::temp_dir().join("cloufam_test_settings_guard");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let s = Settings { chat_model: "m".into(), ..Settings::default() };

        // Corrupt: refused, bytes untouched, backup kept.
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("json.corrupt"));
        std::fs::write(&path, "oops {{{").unwrap();
        assert!(s.save_to(&path).is_err(), "битый файл — ошибка, а не пустые настройки");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "oops {{{", "исходник перезаписан");
        assert!(path.with_extension("json.corrupt").exists(), "копия повреждённого файла не создана");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("json.corrupt"));

        // Unreadable: refused, raw bytes untouched.
        let raw = vec![0xff, 0xfe];
        std::fs::write(&path, &raw).unwrap();
        assert!(s.save_to(&path).is_err(), "нечитаемый файл — ошибка, а не пустые настройки");
        assert_eq!(std::fs::read(&path).unwrap(), raw, "файл перезаписан поверх неизвестного содержимого");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    /// save_to writes atomically (tmp + fsync + rename) and the file parses
    /// back to the same values.
    #[test]
    fn save_to_roundtrips() {
        let dir = std::env::temp_dir().join("cloufam_test_settings_atomic");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let _ = std::fs::remove_file(&path);
        let s = Settings { chat_model: "m".into(), ..Settings::default() };
        s.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path).chat_model, "m");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    /// Old settings.json files carry keys that no longer exist (`top_k` until
    /// 1.2.66) plus arbitrary leftovers — they must keep loading and saving
    /// (Settings has no `deny_unknown_fields` on purpose).
    #[test]
    fn legacy_and_unknown_keys_keep_loading() {
        let dir = std::env::temp_dir().join("cloufam_test_settings_legacy");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, r#"{"chat_model": "m", "top_k": 40, "gone_key": true}"#).unwrap();
        let s = Settings::load_from(&path);
        assert_eq!(s.chat_model, "m");
        s.save_to(&path).expect("легаси-файл не должен отклоняться гардом записи");
        assert_eq!(Settings::load_from(&path).chat_model, "m");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}

