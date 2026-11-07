//! Quest system: multi-stage tasks, AI generation + manual authoring.
//! Quests progress in stages: "go there → collect that → talk to someone".

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::paths;
use crate::relationships::{RelationshipAxes, RelationshipStore};
use crate::rpg::inventory::{self, InventoryStore};
use crate::rpg::stats::{self, StatsStore};

/// Quest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quest {
    pub id: String,
    pub title: String,
    pub description: String,
    pub stages: Vec<QuestStage>,
    pub current_stage: usize,
    pub status: QuestStatus,
    pub rewards: QuestRewards,
    /// Who gave the quest: an NPC id or "director".
    pub giver: Option<String>,
    /// Reward already granted (`claim_rewards`): the flag is set as the first step
    /// of granting, so a failure of the following writes loses the reward but does not
    /// double it. Old files without the field — the reward is not granted.
    #[serde(default)]
    pub rewards_claimed: bool,
}

/// Quest stage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestStage {
    pub description: String,
    /// Objective: "talk to X", "collect Y", "go to Z".
    pub objective: String,
    pub completed: bool,
    /// Completion condition for AI evaluation.
    pub completion_condition: String,
}

/// Quest status.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum QuestStatus {
    Active,
    Completed,
    Failed,
}

/// Quest rewards.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct QuestRewards {
    pub experience: u32,
    pub stat_boosts: BTreeMap<String, u32>,
    pub items: Vec<String>,
    pub relationship_shifts: BTreeMap<String, RelationshipAxes>,
}

pub type QuestStore = BTreeMap<String, Quest>;

impl Quest {
    /// Current stage (if the quest is active).
    pub fn current_stage(&self) -> Option<&QuestStage> {
        if self.status != QuestStatus::Active {
            return None;
        }
        self.stages.get(self.current_stage)
    }

    /// Advance the quest to the next stage. Returns true if the quest is completed.
    pub fn advance(&mut self) -> bool {
        if self.status != QuestStatus::Active {
            return false;
        }
        // An empty quest has nothing to advance.
        if self.stages.is_empty() {
            return false;
        }
        // Guard against a double advance on one stage: mark the current one completed
        // and move exactly by 1.
        if let Some(stage) = self.stages.get_mut(self.current_stage) {
            if stage.completed {
                return false;
            }
            stage.completed = true;
        }
        self.current_stage = self.current_stage.saturating_add(1);
        if self.current_stage >= self.stages.len() {
            self.status = QuestStatus::Completed;
            true
        } else {
            false
        }
    }
}

/// Prompt for AI evaluation: which stats were relevant to the quest.
pub fn quest_xp_prompt(quest_title: &str, quest_description: &str) -> String {
    format!(
        "Оцени, какие характеристики персонажа были наиболее релевантны \
         для выполнения квеста «{quest_title}».\n\n\
         Описание квеста: {description}\n\n\
         Верни ТОЛЬКО JSON:\n\
         {{\"stats\": {{\"Endurance\": N, \"Strength\": N, \"Dexterity\": N, \
         \"Intelligence\": N, \"Charisma\": N, \"Perception\": N}}}}\n\
         N — процент релевантности (0–100). Сумма должна быть ~100.",
        quest_title = quest_title,
        description = quest_description.chars().take(600).collect::<String>(),
    )
}

/// Prompt for AI evaluation: which stats were used in combat.
pub fn combat_xp_prompt(enemy_name: &str, mode: &str, rounds: u32) -> String {
    format!(
        "Оцени, какие характеристики персонажа были задействованы в бою с {enemy}.\n\n\
         Режим боя: {mode}. Раундов: {rounds}.\n\n\
         Верни ТОЛЬКО JSON:\n\
         {{\"stats\": {{\"Endurance\": N, \"Strength\": N, \"Dexterity\": N, \
         \"Intelligence\": N, \"Charisma\": N, \"Perception\": N}}}}\n\
         N — процент релевантности (0–100). Сумма должна быть ~100.",
        enemy = enemy_name,
        mode = mode,
        rounds = rounds,
    )
}

fn store_path() -> PathBuf {
    paths::base_dir().join("data").join("quests.json")
}

static STORE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Clamp current_stage to the number of stages (corrupt data / manual JSON edit).
fn clamp_current_stage(quest: &mut Quest) {
    quest.current_stage = quest.current_stage.min(quest.stages.len());
}

pub fn load() -> QuestStore {
    let mut store: QuestStore = crate::json_store::load(&STORE, &store_path());
    for quest in store.values_mut() {
        clamp_current_stage(quest);
    }
    store
}

pub fn update<T>(change: impl FnOnce(&mut QuestStore) -> T) -> Result<T, String> {
    update_at(&store_path(), change)
}

fn update_at<T>(path: &Path, change: impl FnOnce(&mut QuestStore) -> T) -> Result<T, String> {
    crate::json_store::update(&STORE, path, change)
}

/// Text for the prompt: active quests and their stages.
pub fn quests_note(store: &QuestStore) -> Option<String> {
    let active: Vec<&Quest> = store.values().filter(|q| q.status == QuestStatus::Active).collect();
    if active.is_empty() {
        return None;
    }
    let lines: Vec<String> = active
        .iter()
        .map(|quest| {
            let total = quest.stages.len();
            let current = quest
                .current_stage()
                .map(|s| s.objective.as_str())
                .unwrap_or("—");
            if total == 0 {
                format!("«{}» — нет стадий", quest.title)
            } else {
                format!(
                    "«{}» — стадия {}/{} ({})",
                    quest.title, quest.current_stage.saturating_add(1), total, current
                )
            }
        })
        .collect();
    Some(format!("Квесты:\n{}", lines.join("\n")))
}

/// Prompt for quest generation considering NPCs and rewards.
pub fn generation_prompt(
    location: &str,
    npcs: &[String],
    npc_details: &[(String, String)], // (id, name)
    giver_name: Option<&str>,
) -> String {
    let npcs_str = if npcs.is_empty() { "нет".to_string() } else { npcs.join(", ") };
    let details_str = if npc_details.is_empty() {
        String::new()
    } else {
        let pairs: Vec<String> = npc_details
            .iter()
            .map(|(id, name)| format!("{name} (id: {id})"))
            .collect();
        format!("\nДоступные NPC: {}", pairs.join(", "))
    };
    let giver_hint = match giver_name {
        Some(name) => format!(
            "\nКвест выдаёт: {name}. Награды должны соответствовать \
             характеру NPC (торговец → золото, кузнец → оружие, травница → зелья)."
        ),
        None => String::new(),
    };
    format!(
        "Сгенерируй квест для ролевой игры. Локация: «{location}». NPC рядом: {npcs_str}.{details_str}{giver_hint}\n\n\
         Верни ТОЛЬКО JSON:\n\
         {{\"title\": \"Название\", \"description\": \"Описание\", \
         \"stages\": [{{\"description\": \"Описание стадии\", \"objective\": \"Что сделать\", \
         \"completion_condition\": \"Условие завершения\"}}], \
         \"giver\": \"id NPC или director\", \
         \"rewards\": {{\"experience\": N, \"items\": [\"предмет\"]}}}}\n\n\
         Квест должен иметь 2–5 стадий."
    )
}

/// Prompt for AI evaluation: whether the quest advanced.
pub fn evaluator_prompt(
    quest_title: &str,
    stage_objective: &str,
    user_text: &str,
    reply_text: &str,
) -> String {
    format!(
        "Оцени, продвинулся ли игрок в квесте «{quest_title}».\n\n\
         Текущая цель: {stage_objective}\n\
         Реплика пользователя: «{user}»\n\
         Ответ персонажа: «{reply}»\n\n\
         Верни ТОЛЬКО JSON:\n\
         {{\"advanced\": true/false, \"reason\": \"кратко почему\"}}",
        quest_title = quest_title,
        stage_objective = stage_objective,
        user = user_text.chars().take(600).collect::<String>(),
        reply = reply_text.chars().take(600).collect::<String>(),
    )
}

// -- Writers: parsing model replies with clamps, "commit only on success" --

/// Truncate a string by characters (prompt contract clamp).
fn clamp_chars(raw: &str, max: usize) -> String {
    raw.trim().chars().take(max).collect()
}

/// Quest draft from the model reply (after clamps).
#[derive(Debug, Clone)]
pub struct QuestDraft {
    pub title: String,
    pub description: String,
    pub stages: Vec<QuestStage>,
    pub giver: Option<String>,
    pub rewards: QuestRewards,
}

/// Parse the model reply from `generation_prompt`: {"title", "description",
/// "stages": [...], "giver", "rewards": {"experience", "items",
/// "relationship_shifts"}}. Fail closed: garbage → None. Clamps under the prompt
/// contract: title 1–120, description up to 1000, 1–8 stages (fields up to 400),
/// XP up to 10,000, up to 8 items, up to 5 relationship shifts (axes ±100).
pub fn parse_generated(raw: &str) -> Option<QuestDraft> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end <= start {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(&raw[start..=end]).ok()?;

    let title = clamp_chars(value.get("title")?.as_str()?, 120);
    if title.is_empty() {
        return None;
    }
    let description = clamp_chars(
        value.get("description").and_then(|v| v.as_str()).unwrap_or_default(),
        1000,
    );

    let mut stages = Vec::new();
    if let Some(list) = value.get("stages").and_then(|v| v.as_array()) {
        for stage in list.iter().take(8) {
            let objective = clamp_chars(
                stage.get("objective").and_then(|v| v.as_str()).unwrap_or_default(),
                400,
            );
            if objective.is_empty() {
                continue;
            }
            stages.push(QuestStage {
                description: clamp_chars(
                    stage.get("description").and_then(|v| v.as_str()).unwrap_or_default(),
                    400,
                ),
                objective,
                completed: false,
                completion_condition: clamp_chars(
                    stage.get("completion_condition").and_then(|v| v.as_str()).unwrap_or_default(),
                    400,
                ),
            });
        }
    }
    // A quest without objectives is not a quest: reject it.
    if stages.is_empty() {
        return None;
    }

    let giver = value
        .get("giver")
        .and_then(|v| v.as_str())
        .map(|s| clamp_chars(s, 60))
        .filter(|s| !s.is_empty());

    let rewards_json = value.get("rewards");
    let experience = rewards_json
        .and_then(|r| r.get("experience"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        .min(10_000) as u32;
    let items: Vec<String> = rewards_json
        .and_then(|r| r.get("items"))
        .and_then(|v| v.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_str())
                .map(|s| clamp_chars(s, 60))
                .filter(|s| !s.is_empty())
                .take(8)
                .collect()
        })
        .unwrap_or_default();
    let mut relationship_shifts = BTreeMap::new();
    if let Some(shifts) = rewards_json
        .and_then(|r| r.get("relationship_shifts"))
        .and_then(|v| v.as_object())
    {
        for (target, axes) in shifts.iter().take(5) {
            if let Ok(mut axes) = serde_json::from_value::<RelationshipAxes>(axes.clone()) {
                axes.sanitize();
                for axis in [
                    &mut axes.friendship,
                    &mut axes.romance,
                    &mut axes.respect,
                    &mut axes.fear,
                    &mut axes.hatred,
                ] {
                    *axis = axis.clamp(-100.0, 100.0);
                }
                relationship_shifts.insert(clamp_chars(target, 60), axes);
            }
        }
    }

    Some(QuestDraft {
        title,
        description,
        stages,
        giver,
        rewards: QuestRewards {
            experience,
            stat_boosts: BTreeMap::new(),
            items,
            relationship_shifts,
        },
    })
}

/// Parse and write a quest from the model reply. "Commit only on success":
/// on garbage the file is neither created nor rewritten.
pub fn commit_generated(raw: &str) -> Result<Quest, String> {
    let draft = parse_generated(raw)
        .ok_or_else(|| "Модель вернула не JSON или квест без целей".to_string())?;
    update(|store| insert_quest(store, draft))
}

/// Insert a quest with a new id (ids are monotonic: `quest_1`, `quest_2`, …).
fn insert_quest(store: &mut QuestStore, draft: QuestDraft) -> Quest {
    let mut n = 1;
    while store.contains_key(&format!("quest_{n}")) {
        n += 1;
    }
    let quest = Quest {
        id: format!("quest_{n}"),
        title: draft.title,
        description: draft.description,
        stages: draft.stages,
        current_stage: 0,
        status: QuestStatus::Active,
        rewards: draft.rewards,
        giver: draft.giver,
        rewards_claimed: false,
    };
    store.insert(quest.id.clone(), quest.clone());
    quest
}

/// Result of advancing a quest based on the model reply.
#[derive(Debug, Clone)]
pub enum QuestProgress {
    /// The stage is cleared, the quest continues.
    Advanced,
    /// Quest completed — the reward is claimed via `claim_rewards` (exactly once).
    Completed,
    /// The model saw no progress (or the quest is no longer active).
    Unchanged,
}

/// Parse the `evaluator_prompt` reply: {"advanced": bool, "reason": "..."}.
/// Fail closed: without a valid `advanced` — None.
pub fn parse_progress(raw: &str) -> Option<bool> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end <= start {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(&raw[start..=end]).ok()?;
    value.get("advanced")?.as_bool()
}

/// Advance the quest by the model reply. "Commit only on success": on garbage
/// the file is left untouched, and without progress too.
pub fn commit_progress(quest_id: &str, raw: &str) -> Result<QuestProgress, String> {
    commit_progress_at(&store_path(), quest_id, raw)
}

fn commit_progress_at(path: &Path, quest_id: &str, raw: &str) -> Result<QuestProgress, String> {
    let advanced = parse_progress(raw)
        .ok_or_else(|| "Модель вернула не JSON или без поля advanced".to_string())?;
    if !advanced {
        return Ok(QuestProgress::Unchanged);
    }
    crate::json_store::update_if_ok(&STORE, path, |store: &mut QuestStore| {
        let quest = store
            .get_mut(quest_id)
            .ok_or_else(|| format!("Квест «{quest_id}» не найден"))?;
        if quest.status != QuestStatus::Active {
            return Ok(QuestProgress::Unchanged);
        }
        if quest.advance() {
            Ok(QuestProgress::Completed)
        } else {
            Ok(QuestProgress::Advanced)
        }
    })
    .and_then(|result| result)
}

/// Result of granting quest rewards (for UI and log).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RewardsSummary {
    pub experience: u32,
    pub items: Vec<String>,
    pub relationship_shifts: usize,
}

/// Apply quest rewards to the stores (pure part, no file IO):
/// XP — into stats (`stats::award_experience`, weights — `weights` from
/// `quest_xp_prompt`/`stats::parse_stat_deltas`, None — evenly) plus direct
/// XP from `stat_boosts`; items — into inventory (`inventory::grant_items`);
/// relationship shifts — by axes (`RelationshipStore::shift_axis`).
#[cfg(test)]
pub fn apply_rewards(
    quest: &Quest,
    avatar: &str,
    weights: Option<&BTreeMap<String, u32>>,
    stats_store: &mut StatsStore,
    inventory_store: &mut InventoryStore,
    rel_store: &mut RelationshipStore,
) -> RewardsSummary {
    let experience = grant_stats(quest, avatar, weights, stats_store);
    let items = grant_items(quest, inventory_store);
    let relationship_shifts = grant_relationships(quest, avatar, rel_store);
    RewardsSummary {
        experience,
        items,
        relationship_shifts,
    }
}

fn grant_stats(
    quest: &Quest,
    avatar: &str,
    weights: Option<&BTreeMap<String, u32>>,
    stats_store: &mut StatsStore,
) -> u32 {
    let effective_weights = weights.cloned().unwrap_or_default();
    let experience = stats::award_experience(
        stats_store,
        avatar,
        &effective_weights,
        quest.rewards.experience,
    );
    // stat_boosts — direct XP for named stats.
    if let Some(cs) = stats_store.get_mut(avatar) {
        for (stat, boost) in &quest.rewards.stat_boosts {
            if let Some(entry) = cs.stats.get_mut(stat.as_str()) {
                stats::add_experience(entry, *boost);
            }
        }
    }
    experience
}

fn grant_items(quest: &Quest, inventory_store: &mut InventoryStore) -> Vec<String> {
    inventory::grant_items(inventory_store, &quest.rewards.items)
        .into_iter()
        .map(|item| item.name)
        .collect()
}

fn grant_relationships(
    quest: &Quest,
    avatar: &str,
    rel_store: &mut RelationshipStore,
) -> usize {
    let display = crate::traits::display_name(avatar);
    let mut relationship_shifts = 0usize;
    for (target, axes) in &quest.rewards.relationship_shifts {
        for (axis, delta) in [
            ("friendship", axes.friendship),
            ("romance", axes.romance),
            ("respect", axes.respect),
            ("fear", axes.fear),
            ("hatred", axes.hatred),
        ] {
            if delta != 0.0 {
                rel_store.shift_axis(&display, target, axis, delta);
                relationship_shifts += 1;
            }
        }
    }
    relationship_shifts
}

/// Claim the reward of a completed quest to disk: XP — into stats,
/// items — into inventory, shifts — into relationships. The reward is granted exactly
/// once: `rewards_claimed` is set as the first step (fail closed — a failure of the following
/// writes loses the reward but does not double it).
pub fn claim_rewards(
    quest_id: &str,
    avatar: &str,
    weights: Option<&BTreeMap<String, u32>>,
) -> Result<RewardsSummary, String> {
    let quest: Quest = crate::json_store::update_if_ok(&STORE, &store_path(), |store: &mut QuestStore| {
        let quest = store
            .get_mut(quest_id)
            .ok_or_else(|| format!("Квест «{quest_id}» не найден"))?;
        if quest.status != QuestStatus::Completed {
            return Err("Награда выдаётся только за завершённый квест".to_string());
        }
        if quest.rewards_claimed {
            return Err("Награда за этот квест уже выдана".to_string());
        }
        quest.rewards_claimed = true;
        Ok(quest.clone())
    })
    .and_then(|result| result)?;

    // Each reward is applied inside its store's update closure:
    // a `load` snapshot + writing the whole store lost concurrent writes by the evaluator,
    // combat or trade made between read and commit.
    let experience = stats::update(|store| grant_stats(&quest, avatar, weights, store))?;
    let items = inventory::update(|store| grant_items(&quest, store))?;
    let rel_path = crate::paths::data_dir().join("relationships.json");
    let relationship_shifts =
        RelationshipStore::update(&rel_path, |store| grant_relationships(&quest, avatar, store))?;
    Ok(RewardsSummary {
        experience,
        items,
        relationship_shifts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_quest() -> Quest {
        Quest {
            id: "q1".into(),
            title: "Найти артефакт".into(),
            description: "Древний артефакт спрятан в пещере.".into(),
            stages: vec![
                QuestStage {
                    description: "Поговорить со старостой".into(),
                    objective: "Поговорить со старостой о пещере".into(),
                    completed: false,
                    completion_condition: "Игрок расспросил старосту".into(),
                },
                QuestStage {
                    description: "Добраться до пещеры".into(),
                    objective: "Перейти в локацию Пещера".into(),
                    completed: false,
                    completion_condition: "Игрок в локации Пещера".into(),
                },
                QuestStage {
                    description: "Найти артефакт".into(),
                    objective: "Обыскать пещеру".into(),
                    completed: false,
                    completion_condition: "Игрок нашёл артефакт".into(),
                },
            ],
            current_stage: 0,
            status: QuestStatus::Active,
            rewards: QuestRewards {
                experience: 300,
                ..Default::default()
            },
            giver: Some("elder".into()),
            rewards_claimed: false,
        }
    }

    #[test]
    fn current_stage_works() {
        let quest = sample_quest();
        let stage = quest.current_stage().unwrap();
        assert_eq!(stage.objective, "Поговорить со старостой о пещере");
    }

    #[test]
    fn advance_moves_stage() {
        let mut quest = sample_quest();
        assert!(!quest.advance());
        assert_eq!(quest.current_stage, 1);
        assert!(quest.stages[0].completed);
        assert_eq!(quest.status, QuestStatus::Active);
    }

    #[test]
    fn advance_completes_quest() {
        let mut quest = sample_quest();
        quest.advance(); // stage 0
        quest.advance(); // stage 1
        assert!(quest.advance()); // stage 2 → completed
        assert_eq!(quest.status, QuestStatus::Completed);
        assert!(quest.current_stage().is_none());
    }

    #[test]
    fn quests_note_format() {
        let mut store = QuestStore::new();
        store.insert("q1".into(), sample_quest());
        let note = quests_note(&store).unwrap();
        assert!(note.contains("Найти артефакт"), "{note}");
        assert!(note.contains("1/3"), "{note}");
    }

    #[test]
    fn evaluator_prompt_format() {
        let prompt = evaluator_prompt("Найти артефакт", "Поговорить со старостой", "расскажи", "староста рассказал");
        assert!(prompt.contains("Найти артефакт"), "{prompt}");
        assert!(prompt.contains("Поговорить со старостой"), "{prompt}");
    }

    #[test]
    fn advance_empty_stages() {
        let mut quest = Quest {
            id: "q_empty".into(),
            title: "Пустой квест".into(),
            description: "Без стадий.".into(),
            stages: vec![],
            current_stage: 0,
            status: QuestStatus::Active,
            rewards: QuestRewards::default(),
            giver: None,
            rewards_claimed: false,
        };
        // Does not panic and does not complete the quest.
        assert!(!quest.advance());
        assert_eq!(quest.status, QuestStatus::Active);
    }

    #[test]
    fn advance_single_stage() {
        let mut quest = Quest {
            id: "q1s".into(),
            title: "Одна стадия".into(),
            description: "Простой квест.".into(),
            stages: vec![QuestStage {
                description: "Сделай".into(),
                objective: "Сделай дело".into(),
                completed: false,
                completion_condition: "Дело сделано".into(),
            }],
            current_stage: 0,
            status: QuestStatus::Active,
            rewards: QuestRewards::default(),
            giver: None,
            rewards_claimed: false,
        };
        assert!(quest.advance());
        assert_eq!(quest.status, QuestStatus::Completed);
        assert!(quest.stages[0].completed);
    }

    #[test]
    fn quests_note_empty_store() {
        let store = QuestStore::new();
        assert!(quests_note(&store).is_none());
    }

    #[test]
    fn quests_note_all_completed() {
        let mut store = QuestStore::new();
        let mut quest = sample_quest();
        quest.status = QuestStatus::Completed;
        store.insert("q1".into(), quest);
        assert!(quests_note(&store).is_none());
    }

    #[test]
    fn advance_completed_quest() {
        let mut quest = sample_quest();
        quest.status = QuestStatus::Completed;
        // A repeated advance on completed is a no-op.
        assert!(!quest.advance());
        assert_eq!(quest.status, QuestStatus::Completed);
        assert_eq!(quest.current_stage, 0);
    }

    #[test]
    fn generation_prompt_empty_npcs() {
        let prompt = generation_prompt("Лес", &[], &[], None);
        assert!(prompt.contains("нет"), "{prompt}");
        assert!(prompt.contains("Лес"), "{prompt}");
    }

    #[test]
    fn generation_prompt_with_npcs() {
        let prompt = generation_prompt(
            "Таверна",
            &["bob".into(), "alice".into()],
            &[("bob".into(), "Боб".into()), ("alice".into(), "Алиса".into())],
            Some("Боб"),
        );
        assert!(prompt.contains("bob"), "{prompt}");
        assert!(prompt.contains("alice"), "{prompt}");
        assert!(prompt.contains("Боб"), "{prompt}");
        assert!(prompt.contains("Алиса"), "{prompt}");
        assert!(prompt.contains("Квест выдаёт: Боб"), "{prompt}");
        assert!(prompt.contains("торговец"), "должны быть примеры наград: {prompt}");
    }

    #[test]
    fn evaluator_prompt_truncates_long_text() {
        let long = "а".repeat(1000);
        let prompt = evaluator_prompt("Квест", "Цель", &long, &long);
        // Truncated to 600 characters — the prompt must not contain the full thousand.
        assert!(!prompt.contains(&"а".repeat(700)), "должна быть обрезка");
    }

    #[test]
    fn advance_double_same_stage() {
        let mut quest = sample_quest();
        assert!(!quest.advance()); // stage 0 → 1
        // A repeated advance on the same stage 1 (it is not completed yet).
        assert!(!quest.advance()); // stage 1 → 2
        // But there must be no double jump: current_stage == 2, not 3.
        assert_eq!(quest.current_stage, 2);
        assert!(quest.stages[0].completed);
        assert!(quest.stages[1].completed);
        assert!(!quest.stages[2].completed);
    }

    #[test]
    fn quests_note_zero_stages() {
        let mut store = QuestStore::new();
        let mut quest = sample_quest();
        quest.stages = vec![];
        store.insert("q1".into(), quest);
        let note = quests_note(&store).unwrap();
        assert!(note.contains("нет стадий"), "{note}");
        assert!(!note.contains("1/0"), "не должно быть формата 1/0: {note}");
    }

    #[test]
    fn current_stage_completed_quest() {
        let mut quest = sample_quest();
        quest.status = QuestStatus::Completed;
        assert!(quest.current_stage().is_none());
    }

    #[test]
    fn current_stage_failed_quest() {
        let mut quest = sample_quest();
        quest.status = QuestStatus::Failed;
        assert!(quest.current_stage().is_none());
    }

    #[test]
    fn clamp_current_stage_on_load() {
        let mut quest = sample_quest();
        quest.current_stage = 99;
        clamp_current_stage(&mut quest);
        assert_eq!(quest.current_stage, quest.stages.len());
    }

    #[test]
    fn advance_uses_saturating_add() {
        let mut quest = sample_quest();
        quest.current_stage = usize::MAX;
        // saturating_add(1) must not panic/overflow
        quest.advance();
        assert_eq!(quest.current_stage, usize::MAX);
    }

    #[test]
    fn quests_note_stage_display_saturating() {
        let mut quest = sample_quest();
        quest.current_stage = usize::MAX;
        let mut store = QuestStore::new();
        store.insert("q1".into(), quest);
        let note = quests_note(&store).unwrap();
        assert!(note.contains("стадия"), "{note}");
    }

    #[test]
    fn quest_xp_prompt_format() {
        let prompt = quest_xp_prompt("Найти артефакт", "Древний артефакт спрятан в пещере.");
        assert!(prompt.contains("Найти артефакт"), "{prompt}");
        assert!(prompt.contains("пещере"), "{prompt}");
        assert!(prompt.contains("JSON"), "{prompt}");
    }

    #[test]
    fn quest_xp_prompt_truncates_long_description() {
        let long = "а".repeat(1000);
        let prompt = quest_xp_prompt("Квест", &long);
        assert!(!prompt.contains(&"а".repeat(700)), "должна быть обрезка");
    }

    #[test]
    fn combat_xp_prompt_format() {
        let prompt = combat_xp_prompt("Гоблин", "melee", 5);
        assert!(prompt.contains("Гоблин"), "{prompt}");
        assert!(prompt.contains("melee"), "{prompt}");
        assert!(prompt.contains("5"), "{prompt}");
    }

    // --- Writers: parse/commit for model answers (V-03/V-57) ---

    fn temp_quest_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("cloufam_test_quests");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("json.corrupt"));
        path
    }

    const GENERATED: &str = r#"{
        "title": "Пропавший купец",
        "description": "Купец не вернулся с ярмарки.",
        "stages": [
            {"description": "Расспросить", "objective": "Поговорить с торговцем", "completion_condition": "Игрок расспросил торговца"},
            {"description": "Найти", "objective": "Обыскать лесную тропу", "completion_condition": "Игрок нашёл купца"}
        ],
        "giver": "merchant",
        "rewards": {"experience": 300, "items": ["Зелье здоровья"], "relationship_shifts": {"merchant": {"friendship": 10}}}
    }"#;

    #[test]
    fn parse_generated_accepts_model_json() {
        let draft = parse_generated(GENERATED).expect("валидный ответ модели");
        assert_eq!(draft.title, "Пропавший купец");
        assert_eq!(draft.stages.len(), 2);
        assert_eq!(draft.giver.as_deref(), Some("merchant"));
        assert_eq!(draft.rewards.experience, 300);
        assert_eq!(draft.rewards.items, vec!["Зелье здоровья".to_string()]);
        assert_eq!(draft.rewards.relationship_shifts["merchant"].friendship, 10.0);
    }

    #[test]
    fn parse_generated_rejects_garbage_and_no_stages() {
        assert!(parse_generated("не json").is_none());
        assert!(parse_generated("{\"title\": \"Без стадий\", \"stages\": []}").is_none());
        assert!(parse_generated("{\"stages\": [{\"objective\": \"x\"}]}").is_none(), "без названия не берём");
    }

    #[test]
    fn parse_generated_clamps_per_prompt_contract() {
        let raw = format!(
            r#"{{"title": "{}", "description": "{}", "stages": [
                {{"objective": "a"}}, {{"objective": "b"}}, {{"objective": "c"}},
                {{"objective": "d"}}, {{"objective": "e"}}, {{"objective": "f"}},
                {{"objective": "g"}}, {{"objective": "h"}}, {{"objective": "i"}}],
                "rewards": {{"experience": 999999, "items": ["x","y","z","p","q","r","s","t","u","v"],
                "relationship_shifts": {{"a": {{"friendship": 999}}, "b": {{}}, "c": {{}}, "d": {{}}, "e": {{}}, "f": {{}}}}}}}}"#,
            "т".repeat(200),
            "о".repeat(2000),
        );
        let draft = parse_generated(&raw).expect("валидный с клэмпами");
        assert_eq!(draft.title.chars().count(), 120);
        assert_eq!(draft.description.chars().count(), 1000);
        assert_eq!(draft.stages.len(), 8, "стадий 2–8, лишние отрезаются");
        assert_eq!(draft.rewards.experience, 10_000);
        assert_eq!(draft.rewards.items.len(), 8);
        assert_eq!(draft.rewards.relationship_shifts.len(), 5);
        assert_eq!(draft.rewards.relationship_shifts["a"].friendship, 100.0, "ось зажата к ±100");
    }

    /// "Commit only on success": there is nothing to commit from model garbage,
    /// a valid reply creates a quest with an id and Active status.
    #[test]
    fn commit_generated_commits_only_on_success() {
        let path = temp_quest_path("commit.json");
        assert!(parse_generated("мусор").is_none(), "мусор не разбирается");
        // The production path refuses before any IO: no file is created.
        assert!(commit_generated("мусор").is_err());
        assert!(!path.exists());

        let quest = update_at(&path, |store| insert_quest(store, parse_generated(GENERATED).unwrap()))
            .expect("валидный квест пишется");
        assert_eq!(quest.id, "quest_1");
        assert_eq!(quest.status, QuestStatus::Active);
        assert!(!quest.rewards_claimed);
        let store: QuestStore = crate::json_store::load(&STORE, &path);
        assert_eq!(store.len(), 1, "квест записан на диск");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn parse_progress_reads_only_valid_bool() {
        assert_eq!(parse_progress(r#"{"advanced": true, "reason": "да"}"#), Some(true));
        assert_eq!(parse_progress("Вот:\n```json\n{\"advanced\": false}\n```"), Some(false));
        assert_eq!(parse_progress("не json"), None);
        assert_eq!(parse_progress(r#"{"reason": "нет поля"}"#), None);
        assert_eq!(parse_progress(r#"{"advanced": "yes"}"#), None);
    }

    /// "Commit only on success" for progress: garbage does not write the file, "no
    /// progress" leaves it untouched, success moves the stage.
    #[test]
    fn commit_progress_commits_only_on_success() {
        let path = temp_quest_path("progress.json");
        update_at(&path, |store| insert_quest(store, parse_generated(GENERATED).unwrap())).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        assert!(commit_progress_at(&path, "quest_1", "мусор").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "мусор не пишет файл");

        let progress = commit_progress_at(&path, "quest_1", r#"{"advanced": false}"#).unwrap();
        assert!(matches!(progress, QuestProgress::Unchanged));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "без прогресса файл не тронут");

        let progress = commit_progress_at(&path, "quest_1", r#"{"advanced": true}"#).unwrap();
        assert!(matches!(progress, QuestProgress::Advanced));
        let store: QuestStore = crate::json_store::load(&STORE, &path);
        assert_eq!(store["quest_1"].current_stage, 1);

        // The last stage completes the quest and yields Completed for rewards.
        let progress = commit_progress_at(&path, "quest_1", r#"{"advanced": true}"#).unwrap();
        assert!(matches!(progress, QuestProgress::Completed), "{progress:?}");
        let store: QuestStore = crate::json_store::load(&STORE, &path);
        assert_eq!(store["quest_1"].status, QuestStatus::Completed);
        assert!(!store["quest_1"].rewards_claimed, "награда не выдаётся сама");

        // A completed quest no longer moves.
        let progress = commit_progress_at(&path, "quest_1", r#"{"advanced": true}"#).unwrap();
        assert!(matches!(progress, QuestProgress::Unchanged));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn commit_progress_unknown_quest_is_error_without_write() {
        let path = temp_quest_path("unknown_quest.json");
        assert!(commit_progress_at(&path, "нет_такого", r#"{"advanced": true}"#).is_err());
        assert!(!path.exists(), "неизвестный квест не создаёт файл");
    }

    #[test]
    fn apply_rewards_spreads_xp_items_and_shifts() {
        let mut quest = sample_quest();
        quest.rewards.experience = 120;
        quest.rewards.items = vec!["Зелье здоровья".into(), "нет такого".into()];
        quest.rewards.relationship_shifts.insert(
            "merchant".into(),
            RelationshipAxes {
                friendship: 12.0,
                ..Default::default()
            },
        );

        let mut stats_store = StatsStore::new();
        let mut inventory_store = InventoryStore::default();
        let mut rel_store = RelationshipStore::default();
        let summary = apply_rewards(
            &quest,
            "hero.png",
            None,
            &mut stats_store,
            &mut inventory_store,
            &mut rel_store,
        );

        assert_eq!(summary.experience, 120, "опыт разошёлся по характеристикам");
        assert_eq!(
            summary.items,
            vec!["Зелье здоровья".to_string(), "нет такого".to_string()],
            "неизвестная награда выдаётся, но не пропадает молча"
        );
        assert_eq!(summary.relationship_shifts, 1);
        let cs = stats_store.get("hero.png").expect("характеристики заведены");
        for entry in cs.stats.values() {
            assert_eq!(entry.experience, 20, "120 / 6 характеристик");
        }
        assert!(inventory_store.player.iter().any(|i| i.id == "health_potion"));
        // An unknown reward is not silently lost, but it does not become currency either.
        assert!(inventory_store.player.iter().any(|i| i.name == "нет такого"));
        let axes = rel_store.get("hero", "merchant");
        assert_eq!(axes.friendship, 12.0);
    }

    #[test]
    fn apply_rewards_honors_weights() {
        let mut quest = sample_quest();
        quest.rewards.experience = 100;
        let mut weights = BTreeMap::new();
        weights.insert("Strength".to_string(), 3u32);
        weights.insert("Charisma".to_string(), 1u32);

        let mut stats_store = StatsStore::new();
        let summary = apply_rewards(
            &quest,
            "hero.png",
            Some(&weights),
            &mut stats_store,
            &mut InventoryStore::default(),
            &mut RelationshipStore::default(),
        );
        assert_eq!(summary.experience, 100);
        let cs = stats_store.get("hero.png").unwrap();
        // 75 XP exceed the level 1 threshold (50): Strength → level 2, 25 left over.
        assert_eq!(cs.stats["Strength"].level, 2);
        assert_eq!(cs.stats["Strength"].experience, 25);
        assert_eq!(cs.stats["Charisma"].level, 1);
        assert_eq!(cs.stats["Charisma"].experience, 25);
        assert_eq!(cs.stats["Endurance"].experience, 0, "вне весов опыт не капает");
    }
}

