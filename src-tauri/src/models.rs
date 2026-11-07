use std::time::Duration;

use crate::downloads::{FlagsRef, ProgressFn, PAUSED};
use crate::ollama_client::OllamaClient;

/// How many times we try Ollama's own client before downloading from the registry
/// ourselves. A DNS failure answers instantly, so three attempts cost a few seconds.
const PULL_ATTEMPTS: u32 = 3;

/// The download stream closed, yet the model is not in the store: the server
/// restarted or dropped the connection. The error is network — fixed by a retry and a registry bypass.
pub const INCOMPLETE_PULL: &str = "поток скачивания закрылся, модель не докачана";

/// Move the model stores to the warehouse from settings — once, on
/// update. Returns the list of performed actions for the download log:
/// on one volume the move is instant (rename), between volumes — a copy.
pub fn migrate_to_shared_root() -> Result<Vec<String>, String> {
    let root = crate::paths::models_root();
    // The warehouse is the program's `models` folder: the old Ollama store is its
    // parent there, so it cannot be moved into itself entirely.
    let nested = root == crate::paths::base_dir().join("models");
    let mut done = adopt_nested_legacy_store()?;

    let legacy_ollama = crate::paths::legacy_models_dir();
    let ollama = crate::paths::models_dir();
    if !nested && legacy_ollama.exists() && legacy_ollama != ollama {
        if let Some(parent) = ollama.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        if !ollama.exists() {
            move_dir(&legacy_ollama, &ollama)?;
            done.push(format!("модели Ollama → {}", ollama.display()));
        } else {
            // The store on the warehouse already exists (e.g. Ollama managed to
            // start earlier): move manifests and blobs, skip duplicates,
            // remove unfinished files.
            let (moved, conflicts) = merge_dir(&legacy_ollama, &ollama)?;
            if moved > 0 {
                done.push(format!(
                    "модели Ollama: добавлено {moved} объектов в {}",
                    ollama.display()
                ));
            }
            cleanup_merge_source(&legacy_ollama, &conflicts, "модели Ollama", &mut done);
        }
        remove_partial_blobs(&ollama);
    }

    let legacy_hf = crate::paths::data_dir().join("hf-cache");
    let hf = crate::paths::hf_cache_dir();
    if legacy_hf.exists() && legacy_hf != hf {
        if let Some(parent) = hf.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        if !hf.exists() {
            move_dir(&legacy_hf, &hf)?;
            done.push(format!("кэш Hugging Face → {}", hf.display()));
        } else {
            // The warehouse already has a foreign HF cache (e.g. from LM Studio):
            // merge the contents, leave identical files alone.
            let (moved, conflicts) = merge_dir(&legacy_hf, &hf)?;
            if moved > 0 {
                done.push(format!("кэш Hugging Face: добавлено {moved} объектов в {}", hf.display()));
            }
            cleanup_merge_source(&legacy_hf, &conflicts, "кэш Hugging Face", &mut done);
        }
    }

    Ok(done)
}

/// Ollama store before 1.2.3 without a warehouse: `blobs` and `manifests` lay right in
/// `<base>\models`, while Ollama now reads `<base>\models\ollama` — the old models are
/// invisible to it and would be downloaded again. Safe to call on every start: with no
/// old folders it does nothing, and with a configured warehouse `migrate_to_shared_root`
/// moves them.
pub fn adopt_nested_legacy_store() -> Result<Vec<String>, String> {
    let root = crate::paths::models_root();
    let mut done = Vec::new();
    // A bare Ollama store in the root is used in place (`ollama_store_in`):
    // it must not be moved into itself — the merge would delete the source.
    let store = crate::paths::models_dir();
    if root == crate::paths::base_dir().join("models") && store != root {
        adopt_nested_store(&root, &store, &mut done)?;
    }
    Ok(done)
}

/// Move into the `store` only the Ollama folders from its parent `legacy`:
/// the neighbours in the same root (ComfyUI, Hugging Face) do not belong to Ollama.
fn adopt_nested_store(
    legacy: &std::path::Path,
    store: &std::path::Path,
    done: &mut Vec<String>,
) -> Result<(), String> {
    for part in ["blobs", "manifests"] {
        let from = legacy.join(part);
        if !from.is_dir() {
            continue;
        }
        let to = store.join(part);
        if to.exists() {
            let (moved, conflicts) = merge_dir(&from, &to)?;
            if moved > 0 {
                done.push(format!("модели Ollama: добавлено {moved} объектов в {}", to.display()));
            }
            cleanup_merge_source(&from, &conflicts, "модели Ollama", done);
        } else {
            std::fs::create_dir_all(store).map_err(|e| e.to_string())?;
            move_dir(&from, &to)?;
            done.push(format!("модели Ollama → {}", to.display()));
        }
    }
    Ok(())
}

/// Remove unfinished blobs (`*-partial*`): after the finished blobs are moved they
/// only take up space.
fn remove_partial_blobs(store: &std::path::Path) {
    let blobs = store.join("blobs");
    let Ok(entries) = std::fs::read_dir(&blobs) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().contains("-partial") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Remove the source after a merge: only when there are no conflicts. Conflicting
/// files (same name, different content) stay in the source — `remove_dir_all`
/// would destroy the only copy.
fn cleanup_merge_source(
    legacy: &std::path::Path,
    conflicts: &[String],
    label: &str,
    done: &mut Vec<String>,
) {
    if conflicts.is_empty() {
        let _ = std::fs::remove_dir_all(legacy);
    } else {
        done.push(format!(
            "{label}: {} объектов с другим содержимым оставлены в {}",
            conflicts.len(),
            legacy.display()
        ));
    }
}

/// Merge a directory's contents into an existing one: free names are moved,
/// same-content matches are duplicates and their source is removed; conflicts
/// stay in the source. Ollama manifests and HF refs are not content-addressed.
/// Returns the number of moved entries and the names of conflicts.
fn merge_dir(
    from: &std::path::Path,
    to: &std::path::Path,
) -> Result<(usize, Vec<String>), String> {
    merge_dir_with(from, to, true)
}

/// `deduplicate` allows removing only byte-identical files.
/// ComfyUI saves any taken names next to the original directory.
fn merge_dir_with(
    from: &std::path::Path,
    to: &std::path::Path,
    deduplicate: bool,
) -> Result<(usize, Vec<String>), String> {
    let mut moved = 0;
    let mut conflicts = Vec::new();
    for entry in std::fs::read_dir(from).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if dst.exists() {
            if src.is_dir() && dst.is_dir() {
                let (m, c) = merge_dir_with(&src, &dst, deduplicate)?;
                moved += m;
                conflicts.extend(c);
            } else if deduplicate
                && src.is_file()
                && dst.is_file()
                && same_content(&src, &dst)?
            {
                std::fs::remove_file(&src).map_err(|e| e.to_string())?;
            } else if deduplicate && truncated_blob(&src, &dst) {
                // A blob under a digest name shorter than the source is a leftover of an interrupted move,
                // not another file: replace it with the full one, otherwise the warehouse keeps a broken layer forever.
                copy_file_atomic(&src, &dst)?;
                let _ = std::fs::remove_file(&src);
                moved += 1;
            } else {
                conflicts.push(src.display().to_string());
            }
            continue;
        }
        if src.is_dir() {
            move_dir(&src, &dst)?;
        } else {
            if std::fs::rename(&src, &dst).is_err() {
                copy_file_atomic(&src, &dst)?;
            }
            // copy leaves the source — remove it right away so no
            // remove_dir_all is needed outside for the moved one.
            let _ = std::fs::remove_file(&src);
        }
        moved += 1;
    }
    Ok((moved, conflicts))
}

/// Move a directory: first try a rename (one volume), then a copy.
fn move_dir(from: &std::path::Path, to: &std::path::Path) -> Result<(), String> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    copy_dir(from, to)?;
    std::fs::remove_dir_all(from).map_err(|e| format!("не удалось удалить {}: {e}", from.display()))
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| e.to_string())?;
    for entry in std::fs::read_dir(from).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if src.is_dir() {
            copy_dir(&src, &dst)?;
        } else if dst.exists() {
            if !dst.is_file() || !files_equal(&src, &dst)? {
                return Err(format!("{}: файл назначения отличается — источник сохранён", dst.display()));
            }
        } else {
            copy_file_atomic(&src, &dst)?;
        }
    }
    Ok(())
}

/// Cross-volume copy via `<name>.part` + rename: an interruption (no space, exit, power)
/// does not leave a truncated file under the final name that would later be taken for complete.
fn copy_file_atomic(src: &std::path::Path, dst: &std::path::Path) -> Result<(), String> {
    let mut name = dst.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    let part = dst.with_file_name(name);
    let result = std::fs::copy(src, &part)
        .and_then(|_| std::fs::OpenOptions::new().write(true).open(&part)?.sync_all())
        .and_then(|_| std::fs::rename(&part, dst));
    if let Err(e) = result {
        let _ = std::fs::remove_file(&part);
        return Err(format!("{}: {e}", src.display()));
    }
    Ok(())
}

/// Content-addressed name (`sha256-…`, 40/64 hex) and a destination shorter than the source.
fn truncated_blob(src: &std::path::Path, dst: &std::path::Path) -> bool {
    let name = src.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let hex = name.strip_prefix("sha256-").unwrap_or(&name);
    let hashed = matches!(hex.len(), 40 | 64) && hex.bytes().all(|b| b.is_ascii_hexdigit());
    let len = |p: &std::path::Path| std::fs::metadata(p).map(|m| m.len()).ok();
    hashed && matches!((len(src), len(dst)), (Some(s), Some(d)) if d < s)
}

/// Ollama and HF blob names are content hashes: with equal size it is the same
/// file, while a byte-by-byte check of gigabyte weights would drag the move out for minutes.
fn same_content(from: &std::path::Path, to: &std::path::Path) -> Result<bool, String> {
    let name = from.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let hex = name.strip_prefix("sha256-").unwrap_or(&name);
    let hashed = matches!(hex.len(), 40 | 64) && hex.bytes().all(|b| b.is_ascii_hexdigit());
    if hashed {
        let size = |p: &std::path::Path| std::fs::metadata(p).map(|m| m.len()).map_err(|e| format!("{}: {e}", p.display()));
        return Ok(size(from)? == size(to)?);
    }
    files_equal(from, to)
}

/// Blocking move: byte verification before removing the source. Manifests
/// of the same size are not duplicates.
fn files_equal(from: &std::path::Path, to: &std::path::Path) -> Result<bool, String> {
    use std::io::Read;
    let mut left = std::fs::File::open(from).map_err(|e| format!("{}: {e}", from.display()))?;
    let mut right = std::fs::File::open(to).map_err(|e| format!("{}: {e}", to.display()))?;
    let size = left.metadata().map_err(|e| e.to_string())?.len();
    if size != right.metadata().map_err(|e| e.to_string())?.len() {
        return Ok(false);
    }
    let mut left_buf = [0u8; 64 * 1024];
    let mut right_buf = [0u8; 64 * 1024];
    let mut remaining = size;
    while remaining > 0 {
        let count = remaining.min(left_buf.len() as u64) as usize;
        left.read_exact(&mut left_buf[..count]).map_err(|e| e.to_string())?;
        right.read_exact(&mut right_buf[..count]).map_err(|e| e.to_string())?;
        if left_buf[..count] != right_buf[..count] {
            return Ok(false);
        }
        remaining -= count as u64;
    }
    Ok(true)
}

/// Attach ComfyUI's models folder to the shared warehouse via a junction: the bundled
/// ComfyUI and an external one (D:\Tools\ComfyUI) see one set of files, and nodes with a
/// hardcoded `models/...` path keep working.
#[cfg(windows)]
pub fn link_comfy_models() -> Result<String, String> {
    let comfy = crate::paths::comfyui_dir();
    if !comfy.exists() {
        return Ok(String::new());
    }
    // The junction is placed in the `models` the installation itself reads — next to
    // its `main.py`. In the portable layout that is `<ComfyUI_windows_portable>\ComfyUI\models`,
    // in an unpacked one without the top level — `<app folder>\comfyui\models`.
    // Missing the path left ComfyUI without the warehouse: checkpoints lay nearby,
    // but the server did not see them.
    let link = crate::comfyui::get_comfy_main_dir()
        .map(|main| main.join("models"))
        .unwrap_or_else(|| comfy.join("models"));
    let target = crate::paths::comfyui_models_dir();
    std::fs::create_dir_all(&target).map_err(|e| e.to_string())?;

    let mut kept = String::new();
    if let Ok(meta) = std::fs::symlink_metadata(&link) {
        if meta.file_type().is_symlink() {
            return Ok(String::new());
        }
        let has_files = std::fs::read_dir(&link)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false);
        if has_files {
            // A merge, not a copy that skips taken names: an own checkpoint with the
            // same name as on the warehouse would otherwise vanish with the deleted folder.
            // The space for the junction must be freed, so conflicts go nearby.
            let (_, conflicts) = merge_dir_with(&link, &target, false)?;
            if !conflicts.is_empty() {
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let aside = link.with_file_name(format!("models.conflicts-{stamp}"));
                std::fs::rename(&link, &aside).map_err(|e| e.to_string())?;
                kept = format!(
                    "; {} файлов с другим содержимым оставлены в {}",
                    conflicts.len(),
                    aside.display()
                );
            }
        }
        if link.exists() {
            std::fs::remove_dir_all(&link).map_err(|e| e.to_string())?;
        }
    }

    let mut cmd = std::process::Command::new("cmd");
    cmd.args([
        "/C",
        "mklink",
        "/J",
        &link.to_string_lossy(),
        &target.to_string_lossy(),
    ]);
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "mklink /J: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(format!("модели ComfyUI → {}{kept}", target.display()))
}

#[cfg(not(windows))]
pub fn link_comfy_models() -> Result<String, String> {
    Ok(String::new())
}

/// Estimate of the free space needed for a specific model (size + layer buffer).
pub fn min_space_for_model(model: &str) -> u64 {
    let lower = model.to_lowercase();
    // The size is searched in the model name on a boundary: "1.7b" contains "7b" as a substring
    // and without a separator would demand 8 GB instead of ~1 GB.
    fn has_size(name: &str, size: &str) -> bool {
        name.match_indices(size).any(|(i, _)| {
            let before_ok = i == 0
                || !name[..i]
                    .chars()
                    .next_back()
                    .map(|c| c.is_ascii_digit() || c == '.')
                    .unwrap_or(false);
            let end = i + size.len();
            let after_ok = end == name.len()
                || !name[end..]
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_digit())
                    .unwrap_or(false);
            before_ok && after_ok
        })
    }
    if has_size(&lower, "70b") || has_size(&lower, "72b") {
        50 * 1024 * 1024 * 1024
    } else if has_size(&lower, "30b") || has_size(&lower, "32b") {
        25 * 1024 * 1024 * 1024
    } else if has_size(&lower, "14b") {
        10 * 1024 * 1024 * 1024
    } else if has_size(&lower, "9b") {
        8 * 1024 * 1024 * 1024
    } else if has_size(&lower, "8b") || has_size(&lower, "7b") {
        6 * 1024 * 1024 * 1024
    } else if has_size(&lower, "0.8b") || has_size(&lower, "0.6b") {
        2 * 1024 * 1024 * 1024
    } else if has_size(&lower, "4b") {
        3 * 1024 * 1024 * 1024
    } else if has_size(&lower, "2b") || has_size(&lower, "1.7b") {
        2 * 1024 * 1024 * 1024
    } else if lower.contains("embed") || lower.contains("nomic") || lower.contains("bge") {
        1 * 1024 * 1024 * 1024
    } else {
        4 * 1024 * 1024 * 1024
    }
}

/// Remove unfinished files of past failures: `*-partial*` in the Ollama store and
/// `.download` of image checkpoints. Fresh ones are left alone: another process may
/// still be finishing them (including a system Ollama on the same warehouse).
/// Returns the freed bytes — they go to `data/boot.log`.
pub fn cleanup_stale_partials() -> u64 {
    // A week, not a day: a paused model download is a resume point for
    // several gigabytes, and a user who came back a day later would
    // start it over.
    const STALE_AFTER: Duration = Duration::from_secs(7 * 24 * 3600);
    let mut freed = 0;
    let targets: [(std::path::PathBuf, fn(&str) -> bool); 2] = [
        (
            crate::paths::models_dir().join("blobs"),
            // `.part` — an interrupted warehouse move (copy_file_atomic)
            |name| name.contains("-partial") || name.ends_with(".part"),
        ),
        (
            crate::comfyui::checkpoints_dir(),
            // Next to the stub lies its full size (`<file>.download.total`) —
            // without it the cleanup would leave junk files in the checkpoints folder.
            |name| name.ends_with(".download") || name.ends_with(".download.total") || name.ends_with(".download.complete"),
        ),
    ];
    for (dir, is_partial) in targets {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !is_partial(&name) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            // CopyFile carries over the source mtime: a fresh `.part` copy would look old
            let stamp = if name.ends_with(".part") { meta.created() } else { meta.modified() };
            let old_enough = stamp
                .ok()
                .and_then(|time| time.elapsed().ok())
                .is_some_and(|age| age > STALE_AFTER);
            if old_enough && std::fs::remove_file(entry.path()).is_ok() {
                freed += meta.len();
            }
        }
    }
    freed
}

fn normalize_model_name(name: &str) -> &str {
    let mut s = name.trim();
    if let Some(stripped) = s.strip_prefix("registry.ollama.ai/") {
        s = stripped;
    }
    if let Some(stripped) = s.strip_prefix("library/") {
        s = stripped;
    }
    s
}

pub fn is_model_installed(installed: &[String], target: &str) -> bool {
    let target = normalize_model_name(target);
    if target.is_empty() {
        return false;
    }
    installed.iter().any(|m| {
        let m = normalize_model_name(m);
        if m.eq_ignore_ascii_case(target) {
            return true;
        }
        if !target.contains(':') {
            let with_latest = format!("{target}:latest");
            if m.eq_ignore_ascii_case(&with_latest) {
                return true;
            }
        }
        if let Some(target_base) = target.strip_suffix(":latest") {
            if m.eq_ignore_ascii_case(target_base) {
                return true;
            }
        }
        false
    })
}

/// Why the download failed. The class selects the treatment: a retry, a bypass of an
/// unreachable registry, or a clear text instead of a raw Go error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullFailure {
    /// The registry name does not resolve: provider DNS, VPN, antivirus.
    Dns,
    /// The connection is not established or drops.
    Network,
    /// The registry answered that the model or tag does not exist.
    NotFound,
    /// Access to the repository is closed.
    Forbidden,
    /// Not enough disk space.
    Disk,
    Other,
}

pub fn classify_pull_error(error: &str) -> PullFailure {
    let text = error.to_ascii_lowercase();
    // Go's DNS error is always inside `dial tcp: lookup <host>…` — a bare "lookup "
    // would also catch non-network texts.
    if text.contains("no such host")
        || text.contains("dial tcp: lookup ")
        || text.contains("name resolution")
        || text.contains("сервер dns")
    {
        return PullFailure::Dns;
    }
    if text.contains("no space left") || text.contains("not enough space") || text.contains("disk full")
    {
        return PullFailure::Disk;
    }
    if text.contains("http 404")
        || text.contains("http 400")
        || text.contains("manifest unknown")
        || text.contains("invalid name")
        || text.contains("file does not exist")
        || text.contains("not found")
    {
        return PullFailure::NotFound;
    }
    // A bare "denied" caught Windows' "Access is denied." (antivirus holds the blob):
    // that is not registry authorization but a file on disk.
    if text.contains("http 401")
        || text.contains("http 403")
        || text.contains("unauthorized")
        || text.contains("access denied")
    {
        return PullFailure::Forbidden;
    }
    if text.contains("timeout")
        || text.contains("таймаут")
        || text.contains("connection refused")
        || text.contains("connection reset")
        || text.contains("broken pipe")
        || text.contains("unexpected eof")
        || text.contains("не докачана")
        || text.contains("tls:")
        || text.contains("tls handshake")
        || text.contains("x509:")
    {
        return PullFailure::Network;
    }
    PullFailure::Other
}

/// Error text for the user: raw Ollama messages ("dial tcp: lookup …")
/// explain nothing, while their cause is always one of a few.
pub fn human_pull_error(model: &str, error: &str) -> String {
    match classify_pull_error(error) {
        PullFailure::Dns => format!(
            "Реестр моделей Ollama (registry.ollama.ai) не разрешается по DNS. Обычно это VPN, \
             антивирус или DNS провайдера. Модель {model} скачать не удалось — проверьте сеть и \
             нажмите «Повторить»."
        ),
        PullFailure::Network => format!(
            "Нет устойчивой связи с реестром моделей Ollama: {error}. Проверьте интернет и повторите."
        ),
        PullFailure::NotFound => format!(
            "Модели {model} нет в реестре Ollama: тег удалён или переименован автором."
        ),
        PullFailure::Forbidden => {
            format!("Доступ к модели {model} в реестре Ollama закрыт: нужна авторизация.")
        }
        PullFailure::Disk | PullFailure::Other => error.to_string(),
    }
}

/// Download a model. Progress goes to `on_progress` (the window and the download queue
/// listen to it), the outcome — into the return value: both the queue and the model menu
/// inside SillyTavern judge the state by a queue snapshot, not by a separate failure
/// event (an item removed by cancel before start would not have produced such an event).
pub async fn download(
    client: &OllamaClient,
    model: &str,
    flags: FlagsRef,
    on_progress: ProgressFn,
) -> Result<(), String> {
    download_inner(client, model, flags, on_progress).await
}

async fn download_inner(
    client: &OllamaClient,
    model: &str,
    flags: FlagsRef,
    on_progress: ProgressFn,
) -> Result<(), String> {
    let installed_list = installed(client).await;
    if is_model_installed(&installed_list, model) {
        on_progress(1, 1, "уже установлено".into(), 0, 0);
        return Ok(());
    }

    let store = crate::paths::models_dir();
    if let Some(free) = crate::gpu::free_space_bytes(&store) {
        let needed = min_space_for_model(model).saturating_sub(partial_bytes(&store.join("blobs")));
        if free < needed {
            return Err(format!(
                "недостаточно места на диске: свободно {:.1} ГБ, нужно ~{:.1} ГБ",
                free as f64 / 1024.0 / 1024.0 / 1024.0,
                needed as f64 / 1024.0 / 1024.0 / 1024.0
            ));
        }
    }

    let mut last_error = String::new();
    for attempt in 1..=PULL_ATTEMPTS {
        match client.pull(model, flags.clone(), on_progress.clone()).await {
            // The stream could close cleanly, without an error line: that is what a
            // restart or a crash of `ollama serve` looks like mid-download. Trust
            // only the store — no model means the download did not happen.
            Ok(()) => match confirm_pull(client.list_models().await, model) {
                Ok(()) => {
                    on_progress(1, 1, "готово".into(), 0, 0);
                    return Ok(());
                }
                Err(error) => last_error = error,
            },
            Err(error) => {
                last_error = error;
            }
        }
        if last_error == PAUSED || last_error == crate::downloads::CANCELLED {
            return Err(last_error);
        }
        if !matches!(
            classify_pull_error(&last_error),
            PullFailure::Dns | PullFailure::Network
        ) {
            return Err(human_pull_error(model, &last_error));
        }
        if attempt < PULL_ATTEMPTS {
            on_progress(
                0,
                0,
                format!("связь с реестром моделей прервалась — повтор {attempt} из {PULL_ATTEMPTS}…"),
                0,
                0,
            );
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }

    // Own download: Ollama's client has neither a resolver change nor a filter bypass,
    // so when the registry is unreachable by DNS the last chance is to fetch the model ourselves.
    match crate::registry::pull(
        &crate::paths::models_dir(),
        model,
        flags,
        on_progress.clone(),
    )
    .await
    {
        Ok(()) => {
            // The files are laid out in the store, but Ollama itself must verify them:
            // the manifest is read from disk, so the answer to /api/show is the
            // confirmation that the write was accepted.
            let mut accepted = false;
            for _ in 0..3 {
                if client.show(model).await.is_ok() {
                    accepted = true;
                    break;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            if !accepted {
                return Err(format!(
                    "модель {model} скачана, но Ollama её не видит — перезапустите приложение \
                     и повторите: возможно, хранилище моделей занято другим процессом."
                ));
            }
            on_progress(1, 1, "готово".into(), 0, 0);
            Ok(())
        }
        // Pause and cancel stay themselves: wrapped in text about "also failed"
        // they would look like a failure, and the queue would move to the next item
        // instead of pausing or stopping.
        Err(direct_error) if direct_error == PAUSED || direct_error == crate::downloads::CANCELLED => {
            Err(direct_error)
        }
        Err(direct_error) => Err(format!(
            "{} Прямое скачивание тоже не прошло: {direct_error}",
            human_pull_error(model, &last_error)
        )),
    }
}

/// A clean close of the `/api/pull` stream is not yet success: a crashed
/// `ollama serve` looks the same. Only what the model list shows is done; an
/// unavailable list is the same sign of a dead serve, not "done".
fn confirm_pull(listed: Result<Vec<String>, String>, model: &str) -> Result<(), String> {
    match listed {
        Ok(list) if is_model_installed(&list, model) => Ok(()),
        Ok(_) => Err(INCOMPLETE_PULL.to_string()),
        Err(error) => Err(format!("{INCOMPLETE_PULL} (список моделей недоступен: {error})")),
    }
}

/// How many bytes already lie in unfinished store layers: a resume does not repeat
/// them, and without them the space check refused to continue an almost
/// downloaded model. Whose layer it is cannot be known without a manifest, so all are
/// counted: the check is only an estimate, the real lack of space will be shown by the download itself.
fn partial_bytes(blobs: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(blobs) else {
        return 0;
    };
    let files: Vec<(String, u64)> = entries
        .flatten()
        .filter_map(|e| Some((e.file_name().to_string_lossy().into_owned(), e.metadata().ok()?.len())))
        .collect();
    files
        .iter()
        .filter(|(name, _)| name.ends_with("-partial"))
        .map(|(name, len)| {
            // Ollama creates `-partial` as a sparse file of full length, while the
            // downloaded part goes to `-partial-N` (JSON with a Completed field). By length
            // the whole model would be subtracted, and the space check would turn off.
            let prefix = format!("{name}-");
            let parts: Vec<u64> = files
                .iter()
                .filter(|(part, _)| {
                    part.strip_prefix(&prefix).is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                })
                .map(|(part, _)| {
                    std::fs::read_to_string(blobs.join(part))
                        .ok()
                        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                        .and_then(|v| v.get("Completed").and_then(|c| c.as_u64()))
                        .unwrap_or(0)
                })
                .collect();
            // Our own resume of the registry does not write parts: there the file length is honest.
            if parts.is_empty() { *len } else { parts.iter().sum::<u64>().min(*len) }
        })
        .sum()
}

pub async fn installed(client: &OllamaClient) -> Vec<String> {
    client.list_models().await.unwrap_or_default()
}

/// The app's minimum context: less than 8192 cuts history and breaks
/// group chats, so the ceiling is not lowered below this value.
pub const MIN_CONTEXT: u32 = 8192;
/// Headroom for llama.cpp compute buffers and the Windows compositor, MB.
const COMPUTE_RESERVE_MB: u64 = 1200;
/// Step of the context ceiling. Every new num_ctx value means a model
/// reload in Ollama (~3 s for 14B) and a lost prompt cache.
pub const CONTEXT_STEP: u32 = 2048;
/// Spread of the budget measurement noise: other apps wander by ~230 MB between
/// polls, and Ollama's estimate in `/api/ps` differs from nvidia-smi by up to ~500 MB
/// (measured 25.09.2026).
const VRAM_NOISE_MB: u64 = 512;

/// num_ctx of the chat model: the requested context trimmed by the VRAM ceiling.
/// One number both for the gateway and for max_context in ST: otherwise ST trims history
/// to one context while the model loads with another.
pub fn chat_context(requested: u32, ceiling: u32) -> u32 {
    let requested = requested.max(MIN_CONTEXT);
    if ceiling > 0 {
        requested.min(ceiling)
    } else {
        requested
    }
}

/// Context budget for the active model and the current free VRAM.
#[derive(Debug, Clone, Copy, Default)]
pub struct ContextBudget {
    /// How many context tokens fit (not less than MIN_CONTEXT).
    pub ceiling: u32,
    /// Whether VRAM is enough at least for the weights with the minimal context:
    /// if not, the model will still be partially on the CPU.
    pub fits: bool,
    /// Free on the GPU right now, MB.
    pub free_mb: u64,
    /// Free plus the share of our loaded model — the budget for exactly that model.
    pub budget_mb: u64,
    /// What the model needs: weights, minimal KV cache and a buffer headroom.
    pub needed_mb: u64,
    /// Total VRAM on the GPU, MB.
    pub total_mb: u64,
    weights_mb: u64,
    kv_bytes_per_token: u64,
    native_ctx: u32,
    /// Budget noise, MB: absent for the "4/5 of total" estimate (Vulkan).
    noise_mb: u64,
}

impl ContextBudget {
    fn tokens_at(&self, budget_mb: u64) -> u32 {
        context_tokens_for_budget(budget_mb, self.weights_mb, self.kv_bytes_per_token, self.native_ctx).0
    }

    /// The ceiling one step below the computed one with a noise headroom.
    fn settled(&self) -> u32 {
        step_down(self.tokens_at(self.budget_mb.saturating_sub(self.noise_mb)))
    }

    /// A grown budget raises the ceiling only if it grew by more than the noise.
    fn grown(&self, current: u32) -> bool {
        step_down(self.tokens_at(self.budget_mb.saturating_sub(2 * self.noise_mb))) > current
    }

    /// Whether the model fits, also with a noise headroom: otherwise, at the boundary,
    /// the estimator's choice (chat model or light one) would jump from poll to poll.
    fn fits_settled(&self) -> bool {
        context_tokens_for_budget(
            self.budget_mb.saturating_sub(self.noise_mb),
            self.weights_mb,
            self.kv_bytes_per_token,
            self.native_ctx,
        )
        .1
    }
}

fn step_down(tokens: u32) -> u32 {
    (tokens / CONTEXT_STEP * CONTEXT_STEP).max(MIN_CONTEXT)
}

/// Stable context ceiling and the "model fits" flag for one
/// chat model. The ceiling is set with a noise headroom and a step down; it is lowered
/// when the headroom is exhausted, and grows when the budget grew by more than the noise.
///
/// Lowering and `fits = false` apply from the second measurement in a row: while Ollama
/// loads the model, free VRAM is already taken, while `/api/ps` does not show the model
/// yet (measurement: 11.5 → 3.3 GB for ~2.5 s) — a single such measurement dropped the
/// ceiling to the minimum and switched the estimator to the light model.
#[derive(Debug, Default)]
pub struct CeilingTracker {
    model: String,
    ceiling: u32,
    fits: bool,
    lowering: bool,
}

impl CeilingTracker {
    pub const fn new() -> Self {
        Self { model: String::new(), ceiling: 0, fits: true, lowering: false }
    }

    /// Last stable value for the model; (0, true) — not computed yet.
    pub fn current(&self, model: &str) -> (u32, bool) {
        if self.model == model {
            (self.ceiling, self.fits)
        } else {
            (0, true)
        }
    }

    /// The value when the budget failed to compute. For the same model keep the previous
    /// ceiling; for a new one a zero ceiling must not be kept (it would remove the
    /// limit and bring back swapping) — take the minimal context and `fits=false`.
    pub fn fallback(&self, model: &str) -> (u32, bool) {
        if self.model == model && self.ceiling > 0 {
            (self.ceiling, self.fits)
        } else {
            (MIN_CONTEXT, false)
        }
    }

    pub fn update(&mut self, model: &str, budget: &ContextBudget) -> (u32, bool) {
        if self.model != model {
            *self = Self { model: model.to_string(), ceiling: budget.settled(), fits: budget.fits, lowering: false };
            return (self.ceiling, self.fits);
        }
        let exhausted = budget.ceiling < self.ceiling || (self.fits && !budget.fits);
        if exhausted {
            if self.lowering {
                self.ceiling = budget.settled();
                self.fits = budget.fits;
                self.lowering = false;
            } else {
                self.lowering = true;
            }
        } else {
            self.lowering = false;
            let rises = if self.fits { budget.grown(self.ceiling) } else { budget.fits_settled() };
            if rises {
                self.ceiling = budget.settled();
                self.fits = true;
            }
        }
        (self.ceiling, self.fits)
    }
}

/// Compute the context budget: the weights and the KV cache must fit into free
/// VRAM. On overflow llama.cpp goes into swap and generation drops
/// from ~50 to 0.3 tok/s (measurement: 14B Q4, 16 GB, 16384). None — nothing to compute with
/// (no GPU or no metadata), the limit is not applied.
///
/// `embed` — the memory model (embeddings), if memory is enabled. Ollama keeps
/// it on the same GPU between turns, and without a reserve the ceiling took all free
/// VRAM: the chat model and bge-m3 evicted each other on every turn
/// (25.09.2026). An already loaded embedder needs no reserve — its memory
/// is already subtracted from `free_mb`.
pub async fn context_budget(
    client: &OllamaClient,
    model: &str,
    embed: Option<&str>,
) -> Option<ContextBudget> {
    let (free_mb, free_is_measured) = crate::gpu::free_vram()?;
    let show = client.show(model).await.ok()?;
    let info = show.get("model_info")?.as_object()?;

    // Metadata keys go with an architecture prefix: `qwen3.block_count`.
    let arch = info.keys().find_map(|k| k.strip_suffix(".block_count"))?.to_string();
    let get = |suffix: &str| info.get(&format!("{arch}.{suffix}")).and_then(|v| v.as_u64());
    get("block_count")?;
    // In hybrid qwen3.5 `head_count_kv` is a per-layer array (the linear
    // attention layers have no KV cache). `as_u64` on an array gave None, the budget
    // was not computed, the ceiling stayed zero, and the request went out with the full
    // context — swap on 11–12 GB GPUs.
    let kv_heads_total: u64 = match info.get(&format!("{arch}.attention.head_count_kv"))? {
        serde_json::Value::Array(items) => items.iter().filter_map(|v| v.as_u64()).sum(),
        other => other.as_u64()?,
    };
    if kv_heads_total == 0 {
        return None;
    }
    let key_len = get("attention.key_length")?;
    let value_len = get("attention.value_length").unwrap_or(key_len);
    let native_ctx = get("context_length").unwrap_or(32768) as u32;

    let kv_bytes_per_token = kv_bytes_per_token(
        kv_heads_total,
        key_len + value_len,
        active_kv_cache_q8(client),
    );
    if kv_bytes_per_token == 0 {
        return None;
    }

    // An unknown weight size is not "zero": the budget would be computed from the free
    // VRAM alone and let the full context through (fail-open swap).
    let weights_mb = client.model_size(model).await? / (1024 * 1024);
    // A loaded model already occupies VRAM: return its share to the budget,
    // otherwise the ceiling would be computed twice net of the weights.
    let running = client.running_models().await;
    let loaded_mb: u64 = running
        .iter()
        .filter(|(name, _, _)| is_model_installed(&[name.clone()], model))
        .map(|(_, _, vram)| vram / (1024 * 1024))
        .sum();
    let embed_reserve_mb = match embed {
        Some(name) if !name.trim().is_empty() => {
            let loaded = running
                .iter()
                .any(|(n, _, vram)| *vram > 0 && is_model_installed(std::slice::from_ref(n), name));
            if loaded {
                0
            } else {
                client.model_size(name).await.unwrap_or(0) / (1024 * 1024)
            }
        }
        _ => 0,
    };

    // The share of an already loaded model is returned to the budget only on top of a real
    // measurement: the "4/5 of total" estimate counts it as free anyway, and
    // adding it gave a ceiling above the physical VRAM.
    let total_mb = crate::gpu::gpu_info().vram_mb.unwrap_or(0);
    let budget_mb = chat_budget_mb(free_mb, loaded_mb, free_is_measured, total_mb, embed_reserve_mb);
    let (ceiling, fits) = context_tokens_for_budget(budget_mb, weights_mb, kv_bytes_per_token, native_ctx);
    let min_kv_mb = (MIN_CONTEXT as u64 * kv_bytes_per_token) / (1024 * 1024);
    Some(ContextBudget {
        ceiling,
        fits,
        free_mb,
        budget_mb,
        needed_mb: weights_mb + min_kv_mb + COMPUTE_RESERVE_MB + embed_reserve_mb,
        total_mb,
        weights_mb,
        kv_bytes_per_token,
        native_ctx,
        noise_mb: if free_is_measured { VRAM_NOISE_MB } else { 0 },
    })
}

/// Budget for the chat model: free plus our loaded share, minus the reserve
/// for the memory embedder, but not more than the GPU's physical VRAM.
fn chat_budget_mb(
    free_mb: u64,
    loaded_mb: u64,
    measured: bool,
    total_mb: u64,
    embed_reserve_mb: u64,
) -> u64 {
    let base = if measured { free_mb + loaded_mb } else { free_mb };
    let base = base.saturating_sub(embed_reserve_mb);
    if total_mb > 0 { base.min(total_mb) } else { base }
}

/// KV cache per token: (key_length + value_length) elements per layer and KV head.
/// f16 — 2 bytes per element (for Qwen3-14B 160 KiB per token, at 16384 tokens
/// 2.5 GB on top of the weights), q8_0 — 34 bytes per block of 32 elements (85 KiB).
fn kv_bytes_per_token(kv_heads_total: u64, key_value_len: u64, q8: bool) -> u64 {
    let block_bytes = if q8 { 34 } else { 64 };
    kv_heads_total * key_value_len * block_bytes / 32
}

/// The real KV cache type of the active serve: q8_0 is confirmed only by the
/// serve process being started with our `OLLAMA_KV_CACHE_TYPE` (that is what
/// `runtime::spawn_portable` does). `runtime::kv_cache_type()` answers "what we would
/// set", not "what the serve has": a reused serve of a previous
/// launch (including one raised in boot with a false CUDA detect — without env), a foreign or
/// system `ollama serve` without our env keeps f16, and a budget by q8_0 would overstate
/// the context ceiling twice — exactly the swap this function protects against
/// (V-34). The conservative default is f16: underestimating the context is safer than
/// overflowing the GPU.
fn active_kv_cache_q8(client: &OllamaClient) -> bool {
    let Some(port) = ollama_port(&client.base_url) else {
        return false;
    };
    // A blocking process scan — milliseconds, and only on a ceiling recompute
    // (start, saving settings, model change), not on every request.
    use sysinfo::{ProcessRefreshKind, System, UpdateKind};
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessRefreshKind::new());
    let candidates: Vec<sysinfo::Pid> = sys
        .processes()
        .iter()
        .filter(|(_, proc)| proc.name().to_lowercase().starts_with("ollama"))
        .map(|(pid, _)| *pid)
        .collect();
    let mut confirmed: Option<bool> = None;
    for pid in candidates {
        sys.refresh_process_specifics(pid, ProcessRefreshKind::new().with_environ(UpdateKind::Always));
        if let Some(proc) = sys.process(pid) {
            confirmed = fold_kv_cache_q8(confirmed, proc.environ(), port);
        }
    }
    confirmed.unwrap_or(false)
}

/// Ollama port from the client address (`http://127.0.0.1:11435`).
fn ollama_port(base_url: &str) -> Option<u16> {
    base_url.rsplit(':').next()?.trim().parse().ok()
}

fn env_value<'a>(environ: &'a [String], name: &str) -> Option<&'a str> {
    environ.iter().find_map(|entry| {
        let (key, value) = entry.split_once('=')?;
        key.eq_ignore_ascii_case(name).then_some(value)
    })
}

/// Whether this process holds the serve on the port and its q8_0 KV cache is confirmed.
/// None — the process is not on our port; Some(false) — the serve is ours, but q8_0 is
/// not confirmed: assume f16.
fn serve_kv_cache_q8(environ: &[String], port: u16) -> Option<bool> {
    let host = format!("127.0.0.1:{port}");
    if env_value(environ, "OLLAMA_HOST")?.trim() != host {
        return None;
    }
    Some(env_value(environ, "OLLAMA_KV_CACHE_TYPE").is_some_and(|v| v.trim().eq_ignore_ascii_case("q8_0")))
}

/// Fold over processes: q8_0 is confirmed only if all serves on the
/// port hold it. A process with the same `OLLAMA_HOST` but without our env (a client, a foreign
/// daemon) conservatively drops the answer to f16.
fn fold_kv_cache_q8(acc: Option<bool>, environ: &[String], port: u16) -> Option<bool> {
    match serve_kv_cache_q8(environ, port) {
        None => acc,
        Some(q8) => Some(acc.unwrap_or(true) && q8),
    }
}

/// How many context tokens fit into the VRAM budget and whether it is enough
/// at least for the minimal context. Pure arithmetic — separated from requests
/// to Ollama so that it can be checked by a test.
pub fn context_tokens_for_budget(
    budget_mb: u64,
    weights_mb: u64,
    kv_bytes_per_token: u64,
    native_ctx: u32,
) -> (u32, bool) {
    if kv_bytes_per_token == 0 {
        return (MIN_CONTEXT, true);
    }
    let min_kv_mb = (MIN_CONTEXT as u64 * kv_bytes_per_token) / (1024 * 1024);
    if budget_mb <= weights_mb + min_kv_mb + COMPUTE_RESERVE_MB {
        return (MIN_CONTEXT, false);
    }
    let available_bytes = (budget_mb - weights_mb - COMPUTE_RESERVE_MB) * 1024 * 1024;
    let tokens = (available_bytes / kv_bytes_per_token) as u32;
    // The native context is sometimes smaller than the app's minimum (old and fine-tuned
    // models at 2048/4096): clamp panics with max < min, while the ceiling is not lowered
    // below MIN_CONTEXT in the branches above either.
    (tokens.clamp(MIN_CONTEXT, native_ctx.max(MIN_CONTEXT)), true)
}

pub fn resolve_model_to_delete<'a>(installed: &'a [String], model: &'a str) -> &'a str {
    let target = normalize_model_name(model);
    if target.is_empty() {
        return model;
    }
    if let Some(found) = installed.iter().find(|m| {
        let norm = normalize_model_name(m);
        if norm.eq_ignore_ascii_case(target) {
            return true;
        }
        if !target.contains(':') && norm.eq_ignore_ascii_case(&format!("{target}:latest")) {
            return true;
        }
        if let Some(base) = target.strip_suffix(":latest") {
            if norm.eq_ignore_ascii_case(base) {
                return true;
            }
        }
        false
    }) {
        found.as_str()
    } else {
        model
    }
}

pub async fn delete(client: &OllamaClient, model: &str) -> Result<(), String> {
    let installed_list = installed(client).await;
    let model_to_delete = resolve_model_to_delete(&installed_list, model);
    client.delete(model_to_delete).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_merge_keeps_same_size_manifest_conflicts() {
        let root = std::env::temp_dir().join(format!("lk-audit-manifest-{}", std::process::id()));
        let from = root.join("from");
        let to = root.join("to");
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        std::fs::write(from.join("latest"), b"new-manifest").unwrap();
        std::fs::write(to.join("latest"), b"old-manifest").unwrap();
        let (_, conflicts) = merge_dir(&from, &to).unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(std::fs::read(from.join("latest")).unwrap(), b"new-manifest");
        assert_eq!(std::fs::read(to.join("latest")).unwrap(), b"old-manifest");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hashed_blob_with_equal_size_is_a_duplicate_without_reading() {
        let root = std::env::temp_dir().join(format!("lk-blob-dup-{}", std::process::id()));
        let (from, to) = (root.join("from"), root.join("to"));
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        let blob = format!("sha256-{}", "a".repeat(64));
        std::fs::write(from.join(&blob), b"same").unwrap();
        std::fs::write(to.join(&blob), b"SAME").unwrap();
        let (_, conflicts) = merge_dir(&from, &to).unwrap();
        assert!(conflicts.is_empty());
        assert!(!from.join(&blob).exists());
        assert!(to.join(&blob).exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn audit_move_refuses_partial_destination_without_deleting_source() {
        let root = std::env::temp_dir().join(format!("lk-audit-partial-{}", std::process::id()));
        let from = root.join("from");
        let to = root.join("to");
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        std::fs::write(from.join("weights"), b"complete-model").unwrap();
        std::fs::write(to.join("weights"), b"partial").unwrap();
        assert!(move_dir(&from, &to).is_err());
        assert_eq!(std::fs::read(from.join("weights")).unwrap(), b"complete-model");
        assert_eq!(std::fs::read(to.join("weights")).unwrap(), b"partial");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn test_merge_dir_skips_existing_names() {
        let root = std::env::temp_dir().join(format!("lk-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let from = root.join("from");
        let to = root.join("to");
        std::fs::create_dir_all(from.join("hub").join("models--a")).unwrap();
        std::fs::create_dir_all(to.join("hub").join("models--b")).unwrap();
        std::fs::write(from.join("hub").join("models--a").join("config.json"), b"new").unwrap();
        std::fs::write(to.join("hub").join("models--b").join("config.json"), b"old").unwrap();
        std::fs::write(from.join("hub").join("models--b").join("stale"), b"x").unwrap_or(());

        let (moved, conflicts) = merge_dir(&from, &to).unwrap();
        assert!(moved >= 1, "ожидался перенос хотя бы одного объекта");
        assert!(conflicts.is_empty(), "конфликтов быть не должно: {conflicts:?}");
        assert!(to.join("hub").join("models--a").join("config.json").exists());
        assert!(to.join("hub").join("models--b").join("config.json").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A file with the same name and different content must not be lost: remove_dir_all
    /// of the source would destroy the only copy.
    #[test]
    fn test_merge_dir_keeps_conflicting_files_in_source() {
        let root = std::env::temp_dir().join(format!("lk-merge-conflict-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let from = root.join("from");
        let to = root.join("to");
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        std::fs::write(from.join("sha256-abc"), b"legacy-blob-content").unwrap();
        std::fs::write(to.join("sha256-abc"), b"other").unwrap();
        std::fs::write(from.join("unique"), b"only-in-source").unwrap();

        let (moved, conflicts) = merge_dir(&from, &to).unwrap();
        assert_eq!(moved, 1, "уникальный файл переносится");
        assert_eq!(conflicts.len(), 1, "конфликт должен быть учтён");
        assert!(from.join("sha256-abc").exists(), "конфликтный файл остаётся в источнике");
        assert_eq!(std::fs::read(from.join("sha256-abc")).unwrap(), b"legacy-blob-content");
        assert!(!from.join("unique").exists(), "перенесённый файл убран из источника");
        assert_eq!(std::fs::read(to.join("unique")).unwrap(), b"only-in-source");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_merge_dir_replaces_truncated_hashed_blob() {
        let root = std::env::temp_dir().join(format!("lk-merge-trunc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let from = root.join("from");
        let to = root.join("to");
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        let blob = format!("sha256-{}", "a".repeat(64));
        std::fs::write(from.join(&blob), b"full-layer-bytes").unwrap();
        std::fs::write(to.join(&blob), b"full-la").unwrap();

        let (moved, conflicts) = merge_dir(&from, &to).unwrap();
        assert_eq!((moved, conflicts.len()), (1, 0));
        assert_eq!(std::fs::read(to.join(&blob)).unwrap(), b"full-layer-bytes");
        assert!(!from.join(&blob).exists());
        assert!(!to.join(format!("{blob}.part")).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An equal size on a content-addressed warehouse is a duplicate: the source
    /// is removed, it is not counted as a conflict.
    #[test]
    fn test_merge_dir_drops_same_size_duplicates() {
        let root = std::env::temp_dir().join(format!("lk-merge-same-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let from = root.join("from");
        let to = root.join("to");
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        std::fs::write(from.join("sha256-xyz"), b"same-size").unwrap();
        std::fs::write(to.join("sha256-xyz"), b"same-size").unwrap();

        let (moved, conflicts) = merge_dir(&from, &to).unwrap();
        assert_eq!(moved, 0);
        assert!(conflicts.is_empty());
        assert!(!from.join("sha256-xyz").exists(), "дубликат убран из источника");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_copy_dir_moves_nested_files() {
        let root = std::env::temp_dir().join(format!("lk-models-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let from = root.join("from");
        let to = root.join("to");
        std::fs::create_dir_all(from.join("blobs")).unwrap();
        std::fs::write(from.join("manifests.json"), b"a").unwrap();
        std::fs::write(from.join("blobs").join("sha256-1"), b"b").unwrap();

        copy_dir(&from, &to).unwrap();
        assert!(to.join("manifests.json").exists());
        assert!(to.join("blobs").join("sha256-1").exists());

        move_dir(&from, &to).unwrap();
        assert!(!from.exists(), "источник должен исчезнуть после переноса");
        assert!(to.join("blobs").join("sha256-1").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_min_space_for_model() {
        assert_eq!(min_space_for_model("huihui_ai/qwen3-abliterated:14b"), 10 * 1024 * 1024 * 1024);
        assert_eq!(min_space_for_model("huihui_ai/qwen3-abliterated:8b"), 6 * 1024 * 1024 * 1024);
        assert_eq!(min_space_for_model("qwen2.5vl:7b"), 6 * 1024 * 1024 * 1024);
        assert_eq!(min_space_for_model("nomic-embed-text"), 1 * 1024 * 1024 * 1024);
        assert_eq!(min_space_for_model("bge-m3:latest"), 1 * 1024 * 1024 * 1024);
        assert_eq!(min_space_for_model("bge-m3"), 1 * 1024 * 1024 * 1024);
        // The size is searched in the name on a boundary: "1.7b" must not turn into "7b".
        assert_eq!(min_space_for_model("huihui_ai/qwen3-abliterated:1.7b"), 2 * 1024 * 1024 * 1024);
        assert_eq!(min_space_for_model("huihui_ai/qwen3-vl-abliterated:2b-instruct-q4_K_M"), 2 * 1024 * 1024 * 1024);
        assert_eq!(min_space_for_model("huihui_ai/qwen3-abliterated:4b"), 3 * 1024 * 1024 * 1024);
    }

    /// The reserve for the memory embedder is subtracted from the chat model's budget; an estimate
    /// without a measurement is trimmed too, and the ceiling does not exceed the GPU's physical memory.
    #[test]
    fn test_chat_budget_reserves_embedder() {
        // A 16 GB GPU, 12 GB free, our model already holds 1 GB, bge-m3 ≈1.2 GB.
        assert_eq!(chat_budget_mb(12_000, 1_000, true, 16_000, 1_200), 11_800);
        // Without a reserve — the previous behaviour.
        assert_eq!(chat_budget_mb(12_000, 1_000, true, 16_000, 0), 13_000);
        // The "4/5 of total" estimate (Vulkan): a foreign loaded share is not returned.
        assert_eq!(chat_budget_mb(12_000, 1_000, false, 16_000, 1_200), 10_800);
        // The reserve is larger than the free amount — zero, not an overflow.
        assert_eq!(chat_budget_mb(500, 0, true, 16_000, 1_200), 0);
        // The ceiling does not exceed the GPU's physical memory.
        assert_eq!(chat_budget_mb(16_000, 2_000, true, 16_000, 1_200), 16_000);
    }

    #[test]
    fn test_context_tokens_for_budget() {
        // Qwen3-14B: 40 layers, 8 KV heads, head_dim 128 → 160 KiB per token.
        // Cross-checked against a live measurement: 8192 → 9.49 GB, 16384 → 10.76 GB in /api/ps.
        let kv = 40 * 8 * (128 + 128) * 2;
        assert_eq!(kv, 163_840);

        // Measurement on a live machine: a 16 GB GPU, 14B Q4 (9718 MB of weights), but 5 GB
        // already taken by other apps — 16384 tokens do not fit there,
        // and generation fell from 50 to 0.3 tok/s.
        let (tokens, fits) = context_tokens_for_budget(10_966, 9_718, kv, 40_960);
        assert!(!fits, "модели не хватает памяти даже на минимальный контекст");
        assert_eq!(tokens, MIN_CONTEXT);

        // The same GPU, but free: 32768 tokens fit (≈34k ceiling).
        let (tokens, fits) = context_tokens_for_budget(16_303, 9_718, kv, 40_960);
        assert!(tokens > 32_768, "на свободной карте потолок выше 32k: {tokens}");
        assert!(fits);

        // The ceiling does not exceed the model's native context.
        let (tokens, _) = context_tokens_for_budget(64_000, 2_000, kv, 40_960);
        assert_eq!(tokens, 40_960);

        // A small model on the same GPU with headroom: the native context limits it,
        // not the VRAM.
        let (tokens, fits) = context_tokens_for_budget(10_966, 5_000, kv, 40_960);
        assert!(fits);
        assert!(tokens > 16_384);

        // The native context is smaller than the app's minimum (a model at 4096): before,
        // clamp panicked and broke the startup and the VRAM poll.
        let (tokens, fits) = context_tokens_for_budget(64_000, 2_000, kv, 4_096);
        assert!(fits);
        assert_eq!(tokens, MIN_CONTEXT);
    }

    /// Cross-checked against the llama-server log of 25.09.2026 (8192 cells): 14B — 1280 MiB
    /// f16 and 680 MiB q8_0, 4B (36 layers) — 1152 and 612 MiB.
    #[test]
    fn test_kv_bytes_per_token_matches_llama_server() {
        let mib = |bytes_per_token: u64| bytes_per_token * 8192 / (1024 * 1024);
        // Hybrid qwen3.5 returns head_count_kv as an array: the budget is computed from the
        // sum of heads, and an unknown model does not get a zero ceiling.
        let tracker = CeilingTracker::new();
        assert_eq!(tracker.fallback("new-model"), (MIN_CONTEXT, false));
        assert_eq!(mib(kv_bytes_per_token(320, 256, false)), 1280);
        assert_eq!(mib(kv_bytes_per_token(320, 256, true)), 680);
        assert_eq!(mib(kv_bytes_per_token(288, 256, false)), 1152);
        assert_eq!(mib(kv_bytes_per_token(288, 256, true)), 612);
    }

    /// V-34: q8_0 is confirmed only by the env of the serve process itself; a serve without
    /// our env (foreign, system, an old launch) is conservatively treated as f16 —
    /// otherwise the context ceiling would be doubled and generation would go into swap.
    #[test]
    fn test_kv_cache_q8_follows_the_serving_process_env() {
        let serve_q8 = vec![
            "OLLAMA_HOST=127.0.0.1:11435".to_string(),
            "OLLAMA_KV_CACHE_TYPE=q8_0".to_string(),
        ];
        let serve_f16 = vec!["OLLAMA_HOST=127.0.0.1:11435".to_string()];
        let other_port = vec![
            "OLLAMA_HOST=127.0.0.1:11434".to_string(),
            "OLLAMA_KV_CACHE_TYPE=q8_0".to_string(),
        ];
        assert_eq!(serve_kv_cache_q8(&serve_q8, 11435), Some(true));
        assert_eq!(serve_kv_cache_q8(&serve_f16, 11435), Some(false));
        assert_eq!(serve_kv_cache_q8(&other_port, 11435), None);
        assert_eq!(serve_kv_cache_q8(&[], 11435), None);

        // No serve found on the port — f16 conservatively.
        assert_eq!(fold_kv_cache_q8(None, &other_port, 11435), None);
        // Two processes on the port: q8 is confirmed only if both have it.
        let both_q8 = fold_kv_cache_q8(fold_kv_cache_q8(None, &serve_q8, 11435), &serve_q8, 11435);
        assert_eq!(both_q8, Some(true));
        let mixed = fold_kv_cache_q8(fold_kv_cache_q8(None, &serve_q8, 11435), &serve_f16, 11435);
        assert_eq!(mixed, Some(false));
    }

    #[test]
    fn test_ollama_port_from_base_url() {
        assert_eq!(ollama_port("http://127.0.0.1:11435"), Some(11435));
        assert_eq!(ollama_port("http://127.0.0.1"), None);
    }

    #[test]
    fn test_chat_context() {
        assert_eq!(chat_context(32_768, 0), 32_768, "потолок неизвестен — без урезания");
        assert_eq!(chat_context(32_768, 24_576), 24_576);
        assert_eq!(chat_context(16_384, 24_576), 16_384);
        assert_eq!(chat_context(4_096, 0), MIN_CONTEXT);
    }

    /// Budget for 14B q8_0 on a 16 GB GPU: 8584 MB of weights, 85 KiB of KV per token.
    fn budget_14b(budget_mb: u64) -> ContextBudget {
        let kv = kv_bytes_per_token(320, 256, true);
        let (ceiling, fits) = context_tokens_for_budget(budget_mb, 8_584, kv, 40_960);
        ContextBudget {
            ceiling,
            fits,
            budget_mb,
            weights_mb: 8_584,
            kv_bytes_per_token: kv,
            native_ctx: 40_960,
            noise_mb: VRAM_NOISE_MB,
            ..Default::default()
        }
    }

    #[test]
    fn test_ceiling_rounds_down_with_noise_margin() {
        let mut tracker = CeilingTracker::new();
        let (ceiling, fits) = tracker.update("14b", &budget_14b(11_500));
        assert!(fits);
        assert_eq!(ceiling % CONTEXT_STEP, 0);
        assert_eq!(ceiling, 14_336);
        assert!(ceiling <= budget_14b(11_500).ceiling);
    }

    /// Other apps wander by ~230 MB between polls, Ollama's estimate is off
    /// by up to ~500 MB: such noise must not change num_ctx (every new value means a
    /// model reload).
    #[test]
    fn test_ceiling_ignores_vram_noise() {
        let mut tracker = CeilingTracker::new();
        let (first, _) = tracker.update("14b", &budget_14b(11_500));
        for budget in [11_300, 11_700, 11_250, 11_990, 11_020, 11_760, 11_500] {
            assert_eq!(tracker.update("14b", &budget_14b(budget)), (first, true), "бюджет {budget}");
        }
    }

    /// While Ollama loads the model, VRAM is already taken, while `/api/ps` is empty:
    /// a single such measurement does not lower the ceiling nor declare that the model
    /// does not fit. A confirmed drop lowers it right away to what fits.
    #[test]
    fn test_ceiling_lowers_on_confirmed_drop_only() {
        let mut tracker = CeilingTracker::new();
        let (first, _) = tracker.update("14b", &budget_14b(11_500));
        assert_eq!(tracker.update("14b", &budget_14b(3_300)), (first, true), "замер посреди загрузки");
        assert_eq!(tracker.update("14b", &budget_14b(11_500)), (first, true));

        assert_eq!(tracker.update("14b", &budget_14b(10_800)), (first, true), "первый низкий замер");
        let (lowered, fits) = tracker.update("14b", &budget_14b(10_800));
        assert!(lowered < first);
        assert!(lowered <= budget_14b(10_800).ceiling, "потолок не выше того, что помещается");
        assert!(fits);

        tracker.update("14b", &budget_14b(10_000));
        assert_eq!(tracker.update("14b", &budget_14b(10_000)), (MIN_CONTEXT, false));
    }

    #[test]
    fn test_ceiling_rises_only_beyond_noise() {
        let mut tracker = CeilingTracker::new();
        let (first, _) = tracker.update("14b", &budget_14b(11_500));
        assert_eq!(tracker.update("14b", &budget_14b(12_000)), (first, true));
        let (raised, _) = tracker.update("14b", &budget_14b(12_600));
        assert_eq!(raised, 26_624);
        assert_eq!(raised % CONTEXT_STEP, 0);

        // "Does not fit" is cleared also only with a noise headroom: at the boundary the
        // estimator's choice would otherwise jump between the chat model and the light one.
        let mut tracker = CeilingTracker::new();
        assert_eq!(tracker.update("14b", &budget_14b(10_000)), (MIN_CONTEXT, false));
        assert_eq!(tracker.update("14b", &budget_14b(10_600)), (MIN_CONTEXT, false));
        assert_eq!(tracker.update("14b", &budget_14b(11_100)), (MIN_CONTEXT, true));
    }

    #[test]
    fn test_ceiling_resets_for_another_model() {
        let mut tracker = CeilingTracker::new();
        tracker.update("14b", &budget_14b(11_500));
        assert_eq!(tracker.current("4b"), (0, true));
        let (ceiling, _) = tracker.update("4b", &budget_14b(12_600));
        assert_eq!(ceiling, 26_624, "новая модель — без гистерезиса прежней");
        assert_eq!(tracker.current("4b"), (26_624, true));
    }

    #[test]
    fn test_is_model_installed() {
        let list = vec![
            "bge-m3:latest".to_string(),
            "huihui_ai/qwen3-abliterated:8b".to_string(),
            "library/nomic-embed-text:latest".to_string(),
        ];
        assert!(is_model_installed(&list, "bge-m3"));
        assert!(is_model_installed(&list, "bge-m3:latest"));
        assert!(is_model_installed(&list, "huihui_ai/qwen3-abliterated:8b"));
        assert!(is_model_installed(&list, "nomic-embed-text"));
        assert!(is_model_installed(&list, "library/nomic-embed-text"));
        assert!(!is_model_installed(&list, "huihui_ai/qwen3-abliterated:14b"));
        assert!(!is_model_installed(&list, ""));
    }

    #[test]
    fn test_normalize_model_name() {
        assert_eq!(normalize_model_name("registry.ollama.ai/huihui_ai/qwen3-abliterated:8b"), "huihui_ai/qwen3-abliterated:8b");
        assert_eq!(normalize_model_name("library/nomic-embed-text"), "nomic-embed-text");
        assert_eq!(normalize_model_name("  bge-m3:latest  "), "bge-m3:latest");
    }

    #[test]
    fn test_classify_pull_error() {
        // The text is a real Go error from Ollama on a machine with a filtered DNS.
        let dns = r#"pull model manifest: Get "https://registry.ollama.ai/v2/huihui_ai/qwen3-abliterated/manifests/8b-v2": dial tcp: lookup registry.ollama.ai: no such host"#;
        assert_eq!(classify_pull_error(dns), PullFailure::Dns);
        assert_eq!(
            classify_pull_error("dial tcp 104.18.16.170:443: connectex: connection refused"),
            PullFailure::Network
        );
        assert_eq!(
            classify_pull_error("pull model manifest: file does not exist"),
            PullFailure::NotFound
        );
        // A typo in the model name: Ollama answers 400, and the person needs the same
        // explanation as for a missing tag.
        assert_eq!(
            classify_pull_error("pull huihui_ai/нет-такой:0b: HTTP 400 Bad Request"),
            PullFailure::NotFound
        );
        assert_eq!(
            classify_pull_error("no space left on device"),
            PullFailure::Disk
        );
        assert_eq!(
            classify_pull_error("unexpected status code 500"),
            PullFailure::Other
        );
    }

    /// Our own stream-idle text — Cyrillic: it must go to retry, not to
    /// "other" without retries and registry bypass.
    #[test]
    fn test_classify_pull_error_own_idle_timeout_is_network() {
        assert_eq!(
            classify_pull_error("таймаут ожидания данных от Ollama при скачивании модели (300 с)"),
            PullFailure::Network
        );
        assert_eq!(classify_pull_error(INCOMPLETE_PULL), PullFailure::Network);
    }

    /// Windows' "Access is denied." is a file held by antivirus, not a
    /// closed repository: the "authorization required" hint lies here.
    #[test]
    fn test_classify_pull_error_local_file_errors_are_not_registry_errors() {
        let locked = r"open Z:\AImodels\ollama\blobs\sha256-6da1-partial: Access is denied.";
        assert_eq!(classify_pull_error(locked), PullFailure::Other);
        assert_eq!(
            classify_pull_error("pull model manifest: unauthorized: access denied"),
            PullFailure::Forbidden
        );
        // The word "lookup" without Go's DNS error is not DNS.
        assert_eq!(classify_pull_error("tensor lookup failed"), PullFailure::Other);
        assert_eq!(
            classify_pull_error(
                r#"Get "https://registry.ollama.ai/v2/": dial tcp: lookup registry.ollama.ai on 8.8.8.8:53: i/o timeout"#
            ),
            PullFailure::Dns
        );
        assert_eq!(
            classify_pull_error("tls: failed to verify certificate: x509: certificate signed by unknown authority"),
            PullFailure::Network
        );
    }

    /// A clean close of the stream with an unavailable model list is not "done":
    /// that is what a crashed serve looks like, and the model might not have finished downloading.
    #[test]
    fn test_confirm_pull_trusts_only_the_listed_store() {
        let listed = vec!["bge-m3:latest".to_string()];
        assert_eq!(confirm_pull(Ok(listed), "bge-m3"), Ok(()));
        assert_eq!(confirm_pull(Ok(Vec::new()), "bge-m3"), Err(INCOMPLETE_PULL.to_string()));
        let down = confirm_pull(Err("connection refused".into()), "bge-m3").unwrap_err();
        assert_eq!(classify_pull_error(&down), PullFailure::Network, "упавший serve идёт в повтор: {down}");
    }

    /// The already downloaded part counts toward the space check. For an Ollama download that is the
    /// sum of Completed from `-partial-N` (the `-partial` itself is sparse, of full
    /// length), for our own resume without parts — the file length; finished blobs do not count.
    #[test]
    fn test_partial_bytes_counts_resume_points_only() {
        let blobs = std::env::temp_dir().join(format!("lk-partial-bytes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&blobs);
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::write(blobs.join("sha256-aaa-partial"), vec![0u8; 1000]).unwrap();
        std::fs::write(blobs.join("sha256-aaa-partial-0"), r#"{"N":0,"Offset":0,"Size":500,"Completed":30}"#).unwrap();
        std::fs::write(blobs.join("sha256-aaa-partial-1"), r#"{"N":1,"Offset":500,"Size":500,"Completed":20}"#).unwrap();
        std::fs::write(blobs.join("sha256-ccc-partial"), vec![0u8; 100]).unwrap();
        std::fs::write(blobs.join("sha256-bbb"), vec![0u8; 50]).unwrap();

        assert_eq!(partial_bytes(&blobs), 50 + 100);
        assert_eq!(partial_bytes(&blobs.join("нет-такой")), 0);
        let _ = std::fs::remove_dir_all(&blobs);
    }

    /// A ComfyUI checkpoint with the same name and size is not necessarily the same file:
    /// without size-based deduplication it goes to conflicts rather than being deleted.
    #[test]
    fn test_merge_without_size_dedup_keeps_same_size_files() {
        let root = std::env::temp_dir().join(format!("lk-merge-comfy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (from, to) = (root.join("from"), root.join("to"));
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        std::fs::write(from.join("model.safetensors"), b"AAAA").unwrap();
        std::fs::write(to.join("model.safetensors"), b"BBBB").unwrap();

        let (moved, conflicts) = merge_dir_with(&from, &to, false).unwrap();
        assert_eq!(moved, 0);
        assert_eq!(conflicts.len(), 1, "одноимённый файл — конфликт");
        assert!(from.join("model.safetensors").exists(), "чужая модель не удалена");
        assert_eq!(std::fs::read(to.join("model.safetensors")).unwrap(), b"BBBB");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A store before 1.2.3 without a warehouse lay right in `<base>\models`: blobs and
    /// manifests move to `<base>\models\ollama`, the neighbouring folders of the root
    /// stay in place, a repeat does nothing.
    #[test]
    fn test_adopt_nested_store_moves_only_ollama_folders() {
        let root = std::env::temp_dir().join(format!("lk-nested-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let manifest = ["manifests", "registry.ollama.ai", "library", "bge-m3"];
        std::fs::create_dir_all(root.join("blobs")).unwrap();
        std::fs::create_dir_all(manifest.iter().fold(root.clone(), |p, part| p.join(part))).unwrap();
        std::fs::create_dir_all(root.join("comfyui")).unwrap();
        std::fs::write(root.join("blobs").join("sha256-old"), b"weights").unwrap();
        std::fs::write(
            manifest.iter().fold(root.clone(), |p, part| p.join(part)).join("latest"),
            b"{}",
        )
        .unwrap();
        // The new store is already created and has its own layer.
        let store = root.join("ollama");
        std::fs::create_dir_all(store.join("blobs")).unwrap();
        std::fs::write(store.join("blobs").join("sha256-new"), b"fresh").unwrap();

        let mut done = Vec::new();
        adopt_nested_store(&root, &store, &mut done).unwrap();

        assert!(store.join("blobs").join("sha256-old").exists());
        assert!(store.join("blobs").join("sha256-new").exists());
        let moved_manifest = manifest[1..]
            .iter()
            .fold(store.join("manifests"), |p, part| p.join(part))
            .join("latest");
        assert!(moved_manifest.exists(), "манифест не переехал: {done:?}");
        assert!(!root.join("blobs").exists() && !root.join("manifests").exists());
        assert!(root.join("comfyui").exists(), "соседняя папка склада тронута");

        let mut again = Vec::new();
        adopt_nested_store(&root, &store, &mut again).unwrap();
        assert!(again.is_empty(), "повтор не должен ничего делать: {again:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_human_pull_error_explains_dns_failure() {
        let text = human_pull_error(
            "huihui_ai/qwen3-abliterated:8b-v2",
            "dial tcp: lookup registry.ollama.ai: no such host",
        );
        assert!(text.contains("registry.ollama.ai"), "получено: {text}");
        assert!(text.contains("DNS"), "получено: {text}");
        assert!(
            !text.contains("dial tcp"),
            "сырая ошибка Go до пользователя доходить не должна: {text}"
        );
    }

    #[test]
    fn test_resolve_model_to_delete() {
        let list = vec![
            "bge-m3:latest".to_string(),
            "huihui_ai/qwen3-abliterated:8b".to_string(),
            "library/nomic-embed-text:latest".to_string(),
        ];
        assert_eq!(resolve_model_to_delete(&list, "bge-m3"), "bge-m3:latest");
        assert_eq!(resolve_model_to_delete(&list, "bge-m3:latest"), "bge-m3:latest");
        assert_eq!(resolve_model_to_delete(&list, "huihui_ai/qwen3-abliterated:8b"), "huihui_ai/qwen3-abliterated:8b");
        assert_eq!(resolve_model_to_delete(&list, "nomic-embed-text"), "library/nomic-embed-text:latest");
        assert_eq!(resolve_model_to_delete(&list, "unknown-model"), "unknown-model");
    }
}

