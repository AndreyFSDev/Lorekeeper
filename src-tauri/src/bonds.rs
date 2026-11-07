//! Character bonds between each other: what one thinks of another and what he is ready
//! to tell behind his back.
//!
//! Stored in `data/bonds.json`: `{ "<avatar.png>": [{ "target": "A.png",
//! "kind": "sister", "strength": 70, "note": "jealous", "secret": true }] }`.
//! The key is the card avatar, as in `traits.json`; the target is stored as an avatar too, but
//! always expanded into text through `traits::display_name`: by the raw file
//! name the entry was not found, and in prose the model called her "Gawr underscore Gura
//! dot png" — a lesson from `rpg/prompts.rs`.
//!
//! Delivery is card text only (`depth_prompt`), the same way as character traits:
//! bonds are edited by a human, and they do not change from a line of dialogue. A gateway note
//! would give a second copy of the same text, while an RPG snapshot with its own cache would make
//! an edit invisible for a minute.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::traits;

pub type Store = BTreeMap<String, Vec<Bond>>;

/// One bond: who is to whom.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Bond {
    /// Target card avatar (`Gawr_Gura.png`).
    pub target: String,
    /// Bond name in words ("younger sister", "debtor"), not an axis or a number.
    pub kind: String,
    /// How much the bond defines behavior, 0..100.
    pub strength: u8,
    /// What to gossip about: a fact, a grudge, shared history.
    pub note: String,
    /// "He does not know" — the target is unaware of this attitude.
    pub secret: bool,
}

/// The middle of the scale — the same as a character trait without a configured
/// intensity (`traits::DEFAULT_TAG_INTENSITY`).
const DEFAULT_STRENGTH: u8 = 50;

impl Default for Bond {
    fn default() -> Self {
        Self {
            target: String::new(),
            kind: String::new(),
            strength: DEFAULT_STRENGTH,
            note: String::new(),
            secret: false,
        }
    }
}

static STORE: Mutex<()> = Mutex::new(());

/// More than this — the card note turns into a reference book that the model
/// retells instead of playing.
pub const MAX_BONDS: usize = 12;
pub const MAX_KIND_LEN: usize = 40;
pub const MAX_NOTE_LEN: usize = 200;

pub fn path() -> PathBuf {
    crate::paths::data_dir().join("bonds.json")
}

pub fn load() -> Store {
    load_from(&path())
}

/// Store from the given file — like `traits::load_from`: tests and the gateway read
/// their own path without relying on the installed program directory.
pub fn load_from(file: &Path) -> Store {
    crate::json_store::load(&STORE, file)
}

pub fn bonds_of(avatar: &str) -> Vec<Bond> {
    load().remove(avatar).unwrap_or_default()
}

/// Write a character's bonds. `known` — avatars present in the card
/// catalog (`traits::character_list()`): pure `validate` does not touch the file
/// system, so the list is passed from outside.
pub fn set(avatar: &str, input: Vec<Bond>, known: &BTreeSet<String>) -> Result<Vec<Bond>, String> {
    set_to(&path(), avatar, input, known)
}

fn set_to(
    file: &Path,
    avatar: &str,
    input: Vec<Bond>,
    known: &BTreeSet<String>,
) -> Result<Vec<Bond>, String> {
    let owner = avatar.to_string();
    // `update_if_ok`, not `update`: `update` writes the file even when the closure
    // returned Err, and a refusal on the very first edit would create an empty store.
    crate::json_store::update_if_ok(&STORE, file, |store: &mut Store| {
        let cleaned = validate(&owner, &input, known)?;
        if cleaned.is_empty() {
            store.remove(&owner);
        } else {
            store.insert(owner.clone(), cleaned.clone());
        }
        Ok(cleaned)
    })?
}

/// Validate and normalize: no file system and no lock.
pub fn validate(owner: &str, input: &[Bond], known: &BTreeSet<String>) -> Result<Vec<Bond>, String> {
    let owner = owner.trim();
    let mut out: Vec<Bond> = Vec::new();
    for bond in input {
        let target = bond.target.trim();
        if target.is_empty() {
            return Err("В связи не выбран персонаж.".to_string());
        }
        if target == owner {
            return Err("Персонаж не может быть связан сам с собой.".to_string());
        }
        if !known.contains(target) {
            return Err(format!(
                "Карточки «{}» нет в каталоге SillyTavern.",
                traits::display_name(target)
            ));
        }
        let kind = one_line(&bond.kind);
        if kind.is_empty() {
            return Err("Нужно назвать связь словами: сестра, должник, соперница.".to_string());
        }
        // A duplicate target is silently dropped: a second line about the same character
        // adds nothing, and rejecting the whole save because of it is odd.
        if out.iter().any(|saved| saved.target == target) {
            continue;
        }
        out.push(Bond {
            target: target.to_string(),
            kind: clip(&kind, MAX_KIND_LEN),
            strength: bond.strength.min(100),
            note: clip(&one_line(&bond.note), MAX_NOTE_LEN),
            secret: bond.secret,
        });
    }
    // A1-04: the limit is counted AFTER deduplication — a duplicate target is silently
    // dropped above and must not take up the bond budget.
    if out.len() > MAX_BONDS {
        return Err(format!("Связей не больше {MAX_BONDS}: сейчас {}.", out.len()));
    }
    sort_by_name(&mut out);
    Ok(out)
}

/// Entry order is a contract, not style: `applyTraits` in SillyTavern
/// rewrites the PNG only if the text diverged byte for byte. Unstable
/// order would rewrite the card on every save and reset the token
/// recount at its depth.
fn sort_by_name(bonds: &mut [Bond]) {
    bonds.sort_by_cached_key(|bond| {
        (
            traits::display_name(&bond.target).to_lowercase(),
            bond.target.clone(),
        )
    });
}

/// Append a sentence, putting a period after the previous one.
fn add_sentence(line: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    if !line.is_empty() {
        line.push_str(if ends_sentence(line) { " " } else { ". " });
    }
    line.push_str(text);
}

fn ends_sentence(line: &str) -> bool {
    line.ends_with(['.', '!', '?', '…'])
}

/// Block for the card. An empty list — an empty string: then `join` returns the trait
/// text unchanged, and removing all bonds removes the block from the card.
pub fn block(bonds: &[Bond]) -> String {
    if bonds.is_empty() {
        return String::new();
    }
    let mut lines = vec!["Отношения с другими персонажами — держаться этих оценок:".to_string()];
    let mut has_secret = false;
    for bond in bonds {
        let name = traits::display_name(&bond.target);
        let mut line = format!(
            "— {name}: {}, {}% ({})",
            bond.kind,
            bond.strength,
            strength_word(bond.strength)
        );
        add_sentence(&mut line, &bond.note);
        if bond.secret {
            has_secret = true;
            add_sentence(&mut line, &format!("{name} не догадывается"));
        }
        if !ends_sentence(&line) {
            line.push('.');
        }
        lines.push(line);
    }
    lines.push(
        "О других говорить как о живых: помнить общее, колоть, хвалить вполоборота, \
         припоминать. Не пересказывать чужие мысли и не выдумывать того, чего нет в \
         этих строках."
            .to_string(),
    );
    if has_secret {
        lines.push(
            "Строка «не догадывается» — держать втайне: намёки, оговорки, \
             двусмысленность; прямо не признаваться, пока не прижмут."
                .to_string(),
        );
    }
    lines.join("\n")
}

/// Build the card note: traits, bonds below.
pub fn join(traits_part: &str, bonds_part: &str) -> String {
    match (traits_part.is_empty(), bonds_part.is_empty()) {
        (true, true) => String::new(),
        (true, false) => bonds_part.to_string(),
        (false, true) => traits_part.to_string(),
        (false, false) => format!("{traits_part}\n\n{bonds_part}"),
    }
}

/// In words, not a number — like `traits::wording`: the model follows wording
/// better than a percentage. Gender is never named: cards and players refer to
/// the character differently.
pub fn strength_word(percent: u8) -> &'static str {
    match percent {
        0 => "пустяковое",
        1..=25 => "слабое",
        26..=50 => "сдержанное",
        51..=75 => "заметное",
        76..=90 => "сильное",
        _ => "всепоглощающее",
    }
}

/// On one line: a line break inside a bond would break the note structure where
/// each bond is its own line.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncate by characters, not bytes: a byte slice would land in the middle of
/// a multibyte character. The tail is dropped with a period at the end, and the length together
/// with the period does not exceed the limit.
fn clip(text: &str, limit: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= limit {
        return text.to_string();
    }
    let mut cut: String = chars[..limit.saturating_sub(1)].iter().collect();
    while cut.ends_with(char::is_whitespace) {
        cut.pop();
    }
    if !ends_sentence(&cut) {
        cut.push('.');
    }
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cloufam_test_bonds_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn known(avatars: &[&str]) -> BTreeSet<String> {
        avatars.iter().map(|avatar| avatar.to_string()).collect()
    }

    fn bond(target: &str, kind: &str, strength: u8, note: &str, secret: bool) -> Bond {
        Bond {
            target: target.to_string(),
            kind: kind.to_string(),
            strength,
            note: note.to_string(),
            secret,
        }
    }

    /// An old file without the new fields must load: `#[serde(default)]` covers
    /// missing keys, not just the whole file.
    #[test]
    fn test_old_file_without_fields_loads() {
        let file = dir("old_fields").join("bonds.json");
        std::fs::write(
            &file,
            r#"{ "Akihoshi_Roshito.png": [ { "target": "Gawr_Gura.png", "kind": "сестра" } ] }"#,
        )
        .unwrap();
        let mut store = load_from(&file);
        let bonds = store.remove("Akihoshi_Roshito.png").unwrap_or_default();
        assert_eq!(bonds.len(), 1);
        assert_eq!(bonds[0].kind, "сестра");
        assert_eq!(bonds[0].strength, DEFAULT_STRENGTH);
        assert!(bonds[0].note.is_empty());
        assert!(!bonds[0].secret);
    }

    #[test]
    fn test_rejects_self() {
        let err = validate(
            "A.png",
            &[bond("A.png", "подруга", 50, "", false)],
            &known(&["A.png"]),
        )
        .unwrap_err();
        assert!(err.contains("сам с собой"), "{err}");
    }

    #[test]
    fn test_rejects_unknown_target() {
        let err = validate(
            "A.png",
            &[bond("Ghost.png", "должник", 50, "", false)],
            &known(&["A.png"]),
        )
        .unwrap_err();
        // The error text names the person: the user picks a character by
        // display name, not by file name.
        assert!(err.contains("Ghost"), "{err}");
    }

    #[test]
    fn test_rejects_empty_kind() {
        let err = validate(
            "A.png",
            &[bond("B.png", "   ", 50, "", false)],
            &known(&["B.png"]),
        )
        .unwrap_err();
        assert!(err.contains("словами"), "{err}");
    }

    #[test]
    fn test_rejects_more_than_limit() {
        let all: Vec<&str> = (0..=MAX_BONDS).map(|i| TARGETS[i]).collect();
        let input: Vec<Bond> = (0..=MAX_BONDS)
            .map(|i| bond(TARGETS[i], "сестра", 50, "", false))
            .collect();
        let err = validate("A.png", &input, &known(&all)).unwrap_err();
        assert!(err.contains(&MAX_BONDS.to_string()), "{err}");
    }

    /// A1-04: the limit counts unique targets: 13 lines with a duplicate give 12 bonds
    /// and must be saved.
    #[test]
    fn test_limit_counts_after_dedup() {
        let mut input: Vec<Bond> = (0..MAX_BONDS)
            .map(|i| bond(TARGETS[i], "сестра", 50, "", false))
            .collect();
        input.push(bond(TARGETS[0], "сестра", 50, "", false));
        assert_eq!(input.len(), MAX_BONDS + 1, "строк больше лимита");
        let all: Vec<&str> = TARGETS.to_vec();
        let cleaned =
            validate("A.png", &input, &known(&all)).expect("дубль не должен занимать лимит");
        assert_eq!(cleaned.len(), MAX_BONDS);
    }

    const TARGETS: [&str; 13] = [
        "T0.png", "T1.png", "T2.png", "T3.png", "T4.png", "T5.png", "T6.png", "T7.png", "T8.png",
        "T9.png", "T10.png", "T11.png", "T12.png",
    ];

    #[test]
    fn test_trims_kind_clamps_strength_and_drops_duplicates() {
        let cleaned = validate(
            "A.png",
            &[
                bond("B.png", &"с".repeat(60), 140, "  много   пробелов  ", false),
                bond("B.png", "дубль", 10, "", false),
            ],
            &known(&["B.png"]),
        )
        .unwrap();
        assert_eq!(cleaned.len(), 1, "дубликат цели не убран");
        assert_eq!(cleaned[0].kind.chars().count(), MAX_KIND_LEN);
        assert_eq!(cleaned[0].strength, 100);
        assert_eq!(cleaned[0].note, "много пробелов");
    }

    /// Truncation appends a period: otherwise the card ended up with a clipped word
    /// without a sentence end.
    #[test]
    fn test_truncation_ends_with_a_period_inside_the_limit() {
        let cleaned = validate(
            "A.png",
            &[bond(
                "B.png",
                "сестра",
                50,
                &"р".repeat(MAX_NOTE_LEN + 20),
                false,
            )],
            &known(&["B.png"]),
        )
        .unwrap();
        assert_eq!(cleaned[0].note.chars().count(), MAX_NOTE_LEN);
        assert!(cleaned[0].note.ends_with('.'));
    }

    #[test]
    fn test_order_is_deterministic() {
        let known = known(&["C.png", "B.png", "A.png"]);
        let first = validate(
            "X.png",
            &[
                bond("C.png", "соперница", 60, "", false),
                bond("B.png", "сестра", 70, "", false),
                bond("A.png", "должник", 50, "", false),
            ],
            &known,
        )
        .unwrap();
        let second = validate(
            "X.png",
            &[
                bond("A.png", "должник", 50, "", false),
                bond("C.png", "соперница", 60, "", false),
                bond("B.png", "сестра", 70, "", false),
            ],
            &known,
        )
        .unwrap();
        assert_eq!(first, second, "порядок записей поплыл");
        // Byte for byte: whether SillyTavern rewrites the card depends on it.
        assert_eq!(block(&first), block(&second));
        let text = block(&first);
        // Sort by display name: A, B, C.
        assert!(
            text.find("должник").unwrap() < text.find("сестра").unwrap()
                && text.find("сестра").unwrap() < text.find("соперница").unwrap(),
            "{text}"
        );
    }

    /// The text contains the card display name, not the file name: `display_name`
    /// replaces underscores with spaces and does not translate — the name stays the way
    /// the player sees it.
    #[test]
    fn test_block_prints_names_not_avatars() {
        let text = block(&[bond(
            "Gawr_Gura.png",
            "предатель",
            85,
            "сдала на прослушивании",
            false,
        )]);
        assert!(text.contains("Gawr Gura: предатель, 85% (сильное)."), "{text}");
        assert!(text.contains("сдала на прослушивании."), "{text}");
        assert!(!text.contains("Gawr_Gura"), "в текст ушло имя файла");
        assert!(!text.contains("не догадывается"), "{text}");
    }

    #[test]
    fn test_secret_sentence_only_when_secret() {
        let open = block(&[bond("Gawr_Gura.png", "сестра", 70, "", false)]);
        let hidden = block(&[bond("Gawr_Gura.png", "сестра", 70, "", true)]);
        assert!(!open.contains("не догадывается"), "{open}");
        assert!(hidden.contains("Gawr Gura не догадывается."), "{hidden}");
        assert!(
            hidden.contains("Строка «не догадывается»"),
            "нет пояснения, как держать тайну"
        );
    }

    #[test]
    fn test_empty_block_and_join() {
        assert_eq!(block(&[]), "");
        assert_eq!(join("черты", ""), "черты");
        assert_eq!(join("", "связи"), "связи");
        assert_eq!(join("", ""), "");
        assert_eq!(join("черты", "связи"), "черты\n\nсвязи");
    }

    /// A validation refusal must not create a file — otherwise the very first error
    /// would create an empty store in the program folder.
    #[test]
    fn test_set_does_not_create_file_on_refusal() {
        let file = dir("refusal").join("bonds.json");
        let err = set_to(&file, "A.png", vec![bond("", "", 0, "", false)], &known(&["B.png"]))
            .unwrap_err();
        assert!(!err.is_empty(), "{err}");
        assert!(!file.exists());
    }

    #[test]
    fn test_set_roundtrip_and_clearing() {
        let file = dir("roundtrip").join("bonds.json");
        let known = known(&["B.png"]);
        let saved = set_to(
            &file,
            "A.png",
            vec![bond("B.png", "сестра", 70, "", false)],
            &known,
        )
        .unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(bonds_at(&file, "A.png").len(), 1);
        // An empty list removes the entry entirely: the card stops receiving the block.
        set_to(&file, "A.png", vec![], &known).unwrap();
        assert!(bonds_at(&file, "A.png").is_empty());
        assert!(!load_from(&file).contains_key("A.png"));
    }

    fn bonds_at(file: &Path, avatar: &str) -> Vec<Bond> {
        load_from(file).remove(avatar).unwrap_or_default()
    }
}

