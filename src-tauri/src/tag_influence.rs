use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Tag category by degree of influence on similarity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagCategory {
    Universal,
    Generation,
    Personality,
    CharacterSpecific,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TagInfluenceConfig {
    pub enabled: bool,
    pub weights: TagWeights,
    pub top_n: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagWeights {
    pub universal: f64,
    pub generation: f64,
    pub personality: f64,
    pub character_specific: f64,
}

impl Default for TagInfluenceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            weights: TagWeights {
                universal: 0.05,
                generation: 0.15,
                personality: 0.25,
                character_specific: 0.35,
            },
            top_n: 5,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TagWithWeight {
    pub name: String,
    pub category: TagCategory,
    pub weight: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SimilarCharacter {
    pub avatar: String,
    pub name: String,
    pub similarity: f64,
    pub shared_tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InfluenceReport {
    pub avatar: String,
    pub name: String,
    pub tags: Vec<TagWithWeight>,
    pub similar: Vec<SimilarCharacter>,
}

pub struct SimilarityCache {
    pub matrix: HashMap<String, Vec<SimilarCharacter>>,
}

// --- Classification ---

const UNIVERSAL: &[&str] = &["hololive", "vtuber", "uncensored", "russian", "anime"];

const GENERATION: &[&str] = &[
    "gen-0", "gen-1", "gen-2", "gen-3", "gen-4", "gen-5", "gen-6",
    "en", "id", "myth", "council", "promise", "advent", "justice",
    "dev-is", "gamers", "holox",
];

/// Generation folders in the library are called "Hololive EN -Myth-", "Hololive ID
/// 2期生", etc. — this is the same category as short English labels.
const GENERATION_FOLDER_PREFIX: &str = "hololive ";

const PERSONALITY: &[&str] = &[
    "tsundere", "gentle", "energetic", "crybaby", "calm", "mischievous",
    "kind", "clumsy", "bratty", "chaotic", "gremlin", "sleepy", "tomboy",
    "confident", "seductive", "flirtatious", "competitive", "narcissist",
    "perfectionist", "affectionate", "clingy", "teasing", "yandere",
    "sensitive", "motherly", "forgetful", "giggly", "jokester", "seiso",
    "stoic", "leader", "elegant", "cheerful", "laid-back", "good-talker",
    "promoter", "attention-seeking", "androgynous", "cool-facade", "otaku",
    "bookworm", "storyteller", "archiver", "goth", "idol", "singer",
    "artist", "programmer", "drinker", "museum", "romance", "sarcasm",
    "short", "tall", "muscular", "deep-voice", "social", "meme-queen",
];

/// Character traits of the set shipped with the app: the library is tagged
/// in Russian (`default_tags.json`), and without this dictionary each trait would be
/// considered unique to a character — similar characters would not be found at all.
const PERSONALITY_RU: &[&str] = &[
    "озорная", "заботливая", "энергичная", "ласковая", "спокойная", "весёлая",
    "дерзкая", "цундере", "яндере", "добрая", "смешливая", "привязчивая",
    "хаотичная", "дразнящая", "неуклюжая", "соблазнительница", "соревновательная",
    "вежливая", "застенчивая", "невозмутимая", "болтушка", "трудолюбивая",
    "элегантная", "гордая", "доминантная", "саркастичная", "ленивая", "плакса",
    "загадочная", "мемная", "общительная", "ранимая", "мечтательница", "пацанка",
    "перфекционистка", "вспыльчивая", "строгая", "серьёзная", "романтичная",
    "рассеянная", "беззаботная", "тихая", "капризная", "сонная", "мудрая",
    "уверенная", "откровенная", "своенравная", "гиперактивная", "сорванец",
];

/// Tag category by name alone — weights don't affect it.
pub fn tag_category(name: &str) -> TagCategory {
    let lower = name.to_lowercase();
    if UNIVERSAL.iter().any(|u| lower == *u) {
        return TagCategory::Universal;
    }
    if GENERATION.iter().any(|g| lower == *g) || lower.starts_with(GENERATION_FOLDER_PREFIX) {
        return TagCategory::Generation;
    }
    if PERSONALITY.iter().any(|p| lower == *p) || PERSONALITY_RU.iter().any(|p| lower == *p) {
        return TagCategory::Personality;
    }
    TagCategory::CharacterSpecific
}

/// A brand ("hololive", "vtuber", "russian", "uncensored", "anime") and generation
/// folders ("Hololive EN -Advent-") are not character traits. The same
/// filter limits tag boosting in the evolution writer
/// (`character_evolution::apply_exchange`): brand and generation are identity,
/// not behavior.
pub fn is_character_trait(name: &str) -> bool {
    matches!(
        tag_category(name),
        TagCategory::Personality | TagCategory::CharacterSpecific
    )
}

pub fn classify_tag(name: &str, w: &TagWeights) -> (TagCategory, f64) {
    let category = tag_category(name);
    let weight = match category {
        TagCategory::Universal => w.universal,
        TagCategory::Generation => w.generation,
        TagCategory::Personality => w.personality,
        TagCategory::CharacterSpecific => w.character_specific,
    };
    (category, weight)
}

/// Weighted Jaccard Similarity: Σweight(A∩B) / Σweight(A∪B).
pub fn compute_similarity(
    tags_a: &[String],
    tags_b: &[String],
    weights: &TagWeights,
) -> (f64, Vec<String>) {
    let lower_a: Vec<String> = tags_a.iter().map(|t| t.to_lowercase()).collect();
    let lower_b: Vec<String> = tags_b.iter().map(|t| t.to_lowercase()).collect();

    let mut shared_weight = 0.0;
    let mut shared_tags = Vec::new();

    for (i, tag_a) in lower_a.iter().enumerate() {
        if lower_b.iter().any(|b| b == tag_a) {
            shared_weight += classify_tag(tag_a, weights).1;
            shared_tags.push(tags_a[i].clone());
        }
    }

    let mut all: Vec<&str> = lower_a.iter().map(|s| s.as_str()).collect();
    for tag_b in &lower_b {
        if !all.iter().any(|a| *a == tag_b.as_str()) {
            all.push(tag_b);
        }
    }
    let union_weight: f64 = all.iter().map(|t| classify_tag(t, weights).1).sum();

    let sim = if union_weight > 0.0 { shared_weight / union_weight } else { 0.0 };
    (sim, shared_tags)
}

/// Precompute the similarity matrix for all characters. O(n²) CPU work on the
/// caller's thread: async handlers must run it (and `get_report`) via
/// `spawn_blocking` (V-17).
pub fn compute_all(
    characters: &[(String, String, Vec<String>)],
    weights: &TagWeights,
    top_n: usize,
) -> SimilarityCache {
    let mut matrix = HashMap::new();
    for (i, (avatar_a, _name_a, tags_a)) in characters.iter().enumerate() {
        let mut sims: Vec<SimilarCharacter> = characters
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .filter_map(|(_, (avatar_b, name_b, tags_b))| {
                let (sim, shared) = compute_similarity(tags_a, tags_b, weights);
                if sim > 0.01 {
                    Some(SimilarCharacter {
                        avatar: avatar_b.clone(),
                        name: name_b.clone(),
                        similarity: (sim * 100.0).round() / 100.0,
                        shared_tags: shared,
                    })
                } else {
                    None
                }
            })
            .collect();

        sims.sort_by(|a, b| b.similarity.partial_cmp(&a.similarity).unwrap_or(std::cmp::Ordering::Equal));
        sims.truncate(top_n);
        matrix.insert(avatar_a.clone(), sims);
    }
    SimilarityCache { matrix }
}

pub fn get_report(
    avatar: &str,
    name: &str,
    tags: &[String],
    cache: &SimilarityCache,
    config: &TagInfluenceConfig,
) -> InfluenceReport {
    InfluenceReport {
        avatar: avatar.to_string(),
        name: name.to_string(),
        tags: tags
            .iter()
            .map(|t| {
                let (cat, w) = classify_tag(t, &config.weights);
                TagWithWeight { name: t.clone(), category: cat, weight: w }
            })
            .collect(),
        similar: cache.matrix.get(avatar).cloned().unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w() -> TagWeights { TagInfluenceConfig::default().weights }

    #[test]
    fn classify_works() {
        assert_eq!(classify_tag("hololive", &w()).0, TagCategory::Universal);
        assert_eq!(classify_tag("gen-0", &w()).0, TagCategory::Generation);
        assert_eq!(classify_tag("tsundere", &w()).0, TagCategory::Personality);
        assert_eq!(classify_tag("kitsune", &w()).0, TagCategory::CharacterSpecific);
        // character-specific tags are NOT personality
        assert_eq!(classify_tag("miko-shrine", &w()).0, TagCategory::CharacterSpecific);
        assert_eq!(classify_tag("gura-shark", &w()).0, TagCategory::CharacterSpecific);
    }

    #[test]
    fn identical_tags() {
        let tags = vec!["hololive".into(), "tsundere".into()];
        let (sim, _) = compute_similarity(&tags, &tags, &w());
        assert!((sim - 1.0).abs() < 0.001);
    }

    #[test]
    fn no_overlap() {
        let a = vec!["hololive".into()];
        let b = vec!["vtuber".into()];
        let (sim, shared) = compute_similarity(&a, &b, &w());
        assert!(sim < 0.1);
        assert!(shared.is_empty());
    }

    #[test]
    fn partial_overlap() {
        let a = vec!["hololive".into(), "vtuber".into(), "gen-0".into(), "tsundere".into()];
        let b = vec!["hololive".into(), "vtuber".into(), "gen-0".into(), "gentle".into()];
        let (sim, shared) = compute_similarity(&a, &b, &w());
        assert!(sim > 0.3, "expected > 0.3, got {sim}");
        assert!(shared.contains(&"hololive".to_string()));
        assert!(shared.contains(&"gen-0".to_string()));
    }

    #[test]
    fn top_n_ordering() {
        let chars = vec![
            ("A.png".into(), "A".into(), vec!["hololive".into(), "tsundere".into()]),
            ("B.png".into(), "B".into(), vec!["hololive".into(), "gentle".into()]),
            ("C.png".into(), "C".into(), vec!["hololive".into(), "tsundere".into()]),
        ];
        let cache = compute_all(&chars, &w(), 2);
        let top = cache.matrix.get("A.png").unwrap();
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].avatar, "C.png"); // C is closer to A (both tsundere)
    }

    #[test]
    fn report_structure() {
        let chars = vec![
            ("A.png".into(), "A".into(), vec!["hololive".into(), "tsundere".into()]),
            ("B.png".into(), "B".into(), vec!["hololive".into()]),
        ];
        let cache = compute_all(&chars, &w(), 5);
        let config = TagInfluenceConfig::default();
        let r = get_report("A.png", "A", &["hololive".into(), "tsundere".into()], &cache, &config);
        assert_eq!(r.tags.len(), 2);
        assert!(!r.similar.is_empty());
    }

    /// The set markup and the category dictionary must agree: while the dictionary was
    /// English-only, every Russian trait was considered unique to
    /// a character, and "similar characters" turned into a random list.
    #[test]
    fn bundled_characters_are_classified_by_category() {
        // The set of 63 cards — characters/tags.json; in the built-in DEFAULT_TAGS_JSON only
        // two default_*, and the check on it never reached the assert.
        let pack = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../characters/tags.json");
        let tags: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(pack).unwrap()).unwrap();
        let names: std::collections::HashMap<&str, &str> = tags["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| (t["id"].as_str().unwrap(), t["name"].as_str().unwrap()))
            .collect();
        let weights = w();
        let mut checked = 0;

        for (avatar, ids) in tags["tag_map"].as_object().unwrap() {
            let mut generation = 0;
            let mut personality = 0;
            for id in ids.as_array().unwrap() {
                let name = names[id.as_str().unwrap()];
                match classify_tag(name, &weights).0 {
                    TagCategory::Generation => generation += 1,
                    TagCategory::Personality => personality += 1,
                    _ => {}
                }
            }
            if avatar.starts_with("default_") {
                continue;
            }
            assert_eq!(generation, 1, "{avatar}: ровно одна папка поколения");
            assert!(personality >= 2, "{avatar}: черт характера {personality}, нужно 2+");
            checked += 1;
        }
        assert!(checked > 0, "в наборе нет ни одной карточки для проверки");
    }
}
