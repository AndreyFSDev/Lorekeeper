//! Imports a text draft into SillyTavern V2 character card fields: the local
//! model distributes disconnected notes/prose/dialogue fragments across the fields
//! (name, description, personality, scenario, first message, example dialogue).
//! Mirrors `tagger.rs` in outline — the same prompt+parse pattern, the same
//! bridge `tagger/plugin-index.js` (`/api/plugins/cloufam/md-import`).

/// The context the gateway sends for this generation and the response reserve: the draft
/// must fit into `num_ctx − rules − num_predict`; otherwise Ollama truncates
/// the prompt itself, losing its tail.
const NUM_CTX_TOKENS: usize = 8192;
const NUM_PREDICT_TOKENS: usize = 900;
/// Margin for tokenizing names, markup and digits beyond the estimate below.
const TOKEN_MARGIN: usize = 64;

/// Conservative cost of a character in thirds of a token: ASCII is a third of a token
/// (Latin letters, digits and punctuation fit 3–4 characters per token), any other
/// character is a whole token: with most tokenizers Cyrillic runs at nearly one
/// character per token, and the old "~2 characters per token" budget is unsafe for it.
/// Overestimating the cost is safe, underestimating is not.
fn char_cost_thirds(c: char) -> usize {
    if c.is_ascii() {
        1
    } else {
        3
    }
}

/// Estimates text length in tokens (top-down, via `char_cost_thirds`).
fn estimate_tokens(text: &str) -> usize {
    text.chars().map(char_cost_thirds).sum::<usize>().div_ceil(3)
}

/// The built prompt and a flag that the draft did not fit the budget and was
/// truncated: truncation must be visible to the caller, not stay silent
/// (w1 №7).
#[derive(Debug, Clone, PartialEq)]
pub struct BuiltPrompt {
    pub prompt: String,
    pub truncated: bool,
}

fn format_prompt(cut: &str) -> String {
    format!(
        "Ты — редактор карточек персонажей SillyTavern. Тебе дают черновик — заметки, обрывки \
         диалогов или прозу, из которых нужно собрать карточку персонажа.\n\n\
         Правила:\n\
         - Ответ — ТОЛЬКО JSON вида {{\"name\": \"...\", \"description\": \"...\", \"personality\": \"...\", \
         \"scenario\": \"...\", \"first_mes\": \"...\", \"mes_example\": \"...\"}}, без пояснений и без markdown.\n\
         - Ничего не выдумывай сверх того, что есть или прямо следует из черновика; неизвестное поле — пустая строка.\n\
         - description — внешность и факты о персонаже от третьего лица.\n\
         - personality — черты характера коротко.\n\
         - scenario — обстановка и контекст сцены, если она есть в черновике.\n\
         - first_mes — приветствие персонажа от первого лица, в его голосе; готовую реплику из черновика используй как есть.\n\
         - mes_example — пример диалога в формате SillyTavern (каждый пример начинается с новой строки <START>), \
         только если в черновике есть подходящие обрывки диалога.\n\n\
         Черновик:\n{cut}"
    )
}

/// Prompt for the local model: strict JSON, V2 card fields. The draft
/// is cut by the conservative token estimate; `truncated` reports that it did not
/// fit entirely. There is no blocking IO.
pub fn build_prompt_ex(draft: &str) -> BuiltPrompt {
    // The rules and the prompt frame also take context — their estimate uses the same
    // conservative scale (`estimate_tokens`) as the draft slice.
    let overhead_tokens = estimate_tokens(&format_prompt(""));
    let budget_tokens = NUM_CTX_TOKENS
        .saturating_sub(NUM_PREDICT_TOKENS)
        .saturating_sub(overhead_tokens)
        .saturating_sub(TOKEN_MARGIN);
    let budget_thirds = budget_tokens * 3;
    let mut used = 0usize;
    let mut kept = 0usize;
    for c in draft.chars() {
        let cost = char_cost_thirds(c);
        if used + cost > budget_thirds {
            break;
        }
        used += cost;
        kept += 1;
    }
    let total = draft.chars().count();
    let cut: String = draft.chars().take(kept).collect();
    BuiltPrompt {
        prompt: format_prompt(&cut),
        truncated: kept < total,
    }
}

/// The legacy form without the truncation flag — tests only; production calls
/// go through `build_prompt_ex` so the truncation flag is not lost.
#[cfg(test)]
pub fn build_prompt(draft: &str) -> String {
    build_prompt_ex(draft).prompt
}

/// Parsed card fields. An empty string means the model found no
/// material for this field in the draft, not that it "forgot to fill it in".
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize)]
pub struct ImportedFields {
    pub name: String,
    pub description: String,
    pub personality: String,
    pub scenario: String,
    pub first_mes: String,
    pub mes_example: String,
}

impl ImportedFields {
    fn is_empty(&self) -> bool {
        self.name.is_empty()
            && self.description.is_empty()
            && self.personality.is_empty()
            && self.scenario.is_empty()
            && self.first_mes.is_empty()
            && self.mes_example.is_empty()
    }
}

/// Extract the first balanced JSON object from the model reply. The old slice
/// "first `{` … last `}`" (w1 #8) failed when the text around the JSON or inside
/// field strings contained its own braces (`{{char}}`, "{…}" in the description):
/// braces inside strings do not count toward the balance, and an invalid object is skipped
/// in favor of the next one.
fn extract_json_object(raw: &str) -> Option<serde_json::Value> {
    json_objects(raw).into_iter().next()
}

/// All balanced JSON objects of the reply, in order. Braces inside strings
/// are not counted. An invalid object is skipped in favor of the next one.
pub(crate) fn json_objects(raw: &str) -> Vec<serde_json::Value> {
    let mut found = Vec::new();
    let mut search_from = 0usize;
    while let Some(rel_start) = raw[search_from..].find('{') {
        let start = search_from + rel_start;
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        let mut parsed_end = None;
        for (offset, c) in raw[start..].char_indices() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    in_string = false;
                }
                continue;
            }
            match c {
                '"' => in_string = true,
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        let end = start + offset + c.len_utf8();
                        if let Ok(value) =
                            serde_json::from_str::<serde_json::Value>(&raw[start..end])
                        {
                            found.push(value);
                            parsed_end = Some(end);
                        }
                        break;
                    }
                }
                _ => {}
            }
        }
        search_from = parsed_end.unwrap_or(start + 1);
    }
    found
}

/// Parses the model reply. `None` — the model sent no JSON or the reply is empty in
/// all fields (so the parse is good for nothing; an explicit error is better than
/// handing the user an empty card).
pub fn parse_response(raw: &str) -> Option<ImportedFields> {
    let value = extract_json_object(raw)?;
    let field = |key: &str| {
        value
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let fields = ImportedFields {
        name: field("name"),
        description: field("description"),
        personality: field("personality"),
        scenario: field("scenario"),
        first_mes: field("first_mes"),
        mes_example: field("mes_example"),
    };
    if fields.is_empty() {
        None
    } else {
        Some(fields)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_prompt_contains_draft_and_contract() {
        let prompt = build_prompt("Эльфийка-архивариус, саркастичная, живёт в библиотеке.");
        assert!(prompt.contains("Эльфийка-архивариус"));
        assert!(prompt.contains("\"first_mes\""));
        assert!(prompt.contains("Ничего не выдумывай"));
    }

    /// The budget is in tokens, not characters: a Cyrillic draft is cut earlier than
    /// a Latin one of the same size, and the prompt plus response reserve fits the context.
    #[test]
    fn build_prompt_budgets_by_tokens_and_reports_truncation() {
        let cyrillic = "я".repeat(NUM_CTX_TOKENS);
        let built = build_prompt_ex(&cyrillic);
        assert!(built.truncated, "кириллица должна усечься");
        assert!(
            estimate_tokens(&built.prompt) + NUM_PREDICT_TOKENS <= NUM_CTX_TOKENS,
            "промпт с ответом не влезает в контекст"
        );
        // The same size in Latin fits: a cut by characters (12,000) would have trimmed
        // it too, although it has three times fewer tokens.
        let latin = "a".repeat(16_000);
        let built = build_prompt_ex(&latin);
        assert!(!built.truncated, "латиница укладывается в токенный бюджет");
        // A short draft is left untouched, the flag is honest.
        let built = build_prompt_ex("Эльфийка-архивариус.");
        assert!(!built.truncated);
        assert!(built.prompt.contains("Эльфийка-архивариус."));
    }

    #[test]
    fn parse_response_happy_path() {
        let raw = r#"{"name": "Эхо", "description": "Эльфийка-архивариус.", "personality": "саркастичная",
                       "scenario": "", "first_mes": "Опять ты тут?", "mes_example": ""}"#;
        let fields = parse_response(raw).expect("должен распарсить");
        assert_eq!(fields.name, "Эхо");
        assert_eq!(fields.description, "Эльфийка-архивариус.");
        assert_eq!(fields.scenario, "");
    }

    #[test]
    fn parse_response_extracts_json_from_surrounding_text() {
        let raw = r#"Вот разбор: {"name": "Лира", "description": "", "personality": "", "scenario": "", "first_mes": "", "mes_example": ""} — готово."#;
        let fields = parse_response(raw).expect("должен найти JSON");
        assert_eq!(fields.name, "Лира");
    }

    /// w1 #8: braces in the text around the JSON and inside field strings must not
    /// break object extraction.
    #[test]
    fn parse_response_survives_braces_in_prose_and_strings() {
        let raw = "Черновик с {{char}} и {пометкой} — результат:\n\
                   {\"name\": \"Лира {прозвище}\", \"description\": \"шрам {на щеке}\", \"personality\": \"\", \
                   \"scenario\": \"\", \"first_mes\": \"\", \"mes_example\": \"\"} — готово.";
        let fields = parse_response(raw).expect("должен найти JSON несмотря на скобки");
        assert_eq!(fields.name, "Лира {прозвище}");
        assert_eq!(fields.description, "шрам {на щеке}");
    }

    /// A balanced but non-JSON block before the real object is skipped.
    #[test]
    fn parse_response_skips_balanced_non_json_blocks() {
        let raw = "Сначала {не json}, а потом {\"name\": \"Эхо\"}";
        let fields = parse_response(raw).expect("второй блок — валидный JSON");
        assert_eq!(fields.name, "Эхо");
    }

    #[test]
    fn parse_response_returns_none_for_garbage() {
        assert!(parse_response("не json").is_none());
        assert!(parse_response("").is_none());
    }

    #[test]
    fn parse_response_returns_none_when_all_fields_empty() {
        let raw = r#"{"name": "", "description": "", "personality": "", "scenario": "", "first_mes": "", "mes_example": ""}"#;
        assert!(parse_response(raw).is_none(), "все поля пусты — разбор бесполезен");
    }

    #[test]
    fn parse_response_defaults_missing_fields_to_empty() {
        let raw = r#"{"name": "Эхо"}"#;
        let fields = parse_response(raw).expect("хотя бы одно поле заполнено");
        assert_eq!(fields.name, "Эхо");
        assert_eq!(fields.description, "");
        assert_eq!(fields.mes_example, "");
    }

    #[test]
    fn parse_response_trims_whitespace() {
        let raw = r#"{"name": "  Эхо  ", "description": "", "personality": "", "scenario": "", "first_mes": "", "mes_example": ""}"#;
        let fields = parse_response(raw).expect("должен распарсить");
        assert_eq!(fields.name, "Эхо");
    }
}

