//! RPG prompt generation: collects the state of all subsystems into a single
//! note for the LLM. Mixed in by the gateway as the last system message
//! (the `gateway::live_state_note` pattern).

use crate::relationships::{RelationshipAxes, RelationshipStore};
use crate::rpg::{enemies, inventory, npcs, quests, skills, stats, world};

/// Textual summary of relationships: the AI immediately sees the nature of the bond,
/// not just raw numbers.
fn relationship_summary(axes: &RelationshipAxes) -> &'static str {
    // Priority: hatred > fear > romance > friendship > respect.
    let hatred = axes.hatred as i32;
    let fear = axes.fear as i32;
    let romance = axes.romance as i32;
    let friendship = axes.friendship as i32;
    let respect = axes.respect as i32;

    if hatred >= 60 {
        "враг"
    } else if hatred >= 30 {
        "недоброжелатель"
    } else if fear >= 60 {
        "боится"
    } else if fear >= 30 {
        "насторожен"
    } else if romance >= 60 {
        "влюблён"
    } else if romance >= 30 {
        "симпатия"
    } else if friendship >= 60 {
        "друг"
    } else if friendship >= 30 {
        "приятель"
    } else if respect >= 60 {
        "уважает"
    } else if respect >= 30 {
        "нейтрален-положительный"
    } else {
        "нейтральный"
    }
}

/// Full RPG note for the prompt. Collects data from all subsystems.
/// Empty blocks are skipped (they do not clutter the context).
pub fn rpg_state_note(
    avatar: &str,
    persona: &str,
    world_store: Option<&world::World>,
    stats_store: &stats::StatsStore,
    skills_store: &skills::SkillsStore,
    quests_store: &quests::QuestStore,
    inventory_store: &inventory::InventoryStore,
    npc_store: &npcs::NpcStore,
    rel_store: &RelationshipStore,
    active_combat: Option<&enemies::ActiveCombat>,
) -> String {
    let mut blocks: Vec<String> = Vec::new();

    // World and location. `None` — the player has not started RPG yet: the default fantasy city
    // from `World::default()` led any scene to the tavern on the square.
    if let Some(world_store) = world_store {
        let world_note = world::world_note(world_store);
        if !world_note.is_empty() {
            blocks.push(world_note);
        }
    }

    // Stats, skills, quests and inventory — only when RPG has started.
    // They grow from ordinary conversation (written by the state evaluator), and in
    // a roleplay chat without RPG the line "Stats: Charisma 1, Dexterity 1…"
    // only confuses the model — as the default city used to.
    if world_store.is_some() {
        if let Some(note) = stats::stats_note(avatar, stats_store) {
            blocks.push(note);
        }

        if let Some(note) = skills::skills_note(avatar, skills_store) {
            blocks.push(note);
        }

        if let Some(note) = quests::quests_note(quests_store) {
            blocks.push(note);
        }

        if let Some(note) = inventory::inventory_note(inventory_store) {
            blocks.push(note);
        }
    }

    // NPCs in the location.
    if let Some(location) = world_store.and_then(|w| w.current()) {
        let current = world_store.map(|w| w.current_location.as_str()).unwrap_or_default();
        if let Some(note) = npcs::npcs_note(npc_store, current) {
            blocks.push(note);
        }
        // Shop in the location.
        if location.has_shop {
            if let Some(shop_type) = &location.shop_type {
                blocks.push(format!("Здесь есть магазин: {shop_type}"));
            }
        }
    }

    // Relationships with NPCs in the location. The key is the display name: the
    // evaluator writes under it, and the relationships screen reads under it. By raw avatar
    // ("Gawr_Gura.png") the entry was not found, and the block never made it into the prompt.
    if !persona.is_empty() {
        let axes = rel_store.get(&crate::traits::display_name(avatar), persona);
        if axes != RelationshipAxes::default() {
            let summary = relationship_summary(&axes);
            blocks.push(format!(
                "Отношения к {}: {} (дружба {}, романтика {}, уважение {}, страх {}, ненависть {})",
                persona,
                summary,
                axes.friendship as i32,
                axes.romance as i32,
                axes.respect as i32,
                axes.fear as i32,
                axes.hatred as i32,
            ));
        }
    }

    // An ongoing fight — only in a started world: a finished fight ("Enemy HP: 0") otherwise
    // hung in every scene until the fight file was removed.
    if let Some(combat) = active_combat.filter(|c| world_store.is_some() && !c.is_over()) {
        blocks.push(enemies::combat_prompt(combat));
    }

    if blocks.is_empty() {
        return String::new();
    }

    format!("[RPG]\n{}", blocks.join("\n"))
}

/// Combined prompt evaluator: one model call instead of three.
/// Evaluates the character state, the stats and skills involved.
pub fn combined_evaluator_prompt(
    character: &str,
    persona: &str,
    user_text: &str,
    reply_text: &str,
    current_state: &crate::traits::LiveState,
) -> String {
    let display = crate::traits::display_name(character);
    let user_short: String = user_text.chars().take(600).collect();
    let reply_short: String = reply_text.chars().take(600).collect();

    let persona_line = if persona.is_empty() {
        String::new()
    } else {
        format!("Собеседник: {persona}\n")
    };

    // The keys come from the same lists that `init_character` creates: a hardcoded
    // list lagged two skills behind (Investigation, Archery), and they never grew.
    let stats_keys = stats::DEFAULT_STATS
        .iter()
        .map(|name| format!("\"{name}\": N"))
        .collect::<Vec<_>>()
        .join(", ");
    let skills_keys = skills::DEFAULT_SKILLS
        .iter()
        .map(|(name, _)| format!("\"{name}\": N"))
        .collect::<Vec<_>>()
        .join(", ");

    format!(
        "Ты — анализатор ролевого диалога. Оцени последний обмен репликами.\n\n\
         Персонаж: {display}\n\
         {persona_line}\
         Текущее состояние: нежность {}/100, доверие {}/100, энергия {}/100, раздражение {}/100.\n\n\
         Реплика пользователя: «{user_short}»\n\
         Ответ персонажа: «{reply_short}»\n\n\
         Оцени эмоциональный тон реплики (не только содержание, но и подтекст, сарказм, иронию, \
         скрытые мотивы). Учитывай контекст отношений между персонажами.\n\n\
         Верни ТОЛЬКО JSON без пояснений. Все числа — СДВИГИ (дельты) за этот обмен, а НЕ текущие \
         значения: текущие значения уже известны и в ответ не нужны. Не копируй их в ответ.\n\
         {{\n\
           \"state\": {{\"affection\": 2, \"trust\": 0, \"energy\": -1, \"irritation\": 1, \"tone\": \"neutral\"}},\n\
           \"stats\": {{{stats_keys}}},\n\
           \"skills\": {{{skills_keys}}},\n\
           \"relationship\": {{\"friendship\": N, \"romance\": N, \"respect\": N, \"fear\": N, \"hatred\": N}}\n\
         }}\n\n\
         state: сдвиг каждого показателя от -15 до 15 (обычно от -3 до 3). 0–3 — обычная болтовня, \
         4–8 — заметное событие, 9–15 — ключевой момент (ссора, признание, удар по больному). \
         tone: tenderness|flirt|playful|neutral|cold|hostile.\n\
         stats: опыт 0–15 (0 если не задействована, 1–3 болтовня, 4–8 заметно, 9–15 ключевой момент).\n\
         skills: опыт 0–10 (0 если не задействован, 1–3 обычное, 4–7 заметно, 8–10 ключевой момент).\n\
         relationship: сдвиг отношения персонажа к собеседнику, N от -15 до 15 по каждой оси \
         (дружба, романтика, уважение, страх, ненависть); 0 — если ось в обмене не затронута.",
        current_state.affection,
        current_state.trust,
        current_state.energy,
        current_state.irritation,
    )
}

/// Result of the combined evaluation.
#[derive(Debug, Default)]
pub struct CombinedEvaluation {
    pub state_deltas: Option<crate::traits::Deltas>,
    pub stat_deltas: Option<std::collections::BTreeMap<String, u32>>,
    pub skill_deltas: Option<std::collections::BTreeMap<String, u32>>,
    /// Shift of the character's attitude toward the interlocutor along five axes. Without it
    /// nobody wrote `relationships.json`: the relationships screen was always empty,
    /// and the "Attitude toward …" block never made it into the prompt.
    pub relationship_deltas: Option<RelationshipAxes>,
}

/// Parsing of the combined model reply.
pub fn parse_combined_evaluator(raw: &str) -> Option<CombinedEvaluation> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end <= start {
        return None;
    }
    let inner = &raw[start..=end];
    let value: serde_json::Value = serde_json::from_str(inner).ok()?;

    let mut result = CombinedEvaluation::default();

    // Character state. If the `state` key is present, parsing must succeed: otherwise
    // there is no trust in the rest of the reply, and parsing is discarded entirely
    // (fail closed, see Deltas::is_plausible). Earlier an invalid `state` type
    // was silently skipped, and stats/skills/relationship were still applied.
    if let Some(state) = value.get("state") {
        // Numbers are a strict contract (fail closed, see Deltas::is_plausible), but
        // tone is an informational label: an unknown word from the model ("dramatic")
        // must not drop correct shifts entirely (18.10.2026 — live state
        // did not move precisely because of the strict tone enum).
        let mut deltas = serde_json::from_value::<crate::traits::Deltas>(state.clone()).ok();
        if deltas.is_none() {
            #[derive(serde::Deserialize, Default)]
            #[serde(default)]
            struct RawDeltas {
                affection: i32,
                trust: i32,
                energy: i32,
                irritation: i32,
                tone: Option<String>,
            }
            let raw: RawDeltas = serde_json::from_value(state.clone()).ok()?;
            deltas = Some(crate::traits::Deltas {
                affection: raw.affection,
                trust: raw.trust,
                energy: raw.energy,
                irritation: raw.irritation,
                tone: raw
                    .tone
                    .as_deref()
                    .map(crate::traits::Tone::parse_lossy)
                    .unwrap_or_default(),
            });
        }
        let deltas = deltas?;
        if !deltas.is_plausible() {
            return None;
        }
        if !deltas.is_zero() {
            result.state_deltas = Some(deltas);
        }
    }

    // Stats: the prompt asks for 0–15 XP per line but does not verify
    // the reply — we clamp the delta itself, not just the type and sign (otherwise, as with
    // live state, a randomly large number from the model gives an instant
    // level jump instead of gradual progression through add_experience).
    if let Some(stats) = value.get("stats").and_then(|v| v.as_object()) {
        let map: std::collections::BTreeMap<String, u32> = stats
            .iter()
            .filter_map(|(k, v)| v.as_u64().map(|n| (k.clone(), (n.min(MAX_STAT_XP as u64)) as u32)))
            .filter(|(_, n)| *n > 0)
            .collect();
        if !map.is_empty() {
            result.stat_deltas = Some(map);
        }
    }

    // Attitude toward the interlocutor.
    // An axis above step 15 is a rewritten absolute value ("friendship: 50"), not
    // a shift: the same guard as is_plausible for state, otherwise relationships overheated toward ±100
    let absolute = |rel: &serde_json::Map<String, serde_json::Value>| {
        rel.values().filter_map(|v| v.as_f64()).any(|n| n.abs() > 15.0)
    };
    if let Some(rel) = value.get("relationship").and_then(|v| v.as_object()).filter(|rel| !absolute(rel)) {
        let axis = |name: &str| {
            rel.get(name)
                .and_then(|v| v.as_f64())
                .filter(|n| n.is_finite())
                .map(|n| n.clamp(-15.0, 15.0) as f32)
                .unwrap_or(0.0)
        };
        let deltas = RelationshipAxes {
            friendship: axis("friendship"),
            romance: axis("romance"),
            respect: axis("respect"),
            fear: axis("fear"),
            hatred: axis("hatred"),
        };
        let zero = [deltas.friendship, deltas.romance, deltas.respect, deltas.fear, deltas.hatred]
            .iter()
            .all(|v| *v == 0.0);
        if !zero {
            result.relationship_deltas = Some(deltas);
        }
    }

    // Skills: 0–10 XP per line under the same contract as stats.
    if let Some(skills) = value.get("skills").and_then(|v| v.as_object()) {
        let map: std::collections::BTreeMap<String, u32> = skills
            .iter()
            .filter_map(|(k, v)| v.as_u64().map(|n| (k.clone(), (n.min(MAX_SKILL_XP as u64)) as u32)))
            .filter(|(_, n)| *n > 0)
            .collect();
        if !map.is_empty() {
            result.skill_deltas = Some(map);
        }
    }

    Some(result)
}

/// Prompt contract: stats grow 0–15 XP per line.
const MAX_STAT_XP: u32 = 15;
/// Prompt contract: skills grow 0–10 XP per line.
const MAX_SKILL_XP: u32 = 10;

/// How successful the player's action was — a tier, not a number. Numbers for
/// apply_skill/player_attack come from `power()`; the model does not compute them and
/// cannot return an arbitrary value (see `gateway::rpg_combat_action`):
/// earlier damage came from the model directly, with no contract check at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionOutcome {
    Critical,
    Hit,
    Graze,
    Miss,
}

impl ActionOutcome {
    /// Action power for `apply_skill`/`player_attack`: chosen to match the scale
    /// of existing enemy `combat_skills` (8–18 in default_enemies).
    /// Exact balancing is up to the owner; what matters here is not the number, but
    /// the fact that it does not come from the model.
    pub fn power(self) -> u32 {
        match self {
            ActionOutcome::Critical => 24,
            ActionOutcome::Hit => 14,
            ActionOutcome::Graze => 6,
            ActionOutcome::Miss => 0,
        }
    }

    fn parse(raw: &str) -> Self {
        // "Miss" with a capital letter otherwise became a hit with power 14.
        match raw.trim().to_ascii_lowercase().as_str() {
            "critical" => ActionOutcome::Critical,
            "graze" => ActionOutcome::Graze,
            "miss" => ActionOutcome::Miss,
            _ => ActionOutcome::Hit,
        }
    }
}

/// Prompt for a free action in combat: the AI interprets the player's text.
pub fn combat_action_prompt(combat: &enemies::ActiveCombat, player_text: &str) -> String {
    let skills: Vec<String> = combat.enemy.combat_skills.iter()
        .map(|s| format!("{} (сила: {}, цель: {})", s.name, s.power,
            if s.target_stat.is_empty() { "HP" } else { &s.target_stat }))
        .collect();

    let stats: Vec<String> = combat.enemy_stats.iter()
        .map(|(k, v)| format!("{}: {}", k, (*v).max(0)))
        .collect();

    let companion_note = if combat.companion_assists {
        format!(
            "\nКомпаньон {} помогает — его навыки тоже учитываются при расчёте.",
            combat.companion_name.as_deref().unwrap_or("спутник")
        )
    } else {
        String::new()
    };

    let mode_desc = match combat.mode {
        enemies::CombatMode::Fatal => "фатальная — до победы или поражения",
        enemies::CombatMode::NonLethal => "бытовая (несмертельная) — враг сдаётся при истощении характеристики",
        enemies::CombatMode::Negotiation => "переговоры — победа через убеждение (Charisma/Persuasion)",
    };

    // The engine counts negotiations and bribery only in negotiation mode: in other
    // modes a model that was offered them would describe the bribed enemy leaving,
    // while the game would continue the fight.
    let negotiation = combat.mode == enemies::CombatMode::Negotiation;
    let negotiation_hint = if !negotiation {
        "\nПереговоры и подкуп в этом бою не действуют — только сила и ловкость."
    } else if combat.enemy.negotiable {
        "\nВраг расположен к переговорам — можно попробовать договориться."
    } else if combat.enemy.hireable {
        "\nВрага можно нанять при хороших отношениях."
    } else {
        ""
    };
    let negotiation_fields = if negotiation {
        ", \"persuasion_level\": N от 1 до 10 (насколько убедительны аргументы игрока), \
         \"bribe_amount\": N (только если игрок явно предлагает конкретную сумму денег)"
    } else {
        ""
    };

    format!(
        "Бой с {enemy}. Раунд {round}.\n\
         HP врага: {ehp}, HP игрока: {php}.\n\
         Режим: {mode_desc}.\n\
         Характеристики врага: {stats}.\n\
         Навыки врага: {skills}.{companion_note}{negotiation_hint}\n\n\
         Действие игрока: «{text}»\n\n\
         Опиши раунд боя ярко и атмосферно (2–3 предложения): что делают бойцы, \
         как выглядит удар/манёвр, как реагирует враг. \
         Определи, НАСКОЛЬКО удачным вышло действие игрока — \
         конкретный урон и исход боя считает игра, не ты.\n\n\
         Верни ТОЛЬКО JSON:\n\
         {{\"description\": \"Кинематографичное описание раунда\", \
         \"outcome\": \"critical|hit|graze|miss\", \
         \"target_stat\": \"Endurance|Strength|Morale\"{negotiation_fields}}}",
        enemy = combat.enemy.name,
        round = combat.round + 1,
        ehp = combat.enemy_hp.max(0),
        php = combat.player_hp.max(0),
        stats = if stats.is_empty() { "нет".to_string() } else { stats.join(", ") },
        skills = skills.join(", "),
        text = player_text.chars().take(400).collect::<String>(),
    )
}

/// Result of a combat action.
#[derive(Debug)]
pub struct CombatActionResult {
    pub description: String,
    pub outcome: ActionOutcome,
    pub target_stat: String,
    pub persuasion_level: u8,
    pub bribe_amount: u32,
}

/// Enemy stat for `apply_skill`. The model writes it however it likes
/// ("HP", "endurance", "Charisma"), while `apply_skill` silently skips an unknown
/// name — the round went to the log with no effect. The mapping is as in the description
/// of domestic combat (`enemies.rs`): Charisma → Morale, Strength → Strength, the rest →
/// Endurance.
fn normalize_target_stat(raw: &str) -> &'static str {
    match raw.trim().to_lowercase().as_str() {
        "strength" | "str" | "сила" => "Strength",
        "morale" | "мораль" | "дух" | "воля" | "charisma" | "харизма" => "Morale",
        _ => "Endurance",
    }
}

/// Effective persuasion in negotiations: the character's ability (skill level
/// `Persuasion`) plus a −1/0/+1 adjustment for argument quality as judged by the
/// model. Until 31.10.2026 only the model's number decided the outcome, so leveling
/// Persuasion did not affect negotiations.
pub fn effective_persuasion(skill: u8, argument: u8) -> u8 {
    let skill = skill.clamp(1, 10) as i16;
    let argument = argument.clamp(1, 10) as i16;
    let bonus = (argument - 5) / 3;
    (skill + bonus).clamp(1, 10) as u8
}

/// Parsing of the model reply to a combat action. `mode` — the active combat mode:
/// the gateway branches on it, not on the model's words.
pub fn parse_combat_action(raw: &str, mode: enemies::CombatMode) -> Option<CombatActionResult> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end <= start {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(&raw[start..=end]).ok()?;
    let description = value.get("description")?.as_str()?.to_string();
    if description.is_empty() {
        return None;
    }
    let outcome = ActionOutcome::parse(value.get("outcome").and_then(|v| v.as_str()).unwrap_or("hit"));
    // For negotiations a missing argument quality means neutral 5: the model's
    // number now only shifts the character's skill by −1/0/+1, and zero
    // would punish the player for the model staying silent.
    let persuasion_default = if mode == enemies::CombatMode::Negotiation { 5 } else { 0 };
    Some(CombatActionResult {
        description,
        outcome,
        target_stat: normalize_target_stat(
            value.get("target_stat").and_then(|v| v.as_str()).unwrap_or_default(),
        )
        .to_string(),
        persuasion_level: value
            .get("persuasion_level")
            .and_then(|v| v.as_u64())
            .unwrap_or(persuasion_default)
            .clamp(0, 10) as u8,
        bribe_amount: value
            .get("bribe_amount")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .min(1_000_000) as u32,
    })
}

/// Apply the parsed player action to combat, then the enemy turn. Damage and outcome
/// are computed by deterministic `ActiveCombat` methods; the model chooses only the luck
/// tier (outcome), the stat, and the negotiation numbers, while the mode comes from
/// the combat itself — the model's `mode` is not asked. Bribery deducts valuables from
/// the inventory (V-05): without payment, "I give 200" cost nothing. The inventory file
/// is touched only on a successful deduction; a call with a store from the cache —
/// `apply_combat_action_with`.
pub fn apply_combat_action(combat: &mut enemies::ActiveCombat, result: &CombatActionResult) {
    if combat.mode == enemies::CombatMode::Negotiation && result.bribe_amount > 0 {
        // Parsing and deduction happen on a copy of the combat inside the update closure:
        // an inventory snapshot read before the calculation overwrote a parallel
        // write (reward, trade) made between load and commit.
        let committed = inventory::update_if_ok(|store| {
            let mut trial = combat.clone();
            apply_combat_action_with(&mut trial, store, result);
            Ok(trial)
        });
        match committed {
            Ok(Ok(trial)) => *combat = trial,
            Ok(Err(e)) | Err(e) => {
                crate::boot_log(&format!("Подкуп: инвентарь не сохранён: {e}"));
            }
        }
    } else {
        let _ = apply_combat_action_with(combat, &mut inventory::InventoryStore::default(), result);
    }
}

/// Pure core of `apply_combat_action` without file IO: the inventory is passed
/// from outside, the caller commits. Returns the bribe outcome if one was attempted.
pub fn apply_combat_action_with(
    combat: &mut enemies::ActiveCombat,
    inventory: &mut inventory::InventoryStore,
    result: &CombatActionResult,
) -> Option<enemies::BribeOutcome> {
    combat.log.push(result.description.clone());
    let mut bribe = None;
    match combat.mode {
        enemies::CombatMode::Negotiation => {
            if result.bribe_amount > 0 {
                bribe = Some(combat.attempt_bribe(result.bribe_amount, inventory));
            } else {
                combat.attempt_negotiation(result.persuasion_level);
            }
        }
        enemies::CombatMode::NonLethal => {
            if result.outcome == ActionOutcome::Miss {
                combat.round += 1;
                combat.log.push(format!("Раунд {}: промах", combat.round));
            } else {
                combat.apply_skill(result.outcome.power(), &result.target_stat);
            }
        }
        enemies::CombatMode::Fatal => {
            if result.outcome == ActionOutcome::Miss {
                combat.round += 1;
                combat.log.push(format!("Раунд {}: промах", combat.round));
            } else {
                combat.player_attack(result.outcome.power());
            }
        }
    }
    // Without a return turn the player's HP is always full, and defeat is unreachable.
    // After the player's victory `enemy_attack` does nothing anyway.
    combat.enemy_attack();
    bribe
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_stores() -> (
        world::World,
        stats::StatsStore,
        skills::SkillsStore,
        quests::QuestStore,
        inventory::InventoryStore,
        npcs::NpcStore,
        RelationshipStore,
    ) {
        (
            world::World::default(),
            stats::StatsStore::new(),
            skills::SkillsStore::new(),
            quests::QuestStore::new(),
            inventory::InventoryStore::default(),
            npcs::NpcStore::new(),
            RelationshipStore::default(),
        )
    }

    #[test]
    fn rpg_state_note_collects_all_blocks() {
        let (world, mut stats_store, skills_store, quests_store, inv, npc_store, rel_store) =
            default_stores();
        stats::init_character(&mut stats_store, "test.png");

        let note = rpg_state_note(
            "test.png",
            "",
            Some(&world),
            &stats_store,
            &skills_store,
            &quests_store,
            &inv,
            &npc_store,
            &rel_store,
            None,
        );
        assert!(note.contains("[RPG]"), "{note}");
        assert!(note.contains("Городская площадь"), "{note}");
        assert!(note.contains("Endurance"), "{note}");
    }

    /// Without a started RPG stats and skills do not go into the prompt: they grow
    /// from ordinary conversation and only get in the way in a roleplay chat.
    #[test]
    fn rpg_state_note_skips_rpg_blocks_until_world_started() {
        let (_, mut stats_store, skills_store, quests_store, inv, npc_store, rel_store) =
            default_stores();
        stats::init_character(&mut stats_store, "test.png");

        let note = rpg_state_note(
            "test.png",
            "",
            None,
            &stats_store,
            &skills_store,
            &quests_store,
            &inv,
            &npc_store,
            &rel_store,
            None,
        );
        assert!(note.is_empty(), "{note}");
    }

    #[test]
    fn rpg_state_note_empty_when_nothing() {
        let (world, stats_store, skills_store, quests_store, inv, npc_store, rel_store) =
            default_stores();

        let note = rpg_state_note(
            "unknown.png",
            "",
            Some(&world),
            &stats_store,
            &skills_store,
            &quests_store,
            &inv,
            &npc_store,
            &rel_store,
            None,
        );
        // The world still exists (the default one), but stats and skills are empty.
        assert!(note.contains("[RPG]"), "{note}");
        assert!(note.contains("Городская площадь"), "{note}");
    }

    #[test]
    fn rpg_state_note_with_active_combat() {
        let (world, stats_store, skills_store, quests_store, inv, npc_store, rel_store) =
            default_stores();
        let enemy = enemies::EnemyTemplate {
            id: "goblin".into(),
            name: "Гоблин".into(),
            hp: 30,
            attack: 10,
            defense: 3,
            tags: vec![],
            skills: vec!["Melee".into()],
            is_boss: false,
            negotiable: false,
            hireable: false,
            combat_skills: vec![enemies::CombatSkill {
                name: "Удар".into(),
                power: 10,
                is_non_lethal: false,
                target_stat: "Endurance".into(),
            }],
        };
        let combat = enemies::ActiveCombat::new(enemy, enemies::CombatMode::Fatal);

        let note = rpg_state_note(
            "test.png",
            "",
            Some(&world),
            &stats_store,
            &skills_store,
            &quests_store,
            &inv,
            &npc_store,
            &rel_store,
            Some(&combat),
        );
        assert!(note.contains("Бой с Гоблин"), "{note}");
        assert!(note.contains("HP врага"), "{note}");
    }

    #[test]
    fn rpg_state_note_with_active_quest() {
        let (world, stats_store, skills_store, mut quests_store, inv, npc_store, rel_store) =
            default_stores();
        quests_store.insert(
            "q1".into(),
            quests::Quest {
                id: "q1".into(),
                title: "Найти артефакт".into(),
                description: "Поиск.".into(),
                stages: vec![quests::QuestStage {
                    description: "Шаг 1".into(),
                    objective: "Поговорить со старостой".into(),
                    completed: false,
                    completion_condition: "done".into(),
                }],
                current_stage: 0,
                status: quests::QuestStatus::Active,
                rewards: quests::QuestRewards::default(),
                giver: None,
                rewards_claimed: false,
            },
        );

        let note = rpg_state_note(
            "test.png",
            "",
            Some(&world),
            &stats_store,
            &skills_store,
            &quests_store,
            &inv,
            &npc_store,
            &rel_store,
            None,
        );
        assert!(note.contains("Найти артефакт"), "{note}");
        assert!(note.contains("Поговорить со старостой"), "{note}");
    }

    #[test]
    fn rpg_state_note_with_npcs_in_location() {
        let (world, stats_store, skills_store, quests_store, inv, mut npc_store, rel_store) =
            default_stores();
        npc_store.insert(
            "blacksmith".into(),
            npcs::Npc {
                id: "blacksmith".into(),
                name: "Кузнец Борис".into(),
                appearance: "мускулистый мужчина".into(),
                personality_tags: vec!["серьёзный".into()],
                current_location: "town_square".into(),
                dialogue_state: npcs::NpcDialogue::Idle,
                relationship_with_player: crate::relationships::RelationshipAxes::default(),
                alive: true,
                shop_inventory: None,
            },
        );

        let note = rpg_state_note(
            "test.png",
            "",
            Some(&world),
            &stats_store,
            &skills_store,
            &quests_store,
            &inv,
            &npc_store,
            &rel_store,
            None,
        );
        assert!(note.contains("Кузнец Борис"), "{note}");
        assert!(note.contains("В локации находятся"), "{note}");
    }

    #[test]
    fn rpg_state_note_with_relationships() {
        let (world, stats_store, skills_store, quests_store, inv, npc_store, mut rel_store) =
            default_stores();
        // The key is the display name: that is how the gateway evaluator writes.
        rel_store.shift(
            "test",
            "Лира",
            &crate::relationships::RelationshipAxes {
                friendship: 50.0,
                romance: 30.0,
                ..Default::default()
            },
        );

        let note = rpg_state_note(
            "test.png",
            "Лира",
            Some(&world),
            &stats_store,
            &skills_store,
            &quests_store,
            &inv,
            &npc_store,
            &rel_store,
            None,
        );
        assert!(note.contains("Отношения к Лира"), "{note}");
        assert!(note.contains("дружба 50"), "{note}");
        assert!(note.contains("романтика 30"), "{note}");
        assert!(note.contains("симпатия"), "{note}");
    }

    #[test]
    fn rpg_state_note_no_relationships_when_default() {
        let (world, stats_store, skills_store, quests_store, inv, npc_store, rel_store) =
            default_stores();

        let note = rpg_state_note(
            "test.png",
            "Лира",
            Some(&world),
            &stats_store,
            &skills_store,
            &quests_store,
            &inv,
            &npc_store,
            &rel_store,
            None,
        );
        // Default relationships are not printed.
        assert!(!note.contains("Отношения"), "{note}");
    }

    #[test]
    fn rpg_state_note_hostile_relationship() {
        let (world, stats_store, skills_store, quests_store, inv, npc_store, mut rel_store) =
            default_stores();
        // The key is the display name: that is how the gateway evaluator writes.
        rel_store.shift(
            "test",
            "Враг",
            &crate::relationships::RelationshipAxes {
                hatred: 70.0,
                fear: 20.0,
                ..Default::default()
            },
        );

        let note = rpg_state_note(
            "test.png",
            "Враг",
            Some(&world),
            &stats_store,
            &skills_store,
            &quests_store,
            &inv,
            &npc_store,
            &rel_store,
            None,
        );
        assert!(note.contains("враг"), "{note}");
        assert!(note.contains("ненависть 70"), "{note}");
    }

    #[test]
    fn relationship_summary_works() {
        use super::relationship_summary;
        let axes = RelationshipAxes { friendship: 70.0, ..Default::default() };
        assert_eq!(relationship_summary(&axes), "друг");

        let axes = RelationshipAxes { romance: 50.0, ..Default::default() };
        assert_eq!(relationship_summary(&axes), "симпатия");

        let axes = RelationshipAxes { hatred: 80.0, ..Default::default() };
        assert_eq!(relationship_summary(&axes), "враг");

        let axes = RelationshipAxes::default();
        assert_eq!(relationship_summary(&axes), "нейтральный");
    }

    #[test]
    fn rpg_state_note_with_shop_in_location() {
        // tavern in the default world: has_shop=true, shop_type="food and drinks"
        let mut world = world::World::default();
        world.current_location = "tavern".into();
        let stats_store = stats::StatsStore::new();
        let skills_store = skills::SkillsStore::new();
        let quests_store = quests::QuestStore::new();
        let inv = inventory::InventoryStore::default();
        let npc_store = npcs::NpcStore::new();
        let rel_store = RelationshipStore::default();

        let note = rpg_state_note(
            "test.png",
            "",
            Some(&world),
            &stats_store,
            &skills_store,
            &quests_store,
            &inv,
            &npc_store,
            &rel_store,
            None,
        );
        assert!(note.contains("Здесь есть магазин: еда и напитки"), "{note}");
    }

    // --- combat_action_prompt ---

    fn make_enemy() -> enemies::EnemyTemplate {
        enemies::EnemyTemplate {
            id: "goblin".into(),
            name: "Гоблин".into(),
            hp: 30,
            attack: 10,
            defense: 3,
            tags: vec![],
            skills: vec!["Melee".into()],
            is_boss: false,
            negotiable: false,
            hireable: false,
            combat_skills: vec![enemies::CombatSkill {
                name: "Удар".into(),
                power: 10,
                is_non_lethal: false,
                target_stat: "Endurance".into(),
            }],
        }
    }

    #[test]
    fn combat_action_prompt_format() {
        let combat = enemies::ActiveCombat::new(make_enemy(), enemies::CombatMode::Fatal);
        let prompt = combat_action_prompt(&combat, "Бью мечом");
        assert!(prompt.contains("Бой с Гоблин"), "{prompt}");
        assert!(prompt.contains("Раунд 1"), "{prompt}");
        assert!(prompt.contains("HP врага: 30"), "{prompt}");
        assert!(prompt.contains("HP игрока: 100"), "{prompt}");
        assert!(prompt.contains("Бью мечом"), "{prompt}");
        assert!(prompt.contains("Удар"), "{prompt}");
        assert!(prompt.contains("Режим:"), "{prompt}");
        assert!(prompt.contains("фатальная"), "{prompt}");
        assert!(prompt.contains("ярко и атмосферно"), "{prompt}");
    }

    #[test]
    fn combat_action_prompt_with_companion() {
        let mut combat = enemies::ActiveCombat::new(make_enemy(), enemies::CombatMode::Fatal);
        combat.companion_assists = true;
        combat.companion_name = Some("Рыцарь".into());
        let prompt = combat_action_prompt(&combat, "Атакуем вместе");
        assert!(prompt.contains("Компаньон Рыцарь помогает"), "{prompt}");
    }

    #[test]
    fn combat_action_prompt_negotiable_enemy() {
        let mut enemy = make_enemy();
        enemy.negotiable = true;
        let combat = enemies::ActiveCombat::new(enemy, enemies::CombatMode::Negotiation);
        let prompt = combat_action_prompt(&combat, "Предлагаю перемирие");
        assert!(prompt.contains("переговоры"), "{prompt}");
        assert!(prompt.contains("расположен к переговорам"), "{prompt}");
    }

    #[test]
    fn combat_action_prompt_offers_bribes_only_in_negotiation() {
        let mut enemy = make_enemy();
        enemy.negotiable = true;
        let fatal = combat_action_prompt(
            &enemies::ActiveCombat::new(enemy.clone(), enemies::CombatMode::Fatal),
            "Бросаю кошель",
        );
        assert!(!fatal.contains("bribe_amount"), "{fatal}");
        assert!(!fatal.contains("расположен к переговорам"), "{fatal}");
        assert!(fatal.contains("не действуют"), "{fatal}");
        let talk = combat_action_prompt(
            &enemies::ActiveCombat::new(enemy, enemies::CombatMode::Negotiation),
            "Бросаю кошель",
        );
        assert!(talk.contains("bribe_amount") && talk.contains("persuasion_level"), "{talk}");
    }

    #[test]
    fn combat_action_prompt_truncates_player_text() {
        let combat = enemies::ActiveCombat::new(make_enemy(), enemies::CombatMode::Fatal);
        let long_text: String = "А".repeat(500);
        let prompt = combat_action_prompt(&combat, &long_text);
        // 400 characters — the limit in the prompt.
        assert!(prompt.contains(&"А".repeat(400)));
        assert!(!prompt.contains(&"А".repeat(401)));
    }

    #[test]
    fn combat_action_prompt_empty_skills() {
        let mut enemy = make_enemy();
        enemy.combat_skills = vec![];
        let combat = enemies::ActiveCombat::new(enemy, enemies::CombatMode::Fatal);
        let prompt = combat_action_prompt(&combat, "Убежать");
        assert!(prompt.contains("Навыки врага: ."), "{prompt}");
    }

    #[test]
    fn combat_action_prompt_empty_target_stat_defaults_to_hp() {
        let mut enemy = make_enemy();
        enemy.combat_skills = vec![enemies::CombatSkill {
            name: "Пинок".into(),
            power: 5,
            is_non_lethal: false,
            target_stat: String::new(),
        }];
        let combat = enemies::ActiveCombat::new(enemy, enemies::CombatMode::Fatal);
        let prompt = combat_action_prompt(&combat, "Атака");
        assert!(prompt.contains("цель: HP"), "{prompt}");
    }

    // --- ActionOutcome ---

    #[test]
    fn action_outcome_power_is_fixed_per_tier() {
        assert_eq!(ActionOutcome::Critical.power(), 24);
        assert_eq!(ActionOutcome::Hit.power(), 14);
        assert_eq!(ActionOutcome::Graze.power(), 6);
        assert_eq!(ActionOutcome::Miss.power(), 0);
    }

    #[test]
    fn action_outcome_unknown_string_defaults_to_hit() {
        assert_eq!(ActionOutcome::parse("critical"), ActionOutcome::Critical);
        assert_eq!(ActionOutcome::parse("graze"), ActionOutcome::Graze);
        assert_eq!(ActionOutcome::parse("miss"), ActionOutcome::Miss);
        assert_eq!(ActionOutcome::parse("что-то странное"), ActionOutcome::Hit);
    }

    #[test]
    fn action_outcome_parse_ignores_case_and_spaces() {
        assert_eq!(ActionOutcome::parse("Miss"), ActionOutcome::Miss);
        assert_eq!(ActionOutcome::parse(" GRAZE "), ActionOutcome::Graze);
        assert_eq!(ActionOutcome::parse("Critical"), ActionOutcome::Critical);
    }

    // --- parse_combat_action ---

    use enemies::CombatMode;

    #[test]
    fn parse_combat_action_happy_path() {
        let raw = r#"{"description": "Гоблин пошатнулся.", "outcome": "hit", "target_stat": "Endurance"}"#;
        let result = parse_combat_action(raw, CombatMode::Fatal).expect("должен распарсить");
        assert_eq!(result.description, "Гоблин пошатнулся.");
        assert_eq!(result.outcome, ActionOutcome::Hit);
        assert_eq!(result.target_stat, "Endurance");
    }

    #[test]
    fn parse_combat_action_returns_none_for_missing_description() {
        let raw = r#"{"outcome": "hit", "target_stat": "Endurance"}"#;
        assert!(parse_combat_action(raw, CombatMode::Fatal).is_none());
    }

    #[test]
    fn parse_combat_action_returns_none_for_empty_description() {
        let raw = r#"{"description": "", "outcome": "miss", "target_stat": "Endurance"}"#;
        assert!(parse_combat_action(raw, CombatMode::Fatal).is_none());
    }

    #[test]
    fn parse_combat_action_defaults_when_type_mismatch() {
        // outcome/target_stat are not strings → defaults (Hit/"Endurance").
        let raw = r#"{"description": "Удар!", "mode": 42, "outcome": 7, "target_stat": 123}"#;
        let result = parse_combat_action(raw, CombatMode::Fatal).expect("должен распарсить с дефолтами");
        assert_eq!(result.outcome, ActionOutcome::Hit);
        assert_eq!(result.target_stat, "Endurance");
    }

    #[test]
    fn parse_combat_action_without_mode_field() {
        let raw = r#"{"description": "Удар!", "outcome": "critical", "target_stat": "Endurance"}"#;
        let result = parse_combat_action(raw, CombatMode::Fatal).expect("mode от модели не нужен");
        assert_eq!(result.outcome, ActionOutcome::Critical);
    }

    #[test]
    fn parse_combat_action_returns_none_for_garbage() {
        assert!(parse_combat_action("нет json тут", CombatMode::Fatal).is_none());
        assert!(parse_combat_action("", CombatMode::Fatal).is_none());
    }

    #[test]
    fn parse_combat_action_extracts_json_from_surrounding_text() {
        let raw = r#"Вот результат: {"description": "Попадание!", "outcome": "hit", "target_stat": "Strength"} — конец."#;
        let result = parse_combat_action(raw, CombatMode::Fatal).expect("должен найти JSON");
        assert_eq!(result.description, "Попадание!");
    }

    /// The model cannot negotiate/bribe with a number beyond the contract —
    /// persuasion_level is clamped to 0..=10, bribe_amount is capped by a reasonable ceiling.
    #[test]
    fn parse_combat_action_clamps_persuasion_and_bribe() {
        let raw = r#"{"description": "Торг.", "outcome": "hit", "persuasion_level": 900, "bribe_amount": 99999999999}"#;
        let result = parse_combat_action(raw, CombatMode::Negotiation).expect("должен распарсить");
        assert_eq!(result.persuasion_level, 10);
        assert_eq!(result.bribe_amount, 1_000_000);
    }

    #[test]
    fn parse_combat_action_defaults_persuasion_and_bribe_to_zero() {
        let raw = r#"{"description": "Удар!", "outcome": "hit"}"#;
        let result = parse_combat_action(raw, CombatMode::Fatal).expect("должен распарсить");
        assert_eq!(result.persuasion_level, 0);
        assert_eq!(result.bribe_amount, 0);
    }

    /// For negotiations a missing argument quality is neutral 5.
    /// The active combat mode decides, not the model's `mode`: the reply "fatal" in
    /// a negotiation fight used to give 0 and eternal failure.
    #[test]
    fn parse_combat_action_negotiation_default_persuasion_follows_stored_mode() {
        let raw = r#"{"description": "Торг.", "outcome": "hit"}"#;
        let result = parse_combat_action(raw, CombatMode::Negotiation).expect("должен распарсить");
        assert_eq!(result.persuasion_level, 5);
        let lying = r#"{"description": "Торг.", "mode": "fatal", "outcome": "hit"}"#;
        assert_eq!(parse_combat_action(lying, CombatMode::Negotiation).unwrap().persuasion_level, 5);
        assert_eq!(parse_combat_action(raw, CombatMode::Fatal).unwrap().persuasion_level, 0);
        // An explicit 0 from the model is kept: parsing does not replace the number, the shift
        // of quality is computed by `effective_persuasion`.
        let raw = r#"{"description": "Торг.", "outcome": "hit", "persuasion_level": 0}"#;
        assert_eq!(parse_combat_action(raw, CombatMode::Negotiation).unwrap().persuasion_level, 0);
    }

    /// The character's skill conducts negotiations, the model moves it by −1/0/+1.
    #[test]
    fn effective_persuasion_lets_skill_decide() {
        // A neutral argument is exactly the skill.
        assert_eq!(effective_persuasion(4, 5), 4);
        // An excellent argument raises a weak speaker a step.
        assert_eq!(effective_persuasion(3, 10), 4);
        // A bad argument lowers a strong one, but not below one.
        assert_eq!(effective_persuasion(10, 1), 9);
        assert_eq!(effective_persuasion(1, 1), 1);
        // A normal enemy's threshold (4) is met with skill 4, a boss's (7) — with skill 7.
        assert!(effective_persuasion(4, 5) >= 4);
        assert!(effective_persuasion(7, 4) >= 7);
        assert!(effective_persuasion(5, 10) < 7);
    }

    /// `apply_skill` silently skips an unknown stat name — parsing
    /// normalizes the model reply to Endurance/Strength/Morale.
    #[test]
    fn parse_combat_action_normalizes_target_stat() {
        let stat = |s: &str| {
            let raw = format!(r#"{{"description": "Удар!", "outcome": "hit", "target_stat": "{s}"}}"#);
            parse_combat_action(&raw, CombatMode::NonLethal).unwrap().target_stat
        };
        assert_eq!(stat("strength"), "Strength");
        assert_eq!(stat("Сила"), "Strength");
        assert_eq!(stat("МОРАЛЬ"), "Morale");
        assert_eq!(stat("Charisma"), "Morale");
        assert_eq!(stat("HP"), "Endurance");
        assert_eq!(stat("выносливость"), "Endurance");
        assert_eq!(stat("что-то"), "Endurance");
    }

    #[test]
    fn combat_action_prompt_does_not_ask_mode() {
        let combat = enemies::ActiveCombat::new(make_enemy(), CombatMode::Fatal);
        let prompt = combat_action_prompt(&combat, "Бью");
        assert!(!prompt.contains("\"mode\""), "{prompt}");
    }

    // --- apply_combat_action ---

    fn action(outcome: ActionOutcome, target_stat: &str) -> CombatActionResult {
        CombatActionResult {
            description: "Описание раунда.".into(),
            outcome,
            target_stat: target_stat.into(),
            persuasion_level: 0,
            bribe_amount: 0,
        }
    }

    /// The enemy responds to every action that did not end the fight: otherwise the player's
    /// HP is always 100 and defeat is unreachable. Round — one per exchange.
    #[test]
    fn apply_combat_action_enemy_strikes_back() {
        let mut combat = enemies::ActiveCombat::new(make_enemy(), CombatMode::Fatal);
        apply_combat_action(&mut combat, &action(ActionOutcome::Graze, "Endurance"));
        assert!(combat.enemy_hp < 30, "игрок не нанёс урон");
        assert!(combat.player_hp < 100, "враг не ответил");
        assert_eq!(combat.round, 1);
        assert_eq!(combat.log[0], "Описание раунда.");
        assert!(combat.log.last().unwrap().contains("враг"), "{:?}", combat.log);
    }

    #[test]
    fn apply_combat_action_miss_still_gets_enemy_turn() {
        let mut combat = enemies::ActiveCombat::new(make_enemy(), CombatMode::Fatal);
        apply_combat_action(&mut combat, &action(ActionOutcome::Miss, "Endurance"));
        assert_eq!(combat.enemy_hp, 30);
        assert!(combat.player_hp < 100);
        assert_eq!(combat.round, 1);
    }

    #[test]
    fn apply_combat_action_no_enemy_turn_after_win() {
        let mut enemy = make_enemy();
        enemy.hp = 5;
        let mut combat = enemies::ActiveCombat::new(enemy, CombatMode::Fatal);
        apply_combat_action(&mut combat, &action(ActionOutcome::Critical, "Endurance"));
        assert!(combat.is_over());
        assert_eq!(combat.player_hp, 100, "побеждённый враг не бьёт");
        assert_eq!(combat.result(), Some(enemies::CombatResult::EnemyDefeated));
    }

    /// Player defeat is reachable: earlier `enemy_attack` was not called at all.
    #[test]
    fn apply_combat_action_player_can_lose() {
        let mut enemy = make_enemy();
        // In domestic combat enemy damage is half: (300 − 5) / 2 > 100.
        enemy.attack = 300;
        let mut combat = enemies::ActiveCombat::new(enemy, CombatMode::NonLethal);
        apply_combat_action(&mut combat, &action(ActionOutcome::Graze, "Strength"));
        assert!(combat.is_over());
        assert_eq!(combat.result(), Some(enemies::CombatResult::NonLethalLoss));
    }

    #[test]
    fn apply_combat_action_non_lethal_hits_target_stat() {
        let mut combat = enemies::ActiveCombat::new(make_enemy(), CombatMode::NonLethal);
        let morale_before = combat.enemy_stats["Morale"];
        apply_combat_action(&mut combat, &action(ActionOutcome::Hit, "Morale"));
        assert!(combat.enemy_stats["Morale"] < morale_before);
        assert_eq!(combat.round, 1);
    }

    #[test]
    fn apply_combat_action_negotiation_uses_persuasion() {
        let mut enemy = make_enemy();
        enemy.negotiable = true;
        let mut combat = enemies::ActiveCombat::new(enemy, CombatMode::Negotiation);
        let mut result = action(ActionOutcome::Hit, "Endurance");
        result.persuasion_level = 8;
        apply_combat_action(&mut combat, &result);
        assert_eq!(combat.result(), Some(enemies::CombatResult::NonLethalWin));
        assert_eq!(combat.player_hp, 100);
    }

    /// V-05 through the dispatcher: bribery in negotiation mode deducts valuables
    /// from the passed inventory (pure call, commit — at the caller).
    #[test]
    fn apply_combat_action_with_charges_bribe() {
        let mut enemy = make_enemy();
        enemy.negotiable = true;
        let mut combat = enemies::ActiveCombat::new(enemy, CombatMode::Negotiation);
        let mut inv = inventory::InventoryStore::default();
        inv.add_to_player(inventory::InventoryItem {
            id: "coin".into(),
            name: "Монета".into(),
            description: String::new(),
            rarity: inventory::Rarity::Common,
            base_price: 10,
            tags: vec![],
            quantity: 20,
        });
        let mut result = action(ActionOutcome::Hit, "Endurance");
        result.bribe_amount = 1_000;
        let outcome = apply_combat_action_with(&mut combat, &mut inv, &result);
        assert!(
            matches!(outcome, Some(enemies::BribeOutcome::Accepted { .. })),
            "подкуп с ценностями проходит: {outcome:?}"
        );
        assert!(combat.is_over());
        assert!(
            inv.player.iter().any(|i| i.quantity < 20),
            "подкуп должен быть оплачен из инвентаря"
        );
    }

    // --- combined_evaluator_prompt ---

    #[test]
    fn combined_evaluator_prompt_format() {
        let state = crate::traits::LiveState {
            affection: 50,
            trust: 30,
            energy: 70,
            irritation: 10,
        };
        let prompt = combined_evaluator_prompt("test.png", "", "Привет", "Здравствуй!", &state);
        assert!(prompt.contains("нежность 50/100"), "{prompt}");
        assert!(prompt.contains("доверие 30/100"), "{prompt}");
        assert!(prompt.contains("энергия 70/100"), "{prompt}");
        assert!(prompt.contains("раздражение 10/100"), "{prompt}");
        assert!(prompt.contains("«Привет»"), "{prompt}");
        assert!(prompt.contains("«Здравствуй!»"), "{prompt}");
        assert!(prompt.contains("эмоциональный тон"), "{prompt}");
        assert!(!prompt.contains("Собеседник"), "{prompt}");
    }

    /// Regression 17.10.2026: the model copied absolute values from the
    /// "Current state" line into the state field (affection: 52 instead of shift +2), and live
    /// state did not move — parsing honestly rejected values outside the contract.
    /// The prompt must explicitly ask for shifts and forbid copying current values.
    #[test]
    fn combined_evaluator_prompt_asks_for_deltas_not_absolute() {
        let state = crate::traits::LiveState {
            affection: 52,
            trust: 50,
            energy: 55,
            irritation: 12,
        };
        let prompt = combined_evaluator_prompt("test.png", "", "Привет", "Здравствуй!", &state);
        assert!(prompt.contains("СДВИГИ"), "{prompt}");
        assert!(prompt.contains("Не копируй"), "{prompt}");
        // The schema contains small deltas, not absolutes of the current state.
        assert!(prompt.contains("\"affection\": 2"), "{prompt}");
        assert!(!prompt.contains("\"affection\": 52"), "{prompt}");
    }

    #[test]
    fn combined_evaluator_prompt_with_persona() {
        let state = crate::traits::LiveState::default();
        let prompt = combined_evaluator_prompt("test.png", "Лира", "Привет", "Здравствуй!", &state);
        assert!(prompt.contains("Собеседник: Лира"), "{prompt}");
    }

    #[test]
    fn combined_evaluator_prompt_truncates_long_text() {
        let state = crate::traits::LiveState::default();
        let long: String = "Б".repeat(700);
        let prompt = combined_evaluator_prompt("test.png", "", &long, "ok", &state);
        // 600 characters — the limit.
        assert!(prompt.contains(&"Б".repeat(600)));
        assert!(!prompt.contains(&"Б".repeat(601)));
    }

    #[test]
    fn combined_evaluator_prompt_zero_state() {
        let state = crate::traits::LiveState {
            affection: 0,
            trust: 0,
            energy: 0,
            irritation: 0,
        };
        let prompt = combined_evaluator_prompt("test.png", "", "", "", &state);
        assert!(prompt.contains("нежность 0/100"), "{prompt}");
    }

    // --- parse_combined_evaluator ---

    /// The model returned the current state instead of shifts: parsing is discarded
    /// entirely, otherwise "hello" shifts state by 40 points.
    #[test]
    fn parse_combined_evaluator_rejects_absolute_state() {
        let raw = r#"{
            "state": {"affection": 50, "trust": 50, "energy": 55, "irritation": 10, "tone": "tenderness"},
            "relationship": {"friendship": 5, "romance": 0, "respect": 0, "fear": 0, "hatred": 0}
        }"#;
        assert!(parse_combined_evaluator(raw).is_none());
    }

    /// Regression 18.10.2026: qwen3.5-9b set a tone outside the enum ("dramatic"), and
    /// strict Tone parsing dropped the whole evaluation with correct shifts — live state
    /// did not move. Tone is an informational label: an unknown word becomes
    /// neutral, numbers stay strict.
    #[test]
    fn parse_combined_evaluator_keeps_deltas_on_unknown_tone() {
        let raw = r#"{
            "state": {"affection": 2, "trust": 0, "energy": -1, "irritation": 1, "tone": "dramatic"},
            "stats": {"Endurance": 3, "Strength": 3},
            "skills": {"Melee": 2, "Stealth": 1}
        }"#;
        let result =
            parse_combined_evaluator(raw).expect("дельты с неизвестным тоном обязаны разбираться");
        let state = result.state_deltas.expect("state не None");
        assert_eq!(state.affection, 2);
        assert_eq!(state.tone, crate::traits::Tone::Neutral);
    }

    #[test]
    fn parse_combined_evaluator_happy_path() {
        let raw = r#"{
            "state": {"affection": 3, "trust": -2, "energy": 0, "irritation": 1, "tone": "playful"},
            "stats": {"Strength": 5, "Dexterity": 0},
            "skills": {"Melee": 3, "Stealth": 0}
        }"#;
        let result = parse_combined_evaluator(raw).expect("должен распарсить");
        let state = result.state_deltas.expect("state не None");
        assert_eq!(state.affection, 3);
        assert_eq!(state.trust, -2);
        assert_eq!(state.irritation, 1);

        let stats = result.stat_deltas.expect("stats не None");
        assert_eq!(stats.get("Strength"), Some(&5));
        assert_eq!(stats.get("Dexterity"), None); // 0 filtered out

        let skills = result.skill_deltas.expect("skills не None");
        assert_eq!(skills.get("Melee"), Some(&3));
        assert_eq!(skills.get("Stealth"), None); // 0 filtered out
    }

    #[test]
    fn parse_combined_evaluator_all_zero_state_returns_none_deltas() {
        let raw = r#"{
            "state": {"affection": 0, "trust": 0, "energy": 0, "irritation": 0, "tone": "neutral"},
            "stats": {},
            "skills": {}
        }"#;
        let result = parse_combined_evaluator(raw).expect("должен распарсить");
        assert!(result.state_deltas.is_none(), "все нули → is_zero → None");
        assert!(result.stat_deltas.is_none());
        assert!(result.skill_deltas.is_none());
    }

    #[test]
    fn parse_combined_evaluator_missing_fields() {
        let raw = r#"{"state": {"affection": 5, "trust": 0, "energy": 0, "irritation": 0, "tone": "neutral"}}"#;
        let result = parse_combined_evaluator(raw).expect("должен распарсить");
        assert!(result.state_deltas.is_some());
        assert!(result.stat_deltas.is_none());
        assert!(result.skill_deltas.is_none());
    }

    #[test]
    fn parse_combined_evaluator_empty_json() {
        let raw = r#"{}"#;
        let result = parse_combined_evaluator(raw).expect("пустой JSON — Some с пустыми полями");
        assert!(result.state_deltas.is_none());
        assert!(result.stat_deltas.is_none());
        assert!(result.skill_deltas.is_none());
    }

    /// The model may return XP far beyond the prompt contract (0–15 for
    /// stats, 0–10 for skills) — the delta is clamped, not passed as is,
    /// otherwise one reply instantly raises the level through add_experience.
    #[test]
    fn parse_combined_evaluator_clamps_stat_and_skill_xp() {
        let raw = r#"{"stats": {"Strength": 900}, "skills": {"Melee": 900}}"#;
        let result = parse_combined_evaluator(raw).expect("должен распарсить");
        assert_eq!(result.stat_deltas.unwrap().get("Strength"), Some(&15));
        assert_eq!(result.skill_deltas.unwrap().get("Melee"), Some(&10));
    }

    /// Value > u32::MAX: clamp to MAX_* BEFORE the cast, otherwise `as u32` wraps.
    #[test]
    fn parse_combined_evaluator_clamps_xp_before_cast() {
        let raw = r#"{"stats": {"Strength": 4294967296}, "skills": {"Melee": 4294967306}}"#;
        let result = parse_combined_evaluator(raw).expect("должен распарсить");
        assert_eq!(result.stat_deltas.unwrap().get("Strength"), Some(&15));
        assert_eq!(result.skill_deltas.unwrap().get("Melee"), Some(&10));
    }

    #[test]
    fn parse_combined_evaluator_returns_none_for_garbage() {
        assert!(parse_combined_evaluator("не json").is_none());
        assert!(parse_combined_evaluator("").is_none());
    }

    /// An invalid `state` type discards parsing entirely: stats/relationship
    /// from the same reply must not be applied (see the fail-closed comment).
    #[test]
    fn parse_combined_evaluator_rejects_invalid_state_type() {
        let raw = r#"{
            "state": "не объект",
            "stats": {"Strength": 5},
            "relationship": {"friendship": 4}
        }"#;
        assert!(parse_combined_evaluator(raw).is_none());
        let raw = r#"{"state": 42, "stats": {"Strength": 5}}"#;
        assert!(parse_combined_evaluator(raw).is_none());
        let raw = r#"{"state": {"affection": "много"}, "skills": {"Melee": 3}}"#;
        assert!(parse_combined_evaluator(raw).is_none());
    }

    #[test]
    fn parse_combined_evaluator_surrounding_text() {
        let raw = "Результат:\n```json\n{\"stats\": {\"Strength\": 4}}\n```\nКонец.";
        let result = parse_combined_evaluator(raw).expect("должен найти JSON");
        let stats = result.stat_deltas.expect("stats");
        assert_eq!(stats.get("Strength"), Some(&4));
    }

    /// While RPG is not started, the world must not be in the prompt: the default
    /// `World::default()` led any scene to the city square with a tavern.
    #[test]
    fn rpg_state_note_without_world_has_no_location() {
        let note = rpg_state_note(
            "test.png",
            "",
            None,
            &stats::StatsStore::default(),
            &skills::SkillsStore::default(),
            &quests::QuestStore::default(),
            &inventory::InventoryStore::default(),
            &npcs::NpcStore::default(),
            &RelationshipStore::default(),
            None,
        );
        assert!(note.is_empty(), "в промпт ушло: {note}");
    }

    /// The evaluator must also return the relationship shift: without this field
    /// relationships.json was not created, and the relationships screen was always empty.
    #[test]
    fn combined_evaluator_parses_relationship_deltas() {
        let raw = r#"{"state": {"affection": 2}, "relationship": {"friendship": 4, "romance": 2, "respect": 0, "fear": 0, "hatred": 0}}"#;
        let eval = parse_combined_evaluator(raw).expect("разбор");
        let rel = eval.relationship_deltas.expect("оси отношений");
        assert_eq!(rel.friendship, 4.0);
        assert_eq!(rel.romance, 2.0);
        assert_eq!(rel.hatred, 0.0);

        let zero = r#"{"relationship": {"friendship": 0, "romance": 0, "respect": 0, "fear": 0, "hatred": 0}}"#;
        assert!(parse_combined_evaluator(zero).unwrap().relationship_deltas.is_none());

        // A number larger than the step is a rewritten absolute, not a shift: the relationship block is discarded entirely.
        let wild = r#"{"relationship": {"friendship": 900, "hatred": "много"}}"#;
        assert!(parse_combined_evaluator(wild).unwrap().relationship_deltas.is_none());
        let absolute = r#"{"relationship": {"friendship": 50, "romance": 2}}"#;
        assert!(parse_combined_evaluator(absolute).unwrap().relationship_deltas.is_none());
    }

    #[test]
    fn combined_evaluator_prompt_asks_for_relationship() {
        let state = crate::traits::LiveState::default();
        let prompt = combined_evaluator_prompt("test.png", "Лира", "Привет", "Здравствуй", &state);
        assert!(prompt.contains("relationship"), "{prompt}");
        assert!(prompt.contains("friendship"), "{prompt}");
    }

    /// The evaluator asks about all default skills and stats: those missing
    /// from the prompt (Investigation, Archery) never grew.
    #[test]
    fn combined_evaluator_prompt_lists_all_default_stats_and_skills() {
        let state = crate::traits::LiveState::default();
        let prompt = combined_evaluator_prompt("test.png", "", "Привет", "Здравствуй", &state);
        for name in stats::DEFAULT_STATS {
            assert!(prompt.contains(&format!("\"{name}\": N")), "нет {name}: {prompt}");
        }
        for (name, _) in skills::DEFAULT_SKILLS {
            assert!(prompt.contains(&format!("\"{name}\": N")), "нет {name}: {prompt}");
        }
    }

    /// A finished fight does not go into the prompt: "Enemy HP: 0" hung in every scene.
    #[test]
    fn rpg_state_note_skips_finished_combat() {
        let (world, stats_store, skills_store, quests_store, inv, npc_store, rel_store) =
            default_stores();
        let mut combat = enemies::ActiveCombat::new(make_enemy(), enemies::CombatMode::Fatal);
        combat.enemy_hp = 0;
        let note = rpg_state_note(
            "test.png", "", Some(&world), &stats_store, &skills_store, &quests_store, &inv,
            &npc_store, &rel_store, Some(&combat),
        );
        assert!(!note.contains("Бой с"), "{note}");
    }

    /// Combat is part of RPG: without a started world it does not get into the prompt, as with
    /// stats and skills.
    #[test]
    fn rpg_state_note_skips_combat_without_world() {
        let (_, stats_store, skills_store, quests_store, inv, npc_store, rel_store) =
            default_stores();
        let combat = enemies::ActiveCombat::new(make_enemy(), enemies::CombatMode::Fatal);
        let note = rpg_state_note(
            "test.png", "", None, &stats_store, &skills_store, &quests_store, &inv, &npc_store,
            &rel_store, Some(&combat),
        );
        assert!(note.is_empty(), "{note}");
    }

    /// Location NPCs are listed once (`npcs_note`), without a duplicate from `world_note`.
    #[test]
    fn rpg_state_note_lists_npc_once() {
        let (world, stats_store, skills_store, quests_store, inv, mut npc_store, rel_store) =
            default_stores();
        npc_store.insert(
            "blacksmith".into(),
            npcs::Npc {
                id: "blacksmith".into(),
                name: "Кузнец Борис".into(),
                appearance: "мускулистый мужчина".into(),
                personality_tags: vec![],
                current_location: "town_square".into(),
                dialogue_state: npcs::NpcDialogue::Idle,
                relationship_with_player: crate::relationships::RelationshipAxes::default(),
                alive: true,
                shop_inventory: None,
            },
        );
        let note = rpg_state_note(
            "test.png", "", Some(&world), &stats_store, &skills_store, &quests_store, &inv,
            &npc_store, &rel_store, None,
        );
        assert_eq!(note.matches("Кузнец Борис").count(), 1, "{note}");
    }
}
