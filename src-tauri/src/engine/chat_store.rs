//! Own-engine chat history: one JSONL file per chat,
//! `data/chats/<character_id>/<chat_id>.jsonl`, one line per message.
//!
//! Appending a line instead of rewriting the file: a message written before a failure
//! survives a crash in the middle of the next one. A broken line (an interrupted write,
//! a manual edit) is skipped and counted, it does not bring down the whole chat.
//!
//! Execution: blocking file IO on the caller's thread; async commands
//! call it through `spawn_blocking`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// One lock for all chats: appending a reply and deleting a chat from different commands must not
/// interleave, and the operation rate is a few per second.
static STORE: Mutex<()> = Mutex::new(());

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    /// Unix time in milliseconds.
    pub at_ms: u64,
}

#[derive(serde::Serialize, Clone, Debug, PartialEq)]
pub struct ChatSummary {
    pub id: String,
    pub updated_ms: u64,
}

#[derive(serde::Serialize, Clone, Debug, PartialEq)]
pub struct LoadedChat {
    pub messages: Vec<ChatMessage>,
    /// How many lines failed to parse — the UI can warn.
    pub skipped_lines: usize,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// IDs come from the frontend and become file names: only
/// `[A-Za-z0-9_-]`, otherwise `..\` would take a write outside `data/chats`.
pub fn validate_id(id: &str) -> Result<(), String> {
    let ok = !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(format!("недопустимый идентификатор: {id:?}"))
    }
}

/// New ID: time plus a counter — two chats in the same millisecond will not
/// collide within the process.
pub fn new_id(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed) % 1000;
    format!("{prefix}{}{n:03}", now_ms())
}

pub struct ChatStore {
    root: PathBuf,
}

impl ChatStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn chat_path(&self, character_id: &str, chat_id: &str) -> Result<PathBuf, String> {
        validate_id(character_id)?;
        validate_id(chat_id)?;
        Ok(self.root.join(character_id).join(format!("{chat_id}.jsonl")))
    }

    /// Create an empty chat and return its id.
    pub fn create(&self, character_id: &str) -> Result<String, String> {
        let _guard = STORE.lock().unwrap_or_else(|e| e.into_inner());
        let id = new_id("chat");
        let path = self.chat_path(character_id, &id)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("папка чатов не создана: {e}"))?;
        }
        std::fs::File::create(&path).map_err(|e| format!("чат не создан: {e}"))?;
        Ok(id)
    }

    /// The character's chats, newest first.
    pub fn list(&self, character_id: &str) -> Result<Vec<ChatSummary>, String> {
        validate_id(character_id)?;
        let dir = self.root.join(character_id);
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("чаты не прочитаны: {e}")),
        };
        let mut chats: Vec<ChatSummary> = entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension()? != "jsonl" {
                    return None;
                }
                let id = path.file_stem()?.to_str()?.to_string();
                let updated_ms = entry
                    .metadata()
                    .ok()?
                    .modified()
                    .ok()?
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?
                    .as_millis() as u64;
                Some(ChatSummary { id, updated_ms })
            })
            .collect();
        chats.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms).then_with(|| b.id.cmp(&a.id)));
        Ok(chats)
    }

    pub fn load(&self, character_id: &str, chat_id: &str) -> Result<LoadedChat, String> {
        let path = self.chat_path(character_id, chat_id)?;
        let _guard = STORE.lock().unwrap_or_else(|e| e.into_inner());
        read_messages(&path)
    }

    pub fn append(&self, character_id: &str, chat_id: &str, message: &ChatMessage) -> Result<(), String> {
        let path = self.chat_path(character_id, chat_id)?;
        let mut line = serde_json::to_string(message).map_err(|e| e.to_string())?;
        line.push('\n');
        let _guard = STORE.lock().unwrap_or_else(|e| e.into_inner());
        // Append only to an existing chat: a chat deleted during generation
        // must not come back to life with a single reply.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|e| format!("чат не открыт для записи: {e}"))?;
        file.write_all(line.as_bytes()).map_err(|e| format!("сообщение не записано: {e}"))?;
        file.sync_data().map_err(|e| format!("сообщение не сброшено на диск: {e}"))
    }

    /// Remove the model's last reply (regeneration). The file is rewritten
    /// via tmp + rename: an interrupted write does not eat the history.
    pub fn pop_last_assistant(&self, character_id: &str, chat_id: &str) -> Result<Option<ChatMessage>, String> {
        let path = self.chat_path(character_id, chat_id)?;
        let _guard = STORE.lock().unwrap_or_else(|e| e.into_inner());
        let mut loaded = read_messages(&path)?;
        if loaded.messages.last().map(|m| m.role) != Some(Role::Assistant) {
            return Ok(None);
        }
        let popped = loaded.messages.pop();
        rewrite(&path, &loaded.messages)?;
        Ok(popped)
    }

    pub fn delete(&self, character_id: &str, chat_id: &str) -> Result<(), String> {
        let path = self.chat_path(character_id, chat_id)?;
        let _guard = STORE.lock().unwrap_or_else(|e| e.into_inner());
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("чат не удалён: {e}")),
        }
    }
}

fn read_messages(path: &Path) -> Result<LoadedChat, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("чат не прочитан: {e}"))?;
    let mut messages = Vec::new();
    let mut skipped_lines = 0;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<ChatMessage>(line) {
            Ok(message) => messages.push(message),
            Err(_) => skipped_lines += 1,
        }
    }
    if skipped_lines > 0 {
        crate::boot_log(&format!("Чат {}: пропущено битых строк — {skipped_lines}", path.display()));
    }
    Ok(LoadedChat { messages, skipped_lines })
}

fn rewrite(path: &Path, messages: &[ChatMessage]) -> Result<(), String> {
    let mut body = String::new();
    for message in messages {
        body.push_str(&serde_json::to_string(message).map_err(|e| e.to_string())?);
        body.push('\n');
    }
    let tmp = path.with_extension("jsonl.tmp");
    {
        let mut file = std::fs::File::create(&tmp).map_err(|e| format!("чат не переписан: {e}"))?;
        file.write_all(body.as_bytes()).map_err(|e| format!("чат не переписан: {e}"))?;
        file.sync_all().map_err(|e| format!("чат не сброшен на диск: {e}"))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("чат не заменён: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(name: &str) -> ChatStore {
        let root = std::env::temp_dir().join("cloufam_test_chat_store").join(name);
        let _ = std::fs::remove_dir_all(&root);
        ChatStore::new(root)
    }

    fn msg(role: Role, content: &str) -> ChatMessage {
        ChatMessage { role, content: content.to_string(), at_ms: 1 }
    }

    #[test]
    fn test_append_and_load_roundtrip() {
        let store = temp_store("roundtrip");
        let chat = store.create("hero").unwrap();
        store.append("hero", &chat, &msg(Role::User, "Привет")).unwrap();
        store.append("hero", &chat, &msg(Role::Assistant, "Здравствуй\nпутник")).unwrap();
        let loaded = store.load("hero", &chat).unwrap();
        assert_eq!(loaded.messages, vec![msg(Role::User, "Привет"), msg(Role::Assistant, "Здравствуй\nпутник")]);
        assert_eq!(loaded.skipped_lines, 0);
    }

    #[test]
    fn test_broken_line_is_skipped_not_fatal() {
        let store = temp_store("broken");
        let chat = store.create("hero").unwrap();
        store.append("hero", &chat, &msg(Role::User, "раз")).unwrap();
        let path = store.chat_path("hero", &chat).unwrap();
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"role\":\"assist").unwrap();
        file.write_all(b"\n").unwrap();
        store.append("hero", &chat, &msg(Role::User, "два")).unwrap();
        let loaded = store.load("hero", &chat).unwrap();
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.skipped_lines, 1);
    }

    #[test]
    fn test_path_traversal_ids_are_rejected() {
        let store = temp_store("ids");
        assert!(store.create("..\\evil").is_err());
        assert!(store.load("hero", "../x").is_err());
        assert!(store.list("a/b").is_err());
        assert!(validate_id("chat_1-2").is_ok());
    }

    #[test]
    fn test_append_to_deleted_chat_fails() {
        let store = temp_store("deleted");
        let chat = store.create("hero").unwrap();
        store.delete("hero", &chat).unwrap();
        assert!(store.append("hero", &chat, &msg(Role::Assistant, "поздно")).is_err());
        assert!(store.list("hero").unwrap().is_empty());
    }

    #[test]
    fn test_pop_last_assistant_only_pops_reply() {
        let store = temp_store("pop");
        let chat = store.create("hero").unwrap();
        store.append("hero", &chat, &msg(Role::User, "вопрос")).unwrap();
        assert_eq!(store.pop_last_assistant("hero", &chat).unwrap(), None);
        store.append("hero", &chat, &msg(Role::Assistant, "ответ")).unwrap();
        assert_eq!(store.pop_last_assistant("hero", &chat).unwrap(), Some(msg(Role::Assistant, "ответ")));
        assert_eq!(store.load("hero", &chat).unwrap().messages, vec![msg(Role::User, "вопрос")]);
    }

    #[test]
    fn test_list_of_missing_character_is_empty() {
        let store = temp_store("missing");
        assert!(store.list("nobody").unwrap().is_empty());
        let a = store.create("hero").unwrap();
        let b = store.create("hero").unwrap();
        assert_ne!(a, b);
        assert_eq!(store.list("hero").unwrap().len(), 2);
    }
}

