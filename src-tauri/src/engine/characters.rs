//! Own-core characters: `data/characters/<id>/character.json` + `avatar.png`.
//!
//! Import — the open card format: JSON in base64 in a PNG text chunk
//! (`ccv3` for V3, `chara` for V1/V2). The embedded `character_book` of the card
//! becomes the character's lorebook.
//!
//! Execution: blocking file IO on the caller's thread.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use base64::Engine as _;
use serde_json::Value;

use super::chat_store::{new_id, validate_id};
use super::lorebook::Lorebook;

static STORE: Mutex<()> = Mutex::new(());

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Character {
    pub id: String,
    pub name: String,
    pub description: String,
    pub personality: String,
    pub scenario: String,
    pub first_message: String,
    pub example_dialogue: String,
    /// The card's own system prompt; empty — the default prompt is used.
    pub system_prompt: String,
    pub tags: Vec<String>,
    pub has_avatar: bool,
}

pub struct CharacterStore {
    root: PathBuf,
}

impl CharacterStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn dir(&self, id: &str) -> Result<PathBuf, String> {
        validate_id(id)?;
        Ok(self.root.join(id))
    }

    pub fn avatar_path(&self, id: &str) -> Result<PathBuf, String> {
        Ok(self.dir(id)?.join("avatar.png"))
    }

    pub fn lorebook_path(&self, id: &str) -> Result<PathBuf, String> {
        Ok(self.dir(id)?.join("lorebook.json"))
    }

    /// All characters by name. An unreadable folder is skipped with a log entry:
    /// one corrupted file doesn't hide the rest.
    pub fn list(&self) -> Result<Vec<Character>, String> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("персонажи не прочитаны: {e}")),
        };
        let _guard = STORE.lock().unwrap_or_else(|e| e.into_inner());
        let mut characters = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path().join("character.json");
            match crate::json_store::read_state::<Character>(&path) {
                Ok(Some(character)) => characters.push(character),
                Ok(None) => {}
                Err(e) => crate::boot_log(&format!("Персонаж {} пропущен: {e}", path.display())),
            }
        }
        characters.sort_by_key(|c| c.name.to_lowercase());
        Ok(characters)
    }

    pub fn get(&self, id: &str) -> Result<Character, String> {
        let path = self.dir(id)?.join("character.json");
        let _guard = STORE.lock().unwrap_or_else(|e| e.into_inner());
        crate::json_store::read_state::<Character>(&path)?.ok_or_else(|| format!("персонаж {id} не найден"))
    }

    pub fn save(&self, character: &Character) -> Result<(), String> {
        let dir = self.dir(&character.id)?;
        let _guard = STORE.lock().unwrap_or_else(|e| e.into_inner());
        std::fs::create_dir_all(&dir).map_err(|e| format!("папка персонажа не создана: {e}"))?;
        crate::json_store::save_value(character, &dir.join("character.json"))
    }

    /// Import a PNG card: the character, the avatar (the PNG itself), and the lorebook from the card.
    pub fn import_png(&self, png: &[u8]) -> Result<(Character, Option<Lorebook>), String> {
        let card = card_json_from_png(png)?;
        let (mut character, book) = character_from_card(&card)?;
        character.id = new_id("char");
        character.has_avatar = true;
        self.save(&character)?;
        let avatar = self.avatar_path(&character.id)?;
        std::fs::write(&avatar, png).map_err(|e| format!("аватар не сохранён: {e}"))?;
        if let Some(book) = &book {
            book.save(&self.lorebook_path(&character.id)?)?;
        }
        Ok((character, book))
    }

    pub fn import_png_file(&self, path: &Path) -> Result<(Character, Option<Lorebook>), String> {
        let png = std::fs::read(path).map_err(|e| format!("файл карточки не прочитан: {e}"))?;
        self.import_png(&png)
    }

    pub fn delete(&self, id: &str) -> Result<(), String> {
        let dir = self.dir(id)?;
        let _guard = STORE.lock().unwrap_or_else(|e| e.into_inner());
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("персонаж не удалён: {e}")),
        }
    }
}

/// Card JSON from PNG text chunks. `ccv3` is preferred over `chara`:
/// V3 cards write both, and V3 is more complete.
pub fn card_json_from_png(png: &[u8]) -> Result<Value, String> {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    if !png.starts_with(SIGNATURE) {
        return Err("это не PNG".to_string());
    }
    let mut chara = None;
    let mut ccv3 = None;
    let mut pos = SIGNATURE.len();
    // Chunk: length (4, big-endian), type (4), data, CRC (4). We don't verify the CRC:
    // cards from editors are often re-saved with a wrong checksum.
    while pos + 8 <= png.len() {
        let len = u32::from_be_bytes([png[pos], png[pos + 1], png[pos + 2], png[pos + 3]]) as usize;
        let kind = &png[pos + 4..pos + 8];
        let data_end = pos + 8 + len;
        if data_end > png.len() {
            break;
        }
        if kind == b"tEXt" {
            let data = &png[pos + 8..data_end];
            if let Some(nul) = data.iter().position(|&b| b == 0) {
                let (keyword, text) = (&data[..nul], &data[nul + 1..]);
                match keyword {
                    b"ccv3" => ccv3 = Some(text.to_vec()),
                    b"chara" => chara = Some(text.to_vec()),
                    _ => {}
                }
            }
        }
        if kind == b"IEND" {
            break;
        }
        pos = data_end + 4;
    }
    let raw = ccv3.or(chara).ok_or("в PNG нет данных карточки персонажа")?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(raw.trim_ascii())
        .map_err(|e| format!("данные карточки не в base64: {e}"))?;
    serde_json::from_slice(&decoded).map_err(|e| format!("данные карточки не JSON: {e}"))
}

/// Character from card JSON. V2/V3 keep fields in `data`, V1 — at the root.
pub fn character_from_card(card: &Value) -> Result<(Character, Option<Lorebook>), String> {
    let data = card.get("data").filter(|d| d.is_object()).unwrap_or(card);
    let text = |key: &str| data.get(key).and_then(Value::as_str).unwrap_or_default().trim().to_string();
    let name = text("name");
    if name.is_empty() {
        return Err("у карточки нет имени".to_string());
    }
    let character = Character {
        name: name.clone(),
        description: text("description"),
        personality: text("personality"),
        scenario: text("scenario"),
        first_message: text("first_mes"),
        example_dialogue: text("mes_example"),
        system_prompt: text("system_prompt"),
        tags: data
            .get("tags")
            .and_then(Value::as_array)
            .map(|tags| tags.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default(),
        ..Character::default()
    };
    let book = data
        .get("character_book")
        .filter(|b| b.is_object())
        .map(|b| Lorebook::from_import(b, &name))
        .filter(|b| !b.entries.is_empty());
    Ok((character, book))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// A minimal PNG with text chunks (the card doesn't need the image).
    pub(crate) fn png_with_text(chunks: &[(&str, &str)]) -> Vec<u8> {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut push = |kind: &[u8], data: &[u8]| {
            png.extend_from_slice(&(data.len() as u32).to_be_bytes());
            png.extend_from_slice(kind);
            png.extend_from_slice(data);
            png.extend_from_slice(&[0, 0, 0, 0]);
        };
        push(b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
        for (keyword, text) in chunks {
            let mut data = keyword.as_bytes().to_vec();
            data.push(0);
            data.extend_from_slice(text.as_bytes());
            push(b"tEXt", &data);
        }
        push(b"IEND", &[]);
        png
    }

    fn b64(value: &Value) -> String {
        base64::engine::general_purpose::STANDARD.encode(value.to_string())
    }

    fn temp_store(name: &str) -> CharacterStore {
        let root = std::env::temp_dir().join("cloufam_test_characters").join(name);
        let _ = std::fs::remove_dir_all(&root);
        CharacterStore::new(root)
    }

    #[test]
    fn test_v3_chunk_wins_over_chara() {
        let v2 = json!({ "spec": "chara_card_v2", "data": { "name": "Старое имя" } });
        let v3 = json!({ "spec": "chara_card_v3", "data": { "name": "Сераф", "first_mes": "Привет, {{user}}" } });
        let png = png_with_text(&[("chara", &b64(&v2)), ("ccv3", &b64(&v3))]);
        let card = card_json_from_png(&png).unwrap();
        let (character, _) = character_from_card(&card).unwrap();
        assert_eq!(character.name, "Сераф");
        assert_eq!(character.first_message, "Привет, {{user}}");
    }

    #[test]
    fn test_v1_card_fields_at_root() {
        let v1 = json!({ "name": "Ава", "description": "Библиотекарь", "mes_example": "<START>" });
        let (character, book) = character_from_card(&v1).unwrap();
        assert_eq!((character.name.as_str(), character.description.as_str()), ("Ава", "Библиотекарь"));
        assert_eq!(character.example_dialogue, "<START>");
        assert!(book.is_none());
    }

    #[test]
    fn test_png_without_card_and_non_png_are_errors() {
        assert!(card_json_from_png(b"GIF89a").unwrap_err().contains("не PNG"));
        let png = png_with_text(&[("Comment", "просто картинка")]);
        assert!(card_json_from_png(&png).unwrap_err().contains("нет данных"));
        let nameless = json!({ "data": { "description": "без имени" } });
        assert!(character_from_card(&nameless).is_err());
    }

    #[test]
    fn test_import_saves_character_avatar_and_book() {
        let store = temp_store("import");
        let card = json!({ "spec": "chara_card_v3", "data": {
            "name": "Сераф",
            "tags": ["fantasy"],
            "character_book": { "entries": [ { "keys": ["лес"], "content": "Лес Элдории", "enabled": true } ] }
        }});
        let png = png_with_text(&[("ccv3", &b64(&card))]);
        let (character, book) = store.import_png(&png).unwrap();
        assert!(book.is_some());
        assert_eq!(store.list().unwrap(), vec![character.clone()]);
        assert_eq!(std::fs::read(store.avatar_path(&character.id).unwrap()).unwrap(), png);
        let saved = Lorebook::load(&store.lorebook_path(&character.id).unwrap()).unwrap().unwrap();
        assert_eq!(saved.entries[0].content, "Лес Элдории");
        store.delete(&character.id).unwrap();
        assert!(store.list().unwrap().is_empty());
    }
}

