//! LoRA fine-tuning for image models: presets from checkpoints on disk,
//! a job runner around the embedded `scripts/train_image_lora.py` (kohya sd-scripts),
//! dataset import, WD14 captions and parsing of `@@LK@@` markers.
//!
//! The environment (venv-sd with the sd-scripts stack and its clone in `runtime/trainer`)
//! is installed by hand — see the journal, spike 13.09.2026. The app only
//! uses it: the training command repeats the verified run (SDXL, dim 8,
//! UNet only, latent and text-encoder output caches).
//!
//! The finished LoRA is placed directly into `comfyui_loras_dir()` — the model store folder,
//! which ComfyUI sees through a junction, so the result is available there without
//! copying (the SillyTavern image extension has no LoRA field, the adapter must be
//! selected in ComfyUI or in ST's own workflow).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

use tauri::{AppHandle, Emitter};

use crate::gpu::HardwareTier;

/// Repeats the verified spike: dim 8 / alpha 4, UNet only, AdamW8bit, lr 1e-4.
const NETWORK_DIM: u32 = 8;
const NETWORK_ALPHA: u32 = 4;
const LEARNING_RATE: f64 = 1e-4;
const SEED: u32 = 42;
const REPEAT_RANGE: (u32, u32) = (1, 100);

pub const IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "webp", "bmp"];

/// Checkpoint family: the kohya entry point, bucket grid and tier depend on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFamily {
    Sd15,
    Sdxl,
}

impl ImageFamily {
    pub fn id(self) -> &'static str {
        match self {
            ImageFamily::Sd15 => "sd15",
            ImageFamily::Sdxl => "sdxl",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ImageFamily::Sd15 => "SD 1.5",
            ImageFamily::Sdxl => "SDXL",
        }
    }

    /// Minimum tier: SD 1.5 trains from 8 GB, SDXL from 11 GB (1024px activations
    /// do not fit in 8 GB even with latent caching and checkpointing).
    pub fn min_tier(self) -> HardwareTier {
        match self {
            ImageFamily::Sd15 => HardwareTier::Low,
            ImageFamily::Sdxl => HardwareTier::Mid,
        }
    }
}

/// Training resolution: native for the family, trimmed by memory. SDXL on the lower
/// tier (11 GB) trains at 768px — 1024px hits the VRAM limit there.
pub fn resolution_for(family: ImageFamily, tier: HardwareTier) -> u32 {
    match (family, tier) {
        (ImageFamily::Sd15, _) => 512,
        (ImageFamily::Sdxl, HardwareTier::High) => 1024,
        (ImageFamily::Sdxl, _) => 768,
    }
}

/// Steady-state step speed.
///
/// The calibration is live, from three runs on an RTX 5080 (High tier, SDXL, dim 8,
/// UNet only, latent and text-encoder output caches):
///   • spike 13.09.2026, 512px, 20 steps in 57 s. The 2.9 s/step recorded then is
///     the run average: the first step costs about 40 s (8-bit optimizer init
///     and CUDA warm-up), then ~1 s/step;
///   • embedded script run, 512px, 2 steps: first 39 s, second 1.2 s;
///   • app run, 12 steps, 116 s total, kohya showed 4.77 s/step.
/// Hence 1.1 s/step at 512px and quadratic growth by frame area. The measurements
/// were made on 512px images, and `bucket_no_upscale` does not upscale small ones —
/// on such a dataset the real time is less than the estimate; exact numbers come
/// from the run itself.
pub fn estimate_seconds_per_step(tier: HardwareTier, family: ImageFamily, resolution: u32) -> f64 {
    const MEASURED_SDXL_512_STEADY: f64 = 1.1;
    let area = (resolution as f64 / 512.0).powi(2);
    // The SD 1.5 UNet is three times smaller than SDXL; 0.4 leaves room for overhead.
    let family_factor = match family {
        ImageFamily::Sdxl => 1.0,
        ImageFamily::Sd15 => 0.4,
    };
    let card_factor = match tier {
        HardwareTier::High => 1.0,
        HardwareTier::Mid => 1.5,
        HardwareTier::Low => 2.6,
        HardwareTier::Minimal => 6.0,
    };
    (MEASURED_SDXL_512_STEADY * area * family_factor * card_factor).max(0.2)
}

/// Whole-run estimate: steps plus one-off start costs — checkpoint load,
/// latent and encoder output caches, the first (most expensive) step.
pub fn estimate_total_seconds(
    tier: HardwareTier,
    family: ImageFamily,
    resolution: u32,
    steps: u32,
) -> f64 {
    const STARTUP_SECONDS: f64 = 60.0;
    STARTUP_SECONDS + estimate_seconds_per_step(tier, family, resolution) * steps.max(1) as f64
}

fn venv_sd_python() -> PathBuf {
    crate::paths::trainer_dir()
        .join("venv-sd")
        .join("Scripts")
        .join("python.exe")
}

fn sd_scripts_dir() -> PathBuf {
    crate::paths::trainer_dir().join("sd-scripts")
}

#[cfg(windows)]
fn hide_console(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn hide_console(_cmd: &mut Command) {}

fn run_id_now() -> String {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    // Seconds alone — two starts in the same second shared a run directory and
    // overwrote each other's train-config.json.
    format!("img-{}-{:09}-{}", now.as_secs(), now.subsec_nanos(), n)
}

/// Preset id from the checkpoint file name: Latin letters, no spaces.
fn preset_id(file_name: &str, taken: &[String]) -> String {
    let stem = Path::new(file_name)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| file_name.to_string());
    let base = crate::text_lora::slug(&stem);
    if !taken.iter().any(|id| id == &base) {
        return base;
    }
    let mut index = 2;
    while taken.iter().any(|id| id == &format!("{base}-{index}")) {
        index += 1;
    }
    format!("{base}-{index}")
}

/// Family by tensor composition, not by file name: SDXL checkpoints have two
/// text encoders (`conditioner.embedders.1`) and a class-size embedding
/// (`label_emb`), which SD 1.5 never has. Only the safetensors header is read —
/// the weights are not touched.
fn family_from_header(path: &Path) -> Option<ImageFamily> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut size = [0u8; 8];
    file.read_exact(&mut size).ok()?;
    let length = u64::from_le_bytes(size).min(16 * 1024 * 1024) as usize;
    let mut header = vec![0u8; length];
    file.read_exact(&mut header).ok()?;
    let text = String::from_utf8_lossy(&header);
    if text.contains("conditioner.embedders.1") || text.contains("label_emb") {
        Some(ImageFamily::Sdxl)
    } else {
        Some(ImageFamily::Sd15)
    }
}

/// Checkpoint family: the file header, and on failure — a hint in the name.
/// Neither path guesses silently: an unknown format (.ckpt) is identified by
/// name, and a miss is immediately visible — kohya fails on an architecture mismatch.
fn detect_family(path: &Path, file_name: &str) -> ImageFamily {
    family_from_header(path).unwrap_or_else(|| family_from_name(file_name))
}

/// Fallback family signal when the header is not parsed (not safetensors):
/// the file name. The same boundary as `detect_family`, so the name has a single
/// interpretation (V-23).
fn family_from_name(file_name: &str) -> ImageFamily {
    if file_name.to_ascii_lowercase().contains("xl") {
        ImageFamily::Sdxl
    } else {
        ImageFamily::Sd15
    }
}

/// Checkpoint path by name: the same directories `comfyui::scan_local_checkpoints` walks.
fn checkpoint_path(file_name: &str) -> Option<PathBuf> {
    [
        crate::comfyui::checkpoints_dir(),
        crate::paths::comfyui_dir().join("models").join("checkpoints"),
        crate::paths::models_dir().join("checkpoints"),
    ]
    .into_iter()
    .map(|dir| dir.join(file_name))
    .find(|path| path.is_file())
}

/// Family cache keyed by "name + size + file mtime" (V-52): a safetensors
/// header of up to 16 MB is read once per file, not on every estimate call —
/// the UI calls it on every character typed into the "steps" field. Size and mtime
/// in the key: replacing the file in place rereads the header instead of returning stale data.
type FamilyCacheKey = (String, u64, Option<SystemTime>);
static FAMILY_CACHE: Mutex<Option<HashMap<FamilyCacheKey, ImageFamily>>> = Mutex::new(None);

fn cached_family_with(key: Option<FamilyCacheKey>, detect: impl FnOnce() -> ImageFamily) -> ImageFamily {
    let Some(key) = key else {
        return detect();
    };
    if let Ok(guard) = FAMILY_CACHE.lock() {
        if let Some(family) = guard.as_ref().and_then(|map| map.get(&key)) {
            return *family;
        }
    }
    let family = detect();
    if let Ok(mut guard) = FAMILY_CACHE.lock() {
        guard.get_or_insert_with(HashMap::new).insert(key, family);
    }
    family
}

/// Checkpoint family on disk with a header cache.
fn cached_family(path: &Path, file_name: &str) -> ImageFamily {
    let key = std::fs::metadata(path)
        .ok()
        .map(|meta| (file_name.to_string(), meta.len(), meta.modified().ok()));
    cached_family_with(key, || detect_family(path, file_name))
}

/// Single checkpoint family signal for all consumers (V-23).
///
/// Authoritative — the file header on disk (`family_from_header`), fallback
/// when it is missing or the format is not parsed — the name (`family_from_name`).
/// The same signal is used by the training time estimate and by the generation resolution
/// in `sillytavern::sd_target_size`: there must not be two sources of truth about the family,
/// otherwise an SD 1.5 with a name outside the substring list got an SDXL frame.
pub fn family_for_checkpoint(file_name: &str) -> ImageFamily {
    match checkpoint_path(file_name) {
        Some(path) => cached_family(&path, file_name),
        None => family_from_name(file_name),
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ImageTrainPreset {
    pub id: String,
    pub name: String,
    /// Checkpoint file name on disk.
    pub file_name: String,
    pub path: String,
    pub family: String,
    pub family_label: String,
    pub description: String,
    pub resolution: u32,
    pub size_gb: f32,
    pub min_tier: HardwareTier,
    pub recommended: bool,
}

/// Checkpoint store fingerprint for the preset cache: names and sizes are the cheap
/// directory walk, while file headers are expensive, reading up to 16 MB per file.
type StoreFingerprint = Vec<(String, u64)>;

struct PresetCache {
    store: StoreFingerprint,
    /// `image_model` lives in settings and changes the `recommended` flag, so
    /// the settings.json mtime is part of the key (invalidation on settings change).
    settings_mtime: Option<SystemTime>,
    tier: HardwareTier,
    presets: Vec<ImageTrainPreset>,
}

static PRESET_CACHE: Mutex<Option<PresetCache>> = Mutex::new(None);

/// Preset list with a cache (V-52): the useful work of the call is arithmetic over an already
/// built list, while `Settings::load()` + reading checkpoint headers
/// repeat only when the store (fingerprint), settings (file mtime)
/// or hardware tier change. The lock is held during the build: parallel command calls
/// get one result instead of two identical builds.
fn cached_presets(
    store: &StoreFingerprint,
    settings_mtime: Option<SystemTime>,
    tier: HardwareTier,
    build: impl FnOnce() -> Vec<ImageTrainPreset>,
) -> Vec<ImageTrainPreset> {
    let Ok(mut guard) = PRESET_CACHE.lock() else {
        // A poisoned lock does not crash the estimate: build the list without the cache.
        return build();
    };
    if let Some(cache) = guard.as_ref() {
        if cache.tier == tier && cache.store == *store && cache.settings_mtime == settings_mtime {
            return cache.presets.clone();
        }
    }
    let presets = build();
    *guard = Some(PresetCache {
        store: store.clone(),
        settings_mtime,
        tier,
        presets: presets.clone(),
    });
    presets
}

/// Presets are image checkpoints already on disk: LoRA trains on the same
/// file the user generates with, so there is no separate base directory.
///
/// Entry point: synchronous main-thread commands (`image_training_presets`,
/// `image_training_estimate`) — the UI calls the estimate on every character in the
/// "steps" field, so the path must be cheap (cache, V-52).
pub fn presets() -> Vec<ImageTrainPreset> {
    let tier = crate::gpu::gpu_info().tier;
    let store = crate::comfyui::scan_local_checkpoints();
    let mut fingerprint = store.clone();
    fingerprint.sort();
    let settings_mtime = std::fs::metadata(crate::paths::settings_path())
        .and_then(|m| m.modified())
        .ok();
    cached_presets(&fingerprint, settings_mtime, tier, || {
        build_presets(tier, &store)
    })
}

/// Building the preset list from a store snapshot: the expensive part (checkpoint
/// headers) is covered by the family cache.
fn build_presets(tier: HardwareTier, store: &[(String, u64)]) -> Vec<ImageTrainPreset> {
    let active = crate::settings::Settings::load().image_model;
    let mut ids: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for (file_name, bytes) in store {
        if !crate::comfyui::is_image_checkpoint(file_name) {
            continue;
        }
        let Some(path) = checkpoint_path(file_name) else {
            continue;
        };
        let family = cached_family(&path, file_name);
        let id = preset_id(file_name, &ids);
        ids.push(id.clone());
        let description = match family {
            ImageFamily::Sdxl => format!(
                "SDXL, обучение на {}px, нужно от 11 ГБ видеопамяти. Адаптер ложится на этот же чекпоинт — в ComfyUI он виден сразу.",
                resolution_for(family, tier)
            ),
            ImageFamily::Sd15 => format!(
                "SD 1.5, обучение на {}px, хватает 8 ГБ видеопамяти. Быстрее SDXL, но детализация ниже.",
                resolution_for(family, tier)
            ),
        };
        out.push(ImageTrainPreset {
            id,
            name: Path::new(file_name)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| file_name.clone()),
            file_name: file_name.clone(),
            path: path.to_string_lossy().to_string(),
            family: family.id().to_string(),
            family_label: family.label().to_string(),
            description,
            resolution: resolution_for(family, tier),
            size_gb: *bytes as f32 / (1024.0 * 1024.0 * 1024.0),
            min_tier: family.min_tier(),
            recommended: file_name.eq_ignore_ascii_case(&active),
        });
    }
    // If the active checkpoint is not in the list — recommend the highest in the family
    // (the largest SDXL): otherwise the choice would depend on file order on disk.
    if !out.iter().any(|p| p.recommended) {
        let fallback = out
            .iter()
            .enumerate()
            .filter(|(_, p)| p.family == ImageFamily::Sdxl.id())
            .max_by(|a, b| a.1.size_gb.total_cmp(&b.1.size_gb))
            .map(|(index, _)| index);
        if let Some(index) = fallback {
            out[index].recommended = true;
        }
    }
    out
}

pub fn preset_by_id(id: &str) -> Option<ImageTrainPreset> {
    presets().into_iter().find(|p| p.id == id)
}

impl ImageTrainPreset {
    pub fn family_kind(&self) -> ImageFamily {
        if self.family == ImageFamily::Sd15.id() {
            ImageFamily::Sd15
        } else {
            ImageFamily::Sdxl
        }
    }
}

/// Environment readiness in parts: the UI needs to show exactly what is
/// missing, while `availability()` gives only the first failure reason.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ImageRuntime {
    pub cuda: bool,
    pub venv: bool,
    pub sd_scripts: bool,
    pub checkpoints: usize,
    pub tagger: bool,
    pub reason: Option<String>,
}

/// The onnxruntime probe runs python; the environment does not change during a session,
/// so the result is kept until process exit.
fn tagger_ready() -> bool {
    // Importing onnxruntime could hang and hold the OnceLock forever: the status
    // is polled every 2 seconds and all calls queued behind it. Cache with a TTL,
    // the probe is capped at 10 seconds and kills the child.
    static READY: Mutex<Option<(Instant, bool)>> = Mutex::new(None);
    const TTL: std::time::Duration = std::time::Duration::from_secs(60);
    if let Ok(guard) = READY.lock() {
        if let Some((at, ready)) = guard.as_ref() {
            if at.elapsed() < TTL {
                return *ready;
            }
        }
    }
    let ready = probe_tagger();
    if let Ok(mut guard) = READY.lock() {
        *guard = Some((Instant::now(), ready));
    }
    ready
}

fn probe_tagger() -> bool {
    let python = venv_sd_python();
    if !python.exists() {
        return false;
    }
    let mut cmd = Command::new(python);
    cmd.args(["-c", "import onnx, onnxruntime"])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    hide_console(&mut cmd);
    let Ok(mut child) = cmd.spawn() else {
        return false;
    };
    for _ in 0..100 {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    false
}

/// Whether image training is running (for cross-checking trainers).
pub fn is_running() -> bool {
    JOB.lock()
        .ok()
        .and_then(|guard| {
            guard
                .as_ref()
                .map(|job| job.state.lock().map(|s| s.running).unwrap_or(false))
        })
        .unwrap_or(false)
}

pub fn runtime() -> ImageRuntime {
    let gpu = crate::gpu::gpu_info();
    let cuda = matches!(gpu.backend, crate::gpu::GpuBackend::Cuda);
    let venv = venv_sd_python().exists();
    let sd_scripts = sd_scripts_dir().join("sdxl_train_network.py").exists();
    // Names only: `presets()` reads each checkpoint header, while the state
    // is polled every two seconds while the tab is open.
    let checkpoints = crate::comfyui::scan_local_checkpoints()
        .iter()
        .filter(|(name, _)| crate::comfyui::is_image_checkpoint(name))
        .count();
    let tagger = tagger_ready();
    let reason = if !cuda {
        Some("Нужна видеокарта NVIDIA: обучение идёт на kohya с torch cu128. На AMD и на процессоре этого пути нет.".to_string())
    } else if gpu.tier == HardwareTier::Minimal {
        Some("Не хватает видеопамяти: SD 1.5 обучается от 8 ГБ, SDXL — от 11 ГБ.".to_string())
    } else if !venv {
        Some(format!(
            "Нет окружения дообучения картинок: ожидается {} (Python 3.11 со стеком sd-scripts).",
            venv_sd_python().display()
        ))
    } else if !sd_scripts {
        Some(format!(
            "Нет клона sd-scripts: ожидается {}.",
            sd_scripts_dir().join("sdxl_train_network.py").display()
        ))
    } else if checkpoints == 0 {
        Some("Нет ни одного чекпоинта картинок: скачайте модель на вкладке «Изображения».".to_string())
    } else {
        None
    };
    ImageRuntime {
        cuda,
        venv,
        sd_scripts,
        checkpoints,
        tagger,
        reason,
    }
}

pub fn availability() -> Result<(), String> {
    match runtime().reason {
        Some(reason) => Err(reason),
        None => Ok(()),
    }
}

/// WD14 captioning progress: the caption command is async, and the state is read
/// by the UI — a separate slot so it does not disturb the training state.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CaptionProgress {
    pub running: bool,
    pub done: u32,
    pub total: u32,
    pub stage: String,
}

static CAPTION: Mutex<Option<CaptionProgress>> = Mutex::new(None);

fn set_caption(update: impl FnOnce(&mut CaptionProgress)) {
    if let Ok(mut guard) = CAPTION.lock() {
        update(guard.get_or_insert_with(CaptionProgress::default));
    }
}

fn caption_state() -> CaptionProgress {
    CAPTION
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
        .unwrap_or_default()
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ImageDatasetInfo {
    pub slug: String,
    pub path: String,
    pub images: u32,
    pub captions: u32,
}

/// "image + .txt" pairs: kohya takes the caption from the file next to the image.
pub fn count_pairs(dir: &Path) -> (u32, u32) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    let mut images = 0;
    let mut captions = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !IMAGE_EXTENSIONS.contains(&ext.as_str()) {
            continue;
        }
        images += 1;
        if path.with_extension("txt").is_file() {
            captions += 1;
        }
    }
    (images, captions)
}

/// Free dataset name: `slug`, then `slug-2`, `slug-3`, … Creation
/// of the directory is atomic — two concurrent imports do not pick the same folder.
fn fresh_dataset_dir(root: &Path, slug: &str) -> Result<PathBuf, String> {
    for index in 1..=1000u32 {
        let name = if index == 1 {
            slug.to_string()
        } else {
            format!("{slug}-{index}")
        };
        let dest = root.join(&name);
        match std::fs::create_dir(&dest) {
            Ok(()) => return Ok(dest),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("слишком много датасетов с таким именем: переименуйте папку".into())
}

/// Import an image folder: copy only images and adjacent `.txt` captions,
/// the rest of the source folder's contents are left untouched.
fn copy_dataset(source: &Path, root: &Path) -> Result<ImageDatasetInfo, String> {
    copy_dataset_with(source, root, |from, to| std::fs::copy(from, to))
}

// Blocking import; the copier is swapped in the failure-mid-import test.
fn copy_dataset_with(
    source: &Path,
    root: &Path,
    mut copy_file: impl FnMut(&Path, &Path) -> std::io::Result<u64>,
) -> Result<ImageDatasetInfo, String> {
    if !source.is_dir() {
        return Err(format!("папка не найдена: {}", source.display()));
    }
    let name = source
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "images".to_string());
    let slug = crate::text_lora::slug(&name);
    // Re-importing the same folder does not mix files into an existing dataset:
    // text import has long gone to `-2`, while images were written on top.
    let dest = fresh_dataset_dir(root, &slug)?;
    let result = (|| {
    // The marker hides an incomplete import from datasets(), including after a crash.
    let pending = dest.join(".importing");
    std::fs::write(&pending, b"").map_err(|e| e.to_string())?;
    let slug = dest
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or(slug);

    let entries = std::fs::read_dir(source).map_err(|e| e.to_string())?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !IMAGE_EXTENSIONS.contains(&ext.as_str()) && ext != "txt" {
            continue;
        }
        let Some(file_name) = path.file_name() else {
            continue;
        };
        copy_file(&path, &dest.join(file_name))
            .map_err(|e| format!("не удалось скопировать {}: {e}", path.display()))?;
    }

    let (images, captions) = count_pairs(&dest);
    if images == 0 {
        return Err("в папке нет картинок: подойдут png, jpg, jpeg, webp и bmp".into());
    }
    std::fs::remove_file(pending).map_err(|e| e.to_string())?;
    Ok(ImageDatasetInfo {
        slug,
        path: dest.to_string_lossy().to_string(),
        images,
        captions,
    })
    })();
    if let Err(error) = &result {
        if let Err(cleanup) = std::fs::remove_dir_all(&dest) {
            return Err(format!("{error}; не удалось удалить незавершённый импорт {}: {cleanup}", dest.display()));
        }
    }
    result
}

/// Import an image folder into `data/training/images/<slug>`.
pub fn import(source: &Path) -> Result<ImageDatasetInfo, String> {
    let root = crate::paths::image_training_dir();
    sweep_stale_imports(&root);
    copy_dataset(source, &root)
}

/// An import interrupted by exit or crash leaves a folder with `.importing`: the list
/// hides it, but it still takes space. Older than a day — definitely not a running import.
fn sweep_stale_imports(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let marker = entry.path().join(".importing");
        let stale = std::fs::metadata(&marker)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > std::time::Duration::from_secs(24 * 3600));
        if stale {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Already imported image datasets.
pub fn datasets() -> Vec<ImageDatasetInfo> {
    let root = crate::paths::image_training_dir();
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out: Vec<ImageDatasetInfo> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir() && !entry.path().join(".importing").exists())
        .filter_map(|entry| {
            let path = entry.path();
            let slug = path.file_name()?.to_string_lossy().to_string();
            let (images, captions) = count_pairs(&path);
            (images > 0).then(|| ImageDatasetInfo {
                slug,
                path: path.to_string_lossy().to_string(),
                images,
                captions,
            })
        })
        .collect();
    out.sort_by(|a, b| a.slug.cmp(&b.slug));
    out
}

pub fn resolve_dataset(slug: &str) -> Result<PathBuf, String> {
    let dir = crate::paths::image_training_dir().join(crate::text_lora::slug(slug));
    if dir.is_dir() {
        Ok(dir)
    } else {
        Err(format!("датасет не найден: {slug}"))
    }
}

/// WD14 model layout expected by sd-scripts: `<model_dir>/<repo_id with "/" → "_">`.
/// Here it is stored as a flat pair of files (`Z:\AImodels\wd14`), so we prepare
/// a subdirectory with a hard link — 450 MB is not used a second time.
fn ensure_wd14_model() -> Result<PathBuf, String> {
    const REPO: &str = "SmilingWolf/wd-v1-4-convnext-tagger-v2";
    let root = crate::paths::wd14_model_dir();
    let dir = root.join(REPO.replace('/', "_"));
    if dir.join("model.onnx").is_file() {
        return Ok(dir);
    }
    let model = root.join("model.onnx");
    let tags = root.join("selected_tags.csv");
    if !model.is_file() || !tags.is_file() {
        return Err(format!(
            "модель WD14 не найдена: положите model.onnx и selected_tags.csv в {}",
            root.display()
        ));
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    if std::fs::hard_link(&model, dir.join("model.onnx")).is_err() {
        std::fs::copy(&model, dir.join("model.onnx")).map_err(|e| e.to_string())?;
    }
    std::fs::copy(&tags, dir.join("selected_tags.csv")).map_err(|e| e.to_string())?;
    Ok(dir)
}

/// Read the child process stream line by line. There are two delimiters: kohya
/// redraws the progress bar with `\r`, and without splitting on it progress
/// becomes visible only at the end of the run. Bytes are decoded with replacements —
/// an unexpected code page must not cut the log reading short.
fn read_lines(mut stream: impl Read, mut on_line: impl FnMut(&str)) {
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => break,
        };
        pending.extend_from_slice(&chunk[..read]);
        while let Some(pos) = pending.iter().position(|b| *b == b'\r' || *b == b'\n') {
            let line: Vec<u8> = pending.drain(..=pos).collect();
            // Skip \n after \r in the \r\n sequence
            if line.last() == Some(&b'\r') && pending.first() == Some(&b'\n') {
                pending.remove(0);
            }
            on_line(&String::from_utf8_lossy(&line[..line.len() - 1]));
        }
    }
    if !pending.is_empty() {
        on_line(&String::from_utf8_lossy(&pending));
    }
}

/// Captions present in the dataset: a caption is a `.txt` next to an image.
pub fn caption_dataset(dataset: &str) -> Result<String, String> {
    // The check and setting of `running` are separated by dozens of preparation lines:
    // without a shared lock two clicks started two WD14 runs on one dataset.
    let start_guard = crate::trainer::START_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if caption_state().running {
        return Err("подпись уже идёт — дождитесь окончания.".into());
    }
    // Training reads the same .txt files: captioning over a running job is pointless.
    if is_running() {
        return Err("Идёт обучение картинок — подпись после него.".into());
    }
    let python = venv_sd_python();
    if !python.exists() {
        return Err(format!("не найден python окружения: {}", python.display()));
    }
    let tagger = sd_scripts_dir()
        .join("finetune")
        .join("tag_images_by_wd14_tagger.py");
    if !tagger.exists() {
        return Err(format!("не найден скрипт подписи: {}", tagger.display()));
    }
    if !tagger_ready() {
        return Err("В окружении нет onnxruntime и onnx — подписи WD14 недоступны. Положите рядом с картинками .txt с тегами: обучение возьмёт их как есть."
            .to_string());
    }
    let dir = resolve_dataset(dataset)?;
    let (images, _) = count_pairs(&dir);
    if images == 0 {
        return Err("в датасете нет картинок".into());
    }
    ensure_wd14_model()?;

    set_caption(|state| {
        *state = CaptionProgress {
            running: true,
            done: 0,
            total: images,
            stage: "подготовка модели WD14".into(),
        }
    });
    drop(start_guard);

    let mut cmd = Command::new(&python);
    cmd.arg(&tagger)
        .arg(&dir)
        .arg("--repo_id")
        .arg("SmilingWolf/wd-v1-4-convnext-tagger-v2")
        .arg("--model_dir")
        .arg(crate::paths::wd14_model_dir())
        .arg("--onnx")
        .arg("--batch_size")
        .arg("4")
        .arg("--caption_extension")
        .arg(".txt")
        // Without the flag the tagger overwrites .txt: manual and imported
        // captions were lost. With it existing tags stay, new ones are appended.
        .arg("--append_tags")
        .arg("--thresh")
        .arg("0.35")
        .current_dir(sd_scripts_dir())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::pyruntime::training_env(&mut cmd);
    hide_console(&mut cmd);

    let result = (|| -> Result<String, String> {
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("не удалось запустить подпись: {e}"))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let collected = Arc::new(Mutex::new(String::new()));
        let sink = collected.clone();
        let collector = std::thread::spawn(move || {
            if let Some(out) = stdout {
                read_lines(out, |line| {
                    if let Ok(mut text) = sink.lock() {
                        text.push_str(line);
                        text.push('\n');
                    }
                });
            }
        });
        if let Some(err) = stderr {
            read_lines(err, |line| {
                if let Some((done, total)) = parse_bar_progress(line) {
                    set_caption(|state| {
                        state.done = done;
                        state.total = total.max(1);
                        state.stage = "подписи WD14".into();
                    });
                }
            });
        }
        let status = child.wait().map_err(|e| e.to_string())?;
        let _ = collector.join();
        let output = collected.lock().map(|text| text.clone()).unwrap_or_default();
        if !status.success() {
            let tail: Vec<String> = output
                .lines()
                .filter(|line| !line.trim().is_empty())
                .rev()
                .take(5)
                .map(|line| line.trim().to_string())
                .collect();
            return Err(format!(
                "подпись завершилась с ошибкой: {}",
                tail.join(" | ")
            ));
        }
        let (_, captions) = count_pairs(&dir);
        Ok(format!(
            "Подписано {captions} картинок из {images}: файлы .txt лежат рядом с картинками и правятся вручную."
        ))
    })();

    set_caption(|state| {
        state.running = false;
        state.stage = match &result {
            Ok(_) => "готово".into(),
            Err(error) => error.clone(),
        };
    });
    result
}

/// Processed image number from the tqdm bar: "100%|██| 1/1 [00:03<00:00, 3.2s/it]".
pub fn parse_bar_progress(line: &str) -> Option<(u32, u32)> {
    let mut parts = line.split('|');
    parts.next()?;
    parts.next()?;
    let counter = parts.next()?.trim();
    let (done, total) = counter.split_once('/')?;
    let digits: String = total
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    Some((done.trim().parse().ok()?, digits.parse().ok()?))
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ImageTrainingState {
    pub running: bool,
    pub run_id: Option<String>,
    pub preset_id: Option<String>,
    pub dataset: Option<String>,
    pub stage: String,
    pub step: u32,
    pub total: u32,
    pub loss: Option<f32>,
    pub lr: Option<f32>,
    /// Seconds per step as measured by the run itself (kohya tqdm).
    pub speed: Option<f32>,
    pub eta_seconds: Option<u64>,
    pub elapsed_seconds: u64,
    pub images: u32,
    pub captions: u32,
    pub lora_path: Option<String>,
    pub log_tail: Vec<String>,
    pub error: Option<String>,
    pub finished: bool,
    pub runtime: ImageRuntime,
    pub caption: CaptionProgress,
}

impl Default for ImageTrainingState {
    fn default() -> Self {
        Self {
            running: false,
            run_id: None,
            preset_id: None,
            dataset: None,
            stage: "ожидание".into(),
            step: 0,
            total: 0,
            loss: None,
            lr: None,
            speed: None,
            eta_seconds: None,
            elapsed_seconds: 0,
            images: 0,
            captions: 0,
            lora_path: None,
            log_tail: Vec::new(),
            error: None,
            finished: false,
            runtime: ImageRuntime::default(),
            caption: CaptionProgress::default(),
        }
    }
}

struct Job {
    state: Arc<Mutex<ImageTrainingState>>,
    child: Arc<Mutex<Option<Child>>>,
    /// pid lives separately from child: after child is taken out under wait(), cancel
    /// is still required to kill the process tree.
    pid: u32,
    cancel: Arc<AtomicBool>,
    started: Instant,
}

static JOB: Mutex<Option<Job>> = Mutex::new(None);

/// Parsing of the `@@LK@@ {json}` marker from the embedded script. A separate function:
/// tests exercise it, the reader thread only calls it.
fn apply_marker(state: &mut ImageTrainingState, payload: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
        return false;
    };
    if let Some(error) = value["error"].as_str() {
        state.error = Some(error.to_string());
        state.stage = "ошибка".into();
        return true;
    }
    match value["stage"].as_str() {
        Some("loading") => state.stage = "загрузка чекпоинта".into(),
        Some("dataset") => state.stage = "кэш латентов и подписей".into(),
        Some("steps") => state.stage = "обучение".into(),
        Some("saving") => state.stage = "сохранение LoRA".into(),
        Some("done") => {
            state.stage = "готово".into();
            state.finished = true;
        }
        _ => {}
    }
    if let Some(images) = value["images"].as_u64() {
        state.images = images as u32;
    }
    if let Some(captions) = value["captions"].as_u64() {
        state.captions = captions as u32;
    }
    if let Some(step) = value["step"].as_u64() {
        state.step = step as u32;
    }
    if let Some(total) = value["total"].as_u64() {
        state.total = total as u32;
    }
    if let Some(loss) = value["loss"].as_f64() {
        state.loss = Some(loss as f32);
    }
    if let Some(lr) = value["lr"].as_f64() {
        state.lr = Some(lr as f32);
    }
    if let Some(speed) = value["speed"].as_f64() {
        state.speed = Some(speed as f32);
    }
    if let Some(eta) = value["eta"].as_u64() {
        state.eta_seconds = Some(eta);
    }
    if let Some(lora) = value["lora"].as_str() {
        state.lora_path = Some(lora.to_string());
    }
    true
}

pub fn status() -> ImageTrainingState {
    let mut state = match JOB.lock() {
        Ok(guard) => match guard.as_ref() {
            Some(job) => job.state.lock().map(|s| s.clone()).unwrap_or_default(),
            None => ImageTrainingState::default(),
        },
        Err(_) => ImageTrainingState::default(),
    };
    if let Ok(guard) = JOB.lock() {
        if let Some(job) = guard.as_ref() {
            state.elapsed_seconds = job.started.elapsed().as_secs();
        }
    }
    state.runtime = runtime();
    state.caption = caption_state();
    state
}

/// The training script is embedded in the binary and written to `data/training` next to
/// the runs: a built app has no project sources.
fn script_path() -> Result<PathBuf, String> {
    const SCRIPT: &str = include_str!("../scripts/train_image_lora.py");
    let dir = crate::paths::training_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join("train_image_lora.py");
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    if current != SCRIPT {
        std::fs::write(&path, SCRIPT).map_err(|e| e.to_string())?;
    }
    Ok(path)
}

/// Slot reservation under the first lock (the same class as A3-01 in the text
/// trainer): a placeholder with `running=true` sits in `JOB` already during preparation,
/// and a second start is rejected while checkpoint scanning runs. Previously
/// the "running?" check and slot assignment were separated by preparation: two quick
/// starts both passed it, the second python became unmanaged, and both wrote
/// the same safetensors.
fn reserve_job(
    state: Arc<Mutex<ImageTrainingState>>,
    child: Arc<Mutex<Option<Child>>>,
    cancel: Arc<AtomicBool>,
) -> Result<(), String> {
    let mut guard = JOB.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(job) = guard.as_ref() {
        let running = job.state.lock().unwrap_or_else(|e| e.into_inner()).running;
        if running {
            return Err("Обучение уже идёт — дождитесь окончания или отмените его.".into());
        }
    }
    *guard = Some(Job {
        state,
        child,
        pid: 0,
        cancel,
        started: Instant::now(),
    });
    Ok(())
}

/// Release the reservation — only if the slot still holds our job.
fn release_slot(state: &Arc<Mutex<ImageTrainingState>>) {
    let mut guard = JOB.lock().unwrap_or_else(|e| e.into_inner());
    if guard.as_ref().is_some_and(|job| Arc::ptr_eq(&job.state, state)) {
        *guard = None;
    }
}

/// Cancel during preparation must abort the work: python is not started yet.
fn ensure_not_cancelled(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err("отменено пользователем".into())
    } else {
        Ok(())
    }
}

/// Everything preparation hands to spawn.
struct PreparedImageRun {
    dataset_dir: PathBuf,
    images: u32,
    captions: u32,
    steps: u32,
    run_id: String,
    run_dir: PathBuf,
    config_path: PathBuf,
    script: PathBuf,
    log_path: PathBuf,
}

#[allow(clippy::too_many_arguments)]
fn prepare_run(
    preset_id: &str,
    dataset: &str,
    repeats: u32,
    name: &str,
    cancel: &AtomicBool,
) -> Result<PreparedImageRun, String> {
    ensure_not_cancelled(cancel)?;
    availability()?;
    let preset =
        preset_by_id(preset_id).ok_or_else(|| format!("неизвестный пресет: {preset_id}"))?;
    let tier = crate::gpu::gpu_info().tier;
    if tier < preset.min_tier {
        return Err(format!(
            "«{}» требует тир {:?}: на этой видеокарте не хватит видеопамяти.",
            preset.family_label, preset.min_tier
        ));
    }
    let dataset_dir = resolve_dataset(dataset)?;
    let (images, captions) = count_pairs(&dataset_dir);
    if images == 0 {
        return Err("в датасете нет картинок".into());
    }
    let repeats = repeats.clamp(REPEAT_RANGE.0, REPEAT_RANGE.1);
    let steps = images * repeats;

    let output_name = crate::text_lora::slug(name);
    let loras = crate::paths::comfyui_loras_dir();
    std::fs::create_dir_all(&loras).map_err(|e| e.to_string())?;
    let lora_path = loras.join(format!("{output_name}.safetensors"));
    if lora_path.exists() {
        return Err(format!(
            "В папке ComfyUI уже есть LoRA «{output_name}» — возьмите другое имя."
        ));
    }
    ensure_not_cancelled(cancel)?;

    // The finished LoRA is in the ComfyUI folder; the run dir holds only train.log and the config:
    // without cleanup the directories piled up forever (the text trainer already has KEEP_RUNS)
    crate::trainer::prune_runs_in(&crate::paths::image_runs_dir(), crate::trainer::KEEP_RUNS);
    let run_id = run_id_now();
    let run_dir = crate::paths::image_runs_dir().join(&run_id);
    std::fs::create_dir_all(&run_dir).map_err(|e| e.to_string())?;

    let config = serde_json::json!({
        "family": preset.family,
        "base": preset.path,
        "dataset_dir": dataset_dir.to_string_lossy(),
        "output_dir": loras.to_string_lossy(),
        "output_name": output_name,
        "resolution": preset.resolution,
        "repeats": repeats,
        "max_train_steps": steps,
        "network_dim": NETWORK_DIM,
        "network_alpha": NETWORK_ALPHA,
        "learning_rate": LEARNING_RATE,
        "seed": SEED,
        "sd_scripts_dir": sd_scripts_dir().to_string_lossy(),
        "run_dir": run_dir.to_string_lossy(),
        // Only Rust knows the cache and temp paths — the script applies them.
        "env": {
            "HF_HOME": crate::paths::hf_cache_dir().to_string_lossy(),
            "TORCH_HOME": crate::paths::torch_cache_dir().to_string_lossy(),
            "TMP": crate::paths::tmp_dir().to_string_lossy(),
            "TEMP": crate::paths::tmp_dir().to_string_lossy(),
        },
    });
    let config_path = run_dir.join("train-config.json");
    std::fs::write(
        &config_path,
        serde_json::to_string_pretty(&config).unwrap_or_default(),
    )
    .map_err(|e| e.to_string())?;
    let script = script_path()?;
    let log_path = run_dir.join("train.log");

    Ok(PreparedImageRun {
        dataset_dir,
        images,
        captions,
        steps,
        run_id,
        run_dir,
        config_path,
        script,
        log_path,
    })
}

/// Start LoRA training. Returns the run id.
pub fn start(
    app: AppHandle,
    preset_id: &str,
    dataset: &str,
    repeats: u32,
    name: &str,
) -> Result<String, String> {
    let state = Arc::new(Mutex::new(ImageTrainingState {
        running: true,
        stage: "подготовка".into(),
        ..ImageTrainingState::default()
    }));
    let cancel = Arc::new(AtomicBool::new(false));
    let child_handle: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
    // Reservation and foreign-job checks — under the shared start lock: the check
    // "text is not running" after the reservation left a window where both starts
    // passed it simultaneously.
    let start_guard = crate::trainer::START_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    reserve_job(state.clone(), child_handle.clone(), cancel.clone())?;
    // Captioning and training are independent jobs, but both write to the same dataset:
    // kohya caches captions at start and would train on old .txt files while WD14
    // appends new ones. A second trainer on one card is a guaranteed OOM.
    if caption_state().running {
        release_slot(&state);
        return Err("Подпись WD14 ещё идёт — дождитесь окончания.".into());
    }
    if crate::trainer::status().running {
        release_slot(&state);
        return Err("Идёт обучение текста — дождитесь окончания.".into());
    }
    drop(start_guard);
    let prepared = match prepare_run(preset_id, dataset, repeats, name, &cancel) {
        Ok(prepared) => prepared,
        Err(e) => {
            release_slot(&state);
            return Err(e);
        }
    };
    let PreparedImageRun {
        dataset_dir,
        images,
        captions,
        steps,
        run_id,
        run_dir,
        config_path,
        script,
        log_path,
    } = prepared;
    let mut cmd = Command::new(venv_sd_python());
    cmd.arg(&script)
        .arg("--config")
        .arg(&config_path)
        .current_dir(&run_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // The same variable set as in the spike: UTF-8 on output, HF cache and %TEMP%
    // inside the program folder.
    crate::pyruntime::training_env(&mut cmd);
    hide_console(&mut cmd);
    if let Err(e) = ensure_not_cancelled(&cancel) {
        release_slot(&state);
        return Err(e);
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            release_slot(&state);
            return Err(format!("не удалось запустить обучение: {e}"));
        }
    };
    let pid = child.id();

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    if let Ok(mut handle) = child_handle.lock() {
        *handle = Some(child);
    }
    {
        // pid — into the already reserved slot: cancel() kills the tree by it,
        // even when the wait thread has taken child out.
        let mut guard = JOB.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(job) = guard.as_mut() {
            if Arc::ptr_eq(&job.state, &state) {
                job.pid = pid;
            }
        }
    }
    if cancel.load(Ordering::Relaxed) {
        // Cancel arrived during preparation: the process was just started — kill it.
        #[cfg(windows)]
        {
            let mut kill = Command::new("taskkill");
            kill.args(["/PID", &pid.to_string(), "/T", "/F"]);
            hide_console(&mut kill);
            let _ = kill.status();
        }
        if let Ok(mut handle) = child_handle.lock() {
            if let Some(child) = handle.as_mut() {
                let _ = child.kill();
            }
        }
    }
    if let Ok(mut s) = state.lock() {
        s.run_id = Some(run_id.clone());
        s.preset_id = Some(preset_id.to_string());
        s.dataset = Some(dataset_dir.to_string_lossy().to_string());
        s.stage = if cancel.load(Ordering::Relaxed) {
            "отмена…".into()
        } else {
            "запуск".into()
        };
        s.total = steps;
        s.images = images;
        s.captions = captions;
    }
    if let Ok(s) = state.lock() {
        let _ = app.emit("image-training-progress", s.clone());
    }

    let reader_state = state.clone();
    let reader_app = app.clone();
    let started = Instant::now();
    let reader_cancel = cancel.clone();
    std::thread::spawn(move || {
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .ok();
        let mut handle_line = |line: &str| {
            if let Some(log) = log.as_mut() {
                let _ = writeln!(log, "{line}");
            }
            if let Some(payload) = line.strip_prefix("@@LK@@ ") {
                if let Ok(mut s) = reader_state.lock() {
                    if apply_marker(&mut s, payload) {
                        // Fallback calculation: the marker may have no eta field of its own.
                        if s.eta_seconds.is_none() && s.step > 0 && s.total > s.step {
                            let speed = started.elapsed().as_secs_f64() / s.step as f64;
                            s.eta_seconds = Some(((s.total - s.step) as f64 * speed) as u64);
                        }
                    }
                }
                let _ = reader_app.emit("image-training-progress", payload.to_string());
                return;
            }
            if line.trim().is_empty() {
                return;
            }
            if let Ok(mut s) = reader_state.lock() {
                let tail = &mut s.log_tail;
                if tail.len() >= 40 {
                    tail.remove(0);
                }
                tail.push(line.trim().to_string());
            }
        };

        if let Some(out) = stdout {
            read_lines(out, &mut handle_line);
        }
        if let Some(err) = stderr {
            read_lines(err, &mut handle_line);
        }

        // Take child out before wait(): cancel() takes the same lock and must not
        // hang while the process exit wait is in progress. `waited` lives until
        // the end of the thread, that is, longer than running == true: this is what
        // the check in cancel() relies on.
        let mut waited = child_handle.lock().ok().and_then(|mut guard| guard.take());
        let code = waited
            .as_mut()
            .and_then(|c| c.wait().ok())
            .and_then(|status| status.code());
        let cancelled = reader_cancel.load(Ordering::Relaxed);
        if let Ok(mut s) = reader_state.lock() {
            s.running = false;
            s.eta_seconds = None;
            if cancelled {
                s.stage = "отменено".into();
                s.error = None;
            } else if code == Some(0) {
                s.stage = "готово".into();
                s.finished = true;
            } else {
                s.stage = "ошибка".into();
                if s.error.is_none() {
                    s.error = Some(format!(
                        "обучение завершилось с кодом {:?}. Последние строки лога помогут понять причину.",
                        code.unwrap_or(-1)
                    ));
                }
            }
            let _ = reader_app.emit("image-training-progress", s.clone());
        }
    });

    Ok(run_id)
}

/// Cancel the current training along with the process tree.
pub fn cancel() -> Result<(), String> {
    let guard = JOB.lock().map_err(|_| "состояние занято".to_string())?;
    let Some(job) = guard.as_ref() else {
        return Err("обучение не запущено".into());
    };
    // Hold the state lock to the end: the wait thread clears running before
    // it drops Child, and while the handle is open the OS will not give the pid to another
    // process. Do not touch a finished run — its pid is already someone else's (app
    // exit always calls cancel).
    let mut state = job.state.lock().map_err(|_| "состояние занято".to_string())?;
    if !state.running {
        return Ok(());
    }
    job.cancel.store(true, Ordering::Relaxed);
    // Kill the tree by the saved pid: child may already have been taken out of the slot
    // by the wait thread, and handle.kill() cannot reach it. During preparation
    // the pid is still 0 — taskkill on zero would hit System Idle Process.
    #[cfg(windows)]
    if job.pid != 0 {
        let mut cmd = Command::new("taskkill");
        cmd.args(["/PID", &job.pid.to_string(), "/T", "/F"]);
        hide_console(&mut cmd);
        let _ = cmd.status();
    }
    if let Ok(mut child_guard) = job.child.lock() {
        if let Some(child) = child_guard.as_mut() {
            let _ = child.kill();
        }
    }
    state.stage = "отмена…".into();
    Ok(())
}

/// Forget a finished run so the UI returns to its initial state.
pub fn clear() {
    if let Ok(mut guard) = JOB.lock() {
        let finished = guard
            .as_ref()
            .map(|job| !job.state.lock().map(|s| s.running).unwrap_or(false))
            .unwrap_or(false);
        if finished {
            *guard = None;
        }
    }
}

/// Open a folder in Explorer: image datasets or the LoRA folder for ComfyUI.
pub fn open_folder(which: &str) -> Result<(), String> {
    let path = match which {
        "datasets" => crate::paths::image_training_dir(),
        "loras" => crate::paths::comfyui_loras_dir(),
        other => return Err(format!("неизвестная папка: {other}")),
    };
    std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
    #[cfg(windows)]
    {
        std::process::Command::new("explorer")
            .arg(&path)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(not(windows))]
    let _ = &path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    fn temp_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "lk-image-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Minimal safetensors header: length in LE, then JSON.
    fn write_safetensors(path: &Path, tensors: &[&str]) {
        let names: Vec<String> = tensors
            .iter()
            .map(|name| format!("\"{name}\":{{\"dtype\":\"F16\",\"shape\":[1],\"data_offsets\":[0,2]}}"))
            .collect();
        let header = format!("{{{}}}", names.join(","));
        let mut bytes = (header.len() as u64).to_le_bytes().to_vec();
        bytes.extend_from_slice(header.as_bytes());
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn test_family_from_safetensors_header() {
        let dir = temp_dir("family");
        let sdxl = dir.join("model.safetensors");
        write_safetensors(
            &sdxl,
            &["conditioner.embedders.1.transformer.text_model.embeddings", "model.diffusion_model.label_emb.0.0.weight"],
        );
        assert_eq!(family_from_header(&sdxl), Some(ImageFamily::Sdxl));

        let sd15 = dir.join("sd15.safetensors");
        write_safetensors(&sd15, &["cond_stage_model.transformer.text_model.embeddings"]);
        assert_eq!(family_from_header(&sd15), Some(ImageFamily::Sd15));

        // Not safetensors (e.g. .ckpt): the header is not parsed — the name decides.
        let broken = dir.join("anything-v5.ckpt");
        std::fs::write(&broken, b"not a safetensors file").unwrap();
        assert_eq!(family_from_header(&broken), None);
        assert_eq!(detect_family(&broken, "anything-v5.ckpt"), ImageFamily::Sd15);
        assert_eq!(
            detect_family(&broken, "some-model-XL.ckpt"),
            ImageFamily::Sdxl
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Family cache (V-52): a hit on the "name + size + mtime" key does
    /// not reread the header (the detector is not called), a key change — reads it.
    #[test]
    fn test_family_cache_hits_by_file_identity() {
        let key = |size: u64| Some((format!("cache-probe-{}", std::process::id()), size, None));
        let calls = std::cell::Cell::new(0);
        let first = cached_family_with(key(1), || {
            calls.set(calls.get() + 1);
            ImageFamily::Sdxl
        });
        assert_eq!(first, ImageFamily::Sdxl);
        // The second call with the same key must return the cache: the detector would return
        // another value, and the mismatch proves it.
        let second = cached_family_with(key(1), || {
            calls.set(calls.get() + 1);
            ImageFamily::Sd15
        });
        assert_eq!(second, ImageFamily::Sdxl, "тот же ключ — ответ из кэша");
        assert_eq!(calls.get(), 1, "детектор вызван один раз");
        // The file size changed — the header is reread.
        let third = cached_family_with(key(2), || {
            calls.set(calls.get() + 1);
            ImageFamily::Sd15
        });
        assert_eq!(third, ImageFamily::Sd15);
        assert_eq!(calls.get(), 2);
    }

    /// Preset list cache (V-52): a repeated request with the same store fingerprint,
    /// settings mtime and tier does not rebuild the list; any of the three variables
    /// triggers a new build.
    #[test]
    fn test_preset_cache_hits_on_same_fingerprint() {
        let store: StoreFingerprint = vec![("model.safetensors".to_string(), 1_000_000)];
        let mtime = Some(SystemTime::UNIX_EPOCH);
        let builds = std::cell::Cell::new(0);
        let build = || {
            builds.set(builds.get() + 1);
            Vec::new()
        };

        let _ = cached_presets(&store, mtime, HardwareTier::High, build);
        let _ = cached_presets(&store, mtime, HardwareTier::High, build);
        assert_eq!(builds.get(), 1, "второй вызов обязан отдать кэш");

        let changed_store: StoreFingerprint = vec![("model.safetensors".to_string(), 2_000_000)];
        let _ = cached_presets(&changed_store, mtime, HardwareTier::High, build);
        let _ = cached_presets(&store, Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1)), HardwareTier::High, build);
        let _ = cached_presets(&store, mtime, HardwareTier::Low, build);
        assert_eq!(builds.get(), 4, "каждая смена ключа строит список заново");
    }

    #[test]
    fn test_family_tiers_and_resolutions() {
        // SD 1.5 goes from 8 GB, SDXL from 11 GB: on the lower tier it trains at 768px.
        assert_eq!(ImageFamily::Sd15.min_tier(), HardwareTier::Low);
        assert_eq!(ImageFamily::Sdxl.min_tier(), HardwareTier::Mid);
        assert_eq!(resolution_for(ImageFamily::Sd15, HardwareTier::Low), 512);
        assert_eq!(resolution_for(ImageFamily::Sdxl, HardwareTier::Mid), 768);
        assert_eq!(resolution_for(ImageFamily::Sdxl, HardwareTier::High), 1024);
        // kohya bucket grid: 64 for SD 1.5 and 32 for SDXL — the resolution must be divisible.
        assert_eq!(resolution_for(ImageFamily::Sd15, HardwareTier::High) % 64, 0);
        assert_eq!(resolution_for(ImageFamily::Sdxl, HardwareTier::Mid) % 32, 0);
    }

    #[test]
    fn test_estimate_follows_area_family_and_card() {
        let steady = estimate_seconds_per_step(HardwareTier::High, ImageFamily::Sdxl, 512);
        assert!((steady - 1.1).abs() < 0.01, "{steady}");
        let sdxl_1024 = estimate_seconds_per_step(HardwareTier::High, ImageFamily::Sdxl, 1024);
        assert!(sdxl_1024 > steady * 3.5, "площадь кадра должна учитываться");
        let sd15_512 = estimate_seconds_per_step(HardwareTier::High, ImageFamily::Sd15, 512);
        assert!(sd15_512 < steady, "SD 1.5 легче SDXL");
        let on_mid = estimate_seconds_per_step(HardwareTier::Mid, ImageFamily::Sdxl, 768);
        let on_high = estimate_seconds_per_step(HardwareTier::High, ImageFamily::Sdxl, 768);
        assert!(on_mid > on_high, "на младшей карте шаг дольше");

        // The full estimate adds one-off start costs and grows with steps.
        let short = estimate_total_seconds(HardwareTier::High, ImageFamily::Sdxl, 512, 1);
        let long = estimate_total_seconds(HardwareTier::High, ImageFamily::Sdxl, 512, 100);
        assert!(short > 60.0, "старт прогона стоит около минуты: {short}");
        assert!(long > short + 100.0, "оценка должна расти с числом шагов");
    }

    #[test]
    fn test_preset_id_is_safe_and_unique() {
        let taken: Vec<String> = Vec::new();
        assert_eq!(preset_id("animagine-xl-4.0.safetensors", &taken), "animagine-xl-4-0");
        assert_eq!(preset_id("Моя модель.safetensors", &taken), "moya-model");
        let taken = vec!["animagine-xl-4-0".to_string()];
        assert_eq!(preset_id("animagine xl 4.0.safetensors", &taken), "animagine-xl-4-0-2");
    }

    #[test]
    fn test_marker_updates_state() {
        let mut state = ImageTrainingState::default();
        assert!(apply_marker(&mut state, r#"{"stage":"loading","base":"animagine-xl-4.0.safetensors"}"#));
        assert_eq!(state.stage, "загрузка чекпоинта");
        apply_marker(&mut state, r#"{"stage":"dataset","images":12,"captions":12,"total":120}"#);
        assert_eq!(state.stage, "кэш латентов и подписей");
        assert_eq!(state.images, 12);
        assert_eq!(state.total, 120);
        apply_marker(
            &mut state,
            r#"{"stage":"steps","step":7,"total":120,"loss":0.152,"lr":0.0001,"speed":2.86,"eta":321}"#,
        );
        assert_eq!(state.stage, "обучение");
        assert_eq!(state.step, 7);
        assert_eq!(state.loss, Some(0.152));
        assert_eq!(state.lr, Some(0.0001));
        assert_eq!(state.speed, Some(2.86));
        assert_eq!(state.eta_seconds, Some(321));
        apply_marker(&mut state, r#"{"stage":"done","lora":"Z:\\AImodels\\comfyui\\loras\\style.safetensors"}"#);
        assert!(state.finished);
        assert_eq!(state.stage, "готово");
        assert!(state.lora_path.as_ref().unwrap().ends_with("style.safetensors"));
        // Garbage in the marker does not corrupt the state.
        assert!(!apply_marker(&mut state, "не json"));
        assert_eq!(state.stage, "готово");
    }

    #[test]
    fn test_marker_error_is_reported() {
        let mut state = ImageTrainingState::default();
        apply_marker(&mut state, r#"{"error":"kohya завершился с кодом 1: CUDA out of memory"}"#);
        assert_eq!(state.stage, "ошибка");
        assert!(state.error.unwrap().contains("out of memory"));
    }

    #[test]
    fn test_parse_bar_progress() {
        assert_eq!(parse_bar_progress("100%|██████| 1/1 [00:03<00:00,  3.22s/it]"), Some((1, 1)));
        assert_eq!(parse_bar_progress(" 42%|####  | 5/12 [00:03<00:05,  1.72it/s]"), Some((5, 12)));
        assert_eq!(parse_bar_progress("steps:  45%|#| 9/20 [00:26<00:31,  2.86s/it]"), Some((9, 20)));
        assert_eq!(parse_bar_progress("found 4 images"), None);
    }

    #[test]
    fn audit_failed_image_import_leaves_no_partial_dataset() {
        let source = temp_dir("audit-source");
        let root = temp_dir("audit-import");
        std::fs::write(source.join("one.png"), b"png").unwrap();
        std::fs::write(source.join("two.png"), b"png").unwrap();
        let mut copies = 0;
        let result = copy_dataset_with(&source, &root, |from, to| {
            copies += 1;
            if copies == 2 {
                Err(std::io::Error::other("injected disk full"))
            } else {
                std::fs::copy(from, to)
            }
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0, "failed import was published as a dataset");
        std::fs::remove_dir_all(source).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn test_dataset_paths_and_copy() {
        assert!(crate::paths::image_training_dir().starts_with(crate::paths::training_dir()));
        assert!(crate::paths::image_runs_dir().starts_with(crate::paths::training_dir()));
        assert_eq!(
            crate::paths::comfyui_loras_dir(),
            crate::paths::comfyui_models_dir().join("loras")
        );

        let source = temp_dir("source");
        std::fs::write(source.join("one.png"), b"png").unwrap();
        std::fs::write(source.join("one.txt"), b"1girl, solo").unwrap();
        std::fs::write(source.join("two.jpg"), b"jpg").unwrap();
        std::fs::write(source.join("readme.md"), "не картинка".as_bytes()).unwrap();
        let root = temp_dir("datasets");
        let info = copy_dataset(&source, &root).unwrap();
        assert_eq!(info.images, 2);
        assert_eq!(info.captions, 1);
        assert!(root.join(&info.slug).join("one.txt").is_file());
        assert!(!root.join(&info.slug).join("readme.md").exists());
        assert_eq!(datasets_in(&root).len(), 1);
        std::fs::write(root.join(&info.slug).join("one.png"), b"original").unwrap();
        let again = copy_dataset(&source, &root).unwrap();
        assert_ne!(again.slug, info.slug);
        assert!(again.slug.ends_with("-2"), "{}", again.slug);
        assert_eq!(
            std::fs::read(root.join(&info.slug).join("one.png")).unwrap(),
            b"original"
        );

        let empty = temp_dir("empty");
        assert!(copy_dataset(&empty, &root).is_err(), "папка без картинок — ошибка");
        let _ = std::fs::remove_dir_all(&source);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&empty);
    }

    /// Dataset list for an arbitrary root (the public `datasets()` looks
    /// in the app folder).
    fn datasets_in(root: &Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(root) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn test_venv_and_scripts_paths_live_in_trainer_dir() {
        assert!(venv_sd_python().starts_with(crate::paths::trainer_dir()));
        assert!(sd_scripts_dir().starts_with(crate::paths::trainer_dir()));
        assert!(crate::paths::wd14_model_dir().starts_with(crate::paths::models_root()));
    }

    /// The dataset name comes from the UI and goes into a path: the slug drops everything
    /// except letters, digits, hyphen and underscore, so it cannot escape the datasets
    /// folder.
    #[test]
    fn test_dataset_name_cannot_escape_the_root() {
        let root = crate::paths::image_training_dir();
        let probe = root.join("probe");
        std::fs::create_dir_all(&probe).unwrap();

        for name in [
            "probe",
            "../probe",
            "..\\..\\probe",
            "/etc/probe",
            "C:\\Windows\\probe",
            "probe/../../probe",
        ] {
            if let Ok(dir) = resolve_dataset(name) {
                assert!(dir.starts_with(&root), "{name} → {}", dir.display());
            }
        }
        assert_eq!(resolve_dataset("../probe").unwrap(), probe, "слаг приводит имя к probe");
        assert!(resolve_dataset("такого-датасета-нет").is_err());

        let _ = std::fs::remove_dir_all(&probe);
    }

    /// cancel() on a finished run must not taskkill its pid:
    /// the OS could have given it to another process.
    #[cfg(windows)]
    #[test]
    fn test_cancel_leaves_finished_job_pid_alone() {
        let mut bystander = Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdout(Stdio::null())
            .spawn()
            .expect("ping");
        let state = Arc::new(Mutex::new(ImageTrainingState {
            running: false,
            stage: "готово".into(),
            ..ImageTrainingState::default()
        }));
        *JOB.lock().unwrap() = Some(Job {
            state: state.clone(),
            child: Arc::new(Mutex::new(None)),
            pid: bystander.id(),
            cancel: Arc::new(AtomicBool::new(false)),
            started: Instant::now(),
        });
        let result = cancel();
        *JOB.lock().unwrap() = None;
        let survived = bystander.try_wait().unwrap().is_none();
        let _ = bystander.kill();
        let _ = bystander.wait();

        assert!(result.is_ok());
        assert!(survived, "taskkill ушёл по pid завершённого прогона");
        assert_eq!(state.lock().unwrap().stage, "готово");
    }
}
