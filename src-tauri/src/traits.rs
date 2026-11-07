//! Character traits: how vividly a character expresses its tags and whether it
//! repeats the same actions.
//!
//! Stored in `data/traits.json`: `{ "<avatar.png>": { "no_repeat": true,
//! "intensity": { "Энергичность": 30 } } }`. Delivered into generation via the
//! card's `depth_prompt` — SillyTavern injects it into the prompt at the given
//! depth as a separate message (`script.js`, `inject_ids.DEPTH_PROMPT`), so the
//! character description and the user card are not rewritten.
//!
//! Live state is absent from this text on purpose: it changes every turn and
//! goes into the prompt as a separate gateway note (`gateway::live_state_note`) —
//! the note also tracks who exactly the conversation is with. In the card it
//! would grow stale (the card is written when settings are saved) and contradict the note.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::paths;
use crate::sillytavern;

pub type Store = BTreeMap<String, CharacterTraits>;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CharacterTraits {
    /// "Do not repeat the same gestures and actions" — a request familiar from
    /// characters who jump and wave their arms in every turn.
    pub no_repeat: bool,
    /// Behavior chaos: 0 — predictable, 100 — unpredictable.
    pub chaos: u8,
    /// Live state: changes as the conversation goes (see `evolve`).
    pub live: bool,
    /// State toward the current interlocutor (what the UI shows and goes into the text).
    pub state: LiveState,
    /// Attitude toward each interlocutor separately: person name → state.
    /// The same character has a different attitude to different people, so
    /// the state is stored for the "character + interlocutor" pair.
    pub relations: BTreeMap<String, LiveState>,
    /// Last communication tone with each person: used to catch a reversal
    /// (affection followed by rudeness).
    pub tones: BTreeMap<String, Tone>,
    /// Tone for the anonymous interlocutor (empty persona): without it reversal
    /// does not fire, because tones stores no empty key.
    #[serde(default)]
    pub last_tone: Tone,
    /// Tag → how vividly to express it, 0..100.
    pub intensity: BTreeMap<String, u8>,
}

/// A character without its own record behaves alive and does not repeat: the setting
/// was per-card, and out of 72 cards only one had it — the rest went into the prompt
/// without state and without a repetition ban. They can still be disabled in the
/// settings panel, but enabling each one by hand is no longer needed.
impl Default for CharacterTraits {
    fn default() -> Self {
        Self {
            no_repeat: true,
            chaos: 0,
            live: true,
            state: LiveState::neutral(),
            relations: BTreeMap::new(),
            tones: BTreeMap::new(),
            last_tone: Tone::Neutral,
            intensity: BTreeMap::new(),
        }
    }
}

/// Character state in a conversation. Not "personality", but what changes from
/// turn to turn: affection, trust, energy and irritation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct LiveState {
    /// Warmth, affection: what collapses from rudeness in a single turn.
    #[serde(alias = "mood")]
    pub affection: u8,
    pub trust: u8,
    pub energy: u8,
    pub irritation: u8,
}

impl Default for LiveState {
    fn default() -> Self {
        Self::neutral()
    }
}

impl LiveState {
    /// Calm baseline the state returns to on its own.
    pub fn neutral() -> Self {
        Self { affection: 50, trust: 50, energy: 55, irritation: 10 }
    }
}

/// In what key the communication goes. Needed to catch a reversal: after affection
/// rudeness is not "minus a few points" but a break.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    /// Warmth, caress, confessions.
    Tenderness,
    Flirt,
    Playful,
    #[default]
    Neutral,
    /// Coldness, distance, resentment.
    Cold,
    /// Rudeness, insults, aggression.
    Hostile,
}

impl Tone {
    pub fn is_warm(self) -> bool {
        matches!(self, Tone::Tenderness | Tone::Flirt)
    }

    pub fn is_cold(self) -> bool {
        matches!(self, Tone::Hostile | Tone::Cold)
    }

    /// Tone parsing from the model reply without failing: tone is an informational label,
    /// and an unknown word ("dramatic", "весёлый") must not drop the whole evaluation
    /// with its numeric shifts (18.10.2026: because of strict-enum the live state
    /// did not move, although the deltas were correct). Unknown → `Neutral`.
    pub fn parse_lossy(raw: &str) -> Tone {
        match raw.trim().to_lowercase().as_str() {
            "tenderness" | "нежность" | "warm" | "тепло" => Tone::Tenderness,
            "flirt" | "флирт" => Tone::Flirt,
            "playful" | "игривый" | "весёлый" | "веселый" => Tone::Playful,
            "cold" | "холод" | "холодный" | "обида" => Tone::Cold,
            "hostile" | "грубость" | "вражда" | "агрессия" => Tone::Hostile,
            _ => Tone::Neutral,
        }
    }
}

/// Evaluation of one exchange of turns: how much and where the state shifted, and in
/// what key the user's turn was.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Deltas {
    pub affection: i32,
    pub trust: i32,
    pub energy: i32,
    pub irritation: i32,
    pub tone: Tone,
}

impl Deltas {
    pub fn is_zero(&self) -> bool {
        self.affection == 0 && self.trust == 0 && self.energy == 0 && self.irritation == 0
    }

    /// Sharp event: a quarrel, a confession, a hit on a sore spot. Such turns bypass
    /// smoothing — otherwise "beloved" and "go to hell" look the same.
    pub fn is_sharp(&self) -> bool {
        [self.affection, self.trust, self.energy, self.irritation]
            .iter()
            .any(|d| d.abs() >= SHARP_DELTA)
    }

    /// Evaluation within the contract. Weak models, instead of a shift, rewrite the current
    /// state into the answer ("affection 50" → `affection: 50`), and a normal
    /// "hello" turned into a sharp event with a 40-point jump. Such an
    /// evaluation must not be applied: it is not about the conversation.
    pub fn is_plausible(&self) -> bool {
        [self.affection, self.trust, self.energy, self.irritation]
            .iter()
            .all(|d| d.abs() <= MAX_STEP)
    }
}

/// Maximum shift per turn: a quarrel in one phrase does not turn into
/// "I hate you", and a confession — into "love forever". Sharp turns go a separate
/// path (`is_sharp`).
const MAX_STEP: i32 = 15;
/// How large an evaluation counts as a sharp event.
const SHARP_DELTA: i32 = 12;
/// Sharp event step: a hit on a sore spot is not smoothed.
const SHARP_STEP: i32 = 40;
/// Share of the evaluation applied immediately: the rest is character inertia.
const SMOOTHING_PERCENT: i32 = 60;
/// Return to the calm baseline per turn.
const DECAY_TO_NEUTRAL: i32 = 2;
/// Irritation cools faster than the rest.
const DECAY_IRRITATION: i32 = 3;

fn step(value: u8, delta: i32, neutral: u8, decay: i32) -> u8 {
    // Rounding, not truncation toward zero: +1 for "ordinary chatter" otherwise gave 0, and cooling
    // in the same step turned a positive evaluation into a loss
    let scaled = delta.clamp(-MAX_STEP, MAX_STEP) * SMOOTHING_PERCENT;
    let smoothed = (scaled + scaled.signum() * 50) / 100;
    let next = value as i32 + smoothed;
    settle(next, neutral, decay)
}

/// Cooling: the state is pulled toward the calm baseline and stops at it,
/// not jumping over it (otherwise values would jitter around the baseline).
fn settle(next: i32, neutral: u8, decay: i32) -> u8 {
    let neutral = neutral as i32;
    let settled = if next > neutral {
        (next - decay).max(neutral)
    } else if next < neutral {
        (next + decay).min(neutral)
    } else {
        next
    };
    settled.clamp(0, 100) as u8
}

/// New state after an exchange of turns. Three modes:
/// 1. **Tone reversal** — rudeness after affection (or vice versa): affection
///    breaks almost to zero, irritation spikes. Exactly the case when
///    "darling, sweetie" is replaced by an insult — the state does not catch up
///    gradually but breaks at once.
/// 2. **Sharp turn** (evaluation ≥ 12) — a step of up to 40 points with no smoothing.
/// 3. Normal turn — 60% of the evaluation, step no more than 15, then cooling to the baseline.
///
/// Pure function: behavior is checked by a test without a model and without a chat.
pub fn evolve(current: &LiveState, deltas: &Deltas, previous_tone: Tone) -> LiveState {
    let neutral = LiveState::neutral();
    let crossed = (previous_tone.is_warm() && deltas.tone.is_cold())
        || (previous_tone.is_cold() && deltas.tone.is_warm());
    // A break is rudeness on one side of the transition. `Cold` is also light
    // distance: after flirting a reserved "I have to go" with an evaluation of −2 collapsed
    // affection tenfold. Without rudeness, only the transition
    // that the evaluator itself marked sharp counts as a reversal.
    let reversal = crossed
        && (previous_tone == Tone::Hostile || deltas.tone == Tone::Hostile || deltas.is_sharp());

    if reversal {
        // What was warmth is what collapses: after rudeness affection drops almost to
        // zero, after rudeness — warmth returns cautiously (trust more slowly).
        return if deltas.tone.is_cold() {
            LiveState {
                affection: if deltas.is_sharp() { 0 } else { current.affection / 10 },
                trust: current.trust.saturating_sub(25).max(5).min(100),
                energy: settle(current.energy as i32 - 10, neutral.energy, DECAY_TO_NEUTRAL),
                irritation: current.irritation.max(70).min(100),
            }
        } else {
            LiveState {
                affection: current.affection.max(60).min(100),
                trust: settle(current.trust as i32 + 10, neutral.trust, 1),
                energy: settle(current.energy as i32 + 10, neutral.energy, DECAY_TO_NEUTRAL),
                irritation: current.irritation / 4,
            }
        };
    }

    let sharp = deltas.is_sharp();
    let applied = |value: u8, delta: i32, neutral: u8, decay: i32| -> u8 {
        if sharp {
            // Sharp event: a step of up to 40 points, no smoothing applied.
            let moved = (value as i32 + delta.clamp(-SHARP_STEP, SHARP_STEP)).clamp(0, 100);
            settle(moved, neutral, decay)
        } else {
            step(value, delta, neutral, decay)
        }
    };
    LiveState {
        affection: applied(current.affection, deltas.affection, neutral.affection, DECAY_TO_NEUTRAL),
        trust: applied(current.trust, deltas.trust, neutral.trust, DECAY_TO_NEUTRAL),
        energy: applied(current.energy, deltas.energy, neutral.energy, DECAY_TO_NEUTRAL),
        irritation: applied(
            current.irritation,
            deltas.irritation,
            neutral.irritation,
            DECAY_IRRITATION,
        ),
    }
}

fn store_path() -> std::path::PathBuf {
    paths::base_dir().join("data").join("traits.json")
}

/// Lock on the file: two parties can write at once — the evaluator in the gateway (after
/// each turn) and the "Save" button in the settings window. Without a shared lock,
/// parallel "read → modify → write" lose each other's edits.
static STORE: Mutex<()> = Mutex::new(());

/// Read-only load of `data/traits.json`. Missing, unreadable or corrupt files
/// fall back to an empty store — a corrupt one is preserved as
/// `traits.json.corrupt` by `json_store`. Writers go through `update`, which
/// refuses those states instead of overwriting real data (V-09). Blocking IO
/// on the caller's thread.
pub fn load() -> Store {
    crate::json_store::load(&STORE, &store_path())
}

/// Load from an explicit path — for API callers that read another file (for
/// example an exported copy) and do not hold the store lock.
pub fn load_from(path: &std::path::Path) -> Store {
    match crate::json_store::read_state(path) {
        Ok(Some(store)) => store,
        _ => Store::new(),
    }
}

/// Change the store under its lock: read, change and write as one operation.
/// Delegates to `json_store` — the single parse-with-backup and
/// tmp + fsync + rename implementation (V-31) — and follows its V-09 rule:
/// an existing file that cannot be read or parsed is an error, not an empty
/// store. Blocking IO on the caller's thread.
pub fn update<T>(change: impl FnOnce(&mut Store) -> T) -> Result<T, String> {
    crate::json_store::update(&STORE, &store_path(), change)
}

/// In words, not percentages: the model follows wording better than a number.
fn wording(percent: u8) -> &'static str {
    match percent {
        0 => "не проявлять",
        1..=15 => "едва заметно",
        16..=40 => "сдержанно",
        41..=65 => "умеренно",
        66..=90 => "заметно",
        _ => "ведущая черта",
    }
}

/// How the character behaves with such chaos. In words, not a percentage:
/// wording works better than a number.
fn chaos_wording(percent: u8) -> &'static str {
    match percent {
        0 => "держится ровно и предсказуемо",
        1..=20 => "почти всегда предсказуем, изредка удивляет мелочью",
        21..=45 => "иногда ведёт себя неожиданно",
        46..=70 => "часто удивляет: может сменить тему, пошутить невпопад, ошибиться",
        71..=90 => "непредсказуем, любит резкие повороты и странные идеи",
        _ => "совершенно непредсказуем: может выкинуть что угодно в рамках своего характера",
    }
}

/// Text for `depth_prompt`. Empty string — no traits, no reason to touch the card.
/// Zero in the list is kept: "do not express" is an instruction to the model, not a missing
/// setting (in the card description the trait may be written as leading).
pub fn fragment(traits: &CharacterTraits) -> String {
    let mut lines: Vec<String> = Vec::new();
    if !traits.intensity.is_empty() {
        let items: Vec<String> = traits
            .intensity
            .iter()
            .filter_map(|(tag, percent)| {
                let tag = clean_tag(tag);
                (!tag.is_empty()).then(|| format!("— {tag}: {percent}% ({})", wording(*percent)))
            })
            .collect();
        if !items.is_empty() {
            lines.push("Черты характера — соблюдать эти пропорции:".to_string());
            lines.extend(items);
        }
    }
    if traits.chaos > 0 {
        lines.push(format!(
            "Хаотичность: {}% — {}.",
            traits.chaos,
            chaos_wording(traits.chaos)
        ));
    }
    if traits.no_repeat {
        lines.push(
            "Не повторяй жесты, позы и действия из прошлых реплик: каждый ответ приносит новое."
                .to_string(),
        );
    }
    lines.join("\n")
}

/// Tag name on one line: a newline or a long space in a tag would break
/// the note structure, where each trait is its own line.
fn clean_tag(tag: &str) -> String {
    tag.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Character name from the card file name: `Clara_Moss.png` → `Clara Moss`.
pub fn display_name(avatar: &str) -> String {
    // The extension in ST can be in any case: .PNG is a card too.
    // Byte slicing can land inside a multibyte character and panic.
    let stem = match avatar.len().checked_sub(4) {
        Some(cut)
            if avatar
                .get(cut..)
                .is_some_and(|s| s.eq_ignore_ascii_case(".png")) =>
        {
            &avatar[..cut]
        }
        _ => avatar,
    };
    stem.replace('_', " ")
}

/// State note for the prompt: the gateway mixes it in as the last system
/// message, because the SillyTavern history does not change.
///
/// Numbers do not go into the prompt. On a calm baseline (50/50/55/10) the middle of
/// the scale reads to the model as "keep an even polite tone", and that is exactly what
/// players describe as "detached, as if noticing out of the corner of one's eye" — the
/// note that should have enlivened the character was muting it. The thresholds below
/// give a manner of speech in words, not a number, in the middle of the scale.
pub fn state_note(character: &str, persona: &str, state: LiveState) -> String {
    let with = if persona.is_empty() {
        String::new()
    } else {
        format!(" с {persona}")
    };
    format!(
        "Сейчас {character}{with}: {}. \
         Это не справка о персонаже, а тон текущей реплики: пусть он читается в выборе слов, \
         длине ответов и поступках. Не выравнивай подачу в вежливую середину и не пересказывай \
         своё состояние прямым текстом.",
        mood_markers(state)
    )
}

/// Verbal state markers by thresholds. Gender is never named: cards and
/// players address a character differently, and the wrong ending the model
/// carries into the reply.
fn mood_markers(state: LiveState) -> String {
    let mut marks: Vec<&str> = Vec::new();
    if state.irritation >= 60 {
        marks.push("раздражение прёт наружу");
    } else if state.irritation >= 35 {
        marks.push("раздражение копится");
    }
    if state.affection >= 70 {
        marks.push("тепло, тянет к собеседнику");
    } else if state.affection <= 35 {
        marks.push("холодно, держит дистанцию");
    }
    if state.trust <= 35 {
        marks.push("не откровенничает");
    } else if state.trust >= 70 {
        marks.push("держится открыто");
    }
    if state.energy <= 35 {
        marks.push("на спаде, вяло");
    } else if state.energy >= 75 {
        marks.push("на взводе, много энергии");
    }
    if marks.is_empty() {
        marks.push("спокойно и с интересом к происходящему");
    }
    marks.join(", ")
}

/// All characters with tags from SillyTavern settings: avatar → list of tag names.
/// Used by the gateway to generate default fragments for characters with tags
/// but without configured character traits, and by the traits panel. `tag_map` holds
/// identifiers — names are expanded by `read_st_tag_map` (otherwise the UI
/// would get uuids).
pub fn st_tagged_characters() -> Vec<(String, Vec<String>)> {
    let Some(all) = read_st_tag_map() else {
        return Vec::new();
    };
    all.into_iter().collect()
}

/// Shared parsing of `tag_map` and `tags` from SillyTavern's settings.json.
fn read_st_tag_map() -> Option<std::collections::HashMap<String, Vec<String>>> {
    let path = sillytavern::st_dir()
        .join("data")
        .join("default-user")
        .join("settings.json");
    let text = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let names: std::collections::HashMap<String, String> = json
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|tag| {
                    let id = tag.get("id").and_then(|v| v.as_str())?;
                    let name = tag
                        .get("name")
                        .and_then(|v| v.as_str())
                        .or_else(|| tag.get("id").and_then(|v| v.as_str()))?;
                    Some((id.to_string(), name.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();

    let tag_map = json.get("tag_map")?.as_object()?;
    let mut result = std::collections::HashMap::new();
    for (avatar, ids) in tag_map {
        let Some(arr) = ids.as_array() else { continue };
        let mut list: Vec<String> = Vec::new();
        for id in arr.iter().filter_map(|v| v.as_str()) {
            let name = names.get(id).cloned().unwrap_or_else(|| id.to_string());
            push_unique_tag(&mut list, name);
        }
        if !list.is_empty() {
            result.insert(avatar.clone(), list);
        }
    }
    Some(result)
}

/// Add a tag name without case duplicates. `eq_ignore_ascii_case`
/// did not fold Cyrillic: "Эльфийка" and "эльфийка" became two traits.
fn push_unique_tag(list: &mut Vec<String>, name: String) {
    if name.trim().is_empty() {
        return;
    }
    let folded = name.to_lowercase();
    if !list.iter().any(|tag| tag.to_lowercase() == folded) {
        list.push(name);
    }
}

/// Characters whose traits can be configured: cards in the SillyTavern catalog.
pub fn character_list() -> Vec<(String, String)> {
    let dir = sillytavern::st_dir()
        .join("data")
        .join("default-user")
        .join("characters");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut list: Vec<(String, String)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str())?.to_lowercase() != "png" {
                return None;
            }
            // A truncated or empty file is not a card: ST would not open it anyway.
            if entry.metadata().map(|m| m.len()).unwrap_or(0) < 128 {
                return None;
            }
            let avatar = match path.file_name().and_then(|n| n.to_str()) {
                Some(name) => name.to_string(),
                None => {
                    crate::boot_log(&format!(
                        "character_list: пропущено не-UTF-8 имя файла: {}",
                        path.display()
                    ));
                    return None;
                }
            };
            let name = display_name(&avatar);
            Some((avatar, name))
        })
        .collect();
    list.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
    list
}

/// Apply an exchange evaluation to the character state. Returns the new
/// state. An error — if the write failed; the absence of traits for a character is not an
/// error: it means the state is not managed for it.
pub fn apply_deltas(avatar: &str, persona: &str, deltas: &Deltas) -> Result<LiveState, String> {
    update(|store| {
        // The record is created by the very first shift: state lives for all cards,
        // not only for manually configured ones.
        let entry = store.entry(avatar.to_string()).or_default();
        if !entry.live {
            return entry.state;
        }
        entry.apply(persona, deltas)
    })
}

impl CharacterTraits {
    /// Attitude toward the interlocutor: each person has their own. A new interlocutor
    /// starts from an even baseline — someone else's warmth does not transfer to them. An empty
    /// name (persona not reported) means "current state".
    pub fn relation(&self, persona: &str) -> LiveState {
        if persona.is_empty() {
            return self.state;
        }
        self.relations.get(persona).copied().unwrap_or_else(LiveState::neutral)
    }

    pub fn tone_for(&self, persona: &str) -> Tone {
        if persona.is_empty() {
            self.last_tone
        } else {
            self.tones.get(persona).copied().unwrap_or_default()
        }
    }

    /// Shift the state in conversation with this person and remember the tone.
    pub fn apply(&mut self, persona: &str, deltas: &Deltas) -> LiveState {
        let previous = self.relation(persona);
        let tone_before = self.tone_for(persona);
        let next = evolve(&previous, deltas, tone_before);
        self.set_state(persona, next);
        // The tone is always saved: without it reversal (the dramatic collapse of affection
        // on a tone change) does not fire for anonymous users.
        // For an empty persona the tone lives in the shared field, for a named one — in tones.
        if persona.is_empty() {
            self.last_tone = deltas.tone;
        } else {
            self.tones.insert(persona.to_string(), deltas.tone);
        }
        next
    }

    /// Write state manually: the settings panel shows and edits exactly what
    /// goes into the prompt to the current interlocutor.
    pub fn set_state(&mut self, persona: &str, state: LiveState) {
        // The shared state is memory of the unnamed interlocutor: a named one does not touch it,
        // otherwise a persona without a name would receive the feelings of the last named one
        if persona.is_empty() {
            self.state = state;
        } else {
            self.relations.insert(persona.to_string(), state);
        }
    }

    /// Return the state to an even baseline — both for the current interlocutor and in the memory
    /// of relations: the reset must not "resurface" on the next switch.
    /// The tone is forgotten too, otherwise the first warm reply after a quarrel would count as
    /// a reversal.
    pub fn reset_state(&mut self, persona: &str) {
        self.set_state(persona, LiveState::neutral());
        if persona.is_empty() {
            self.last_tone = Tone::Neutral;
        } else {
            self.tones.insert(persona.to_string(), Tone::Neutral);
        }
    }
}

/// Default fragment for a character that has tags in SillyTavern
/// but character traits not configured yet. All tags get 50% — moderate
/// expression so the model does not ignore the card descriptions.
const DEFAULT_TAG_INTENSITY: u8 = 50;

/// Note for the card: own traits, and if intensity is not configured —
/// proportions by tags from SillyTavern. The store record also appears by itself, from
/// the very first state shift, so "has a record" no longer means "traits configured
/// by hand": without this substitution the card would lose the tag proportions
/// after the first conversation.
///
/// Brand and generation folders are excluded from the automatic list: for Fuwawa these are
/// "Hololive", "Hololive EN -Advent-", "vtuber", "russian", "uncensored" — five
/// out of twelve lines, and all of them stood in the same list with "mischievous" and
/// "affectionate" as traits at 50%.
pub fn card_fragment(entry: &CharacterTraits, st_tags: &[String]) -> String {
    if !entry.intensity.is_empty() {
        return fragment(entry);
    }
    let filled = CharacterTraits {
        intensity: st_tags
            .iter()
            .map(|tag| (clean_tag(tag), DEFAULT_TAG_INTENSITY))
            .filter(|(tag, _)| !tag.is_empty() && crate::tag_influence::is_character_trait(tag))
            .collect(),
        ..entry.clone()
    };
    fragment(&filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn traits(pairs: &[(&str, u8)], no_repeat: bool) -> CharacterTraits {
        CharacterTraits {
            no_repeat,
            chaos: 0,
            live: false,
            state: LiveState::neutral(),
            relations: BTreeMap::new(),
            tones: BTreeMap::new(),
            last_tone: Tone::Neutral,
            intensity: pairs.iter().map(|(t, v)| (t.to_string(), *v)).collect(),
        }
    }

    fn warm_tone() -> Deltas {
        Deltas { affection: 15, trust: 8, energy: 5, irritation: -5, tone: Tone::Tenderness }
    }

    fn hostile_tone() -> Deltas {
        Deltas { affection: -15, trust: -15, energy: -5, irritation: 15, tone: Tone::Hostile }
    }

    #[test]
    fn test_fragment_lists_intensity_in_plain_words() {
        let text = fragment(&traits(&[("Энергичность", 30), ("Застенчивость", 80)], true));
        assert!(text.contains("Энергичность: 30% (сдержанно)"), "{text}");
        assert!(text.contains("Застенчивость: 80% (заметно)"), "{text}");
        assert!(text.contains("Не повторяй жесты"), "{text}");
    }

    /// Zero — "do not express": the trait stays in the list but with an explicit wording,
    /// otherwise the model will keep treating it as leading per the card description.
    #[test]
    fn test_zero_intensity_is_explicit() {
        let text = fragment(&traits(&[("Агрессия", 0)], false));
        assert!(text.contains("Агрессия: 0% (не проявлять)"), "{text}");
        assert!(!text.contains("Не повторяй"), "{text}");
    }

    #[test]
    fn test_empty_traits_give_only_the_repeat_ban() {
        // No traits, but the repetition ban is there: it is enabled by default.
        assert_eq!(
            fragment(&CharacterTraits::default()),
            "Не повторяй жесты, позы и действия из прошлых реплик: каждый ответ приносит новое."
        );
        // Everything disabled by hand — the note is empty and is removed from the card.
        assert_eq!(fragment(&traits(&[], false)), "");
        // A character with only zeros still gets text: it is an instruction to the model.
        assert!(fragment(&traits(&[("Тишина", 0)], false)).contains("Тишина: 0%"));
    }

    #[test]
    fn test_wording_covers_range() {
        assert_eq!(wording(0), "не проявлять");
        assert_eq!(wording(5), "едва заметно");
        assert_eq!(wording(100), "ведущая черта");
    }

    /// A sharp event bypasses smoothing: "go to hell" after affection must not
    /// look like minus ten points.
    #[test]
    fn test_sharp_turn_breaks_the_state() {
        let loving = LiveState { affection: 100, trust: 90, energy: 70, irritation: 5 };
        // Sharp rudeness (evaluation −15): affection to zero, irritation to the ceiling,
        // trust drops at once — and this happens in a single turn.
        let after = evolve(&loving, &hostile_tone(), Tone::Tenderness);
        assert_eq!(after.affection, 0, "{after:?}");
        assert!(after.irritation >= 70, "{after:?}");
        assert!(after.trust <= 65, "{after:?}");

        // Reverse reversal: after rudeness warm words bring affection back,
        // but trust grows cautiously.
        let cold = LiveState { affection: 5, trust: 20, energy: 40, irritation: 85 };
        let thaw = evolve(&cold, &warm_tone(), Tone::Hostile);
        assert!(thaw.affection >= 60, "{thaw:?}");
        assert!(thaw.irritation <= 25, "{thaw:?}");
        assert!(thaw.trust < 40, "доверие восстанавливается медленнее: {thaw:?}");
    }

    /// Reserved coldness after flirting is not a break: it becomes a reversal
    /// only if the evaluator marked the turn sharp. Rudeness always breaks.
    #[test]
    fn test_mild_cold_after_warmth_is_gradual() {
        let warm = LiveState { affection: 80, trust: 70, energy: 60, irritation: 5 };
        let mild_cold = Deltas { affection: -2, trust: 0, energy: -1, irritation: 1, tone: Tone::Cold };
        let after = evolve(&warm, &mild_cold, Tone::Flirt);
        assert!(after.affection >= 70, "{after:?}");
        assert!(after.irritation < 35, "{after:?}");

        let sharp_cold = Deltas { affection: -13, trust: -12, energy: -3, irritation: 12, tone: Tone::Cold };
        let broken = evolve(&warm, &sharp_cold, Tone::Flirt);
        assert_eq!(broken.affection, 0, "{broken:?}");
        assert!(broken.irritation >= 70, "{broken:?}");

        let rude = Deltas { affection: -3, trust: -2, energy: 0, irritation: 3, tone: Tone::Hostile };
        assert!(evolve(&warm, &rude, Tone::Flirt).affection <= 8);

        // The other side: a slight warming after cold does not whip affection up to 60.
        let chilly = LiveState { affection: 20, trust: 40, energy: 50, irritation: 30 };
        let small_flirt = Deltas { affection: 2, trust: 1, energy: 1, irritation: -1, tone: Tone::Flirt };
        assert!(evolve(&chilly, &small_flirt, Tone::Cold).affection < 30);
    }

    #[test]
    fn test_unique_tags_fold_cyrillic_case() {
        let mut list = Vec::new();
        push_unique_tag(&mut list, "Эльфийка".into());
        push_unique_tag(&mut list, "эльфийка".into());
        push_unique_tag(&mut list, "ELF".into());
        push_unique_tag(&mut list, "elf".into());
        push_unique_tag(&mut list, "  ".into());
        assert_eq!(list, vec!["Эльфийка".to_string(), "ELF".to_string()]);
    }

    /// Without a tone reversal and without sharp evaluations the state changes gradually.
    #[test]
    fn test_smooth_turn_stays_gradual() {
        let start = LiveState::neutral();
        let gentle = Deltas { affection: 8, trust: 8, energy: 8, irritation: -8, tone: Tone::Flirt };
        let after = evolve(&start, &gentle, Tone::Flirt);
        // 60% of 8 is 4.8, rounded to 5, then cooling by 2 toward the baseline.
        assert_eq!(after.affection, 53);
        assert_eq!(after.trust, 53);
        assert_eq!(after.energy, 58);
        assert_eq!(after.irritation, 8);
        assert!(!gentle.is_sharp());
    }

    /// Attitude toward interlocutors is kept separately: warmth to one character,
    /// coldness to another, and the states do not mix.
    #[test]
    fn test_relations_are_tracked_per_persona() {
        let mut entry = traits(&[("энергичность", 70)], false);
        entry.live = true;
        entry.apply("Алекс", &warm_tone());
        let for_andrey = entry.relation("Алекс");
        assert!(for_andrey.affection > 50, "{for_andrey:?}");
        assert_eq!(entry.tone_for("Алекс"), Tone::Tenderness);

        // The second interlocutor starts even: someone else's warmth does not affect them.
        assert_eq!(entry.relation("Гость"), LiveState::neutral());
        entry.apply("Гость", &hostile_tone());
        assert!(entry.relation("Гость").irritation > 10);
        assert_eq!(entry.relation("Алекс"), for_andrey, "отношение к Алексу не изменилось");

        entry.reset_state("Алекс");
        assert_eq!(entry.relation("Алекс"), LiveState::neutral());
        assert_eq!(entry.tone_for("Алекс"), Tone::Neutral);
    }

    /// The persona did not arrive (the user has no name): the state is shared by all, no empty key
    /// is created in relations — otherwise it would be indistinguishable from a persona with an
    /// empty name and would survive a switch of interlocutor.
    #[test]
    fn test_empty_persona_keeps_a_single_state() {
        let mut entry = traits(&[], false);
        entry.live = true;
        entry.apply("", &warm_tone());
        assert!(entry.state.affection > 50, "{:?}", entry.state);
        assert!(entry.relations.is_empty(), "пустой ключ не пишем: {:?}", entry.relations);
        assert!(entry.tones.is_empty());

        // The persona changed — it has its own, even attitude; the previous state
        // remained as memory of the unnamed interlocutor and did not move to the new one.
        assert_eq!(entry.relation("Гость"), LiveState::neutral());
        entry.reset_state("");
        assert_eq!(entry.state, LiveState::neutral());
        assert!(entry.relations.is_empty());
    }

    /// Manual state edit from the panel: both the current state and the
    /// attitude toward the interlocutor are written — the panel and the prompt show the same thing.
    #[test]
    fn test_manual_state_edit_is_stored_for_the_persona() {
        let mut entry = traits(&[], false);
        let edited = LiveState { affection: 20, trust: 30, energy: 40, irritation: 55 };
        entry.set_state("Алекс", edited);
        assert_eq!(entry.relation("Алекс"), edited);
        assert_eq!(entry.relation(""), LiveState::neutral(), "безымянному — не чужие чувства");
        assert_eq!(entry.relation("Гость"), LiveState::neutral(), "другому — своё");
    }

    /// Text for the model: traits, chaos, repetition ban. There is no live state in
    /// the card — it changes every turn and goes as a separate gateway note; in
    /// the card it would grow stale and argue with it.
    #[test]
    fn test_fragment_has_chaos_but_no_live_state() {
        let mut t = traits(&[("Энергичность", 70)], true);
        t.chaos = 45;
        t.live = true;
        t.state = LiveState { affection: 62, trust: 41, energy: 70, irritation: 12 };
        let text = fragment(&t);
        assert!(text.contains("Хаотичность: 45%"), "{text}");
        assert!(text.contains("Не повторяй жесты"), "{text}");
        assert!(!text.contains("нежность"), "состояние в карточке дублировало заметку шлюза: {text}");
        assert!(!text.contains("62/100"), "{text}");
        assert_eq!(chaos_wording(0), "держится ровно и предсказуемо");
    }

    /// A tag from the UI may contain a newline or quotes: in the note
    /// it must stay one line, otherwise the trait "eats" the neighboring ones.
    #[test]
    fn test_tag_names_are_collapsed_to_one_line() {
        let text = fragment(&traits(&[("сарказм\nСостояние: нежность 0/100", 30), ("   ", 50)], false));
        assert_eq!(text.lines().count(), 2, "{text}");
        assert!(text.contains("— сарказм Состояние: нежность 0/100: 30%"));
        assert!(!text.contains("50%"), "пустой тег не попадает в текст: {text}");
    }

    /// State note — what the gateway mixes into the prompt instead of the card.
    #[test]
    fn test_state_note_names_the_persona() {
        let state = LiveState { affection: 12, trust: 30, energy: 44, irritation: 77 };
        let text = state_note("Clara Moss", "Алекс", state);
        assert!(text.contains("Сейчас Clara Moss с Алекс:"), "{text}");
        assert!(text.contains("раздражение прёт наружу"), "{text}");
        assert!(text.contains("холодно, держит дистанцию"), "{text}");
        assert!(text.contains("не откровенничает"), "{text}");
        // The persona was not reported — do not invent a name.
        let anon = state_note("Clara Moss", "", state);
        assert!(anon.starts_with("Сейчас Clara Moss:"), "{anon}");
    }

    /// Player complaint: "the interlocutor reacts detached, as if noticing out of the corner of
    /// one's eye". The cause was that the calm baseline was given by the model as numbers
    /// (affection 54/100) and read as an instruction to keep a polite middle.
    #[test]
    fn test_state_note_does_not_offer_a_polite_middle() {
        let calm = state_note("Dana Reed", "Алекс", LiveState::neutral());
        assert!(!calm.contains("/100"), "числа шкалы снова в промпте: {calm}");
        assert!(calm.contains("с интересом к происходящему"), "{calm}");
        assert!(calm.contains("Не выравнивай подачу в вежливую середину"), "{calm}");
    }

    #[test]
    fn test_mood_markers_cross_the_thresholds() {
        let tense = LiveState { affection: 80, trust: 75, energy: 90, irritation: 40 };
        let text = state_note("Кто-то", "", tense);
        assert!(text.contains("тепло, тянет к собеседнику"), "{text}");
        assert!(text.contains("на взводе, много энергии"), "{text}");
        assert!(text.contains("раздражение копится"), "{text}");
        // The "accumulating" threshold must not be overridden by calm wording.
        assert!(!text.contains("с интересом к происходящему"), "{text}");
    }

    /// Without events the state returns to an even one by itself.
    #[test]
    fn test_evolve_decays_to_neutral() {
        let tense = LiveState { affection: 5, trust: 10, energy: 95, irritation: 90 };
        let after = evolve(&tense, &Deltas::default(), Tone::Neutral);
        assert!(after.affection > tense.affection, "нежность подтягивается: {after:?}");
        assert!(after.trust > tense.trust);
        assert!(after.energy < tense.energy);
        assert!(after.irritation < tense.irritation, "раздражение остывает: {after:?}");

        // Many calm turns — the state is at the baseline, not beyond it.
        let mut state = tense;
        for _ in 0..120 {
            state = evolve(&state, &Deltas::default(), Tone::Neutral);
        }
        assert_eq!(state, LiveState::neutral());
    }

    /// Bounds: state stays within 0..100 under any evaluations and tone reversals.
    #[test]
    fn test_evolve_keeps_bounds() {
        let mut state = LiveState { affection: 98, trust: 2, energy: 99, irritation: 1 };
        for i in 0..30 {
            let tone = if i % 2 == 0 { Tone::Hostile } else { Tone::Tenderness };
            let previous = if i % 2 == 0 { Tone::Tenderness } else { Tone::Hostile };
            state = evolve(
                &state,
                &Deltas { affection: -15, trust: -15, energy: 15, irritation: 15, tone },
                previous,
            );
            assert!(state.affection <= 100 && state.trust <= 100 && state.energy <= 100);
        }
    }

    #[test]
    fn test_card_fragment_falls_back_to_st_tags() {
        let tags = vec!["Эльфийка".into(), "сарказм".into()];
        let text = card_fragment(&CharacterTraits::default(), &tags);
        assert!(text.contains("Эльфийка: 50% (умеренно)"), "{text}");
        assert!(text.contains("сарказм: 50% (умеренно)"), "{text}");
        assert!(text.contains("Не повторяй жесты"), "{text}");

        // Manually configured traits are not overridden by ST tags.
        let own = card_fragment(&traits(&[("Эльфийка", 90)], true), &tags);
        assert!(own.contains("Эльфийка: 90% (заметно)"), "{own}");
        assert!(!own.contains("сарказм"), "{own}");
    }

    /// Brand and generation folder are not character. For cards from the library
    /// "Hololive", "Hololive EN -Advent-", "vtuber", "russian", "uncensored"
    /// accounted for five of twelve prompt lines, and all of them stood in the same
    /// list with real traits.
    #[test]
    fn test_card_fragment_drops_brand_and_generation_tags() {
        let tags = vec![
            "Hololive".into(),
            "Hololive EN -Advent-".into(),
            "vtuber".into(),
            "russian".into(),
            "uncensored".into(),
            "озорная".into(),
            "богиня хаоса".into(),
        ];
        let text = card_fragment(&CharacterTraits::default(), &tags);
        for junk in ["Hololive", "vtuber", "russian", "uncensored", "Advent"] {
            assert!(!text.contains(junk), "служебный тег стал чертой: {text}");
        }
        assert!(text.contains("озорная: 50%"), "{text}");
        assert!(text.contains("богиня хаоса: 50%"), "{text}");
    }

    /// Even without a single tag the card gets a repetition ban: it comes from the
    /// defaults, not from configured traits.
    #[test]
    fn test_card_fragment_without_tags_still_forbids_repeats() {
        let text = card_fragment(&CharacterTraits::default(), &[]);
        assert!(!text.contains("Черты характера"), "{text}");
        assert!(text.contains("Не повторяй жесты"), "{text}");
        assert_eq!(card_fragment(&CharacterTraits::default(), &["  ".into()]), text);
    }

    /// Bounds of the `Deltas` contract that `parse_deltas` used to enforce (it was
    /// superseded by `rpg::prompts::parse_combined_evaluator`): zero shift, a "not
    /// sharp" small shift and the plausibility threshold ±15 — applied by
    /// `Deltas::is_plausible` when parsing the evaluator's reply.
    #[test]
    fn deltas_contract_boundaries() {
        assert!(Deltas::default().is_zero());
        assert!(!Deltas { affection: 3, ..Deltas::default() }.is_sharp());
        assert!(Deltas { affection: 15, trust: -15, ..Deltas::default() }.is_plausible());
        assert!(!Deltas { affection: 16, ..Deltas::default() }.is_plausible());
    }

    #[test]
    fn test_default_traits_are_live_and_non_repeating() {
        let entry = CharacterTraits::default();
        assert!(entry.live, "живое состояние включено по умолчанию");
        assert!(entry.no_repeat, "запрет повторов включён по умолчанию");
        assert_eq!(entry.state, LiveState::neutral());
    }

    /// A manually disabled state survives file reading: defaults are
    /// substituted only for missing fields.
    #[test]
    fn test_saved_false_survives_deserialization() {
        let entry: CharacterTraits =
            serde_json::from_str(r#"{"live": false, "no_repeat": false}"#).unwrap();
        assert!(!entry.live);
        assert!(!entry.no_repeat);

        let fresh: CharacterTraits = serde_json::from_str("{}").unwrap();
        assert!(fresh.live);
        assert!(fresh.no_repeat);
    }

    /// Byte slicing `avatar[cut..]` panicked on multibyte names without .png.
    #[test]
    fn test_display_name_survives_multibyte_names() {
        assert_eq!(display_name("Clara_Moss.png"), "Clara Moss");
        assert_eq!(display_name("Clara_Moss.PNG"), "Clara Moss");
        assert_eq!(display_name("имя.png"), "имя");
        // 9 bytes, cut = 5 lands inside a hieroglyph — used to panic.
        assert_eq!(display_name("日本語"), "日本語");
        assert_eq!(display_name("日本語.png"), "日本語");
        assert_eq!(display_name("имя"), "имя");
        assert_eq!(display_name(""), "");
        assert_eq!(display_name("png"), "png");
    }

    /// V-09/V-31 delegation: a corrupt file read through the shared body is
    /// preserved as `.json.corrupt` before the fallback to an empty store.
    #[test]
    fn load_from_corrupt_backs_up_and_defaults() {
        let dir = std::env::temp_dir().join("cloufam_test_traits_corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("traits.json");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("json.corrupt"));
        std::fs::write(&path, "oops {{{").unwrap();
        let store = load_from(&path);
        assert!(store.is_empty(), "битый файл должен отдавать пустое хранилище");
        let backup = path.with_extension("json.corrupt");
        assert!(backup.exists(), "копия повреждённого файла не создана");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "oops {{{");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&backup);
        let _ = std::fs::remove_dir(&dir);
    }
}

