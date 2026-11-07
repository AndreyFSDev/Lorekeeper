//! Tuning settings to the hardware: what this machine can really handle and why.
//!
//! We recommend only what is already on disk: suggesting a model that is absent
//! means suggesting broken generation — the showcase shows not-yet-downloaded models.
//! The rules are pure (input — a snapshot of hardware and disk contents), so they are
//! checked by tests without a machine: see `plan`.

use crate::gpu::{GpuBackend, GpuInfo, HardwareTier};
use crate::settings::Settings;

/// Context sizes the settings UI understands: we recommend a step from
/// this series, otherwise the chosen value would not be highlighted in the list.
const CONTEXT_STEPS: [u32; 3] = [32768, 16384, 8192];

/// "Heavy" checkpoint threshold: SDXL weighs ~6.5 GB, SD 1.5 — 2–3.5 GB.
const SDXL_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// Key of a setting controlled by tuning. A typed contract
/// "item → `Settings` field" (V-59): the field name lives only in `as_str`, while
/// the value is written through `write` into a concrete field — an unknown key name and
/// a value of a foreign type are rejected with an error in `apply`, not silently lost.
/// The name on the wire (IPC) is the same as the field name (checked by tests across all keys).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKey {
    ChatModel,
    VisionModel,
    EmbedModel,
    AutoMemory,
    ContextSize,
    MaxTokens,
    MemoryMaxChunks,
    AutoLaunchComfy,
    ImageModel,
}

impl SettingKey {
    /// All tuning keys. Tests walk the whole list so a new key
    /// cannot be added without typed-write coverage.
    pub const ALL: [SettingKey; 9] = [
        SettingKey::ChatModel,
        SettingKey::VisionModel,
        SettingKey::EmbedModel,
        SettingKey::AutoMemory,
        SettingKey::ContextSize,
        SettingKey::MaxTokens,
        SettingKey::MemoryMaxChunks,
        SettingKey::AutoLaunchComfy,
        SettingKey::ImageModel,
    ];

    /// `Settings` field name (also the key value on the IPC wire).
    pub fn as_str(self) -> &'static str {
        match self {
            SettingKey::ChatModel => "chat_model",
            SettingKey::VisionModel => "vision_model",
            SettingKey::EmbedModel => "embed_model",
            SettingKey::AutoMemory => "auto_memory",
            SettingKey::ContextSize => "context_size",
            SettingKey::MaxTokens => "max_tokens",
            SettingKey::MemoryMaxChunks => "memory_max_chunks",
            SettingKey::AutoLaunchComfy => "auto_launch_comfy",
            SettingKey::ImageModel => "image_model",
        }
    }

    /// Back from the field name; an unknown name is not a key.
    pub fn from_str(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|key| key.as_str() == name)
    }

    /// Typed write of the value into a `Settings` field. Each variant
    /// maps to exactly one field: an unmapped variant does not compile.
    fn write(self, settings: &mut Settings, value: serde_json::Value) -> Result<(), String> {
        fn take<T: serde::de::DeserializeOwned>(field: &str, value: serde_json::Value) -> Result<T, String> {
            serde_json::from_value(value).map_err(|e| format!("значение поля {field} не подходит: {e}"))
        }
        let field = self.as_str();
        match self {
            SettingKey::ChatModel => settings.chat_model = take(field, value)?,
            SettingKey::VisionModel => settings.vision_model = take(field, value)?,
            SettingKey::EmbedModel => settings.embed_model = take(field, value)?,
            SettingKey::AutoMemory => settings.auto_memory = take(field, value)?,
            SettingKey::ContextSize => settings.context_size = take(field, value)?,
            SettingKey::MaxTokens => settings.max_tokens = take(field, value)?,
            SettingKey::MemoryMaxChunks => settings.memory_max_chunks = take(field, value)?,
            SettingKey::AutoLaunchComfy => settings.auto_launch_comfy = take(field, value)?,
            SettingKey::ImageModel => settings.image_model = take(field, value)?,
        }
        Ok(())
    }
}

impl serde::Serialize for SettingKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for SettingKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = <String as serde::Deserialize>::deserialize(deserializer)?;
        SettingKey::from_str(&name).ok_or_else(|| serde::de::Error::custom(format!("неизвестный ключ настройки: {name}")))
    }
}

impl From<SettingKey> for String {
    fn from(key: SettingKey) -> Self {
        key.as_str().to_string()
    }
}

/// Comparing a key with a string (the UI and `lib.rs` compare `item.key` both with
/// IPC strings and with enum variants).
impl PartialEq<SettingKey> for String {
    fn eq(&self, other: &SettingKey) -> bool {
        self == other.as_str()
    }
}

#[derive(serde::Serialize, Clone, Debug)]
pub struct Recommendation {
    /// `Settings` field name (`SettingKey::as_str`) — used to apply
    /// the value; the "key → field" link is typed through `SettingKey` (V-59).
    pub key: String,
    pub title: String,
    /// What is there now, in human language.
    pub current: String,
    /// What we propose — also in human language.
    pub recommended: String,
    /// Why exactly so: hardware, VRAM, disk contents.
    pub reason: String,
    /// Value to write into the setting.
    pub value: serde_json::Value,
    /// Lowers a value set by the user (or disables an enabled one):
    /// the `lowers_user_value` function flag set in `plan`. Auto-tuning does not
    /// silently apply such an item (V-58) — it remains a report suggestion.
    pub lowers_user_value: bool,
}

#[derive(serde::Serialize, Clone)]
pub struct HardwareSummary {
    pub name: String,
    pub vram: String,
    pub backend: String,
    pub tier: String,
    pub notes: Vec<String>,
    pub fingerprint: String,
}

#[derive(serde::Serialize, Clone)]
pub struct Report {
    pub hardware: HardwareSummary,
    /// CPU, RAM and disk space — together with the GPU
    /// this is "the whole system" the settings are tuned for.
    pub system: crate::system::SystemInfo,
    /// Hardware fingerprint the settings were tuned for last time.
    pub tuned_for: String,
    /// Settings are already tuned for the current hardware.
    pub matches: bool,
    pub items: Vec<Recommendation>,
}

/// What is on the machine now — the input for tuning.
pub struct Input {
    pub hardware: GpuInfo,
    pub system: crate::system::SystemInfo,
    /// Tags of installed Ollama models.
    pub installed_models: Vec<String>,
    /// ComfyUI checkpoints: name and size.
    pub checkpoints: Vec<(String, u64)>,
    /// Live context ceiling for the active model (0 — could not be computed).
    pub context_ceiling: u32,
}

/// Hardware fingerprint: it shows that the settings were tuned for another machine.
/// Tier and backend only — recommendations depend on them (moving from 8 GB to 16 GB
/// changes the tier). nvidia-smi and the DXGI fallback report the
/// card name and exact VRAM differently (16303 vs ~15977 MiB): a one-off nvidia-smi failure
/// restarted tuning over manual settings.
pub fn fingerprint(hardware: &GpuInfo) -> String {
    format!("{}|{:?}", hardware.tier as u8, hardware.backend)
}

/// Whether the settings are tuned for this hardware. A previous-format fingerprint
/// ("tier|name|VRAM|backend") is checked by the same outer fields: otherwise the app
/// update itself would restart tuning for everyone who edited settings.
pub fn fingerprint_matches(stored: &str, hardware: &GpuInfo) -> bool {
    if !stored.contains('|') {
        return false;
    }
    let tier = stored.split('|').next().unwrap_or_default();
    let backend = stored.rsplit('|').next().unwrap_or_default();
    format!("{tier}|{backend}") == fingerprint(hardware)
}

pub fn summary(hardware: &GpuInfo) -> HardwareSummary {
    let backend = match hardware.backend {
        GpuBackend::Cuda => "CUDA (NVIDIA)",
        GpuBackend::Vulkan => "Vulkan (AMD/Intel)",
        GpuBackend::Cpu => "процессор, ускорения нет",
    };
    let tier = match crate::catalog::chat_ceiling(hardware) {
        HardwareTier::Minimal => "минимальный",
        HardwareTier::Low => "низкий",
        HardwareTier::Mid => "средний",
        HardwareTier::High => "высокий",
    };
    HardwareSummary {
        name: hardware.name.clone().unwrap_or_else(|| "дискретная видеокарта не найдена".into()),
        vram: match hardware.vram_mb {
            Some(mb) => format!("{:.1} ГБ VRAM", mb as f64 / 1024.0),
            None => "видеопамять недоступна".into(),
        },
        backend: backend.into(),
        tier: tier.into(),
        notes: hardware.notes.clone(),
        fingerprint: fingerprint(hardware),
    }
}

/// Context ceiling by hardware tier: above it only the KV cache grows, and with it
/// the reply time. The live VRAM ceiling may be lower — then we take it.
fn context_target(tier: HardwareTier) -> u32 {
    match tier {
        HardwareTier::Minimal | HardwareTier::Low => 8192,
        HardwareTier::Mid => 16384,
        HardwareTier::High => 32768,
    }
}

/// Reply length worth expecting on this hardware.
fn answer_target(tier: HardwareTier) -> u32 {
    match tier {
        HardwareTier::Minimal => 400,
        HardwareTier::Low => 600,
        HardwareTier::Mid => 800,
        HardwareTier::High => 1000,
    }
}

/// How many memory chunks to stitch into the prompt: on a weak machine they eat
/// the context the history itself needs.
fn memory_target(tier: HardwareTier) -> u32 {
    match tier {
        HardwareTier::Minimal => 3,
        HardwareTier::Low => 5,
        HardwareTier::Mid | HardwareTier::High => 8,
    }
}

fn has_tag(installed: &[String], tag: &str) -> bool {
    // Tags come in different forms (`bge-m3` / `bge-m3:latest`): it is one
    // model — compare through the shared contract, not exact text.
    crate::models::is_model_installed(installed, tag)
}

/// Tuning rules. Order as in the UI: chat model first, then its
/// parameters, then images.
pub fn plan(settings: &Settings, input: &Input) -> Vec<Recommendation> {
    let mut items: Vec<Recommendation> = Vec::new();
    let tier = crate::catalog::chat_ceiling(&input.hardware);
    let visible = crate::catalog::visible(&input.hardware);
    let hardware = summary(&input.hardware);

    // Models: take the highest downloaded from this hardware's showcase that also
    // fits memory — in CPU mode weights live in RAM, and a model
    // over half of memory sends the system into swap. If the catalog-recommended
    // one is downloaded, there is nothing to suggest.
    for (category, key, title) in [
        ("chat", SettingKey::ChatModel, "Модель чата"),
        ("vision", SettingKey::VisionModel, "Модель зрения"),
    ] {
        let current = match key {
            SettingKey::ChatModel => settings.chat_model.clone(),
            _ => settings.vision_model.clone(),
        };
        // An own model outside the catalog (fine-tuned cloufam-*, downloaded by hand)
        // is a deliberate choice: "Apply all" and auto_tune would silently swap it for the
        // catalog one, and the replacement item would hang forever.
        if has_tag(&input.installed_models, &current)
            && !crate::catalog::catalog().iter().any(|m| has_tag(&[m.tag.clone()], &current))
        {
            continue;
        }
        let installed_here: Vec<&crate::catalog::CatalogModel> = visible
            .iter()
            .filter(|m| m.category == category && has_tag(&input.installed_models, &m.tag))
            .collect();
        let fits = |m: &&crate::catalog::CatalogModel| {
            crate::catalog::model_size_bytes(&m.size)
                .map(|bytes| input.system.fits_in_ram(bytes, input.hardware.vram_mb))
                .unwrap_or(true)
        };
        let fitting: Vec<&crate::catalog::CatalogModel> =
            installed_here.iter().copied().filter(fits).collect();
        let best = fitting
            .iter()
            .max_by_key(|m| (m.min_tier, crate::catalog::model_size_bytes(&m.size).unwrap_or(0), m.tag.clone()))
            .copied();
        // A downloaded model is larger than the selected one but did not fit — say so:
        // otherwise the choice "why not 14B" looks arbitrary.
        let skipped = installed_here
            .iter()
            .copied()
            .filter(|m| !fits(&m))
            .max_by_key(|m| (m.min_tier, crate::catalog::model_size_bytes(&m.size).unwrap_or(0), m.tag.clone()));
        let Some(best) = best else {
            // All downloaded models of this category do not fit memory —
            // say so instead of staying silent (otherwise the user does not understand
            // why there are no recommendations).
            if let Some(largest) = installed_here
                .iter()
                .max_by_key(|m| crate::catalog::model_size_bytes(&m.size).unwrap_or(0))
            {
                if !largest.tag.eq_ignore_ascii_case(&current) {
                    items.push(Recommendation {
                        key: key.into(),
                        title: title.into(),
                        current: if current.is_empty() { "не выбрана".into() } else { current },
                        recommended: format!("{} ({})", largest.name, largest.tag),
                        reason: format!(
                            "Все скачанные модели {} не помещаются в оперативную память ({:.0} ГБ). \
                             {} — самая крупная, но будет работать частично на процессоре.",
                            category,
                            input.system.ram_total_gb(),
                            largest.name,
                        ),
                        value: serde_json::json!(largest.tag),
                        lowers_user_value: false,
                    });
                }
            }
            continue;
        };
        if best.tag.eq_ignore_ascii_case(&current) {
            continue;
        }
        let mut reason = format!(
            "Старшая из скачанных, которая помещается в память: {} ({}, ускорение — {}).",
            best.size, best.quant, hardware.backend
        );
        if let Some(skipped) = skipped {
            reason.push_str(&format!(
                " {} не влезает: весов больше половины оперативной памяти ({:.0} ГБ).",
                skipped.name,
                input.system.ram_total_gb()
            ));
        }
        items.push(Recommendation {
            key: key.into(),
            title: title.into(),
            current: if current.is_empty() { "не выбрана".into() } else { current },
            recommended: format!("{} ({})", best.name, best.tag),
            reason,
            value: serde_json::json!(best.tag),
            lowers_user_value: false,
        });
    }

    // Memory model: acceleration exists — bge-m3, on CPU — nomic-embed-text.
    // We do not change the downloaded selected model: chat vectors in ST are built by it
    // (the index lives under the model name), and nomic and bge-m3 have different dimensions —
    // a change would silently zero the memory. We offer only if the selected one is not on disk.
    let embed = crate::catalog::recommended_embed_model(&input.hardware);
    if has_tag(&input.installed_models, &embed) && !has_tag(&input.installed_models, &settings.embed_model) {
        items.push(Recommendation {
            key: SettingKey::EmbedModel.into(),
            title: "Модель векторной памяти".into(),
            current: if settings.embed_model.is_empty() {
                "не выбрана".into()
            } else {
                settings.embed_model.clone()
            },
            recommended: embed.clone(),
            reason: format!(
                "Выбранной модели памяти нет на диске, а эта уже скачана. Под {} каталог даёт именно её.",
                hardware.backend
            ),
            value: serde_json::json!(embed),
            lowers_user_value: false,
        });
    }
    if !settings.auto_memory && has_tag(&input.installed_models, &embed) {
        items.push(Recommendation {
            key: SettingKey::AutoMemory.into(),
            title: "Векторная память".into(),
            current: "выключена".into(),
            recommended: "включена".into(),
            reason: format!("Модель памяти {embed} на диске — память можно включить."),
            value: serde_json::json!(true),
            lowers_user_value: false,
        });
    }

    // Context: a step from the UI series, not above the tier target and not above
    // the live VRAM ceiling. We do not propose lowering: the ceiling is computed from
    // free memory at the moment (a game or a foreign model on the card lowers it),
    // and for such a case the context is already trimmed on the fly — see
    // `with_effective_context` and `num_ctx` in the gateway.
    let target = context_target(tier).min(if input.context_ceiling > 0 {
        input.context_ceiling
    } else {
        u32::MAX
    });
    let context = CONTEXT_STEPS
        .iter()
        .copied()
        .find(|step| *step <= target)
        .unwrap_or(8192);
    if context > settings.context_size {
        items.push(Recommendation {
            key: SettingKey::ContextSize.into(),
            title: "Размер контекста".into(),
            current: format!("{} токенов", settings.context_size),
            recommended: format!("{context} токенов"),
            reason: format!(
                "{context} токенов помещаются в видеопамять под модель {} — история диалога станет длиннее.",
                settings.chat_model
            ),
            value: serde_json::json!(context),
            lowers_user_value: false,
        });
    }

    // The numbers below are compared with the tier target in both directions: a proposal to lower
    // stays in the report (the user applies it) but is marked as
    // a lowering — auto-tuning does not apply it silently (V-58). Same motive as
    // for the context above: the tier ceiling is computed from hardware at the moment,
    // and a false GPU detection has no right to overwrite manual numbers.
    let answer = answer_target(tier);
    if settings.max_tokens != answer {
        items.push(Recommendation {
            key: SettingKey::MaxTokens.into(),
            title: "Длина ответа".into(),
            current: format!("{} токенов", settings.max_tokens),
            recommended: format!("{answer} токенов"),
            reason: match tier {
                HardwareTier::Minimal => "На процессоре ответ длиннее 400 токенов ждать минутами.".into(),
                HardwareTier::Low => "Карта считает медленнее старших: 600 токенов — предел без долгого ожидания.".into(),
                _ => "Видеопамяти хватает на длинные ответы.".into(),
            },
            value: serde_json::json!(answer),
            lowers_user_value: false,
        });
    }

    let chunks = memory_target(tier);
    if settings.memory_max_chunks as u32 != chunks {
        items.push(Recommendation {
            key: SettingKey::MemoryMaxChunks.into(),
            title: "Кусков памяти в промпте".into(),
            current: format!("{}", settings.memory_max_chunks),
            recommended: format!("{chunks}"),
            reason: match tier {
                HardwareTier::Minimal => "Куски памяти занимают контекст, которого на слабой машине и так немного.".into(),
                HardwareTier::Low => "Пять кусков — компромисс: память о прошлом против места под историю.".into(),
                _ => "Контекста хватает на восемь кусков — персонаж помнит больше деталей.".into(),
            },
            value: serde_json::json!(chunks),
            lowers_user_value: false,
        });
    }

    // Images: ComfyUI computes only on CUDA, and the checkpoint is chosen by VRAM.
    let images = crate::catalog::image_ceiling(&input.hardware).is_some();
    if settings.auto_launch_comfy != images {
        items.push(Recommendation {
            key: SettingKey::AutoLaunchComfy.into(),
            title: "Запускать ComfyUI со стартом".into(),
            current: if settings.auto_launch_comfy { "включено".into() } else { "выключено".into() },
            recommended: if images { "включено".into() } else { "выключено".into() },
            reason: if images {
                "Картинки на этой карте работают — сервер поднимется вместе с программой.".into()
            } else {
                "ComfyUI считает только на CUDA, поэтому картинок на этом железе нет: держать сервер запущенным незачем.".to_string()
            },
            value: serde_json::json!(images),
            lowers_user_value: false,
        });
    }
    if images {
        let sdxl = tier >= HardwareTier::Mid;
        let suits = |name: &str, size: u64| crate::comfyui::is_image_checkpoint(name) && (size > SDXL_BYTES) == sdxl;
        // A selected checkpoint of the right class is kept: otherwise, with several
        // files of one class, tuning would change it to the first in folder order.
        let current_suits = input
            .checkpoints
            .iter()
            .any(|(name, size)| name.eq_ignore_ascii_case(&settings.image_model) && suits(name, *size));
        let wanted = input.checkpoints.iter().find(|(name, size)| !current_suits && suits(name, *size));
        if let Some((name, size)) = wanted {
            if !name.eq_ignore_ascii_case(&settings.image_model) {
                items.push(Recommendation {
                    key: SettingKey::ImageModel.into(),
                    title: "Чекпоинт картинок".into(),
                    current: if settings.image_model.is_empty() {
                        "не выбран".into()
                    } else {
                        settings.image_model.clone()
                    },
                    recommended: name.clone(),
                    reason: if sdxl {
                        format!(
                            "SDXL: {:.1} ГБ, на {:.1} ГБ видеопамяти считается в 1024×1024.",
                            *size as f64 / 1073741824.0,
                            input.hardware.vram_mb.unwrap_or(0) as f64 / 1024.0
                        )
                    } else {
                        format!(
                            "SDXL на {:.1} ГБ видеопамяти не помещается, а этот SD 1.5 ({:.1} ГБ) считается вдвое быстрее.",
                            input.hardware.vram_mb.unwrap_or(0) as f64 / 1024.0,
                            *size as f64 / 1073741824.0
                        )
                    },
                    value: serde_json::json!(name),
                    lowers_user_value: false,
                });
            }
        }
    }

    // V-58 policy (`lowers_user_value`) is set here — one flag for
    // both the report and auto-tuning.
    for item in &mut items {
        item.lowers_user_value = lowers_user_value(item, settings, &input);
    }
    items
}

/// Write the recommended values into settings. `fingerprint` — the hardware
/// fingerprint they were tuned for: empty — settings stay "untuned".
/// The key is checked against the `Settings` field through `SettingKey` and written typed
/// (V-59): an unknown name or a value of a foreign type is an error, not a silent write
/// past the field (the former raw string write into the settings JSON object is removed).
pub fn apply(
    settings: &Settings,
    items: &[Recommendation],
    fingerprint: Option<&str>,
) -> Result<Settings, String> {
    let mut next = settings.clone();
    for item in items {
        let key = SettingKey::from_str(&item.key)
            .ok_or_else(|| format!("неизвестный ключ настройки: {}", item.key))?;
        key.write(&mut next, item.value.clone())?;
    }
    if let Some(fp) = fingerprint {
        next.tuned_for = fp.to_string();
    }
    Ok(next)
}

/// Whether the item lowers a value set by the user (V-58): auto-tuning does not
/// silently apply such an item — it remains a report suggestion.
/// An unknown key counts as a lowering: auto-application does not guess (V-59).
pub fn lowers_user_value(item: &Recommendation, settings: &Settings, input: &Input) -> bool {
    let Some(key) = SettingKey::from_str(&item.key) else {
        return true;
    };
    match key {
        // Numbers (context, reply length, memory chunks) — only upward: the same motive
        // as with `context_size` in `plan` — the hardware snapshot is computed
        // at the moment and may underestimate the tier.
        SettingKey::ContextSize => lower_number(item.value.as_f64(), settings.context_size as f64),
        SettingKey::MaxTokens => lower_number(item.value.as_f64(), settings.max_tokens as f64),
        SettingKey::MemoryMaxChunks => lower_number(item.value.as_f64(), settings.memory_max_chunks as f64),
        // Flags — only enabling: turning off ComfyUI autolaunch over a manual
        // "enabled" is also a lowering.
        SettingKey::AutoMemory => lower_flag(item.value.as_bool(), settings.auto_memory),
        SettingKey::AutoLaunchComfy => lower_flag(item.value.as_bool(), settings.auto_launch_comfy),
        // Models: replacing a working choice — only as a suggestion. We silently fix
        // what no longer works: empty or removed from disk.
        SettingKey::ChatModel => lower_model(item.value.as_str(), &settings.chat_model, input),
        SettingKey::VisionModel => lower_model(item.value.as_str(), &settings.vision_model, input),
        SettingKey::EmbedModel => lower_model(item.value.as_str(), &settings.embed_model, input),
        SettingKey::ImageModel => lower_model(item.value.as_str(), &settings.image_model, input),
    }
}

/// A value of the wrong type — auto-application does not guess (V-59).
fn lower_number(next: Option<f64>, current: f64) -> bool {
    match next {
        Some(next) => next < current,
        None => true,
    }
}

fn lower_flag(next: Option<bool>, current: bool) -> bool {
    !next.unwrap_or(false) && current
}

fn lower_model(next: Option<&str>, current: &str, input: &Input) -> bool {
    match next {
        Some(next) if !current.trim().is_empty() && !next.eq_ignore_ascii_case(current) => {
            model_on_disk(current, input)
        }
        _ => false,
    }
}

/// Whether the model is on disk: an Ollama tag among installed ones or a ComfyUI
/// checkpoint among those in the folder.
fn model_on_disk(tag: &str, input: &Input) -> bool {
    has_tag(&input.installed_models, tag)
        || input.checkpoints.iter().any(|(name, _)| name.eq_ignore_ascii_case(tag))
}

/// What auto-tuning (`lib::auto_tune`) may do silently on this start.
pub struct AutoTunePlan {
    /// Hardware fingerprint the settings are aligned to.
    pub fingerprint: String,
    /// Items applied without asking: they do not lower user values.
    pub applicable: Vec<Recommendation>,
    /// Would lower manual values — report suggestion and log only,
    /// no silent writes.
    pub offered: Vec<Recommendation>,
    /// Whether to set `Settings::tuned_for` with this fingerprint. False while there are
    /// suggestions (the plan did not settle) or no downloaded models — tuning must
    /// come back when there is something to choose from.
    pub stamp: bool,
}

/// Auto-tuning decision for the current hardware (V-58).
pub enum AutoTuneAction {
    /// Settings are already tuned for this hardware — nothing to touch.
    Skip,
    /// The hardware probe looks failed (see `detection_suspect`): touch neither
    /// values nor fingerprint — otherwise a false boot would overwrite manual settings,
    /// and the next one would return them back.
    Suspect,
    /// Align settings to the plan: only `applicable` is written, the fingerprint —
    /// only with `stamp`.
    Apply(AutoTunePlan),
}

/// Auto-tuning policy in one place: lowering — suggestion only,
/// a suspicious hardware probe — with no writes at all.
pub fn auto_tune_action(stored: &str, settings: &Settings, input: &Input) -> AutoTuneAction {
    if fingerprint_matches(stored, &input.hardware) {
        return AutoTuneAction::Skip;
    }
    if detection_suspect(stored, &input.hardware) {
        return AutoTuneAction::Suspect;
    }
    let (mut applicable, mut offered) = (Vec::new(), Vec::new());
    for item in plan(settings, input) {
        if item.lowers_user_value {
            offered.push(item);
        } else {
            applicable.push(item);
        }
    }
    AutoTuneAction::Apply(AutoTunePlan {
        fingerprint: fingerprint(&input.hardware),
        stamp: offered.is_empty() && !input.installed_models.is_empty(),
        applicable,
        offered,
    })
}

/// The same plan in `Option` form: `None` — touch nothing (`Skip` or
/// a suspicious hardware probe), `Some` — align settings to the plan.
pub fn auto_tune_plan(stored: &str, settings: &Settings, input: &Input) -> Option<AutoTunePlan> {
    match auto_tune_action(stored, settings, input) {
        AutoTuneAction::Apply(plan) => Some(plan),
        AutoTuneAction::Skip | AutoTuneAction::Suspect => None,
    }
}

/// (tier, backend) from the hardware fingerprint in settings; both fingerprint formats
/// ("tier|backend" and the former "tier|name|VRAM|backend") are checked by the outer fields.
fn fingerprint_parts(stored: &str) -> Option<(u8, &str)> {
    if !stored.contains('|') {
        return None;
    }
    let tier = stored.split('|').next()?.trim().parse::<u8>().ok()?;
    let backend = stored.rsplit('|').next()?.trim();
    if backend.is_empty() {
        return None;
    }
    Some((tier, backend))
}

/// Whether the hardware detection looks failed and the card did not change: the previous
/// fingerprint was for an accelerated card, and now it is not visible (a laptop dGPU
/// powered off — nvidia-smi fails, DXGI returns the iGPU; both probes failed —
/// `Cpu`/`Minimal`) or a weaker one is visible. In such a setting auto-tuning touches
/// nothing: suggestions stay in the report, the user applies them.
fn detection_suspect(stored: &str, hardware: &GpuInfo) -> bool {
    let Some((tier, backend)) = fingerprint_parts(stored) else {
        // No fingerprint yet — this is the first tuning, it has nothing to overwrite.
        return false;
    };
    if !matches!(backend, "Cuda" | "Vulkan") {
        return false;
    }
    if !hardware.has_gpu || hardware.backend == GpuBackend::Cpu {
        return true;
    }
    // Acceleration exists, but the tier is lower than recorded: the card suddenly "weakened" —
    // either it is another (integrated) adapter, or a card replacement is a human decision.
    (hardware.tier as u8) < tier
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::{build_vendor_info, GpuVendor};

    fn nvidia(vram_mb: u64) -> GpuInfo {
        build_vendor_info(GpuVendor::Nvidia, "NVIDIA GeForce RTX".into(), vram_mb)
    }

    fn cpu() -> GpuInfo {
        GpuInfo {
            has_gpu: false,
            vendor: GpuVendor::Unknown,
            name: None,
            vram_mb: None,
            backend: GpuBackend::Cpu,
            tier: HardwareTier::Minimal,
            notes: Vec::new(),
        }
    }

    fn input(hardware: GpuInfo, installed: &[&str], checkpoints: &[(&str, u64)]) -> Input {
        Input {
            hardware,
            system: crate::system::SystemInfo {
                cpu: "тестовый процессор".into(),
                cpu_cores: 8,
                cpu_threads: 16,
                ram_total_mb: 32768,
                ram_free_mb: 16384,
                disk_free_gb: 500.0,
            },
            installed_models: installed.iter().map(|s| s.to_string()).collect(),
            checkpoints: checkpoints.iter().map(|(n, s)| (n.to_string(), *s)).collect(),
            context_ceiling: 0,
        }
    }

    fn find<'a>(items: &'a [Recommendation], key: SettingKey) -> Option<&'a Recommendation> {
        items.iter().find(|i| i.key == key.as_str())
    }

    /// Installed models: on 16 GB the highest downloaded is 14B, on 8 GB — 8B.
    #[test]
    fn test_chat_model_follows_installed_and_tier() {
        let s = Settings {
            chat_model: "huihui_ai/qwen3.5-abliterated:4B".into(),
            ..Settings::default()
        };
        let items = plan(
            &s,
            &input(
                nvidia(16303),
                &["huihui_ai/qwen3.5-abliterated:4B", "huihui_ai/qwen3.5-abliterated:9b"],
                &[],
            ),
        );
        assert_eq!(find(&items, SettingKey::ChatModel).unwrap().value, serde_json::json!("huihui_ai/qwen3.5-abliterated:9b"));

        // 14B on 8 GB does not make it into the showcase — we recommend what really fits.
        let s = Settings {
            chat_model: "huihui_ai/qwen3.5-abliterated:9b".into(),
            ..Settings::default()
        };
        let items = plan(
            &s,
            &input(
                nvidia(8192),
                &["huihui_ai/qwen3.5-abliterated:4B", "huihui_ai/qwen3.5-abliterated:9b"],
                &[],
            ),
        );
        assert_eq!(find(&items, SettingKey::ChatModel).unwrap().value, serde_json::json!("huihui_ai/qwen3.5-abliterated:4B"));
    }

    /// Nothing downloaded — stay silent: nothing to suggest, the model is not on disk.
    #[test]
    fn test_nothing_installed_gives_no_model_items() {
        let s = Settings::default();
        let items = plan(&s, &input(nvidia(16303), &[], &[]));
        assert!(find(&items, SettingKey::ChatModel).is_none());
        assert!(find(&items, SettingKey::VisionModel).is_none());
        assert!(find(&items, SettingKey::EmbedModel).is_none());
    }

    /// `has_tag` must understand forms of one tag: the catalog and Ollama return
    /// `bge-m3` / `bge-m3:latest` for the same model.
    #[test]
    fn test_has_tag_matches_tag_forms() {
        let installed = vec![
            "bge-m3:latest".to_string(),
            "huihui_ai/qwen3.5-abliterated:9b".to_string(),
        ];
        assert!(has_tag(&installed, "bge-m3"));
        assert!(has_tag(&installed, "bge-m3:latest"));
        assert!(has_tag(&installed, "huihui_ai/qwen3.5-abliterated:9b"));
        assert!(!has_tag(&installed, "missing-model"));
    }

    /// Without a GPU the model lives in RAM: on 4 GB we do not advise 4B (2.3 GB
    /// of weights), while on 8 GB the same model passes. The catalog does not filter
    /// by one memory figure — tuning does that.
    #[test]
    fn test_cpu_mode_respects_ram() {
        let s = Settings {
            chat_model: "huihui_ai/qwen3.5-abliterated:2B".into(),
            ..Settings::default()
        };
        let tiny_models = ["huihui_ai/qwen3.5-abliterated:2B", "huihui_ai/qwen3.5-abliterated:4B"];

        let mut big = input(cpu(), &tiny_models, &[]);
        big.system.ram_total_mb = 8192;
        let items = plan(&s, &big);
        assert_eq!(
            find(&items, SettingKey::ChatModel).unwrap().value,
            serde_json::json!("huihui_ai/qwen3.5-abliterated:4B"),
            "на 8 ГБ памяти 4B проходит"
        );

        // On 4 GB of memory 4B does not fit anymore: stay on the manually selected model.
        let mut tiny = input(cpu(), &tiny_models, &[]);
        tiny.system.ram_total_mb = 4096;
        let items = plan(&s, &tiny);
        assert!(find(&items, SettingKey::ChatModel).is_none(), "4B не помещается в 4 ГБ памяти");
    }

    /// When the highest downloaded model is filtered out by memory, the reason says so.
    #[test]
    fn test_ram_skip_is_explained() {
        let s = Settings {
            chat_model: String::new(),
            ..Settings::default()
        };
        let mut small = input(
            cpu(),
            &["huihui_ai/qwen3.5-abliterated:2B", "huihui_ai/qwen3.5-abliterated:4B"],
            &[],
        );
        small.system.ram_total_mb = 4096;
        let items = plan(&s, &small);
        let chat = find(&items, SettingKey::ChatModel).expect("1.7B помещается в 4 ГБ");
        assert_eq!(chat.value, serde_json::json!("huihui_ai/qwen3.5-abliterated:2B"));
        assert!(chat.reason.contains("оперативной памяти"), "причина: {}", chat.reason);
    }

    /// We raise the context only if it fits the live VRAM
    /// ceiling, and never propose lowering: the ceiling is computed from
    /// free memory at the moment, and on-the-fly trimming is already done in the raise.
    #[test]
    fn test_context_only_raised_and_capped_by_vram() {
        let s = Settings { context_size: 8192, ..Settings::default() };
        let mut i = input(nvidia(16303), &[], &[]);
        i.context_ceiling = 20000;
        let items = plan(&s, &i);
        assert_eq!(find(&items, SettingKey::ContextSize).unwrap().value, serde_json::json!(16384));

        let mut i = input(nvidia(16303), &[], &[]);
        i.context_ceiling = 9000;
        let items = plan(&s, &i);
        assert!(find(&items, SettingKey::ContextSize).is_none(), "9000 не даёт поднять до 16384");

        // A high configured context under a tight ceiling is not lowered.
        let s = Settings { context_size: 32768, ..Settings::default() };
        let mut i = input(nvidia(16303), &[], &[]);
        i.context_ceiling = 8192;
        let items = plan(&s, &i);
        assert!(find(&items, SettingKey::ContextSize).is_none(), "понижение контекста не предлагаем");
        assert_eq!(context_target(HardwareTier::High), 32768);
    }

    /// Images: SDXL on mid tier and above, SD 1.5 on a weak card; on CPU — off.
    #[test]
    fn test_image_checkpoint_follows_vram() {
        let s = Settings { image_model: String::new(), ..Settings::default() };
        let big = (crate::comfyui::DEFAULT_IMAGE_CHECKPOINT, 7_000_000_000u64);
        let small = ("AOM3_orangemixs.safetensors", 2_100_000_000u64);

        let items = plan(&s, &input(nvidia(16303), &[], &[big, small]));
        assert_eq!(find(&items, SettingKey::ImageModel).unwrap().value, serde_json::json!(big.0));

        let items = plan(&s, &input(nvidia(8192), &[], &[big, small]));
        assert_eq!(find(&items, SettingKey::ImageModel).unwrap().value, serde_json::json!(small.0));

        // An audio checkpoint in the same folder does not count as an image model.
        let audio = ("stable_audio_open_1.0.safetensors", 4_500_000_000u64);
        let items = plan(&s, &input(nvidia(8192), &[], &[audio, small]));
        assert_eq!(find(&items, SettingKey::ImageModel).unwrap().value, serde_json::json!(small.0));

        let items = plan(&s, &input(cpu(), &[], &[big, small]));
        assert!(find(&items, SettingKey::ImageModel).is_none());
        assert_eq!(find(&items, SettingKey::AutoLaunchComfy).unwrap().value, serde_json::json!(false));
    }

    /// Application writes values into settings and marks what hardware they were tuned for.
    #[test]
    fn test_apply_writes_values_and_fingerprint() {
        let s = Settings::default();
        let items = vec![
            Recommendation {
                key: SettingKey::ContextSize.into(),
                title: "Размер контекста".into(),
                current: "8192".into(),
                recommended: "16384".into(),
                reason: "тест".into(),
                value: serde_json::json!(16384),
                lowers_user_value: false,
            },
            Recommendation {
                key: SettingKey::MemoryMaxChunks.into(),
                title: "Куски памяти".into(),
                current: "5".into(),
                recommended: "8".into(),
                reason: "тест".into(),
                value: serde_json::json!(8),
                lowers_user_value: false,
            },
        ];
        let applied = apply(&s, &items, Some("fp-1")).unwrap();
        assert_eq!(applied.context_size, 16384);
        assert_eq!(applied.memory_max_chunks, 8);
        assert_eq!(applied.tuned_for, "fp-1");
        // The other fields are untouched.
        assert_eq!(applied.temperature, s.temperature);
        // Without a fingerprint the setting stays "untuned".
        assert!(apply(&s, &items, None).unwrap().tuned_for.is_empty());
    }

    /// The fingerprint changes with hardware and does not depend on free memory at the moment.
    #[test]
    fn test_fingerprint_tracks_hardware() {
        assert_eq!(fingerprint(&nvidia(16303)), fingerprint(&nvidia(16303)));
        assert_ne!(fingerprint(&nvidia(16303)), fingerprint(&nvidia(8192)));
        assert_ne!(fingerprint(&nvidia(16303)), fingerprint(&cpu()));
    }

    /// nvidia-smi and DXGI report different names and sizes for one card: the fingerprint
    /// does not change, and a previous-format one counts as the same hardware.
    #[test]
    fn test_fingerprint_is_stable_across_detection_paths() {
        let smi = build_vendor_info(GpuVendor::Nvidia, "NVIDIA GeForce RTX 5080".into(), 16303);
        let dxgi = build_vendor_info(GpuVendor::Nvidia, "NVIDIA GeForce RTX 5080 Laptop".into(), 15977);
        assert_eq!(fingerprint(&smi), fingerprint(&dxgi));
        assert!(fingerprint_matches(&fingerprint(&smi), &dxgi));
        assert!(fingerprint_matches("3|NVIDIA GeForce RTX 5080|16303|Cuda", &dxgi));
        assert!(!fingerprint_matches("3|NVIDIA GeForce RTX 5080|16303|Cuda", &nvidia(8192)));
        assert!(!fingerprint_matches(&fingerprint(&cpu()), &smi));
        assert!(!fingerprint_matches("", &smi));
    }

    /// An own model outside the catalog (fine-tuned, downloaded by hand) is not
    /// displaced by tuning; one removed from disk is no longer a choice.
    #[test]
    fn test_custom_chat_model_is_kept() {
        let s = Settings { chat_model: "cloufam-aria:latest".into(), ..Settings::default() };
        let items = plan(
            &s,
            &input(nvidia(16303), &["cloufam-aria:latest", "huihui_ai/qwen3.5-abliterated:9b"], &[]),
        );
        assert!(find(&items, SettingKey::ChatModel).is_none());

        let items = plan(&s, &input(nvidia(16303), &["huihui_ai/qwen3.5-abliterated:9b"], &[]));
        assert_eq!(
            find(&items, SettingKey::ChatModel).unwrap().value,
            serde_json::json!("huihui_ai/qwen3.5-abliterated:9b")
        );
    }

    /// We do not change a downloaded memory model: chat vectors are built by it. We suggest
    /// a replacement only instead of a model that is not on disk.
    #[test]
    fn test_embed_model_is_not_switched_when_installed() {
        let s = Settings { embed_model: "nomic-embed-text:latest".into(), ..Settings::default() };
        let items = plan(&s, &input(nvidia(16303), &["nomic-embed-text:latest", "bge-m3:latest"], &[]));
        assert!(find(&items, SettingKey::EmbedModel).is_none());

        // `bge-m3` and `bge-m3:latest` are one model, there should be no item.
        let s = Settings { embed_model: "bge-m3".into(), ..Settings::default() };
        let items = plan(&s, &input(nvidia(16303), &["bge-m3:latest"], &[]));
        assert!(find(&items, SettingKey::EmbedModel).is_none());

        let s = Settings { embed_model: "nomic-embed-text:latest".into(), ..Settings::default() };
        let items = plan(&s, &input(nvidia(16303), &["bge-m3:latest"], &[]));
        assert_eq!(find(&items, SettingKey::EmbedModel).unwrap().value, serde_json::json!("bge-m3:latest"));
    }

    /// The selected checkpoint of a suitable class stays, even if it is not first in the folder.
    #[test]
    fn test_image_checkpoint_keeps_suitable_current() {
        let first = ("animagine-xl-4.0.safetensors", 6_900_000_000u64);
        let current = (crate::comfyui::DEFAULT_IMAGE_CHECKPOINT, 7_000_000_000u64);
        let s = Settings { image_model: current.0.into(), ..Settings::default() };
        let items = plan(&s, &input(nvidia(16303), &[], &[first, current]));
        assert!(find(&items, SettingKey::ImageModel).is_none());

        // SDXL on 8 GB does not fit by class — we suggest SD 1.5.
        let small = ("AOM3_orangemixs.safetensors", 2_100_000_000u64);
        let items = plan(&s, &input(nvidia(8192), &[], &[current, small]));
        assert_eq!(find(&items, SettingKey::ImageModel).unwrap().value, serde_json::json!(small.0));
    }

    fn item(key: SettingKey, value: serde_json::Value) -> Recommendation {
        Recommendation {
            key: key.into(),
            title: "тест".into(),
            current: String::new(),
            recommended: String::new(),
            reason: "тест".into(),
            value,
            lowers_user_value: false,
        }
    }

    /// A value different from the default for the key. The match is exhaustive: a new
    /// key variant will force adding both the value and the check.
    fn sample(key: SettingKey, s: &Settings) -> serde_json::Value {
        match key {
            SettingKey::ChatModel => serde_json::json!("custom-chat"),
            SettingKey::VisionModel => serde_json::json!("custom-vision"),
            SettingKey::EmbedModel => serde_json::json!("custom-embed"),
            SettingKey::AutoMemory => serde_json::json!(!s.auto_memory),
            SettingKey::ContextSize => serde_json::json!(s.context_size + 4096),
            SettingKey::MaxTokens => serde_json::json!(s.max_tokens + 111),
            SettingKey::MemoryMaxChunks => serde_json::json!(s.memory_max_chunks + 3),
            SettingKey::AutoLaunchComfy => serde_json::json!(!s.auto_launch_comfy),
            SettingKey::ImageModel => serde_json::json!("custom.safetensors"),
        }
    }

    /// V-59: each key is written typed into its own field — iterating all
    /// keys, not a selection of two. The matches here and in `SettingKey::write`
    /// are exhaustive, so a new key cannot be added without handling.
    #[test]
    fn test_setting_key_writes_each_field_typed() {
        let s = Settings::default();
        for key in SettingKey::ALL {
            let applied = apply(&s, &[item(key, sample(key, &s))], None).unwrap();
            match key {
                SettingKey::ChatModel => assert_eq!(applied.chat_model, "custom-chat"),
                SettingKey::VisionModel => assert_eq!(applied.vision_model, "custom-vision"),
                SettingKey::EmbedModel => assert_eq!(applied.embed_model, "custom-embed"),
                SettingKey::AutoMemory => assert_eq!(applied.auto_memory, !s.auto_memory),
                SettingKey::ContextSize => assert_eq!(applied.context_size, s.context_size + 4096),
                SettingKey::MaxTokens => assert_eq!(applied.max_tokens, s.max_tokens + 111),
                SettingKey::MemoryMaxChunks => assert_eq!(applied.memory_max_chunks, s.memory_max_chunks + 3),
                SettingKey::AutoLaunchComfy => assert_eq!(applied.auto_launch_comfy, !s.auto_launch_comfy),
                SettingKey::ImageModel => assert_eq!(applied.image_model, "custom.safetensors"),
            }
        }
    }

    /// V-59: the key name on the wire (IPC: report → UI → `keys`) must
    /// coincide with the `Settings` field name — otherwise selecting an item in the UI
    /// writes past the field. And vice versa: a string with a typo is not a key.
    #[test]
    fn test_setting_key_wire_names_match_settings_fields() {
        let s = Settings::default();
        for key in SettingKey::ALL {
            let value = sample(key, &s);
            let applied = apply(&s, &[item(key, value.clone())], None).unwrap();
            let obj = serde_json::to_value(&applied).unwrap();
            assert_eq!(obj.get(key.as_str()), Some(&value), "ключ {} не доехал до поля", key.as_str());
            assert_eq!(serde_json::to_value(key).unwrap(), serde_json::json!(key.as_str()));
            assert_eq!(SettingKey::from_str(key.as_str()), Some(key));
        }
        assert_eq!(SettingKey::from_str("memory_max_chunk"), None);
    }

    /// A value of a foreign type is rejected with an error instead of silently disappearing.
    #[test]
    fn test_apply_rejects_wrong_value_type() {
        let s = Settings::default();
        let bad = item(SettingKey::ContextSize, serde_json::json!("не число"));
        assert!(apply(&s, &[bad], None).is_err());
    }

    /// V-58: lowering a number or disabling a flag is lowering a user
    /// value, and auto-tuning does not apply such items silently. Raising
    /// a number and enabling a flag is not lowering: it writes them. Replacing a model is
    /// the subject of tuning, but even a working model is not overwritten silently:
    /// only what is no longer on disk is repaired.
    #[test]
    fn test_policy_marks_lowering_items() {
        let s = Settings {
            max_tokens: 4000,
            memory_max_chunks: 8,
            auto_launch_comfy: true,
            chat_model: "huihui_ai/qwen3.5-abliterated:4B".into(),
            ..Settings::default()
        };
        let cpu = input(cpu(), &["huihui_ai/qwen3.5-abliterated:4B"], &[]);
        let items = plan(&s, &cpu);
        assert!(lowers_user_value(find(&items, SettingKey::MaxTokens).unwrap(), &s, &cpu));
        assert!(lowers_user_value(find(&items, SettingKey::MemoryMaxChunks).unwrap(), &s, &cpu));
        assert!(lowers_user_value(find(&items, SettingKey::AutoLaunchComfy).unwrap(), &s, &cpu));

        let s = Settings { max_tokens: 200, auto_launch_comfy: false, ..Settings::default() };
        let mid = input(nvidia(12288), &[], &[]);
        let items = plan(&s, &mid);
        assert!(!lowers_user_value(find(&items, SettingKey::MaxTokens).unwrap(), &s, &mid));
        assert!(!lowers_user_value(find(&items, SettingKey::AutoLaunchComfy).unwrap(), &s, &mid));

        // A working model changes only by suggestion, one removed from disk — is repaired silently.
        let s = Settings { chat_model: "huihui_ai/qwen3.5-abliterated:4B".into(), ..Settings::default() };
        let working = input(nvidia(16303), &["huihui_ai/qwen3.5-abliterated:4B", "huihui_ai/qwen3.5-abliterated:9b"], &[]);
        assert!(lowers_user_value(find(&plan(&s, &working), SettingKey::ChatModel).unwrap(), &s, &working));
        let broken = input(nvidia(16303), &["huihui_ai/qwen3.5-abliterated:9b"], &[]);
        assert!(!lowers_user_value(find(&plan(&s, &broken), SettingKey::ChatModel).unwrap(), &s, &broken));

        // An unknown key — auto-application does not guess (V-59).
        let unknown = Recommendation {
            key: "memory_max_chunk".into(),
            title: "тест".into(),
            current: String::new(),
            recommended: String::new(),
            reason: "тест".into(),
            value: serde_json::json!(9),
            lowers_user_value: true,
        };
        assert!(lowers_user_value(&unknown, &s, &broken));
        assert!(apply(&s, &[unknown], None).is_err());
    }

    /// V-58: auto-tuning writes only `applicable`, a lowering stays
    /// `offered` — a report suggestion; the fingerprint is set by a plan without suggestions.
    #[test]
    fn test_auto_tune_action_applies_only_non_lowering() {
        let s = Settings {
            max_tokens: 2000,
            memory_max_chunks: 5,
            auto_launch_comfy: false,
            ..Settings::default()
        };
        // Card change without suspicions: Low → Mid, previous fingerprint for the same.
        let i = input(nvidia(12288), &[], &[]);
        let AutoTuneAction::Apply(tuned) = auto_tune_action(&fingerprint(&nvidia(8192)), &s, &i) else {
            panic!("апгрейд карты подбирается");
        };
        assert!(tuned.applicable.iter().all(|item| !lowers_user_value(item, &s, &i)));
        assert!(
            find(&tuned.applicable, SettingKey::MaxTokens).is_none(),
            "2000 → 800 — понижение, молча не применяем"
        );
        assert!(find(&tuned.offered, SettingKey::MaxTokens).is_some(), "понижение остаётся предложением");
        assert_eq!(find(&tuned.applicable, SettingKey::MemoryMaxChunks).unwrap().value, serde_json::json!(8));
        assert_eq!(find(&tuned.applicable, SettingKey::AutoLaunchComfy).unwrap().value, serde_json::json!(true));
        assert!(!tuned.stamp, "пока есть предложения, отпечаток не ставится");
        // `items` — the same list as `applicable`; the Option form of the same plan.
        assert!(auto_tune_plan(&fingerprint(&nvidia(8192)), &s, &i).is_some());
    }

    /// V-58: a boot with a failed hardware probe gives no plan at all — and the next
    /// successful boot does not touch manual values either.
    #[test]
    fn test_auto_tune_action_skips_suspect_detection() {
        let tuned_for = fingerprint(&nvidia(16303));
        // A dGPU powered off: nvidia-smi fails, both probes returned no card.
        assert!(matches!(auto_tune_action(&tuned_for, &Settings::default(), &input(cpu(), &[], &[])), AutoTuneAction::Suspect));
        assert!(auto_tune_plan(&tuned_for, &Settings::default(), &input(cpu(), &[], &[])).is_none());
        // Or DXGI returned integrated graphics of a lower tier.
        let igpu = build_vendor_info(GpuVendor::Intel, "Intel(R) UHD Graphics 630".into(), 512);
        assert!(matches!(auto_tune_action(&tuned_for, &Settings::default(), &input(igpu, &[], &[])), AutoTuneAction::Suspect));
        // The same fingerprint with a live card — no tuning needed at all.
        assert!(matches!(auto_tune_action(&tuned_for, &Settings::default(), &input(nvidia(16303), &[], &[])), AutoTuneAction::Skip));
        // But an upgrade from a CPU machine to a card is ordinary tuning.
        assert!(matches!(auto_tune_action(&fingerprint(&cpu()), &Settings::default(), &input(nvidia(16303), &[], &[])), AutoTuneAction::Apply(_)));
    }

    /// V-58: manual 200 tokens survive the pair "false boot → successful boot":
    /// the false one writes no fingerprint, the successful one sees the old one and skips tuning.
    #[test]
    fn test_manual_values_survive_false_detection_flip_flop() {
        let s = Settings { max_tokens: 200, ..Settings::default() };
        let tuned_for = fingerprint(&nvidia(16303));
        assert!(matches!(auto_tune_action(&tuned_for, &s, &input(cpu(), &[], &[])), AutoTuneAction::Suspect));
        assert!(matches!(auto_tune_action(&tuned_for, &s, &input(nvidia(16303), &[], &[])), AutoTuneAction::Skip));
        assert_eq!(s.max_tokens, 200);
    }

    /// The fingerprint is set only when the plan has settled and there are downloaded models:
    /// otherwise tuning must come back when there is something to choose from.
    #[test]
    fn test_auto_tune_stamp_waits_for_settled_plan_and_models() {
        let empty = input(nvidia(16303), &[], &[]);
        let AutoTuneAction::Apply(tuned) = auto_tune_action("", &Settings::default(), &empty) else {
            panic!("первая настройка под железо подбирается");
        };
        assert!(!tuned.stamp, "без скачанных моделей отпечаток не ставим");

        let with_models = input(nvidia(16303), &["huihui_ai/qwen3.5-abliterated:9b"], &[]);
        let AutoTuneAction::Apply(tuned) = auto_tune_action("", &Settings::default(), &with_models) else {
            panic!("первая настройка под железо подбирается");
        };
        assert!(tuned.offered.is_empty(), "дефолты ничего не понижают: {:?}", tuned.offered);
        assert!(tuned.stamp);
    }
}

