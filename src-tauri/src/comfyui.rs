use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Duration;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

static COMFY_CHILD: Mutex<Option<Child>> = Mutex::new(None);
static COMFY_STARTING: AtomicBool = AtomicBool::new(false);
static COMFY_INSTALLING: AtomicBool = AtomicBool::new(false);
/// unpacking tar.exe: it used to live outside accounting — closing the app left
/// it finishing comfyui/, and half the tree looked installed.
static EXTRACT_PID: AtomicU32 = AtomicU32::new(0);
/// Its own cancel flag: the wizard's shared `cancel_download` would also kill the runtime and
/// SillyTavern, and their cancel would kill the ComfyUI install.
static INSTALL_CANCEL: std::sync::OnceLock<std::sync::Arc<AtomicBool>> = std::sync::OnceLock::new();

use crate::l10n;

pub fn install_cancel_flag() -> std::sync::Arc<AtomicBool> {
    INSTALL_CANCEL
        .get_or_init(|| std::sync::Arc::new(AtomicBool::new(false)))
        .clone()
}

/// One install at a time: two settings windows would otherwise write Range downloads
/// into the same `.7z`. The flag is cleared on leaving the install by any path.
struct InstallGuard;

impl InstallGuard {
    fn acquire() -> Option<Self> {
        (!COMFY_INSTALLING.swap(true, Ordering::SeqCst)).then_some(InstallGuard)
    }
}

impl Drop for InstallGuard {
    fn drop(&mut self) {
        COMFY_INSTALLING.store(false, Ordering::SeqCst);
    }
}

pub const COMFY_PORTABLE_URL: &str =
    "https://github.com/Comfy-Org/ComfyUI/releases/download/v0.34.0/ComfyUI_windows_portable_nvidia.7z";

/// Default image checkpoint: set by settings normalization and highlighted by
/// `sync_st_object`, which also ends the active-model fallback. One constant
/// for all modules (V-30): when the model bundle changes, the edit must touch every point.
pub const DEFAULT_IMAGE_CHECKPOINT: &str = "waiIllustriousSDXL_v170.safetensors";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ComfyStatus {
    pub running: bool,
    pub port: u16,
    pub path: Option<String>,
    pub installed: bool,
    /// Partially downloaded install archive: how much is downloaded and the full size.
    /// The UI uses them to offer resuming instead of starting over.
    pub partial_bytes: u64,
    pub partial_total_bytes: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ImageModelInfo {
    pub filename: String,
    pub name: String,
    pub badge: String,
    pub size: String,
    pub description: String,
    pub roleplay_style: String,
    pub is_installed: bool,
    pub is_active: bool,
    pub recommended: bool,
    /// VRAM tier: "sdxl" for 12 GB+, "sd15" for 8 GB and below.
    pub vram_tier: String,
    /// Minimum hardware tier where the checkpoint makes sense.
    pub min_tier: crate::gpu::HardwareTier,
}

/// Where to look for an install: the app directory and the portable layout inside it.
/// The ComfyUI archive unpacks into `ComfyUI_windows_portable\`, and if it
/// was unpacked by hand (or an older app version did not strip the top
/// level), the install is right there — without this variant the app considered
/// a ready ComfyUI missing and downloaded the same 2 GB again.
fn install_roots() -> Vec<PathBuf> {
    let root = crate::paths::comfyui_dir();
    vec![root.clone(), root.join("ComfyUI_windows_portable")]
}

/// Find the directory with `main.py` strictly inside the app directory `paths::comfyui_dir()`.
pub fn get_comfy_main_dir() -> Option<PathBuf> {
    for root in install_roots() {
        for candidate in [root.clone(), root.join("ComfyUI")] {
            if candidate.join("main.py").exists() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Find the Python executable for ComfyUI strictly inside the app directory.
pub fn detect_python_exe() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for root in install_roots() {
        candidates.push(root.join("python_embeded").join("python.exe"));
        candidates.push(root.join("standalone-env").join("python.exe"));
        candidates.push(root.join("venv").join("Scripts").join("python.exe"));
        candidates.push(root.join("ComfyUI").join("venv").join("Scripts").join("python.exe"));
    }
    candidates.push(crate::paths::base_dir().join("runtime").join("python").join("python.exe"));

    candidates.into_iter().find(|exe| exe.exists())
}

/// Check whether ComfyUI is installed inside the app folder.
pub fn is_installed() -> bool {
    get_comfy_main_dir().is_some() && detect_python_exe().is_some()
}

/// Discard the partially downloaded installer archive if ComfyUI is already installed: there is
/// nowhere to finish it, and it weighs up to 2 GB. Returns the bytes freed.
pub fn discard_installer_archive() -> u64 {
    if !is_installed() {
        return 0;
    }
    let archive = crate::paths::base_dir().join("comfyui_portable.7z");
    let freed = std::fs::metadata(&archive).map(|m| m.len()).unwrap_or(0);
    if freed == 0 {
        return 0;
    }
    if std::fs::remove_file(&archive).is_err() {
        return 0;
    }
    let _ = std::fs::remove_file(crate::paths::download_total_path(&archive));
    freed
}

/// Directory for placing checkpoints inside the app.
pub fn checkpoints_dir() -> PathBuf {
    if let Some(main_dir) = get_comfy_main_dir() {
        main_dir.join("models").join("checkpoints")
    } else {
        crate::paths::comfyui_dir().join("models").join("checkpoints")
    }
}

/// Check whether the ComfyUI port responds.
pub fn is_comfy_running(port: u16) -> bool {
    let addr = format!("127.0.0.1:{port}");
    if let Ok(stream) = std::net::TcpStream::connect_timeout(
        &addr.parse().unwrap(),
        Duration::from_millis(600),
    ) {
        drop(stream);
        return true;
    }
    false
}

/// Get the current ComfyUI status.
pub fn get_status(port: u16) -> ComfyStatus {
    let running = is_comfy_running(port);
    let installed = is_installed();
    let path = get_comfy_main_dir().map(|p| p.to_string_lossy().to_string());
    let (partial_bytes, partial_total_bytes) =
        crate::paths::download_partial(&crate::paths::base_dir().join("comfyui_portable.7z"));
    ComfyStatus {
        running,
        port,
        path,
        installed,
        partial_bytes,
        partial_total_bytes,
    }
}

/// Start the ComfyUI process in the background without a console window.
/// Before starting, syncs pip dependencies from requirements.txt
/// so ComfyUI updates do not break imports of outdated packages.
pub fn start_comfy(port: u16) -> Result<(), String> {
    if is_comfy_running(port) {
        return Ok(());
    }

    if COMFY_STARTING.swap(true, Ordering::SeqCst) {
        return Ok(());
    }

    struct StartingGuard;
    impl Drop for StartingGuard {
        fn drop(&mut self) {
            COMFY_STARTING.store(false, Ordering::SeqCst);
        }
    }
    let _guard = StartingGuard;

    let comfy_dir = get_comfy_main_dir()
        .ok_or_else(|| "Каталог ComfyUI не найден в папке приложения. Установите ComfyUI через меню настроек.".to_string())?;
    let python_exe = detect_python_exe()
        .ok_or_else(|| "Интерпретатор Python для ComfyUI не найден в папке приложения.".to_string())?;

    let req_txt = comfy_dir.join("requirements.txt");
    let req_marker = comfy_dir.join(".requirements_installed");
    let needs_pip = if req_txt.exists() {
        if req_marker.exists() {
            let txt_mtime = std::fs::metadata(&req_txt).and_then(|m| m.modified()).ok();
            let marker_mtime = std::fs::metadata(&req_marker).and_then(|m| m.modified()).ok();
            match (txt_mtime, marker_mtime) {
                (Some(t), Some(m)) => t > m,
                _ => true,
            }
        } else {
            true
        }
    } else {
        false
    };

    if needs_pip {
        let mut pip = Command::new(&python_exe);
        pip.args(["-m", "pip", "install", "-q", "-r"]);
        pip.arg(&req_txt);
        pip.current_dir(&comfy_dir);
        // pip cache and temporary builds — in the program folder (see paths::*).
        pip.env("PIP_CACHE_DIR", crate::paths::pip_cache_dir())
            .env("PIP_DISABLE_PIP_VERSION_CHECK", "1")
            .env("TEMP", crate::paths::tmp_dir())
            .env("TMP", crate::paths::tmp_dir());
        #[cfg(windows)]
        pip.creation_flags(0x0800_0000);
        pip.stdout(std::process::Stdio::null());
        pip.stderr(std::process::Stdio::null());
        if let Ok(status) = pip.status() {
            if status.success() {
                let _ = std::fs::write(&req_marker, b"ok");
            }
        }
    }

    let mut cmd = Command::new(&python_exe);
    cmd.current_dir(&comfy_dir);
    cmd.arg("main.py");
    cmd.arg("--listen");
    cmd.arg("127.0.0.1");
    cmd.arg("--port");
    cmd.arg(port.to_string());
    // Nodes with hf_hub_download otherwise write to %USERPROFILE%\.cache\huggingface.
    cmd.env("HF_HOME", crate::paths::hf_cache_dir())
        .env("TORCH_HOME", crate::paths::torch_cache_dir())
        .env("TEMP", crate::paths::tmp_dir())
        .env("TMP", crate::paths::tmp_dir());

    // On weak cards (8 GB and below) enable --lowvram to save VRAM.
    // Without a GPU — force CPU mode.
    let gpu = crate::gpu::gpu_info();
    if !gpu.has_gpu {
        cmd.arg("--cpu");
    } else if gpu.vram_mb.map(|v| v < 11000).unwrap_or(true) {
        cmd.arg("--lowvram");
    }

    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW
        cmd.creation_flags(0x0800_0000);
    }

    let log_path = comfy_dir.join("comfyui.log");
    let log_out = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_path)
        .map_err(|e| format!("Не удалось открыть log-файл: {e}"))?;
    let log_err = log_out
        .try_clone()
        .map_err(|e| format!("Не удалось клонировать дескриптор log-файла: {e}"))?;

    cmd.stdout(std::process::Stdio::from(log_out));
    cmd.stderr(std::process::Stdio::from(log_err));

    let mut child = cmd.spawn().map_err(|e| format!("Не удалось запустить ComfyUI: {e}"))?;

    // If the process dies within the first 2 seconds — read the log file and return an error.
    std::thread::sleep(Duration::from_secs(2));
    if let Ok(Some(status)) = child.try_wait() {
        let text = std::fs::read_to_string(&log_path).unwrap_or_default();
        let tail: String = text
            .lines()
            .rev()
            .take(5)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!("ComfyUI завершился с кодом {}: {}", status, tail.trim()));
    }

    // unwrap_or_else(|e| e.into_inner()): a poisoned lock must not crash
    // the ComfyUI start — the previous child is already dead.
    *COMFY_CHILD.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);

    Ok(())
}

/// Whether the exe belongs to the ComfyUI directory. The comparison is by directory boundary,
/// not by substring: `contains` and a bare string `starts_with` catch
/// the neighboring `comfyui-backup` folder — its python processes must not be killed (V-39).
#[cfg(any(windows, test))]
fn exe_inside_root(clean_exe: &str, clean_root: &str) -> bool {
    clean_exe == clean_root
        || clean_exe
            .strip_prefix(clean_root)
            .is_some_and(|tail| tail.starts_with('\\') || tail.starts_with('/'))
}

/// Gracefully terminate the ComfyUI child process on app exit.
/// Called from the main thread on exit (`lib.rs`); it holds `COMFY_CHILD`
/// under lock for the whole path, so the live child's pid is not reused by the OS
/// between identification and killing.
pub fn kill_comfy_child() {
    let extract_pid = EXTRACT_PID.swap(0, Ordering::SeqCst);
    if extract_pid != 0 {
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .args(["/PID", &extract_pid.to_string(), "/T", "/F"])
                .status();
        }
    }
    let mut lock = COMFY_CHILD.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(mut child) = lock.take() {
        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(windows)]
    {
        use sysinfo::System;
        let comfy_root = crate::paths::comfyui_dir();
        let root_str = comfy_root.to_string_lossy();
        let clean_root = root_str.strip_prefix(r"\\?\").unwrap_or(&root_str).to_lowercase();

        let mut sys = System::new();
        sys.refresh_processes();
        for proc in sys.processes().values() {
            let name = proc.name().to_lowercase();
            if name.contains("python") {
                if let Some(exe) = proc.exe() {
                    let exe_str = exe.to_string_lossy().to_lowercase();
                    let clean_exe = exe_str.strip_prefix(r"\\?\").unwrap_or(&exe_str);
                    if exe_inside_root(clean_exe, &clean_root) {
                        let _ = proc.kill();
                    }
                }
            }
        }
    }
}

/// Search for checkpoints on disk strictly inside the app folder.
pub fn scan_local_checkpoints() -> Vec<(String, u64)> {
    let mut results = Vec::new();
    let dirs = [
        checkpoints_dir(),
        crate::paths::comfyui_dir().join("models").join("checkpoints"),
        crate::paths::models_dir().join("checkpoints"),
    ];

    for dir in dirs {
        if !dir.exists() {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_file() {
                    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
                    if ext == "safetensors" || ext == "ckpt" {
                        if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                            if size > 1_000_000 && !results.iter().any(|(n, _)| n == name) {
                                results.push((name.to_string(), size));
                            }
                        }
                    }
                }
            }
        }
    }
    results
}

fn format_bytes(bytes: u64) -> String {
    let gb = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    format!("{gb:.1} ГБ")
}

/// Whether a file in the checkpoints folder belongs to image generation.
/// The name is the only type signal: the checkpoint itself has no metadata about whether
/// it is images or audio, and ComfyUI puts audio checkpoints (`stable_audio`,
/// `ace_step`) and video models (`wan2`) in the same `models/checkpoints` folder.
/// Known-foreign families are listed by name: in the image showcase such
/// a file can be neither selected as the active model nor downloaded — it only gets in the way.
pub fn is_image_checkpoint(filename: &str) -> bool {
    const NOT_IMAGE: [&str; 8] = [
        "audio", "ace_step", "ace-step", "diffrhythm", "hunyuanvideo", "hunyuan_video", "wan2", "wan_2",
    ];
    let lower = filename.to_ascii_lowercase();
    !NOT_IMAGE.iter().any(|needle| lower.contains(needle))
}

/// Name of the active image model that can be handed to generation: the selected
/// file may be missing from disk (deleted outside the app, folder moved) —
/// then take the first installed checkpoint, otherwise ST is configured for
/// a nonexistent file and generation fails. If there is not a single
/// checkpoint on disk, leave the user's choice as is.
pub fn resolve_active_model(active: &str) -> Option<String> {
    let installed = scan_local_checkpoints();
    let usable = |name: &str| is_image_checkpoint(name) && installed.iter().any(|(n, _)| n.eq_ignore_ascii_case(name));
    if usable(active) {
        return None;
    }
    installed
        .iter()
        .find(|(name, _)| is_image_checkpoint(name))
        .map(|(name, _)| name.clone())
}

pub fn get_curated_model_url(filename: &str) -> Option<&'static str> {
    match filename {
        // SDXL (12 GB+ VRAM)
        name if name == DEFAULT_IMAGE_CHECKPOINT => {
            Some("https://huggingface.co/frankjoshua/waiIllustriousSDXL_v170/resolve/main/waiIllustriousSDXL_v170.safetensors")
        }
        "animagine-xl-4.0.safetensors" => {
            Some("https://huggingface.co/cagliostrolab/animagine-xl-4.0/resolve/main/animagine-xl-4.0.safetensors")
        }
        "ponyDiffusionV6XL_v6StartWithThisOne.safetensors" => {
            Some("https://huggingface.co/Magamanny/Pony-Diffusion-V6-XL/resolve/main/ponyDiffusionV6XL_v6StartWithThisOne.safetensors")
        }
        // SD 1.5 (8 GB VRAM and below)
        "AnythingV5V3_v5PrtRE.safetensors" => {
            Some("https://huggingface.co/ckpt/anything-v5.0/resolve/main/AnythingV5V3_v5PrtRE.safetensors")
        }
        "Meina V10 - baked VAE.safetensors" => {
            Some("https://huggingface.co/Meina/MeinaMix/resolve/main/Meina%20V10%20-%20baked%20VAE.safetensors")
        }
        "AOM3_orangemixs.safetensors" => {
            Some("https://huggingface.co/WarriorMama777/OrangeMixs/resolve/main/Models/AbyssOrangeMix3/AOM3_orangemixs.safetensors")
        }
        _ => None,
    }
}

pub fn get_curated_model_urls(filename: &str) -> Vec<&'static str> {
    match filename {
        "ponyDiffusionV6XL_v6StartWithThisOne.safetensors" => vec![
            "https://huggingface.co/Magamanny/Pony-Diffusion-V6-XL/resolve/main/ponyDiffusionV6XL_v6StartWithThisOne.safetensors",
            "https://huggingface.co/AiAF/ponyDiffusionV6XL_v6StartWithThisOne.safetensors/resolve/main/ponyDiffusionV6XL_v6StartWithThisOne.safetensors",
        ],
        _ => get_curated_model_url(filename).into_iter().collect(),
    }
}

/// Get the catalog of curated and local checkpoint models.
/// The showcase is trimmed by hardware tier: on a card without image generation
/// (pure CPU) the list is empty, on 8 GB there is no SDXL.
pub fn get_curated_models(active_model: &str, hardware: &crate::gpu::GpuInfo) -> Vec<ImageModelInfo> {
    let Some(ceiling) = crate::catalog::image_ceiling(hardware) else {
        return Vec::new();
    };
    let installed_checkpoints = scan_local_checkpoints();

    let curated = [
        // SDXL — for 12 GB+ VRAM
        ImageModelInfo {
            filename: DEFAULT_IMAGE_CHECKPOINT.to_string(),
            name: "Illustrious SDXL (v1.7.0)".to_string(),
            badge: l10n::t("Флагман аниме без цензуры · SDXL", "Anime flagship, uncensored · SDXL"),
            size: l10n::t("~6.9 ГБ", "~6.9 GB"),
            description: l10n::t(
                "Передовой чекпоинт поколения SDXL для аниме- и VTuber-стилистики. Полное отсутствие цензуры, точное понимание сложной анатомии, поз, эмоций и деталей одежды персонажей.",
                "A cutting-edge SDXL-generation checkpoint for anime and VTuber styling. Fully uncensored, with a precise grasp of complex anatomy, poses, emotions and character outfit details.",
            ),
            roleplay_style: l10n::t(
                "Каноничные аниме-портреты, динамичные ракурсы, эротические и романтические сцены без цензурных искажений.",
                "Canonical anime portraits, dynamic angles, erotic and romantic scenes without censorship artifacts.",
            ),
            is_installed: false,
            is_active: false,
            recommended: true,
            vram_tier: "sdxl".to_string(),
            min_tier: crate::gpu::HardwareTier::Mid,
        },
        ImageModelInfo {
            filename: "animagine-xl-4.0.safetensors".to_string(),
            name: "Animagine XL (4.0)".to_string(),
            badge: l10n::t("Высокая детализация аниме · SDXL", "Highly detailed anime · SDXL"),
            size: l10n::t("~6.9 ГБ", "~6.9 GB"),
            description: l10n::t(
                "Классический проверенный чекпоинт поколения SDXL с широкой базой тегов Danbooru без цензуры. Идеальное попадание в дизайн персонажей по имени и ключевым атрибутам внешности.",
                "A classic, proven SDXL-generation checkpoint with a broad uncensored Danbooru tag base. Nails character designs by name and key appearance attributes.",
            ),
            roleplay_style: l10n::t(
                "Чистый стиль японской анимации, точные костюмы, высокая детализация фонов.",
                "Clean Japanese animation style, accurate outfits, highly detailed backgrounds.",
            ),
            is_installed: false,
            is_active: false,
            recommended: false,
            vram_tier: "sdxl".to_string(),
            min_tier: crate::gpu::HardwareTier::Mid,
        },
        ImageModelInfo {
            filename: "ponyDiffusionV6XL_v6StartWithThisOne.safetensors".to_string(),
            name: "Pony Diffusion V6 XL".to_string(),
            badge: l10n::t("Эталон NSFW без цензуры · SDXL Pony", "The NSFW reference, uncensored · SDXL Pony"),
            size: l10n::t("~6.9 ГБ", "~6.9 GB"),
            description: l10n::t(
                "Культовый чекпоинт без цензуры для любых персонажей и откровенных сцен. Максимальная свобода генерации без каких-либо фильтров, идеальная анатомия и эротика.",
                "An iconic uncensored checkpoint for any characters and explicit scenes. Maximum generation freedom without any filters, perfect anatomy and erotica.",
            ),
            roleplay_style: l10n::t(
                "Любые откровенные и пикантные сцены 18+, динамичные ракурсы и взаимодействие.",
                "Any explicit and spicy 18+ scenes, dynamic angles and interaction.",
            ),
            is_installed: false,
            is_active: false,
            recommended: false,
            vram_tier: "sdxl".to_string(),
            min_tier: crate::gpu::HardwareTier::Mid,
        },
        // SD 1.5 — for 8 GB VRAM and below
        ImageModelInfo {
            filename: "AnythingV5V3_v5PrtRE.safetensors".to_string(),
            name: "Anything V5".to_string(),
            badge: l10n::t("Быстрый аниме без цензуры · SD 1.5 · 8 ГБ", "Fast uncensored anime · SD 1.5 · 8 GB"),
            size: l10n::t("~2.1 ГБ", "~2.1 GB"),
            description: l10n::t(
                "Легковесный чекпоинт SD 1.5. Полная свобода генерации, отличная анатомия, быстрый инференс на 8 ГБ VRAM.",
                "A lightweight SD 1.5 checkpoint. Full generation freedom, great anatomy, fast inference on 8 GB VRAM.",
            ),
            roleplay_style: l10n::t(
                "Быстрая генерация аниме-артов, откровенные сцены без ограничений.",
                "Fast anime art generation, explicit scenes without limits.",
            ),
            is_installed: false,
            is_active: false,
            recommended: true,
            vram_tier: "sd15".to_string(),
            min_tier: crate::gpu::HardwareTier::Minimal,
        },
        ImageModelInfo {
            filename: "Meina V10 - baked VAE.safetensors".to_string(),
            name: "MeinaMix V10".to_string(),
            badge: l10n::t("Высокое качество аниме · SD 1.5 · 8 ГБ", "High-quality anime · SD 1.5 · 8 GB"),
            size: l10n::t("~3.5 ГБ", "~3.5 GB"),
            description: l10n::t(
                "Эталонный аниме-чекпоинт SD 1.5. Без цензуры, идеален для портретов и сцен с персонажами.",
                "A reference SD 1.5 anime checkpoint. Uncensored, ideal for portraits and character scenes.",
            ),
            roleplay_style: l10n::t(
                "Красивые аниме-портреты, мягкие тона, высокая детализация волос и глаз.",
                "Beautiful anime portraits, soft tones, highly detailed hair and eyes.",
            ),
            is_installed: false,
            is_active: false,
            recommended: false,
            vram_tier: "sd15".to_string(),
            min_tier: crate::gpu::HardwareTier::Minimal,
        },
        ImageModelInfo {
            filename: "AOM3_orangemixs.safetensors".to_string(),
            name: "AbyssOrangeMix3 (AOM3)".to_string(),
            badge: l10n::t("Универсальный аниме · SD 1.5 · 8 ГБ", "Versatile anime · SD 1.5 · 8 GB"),
            size: l10n::t("~2.1 ГБ", "~2.1 GB"),
            description: l10n::t(
                "Популярный универсальный аниме-чекпоинт SD 1.5 без цензуры. Широкий спектр стилей.",
                "A popular all-round uncensored SD 1.5 anime checkpoint. A wide range of styles.",
            ),
            roleplay_style: l10n::t(
                "Разнообразные стили аниме, от дружеских сцен до откровенных.",
                "Diverse anime styles, from friendly scenes to explicit ones.",
            ),
            is_installed: false,
            is_active: false,
            recommended: false,
            vram_tier: "sd15".to_string(),
            min_tier: crate::gpu::HardwareTier::Minimal,
        },
    ];

    let mut list = Vec::new();
    let mut matched_filenames = std::collections::HashSet::new();

    for mut m in curated {
        if m.min_tier > ceiling {
            continue;
        }
        if let Some((installed_name, size)) = installed_checkpoints.iter().find(|(name, _)| name.eq_ignore_ascii_case(&m.filename)) {
            m.is_installed = true;
            m.filename = installed_name.clone();
            m.size = format!("~{}", format_bytes(*size));
        }
        if !m.is_installed && m.filename == "animagine-xl-4.0.safetensors" {
            if let Some((alt_name, size)) = installed_checkpoints.iter().find(|(name, _)| name.to_ascii_lowercase().starts_with("animagine-xl-4.0")) {
                m.is_installed = true;
                m.filename = alt_name.clone();
                m.size = format!("~{}", format_bytes(*size));
            }
        }
        if !m.is_installed && m.filename == "ponyDiffusionV6XL_v6StartWithThisOne.safetensors" {
            if let Some((alt_name, size)) = installed_checkpoints.iter().find(|(name, _)| {
                let lower = name.to_ascii_lowercase();
                lower.starts_with("ponydiffusionv6xl") || lower.starts_with("v6_start_with_this_one") || lower.starts_with("pony_diffusion_v6")
            }) {
                m.is_installed = true;
                m.filename = alt_name.clone();
                m.size = format!("~{}", format_bytes(*size));
            }
        }
        if !m.is_installed && m.filename.to_ascii_lowercase().starts_with("anything") {
            if let Some((alt_name, size)) = installed_checkpoints.iter().find(|(name, _)| {
                let lower = name.to_ascii_lowercase();
                lower.starts_with("anything-v5") || lower.starts_with("anything_v5") || lower.starts_with("anythingv5")
            }) {
                m.is_installed = true;
                m.filename = alt_name.clone();
                m.size = format!("~{}", format_bytes(*size));
            }
        }
        if !m.is_installed && m.filename.to_ascii_lowercase().starts_with("meina") {
            if let Some((alt_name, size)) = installed_checkpoints.iter().find(|(name, _)| {
                let lower = name.to_ascii_lowercase();
                lower.starts_with("meinamix") || lower.starts_with("meina_v10") || lower.starts_with("meina")
            }) {
                m.is_installed = true;
                m.filename = alt_name.clone();
                m.size = format!("~{}", format_bytes(*size));
            }
        }
        if !m.is_installed && m.filename.to_ascii_lowercase().starts_with("aom3") {
            if let Some((alt_name, size)) = installed_checkpoints.iter().find(|(name, _)| {
                let lower = name.to_ascii_lowercase();
                lower.starts_with("aom3") || lower.starts_with("abyssorangemix3") || lower.starts_with("abyss_orange_mix")
            }) {
                m.is_installed = true;
                m.filename = alt_name.clone();
                m.size = format!("~{}", format_bytes(*size));
            }
        }
        m.is_active = m.filename.eq_ignore_ascii_case(active_model)
            || (active_model.is_empty() && m.filename == DEFAULT_IMAGE_CHECKPOINT);
        matched_filenames.insert(m.filename.clone());
        list.push(m);
    }

    for (filename, size) in &installed_checkpoints {
        if !matched_filenames.contains(filename) && is_image_checkpoint(filename) {
            let is_active = filename.eq_ignore_ascii_case(active_model);
            let display_name = filename
                .strip_suffix(".safetensors")
                .or_else(|| filename.strip_suffix(".ckpt"))
                .unwrap_or(filename)
                .replace('_', " ");

            // Estimate a local file's tier by size: SDXL checkpoints ~6.9 GB,
            // SD 1.5 — 2–4 GB. Do not show a heavy file on a weak card.
            let min_tier = if *size > 5 * 1024 * 1024 * 1024 {
                crate::gpu::HardwareTier::Mid
            } else {
                crate::gpu::HardwareTier::Minimal
            };
            if min_tier > ceiling {
                continue;
            }

            list.push(ImageModelInfo {
                filename: filename.clone(),
                name: display_name,
                badge: l10n::t("Локальный чекпоинт", "Local checkpoint"),
                size: format!("~{}", format_bytes(*size)),
                description: l10n::t("Локальная модель из каталога чекпоинтов ComfyUI.", "A local model from the ComfyUI checkpoint folder."),
                roleplay_style: l10n::t("Пользовательский стиль.", "Your own style."),
                is_installed: true,
                is_active,
                recommended: false,
                vram_tier: "unknown".to_string(),
                min_tier,
            });
        }
    }

    list
}

/// Mirror order: the one owning the partial stub goes first, the rest
/// in the previous order.
fn mirror_order<'a>(urls: &[&'a str], partial_source: Option<&str>) -> Vec<&'a str> {
    let mut ordered = urls.to_vec();
    if let Some(at) = partial_source.and_then(|source| ordered.iter().position(|url| *url == source)) {
        let owner = ordered.remove(at);
        ordered.insert(0, owner);
    }
    ordered
}

/// Download a checkpoint from the curated model catalog.
///
/// The partial `.download` is the resume point: a connection drop, pause and app exit
/// do not restart the six-gigabyte file from scratch.
pub async fn download_image_model(
    filename: &str,
    flags: crate::downloads::FlagsRef,
    on_progress: std::sync::Arc<dyn Fn(u64, u64, String) + Send + Sync>,
) -> Result<PathBuf, String> {
    let urls = get_curated_model_urls(filename);
    if urls.is_empty() {
        return Err(format!("Неизвестный URL для модели: {filename}"));
    }

    let dest_dir = checkpoints_dir();
    std::fs::create_dir_all(&dest_dir).map_err(|e| e.to_string())?;
    let target_path = dest_dir.join(filename);
    if target_path.exists() {
        return Ok(target_path);
    }

    let temp_path = dest_dir.join(format!("{filename}.download"));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3 * 3600))
        .build()
        .map_err(|e| e.to_string())?;

    let label = format!("Скачивание {filename}");
    // The previous run downloaded the whole file, but rename failed: download_to_file already erased the meta,
    // and without the marker the "sourceless" stub would be deleted and downloaded again
    let complete = complete_marker(&temp_path);
    if complete.exists() && temp_path.exists() {
        if std::fs::rename(&temp_path, &target_path).is_ok() {
            let _ = std::fs::remove_file(&complete);
            let size = std::fs::metadata(&target_path).map(|m| m.len()).unwrap_or(0);
            on_progress(size, size, "Модель готова".into());
            return Ok(target_path);
        }
        return Err("Файл модели скачан, но занят другой программой — закройте её и повторите".into());
    }
    let _ = std::fs::remove_file(&complete);
    let mut last_err = String::new();
    // The second line of `.download.total` is the URL the stub was downloaded from (format
    // `paths::download_to_file`). Start from its mirror: on the first mirror
    // download_to_file would discard the foreign stub, and a pause on the second would download
    // the file from scratch.
    let partial_source = std::fs::read_to_string(crate::paths::download_total_path(&temp_path))
        .ok()
        .and_then(|text| text.lines().nth(1).map(|line| line.trim().to_string()));
    let urls = mirror_order(&urls, partial_source.as_deref());

    for (index, url) in urls.iter().enumerate() {
        for attempt in 1..=2 {
            // Do not reset progress to zero: retrying the same mirror continues
            // what is already downloaded, and the bar must not jump back.
            let (have, known_total) = crate::paths::download_partial(&temp_path);
            if attempt > 1 {
                on_progress(
                    have,
                    known_total,
                    format!("Связь прервалась — продолжаю {filename}…"),
                );
            } else {
                on_progress(
                    have,
                    known_total,
                    format!("Подключение к серверу загрузки {filename}…"),
                );
            }
            match crate::paths::download_to_file(
                &client,
                url,
                &temp_path,
                &label,
                flags.clone(),
                on_progress.clone(),
            )
            .await
            {
                Ok(size) => {
                    // Antivirus or Explorer holds the file for seconds: retry, and do not erase
                    // the fully downloaded file — the next attempt will finish without the network.
                    let mut renamed = std::fs::rename(&temp_path, &target_path);
                    for _ in 0..3 {
                        if renamed.is_ok() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        renamed = std::fs::rename(&temp_path, &target_path);
                    }
                    if let Err(e) = renamed {
                        let _ = std::fs::write(complete_marker(&temp_path), b"");
                        return Err(format!("Не удалось сохранить файл: {e}. Скачанное сохранено — повторите позже"));
                    }
                    on_progress(size, size, "Модель готова".into());
                    return Ok(target_path);
                }
                // Pause and cancel do not touch the stub: it is the resume point.
                Err(error) if error == crate::downloads::PAUSED || error == crate::downloads::CANCELLED => {
                    return Err(error);
                }
                Err(error) => last_err = error,
            }
        }
        // Another mirror — from scratch: the file on the second source may differ
        // from what is already in the stub. Do not touch the last mirror's stub:
        // it is the resume point for the next attempt; otherwise a drop
        // in the middle of the six-gigabyte file would start it from scratch.
        if index + 1 < urls.len() {
            let _ = std::fs::remove_file(&temp_path);
            let _ = std::fs::remove_file(crate::paths::download_total_path(&temp_path));
        }
    }

    Err(last_err)
}

fn complete_marker(temp_path: &std::path::Path) -> PathBuf {
    let mut name = temp_path.as_os_str().to_os_string();
    name.push(".complete");
    PathBuf::from(name)
}

/// Delete a checkpoint file from disk to free space.
pub async fn delete_image_model(filename: &str) -> Result<(), String> {
    let filename_path = std::path::Path::new(filename);
    if filename_path.file_name().and_then(|n| n.to_str()) != Some(filename) {
        return Err("Недопустимое имя файла модели".to_string());
    }

    // If ComfyUI is running, send a request to unload models from memory,
    // to release file handles on Windows before deleting.
    if is_comfy_running(8188) {
        // Loopback ComfyUI (127.0.0.1:8188): bypass the system proxy (otherwise
        // the user's machine proxy answers 503 instead of the local server).
        if let Ok(client) = reqwest::Client::builder()
            .timeout(Duration::from_millis(1500))
            .no_proxy()
            .build()
        {
            let body = serde_json::json!({
                "unload_models": true,
                "free_memory": true,
            });
            let _ = client.post("http://127.0.0.1:8188/free").json(&body).send().await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    let dirs = [
        checkpoints_dir(),
        crate::paths::comfyui_dir().join("models").join("checkpoints"),
        crate::paths::models_dir().join("checkpoints"),
    ];

    let mut deleted_any = false;
    let mut last_error = None;

    for dir in dirs {
        if !dir.exists() {
            continue;
        }

        // 1. Check the exact file name
        let exact_path = dir.join(filename);
        if exact_path.exists() && exact_path.is_file() {
            if let Err(e) = std::fs::remove_file(&exact_path) {
                last_error = Some(format!("Не удалось удалить {}: {e}", exact_path.display()));
            } else {
                deleted_any = true;
            }
        }

        // 2. Delete the temporary .download file if it is left (and its full
        // size next to it: without it the `.download.total` file remains as garbage).
        let temp_download = dir.join(format!("{filename}.download"));
        if temp_download.exists() {
            let _ = std::fs::remove_file(&temp_download);
            let _ = std::fs::remove_file(crate::paths::download_total_path(&temp_download));
        }

        // 3. If no exact match is found, search case-insensitively
        if !deleted_any {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_file() {
                        if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                            if name.eq_ignore_ascii_case(filename) {
                                if let Err(e) = std::fs::remove_file(&p) {
                                    last_error = Some(format!("Не удалось удалить {}: {e}", p.display()));
                                } else {
                                    deleted_any = true;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    if deleted_any {
        Ok(())
    } else if let Some(err) = last_error {
        Err(err)
    } else {
        Err(format!("Файл модели «{filename}» не найден на диске"))
    }
}

/// Remove a half-assembled portable ComfyUI tree. Delete only the top
/// archive directory: the model store junction and neighboring files are not touched.
fn cleanup_partial_extract(root: &std::path::Path) {
    let _ = std::fs::remove_dir_all(root.join("ComfyUI_windows_portable"));
}

// Blocking unpacking thread: owns tar until exit or cancel.
fn wait_extract_child(mut child: Child, cancel: &AtomicBool) -> Result<(std::process::ExitStatus, String), String> {
    let pid = child.id();
    EXTRACT_PID.store(pid, Ordering::SeqCst);
    let stderr = child.stderr.take();
    let drain = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
        use std::io::Read;
        let mut tail = Vec::new();
        if let Some(mut stderr) = stderr {
            let mut buffer = [0u8; 8192];
            loop {
                let read = stderr.read(&mut buffer)?;
                if read == 0 { break; }
                tail.extend_from_slice(&buffer[..read]);
                if tail.len() > 16 * 1024 { tail.drain(..tail.len() - 16 * 1024); }
            }
        }
        Ok(tail)
    });
    let outcome = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            break Err("отменено пользователем".to_string());
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(format!("Не удалось дождаться распаковки: {e}"));
            }
        }
    };
    EXTRACT_PID.store(0, Ordering::SeqCst);
    let tail = drain.join().map_err(|_| "Не удалось прочитать журнал распаковки".to_string())?
        .map_err(|e| format!("Не удалось прочитать журнал распаковки: {e}"))?;
    Ok((outcome?, String::from_utf8_lossy(&tail).into_owned()))
}

/// Install portable ComfyUI into the `comfyui/` directory inside the app folder.
pub async fn install_comfyui(
    cancel: std::sync::Arc<AtomicBool>,
    on_progress: std::sync::Arc<dyn Fn(u64, u64, String) + Send + Sync>,
) -> Result<PathBuf, String> {
    let root = crate::paths::comfyui_dir();
    if is_installed() {
        return Ok(root);
    }
    let Some(_guard) = InstallGuard::acquire() else {
        return Err("Установка ComfyUI уже идёт — дождитесь её окончания.".into());
    };
    // Cancel reset — only by the install owner: a repeated click that
    // the guard rejects would otherwise erase the cancel of a download still in progress.
    cancel.store(false, Ordering::Relaxed);

    let url = COMFY_PORTABLE_URL;
    // No overall timeout: a two-gigabyte archive on a slow link would not
    // fit in it. Connection silence is caught by the downloader itself (60 s).
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;

    let archive_path = crate::paths::base_dir().join("comfyui_portable.7z");
    // "archive fully downloaded" marker: `download_to_file` deletes
    // `.total` on success, and after an unpack failure a whole archive looked like a foreign stub —
    // a retry downloaded ~2 GB again.
    let complete_marker = crate::paths::base_dir().join("comfyui_portable.7z.complete");
    let archive_ready = std::fs::read_to_string(&complete_marker).ok().as_deref() == Some(url)
        && std::fs::metadata(&archive_path)
            .map(|meta| meta.len() > 0)
            .unwrap_or(false);
    if archive_ready {
        let total = std::fs::metadata(&archive_path).map(|m| m.len()).unwrap_or(0);
        on_progress(total, total, "Архив ComfyUI уже скачан — распаковка".into());
    } else {
        let (have, known_total) = crate::paths::download_partial(&archive_path);
        if have > 0 {
            // Show that the download is not from scratch: progress is visible before the first byte.
            on_progress(have, known_total, "Загрузка ComfyUI… (продолжаю)".to_string());
        } else {
            on_progress(0, 0, "Подключение к репозиторию ComfyUI…".into());
        }

        // Resume: the partial archive stays on disk, so exiting the app
        // or a connection drop does not restart the download.
        let progress_inner = on_progress.clone();
        crate::paths::download_to_file(
            &client,
            url,
            &archive_path,
            "Загрузка ComfyUI",
            crate::downloads::Flags::with_cancel(cancel.clone()),
            std::sync::Arc::new(move |got, total, status| progress_inner(got, total, status)),
        )
        .await?;
        let _ = std::fs::write(&complete_marker, url);
    }

    let total = std::fs::metadata(&archive_path).map(|m| m.len()).unwrap_or(0);
    on_progress(total, total, "Распаковка ComfyUI в папку приложения…".into());
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;

    let mut cmd = Command::new("tar.exe");
    cmd.arg("-xf")
        .arg(&archive_path)
        .arg("-C")
        .arg(&root)
        .stderr(Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);

    let child = cmd
        .spawn()
        .map_err(|e| format!("Не удалось запустить распаковку: {e}"))?;
    let wait_cancel = cancel.clone();
    let waited = tokio::task::spawn_blocking(move || wait_extract_child(child, &wait_cancel))
        .await
        .unwrap_or_else(|e| Err(format!("Распаковка прервалась: {e}")));
    let (status, text) = match waited {
        Ok(output) => output,
        Err(error) => {
            cleanup_partial_extract(&root);
            return Err(error);
        }
    };
    if !status.success() {
        // Keep the archive and the marker: the next attempt does not download again. And
        // remove the half-assembled tree: otherwise `is_installed()` saw main.py
        // and python.exe, the install was considered complete, and the archive was deleted.
        cleanup_partial_extract(&root);
        return Err(format!("Ошибка распаковки архива: {text}"));
    }

    // Delete the archive only after a successful unpack — otherwise on error
    // the user loses ~2 GB of downloads and is forced to download again.
    let _ = std::fs::remove_file(&archive_path);
    let _ = std::fs::remove_file(&complete_marker);

    let _ = std::fs::create_dir_all(checkpoints_dir());
    if is_installed() {
        on_progress(total, total, "ComfyUI успешно установлен".into());
        Ok(root)
    } else {
        Err("После распаковки исполняемые файлы ComfyUI не обнаружены в папке приложения".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::{build_vendor_info, GpuVendor};

    #[cfg(windows)]
    #[test]
    fn audit_extract_drains_stderr_before_waiting_for_exit() {
        let mut command = Command::new("powershell.exe");
        command.args(["-NoProfile", "-NonInteractive", "-Command", "[Console]::Error.Write(('x' * 262144)); exit 0"])
            .stdout(Stdio::null()).stderr(Stdio::piped()).creation_flags(0x0800_0000);
        let child = command.spawn().unwrap();
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let timeout_cancel = cancel.clone();
        let timer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(2));
            timeout_cancel.store(true, Ordering::Relaxed);
        });
        let output = wait_extract_child(child, &cancel);
        timer.join().unwrap();
        let (status, stderr) = output.expect("stderr pipe blocked child completion");
        assert!(status.success());
        assert!(stderr.ends_with('x'));
    }

    /// Powerful NVIDIA machine profile: the checkpoint showcase is not trimmed.
    fn cuda_16gb() -> crate::gpu::GpuInfo {
        build_vendor_info(GpuVendor::Nvidia, "NVIDIA GeForce RTX 5080".into(), 16303)
    }

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0 ГБ");
        assert_eq!(format_bytes(6938350040), "6.5 ГБ");
    }

    #[test]
    fn test_get_curated_models_active() {
        let models = get_curated_models(DEFAULT_IMAGE_CHECKPOINT, &cuda_16gb());
        assert!(!models.is_empty());
        let illustrious = models.iter().find(|m| m.filename == DEFAULT_IMAGE_CHECKPOINT).unwrap();
        assert!(illustrious.is_active);
    }

    #[test]
    fn test_get_curated_model_url() {
        // SDXL (12 GB+)
        assert!(get_curated_model_url(DEFAULT_IMAGE_CHECKPOINT).is_some());
        assert!(get_curated_model_url("animagine-xl-4.0.safetensors").is_some());
        assert!(get_curated_model_url("ponyDiffusionV6XL_v6StartWithThisOne.safetensors").is_some());
        // SD 1.5 (8 GB)
        assert!(get_curated_model_url("AnythingV5V3_v5PrtRE.safetensors").is_some());
        assert!(get_curated_model_url("Meina V10 - baked VAE.safetensors").is_some());
        assert!(get_curated_model_url("AOM3_orangemixs.safetensors").is_some());
        // Nonexistent model
        assert!(get_curated_model_url("unknown.safetensors").is_none());
    }

    #[test]
    fn test_get_curated_models_vram_tiers() {
        let models = get_curated_models("", &cuda_16gb());
        let sdxl: Vec<_> = models.iter().filter(|m| m.vram_tier == "sdxl").collect();
        let sd15: Vec<_> = models.iter().filter(|m| m.vram_tier == "sd15").collect();
        assert_eq!(sdxl.len(), 3, "Должно быть 3 SDXL модели");
        assert_eq!(sd15.len(), 3, "Должно быть 3 SD 1.5 модели");
    }

    #[test]
    fn test_vitrine_hides_sdxl_on_eight_gigabytes() {
        let weak = build_vendor_info(GpuVendor::Nvidia, "NVIDIA GeForce RTX 3060".into(), 8192);
        let models = get_curated_models("", &weak);
        assert!(models.iter().all(|m| m.vram_tier != "sdxl"), "SDXL не должен показываться на 8 ГБ");
        assert!(models.iter().any(|m| m.vram_tier == "sd15"));
    }

    #[test]
    fn test_vitrine_is_empty_without_gpu() {
        let cpu = build_vendor_info(GpuVendor::Unknown, "Microsoft Basic Display Adapter".into(), 0);
        assert!(get_curated_models("", &cpu).is_empty());
    }

    #[test]
    fn test_amd_polaris_has_no_checkpoints() {
        let polaris = build_vendor_info(GpuVendor::Amd, "Radeon RX 580 Series".into(), 8192);
        assert!(get_curated_models("", &polaris).is_empty());
    }

    #[test]
    fn test_comfyui_is_installed() {
        // Verify the call does not panic regardless of whether ComfyUI is present on the machine
        let _ = is_installed();
    }

    /// The archive's portable layout (`ComfyUI_windows_portable\ComfyUI\main.py` and
    /// `python_embeded\python.exe`) must be recognized as an install, and its models
    /// folder must sit next to `main.py`: the store junction is placed by it.
    /// A miss cost 2 GB of re-downloading the archive and an empty checkpoint showcase.
    /// Also verify that the archive of an interrupted install with a ready ComfyUI
    /// is deleted — otherwise it stays in the program folder.
    #[test]
    fn test_portable_layout_is_detected_and_archive_dropped() {
        assert_eq!(discard_installer_archive(), 0, "без установки архив не трогаем");

        let root = crate::paths::comfyui_dir().join("ComfyUI_windows_portable");
        let main = root.join("ComfyUI");
        std::fs::create_dir_all(&main).unwrap();
        std::fs::write(main.join("main.py"), b"# test").unwrap();
        std::fs::create_dir_all(root.join("python_embeded")).unwrap();
        std::fs::write(root.join("python_embeded").join("python.exe"), b"# test").unwrap();

        assert_eq!(get_comfy_main_dir(), Some(main.clone()));
        assert!(is_installed());
        assert_eq!(checkpoints_dir(), main.join("models").join("checkpoints"));

        let archive = crate::paths::base_dir().join("comfyui_portable.7z");
        std::fs::write(&archive, b"partial archive").unwrap();
        std::fs::write(crate::paths::download_total_path(&archive), b"2047").unwrap();
        assert!(discard_installer_archive() > 0);
        assert!(!archive.exists());
        assert!(!crate::paths::download_total_path(&archive).exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_scan_local_checkpoints_finds_files() {
        let models = get_curated_models(DEFAULT_IMAGE_CHECKPOINT, &cuda_16gb());
        assert!(!models.is_empty());
        assert!(models.iter().any(|m| m.filename == DEFAULT_IMAGE_CHECKPOINT));
    }

    #[test]
    fn test_audio_and_video_checkpoints_are_not_image_models() {
        assert!(!is_image_checkpoint("stable_audio_open_1.0.safetensors"));
        assert!(!is_image_checkpoint("ace_step_v1_3.5b.safetensors"));
        assert!(!is_image_checkpoint("wan2.1_t2v_14B.safetensors"));
        assert!(is_image_checkpoint(DEFAULT_IMAGE_CHECKPOINT));
        assert!(is_image_checkpoint("DreamShaperXL_Turbo_v2_1.safetensors"));
        assert!(is_image_checkpoint("ponyDiffusionV6XL_v6StartWithThisOne.safetensors"));
    }

    #[tokio::test]
    async fn test_delete_image_model_validation_and_lifecycle() {
        // Path traversal protection check
        assert!(delete_image_model("../sneaky.safetensors").await.is_err());
        assert!(delete_image_model("sub/folder/model.safetensors").await.is_err());
        assert!(delete_image_model("non_existent_model_xyz_123.safetensors").await.is_err());

        // Check deleting an existing test file
        let test_dir = checkpoints_dir();
        let _ = std::fs::create_dir_all(&test_dir);
        let test_filename = "temp_test_dummy_model.safetensors";
        let test_path = test_dir.join(test_filename);
        let test_download_path = test_dir.join(format!("{test_filename}.download"));
        let test_total_path = crate::paths::download_total_path(&test_download_path);

        std::fs::write(&test_path, b"dummy checkpoint data").unwrap();
        std::fs::write(&test_download_path, b"temp download data").unwrap();
        std::fs::write(&test_total_path, b"2048").unwrap();
        assert!(test_path.exists());
        assert!(test_download_path.exists());
        assert!(test_total_path.exists());

        let res = delete_image_model(test_filename).await;
        assert!(res.is_ok(), "delete_image_model should succeed for existing file: {:?}", res);
        assert!(!test_path.exists(), "Model file should be deleted");
        assert!(!test_download_path.exists(), "Download temp file should be deleted");
        // The full size of a partial file goes away with it: otherwise in the checkpoints
        // folder `.download.total` remains without the stub itself.
        assert!(!test_total_path.exists(), "Размер огрызка должен удаляться вместе с ним");
    }

    #[test]
    fn test_mirror_owning_the_partial_goes_first() {
        let urls = ["https://a/x", "https://b/x", "https://c/x"];
        assert_eq!(mirror_order(&urls, Some("https://b/x")), ["https://b/x", "https://a/x", "https://c/x"]);
        assert_eq!(mirror_order(&urls, None), urls);
        assert_eq!(mirror_order(&urls, Some("https://чужой/x")), urls);
    }

    /// ComfyUI process identification — by directory boundary (V-39): `comfyui-backup`
    /// next to `comfyui` is not ours, even though as a string it starts with the same root.
    #[test]
    fn test_comfy_process_match_respects_the_directory_boundary() {
        let root = r"z:\aimodels\cloufam\comfyui";
        assert!(exe_inside_root(
            r"z:\aimodels\cloufam\comfyui\python_embeded\python.exe",
            root
        ));
        assert!(exe_inside_root(root, root), "exe ровно на корне — тоже свой");
        assert!(!exe_inside_root(
            r"z:\aimodels\cloufam\comfyui-backup\python_embeded\python.exe",
            root
        ));
        assert!(!exe_inside_root(
            r"z:\aimodels\comfyui\python_embeded\python.exe",
            root
        ));
        assert!(!exe_inside_root(r"c:\python311\python.exe", root));
    }

    #[test]
    fn test_second_install_is_rejected_while_first_runs() {
        let first = InstallGuard::acquire().expect("первая установка");
        assert!(InstallGuard::acquire().is_none(), "вторая установка параллельно первой");
        drop(first);
        let again = InstallGuard::acquire();
        assert!(again.is_some(), "после окончания установка снова доступна");
    }
}

