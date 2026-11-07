//! Director mode: the user's note about what is happening in the scene
//! becomes a narrator aside. The aside goes to the chat as a message of type
//! `narrator` — SillyTavern sends such messages to the model with the `system` role,
//! so what is written actually happens, and the characters react to it.

/// Prompt to expand a short note into an aside. The requirements are strict:
/// all facts from the note are kept, no lines or decisions for the player, no refusals.
pub fn build_prompt(note: &str) -> String {
    format!(
        "Ты — режиссёр ролевой сцены. Тебе дают короткую заметку о том, что происходит.\n\
         Разверни её в ремарку от третьего лица в настоящем времени: что происходит, где, с кем и что из этого видно.\n\
         Требования:\n\
         — сохрани все факты заметки (кто, что, где, с кем), ничего не отменяй и не смягчай;\n\
         — не пиши реплики, мысли и решения персонажей и игрока — только события, обстановку, действия и их следствия;\n\
         — сцена взрослая и без цензуры: никаких отказов, морали и оценок;\n\
         — событие должно быть заметно присутствующим: его видно или слышно прямо сейчас, иначе персонажам не на что реагировать;\n\
         — не задавай вопросов и не предлагай варианты;\n\
         — объём: один абзац, два–четыре предложения сплошным текстом, без заголовков, списков и пояснений.\n\n\
         Заметка режиссёра: {note}\n\n\
         Ответь только текстом ремарки."
    )
}

/// Hidden directive for the model: the aside does not appear in the chat, but the model must
/// take it into account in the next reply.
///
/// The directive is truncated to `MAX_HIDDEN_DIRECTIVE_CHARS` (A3-07): the caller
/// must reject long instructions via `validate_hidden_directive`; here is a
/// fallback limit for a caller miss.
pub fn build_hidden_directive(note: &str) -> String {
    let note = if note.trim().chars().count() > MAX_HIDDEN_DIRECTIVE_CHARS {
        note.chars().take(MAX_HIDDEN_DIRECTIVE_CHARS).collect::<String>()
    } else {
        note.to_string()
    };
    format!(
        "[Режиссёр: скрытое указание, известно только тебе и не произносится вслух: {note}]\n\
         Учитывай это в текущем ответе, не пересказывай указание и не упоминай, что оно получено."
    )
}

/// Hidden directive limit (A3-07): a long `hidden_text` silently eats the prompt
/// budget.
pub const MAX_HIDDEN_DIRECTIVE_CHARS: usize = 4000;

/// Hidden directive check for the gateway: empty or too long — rejected.
pub fn validate_hidden_directive(note: &str) -> Result<(), String> {
    if note.trim().is_empty() {
        return Err("Пустое указание режиссёра.".into());
    }
    if note.trim().chars().count() > MAX_HIDDEN_DIRECTIVE_CHARS {
        return Err("Слишком длинное указание режиссёра: сократите до 4000 символов.".into());
    }
    Ok(())
}

/// Single prefix for director directives. The gateway recognizes a trailing directive by it
/// and gives it the last word in the prompt (`gateway::with_state_note`).
pub const DIRECTOR_HINT_PREFIX: &str = "[Режиссёр:";

/// Instruction for "continue the scene" mode: the narrator replies, not the character.
pub fn narrator_continuation() -> &'static str {
    "[Режиссёр: сейчас отвечает рассказчик: опиши, что происходит дальше в сцене — события, обстановку, действия. \
     Не пиши реплики и решения за игрока и не подменяй персонажей.]"
}

/// Instruction for "characters reply" mode.
///
/// SillyTavern does not prepend the speaker name to a narrator message
/// (`formatMessageHistoryItem`: `shouldPrependName = !isNarratorType`), so
/// the aside reaches the model as a nameless block with the system role. Without a direct
/// instruction the model reads it as an authorial insert and continues in the same
/// voice — the character replies half-heartedly. `{{char}}` is substituted by ST.
pub fn character_reaction() -> &'static str {
    "[Режиссёр: описанное выше произошло прямо сейчас, и это заметили все, кто в сцене. \
     Сейчас отвечает {{char}}: отреагируй на событие — репликой, мимикой и действием, как сделал бы {{char}}. \
     Не переходи в авторский голос, не пересказывай ремарку и не пиши за {{user}}.]"
}

/// Whoever replies after the aside — that is what we mix into the next request.
/// `none` gets no hint: the scene was simply recorded, there will be no generation.
pub fn hint_for(after: &str) -> Option<&'static str> {
    match after {
        "narrator" => Some(narrator_continuation()),
        "characters" => Some(character_reaction()),
        _ => None,
    }
}

/// Strip the service wrapping the model sometimes adds to the aside.
pub fn clean_narration(raw: &str) -> String {
    let mut text = raw.trim().to_string();
    for prefix in ["Ремарка:", "Ремарка режиссёра:", "Ответ:", "Narration:"] {
        if let Some(rest) = text.strip_prefix(prefix) {
            text = rest.trim().to_string();
        }
    }
    // Models like to wrap the reply in quotes or asterisks.
    text = text.trim_matches(|c| c == '"' || c == '«' || c == '»' || c == '*').trim().to_string();
    text
}

/// The command word after `//` and the rest of the line. The word is parsed the same way as
/// the extension (`/^\/\/\s*shop(_remove)?\b/i`): an ASCII word up to a boundary, case-
/// insensitive — "//shop potions" without a space is also a command, while "// shopping"
/// does not count as the word `shop`.
fn command_word(note: &str) -> Option<(String, &str)> {
    let rest = note.trim().strip_prefix("//")?.trim_start();
    let command_len = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    Some((rest[..command_len].to_ascii_lowercase(), &rest[command_len..]))
}

/// Command argument limits (A3-07): short ones go into RPG state,
/// long ones — a rejection with a hint, not silent truncation.
pub const MAX_SHOP_TYPE_CHARS: usize = 40;
pub const MAX_SKILL_NAME_CHARS: usize = 40;
pub const MAX_SKILL_STAT_CHARS: usize = 40;
pub const MAX_LOCATION_HINT_CHARS: usize = 500;

/// Director commands for managing shops.
/// `// shop <type>` — place a shop in the current location.
/// `// shop_remove` — remove the shop.
///
/// Parse by words, not by prefix: the extension strips the `//` marker from the aside
/// only when it recognizes the command, and the player may write `//shop potions` without
/// a space — with `strip_prefix("// shop ")` such input slipped into the scene
/// as ordinary text.
pub fn parse_shop_command(note: &str) -> Option<ShopCommand> {
    let (command, rest) = command_word(note)?;
    let argument = rest
        .trim_start_matches(|c: char| c.is_whitespace() || matches!(c, ':' | ',' | '-' | '—'))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    match command.as_str() {
        "shop_remove" => Some(if argument.is_empty() {
            ShopCommand::Remove
        } else {
            ShopCommand::Invalid(SHOP_USAGE)
        }),
        "shop" => Some(if argument.is_empty() {
            ShopCommand::Invalid(SHOP_USAGE)
        } else if argument.chars().count() > MAX_SHOP_TYPE_CHARS {
            ShopCommand::Invalid(SHOP_TOO_LONG)
        } else {
            ShopCommand::Set(argument)
        }),
        _ => None,
    }
}

pub const SHOP_USAGE: &str =
    "Команда магазина: «// shop <тип>» ставит магазин в текущую локацию, «// shop_remove» убирает его.";

pub const SHOP_TOO_LONG: &str = "Тип магазина — до 40 символов. Сократите.";

pub enum ShopCommand {
    /// Place a shop of the specified type.
    Set(String),
    /// Remove the shop.
    Remove,
    /// The command is recognized but malformed — a hint instead of the scene.
    Invalid(&'static str),
}

/// Custom skill command: `// skill <Name> -> <Stat>`.
/// Adding goes through `rpg::skills::add_custom_skill` (the HTTP handler
/// `POST /api/cloufam/rpg/skills/custom`), bound to a stat.
pub enum SkillCommand {
    /// Add the skill `name` bound to the stat `stat`.
    Add { name: String, stat: String },
    /// The command is recognized but malformed — a hint instead of the scene.
    Invalid(&'static str),
}

pub const SKILL_USAGE: &str =
    "Команда навыка: «// skill <Имя> -> <Характеристика>», например «// skill Фехтование -> Strength».";

pub const SKILL_TOO_LONG: &str = "Имя навыка и характеристика — до 40 символов каждое. Сократите.";

/// Parsing `// skill <Name> -> <Stat>` (ASCII `->` or the typographic
/// `→`). Empty parts and overlong arguments — a rejection with a hint.
pub fn parse_skill_command(note: &str) -> Option<SkillCommand> {
    let (command, rest) = command_word(note)?;
    if command != "skill" {
        return None;
    }
    let Some((name, stat)) = rest.split_once("->").or_else(|| rest.split_once("→")) else {
        return Some(SkillCommand::Invalid(SKILL_USAGE));
    };
    let name = name.trim();
    let stat = stat.trim();
    if name.is_empty() || stat.is_empty() {
        return Some(SkillCommand::Invalid(SKILL_USAGE));
    }
    if name.chars().count() > MAX_SKILL_NAME_CHARS || stat.chars().count() > MAX_SKILL_STAT_CHARS {
        return Some(SkillCommand::Invalid(SKILL_TOO_LONG));
    }
    Some(SkillCommand::Add {
        name: name.to_string(),
        stat: stat.to_string(),
    })
}

/// New location command: `// location [description]`. Generation uses
/// `rpg::world::generation_prompt`, commit — `rpg::world::commit_generated`.
pub enum LocationCommand {
    /// Create a location; the text after the command is its premise (may be empty).
    Generate(Option<String>),
    /// The command is recognized but malformed — a hint instead of the scene.
    Invalid(&'static str),
}

pub const LOCATION_TOO_LONG: &str =
    "Описание локации — до 500 символов. Сократите («// location» — генерация без описания).";

/// Parsing `// location [description]`.
pub fn parse_location_command(note: &str) -> Option<LocationCommand> {
    let (command, rest) = command_word(note)?;
    if command != "location" {
        return None;
    }
    let hint = rest.trim();
    if hint.is_empty() {
        return Some(LocationCommand::Generate(None));
    }
    if hint.chars().count() > MAX_LOCATION_HINT_CHARS {
        return Some(LocationCommand::Invalid(LOCATION_TOO_LONG));
    }
    Some(LocationCommand::Generate(Some(hint.to_string())))
}

/// An empty note or an empty model reply — a reason to say so outright.
pub fn validate(note: &str) -> Result<(), String> {
    if note.trim().is_empty() {
        return Err("Пустая ремарка: напишите, что должно произойти.".into());
    }
    if note.trim().chars().count() > 4000 {
        return Err("Слишком длинная ремарка: сократите до 4000 символов.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prompt_keeps_note_and_forbids_refusals() {
        let prompt = build_prompt("Алиса и Белла встречают незнакомца в плаще");
        assert!(prompt.contains("Алиса и Белла встречают незнакомца в плаще"));
        assert!(prompt.contains("ничего не отменяй"));
        assert!(prompt.contains("без цензуры"));
        assert!(prompt.contains("не пиши реплики"));
    }

    /// A long literary aside sets an authorial register, and the model replies
    /// in it instead of a character line — the expansion must stay short.
    #[test]
    fn test_prompt_caps_narration_at_one_paragraph() {
        let prompt = build_prompt("Мы в баре");
        assert!(prompt.contains("один абзац, два–четыре предложения"));
        assert!(!prompt.contains("один–три абзаца"));
        assert!(prompt.contains("заметно присутствующим"));
    }

    #[test]
    fn test_clean_narration_strips_wrappers() {
        assert_eq!(clean_narration("Ремарка: В комнате гаснет свет."), "В комнате гаснет свет.");
        assert_eq!(clean_narration("  «Дверь открывается.»  "), "Дверь открывается.");
        assert_eq!(clean_narration("*Гроза усиливается.*"), "Гроза усиливается.");
        assert_eq!(clean_narration("Гроза усиливается."), "Гроза усиливается.");
    }

    #[test]
    fn test_validate_rejects_empty_and_huge_notes() {
        assert!(validate("   ").is_err());
        assert!(validate(&"а".repeat(4001)).is_err());
        assert!(validate("Мы в баре").is_ok());
    }

    #[test]
    fn test_hidden_directive_marks_itself_secret() {
        let text = build_hidden_directive("Алиса влюблена, но скрывает это");
        // Shared prefix with asides: by it `with_state_note` gives
        // the directive the last word in the prompt.
        assert!(text.starts_with(DIRECTOR_HINT_PREFIX), "{text}");
        assert!(text.contains("не пересказывай указание"));
    }

    /// Without this directive the character replies in the authorial voice: the aside in the chat
    /// arrives without a speaker name, and the model has no one to address.
    #[test]
    fn test_character_reaction_addresses_the_responding_character() {
        let text = character_reaction();
        assert!(text.contains("{{char}}"));
        assert!(text.contains("Не переходи в авторский голос"));
        assert!(text.contains("{{user}}"));
    }

    #[test]
    fn test_hint_for_picks_the_directive_by_mode() {
        assert_eq!(hint_for("characters"), Some(character_reaction()));
        assert_eq!(hint_for("narrator"), Some(narrator_continuation()));
        assert_eq!(hint_for("none"), None);
        assert_eq!(hint_for(""), None);
    }

    /// The gateway recognizes the trailing directive by prefix and does not let the state note
    /// override it — without a shared prefix the directive search would diverge from the text.
    #[test]
    fn test_directives_carry_the_shared_prefix() {
        for hint in [character_reaction(), narrator_continuation()] {
            assert!(hint.starts_with(DIRECTOR_HINT_PREFIX), "{hint}");
        }
    }

    // parse_shop_command

    #[test]
    fn test_shop_set_parses_type() {
        assert!(matches!(
            parse_shop_command("// shop potion"),
            Some(ShopCommand::Set(t)) if t == "potion"
        ));
    }

    #[test]
    fn test_shop_set_trims_type() {
        assert!(matches!(
            parse_shop_command("  // shop potion  "),
            Some(ShopCommand::Set(t)) if t == "potion"
        ));
    }

    #[test]
    fn test_shop_remove_works() {
        assert!(matches!(
            parse_shop_command("// shop_remove"),
            Some(ShopCommand::Remove)
        ));
    }

    #[test]
    fn test_shop_remove_with_trailing_whitespace() {
        assert!(matches!(
            parse_shop_command("// shop_remove "),
            Some(ShopCommand::Remove)
        ));
    }

    #[test]
    fn test_shop_remove_boundary_rejects_suffix() {
        // "// shop_removed" must not match as Remove
        assert!(parse_shop_command("// shop_removed").is_none());
    }

    #[test]
    fn test_shop_without_type_is_rejected_with_usage() {
        assert!(matches!(parse_shop_command("// shop"), Some(ShopCommand::Invalid(_))));
        assert!(matches!(
            parse_shop_command("// shop_remove зелья"),
            Some(ShopCommand::Invalid(_))
        ));
    }

    #[test]
    fn test_shop_command_splits_like_the_extension() {
        for note in ["// shop: зелья", "// shop,зелья", "// shopзелья", "//shop — зелья"] {
            assert!(
                matches!(parse_shop_command(note), Some(ShopCommand::Set(t)) if t == "зелья"),
                "{note}"
            );
        }
        assert!(parse_shop_command("// shopping list").is_none());
    }

    #[test]
    fn test_shop_command_ignores_case() {
        assert!(matches!(
            parse_shop_command("// Shop зелья"),
            Some(ShopCommand::Set(t)) if t == "зелья"
        ));
        assert!(matches!(parse_shop_command("//SHOP_REMOVE"), Some(ShopCommand::Remove)));
    }

    /// Without a space it is still a command, not an aside: the extension gives the server
    /// the whole line precisely expecting `// shop`.
    #[test]
    fn test_shop_command_tolerates_missing_space() {
        assert!(matches!(
            parse_shop_command("//shop зелья"),
            Some(ShopCommand::Set(t)) if t == "зелья"
        ));
        assert!(matches!(
            parse_shop_command("//shop_remove"),
            Some(ShopCommand::Remove)
        ));
    }

    #[test]
    fn test_shop_unrelated_text_returns_none() {
        assert!(parse_shop_command("обычный текст").is_none());
    }

    // clean_narration edge cases

    #[test]
    fn test_clean_narration_empty_string() {
        assert_eq!(clean_narration(""), "");
    }

    // validate boundary

    #[test]
    fn test_validate_accepts_exactly_4000() {
        assert!(validate(&"а".repeat(4000)).is_ok());
    }

    #[test]
    fn test_validate_rejects_exactly_4001() {
        assert!(validate(&"а".repeat(4001)).is_err());
    }

    // parse_skill_command / parse_location_command

    #[test]
    fn test_skill_command_parses_name_and_stat() {
        assert!(matches!(
            parse_skill_command("// skill Фехтование -> Strength"),
            Some(SkillCommand::Add { name, stat }) if name == "Фехтование" && stat == "Strength"
        ));
        assert!(matches!(
            parse_skill_command("//skill Фехтование → Strength"),
            Some(SkillCommand::Add { name, stat }) if name == "Фехтование" && stat == "Strength"
        ));
    }

    #[test]
    fn test_skill_command_without_arrow_is_rejected_with_usage() {
        // The hint is the payload of the Invalid variant: the gateway displays it.
        assert!(matches!(
            parse_skill_command("// skill Фехтование"),
            Some(SkillCommand::Invalid(usage)) if usage == SKILL_USAGE
        ));
        assert!(matches!(
            parse_skill_command("// skill -> Strength"),
            Some(SkillCommand::Invalid(usage)) if usage == SKILL_USAGE
        ));
        assert!(matches!(
            parse_skill_command("// skill Фехтование -> "),
            Some(SkillCommand::Invalid(usage)) if usage == SKILL_USAGE
        ));
    }

    /// Argument limits (A3-07): long ones — a rejection with a hint, not truncation.
    #[test]
    fn test_skill_and_shop_arguments_are_limited() {
        let long_name = format!("// skill {} -> Strength", "а".repeat(41));
        assert!(matches!(
            parse_skill_command(&long_name),
            Some(SkillCommand::Invalid(usage)) if usage == SKILL_TOO_LONG
        ));
        let long_stat = format!("// skill Фехтование -> {}", "s".repeat(41));
        assert!(matches!(parse_skill_command(&long_stat), Some(SkillCommand::Invalid(_))));
        let long_shop = format!("// shop {}", "зелье ".repeat(10));
        assert!(matches!(
            parse_shop_command(&long_shop),
            Some(ShopCommand::Invalid(usage)) if usage == SHOP_TOO_LONG
        ));
        assert!(matches!(
            parse_shop_command("// shop зелья"),
            Some(ShopCommand::Set(t)) if t == "зелья"
        ));
    }

    #[test]
    fn test_skill_command_is_not_confused_with_other_commands() {
        assert!(parse_skill_command("// shop potion").is_none());
        assert!(parse_skill_command("обычный текст").is_none());
        assert!(parse_skill_command("// skills Фехтование -> Strength").is_none(), "граница слова");
    }

    #[test]
    fn test_location_command_parses_hint() {
        assert!(matches!(
            parse_location_command("// location"),
            Some(LocationCommand::Generate(None))
        ));
        assert!(matches!(
            parse_location_command("//location узкий переулок за таверной"),
            Some(LocationCommand::Generate(Some(hint))) if hint == "узкий переулок за таверной"
        ));
        let long_hint = format!("// location {}", "место ".repeat(100));
        assert!(matches!(
            parse_location_command(&long_hint),
            Some(LocationCommand::Invalid(usage)) if usage == LOCATION_TOO_LONG
        ));
        assert!(parse_location_command("// shop potion").is_none());
    }

    /// The hidden directive is limited (A3-07): the gateway rejects long ones via
    /// `validate_hidden_directive`, and `build_hidden_directive` truncates as a
    /// fallback limit.
    #[test]
    fn test_hidden_directive_is_bounded() {
        assert!(validate_hidden_directive("Алиса влюблена").is_ok());
        assert!(validate_hidden_directive("").is_err());
        assert!(validate_hidden_directive(&"а".repeat(4001)).is_err());
        let clamped = build_hidden_directive(&"а".repeat(5000));
        assert!(clamped.chars().count() < 4200, "{}", clamped.chars().count());
    }
}

