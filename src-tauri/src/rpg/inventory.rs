//! Inventory and trade system: items, rarity, prices.
//! Trade through a dialogue with an AI merchant.
//! Prices depend on rarity, location and the relationship with the merchant.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::paths;

/// An item in the inventory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventoryItem {
    pub id: String,
    pub name: String,
    pub description: String,
    pub rarity: Rarity,
    pub base_price: u32,
    pub tags: Vec<String>,
    pub quantity: i32, // -1 = infinite (for shops)
}

/// Item rarity.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Rarity {
    Common,
    Uncommon,
    Rare,
    Epic,
    Legendary,
}

impl Rarity {
    /// Price multiplier by rarity.
    pub fn price_mult(&self) -> f32 {
        match self {
            Rarity::Common => 1.0,
            Rarity::Uncommon => 1.5,
            Rarity::Rare => 2.5,
            Rarity::Epic => 4.0,
            Rarity::Legendary => 6.0,
        }
    }

    /// Name in Russian.
    pub fn display_name(&self) -> &'static str {
        match self {
            Rarity::Common => "обычный",
            Rarity::Uncommon => "необычный",
            Rarity::Rare => "редкий",
            Rarity::Epic => "эпический",
            Rarity::Legendary => "легендарный",
        }
    }
}

/// Location type for the price multiplier.
pub fn location_price_mult(location_tags: &[String]) -> f32 {
    // The model writes tags of generated locations however it likes ("Лес", " город")
    let has = |names: &[&str]| location_tags.iter().any(|t| names.contains(&t.trim().to_lowercase().as_str()));
    if has(&["город"]) {
        0.8
    } else if has(&["деревня"]) {
        1.0
    } else if has(&["лес", "поле"]) {
        1.5
    } else if has(&["подземелье", "пещера"]) {
        2.0
    } else {
        1.0
    }
}

/// Price multiplier by the relationship with the merchant.
/// Friendship lowers the price, hatred raises it.
pub fn relationship_price_mult(friendship: f32) -> f32 {
    // friendship: -100..+100 → mult: 1.5..0.5
    (1.0 - friendship / 200.0).clamp(0.5, 1.5)
}

/// Final price of the item.
pub fn final_price(
    item: &InventoryItem,
    location_tags: &[String],
    friendship: f32,
) -> u32 {
    let base = item.base_price as f32;
    let rarity = item.rarity.price_mult();
    let location = location_price_mult(location_tags);
    let relationship = relationship_price_mult(friendship);
    (base * rarity * location * relationship).round() as u32
}

/// Payment value of the item: `final_price` under neutral conditions (without
/// location and relationship markups). It is paid in kind — bribe, ransom (V-05:
/// the world model has no currency, and "I give 200" from the model's reply cost nothing).
pub fn item_value(item: &InventoryItem) -> u32 {
    final_price(item, &[], 0.0)
}

/// Inventory: player + NPC.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InventoryStore {
    pub player: Vec<InventoryItem>,
    pub npcs: BTreeMap<String, Vec<InventoryItem>>,
}

impl InventoryStore {
    /// Add an item to the player.
    pub fn add_to_player(&mut self, item: InventoryItem) {
        if item.quantity == 0 {
            return;
        }
        if item.quantity > 0 {
            // Stack cap: clamp the incoming quantity and the sum (i32 overflow / UI).
            let qty = item.quantity.min(9999);
            if let Some(existing) = self.player.iter_mut().find(|i| i.id == item.id && i.quantity >= 0) {
                existing.quantity = existing.quantity.saturating_add(qty).min(9999);
                return;
            }
            self.player.push(InventoryItem { quantity: qty, ..item });
        } else if self.player.iter().any(|i| i.id == item.id && i.quantity < 0) {
            return;
        } else {
            self.player.push(item);
        }
    }

    /// Remove an item from the player's inventory.
    pub fn remove_from_player(&mut self, item_id: &str, quantity: i32) -> bool {
        if quantity <= 0 {
            return false;
        }
        // Look for a finite item — infinite ones (quantity < 0) cannot be removed.
        if let Some(idx) = self.player.iter().position(|i| i.id == item_id && i.quantity >= 0) {
            if quantity >= self.player[idx].quantity {
                self.player.remove(idx);
            } else {
                self.player[idx].quantity -= quantity;
            }
            true
        } else {
            false
        }
    }

    /// Give an item to an NPC (gift, bribe, goods to the counter).
    /// Merge into a stack and the cap — like `add_to_player`: previously a plain `push`
    /// accumulated duplicates without a cap, which would have surfaced with gift recording.
    pub fn give_to_npc(&mut self, npc_id: &str, item: InventoryItem) {
        if item.quantity == 0 {
            return;
        }
        let entry = self.npcs.entry(npc_id.to_string()).or_default();
        if item.quantity > 0 {
            let qty = item.quantity.min(9999);
            if let Some(existing) = entry.iter_mut().find(|i| i.id == item.id && i.quantity >= 0) {
                existing.quantity = existing.quantity.saturating_add(qty).min(9999);
                return;
            }
            entry.push(InventoryItem { quantity: qty, ..item });
        } else if entry.iter().any(|i| i.id == item.id && i.quantity < 0) {
            return;
        } else {
            entry.push(item);
        }
    }
}

fn store_path() -> PathBuf {
    paths::base_dir().join("data").join("inventory.json")
}

static STORE: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn load() -> InventoryStore {
    crate::json_store::load(&STORE, &store_path())
}

pub fn update<T>(change: impl FnOnce(&mut InventoryStore) -> T) -> Result<T, String> {
    crate::json_store::update(&STORE, &store_path(), change)
}

/// Like `update`, but the file is written only on `Ok` from `change` — the
/// "commit only on success" contract for writers that accept a model decision
/// (purchase, gift).
pub fn update_if_ok<T, E>(
    change: impl FnOnce(&mut InventoryStore) -> Result<T, E>,
) -> Result<Result<T, E>, String> {
    crate::json_store::update_if_ok(&STORE, &store_path(), change)
}

/// Deduct goods worth `budget` from the player (estimate by `item_value`) as
/// payment in kind. `None` — the amount is not covered: nothing is removed
/// (the plan is built before the first change). An infinite shop stock
/// (quantity < 0) is not spent, zero-priced items do not serve as payment.
pub fn pay_value(store: &mut InventoryStore, budget: u32) -> Option<Vec<InventoryItem>> {
    if budget == 0 {
        return Some(Vec::new());
    }
    // Candidates: the player's finite stacks with a positive price. Cheap first
    // (we pay with small change before relics), on a tie — by id for
    // determinism.
    let mut candidates: Vec<(usize, u32)> = store
        .player
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| {
            let unit = item_value(item);
            (item.quantity > 0 && unit > 0).then_some((idx, unit))
        })
        .collect();
    candidates.sort_by(|a, b| {
        a.1.cmp(&b.1)
            .then_with(|| store.player[a.0].id.cmp(&store.player[b.0].id))
    });

    let mut need = u64::from(budget);
    let mut plan: Vec<(usize, i32)> = Vec::new();
    for (idx, unit) in candidates {
        if need == 0 {
            break;
        }
        // quantity > 0 filtered by the candidates — the u64 conversion is safe.
        let available = store.player[idx].quantity as u64;
        let wanted = need.div_ceil(u64::from(unit));
        let take = wanted.min(available) as i32;
        plan.push((idx, take));
        // take > 0: wanted and available are positive — the u64 conversion is safe.
        need = need.saturating_sub(u64::from(unit) * (take as u64));
    }
    if need > 0 {
        // Not enough — nothing touched (commit only on success).
        return None;
    }

    let mut removed = Vec::new();
    // Stack templates are taken before the first change — indices shift after
    // removal. Deduction goes through `remove_from_player` (the standard removal
    // of player items). Duplicate ids with different prices are possible only from
    // a manual JSON: then the first finite stack of that id is removed,
    // and payment does not become cheaper than the plan (coverage is guaranteed).
    let steps: Vec<(InventoryItem, i32)> = plan
        .iter()
        .map(|(idx, take)| (store.player[*idx].clone(), *take))
        .collect();
    for (item, take) in steps {
        if store.remove_from_player(&item.id, take) {
            removed.push(InventoryItem {
                quantity: take,
                ..item
            });
        }
    }
    Some(removed)
}

/// Stable reward id from its name: the line index gave two different quests
/// with a reward at the same position the same id, and the second thing silently merged
/// with the first by quantity. FNV-1a — dependency-free, deterministic.
fn reward_id(name: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in name.to_lowercase().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("quest_reward_{hash:016x}")
}

/// Grant the player items by reward lines (id or name from
/// `default_shop_items`). An unknown line is not silently lost: the item
/// is created with zero market value (it cannot serve as "currency").
/// Returns what was actually granted.
pub fn grant_items(store: &mut InventoryStore, rewards: &[String]) -> Vec<InventoryItem> {
    let catalog = default_shop_items();
    let mut granted = Vec::new();
    for name in rewards {
        let key = name.trim();
        if key.is_empty() {
            continue;
        }
        let mut item = catalog
            .iter()
            // to_lowercase: ASCII comparison does not fold Cyrillic, and "Зелье Здоровья"
            // became a priceless reward item instead of the catalog one
            .find(|i| i.id.to_lowercase() == key.to_lowercase() || i.name.to_lowercase() == key.to_lowercase())
            .cloned()
            .unwrap_or_else(|| InventoryItem {
                id: reward_id(key),
                name: key.to_string(),
                description: String::new(),
                rarity: Rarity::Common,
                base_price: 0,
                tags: vec!["награда".to_string()],
                quantity: 1,
            });
        if item.quantity < 0 {
            // The shop stock is infinite; the reward is a real item.
            item.quantity = 1;
        }
        granted.push(item.clone());
        store.add_to_player(item);
    }
    granted
}

/// Purchase from an NPC in kind (the world model has no currency): price — `final_price`
/// (location + relationship), payment — the player's goods by `item_value`
/// (`pay_value`), the goods leave the NPC's counter. Commit "only on success":
/// if funds or goods are insufficient, nothing in the store changes.
pub fn commit_purchase(
    store: &mut InventoryStore,
    npc_id: &str,
    item_id: &str,
    quantity: i32,
    location_tags: &[String],
    friendship: f32,
) -> Result<u32, String> {
    if quantity <= 0 {
        return Err("Количество должно быть больше нуля".to_string());
    }
    // Take everything needed from the counter before payment: the store borrow is free afterwards.
    let (template, in_stock) = {
        let stock = store
            .npcs
            .get(npc_id)
            .ok_or_else(|| format!("У {npc_id} нет прилавка"))?;
        let item = stock
            .iter()
            .find(|i| i.id == item_id && i.quantity != 0)
            .ok_or_else(|| format!("«{item_id}» нет в наличии"))?;
        if item.quantity > 0 && item.quantity < quantity {
            return Err(format!("«{}» в наличии только {}", item.name, item.quantity));
        }
        (item.clone(), item.quantity)
    };
    // The player's stack caps at 9999: no selling beyond it, or the surplus is paid for and lost
    let held = store.player.iter().find(|i| i.id == item_id && i.quantity >= 0).map_or(0, |i| i.quantity);
    let quantity = quantity.min(9999 - held.min(9999));
    if quantity <= 0 {
        return Err(format!("«{}» больше не помещается в инвентарь", template.name));
    }
    let unit_price = final_price(&template, location_tags, friendship);
    let total = (u64::from(unit_price) * u64::from(quantity.max(0) as u32))
        .min(u64::from(u32::MAX)) as u32;

    // Payment first: pay_value touches nothing when funds are insufficient.
    let paid = pay_value(store, total).ok_or_else(|| "Не хватает ценностей на покупку".to_string())?;
    // Payment goes to the merchant, as with a bribe: unique valuables must not disappear
    for item in paid {
        store.give_to_npc(npc_id, item);
    }

    if in_stock > 0 {
        if let Some(stock) = store.npcs.get_mut(npc_id) {
            if let Some(entry) = stock.iter_mut().find(|i| i.id == item_id) {
                entry.quantity = (entry.quantity - quantity).max(0);
            }
            stock.retain(|i| i.quantity != 0);
        }
    }
    store.add_to_player(InventoryItem {
        quantity,
        ..template
    });
    Ok(total)
}

/// Text for the prompt: the player's inventory.
pub fn inventory_note(store: &InventoryStore) -> Option<String> {
    if store.player.is_empty() {
        return None;
    }
    let items: Vec<String> = store
        .player
        .iter()
        .map(|item| {
            let qty = if item.quantity < 0 {
                "∞".to_string()
            } else {
                item.quantity.to_string()
            };
            format!("{} ×{}", item.name, qty)
        })
        .collect();
    Some(format!("Инвентарь: {}", items.join(", ")))
}

/// Prompt for trading with an NPC.
pub fn trade_prompt(
    npc_name: &str,
    npc_items: &[InventoryItem],
    player_items: &[InventoryItem],
    friendship: f32,
    location_tags: &[String],
    trader_description: &str,
) -> String {
    let npc_list: Vec<String> = npc_items
        .iter()
        .map(|i| format!("{} ({} монет)", i.name, final_price(i, location_tags, friendship)))
        .collect();
    let player_list: Vec<String> = player_items
        .iter()
        .map(|i| format!("{} ×{}", i.name, if i.quantity < 0 { "∞".to_string() } else { i.quantity.to_string() }))
        .collect();

    let desc_part = if trader_description.is_empty() {
        String::new()
    } else {
        format!(" {trader_description}")
    };

    let relationship_hint = if friendship >= 50.0 {
        "Он твой друг — будь сговорчивее."
    } else if friendship <= -50.0 {
        "Он тебе враг — будь подозрительнее."
    } else {
        "Отношения нейтральные."
    };

    let location_str = if location_tags.is_empty() {
        "неизвестно".to_string()
    } else {
        location_tags.join(", ")
    };

    format!(
        "Ты — торговец {npc_name}.{desc_part} \
У тебя на прилавке: {npc_list}. \
У покупателя: {player_list}. \
Отношения с покупателем: дружба {friendship}/100. {relationship_hint} \
Местоположение: {location_str}.\n\n\
Покупатель хочет торговаться. Ты можешь предложить скидку, запросить больше \
или обменять товар на товар. Учитывай своё местоположение и отношения. \
Будь живым торговцем.",
        npc_name = npc_name,
        desc_part = desc_part,
        npc_list = npc_list.join(", "),
        player_list = if player_list.is_empty() { "ничего".to_string() } else { player_list.join(", ") },
        friendship = friendship as i32,
        relationship_hint = relationship_hint,
        location_str = location_str,
    )
}

/// Default items for shops.
pub fn default_shop_items() -> Vec<InventoryItem> {
    vec![
        // Potions
        InventoryItem {
            id: "health_potion".into(),
            name: "Зелье здоровья".into(),
            description: "Восстанавливает 30 HP.".into(),
            rarity: Rarity::Common,
            base_price: 20,
            tags: vec!["зелье".into(), "лечение".into()],
            quantity: -1,
        },
        InventoryItem {
            id: "mana_potion".into(),
            name: "Зелье маны".into(),
            description: "Восстанавливает 30 MP.".into(),
            rarity: Rarity::Common,
            base_price: 25,
            tags: vec!["зелье".into(), "магия".into()],
            quantity: -1,
        },
        InventoryItem {
            id: "antidote".into(),
            name: "Противоядие".into(),
            description: "Снимает отравление.".into(),
            rarity: Rarity::Common,
            base_price: 15,
            tags: vec!["зелье".into(), "лечение".into()],
            quantity: -1,
        },
        // Weapons
        InventoryItem {
            id: "iron_sword".into(),
            name: "Железный меч".into(),
            description: "Простой, но надёжный меч.".into(),
            rarity: Rarity::Common,
            base_price: 50,
            tags: vec!["оружие".into(), "меч".into()],
            quantity: -1,
        },
        InventoryItem {
            id: "steel_sword".into(),
            name: "Стальной меч".into(),
            description: "Крепкий меч из закалённой стали.".into(),
            rarity: Rarity::Uncommon,
            base_price: 120,
            tags: vec!["оружие".into(), "меч".into()],
            quantity: -1,
        },
        InventoryItem {
            id: "hunting_bow".into(),
            name: "Охотничий лук".into(),
            description: "Лёгкий лук для охоты и боя.".into(),
            rarity: Rarity::Uncommon,
            base_price: 90,
            tags: vec!["оружие".into(), "лук".into()],
            quantity: -1,
        },
        // Armor
        InventoryItem {
            id: "leather_armor".into(),
            name: "Кожаная броня".into(),
            description: "Лёгкая, но прочная броня.".into(),
            rarity: Rarity::Common,
            base_price: 80,
            tags: vec!["броня".into()],
            quantity: -1,
        },
        // Food
        InventoryItem {
            id: "bread".into(),
            name: "Хлеб".into(),
            description: "Свежий хлеб. Восстанавливает 5 HP.".into(),
            rarity: Rarity::Common,
            base_price: 5,
            tags: vec!["еда".into(), "лечение".into()],
            quantity: -1,
        },
        // Magic items
        InventoryItem {
            id: "fire_scroll".into(),
            name: "Свиток огня".into(),
            description: "Одноразовый свиток с заклинанием огненного шара.".into(),
            rarity: Rarity::Rare,
            base_price: 150,
            tags: vec!["магия".into(), "расходник".into()],
            quantity: -1,
        },
        // Gifts
        InventoryItem {
            id: "silver_earrings".into(),
            name: "Серебряные серьги".into(),
            description: "Красивые серьги из серебра. Подарок.".into(),
            rarity: Rarity::Uncommon,
            base_price: 80,
            tags: vec!["подарок".into(), "украшение".into()],
            quantity: -1,
        },
        InventoryItem {
            id: "enchanted_ring".into(),
            name: "Зачарованное кольцо".into(),
            description: "Кольцо с магической аурой.".into(),
            rarity: Rarity::Rare,
            base_price: 200,
            tags: vec!["подарок".into(), "украшение".into(), "магия".into()],
            quantity: 1,
        },
        InventoryItem {
            id: "gold_necklace".into(),
            name: "Золотое ожерелье".into(),
            description: "Роскошное ожерелье из чистого золота.".into(),
            rarity: Rarity::Rare,
            base_price: 300,
            tags: vec!["подарок".into(), "украшение".into()],
            quantity: -1,
        },
        InventoryItem {
            id: "diamond_ring".into(),
            name: "Бриллиантовое кольцо".into(),
            description: "Редкое кольцо с бриллиантом. Бесценный подарок.".into(),
            rarity: Rarity::Epic,
            base_price: 500,
            tags: vec!["подарок".into(), "украшение".into()],
            quantity: 1,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unknown rewards with the same position in different quests got
    /// one id `quest_reward_0` and merged into one inventory line.
    #[test]
    fn unknown_rewards_with_same_position_get_distinct_ids() {
        let mut store = InventoryStore::default();
        let first = grant_items(&mut store, &["Серебряный амулет".to_string()]);
        let second = grant_items(&mut store, &["Клык волка".to_string()]);
        assert_ne!(first[0].id, second[0].id);
        assert_eq!(store.player.len(), 2);
        assert!(store.player.iter().any(|i| i.name == "Серебряный амулет"));
        assert!(store.player.iter().any(|i| i.name == "Клык волка"));
    }

    #[test]
    fn rarity_price_mult() {
        assert_eq!(Rarity::Common.price_mult(), 1.0);
        assert_eq!(Rarity::Legendary.price_mult(), 6.0);
    }

    #[test]
    fn location_price_mult_city() {
        let tags = vec!["город".to_string()];
        assert_eq!(location_price_mult(&tags), 0.8);
    }

    #[test]
    fn location_price_mult_dungeon() {
        let tags = vec!["подземелье".to_string()];
        assert_eq!(location_price_mult(&tags), 2.0);
    }

    #[test]
    fn relationship_price_mult_friend() {
        let mult = relationship_price_mult(80.0);
        assert!((mult - 0.6).abs() < 0.01, "mult={mult}");
    }

    #[test]
    fn relationship_price_mult_enemy() {
        let mult = relationship_price_mult(-80.0);
        assert!((mult - 1.4).abs() < 0.01, "mult={mult}");
    }

    #[test]
    fn final_price_calculation() {
        let item = InventoryItem {
            id: "test".into(),
            name: "Test".into(),
            description: "".into(),
            rarity: Rarity::Rare,
            base_price: 100,
            tags: vec![],
            quantity: 1,
        };
        // Rare (2.5) * city (0.8) * friend (0.6) = 1.2 → 120
        let price = final_price(&item, &["город".to_string()], 80.0);
        assert_eq!(price, 120);
    }

    #[test]
    fn inventory_add_and_remove() {
        let mut inv = InventoryStore::default();
        inv.add_to_player(InventoryItem {
            id: "sword".into(),
            name: "Меч".into(),
            description: "".into(),
            rarity: Rarity::Common,
            base_price: 50,
            tags: vec![],
            quantity: 1,
        });
        assert_eq!(inv.player.len(), 1);
        assert!(inv.remove_from_player("sword", 1));
        assert!(inv.player.is_empty());
    }

    #[test]
    fn inventory_stack() {
        let mut inv = InventoryStore::default();
        inv.add_to_player(InventoryItem {
            id: "potion".into(),
            name: "Зелье".into(),
            description: "".into(),
            rarity: Rarity::Common,
            base_price: 10,
            tags: vec![],
            quantity: 1,
        });
        inv.add_to_player(InventoryItem {
            id: "potion".into(),
            name: "Зелье".into(),
            description: "".into(),
            rarity: Rarity::Common,
            base_price: 10,
            tags: vec![],
            quantity: 1,
        });
        assert_eq!(inv.player.len(), 1);
        assert_eq!(inv.player[0].quantity, 2);
    }

    #[test]
    fn inventory_note_format() {
        let mut inv = InventoryStore::default();
        inv.add_to_player(InventoryItem {
            id: "potion".into(),
            name: "Зелье".into(),
            description: "".into(),
            rarity: Rarity::Common,
            base_price: 10,
            tags: vec![],
            quantity: 3,
        });
        let note = inventory_note(&inv).unwrap();
        assert!(note.contains("Зелье ×3"), "{note}");
    }

    #[test]
    fn trade_prompt_format() {
        let items = default_shop_items();
        let prompt = trade_prompt("Торговец Джек", &items, &[], 30.0, &[], "Продавец зелий в трактире.");
        assert!(prompt.contains("Торговец Джек"), "{prompt}");
        assert!(prompt.contains("Зелье здоровья"), "{prompt}");
        assert!(prompt.contains("Продавец зелий в трактире."), "{prompt}");
        assert!(prompt.contains("Отношения нейтральные"), "{prompt}");
    }

    #[test]
    fn trade_prompt_uses_final_price() {
        let items = vec![InventoryItem {
            id: "ring".into(),
            name: "Кольцо".into(),
            description: "".into(),
            rarity: Rarity::Rare,
            base_price: 100,
            tags: vec![],
            quantity: 1,
        }];
        // Rare (2.5) * friendship 100 (mult 0.5) = 125
        let prompt = trade_prompt("Торговец", &items, &[], 100.0, &[], "");
        assert!(prompt.contains("125 монет"), "{prompt}");
    }

    #[test]
    fn trade_prompt_with_location() {
        let items = vec![InventoryItem {
            id: "ring".into(),
            name: "Кольцо".into(),
            description: "".into(),
            rarity: Rarity::Rare,
            base_price: 100,
            tags: vec![],
            quantity: 1,
        }];
        // Rare (2.5) * dungeon (2.0) * friendship 0 (1.0) = 500
        let location = vec!["подземелье".to_string()];
        let prompt = trade_prompt("Торговец", &items, &[], 0.0, &location, "");
        assert!(prompt.contains("500 монет"), "{prompt}");
        assert!(prompt.contains("подземелье"), "{prompt}");
    }

    #[test]
    fn trade_prompt_friend_hint() {
        let prompt = trade_prompt("Торговец", &[], &[], 80.0, &[], "");
        assert!(prompt.contains("сговорчивее"), "{prompt}");
    }

    #[test]
    fn trade_prompt_enemy_hint() {
        let prompt = trade_prompt("Торговец", &[], &[], -80.0, &[], "");
        assert!(prompt.contains("подозрительнее"), "{prompt}");
    }

    #[test]
    fn add_infinite_item_does_not_stack_with_finite() {
        let mut inv = InventoryStore::default();
        inv.add_to_player(InventoryItem {
            id: "potion".into(),
            name: "Зелье".into(),
            description: "".into(),
            rarity: Rarity::Common,
            base_price: 10,
            tags: vec![],
            quantity: 3,
        });
        // An infinite item does not stack with a finite one.
        inv.add_to_player(InventoryItem {
            id: "potion".into(),
            name: "Зелье".into(),
            description: "".into(),
            rarity: Rarity::Common,
            base_price: 10,
            tags: vec![],
            quantity: -1,
        });
        assert_eq!(inv.player.len(), 2);
        assert_eq!(inv.player[0].quantity, 3);
        assert_eq!(inv.player[1].quantity, -1);
    }

    #[test]
    fn add_duplicate_infinite_item_is_noop() {
        let mut inv = InventoryStore::default();
        inv.add_to_player(InventoryItem {
            id: "potion".into(),
            name: "Зелье".into(),
            description: "".into(),
            rarity: Rarity::Common,
            base_price: 10,
            tags: vec![],
            quantity: -1,
        });
        inv.add_to_player(InventoryItem {
            id: "potion".into(),
            name: "Зелье".into(),
            description: "".into(),
            rarity: Rarity::Common,
            base_price: 10,
            tags: vec![],
            quantity: -1,
        });
        assert_eq!(inv.player.len(), 1);
        assert_eq!(inv.player[0].quantity, -1);
    }

    #[test]
    fn remove_infinite_returns_false() {
        let mut inv = InventoryStore::default();
        inv.add_to_player(InventoryItem {
            id: "potion".into(),
            name: "Зелье".into(),
            description: "".into(),
            rarity: Rarity::Common,
            base_price: 10,
            tags: vec![],
            quantity: -1,
        });
        assert!(!inv.remove_from_player("potion", 1));
        assert_eq!(inv.player.len(), 1);
    }

    #[test]
    fn remove_zero_quantity_returns_false() {
        let mut inv = InventoryStore::default();
        inv.add_to_player(InventoryItem {
            id: "sword".into(),
            name: "Меч".into(),
            description: "".into(),
            rarity: Rarity::Common,
            base_price: 50,
            tags: vec![],
            quantity: 1,
        });
        assert!(!inv.remove_from_player("sword", 0));
        assert_eq!(inv.player.len(), 1);
        assert_eq!(inv.player[0].quantity, 1);
    }

    #[test]
    fn remove_negative_quantity_returns_false() {
        let mut inv = InventoryStore::default();
        inv.add_to_player(InventoryItem {
            id: "sword".into(),
            name: "Меч".into(),
            description: "".into(),
            rarity: Rarity::Common,
            base_price: 50,
            tags: vec![],
            quantity: 5,
        });
        assert!(!inv.remove_from_player("sword", -1));
        assert_eq!(inv.player[0].quantity, 5);
    }

    #[test]
    fn remove_more_than_available_removes_item() {
        let mut inv = InventoryStore::default();
        inv.add_to_player(InventoryItem {
            id: "sword".into(),
            name: "Меч".into(),
            description: "".into(),
            rarity: Rarity::Common,
            base_price: 50,
            tags: vec![],
            quantity: 2,
        });
        // Removing more than there is — the item is removed.
        assert!(inv.remove_from_player("sword", 5));
        assert!(inv.player.is_empty());
    }

    #[test]
    fn remove_nonexistent_returns_false() {
        let mut inv = InventoryStore::default();
        assert!(!inv.remove_from_player("no_such_item", 1));
    }

    #[test]
    fn final_price_extreme_friendship() {
        let item = InventoryItem {
            id: "test".into(),
            name: "Test".into(),
            description: "".into(),
            rarity: Rarity::Common,
            base_price: 100,
            tags: vec![],
            quantity: 1,
        };
        // friendship -100 → mult 1.5, Common 1.0, no location → 1.0
        let price_enemy = final_price(&item, &[], -100.0);
        assert_eq!(price_enemy, 150);
        // friendship +100 → mult 0.5
        let price_friend = final_price(&item, &[], 100.0);
        assert_eq!(price_friend, 50);
    }

    #[test]
    fn location_price_mult_unknown_tags() {
        let tags = vec!["портал".to_string(), "космос".to_string()];
        assert_eq!(location_price_mult(&tags), 1.0);
    }

    #[test]
    fn location_price_mult_empty() {
        let tags: Vec<String> = vec![];
        assert_eq!(location_price_mult(&tags), 1.0);
    }

    #[test]
    fn give_to_npc_basic() {
        let mut inv = InventoryStore::default();
        inv.give_to_npc("merchant", InventoryItem {
            id: "gold".into(),
            name: "Золото".into(),
            description: "".into(),
            rarity: Rarity::Common,
            base_price: 1,
            tags: vec![],
            quantity: 100,
        });
        assert_eq!(inv.npcs.len(), 1);
        assert_eq!(inv.npcs["merchant"].len(), 1);
        assert_eq!(inv.npcs["merchant"][0].quantity, 100);
    }

    #[test]
    fn final_price_zero_base() {
        let item = InventoryItem {
            id: "free".into(),
            name: "Бесплатный".into(),
            description: "".into(),
            rarity: Rarity::Common,
            base_price: 0,
            tags: vec![],
            quantity: 1,
        };
        assert_eq!(final_price(&item, &[], 0.0), 0);
    }

    #[test]
    fn inventory_note_empty() {
        let inv = InventoryStore::default();
        assert!(inventory_note(&inv).is_none());
    }

    #[test]
    fn inventory_note_infinite() {
        let mut inv = InventoryStore::default();
        inv.add_to_player(InventoryItem {
            id: "potion".into(),
            name: "Зелье".into(),
            description: "".into(),
            rarity: Rarity::Common,
            base_price: 10,
            tags: vec![],
            quantity: -1,
        });
        let note = inventory_note(&inv).unwrap();
        assert!(note.contains("Зелье ×∞"), "{note}");
    }

    #[test]
    fn add_zero_quantity_is_noop() {
        let mut inv = InventoryStore::default();
        inv.add_to_player(InventoryItem {
            id: "potion".into(),
            name: "Зелье".into(),
            description: "".into(),
            rarity: Rarity::Common,
            base_price: 10,
            tags: vec![],
            quantity: 0,
        });
        assert!(inv.player.is_empty());
    }

    #[test]
    fn remove_finite_when_infinite_exists() {
        let mut inv = InventoryStore::default();
        // The infinite item was added first.
        inv.add_to_player(InventoryItem {
            id: "potion".into(),
            name: "Зелье".into(),
            description: "".into(),
            rarity: Rarity::Common,
            base_price: 10,
            tags: vec![],
            quantity: -1,
        });
        // The finite item was added second.
        inv.add_to_player(InventoryItem {
            id: "potion".into(),
            name: "Зелье".into(),
            description: "".into(),
            rarity: Rarity::Common,
            base_price: 10,
            tags: vec![],
            quantity: 3,
        });
        assert_eq!(inv.player.len(), 2);
        // Removal must find the finite item, not the infinite one.
        assert!(inv.remove_from_player("potion", 1));
        assert_eq!(inv.player.len(), 2);
        assert_eq!(inv.player[0].quantity, -1);
        assert_eq!(inv.player[1].quantity, 2);
    }

    #[test]
    fn add_to_player_clamps_incoming_quantity() {
        let mut inv = InventoryStore::default();
        inv.add_to_player(InventoryItem {
            id: "potion".into(),
            name: "Зелье".into(),
            description: "".into(),
            rarity: Rarity::Common,
            base_price: 10,
            tags: vec![],
            quantity: 20_000,
        });
        assert_eq!(inv.player[0].quantity, 9999);
    }

    #[test]
    fn add_to_player_stack_caps_at_9999() {
        let mut inv = InventoryStore::default();
        inv.add_to_player(InventoryItem {
            id: "potion".into(),
            name: "Зелье".into(),
            description: "".into(),
            rarity: Rarity::Common,
            base_price: 10,
            tags: vec![],
            quantity: 9000,
        });
        inv.add_to_player(InventoryItem {
            id: "potion".into(),
            name: "Зелье".into(),
            description: "".into(),
            rarity: Rarity::Common,
            base_price: 10,
            tags: vec![],
            quantity: 5000,
        });
        assert_eq!(inv.player.len(), 1);
        assert_eq!(inv.player[0].quantity, 9999);
    }

    #[test]
    fn add_to_player_saturating_add_no_overflow() {
        let mut inv = InventoryStore::default();
        inv.player.push(InventoryItem {
            id: "gem".into(),
            name: "Самоцвет".into(),
            description: "".into(),
            rarity: Rarity::Rare,
            base_price: 100,
            tags: vec![],
            quantity: i32::MAX,
        });
        inv.add_to_player(InventoryItem {
            id: "gem".into(),
            name: "Самоцвет".into(),
            description: "".into(),
            rarity: Rarity::Rare,
            base_price: 100,
            tags: vec![],
            quantity: 1,
        });
        // saturating_add + min(9999): no overflow panic, the stack is pinned to the cap
        assert_eq!(inv.player[0].quantity, 9999);
    }

    // --- Writers wired up for V-05/V-57: payment, purchase, rewards ---

    fn pay_item(id: &str, base_price: u32, quantity: i32) -> InventoryItem {
        InventoryItem {
            id: id.into(),
            name: id.into(),
            description: String::new(),
            rarity: Rarity::Common,
            base_price,
            tags: vec![],
            quantity,
        }
    }

    #[test]
    fn item_value_is_neutral_final_price() {
        let item = InventoryItem {
            rarity: Rarity::Rare,
            ..pay_item("x", 100, 1)
        };
        // 100 * 2.5 (rare) * 1.0 (no location markup) * 1.0 (neutral ties)
        assert_eq!(item_value(&item), 250);
    }

    #[test]
    fn pay_value_takes_cheapest_first_and_partial_stack() {
        let mut inv = InventoryStore::default();
        inv.add_to_player(pay_item("rich", 100, 1));
        inv.add_to_player(pay_item("coin", 10, 3));
        let paid = pay_value(&mut inv, 15).expect("мелочью хватает");
        // Cheap first: 2 coins (20) cover 15, the relic is untouched.
        assert_eq!(paid.len(), 1);
        assert_eq!(paid[0].id, "coin");
        assert_eq!(paid[0].quantity, 2);
        assert_eq!(inv.player.iter().find(|i| i.id == "coin").unwrap().quantity, 1);
        assert_eq!(inv.player.iter().find(|i| i.id == "rich").unwrap().quantity, 1);
    }

    #[test]
    fn pay_value_insufficient_removes_nothing() {
        let mut inv = InventoryStore::default();
        inv.add_to_player(pay_item("coin", 10, 2));
        assert!(pay_value(&mut inv, 100).is_none(), "сумма не покрыта");
        assert_eq!(inv.player[0].quantity, 2, "неудачная оплата ничего не списывает");
    }

    #[test]
    fn pay_value_skips_infinite_stock_and_zero_value() {
        let mut inv = InventoryStore::default();
        inv.add_to_player(InventoryItem {
            quantity: -1,
            ..pay_item("shop_stock", 500, -1)
        });
        inv.add_to_player(pay_item("free", 0, 5));
        assert!(
            pay_value(&mut inv, 10).is_none(),
            "бесконечный склад и бесплатные предметы не служат оплатой"
        );
        assert_eq!(inv.player.len(), 2);
    }

    #[test]
    fn pay_value_zero_budget_is_free() {
        let mut inv = InventoryStore::default();
        inv.add_to_player(pay_item("coin", 10, 2));
        assert!(pay_value(&mut inv, 0).unwrap().is_empty());
        assert_eq!(inv.player[0].quantity, 2);
    }

    #[test]
    fn commit_purchase_charges_and_transfers() {
        let mut inv = InventoryStore::default();
        inv.npcs.insert("merchant".into(), vec![pay_item("sword", 50, 2)]);
        inv.add_to_player(pay_item("coin", 10, 20));
        // 50 * 1.0 (common) * 1.0 * 1.0 = 50 per unit, two = 100.
        let paid = commit_purchase(&mut inv, "merchant", "sword", 2, &[], 0.0).unwrap();
        assert_eq!(paid, 100);
        assert_eq!(inv.player.iter().find(|i| i.id == "coin").unwrap().quantity, 10);
        assert_eq!(inv.player.iter().find(|i| i.id == "sword").unwrap().quantity, 2);
        assert!(!inv.npcs["merchant"].iter().any(|i| i.id == "sword"), "товар должен уйти с прилавка");
        assert_eq!(inv.npcs["merchant"].iter().find(|i| i.id == "coin").unwrap().quantity, 10, "оплата — торговцу");
    }

    #[test]
    fn commit_purchase_insufficient_funds_changes_nothing() {
        let mut inv = InventoryStore::default();
        inv.npcs.insert("merchant".into(), vec![pay_item("sword", 50, 2)]);
        inv.add_to_player(pay_item("coin", 10, 3));
        assert!(commit_purchase(&mut inv, "merchant", "sword", 1, &[], 0.0).is_err());
        assert_eq!(inv.player.iter().find(|i| i.id == "coin").unwrap().quantity, 3);
        assert_eq!(inv.npcs["merchant"].len(), 1, "прилавок не тронут");
        assert!(inv.player.iter().all(|i| i.id != "sword"), "товар не выдан без оплаты");
    }

    #[test]
    fn commit_purchase_rejects_bad_requests() {
        let mut inv = InventoryStore::default();
        assert!(commit_purchase(&mut inv, "ghost", "x", 1, &[], 0.0).is_err(), "нет прилавка");
        inv.npcs.insert("merchant".into(), vec![pay_item("sword", 50, 1)]);
        assert!(commit_purchase(&mut inv, "merchant", "sword", 5, &[], 0.0).is_err(), "больше, чем в наличии");
        assert!(commit_purchase(&mut inv, "merchant", "nope", 1, &[], 0.0).is_err(), "нет такого товара");
        assert!(commit_purchase(&mut inv, "merchant", "sword", 0, &[], 0.0).is_err(), "нулевое количество");
    }

    #[test]
    fn commit_purchase_infinite_stock_never_runs_out() {
        let mut inv = InventoryStore::default();
        inv.npcs.insert("shop".into(), vec![InventoryItem {
            quantity: -1,
            ..pay_item("potion", 20, -1)
        }]);
        inv.add_to_player(pay_item("coin", 10, 10));
        assert_eq!(commit_purchase(&mut inv, "shop", "potion", 3, &[], 0.0).unwrap(), 60);
        assert_eq!(inv.npcs["shop"][0].quantity, -1, "бесконечный склад не тратится");
        assert_eq!(inv.player.iter().find(|i| i.id == "potion").unwrap().quantity, 3);
    }

    #[test]
    fn commit_purchase_uses_location_and_relationship_pricing() {
        // Dungeon: 2.0 markup; hostility -100 → multiplier 1.5.
        // 50 * 1.0 * 2.0 * 1.5 = 150.
        let mut inv = InventoryStore::default();
        inv.npcs.insert("merchant".into(), vec![pay_item("sword", 50, 1)]);
        inv.add_to_player(pay_item("coin", 10, 20));
        let tags = vec!["подземелье".to_string()];
        assert_eq!(commit_purchase(&mut inv, "merchant", "sword", 1, &tags, -100.0).unwrap(), 150);
    }

    #[test]
    fn grant_items_catalog_and_unknown() {
        let mut inv = InventoryStore::default();
        let granted = grant_items(
            &mut inv,
            &[
                "Зелье здоровья".to_string(),
                "gold_necklace".to_string(),
                "Трофей старосты".to_string(),
            ],
        );
        assert_eq!(granted.len(), 3);
        // The shop stock is infinite, the reward is a finite item.
        assert_eq!(inv.player.iter().find(|i| i.id == "health_potion").unwrap().quantity, 1);
        assert_eq!(inv.player.iter().find(|i| i.id == "gold_necklace").unwrap().quantity, 1);
        let unknown = inv.player.iter().find(|i| i.name == "Трофей старосты").unwrap();
        assert_eq!(unknown.base_price, 0, "неизвестная награда не становится валютой");
    }

    #[test]
    fn grant_items_ignores_blank_lines() {
        let mut inv = InventoryStore::default();
        let granted = grant_items(&mut inv, &["".to_string(), "   ".to_string()]);
        assert!(granted.is_empty());
        assert!(inv.player.is_empty());
    }

    #[test]
    fn give_to_npc_merges_stack_and_caps() {
        let mut inv = InventoryStore::default();
        inv.give_to_npc("merchant", pay_item("gold", 1, 9000));
        inv.give_to_npc("merchant", pay_item("gold", 1, 5000));
        assert_eq!(inv.npcs["merchant"].len(), 1, "подарки мержатся в стак");
        assert_eq!(inv.npcs["merchant"][0].quantity, 9999);
    }

    #[test]
    fn give_to_npc_zero_quantity_is_noop() {
        let mut inv = InventoryStore::default();
        inv.give_to_npc("merchant", pay_item("gold", 1, 0));
        assert!(inv.npcs.get("merchant").map(|v| v.is_empty()).unwrap_or(true));
    }
}
