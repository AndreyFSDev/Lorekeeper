//! A chat core of our own without SillyTavern: characters, lorebook, history and streaming
//! the local model's reply through Ollama. Data lives in `data/characters` and `data/chats`.
//!
//! Commands without `async` are executed by Tauri on the main thread and do only short
//! file IO; `engine_send` is async on the tokio runtime, and its file IO goes into
//! `spawn_blocking`.

pub mod characters;
pub mod chat_store;
pub mod llm;
pub mod lorebook;
pub mod prompt;

use std::sync::{Arc, Mutex};

use base64::Engine as _;
use tauri::ipc::Channel;
use tauri::Manager;
use tokio::sync::watch;

use characters::{Character, CharacterStore};
use chat_store::{ChatMessage, ChatStore, ChatSummary, LoadedChat, Role};
use lorebook::Lorebook;

fn characters() -> CharacterStore {
    CharacterStore::new(crate::paths::data_dir().join("characters"))
}

fn chats() -> ChatStore {
    ChatStore::new(crate::paths::data_dir().join("chats"))
}

/// The running generation. One per app: there is one model on the GPU, and a
/// second parallel request would only double the time of both replies.
#[derive(Default)]
pub struct EngineState {
    cancel: Mutex<Option<watch::Sender<bool>>>,
}

/// Frees the generation slot on any exit from `engine_send`, including an error.
struct GenerationSlot<'a>(&'a EngineState);

impl Drop for GenerationSlot<'_> {
    fn drop(&mut self) {
        *self.0.cancel.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

#[derive(serde::Serialize, Clone)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ChatEvent {
    Delta { text: String },
    Done { message: Option<ChatMessage>, cancelled: bool },
}

fn user_name() -> String {
    crate::l10n::t("Пользователь", "User")
}

#[tauri::command]
pub fn engine_list_characters() -> Result<Vec<Character>, String> {
    characters().list()
}

#[tauri::command]
pub fn engine_import_character(path: String) -> Result<Character, String> {
    characters().import_png_file(std::path::Path::new(&path)).map(|(character, _)| character)
}

#[tauri::command]
pub fn engine_delete_character(character_id: String) -> Result<(), String> {
    characters().delete(&character_id)?;
    // Chats of a deleted character can no longer be opened — remove them with it.
    for chat in chats().list(&character_id)? {
        chats().delete(&character_id, &chat.id)?;
    }
    Ok(())
}

/// Avatar as a data URL: the window's CSP allows images only from `self` and `data:`.
#[tauri::command]
pub fn engine_avatar(character_id: String) -> Result<Option<String>, String> {
    let path = characters().avatar_path(&character_id)?;
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("аватар не прочитан: {e}")),
    }
}

/// Chat screen background: a copy of the chosen image, `data/chat/background.<ext>`.
fn background_dir() -> std::path::PathBuf {
    crate::paths::data_dir().join("chat")
}

const BACKGROUND_EXTS: [&str; 4] = ["png", "jpg", "jpeg", "webp"];
/// The background goes to the frontend as a full data URL; larger ones mean a visible pause on chat open.
const BACKGROUND_MAX_BYTES: u64 = 20 * 1024 * 1024;

fn image_mime(ext: &str) -> Option<&'static str> {
    match ext.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

fn find_background(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    BACKGROUND_EXTS.iter().map(|ext| dir.join(format!("background.{ext}"))).find(|p| p.is_file())
}

/// Background as a data URL (the window's CSP allows images only from `self` and `data:`).
#[tauri::command]
pub fn engine_background() -> Result<Option<String>, String> {
    let Some(path) = find_background(&background_dir()) else {
        return Ok(None);
    };
    let mime = path.extension().and_then(|e| e.to_str()).and_then(image_mime).unwrap_or("image/png");
    let bytes = std::fs::read(&path).map_err(|e| format!("фон не прочитан: {e}"))?;
    Ok(Some(format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))))
}

/// Set the background from a file (`path`) or remove it (`None`).
#[tauri::command]
pub fn engine_set_background(path: Option<String>) -> Result<Option<String>, String> {
    let dir = background_dir();
    let target = match path {
        Some(path) => Some(copy_background(std::path::Path::new(&path), &dir)?),
        None => None,
    };
    // The old background is removed only after the new one is copied: on error it stays.
    for ext in BACKGROUND_EXTS {
        let old = dir.join(format!("background.{ext}"));
        if Some(&old) != target.as_ref() && old.is_file() {
            std::fs::remove_file(&old).map_err(|e| format!("старый фон не удалён: {e}"))?;
        }
    }
    engine_background()
}

fn copy_background(src: &std::path::Path, dir: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .filter(|e| image_mime(e).is_some())
        .ok_or_else(|| crate::l10n::t("Фон: нужен PNG, JPEG или WebP", "Background: PNG, JPEG or WebP only"))?;
    let size = std::fs::metadata(src).map_err(|e| format!("файл фона недоступен: {e}"))?.len();
    if size > BACKGROUND_MAX_BYTES {
        return Err(crate::l10n::t("Фон больше 20 МБ", "Background is larger than 20 MB"));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("папка фона не создана: {e}"))?;
    let target = dir.join(format!("background.{ext}"));
    std::fs::copy(src, &target).map_err(|e| format!("фон не скопирован: {e}"))?;
    Ok(target)
}

#[tauri::command]
pub fn engine_get_lorebook(character_id: String) -> Result<Lorebook, String> {
    let path = characters().lorebook_path(&character_id)?;
    Ok(Lorebook::load(&path)?.unwrap_or_default())
}

#[tauri::command]
pub fn engine_save_lorebook(character_id: String, lorebook: Lorebook) -> Result<(), String> {
    characters().get(&character_id)?;
    lorebook.save(&characters().lorebook_path(&character_id)?)
}

/// Import a lorebook from a JSON file (a world file or `character_book`) instead of the current one.
#[tauri::command]
pub fn engine_import_lorebook(character_id: String, path: String) -> Result<Lorebook, String> {
    let character = characters().get(&character_id)?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("файл лорбука не прочитан: {e}"))?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("лорбук не JSON: {e}"))?;
    let fallback = std::path::Path::new(&path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(&character.name)
        .to_string();
    let book = Lorebook::from_import(&value, &fallback);
    if book.entries.is_empty() {
        return Err("в файле нет записей лорбука".to_string());
    }
    book.save(&characters().lorebook_path(&character_id)?)?;
    Ok(book)
}

#[tauri::command]
pub fn engine_list_chats(character_id: String) -> Result<Vec<ChatSummary>, String> {
    chats().list(&character_id)
}

#[derive(serde::Serialize)]
pub struct NewChat {
    id: String,
    messages: Vec<ChatMessage>,
}

/// A new chat; the card's greeting becomes the character's first message.
#[tauri::command]
pub fn engine_create_chat(character_id: String) -> Result<NewChat, String> {
    let character = characters().get(&character_id)?;
    let store = chats();
    let id = store.create(&character_id)?;
    let mut messages = Vec::new();
    if !character.first_message.trim().is_empty() {
        let greeting = ChatMessage {
            role: Role::Assistant,
            content: prompt::substitute(&character.first_message, &character.name, &user_name()),
            at_ms: chat_store::now_ms(),
        };
        store.append(&character_id, &id, &greeting)?;
        messages.push(greeting);
    }
    Ok(NewChat { id, messages })
}

#[tauri::command]
pub fn engine_load_chat(character_id: String, chat_id: String) -> Result<LoadedChat, String> {
    chats().load(&character_id, &chat_id)
}

#[tauri::command]
pub fn engine_delete_chat(character_id: String, chat_id: String) -> Result<(), String> {
    chats().delete(&character_id, &chat_id)
}

#[tauri::command]
pub fn engine_cancel(state: tauri::State<'_, EngineState>) {
    if let Some(cancel) = state.cancel.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        let _ = cancel.send(true);
    }
}

/// Send a turn (`text`) or regenerate the last reply (`text` = None) and stream
/// the reply into `on_event`. The reply is saved to history in full after the
/// stream ends; on cancellation — the part already shown, if it is not empty.
#[tauri::command]
pub async fn engine_send(
    app: tauri::AppHandle,
    state: tauri::State<'_, EngineState>,
    character_id: String,
    chat_id: String,
    text: Option<String>,
    on_event: Channel<ChatEvent>,
) -> Result<(), String> {
    let mut cancel_rx = {
        let mut slot = state.cancel.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return Err(crate::l10n::t("Ответ уже генерируется", "A reply is already being generated"));
        }
        let (tx, rx) = watch::channel(false);
        *slot = Some(tx);
        rx
    };
    let _slot = GenerationSlot(state.inner());

    let ollama_port = crate::gateway::ollama_port();
    if ollama_port == 0 {
        return Err(crate::l10n::t("Ollama ещё не запущена", "Ollama is not running yet"));
    }
    let app_state = app.state::<Arc<crate::AppState>>();
    let settings = app_state.settings.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let num_ctx = crate::gateway::chat_num_ctx(&app);

    let (cid, chid) = (character_id.clone(), chat_id.clone());
    let (character, book, history) = tokio::task::spawn_blocking(move || -> Result<_, String> {
        let character = characters().get(&cid)?;
        let book = Lorebook::load(&characters().lorebook_path(&cid)?)?.unwrap_or_default();
        let store = chats();
        match text {
            Some(text) if !text.trim().is_empty() => {
                let message = ChatMessage { role: Role::User, content: text, at_ms: chat_store::now_ms() };
                store.append(&cid, &chid, &message)?;
            }
            Some(_) => return Err("пустое сообщение".to_string()),
            None => {
                store.pop_last_assistant(&cid, &chid)?;
            }
        }
        let history = store.load(&cid, &chid)?.messages;
        Ok((character, book, history))
    })
    .await
    .map_err(|e| e.to_string())??;

    let recent: Vec<&str> = history.iter().map(|m| m.content.as_str()).collect();
    let lore = book.activate(&recent);
    let messages = prompt::build_messages(&character, &user_name(), &lore, &history);
    let messages = prompt::fit_to_context(messages, num_ctx, i64::from(settings.max_tokens))?;
    let body = crate::gateway::build_ollama_request(
        &serde_json::json!({
            "temperature": settings.temperature,
            "top_p": settings.top_p,
            "max_tokens": settings.max_tokens,
        }),
        settings.chat_model.clone(),
        messages,
        num_ctx,
    );

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(2))
        .no_proxy()
        .build()
        .map_err(|e| format!("http-клиент недоступен: {e}"))?;
    let reply = llm::stream_chat(&client, ollama_port, &body, &mut cancel_rx, |delta| {
        let _ = on_event.send(ChatEvent::Delta { text: delta.to_string() });
    })
    .await?;

    let text = reply.text.trim().to_string();
    let message = if text.is_empty() {
        None
    } else {
        let message = ChatMessage { role: Role::Assistant, content: text, at_ms: chat_store::now_ms() };
        let saved = message.clone();
        tokio::task::spawn_blocking(move || chats().append(&character_id, &chat_id, &saved))
            .await
            .map_err(|e| e.to_string())??;
        Some(message)
    };
    let _ = on_event.send(ChatEvent::Done { message, cancelled: reply.cancelled });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_image_mime() {
        assert_eq!(image_mime("JPG"), Some("image/jpeg"));
        assert_eq!(image_mime("webp"), Some("image/webp"));
        assert_eq!(image_mime("gif"), None);
    }

    #[test]
    fn test_copy_background_rejects_other_formats_and_keeps_extension() {
        let tmp = std::env::temp_dir().join(format!("lk-bg-{}", std::process::id()));
        let src_dir = tmp.join("src");
        let dir = tmp.join("chat");
        std::fs::create_dir_all(&src_dir).unwrap();
        let gif = src_dir.join("a.gif");
        std::fs::write(&gif, b"GIF").unwrap();
        assert!(copy_background(&gif, &dir).is_err());
        assert!(find_background(&dir).is_none());

        let jpg = src_dir.join("Room.JPG");
        std::fs::write(&jpg, b"jpeg").unwrap();
        let target = copy_background(&jpg, &dir).unwrap();
        assert_eq!(target, dir.join("background.jpg"));
        assert_eq!(find_background(&dir), Some(target));
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
