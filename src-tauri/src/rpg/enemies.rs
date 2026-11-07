//! Enemy and combat system: enemy templates with HP/ATK/DEF, tags, and skills.
//! Combat through free-form dialogue — the AI describes the situation, the player writes
//! what they want to do, the AI interprets it and computes the mechanics.
//!
//! In a non-lethal fight, each skill attacks a specific enemy
//! stat: Agility → Endurance, Strength → Strength, Charisma → Morale.
//! When the attacked stat is ≤ 0, the enemy surrenders.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::paths;
use crate::rpg::inventory::{self, InventoryStore};
use crate::rpg::skills::{self, SkillsStore};
use crate::rpg::stats::StatsStore;

/// Enemy template.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnemyTemplate {
    pub id: String,
    pub name: String,
    pub hp: u32,
    pub attack: u32,
    pub defense: u32,
    pub tags: Vec<String>,
    pub skills: Vec<String>,
    pub is_boss: bool,
    /// Whether negotiation is possible.
    pub negotiable: bool,
    /// Whether hiring is possible (with good relations).
    pub hireable: bool,
    /// Skills for non-lethal combat.
    pub combat_skills: Vec<CombatSkill>,
}

/// Skill for combat.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CombatSkill {
    pub name: String,
    pub power: u32,
    pub is_non_lethal: bool,
    /// Which enemy stat it attacks (for NonLethal).
    /// "Endurance", "Strength", "Morale". Empty — attacks HP.
    #[serde(default)]
    pub target_stat: String,
}

/// Active combat.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveCombat {
    pub enemy: EnemyTemplate,
    pub enemy_hp: i32,
    pub player_hp: i32,
    /// The mode is determined by the AI from the player's text, not by buttons.
    pub mode: CombatMode,
    pub round: u32,
    pub log: Vec<String>,
    /// The companion helps (their skills are taken into account).
    pub companion_assists: bool,
    pub companion_name: Option<String>,
    /// Enemy stats for non-lethal combat.
    /// Key: "Endurance", "Strength", "Morale". Value: current value.
    /// When a stat is ≤ 0, the enemy surrenders (for NonLethal).
    #[serde(default)]
    pub enemy_stats: BTreeMap<String, i32>,
    /// Player fighter parameters (V-04): `player_hp = 100` and `player_defense = 10`
    /// were hardcoded, even though the prompt promised to account for companion skills. They are set
    /// by `set_companion` at combat start from the character's stats/skills; defaults —
    /// the previous constants, so combat files from older versions read as before.
    #[serde(default = "default_player_max_hp")]
    pub player_max_hp: i32,
    #[serde(default = "default_player_defense")]
    pub player_defense: i32,
    /// The companion's skill contribution (`skills::combat_bonus`) to damage and defense.
    #[serde(default)]
    pub companion_bonus: u32,
    /// The avatar that started the fight: victory XP goes to it, even if the card
    /// was switched before the last hit.
    #[serde(default)]
    pub owner: Option<String>,
}

/// Combat mode.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CombatMode {
    /// A normal, lethal fight.
    Fatal,
    /// Non-lethal, everyday. Affects relations.
    NonLethal,
    /// Negotiation (Charisma/Persuasion).
    Negotiation,
}

/// Enemy HP ceiling: above it — overflow in `as i32` (display/combat) and in `hp * 2`.
const MAX_ENEMY_HP: u32 = 10_000;

/// Fighter stat ceilings: progression limits them to levels 1–10, but the
/// combat file is edited by hand — clamp on load (like enemy HP).
const MAX_PLAYER_HP: i32 = 1_000;
const MAX_PLAYER_DEFENSE: i32 = 200;
const MAX_COMPANION_BONUS: u32 = 200;

fn default_player_max_hp() -> i32 {
    100
}

fn default_player_defense() -> i32 {
    10
}

fn clamp_hp(hp: u32) -> u32 {
    hp.min(MAX_ENEMY_HP)
}

/// i32-view of HP without overflow: `u32::MAX as i32` yields a negative display.
fn hp_as_i32(hp: u32) -> i32 {
    clamp_hp(hp) as i32
}

/// HP invariant after load: ceiling and non-negative enemy_hp.
fn clamp_combat_hp(combat: &mut ActiveCombat) {
    combat.enemy.hp = clamp_hp(combat.enemy.hp);
    combat.enemy_hp = combat.enemy_hp.clamp(0, MAX_ENEMY_HP as i32);
    combat.player_max_hp = combat.player_max_hp.clamp(1, MAX_PLAYER_HP);
    combat.player_hp = combat.player_hp.clamp(0, combat.player_max_hp);
    combat.player_defense = combat.player_defense.clamp(0, MAX_PLAYER_DEFENSE);
    combat.companion_bonus = combat.companion_bonus.min(MAX_COMPANION_BONUS);
}

/// The player side's combat parameters computed from character progression
/// (V-04). Exact balancing is up to the owner; what matters is that the numbers
/// come from stats/skills, not hardcoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayerPower {
    pub max_hp: i32,
    pub defense: i32,
    /// Total contribution of combat skills (`skills::combat_bonus`) to damage and defense.
    pub skill_bonus: u32,
}

/// Fighter parameters from character stats and skills: HP — from Endurance,
/// defense — from Strength/Dexterity, skills — into damage and defense. Levels are taken
/// clamped to 1–10: the data is edited by hand.
pub fn player_power(
    stats_store: &StatsStore,
    skills_store: &SkillsStore,
    avatar: &str,
) -> PlayerPower {
    let level = |name: &str| -> u32 {
        stats_store
            .get(avatar)
            .and_then(|cs| cs.stats.get(name))
            .map(|entry| u32::from(entry.level.clamp(1, 10)))
            .unwrap_or(1)
    };
    let endurance = level("Endurance");
    let strength = level("Strength");
    let dexterity = level("Dexterity");
    PlayerPower {
        max_hp: (80 + 5 * endurance) as i32,
        defense: (5 + (strength + dexterity) / 2) as i32,
        skill_bonus: skills::combat_bonus(skills_store, avatar, "Strength")
            + skills::combat_bonus(skills_store, avatar, "Dexterity"),
    }
}

impl ActiveCombat {
    /// Create a new combat.
    pub fn new(mut enemy: EnemyTemplate, mode: CombatMode) -> Self {
        enemy.hp = clamp_hp(enemy.hp);
        let player_hp = default_player_max_hp();
        // Enemy stats for non-lethal combat: Endurance = HP, Strength = attack, Morale = hp/2.
        let mut enemy_stats = BTreeMap::new();
        enemy_stats.insert("Endurance".to_string(), enemy.hp as i32);
        enemy_stats.insert("Strength".to_string(), enemy.attack as i32);
        enemy_stats.insert("Morale".to_string(), (enemy.hp / 2) as i32);
        Self {
            enemy_hp: hp_as_i32(enemy.hp),
            enemy,
            player_hp,
            mode,
            round: 0,
            log: Vec::new(),
            companion_assists: false,
            companion_name: None,
            enemy_stats,
            player_max_hp: default_player_max_hp(),
            player_defense: default_player_defense(),
            companion_bonus: 0,
            owner: None,
        }
    }

    /// Attach companion help: their stats and skills enter the combat
    /// calculation (V-04 — the prompt promised it, but the calculation ignored them). Called
    /// at combat start; `player_hp` is set from `max_hp`.
    pub fn set_companion(&mut self, name: Option<String>, power: PlayerPower) {
        self.set_player(power);
        self.companion_assists = true;
        self.companion_name = name;
    }

    /// The player's own parameters: the same V-04 calculations, but without the companion flag.
    /// Combat start passes the active card here — previously it was set as
    /// "companion", and prompts described a helper with the player's name.
    pub fn set_player(&mut self, power: PlayerPower) {
        self.companion_assists = false;
        self.companion_name = None;
        self.player_max_hp = power.max_hp.clamp(1, MAX_PLAYER_HP);
        self.player_hp = self.player_max_hp;
        self.player_defense = power.defense.clamp(0, MAX_PLAYER_DEFENSE);
        self.companion_bonus = power.skill_bonus.min(MAX_COMPANION_BONUS);
    }

    /// Skill attack on an enemy stat — the non-lethal combat
    /// path. Non-lethal combat hits at half force: the rule "NonLethal ⇒ damage ×0.5" used
    /// to live in `player_attack`, which this mode never reaches (V-06) — here it is
    /// reachable and testable. Empty target_stat — attack on HP.
    /// Returns (damage, stat_reached_0 / HP_reached_0).
    pub fn apply_skill(&mut self, skill_power: u32, target_stat: &str) -> (i32, bool) {
        if self.is_over() {
            return (0, false);
        }
        let defense = self.enemy.defense as i32 / 2;
        // Fighter skills enter non-lethal combat too: `player_attack` already counts them,
        // but NonLethal — the only path through `apply_skill` — ignored them.
        let damage = (((skill_power as i32 + self.companion_bonus as i32 - defense).max(1)) as f64
            * 0.5)
            .max(1.0) as i32;

        if target_stat.is_empty() {
            self.enemy_hp -= damage;
            self.round += 1;
            let defeated = self.enemy_hp <= 0;
            self.log.push(format!(
                "Раунд {}: атака -{} (HP: {})",
                self.round, damage, self.enemy_hp.max(0)
            ));
            if defeated {
                self.log.push("Враг сдаётся!".to_string());
            }
            return (damage, defeated);
        }

        let Some(stat) = self.enemy_stats.get_mut(target_stat) else {
            return (0, false);
        };
        *stat -= damage;
        self.round += 1;
        let defeated = *stat <= 0;
        self.log.push(format!(
            "Раунд {}: {} -{} ({}: {})",
            self.round, target_stat, damage, target_stat, (*stat).max(0)
        ));
        if defeated {
            self.enemy_hp = 0;
            self.log.push("Враг сдаётся!".to_string());
        }
        (damage, defeated)
    }

    /// Combat finished?
    pub fn is_over(&self) -> bool {
        self.enemy_hp <= 0 || self.player_hp <= 0
    }

    /// Combat outcome.
    pub fn result(&self) -> Option<CombatResult> {
        if self.enemy_hp <= 0 {
            Some(if self.mode == CombatMode::Fatal {
                CombatResult::EnemyDefeated
            } else {
                CombatResult::NonLethalWin
            })
        } else if self.player_hp <= 0 {
            Some(if self.mode == CombatMode::Fatal {
                CombatResult::PlayerDefeated
            } else {
                CombatResult::NonLethalLoss
            })
        } else {
            None
        }
    }

    /// Player attack (lethal combat path): damage = action strength + companion
    /// skill contribution - enemy.defense/2. The rule "NonLethal ⇒ damage ×0.5" was
    /// removed from here (the branch was unreachable: non-lethal combat goes through `apply_skill`)
    /// and moved there (V-06).
    pub fn player_attack(&mut self, skill_power: u32) -> i32 {
        if self.is_over() {
            return 0;
        }
        let damage =
            (skill_power as i32 + self.companion_bonus as i32 - self.enemy.defense as i32 / 2).max(1);
        self.enemy_hp -= damage;
        self.round += 1;
        self.log.push(format!(
            "Раунд {}: игрок наносит {} урона (HP врага: {})",
            self.round, damage, self.enemy_hp.max(0)
        ));
        damage
    }

    /// Enemy counterattack: damage = enemy.attack - player_defense/2. Defense is
    /// `player_defense` from progression plus half of the companion's skill contribution
    /// (V-04). In non-lethal combat both sides hit at half force — this branch is live:
    /// the enemy attacks in all modes.
    /// Doesn't advance the round: this is the second half of the same exchange as
    /// the player's action.
    pub fn enemy_attack(&mut self) -> i32 {
        if self.is_over() {
            return 0;
        }
        let player_defense = self.player_defense + (self.companion_bonus / 2) as i32;
        let base = (self.enemy.attack as i32 - player_defense / 2).max(1);
        let damage = if self.mode == CombatMode::NonLethal {
            (base as f64 * 0.5).max(1.0) as i32
        } else {
            base
        };
        self.player_hp -= damage;
        self.log.push(format!(
            "Раунд {}: враг наносит {} урона (HP игрока: {})",
            self.round, damage, self.player_hp.max(0)
        ));
        damage
    }

    /// Negotiation attempt: success depends on Charisma/Persuasion.
    /// Returns true if negotiations succeed.
    pub fn attempt_negotiation(&mut self, persuasion_level: u8) -> bool {
        if self.is_over() {
            return false;
        }
        if !self.enemy.negotiable {
            self.round += 1;
            self.log.push(format!(
                "Раунд {}: {} отказывается вести переговоры",
                self.round, self.enemy.name
            ));
            return false;
        }
        let threshold = if self.enemy.is_boss { 7 } else { 4 };
        let success = persuasion_level >= threshold;
        self.round += 1;
        if success {
            self.enemy_hp = 0;
            self.log.push(format!(
                "Раунд {}: переговоры успешны (Persuasion {} >= {})",
                self.round, persuasion_level, threshold
            ));
        } else {
            self.log.push(format!(
                "Раунд {}: переговоры провалены (Persuasion {} < {})",
                self.round, persuasion_level, threshold
            ));
        }
        success
    }

    /// Bribe price: valuables worth `HP × 2` in item evaluation
    /// (`inventory::item_value`).
    pub fn bribe_cost(&self) -> u32 {
        self.enemy.hp.saturating_mul(2)
    }

    /// Honest bribe (V-05): previously the model's number cost nothing — the line
    /// "I give 200" instantly won the negotiations. Now we first check the
    /// offer against the enemy's price, then deduct valuables from the inventory
    /// (`inventory::pay_value`, evaluation via `inventory::item_value`), and only
    /// on successful deduction does the enemy surrender; the deducted items go to the one who was
    /// bribed (`give_to_npc`). On any rejection the inventory is untouched —
    /// the "commit only on success" contract.
    pub fn attempt_bribe(&mut self, offered: u32, inventory: &mut InventoryStore) -> BribeOutcome {
        if self.is_over() {
            return BribeOutcome::Refused;
        }
        if !self.enemy.negotiable {
            self.round += 1;
            self.log.push(format!(
                "Раунд {}: {} не поддаётся на подкуп",
                self.round, self.enemy.name
            ));
            return BribeOutcome::Refused;
        }
        let cost = self.bribe_cost();
        if offered < cost {
            self.round += 1;
            self.log.push(format!(
                "Раунд {}: подкуп провален — предложение {offered} ниже цены {cost}",
                self.round
            ));
            return BribeOutcome::BelowPrice { cost };
        }
        let Some(paid) = inventory::pay_value(inventory, cost) else {
            self.round += 1;
            self.log.push(format!(
                "Раунд {}: подкуп провален — нет ценностей на {cost}",
                self.round
            ));
            return BribeOutcome::InsufficientFunds { cost };
        };
        let value = paid
            .iter()
            // pay_value takes only from finite stacks — quantity > 0.
            .map(|item| inventory::item_value(item).saturating_mul(item.quantity.max(1) as u32))
            .fold(0u32, u32::saturating_add);
        for item in paid {
            inventory.give_to_npc(&self.enemy.id, item);
        }
        self.enemy_hp = 0;
        self.round += 1;
        self.log.push(format!(
            "Раунд {}: подкуп принят (ценности на {value}, цена {cost})",
            self.round
        ));
        BribeOutcome::Accepted { value }
    }
}

/// Bribe outcome (V-05).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BribeOutcome {
    /// The enemy accepted the valuables and surrendered: deducted `value`.
    Accepted { value: u32 },
    /// Offer below the enemy's price (`bribe_cost`). Nothing deducted.
    BelowPrice { cost: u32 },
    /// The player has no valuables for the required amount. Nothing deducted.
    InsufficientFunds { cost: u32 },
    /// The enemy doesn't take bribes or the combat is already over. Nothing deducted.
    Refused,
}

/// Combat outcome.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CombatResult {
    /// Enemy defeated (lethal combat).
    EnemyDefeated,
    /// Player defeated (lethal combat).
    PlayerDefeated,
    /// Player won the non-lethal combat (non-lethally).
    NonLethalWin,
    /// Player lost the non-lethal combat.
    NonLethalLoss,
}

impl CombatResult {
    /// Key for gateway responses: the panel compares snake_case strings.
    pub fn key(self) -> &'static str {
        match self {
            CombatResult::EnemyDefeated => "enemy_defeated",
            CombatResult::PlayerDefeated => "player_defeated",
            CombatResult::NonLethalWin => "non_lethal_win",
            CombatResult::NonLethalLoss => "non_lethal_loss",
        }
    }
}

pub type EnemyStore = BTreeMap<String, EnemyTemplate>;

/// Default enemies.
pub fn default_enemies() -> EnemyStore {
    let mut store = BTreeMap::new();
    store.insert(
        "bandit".into(),
        EnemyTemplate {
            id: "bandit".into(),
            name: "Бандит".into(),
            hp: 40,
            attack: 12,
            defense: 5,
            tags: vec!["бандит".into(), "человек".into()],
            skills: vec!["Melee".into()],
            is_boss: false,
            negotiable: true,
            hireable: true,
            combat_skills: vec![
                CombatSkill { name: "Драка".into(), power: 12, is_non_lethal: true, target_stat: "Strength".into() },
                CombatSkill { name: "Запугивание".into(), power: 8, is_non_lethal: true, target_stat: "Morale".into() },
            ],
        },
    );
    store.insert(
        "wolf".into(),
        EnemyTemplate {
            id: "wolf".into(),
            name: "Волк".into(),
            hp: 30,
            attack: 15,
            defense: 3,
            tags: vec!["зверь".into(), "волк".into()],
            skills: vec!["Melee".into()],
            is_boss: false,
            negotiable: false,
            hireable: false,
            combat_skills: vec![
                CombatSkill { name: "Укус".into(), power: 15, is_non_lethal: false, target_stat: "Endurance".into() },
            ],
        },
    );
    store.insert(
        "bandit_chief".into(),
        EnemyTemplate {
            id: "bandit_chief".into(),
            name: "Главарь бандитов".into(),
            hp: 80,
            attack: 18,
            defense: 10,
            tags: vec!["бандит".into(), "босс".into(), "человек".into()],
            skills: vec!["Melee".into(), "Stealth".into()],
            is_boss: true,
            negotiable: true,
            hireable: false,
            combat_skills: vec![
                CombatSkill { name: "Драка".into(), power: 18, is_non_lethal: true, target_stat: "Strength".into() },
                CombatSkill { name: "Запугивание".into(), power: 15, is_non_lethal: true, target_stat: "Morale".into() },
                CombatSkill { name: "Торговля".into(), power: 10, is_non_lethal: true, target_stat: "Morale".into() },
            ],
        },
    );
    store
}

// -- Persistence of active combat (a separate file, like other RPG modules) --

fn combat_store_path() -> PathBuf {
    paths::base_dir().join("data").join("active_combat.json")
}

static COMBAT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Load active combat (None if there is none). Via `json_store`: a corrupted file is
/// saved as a `.json.corrupt` copy with a boot.log entry, not silently lost.
pub fn load_combat() -> Option<ActiveCombat> {
    load_combat_at(&combat_store_path())
}

fn load_combat_at(path: &Path) -> Option<ActiveCombat> {
    let Some(mut combat) = crate::json_store::load::<Option<ActiveCombat>>(&COMBAT_LOCK, path) else {
        // Remove only an explicit `null` (that's how `save_combat(None)` closes combat).
        // An unreadable file is a lock, non-UTF-8 — leave it on disk: V-09,
        // unread data must not be lost (the corrupted one is already copied to .corrupt).
        let explicit_none = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .is_some_and(|value| value.is_null());
        // Broken JSON is already in a `.json.corrupt` copy — remove the original.
        // A read error (lock, non-UTF-8) produces no copy: keep the file.
        if explicit_none || path.with_extension("json.corrupt").exists() {
            let _ = crate::json_store::remove(&COMBAT_LOCK, path);
        }
        return None;
    };
    clamp_combat_hp(&mut combat);
    Some(combat)
}

/// Save or clear active combat.
pub fn save_combat(combat: Option<&ActiveCombat>) -> Result<(), String> {
    save_combat_at(&combat_store_path(), combat)
}

fn save_combat_at(path: &Path, combat: Option<&ActiveCombat>) -> Result<(), String> {
    match combat {
        Some(c) => crate::json_store::update(&COMBAT_LOCK, path, |slot: &mut Option<ActiveCombat>| {
            *slot = Some(c.clone());
        }),
        None => crate::json_store::remove(&COMBAT_LOCK, path),
    }
}

/// Combat prompt: describes the state and asks the AI to describe the next round.
/// The player acts through free text, the AI interprets.
pub fn combat_prompt(combat: &ActiveCombat) -> String {
    let skills_list = combat
        .enemy
        .combat_skills
        .iter()
        .map(|s| format!("{} (сила: {}, атакует: {})", s.name, s.power,
            if s.target_stat.is_empty() { "HP" } else { &s.target_stat }))
        .collect::<Vec<_>>()
        .join(", ");

    let companion_note = if combat.companion_assists {
        format!(
            "\nКомпаньон {} помогает — его навыки тоже учитываются.",
            combat.companion_name.as_deref().unwrap_or("спутник")
        )
    } else {
        String::new()
    };

    let stats_note = if !combat.enemy_stats.is_empty() {
        let stats: Vec<String> = combat.enemy_stats.iter()
            .map(|(k, v)| format!("{}: {}", k, (*v).max(0)))
            .collect();
        format!("\nХарактеристики врага: {}.", stats.join(", "))
    } else {
        String::new()
    };

    let tags_note = if !combat.enemy.tags.is_empty() {
        format!("\nТип врага: {}.", combat.enemy.tags.join(", "))
    } else {
        String::new()
    };

    // The engine counts bribes and persuasion only in negotiation mode.
    let negotiate_note = match (combat.mode, combat.enemy.negotiable) {
        (CombatMode::Negotiation, true) => "\nВраг открыт к переговорам и подкупу.",
        (CombatMode::Negotiation, false) => "\nВраг не поддаётся на переговоры.",
        _ => "\nВ этом бою переговоры и подкуп не действуют.",
    };

    format!(
        "Бой с {}.{tags_note}\n\
         HP врага: {}, HP игрока: {}.\n\
         Режим: {}.{stats_note}\n\
         Навыки врага: {}.{negotiate_note}\n\
         Раунд: {}.{companion_note}\n\n\
         Опиши обстановку и атмосферу боя, внешность и поведение врага. \
         Ходы боя игрок делает в RPG-панели, а урон и исход раундов считает игра: \
         HP выше — актуальные, не меняй их и не объявляй победу или поражение сам. \
         Здесь только атмосфера и реакции, вариантов действий не предлагай.",
        combat.enemy.name,
        combat.enemy_hp.max(0),
        combat.player_hp.max(0),
        match combat.mode {
            CombatMode::Fatal => "фатальная",
            CombatMode::NonLethal => "бытовая (несмертельная)",
            CombatMode::Negotiation => "переговоры",
        },
        skills_list,
        combat.round + 1,
    )
}

/// Combat reward (XP for distribution across stats).
pub fn combat_reward(combat: &ActiveCombat) -> u32 {
    match combat.mode {
        CombatMode::Fatal => combat.enemy.hp,
        CombatMode::NonLethal => combat.enemy.hp / 2,
        CombatMode::Negotiation => combat.enemy.hp / 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bandit() -> EnemyTemplate {
        default_enemies()["bandit"].clone()
    }

    /// The active card is the player, not the companion: the prompt must not
    /// describe a helper with the player's name.
    #[test]
    fn set_player_does_not_claim_a_companion() {
        let mut combat = ActiveCombat::new(bandit(), CombatMode::Fatal);
        combat.set_player(PlayerPower {
            max_hp: 100,
            defense: 10,
            skill_bonus: 0,
        });
        assert!(!combat.companion_assists);
        assert!(combat.companion_name.is_none());
        assert_eq!(combat.player_hp, 100);
    }

    #[test]
    fn new_combat() {
        let combat = ActiveCombat::new(bandit(), CombatMode::Fatal);
        assert_eq!(combat.enemy_hp, 40);
        assert_eq!(combat.player_hp, 100);
        assert!(!combat.is_over());
    }

    #[test]
    fn player_attack_deals_damage() {
        let mut combat = ActiveCombat::new(bandit(), CombatMode::Fatal);
        let damage = combat.player_attack(15);
        assert!(damage > 0);
        assert!(combat.enemy_hp < 40);
        assert_eq!(combat.round, 1);
    }

    #[test]
    fn enemy_attack_deals_damage() {
        let mut combat = ActiveCombat::new(bandit(), CombatMode::Fatal);
        combat.enemy_attack();
        assert!(combat.player_hp < 100);
    }

    #[test]
    fn combat_ends_when_enemy_hp_zero() {
        let mut combat = ActiveCombat::new(bandit(), CombatMode::Fatal);
        // Deal damage until the enemy falls
        while !combat.is_over() {
            combat.player_attack(100);
        }
        assert_eq!(combat.result(), Some(CombatResult::EnemyDefeated));
    }

    #[test]
    fn combat_ends_when_player_hp_zero() {
        let mut enemy = bandit();
        enemy.attack = 200;
        let mut combat = ActiveCombat::new(enemy, CombatMode::Fatal);
        combat.enemy_attack();
        assert!(combat.is_over());
        assert_eq!(combat.result(), Some(CombatResult::PlayerDefeated));
    }

    #[test]
    fn negotiation_success() {
        let mut combat = ActiveCombat::new(bandit(), CombatMode::Negotiation);
        assert!(combat.attempt_negotiation(7));
        assert!(combat.is_over());
        assert_eq!(combat.result(), Some(CombatResult::NonLethalWin));
    }

    #[test]
    fn negotiation_failure() {
        let mut combat = ActiveCombat::new(bandit(), CombatMode::Negotiation);
        assert!(!combat.attempt_negotiation(2));
        assert!(!combat.is_over());
    }

    #[test]
    fn boss_negotiation_requires_higher() {
        let chief = default_enemies()["bandit_chief"].clone();
        let mut combat = ActiveCombat::new(chief, CombatMode::Negotiation);
        assert!(!combat.attempt_negotiation(5)); // < 7 for the boss
        assert!(combat.attempt_negotiation(7));
    }

    /// Wallet for bribe tests: finite coins with price `base_price`.
    fn bribe_wallet(quantity: i32, base_price: u32) -> InventoryStore {
        let mut inv = InventoryStore::default();
        inv.add_to_player(inventory::InventoryItem {
            id: "coin".into(),
            name: "Монета".into(),
            description: String::new(),
            rarity: inventory::Rarity::Common,
            base_price,
            tags: vec![],
            quantity,
        });
        inv
    }

    /// V-05: a bribe costs valuables from the inventory; deduction only on
    /// successful outcome ("commit only on success").
    #[test]
    fn bribe_success_charges_valuables() {
        let mut combat = ActiveCombat::new(bandit(), CombatMode::Negotiation);
        let mut inv = bribe_wallet(10, 10); // 10 coins at 10 = 100 in evaluation
        // bandit.hp = 40 → bribe price 80.
        assert_eq!(combat.bribe_cost(), 80);
        assert_eq!(
            combat.attempt_bribe(100, &mut inv),
            BribeOutcome::Accepted { value: 80 }
        );
        assert!(combat.is_over());
        assert_eq!(combat.result(), Some(CombatResult::NonLethalWin));
        // Deducted exactly the price: 8 coins, two left with the player.
        assert_eq!(inv.player.iter().map(|i| i.quantity).sum::<i32>(), 2);
        // The valuables went to the one who was bribed.
        assert_eq!(inv.npcs["bandit"].iter().map(|i| i.quantity).sum::<i32>(), 8);
    }

    #[test]
    fn bribe_below_price_is_refused_without_charge() {
        let mut combat = ActiveCombat::new(bandit(), CombatMode::Negotiation);
        let mut inv = bribe_wallet(100, 10);
        assert_eq!(
            combat.attempt_bribe(50, &mut inv),
            BribeOutcome::BelowPrice { cost: 80 }
        );
        assert!(!combat.is_over());
        assert_eq!(inv.player[0].quantity, 100, "отказ не должен ничего списывать");
    }

    #[test]
    fn bribe_without_valuables_is_refused() {
        let mut combat = ActiveCombat::new(bandit(), CombatMode::Negotiation);
        let mut inv = InventoryStore::default();
        assert_eq!(
            combat.attempt_bribe(1_000, &mut inv),
            BribeOutcome::InsufficientFunds { cost: 80 }
        );
        assert!(!combat.is_over(), "без оплаты подкуп не выигрывает переговоры");
    }

    #[test]
    fn nonlethal_result() {
        let mut combat = ActiveCombat::new(bandit(), CombatMode::NonLethal);
        while !combat.is_over() {
            combat.player_attack(100);
        }
        assert_eq!(combat.result(), Some(CombatResult::NonLethalWin));
    }

    #[test]
    fn combat_log_grows() {
        let mut combat = ActiveCombat::new(bandit(), CombatMode::Fatal);
        combat.player_attack(10);
        combat.enemy_attack();
        assert_eq!(combat.log.len(), 2);
    }

    /// Enemy response is the same half of the exchange: the round grows by 1 per exchange, not 2.
    #[test]
    fn enemy_attack_keeps_round() {
        let mut combat = ActiveCombat::new(bandit(), CombatMode::Fatal);
        combat.player_attack(10);
        combat.enemy_attack();
        assert_eq!(combat.round, 1);
        assert!(combat.log[1].starts_with("Раунд 1: враг"), "{:?}", combat.log);
    }

    #[test]
    fn combat_result_keys_are_snake_case() {
        assert_eq!(CombatResult::EnemyDefeated.key(), "enemy_defeated");
        assert_eq!(CombatResult::PlayerDefeated.key(), "player_defeated");
        assert_eq!(CombatResult::NonLethalWin.key(), "non_lethal_win");
        assert_eq!(CombatResult::NonLethalLoss.key(), "non_lethal_loss");
    }

    /// Fighter skills are counted in non-lethal combat too: previously `apply_skill` was
    /// the only NonLethal path and ignored `companion_bonus`.
    #[test]
    fn non_lethal_damage_includes_skill_bonus() {
        let mut without = ActiveCombat::new(bandit(), CombatMode::NonLethal);
        let (base, _) = without.apply_skill(12, "Morale");
        let mut with = ActiveCombat::new(bandit(), CombatMode::NonLethal);
        with.set_player(PlayerPower {
            max_hp: 100,
            defense: 10,
            skill_bonus: 8,
        });
        let (boosted, _) = with.apply_skill(12, "Morale");
        assert!(boosted > base, "бонус навыков не учтён: {base} -> {boosted}");
    }

    fn temp_combat_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("cloufam_test_combat");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("json.corrupt"));
        path
    }

    #[test]
    fn combat_save_load_clear_roundtrip() {
        let path = temp_combat_path("roundtrip.json");
        assert!(load_combat_at(&path).is_none());
        let mut combat = ActiveCombat::new(bandit(), CombatMode::NonLethal);
        combat.player_attack(20);
        save_combat_at(&path, Some(&combat)).unwrap();
        let loaded = load_combat_at(&path).expect("бой сохранён");
        assert_eq!(loaded.enemy_hp, combat.enemy_hp);
        assert_eq!(loaded.mode, CombatMode::NonLethal);
        assert_eq!(loaded.log, combat.log);
        save_combat_at(&path, None).unwrap();
        assert!(!path.exists(), "Завершить должен убирать файл боя");
        assert!(load_combat_at(&path).is_none());
    }

    /// A corrupted combat file is not "no combat" silently: the copy remains for analysis.
    #[test]
    fn corrupt_combat_file_is_backed_up() {
        let path = temp_combat_path("corrupt.json");
        std::fs::write(&path, "{ не json").unwrap();
        assert!(load_combat_at(&path).is_none());
        assert!(path.with_extension("json.corrupt").exists());
        assert!(!path.exists(), "битый файл убран, копия осталась");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("json.corrupt"));
    }

    #[test]
    fn combat_prompt_format() {
        let combat = ActiveCombat::new(bandit(), CombatMode::Fatal);
        let prompt = combat_prompt(&combat);
        assert!(prompt.contains("Бандит"), "{prompt}");
        assert!(prompt.contains("HP врага: 40"), "{prompt}");
        assert!(prompt.contains("фатальная"), "{prompt}");
    }

    #[test]
    fn bribe_exact_amount_with_goods_wins() {
        let mut combat = ActiveCombat::new(bandit(), CombatMode::Negotiation);
        let mut inv = bribe_wallet(8, 10); // exactly 80 in evaluation
        assert_eq!(
            combat.attempt_bribe(80, &mut inv),
            BribeOutcome::Accepted { value: 80 }
        );
        assert!(combat.is_over());
        assert!(inv.player.is_empty(), "заплатил всем, что есть");
    }

    #[test]
    fn player_attack_after_combat_over() {
        let mut combat = ActiveCombat::new(bandit(), CombatMode::Fatal);
        while !combat.is_over() {
            combat.player_attack(100);
        }
        let round_before = combat.round;
        let damage = combat.player_attack(50);
        assert_eq!(damage, 0);
        assert_eq!(combat.round, round_before);
    }

    #[test]
    fn enemy_attack_after_combat_over() {
        let mut enemy = bandit();
        enemy.attack = 200;
        let mut combat = ActiveCombat::new(enemy, CombatMode::Fatal);
        combat.enemy_attack(); // player dies
        assert!(combat.is_over());
        let log_before = combat.log.len();
        let damage = combat.enemy_attack();
        assert_eq!(damage, 0);
        assert_eq!(combat.log.len(), log_before);
    }

    #[test]
    fn negotiation_after_combat_over() {
        let mut combat = ActiveCombat::new(bandit(), CombatMode::NonLethal);
        while !combat.is_over() {
            combat.player_attack(100);
        }
        assert!(!combat.attempt_negotiation(10));
    }

    #[test]
    fn companion_prompt() {
        let mut combat = ActiveCombat::new(bandit(), CombatMode::Fatal);
        combat.companion_assists = true;
        combat.companion_name = Some("Лира".into());
        let prompt = combat_prompt(&combat);
        assert!(prompt.contains("Лира"), "{prompt}");
        assert!(prompt.contains("помогает"), "{prompt}");
    }

    /// V-06: the "NonLethal ⇒ damage ×0.5" branch lived in `player_attack`, which
    /// non-lethal combat never reaches (its path is `apply_skill`). The rule was moved
    /// there, and the test measures real paths: a non-lethal attack hits at half
    /// the lethal one's force, all else equal.
    #[test]
    fn nonlethal_damage_less_than_fatal() {
        let enemy = bandit();

        let mut fatal = ActiveCombat::new(enemy.clone(), CombatMode::Fatal);
        let fatal_dmg = fatal.player_attack(20);

        let mut nonlethal = ActiveCombat::new(enemy, CombatMode::NonLethal);
        let (nonlethal_dmg, _) = nonlethal.apply_skill(20, "Endurance");

        assert!(nonlethal_dmg < fatal_dmg, "nonlethal={nonlethal_dmg} fatal={fatal_dmg}");
        // bandit.defense = 5 → base 18: lethal 18, non-lethal — half.
        assert_eq!(fatal_dmg, 18, "фатальная атака не должна резаться: {fatal_dmg}");
        assert_eq!(nonlethal_dmg, 9, "бытовая атака — половина урона: {nonlethal_dmg}");
    }

    #[test]
    fn nonlethal_enemy_damage_less_than_fatal() {
        let enemy = bandit();

        let mut fatal = ActiveCombat::new(enemy.clone(), CombatMode::Fatal);
        let fatal_dmg = fatal.enemy_attack();

        let mut nonlethal = ActiveCombat::new(enemy, CombatMode::NonLethal);
        let nonlethal_dmg = nonlethal.enemy_attack();

        assert!(nonlethal_dmg < fatal_dmg, "nonlethal={nonlethal_dmg} fatal={fatal_dmg}");
    }

    #[test]
    fn default_enemies_has_all_three() {
        let store = default_enemies();
        assert!(store.contains_key("bandit"), "missing bandit");
        assert!(store.contains_key("wolf"), "missing wolf");
        assert!(store.contains_key("bandit_chief"), "missing bandit_chief");
        assert_eq!(store.len(), 3);
    }

    #[test]
    fn enemy_store_roundtrip() {
        let store = default_enemies();
        let json = serde_json::to_string_pretty(&store).unwrap();
        let loaded: EnemyStore = serde_json::from_str(&json).unwrap();
        assert_eq!(store.len(), loaded.len());
        for (id, template) in &store {
            let t = &loaded[id];
            assert_eq!(t.id, template.id);
            assert_eq!(t.name, template.name);
            assert_eq!(t.hp, template.hp);
            assert_eq!(t.attack, template.attack);
            assert_eq!(t.defense, template.defense);
        }
    }

    #[test]
    fn companion_default_off() {
        let combat = ActiveCombat::new(bandit(), CombatMode::Fatal);
        assert!(!combat.companion_assists);
        assert!(combat.companion_name.is_none());
    }

    #[test]
    fn apply_skill_reduces_stat() {
        let mut combat = ActiveCombat::new(bandit(), CombatMode::NonLethal);
        let (damage, defeated) = combat.apply_skill(15, "Endurance");
        assert!(damage > 0);
        assert!(!defeated);
        assert_eq!(combat.round, 1);
        let endurance = combat.enemy_stats["Endurance"];
        assert!(endurance < 40, "Endurance={endurance}");
    }

    #[test]
    fn apply_skill_defeats_when_stat_zero() {
        let mut combat = ActiveCombat::new(bandit(), CombatMode::NonLethal);
        // bandit Endurance = 40, we deal big damage
        let (_, defeated) = combat.apply_skill(100, "Endurance");
        assert!(defeated);
        assert!(combat.is_over());
    }

    #[test]
    fn apply_skill_morale_target() {
        let mut combat = ActiveCombat::new(bandit(), CombatMode::NonLethal);
        let (damage, _) = combat.apply_skill(10, "Morale");
        assert!(damage > 0);
        assert!(combat.enemy_stats["Morale"] < 20);
    }

    #[test]
    fn apply_skill_after_over() {
        let mut combat = ActiveCombat::new(bandit(), CombatMode::NonLethal);
        combat.enemy_hp = 0;
        let (damage, defeated) = combat.apply_skill(10, "Endurance");
        assert_eq!(damage, 0);
        assert!(!defeated);
    }

    #[test]
    fn enemy_stats_initialized() {
        let combat = ActiveCombat::new(bandit(), CombatMode::NonLethal);
        assert_eq!(combat.enemy_stats["Endurance"], 40);
        assert_eq!(combat.enemy_stats["Strength"], 12);
        assert_eq!(combat.enemy_stats["Morale"], 20);
    }

    #[test]
    fn combat_reward_fatal() {
        let combat = ActiveCombat::new(bandit(), CombatMode::Fatal);
        assert_eq!(combat_reward(&combat), 40);
    }

    #[test]
    fn combat_reward_nonlethal() {
        let combat = ActiveCombat::new(bandit(), CombatMode::NonLethal);
        assert_eq!(combat_reward(&combat), 20);
    }

    #[test]
    fn combat_reward_negotiation() {
        let combat = ActiveCombat::new(bandit(), CombatMode::Negotiation);
        assert_eq!(combat_reward(&combat), 10);
    }

    #[test]
    fn combat_prompt_contains_stats() {
        let combat = ActiveCombat::new(bandit(), CombatMode::NonLethal);
        let prompt = combat_prompt(&combat);
        assert!(prompt.contains("Endurance"), "{prompt}");
        assert!(prompt.contains("Strength"), "{prompt}");
        assert!(prompt.contains("Morale"), "{prompt}");
    }

    #[test]
    fn combat_prompt_no_buttons() {
        let combat = ActiveCombat::new(bandit(), CombatMode::Fatal);
        let prompt = combat_prompt(&combat);
        assert!(!prompt.contains("Атаковать"), "не должно быть кнопок: {prompt}");
        assert!(!prompt.contains("Переговорить"), "не должно быть кнопок: {prompt}");
        assert!(prompt.contains("RPG-панели"), "ходы боя — в панели: {prompt}");
    }

    /// Damage is computed by the gateway: the role-play model in chat must not keep its own HP count.
    #[test]
    fn combat_prompt_leaves_math_to_game() {
        let prompt = combat_prompt(&ActiveCombat::new(bandit(), CombatMode::Fatal));
        assert!(prompt.contains("считает игра"), "{prompt}");
        assert!(!prompt.contains("рассчитывай"), "{prompt}");
    }

    #[test]
    fn target_stat_in_combat_skill() {
        let enemy = bandit();
        let skill = &enemy.combat_skills[0];
        assert_eq!(skill.target_stat, "Strength");
        assert!(skill.is_non_lethal);
    }

    #[test]
    fn attempt_bribe_after_combat_over() {
        let mut combat = ActiveCombat::new(bandit(), CombatMode::NonLethal);
        while !combat.is_over() {
            combat.player_attack(100);
        }
        let mut inv = bribe_wallet(100, 10);
        assert_eq!(combat.attempt_bribe(1000, &mut inv), BribeOutcome::Refused);
        assert_eq!(inv.player[0].quantity, 100);
    }

    #[test]
    fn apply_skill_unknown_stat_no_crash() {
        let mut combat = ActiveCombat::new(bandit(), CombatMode::NonLethal);
        let (damage, defeated) = combat.apply_skill(100, "UnknownStat");
        assert_eq!(damage, 0);
        assert!(!defeated);
        assert_eq!(combat.round, 0);
    }

    #[test]
    fn apply_skill_empty_target_attacks_hp() {
        let mut combat = ActiveCombat::new(bandit(), CombatMode::NonLethal);
        let skill = CombatSkill {
            name: "Удар".into(),
            power: 15,
            is_non_lethal: true,
            target_stat: String::new(),
        };
        let (damage, defeated) = combat.apply_skill(skill.power, &skill.target_stat);
        assert!(damage > 0, "урон должен быть > 0");
        assert!(!defeated);
        assert!(combat.enemy_hp < 40, "HP должен уменьшиться: {}", combat.enemy_hp);
        assert_eq!(combat.round, 1);
    }

    #[test]
    fn apply_skill_empty_target_can_defeat() {
        let mut combat = ActiveCombat::new(bandit(), CombatMode::NonLethal);
        let (_, defeated) = combat.apply_skill(100, "");
        assert!(defeated);
        assert!(combat.is_over());
    }

    #[test]
    fn negotiation_rejected_for_non_negotiable() {
        let mut wolf = bandit();
        wolf.negotiable = false;
        let mut combat = ActiveCombat::new(wolf, CombatMode::Negotiation);
        assert!(!combat.attempt_negotiation(10));
        assert!(!combat.is_over());
        assert!(combat.log.last().unwrap().contains("отказывается"));
    }

    #[test]
    fn bribe_rejected_for_non_negotiable() {
        let mut wolf = bandit();
        wolf.negotiable = false;
        let mut combat = ActiveCombat::new(wolf, CombatMode::Negotiation);
        let mut inv = bribe_wallet(100, 10);
        assert_eq!(combat.attempt_bribe(10000, &mut inv), BribeOutcome::Refused);
        assert!(!combat.is_over());
        assert!(combat.log.last().unwrap().contains("не поддаётся"));
        assert_eq!(inv.player[0].quantity, 100);
    }

    #[test]
    fn combat_prompt_contains_tags() {
        let combat = ActiveCombat::new(bandit(), CombatMode::Fatal);
        let prompt = combat_prompt(&combat);
        assert!(prompt.contains("Тип врага"), "{prompt}");
        assert!(prompt.contains("бандит"), "{prompt}");
        assert!(prompt.contains("человек"), "{prompt}");
    }

    #[test]
    fn combat_prompt_negotiable_hint() {
        let combat = ActiveCombat::new(bandit(), CombatMode::Negotiation);
        let prompt = combat_prompt(&combat);
        assert!(prompt.contains("открыт к переговорам"), "{prompt}");
    }

    /// Outside negotiation mode a bribe is not counted — we don't offer it to the model.
    #[test]
    fn combat_prompt_fatal_rules_out_bribes() {
        let prompt = combat_prompt(&ActiveCombat::new(bandit(), CombatMode::Fatal));
        assert!(prompt.contains("не действуют"), "{prompt}");
        assert!(!prompt.contains("открыт к переговорам"), "{prompt}");
    }

    #[test]
    fn combat_prompt_non_negotiable_hint() {
        let combat = ActiveCombat::new(default_enemies()["wolf"].clone(), CombatMode::Negotiation);
        let prompt = combat_prompt(&combat);
        assert!(prompt.contains("не поддаётся на переговоры"), "{prompt}");
    }

    #[test]
    fn combat_prompt_atmosphere_instruction() {
        let combat = ActiveCombat::new(bandit(), CombatMode::Fatal);
        let prompt = combat_prompt(&combat);
        assert!(prompt.contains("обстановку и атмосферу"), "{prompt}");
    }

    #[test]
    fn new_clamps_hp_to_max() {
        let mut enemy = bandit();
        enemy.hp = u32::MAX;
        let combat = ActiveCombat::new(enemy, CombatMode::Fatal);
        assert_eq!(combat.enemy.hp, MAX_ENEMY_HP);
        assert_eq!(combat.enemy_hp, MAX_ENEMY_HP as i32);
        assert_eq!(combat.enemy_stats["Endurance"], MAX_ENEMY_HP as i32);
    }

    #[test]
    fn hp_as_i32_avoids_overflow() {
        assert_eq!(hp_as_i32(u32::MAX), MAX_ENEMY_HP as i32);
        assert_eq!(hp_as_i32(0), 0);
        assert_eq!(hp_as_i32(40), 40);
    }

    #[test]
    fn clamp_combat_hp_on_load() {
        let mut combat = ActiveCombat::new(bandit(), CombatMode::Fatal);
        combat.enemy.hp = u32::MAX;
        combat.enemy_hp = -50;
        clamp_combat_hp(&mut combat);
        assert_eq!(combat.enemy.hp, MAX_ENEMY_HP);
        assert_eq!(combat.enemy_hp, 0);
    }

    #[test]
    fn attempt_bribe_cost_saturates_on_huge_hp() {
        let mut combat = ActiveCombat::new(bandit(), CombatMode::Negotiation);
        // As if hp came corrupted: cost = saturating_mul, no panic overflow
        combat.enemy.hp = u32::MAX;
        let mut inv = bribe_wallet(1_000, 10);
        assert_eq!(
            combat.attempt_bribe(1_000_000, &mut inv),
            BribeOutcome::BelowPrice { cost: u32::MAX }
        );
        assert!(!combat.is_over());
        assert_eq!(inv.player[0].quantity, 1_000, "отказ не списывает");
    }

    /// V-04: fighter parameters come from progression, not constants.
    #[test]
    fn player_power_derives_from_stats_and_skills() {
        let mut stats_store = StatsStore::new();
        crate::rpg::stats::init_character(&mut stats_store, "hero.png");
        {
            let cs = stats_store.get_mut("hero.png").unwrap();
            cs.stats.get_mut("Endurance").unwrap().level = 6;
            cs.stats.get_mut("Strength").unwrap().level = 8;
            cs.stats.get_mut("Dexterity").unwrap().level = 4;
        }
        let mut skills_store = SkillsStore::new();
        skills::init_character(&mut skills_store, "hero.png");
        {
            let cs = skills_store.get_mut("hero.png").unwrap();
            cs.skills.get_mut("Melee").unwrap().level = 5; // Strength
            cs.skills.get_mut("Archery").unwrap().level = 3; // Dexterity
        }
        let power = player_power(&stats_store, &skills_store, "hero.png");
        assert_eq!(power.max_hp, 80 + 5 * 6);
        assert_eq!(power.defense, 5 + (8 + 4) / 2);
        assert_eq!(power.skill_bonus, 5 + 1 + 3, "Melee(5) + Stealth(1) + Archery(3)");
    }

    /// V-04: the prompt promises "companion skills are accounted for" — the calculation sees them.
    #[test]
    fn set_companion_feeds_the_math() {
        let mut combat = ActiveCombat::new(bandit(), CombatMode::Fatal);
        combat.set_companion(
            Some("Лира".into()),
            PlayerPower {
                max_hp: 120,
                defense: 12,
                skill_bonus: 10,
            },
        );
        assert_eq!(combat.player_hp, 120);
        assert_eq!(combat.player_max_hp, 120);
        assert_eq!(combat.companion_bonus, 10);
        assert!(combat.companion_assists);
        // Player damage sees skills: 20 + 10 - 5/2 = 28 (would have been 18).
        assert_eq!(combat.player_attack(20), 28);
    }

    #[test]
    fn enemy_attack_sees_player_defense_from_power() {
        let mut combat = ActiveCombat::new(bandit(), CombatMode::Fatal);
        combat.set_companion(
            None,
            PlayerPower {
                max_hp: 100,
                defense: 20,
                skill_bonus: 0,
            },
        );
        // bandit.attack = 12: 12 - 20/2 = 2.
        assert_eq!(combat.enemy_attack(), 2);
    }

    #[test]
    fn set_companion_clamps_hostile_stats() {
        let mut combat = ActiveCombat::new(bandit(), CombatMode::Fatal);
        combat.set_companion(
            None,
            PlayerPower {
                max_hp: i32::MAX,
                defense: -5,
                skill_bonus: u32::MAX,
            },
        );
        assert_eq!(combat.player_max_hp, MAX_PLAYER_HP);
        assert_eq!(combat.player_defense, 0);
        assert_eq!(combat.companion_bonus, MAX_COMPANION_BONUS);
    }

    /// A combat file from an older version (without fighter fields) reads with the previous
    /// defaults: 100 HP and defense 10.
    #[test]
    fn combat_json_without_player_fields_loads_defaults() {
        let combat = ActiveCombat::new(bandit(), CombatMode::Fatal);
        let mut value = serde_json::to_value(&combat).unwrap();
        let obj = value.as_object_mut().unwrap();
        obj.remove("player_max_hp");
        obj.remove("player_defense");
        obj.remove("companion_bonus");
        let loaded: ActiveCombat = serde_json::from_value(value).unwrap();
        assert_eq!(loaded.player_max_hp, 100);
        assert_eq!(loaded.player_defense, 10);
        assert_eq!(loaded.companion_bonus, 0);
    }
}

