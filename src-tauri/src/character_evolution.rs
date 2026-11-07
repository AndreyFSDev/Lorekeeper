//! Character evolution: per-character tag weights that drift from real
//! conversation (V-40). The store and the mechanics live here; the honest
//! signal — literal mentions of tags/traits in the exchanged texts plus the
//! state evaluator's tone — is committed by `apply_exchange` once per shown
//! exchange, and `decay_session` cools everything at session boundaries.
//!
//! Persistence follows the shared `json_store` rules (V-31/V-09): one lock per
//! store, read-modify-write that refuses to clobber unreadable files, and a
//! commit happens only on success. Execution: blocking file IO on the caller's
//! thread — async gateway paths run the writer via `spawn_blocking` (V-17).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Per-character evolution store: weights of the card's tags (0.0 .. 2.0,
/// start 1.0), tags acquired through conversation, faded history, exchange
/// counter and the smoothed tone of recent exchanges.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CharacterEvolution {
    pub characters: HashMap<String, EvolvedCharacter>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolvedCharacter {
    /// Tag → current weight (0.0 .. 2.0). The initial one = 1.0.
    #[serde(default)]
    pub tag_weights: HashMap<String, f32>,
    /// Tags acquired through conversation (not present in the card).
    #[serde(default)]
    pub acquired_tags: HashMap<String, f32>,
    /// Tags that dropped below the decay threshold.
    #[serde(default)]
    pub faded_tags: Vec<String>,
    /// Number of interactions.
    #[serde(default)]
    pub interaction_count: u32,
    /// Average tone of recent interactions (-1.0 .. +1.0).
    #[serde(default)]
    pub recent_sentiment: f32,
}

const DECAY_PER_SESSION: f32 = 0.01;
const FADE_THRESHOLD: f32 = 0.15;
const MAX_WEIGHT: f32 = 2.0;
/// Ceiling on the number of tags per character: protection against unbounded growth
/// from external input (the model may name new tags on every turn).
const MAX_TAGS: usize = 200;
/// Cap on the history of faded tags — it is for reference only.
const MAX_FADED: usize = 200;

// --- Writer constants (V-40): bounded shifts plus cool-down, the same pattern
// as `traits::evolve` / `relationships::shift_and_settle`. ---

/// Reinforcement for one literal mention of a tag in the exchange texts.
const MENTION_STEP: f32 = 0.1;
/// Per-exchange bound on one tag's reinforcement, whatever the mention count.
const MAX_STEP: f32 = 0.3;
/// Strength of a tag acquired from conversation (not on the card): below the
/// card baseline 1.0 so a passing mention never outshouts the card.
const ACQUIRE_STRENGTH: f32 = 0.25;
/// Cool-down applied to every committed exchange: weights without
/// reinforcement fade instead of only growing.
const DECAY_PER_EXCHANGE: f32 = 0.005;

/// Store lock: the writer (`apply_exchange`), the session cool-down and the
/// report read all touch one file; `json_store` serializes them through it.
static STORE: Mutex<()> = Mutex::new(());

impl CharacterEvolution {
    /// Read-only load for the report endpoint. Missing, unreadable or corrupt
    /// files fall back to an empty store — a corrupt one is preserved as
    /// `<name>.json.corrupt` by `json_store`. Writers go through
    /// `apply_exchange`/`decay_session`, which refuse those states instead of
    /// overwriting real data (V-09). Blocking IO on the caller's thread.
    pub fn load(path: &Path) -> Self {
        crate::json_store::load(&STORE, path)
    }

    /// Initialize a character with starting tags (weight = 1.0).
    pub fn init_character(&mut self, name: &str, tags: &[String]) {
        if self.characters.contains_key(name) {
            return;
        }
        self.characters.insert(
            name.to_string(),
            EvolvedCharacter {
                // Brand and generation are identity, not behaviour: they are not
                // reinforced and must not fade into faded_tags.
                tag_weights: tags
                    .iter()
                    .filter(|tag| crate::tag_influence::is_character_trait(tag))
                    .map(|t| (t.clone(), 1.0f32))
                    .collect(),
                acquired_tags: HashMap::new(),
                faded_tags: Vec::new(),
                interaction_count: 0,
                recent_sentiment: 0.0,
            },
        );
    }

    /// Record an interaction: update the counter and the tone.
    /// Tag shifts come separately through `shift_tags`.
    pub fn record_interaction(&mut self, character: &str, sentiment: f32) {
        if !sentiment.is_finite() {
            return;
        }
        let entry = self.characters.entry(character.to_string()).or_insert_with(|| EvolvedCharacter {
            tag_weights: HashMap::new(),
            acquired_tags: HashMap::new(),
            faded_tags: Vec::new(),
            interaction_count: 0,
            recent_sentiment: 0.0,
        });
        entry.interaction_count += 1;
        // Moving average of the tone
        entry.recent_sentiment = entry.recent_sentiment * 0.9 + sentiment * 0.1;
    }

    /// Shift a tag's weight (called by gateway/tagger based on analysis).
    pub fn shift_tag(&mut self, character: &str, tag: &str, delta: f32) {
        // NaN would slip through clamp and stay in the weight forever.
        if !delta.is_finite() {
            return;
        }
        let entry = self.characters.entry(character.to_string()).or_insert_with(|| EvolvedCharacter {
            tag_weights: HashMap::new(),
            acquired_tags: HashMap::new(),
            faded_tags: Vec::new(),
            interaction_count: 0,
            recent_sentiment: 0.0,
        });

        if !entry.tag_weights.contains_key(tag)
            && entry.tag_weights.len() + entry.acquired_tags.len() >= MAX_TAGS
        {
            return;
        }
        // The tag may come from acquired_tags: take its weight instead of starting at 1.0.
        let start = entry.acquired_tags.remove(tag).unwrap_or(1.0);
        let weight = entry.tag_weights.entry(tag.to_string()).or_insert(start);
        *weight = (*weight + delta).clamp(0.0, MAX_WEIGHT);

        if *weight < FADE_THRESHOLD {
            // The tag has faded — add it to faded_tags (with a duplicate check)
            if !entry.faded_tags.iter().any(|t| t == tag) {
                entry.faded_tags.push(tag.to_string());
                if entry.faded_tags.len() > MAX_FADED {
                    entry.faded_tags.remove(0);
                }
            }
            entry.tag_weights.remove(tag);
        } else {
            // The tag came back — remove it from faded_tags
            entry.faded_tags.retain(|t| t != tag);
        }
    }

    /// Acquire a new tag (one that was not in the card).
    pub fn acquire_tag(&mut self, character: &str, tag: &str, strength: f32) {
        if !strength.is_finite() {
            return;
        }
        let entry = self.characters.entry(character.to_string()).or_insert_with(|| EvolvedCharacter {
            tag_weights: HashMap::new(),
            acquired_tags: HashMap::new(),
            faded_tags: Vec::new(),
            interaction_count: 0,
            recent_sentiment: 0.0,
        });
        if !entry.acquired_tags.contains_key(tag)
            && !entry.tag_weights.contains_key(tag)
            && entry.tag_weights.len() + entry.acquired_tags.len() >= MAX_TAGS
        {
            return;
        }
        entry.acquired_tags.insert(tag.to_string(), strength.clamp(0.0, 1.0));
    }

    /// Get effective tags with weights (for the prompt).
    /// Where tag_weights and acquired_tags overlap, tag_weights wins.
    pub fn effective_tags(&self, character: &str) -> Vec<(String, f32)> {
        match self.characters.get(character) {
            Some(evo) => {
                let mut map: HashMap<String, f32> = evo
                    .tag_weights
                    .iter()
                    .map(|(t, &w)| (t.clone(), w))
                    .collect();
                for (t, &w) in &evo.acquired_tags {
                    map.entry(t.clone()).or_insert(w);
                }
                // Zero and NaN weights carry no meaning but take up room in the prompt.
                let mut tags: Vec<(String, f32)> = map
                    .into_iter()
                    .filter(|(_, w)| w.is_finite() && *w > 0.0)
                    .collect();
                tags.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                tags
            }
            None => Vec::new(),
        }
    }

    /// Per-session decay (call at startup). `true` — at least one weight
    /// moved.
    pub fn session_decay(&mut self) -> bool {
        let mut moved = false;
        for evo in self.characters.values_mut() {
            moved |= decay_entry(evo, DECAY_PER_SESSION);
        }
        moved
    }

    /// Seed card tags added after the character was first seen (the card was
    /// edited in SillyTavern). Evolved and faded weights are never reset.
    fn seed_missing_tags(&mut self, name: &str, tags: &[String]) {
        let Some(entry) = self.characters.get_mut(name) else {
            return;
        };
        for tag in tags {
            if tag.trim().is_empty() || !crate::tag_influence::is_character_trait(tag) {
                continue;
            }
            if entry.tag_weights.contains_key(tag) || entry.faded_tags.iter().any(|t| t == tag) {
                continue;
            }
            if entry.tag_weights.len() + entry.acquired_tags.len() >= MAX_TAGS {
                return;
            }
            entry.tag_weights.insert(tag.clone(), 1.0);
        }
    }

    /// Cool one character's weights after an exchange (`shift_and_settle`
    /// pattern): every weight decays toward zero and tags below
    /// `FADE_THRESHOLD` move to `faded_tags`.
    fn decay_character(&mut self, name: &str, decay: f32) {
        if let Some(entry) = self.characters.get_mut(name) {
            decay_entry(entry, decay);
        }
    }
}

/// The fade rule shared by the per-exchange cool-down and the session decay:
/// card tags lose `decay`, acquired tags decay twice as fast (they need active
/// reinforcement), and tags below `FADE_THRESHOLD` leave `tag_weights` for
/// `faded_tags` instead of hanging around as zeros. `true` — anything moved.
fn decay_entry(evo: &mut EvolvedCharacter, decay: f32) -> bool {
    let mut changed = false;
    // Collect the tags to remove so as not to mutate the HashMap during iteration
    let mut faded = Vec::new();
    for (tag, weight) in evo.tag_weights.iter_mut() {
        // Installations before this fix already contain identity tags with weight 1.0:
        // do not let them burn down into faded_tags forever.
        if !crate::tag_influence::is_character_trait(tag) {
            continue;
        }
        let before = *weight;
        *weight = (*weight - decay).max(0.0);
        changed |= *weight != before;
        if *weight < FADE_THRESHOLD {
            faded.push(tag.clone());
        }
    }
    for tag in faded {
        changed = true;
        if !evo.faded_tags.iter().any(|t| *t == tag) {
            evo.faded_tags.push(tag.clone());
            if evo.faded_tags.len() > MAX_FADED {
                evo.faded_tags.remove(0);
            }
        }
        evo.tag_weights.remove(&tag);
    }
    for weight in evo.acquired_tags.values_mut() {
        let before = *weight;
        *weight = (*weight - decay * 2.0).max(0.0);
        changed |= *weight != before;
    }
    // Burned-out acquired tags must not hang in the prompt as zeros.
    let before_len = evo.acquired_tags.len();
    evo.acquired_tags.retain(|_, w| *w >= FADE_THRESHOLD);
    changed |= evo.acquired_tags.len() != before_len;
    changed
}

// --- Writer (V-40) ---

/// One bounded tag-weight shift in delta format: what the mention analysis
/// produces and what the report can show as "why".
#[derive(Debug, Clone, PartialEq)]
pub struct TagShift {
    pub tag: String,
    /// Signed weight change, already bounded to ±`MAX_STEP`.
    pub delta: f32,
    /// Literal mention count behind the delta.
    pub mentions: u32,
}

/// Evidence of one user ↔ character exchange. The gateway builds it at the
/// call point from what it already has (`spawn_evaluation`/`evaluate_state`).
#[derive(Debug, Clone, Copy)]
pub struct ExchangeSignal<'a> {
    /// Character key: avatar file name — the same key as
    /// `traits::st_tagged_characters` and the tag-influence reports.
    pub avatar: &'a str,
    /// The character's card tags; missing ones are seeded at weight 1.0.
    pub card_tags: &'a [String],
    /// The shown reply — the primary honest signal (trait mentions).
    pub reply_text: &'a str,
    /// The user's message; mentions there count as salience too.
    pub user_text: &'a str,
    /// Global tag vocabulary (every name of `traits::st_tagged_characters()`).
    /// A vocabulary tag mentioned in the reply but absent from the card is
    /// acquired at `ACQUIRE_STRENGTH`; empty vocabulary disables acquisition.
    pub vocabulary: &'a [String],
    /// Tone of the exchange in -1..=1 from the state evaluator's verdict
    /// (`sentiment_from_state`). `None` — no evaluation ran (neutral). A value
    /// outside -1..=1 or non-finite breaks the contract: the exchange is
    /// refused and nothing is written.
    pub sentiment: Option<f32>,
}

/// What one committed exchange did — for `boot_log` at the call site and tests.
#[derive(Debug, Clone, PartialEq)]
pub struct ExchangeOutcome {
    pub avatar: String,
    /// Mention-driven shifts actually applied (bounded, delta format).
    pub reinforced: Vec<TagShift>,
    /// Tags acquired from the reply that were not on the card.
    pub acquired: Vec<String>,
    /// Interaction counter after the commit.
    pub interaction_count: u32,
    /// Smoothed tone after the commit (-1..=1).
    pub recent_sentiment: f32,
}

/// Tone of an exchange from the state evaluator's verdict: warm axes up,
/// irritation down. Normalized by the plausible per-axis bound (±15 → ±45 for
/// the sum) and clamped so the EMA in `record_interaction` never overshoots.
pub fn sentiment_from_state(state: &crate::traits::Deltas) -> f32 {
    let raw = (state.affection + state.trust - state.irritation) as f32 / 45.0;
    raw.clamp(-1.0, 1.0)
}

/// Literal mention counts of the character's trait tags in the given texts,
/// mapped to bounded deltas. Only trait tags take part (brand and generation
/// folders are identity, not evolution); matching is case-insensitive
/// containment — the same textual match the tag classifier uses. Unknown words
/// never enter: prose is not cut into "tags" (V-43).
fn parse_mentions(tags: &[String], texts: &[&str]) -> Vec<TagShift> {
    let lowered: Vec<String> = texts.iter().map(|t| t.to_lowercase()).collect();
    let mut shifts = Vec::new();
    for tag in tags {
        let needle = tag.trim().to_lowercase();
        // A one-character needle would match inside almost every word.
        if needle.chars().count() < 2 || !crate::tag_influence::is_character_trait(tag) {
            continue;
        }
        let mentions: u32 = lowered.iter().map(|text| count_occurrences(text, &needle)).sum();
        if mentions == 0 {
            continue;
        }
        shifts.push(TagShift { tag: tag.clone(), delta: mention_delta(mentions), mentions });
    }
    shifts
}

fn count_occurrences(haystack: &str, needle: &str) -> u32 {
    haystack.matches(needle).count() as u32
}

/// Bounded reinforcement for `mentions` hits: `MENTION_STEP` each, saturating
/// at `MAX_STEP` per tag per exchange.
fn mention_delta(mentions: u32) -> f32 {
    (mentions as f32 * MENTION_STEP).clamp(0.0, MAX_STEP)
}

/// Vocabulary tags the reply manifests but the card does not list — candidates
/// for `acquire_tag`. The reply only: the user's message talks about whatever,
/// the character's own lines are the evidence of behavior.
fn parse_acquires(reply_text: &str, card_tags: &[String], vocabulary: &[String]) -> Vec<String> {
    let folded_card: Vec<String> = card_tags.iter().map(|t| t.trim().to_lowercase()).collect();
    let lower = reply_text.to_lowercase();
    vocabulary
        .iter()
        .filter(|tag| {
            let needle = tag.trim().to_lowercase();
            needle.chars().count() >= 2
                && crate::tag_influence::is_character_trait(tag)
                && !folded_card.iter().any(|c| *c == needle)
                && lower.contains(&needle)
        })
        .cloned()
        .collect()
}

/// Path of `data/character_evolution.json` — the same layout as the other
/// stores.
pub fn store_path() -> PathBuf {
    crate::paths::data_dir().join("character_evolution.json")
}

/// Commit one exchange — the writer V-40 was missing. Blocking file IO on the
/// caller's thread; async gateway paths run it via `spawn_blocking` (V-17).
///
/// Signal and clamps: literal mentions of trait tags in the reply and the
/// user's message reinforce them by `MENTION_STEP` each, at most `MAX_STEP` per
/// tag per exchange; vocabulary tags mentioned in the reply but absent from the
/// card are acquired at `ACQUIRE_STRENGTH`; the evaluator's tone (`sentiment`,
/// -1..=1) moves the smoothed `recent_sentiment`. Afterwards every weight of
/// the character cools toward zero by `DECAY_PER_EXCHANGE` and tags below
/// `FADE_THRESHOLD` fade — the `traits::evolve`/`shift_and_settle` cool-down,
/// without it weights only grew (V-41 lesson).
///
/// Commit only on success: a violated signal contract (empty avatar, sentiment
/// outside -1..=1) refuses the whole exchange — no file is created or touched
/// — and a store file that exists but cannot be read or parsed is refused the
/// same way (V-09), its bytes preserved as `.json.corrupt`.
pub fn apply_exchange(signal: &ExchangeSignal<'_>) -> Result<ExchangeOutcome, String> {
    apply_exchange_at(&store_path(), signal)
}

fn apply_exchange_at(path: &Path, signal: &ExchangeSignal<'_>) -> Result<ExchangeOutcome, String> {
    let result = crate::json_store::update_if_ok(&STORE, path, |evo: &mut CharacterEvolution| {
        // Fail closed: any contract violation below refuses the exchange and
        // `update_if_ok` leaves the file untouched.
        let avatar = signal.avatar.trim();
        if avatar.is_empty() {
            return Err("character_evolution: avatar is required".to_string());
        }
        let sentiment = match signal.sentiment {
            Some(s) if !s.is_finite() || !(-1.0..=1.0).contains(&s) => {
                return Err(format!("character_evolution: sentiment {s} outside -1..=1"));
            }
            Some(s) => s,
            None => 0.0,
        };
        let card_tags: Vec<String> = signal
            .card_tags
            .iter()
            .filter(|t| !t.trim().is_empty())
            .cloned()
            .collect();

        evo.init_character(avatar, &card_tags);
        evo.seed_missing_tags(avatar, &card_tags);

        // Acquired tags are reinforced by mention too: before, they were only
        // re-acquired at 0.25 and faded, the weight never grew.
        let mut mention_tags = card_tags.clone();
        if let Some(entry) = evo.characters.get(avatar) {
            mention_tags.extend(entry.acquired_tags.keys().cloned());
        }
        let mut reinforced = parse_mentions(&mention_tags, &[signal.reply_text, signal.user_text]);
        for shift in &reinforced {
            evo.shift_tag(avatar, &shift.tag, shift.delta);
        }
        let mut acquired = parse_acquires(signal.reply_text, &card_tags, signal.vocabulary);
        for tag in &acquired {
            evo.acquire_tag(avatar, tag, ACQUIRE_STRENGTH);
        }
        evo.record_interaction(avatar, sentiment);
        evo.decay_character(avatar, DECAY_PER_EXCHANGE);

        // Report only what survived the caps (`MAX_TAGS` may refuse new tags).
        let entry = evo.characters.get(avatar);
        reinforced.retain(|s| entry.is_some_and(|e| e.tag_weights.contains_key(&s.tag)));
        acquired.retain(|t| entry.is_some_and(|e| e.acquired_tags.contains_key(t)));
        let (interaction_count, recent_sentiment) = entry
            .map(|e| (e.interaction_count, e.recent_sentiment))
            .unwrap_or((0, 0.0));
        Ok(ExchangeOutcome {
            avatar: avatar.to_string(),
            reinforced,
            acquired,
            interaction_count,
            recent_sentiment,
        })
    });
    match result {
        Ok(Ok(outcome)) => Ok(outcome),
        Ok(Err(msg)) | Err(msg) => Err(msg),
    }
}

/// Session-boundary cool-down on disk (the `relationships::decay_session`
/// analogue): the gateway calls it once per session — at app start or at an
/// active-card switch. `true` — at least one weight moved. Blocking IO on the
/// caller's thread.
pub fn decay_session() -> Result<bool, String> {
    decay_session_at(&store_path())
}

fn decay_session_at(path: &Path) -> Result<bool, String> {
    crate::json_store::update(&STORE, path, |evo: &mut CharacterEvolution| evo.session_decay())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_and_get() {
        let mut evo = CharacterEvolution::default();
        evo.init_character("Alice", &["tsundere".into(), "gamer".into()]);
        let tags = evo.effective_tags("Alice");
        assert_eq!(tags.len(), 2);
        assert!((tags[0].1 - 1.0).abs() < 0.01);
    }

    #[test]
    fn shift_tag_works() {
        let mut evo = CharacterEvolution::default();
        evo.init_character("Dana", &["gentle".into()]);
        evo.shift_tag("Dana", "gentle", 0.5);
        let tags = evo.effective_tags("Dana");
        assert!(tags.iter().any(|(t, w)| t == "gentle" && *w > 1.0));
    }

    #[test]
    fn acquire_tag_works() {
        let mut evo = CharacterEvolution::default();
        evo.init_character("Bella", &["chaotic".into()]);
        evo.acquire_tag("Bella", "romantic", 0.6);
        let tags = evo.effective_tags("Bella");
        assert!(tags.iter().any(|(t, _)| t == "romantic"));
    }

    #[test]
    fn fade_works() {
        let mut evo = CharacterEvolution::default();
        evo.init_character("Test", &["rare".into()]);
        evo.shift_tag("Test", "rare", -0.9); // weight → 0.1 < FADE_THRESHOLD
        let evo_char = evo.characters.get("Test").unwrap();
        assert!(evo_char.faded_tags.contains(&"rare".to_string()));
        assert!(!evo_char.tag_weights.contains_key("rare"));
    }

    #[test]
    fn session_decay() {
        let mut evo = CharacterEvolution::default();
        evo.init_character("A", &["tag1".into()]);
        evo.session_decay();
        let w = evo.characters.get("A").unwrap().tag_weights["tag1"];
        assert!(w < 1.0);
    }

    /// NaN would slip through clamp and stay in the weight forever.
    #[test]
    fn nan_delta_is_ignored() {
        let mut evo = CharacterEvolution::default();
        evo.init_character("B", &["calm".into()]);
        evo.shift_tag("B", "calm", f32::NAN);
        let w = evo.characters.get("B").unwrap().tag_weights["calm"];
        assert!(w.is_finite());
        assert!((w - 1.0).abs() < 0.01);
    }

    /// Burned-out acquired tags must not remain zeros in the prompt.
    #[test]
    fn decayed_acquired_tags_are_dropped() {
        let mut evo = CharacterEvolution::default();
        evo.acquire_tag("C", "romantic", 0.2);
        // While the weight is above the threshold, the tag lives.
        evo.session_decay();
        assert!(!evo.effective_tags("C").is_empty());
        // Beyond that the weight is too low — the entry must disappear, not hang as a zero.
        for _ in 0..4 {
            evo.session_decay();
        }
        assert!(evo.effective_tags("C").is_empty());
    }

    /// A tag from acquired_tags must not start over at weight 1.0.
    #[test]
    fn shift_takes_weight_from_acquired() {
        let mut evo = CharacterEvolution::default();
        evo.acquire_tag("D", "brave", 0.4);
        evo.shift_tag("D", "brave", 0.1);
        let entry = evo.characters.get("D").unwrap();
        assert!(!entry.acquired_tags.contains_key("brave"));
        let w = entry.tag_weights["brave"];
        assert!((w - 0.5).abs() < 0.01, "ожидалось 0.5, получено {w}");
    }

    #[test]
    fn effective_tags_skips_zero_weights() {
        let mut evo = CharacterEvolution::default();
        evo.acquire_tag("E", "faded", 0.0);
        assert!(evo.effective_tags("E").iter().all(|(t, _)| t != "faded"));
    }

    /// An old file without the new fields loads instead of resetting to empty.
    #[test]
    fn old_schema_without_new_fields_loads() {
        let json = r#"{"characters": {"Alice": {"tag_weights": {"tsundere": 1.5}}}}"#;
        let evo: CharacterEvolution = serde_json::from_str(json).unwrap();
        let m = evo.characters.get("Alice").unwrap();
        assert!((m.tag_weights["tsundere"] - 1.5).abs() < 0.01);
        assert!(m.acquired_tags.is_empty());
        assert!(m.faded_tags.is_empty());
        assert_eq!(m.interaction_count, 0);
        assert!(m.recent_sentiment.abs() < 0.01);
    }

    /// An empty object is valid too — the fields are filled in with defaults.
    #[test]
    fn empty_object_loads() {
        let evo: CharacterEvolution = serde_json::from_str("{}").unwrap();
        assert!(evo.characters.is_empty());
    }

    /// A corrupted file is kept as a copy instead of being overwritten by the next save.
    #[test]
    fn load_corrupt_backs_up_and_defaults() {
        let dir = std::env::temp_dir().join("cloufam_test_evo_corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("character_evolution.json");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("json.corrupt"));
        std::fs::write(&path, "not json {{{").unwrap();
        let evo = CharacterEvolution::load(&path);
        assert!(evo.characters.is_empty());
        let backup = path.with_extension("json.corrupt");
        assert!(backup.exists(), "копия повреждённого файла не создана");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "not json {{{");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&backup);
        let _ = std::fs::remove_dir(&dir);
    }

    // --- V-40 writer: parsing, clamp, commit-on-success ---

    fn temp_store(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("cloufam_test_evo_writer");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("json.corrupt"));
        path
    }

    fn tags(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn signal<'a>(
        avatar: &'a str,
        card: &'a [String],
        reply: &'a str,
        user: &'a str,
        vocab: &'a [String],
        sentiment: Option<f32>,
    ) -> ExchangeSignal<'a> {
        ExchangeSignal {
            avatar,
            card_tags: card,
            reply_text: reply,
            user_text: user,
            vocabulary: vocab,
            sentiment,
        }
    }

    #[test]
    fn parse_mentions_counts_trait_tags_only() {
        let card = tags(&["hololive", "Hololive EN -Myth-", "цундере", "kitsune"]);
        let shifts =
            parse_mentions(&card, &["Ты настоящая ЦУНДЕРЕ, kitsune!", "и снова Kitsune тут"]);
        // Brand and generation folder are identity, not evolution: they are not
        // reinforced; traits are matched case-insensitively.
        assert_eq!(shifts.len(), 2, "{shifts:?}");
        let kitsune = shifts.iter().find(|s| s.tag == "kitsune").unwrap();
        assert_eq!(kitsune.mentions, 2);
        let tsundere = shifts.iter().find(|s| s.tag == "цундере").unwrap();
        assert_eq!(tsundere.mentions, 1);
    }

    /// Brand and generation do not become weights: they are not reinforced, and decay
    /// would eventually send them to faded_tags forever.
    #[test]
    fn identity_tags_are_not_seeded() {
        let mut evo = CharacterEvolution::default();
        evo.init_character(
            "Alice",
            &tags(&["russian", "Hololive EN -Myth-", "цундере"]),
        );
        let effective = evo.effective_tags("Alice");
        assert!(!effective.iter().any(|(tag, _)| tag == "russian"));
        assert!(!effective.iter().any(|(tag, _)| tag.starts_with("Hololive")));
        assert!(effective.iter().any(|(tag, _)| tag == "цундере"));
    }

    /// An acquired tag is reinforced by the next mention, not merely
    /// re-acquired at 0.25 and faded.
    #[test]
    fn acquired_tag_is_reinforced_on_next_mention() {
        let path = temp_store("acquired-reinforce.json");
        let card = tags(&["цундере"]);
        let vocab = tags(&["кулинария"]);
        let signal = signal("Alice.png", &card, "*ловко готовит кулинария*", "", &vocab, None);
        let first = apply_exchange_at(&path, &signal).unwrap();
        assert_eq!(first.acquired, vec!["кулинария".to_string()]);
        let second = apply_exchange_at(&path, &signal).unwrap();
        assert!(
            second.reinforced.iter().any(|shift| shift.tag == "кулинария"),
            "упоминание не усилило приобретённый тег: {second:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// V-43: prose is not cut into "tags" — only registered character
    /// traits are matched.
    #[test]
    fn parse_mentions_ignores_prose() {
        let card = tags(&["цундере"]);
        let shifts = parse_mentions(&card, &["лес шумит, рыцарь едет через дерево"]);
        assert!(shifts.is_empty());
    }

    #[test]
    fn mention_delta_is_bounded_per_exchange() {
        assert_eq!(mention_delta(0), 0.0);
        assert!((mention_delta(1) - MENTION_STEP).abs() < 1e-6);
        assert!(
            (mention_delta(100) - MAX_STEP).abs() < 1e-6,
            "дельта обязана упираться в потолок"
        );
    }

    #[test]
    fn sentiment_from_state_is_clamped() {
        let warm = crate::traits::Deltas {
            affection: 15,
            trust: 15,
            energy: 5,
            irritation: -15,
            tone: crate::traits::Tone::Tenderness,
        };
        assert!((sentiment_from_state(&warm) - 1.0).abs() < 1e-6);
        let cold = crate::traits::Deltas {
            affection: -15,
            trust: -15,
            energy: 0,
            irritation: 15,
            tone: crate::traits::Tone::Hostile,
        };
        assert!((sentiment_from_state(&cold) + 1.0).abs() < 1e-6);
        assert_eq!(sentiment_from_state(&crate::traits::Deltas::default()), 0.0);
    }

    #[test]
    fn apply_exchange_commits_on_success() {
        let path = temp_store("commit_ok.json");
        let card = tags(&["цундере", "kitsune"]);
        let vocab = tags(&["цундере", "kitsune", "певица"]);
        let outcome = apply_exchange_at(
            &path,
            &signal("Alice.png", &card, "Я сегодня особенно цундере!", "привет", &vocab, Some(0.5)),
        )
        .unwrap();
        assert_eq!(outcome.interaction_count, 1);
        assert!(outcome.reinforced.iter().any(|s| s.tag == "цундере"));
        let saved = CharacterEvolution::load(&path);
        let entry = saved.characters.get("Alice.png").unwrap();
        assert!(
            entry.tag_weights["цундере"] > 1.0,
            "упоминание обязано усилить тег: {}",
            entry.tag_weights["цундере"]
        );
        assert!(
            entry.tag_weights["kitsune"] < 1.0,
            "неподкреплённый тег обязан остывать: {}",
            entry.tag_weights["kitsune"]
        );
        assert!((entry.recent_sentiment - 0.05).abs() < 1e-6);
        let _ = std::fs::remove_file(&path);
    }

    /// Commit only on success: a signal contract violation creates no file and
    /// does not touch the existing one.
    #[test]
    fn refused_signal_writes_nothing() {
        let path = temp_store("commit_refused.json");
        let card = tags(&["цундере"]);
        let vocab = tags(&["цундере"]);

        let bad_sentiment = signal("Alice.png", &card, "привет", "", &vocab, Some(42.0));
        let err = apply_exchange_at(&path, &bad_sentiment).unwrap_err();
        assert!(err.contains("sentiment"), "{err}");
        assert!(!path.exists(), "отказ не должен создавать файл");

        std::fs::write(&path, r#"{"characters": {}}"#).unwrap();
        let before = std::fs::read(&path).unwrap();

        let no_avatar = signal("  ", &card, "привет", "", &vocab, Some(0.0));
        assert!(apply_exchange_at(&path, &no_avatar).is_err());
        let nan = signal("Alice.png", &card, "привет", "", &vocab, Some(f32::NAN));
        assert!(apply_exchange_at(&path, &nan).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before, "отказ перезаписал хранилище");
        let _ = std::fs::remove_file(&path);
    }

    /// V-09/V-31: a corrupted file is not overwritten with an empty store, its bytes
    /// are preserved as a copy.
    #[test]
    fn corrupt_store_is_refused_and_preserved() {
        let path = temp_store("commit_corrupt.json");
        std::fs::write(&path, "not json {{{").unwrap();
        let card = tags(&["цундере"]);
        let vocab = tags(&["цундере"]);
        let err =
            apply_exchange_at(&path, &signal("Alice.png", &card, "цундере", "", &vocab, None))
                .unwrap_err();
        assert!(err.contains("повреждён"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json {{{");
        let backup = path.with_extension("json.corrupt");
        assert!(backup.exists(), "копия повреждённого файла не создана");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&backup);
    }

    /// Cooling like in `traits::evolve`: unreinforced tags return to
    /// zero and fade, rather than only growing (V-41).
    #[test]
    fn unmentioned_tags_cool_down_and_fade() {
        let path = temp_store("cooling.json");
        let card = tags(&["цундере", "kitsune"]);
        let vocab = tags(&["цундере", "kitsune"]);
        for _ in 0..3 {
            apply_exchange_at(&path, &signal("A.png", &card, "цундере", "", &vocab, None)).unwrap();
        }
        let saved = CharacterEvolution::load(&path);
        let entry = saved.characters.get("A.png").unwrap();
        assert!(entry.tag_weights["цундере"] > entry.tag_weights["kitsune"]);
        assert!(entry.tag_weights["kitsune"] < 1.0);
        // Session cooling brings an unreinforced tag to the threshold: it goes
        // to faded_tags, while a reinforced one lives on.
        for _ in 0..100 {
            assert!(decay_session_at(&path).is_ok());
        }
        let saved = CharacterEvolution::load(&path);
        let entry = saved.characters.get("A.png").unwrap();
        assert!(entry.faded_tags.contains(&"kitsune".to_string()));
        assert!(!entry.faded_tags.contains(&"цундере".to_string()));
        assert!(entry.tag_weights.contains_key("цундере"));
        let _ = std::fs::remove_file(&path);
    }

    /// An acquired tag: only a vocabulary tag from the character's reply — never
    /// from the user's turn and never an unknown word.
    #[test]
    fn acquisition_comes_from_the_reply_vocabulary_only() {
        let path = temp_store("acquire.json");
        let card = tags(&["цундере"]);
        let vocab = tags(&["цундере", "певица", "kitsune"]);

        // The user mentions kitsune — that is not the character's behaviour.
        apply_exchange_at(&path, &signal("A.png", &card, "ла-ла-ла", "ты же kitsune?", &vocab, None))
            .unwrap();
        let saved = CharacterEvolution::load(&path);
        let entry = saved.characters.get("A.png").unwrap();
        assert!(!entry.acquired_tags.contains_key("kitsune"));

        // The character's reply mentions the singer — the tag is acquired at weak strength.
        apply_exchange_at(&path, &signal("A.png", &card, "я же певица!", "", &vocab, None)).unwrap();
        let saved = CharacterEvolution::load(&path);
        let entry = saved.characters.get("A.png").unwrap();
        assert!(entry.acquired_tags.contains_key("певица"));
        assert!(
            entry.acquired_tags["певица"] < 1.0,
            "приобретённый тег не перекрикивает карточку"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The card was extended with tags: new ones take the base weight, the evolution of
    /// the previous ones is not recomputed.
    #[test]
    fn new_card_tags_are_seeded_without_resetting_evolved_weights() {
        let path = temp_store("seed.json");
        let vocab = tags(&["цундере", "kitsune"]);
        let first = tags(&["цундере"]);
        apply_exchange_at(&path, &signal("A.png", &first, "цундере", "", &vocab, None)).unwrap();
        let grown = tags(&["цундере", "kitsune"]);
        apply_exchange_at(&path, &signal("A.png", &grown, "", "", &vocab, None)).unwrap();
        let saved = CharacterEvolution::load(&path);
        let entry = saved.characters.get("A.png").unwrap();
        assert!(
            entry.tag_weights["цундере"] > 1.0,
            "эволюционировавший вес не сбрасывается"
        );
        assert!(
            (entry.tag_weights["kitsune"] - 1.0).abs() < 0.01,
            "новый тег карточки стартует с 1.0: {}",
            entry.tag_weights["kitsune"]
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn decay_session_reports_movement() {
        let path = temp_store("decay_session.json");
        let card = tags(&["цундере"]);
        let vocab = tags(&["цундере"]);
        assert!(!decay_session_at(&path).unwrap(), "пустое хранилище не двигается");
        apply_exchange_at(&path, &signal("A.png", &card, "цундере", "", &vocab, None)).unwrap();
        assert!(decay_session_at(&path).unwrap());
        let _ = std::fs::remove_file(&path);
    }
}

