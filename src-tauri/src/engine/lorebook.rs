//! Lorebook: world entries that enter the prompt when their key occurs
//! in recent messages. Constant entries always enter.
//!
//! Import understands two open JSON shapes: the `character_book` of a V2/V3 card
//! (`entries` — array, `keys`/`enabled`/`insertion_order`) and a world file
//! (`entries` — object, `key`/`disable`/`order`).

use std::path::Path;

use serde_json::Value;

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct LoreEntry {
    pub title: String,
    pub keys: Vec<String>,
    pub content: String,
    pub enabled: bool,
    /// Enters the prompt without a key.
    pub constant: bool,
    /// A higher priority is inserted earlier and evicted last by the budget.
    pub priority: i32,
    pub case_sensitive: bool,
}

impl Default for LoreEntry {
    fn default() -> Self {
        Self {
            title: String::new(),
            keys: Vec::new(),
            content: String::new(),
            enabled: true,
            constant: false,
            priority: 100,
            case_sensitive: false,
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Lorebook {
    pub name: String,
    /// How many recent messages to scan for keys.
    pub scan_depth: usize,
    /// Lore ceiling in the prompt, tokens (estimated by `estimate_tokens`).
    pub token_budget: usize,
    pub entries: Vec<LoreEntry>,
}

impl Default for Lorebook {
    fn default() -> Self {
        Self { name: String::new(), scan_depth: 4, token_budget: 1024, entries: Vec::new() }
    }
}

impl Lorebook {
    /// Import from the card's `character_book` or a world file.
    pub fn from_import(value: &Value, fallback_name: &str) -> Self {
        let name = value
            .get("name")
            .and_then(Value::as_str)
            .filter(|n| !n.trim().is_empty())
            .unwrap_or(fallback_name)
            .to_string();
        let raw: Vec<&Value> = match value.get("entries") {
            Some(Value::Array(list)) => list.iter().collect(),
            Some(Value::Object(map)) => map.values().collect(),
            _ => Vec::new(),
        };
        let mut book = Lorebook { name, ..Lorebook::default() };
        if let Some(depth) = value.get("scan_depth").and_then(Value::as_u64) {
            book.scan_depth = depth.clamp(1, 100) as usize;
        }
        if let Some(budget) = value.get("token_budget").and_then(Value::as_u64) {
            book.token_budget = budget.clamp(64, 16384) as usize;
        }
        book.entries = raw.into_iter().filter_map(entry_from_import).collect();
        book
    }

    pub fn load(path: &Path) -> Result<Option<Self>, String> {
        crate::json_store::read_state(path)
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        crate::json_store::save_value(self, path)
    }

    /// Entries for the prompt by recent messages (newest at the end of `history`).
    pub fn activate<'a>(&'a self, history: &[&str]) -> Vec<&'a LoreEntry> {
        let window = &history[history.len().saturating_sub(self.scan_depth)..];
        let haystack = window.join("\n");
        let haystack_lower = haystack.to_lowercase();
        let mut active: Vec<&LoreEntry> = self
            .entries
            .iter()
            .filter(|e| e.enabled && !e.content.trim().is_empty())
            .filter(|e| {
                e.constant
                    || e.keys.iter().map(|k| k.trim()).filter(|k| !k.is_empty()).any(|key| {
                        if e.case_sensitive {
                            haystack.contains(key)
                        } else {
                            haystack_lower.contains(&key.to_lowercase())
                        }
                    })
            })
            .collect();
        // Stable sort: equal priority keeps lorebook order.
        active.sort_by_key(|e| std::cmp::Reverse(e.priority));
        let mut used = 0;
        active.retain(|e| {
            let cost = crate::gateway::estimate_tokens(&e.content);
            if used + cost > self.token_budget {
                return false;
            }
            used += cost;
            true
        });
        active
    }
}

fn entry_from_import(value: &Value) -> Option<LoreEntry> {
    let content = value.get("content").and_then(Value::as_str)?.to_string();
    let strings = |key: &str| -> Vec<String> {
        value
            .get(key)
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default()
    };
    let keys = if value.get("keys").is_some() { strings("keys") } else { strings("key") };
    let enabled = match (value.get("enabled").and_then(Value::as_bool), value.get("disable").and_then(Value::as_bool)) {
        (Some(enabled), _) => enabled,
        (None, Some(disable)) => !disable,
        (None, None) => true,
    };
    let priority = value
        .get("insertion_order")
        .or_else(|| value.get("order"))
        .and_then(Value::as_i64)
        .unwrap_or(100)
        .clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    let title = ["name", "comment"]
        .iter()
        .find_map(|k| value.get(*k).and_then(Value::as_str).filter(|t| !t.trim().is_empty()))
        .unwrap_or_default()
        .to_string();
    Some(LoreEntry {
        title,
        keys,
        content,
        enabled,
        constant: value.get("constant").and_then(Value::as_bool).unwrap_or(false),
        priority,
        case_sensitive: ["case_sensitive", "caseSensitive"]
            .iter()
            .find_map(|k| value.get(*k).and_then(Value::as_bool))
            .unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(keys: &[&str], content: &str) -> LoreEntry {
        LoreEntry { keys: keys.iter().map(|k| k.to_string()).collect(), content: content.into(), ..LoreEntry::default() }
    }

    fn contents(active: Vec<&LoreEntry>) -> Vec<&str> {
        active.iter().map(|e| e.content.as_str()).collect()
    }

    #[test]
    fn test_key_match_is_case_insensitive_by_default() {
        let book = Lorebook { entries: vec![entry(&["Элдория"], "Лес-убежище")], ..Lorebook::default() };
        assert_eq!(contents(book.activate(&["Что такое ЭЛДОРИЯ?"])), vec!["Лес-убежище"]);
        assert!(book.activate(&["Расскажи о городе"]).is_empty());
    }

    #[test]
    fn test_case_sensitive_entry() {
        let mut e = entry(&["Ra"], "Бог солнца");
        e.case_sensitive = true;
        let book = Lorebook { entries: vec![e], ..Lorebook::default() };
        assert!(book.activate(&["rain"]).is_empty());
        assert_eq!(book.activate(&["Ra смотрит"]).len(), 1);
    }

    #[test]
    fn test_scan_depth_limits_window() {
        let book = Lorebook { scan_depth: 2, entries: vec![entry(&["дракон"], "Огнедышащий")], ..Lorebook::default() };
        assert!(book.activate(&["дракон", "привет", "как дела"]).is_empty());
        assert_eq!(book.activate(&["привет", "дракон", "как дела"]).len(), 1);
    }

    #[test]
    fn test_constant_disabled_and_empty_keys() {
        let mut constant = entry(&[], "Всегда");
        constant.constant = true;
        let mut disabled = entry(&["лес"], "Выключено");
        disabled.enabled = false;
        let blank = entry(&["  "], "Пустой ключ");
        let book = Lorebook { entries: vec![constant, disabled, blank], ..Lorebook::default() };
        assert_eq!(contents(book.activate(&["лес"])), vec!["Всегда"]);
    }

    #[test]
    fn test_budget_keeps_high_priority_first() {
        let mut low = entry(&["a"], &"x".repeat(300));
        low.priority = 1;
        let mut high = entry(&["a"], &"y".repeat(300));
        high.priority = 200;
        // 300 ASCII ≈ 100 tokens; the budget lets one entry through.
        let book = Lorebook { token_budget: 150, entries: vec![low, high], ..Lorebook::default() };
        assert_eq!(contents(book.activate(&["a"])), vec!["y".repeat(300).as_str()]);
    }

    #[test]
    fn test_import_world_file_and_character_book() {
        let world = json!({ "entries": {
            "0": { "key": ["eldoria", "forest"], "content": "Лес", "comment": "eldoria", "disable": false, "order": 100, "caseSensitive": null },
            "1": { "key": ["x"], "content": "Выкл", "disable": true }
        }});
        let book = Lorebook::from_import(&world, "Eldoria");
        assert_eq!(book.name, "Eldoria");
        assert_eq!(book.entries.len(), 2);
        let first = book.entries.iter().find(|e| e.content == "Лес").unwrap();
        assert_eq!((first.title.as_str(), first.keys.len(), first.enabled), ("eldoria", 2, true));
        assert!(!book.entries.iter().find(|e| e.content == "Выкл").unwrap().enabled);

        let card_book = json!({ "name": "Книга", "scan_depth": 6, "entries": [
            { "keys": ["меч"], "content": "Клинок", "enabled": true, "insertion_order": 7, "constant": true, "case_sensitive": true }
        ]});
        let book = Lorebook::from_import(&card_book, "запасное");
        assert_eq!((book.name.as_str(), book.scan_depth), ("Книга", 6));
        let e = &book.entries[0];
        assert_eq!((e.priority, e.constant, e.case_sensitive), (7, true, true));
    }
}
