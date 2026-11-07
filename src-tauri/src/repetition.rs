//! Anti-repetition: what the character has already done and said in the last replies.
//!
//! The "don't repeat yourself" request in the card (`traits::fragment`) is weak — the model
//! sees nothing concrete in it and keeps recycling the same gestures and turns
//! in circles. The note from here lists real fragments from the chat history, and
//! the ban becomes concrete. It goes into the prompt together with the live state
//! as the last system message (`gateway::live_state_note`).

use std::collections::{HashMap, HashSet};

use serde_json::Value;

/// How many last replies we scan: further back a repeat no longer reads as
/// intrusive, and the history goes into the prompt in full anyway.
const WINDOW: usize = 6;
/// How many fragments we list in the note.
const RECENT_LIMIT: usize = 6;
/// Fragment length in the note: beyond that it is already a retelling of the turn.
const FRAGMENT_CHARS: usize = 70;
/// Shorter than this a fragment says nothing ("She nods").
const MIN_FRAGMENT_CHARS: usize = 12;
/// How many actions we take from one turn: the rest is noise for comparison.
const MAX_FRAGMENTS_PER_REPLY: usize = 15;
/// By how many first words fragments are considered identical: verbatim repeats
/// are rare, usually the beginning matches ("she smiles again and…").
const KEY_WORDS: usize = 5;

/// Note for the prompt or `None` if there is nothing to ban in the history yet.
/// Test entry into the shared filter: the working call goes through
/// `repetition_note_for_speaker`, so as not to diverge from it.
#[cfg(test)]
pub fn repetition_note(messages: &Value) -> Option<String> {
    repetition_note_for_speaker(messages, None)
}

/// Same with the speaker filter: in a group chat all characters share the `assistant`
/// role, and the note attributed someone else's gestures to the current one.
pub fn repetition_note_for_speaker(messages: &Value, speaker: Option<&str>) -> Option<String> {
    let mut fragments: Vec<String> = Vec::new();
    for reply in assistant_replies(messages, speaker) {
        fragments.extend(extract_fragments(&reply));
    }
    if fragments.is_empty() {
        return None;
    }

    // The key is computed once: it allocates a string through lowercase and join.
    let keyed: Vec<(String, String)> = fragments
        .iter()
        .map(|fragment| (fragment.clone(), key(fragment)))
        .collect();

    let mut counts: HashMap<&str, usize> = HashMap::new();
    for (_, k) in &keyed {
        *counts.entry(k.as_str()).or_default() += 1;
    }

    // Fresh ones come first: what was in the last reply is easiest to repeat.
    let mut seen: HashSet<&str> = HashSet::new();
    let mut recent: Vec<String> = Vec::new();
    for (fragment, k) in keyed.iter().rev() {
        if !seen.insert(k.as_str()) {
            continue;
        }
        recent.push(shorten(fragment));
        if recent.len() >= RECENT_LIMIT {
            break;
        }
    }

    let mut repeated_keys: HashSet<&str> = HashSet::new();
    let mut repeated: Vec<String> = Vec::new();
    for (fragment, k) in &keyed {
        if counts.get(k.as_str()).copied().unwrap_or(0) < 2 || !repeated_keys.insert(k.as_str()) {
            continue;
        }
        repeated.push(shorten(fragment));
        if repeated.len() >= 3 {
            break;
        }
    }

    let mut note = format!("Это уже было в последних ответах: {}.", recent.join("; "));
    if !repeated.is_empty() {
        note.push_str(&format!(
            " Повторялось не один раз: {} — так больше не писать.",
            repeated.join("; ")
        ));
    }
    note.push_str(
        " В этом ответе — другие действия, другие жесты и другие формулировки; \
         сцена должна сдвинуться, а не повториться.",
    );
    Some(note)
}

/// How many words in a row count as verbatim copying of a message: fewer — it is
/// the common language of the scene, not a retelling of someone else's turn.
const COPY_RUN_WORDS: usize = 8;
/// How many words we compare: replies are never longer, and quadratic comparison
/// on long replies is already noticeable.
const COPY_COMPARE_WORDS: usize = 200;
/// How many words of the generation tail participate in the echo check per frame.
const ECHO_TAIL_WORDS: usize = 64;

/// Index of the message the reply repeats verbatim (8+ words in a row), or
/// `None`. This is "the character repeats past messages": the model
/// retells a turn from the history instead of giving a new reply.
#[cfg(test)]
pub fn copied_from(reply: &str, messages: &Value) -> Option<usize> {
    copied_from_with_speaker(reply, messages, None)
}

/// Same with the speaker filter (group chat): the copy is searched only among
/// the current character's turns.
pub fn copied_from_with_speaker(
    reply: &str,
    messages: &Value,
    speaker: Option<&str>,
) -> Option<usize> {
    let words = |text: &str| -> Vec<String> {
        text.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .take(COPY_COMPARE_WORDS)
            .map(|w| w.to_string())
            .collect()
    };
    let reply_words = words(reply);
    if reply_words.len() < COPY_RUN_WORDS {
        return None;
    }
    let mut best: Option<(usize, usize)> = None;
    for (index, message) in messages.as_array()?.iter().enumerate() {
        if message.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        let Some(text) = own_line(&message_text(message), speaker) else {
            continue;
        };
        let past = words(&text);
        let run = longest_common_run(&reply_words, &past);
        if run > best.map(|(_, run)| run).unwrap_or(0) {
            best = Some((index, run));
        }
    }
    match best {
        Some((index, run)) if run >= COPY_RUN_WORDS => Some(index),
        _ => None,
    }
}

/// Message text: a plain string or a multimodal array of parts.
fn message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// A word with its byte offset in the source text: the offset is needed to
/// find the cut position when a copy is detected.
fn word_spans(text: &str) -> Vec<(String, usize)> {
    let mut spans: Vec<(String, usize)> = Vec::new();
    let mut current = String::new();
    let mut start = 0;
    for (i, ch) in text.char_indices() {
        if ch.is_alphanumeric() {
            if current.is_empty() {
                start = i;
            }
            for lower in ch.to_lowercase() {
                current.push(lower);
            }
        } else if !current.is_empty() {
            spans.push((std::mem::take(&mut current), start));
        }
    }
    if !current.is_empty() {
        spans.push((current, start));
    }
    spans
}

fn gram_hash<W: std::hash::Hash>(words: &[W]) -> u64 {
    use std::hash::Hasher;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for word in words {
        word.hash(&mut hasher);
    }
    hasher.finish()
}

/// Echo-guard verdict for a piece of generation.
#[derive(Debug, PartialEq, Eq)]
pub enum EchoVerdict {
    /// The text is safe to show (may be empty: the tail was held back).
    Safe(String),
    /// The tail is a verbatim copy of the previous message: the remainder up to the start
    /// of the copy was returned, and generation should be stopped.
    Copied(String),
}

/// Tail echo guard: keeps a reply from ending with a verbatim repeat
/// of past messages ("pieces of old turns at the end of new ones"). The "head" check
/// (`copied_from_with_speaker`) regenerates a reply that started with a copy —
/// an echo at the end is too late to catch by regeneration, the text is already shown, so
/// the guard holds the last words to itself and cuts the copy when
/// it reaches the `COPY_RUN_WORDS` threshold.
///
/// Comparison sources are the character's turns (with the speaker filter) and the
/// user's messages: vector memory mixes in old turns of both roles, and
/// the model repeats both. Runs on the gateway thread; no concurrent
/// access — one request, one guard.
pub struct EchoGuard {
    /// Nothing to show: a continuation of the turn (`continue`) honestly continues
    /// the started text, it must not be cut.
    active: bool,
    stopped: bool,
    /// The tail of the shown text not yet returned to the client: a full copy can
    /// begin only inside it.
    pending: String,
    /// Hashes of all `COPY_RUN_WORDS`-word tuples of sources → source.
    /// The hash is not a decision but a filter: a matching source is confirmed by words.
    grams: HashMap<u64, usize>,
    /// Source words to confirm a hash hit and find the run length.
    sources: Vec<Vec<String>>,
}

impl EchoGuard {
    pub fn new(messages: &Value, speaker: Option<&str>) -> Self {
        let mut grams = HashMap::new();
        let mut sources: Vec<Vec<String>> = Vec::new();
        if let Some(list) = messages.as_array() {
            for message in list {
                let role = message.get("role").and_then(|r| r.as_str());
                let text = message_text(message);
                // Turns of other characters in a group chat are not a source: repeating
                // them is not an echo of the current speaker.
                let text = match role {
                    Some("assistant") => match own_line(&text, speaker) {
                        Some(own) => own,
                        None => continue,
                    },
                    Some("user") => text,
                    _ => continue,
                };
                let words: Vec<String> = text
                    .to_lowercase()
                    .split(|c: char| !c.is_alphanumeric())
                    .filter(|w| !w.is_empty())
                    .take(COPY_COMPARE_WORDS)
                    .map(|w| w.to_string())
                    .collect();
                if words.len() < COPY_RUN_WORDS {
                    continue;
                }
                let index = sources.len();
                for window in words.windows(COPY_RUN_WORDS) {
                    grams.entry(gram_hash(window)).or_insert(index);
                }
                sources.push(words);
            }
        }
        Self { active: true, stopped: false, pending: String::new(), grams, sources }
    }

    /// Guard disabled (reply continuation): everything passes through.
    pub fn inactive() -> Self {
        Self {
            active: false,
            stopped: false,
            pending: String::new(),
            grams: HashMap::new(),
            sources: Vec::new(),
        }
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    /// Run of a new generation piece. Returns a safe prefix; if a copy is
    /// found — the remainder up to its start, then generation stops.
    pub fn feed(&mut self, chunk: &str) -> EchoVerdict {
        if !self.active || self.stopped {
            return EchoVerdict::Safe(if self.active { String::new() } else { chunk.to_string() });
        }
        self.pending.push_str(chunk);
        self.settle(false)
    }

    /// End of generation: final check and release of everything held back.
    pub fn finish(&mut self) -> EchoVerdict {
        if !self.active || self.stopped {
            return EchoVerdict::Safe(String::new());
        }
        self.settle(true)
    }

    fn settle(&mut self, final_flush: bool) -> EchoVerdict {
        let spans = word_spans(&self.pending);
        if spans.len() < COPY_RUN_WORDS {
            // The run words are already released, or there are fewer than the threshold: no check.
            // In the final there is nothing to hold — generation is over.
            return if final_flush {
                EchoVerdict::Safe(std::mem::take(&mut self.pending))
            } else {
                EchoVerdict::Safe(String::new())
            };
        }
        let tail: Vec<&str> = spans
            .iter()
            .rev()
            .take(ECHO_TAIL_WORDS)
            .rev()
            .map(|(word, _)| word.as_str())
            .collect();
        if let Some(run) = self.copy_run_ending_here(&tail) {
            // The run ends at the last word: cut from its start, everything after is
            // the copy. A run longer than the window is cut at the window start: the copy
            // never reaches the client in full anyway.
            let run = run.min(tail.len());
            let cut = spans[spans.len() - run].1;
            let emit = self.pending[..cut].trim_end().to_string();
            self.pending.clear();
            self.stopped = true;
            return EchoVerdict::Copied(emit);
        }
        if final_flush {
            return EchoVerdict::Safe(std::mem::take(&mut self.pending));
        }
        EchoVerdict::Safe(self.release_tail(spans))
    }

    /// Returns everything except the last `COPY_RUN_WORDS - 1` words: any of them
    /// may turn out to be the start of a copy that reaches the threshold with the next frame.
    fn release_tail(&mut self, spans: Vec<(String, usize)>) -> String {
        let cut = spans[spans.len() - (COPY_RUN_WORDS - 1)].1;
        let emit = self.pending[..cut].to_string();
        self.pending = self.pending[cut..].to_string();
        emit
    }

    /// The longest verbatim run of words ending at the last word
    /// of the tail, if it reaches the copy threshold.
    fn copy_run_ending_here(&self, tail: &[&str]) -> Option<usize> {
        let candidates: Vec<usize> = tail
            .windows(COPY_RUN_WORDS)
            .filter_map(|window| self.grams.get(&gram_hash(window)).copied())
            .collect::<std::collections::HashSet<usize>>()
            .into_iter()
            .collect();
        if candidates.is_empty() {
            return None;
        }
        let mut best = 0;
        for index in candidates {
            let source = &self.sources[index];
            // dp[j] — the length of the run ending at the current tail word and
            // the j-th source word; after the last tail word the row maximum
            // is the run that reached the end of the tail.
            let mut prev = vec![0usize; source.len() + 1];
            for word in tail {
                let mut cur = vec![0usize; source.len() + 1];
                for (j, other) in source.iter().enumerate() {
                    if *word == other.as_str() {
                        cur[j + 1] = prev[j] + 1;
                    }
                }
                prev = cur;
            }
            best = best.max(prev.into_iter().max().unwrap_or(0));
        }
        (best >= COPY_RUN_WORDS).then_some(best)
    }
}

/// Length of the longest common contiguous piece of two word lists (DP over
/// common substrings).
fn longest_common_run(left: &[String], right: &[String]) -> usize {
    let mut prev = vec![0usize; right.len() + 1];
    let mut best = 0;
    for word in left {
        let mut cur = vec![0usize; right.len() + 1];
        for (j, other) in right.iter().enumerate() {
            if word == other {
                cur[j + 1] = prev[j] + 1;
                best = best.max(cur[j + 1]);
            }
        }
        prev = cur;
    }
    best
}

/// The character's last replies. Multimodal messages (with an image) come as
/// an array of parts — we take the text from them.
///
/// The character greeting (`first_mes`) is also an `assistant` role message, and
/// it used to fall under the ban: on the second turn the model got "this already
/// happened, don't write it again" about the very scene we are in, plus
/// "the scene must move on". The reply — escaping the current setting and a turn
/// past the user's word. We cut everything before the player's first turn.
fn assistant_replies(messages: &Value, speaker: Option<&str>) -> Vec<String> {
    let Some(list) = messages.as_array() else {
        return Vec::new();
    };
    let after_user = list
        .iter()
        .position(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        .unwrap_or(list.len());
    let mut replies: Vec<String> = Vec::new();
    for message in list[after_user..].iter().rev() {
        if message.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        let Some(text) = own_line(&message_text(message), speaker) else {
            continue;
        };
        if !text.trim().is_empty() {
            replies.push(text);
        }
        if replies.len() >= WINDOW {
            break;
        }
    }
    replies.reverse();
    replies
}

/// Speaker name, not a piece of prose before the colon: one to four words,
/// each capitalized ("Alice Carter", "Алиса"). "She sighs"
/// is filtered out — the second word is lowercase, and the turn remains its own.
pub fn looks_like_speaker_name(name: &str) -> bool {
    let words: Vec<&str> = name.split_whitespace().filter(|word| !word.is_empty()).collect();
    (1..=4).contains(&words.len())
        && words.iter().all(|word| word.chars().next().is_some_and(char::is_uppercase))
}

/// Turn of the current speaker: `Name: text` in a group chat. Someone else's name —
/// `None`; without a prefix the turn is its own (single chat). The prefix heuristic
/// is conservative: a colon in the first 40 characters, a name with capital letters,
/// no period/exclamation mark, a space after the colon.
fn own_line(text: &str, speaker: Option<&str>) -> Option<String> {
    let Some(speaker) = speaker else {
        return Some(text.to_string());
    };
    let Some((head, rest)) = text.split_once(':') else {
        return Some(text.to_string());
    };
    let head = head.trim();
    let plausible = !head.is_empty()
        && head.chars().count() <= 40
        && !head.contains(['.', '!', '?', '\n'])
        && rest.starts_with(char::is_whitespace)
        && looks_like_speaker_name(head);
    if !plausible {
        return Some(text.to_string());
    }
    // to_lowercase, not ASCII: "Сакура"/"сакура" is one speaker; `_` from the file name is a space
    let norm = |s: &str| s.trim().replace('_', " ").to_lowercase();
    if norm(head) == norm(speaker) {
        Some(rest.trim_start().to_string())
    } else {
        None
    }
}

/// What exactly repeats: actions in asterisks, and if there are none — sentences.
fn extract_fragments(reply: &str) -> Vec<String> {
    // Bold `**…**` inside an action cut it into pieces shorter than the threshold, and a repeat
    // of the same gesture with and without emphasis did not match by key.
    let actions: Vec<String> = reply
        .replace("**", "")
        .split('*')
        .skip(1)
        .step_by(2)
        .map(|part| part.trim().to_string())
        .filter(|part| part.chars().count() >= MIN_FRAGMENT_CHARS)
        // A turn with dozens of asterisks otherwise inflates the vector for no benefit.
        .take(MAX_FRAGMENTS_PER_REPLY)
        .collect();
    if !actions.is_empty() {
        return actions;
    }
    reply
        .split(['.', '!', '?', '\n', '…'])
        .map(|part| part.trim().to_string())
        .filter(|part| part.chars().count() >= MIN_FRAGMENT_CHARS)
        .take(3)
        .collect()
}

/// Comparison key: case, punctuation and phrase tail discarded.
fn key(fragment: &str) -> String {
    fragment
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .take(KEY_WORDS)
        .collect::<Vec<_>>()
        .join(" ")
}

fn shorten(fragment: &str) -> String {
    let flat = fragment.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= FRAGMENT_CHARS {
        return flat;
    }
    let cut: String = flat.chars().take(FRAGMENT_CHARS).collect();
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn chat(replies: &[&str]) -> Value {
        let mut list: Vec<Value> = Vec::new();
        for reply in replies {
            list.push(json!({ "role": "user", "content": "и что дальше?" }));
            list.push(json!({ "role": "assistant", "content": reply }));
        }
        Value::Array(list)
    }

    #[test]
    fn test_no_note_without_replies() {
        assert!(repetition_note(&json!([{ "role": "user", "content": "привет" }])).is_none());
        assert!(repetition_note(&json!([])).is_none());
    }

    #[test]
    fn test_actions_in_asterisks_are_listed() {
        let note = repetition_note(&chat(&[
            "*поправляет прядь волос и смеётся* Ну и денёк!",
            "*снова поправляет прядь волос* Слушай...",
        ]))
        .expect("заметка должна быть");
        assert!(note.contains("поправляет прядь волос"), "{note}");
        assert!(note.contains("другие жесты"), "{note}");
    }

    #[test]
    fn test_repeated_fragment_marked_separately() {
        let note = repetition_note(&chat(&[
            "*она хихикает и машет рукой* Привет!",
            "*она хихикает и машет рукой снова* Ну привет.",
            "*отводит взгляд в сторону окна* Хм.",
        ]))
        .expect("заметка должна быть");
        assert!(note.contains("Повторялось не один раз"), "{note}");
        assert!(note.contains("она хихикает и машет рукой"), "{note}");
    }

    /// Scene greeting (`first_mes`) is not "already happened": otherwise on the second turn
    /// the model receives a ban on the setting the scene just started in,
    /// and leaves it.
    #[test]
    fn test_greeting_is_not_banned_as_a_repetition() {
        let messages = json!([
            { "role": "system", "content": "описание персонажа" },
            { "role": "assistant", "content": "*в спальне мягко мерцает ночник, она сидит на простынях* Ауф!" },
            { "role": "user", "content": "привет, Фувава" },
            { "role": "assistant", "content": "*наклоняет голову набок и скалит зубы* Ауф-ауф!" },
        ]);
        let note = repetition_note(&messages).expect("ответ после реплики игрока есть");
        assert!(!note.contains("ночник"), "приветствие попало в запрет: {note}");
        assert!(!note.contains("простын"), "приветствие попало в запрет: {note}");
        assert!(note.contains("наклоняет голову набок"), "{note}");
    }

    /// Bold text outside actions does not shift the parity of asterisks, and inside
    /// an action does not cut it into fragments.
    #[test]
    fn test_bold_markdown_keeps_actions_whole() {
        let outside = extract_fragments("**Важно!** *тихо смеётся в ладонь* и *поправляет длинную прядь*");
        assert_eq!(outside, vec!["тихо смеётся в ладонь", "поправляет длинную прядь"]);

        let inside = extract_fragments("*она **очень** тихо смеётся в ладонь* Ну и ну.");
        assert_eq!(inside, vec!["она очень тихо смеётся в ладонь"]);
        assert_eq!(key(&inside[0]), key("она очень тихо смеётся в ладонь"));

        let bold_italic = extract_fragments("***резко оборачивается к двери*** Кто там?");
        assert_eq!(bold_italic, vec!["резко оборачивается к двери"]);
    }

    #[test]
    fn test_prose_without_asterisks_uses_sentences() {
        let note = repetition_note(&chat(&[
            "Она садится на край стола и смотрит в окно. За окном идёт дождь.",
        ]))
        .expect("заметка должна быть");
        assert!(note.contains("Она садится на край стола"), "{note}");
    }

    /// A reply that verbatim repeats a turn from the history is found together with its
    /// index — that is the copy source for regenerating without it.
    #[test]
    fn test_copied_from_finds_the_copied_message() {
        let messages = json!([
            { "role": "user", "content": "привет" },
            { "role": "assistant", "content": "*садится у окна и качает ногой* Пусть этот вечер продлится ещё немного, и тогда всё будет хорошо." },
            { "role": "user", "content": "продолжай" },
        ]);
        let copy = "*садится у окна и качает ногой* Пусть этот вечер продлится ещё немного, но не всё так плохо.";
        assert_eq!(copied_from(copy, &messages), Some(1));
        assert_eq!(
            copied_from("*встаёт и уходит к двери* Надо идти, время не ждёт.", &messages),
            None
        );
    }

    #[test]
    fn test_short_replies_are_not_copies() {
        let messages = chat(&["*садится у окна и качает ногой* Пусть этот вечер продлится ещё немного."]);
        assert_eq!(copied_from("Ага.", &messages), None);
    }

    #[test]
    fn test_short_replies_give_nothing() {
        assert!(repetition_note(&chat(&["Ага.", "Ну да."])).is_none());
    }

    #[test]
    fn test_fragment_is_shortened() {
        let long = format!("*{}*", "она очень медленно поворачивает голову к собеседнику ".repeat(3));
        let note = repetition_note(&chat(&[&long])).expect("заметка должна быть");
        assert!(note.contains('…'), "{note}");
        // A long turn must not inflate the prompt.
        assert!(note.chars().count() < 400, "{}", note.chars().count());
    }

    /// In a group chat the assistant role belongs to everyone: the note must not attribute
    /// someone else's gestures to the current character; copies do not search others' turns.
    #[test]
    fn test_group_chat_filters_other_speakers() {
        let long = "*поправляет прядь волос и тихо смеётся в ладонь* Привет, путник.";
        let messages = json!([
            { "role": "user", "content": "привет" },
            { "role": "assistant", "content": format!("Alice: {long}") },
            { "role": "assistant", "content": "*поправляет прядь волос и тихо смеётся* Ну привет." },
        ]);
        let note = repetition_note_for_speaker(&messages, Some("Alice")).expect("своя реплика");
        assert!(note.contains("тихо смеётся"), "{note}");
        // Bella's turn did not get into the current speaker's list.
        let foreign = json!([
            { "role": "user", "content": "привет" },
            { "role": "assistant", "content": format!("Bella: {long}") },
        ]);
        assert_eq!(
            copied_from_with_speaker(long, &foreign, Some("Alice")),
            None,
            "чужая реплика не должна считаться копией"
        );
    }

    #[test]
    fn test_multimodal_content_is_read() {
        let messages = json!([
            { "role": "user", "content": "что видишь?" },
            { "role": "assistant", "content": [
                { "type": "text", "text": "*щурится на яркое солнце* Ничего особенного." }
            ] },
        ]);
        let note = repetition_note(&messages).expect("заметка должна быть");
        assert!(note.contains("щурится на яркое солнце"), "{note}");
    }

    #[test]
    fn test_only_last_window_is_used() {
        let replies: Vec<String> = (0..10)
            .map(|i| format!("*действие номер {i} повторяется в этой реплике*"))
            .collect();
        let refs: Vec<&str> = replies.iter().map(|s| s.as_str()).collect();
        let note = repetition_note(&chat(&refs)).expect("заметка должна быть");
        assert!(!note.contains("действие номер 0"), "{note}");
        assert!(note.contains("действие номер 9"), "{note}");
    }

    fn echo_chat() -> Value {
        json!([
            { "role": "user", "content": "привет, расскажи о себе" },
            { "role": "assistant", "content": "*садится у окна и качает ногой* Пусть этот вечер продлится ещё немного, и тогда всё будет хорошо." },
            { "role": "user", "content": "а что дальше?" },
        ])
    }

    /// A copy at the end of the reply is cut when it reaches the threshold: the fresh
    /// part reaches the client, the copied tail does not.
    #[test]
    fn test_echo_guard_cuts_copy_at_tail() {
        let mut echo = EchoGuard::new(&echo_chat(), None);
        let fresh = "Она подходит к двери и оглядывается";
        assert_eq!(echo.feed(&format!("{fresh}. ")), EchoVerdict::Safe(String::new()));
        let copy = "Садится у окна и качает ногой пусть этот вечер продлится";
        let verdict = echo.feed(copy);
        assert_eq!(
            verdict,
            EchoVerdict::Copied(format!("{fresh}.")),
            "свежая часть выдана (с пунктуацией), копия отрезана"
        );
        assert!(echo.is_stopped());
        assert_eq!(echo.feed("ещё слова после остановки"), EchoVerdict::Safe(String::new()));
    }

    /// The tail is held until confirmation: without a copy the whole text goes through.
    #[test]
    fn test_echo_guard_holds_tail_and_flushes() {
        let mut echo = EchoGuard::new(&echo_chat(), None);
        let text = "Совершенно новый ответ из свежих слов, которых не было в истории диалога раньше";
        let mut shown = match echo.feed(text) {
            EchoVerdict::Safe(part) => part,
            other => panic!("копии нет, а защитник сработал: {other:?}"),
        };
        assert!(shown.chars().count() < text.chars().count(), "хвост обязан удерживаться");
        let rest = match echo.finish() {
            EchoVerdict::Safe(rest) => rest,
            other => panic!("копии нет, а финал сработал: {other:?}"),
        };
        shown.push_str(&rest);
        assert_eq!(shown, text);
    }

    /// A colon in prose ("She sighs: …") is not someone else's name. Otherwise the turn
    /// falls out of the index, and verbatim echo of that line at the end of the reply is not cut.
    #[test]
    fn test_echo_guard_indexes_prose_before_a_colon() {
        let messages = json!([
            { "role": "user", "content": "привет" },
            { "role": "assistant", "content": "Она вздыхает: тихо и долго, почти без слов, глядя в окно" },
        ]);
        let mut echo = EchoGuard::new(&messages, Some("Алиса"));
        let copy = "Она вздыхает: тихо и долго, почти без слов, глядя в окно";
        let verdict = match echo.feed(copy) {
            copied @ EchoVerdict::Copied(_) => copied,
            EchoVerdict::Safe(_) => echo.finish(),
        };
        match verdict {
            EchoVerdict::Copied(text) => assert!(
                !text.contains("глядя"),
                "копия прозы доехала до клиента: {text}"
            ),
            EchoVerdict::Safe(text) => panic!("проза с двоеточием не попала в индекс: {text}"),
        }
    }

    /// A quote shorter than the threshold is not a copy: a turn with a short reference is not cut.
    #[test]
    fn test_echo_guard_keeps_short_quote() {
        let mut echo = EchoGuard::new(&echo_chat(), None);
        let text = "Она смеётся и вспоминает: садится у окна и качает ногой, но сегодня по-другому";
        let mut shown = match echo.feed(text) {
            EchoVerdict::Safe(part) => part,
            other => panic!("короткая цитата не копия: {other:?}"),
        };
        let rest = match echo.finish() {
            EchoVerdict::Safe(rest) => rest,
            other => panic!("короткая цитата не копия: {other:?}"),
        };
        shown.push_str(&rest);
        assert_eq!(shown, text);
    }

    /// User messages are a source too: an echo of the player's turn is cut the same way.
    #[test]
    fn test_echo_guard_user_message_is_source() {
        let messages = json!([
            { "role": "user", "content": "Расскажи про сад за окном и старую скамейку под яблоней" },
            { "role": "assistant", "content": "*слушает внимательно*" },
        ]);
        let mut echo = EchoGuard::new(&messages, None);
        assert_eq!(echo.feed("Она кивает и начинает рассказ про "), EchoVerdict::Safe(String::new()));
        let verdict = echo.feed("сад за окном и старую скамейку под яблоней");
        assert!(matches!(verdict, EchoVerdict::Copied(_)), "эхо реплики игрока должно резаться: {verdict:?}");
    }

    /// Reply continuation (`continue`) passes through: it honestly
    /// continues the started text.
    #[test]
    fn test_echo_guard_inactive_passthrough() {
        let mut echo = EchoGuard::inactive();
        let text = "Садится у окна и качает ногой пусть этот вечер продлится";
        assert_eq!(echo.feed(text), EchoVerdict::Safe(text.to_string()));
        assert_eq!(echo.finish(), EchoVerdict::Safe(String::new()));
    }

    /// In a group chat someone else's turn is not a source: repeating others' gestures —
    /// not an echo of the current speaker.
    #[test]
    fn test_echo_guard_ignores_foreign_speaker() {
        let messages = json!([
            { "role": "user", "content": "привет" },
            { "role": "assistant", "content": "Alice: *садится у окна и тихонько качает ногой, глядя на звёзды*" },
        ]);
        let mut echo = EchoGuard::new(&messages, Some("Bella"));
        let text = "*садится у окна и тихонько качает ногой, глядя на звёзды*";
        let mut shown = match echo.feed(text) {
            EchoVerdict::Safe(part) => part,
            other => panic!("чужая реплика не источник: {other:?}"),
        };
        let rest = match echo.finish() {
            EchoVerdict::Safe(rest) => rest,
            other => panic!("чужая реплика не источник: {other:?}"),
        };
        shown.push_str(&rest);
        assert_eq!(shown, text);
    }
}
