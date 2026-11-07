//! Importing your own texts into a training dataset.
//!
//! Supported: txt/md (split into chunks by paragraphs), jsonl/json (ready-made
//! dialogues `{"messages":[…]}` or texts `{"text":…}`) and csv (pairs
//! `user,assistant` or a `text` column). The result is a single JSONL inside
//! the application folder, suitable for both dialogue and text modes.

use std::io::BufRead;
use std::path::{Path, PathBuf};

const MAX_CHUNK_CHARS: usize = 1800;
/// The trainer keeps text windows no shorter than 64 tokens. A string shorter
/// than 64 characters definitely won't reach the threshold (characters are always no fewer
/// than tokens), so it doesn't count as an example already at import.
const MIN_CHUNK_CHARS: usize = 64;

#[derive(Debug, Clone, serde::Serialize)]
pub struct DatasetInfo {
    pub name: String,
    pub path: String,
    /// chat — message pairs, text — continuous text.
    pub mode: String,
    pub examples: usize,
    pub chars: usize,
    pub approx_tokens: usize,
    pub preview: Vec<String>,
}

fn slug(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let trimmed = cleaned.trim_matches('-').to_string();
    let base = if trimmed.is_empty() {
        "dataset".to_string()
    } else {
        trimmed
    };
    // Windows reserved names (A3-13): `con.jsonl` is a device,
    // writing goes nowhere. A suffix turns the name into a file.
    if crate::text_lora::is_reserved_file_name(&base) {
        format!("{base}-x")
    } else {
        base
    }
}

/// Record kind in the dataset. Dataset mode follows the record kind; a mix of chat and
/// text is rejected (A3-05): during training such records were silently lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKind {
    Chat,
    Text,
}

impl RecordKind {
    fn as_str(self) -> &'static str {
        match self {
            RecordKind::Chat => "chat",
            RecordKind::Text => "text",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "chat" => Ok(RecordKind::Chat),
            "text" => Ok(RecordKind::Text),
            other => Err(format!("неизвестный режим датасета: «{other}»")),
        }
    }
}

/// Dataset meta first line: the mode is written explicitly, not guessed from
/// content (A3-05). Format — JSON with the `__lk_meta` key; it is skipped by
/// both Rust (`shape_of`) and `train_text_lora.py`.
const META_KEY: &str = "__lk_meta";

fn meta_line(kind: RecordKind) -> String {
    serde_json::json!({ META_KEY: { "mode": kind.as_str(), "version": 1 } }).to_string()
}

/// Parse the meta line: `Some(Err)` — meta exists but the mode is unknown.
fn parse_meta(line: &str) -> Option<Result<RecordKind, String>> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let mode = value.get(META_KEY)?.get("mode")?.as_str()?;
    Some(RecordKind::parse(mode))
}

/// Record kind in a JSONL line: an object with a `messages` array — chat, with a `text`
/// string — text.
fn record_kind(line: &str) -> Option<RecordKind> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("messages").is_some_and(|m| m.is_array()) {
        return Some(RecordKind::Chat);
    }
    if value.get("text").is_some_and(|t| t.as_str().is_some()) {
        return Some(RecordKind::Text);
    }
    None
}

/// Dataset shape for training.
#[derive(Debug, Clone)]
pub struct Shape {
    pub examples: usize,
    pub chars: usize,
    pub mode: String,
}

/// Dataset shape in one streaming pass (A3-04): the file is not held in memory,
/// JSON is parsed only for files without a meta line. Mode — from meta; a mix
/// of chat and text yields `mode = "mixed"`, and training rejects such a dataset
/// (A3-05). An empty file and unparsed lines are an error (fail closed).
pub fn shape_of(path: &Path) -> Result<Shape, String> {
    let file = std::fs::File::open(path)
        .map_err(|e| format!("не удалось прочитать {}: {e}", path.display()))?;
    let mut declared: Option<RecordKind> = None;
    let mut seen: Option<RecordKind> = None;
    let mut mixed = false;
    let mut examples = 0usize;
    let mut chars = 0usize;
    for (index, line) in std::io::BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|e| format!("строка {} не прочитана: {e}", index + 1))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if index == 0 {
            if let Some(meta) = parse_meta(trimmed) {
                declared = Some(meta.map_err(|e| format!("мета датасета: {e}"))?);
                continue;
            }
        }
        if declared.is_none() {
            // No meta (files from before the format): the record kind is recognized by content and
            // a mix is rejected rather than losing half the records silently.
            let kind = record_kind(trimmed)
                .ok_or_else(|| format!("строка {}: не запись ни chat, ни text", index + 1))?;
            match seen {
                None => seen = Some(kind),
                Some(previous) if previous != kind => mixed = true,
                Some(_) => {}
            }
        }
        examples += 1;
        chars += trimmed.chars().count();
    }
    let kind = declared
        .or(seen)
        .ok_or_else(|| format!("датасет пуст: {}", path.display()))?;
    let mode = if mixed { "смешанный" } else { kind.as_str() }.to_string();
    Ok(Shape {
        examples,
        chars,
        mode,
    })
}

/// Token count estimate: for Russian and English text ~3.2 characters per token.
fn approx_tokens(chars: usize) -> usize {
    chars / 3
}

fn chat_record(messages: &[(String, String)]) -> Option<String> {
    let usable: Vec<serde_json::Value> = messages
        .iter()
        .filter(|(_, content)| !content.trim().is_empty())
        .map(|(role, content)| serde_json::json!({ "role": role, "content": content.trim() }))
        .collect();
    if usable.len() < 2 {
        return None;
    }
    let has_user = usable
        .iter()
        .any(|m| m["role"].as_str() == Some("user"));
    let has_assistant = usable
        .iter()
        .any(|m| m["role"].as_str() == Some("assistant"));
    if !has_user || !has_assistant {
        return None;
    }
    Some(serde_json::json!({ "messages": usable }).to_string())
}

fn chunks_from_text(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for paragraph in text.split("\n\n") {
        let paragraph = paragraph.trim();
        if paragraph.is_empty() {
            continue;
        }
        if !current.is_empty() && current.chars().count() + paragraph.chars().count() > MAX_CHUNK_CHARS {
            chunks.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push_str("\n\n");
        }
        current.push_str(paragraph);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
        .into_iter()
        .filter(|c| c.trim().chars().count() >= MIN_CHUNK_CHARS)
        .collect()
}

/// csv/tsv parsing without external dependencies: comma, semicolon, or
/// tab (from the header), quotes handled.
fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let header = text.lines().next().unwrap_or_default();
    let count = |delimiter: char| header.matches(delimiter).count();
    let delimiter = if count('\t') > count(',').max(count(';')) {
        '\t'
    } else if count(';') > count(',') {
        ';'
    } else {
        ','
    };
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            c if c == delimiter && !quoted => {
                row.push(std::mem::take(&mut field));
            }
            '\n' if !quoted => {
                row.push(std::mem::take(&mut field));
                if row.iter().any(|f| !f.trim().is_empty()) {
                    rows.push(std::mem::take(&mut row));
                } else {
                    row.clear();
                }
            }
            // \r inside quotes is part of the value (a newline in a field),
            // outside — the CRLF tail that must not get into the cell.
            '\r' if !quoted => {}
            c => field.push(c),
        }
    }
    row.push(field);
    if row.iter().any(|f| !f.trim().is_empty()) {
        rows.push(row);
    }
    rows
}

fn column_index(header: &[String], names: &[&str]) -> Option<usize> {
    header.iter().position(|h| {
        let h = h.trim().to_lowercase();
        names.iter().any(|n| h == *n)
    })
}

fn read_source(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("не удалось прочитать {}: {e}", path.display()))
}

/// Parsing arbitrary JSON into records. Records go to the sink one by one:
/// import writes them to disk as a stream (A3-04), and the record kind decides the fate of the whole
/// dataset (A3-05).
fn build_from_json_value(
    value: &serde_json::Value,
    sink: &mut dyn FnMut(RecordKind, String) -> Result<(), String>,
) -> Result<usize, String> {
    let mut added = 0;
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                added += build_from_json_value(item, sink)?;
            }
        }
        serde_json::Value::Object(obj) => {
            if let Some(messages) = obj.get("messages").and_then(|m| m.as_array()) {
                let pairs: Vec<(String, String)> = messages
                    .iter()
                    .filter_map(|m| {
                        let role = m.get("role")?.as_str()?.to_string();
                        let content = m.get("content")?.as_str()?.to_string();
                        Some((role, content))
                    })
                    .collect();
                if let Some(record) = chat_record(&pairs) {
                    sink(RecordKind::Chat, record)?;
                    return Ok(1);
                }
            }
            if let Some(text) = obj.get("text").and_then(|t| t.as_str()) {
                for chunk in chunks_from_text(text) {
                    sink(RecordKind::Text, serde_json::json!({ "text": chunk }).to_string())?;
                    added += 1;
                }
            }
        }
        serde_json::Value::String(text) => {
            for chunk in chunks_from_text(text) {
                sink(RecordKind::Text, serde_json::json!({ "text": chunk }).to_string())?;
                added += 1;
            }
        }
        _ => {}
    }
    Ok(added)
}

/// Import record sink: writes JSONL to disk one by one (A3-04), fixes the mode
/// with the first record in the meta line (A3-05), rejects a mix of chat and text
/// — during training records of one kind were silently lost.
struct RecordSink {
    out: std::io::BufWriter<std::fs::File>,
    kind: Option<RecordKind>,
    examples: usize,
    chars: usize,
    preview: Vec<String>,
}

impl RecordSink {
    /// Create a new file atomically: `create_new` prevents two concurrent
    /// imports from choosing the same name and overwriting each other's records.
    fn create_new(path: &Path) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        Ok(Self {
            out: std::io::BufWriter::new(file),
            kind: None,
            examples: 0,
            chars: 0,
            preview: Vec::new(),
        })
    }

    fn push(&mut self, kind: RecordKind, record: String) -> Result<(), String> {
        use std::io::Write;
        // Short text lines (JSONL/CSV) were silently dropped by the trainer at
        // the 64-token threshold, while the interface counted them as examples. The filter here
        // covers all sources, not just txt/md chunking.
        if kind == RecordKind::Text {
            let short = serde_json::from_str::<serde_json::Value>(&record)
                .ok()
                .and_then(|value| value["text"].as_str().map(|text| text.chars().count() < MIN_CHUNK_CHARS))
                .unwrap_or(false);
            if short {
                return Ok(());
            }
        }
        match self.kind {
            None => {
                self.kind = Some(kind);
                let meta = meta_line(kind);
                self.out.write_all(meta.as_bytes()).map_err(|e| e.to_string())?;
                self.out.write_all(b"\n").map_err(|e| e.to_string())?;
            }
            Some(previous) if previous != kind => {
                return Err(
                    "смешанный датасет: в одном файле и пары сообщений, и сплошной текст. \
                     Разделите их на два датасета — при обучении записи одного из видов теряются."
                        .into(),
                );
            }
            Some(_) => {}
        }
        self.examples += 1;
        self.chars += record.chars().count();
        if self.preview.len() < 3 {
            self.preview.push(record.chars().take(220).collect());
        }
        self.out.write_all(record.as_bytes()).map_err(|e| e.to_string())?;
        self.out.write_all(b"\n").map_err(|e| e.to_string())?;
        Ok(())
    }

    fn finish(mut self) -> Result<(usize, RecordKind, usize, Vec<String>), String> {
        use std::io::Write;
        self.out.flush().map_err(|e| e.to_string())?;
        let kind = self
            .kind
            .ok_or_else(|| "в файле не нашлось пригодных примеров".to_string())?;
        Ok((self.examples, kind, self.chars, self.preview))
    }
}

/// Import file name: an already finished dataset is not overwritten (A3-06), a new one
/// gets the suffix `-2`, `-3`, … Creation is atomic (`create_new`): checking
/// `exists` + `File::create` allowed two concurrent imports to pick the same
/// name and overwrite each other's records.
fn create_target(dir: &Path, base: &str) -> Result<(RecordSink, PathBuf, String), String> {
    for index in 1..=1000u32 {
        let name = if index == 1 {
            format!("{base}.jsonl")
        } else {
            format!("{base}-{index}.jsonl")
        };
        let path = dir.join(&name);
        match RecordSink::create_new(&path) {
            Ok(sink) => return Ok((sink, path, name)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.to_string()),
        }
    }
    Err("слишком много датасетов с таким именем: переименуйте исходный файл".into())
}

/// Filling the sink with records from the source. An error on any line — reject
/// the whole import: a partial dataset is never usable.
fn fill_records(source: &Path, ext: &str, sink: &mut RecordSink) -> Result<(), String> {
    match ext {
        "txt" | "md" => {
            let text = read_source(source)?;
            for chunk in chunks_from_text(&text) {
                sink.push(RecordKind::Text, serde_json::json!({ "text": chunk }).to_string())?;
            }
            Ok(())
        }
        "jsonl" | "ndjson" => {
            // Line by line (A3-04): the source is not held entirely in memory; the line
            // number in the error is the source line number (A3-06), not a counter of
            // already assembled records.
            let file = std::fs::File::open(source)
                .map_err(|e| format!("не удалось прочитать {}: {e}", source.display()))?;
            for (index, line) in std::io::BufReader::new(file).lines().enumerate() {
                let line = line.map_err(|e| format!("строка {} не прочитана: {e}", index + 1))?;
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let value: serde_json::Value = serde_json::from_str(line)
                    .map_err(|e| format!("строка {} не разобрана: {e}", index + 1))?;
                build_from_json_value(&value, &mut |kind, record| sink.push(kind, record))?;
            }
            Ok(())
        }
        "json" => {
            let text = read_source(source)?;
            let value: serde_json::Value =
                serde_json::from_str(&text).map_err(|e| format!("json не разобран: {e}"))?;
            build_from_json_value(&value, &mut |kind, record| sink.push(kind, record))?;
            Ok(())
        }
        "csv" | "tsv" => {
            let text = read_source(source)?;
            let rows = parse_csv(&text);
            let Some(header) = rows.first() else {
                return Err("csv пустой".into());
            };
            let user_idx = column_index(header, &["user", "instruction", "prompt", "вопрос", "запрос"]);
            let assistant_idx = column_index(header, &["assistant", "output", "response", "answer", "ответ"]);
            let text_idx = column_index(header, &["text", "content", "текст"]);
            let system_idx = column_index(header, &["system", "character", "персонаж"]);
            for row in rows.iter().skip(1) {
                if let (Some(u), Some(a)) = (user_idx, assistant_idx) {
                    let mut pairs: Vec<(String, String)> = Vec::new();
                    if let Some(s) = system_idx.and_then(|i| row.get(i)) {
                        if !s.trim().is_empty() {
                            pairs.push(("system".into(), s.clone()));
                        }
                    }
                    pairs.push(("user".into(), row.get(u).cloned().unwrap_or_default()));
                    pairs.push(("assistant".into(), row.get(a).cloned().unwrap_or_default()));
                    if let Some(record) = chat_record(&pairs) {
                        sink.push(RecordKind::Chat, record)?;
                    }
                } else if let Some(t) = text_idx.and_then(|i| row.get(i)) {
                    for chunk in chunks_from_text(t) {
                        sink.push(RecordKind::Text, serde_json::json!({ "text": chunk }).to_string())?;
                    }
                }
            }
            Ok(())
        }
        other => Err(format!(
            "формат «{other}» не поддерживается: нужен txt, md, jsonl, json или csv"
        )),
    }
}

/// Import a file or folder into a dataset inside the application folder.
/// Returns a description of the finished dataset (JSONL in `data/training/import`).
pub fn import(source: &Path) -> Result<DatasetInfo, String> {
    if !source.exists() {
        return Err(format!("путь не найден: {}", source.display()));
    }
    let ext = source
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();

    let stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("dataset");
    let base = slug(stem);
    let dir = crate::paths::training_import_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // Repeated import does not overwrite an already finished dataset (A3-06).
    let (mut sink, target, name) = create_target(&dir, &base)?;
    if let Err(e) = fill_records(source, &ext, &mut sink) {
        // A partially written file doesn't remain looking like a dataset: all or nothing.
        drop(sink);
        let _ = std::fs::remove_file(&target);
        return Err(e);
    }
    let (examples, kind, chars, preview) = match sink.finish() {
        Ok(finished) => finished,
        Err(e) => {
            let _ = std::fs::remove_file(&target);
            return Err(e);
        }
    };

    Ok(DatasetInfo {
        name,
        path: target.to_string_lossy().to_string(),
        mode: kind.as_str().to_string(),
        examples,
        chars,
        approx_tokens: approx_tokens(chars),
        preview,
    })
}

/// Already imported datasets (for the list in settings). Each one's shape —
/// in one streaming pass (A3-04): the whole file is not read into memory.
pub fn list() -> Vec<DatasetInfo> {
    let dir = crate::paths::training_import_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut result = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        // An empty or unreadable file is not a dataset; in training it would be
        // rejected anyway (fail closed), and in the list it would only confuse.
        let Ok(shape) = shape_of(&path) else {
            continue;
        };
        if shape.examples == 0 {
            continue;
        }
        result.push(DatasetInfo {
            name: path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string(),
            path: path.to_string_lossy().to_string(),
            mode: shape.mode,
            examples: shape.examples,
            chars: shape.chars,
            approx_tokens: approx_tokens(shape.chars),
            preview: Vec::new(),
        });
    }
    result.sort_by(|a, b| a.name.cmp(&b.name));
    result
}

/// A bare file name without separators and `..` — otherwise resolve would open
/// files outside the import folder (`..\..\data\settings.json`, `C:\...`).
fn is_bare_file_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(['/', '\\', ':'])
        && !name.contains("..")
        && Path::new(name).file_name().and_then(|n| n.to_str()) == Some(name)
}

/// Dataset path by file name from the import folder.
pub fn resolve(name_or_path: &str) -> Result<PathBuf, String> {
    if !is_bare_file_name(name_or_path) {
        return Err(format!("датасет не найден: {name_or_path}"));
    }
    let candidate = crate::paths::training_import_dir().join(name_or_path);
    let canonical = candidate
        .canonicalize()
        .map_err(|_| format!("датасет не найден: {name_or_path}"))?;
    let dir = crate::paths::training_import_dir()
        .canonicalize()
        .map_err(|_| format!("датасет не найден: {name_or_path}"))?;
    // The canonical path must stay inside the import folder: a symlink/junction
    // pointing outside is also discarded.
    if !canonical.starts_with(&dir) || !canonical.is_file() {
        return Err(format!("датасет не найден: {name_or_path}"));
    }
    // Outward — a regular path without the \\?\ prefix and with the drive's name casing: by it
    // the trainer finds the dataset in list(), and it also goes into the python config.
    let file_name = canonical
        .file_name()
        .ok_or_else(|| format!("датасет не найден: {name_or_path}"))?;
    Ok(crate::paths::training_import_dir().join(file_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lk-dataset-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_text_file_is_chunked() {
        let dir = temp_dir("txt");
        let text = "Первый абзац достаточно длинный, чтобы попасть в датасет целиком.\n\n\
                    Второй абзац тоже длинный и содержит продолжение истории персонажа.";
        let file = dir.join("notes.txt");
        std::fs::write(&file, text).unwrap();
        let chunk_dir = dir.join("import");
        std::fs::create_dir_all(&chunk_dir).unwrap();
        // import() writes to the application folder, so we check parsing only.
        let chunks = chunks_from_text(text);
        assert!(!chunks.is_empty());
        assert!(chunks[0].contains("Первый абзац"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_csv_pairs_become_chat_records() {
        let csv = "user,assistant\nПривет!,Здравствуй, путник.\nКак дела?,Лучше всех.\n";
        let rows = parse_csv(csv);
        assert_eq!(rows.len(), 3);
        let header = &rows[0];
        let u = column_index(header, &["user"]).unwrap();
        let a = column_index(header, &["assistant"]).unwrap();
        let record = chat_record(&[
            ("user".into(), rows[1][u].clone()),
            ("assistant".into(), rows[1][a].clone()),
        ])
        .unwrap();
        assert!(record.contains("\"messages\""));
        assert!(record.contains("Здравствуй"));
    }

    /// Collect records into a vector — like the import sink, but without disk.
    fn collect(
        value: &serde_json::Value,
    ) -> Result<(usize, Vec<(RecordKind, String)>), String> {
        let mut out: Vec<(RecordKind, String)> = Vec::new();
        let added = build_from_json_value(value, &mut |kind, record| {
            out.push((kind, record));
            Ok(())
        })?;
        Ok((added, out))
    }

    #[test]
    fn test_json_messages_array() {
        let value: serde_json::Value = serde_json::from_str(
            r#"[{"messages":[{"role":"user","content":"Кто ты?"},{"role":"assistant","content":"Я архивариус."}]}]"#,
        )
        .unwrap();
        let (added, out) = collect(&value).unwrap();
        assert_eq!(added, 1);
        assert_eq!(out[0].0, RecordKind::Chat);
        assert!(out[0].1.contains("архивариус"));
    }

    #[test]
    fn test_jsonl_with_text_lines() {
        let value: serde_json::Value =
            serde_json::from_str(r#"{"text":"Длинный кусок собственного текста для обучения стилю: несколько предложений подряд, чтобы запись прошла порог тренера."}"#).unwrap();
        let (added, out) = collect(&value).unwrap();
        assert_eq!(added, 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, RecordKind::Text);
        assert!(out[0].1.contains("\"text\""));
    }

    #[test]
    fn test_short_and_broken_input_rejected() {
        assert!(chunks_from_text("слишком коротко").is_empty());
        let value: serde_json::Value = serde_json::from_str(r#"{"unknown":1}"#).unwrap();
        let (added, out) = collect(&value).unwrap();
        assert_eq!(added, 0);
        assert!(out.is_empty());
    }

    /// Text containing the `"messages"` substring must not count as chat: the mode
    /// is determined when building records, not by searching the string.
    #[test]
    fn test_mode_ignores_messages_substring_in_text() {
        let value: serde_json::Value = serde_json::from_str(
            r#"{"text":"В этом тексте есть фраза \"messages\" и больше ничего, зато он достаточно длинный, чтобы считаться примером обучения."}"#,
        )
        .unwrap();
        let (added, out) = collect(&value).unwrap();
        assert_eq!(added, out.len());
        assert!(!out.is_empty());
        assert!(out.iter().all(|(kind, _)| *kind == RecordKind::Text), "текстовая запись с подстрокой messages — не чат");
    }

    #[test]
    fn test_csv_preserves_cr_inside_quotes() {
        let csv = "user,assistant\n\"строка\rвнутри\",второй\n";
        let rows = parse_csv(csv);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1][0], "строка\rвнутри", "\\r внутри кавычек — часть значения");
    }

    #[test]
    fn test_resolve_rejects_path_traversal() {
        assert!(resolve("..\\..\\data\\settings.json").is_err());
        assert!(resolve("../data/settings.json").is_err());
        assert!(resolve("C:\\Windows\\system32\\config").is_err());
        assert!(resolve("/etc/passwd").is_err());
        assert!(resolve("..").is_err());
        assert!(resolve("").is_err());
        assert!(resolve("sub/dir.jsonl").is_err());
        assert!(resolve("sub\\dir.jsonl").is_err());
    }

    #[test]
    fn test_resolve_accepts_bare_name_inside_import_dir() {
        let dir = crate::paths::training_import_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("resolve-probe.jsonl");
        std::fs::write(&file, "{\"text\":\"probe\"}").unwrap();
        let resolved = resolve("resolve-probe.jsonl").expect("голое имя внутри папки импорта");
        assert!(resolved.is_file());
        let _ = std::fs::remove_file(&file);
    }

    /// Regression 22.09: resolve returned a \\?\ path, and the trainer didn't find the dataset in
    /// list() — the text dataset went to python in chat mode.
    #[test]
    fn test_resolve_matches_list_entry() {
        let dir = crate::paths::training_import_dir();
        std::fs::create_dir_all(&dir).unwrap();
        // Name with pid: the same test in a parallel cargo test process won't
        // delete the file from under this one.
        let name = format!("resolve-list-probe-{}.jsonl", std::process::id());
        let file = dir.join(&name);
        std::fs::write(&file, "{\"text\":\"probe\"}").unwrap();
        let resolved = resolve(&name).unwrap();
        let listed = list()
            .into_iter()
            .find(|d| d.name == name)
            .expect("датасет в списке");
        let _ = std::fs::remove_file(&file);
        assert!(!resolved.to_string_lossy().starts_with(r"\\?\"), "{}", resolved.display());
        assert_eq!(listed.path, resolved.to_string_lossy());
        assert_eq!(listed.mode, "text");
    }

    #[test]
    fn test_tsv_uses_tab_delimiter() {
        let rows = parse_csv("user\tassistant\nПривет, путник\tЗдравствуй; садись\n");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1], vec!["Привет, путник".to_string(), "Здравствуй; садись".to_string()]);
        // A regular csv is still split by comma.
        assert_eq!(parse_csv("user,assistant\nа,б\n")[1], vec!["а".to_string(), "б".to_string()]);
    }

    #[test]
    fn test_slug_is_safe_for_file_names() {
        assert_eq!(slug("Мои тексты 2026"), "Мои-тексты-2026");
        assert_eq!(slug("../../etc/passwd"), "etc-passwd");
        assert_eq!(slug("///"), "dataset");
    }

    /// Windows reserved names are devices, not files (A3-13):
    /// `con.jsonl` would send writing nowhere.
    #[test]
    fn test_slug_rejects_reserved_windows_names() {
        assert_eq!(slug("CON"), "CON-x");
        assert_eq!(slug("nul"), "nul-x");
        assert_eq!(slug("com1"), "com1-x");
        assert_eq!(slug("Lpt9"), "Lpt9-x");
        assert_eq!(slug("com10"), "com10", "com10 — обычное имя");
        assert_eq!(slug("console"), "console", "console — обычное имя");
    }

    fn write_dataset(tag: &str, body: &str) -> PathBuf {
        let dir = crate::paths::training_import_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("a3-{tag}-{}.jsonl", std::process::id()));
        std::fs::write(&path, body).unwrap();
        path
    }

    /// Meta line is the authoritative mode (A3-05); examples are counted without meta.
    #[test]
    fn test_shape_reads_meta_and_counts_examples() {
        let path = write_dataset(
            "meta",
            "{\"__lk_meta\":{\"mode\":\"chat\",\"version\":1}}\n\
             {\"messages\":[{\"role\":\"user\",\"content\":\"а\"},{\"role\":\"assistant\",\"content\":\"б\"}]}\n\
             {\"messages\":[{\"role\":\"user\",\"content\":\"в\"},{\"role\":\"assistant\",\"content\":\"г\"}]}\n",
        );
        let shape = shape_of(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(shape.mode, "chat");
        assert_eq!(shape.examples, 2);
    }

    /// A file without meta (before the format): a mix of chat and text is visible and rejected
    /// with the "mixed" string, not a silent loss of records (A3-05).
    #[test]
    fn test_shape_marks_legacy_mixed_dataset() {
        let path = write_dataset(
            "mixed",
            "{\"text\":\"достаточно длинный кусок текста для записи в датасет обучения\"}\n\
             {\"messages\":[{\"role\":\"user\",\"content\":\"а\"},{\"role\":\"assistant\",\"content\":\"б\"}]}\n",
        );
        let shape = shape_of(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(shape.mode, "смешанный", "{:?}", shape);
        assert_eq!(shape.examples, 2);
    }

    /// Fail closed (A3-05): a garbage line is a rejection with the line number, not
    /// a silent loss of a record.
    #[test]
    fn test_shape_fails_closed_on_garbage_line() {
        let path = write_dataset("garbage", "{\"text\":\"раз\"}\nэто не json\n");
        let error = shape_of(&path).unwrap_err();
        let _ = std::fs::remove_file(&path);
        assert!(error.contains("строка 2"), "{error}");
    }

    /// A mix is rejected entirely at import, the partially written file is deleted (A3-05).
    #[test]
    fn test_import_rejects_mixed_dataset_and_cleans_up() {
        let dir = std::env::temp_dir().join(format!("a3-import-mixed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("a3-mixed-source.jsonl");
        std::fs::write(
            &source,
            "{\"messages\":[{\"role\":\"user\",\"content\":\"а\"},{\"role\":\"assistant\",\"content\":\"б\"}]}\n\
             {\"text\":\"достаточно длинный кусок текста для записи в датасет обучения: он длиннее шестидесяти четырёх символов\"}\n",
        )
        .unwrap();

        let error = import(&source).unwrap_err();
        let target = crate::paths::training_import_dir().join("a3-mixed-source.jsonl");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&target);
        assert!(error.contains("смешанный"), "{error}");
        assert!(!target.exists(), "недописанный датасет обязан удалиться");
    }

    /// The line number in the error is the source line number (A3-06), not a counter of
    /// already assembled records.
    #[test]
    fn test_import_reports_source_line_numbers() {
        let dir = std::env::temp_dir().join(format!("a3-import-lines-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join(format!("a3-lines-{}.jsonl", std::process::id()));
        std::fs::write(
            &source,
            "{\"text\":\"первый длинный кусок текста для датасета обучения\"}\n\n\
             {\"text\":\"второй длинный кусок текста для датасета обучения\"}\n\
             это не json\n",
        )
        .unwrap();

        let error = import(&source).unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(error.contains("строка 4"), "{error}");
    }

    /// Repeated import does not overwrite a finished dataset (A3-06): the new one gets
    /// a suffix, the old file remains byte for byte.
    #[test]
    fn test_reimport_does_not_wipe_the_previous_dataset() {
        let dir = std::env::temp_dir().join(format!("a3-import-re-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join(format!("a3-reimport-{}.txt", std::process::id()));
        std::fs::write(
            &source,
            "Первый абзац достаточно длинный, чтобы попасть в датасет целиком без обрезки.",
        )
        .unwrap();

        let first = import(&source).expect("первый импорт");
        let second = import(&source).expect("повторный импорт");
        let first_path = std::path::PathBuf::from(&first.path);
        let second_path = std::path::PathBuf::from(&second.path);
        let first_body = std::fs::read_to_string(&first_path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&first_path);
        let _ = std::fs::remove_file(&second_path);

        assert_ne!(first.name, second.name, "имена должны разойтись: {first:?} {second:?}");
        assert!(second.name.ends_with("-2.jsonl"), "{}", second.name);
        assert!(first_body.contains("Первый абзац"), "первый датасет не переписан");
    }
}
