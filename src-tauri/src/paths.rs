use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;

/// Root of the model store. Re-assignable (V-12): boot and `retry_boot` both
/// call `set_models_root` from settings, and a second call used to be a silent
/// `OnceLock` no-op — a corrected store applied only after a full restart.
/// A running `ollama serve` still keeps the `OLLAMA_MODELS` it was spawned
/// with, so switching the store under a live runtime needs its restart.
struct ModelsRootState(RwLock<Option<PathBuf>>);

impl ModelsRootState {
    /// Read the configured root; `None` means "use the default lookup".
    /// Blocking a reader can never panic: the lock is taken for a clone only.
    fn get(&self) -> Option<PathBuf> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Store the configured root and return the one it replaces (if any).
    fn set(&self, root: PathBuf) -> Option<PathBuf> {
        self.0.write().unwrap_or_else(|e| e.into_inner()).replace(root)
    }
}

static MODELS_ROOT: ModelsRootState = ModelsRootState(RwLock::new(None));

/// Application paths — everything lives in the installation folder (the exe's directory).
/// In a dev build that is `src-tauri/target/debug` — dev-run data does not leak into the release.
///
/// `current_exe` — an OS call that can fail: the typed form is
/// `try_base_dir`, while this wrapper keeps the infallible signature the whole
/// crate relies on and, instead of panicking, degrades to the current directory with
/// an entry in boot.log. A blocking OS call, cheap, but not for hot paths.
pub fn base_dir() -> PathBuf {
    match try_base_dir() {
        Ok(dir) => dir,
        Err(e) => {
            crate::boot_log(&format!(
                "Каталог приложения не определён ({e}), берём текущий каталог"
            ));
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        }
    }
}

/// Typed detection of the installation folder (the exe's parent).
pub fn try_base_dir() -> Result<PathBuf, std::io::Error> {
    let exe = std::env::current_exe()?;
    exe.parent()
        .map(|parent| parent.to_path_buf())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "у exe нет каталога"))
}

pub fn runtime_dir() -> PathBuf {
    base_dir().join("runtime").join("ollama")
}

/// Root of the model warehouse: Ollama, ComfyUI and the Hugging Face cache in one place.
/// The value comes from settings at startup; without it — `CLOUFAM_MODELS_ROOT`
/// (the old `LOREKEEPER_MODELS_ROOT` is still accepted), then the old `models`
/// folder next to the program (the weights of previous installations already lie
/// there), otherwise the shared `Z:\AImodels`; models never go outside it.
pub fn models_root() -> PathBuf {
    if let Some(root) = MODELS_ROOT.get() {
        return root;
    }
    if let Some(root) = models_root_env() {
        return root;
    }
    let legacy = legacy_models_dir();
    if legacy.exists() {
        return legacy;
    }
    system_models_root().unwrap_or(legacy)
}

/// Default warehouse for new installations — the shared `Z:\AImodels`.
/// No Z: drive — `None`, and the caller falls back to the folder next to the exe.
pub fn system_models_root() -> Option<PathBuf> {
    let shared = PathBuf::from(r"Z:\AImodels");
    shared.is_dir().then_some(shared)
}

/// Warehouse root environment variable: the new name, then the old one.
fn models_root_env() -> Option<PathBuf> {
    env_dir("CLOUFAM_MODELS_ROOT").or_else(|| env_dir("LOREKEEPER_MODELS_ROOT"))
}

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|value| !value.is_empty()).map(PathBuf::from)
}

/// Set the model warehouse root (called at startup from settings).
/// Repeated calls apply: `retry_boot` may fix the configured root and used to
/// hit a silent `OnceLock` no-op (V-12). A real change is logged to boot.log.
pub fn set_models_root(root: PathBuf) {
    if let Some(previous) = MODELS_ROOT.set(root.clone()) {
        if previous != root {
            crate::boot_log(&format!(
                "Склад моделей изменён: {} -> {} (новые загрузки пойдут сюда; рантайм перезапустите)",
                previous.display(),
                root.display()
            ));
        }
    }
}

/// Directory names of the model-store layout. Shared by the path accessors
/// and by `create_model_store_dirs` so the two cannot drift apart.
const OLLAMA_STORE_DIR: &str = "ollama";
const HF_CACHE_DIR: &str = "huggingface";
const TORCH_CACHE_DIR: &str = "torch";

/// Ollama model store (`OLLAMA_MODELS`).
pub fn models_dir() -> PathBuf {
    ollama_store_in(&models_root())
}

/// Ollama store inside the warehouse. If a ready-made Ollama store itself is
/// chosen as the warehouse (`manifests` and `blobs`, e.g. `~/.ollama/models`), the weights come
/// right from it: a nested `ollama` would be empty, and the models would be downloaded again.
/// An already populated nested `ollama` wins: after a partial move of the old
/// warehouse, conflicting leftovers stay in the root, and the store would be lost through them.
pub fn ollama_store_in(root: &std::path::Path) -> PathBuf {
    let nested = root.join(OLLAMA_STORE_DIR);
    let bare = root.join("manifests").is_dir() && root.join("blobs").is_dir();
    if bare && !nested.join("manifests").is_dir() {
        root.to_path_buf()
    } else {
        nested
    }
}

/// How many models lie in the Ollama store: one manifest file — one tag.
pub fn count_ollama_models(store: &std::path::Path) -> usize {
    fn walk(dir: &std::path::Path, depth: u8) -> usize {
        // Manifests lie at the depth `registry/namespace/model/tag`; the limit
        // protects against junction loops in a foreign folder.
        if depth > 6 {
            return 0;
        }
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| match entry.file_type() {
                        Ok(kind) if kind.is_dir() => walk(&entry.path(), depth + 1),
                        Ok(kind) if kind.is_file() => 1,
                        _ => 0,
                    })
                    .sum()
            })
            .unwrap_or(0)
    }
    walk(&store.join("manifests"), 0)
}

/// `OLLAMA_MODELS` often points to `ollama` inside the Cloufam warehouse: take the
/// parent as the warehouse, otherwise ComfyUI and the Hugging Face cache would land inside the Ollama store.
fn store_root_of(path: PathBuf) -> PathBuf {
    match path.parent() {
        Some(parent) if path.file_name().is_some_and(|name| name == OLLAMA_STORE_DIR) && ollama_store_in(parent) == path => {
            parent.to_path_buf()
        }
        _ => path,
    }
}

/// Folders where Ollama models may already lie: the current warehouse, environment
/// variables, the standard Ollama store and the default warehouse. No duplicates,
/// in priority order.
pub fn model_store_candidates() -> Vec<PathBuf> {
    let mut candidates = vec![models_root()];
    candidates.extend(models_root_env());
    candidates.extend(env_dir("OLLAMA_MODELS"));
    candidates.extend(env_dir("USERPROFILE").map(|home| home.join(".ollama").join("models")));
    candidates.push(legacy_models_dir());
    candidates.extend(system_models_root());
    let mut unique: Vec<PathBuf> = Vec::new();
    for path in candidates.into_iter().map(store_root_of) {
        let key = path.to_string_lossy().to_lowercase();
        if !unique.iter().any(|seen| seen.to_string_lossy().to_lowercase() == key) {
            unique.push(path);
        }
    }
    unique
}

/// ComfyUI models: checkpoints, LoRA, encoders. The installation's `comfyui\models` folder
/// is attached to it with a junction, so both ComfyUIs (bundled and external)
/// see the same set.
pub fn comfyui_models_dir() -> PathBuf {
    models_root().join("comfyui")
}

/// Image LoRA: a trained adapter is placed right here, and ComfyUI sees it
/// without copying — the warehouse folder is attached to the installation with a junction.
pub fn comfyui_loras_dir() -> PathBuf {
    comfyui_models_dir().join("loras")
}

/// ONNX model of the WD14 tagger (dataset captions): `model.onnx` + `selected_tags.csv`.
pub fn wd14_model_dir() -> PathBuf {
    models_root().join("wd14")
}

/// The old Ollama model store (before the move to the warehouse): the `models` folder
/// next to the program.
pub fn legacy_models_dir() -> PathBuf {
    base_dir().join("models")
}

pub fn data_dir() -> PathBuf {
    base_dir().join("data")
}

pub fn memory_files_dir() -> PathBuf {
    base_dir().join("memory")
}

pub fn settings_path() -> PathBuf {
    data_dir().join("settings.json")
}

/// Temporary files of subprocesses (pip, unpacking, extension builds) —
/// inside the program folder, not in system %TEMP%.
pub fn tmp_dir() -> PathBuf {
    data_dir().join("tmp")
}

/// pip cache: without it the wheels settle in %LOCALAPPDATA%\pip\Cache.
pub fn pip_cache_dir() -> PathBuf {
    data_dir().join("pip-cache")
}

/// Hugging Face cache: bases for fine-tuning and ComfyUI nodes. Lives on the model
/// warehouse — the same place the user-level `HF_HOME` points to.
pub fn hf_cache_dir() -> PathBuf {
    models_root().join(HF_CACHE_DIR)
}

/// torch cache (pretrained heads downloaded by libraries) — these are weights too,
/// so their place is on the model warehouse, next to Ollama and Hugging Face, not in
/// the program folder: the warehouse survives a system reinstall.
pub fn torch_cache_dir() -> PathBuf {
    models_root().join(TORCH_CACHE_DIR)
}

/// Cleanup of the temp directory at startup: the files there are one-shot. The directory
/// itself stays — pip, ComfyUI and fine-tuning set TEMP/TMP to it, and without it
/// Python falls back to system %TEMP% or crashes.
pub fn cleanup_tmp() {
    reset_dir(&tmp_dir());
}

fn reset_dir(dir: &std::path::Path) {
    if dir.exists() {
        let _ = std::fs::remove_dir_all(dir);
    }
    let _ = std::fs::create_dir_all(dir);
}

/// File with the full size of the downloaded archive (next to it).
pub fn download_total_path(dest: &std::path::Path) -> PathBuf {
    let mut name = dest.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".total");
    dest.with_file_name(name)
}

/// Size of the already downloaded part of the archive: the UI shows from it that the download
/// can be resumed rather than started over.
pub fn download_partial(dest: &std::path::Path) -> (u64, u64) {
    let have = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
    let total = read_download_meta(dest).0;
    (have, total)
}

/// A note next to the stub: the full size and the address it was downloaded from.
/// The first line — the size (the format stayed the same), the second — the source.
fn read_download_meta(dest: &std::path::Path) -> (u64, Option<String>) {
    let Ok(text) = std::fs::read_to_string(download_total_path(dest)) else {
        return (0, None);
    };
    let mut lines = text.lines();
    let total = lines.next().and_then(|l| l.trim().parse::<u64>().ok()).unwrap_or(0);
    let source = lines.next().map(|l| l.trim().to_string()).filter(|s| !s.is_empty());
    (total, source)
}

fn write_download_meta(dest: &std::path::Path, total: u64, url: &str) {
    let _ = std::fs::write(download_total_path(dest), format!("{total}\n{url}"));
}

/// Full size from `Content-Length`. A chunked 206 has no length: substituting
/// the bytes already on disk is not allowed — the next attempt would take the file for a foreign
/// stub and delete it.
fn declared_download_total(resuming: bool, have: u64, content_length: Option<u64>) -> u64 {
    match (resuming, content_length) {
        (true, Some(remaining)) => have.saturating_add(remaining),
        (false, Some(len)) => len,
        (_, None) => 0,
    }
}

/// Pause and cancel act within 100 ms, even if the socket stays silent until its
/// timeout (60 s, 5 minutes for the model check).
pub(crate) async fn with_download_control<F: std::future::Future>(
    flags: &crate::downloads::Flags,
    operation: F,
) -> Result<F::Output, String> {
    tokio::pin!(operation);
    let mut control = tokio::time::interval(Duration::from_millis(100));
    control.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if let Some(reason) = flags.stop() {
            return Err(reason.to_string());
        }
        tokio::select! {
            outcome = &mut operation => return Ok(outcome),
            _ = control.tick() => {},
        }
    }
}

/// Download a file with resume.
///
/// If a part is already on disk, continue from it (`Range: bytes=<have>-`),
/// so a connection drop or an app exit no longer force downloading a
/// two-gigabyte archive again. An unfinished file is not deleted on error — it is
/// the resume point. The full size is saved next to it (`<file>.total`),
/// so that progress is visible before the first request.
pub async fn download_to_file(
    client: &reqwest::Client,
    url: &str,
    dest: &std::path::Path,
    label: &str,
    flags: crate::downloads::FlagsRef,
    on_progress: Arc<dyn Fn(u64, u64, String) + Send + Sync>,
) -> Result<u64, String> {
    let mut have = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
    // A stub from a different address is not a resume point but garbage: the file name
    // does not depend on the version (`node.zip`, `ollama.zip`), while the link does, and
    // gluing two different archives together passed all checks and broke the installation.
    let (known_total, source) = read_download_meta(dest);

    if have > 0 && source.as_deref() != Some(url) {
        // A stub without a recorded source — from a version that did not write it,
        // or one that survived a failed unpack: there is no way to check whose it is, and
        // gluing two different archives together passes all further checks.
        let _ = std::fs::remove_file(dest);
        let _ = std::fs::remove_file(download_total_path(dest));
        have = 0;
    }
    let mut request = client.get(url);
    if have > 0 {
        request = request.header("range", format!("bytes={have}-"));
    }
    let resp = with_download_control(&flags, request.send())
        .await?
        .map_err(|e| format!("Не удалось связаться с сервером: {e}"))?;
    let status = resp.status();
    if status.as_u16() == 416 && have > 0 {
        // Range beyond the file. This is "fully downloaded" only if the length
        // matched the known one: otherwise the disk holds a stub of another archive, and
        // declaring it done would wedge the installation forever.
        if known_total > 0 && have == known_total {
            let _ = std::fs::remove_file(download_total_path(dest));
            on_progress(have, have, format!("{label}: файл уже скачан"));
            return Ok(have);
        }
        let _ = std::fs::remove_file(dest);
        let _ = std::fs::remove_file(download_total_path(dest));
        return Err(format!(
            "{label}: на диске обрывок чужого файла, он удалён — повторите загрузку"
        ));
    }
    if !status.is_success() {
        return Err(format!("Сервер вернул HTTP {status}"));
    }

    let resuming = have > 0 && status.as_u16() == 206;
    let total = declared_download_total(resuming, have, resp.content_length());
    if have > 0 && !resuming {
        // The server did not support Range — start over, otherwise the file would be glued from
        // two different responses. The size of the previous attempt goes with the stub:
        // without Content-Length the new one would not be written, and the old one would lie about readiness.
        let _ = std::fs::remove_file(dest);
        let _ = std::fs::remove_file(download_total_path(dest));
    }
    if total > 0 {
        write_download_meta(dest, total, url);
        // The lack of space is visible before writing, not after gigabytes of traffic; the stub is left alone.
        let need = total.saturating_sub(if resuming { have } else { 0 });
        let free = dest.parent().and_then(crate::gpu::free_space_bytes);
        if free.is_some_and(|free| free < need) {
            return Err(format!(
                "{label}: на диске не хватает места — нужно ещё {} МБ",
                need / (1024 * 1024)
            ));
        }
    }

    let result: Result<u64, String> = async {
        use futures_util::StreamExt;
        use tokio::io::AsyncWriteExt;

        let mut file = if resuming {
            tokio::fs::OpenOptions::new()
                .append(true)
                .open(dest)
                .await
                .map_err(|e| e.to_string())?
        } else {
            tokio::fs::File::create(dest).await.map_err(|e| e.to_string())?
        };
        let mut got: u64 = if resuming { have } else { 0 };
        let note = format!("{label}…");
        let mut throttle = crate::downloads::ProgressThrottle::new();
        throttle.due(got, total, &note);
        on_progress(got, total, note.clone());
        let mut stream = resp.bytes_stream();
        loop {
            let next = with_download_control(
            &flags,
            tokio::time::timeout(Duration::from_secs(60), stream.next()),
        )
            .await;
            let chunk = match next {
                Ok(Ok(Some(Ok(chunk)))) => chunk,
                Ok(Ok(None)) => break,
                error => {
                    file.flush().await.map_err(|e| e.to_string())?;
                    return Err(match error {
                        Err(reason) => reason,
                        Ok(Err(_)) => format!("таймаут ожидания данных при загрузке ({label}, 60 с)"),
                        Ok(Ok(Some(Err(error)))) => error.to_string(),
                        _ => unreachable!(),
                    });
                }
            };
            if let Some(reason) = flags.stop() {
                // The file stays on disk: it is the resume point.
                // Flush before Err — otherwise the tokio::fs buffer loses the chunks
                // already received and the resume starts from the wrong place.
                let _ = file.flush().await;
                return Err(reason.to_string());
            }
            file.write_all(&chunk).await.map_err(|e| e.to_string())?;
            got += chunk.len() as u64;
            if throttle.due(got, total, &note) {
                on_progress(got, total, note.clone());
            }
        }
        file.flush().await.map_err(|e| e.to_string())?;
        if total > 0 && got != total {
            return Err(format!("{label}: получено {got} из {total} байт"));
        }
        Ok(got)
    }
    .await;

    match result {
        Ok(size) => {
            let _ = std::fs::remove_file(download_total_path(dest));
            Ok(size)
        }
        Err(e) => Err(e),
    }
}

pub fn characters_dir() -> PathBuf {
    let local = base_dir().join("characters");
    if local.exists() {
        return local;
    }
    let res = base_dir().join("resources").join("characters");
    if res.exists() {
        return res;
    }
    // Installer setup: resources from `../characters/*` Tauri lays out
    // next to the exe in the `_up_` subfolder; its own `characters` and
    // `resources/characters` folders are not there.
    let packaged = base_dir().join("_up_").join("characters");
    if packaged.exists() {
        return packaged;
    }
    local
}

pub fn comfyui_dir() -> PathBuf {
    base_dir().join("comfyui")
}

/// Runtime root (`runtime`): ollama, node and trainer live next to it.
pub fn runtimes_root() -> PathBuf {
    runtime_dir()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| base_dir().join("runtime"))
}

/// Fine-tuning runtime: uv, the managed Python, venv and llama.cpp sources.
pub fn trainer_dir() -> PathBuf {
    runtimes_root().join("trainer")
}

/// Fine-tuning working data: imported datasets and training runs.
pub fn training_dir() -> PathBuf {
    data_dir().join("training")
}

pub fn training_runs_dir() -> PathBuf {
    training_dir().join("runs")
}

pub fn training_import_dir() -> PathBuf {
    training_dir().join("import")
}

/// Image datasets: a subfolder per imported set.
pub fn image_training_dir() -> PathBuf {
    training_dir().join("images")
}

/// Image fine-tuning runs: the dataset config, the launch config and the log.
pub fn image_runs_dir() -> PathBuf {
    training_dir().join("image-runs")
}

/// Create all data directories if they do not exist yet.
///
/// Split in two halves on purpose (V-11): `json_store::save` calls this before
/// every write, so the program's own data dirs must never be blocked by an
/// unavailable model store (it may sit on removable/network
/// storage). Store failures go to boot.log only; program-dir failures are real
/// errors and propagate. Blocking IO on the caller's thread.
pub fn ensure_dirs() -> std::io::Result<()> {
    ensure_dirs_with(&models_root())
}

/// Same split with an explicit store root (kept testable: the default root is
/// a machine-wide global).
fn ensure_dirs_with(store_root: &std::path::Path) -> std::io::Result<()> {
    ensure_program_dirs()?;
    if let Err(error) = create_model_store_dirs(store_root) {
        crate::boot_log(&format!(
            "Склад моделей недоступен ({}): {error}. Данные приложения сохраняются.",
            store_root.display()
        ));
    }
    Ok(())
}

/// Data dirs of the program itself — required for saving stores, so errors
/// propagate to the caller.
fn ensure_program_dirs() -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir())?;
    std::fs::create_dir_all(data_dir().join("webview"))?;
    std::fs::create_dir_all(data_dir().join("ollama"))?;
    std::fs::create_dir_all(data_dir().join("npm-cache"))?;
    std::fs::create_dir_all(tmp_dir())?;
    std::fs::create_dir_all(pip_cache_dir())?;
    std::fs::create_dir_all(memory_files_dir())?;
    std::fs::create_dir_all(training_import_dir())?;
    std::fs::create_dir_all(training_runs_dir())?;
    std::fs::create_dir_all(image_training_dir())?;
    std::fs::create_dir_all(image_runs_dir())?;
    Ok(())
}

/// Model-store layout under `store_root` (weights and weight caches). Called
/// best-effort from `ensure_dirs`: the store is replaceable storage, and its
/// absence must not stop the app from saving its own JSON stores.
pub fn create_model_store_dirs(store_root: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(store_root.join(HF_CACHE_DIR))?;
    std::fs::create_dir_all(store_root.join(TORCH_CACHE_DIR))?;
    std::fs::create_dir_all(ollama_store_in(store_root))?;
    Ok(())
}

/// Unpack a zip archive, dropping the first strip path components.
pub fn extract_zip(
    zip_path: &std::path::Path,
    dest_root: &std::path::Path,
    strip: usize,
) -> Result<(), String> {
    std::fs::create_dir_all(dest_root).map_err(|e| e.to_string())?;
    let file = std::fs::File::open(zip_path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("zip повреждён: {e}"))?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
        let name = match entry.enclosed_name() {
            Some(n) => n.to_path_buf(),
            None => continue,
        };
        let parts: Vec<_> = name.components().collect();
        if parts.len() <= strip {
            continue;
        }
        // enclosed_name() cuts off escaping the root BEFORE stripping the prefix, so
        // `prefix/../evil` passes it with zero depth, and after the strip gives `../evil`.
        // Drop everything that leads outside after the strip. `./` inside a name
        // is harmless and is skipped: `tar -C dir -a -c -f pack.zip .` names
        // entries "./Name.png", and rejecting them unpacked zero files —
        // the character set did not arrive in any layout.
        let rel: PathBuf = parts[strip..]
            .iter()
            .filter(|c| matches!(c, std::path::Component::Normal(_)))
            .collect();
        let escapes = parts[strip..].iter().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        });
        if rel.as_os_str().is_empty() || escapes {
            continue;
        }
        let out = dest_root.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
        } else {
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let mut dst = std::fs::File::create(&out).map_err(|e| e.to_string())?;
            std::io::copy(&mut entry, &mut dst).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[tokio::test]
    async fn audit_silent_download_can_be_cancelled() {
        use axum::{body::Body, response::Response, routing::get, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/file", listener.local_addr().unwrap());
        let app = Router::new().route("/file", get(|| async {
            let chunks = futures_util::stream::pending::<Result<bytes::Bytes, std::io::Error>>();
            Response::new(Body::from_stream(chunks))
        }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let dest = temp_dest("audit-silent");
        let cancel = Arc::new(AtomicBool::new(false));
        let flags = crate::downloads::Flags::with_cancel(cancel.clone());
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let operation = download_to_file(&client, &url, &dest, "test", flags, progress_sink());
        tokio::pin!(operation);
        // The loader is already waiting for a silent answer when we raise the flag.
        assert!(tokio::time::timeout(Duration::from_millis(100), &mut operation).await.is_err());
        cancel.store(true, Ordering::Relaxed);
        let outcome = tokio::time::timeout(Duration::from_secs(1), &mut operation).await;
        server.abort();
        assert_eq!(outcome.expect("cancellation waited for network activity").unwrap_err(), crate::downloads::CANCELLED);
        std::fs::remove_dir_all(dest.parent().unwrap()).unwrap();
    }

    /// Fixture server state: the file body and whether a Range request came.
    struct Fixture {
        body: Vec<u8>,
        honour_range: bool,
        saw_range: AtomicBool,
    }

    async fn fixture_server(body: Vec<u8>, honour_range: bool) -> (String, Arc<Fixture>) {
        use axum::body::Body;
        use axum::extract::State;
        use axum::http::{header, HeaderMap, StatusCode};
        use axum::response::Response;
        use axum::routing::get;
        use axum::Router;

        async fn serve(State(state): State<Arc<Fixture>>, headers: HeaderMap) -> Response {
            let full = state.body.clone();
            let len = full.len();
            let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
            if range.is_some() {
                state.saw_range.store(true, Ordering::Relaxed);
            }
            if state.honour_range {
                if let Some(rest) = range.and_then(|r| r.strip_prefix("bytes=")) {
                    let start: usize = rest.trim_end_matches('-').parse().unwrap_or(0);
                    if start >= len {
                        return Response::builder()
                            .status(StatusCode::RANGE_NOT_SATISFIABLE)
                            .body(Body::empty())
                            .unwrap();
                    }
                    let slice = full[start..].to_vec();
                    return Response::builder()
                        .status(StatusCode::PARTIAL_CONTENT)
                        .header(header::CONTENT_LENGTH, slice.len())
                        .body(Body::from(slice))
                        .unwrap();
                }
            }
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_LENGTH, len)
                .body(Body::from(full))
                .unwrap()
        }

        let state = Arc::new(Fixture {
            body,
            honour_range,
            saw_range: AtomicBool::new(false),
        });
        let app = Router::new().route("/file", get(serve)).with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}/file"), state)
    }

    fn temp_dest(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lk-download-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("archive.bin")
    }

    fn progress_sink() -> Arc<dyn Fn(u64, u64, String) + Send + Sync> {
        Arc::new(|_, _, _| {})
    }

    #[test]
    fn test_chunked_resume_does_not_invent_a_total() {
        assert_eq!(declared_download_total(true, 4_000_000, None), 0);
        assert_eq!(declared_download_total(true, 100, Some(50)), 150);
        assert_eq!(declared_download_total(false, 0, Some(80)), 80);
    }

    /// Resume: half lies on disk, the loader continues from it instead of
    /// starting over — exactly what used to break the ComfyUI installation.
    #[tokio::test]
    async fn test_download_to_file_resumes_from_partial() {
        let payload: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        let dest = temp_dest("resume");
        std::fs::write(&dest, &payload[..2048]).unwrap();
        let (url, fixture) = fixture_server(payload.clone(), true).await;
        // The loader discards a stub without a recorded source as foreign.
        write_download_meta(&dest, payload.len() as u64, &url);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();

        let size = download_to_file(
            &client,
            &url,
            &dest,
            "тест",
            crate::downloads::Flags::new(),
            progress_sink(),
        )
        .await
        .unwrap();

        assert_eq!(size, payload.len() as u64);
        assert_eq!(std::fs::read(&dest).unwrap(), payload, "файл склеился неверно");
        assert!(fixture.saw_range.load(Ordering::Relaxed), "Range не запрошен — качали заново");
        assert!(!download_total_path(&dest).exists(), "файл размера остался после успеха");
        let _ = std::fs::remove_dir_all(dest.parent().unwrap());
    }

    /// Server without Range support: the file starts over rather than being appended
    /// by a second response (otherwise the archive would be broken).
    #[tokio::test]
    async fn test_download_to_file_restarts_when_server_ignores_range() {
        let payload: Vec<u8> = (0..3000u32).map(|i| (i % 97) as u8).collect();
        let dest = temp_dest("restart");
        std::fs::write(&dest, &payload[..1500]).unwrap();
        let (url, fixture) = fixture_server(payload.clone(), false).await;
        write_download_meta(&dest, 9999, &url);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();

        let size = download_to_file(
            &client,
            &url,
            &dest,
            "тест",
            crate::downloads::Flags::new(),
            progress_sink(),
        )
        .await
        .unwrap();

        assert_eq!(size, payload.len() as u64);
        assert_eq!(std::fs::read(&dest).unwrap(), payload, "файл не перезаписан целиком");
        assert!(fixture.saw_range.load(Ordering::Relaxed), "огрызок не дошёл до докачки");
        let _ = std::fs::remove_dir_all(dest.parent().unwrap());
    }

    /// The partial file and its full size are visible to the UI before the download starts.
    #[tokio::test]
    async fn test_download_partial_reports_progress_and_total() {
        let dest = temp_dest("partial");
        std::fs::write(&dest, vec![0u8; 1234]).unwrap();
        std::fs::write(download_total_path(&dest), "9999").unwrap();

        let (have, total) = download_partial(&dest);
        assert_eq!(have, 1234);
        assert_eq!(total, 9999);

        // The server marks an already fully downloaded file with 416 — that is success, not an error.
        let payload = vec![7u8; 64];
        let dest_ok = temp_dest("complete");
        std::fs::write(&dest_ok, &payload).unwrap();
        let (url, _fixture) = fixture_server(payload.clone(), true).await;
        write_download_meta(&dest_ok, payload.len() as u64, &url);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let size = download_to_file(
            &client,
            &url,
            &dest_ok,
            "тест",
            crate::downloads::Flags::new(),
            progress_sink(),
        )
        .await
        .unwrap();
        assert_eq!(size, payload.len() as u64);
        assert_eq!(std::fs::read(&dest_ok).unwrap(), payload);
        let _ = std::fs::remove_dir_all(dest.parent().unwrap());
        let _ = std::fs::remove_dir_all(dest_ok.parent().unwrap());
    }

    /// Weights and weight caches live only on the model warehouse: it survives
    /// a system reinstall and is shared by all tools on the machine.
    #[test]
    fn test_model_caches_live_on_the_shared_store() {
        let root = models_root();
        for dir in [
            models_dir(),
            comfyui_models_dir(),
            comfyui_loras_dir(),
            wd14_model_dir(),
            hf_cache_dir(),
            torch_cache_dir(),
        ] {
            assert!(
                dir.starts_with(&root),
                "{} должен лежать под складом {}",
                dir.display(),
                root.display()
            );
        }
        // Application data — on the contrary, next to the program.
        assert!(!settings_path().starts_with(&root) || base_dir().starts_with(&root));
    }

    /// `tar -C dir -a -c -f pack.zip .` names entries "./Name.png". While such
    /// names were rejected, the character set unpacked to zero files —
    /// and the empty directory forever shadowed the built-in cards.
    #[test]
    fn test_extract_zip_accepts_dot_slash_names() {
        use std::io::Write;
        let dir = std::env::temp_dir().join("lk-zip-dot-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let archive_path = dir.join("pack.zip");

        {
            let file = std::fs::File::create(&archive_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            zip.add_directory("./", options).unwrap();
            zip.start_file("./Alice_Carter.png", options).unwrap();
            zip.write_all(b"card").unwrap();
            zip.start_file("../escape.png", options).unwrap();
            zip.write_all(b"evil").unwrap();
            zip.finish().unwrap();
        }

        let out = dir.join("out");
        extract_zip(&archive_path, &out, 0).unwrap();
        assert!(out.join("Alice_Carter.png").exists(), "карточка распакована");
        assert!(!dir.join("escape.png").exists(), "выход за корень запрещён");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The temp directory cleanup leaves it empty rather than deleted: subprocess
    /// TEMP/TMP point at it. A missing directory is created too.
    #[test]
    fn test_reset_dir_keeps_empty_dir() {
        let dir = std::env::temp_dir().join("lk-reset-dir-test");
        let _ = std::fs::remove_dir_all(&dir);
        reset_dir(&dir);
        assert!(dir.is_dir(), "отсутствующий каталог создан");

        std::fs::create_dir_all(dir.join("pip-build")).unwrap();
        std::fs::write(dir.join("pip-build").join("x.whl"), b"x").unwrap();
        reset_dir(&dir);
        assert!(dir.is_dir(), "каталог остался");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "содержимое очищено");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// V-11: an unavailable model store must not block `ensure_dirs` — the call
    /// `json_store::save` makes before every write. The store root below is
    /// blocked by a regular file, so every store dir creation really fails.
    #[test]
    fn test_unavailable_model_store_does_not_block_data_dirs() {
        let dir = std::env::temp_dir().join("lk-ensure-dirs-store-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("not-a-dir");
        std::fs::write(&blocker, b"x").unwrap();
        let blocked_store = blocker.join("store");

        assert!(
            create_model_store_dirs(&blocked_store).is_err(),
            "склад в тесте действительно недоступен"
        );
        assert!(
            ensure_dirs_with(&blocked_store).is_ok(),
            "недоступный склад не блокирует подготовку каталогов данных"
        );
        assert!(data_dir().is_dir(), "каталоги данных программы созданы");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// V-12: the store root is re-assignable — `retry_boot` calls
    /// `set_models_root` again, and the corrected value used to be swallowed by
    /// a `OnceLock` no-op until the next full restart.
    #[test]
    fn test_models_root_state_keeps_the_last_value() {
        let state = ModelsRootState(RwLock::new(None));
        assert_eq!(state.get(), None, "до настройки корня действует дефолтный поиск");
        assert_eq!(state.set(PathBuf::from("Z:\\first")), None);
        assert_eq!(state.get(), Some(PathBuf::from("Z:\\first")));
        assert_eq!(
            state.set(PathBuf::from("Z:\\second")),
            Some(PathBuf::from("Z:\\first")),
            "повторная установка не no-op: прежнее значение возвращается"
        );
        assert_eq!(state.get(), Some(PathBuf::from("Z:\\second")));
    }

    /// A ready-made Ollama store is taken in place, not as a nested empty `ollama`;
    /// a populated nested one wins over leftovers in the root.
    #[test]
    fn ollama_store_layouts() {
        let dir = std::env::temp_dir().join(format!("lk-store-layout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let fresh = dir.join("fresh");
        std::fs::create_dir_all(&fresh).unwrap();
        assert_eq!(ollama_store_in(&fresh), fresh.join("ollama"));

        let bare = dir.join("bare");
        let manifest = bare.join("manifests").join("registry.ollama.ai").join("library").join("qwen3");
        std::fs::create_dir_all(&manifest).unwrap();
        std::fs::create_dir_all(bare.join("blobs")).unwrap();
        std::fs::write(manifest.join("8b"), "{}").unwrap();
        std::fs::write(manifest.join("4b"), "{}").unwrap();
        assert_eq!(ollama_store_in(&bare), bare);
        assert_eq!(count_ollama_models(&ollama_store_in(&bare)), 2);
        create_model_store_dirs(&bare).unwrap();
        assert!(!bare.join("ollama").exists(), "в готовом хранилище не заводим вложенное");

        std::fs::create_dir_all(bare.join("ollama").join("manifests")).unwrap();
        assert_eq!(ollama_store_in(&bare), bare.join("ollama"));
        assert_eq!(store_root_of(bare.join("ollama")), bare, "вложенная ollama → сам склад");
        assert_eq!(store_root_of(fresh.clone()), fresh);

        let _ = std::fs::remove_dir_all(&dir);
    }

}

