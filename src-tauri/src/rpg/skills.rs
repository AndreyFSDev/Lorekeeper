//! Skill system: each character has a set of skills (1–10).
//! They grow from use (slower than stats).
//! Custom skills are bound to a stat.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::paths;

/// A character skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillEntry {
    #[serde(default = "default_level")]
    pub level: u8,
    #[serde(default)]
    pub experience: u32,
    /// Binding to a stat (for custom skills).
    #[serde(default)]
    pub stat_binding: String,
    #[serde(default)]
    pub is_custom: bool,
}

fn default_level() -> u8 { 1 }

impl Default for SkillEntry {
    fn default() -> Self {
        Self {
            level: 1,
            experience: 0,
            stat_binding: String::new(),
            is_custom: false,
        }
    }
}

/// Skills of a single character.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CharacterSkills {
    pub skills: BTreeMap<String, SkillEntry>,
}

pub type SkillsStore = BTreeMap<String, CharacterSkills>;

/// Default skills.
pub const DEFAULT_SKILLS: &[(&str, &str)] = &[
    ("Melee", "Strength"),
    ("Stealth", "Dexterity"),
    ("Persuasion", "Charisma"),
    ("Survival", "Endurance"),
    ("Crafting", "Intelligence"),
    ("Medicine", "Intelligence"),
    ("Investigation", "Perception"),
    ("Archery", "Dexterity"),
];

/// Experience for the next level: level² * 75 (quadratic, slower than stats).
/// level 0 from corrupted data counts as 1, otherwise needed=0 gives a free level-up.
pub fn exp_to_next(level: u8) -> u32 {
    let l = level.max(1) as u32;
    l * l * 75
}

/// Award experience to a skill. Returns (leveled_up, new_level).
pub fn add_experience(entry: &mut SkillEntry, amount: u32) -> (bool, u8) {
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

/// Initialize a character's default skills.
pub fn init_character(store: &mut SkillsStore, avatar: &str) {
    if avatar.trim().is_empty() {
        return;
    }
    // Fill in missing defaults even for an existing entry: a custom
    // skill added before the first evaluation created an entry without the base
    // skills, and `contains_key` left the character with a single skill forever.
    let entry = store.entry(avatar.to_string()).or_default();
    for &(name, stat) in DEFAULT_SKILLS {
        entry.skills.entry(name.to_string()).or_insert_with(|| SkillEntry {
            stat_binding: stat.to_string(),
            is_custom: false,
            ..Default::default()
        });
    }
}

/// Add a custom skill (the "What's new" promise: "You can add your own
/// skills through the director and bind them to any stat"). Clamps per the
/// contract: name 1–60, stat 1–40 characters.
pub fn add_custom_skill(
    store: &mut SkillsStore,
    avatar: &str,
    name: &str,
    stat_binding: &str,
) -> Result<(), String> {
    let name = name.trim().chars().take(60).collect::<String>();
    if name.is_empty() {
        return Err("Имя навыка не может быть пустым".to_string());
    }
    let stat_binding = canonical_stat(stat_binding);
    if stat_binding.is_empty() {
        return Err("Характеристика (stat_binding) не может быть пустой".to_string());
    }
    // Check for a duplicate before init_character: a name from the base set on a new character
    // is also a duplicate, and the refusal must not create an entry yet.
    let taken = match store.get(avatar) {
        Some(entry) => entry.skills.contains_key(&name),
        None => {
            let mut probe = SkillsStore::new();
            init_character(&mut probe, avatar);
            probe.get(avatar).is_some_and(|entry| entry.skills.contains_key(&name))
        }
    };
    if taken {
        return Err(format!("Навык «{name}» уже существует"));
    }
    // The base set comes first: otherwise the entry created by this very call stayed
    // without the default skills forever.
    init_character(store, avatar);
    let entry = store.entry(avatar.to_string()).or_default();
    entry.skills.insert(
        name,
        SkillEntry {
            stat_binding,
            is_custom: true,
            ..Default::default()
        },
    );
    Ok(())
}

fn store_path() -> PathBuf {
    paths::base_dir().join("data").join("characters_skills.json")
}

static STORE: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn load() -> SkillsStore {
    crate::json_store::load(&STORE, &store_path())
}

pub fn update<T>(change: impl FnOnce(&mut SkillsStore) -> T) -> Result<T, String> {
    crate::json_store::update(&STORE, &store_path(), change)
}

/// Like `update`, but the file is written only on `Ok`: a refusal (duplicate skill) creates no file.
pub fn update_if_ok<T, E>(
    change: impl FnOnce(&mut SkillsStore) -> Result<T, E>,
) -> Result<Result<T, E>, String> {
    crate::json_store::update_if_ok(&STORE, &store_path(), change)
}

/// Prompt text: the character's skills.
pub fn skills_note(avatar: &str, store: &SkillsStore) -> Option<String> {
    let skills = store.get(avatar)?;
    if skills.skills.is_empty() {
        return None;
    }
    let items: Vec<String> = skills
        .skills
        .iter()
        .map(|(name, entry)| format!("{}: {}", name, entry.level))
        .collect();
    Some(format!("Навыки: {}", items.join(", ")))
}

/// A character's skill level (1..10). No entry or no skill — 1, as in
/// `SkillEntry::default`: persuasion without leveling should be possible but
/// hard, not fall to zero.
pub fn level_for(avatar: &str, skill: &str) -> u8 {
    load()
        .get(avatar)
        .and_then(|cs| cs.skills.get(skill))
        .map(|s| s.level.clamp(1, 10))
        .unwrap_or(1)
}

/// "Ловкость" and "Dexterity" are the same stat. The help promises binding
/// by Russian name; combat compares `stat_binding` with the English key.
fn canonical_stat(raw: &str) -> String {
    let trimmed = raw.trim();
    match trimmed.to_lowercase().as_str() {
        "сила" | "strength" => "Strength".to_string(),
        "ловкость" | "dexterity" => "Dexterity".to_string(),
        "выносливость" | "endurance" => "Endurance".to_string(),
        "интеллект" | "intelligence" => "Intelligence".to_string(),
        "харизма" | "charisma" => "Charisma".to_string(),
        "восприятие" | "perception" => "Perception".to_string(),
        _ => trimmed.chars().take(40).collect(),
    }
}

/// Total skill bonus for a combat stat.
/// Returns the sum of levels of skills bound to target_stat.
/// Used by the combat system to scale damage.
pub fn combat_bonus(store: &SkillsStore, avatar: &str, target_stat: &str) -> u32 {
    let Some(skills) = store.get(avatar) else { return 0; };
    skills
        .skills
        .values()
        .filter(|s| s.stat_binding == target_stat)
        .map(|s| s.level as u32)
        .sum()
}

/// Evaluation prompt: which skills were used.
/// Superseded by `prompts::combined_evaluator_prompt`.
#[allow(dead_code)]
pub fn evaluator_prompt(character: &str, user_text: &str, reply_text: &str) -> String {
    format!(
        "Оцени, какие навыки персонажа {character} были задействованы \
         в последнем обмене репликами.\n\n\
         Реплика пользователя: «{user}»\n\
         Ответ персонажа: «{reply}»\n\n\
         Верни ТОЛЬКО JSON без пояснений:\n\
         {{\"skills\": {{\"Melee\": N, \"Persuasion\": N, ...}}}}\n\
         N — опыт (0–10): 0 если не задействован, 1–3 обычное использование, \
         4–7 заметное, 8–10 ключевой момент.\n\
         Навыки: Melee, Stealth, Persuasion, Survival, Crafting, Medicine.",
        character = crate::traits::display_name(character),
        user = user_text.chars().take(600).collect::<String>(),
        reply = reply_text.chars().take(600).collect::<String>(),
    )
}

/// Parsing the model's reply.
/// Superseded by `prompts::parse_combined_evaluator`.
#[allow(dead_code)]
pub fn parse_skill_deltas(raw: &str) -> Option<BTreeMap<String, u32>> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end <= start {
        return None;
    }
    let inner = raw.get(start..=end)?;
    if let Ok(wrapper) = serde_json::from_str::<serde_json::Value>(inner) {
        if let Some(skills) = wrapper.get("skills").and_then(|v| v.as_object()) {
            let map: BTreeMap<String, u32> = skills
                .iter()
                .filter_map(|(k, v)| v.as_u64().map(|n| (k.clone(), n as u32)))
                .filter(|(_, n)| *n > 0)
                .collect();
            if !map.is_empty() {
                return Some(map);
            }
            return None;
        }
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

    /// `// skill` before the first evaluation created an entry without the default skills, and
    /// `init_character` with `contains_key` never added them again.
    #[test]
    fn custom_skill_on_fresh_character_keeps_defaults() {
        let mut store = SkillsStore::default();
        add_custom_skill(&mut store, "hero.png", "Массаж", "Charisma").unwrap();
        let skills = &store["hero.png"].skills;
        for (name, _) in DEFAULT_SKILLS {
            assert!(skills.contains_key(*name), "нет дефолтного навыка {name}");
        }
        assert!(skills.contains_key("Массаж"));
    }

    #[test]
    fn exp_to_next_formula() {
        assert_eq!(exp_to_next(1), 75);
        assert_eq!(exp_to_next(5), 1875);
    }

    #[test]
    fn add_experience_levels_up() {
        // level 1, 75 needed; 50+50 = 100 → level-up, 25 left over
        let mut entry = SkillEntry { level: 1, experience: 50, ..Default::default() };
        let (up, lvl) = add_experience(&mut entry, 50);
        assert!(up);
        assert_eq!(lvl, 2);
        assert_eq!(entry.experience, 25);
    }

    #[test]
    fn level_clamped_at_10() {
        let mut entry = SkillEntry { level: 10, experience: 0, ..Default::default() };
        let (up, lvl) = add_experience(&mut entry, 1000);
        assert!(!up);
        assert_eq!(lvl, 10);
    }

    #[test]
    fn init_creates_defaults() {
        let mut store = SkillsStore::new();
        init_character(&mut store, "test.png");
        let skills = store.get("test.png").unwrap();
        assert_eq!(skills.skills.len(), 8); // was 6, now 8
        assert_eq!(skills.skills["Melee"].stat_binding, "Strength");
        assert!(!skills.skills["Melee"].is_custom);
        assert_eq!(skills.skills["Investigation"].stat_binding, "Perception");
        assert_eq!(skills.skills["Archery"].stat_binding, "Dexterity");
    }

    #[test]
    fn test_add_custom_skill_accepts_russian_stat_name() {
        let mut store = SkillsStore::new();
        super::add_custom_skill(&mut store, "test.png", "Массаж", "Ловкость").unwrap();
        assert_eq!(store["test.png"].skills["Массаж"].stat_binding, "Dexterity");
        super::add_custom_skill(&mut store, "test.png", "Удар", "сила").unwrap();
        assert_eq!(store["test.png"].skills["Удар"].stat_binding, "Strength");
    }

    #[test]
    fn test_add_custom_skill() {
        let mut store = SkillsStore::new();
        init_character(&mut store, "test.png");
        super::add_custom_skill(&mut store, "test.png", "Массаж", "Charisma").unwrap();
        let skill = &store["test.png"].skills["Массаж"];
        assert_eq!(skill.stat_binding, "Charisma");
        assert!(skill.is_custom);
    }

    #[test]
    fn test_add_custom_skill_duplicate() {
        let mut store = SkillsStore::new();
        init_character(&mut store, "test.png");
        assert!(super::add_custom_skill(&mut store, "test.png", "Melee", "Strength").is_err());
    }

    #[test]
    fn add_custom_skill_duplicate_on_new_character_leaves_store_untouched() {
        let mut store = SkillsStore::new();
        assert!(super::add_custom_skill(&mut store, "new.png", "Melee", "Strength").is_err());
        assert!(store.get("new.png").is_none());
    }

    #[test]
    fn skills_note_format() {
        let mut store = SkillsStore::new();
        init_character(&mut store, "test.png");
        let note = skills_note("test.png", &store).unwrap();
        assert!(note.contains("Melee: 1"), "{note}");
        assert!(note.contains("Persuasion: 1"), "{note}");
        assert!(note.contains("Investigation: 1"), "{note}");
        assert!(note.contains("Archery: 1"), "{note}");
    }

    #[test]
    fn test_parse_skill_deltas() {
        let raw = r#"{"skills": {"Melee": 5, "Stealth": 3}}"#;
        let deltas = super::parse_skill_deltas(raw).unwrap();
        assert_eq!(deltas["Melee"], 5);
        assert_eq!(deltas["Stealth"], 3);
    }

    #[test]
    fn add_experience_amount_zero() {
        let mut entry = SkillEntry { level: 1, experience: 50, ..Default::default() };
        let (up, lvl) = add_experience(&mut entry, 0);
        assert!(!up);
        assert_eq!(lvl, 1);
        assert_eq!(entry.experience, 50);
    }

    #[test]
    fn add_experience_multiple_level_up() {
        // level 1, 75 needed; give 500 → level 1→2 (75), 2→3 (300), 125 left over
        let mut entry = SkillEntry { level: 1, experience: 0, ..Default::default() };
        let (up, lvl) = add_experience(&mut entry, 500);
        assert!(up);
        assert_eq!(lvl, 3);
        assert_eq!(entry.experience, 125);
    }

    #[test]
    fn skills_note_unknown_avatar() {
        let store = SkillsStore::new();
        assert!(skills_note("nonexistent.png", &store).is_none());
    }

    #[test]
    fn init_character_rejects_empty_avatar() {
        let mut store = SkillsStore::new();
        init_character(&mut store, "");
        assert!(!store.contains_key(""));
        init_character(&mut store, "  ");
        assert!(!store.contains_key("  "));
    }

    #[test]
    fn add_custom_skill_rejects_empty_name() {
        let mut store = SkillsStore::new();
        init_character(&mut store, "test.png");
        assert!(super::add_custom_skill(&mut store, "test.png", "", "Strength").is_err());
        assert!(super::add_custom_skill(&mut store, "test.png", "  ", "Strength").is_err());
    }

    #[test]
    fn add_custom_skill_rejects_empty_stat_binding() {
        let mut store = SkillsStore::new();
        init_character(&mut store, "test.png");
        assert!(super::add_custom_skill(&mut store, "test.png", "Кулинария", "").is_err());
        assert!(super::add_custom_skill(&mut store, "test.png", "Кулинария", "  ").is_err());
    }

    #[test]
    fn add_custom_skill_clamps_long_strings() {
        let mut store = SkillsStore::new();
        super::add_custom_skill(&mut store, "test.png", &"и".repeat(100), &"с".repeat(80)).unwrap();
        let name = "и".repeat(60);
        let skill = &store["test.png"].skills[&name];
        assert_eq!(skill.stat_binding.chars().count(), 40);
        assert!(skill.is_custom);
    }

    #[test]
    fn parse_skill_deltas_empty_string() {
        assert!(super::parse_skill_deltas("").is_none());
    }

    #[test]
    fn parse_skill_deltas_markdown_wrapper() {
        let raw = "```json\n{\"skills\": {\"Melee\": 5, \"Stealth\": 3}}\n```";
        let deltas = super::parse_skill_deltas(raw).unwrap();
        assert_eq!(deltas["Melee"], 5);
        assert_eq!(deltas["Stealth"], 3);
    }

    #[test]
    fn parse_skill_deltas_empty_skills_object() {
        assert!(super::parse_skill_deltas(r#"{"skills": {}}"#).is_none());
    }

    #[test]
    fn parse_skill_deltas_all_zeros() {
        assert!(super::parse_skill_deltas(r#"{"skills": {"Melee": 0, "Stealth": 0}}"#).is_none());
    }

    #[test]
    fn parse_skill_deltas_mixed_zeros() {
        let raw = r#"{"skills": {"Melee": 5, "Stealth": 0, "Crafting": 3}}"#;
        let deltas = super::parse_skill_deltas(raw).unwrap();
        assert_eq!(deltas.len(), 2);
        assert_eq!(deltas["Melee"], 5);
        assert_eq!(deltas["Crafting"], 3);
        assert!(!deltas.contains_key("Stealth"));
    }

    #[test]
    fn parse_skill_deltas_flat_map_fallback() {
        let raw = r#"{"Melee": 5, "Stealth": 3}"#;
        let deltas = super::parse_skill_deltas(raw).unwrap();
        assert_eq!(deltas["Melee"], 5);
        assert_eq!(deltas["Stealth"], 3);
    }

    #[test]
    fn parse_skill_deltas_flat_map_zeros_filtered() {
        assert!(super::parse_skill_deltas(r#"{"Melee": 0}"#).is_none());
    }

    #[test]
    fn parse_skill_deltas_no_braces() {
        assert!(super::parse_skill_deltas("just text").is_none());
    }

    #[test]
    fn parse_skill_deltas_invalid_json() {
        assert!(super::parse_skill_deltas("{not valid json}").is_none());
    }

    #[test]
    fn parse_skill_deltas_closing_before_opening() {
        assert!(super::parse_skill_deltas("}text{").is_none());
    }

    #[test]
    fn add_experience_saturating() {
        // saturating_add: experience near u32::MAX must not overflow, and the
        // surplus goes into a multi-level level-up to the cap. The model test
        // is `stats::add_experience_saturating` — assert the outcome here too,
        // "does not panic" alone proved nothing (V-08).
        let mut entry = SkillEntry { level: 1, experience: u32::MAX - 10, ..Default::default() };
        let (up, lvl) = add_experience(&mut entry, 1000);
        assert!(up, "surplus experience must level up");
        // saturating_add -> u32::MAX, then level-up 1->10 consumes
        // exp_to_next(1..=9) = 75+300+675+1200+1875+2700+3675+4800+6075 = 21375.
        assert_eq!(lvl, 10);
        assert_eq!(entry.level, 10);
        assert_eq!(entry.experience, u32::MAX - 21375);
    }

    #[test]
    fn combat_bonus_basic() {
        let mut store = SkillsStore::new();
        init_character(&mut store, "test.png");
        store.get_mut("test.png").unwrap().skills.get_mut("Melee").unwrap().level = 3;
        store.get_mut("test.png").unwrap().skills.get_mut("Archery").unwrap().level = 2;
        assert_eq!(combat_bonus(&store, "test.png", "Strength"), 3); // Melee(3)
        assert_eq!(combat_bonus(&store, "test.png", "Dexterity"), 3); // Stealth(1) + Archery(2)
        assert_eq!(combat_bonus(&store, "test.png", "Perception"), 1); // Investigation(1)
    }

    #[test]
    fn combat_bonus_unknown_avatar() {
        let store = SkillsStore::new();
        assert_eq!(combat_bonus(&store, "unknown.png", "Strength"), 0);
    }

    #[test]
    fn combat_bonus_unknown_stat() {
        let mut store = SkillsStore::new();
        init_character(&mut store, "test.png");
        assert_eq!(combat_bonus(&store, "test.png", "NonExistent"), 0);
    }

    #[test]
    fn exp_to_next_sum_1_to_9() {
        let sum: u32 = (1..=9).map(exp_to_next).sum();
        // 75+300+675+1200+1875+2700+3675+4800+6075 = 21375
        assert_eq!(sum, 21375);
    }

    #[test]
    fn exp_to_next_level_zero() {
        // level 0 from corrupted data counts as 1 (otherwise needed=0 and a free level-up)
        assert_eq!(exp_to_next(0), 75);
    }

    #[test]
    fn add_experience_level_zero_treated_as_one() {
        let mut entry = SkillEntry { level: 0, experience: 0, ..Default::default() };
        let (up, lvl) = add_experience(&mut entry, 74);
        assert!(!up);
        assert_eq!(lvl, 1);
        assert_eq!(entry.level, 1);
        let (up, lvl) = add_experience(&mut entry, 1);
        assert!(up);
        assert_eq!(lvl, 2);
    }
}
