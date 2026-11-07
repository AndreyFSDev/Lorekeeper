//! NPC system: template characters with names, appearance, and personality.
//! Controlled by AI. They appear/disappear by plot or location.
//! If NPC > 3 in a location — two chat with each other, the rest are "busy".

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::paths;
use crate::relationships::RelationshipAxes;
use crate::rpg::world::World;

/// NPC in the world.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Npc {
    pub id: String,
    pub name: String,
    pub appearance: String,
    pub personality_tags: Vec<String>,
    pub current_location: String,
    pub dialogue_state: NpcDialogue,
    pub relationship_with_player: RelationshipAxes,
    pub alive: bool,
    pub shop_inventory: Option<Vec<String>>,
}

/// NPC dialogue state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum NpcDialogue {
    Idle,
    InConversation { with: String },
    Excluded,
}

impl Default for NpcDialogue {
    fn default() -> Self {
        Self::Idle
    }
}

pub type NpcStore = BTreeMap<String, Npc>;

/// Maximum active NPCs in a location (the rest are "busy").
const MAX_ACTIVE_NPCS: usize = 3;

/// Location residents stay in the scene before generated `npc_*`: otherwise
/// after the third description the innkeeper drops out of the prompt alphabetically.
fn prefer_residents(a: &str, b: &str) -> std::cmp::Ordering {
    let rank = |id: &str| usize::from(id.starts_with("npc_"));
    rank(a).cmp(&rank(b)).then_with(|| a.cmp(b))
}

impl Npc {
    /// Whether the NPC is available for dialogue.
    pub fn is_available(&self) -> bool {
        self.alive && self.dialogue_state == NpcDialogue::Idle
    }
}

/// Get the available NPCs in the location (no more than MAX_ACTIVE_NPCS).
/// The rest are marked as "busy" (chatting with each other).
pub fn active_npcs_in_location<'a>(store: &'a NpcStore, location: &str) -> Vec<&'a Npc> {
    let mut available: Vec<&Npc> = store
        .values()
        .filter(|npc| npc.current_location == location && npc.is_available())
        .collect();

    if available.len() <= MAX_ACTIVE_NPCS {
        return available;
    }

    // Location residents before generated ids, within a group — by id.
    available.sort_by(|a, b| prefer_residents(&a.id, &b.id));
    available.truncate(MAX_ACTIVE_NPCS);
    available
}

/// Mark the extra NPCs as "chatting with each other".
/// Resets NPCs from the overflow state back to Idle if they are no longer in overflow.
pub fn exclude_overflow_npcs(store: &mut NpcStore, location: &str) {
    // Reset the overflow state: Excluded → Idle, and also legacy InConversation { with: "другой NPC" }.
    for npc in store.values_mut() {
        if npc.current_location == location && npc.alive {
            let is_overflow = npc.dialogue_state == NpcDialogue::Excluded
                || matches!(
                    &npc.dialogue_state,
                    NpcDialogue::InConversation { with } if with == "другой NPC"
                );
            if is_overflow {
                npc.dialogue_state = NpcDialogue::Idle;
            }
        }
    }

    let mut available: Vec<String> = store
        .values()
        .filter(|npc| npc.current_location == location && npc.is_available())
        .map(|npc| npc.id.clone())
        .collect();

    if available.len() <= MAX_ACTIVE_NPCS {
        return;
    }

    available.sort_by(|a, b| prefer_residents(a, b));
    let keep: Vec<String> = available[..MAX_ACTIVE_NPCS].to_vec();

    for npc in store.values_mut() {
        if npc.current_location == location
            && npc.is_available()
            && !keep.contains(&npc.id)
        {
            npc.dialogue_state = NpcDialogue::Excluded;
        }
    }
}

fn store_path() -> PathBuf {
    paths::base_dir().join("data").join("npcs.json")
}

static STORE: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn load() -> NpcStore {
    crate::json_store::load(&STORE, &store_path())
}

pub fn update<T>(change: impl FnOnce(&mut NpcStore) -> T) -> Result<T, String> {
    crate::json_store::update(&STORE, &store_path(), change)
}

/// Text for the prompt: NPCs in the current location.
pub fn npcs_note(store: &NpcStore, location: &str) -> Option<String> {
    let npcs = active_npcs_in_location(store, location);
    if npcs.is_empty() {
        return None;
    }
    let lines: Vec<String> = npcs
        .iter()
        .map(|npc| {
            // NPCs seeded from the world come without appearance and personality —
            // empty parentheses in the prompt are just noise.
            let mut details: Vec<String> = Vec::new();
            if !npc.appearance.is_empty() {
                details.push(npc.appearance.clone());
            }
            details.extend(npc.personality_tags.iter().cloned());
            if details.is_empty() {
                format!("— {}", npc.name)
            } else {
                format!("— {} ({})", npc.name, details.join(", "))
            }
        })
        .collect();
    Some(format!("В локации находятся:\n{}", lines.join("\n")))
}

/// Prompt for NPC generation taking the location type into account.
pub fn generation_prompt(
    location_name: &str,
    location_tags: &[String],
    atmosphere: &str,
    shop_type: Option<&str>,
) -> String {
    let tags_clause = if location_tags.is_empty() {
        String::new()
    } else {
        format!(" (теги: {})", location_tags.join(", "))
    };
    let shop_hint = match shop_type {
        Some(st) => format!("\nВ локации есть магазин: {st}. NPC может быть владельцем или работником."),
        None => String::new(),
    };
    format!(
        "Сгенерируй NPC для ролевой игры. Локация: «{location_name}»{tags_clause}. \
         Атмосфера: {atmosphere}.{shop_hint}\n\n\
         NPC должен подходить под тип локации (таверна → трактирщик, рынок → торговец, \
         подземелье → стражник или пленник).\n\n\
         Верни ТОЛЬКО JSON:\n\
         {{\"name\": \"Имя\", \"appearance\": \"Описание внешности 1-2 предложения\", \
         \"personality_tags\": [\"тег1\", \"тег2\"]}}"
    )
}

// -- Writers: seeding from the world and model generation (V-02) --

/// The starting record of an NPC declared by the world (`Location::npcs`): without it
/// `npcs_note` was always empty, and the model did not know who was in the location.
fn default_npc(id: &str, location: &str) -> Npc {
    // World starting NPCs are declared with English ids; the prompt and the showcase get the Russian name
    let name = match id {
        "tavern_keeper" => "Трактирщик".to_string(),
        "merchant" => "Торговец".to_string(),
        "blacksmith" => "Кузнец".to_string(),
        "guard" => "Стражник".to_string(),
        "elder" => "Староста".to_string(),
        "healer" => "Лекарь".to_string(),
        other => other.replace('_', " "),
    };
    Npc {
        id: id.to_string(),
        name,
        appearance: String::new(),
        personality_tags: Vec::new(),
        current_location: location.to_string(),
        dialogue_state: NpcDialogue::Idle,
        relationship_with_player: RelationshipAxes::default(),
        alive: true,
        shop_inventory: None,
    }
}

/// Seeding NPCs from the world and recounting the "busy" ones in the current location (the pure part).
/// Existing NPCs are not overwritten. `true` — the store changed.
pub fn sync_store(store: &mut NpcStore, world: &World) -> bool {
    let mut seeded = false;
    // current_location of an NPC is the location id (like `World::current_location`),
    // not the display name: `npcs_note` looks up by id.
    for (loc_id, loc) in &world.locations {
        for id in &loc.npcs {
            if !store.contains_key(id) {
                store.insert(id.clone(), default_npc(id, loc_id));
                seeded = true;
            }
        }
    }
    let before: Vec<(String, NpcDialogue)> = store
        .iter()
        .map(|(id, npc)| (id.clone(), npc.dialogue_state.clone()))
        .collect();
    exclude_overflow_npcs(store, &world.current_location);
    let overflow_changed = store.iter().any(|(id, npc)| {
        before
            .iter()
            .find(|(before_id, _)| before_id == id)
            .is_some_and(|(_, state)| *state != npc.dialogue_state)
    });
    seeded || overflow_changed
}

/// Seeding and "busy" marks for the current world on disk. Idempotent; the gateway calls it on
/// entering a location and after NPC generation.
pub fn sync_with_world() -> Result<bool, String> {
    let world = crate::rpg::world::load();
    update(|store| sync_store(store, &world))
}

/// Truncate a string by characters (the prompt contract clamp).
fn clamp_chars(raw: &str, max: usize) -> String {
    raw.trim().chars().take(max).collect()
}

/// An NPC draft from the model reply (after clamps).
#[derive(Debug, Clone)]
pub struct NpcDraft {
    pub name: String,
    pub appearance: String,
    pub personality_tags: Vec<String>,
    pub location: String,
}

/// Parse the model reply from `generation_prompt`: {"name", "appearance",
/// "personality_tags"}. Fail closed: garbage or an empty name → None. Clamps by the
/// prompt contract: name 1–60, appearance up to 400, up to 6 tags of 40.
pub fn parse_generated(raw: &str, location: &str) -> Option<NpcDraft> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end <= start {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(&raw[start..=end]).ok()?;
    let name = clamp_chars(value.get("name")?.as_str()?, 60);
    if name.is_empty() {
        return None;
    }
    let appearance = clamp_chars(
        value.get("appearance").and_then(|v| v.as_str()).unwrap_or_default(),
        400,
    );
    let personality_tags: Vec<String> = value
        .get("personality_tags")
        .and_then(|v| v.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_str())
                .map(|s| clamp_chars(s, 40))
                .filter(|s| !s.is_empty())
                .take(6)
                .collect()
        })
        .unwrap_or_default();
    Some(NpcDraft {
        name,
        appearance,
        personality_tags,
        location: clamp_chars(location, 60),
    })
}

/// Parse and write an NPC from the model reply. Commit "only on success":
/// on garbage the file is not created and not overwritten.
pub fn commit_generated(raw: &str, location: &str) -> Result<Npc, String> {
    let draft = parse_generated(raw, location)
        .ok_or_else(|| "Модель вернула не JSON или NPC без имени".to_string())?;
    commit_draft_at(&store_path(), draft)
}

fn commit_draft_at(path: &Path, draft: NpcDraft) -> Result<Npc, String> {
    crate::json_store::update_if_ok(&STORE, path, |store: &mut NpcStore| {
        // One name per location: repeated generation does not create twins.
        if store
            .values()
            .any(|npc| npc.name == draft.name && npc.current_location == draft.location)
        {
            return Err(format!("NPC «{}» уже есть в локации", draft.name));
        }
        let mut n = 1;
        while store.contains_key(&format!("npc_{n}")) {
            n += 1;
        }
        let npc = Npc {
            id: format!("npc_{n}"),
            name: draft.name.clone(),
            appearance: draft.appearance.clone(),
            personality_tags: draft.personality_tags.clone(),
            current_location: draft.location.clone(),
            dialogue_state: NpcDialogue::Idle,
            relationship_with_player: RelationshipAxes::default(),
            alive: true,
            shop_inventory: None,
        };
        store.insert(npc.id.clone(), npc.clone());
        exclude_overflow_npcs(store, &draft.location);
        Ok(npc)
    })
    .and_then(|result| result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_npc(id: &str, location: &str) -> Npc {
        Npc {
            id: id.to_string(),
            name: format!("NPC {id}"),
            appearance: "обычный вид".to_string(),
            personality_tags: vec!["дружелюбный".to_string()],
            current_location: location.to_string(),
            dialogue_state: NpcDialogue::Idle,
            relationship_with_player: RelationshipAxes::default(),
            alive: true,
            shop_inventory: None,
        }
    }

    #[test]
    fn active_npcs_within_limit() {
        let mut store = NpcStore::new();
        store.insert("a".into(), make_npc("a", "tavern"));
        store.insert("b".into(), make_npc("b", "tavern"));
        let active = active_npcs_in_location(&store, "tavern");
        assert_eq!(active.len(), 2);
    }

    #[test]
    fn active_npcs_overflow() {
        let mut store = NpcStore::new();
        for i in 0..5 {
            store.insert(format!("npc{i}"), make_npc(&format!("npc{i}"), "tavern"));
        }
        let active = active_npcs_in_location(&store, "tavern");
        assert_eq!(active.len(), MAX_ACTIVE_NPCS);
    }

    /// Generated `npc_*` do not push out the innkeeper: their ids sort earlier
    /// alphabetically, and without resident priority he dropped out of the scene.
    #[test]
    fn residents_stay_when_generated_npcs_overflow() {
        let mut store = NpcStore::new();
        store.insert("tavern_keeper".into(), make_npc("tavern_keeper", "tavern"));
        for i in 1..=3 {
            let id = format!("npc_{i}");
            store.insert(id.clone(), make_npc(&id, "tavern"));
        }
        let active = active_npcs_in_location(&store, "tavern");
        let ids: Vec<&str> = active.iter().map(|npc| npc.id.as_str()).collect();
        assert!(ids.contains(&"tavern_keeper"), "{ids:?}");
        exclude_overflow_npcs(&mut store, "tavern");
        assert_eq!(store["tavern_keeper"].dialogue_state, NpcDialogue::Idle);
    }

    #[test]
    fn dead_npc_not_available() {
        let mut npc = make_npc("dead", "tavern");
        npc.alive = false;
        assert!(!npc.is_available());
    }

    #[test]
    fn exclude_overflow_marks_busy() {
        let mut store = NpcStore::new();
        for i in 0..5 {
            store.insert(format!("npc{i}"), make_npc(&format!("npc{i}"), "tavern"));
        }
        exclude_overflow_npcs(&mut store, "tavern");
        let busy_count = store
            .values()
            .filter(|n| n.current_location == "tavern" && n.dialogue_state != NpcDialogue::Idle)
            .count();
        assert_eq!(busy_count, 2); // 5 - 3 = 2 are busy
    }

    #[test]
    fn npcs_note_empty_location() {
        let store = NpcStore::new();
        assert!(npcs_note(&store, "tavern").is_none());
    }

    #[test]
    fn npc_with_empty_personality_tags() {
        let mut store = NpcStore::new();
        let mut npc = make_npc("a", "tavern");
        npc.personality_tags = vec![];
        store.insert("a".into(), npc);
        let note = npcs_note(&store, "tavern").unwrap();
        assert!(note.contains("NPC a"), "{note}");
        assert!(!note.contains(", )"), "пустые теги не должны давать trailing comma: {note}");
    }

    #[test]
    fn exclude_overflow_all_in_conversation() {
        let mut store = NpcStore::new();
        for i in 0..5 {
            let mut npc = make_npc(&format!("npc{i}"), "tavern");
            npc.dialogue_state = NpcDialogue::InConversation {
                with: format!("player{i}"),
            };
            store.insert(format!("npc{i}"), npc);
        }
        exclude_overflow_npcs(&mut store, "tavern");
        // All InConversation — idle_count=0, overflow does not apply.
        let excluded: Vec<_> = store
            .values()
            .filter(|n| n.dialogue_state == NpcDialogue::Excluded)
            .collect();
        assert_eq!(excluded.len(), 0, "InConversation NPC не должны попасть в Excluded");
        let in_conv: Vec<_> = store
            .values()
            .filter(|n| matches!(&n.dialogue_state, NpcDialogue::InConversation { .. }))
            .collect();
        assert_eq!(in_conv.len(), 5);
    }

    #[test]
    fn active_npcs_unknown_location() {
        let mut store = NpcStore::new();
        store.insert("a".into(), make_npc("a", "tavern"));
        let active = active_npcs_in_location(&store, "dungeon");
        assert!(active.is_empty());
    }

    #[test]
    fn generation_prompt_empty_tags() {
        let prompt = generation_prompt("Таверна", &[], "тёплая", Some("еда"));
        assert!(prompt.contains("Таверна"), "{prompt}");
        assert!(!prompt.contains("(теги:"), "пустые теги не должны давать clause: {prompt}");
        assert!(prompt.contains("тёплая"), "{prompt}");
        assert!(prompt.contains("еда"), "{prompt}");
    }

    #[test]
    fn dead_npc_not_in_npcs_note() {
        let mut store = NpcStore::new();
        let mut npc = make_npc("dead", "tavern");
        npc.alive = false;
        store.insert("dead".into(), npc);
        store.insert("alive".into(), make_npc("alive", "tavern"));
        let note = npcs_note(&store, "tavern").unwrap();
        assert!(!note.contains("dead"), "{note}");
        assert!(note.contains("alive"), "{note}");
    }

    #[test]
    fn npcs_note_format() {
        let mut store = NpcStore::new();
        store.insert("a".into(), make_npc("a", "tavern"));
        let note = npcs_note(&store, "tavern").unwrap();
        assert!(note.contains("NPC a"), "{note}");
        assert!(note.contains("дружелюбный"), "{note}");
    }

    #[test]
    fn generation_prompt_format() {
        let prompt = generation_prompt("Таверна", &["крытое".into(), "город".into()], "тёплая", None);
        assert!(prompt.contains("Таверна"), "{prompt}");
        assert!(prompt.contains("крытое"), "{prompt}");
        assert!(prompt.contains("тёплая"), "{prompt}");
        assert!(!prompt.contains("магазин"), "без shop_type не должно быть подсказки: {prompt}");
    }

    #[test]
    fn npc_dialogue_partial_eq_in_conversation() {
        let a = NpcDialogue::InConversation { with: "player".into() };
        let b = NpcDialogue::InConversation { with: "player".into() };
        let c = NpcDialogue::InConversation { with: "other".into() };
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, NpcDialogue::Idle);
        assert_ne!(a, NpcDialogue::Excluded);
    }

    #[test]
    fn is_available_in_conversation() {
        let mut npc = make_npc("a", "tavern");
        npc.dialogue_state = NpcDialogue::InConversation { with: "player".into() };
        assert!(!npc.is_available());
    }

    #[test]
    fn is_available_excluded() {
        let mut npc = make_npc("a", "tavern");
        npc.dialogue_state = NpcDialogue::Excluded;
        assert!(!npc.is_available());
    }

    #[test]
    fn active_npcs_filters_excluded() {
        let mut store = NpcStore::new();
        let mut excluded = make_npc("ex", "tavern");
        excluded.dialogue_state = NpcDialogue::Excluded;
        store.insert("ex".into(), excluded);
        store.insert("ok".into(), make_npc("ok", "tavern"));
        let active = active_npcs_in_location(&store, "tavern");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, "ok");
    }

    #[test]
    fn exclude_overflow_dead_not_counted() {
        let mut store = NpcStore::new();
        // 4 alive + 1 dead → the dead one does not count, overflow 1.
        for i in 0..4 {
            store.insert(format!("npc{i}"), make_npc(&format!("npc{i}"), "tavern"));
        }
        let mut dead = make_npc("dead", "tavern");
        dead.alive = false;
        store.insert("dead".into(), dead);

        exclude_overflow_npcs(&mut store, "tavern");
        let excluded: Vec<_> = store
            .values()
            .filter(|n| n.dialogue_state == NpcDialogue::Excluded)
            .collect();
        assert_eq!(excluded.len(), 1, "мёртвый NPC не должен влиять на overflow");
    }

    #[test]
    fn generation_prompt_empty_name() {
        let prompt = generation_prompt("", &["тег1".into()], "", None);
        assert!(prompt.contains("«»"), "{prompt}");
        assert!(prompt.contains("тег1"), "{prompt}");
    }

    #[test]
    fn generation_prompt_with_shop_hint() {
        let prompt = generation_prompt("Рынок", &["открытое".into()], "шумная", Some("разное"));
        assert!(prompt.contains("магазин"), "{prompt}");
        assert!(prompt.contains("разное"), "{prompt}");
        assert!(prompt.contains("рынок → торговец"), "должны быть примеры типов: {prompt}");
    }

    #[test]
    fn npcs_note_format_empty_tags() {
        let mut store = NpcStore::new();
        let mut npc = make_npc("a", "tavern");
        npc.personality_tags = vec![];
        store.insert("a".into(), npc);
        let note = npcs_note(&store, "tavern").unwrap();
        assert_eq!(note, "В локации находятся:\n— NPC a (обычный вид)");
    }

    #[test]
    fn exclude_overflow_resets_when_npc_moves() {
        let mut store = NpcStore::new();
        for i in 0..5 {
            store.insert(format!("npc{i}"), make_npc(&format!("npc{i}"), "tavern"));
        }
        exclude_overflow_npcs(&mut store, "tavern");
        assert_eq!(
            store
                .values()
                .filter(|n| n.dialogue_state == NpcDialogue::Excluded)
                .count(),
            2
        );

        // Move two NPCs to another location.
        store.get_mut("npc0").unwrap().current_location = "forest".into();
        store.get_mut("npc1").unwrap().current_location = "forest".into();
        exclude_overflow_npcs(&mut store, "tavern");
        // Now there are 3 NPCs in tavern — all Idle.
        assert!(store
            .values()
            .filter(|n| n.current_location == "tavern")
            .all(|n| n.dialogue_state == NpcDialogue::Idle));
    }

    #[test]
    fn exclude_overflow_preserves_in_conversation() {
        let mut store = NpcStore::new();
        // 5 idle + 2 in_conversation = 7 total alive.
        for i in 0..5 {
            store.insert(format!("npc{i}"), make_npc(&format!("npc{i}"), "tavern"));
        }
        for i in 5..7 {
            let mut npc = make_npc(&format!("npc{i}"), "tavern");
            npc.dialogue_state = NpcDialogue::InConversation {
                with: format!("player{i}"),
            };
            store.insert(format!("npc{i}"), npc);
        }
        exclude_overflow_npcs(&mut store, "tavern");
        // 5 idle → keep 3, exclude 2.
        // InConversation is left untouched.
        let excluded: Vec<_> = store
            .values()
            .filter(|n| n.dialogue_state == NpcDialogue::Excluded)
            .collect();
        assert_eq!(excluded.len(), 2);
        let in_conv: Vec<_> = store
            .values()
            .filter(|n| matches!(&n.dialogue_state, NpcDialogue::InConversation { .. }))
            .collect();
        assert_eq!(in_conv.len(), 2, "InConversation не должны стать Excluded");
        let idle: Vec<_> = store
            .values()
            .filter(|n| {
                n.current_location == "tavern" && n.dialogue_state == NpcDialogue::Idle
            })
            .collect();
        assert_eq!(idle.len(), 3);
    }

    // --- Writers: seeding from world and model generation (V-02) ---

    fn temp_npc_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("cloufam_test_npcs");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("json.corrupt"));
        path
    }

    /// V-02: the world declares NPCs in locations, and `npcs_note` was always empty —
    /// seeding makes the record visible to the prompt.
    #[test]
    fn sync_store_seeds_npcs_declared_by_the_world() {
        let world = World::default();
        let mut store = NpcStore::new();
        assert!(sync_store(&mut store, &world), "досев обязан менять стор");
        assert!(store.contains_key("tavern_keeper"));
        assert!(store.contains_key("merchant"));
        assert_eq!(store["tavern_keeper"].current_location, "tavern");
        assert_eq!(store["merchant"].current_location, "market");
        let note = npcs_note(&store, "tavern").expect("трактирщик виден в промпте");
        assert!(note.contains("Трактирщик"), "{note}");
    }

    #[test]
    fn sync_store_is_idempotent() {
        let world = World::default();
        let mut store = NpcStore::new();
        assert!(sync_store(&mut store, &world));
        assert!(!sync_store(&mut store, &world), "повторный досев ничего не меняет");
    }

    #[test]
    fn sync_store_marks_overflow_in_current_location() {
        let mut world = World::default();
        world.current_location = "tavern".into();
        let tavern = world.locations.get_mut("tavern").unwrap();
        tavern.npcs = (0..5).map(|i| format!("guest_{i}")).collect();
        let mut store = NpcStore::new();
        assert!(sync_store(&mut store, &world));
        let excluded = store
            .values()
            .filter(|n| n.dialogue_state == NpcDialogue::Excluded)
            .count();
        assert_eq!(excluded, 2, "из пятерых в локации заняты двое");
    }

    #[test]
    fn parse_generated_accepts_and_clamps_model_json() {
        let raw = format!(
            r#"{{"name": "{}", "appearance": "{}", "personality_tags": ["a","b","c","d","e","f","g"]}}"#,
            "и".repeat(100),
            "в".repeat(600),
        );
        let draft = parse_generated(&raw, "tavern").expect("валидный ответ");
        assert_eq!(draft.name.chars().count(), 60);
        assert_eq!(draft.appearance.chars().count(), 400);
        assert_eq!(draft.personality_tags.len(), 6);
        assert_eq!(draft.location, "tavern");
    }

    #[test]
    fn parse_generated_rejects_garbage_and_empty_name() {
        assert!(parse_generated("не json", "tavern").is_none());
        assert!(parse_generated(r#"{"name": "   "}"#, "tavern").is_none());
        assert!(parse_generated(r#"{"appearance": "только внешность"}"#, "tavern").is_none());
    }

    /// Commit "only on success": garbage does not write the file, a valid reply —
    /// writes the NPC with an id, a duplicate name in the location is rejected without writing.
    #[test]
    fn commit_generated_commits_only_on_success() {
        let path = temp_npc_path("commit.json");
        let draft = parse_generated(r#"{"name": "Лира", "appearance": "тёмные кудри"}"#, "tavern")
            .expect("валидный ответ");
        let npc = commit_draft_at(&path, draft.clone()).expect("NPC записан");
        assert_eq!(npc.id, "npc_1");
        assert!(path.exists());

        let before = std::fs::read_to_string(&path).unwrap();
        let err = commit_draft_at(&path, draft).expect_err("дубль имени в локации отклоняется");
        assert!(err.contains("уже есть"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "отказ не пишет файл");

        let store: NpcStore = crate::json_store::load(&STORE, &path);
        assert_eq!(store.len(), 1);
        let _ = std::fs::remove_file(&path);
    }
}

