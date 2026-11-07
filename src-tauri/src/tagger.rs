use serde::Deserialize;

/// Tag length limit in SillyTavern (the tag input field's maxlength).
pub const MAX_TAG_LEN: usize = 25;
/// How many description characters go into the prompt: cards can be tens of thousands of characters long.
const MAX_FIELD_CHARS: usize = 4000;
/// The whole card in the prompt. The gateway sends num_ctx 8192, and six fields of
/// MAX_FIELD_CHARS — up to 24,000 characters: Ollama truncated such a prompt along with
/// the answer rules. 12,000 characters fit even at ~2 characters per token.
const MAX_CARD_CHARS: usize = 12000;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TagRequest {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub personality: String,
    #[serde(default)]
    pub scenario: String,
    #[serde(default)]
    pub first_mes: String,
    #[serde(default)]
    pub mes_example: String,
    #[serde(default)]
    pub existing_tags: Vec<String>,
    pub max_tags: Option<u8>,
    pub language: Option<String>,
}

pub struct TagOptions<'a> {
    pub max_tags: usize,
    pub language: &'a str,
    pub custom_prompt: &'a str,
}

fn language_rule(language: &str) -> &'static str {
    match language {
        "en" => "Теги — на английском языке, строчными буквами (например: elf, teacher, cyberpunk).",
        "both" => "Часть тегов — на русском, часть — на английском языке, строчными буквами.",
        _ => "Теги — на русском языке, строчными буквами.",
    }
}

/// Prompt for the local model: strict JSON, short tags, no repeats of
/// tags the character already has.
pub fn build_prompt(req: &TagRequest, opts: &TagOptions) -> String {
    let mut card = String::new();
    let fields = [
        ("Имя", &req.name),
        ("Описание", &req.description),
        ("Характер", &req.personality),
        ("Сценарий", &req.scenario),
        ("Приветствие", &req.first_mes),
        ("Пример диалога", &req.mes_example),
    ];
    // Fields go from most to least important: the remaining budget goes to the tail.
    let mut budget = MAX_CARD_CHARS;
    for (title, value) in fields {
        let value = value.trim();
        if value.is_empty() || budget == 0 {
            continue;
        }
        let cut: String = value.chars().take(MAX_FIELD_CHARS.min(budget)).collect();
        budget -= cut.chars().count();
        card.push_str(&format!("{title}:\n{cut}\n\n"));
    }

    let existing = if req.existing_tags.is_empty() {
        "нет".to_string()
    } else {
        req.existing_tags.join(", ")
    };

    let custom = opts.custom_prompt.trim();
    let custom_block = if custom.is_empty() {
        String::new()
    } else {
        format!("\nДополнительные указания пользователя:\n{custom}\n")
    };

    format!(
        "Ты — редактор карточек персонажей. Составь теги персонажа по его карточке.\n\n\
Правила:\n\
- Ответ — ТОЛЬКО JSON вида {{\"tags\": [\"тег\", \"тег\"]}}, без пояснений и без markdown.\n\
- Не больше {max} тегов.\n\
- Тег — 1–3 слова, не длиннее {len} символов, без знаков препинания, без кавычек.\n\
- Теги описывают роль (например: учитель, эльфийка), характер (добрая, саркастичная), \
жанр и сеттинг (фэнтези, киберпанк, школа), тематику (романтика, приключения), \
возраст и статус — если они явно указаны в карточке.\n\
- {lang_rule}\n\
- Не повторяй уже имеющиеся теги персонажа: {existing}.\n\
- Не выдумывай черты, которых нет в карточке.\n{custom}\n\
Карточка персонажа:\n{card}",
        max = opts.max_tags,
        len = MAX_TAG_LEN,
        lang_rule = language_rule(opts.language),
        existing = existing,
        custom = custom_block,
        card = card,
    )
}

fn strip_fences(raw: &str) -> &str {
    let trimmed = raw.trim();
    let without_open = match trimmed.strip_prefix("```json") {
        Some(rest) => rest,
        None => trimmed.strip_prefix("```").unwrap_or(trimmed),
    };
    without_open.trim_end().trim_end_matches("```").trim()
}

/// Extract the array items from the model's reply in any of the forms seen:
/// JSON object, JSON array, markdown wrapper or a comma-separated string.
fn extract_candidates(raw: &str) -> Vec<String> {
    let body = strip_fences(raw);

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        let array = match value {
            serde_json::Value::Array(items) => Some(items),
            serde_json::Value::Object(mut map) => map
                .remove("tags")
                .or_else(|| map.remove("теги"))
                .and_then(|v| v.as_array().cloned()),
            _ => None,
        };
        if let Some(items) = array {
            return items
                .into_iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
        }
    }

    // The model may have wrapped the JSON in text: take the outermost array.
    if let (Some(start), Some(end)) = (body.find('['), body.rfind(']')) {
        if start < end {
            if let Ok(serde_json::Value::Array(items)) =
                serde_json::from_str::<serde_json::Value>(&body[start..=end])
            {
                return items
                    .into_iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect();
            }
        }
    }

    // Plain-text fallback: the model answered outside the JSON contract. Only a
    // tag-like list passes the plausibility threshold; a narrative shredded by
    // commas would turn phrase chunks into "tags". One implausible item drops
    // the whole parse (fail closed) — the same guard class as
    // `traits::Deltas::is_plausible` on the evaluator side.
    let items: Vec<String> = body
        .lines()
        .flat_map(|line| line.split([',', ';']))
        .map(|s| s.to_string())
        .filter(|s| !s.trim().is_empty())
        .collect();
    if items.iter().all(|item| is_plausible_item(item)) {
        items
    } else {
        Vec::new()
    }
}

/// A tag is 1–3 words per the prompt contract; prose clauses are longer.
const MAX_TAG_WORDS: usize = 3;

/// Clean a raw answer item into a tag candidate without truncation: strip list
/// markers, quotes and edge punctuation, collapse whitespace, lower case.
/// `None` — empty or shorter than 2 chars.
fn clean_tag(raw: &str) -> Option<String> {
    let mut s = raw.trim().to_string();
    // The line may have been a list item: "- tag", "1. tag", "* \"tag\"".
    while let Some(rest) = s.strip_prefix(['-', '*', '•']) {
        s = rest.trim_start().to_string();
    }
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() {
        let rest = s[digits.len()..].trim_start();
        if let Some(rest) = rest.strip_prefix(['.', ')', ':']) {
            s = rest.trim_start().to_string();
        }
    }
    const QUOTES: [char; 14] = [
        '"', '\'', '`', '#', '[', ']', '{', '}', '«', '»', '“', '”', '„', '‘',
    ];
    let s = s
        .trim_matches(|c: char| QUOTES.contains(&c) || c == '’')
        .replace(
            ['"', '\\', '\n', '\r', '\t', '«', '»', '“', '”', '„', '‘', '’', '<', '>', '&'],
            " ",
        );
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let s = s.trim_matches(|c: char| matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | '-')).trim();
    let s = s.to_lowercase();
    if s.chars().count() < 2 {
        return None;
    }
    Some(s)
}

/// Plausibility threshold for plain-text answer items: a tag is 1..=MAX_TAG_WORDS
/// words of letters/digits/hyphens only. Judged on the cleaned item BEFORE
/// truncation — truncation cuts a prose clause into something tag-shaped and
/// hides the evidence (that is how prose used to leak into card tags).
fn is_plausible_item(raw: &str) -> bool {
    let Some(cleaned) = clean_tag(raw) else {
        return false;
    };
    let words = cleaned.split_whitespace().count();
    (1..=MAX_TAG_WORDS).contains(&words)
        && cleaned
            .chars()
            .all(|c| c.is_alphanumeric() || c == ' ' || c == '-')
}

fn normalize_tag(raw: &str) -> Option<String> {
    let s = clean_tag(raw)?;
    if s.chars().count() <= MAX_TAG_LEN {
        return Some(s);
    }
    // Cut at a word boundary so the tag stays readable.
    let mut cut = String::new();
    for word in s.split(' ') {
        let candidate = if cut.is_empty() {
            word.to_string()
        } else {
            format!("{cut} {word}")
        };
        if candidate.chars().count() > MAX_TAG_LEN {
            break;
        }
        cut = candidate;
    }
    if cut.chars().count() >= 2 {
        Some(cut)
    } else {
        Some(s.chars().take(MAX_TAG_LEN).collect())
    }
}

/// Normalize the model's reply and drop tags the character already has
/// (case-insensitive comparison), preserving order and the limit.
pub fn parse_tags(raw: &str, existing: &[String], max_tags: usize) -> Vec<String> {
    let existing_lower: Vec<String> = existing.iter().map(|t| t.trim().to_lowercase()).collect();
    let mut result: Vec<String> = Vec::new();
    for candidate in extract_candidates(raw) {
        let Some(tag) = normalize_tag(&candidate) else { continue };
        if existing_lower.iter().any(|e| *e == tag) {
            continue;
        }
        if result.iter().any(|t| *t == tag) {
            continue;
        }
        result.push(tag);
        if result.len() >= max_tags {
            break;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(max_tags: usize, language: &str) -> TagOptions<'_> {
        TagOptions {
            max_tags,
            language,
            custom_prompt: "",
        }
    }

    fn request() -> TagRequest {
        TagRequest {
            name: "Эхо".into(),
            description: "Эльфийка-архивариус из киберпанк-библиотеки, говорит саркастично.".into(),
            existing_tags: vec!["Архивариус".into()],
            ..Default::default()
        }
    }

    #[test]
    fn test_build_prompt_contains_limits_and_existing() {
        let prompt = build_prompt(&request(), &options(6, "ru"));
        assert!(prompt.contains("Не больше 6 тегов"));
        assert!(prompt.contains("Архивариус"));
        assert!(prompt.contains("на русском языке"));
        assert!(prompt.contains("Эльфийка-архивариус"));
    }

    #[test]
    fn test_build_prompt_truncates_long_fields() {
        let mut req = request();
        req.description = "я".repeat(MAX_FIELD_CHARS * 2);
        let prompt = build_prompt(&req, &options(8, "ru"));
        assert!(prompt.chars().count() < MAX_FIELD_CHARS * 2);
    }

    /// Six long fields must not overflow num_ctx 8192: the card in the
    /// prompt is capped by a shared budget, the first fields go in whole.
    #[test]
    fn test_build_prompt_caps_whole_card() {
        let long = |c: &str| c.repeat(MAX_FIELD_CHARS * 2);
        let req = TagRequest {
            name: "Эхо".into(),
            description: long("q"),
            personality: long("w"),
            scenario: long("z"),
            first_mes: long("j"),
            mes_example: long("v"),
            ..Default::default()
        };
        let prompt = build_prompt(&req, &options(8, "ru"));
        let card = prompt.split("Карточка персонажа:\n").nth(1).expect("карточка в промпте");
        let letters = card.chars().filter(|c| "qwzjv".contains(*c)).count();
        assert!(letters <= MAX_CARD_CHARS, "карточка {letters} знаков");
        assert_eq!(card.matches('q').count(), MAX_FIELD_CHARS, "описание идёт целиком");
        assert!(!card.contains("Пример диалога"), "на хвост бюджета не осталось");
    }

    #[test]
    fn test_parse_json_object_and_dedupe() {
        let raw = r#"{"tags": ["Киберпанк", "саркастичная", "киберпанк", "Архивариус"]}"#;
        let existing = vec!["Архивариус".to_string()];
        assert_eq!(
            parse_tags(raw, &existing, 8),
            vec!["киберпанк".to_string(), "саркастичная".to_string()]
        );
    }

    #[test]
    fn test_parse_fenced_and_plain_list() {
        let fenced = "```json\n[\"школа\", \"романтика\"]\n```";
        assert_eq!(parse_tags(fenced, &[], 8), vec!["школа", "романтика"]);

        let plain = "1. фэнтези\n2. «добрая»\n- эльфийка";
        assert_eq!(parse_tags(plain, &[], 8), vec!["фэнтези", "добрая", "эльфийка"]);
    }

    #[test]
    fn test_parse_respects_limit_and_trims_long_tags() {
        let raw = r#"["очень длинное название тега которое не влезает", "ок", "второй", "третий"]"#;
        let tags = parse_tags(raw, &[], 3);
        assert_eq!(tags.len(), 3);
        assert!(tags[0].chars().count() <= MAX_TAG_LEN);
        assert_eq!(tags[0], "очень длинное название");
    }

    #[test]
    fn test_parse_multilingual_key_and_strips_punctuation() {
        let raw = r#"{"теги": ["!!! Сарказм !!!", ""]}"#;
        assert_eq!(parse_tags(raw, &[], 5), vec!["сарказм"]);
    }

    /// V-43: prose is off-contract. Shredding it by commas used to turn phrase
    /// chunks into "tags" that then went into the card. Same guard class as
    /// `traits::Deltas::is_plausible`: the whole plain-text parse is dropped
    /// (fail closed) unless every item looks like a tag.
    #[test]
    fn test_parse_prose_fails_plausibility_threshold() {
        let prose = "Персонаж — добрая эльфийка-архивариус из киберпанк-библиотеки, \
говорит саркастично и умно; помогает героям в их приключениях.";
        assert!(
            parse_tags(prose, &[], 8).is_empty(),
            "проза не должна нарезаться на теги"
        );
    }

    /// One sentence-like chunk poisons the answer: a mixed list is still prose,
    /// not a tag list.
    #[test]
    fn test_parse_drops_mixed_list_with_prose_chunk() {
        let mixed = "эльфийка, работает библиотекарем в большом городе, сарказм";
        assert!(parse_tags(mixed, &[], 8).is_empty());
    }

    /// A real comma-separated tag list still passes the threshold.
    #[test]
    fn test_parse_plain_tag_list_passes_plausibility_threshold() {
        let list = "эльфийка, архивариус, саркастичная";
        assert_eq!(
            parse_tags(list, &[], 8),
            vec!["эльфийка".to_string(), "архивариус".to_string(), "саркастичная".to_string()]
        );
    }
}
