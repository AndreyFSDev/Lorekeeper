//! Exporting a trained LoRA into Ollama: converting the adapter to GGUF and creating
//! a `cloufam-<name>` model on top of the same base model from the registry.
//!
//! Merging into the weights is not needed: `ollama create` accepts `ADAPTER`, even onto
//! a quantized base — verified on a live run (see the log, spike M0).

use std::path::PathBuf;
use std::process::Command;

use tauri::AppHandle;

/// File/model name: Ollama accepts ASCII only, so Cyrillic is transliterated —
/// otherwise `ollama create` fails with "invalid model name". The same slug
/// names image datasets and image LoRAs.
pub(crate) fn slug(name: &str) -> String {
    const TABLE: [(char, &str); 33] = [
        ('а', "a"), ('б', "b"), ('в', "v"), ('г', "g"), ('д', "d"), ('е', "e"), ('ё', "e"),
        ('ж', "zh"), ('з', "z"), ('и', "i"), ('й', "y"), ('к', "k"), ('л', "l"), ('м', "m"),
        ('н', "n"), ('о', "o"), ('п', "p"), ('р', "r"), ('с', "s"), ('т', "t"), ('у', "u"),
        ('ф', "f"), ('х', "h"), ('ц', "c"), ('ч', "ch"), ('ш', "sh"), ('щ', "sch"), ('ъ', ""),
        ('ы', "y"), ('ь', ""), ('э', "e"), ('ю', "yu"), ('я', "ya"),
    ];
    let mut out = String::new();
    for c in name.chars() {
        let lower = c.to_lowercase().next().unwrap_or(c);
        if let Some((_, latin)) = TABLE.iter().find(|(cyr, _)| *cyr == lower) {
            out.push_str(latin);
        } else if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(lower);
        } else {
            out.push('-');
        }
    }
    let cleaned = out.trim_matches('-').to_string();
    let cleaned = if cleaned.is_empty() {
        "lora".to_string()
    } else {
        cleaned
    };
    // Windows reserved names (A3-13): `con.safetensors` is a device, the write
    // goes nowhere. The suffix turns the name into a file.
    if is_reserved_file_name(&cleaned) {
        format!("{cleaned}-x")
    } else {
        cleaned
    }
}

/// Windows reserved file names: `CON`, `NUL`, `COM1`… — a write to such a file
/// goes to the device. The base up to the first dot is checked:
/// `con.jsonl` is reserved just like `con` (A3-13).
pub(crate) fn is_reserved_file_name(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or(name).trim_end();
    let upper = base.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$")
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper[3..].starts_with(|c: char| c.is_ascii_digit() && c != '0'))
}

#[cfg(windows)]
fn hide_console(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn hide_console(_cmd: &mut Command) {}

fn run(mut cmd: Command, what: &str) -> Result<String, String> {
    hide_console(&mut cmd);
    let out = cmd.output().map_err(|e| format!("{what}: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if !out.status.success() {
        let tail: String = stderr
            .lines()
            .chain(stdout.lines())
            .rev()
            .take(8)
            .collect::<Vec<_>>()
            .join(" | ");
        return Err(format!("{what} завершилось с ошибкой: {tail}"));
    }
    Ok(stdout + &stderr)
}

/// Modelfile contents for an adapter on top of a base model from the registry.
/// The value path is in triple quotes (A3-10): a path with spaces otherwise
/// breaks Ollama's parsing. A double quote `"` is impossible in Windows paths,
/// so the content cannot escape the triple quotes. [VERIFY <ollama Modelfile>:
/// triple-quote syntax — docs/Modelfile.md, parsing — parser.go]
pub fn modelfile(ollama_tag: &str, adapter_path: &str) -> String {
    format!("FROM {ollama_tag}\nADAPTER \"\"\"{adapter_path}\"\"\"\n")
}

/// Model name in Ollama from what the user typed.
pub fn model_name(user_name: &str) -> String {
    format!("cloufam-{}", slug(user_name))
}

/// The adapter folder, if training saved it. Separate from `export`: the
/// command checks it before it puts the base model up for download.
///
/// The path comes from the client, so it must canonically lie inside the runs
/// folder (A3-10): otherwise `export` would read an arbitrary directory and
/// carry it into the Modelfile.
pub fn checked_adapter_dir(adapter_dir: &str) -> Result<PathBuf, String> {
    let adapter = PathBuf::from(adapter_dir);
    let canonical = adapter
        .canonicalize()
        .map_err(|_| format!("папка адаптера не найдена: {}", adapter.display()))?;
    let root = crate::paths::training_runs_dir()
        .canonicalize()
        .map_err(|e| format!("папка прогонов недоступна: {e}"))?;
    if !canonical.starts_with(&root) {
        return Err(format!(
            "папка адаптера вне папки прогонов: {}",
            adapter.display()
        ));
    }
    if !canonical.join("adapter_config.json").is_file() {
        return Err(format!(
            "в {} нет adapter_config.json — обучение не сохранило адаптер",
            adapter.display()
        ));
    }
    // Outward — the original path without \\?\: python reads it and carries it into the Modelfile.
    Ok(adapter)
}

/// The CLI of the Ollama that serves the app. In dev mode use_system_ollama
/// (it applies only in a debug build, as in `runtime::Runtime::start`) there is
/// no bundle in the program folder — the system installation is used.
fn ollama_cli() -> Result<PathBuf, String> {
    #[cfg(debug_assertions)]
    if crate::settings::Settings::load().use_system_ollama {
        let installed = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|p| p.join("Programs").join("Ollama").join("ollama.exe"))
            .filter(|p| p.exists());
        return Ok(installed.unwrap_or_else(|| PathBuf::from("ollama")));
    }
    let bundled = crate::paths::runtime_dir().join("ollama.exe");
    if bundled.exists() {
        Ok(bundled)
    } else {
        Err("не найден ollama.exe в папке программы".into())
    }
}

/// Convert the adapter to GGUF and create the model in Ollama.
/// Returns the name of the created model. The base model must already be in
/// the store by this point: otherwise `ollama create` pulls it itself, past the
/// registry DNS bypass the app downloads through.
///
/// Entry point: the `training_export` command (`lib.rs`), `spawn_blocking`.
/// `training-progress` events are removed as dead (A3-11) — the UI polls the
/// state with commands.
pub fn export(
    _app: &AppHandle,
    preset_id: &str,
    adapter_dir: &str,
    user_name: &str,
) -> Result<String, String> {
    let preset = crate::trainer::preset_by_id(preset_id)
        .ok_or_else(|| format!("неизвестный пресет: {preset_id}"))?;
    let adapter = checked_adapter_dir(adapter_dir)?;

    let llama_dir = crate::pyruntime::llama_cpp_dir();
    let converter = llama_dir.join("convert_lora_to_gguf.py");
    if !converter.exists() {
        return Err("не найден convert_lora_to_gguf.py: переустановите рантайм дообучения".into());
    }

    let lora_gguf = adapter.join("lora.gguf");
    let mut cmd = Command::new(crate::pyruntime::python_exe());
    cmd.arg(&converter)
        .arg("--outtype")
        .arg("f16")
        .arg("--outfile")
        .arg(&lora_gguf)
        .arg(&adapter)
        .current_dir(&llama_dir);
    crate::pyruntime::training_env(&mut cmd);
    run(cmd, "конвертация LoRA в GGUF")?;
    if !lora_gguf.exists() {
        return Err("конвертер не создал lora.gguf".into());
    }

    let modelfile_path = adapter.join("Modelfile");
    std::fs::write(
        &modelfile_path,
        modelfile(&preset.ollama_tag, &lora_gguf.to_string_lossy()),
    )
    .map_err(|e| e.to_string())?;

    let name = model_name(user_name);
    let ollama = ollama_cli()?;
    let port = crate::gateway::ollama_port();
    let ollama_home = crate::paths::data_dir().join("ollama");
    let mut cmd = Command::new(&ollama);
    cmd.arg("create")
        .arg(&name)
        .arg("-f")
        .arg(&modelfile_path)
        .env("OLLAMA_HOST", format!("127.0.0.1:{port}"))
        .env("OLLAMA_MODELS", crate::paths::models_dir())
        .env("HOME", &ollama_home)
        .env("USERPROFILE", &ollama_home);
    run(cmd, "ollama create")?;

    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_modelfile_uses_quantized_base_and_adapter() {
        let text = modelfile("huihui_ai/qwen3-abliterated:4b-v2", r"Z:\models\lora.gguf");
        assert!(text.starts_with("FROM huihui_ai/qwen3-abliterated:4b-v2\n"));
        // The value path is in triple quotes in the Modelfile (A3-10): a path
        // with spaces otherwise breaks Ollama's parsing.
        assert!(text.contains("ADAPTER \"\"\"Z:\\models\\lora.gguf\"\"\""), "{text}");
    }

    /// A path with spaces and Cyrillic stays one Modelfile value.
    #[test]
    fn test_modelfile_keeps_spaces_in_one_value() {
        let text = modelfile(
            "huihui_ai/qwen3-abliterated:4b-v2",
            r"Z:\Мои модели\run-1\adapter\lora.gguf",
        );
        assert!(text.contains("ADAPTER \"\"\"Z:\\Мои модели\\run-1\\adapter\\lora.gguf\"\"\""), "{text}");
    }

    /// Windows reserved names must not become file names
    /// (A3-13): `con.safetensors` is a device, the write goes nowhere.
    #[test]
    fn test_slug_rejects_reserved_windows_names() {
        assert_eq!(slug("CON"), "con-x");
        assert_eq!(slug("Nul"), "nul-x");
        assert_eq!(slug("com1"), "com1-x");
        assert_eq!(slug("COM10"), "com10", "com10 — обычное имя");
        assert!(is_reserved_file_name("con.jsonl"));
        assert!(!is_reserved_file_name("console.safetensors"));
    }

    /// The adapter folder must canonically lie in the runs folder (A3-10):
    /// a client path from outside is rejected before any work with it.
    #[test]
    fn test_checked_adapter_dir_requires_containment() {
        let run = crate::paths::training_runs_dir().join(format!("a3-containment-{}", std::process::id()));
        let adapter = run.join("adapter");
        std::fs::create_dir_all(&adapter).unwrap();
        std::fs::write(adapter.join("adapter_config.json"), b"{}").unwrap();

        let outside = std::env::temp_dir().join(format!("a3-outside-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("adapter_config.json"), b"{}").unwrap();

        let ok = checked_adapter_dir(&adapter.to_string_lossy());
        let rejected = checked_adapter_dir(&outside.to_string_lossy());
        let escaped = checked_adapter_dir(
            &run.join("..").join(run.file_name().unwrap()).join("adapter").to_string_lossy(),
        );
        let _ = std::fs::remove_dir_all(&run);
        let _ = std::fs::remove_dir_all(&outside);

        assert!(ok.is_ok(), "{ok:?}");
        assert!(rejected.is_err(), "папка вне прогонов должна отклоняться");
        assert!(escaped.is_ok(), "то же место через .. — нормально: {escaped:?}");
    }

    #[test]
    fn test_model_name_is_safe() {
        // Ollama accepts ASCII only: Cyrillic is transliterated.
        assert_eq!(model_name("Мой стиль 2026"), "cloufam-moy-stil-2026");
        assert_eq!(model_name("проверка стиля"), "cloufam-proverka-stilya");
        assert_eq!(model_name("../../evil"), "cloufam-evil");
        assert_eq!(model_name(""), "cloufam-lora");
        assert!(model_name("Мой стиль").is_ascii());
    }
}

