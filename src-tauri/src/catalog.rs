//! Ollama model catalog with hardware binding.
//!
//! Product rule: the user sees only models their machine
//! can really handle — no "future-proof" entries in the storefront. The hardware tier is computed by
//! `gpu::HardwareTier`, and each model declares a minimum tier.
//!
//! All tags were verified by manifest requests to the Ollama registry (sizes — model
//! layers), gated repositories do not enter the catalog.

use crate::gpu::{GpuBackend, GpuInfo, HardwareTier};

use crate::l10n;

#[derive(Debug, Clone, serde::Serialize)]
pub struct CatalogModel {
    pub tag: String,
    pub name: String,
    /// chat | embed | vision
    pub category: String,
    pub badge: String,
    pub vram: String,
    pub size: String,
    pub quant: String,
    pub description: String,
    pub roleplay_style: String,
    /// The minimum hardware tier on which the model makes sense.
    pub min_tier: HardwareTier,
    /// Set not in the catalog but when selecting for a specific machine.
    pub recommended: bool,
}

fn model(
    tag: &str,
    name: &str,
    category: &str,
    // Text fields are bitext: a Russian reference and an English variant; in
    // the interface language `l10n::t` picks one when building the card.
    badge: (&str, &str),
    vram: (&str, &str),
    size: (&str, &str),
    quant: &str,
    min_tier: HardwareTier,
    description: (&str, &str),
    roleplay_style: (&str, &str),
) -> CatalogModel {
    CatalogModel {
        tag: tag.into(),
        name: name.into(),
        category: category.into(),
        badge: l10n::t(badge.0, badge.1),
        vram: l10n::t(vram.0, vram.1),
        size: l10n::t(size.0, size.1),
        quant: quant.into(),
        description: l10n::t(description.0, description.1),
        roleplay_style: l10n::t(roleplay_style.0, roleplay_style.1),
        min_tier,
        recommended: false,
    }
}

/// The full catalog regardless of hardware (for tests and the "show everything" mode).
/// The row is qwen3.5-abliterated (multimodal: one model both reads images
/// and holds a dialogue), measurements 12.10.2026: 9b vs 14b-v2 of the previous row —
/// 108 vs 75 tok/s and livelier prose with the same character.
pub fn catalog() -> Vec<CatalogModel> {
    vec![
        // ── Chat: uncensored, from weak machines to strong ones ──────────────────────
        model(
            "huihui_ai/qwen3.5-abliterated:0.8B",
            "Qwen 3.5 Abliterated (0.8B)",
            "chat",
            ("Для самых слабых машин · без цензуры", "For the weakest machines · uncensored"),
            ("~2 ГБ VRAM", "~2 GB VRAM"),
            ("~1.0 ГБ", "~1.0 GB"),
            "Q4_K",
            HardwareTier::Minimal,
            (
                "Крошечная модель без цензуры нового поколения: запускается даже на процессоре \
                 и старых картах. Отвечает мгновенно, но в длинных сценах уступает крупным \
                 сёстрам в связности.",
                "A tiny uncensored model of the new generation: runs even on a CPU and old \
                 cards. Replies instantly, but in long scenes it trails the bigger sisters \
                 in coherence.",
            ),
            (
                "Короткие живые реплики, простые диалоги, черновые сцены.",
                "Short lively replies, simple dialogues, draft scenes.",
            ),
        ),
        model(
            "huihui_ai/qwen3.5-abliterated:2B",
            "Qwen 3.5 Abliterated (2B)",
            "chat",
            ("Без цензуры · 2–3 ГБ VRAM или CPU", "Uncensored · 2–3 GB VRAM or CPU"),
            ("~2.5 ГБ VRAM", "~2.5 GB VRAM"),
            ("~1.9 ГБ", "~1.9 GB"),
            "Q4_K",
            HardwareTier::Minimal,
            (
                "Лёгкая и уже умная: осмысленный русский текст, следование характеру и \
                 терпимая скорость даже на процессоре. Хороший минимум для ролевого \
                 диалога на слабой машине.",
                "Light yet already smart: sensible Russian text, character adherence and \
                 tolerable speed even on a CPU. A good minimum for roleplay on a weak \
                 machine.",
            ),
            (
                "Ровный ролевой диалог, короткие описания, стабильное удержание персонажа.",
                "Steady roleplay dialogue, short descriptions, stable character retention.",
            ),
        ),
        model(
            "huihui_ai/qwen3.5-abliterated:4B",
            "Qwen 3.5 Abliterated (4B)",
            "chat",
            ("Разумный компромисс · 4–5 ГБ VRAM", "A reasonable compromise · 4–5 GB VRAM"),
            ("~4 ГБ VRAM", "~4 GB VRAM"),
            ("~3.3 ГБ", "~3.3 GB"),
            "Q4_K",
            HardwareTier::Minimal,
            (
                "Золотая середина нового поколения: богатая лексика, точное следование \
                 тегам персонажа и уверенный русский слог — заметный шаг вверх против \
                 прежней 4B и 8B.",
                "The golden mean of the new generation: rich vocabulary, precise adherence \
                 to character tags and confident Russian prose — a noticeable step up \
                 from the former 4B and 8B.",
            ),
            (
                "Живой диалог средней длины, характер и манера речи держатся крепко.",
                "Lively medium-length dialogue; character and manner of speech hold firmly.",
            ),
        ),
        model(
            "huihui_ai/qwen3.5-abliterated:9b",
            "Qwen 3.5 Abliterated (9b)",
            "chat",
            ("Флагман без цензуры · 11 ГБ+ VRAM", "Uncensored flagship · 11 GB+ VRAM"),
            ("~7 ГБ VRAM", "~7 GB VRAM"),
            ("~6.6 ГБ", "~6.6 GB"),
            "Q4_K",
            HardwareTier::Mid,
            (
                "Флагман каталога: в замере 12.10.2026 обогнал прежнюю 14B и по скорости \
                 (108 против 75 ток/с), и по живости слога. Образные описания, сухой юмор \
                 по запросу, точное следование сложным характерам и длинным сценам.",
                "The catalog's flagship: in the 12 Oct 2026 benchmark it beat the former \
                 14B in both speed (108 vs 75 tok/s) and liveliness of prose. Vivid \
                 descriptions, dry humor on request, precise adherence to complex \
                 characters and long scenes.",
            ),
            (
                "Глубокий отыгрыш, образный слог, длинные сцены без распада.",
                "Deep roleplay, vivid prose, long scenes without falling apart.",
            ),
        ),
        // ── Memory (embeddings): computed on CPU too ───────────────────
        model(
            "nomic-embed-text:latest",
            "Nomic Embed Text v1.5",
            "embed",
            (
                "Лёгкая память · считается даже на CPU",
                "Lightweight memory · runs even on a CPU",
            ),
            ("~0.2 ГБ (или CPU)", "~0.2 GB (or CPU)"),
            ("~0.3 ГБ", "~0.3 GB"),
            "F16",
            HardwareTier::Minimal,
            (
                "Компактная и молниеносная модель эмбеддингов. Минимальное потребление \
                 ресурсов, быстрый расчёт сходства даже на слабом процессоре — верный \
                 выбор для машин без ускорения.",
                "A compact, lightning-fast embedding model. Minimal resource usage, quick \
                 similarity computation even on a weak CPU — the right choice for \
                 machines without acceleration.",
            ),
            (
                "Быстрый легковесный поиск ключевых воспоминаний.",
                "Fast, lightweight retrieval of key memories.",
            ),
        ),
        model(
            "bge-m3:latest",
            "BAAI BGE-M3",
            "embed",
            (
                "Золотой стандарт мультиязычной памяти",
                "The gold standard of multilingual memory",
            ),
            ("~0.3 ГБ (или CPU)", "~0.3 GB (or CPU)"),
            ("~1.2 ГБ", "~1.2 GB"),
            "F16",
            HardwareTier::Minimal,
            (
                "Лучшая многоязычная модель векторных представлений с поддержкой длинного \
                 контекста до 8192 токенов. Непревзойдённая точность поиска прошлых \
                 воспоминаний, фактов диалога и лора персонажей на русском языке.",
                "The best multilingual embedding model with long-context support up to \
                 8192 tokens. Unmatched accuracy at retrieving past memories, dialogue \
                 facts and character lore — Russian included.",
            ),
            (
                "Высочайшая точность извлечения контекста из памяти и документов персонажа.",
                "Top accuracy at pulling context from memory and character documents.",
            ),
        ),
        // ── Vision: the same qwen3.5 models — the family is multimodal ─────────
        model(
            "huihui_ai/qwen3.5-abliterated:2B",
            "Qwen 3.5 Vision (2B)",
            "vision",
            (
                "Компактное зрение · CPU и слабые карты",
                "Compact vision · CPU and weak cards",
            ),
            ("~2.5 ГБ VRAM", "~2.5 GB VRAM"),
            ("~1.9 ГБ", "~1.9 GB"),
            "Q4_K",
            HardwareTier::Minimal,
            (
                "Самое лёгкое зрение без цензуры: распознаёт персонажа, одежду и сцену, \
                 работает даже без видеокарты. Детализация ниже, чем у старших сестёр. \
                 Модель общая с чатом: скачивается один раз.",
                "The lightest uncensored vision: recognizes the character, clothing and \
                 scene, and works even without a GPU. Less detail than the bigger \
                 sisters. The model is shared with chat: downloaded once.",
            ),
            (
                "Короткое описание кадра: кто, что делает, во что одет, где находится.",
                "A short description of the frame: who, what they are doing, what they \
                 wear, where they are.",
            ),
        ),
        model(
            "huihui_ai/qwen3.5-abliterated:4B",
            "Qwen 3.5 Vision (4B)",
            "vision",
            ("Без цензуры · 8 ГБ VRAM", "Uncensored · 8 GB VRAM"),
            ("~4 ГБ VRAM", "~4 GB VRAM"),
            ("~3.3 ГБ", "~3.3 GB"),
            "Q4_K",
            HardwareTier::Low,
            (
                "Зрение без цензуры для карт с 8 ГБ: уверенно читает внешность, позы, \
                 одежду и фон, не отказывается от откровенных сцен и аниме-стилистики. \
                 Модель общая с чатом: скачивается один раз.",
                "Uncensored vision for 8 GB cards: confidently reads appearance, poses, \
                 clothing and background, and does not shy away from explicit scenes \
                 and anime styling. The model is shared with chat: downloaded once.",
            ),
            (
                "Подробное описание персонажа и сцены, включая эротику и обнажённость.",
                "Detailed descriptions of characters and scenes, including erotica and \
                 nudity.",
            ),
        ),
        model(
            "huihui_ai/qwen3.5-abliterated:9b",
            "Qwen 3.5 Vision (9b)",
            "vision",
            ("Точное зрение · 11 ГБ+ VRAM", "Precise vision · 11 GB+ VRAM"),
            ("~7 ГБ VRAM", "~7 GB VRAM"),
            ("~6.6 ГБ", "~6.6 GB"),
            "Q4_K",
            HardwareTier::Mid,
            (
                "Самое сильное зрение в каталоге: в замере 12.10.2026 точно прочитал \
                 персонажа, костюм и фон. Композиция, стиль и мелкие детали — без \
                 отказов и оговорок. Модель общая с чатом: скачивается один раз.",
                "The strongest vision in the catalog: in the 12 Oct 2026 benchmark it \
                 read the character, outfit and background precisely. Composition, \
                 style and fine detail — no refusals or disclaimers. The model is \
                 shared with chat: downloaded once.",
            ),
            (
                "Точные визуальные описания сцен и персонажей, включая эротику и обнажённость.",
                "Precise visual descriptions of scenes and characters, including erotica \
                 and nudity.",
            ),
        ),
    ]
}

/// Model update rule: an old tag of the previous row → a tag of the new one.
/// The role category is needed so the suggestion lands in the same storefront tab:
/// in the qwen3.5 row chat and vision share tags.
#[derive(Debug, Clone, Copy)]
pub struct UpgradeRule {
    pub from: &'static str,
    pub to: &'static str,
    /// chat | vision.
    pub category: &'static str,
}

/// Model update map. It is used by the "Update and
/// delete the old one" suggestion flow (`lib.rs`); settings are not rewritten silently — the choice
/// remains the user's.
pub const MODEL_UPGRADES: &[UpgradeRule] = &[
    // Chat of the previous qwen3 row → qwen3.5.
    UpgradeRule { from: "huihui_ai/qwen3-abliterated:0.6b-v2", to: "huihui_ai/qwen3.5-abliterated:0.8B", category: "chat" },
    UpgradeRule { from: "huihui_ai/qwen3-abliterated:1.7b", to: "huihui_ai/qwen3.5-abliterated:2B", category: "chat" },
    UpgradeRule { from: "huihui_ai/qwen3-abliterated:4b", to: "huihui_ai/qwen3.5-abliterated:4B", category: "chat" },
    UpgradeRule { from: "huihui_ai/qwen3-abliterated:8b", to: "huihui_ai/qwen3.5-abliterated:9b", category: "chat" },
    UpgradeRule { from: "huihui_ai/qwen3-abliterated:8b-v2", to: "huihui_ai/qwen3.5-abliterated:9b", category: "chat" },
    UpgradeRule { from: "huihui_ai/qwen3-abliterated:14b", to: "huihui_ai/qwen3.5-abliterated:9b", category: "chat" },
    UpgradeRule { from: "huihui_ai/qwen3-abliterated:14b-v2", to: "huihui_ai/qwen3.5-abliterated:9b", category: "chat" },
    UpgradeRule { from: "huihui_ai/qwen3-abliterated-nothink:14b", to: "huihui_ai/qwen3.5-abliterated:9b", category: "chat" },
    // Vision of previous multimodal rows → qwen3.5.
    UpgradeRule {
        from: "huihui_ai/qwen3-vl-abliterated:2b-instruct-q4_K_M",
        to: "huihui_ai/qwen3.5-abliterated:2B",
        category: "vision",
    },
    UpgradeRule {
        from: "huihui_ai/qwen3-vl-abliterated:4b-instruct-q4_K_M",
        to: "huihui_ai/qwen3.5-abliterated:4B",
        category: "vision",
    },
    UpgradeRule {
        from: "huihui_ai/qwen3-vl-abliterated:8b-instruct-q4_K_M",
        to: "huihui_ai/qwen3.5-abliterated:9b",
        category: "vision",
    },
    UpgradeRule {
        from: "huihui_ai/qwen2.5-vl-abliterated:7b",
        to: "huihui_ai/qwen3.5-abliterated:4B",
        category: "vision",
    },
];

/// Model update suggestion: a pair old → new and the role category
/// in which the old model lived in the catalog.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UpgradeOffer {
    pub from: String,
    pub to: String,
    /// chat | vision.
    pub category: String,
    /// The new model is already on disk (downloaded manually or by another role):
    /// the update reduces to switching roles and deleting the old one.
    pub to_installed: bool,
}

/// Which installed models should be updated: the old tag is installed. We show it
/// even when the new one is already downloaded — otherwise the old model left in a role would never
/// get a suggestion to switch and free up space.
pub fn upgrade_offers(installed: &[String]) -> Vec<UpgradeOffer> {
    let has = |tag: &str| installed.iter().any(|m| m == tag);
    MODEL_UPGRADES
        .iter()
        .filter(|rule| has(rule.from))
        .map(|rule| UpgradeOffer {
            from: rule.from.into(),
            to: rule.to.into(),
            category: rule.category.into(),
            to_installed: has(rule.to),
        })
        .collect()
}

/// Storefront ceiling: what the machine really computes on.
/// CUDA and Vulkan (AMD/Intel) accelerate models in Ollama, so the ceiling is the tier
/// by VRAM; a pure CPU gets the minimal set.
pub fn chat_ceiling(hardware: &GpuInfo) -> HardwareTier {
    match hardware.backend {
        GpuBackend::Cuda | GpuBackend::Vulkan => hardware.tier,
        GpuBackend::Cpu => HardwareTier::Minimal,
    }
}

/// Image generation: CUDA only — ComfyUI supports no other acceleration,
/// and compromises (DirectML, ZLUDA, ROCm package for Windows) are either dead or
/// not included in the app (see the journal, stage decisions).
pub fn image_ceiling(hardware: &GpuInfo) -> Option<HardwareTier> {
    match hardware.backend {
        GpuBackend::Cuda => Some(hardware.tier),
        GpuBackend::Vulkan | GpuBackend::Cpu => None,
    }
}

/// Models available on this machine. The largest suitable one in each category
/// is marked recommended (for embeddings — by acceleration availability).
pub fn visible(hardware: &GpuInfo) -> Vec<CatalogModel> {
    let ceiling = chat_ceiling(hardware);
    let accelerated = matches!(hardware.backend, GpuBackend::Cuda | GpuBackend::Vulkan);

    let mut list: Vec<CatalogModel> = catalog()
        .into_iter()
        .filter(|m| m.min_tier <= ceiling)
        .collect();

    for category in ["chat", "vision"] {
        let best = list
            .iter()
            .filter(|m| m.category == category)
            .map(|m| m.min_tier)
            .max();
        if let Some(tier) = best {
            if let Some(target) = list
                .iter_mut()
                .filter(|m| m.category == category && m.min_tier == tier)
                // Lexicographic size sorting lies: "~10.0 GB" < "~9.0 GB".
                .max_by_key(|m| model_size_bytes(&m.size).unwrap_or(0))
            {
                target.recommended = true;
            }
        }
    }

    let embed_tag = if accelerated {
        "bge-m3:latest"
    } else {
        "nomic-embed-text:latest"
    };
    if let Some(embed) = list.iter_mut().find(|m| m.tag == embed_tag) {
        embed.recommended = true;
    }

    list
}

/// The memory model we offer the first-run wizard to download.
pub fn recommended_embed_model(hardware: &GpuInfo) -> String {
    visible(hardware)
        .into_iter()
        .filter(|m| m.category == "embed" && m.recommended)
        .map(|m| m.tag)
        .next()
        .unwrap_or_else(|| "nomic-embed-text:latest".to_string())
}

/// Model size from the catalog label ("~9.0 GB") in bytes. Needed by settings
/// selection: in CPU mode weights live in RAM, and a model larger
/// than half the memory must not be installed. `None` — the label was not parsed.
pub fn model_size_bytes(label: &str) -> Option<u64> {
    let cleaned = label.trim().trim_start_matches('~').replace(',', ".");
    let number: String = cleaned
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let value: f64 = number.parse().ok()?;
    let unit = cleaned[number.len()..].trim().to_lowercase();
    let multiplier = if unit.starts_with("гб") || unit.starts_with("gb") {
        1024.0 * 1024.0 * 1024.0
    } else if unit.starts_with("мб") || unit.starts_with("mb") {
        1024.0 * 1024.0
    } else {
        return None;
    };
    Some((value * multiplier) as u64)
}

/// The chat model we offer the first-run wizard to download.
pub fn recommended_chat_model(hardware: &GpuInfo) -> String {
    visible(hardware)
        .into_iter()
        .filter(|m| m.category == "chat" && m.recommended)
        .map(|m| m.tag)
        .next()
        .unwrap_or_else(|| "huihui_ai/qwen3.5-abliterated:4B".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::{build_vendor_info, GpuVendor};

    fn nvidia(vram_mb: u64) -> GpuInfo {
        build_vendor_info(GpuVendor::Nvidia, "NVIDIA GeForce RTX".into(), vram_mb)
    }

    #[test]
    fn test_model_size_label_is_parsed() {
        assert_eq!(model_size_bytes("~9.0 ГБ"), Some(9_663_676_416));
        assert_eq!(model_size_bytes("1.2 ГБ"), Some(1_288_490_188));
        assert_eq!(model_size_bytes("512 МБ"), Some(536_870_912));
        assert_eq!(model_size_bytes("~0,3 ГБ"), Some(322_122_547));
        assert_eq!(model_size_bytes("без размера"), None);
    }

    /// A numeric sort key, not lexicographic: "~10.0 GB" must
    /// be recognized as larger than "~9.0 GB".
    #[test]
    fn test_largest_model_picked_by_bytes_not_string() {
        assert!(model_size_bytes("~10.0 ГБ") > model_size_bytes("~9.0 ГБ"));
        assert!(model_size_bytes("~5.0 ГБ") > model_size_bytes("~4.0 ГБ"));
        // String comparison would give the reverse order.
        assert!("~10.0 ГБ" < "~9.0 ГБ");
    }

    #[test]
    fn test_all_tags_verified_against_registry() {
        // Catalog tags and sizes were verified by manifest requests to the Ollama registry;
        // the test guards against typos during edits. Row 12.10.2026 — qwen3.5.
        let tags: Vec<String> = catalog().into_iter().map(|m| m.tag).collect();
        assert!(tags.contains(&"huihui_ai/qwen3.5-abliterated:4B".to_string()));
        assert!(tags.contains(&"huihui_ai/qwen3.5-abliterated:9b".to_string()));
        assert!(!tags.iter().any(|t| t.starts_with("huihui_ai/qwen3-abliterated")
            || t.starts_with("huihui_ai/qwen3-vl")
            || t.starts_with("huihui_ai/qwen2.5-vl")));
    }

    /// The update map leads only to catalog tags: the "update" suggestion
    /// must not substitute a model that is not in the storefront.
    #[test]
    fn test_upgrade_targets_are_in_catalog() {
        let tags: Vec<String> = catalog().into_iter().map(|m| m.tag).collect();
        for rule in MODEL_UPGRADES {
            assert!(!tags.contains(&rule.from.to_string()), "{} уже не в каталоге", rule.from);
            assert!(tags.contains(&rule.to.to_string()), "{} обязана быть в каталоге", rule.to);
            assert!(
                catalog().into_iter().any(|m| m.tag == rule.to && m.category == rule.category),
                "{}: категория {} не совпадает с каталогом",
                rule.to,
                rule.category
            );
        }
    }

    /// The suggestion is shown while the old model is installed — and when the new one is already
    /// downloaded (then the update reduces to switching roles).
    #[test]
    fn test_upgrade_offers_follow_installed_models() {
        let installed = vec![
            "huihui_ai/qwen3-abliterated:14b-v2".to_string(),
            "huihui_ai/qwen3-vl-abliterated:8b-instruct-q4_K_M".to_string(),
        ];
        let offers = upgrade_offers(&installed);
        assert_eq!(offers.len(), 2);
        assert!(offers.iter().any(|o| o.from.ends_with("14b-v2")
            && o.to == "huihui_ai/qwen3.5-abliterated:9b"
            && o.category == "chat"
            && !o.to_installed));
        assert!(offers.iter().any(|o| o.category == "vision" && o.to == "huihui_ai/qwen3.5-abliterated:9b"));

        // The new model is already on disk — the suggestion remains (the old one is still installed),
        // but marked as nothing to download.
        let upgraded = vec![
            "huihui_ai/qwen3-abliterated:14b-v2".to_string(),
            "huihui_ai/qwen3.5-abliterated:9b".to_string(),
        ];
        let offers = upgrade_offers(&upgraded);
        assert_eq!(offers.len(), 1);
        assert!(offers[0].to_installed);

        // The old model is missing — nothing to suggest.
        let fresh = vec!["huihui_ai/qwen3.5-abliterated:9b".to_string()];
        assert!(upgrade_offers(&fresh).is_empty());
    }

    #[test]
    fn test_high_tier_sees_everything() {
        let list = visible(&nvidia(16303));
        assert_eq!(list.len(), catalog().len());
        let chat = list.iter().find(|m| m.category == "chat" && m.recommended).unwrap();
        assert_eq!(chat.tag, "huihui_ai/qwen3.5-abliterated:9b");
        let vision = list.iter().find(|m| m.category == "vision" && m.recommended).unwrap();
        assert_eq!(vision.tag, "huihui_ai/qwen3.5-abliterated:9b");
        assert!(list.iter().any(|m| m.tag == "bge-m3:latest" && m.recommended));
    }

    #[test]
    fn test_mid_tier_gets_9b_chat_and_vision() {
        let list = visible(&nvidia(12288));
        let chat = list.iter().find(|m| m.category == "chat" && m.recommended).unwrap();
        assert_eq!(chat.tag, "huihui_ai/qwen3.5-abliterated:9b");
        let vision = list.iter().find(|m| m.category == "vision" && m.recommended).unwrap();
        assert_eq!(vision.tag, "huihui_ai/qwen3.5-abliterated:9b");
    }

    /// 8 GB cards don't get 9b: weights 6.6 GB plus KV even in q8 don't fit
    /// with context, and the storefront rule is "the machine really handles it".
    #[test]
    fn test_8gb_card_stays_below_9b() {
        let list = visible(&nvidia(8192));
        let tags: Vec<&str> = list.iter().map(|m| m.tag.as_str()).collect();
        assert!(!tags.contains(&"huihui_ai/qwen3.5-abliterated:9b"));
        assert!(tags.contains(&"huihui_ai/qwen3.5-abliterated:4B"));
        assert!(tags.contains(&"huihui_ai/qwen3.5-abliterated:2B"));
        assert_eq!(recommended_chat_model(&nvidia(8192)), "huihui_ai/qwen3.5-abliterated:4B");
    }

    #[test]
    fn test_amd_polaris_sees_low_tier_and_no_image_generation() {
        let info = build_vendor_info(GpuVendor::Amd, "Radeon RX 580 Series".into(), 8192);
        let list = visible(&info);
        let tags: Vec<&str> = list.iter().map(|m| m.tag.as_str()).collect();
        // 8 GB and a Vulkan backend: 4B fits, 9b no longer (KV without q8 is thicker).
        assert!(tags.contains(&"huihui_ai/qwen3.5-abliterated:4B"));
        assert!(!tags.contains(&"huihui_ai/qwen3.5-abliterated:9b"));
        assert_eq!(recommended_chat_model(&info), "huihui_ai/qwen3.5-abliterated:4B");
        // No images on AMD: ComfyUI requires CUDA.
        assert_eq!(image_ceiling(&info), None);
        assert!(!list.is_empty());
    }

    #[test]
    fn test_recommended_embed_model_follows_acceleration() {
        assert_eq!(recommended_embed_model(&nvidia(16303)), "bge-m3:latest");
        assert_eq!(recommended_embed_model(&nvidia(8192)), "bge-m3:latest");
        let cpu = build_vendor_info(GpuVendor::Unknown, "Microsoft Basic Display Adapter".into(), 0);
        assert_eq!(recommended_embed_model(&cpu), "nomic-embed-text:latest");
    }

    #[test]
    fn test_cpu_only_machine_has_no_image_generation() {
        let info = build_vendor_info(GpuVendor::Unknown, "Microsoft Basic Display Adapter".into(), 0);
        assert_eq!(image_ceiling(&info), None);
        let list = visible(&info);
        assert!(list.iter().all(|m| m.min_tier == HardwareTier::Minimal));
        assert!(list.iter().any(|m| m.tag == "nomic-embed-text:latest" && m.recommended));
    }
}
