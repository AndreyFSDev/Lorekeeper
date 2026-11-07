//! Character relationship system: 5 axes instead of a single number.
//! Each axis is a number from -100 to +100.
//!
//! Axes:
//! — **Friendship**: general liking, trust as a friend.
//! — **Romance**: infatuation, flirting, passion.
//! — **Respect**: recognition of strength, wisdom, skills.
//! — **Fear**: dread, submission, caution.
//! — **Hatred**: hostility, desire to harm.
//!
//! Compatibility: the old format (a single number) is read as friendship,
//! the other axes are zero. Writes always use the new format.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Five relationship axes between two characters.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RelationshipAxes {
    pub friendship: f32,
    pub romance: f32,
    pub respect: f32,
    pub fear: f32,
    pub hatred: f32,
}

impl Default for RelationshipAxes {
    fn default() -> Self {
        Self {
            friendship: 0.0,
            romance: 0.0,
            respect: 0.0,
            fear: 0.0,
            hatred: 0.0,
        }
    }
}

impl RelationshipAxes {
    /// Total relationship "weight": used to sort top-N.
    /// Romance outweighs friendship, hatred outweighs fear.
    pub fn score(&self) -> f32 {
        self.friendship * 0.7 + self.romance * 0.9 + self.respect * 0.6
            - self.fear * 0.4
            - self.hatred * 0.8
    }

    /// Primary axis: the one with the largest absolute value.
    pub fn dominant_axis(&self) -> (&'static str, f32) {
        let axes: [(&str, f32); 5] = [
            ("дружба", self.friendship),
            ("романтика", self.romance),
            ("уважение", self.respect),
            ("страх", self.fear),
            ("ненависть", self.hatred),
        ];
        axes.iter()
            .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap_or(std::cmp::Ordering::Equal))
            .map(|&(name, val)| (name, val))
            .unwrap_or(("нейтрально", 0.0))
    }

    /// Text label for the dominant axis and value.
    pub fn label(&self) -> String {
        let (axis, val) = self.dominant_axis();
        let intensity = if val.abs() > 70.0 {
            "глубоко"
        } else if val.abs() > 40.0 {
            "заметно"
        } else if val.abs() > 15.0 {
            "слегка"
        } else {
            return "нейтрально".to_string();
        };
        format!("{intensity} {axis}")
    }

    /// Shift all axes: clamp to -100..+100.
    pub fn shift(&mut self, delta: &RelationshipAxes) {
        self.friendship = (self.friendship + delta.friendship).clamp(-100.0, 100.0);
        self.romance = (self.romance + delta.romance).clamp(-100.0, 100.0);
        self.respect = (self.respect + delta.respect).clamp(-100.0, 100.0);
        self.fear = (self.fear + delta.fear).clamp(-100.0, 100.0);
        self.hatred = (self.hatred + delta.hatred).clamp(-100.0, 100.0);
    }

    /// Decay toward zero: each axis moves by decay toward zero.
    pub fn decay(&mut self, amount: f32) {
        let d = amount.max(0.0);
        let adjust = |v: &mut f32| {
            if *v > 0.0 { *v = (*v - d).max(0.0); }
            else if *v < 0.0 { *v = (*v + d).min(0.0); }
        };
        adjust(&mut self.friendship);
        adjust(&mut self.romance);
        adjust(&mut self.respect);
        adjust(&mut self.fear);
        adjust(&mut self.hatred);
    }

    /// Replace NaN and Infinity with 0.0. Guard against garbage data from JSON.
    pub fn sanitize(&mut self) {
        let clean = |v: &mut f32| {
            if !v.is_finite() { *v = 0.0; }
        };
        clean(&mut self.friendship);
        clean(&mut self.romance);
        clean(&mut self.respect);
        clean(&mut self.fear);
        clean(&mut self.hatred);
    }
}

/// Custom deserialization: reads both the old format (f32) and the new one (RelationshipAxes).
mod compat {
    use super::*;
    use serde::de::{Deserializer, MapAccess, Visitor};
    use std::fmt;

    pub fn deserialize<'de, D>(deserializer: D) -> Result<HashMap<String, RelationshipAxes>, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct CompatVisitor;

        impl<'de> Visitor<'de> for CompatVisitor {
            type Value = HashMap<String, RelationshipAxes>;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("map of relationship pairs")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut result = HashMap::new();
                while let Some(key) = map.next_key::<String>()? {
                    // Read as Value, then try both formats.
                    let val = map.next_value::<serde_json::Value>()?;
                    if let Ok(mut axes) = serde_json::from_value::<RelationshipAxes>(val.clone()) {
                        axes.sanitize();
                        result.insert(key, axes);
                    } else if let Some(num) = val.as_f64() {
                        let mut axes = RelationshipAxes {
                            friendship: num as f32,
                            ..Default::default()
                        };
                        axes.sanitize();
                        result.insert(key, axes);
                    }
                }
                Ok(result)
            }
        }

        deserializer.deserialize_map(CompatVisitor)
    }
}

/// Relationship store: the key is the pair "A::B" (sorted).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RelationshipStore {
    #[serde(deserialize_with = "compat::deserialize")]
    pub relationships: HashMap<String, RelationshipAxes>,
}

const DECAY_PER_SESSION: f32 = 0.5;

/// File lock: the post-reply evaluator may write (after every message),
/// and several calls may happen at once — regeneration, a group chat with
/// multiple characters, two quick messages in a row. Without a shared lock
/// concurrent "read → modify → write" lose another's shift — the same
/// bug pattern was already fixed in traits.json (see `traits::update`). The
/// read/write body is shared by all JSON stores (`json_store`), the lock is per store.
static STORE: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl RelationshipStore {
    pub fn load(path: &PathBuf) -> Self {
        crate::json_store::load(&STORE, path)
    }

    /// Modify the store under a shared lock: read, edit and write are one
    /// operation. Bypassing it with separate `load`+`save` is not allowed: that is exactly
    /// the race the lock must close.
    pub fn update<T>(path: &PathBuf, change: impl FnOnce(&mut Self) -> T) -> Result<T, String> {
        crate::json_store::update(&STORE, path, change)
    }

    /// Get the relationship between two characters.
    pub fn get(&self, a: &str, b: &str) -> RelationshipAxes {
        if a == b {
            return RelationshipAxes::default();
        }
        self.relationships.get(&Self::key(a, b)).copied().unwrap_or_default()
    }

    /// Shift the relationship along all axes.
    pub fn shift(&mut self, a: &str, b: &str, delta: &RelationshipAxes) {
        if a == b {
            return;
        }
        // NaN would slip through the clamp in score() and the axis clamp.
        let mut delta = *delta;
        delta.sanitize();
        let key = Self::key(a, b);
        let current = self.relationships.entry(key).or_default();
        current.shift(&delta);
    }

    /// Shift a single axis (for backward compatibility and simple shifts).
    pub fn shift_axis(&mut self, a: &str, b: &str, axis: &str, delta: f32) {
        if a == b || !delta.is_finite() {
            return;
        }
        let key = Self::key(a, b);
        let current = self.relationships.entry(key).or_default();
        match axis {
            "friendship" => current.friendship = (current.friendship + delta).clamp(-100.0, 100.0),
            "romance" => current.romance = (current.romance + delta).clamp(-100.0, 100.0),
            "respect" => current.respect = (current.respect + delta).clamp(-100.0, 100.0),
            "fear" => current.fear = (current.fear + delta).clamp(-100.0, 100.0),
            "hatred" => current.hatred = (current.hatred + delta).clamp(-100.0, 100.0),
            _ => {}
        }
    }

    /// Natural decay toward zero per session. `true` — at least one axis
    /// moved (the caller should reset its state cache).
    pub fn session_decay(&mut self) -> bool {
        let mut changed = false;
        for axes in self.relationships.values_mut() {
            let before = *axes;
            axes.decay(DECAY_PER_SESSION);
            if *axes != before {
                changed = true;
            }
        }
        changed
    }

    /// Shift with cooling (following `traits::evolve`): the delta is applied once,
    /// then all axes of the pair are pulled toward zero by `DECAY_PER_SESSION` —
    /// bonds not reinforced by messages weaken instead of only growing
    /// (V-41).
    pub fn shift_and_settle(&mut self, a: &str, b: &str, delta: &RelationshipAxes) {
        self.shift(a, b, delta);
        if a == b {
            return;
        }
        if let Some(axes) = self.relationships.get_mut(&Self::key(a, b)) {
            axes.decay(DECAY_PER_SESSION);
        }
    }

    fn key(a: &str, b: &str) -> String {
        // The separator must not appear in names: the relationships API report
        // splits keys with `split_once("::")`, and an "A::B" inside a name
        // silently breaks the pairing.
        let a = a.replace("::", ":");
        let b = b.replace("::", ":");
        if a < b {
            format!("{}::{}", a, b)
        } else {
            format!("{}::{}", b, a)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_ordering() {
        assert_eq!(RelationshipStore::key("A", "B"), "A::B");
        assert_eq!(RelationshipStore::key("B", "A"), "A::B");
    }

    /// "A::B" in a name broke key parsing into a pair — the separator inside a name
    /// collapses so the key always splits into exactly two parts.
    #[test]
    fn key_collapses_separator_inside_names() {
        let key = RelationshipStore::key("NPC::Guard", "Alice");
        assert_eq!(key.matches("::").count(), 1, "ключ {key} разбирается неоднозначно");
    }

    /// A character talking to itself is a meaningless entry in the store.
    #[test]
    fn self_relationship_is_ignored() {
        let mut store = RelationshipStore::default();
        let delta = RelationshipAxes { friendship: 30.0, ..Default::default() };
        store.shift("A", "A", &delta);
        assert!(store.relationships.is_empty(), "запись о себе не должна заводиться");
        assert_eq!(store.get("A", "A").friendship, 0.0);
    }

    #[test]
    fn nan_delta_does_not_poison_axes() {
        let mut store = RelationshipStore::default();
        let delta = RelationshipAxes { friendship: f32::NAN, ..Default::default() };
        store.shift("A", "B", &delta);
        assert!(store.get("A", "B").friendship.is_finite());
    }

    #[test]
    fn default_zero() {
        let store = RelationshipStore::default();
        let axes = store.get("A", "B");
        assert_eq!(axes.friendship, 0.0);
        assert_eq!(axes.romance, 0.0);
        assert_eq!(axes.hatred, 0.0);
    }

    #[test]
    fn shift_positive() {
        let mut store = RelationshipStore::default();
        let delta = RelationshipAxes { friendship: 10.0, romance: 5.0, ..Default::default() };
        store.shift("A", "B", &delta);
        let axes = store.get("A", "B");
        assert_eq!(axes.friendship, 10.0);
        assert_eq!(axes.romance, 5.0);
    }

    #[test]
    fn shift_negative() {
        let mut store = RelationshipStore::default();
        let delta = RelationshipAxes { hatred: 20.0, fear: 10.0, ..Default::default() };
        store.shift("A", "B", &delta);
        let axes = store.get("A", "B");
        assert_eq!(axes.hatred, 20.0);
        assert_eq!(axes.fear, 10.0);
    }

    #[test]
    fn shift_clamps() {
        let mut store = RelationshipStore::default();
        let delta = RelationshipAxes { friendship: 200.0, ..Default::default() };
        store.shift("A", "B", &delta);
        assert_eq!(store.get("A", "B").friendship, 100.0);
    }

    #[test]
    fn shift_axis_single() {
        let mut store = RelationshipStore::default();
        store.shift_axis("A", "B", "romance", 30.0);
        assert_eq!(store.get("A", "B").romance, 30.0);
        assert_eq!(store.get("A", "B").friendship, 0.0);
    }

    #[test]
    fn decay_works() {
        let mut store = RelationshipStore::default();
        let delta = RelationshipAxes { friendship: 10.0, hatred: -10.0, ..Default::default() };
        store.shift("A", "B", &delta);
        let before = store.get("A", "B");
        store.session_decay();
        let after = store.get("A", "B");
        assert!(after.friendship < before.friendship);
        assert!(after.hatred > before.hatred);
    }

    #[test]
    fn score_combines_axes() {
        let axes = RelationshipAxes {
            friendship: 50.0,
            romance: 30.0,
            respect: 20.0,
            fear: 10.0,
            hatred: 5.0,
        };
        let score = axes.score();
        // 50*0.7 + 30*0.9 + 20*0.6 - 10*0.4 - 5*0.8 = 35 + 27 + 12 - 4 - 4 = 66
        assert!((score - 66.0).abs() < 0.1, "score={score}");
    }

    #[test]
    fn dominant_axis() {
        let axes = RelationshipAxes { friendship: 10.0, romance: 80.0, ..Default::default() };
        let (name, val) = axes.dominant_axis();
        assert_eq!(name, "романтика");
        assert_eq!(val, 80.0);
    }

    #[test]
    fn label_varies_by_intensity() {
        let deep = RelationshipAxes { friendship: 80.0, ..Default::default() };
        assert!(deep.label().contains("глубоко"), "{}", deep.label());

        let slight = RelationshipAxes { fear: 20.0, ..Default::default() };
        assert!(slight.label().contains("слегка"), "{}", slight.label());

        let neutral = RelationshipAxes::default();
        assert_eq!(neutral.label(), "нейтрально");
    }

    #[test]
    fn compat_deserializes_old_format() {
        let json = r#"{"relationships": {"A::B": 42.5, "C::D": -10.0}}"#;
        let store: RelationshipStore = serde_json::from_str(json).unwrap();
        let ab = store.get("A", "B");
        assert_eq!(ab.friendship, 42.5);
        assert_eq!(ab.romance, 0.0);
        let cd = store.get("C", "D");
        assert_eq!(cd.friendship, -10.0);
    }

    #[test]
    fn compat_deserializes_new_format() {
        let json = r#"{"relationships": {"A::B": {"friendship": 30, "romance": 50, "respect": 10, "fear": 0, "hatred": -5}}}"#;
        let store: RelationshipStore = serde_json::from_str(json).unwrap();
        let ab = store.get("A", "B");
        assert_eq!(ab.friendship, 30.0);
        assert_eq!(ab.romance, 50.0);
        assert_eq!(ab.hatred, -5.0);
    }

    #[test]
    fn shift_axis_unknown_is_noop() {
        let mut store = RelationshipStore::default();
        store.shift_axis("A", "B", "friendship", 50.0);
        store.shift_axis("A", "B", "unknown_axis", 999.0);
        let axes = store.get("A", "B");
        assert_eq!(axes.friendship, 50.0);
        assert_eq!(axes.romance, 0.0);
        assert_eq!(axes.respect, 0.0);
    }

    #[test]
    fn session_decay_on_zeros() {
        let mut store = RelationshipStore::default();
        store.shift("A", "B", &RelationshipAxes::default());
        store.session_decay();
        let axes = store.get("A", "B");
        assert_eq!(axes.friendship, 0.0);
        assert_eq!(axes.romance, 0.0);
        assert_eq!(axes.hatred, 0.0);
    }

    #[test]
    fn label_negative_values() {
        let hatred = RelationshipAxes { hatred: -80.0, ..Default::default() };
        let lbl = hatred.label();
        assert!(lbl.contains("глубоко"), "{}", lbl);
        assert!(lbl.contains("ненависть"), "{}", lbl);

        let fear = RelationshipAxes { fear: -25.0, ..Default::default() };
        let lbl = fear.label();
        assert!(lbl.contains("слегка"), "{}", lbl);
        assert!(lbl.contains("страх"), "{}", lbl);
    }

    #[test]
    fn label_neutral_range() {
        // Values between -15 and +15 give "neutral"
        let near_zero = RelationshipAxes { friendship: 14.0, ..Default::default() };
        assert_eq!(near_zero.label(), "нейтрально");

        let neg_near_zero = RelationshipAxes { hatred: -14.0, ..Default::default() };
        assert_eq!(neg_near_zero.label(), "нейтрально");

        let exactly_15 = RelationshipAxes { friendship: 15.0, ..Default::default() };
        assert_eq!(exactly_15.label(), "нейтрально");
    }

    #[test]
    fn shift_zero_delta() {
        let mut axes = RelationshipAxes { friendship: 50.0, romance: -30.0, ..Default::default() };
        let before = axes;
        axes.shift(&RelationshipAxes::default());
        assert_eq!(axes, before);
    }

    // --- shift_axis clamping ---

    #[test]
    fn shift_axis_clamps_both_directions() {
        let mut store = RelationshipStore::default();
        store.shift_axis("A", "B", "friendship", 150.0);
        assert_eq!(store.get("A", "B").friendship, 100.0);

        store.shift_axis("A", "B", "friendship", -250.0);
        assert_eq!(store.get("A", "B").friendship, -100.0);
    }

    // --- label boundaries ---

    #[test]
    fn label_at_exact_boundaries() {
        // Exactly 40 → not > 40, but > 15 → "slightly"
        let at_40 = RelationshipAxes { fear: 40.0, ..Default::default() };
        assert!(at_40.label().contains("слегка"), "{}", at_40.label());

        let over_40 = RelationshipAxes { fear: 40.01, ..Default::default() };
        assert!(over_40.label().contains("заметно"), "{}", over_40.label());

        // Exactly 70 → not > 70, but > 40 → "noticeably"
        let at_70 = RelationshipAxes { hatred: 70.0, ..Default::default() };
        assert!(at_70.label().contains("заметно"), "{}", at_70.label());

        let over_70 = RelationshipAxes { hatred: 70.01, ..Default::default() };
        assert!(over_70.label().contains("глубоко"), "{}", over_70.label());
    }

    // --- get symmetry ---

    #[test]
    fn get_symmetry() {
        let mut store = RelationshipStore::default();
        store.shift("A", "B", &RelationshipAxes { friendship: 42.0, ..Default::default() });
        assert_eq!(store.get("A", "B").friendship, 42.0);
        assert_eq!(store.get("B", "A").friendship, 42.0);
    }

    // --- compat edge cases ---

    #[test]
    fn compat_mixed_old_and_new_format() {
        let json = r#"{"relationships": {"A::B": 30.0, "C::D": {"friendship": 10, "romance": 20, "respect": 0, "fear": 0, "hatred": 0}}}"#;
        let store: RelationshipStore = serde_json::from_str(json).unwrap();
        assert_eq!(store.get("A", "B").friendship, 30.0);
        assert_eq!(store.get("A", "B").romance, 0.0);
        assert_eq!(store.get("C", "D").friendship, 10.0);
        assert_eq!(store.get("C", "D").romance, 20.0);
    }

    #[test]
    fn compat_drops_non_numeric_non_object() {
        // Strings and booleans are discarded; the array [1,2] deserializes
        // positionally via serde as RelationshipAxes{friendship:1, romance:2}.
        let json = r#"{"relationships": {"A::B": "invalid", "C::D": true, "E::F": [1,2], "G::H": 42.0}}"#;
        let store: RelationshipStore = serde_json::from_str(json).unwrap();
        assert_eq!(store.relationships.len(), 2);
        assert_eq!(store.get("G", "H").friendship, 42.0);
        assert_eq!(store.get("E", "F").friendship, 1.0);
        assert_eq!(store.get("E", "F").romance, 2.0);
    }

    #[test]
    fn compat_null_value_dropped() {
        // null + deserialize_struct → error → entry discarded.
        let json = r#"{"relationships": {"A::B": null, "C::D": 50.0}}"#;
        let store: RelationshipStore = serde_json::from_str(json).unwrap();
        assert_eq!(store.relationships.len(), 1);
        assert_eq!(store.get("C", "D").friendship, 50.0);
    }

    #[test]
    fn compat_unknown_object_fields_become_default() {
        // An object with unknown fields — #[serde(default)] ignores them → zeros.
        let json = r#"{"relationships": {"A::B": {"unknown_field": 99}}}"#;
        let store: RelationshipStore = serde_json::from_str(json).unwrap();
        assert_eq!(store.get("A", "B").friendship, 0.0);
        assert_eq!(store.get("A", "B").romance, 0.0);
    }

    // --- score edge cases ---

    #[test]
    fn score_all_negative() {
        let axes = RelationshipAxes {
            friendship: -50.0, romance: -30.0, respect: -20.0, fear: -10.0, hatred: -40.0,
        };
        // -50*0.7 + (-30*0.9) + (-20*0.6) - (-10*0.4) - (-40*0.8) = -35 -27 -12 +4 +32 = -38
        assert!((axes.score() - (-38.0)).abs() < 0.1, "score={}", axes.score());
    }

    #[test]
    fn score_zeros() {
        assert_eq!(RelationshipAxes::default().score(), 0.0);
    }

    // --- dominant_axis edge cases ---

    #[test]
    fn dominant_axis_ties() {
        // max_by on Equal keeps the last → hatred (last in the array).
        let axes = RelationshipAxes { friendship: 50.0, hatred: -50.0, ..Default::default() };
        let (name, val) = axes.dominant_axis();
        assert_eq!(name, "ненависть");
        assert_eq!(val, -50.0);
    }

    #[test]
    fn dominant_axis_all_zeros() {
        // All zeros — max_by on Equal keeps the last element.
        let axes = RelationshipAxes::default();
        let (name, val) = axes.dominant_axis();
        assert_eq!(name, "ненависть");
        assert_eq!(val, 0.0);
    }

    // --- decay edge cases ---

    #[test]
    fn decay_negative_value_toward_zero() {
        let mut axes = RelationshipAxes { hatred: -50.0, ..Default::default() };
        axes.decay(10.0);
        assert!((axes.hatred - (-40.0)).abs() < 0.01);
    }

    #[test]
    fn decay_overshoots_to_zero() {
        // Value 2.0, decay 10.0 → does not go negative, settles at 0.
        let mut axes = RelationshipAxes { friendship: 2.0, ..Default::default() };
        axes.decay(10.0);
        assert_eq!(axes.friendship, 0.0);

        let mut axes2 = RelationshipAxes { fear: -3.0, ..Default::default() };
        axes2.decay(10.0);
        assert_eq!(axes2.fear, 0.0);
    }

    // --- session_decay edge cases ---

    #[test]
    fn session_decay_multiple_times() {
        let mut store = RelationshipStore::default();
        store.shift("A", "B", &RelationshipAxes { friendship: 5.0, ..Default::default() });
        for _ in 0..3 {
            store.session_decay();
        }
        // 5.0 - 0.5*3 = 3.5
        let axes = store.get("A", "B");
        assert!((axes.friendship - 3.5).abs() < 0.01, "friendship={}", axes.friendship);
    }

    // --- load edge cases ---

    #[test]
    fn load_nonexistent_returns_default() {
        let store = RelationshipStore::load(&std::path::PathBuf::from("/nonexistent/path/rels.json"));
        assert!(store.relationships.is_empty());
    }

    /// The race the lock fixes: the post-reply evaluator may start
    /// concurrently for two messages in a row (regeneration, a group chat, two
    /// quick messages). Without a shared lock, read-modify-write through separate
    /// `load`+`save` loses some shifts — the same bug traits.json had.
    #[test]
    fn concurrent_updates_do_not_lose_deltas() {
        let dir = std::env::temp_dir().join("cloufam_test_rels_race");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("relationships_race.json");
        let _ = std::fs::remove_file(&path);

        const THREADS: usize = 20;
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    RelationshipStore::update(&path, |store| {
                        store.shift(
                            "A",
                            "B",
                            &RelationshipAxes { friendship: 1.0, ..Default::default() },
                        );
                    })
                    .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let store = RelationshipStore::load(&path);
        assert_eq!(
            store.get("A", "B").friendship,
            THREADS as f32,
            "часть параллельных сдвигов потеряна"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn load_invalid_json_returns_default() {
        let dir = std::env::temp_dir().join("cloufam_test_rels");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.json");
        std::fs::write(&path, "not valid json {{{").unwrap();
        let store = RelationshipStore::load(&path);
        assert!(store.relationships.is_empty());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    // --- sanitize ---

    #[test]
    fn sanitize_replaces_inf() {
        let mut axes = RelationshipAxes {
            friendship: f32::INFINITY,
            romance: f32::NEG_INFINITY,
            respect: 50.0,
            fear: f32::NAN,
            hatred: 0.0,
        };
        axes.sanitize();
        assert_eq!(axes.friendship, 0.0);
        assert_eq!(axes.romance, 0.0);
        assert_eq!(axes.respect, 50.0);
        assert_eq!(axes.fear, 0.0);
        assert_eq!(axes.hatred, 0.0);
    }

    #[test]
    fn compat_sanitizes_inf() {
        // 1e300 → Inf in f32 → sanitize zeroes it.
        let json = r#"{"relationships": {"A::B": 1e300}}"#;
        let store: RelationshipStore = serde_json::from_str(json).unwrap();
        assert_eq!(store.get("A", "B").friendship, 0.0);
    }

    // --- decay negative amount ---

    #[test]
    fn decay_negative_amount_is_noop() {
        let mut axes = RelationshipAxes { friendship: 50.0, hatred: -30.0, ..Default::default() };
        axes.decay(-10.0);
        assert_eq!(axes.friendship, 50.0);
        assert_eq!(axes.hatred, -30.0);
    }

    // --- score: romance outweighs friendship ---

    #[test]
    fn score_romance_beats_friendship() {
        let romance = RelationshipAxes { romance: 80.0, ..Default::default() };
        let friendship = RelationshipAxes { friendship: 80.0, ..Default::default() };
        assert!(romance.score() > friendship.score(),
            "romance={} friendship={}", romance.score(), friendship.score());
    }

    // --- V-41: cooling actually runs (shift_and_settle, session_decay) ---

    /// Following `traits::evolve`: the shift is applied once, then the pair
    /// cools toward zero — relationships do not grow forever.
    #[test]
    fn shift_and_settle_cools_the_pair() {
        let mut store = RelationshipStore::default();
        let delta = RelationshipAxes { friendship: 10.0, ..Default::default() };
        store.shift_and_settle("A", "B", &delta);
        let axes = store.get("A", "B");
        assert_eq!(axes.friendship, 9.5, "10.0 минус остывание {} ", DECAY_PER_SESSION);
        // Repeated cooling at the same step.
        store.shift_and_settle("A", "B", &RelationshipAxes::default());
        assert_eq!(store.get("A", "B").friendship, 9.0);
    }

    #[test]
    fn shift_and_settle_does_not_cross_zero() {
        let mut store = RelationshipStore::default();
        store.shift_and_settle("A", "B", &RelationshipAxes { hatred: -0.3, ..Default::default() });
        assert_eq!(store.get("A", "B").hatred, 0.0, "остывание не перескакивает ноль");
    }

    #[test]
    fn shift_and_settle_self_is_noop() {
        let mut store = RelationshipStore::default();
        store.shift_and_settle("A", "A", &RelationshipAxes { friendship: 50.0, ..Default::default() });
        assert!(store.relationships.is_empty());
    }

    fn temp_rel_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("cloufam_test_relationships");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("json.corrupt"));
        path
    }

    /// Per-session cooling on disk in the same form the gateway calls when switching
    /// the active card (`RelationshipStore::update` + `session_decay`):
    /// moves the axes and honestly reports whether there was anything to cool (for cache reset).
    #[test]
    fn session_decay_cools_on_disk_and_reports_change() {
        let path = temp_rel_path("decay.json");
        RelationshipStore::update(&path, |store| {
            store.shift("A", "B", &RelationshipAxes { friendship: 20.0, ..Default::default() });
        })
        .unwrap();

        let changed = RelationshipStore::update(&path, |store| store.session_decay()).unwrap();
        assert!(changed, "есть что остужать");
        let store = RelationshipStore::load(&path);
        assert_eq!(store.get("A", "B").friendship, 20.0 - DECAY_PER_SESSION);

        // A fully zero entry: the pass changes nothing.
        RelationshipStore::update(&path, |store| *store = RelationshipStore::default()).unwrap();
        let changed = RelationshipStore::update(&path, |store| store.session_decay()).unwrap();
        assert!(!changed, "на нулях остывать нечему");
        let _ = std::fs::remove_file(&path);
    }
}

