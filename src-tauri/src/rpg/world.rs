//! World system: locations, map, atmosphere, navigation.
//! Locations affect the atmosphere and events. Auto-generation through the director.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::paths;

/// Cloufam world: a set of locations and the player's current position.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct World {
    pub locations: BTreeMap<String, Location>,
    pub current_location: String,
}

impl Default for World {
    fn default() -> Self {
        let mut locations = BTreeMap::new();
        locations.insert(
            "town_square".to_string(),
            Location {
                name: "Городская площадь".to_string(),
                description: "Шумная площадь в центре города. Рядом — фонтан и торговые ряды.".to_string(),
                tags: vec!["город".to_string(), "открытое".to_string()],
                atmosphere: "оживлённая".to_string(),
                connections: vec!["tavern".to_string(), "market".to_string(), "forest_path".to_string(), "castle".to_string()],
                npcs: vec![],
                has_shop: false,
                shop_type: None,
                generated_by_ai: false,
            },
        );
        locations.insert(
            "tavern".to_string(),
            Location {
                name: "Таверна «Золотой дракон»".to_string(),
                description: "Уютная таверна с потрескивающим камином. Пахнет хлебом и элем.".to_string(),
                tags: vec!["город".to_string(), "крытое".to_string()],
                atmosphere: "тёплая".to_string(),
                connections: vec!["town_square".to_string()],
                npcs: vec!["tavern_keeper".to_string()],
                has_shop: true,
                shop_type: Some("еда и напитки".to_string()),
                generated_by_ai: false,
            },
        );
        locations.insert(
            "market".to_string(),
            Location {
                name: "Рынок".to_string(),
                description: "Ряды прилавков с товарами со всех уголков земель.".to_string(),
                tags: vec!["город".to_string(), "открытое".to_string()],
                atmosphere: "шумная".to_string(),
                connections: vec!["town_square".to_string()],
                npcs: vec!["merchant".to_string()],
                has_shop: true,
                shop_type: Some("разное".to_string()),
                generated_by_ai: false,
            },
        );
        locations.insert(
            "forest_path".to_string(),
            Location {
                name: "Лесная тропа".to_string(),
                description: "Узкая тропа, уходящая в чащу леса.".to_string(),
                tags: vec!["лес".to_string(), "открытое".to_string()],
                atmosphere: "тихая".to_string(),
                connections: vec!["town_square".to_string(), "forest_cave".to_string(), "dungeon".to_string()],
                npcs: vec![],
                has_shop: false,
                shop_type: None,
                generated_by_ai: false,
            },
        );
        locations.insert(
            "dungeon".to_string(),
            Location {
                name: "Подземелье".to_string(),
                description: "Тёмные коридоры, пропитанные сыростью. Факелы едва освещают путь.".to_string(),
                tags: vec!["подземелье".to_string(), "крытое".to_string(), "опасное".to_string()],
                atmosphere: "жуткая".to_string(),
                connections: vec!["forest_path".to_string(), "forest_cave".to_string()],
                npcs: vec![],
                has_shop: false,
                shop_type: None,
                generated_by_ai: false,
            },
        );
        locations.insert(
            "forest_cave".to_string(),
            Location {
                name: "Лесная пещера".to_string(),
                description: "Пещера, скрытая за водопадом. Слышно эхо капающей воды.".to_string(),
                tags: vec!["лес".to_string(), "пещера".to_string(), "крытое".to_string()],
                atmosphere: "таинственная".to_string(),
                connections: vec!["forest_path".to_string(), "dungeon".to_string()],
                npcs: vec![],
                has_shop: false,
                shop_type: None,
                generated_by_ai: false,
            },
        );
        locations.insert(
            "castle".to_string(),
            Location {
                name: "Замок".to_string(),
                description: "Величественный замок с высокими башнями. Стражники у ворот.".to_string(),
                tags: vec!["город".to_string(), "крытое".to_string(), "замок".to_string()],
                atmosphere: "величественная".to_string(),
                connections: vec!["town_square".to_string()],
                npcs: vec![],
                has_shop: false,
                shop_type: None,
                generated_by_ai: false,
            },
        );
        Self {
            locations,
            current_location: "town_square".to_string(),
        }
    }
}

/// One location in the world.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Location {
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
    pub atmosphere: String,
    pub connections: Vec<String>,
    pub npcs: Vec<String>,
    pub has_shop: bool,
    pub shop_type: Option<String>,
    pub generated_by_ai: bool,
}

impl World {
    /// Empty placeholder world for "not started" and "broken file". This is
    /// deliberately not `World::default()`: the default fantasy city dragged
    /// every scene to the town square and leaked into the prompt when
    /// `world.json` was corrupt (V-01).
    pub fn empty() -> Self {
        Self {
            locations: BTreeMap::new(),
            current_location: String::new(),
        }
    }

    /// Move to a location. Returns an error if there is no connection.
    pub fn move_to(&mut self, target: &str) -> Result<&Location, String> {
        let current = self.locations.get(&self.current_location)
            .ok_or_else(|| "Текущая локация не найдена".to_string())?;
        if !current.connections.iter().any(|c| c == target) {
            return Err(format!("Нет пути из {} в {}", current.name, target));
        }
        if !self.locations.contains_key(target) {
            return Err(format!("Локация «{target}» не существует"));
        }
        self.current_location = target.to_string();
        Ok(self.locations.get(target).unwrap())
    }

    /// Current location.
    pub fn current(&self) -> Option<&Location> {
        self.locations.get(&self.current_location)
    }

    /// Available directions from the current location.
    pub fn available_exits(&self) -> Vec<(&str, &str)> {
        let current = match self.current() {
            Some(l) => l,
            None => return Vec::new(),
        };
        current
            .connections
            .iter()
            .filter_map(|id| {
                let loc = self.locations.get(id)?;
                Some((id.as_str(), loc.name.as_str()))
            })
            .collect()
    }

    /// Add a location (from the director or auto-generation).
    pub fn add_location(&mut self, id: String, location: Location) {
        self.locations.insert(id, location);
    }

    /// Connect two locations (bidirectional link).
    /// Both locations must exist — otherwise the link is not added.
    pub fn connect(&mut self, a: &str, b: &str) {
        if !self.locations.contains_key(a) || !self.locations.contains_key(b) {
            return;
        }
        if let Some(loc_a) = self.locations.get_mut(a) {
            if !loc_a.connections.iter().any(|c| c == b) {
                loc_a.connections.push(b.to_string());
            }
        }
        if let Some(loc_b) = self.locations.get_mut(b) {
            if !loc_b.connections.iter().any(|c| c == a) {
                loc_b.connections.push(a.to_string());
            }
        }
    }

    /// Set a shop in the current location (from the director).
    pub fn set_shop(&mut self, shop_type: Option<String>) -> Result<(), String> {
        let loc = self.locations.get_mut(&self.current_location)
            .ok_or_else(|| "Текущая локация не найдена".to_string())?;
        match shop_type {
            Some(t) => {
                loc.has_shop = true;
                loc.shop_type = Some(t);
            }
            None => {
                loc.has_shop = false;
                loc.shop_type = None;
            }
        }
        Ok(())
    }
}

fn store_path() -> PathBuf {
    paths::base_dir().join("data").join("world.json")
}

static STORE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// World state on disk. "No file" and "corrupt file" are different situations
/// (V-01): a corrupt world must not look started and must not feed the default
/// fantasy city into the prompt — that city dragged every scene to the square.
pub enum WorldState {
    /// RPG is not started: the file is missing.
    NotStarted,
    /// The world was read successfully.
    Ready(World),
    /// The file exists but cannot be read or parsed. `json_store::read_state`
    /// already preserved it as `world.json.corrupt`; writing over such a file
    /// is refused (V-09) — recovery is an explicit `reset` + `start`.
    Broken,
}

/// World state, read under the store lock. A read or parse failure is
/// `Broken`, not an empty or default world.
pub fn state() -> WorldState {
    state_at(&store_path())
}

fn state_at(path: &Path) -> WorldState {
    let _guard = STORE.lock().unwrap_or_else(|e| e.into_inner());
    match crate::json_store::read_state(path) {
        Ok(Some(world)) => WorldState::Ready(world),
        Ok(None) => WorldState::NotStarted,
        Err(_) => WorldState::Broken,
    }
}

/// Whether the world is started: the file exists and parses. A corrupt world
/// is not "started" — it is hidden from the panel and kept out of the prompt.
pub fn started() -> bool {
    matches!(state(), WorldState::Ready(_))
}

pub const NOT_STARTED: &str = "RPG-мир не начат";

/// The world for prompts and the panel. Before start and with a corrupt file
/// this is an empty world (`world_note` is empty), never the default fantasy
/// city (V-01).
pub fn load() -> World {
    load_at(&store_path())
}

fn load_at(path: &Path) -> World {
    match state_at(path) {
        WorldState::Ready(world) => world,
        _ => World::empty(),
    }
}

/// Edit of a started world: write only on `Ok` and only if the world has been started.
/// Otherwise a transition or a shop would create `world.json` with the default city.
pub fn update<T>(
    change: impl FnOnce(&mut World) -> Result<T, String>,
) -> Result<Result<T, String>, String> {
    update_at(&store_path(), change)
}

fn update_at<T>(
    path: &Path,
    change: impl FnOnce(&mut World) -> Result<T, String>,
) -> Result<Result<T, String>, String> {
    crate::json_store::update_if_ok(&STORE, path, |world| {
        // Check under the store lock: `reset` deletes the file under the same lock, and
        // an edit started before the reset will not revive the world.
        if !path.exists() {
            return Err(NOT_STARTED.to_string());
        }
        change(world)
    })
}

/// Start the RPG: create `world.json` with the default world. An already started world is not
/// touched. `true` — the world was created by this call. A corrupt `world.json` is not
/// silently recreated (that is an outer error, the `.json.corrupt` copy is
/// already kept): recovery is `reset` + `start`.
pub fn start() -> Result<bool, String> {
    start_at(&store_path())
}

fn start_at(path: &Path) -> Result<bool, String> {
    // Err(()) — the world already exists: `update_if_ok` then writes nothing.
    crate::json_store::update_if_ok(&STORE, path, |_: &mut World| {
        if path.exists() {
            Err(())
        } else {
            Ok(())
        }
    })
    .map(|created| created.is_ok())
}

/// Reset the world: RPG blocks leave the prompt again until the next `start`.
pub fn reset() -> Result<(), String> {
    crate::json_store::remove(&STORE, &store_path())
}

/// Text for the prompt: the current location and surroundings. NPCs are not included —
/// their appearance and character are listed by `npcs::npcs_note`.
pub fn world_note(world: &World) -> String {
    let mut lines = Vec::new();
    if let Some(current) = world.current() {
        lines.push(format!(
            "Локация: {} — {} (атмосфера: {})",
            current.name, current.description, current.atmosphere
        ));
        let exits: Vec<String> = world
            .available_exits()
            .iter()
            .map(|(_, name)| name.to_string())
            .collect();
        if !exits.is_empty() {
            lines.push(format!("Рядом: {}", exits.join(", ")));
        }
    }
    lines.join("\n")
}

/// Prompt for location auto-generation.
pub fn generation_prompt(from: &str, to: &str, from_desc: &str) -> String {
    format!(
        "Сгенерируй новую локацию для ролевой игры. Игрок идёт из «{from}» ({from_desc}) в «{to}».\n\n\
         Верни ТОЛЬКО JSON:\n\
         {{\"name\": \"Название\", \"description\": \"Описание 2-3 предложения\", \
         \"tags\": [\"тег1\", \"тег2\"], \"atmosphere\": \"атмосфера одним словом\"}}\n\n\
         Локация должна логически следовать из предыдущей и вести к «{to}».",
        from = from,
        to = to,
        from_desc = from_desc,
    )
}

/// Truncate a string by characters (prompt contract clamp).
fn clamp_chars(raw: &str, max: usize) -> String {
    raw.trim().chars().take(max).collect()
}

/// Parse the model's reply from `generation_prompt`: {"name", "description",
/// "tags", "atmosphere"}. Fail closed: no name — None. Clamps by the
/// prompt contract: name 1–80, description up to 600, up to 6 tags of 40,
/// atmosphere up to 40.
pub fn parse_generated(raw: &str) -> Option<Location> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end <= start {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(&raw[start..=end]).ok()?;
    let name = clamp_chars(value.get("name")?.as_str()?, 80);
    if name.is_empty() {
        return None;
    }
    let description = clamp_chars(
        value.get("description").and_then(|v| v.as_str()).unwrap_or_default(),
        600,
    );
    let tags: Vec<String> = value
        .get("tags")
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
    let atmosphere = clamp_chars(
        value.get("atmosphere").and_then(|v| v.as_str()).unwrap_or_default(),
        40,
    );
    Some(Location {
        name,
        description,
        tags,
        atmosphere,
        connections: Vec::new(),
        npcs: Vec::new(),
        has_shop: false,
        shop_type: None,
        generated_by_ai: true,
    })
}

/// Create a new location from the model's reply and enter it: `add_location` +
/// bidirectional `connect` with `from_id`, then transition. Commit "only on
/// success": on garbage or a taken id the world is not touched. `to_id` — the id of the new
/// location (non-empty and not taken); `from_id` — the player's current location.
pub fn commit_generated(raw: &str, from_id: &str, to_id: &str) -> Result<Location, String> {
    commit_generated_at(&store_path(), raw, from_id, to_id)
}

fn commit_generated_at(
    path: &Path,
    raw: &str,
    from_id: &str,
    to_id: &str,
) -> Result<Location, String> {
    let location = parse_generated(raw)
        .ok_or_else(|| "Модель вернула не JSON или локацию без названия".to_string())?;
    let to_id = to_id.trim().to_string();
    if to_id.is_empty() {
        return Err("Идентификатор локации не может быть пустым".to_string());
    }
    update_at(path, |world| {
        if !world.locations.contains_key(from_id) {
            return Err(format!("Локация «{from_id}» не существует"));
        }
        if world.locations.contains_key(&to_id) {
            return Err(format!("Локация «{to_id}» уже существует"));
        }
        world.add_location(to_id.clone(), location.clone());
        world.connect(from_id, &to_id);
        // Moving to a new place is the generation trigger (changelog:
        // "New locations are generated automatically on transition").
        world.current_location = to_id;
        Ok(location.clone())
    })
    .and_then(|result| result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_world_has_locations() {
        let world = World::default();
        assert!(world.locations.contains_key("town_square"));
        assert!(world.locations.contains_key("tavern"));
        assert!(world.locations.contains_key("market"));
        assert!(world.locations.contains_key("forest_path"));
        assert!(world.locations.contains_key("dungeon"));
        assert!(world.locations.contains_key("forest_cave"));
        assert!(world.locations.contains_key("castle"));
        assert_eq!(world.current_location, "town_square");
    }

    #[test]
    fn default_world_new_locations_reachable() {
        let mut world = World::default();
        // castle is reachable from town_square.
        let loc = world.move_to("castle").unwrap();
        assert_eq!(loc.name, "Замок");
        // forest_path → dungeon.
        world.current_location = "forest_path".into();
        let loc = world.move_to("dungeon").unwrap();
        assert_eq!(loc.name, "Подземелье");
        // dungeon → forest_cave.
        let loc = world.move_to("forest_cave").unwrap();
        assert_eq!(loc.name, "Лесная пещера");
    }

    #[test]
    fn move_to_connected() {
        let mut world = World::default();
        let loc = world.move_to("tavern").unwrap();
        assert_eq!(loc.name, "Таверна «Золотой дракон»");
        assert_eq!(world.current_location, "tavern");
    }

    #[test]
    fn move_to_unconnected_fails() {
        let mut world = World::default();
        world.move_to("tavern").unwrap();
        // tavern is not directly connected to forest_path
        assert!(world.move_to("forest_path").is_err());
    }

    #[test]
    fn available_exits() {
        let world = World::default();
        let exits = world.available_exits();
        assert!(exits.iter().any(|(_, name)| *name == "Таверна «Золотой дракон»"));
    }

    #[test]
    fn connect_adds_bidirectional() {
        let mut world = World::default();
        // Let's add mountain_pass manually to check the bidirectional link.
        world.add_location("mountain_pass".into(), Location {
            name: "Горный перевал".into(),
            description: "Узкий проход между скалами.".into(),
            tags: vec!["горы".into()],
            atmosphere: "ветреная".into(),
            connections: vec![],
            npcs: vec![],
            has_shop: false,
            shop_type: None,
            generated_by_ai: false,
        });
        world.connect("tavern", "mountain_pass");
        let tavern = world.locations.get("tavern").unwrap();
        assert!(tavern.connections.contains(&"mountain_pass".to_string()));
        let pass = world.locations.get("mountain_pass").unwrap();
        assert!(pass.connections.contains(&"tavern".to_string()));
    }

    #[test]
    fn world_note_contains_location() {
        let world = World::default();
        let note = world_note(&world);
        assert!(note.contains("Городская площадь"), "{note}");
        assert!(note.contains("оживлённая"), "{note}");
    }

    #[test]
    fn generation_prompt_contains_context() {
        let prompt = generation_prompt("Таверна", "лес", "уютная таверна с камином");
        assert!(prompt.contains("Таверна"), "{prompt}");
        assert!(prompt.contains("уютная таверна"), "{prompt}");
        assert!(prompt.contains("лес"), "{prompt}");
    }

    #[test]
    fn move_to_nonexistent_location() {
        let mut world = World::default();
        // Let's add a link to a nonexistent location in the CURRENT location (town_square).
        world.locations.get_mut("town_square").unwrap()
            .connections.push("dark_dungeon".to_string());
        let result = world.move_to("dark_dungeon");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("не существует"), "ожидали 'не существует', получили: {err}");
    }

    #[test]
    fn available_exits_empty_connections() {
        let mut world = World::default();
        world.add_location("empty_room".into(), Location {
            name: "Пустая комната".into(),
            description: "Пусто.".into(),
            tags: vec![],
            atmosphere: "тихая".into(),
            connections: vec![],
            npcs: vec![],
            has_shop: false,
            shop_type: None,
            generated_by_ai: false,
        });
        world.current_location = "empty_room".into();
        assert!(world.available_exits().is_empty());
    }

    #[test]
    fn world_note_empty_world() {
        let world = World {
            locations: BTreeMap::new(),
            current_location: "nowhere".into(),
        };
        assert!(world_note(&world).is_empty());
    }

    #[test]
    fn connect_nonexistent_location() {
        let mut world = World::default();
        // A link to a nonexistent location does not panic.
        world.connect("tavern", "void");
        // tavern must not get a link to void (void is not in locations).
        let tavern = world.locations.get("tavern").unwrap();
        assert!(!tavern.connections.contains(&"void".to_string()));
    }

    #[test]
    fn add_location_duplicate_updates() {
        let mut world = World::default();
        let updated = Location {
            name: "Новая площадь".into(),
            description: "Обновлённая.".into(),
            tags: vec![],
            atmosphere: "другая".into(),
            connections: vec![],
            npcs: vec![],
            has_shop: false,
            shop_type: None,
            generated_by_ai: false,
        };
        world.add_location("town_square".into(), updated.clone());
        let loc = world.locations.get("town_square").unwrap();
        assert_eq!(loc.name, "Новая площадь");
        assert_eq!(loc.atmosphere, "другая");
    }

    #[test]
    fn move_to_current_location() {
        let mut world = World::default();
        // town_square has no link to itself.
        let result = world.move_to("town_square");
        assert!(result.is_err());
    }

    #[test]
    fn set_shop_on_valid_location() {
        let mut world = World::default();
        assert!(world.set_shop(Some("оружие".into())).is_ok());
        let loc = world.current().unwrap();
        assert!(loc.has_shop);
        assert_eq!(loc.shop_type.as_deref(), Some("оружие"));
    }

    #[test]
    fn set_shop_remove() {
        let mut world = World::default();
        world.set_shop(Some("оружие".into())).unwrap();
        assert!(world.set_shop(None).is_ok());
        let loc = world.current().unwrap();
        assert!(!loc.has_shop);
        assert!(loc.shop_type.is_none());
    }

    #[test]
    fn set_shop_dangling_current_location() {
        let mut world = World {
            locations: BTreeMap::new(),
            current_location: "void".into(),
        };
        let result = world.set_shop(Some("test".into()));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("не найдена"));
    }

    #[test]
    fn connect_no_duplicates() {
        let mut world = World::default();
        // tavern is already connected to town_square in default.
        let before = world.locations.get("tavern").unwrap().connections.clone();
        world.connect("tavern", "town_square");
        let after = world.locations.get("tavern").unwrap().connections.clone();
        assert_eq!(before, after, "connect не должен добавлять дубликаты");
    }

    #[test]
    fn default_world_connections_valid() {
        let world = World::default();
        for (id, loc) in &world.locations {
            for conn in &loc.connections {
                assert!(
                    world.locations.contains_key(conn),
                    "локация «{id}» ссылается на несуществующую «{conn}»"
                );
            }
        }
    }

    #[test]
    fn move_to_dangling_current_location() {
        let mut world = World {
            locations: BTreeMap::new(),
            current_location: "ghost".into(),
        };
        let result = world.move_to("anywhere");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("не найдена"));
    }

    #[test]
    fn available_exits_preserves_connection_order() {
        let mut world = World::default();
        world.add_location("a".into(), Location {
            name: "A".into(), description: "".into(), tags: vec![],
            atmosphere: "".into(), connections: vec!["tavern".into(), "market".into()],
            npcs: vec![], has_shop: false, shop_type: None, generated_by_ai: false,
        });
        world.current_location = "a".into();
        let exits: Vec<&str> = world.available_exits().iter().map(|(id, _)| *id).collect();
        assert_eq!(exits, vec!["tavern", "market"]);
    }

    #[test]
    fn world_note_dangling_current_location() {
        let world = World {
            locations: BTreeMap::new(),
            current_location: "ghost".into(),
        };
        assert!(world_note(&world).is_empty());
    }

    fn temp_world_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("cloufam_test_world");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        path
    }

    /// Editing an unstarted world (transition, shop) refuses and does not create
    /// `world.json` — otherwise `started()` would rise forever.
    #[test]
    fn update_refuses_until_started_and_does_not_create_file() {
        let path = temp_world_path("not_started.json");
        let result = update_at(&path, |w| w.move_to("tavern").map(|l| l.name.clone())).unwrap();
        assert_eq!(result, Err(NOT_STARTED.to_string()));
        assert!(!path.exists(), "отказ создал world.json");
    }

    #[test]
    fn start_creates_default_world_and_keeps_existing() {
        let path = temp_world_path("start.json");
        assert!(start_at(&path).unwrap(), "первый старт должен создать мир");
        assert!(path.exists());
        let moved = update_at(&path, |w| w.move_to("tavern").map(|l| l.name.clone())).unwrap();
        assert!(moved.is_ok(), "{moved:?}");

        assert!(!start_at(&path).unwrap(), "повторный старт — без создания");
        let world: World = crate::json_store::load(&STORE, &path);
        assert_eq!(world.current_location, "tavern", "повторный старт перезаписал мир");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn failed_move_in_started_world_keeps_file_unchanged() {
        let path = temp_world_path("failed_move.json");
        start_at(&path).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        let result = update_at(&path, |w| w.move_to("dungeon").map(|l| l.name.clone())).unwrap();
        assert!(result.is_err(), "из площади нет пути в подземелье");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn update_after_reset_refuses() {
        let path = temp_world_path("reset.json");
        start_at(&path).unwrap();
        crate::json_store::remove(&STORE, &path).unwrap();
        let result = update_at(&path, |w| w.set_shop(None)).unwrap();
        assert_eq!(result, Err(NOT_STARTED.to_string()));
        assert!(!path.exists(), "правка после сброса воскресила мир");
    }

    /// V-01: a corrupt `world.json` is "broken", not the default city. The
    /// city used to reach the prompt and the first edit pinned it as real.
    #[test]
    fn broken_world_is_broken_not_default_city() {
        let path = temp_world_path("broken.json");
        let _ = std::fs::remove_file(path.with_extension("json.corrupt"));
        std::fs::write(&path, "not valid json {{{").unwrap();

        assert!(matches!(state_at(&path), WorldState::Broken));
        let world = load_at(&path);
        assert!(
            world.locations.is_empty(),
            "corrupt world returned default-city locations: {:?}",
            world.locations.keys().collect::<Vec<_>>()
        );
        assert!(world_note(&world).is_empty());

        // An edit of a corrupt world must not be written and must not pin the
        // city: json_store refuses to write over an unreadable file (V-09).
        let result = update_at(&path, |w| w.move_to("tavern").map(|l| l.name.clone()));
        assert!(result.is_err(), "edit of a corrupt world must be refused: {result:?}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "not valid json {{{",
            "corrupt file was overwritten"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("json.corrupt"));
    }

    /// No file — RPG is not started: neither broken nor "default city".
    #[test]
    fn missing_world_file_is_not_started() {
        let path = temp_world_path("missing_state.json");
        let _ = std::fs::remove_file(&path);
        assert!(matches!(state_at(&path), WorldState::NotStarted));
        assert!(load_at(&path).locations.is_empty());
    }

    /// A corrupt world does not count as started: panel and prompt hide it.
    #[test]
    fn broken_world_is_not_started() {
        let path = temp_world_path("broken_started.json");
        std::fs::write(&path, "}{}{").unwrap();
        assert!(!matches!(state_at(&path), WorldState::Ready(_)));
        assert!(!matches!(state_at(&path), WorldState::NotStarted));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("json.corrupt"));
    }

    /// A started world reads back as `Ready` in full.
    #[test]
    fn started_world_is_ready() {
        let path = temp_world_path("ready_state.json");
        start_at(&path).unwrap();
        match state_at(&path) {
            WorldState::Ready(world) => assert_eq!(world.current_location, "town_square"),
            _ => panic!("a started world must read as Ready"),
        }
        assert!(load_at(&path).locations.contains_key("tavern"));
        let _ = std::fs::remove_file(&path);
    }

    /// Location auto-generation (the "What's new" promise): `add_location` and
    /// bidirectional `connect` work live, the player enters the new place.
    #[test]
    fn commit_generated_creates_connected_location() {
        let path = temp_world_path("generated.json");
        start_at(&path).unwrap();
        let raw = r#"{"name": "Лесная чаща", "description": "Густые ели.", "tags": ["лес"], "atmosphere": "тёмная"}"#;
        let loc = commit_generated_at(&path, raw, "town_square", "dark_grove")
            .expect("локация создаётся");
        assert_eq!(loc.name, "Лесная чаща");
        assert!(loc.generated_by_ai);

        let world = load_at(&path);
        assert_eq!(world.current_location, "dark_grove", "игрок вошёл в новое место");
        assert!(
            world.locations["town_square"].connections.contains(&"dark_grove".to_string()),
            "связь из отправной точки"
        );
        assert!(
            world.locations["dark_grove"].connections.contains(&"town_square".to_string()),
            "связь двусторонняя"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Commit "only on success": garbage, a taken id, someone else's start point and
    /// an empty id do not touch the world.
    #[test]
    fn commit_generated_rejects_without_write() {
        let path = temp_world_path("generated_bad.json");
        start_at(&path).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        let raw = r#"{"name": "Что-то"}"#;

        assert!(commit_generated_at(&path, "мусор", "town_square", "x").is_err());
        assert!(commit_generated_at(&path, raw, "town_square", "town_square").is_err(), "id занят");
        assert!(commit_generated_at(&path, raw, "void", "new_loc").is_err(), "нет отправной");
        assert!(commit_generated_at(&path, raw, "town_square", "  ").is_err(), "пустой id");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "отказ не пишет файл"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn parse_generated_clamps_per_prompt_contract() {
        let raw = format!(
            r#"{{"name": "{}", "description": "{}", "tags": ["a","b","c","d","e","f","g"], "atmosphere": "{}"}}"#,
            "н".repeat(120),
            "о".repeat(900),
            "а".repeat(80),
        );
        let loc = parse_generated(&raw).expect("валидный ответ");
        assert_eq!(loc.name.chars().count(), 80);
        assert_eq!(loc.description.chars().count(), 600);
        assert_eq!(loc.tags.len(), 6);
        assert_eq!(loc.atmosphere.chars().count(), 40);
        assert!(loc.generated_by_ai);
    }

    #[test]
    fn parse_generated_rejects_garbage_and_empty_name() {
        assert!(parse_generated("не json").is_none());
        assert!(parse_generated(r#"{"name": "  "}"#).is_none());
        assert!(parse_generated(r#"{"description": "без названия"}"#).is_none());
    }
}

