//! Prompt assembly: the system message (role, card, triggered lore, example
//! dialogue) and history. The gateway's shared budget fits it to the context:
//! the system part is never cut, the oldest turns are dropped.

use serde_json::{json, Value};

use super::characters::Character;
use super::chat_store::{ChatMessage, Role};
use super::lorebook::LoreEntry;

const DEFAULT_SYSTEM_PROMPT: &str = "You are {{char}} in an ongoing roleplay with {{user}}. \
Stay in character, write only {{char}}'s words and actions, never speak or act for {{user}}. \
Reply in the language {{user}} writes in.";

/// Substitutions of the open card format: `{{char}}` and `{{user}}` in any case.
pub fn substitute(text: &str, char_name: &str, user_name: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        match after[..end].trim().to_ascii_lowercase().as_str() {
            "char" => out.push_str(char_name),
            "user" => out.push_str(user_name),
            _ => out.push_str(&rest[start..start + 2 + end + 2]),
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

pub fn system_message(character: &Character, user_name: &str, lore: &[&LoreEntry]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let base = if character.system_prompt.trim().is_empty() { DEFAULT_SYSTEM_PROMPT } else { &character.system_prompt };
    parts.push(base.to_string());
    let mut card = format!("[Character: {}]", character.name);
    for (label, value) in [
        ("", &character.description),
        ("Personality: ", &character.personality),
        ("Scenario: ", &character.scenario),
    ] {
        if !value.trim().is_empty() {
            card.push_str(&format!("\n{label}{}", value.trim()));
        }
    }
    parts.push(card);
    if !lore.is_empty() {
        let body: Vec<&str> = lore.iter().map(|e| e.content.trim()).collect();
        parts.push(format!("[World info]\n{}", body.join("\n\n")));
    }
    if !character.example_dialogue.trim().is_empty() {
        parts.push(format!("[Example dialogue]\n{}", character.example_dialogue.trim()));
    }
    substitute(&parts.join("\n\n"), &character.name, user_name)
}

/// Messages for `/api/chat`: the system one and the whole history.
pub fn build_messages(character: &Character, user_name: &str, lore: &[&LoreEntry], history: &[ChatMessage]) -> Vec<Value> {
    let mut messages = vec![json!({ "role": "system", "content": system_message(character, user_name, lore) })];
    messages.extend(history.iter().map(|m| {
        let role = match m.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        json!({ "role": role, "content": m.content })
    }));
    messages
}

/// Fit to `num_ctx` with headroom for the reply (the `prompt_budget` contract).
pub fn fit_to_context(messages: Vec<Value>, num_ctx: u32, max_tokens: i64) -> Result<Value, String> {
    crate::gateway::checked_prompt_messages(Value::Array(messages), crate::gateway::prompt_budget(num_ctx, Some(max_tokens)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hero() -> Character {
        Character {
            name: "Сераф".into(),
            description: "Хранительница {{char}} леса".into(),
            personality: "добрая".into(),
            example_dialogue: "{{user}}: Привет\n{{char}}: Здравствуй".into(),
            ..Character::default()
        }
    }

    fn msg(role: Role, content: &str) -> ChatMessage {
        ChatMessage { role, content: content.into(), at_ms: 0 }
    }

    #[test]
    fn test_substitute_macros() {
        assert_eq!(substitute("{{char}} и {{User}} и {{ CHAR }}", "А", "Б"), "А и Б и А");
        assert_eq!(substitute("{{random}} {{user", "А", "Б"), "{{random}} {{user");
        assert_eq!(substitute("без макросов", "А", "Б"), "без макросов");
    }

    #[test]
    fn test_system_message_contains_card_lore_examples() {
        let lore = LoreEntry { content: "Элдория — лес".into(), ..LoreEntry::default() };
        let system = system_message(&hero(), "Путник", &[&lore]);
        assert!(system.contains("You are Сераф in an ongoing roleplay with Путник"));
        assert!(system.contains("Хранительница Сераф леса"));
        assert!(system.contains("Personality: добрая"));
        assert!(!system.contains("Scenario:"), "пустой сценарий не выводится");
        assert!(system.contains("[World info]\nЭлдория — лес"));
        assert!(system.contains("Путник: Привет\nСераф: Здравствуй"));
    }

    #[test]
    fn test_card_system_prompt_replaces_default() {
        let mut character = hero();
        character.system_prompt = "Ты — {{char}}.".into();
        let system = system_message(&character, "Путник", &[]);
        assert!(system.starts_with("Ты — Сераф."));
        assert!(!system.contains("[World info]"));
    }

    #[test]
    fn test_messages_order_and_roles() {
        let history = [msg(Role::Assistant, "Привет"), msg(Role::User, "Кто ты?")];
        let messages = build_messages(&hero(), "Путник", &[], &history);
        let roles: Vec<&str> = messages.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, vec!["system", "assistant", "user"]);
        assert_eq!(messages[2]["content"], "Кто ты?");
    }

    #[test]
    fn test_fit_drops_oldest_dialogue_keeps_system() {
        let mut history = Vec::new();
        for i in 0..400 {
            history.push(msg(if i % 2 == 0 { Role::User } else { Role::Assistant }, &format!("{i} {}", "слово ".repeat(40))));
        }
        let messages = build_messages(&hero(), "Путник", &[], &history);
        let fitted = fit_to_context(messages, 8192, 1000).unwrap();
        let list = fitted.as_array().unwrap();
        assert_eq!(list[0]["role"], "system");
        assert!(list.len() < 401, "история должна быть урезана");
        assert!(list.last().unwrap()["content"].as_str().unwrap().starts_with("399 "), "последняя реплика на месте");
    }
}
