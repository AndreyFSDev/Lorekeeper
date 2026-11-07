//! Character stat system: Endurance, Strength, Dexterity,
//! Intelligence, Charisma, Perception. Each is level 1–10 with experience.
//!
//! They level up from use: the model marks which stats
//! were used in a reply and awards experience.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::paths;

/// Stats of a single character.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CharacterStats {
    pub stats: BTreeMap<String, StatEntry>,
}

/// One stat: level and experience.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatEntry {
    #[serde(default = "default_level")]
    pub level: u8,
    #[serde(default)]
    pub experience: u32,
}

fn default_level() -> u8 { 1 }

impl Default for StatEntry {
    fn default() -> Self {
        Self { level: 1, experience: 0 }
    }
}

/// All characters: avatar → stats.
pub type StatsStore = BTreeMap<String, CharacterStats>;

/// Default stats.
pub const DEFAULT_STATS: &[&str] = &[
    "Endurance",
    "Strength",
    "Dexterity",
    "Intelligence",
    "Charisma",
    "Perception",
];

/// Experience for the next level: level² * 50 (a quadratic curve).
/// Early levels are fast, later ones are substantially more expensive.
/// level 0 from corrupted data counts as 1, otherwise needed=0 gives a free level-up.
pub fn exp_to_next(level: u8) -> u32 {
    let l = level.max(1) as u32;
    l * l * 50
}

/// Award experience and check for a level-up (including a multiple one).
/// Returns (leveled_up, new_level).
pub fn add_experience(entry: &mut StatEntry, amount: u32) -> (bool, u8) {
    if entry.level == 0 {
        entry.level = 1;
    }
    if entry.level >= 10 {
        return (false, 10);
    }
    // saturating_add: models can return unexpectedly large values
    entry.experience = entry.experience.saturating_add(amount);
    let mut leveled = false;
    while entry.level < 10 {
        let needed = exp_to_next(entry.level);
        if entry.experience >= needed {
            entry.experience -= needed;
            entry.level += 1;
            leveled = true;
        } else {
            break;
        }
    }
    (leveled, entry.level)
}

/// Initialize a character with the default stats (all at 1).
pub fn init_character(store: &mut StatsStore, avatar: &str) {
    if avatar.trim().is_empty() || store.contains_key(avatar) {
        return;
    }
    let stats = CharacterStats {
        stats: DEFAULT_STATS
            .iter()
            .map(|&name| (name.to_string(), StatEntry::default()))
            .collect(),
    };
    store.insert(avatar.to_string(), stats);
}

/// Distribute experience across stats: the weights are stat names with
/// a relative weight (empty set or only unknown names → split evenly
/// across the defaults). Used by quest and combat rewards (`quest_xp_prompt` /
/// `combat_xp_prompt` answer with weights through `parse_stat_deltas`). Returns
/// the experience actually awarded (rounded up per stat).
pub fn award_experience(
    store: &mut StatsStore,
    avatar: &str,
    weights: &BTreeMap<String, u32>,
    total_xp: u32,
) -> u32 {
    if total_xp == 0 {
        return 0;
    }
    init_character(store, avatar);
    let Some(cs) = store.get_mut(avatar) else {
        return 0;
    };
    let named: Vec<(String, u32)> = weights
        .iter()
        .filter(|(name, weight)| **weight > 0 && DEFAULT_STATS.contains(&name.as_str()))
        .map(|(name, weight)| (name.clone(), *weight))
        .collect();
    let (targets, total_weight): (Vec<(String, u32)>, u64) = if named.is_empty() {
        (
            DEFAULT_STATS.iter().map(|&n| (n.to_string(), 1u32)).collect(),
            DEFAULT_STATS.len() as u64,
        )
    } else {
        let sum = named.iter().map(|(_, weight)| u64::from(*weight)).sum();
        (named, sum)
    };
    // Each stat gets at least 1 XP while the prize covers all of them:
    // otherwise a small reward dissolves in rounding. A prize smaller than the number of targets
    // does not raise the floor — its remainder units are handed out below, and the sum stays total_xp.
    // The division remainder is not lost either — we give it out one unit at a time to the stats
    // with the largest fractional part, so the total granted matches the promised reward
    // (100 XP over six equal weights used to yield 96).
    let mut shares: Vec<(usize, u64, u64)> = targets
        .iter()
        .enumerate()
        .map(|(index, (_, weight))| {
            let numerator = u64::from(total_xp) * u64::from(*weight);
            (
                index,
                if u64::from(total_xp) >= targets.len() as u64 {
                    (numerator / total_weight).max(1)
                } else {
                    numerator / total_weight
                },
                numerator % total_weight,
            )
        })
        .collect();
    // The floor raised to 1 on small weights could overshoot the prize (3 XP over weights 15/1/1 gave 4):
    // we take the excess from the largest shares, while the small ones keep their units
    let mut over = shares
        .iter()
        .map(|(_, share, _)| share)
        .sum::<u64>()
        .saturating_sub(u64::from(total_xp));
    while over > 0 {
        let Some(top) = shares.iter_mut().filter(|(_, share, _)| *share > 1).max_by_key(|(_, share, _)| *share) else {
            break;
        };
        top.1 -= 1;
        over -= 1;
    }
    let mut left = u64::from(total_xp)
        .saturating_sub(shares.iter().map(|(_, share, _)| share).sum::<u64>());
    let mut order: Vec<usize> = (0..shares.len()).collect();
    order.sort_by_key(|&index| std::cmp::Reverse(shares[index].2));
    for &index in &order {
        if left == 0 {
            break;
        }
        shares[index].1 += 1;
        left -= 1;
    }
    let mut granted = 0u32;
    for (index, share, _) in shares {
        let name = &targets[index].0;
        if let Some(entry) = cs.stats.get_mut(name.as_str()) {
            add_experience(entry, share as u32);
            granted = granted.saturating_add(share as u32);
        }
    }
    granted
}

fn store_path() -> PathBuf {
    paths::base_dir().join("data").join("characters_stats.json")
}

static STORE: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn load() -> StatsStore {
    crate::json_store::load(&STORE, &store_path())
}

pub fn update<T>(change: impl FnOnce(&mut StatsStore) -> T) -> Result<T, String> {
    crate::json_store::update(&STORE, &store_path(), change)
}

/// Prompt text: the character's stats.
pub fn stats_note(avatar: &str, store: &StatsStore) -> Option<String> {
    let stats = store.get(avatar)?;
    if stats.stats.is_empty() {
        return None;
    }
    let items: Vec<String> = stats
        .stats
        .iter()
        .map(|(name, entry)| format!("{}: {}", name, entry.level))
        .collect();
    Some(format!("Характеристики: {}", items.join(", ")))
}

/// Evaluation prompt: which stats were used.
/// Historically placed here, but in fact not called from the gateway —
/// `prompts::combined_evaluator_prompt` is used instead.
#[allow(dead_code)]
pub fn evaluator_prompt(character: &str, user_text: &str, reply_text: &str) -> String {
    format!(
        "Оцени, какие характеристики персонажа {character} были задействованы \
         в последнем обмене репликами.\n\n\
         Реплика пользователя: «{user}»\n\
         Ответ персонажа: «{reply}»\n\n\
         Верни ТОЛЬКО JSON без пояснений:\n\
         {{\"stats\": {{\"Strength\": N, \"Charisma\": N, ...}}}}\n\
         N — опыт (0–15): 0 если не задействована, 1–3 обычная болтовня, \
         4–8 заметное использование, 9–15 ключевой момент.\n\
         Характеристики: Endurance, Strength, Dexterity, Intelligence, Charisma, Perception.",
        character = crate::traits::display_name(character),
        user = user_text.chars().take(600).collect::<String>(),
        reply = reply_text.chars().take(600).collect::<String>(),
    )
}

/// Parsing the model's reply.
/// Historically placed here, but in fact not called from the gateway —
/// `prompts::parse_combined_evaluator` is used instead.
#[allow(dead_code)]
pub fn parse_stat_deltas(raw: &str) -> Option<BTreeMap<String, u32>> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end <= start {
        return None;
    }
    let inner = raw.get(start..=end)?;
    // The model may wrap {"stats": {...}} or return {...} directly.
    if let Ok(wrapper) = serde_json::from_str::<serde_json::Value>(inner) {
        if let Some(stats) = wrapper.get("stats").and_then(|v| v.as_object()) {
            let map: BTreeMap<String, u32> = stats
                .iter()
                .filter_map(|(k, v)| v.as_u64().map(|n| (k.clone(), n as u32)))
                .filter(|(_, n)| *n > 0)
                .collect();
            if !map.is_empty() {
                return Some(map);
            }
            return None;
        }
        // Without the wrapper — return it directly.
        if let Ok(map) = serde_json::from_str::<BTreeMap<String, u32>>(inner) {
            let filtered: BTreeMap<String, u32> =
                map.into_iter().filter(|(_, n)| *n > 0).collect();
            if !filtered.is_empty() {
                return Some(filtered);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The division remainder is not lost: 100 XP over six equal weights used to give
    /// 96 (16 each), although the reward promised 100.
    #[test]
    fn award_experience_keeps_every_point_of_the_reward() {
        let mut store = StatsStore::new();
        let weights: BTreeMap<String, u32> = DEFAULT_STATS
            .iter()
            .map(|name| ((*name).to_string(), 1u32))
            .collect();
        let granted = award_experience(&mut store, "hero.png", &weights, 100);
        assert_eq!(granted, 100);
        let sum: u32 = store["hero.png"].stats.values().map(|s| s.experience).sum();
        assert_eq!(sum, 100);
    }

    #[test]
    fn award_experience_small_prize_is_not_multiplied() {
        let mut store = StatsStore::new();
        let granted = award_experience(&mut store, "hero.png", &BTreeMap::new(), 1);
        assert_eq!(granted, 1);
        let sum: u32 = store["hero.png"].stats.values().map(|s| s.experience).sum();
        assert_eq!(sum, 1);
    }

    #[test]
    fn award_experience_skewed_weights_do_not_exceed_prize() {
        let mut store = StatsStore::new();
        let weights: BTreeMap<String, u32> =
            [("Strength", 15u32), ("Dexterity", 1), ("Charisma", 1)].map(|(k, w)| (k.to_string(), w)).into();
        let granted = award_experience(&mut store, "hero.png", &weights, 3);
        assert_eq!(granted, 3);
        let stats = &store["hero.png"].stats;
        assert!(["Strength", "Dexterity", "Charisma"].iter().all(|name| stats[*name].experience == 1));
    }

    #[test]
    fn exp_to_next_formula() {
        assert_eq!(exp_to_next(1), 50);
        assert_eq!(exp_to_next(5), 1250);
        assert_eq!(exp_to_next(10), 5000);
    }

    #[test]
    fn add_experience_levels_up() {
        // level 1, 50 experience needed
        let mut entry = StatEntry { level: 1, experience: 0 };
        let (up, lvl) = add_experience(&mut entry, 50);
        assert!(up);
        assert_eq!(lvl, 2);
        assert_eq!(entry.experience, 0);
    }

    #[test]
    fn add_experience_no_level_up() {
        // level 1, 50 needed; 25 < 50 → no level-up
        let mut entry = StatEntry { level: 1, experience: 0 };
        let (up, lvl) = add_experience(&mut entry, 25);
        assert!(!up);
        assert_eq!(lvl, 1);
        assert_eq!(entry.experience, 25);
    }

    #[test]
    fn add_experience_overflow() {
        // level 1, exp 30 +30 = 60, 50 needed → level-up, 10 left over
        let mut entry = StatEntry { level: 1, experience: 30 };
        let (up, lvl) = add_experience(&mut entry, 30);
        assert!(up);
        assert_eq!(lvl, 2);
        assert_eq!(entry.experience, 10);
    }

    #[test]
    fn level_clamped_at_10() {
        let mut entry = StatEntry { level: 10, experience: 0 };
        let (up, lvl) = add_experience(&mut entry, 1000);
        assert!(!up);
        assert_eq!(lvl, 10);
    }

    #[test]
    fn init_character_creates_defaults() {
        let mut store = StatsStore::new();
        init_character(&mut store, "test.png");
        let stats = store.get("test.png").unwrap();
        assert_eq!(stats.stats.len(), DEFAULT_STATS.len());
        for entry in stats.stats.values() {
            assert_eq!(entry.level, 1);
        }
    }

    #[test]
    fn init_character_idempotent() {
        let mut store = StatsStore::new();
        init_character(&mut store, "test.png");
        store.get_mut("test.png").unwrap().stats.get_mut("Strength").unwrap().level = 5;
        init_character(&mut store, "test.png");
        assert_eq!(store.get("test.png").unwrap().stats["Strength"].level, 5);
    }

    #[test]
    fn stats_note_format() {
        let mut store = StatsStore::new();
        init_character(&mut store, "test.png");
        let note = stats_note("test.png", &store).unwrap();
        assert!(note.contains("Endurance: 1"), "{note}");
        assert!(note.contains("Charisma: 1"), "{note}");
    }

    #[test]
    fn parse_stat_deltas_wrapper() {
        let raw = r#"{"stats": {"Strength": 5, "Charisma": 3}}"#;
        let deltas = parse_stat_deltas(raw).unwrap();
        assert_eq!(deltas["Strength"], 5);
        assert_eq!(deltas["Charisma"], 3);
    }

    #[test]
    fn parse_stat_deltas_direct() {
        let raw = r#"{"Strength": 2, "Intelligence": 7}"#;
        let deltas = parse_stat_deltas(raw).unwrap();
        assert_eq!(deltas["Strength"], 2);
    }

    #[test]
    fn parse_stat_deltas_with_markdown() {
        let raw = "Вот:\n```json\n{\"stats\": {\"Dexterity\": 4}}\n```";
        let deltas = parse_stat_deltas(raw).unwrap();
        assert_eq!(deltas["Dexterity"], 4);
    }

    #[test]
    fn exp_to_next_level_zero() {
        // level 0 from corrupted data counts as 1 (otherwise needed=0 and a free level-up)
        assert_eq!(exp_to_next(0), 50);
    }

    #[test]
    fn add_experience_level_zero_treated_as_one() {
        let mut entry = StatEntry { level: 0, experience: 0 };
        // 49 XP at level 0→1: short of 50, so no level-up
        let (up, lvl) = add_experience(&mut entry, 49);
        assert!(!up);
        assert_eq!(lvl, 1);
        assert_eq!(entry.level, 1);
        // 1 more XP — exactly 50, level-up to 2
        let (up, lvl) = add_experience(&mut entry, 1);
        assert!(up);
        assert_eq!(lvl, 2);
    }

    #[test]
    fn add_experience_amount_zero() {
        let mut entry = StatEntry { level: 3, experience: 50 };
        let (up, lvl) = add_experience(&mut entry, 0);
        assert!(!up);
        assert_eq!(lvl, 3);
        assert_eq!(entry.experience, 50);
    }

    #[test]
    fn add_experience_multiple_level_up() {
        // 500 exp at level 1: 1→2 (50), 2→3 (200), 250 left over
        let mut entry = StatEntry { level: 1, experience: 0 };
        let (up, lvl) = add_experience(&mut entry, 500);
        assert!(up);
        assert_eq!(lvl, 3);
        assert_eq!(entry.experience, 250);
    }

    #[test]
    fn stats_note_unknown_avatar() {
        let store = StatsStore::new();
        assert!(stats_note("unknown.png", &store).is_none());
    }

    #[test]
    fn parse_stat_deltas_markdown_without_stats_key() {
        // The model wrapped it in markdown but returned the object directly (without "stats")
        let raw = "```json\n{\"Strength\": 5, \"Charisma\": 2}\n```";
        let deltas = parse_stat_deltas(raw).unwrap();
        assert_eq!(deltas["Strength"], 5);
        assert_eq!(deltas["Charisma"], 2);
    }

    #[test]
    fn init_character_empty_avatar() {
        let mut store = StatsStore::new();
        init_character(&mut store, "");
        assert!(!store.contains_key(""));
        init_character(&mut store, "  ");
        assert!(!store.contains_key("  "));
    }

    #[test]
    fn add_experience_saturating() {
        // Overflow protection: experience is close to u32::MAX
        let mut entry = StatEntry { level: 1, experience: u32::MAX - 5 };
        let (up, lvl) = add_experience(&mut entry, 100);
        assert!(up);
        // saturating_add → u32::MAX, then a multiple level-up to 10
        // the sum of exp_to_next(1..=9) = 50+200+450+800+1250+1800+2450+3200+4050 = 14250
        assert_eq!(lvl, 10);
        assert_eq!(entry.experience, u32::MAX - 14250);
    }

    #[test]
    fn add_experience_level9_to10_exact() {
        // level 9, 9²*50 = 4050 experience needed for 10
        let mut entry = StatEntry { level: 9, experience: 0 };
        let (up, lvl) = add_experience(&mut entry, 4050);
        assert!(up);
        assert_eq!(lvl, 10);
        assert_eq!(entry.experience, 0);
    }

    #[test]
    fn add_experience_at_10_discards_xp() {
        // level 10 — no experience is awarded
        let mut entry = StatEntry { level: 10, experience: 0 };
        let (up, lvl) = add_experience(&mut entry, 5000);
        assert!(!up);
        assert_eq!(lvl, 10);
        assert_eq!(entry.experience, 0);
    }

    #[test]
    fn exp_to_next_sum_1_to_9() {
        // Check: sum 1..9 = 14250
        let sum: u32 = (1..=9).map(exp_to_next).sum();
        assert_eq!(sum, 14250);
    }

    #[test]
    fn parse_stat_deltas_empty_string() {
        assert!(parse_stat_deltas("").is_none());
    }

    #[test]
    fn parse_stat_deltas_no_json() {
        assert!(parse_stat_deltas("нет JSON").is_none());
    }

    #[test]
    fn parse_stat_deltas_negative_values_ignored() {
        // as_u64() skips negatives — no panic
        let raw = r#"{"stats": {"Strength": -5, "Charisma": 3}}"#;
        let deltas = parse_stat_deltas(raw).unwrap();
        assert_eq!(deltas["Charisma"], 3);
        assert!(!deltas.contains_key("Strength"));
    }

    #[test]
    fn parse_stat_deltas_all_zeros() {
        // All zeros — None (same as skills)
        assert!(parse_stat_deltas(r#"{"stats": {"Strength": 0, "Dexterity": 0}}"#).is_none());
    }

    #[test]
    fn parse_stat_deltas_mixed_zeros() {
        let raw = r#"{"stats": {"Strength": 5, "Dexterity": 0, "Charisma": 3}}"#;
        let deltas = parse_stat_deltas(raw).unwrap();
        assert_eq!(deltas.len(), 2);
        assert_eq!(deltas["Strength"], 5);
        assert_eq!(deltas["Charisma"], 3);
        assert!(!deltas.contains_key("Dexterity"));
    }

    #[test]
    fn stats_note_empty_stats() {
        // The character exists, but the stats are empty
        let mut store = StatsStore::new();
        store.insert("test.png".to_string(), CharacterStats::default());
        // BTreeMap is empty — Default for CharacterStats yields an empty map
        assert!(stats_note("test.png", &store).is_none());
    }

    #[test]
    fn parse_stat_deltas_empty_stats_object() {
        assert!(parse_stat_deltas(r#"{"stats": {}}"#).is_none());
    }

    #[test]
    fn add_experience_multiple_level_up_to_10() {
        // level 1 + exactly enough experience to reach 10
        let mut entry = StatEntry { level: 1, experience: 0 };
        let needed: u32 = (1..=9).map(exp_to_next).sum(); // 50+200+450+800+1250+1800+2450+3200+4050 = 14250
        let (up, lvl) = add_experience(&mut entry, needed);
        assert!(up);
        assert_eq!(lvl, 10);
        assert_eq!(entry.experience, 0);
    }

    #[test]
    fn parse_stat_deltas_nested_markdown_multiple_times() {
        // The model wrapped the JSON in markdown, with another wrapper inside
        let raw = "Ответ:\n```json\n{\"stats\": {\"Perception\": 12}}\n```\nКонец.";
        let deltas = parse_stat_deltas(raw).unwrap();
        assert_eq!(deltas["Perception"], 12);
    }

    #[test]
    fn parse_stat_deltas_garbage_between_braces() {
        // JSON found between { }, but invalid
        let raw = "{ это не json }";
        assert!(parse_stat_deltas(raw).is_none());
    }
}
