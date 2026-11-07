//! Text fine-tuning runtime: uv, a managed Python, a venv with the training stack
//! and the llama.cpp sources for converting LoRA to GGUF.
//!
//! Everything is installed into `runtime/trainer` inside the program folder; caches (uv, pip, HF)
//! live in `data`, so that installation and removal stay honest.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use tauri::{AppHandle, Emitter};

const UV_VERSION: &str = "0.12.12";
const UV_ZIP_URL: &str =
    "https://github.com/astral-sh/uv/releases/download/{VERSION}/uv-x86_64-pc-windows-msvc.zip";
const PYTHON_VERSION: &str = "3.11";
const TORCH_INDEX_CUDA: &str = "https://download.pytorch.org/whl/cu128";
const TORCH_INDEX_CPU: &str = "https://download.pytorch.org/whl/cpu";
const LLAMA_CPP_TAG: &str = "b9722";
const LLAMA_CPP_ZIP: &str =
    "https://github.com/ggml-org/llama.cpp/archive/refs/tags/{TAG}.zip";

/// Versions are pinned: the set was verified in live training (see the journal, spike M0);
/// the other pins are a snapshot of a working venv from the installed copy on 25.09.2026.
/// An unpinned package would pull an unverified version on a new machine.
const TORCH_PACKAGE: &str = "torch==2.11.0";
const TRAIN_PACKAGES: &[&str] = &[
    "transformers==4.54.1",
    "accelerate==1.6.0",
    "peft==0.20.0",
    "bitsandbytes==0.50.2",
    "datasets==5.0.1",
    "safetensors==0.8.0",
    "sentencepiece==0.2.2",
    "gguf==0.19.0",
    "protobuf==7.36.1",
    "truststore==0.10.4",
];

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct PyruntimeStatus {
    pub installed: bool,
    pub python: Option<String>,
    pub torch: Option<String>,
    pub llama_cpp: bool,
    pub size_mb: u64,
    pub installing: bool,
    pub stage: Option<String>,
    pub progress: Option<u8>,
    pub error: Option<String>,
}

#[derive(Default, Clone)]
struct InstallState {
    installing: bool,
    stage: Option<String>,
    progress: Option<u8>,
    error: Option<String>,
}

static STATE: Mutex<Option<InstallState>> = Mutex::new(None);
/// Cancellation signal for installation. A shared `Arc` because two worlds read it:
/// the canonical downloader (`downloads::Flags::with_cancel`) and child
/// uv/python steps, killed by pid (V-56).
static CANCEL: OnceLock<Arc<AtomicBool>> = OnceLock::new();

fn cancel_flag() -> Arc<AtomicBool> {
    CANCEL
        .get_or_init(|| Arc::new(AtomicBool::new(false)))
        .clone()
}

fn uv_exe() -> PathBuf {
    crate::paths::trainer_dir().join("uv").join("uv.exe")
}

pub fn python_exe() -> PathBuf {
    crate::paths::trainer_dir()
        .join("venv")
        .join("Scripts")
        .join("python.exe")
}

pub fn llama_cpp_dir() -> PathBuf {
    crate::paths::trainer_dir().join("llama.cpp")
}

/// Rebuild the paths inside `pyvenv.cfg` for the current program location.
///
/// The venv is created during installation and remembers absolute paths. The application
/// is portable: the owner moved it from `Z:\Git\Programs\Cloufam` to
/// `Z:\AImodels\Cloufam`, and the venv kept the old `home` — python did not
/// start at all ("uv trampoline failed to spawn Python child process"), while
/// the training screen showed "runtime not installed" with a live 4.5 GB
/// environment. uv's `--relocatable` only helps when the interpreter moves
/// together with the venv; the recorded path still has to be fixed.
fn reroot_pyvenv(text: &str, trainer_dir: &std::path::Path) -> String {
    const MARKER: &str = "runtime\\trainer\\";
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let rewritten = (|| {
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            if !matches!(key, "home" | "executable" | "command") {
                return None;
            }
            let value = value.trim();
            // ASCII-only search: to_lowercase() may change the byte length
            // (İ → i̇), and an offset from lower would slice value at a foreign index.
            let lower = value.to_ascii_lowercase();
            let at = lower.find(&MARKER.to_ascii_lowercase())?;
            let tail = value.get(at + MARKER.len()..)?;
            let rerooted = trainer_dir.join(tail.replace('/', "\\"));
            (rerooted.to_string_lossy().to_string() != value).then(|| format!("{key} = {}", rerooted.display()))
        })();
        out.push_str(&rewritten.unwrap_or_else(|| line.to_string()));
        out.push('\n');
    }
    out
}

/// Fix the venv if it remembers a different program location. Returns true
/// if the file was rewritten.
fn repair_venv_paths() -> bool {
    let cfg = crate::paths::trainer_dir().join("venv").join("pyvenv.cfg");
    let Ok(text) = std::fs::read_to_string(&cfg) else {
        return false;
    };
    let fixed = reroot_pyvenv(&text, &crate::paths::trainer_dir());
    if fixed == text {
        return false;
    }
    match std::fs::write(&cfg, fixed) {
        Ok(()) => {
            PROBE_CACHE.lock().map(|mut c| *c = None).ok();
            true
        }
        Err(_) => false,
    }
}

/// Depth cap for walking the size tree: the same guard as in `lib.rs` for
/// the size summary — a junction/symlink loop on Windows (manual relocation
/// of the app via links) otherwise drives recursion to stack overflow (V-55).
const DIR_SIZE_MAX_DEPTH: usize = 20;

fn dir_size_mb(dir: &std::path::Path) -> u64 {
    fn walk(dir: &std::path::Path, depth: usize, total: &mut u64) {
        if depth >= DIR_SIZE_MAX_DEPTH {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, depth + 1, total);
            } else if let Ok(meta) = entry.metadata() {
                *total += meta.len();
            }
        }
    }
    let mut total = 0;
    walk(dir, 0, &mut total);
    total / (1024 * 1024)
}

/// A running child installation step (uv, python probe). The `Child` lives in the slot
/// until the step ends: while the handle is open, Windows does not give the pid to another process,
/// so killing by pid from `cancel()` will not hit a foreign process.
struct RunningStep {
    pid: u32,
    child: Child,
}

/// Live child steps. A vector, not a single slot: a status probe may run
/// alongside installation, and both must die on cancellation.
static RUNNING_STEPS: Mutex<Vec<RunningStep>> = Mutex::new(Vec::new());

fn track_step(pid: u32, child: Child) {
    // A poisoned lock does not crash the step or leave it unaccounted: otherwise
    // cancel() would not find the process, and it would live until the installation ends.
    let mut guard = RUNNING_STEPS.lock().unwrap_or_else(|e| e.into_inner());
    guard.push(RunningStep { pid, child });
}

/// Dropping the record: the `Child` drop happens inside the lock block, i.e. the pid
/// stays occupied until the very removal and is not reused between the check and
/// the removal.
fn untrack_step(pid: u32) {
    let mut guard = RUNNING_STEPS.lock().unwrap_or_else(|e| e.into_inner());
    guard.retain(|step| step.pid != pid);
}

/// Kill the tree of every live step by pid (Windows: `taskkill /T /F` — uv and
/// pip spawn children that cannot be reached through the direct child's handle), then
/// append `kill()` to the child itself and remove the records. The model is
/// `image_trainer::cancel`. The lock is held the whole way: dropping the children happens in
/// `clear()` under it, the pid is not reused until the kill completes.
/// Kill the tree of one step (deadline probe) without touching an ongoing installation.
fn kill_step(pid: u32) {
    let mut guard = RUNNING_STEPS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(step) = guard.iter_mut().find(|step| step.pid == pid) {
        #[cfg(windows)]
        {
            let mut cmd = Command::new("taskkill");
            cmd.args(["/PID", &pid.to_string(), "/T", "/F"]);
            hide_console(&mut cmd);
            let _ = cmd.status();
        }
        let _ = step.child.kill();
        let _ = step.child.wait();
    }
    guard.retain(|step| step.pid != pid);
}

fn kill_running_steps() {
    let mut guard = RUNNING_STEPS.lock().unwrap_or_else(|e| e.into_inner());
    for step in guard.iter_mut() {
        #[cfg(windows)]
        {
            let mut cmd = Command::new("taskkill");
            cmd.args(["/PID", &step.pid.to_string(), "/T", "/F"]);
            hide_console(&mut cmd);
            let _ = cmd.status();
        }
        let _ = step.child.kill();
        let _ = step.child.wait();
    }
    guard.clear();
}

/// Output of a child step. The pipes are read by separate threads: a filled
/// stdout pipe would otherwise stop the process before `wait` (uv writes its report there).
#[derive(Debug)]
struct StepOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

fn spawn_reader(stream: Option<impl Read + Send + 'static>) -> Option<std::thread::JoinHandle<String>> {
    stream.map(|mut stream| {
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stream.read_to_string(&mut text);
            text
        })
    })
}

fn join_reader(handle: Option<std::thread::JoinHandle<String>>) -> String {
    handle.and_then(|handle| handle.join().ok()).unwrap_or_default()
}

/// Start a child process and wait for it, checking the cancellation flag (V-56).
///
/// Execution thread: blocking — called from the installation's `spawn_blocking` steps
/// and from status probes. While the process is alive, its `Child` sits in
/// `RUNNING_STEPS`, and `cancel()` kills the tree by pid. Polling `try_wait` every
/// 150 ms was chosen over a blocking `wait`: cancellation must not wait for the end
/// of a multi-gigabyte step, and the lock is not held for the whole wait.
fn run_cancellable(cmd: Command) -> Result<StepOutput, String> {
    run_step(cmd, None)
}

/// Probe (`import torch`, interpreter version) — with a deadline: a probe hung on a broken cache
/// or a DLL blockage gave neither an answer nor an error and repeated every 180 s.
/// 10 minutes, not 60 s: a cold `import torch` on an HDD with antivirus takes longer than a minute,
/// and a false refusal triggered a reinstall of the multi-gigabyte stack.
fn run_probe(cmd: Command) -> Result<StepOutput, String> {
    run_step(cmd, Some(std::time::Duration::from_secs(10 * 60)))
}

fn run_step(mut cmd: Command, deadline: Option<std::time::Duration>) -> Result<StepOutput, String> {
    let started = std::time::Instant::now();
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("не удалось запустить шаг: {e}"))?;
    let pid = child.id();
    let out_thread = spawn_reader(child.stdout.take());
    let err_thread = spawn_reader(child.stderr.take());
    track_step(pid, child);

    let mut cancelled = false;
    let mut timed_out = false;
    let mut wait_error: Option<String> = None;
    let exit = loop {
        if cancel_flag().load(Ordering::Relaxed) {
            cancelled = true;
            break None;
        }
        if deadline.is_some_and(|limit| started.elapsed() > limit) {
            timed_out = true;
            break None;
        }
        let poll = {
            let mut guard = RUNNING_STEPS.lock().unwrap_or_else(|e| e.into_inner());
            match guard.iter_mut().find(|step| step.pid == pid) {
                Some(step) => step.child.try_wait(),
                None => {
                    // The slot is empty only if the step has already been killed from cancel(): a new
                    // process with the same pid does not get in here.
                    cancelled = cancel_flag().load(Ordering::Relaxed);
                    if !cancelled {
                        wait_error = Some("шаг установки пропал из учёта".into());
                    }
                    break None;
                }
            }
        };
        match poll {
            Ok(Some(status)) => break Some(status),
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(150)),
            Err(e) => {
                wait_error = Some(format!("шаг не дождался завершения: {e}"));
                break None;
            }
        }
    };

    if cancelled {
        kill_running_steps();
    } else if timed_out {
        kill_step(pid);
    } else {
        untrack_step(pid);
    }
    let stdout = join_reader(out_thread);
    let stderr = join_reader(err_thread);
    if cancelled {
        return Err("отменено пользователем".into());
    }
    if timed_out {
        return Err("проба не ответила за 10 минут".into());
    }
    if let Some(error) = wait_error {
        return Err(error);
    }
    let Some(status) = exit else {
        return Err("шаг установки завершился без статуса".into());
    };
    Ok(StepOutput {
        success: status.success(),
        stdout,
        stderr,
    })
}

/// Cancellation check at step boundaries: otherwise, between the end of one step and the start
/// of the next, the flag would not be checked at all (V-56).
fn ensure_not_cancelled() -> Result<(), String> {
    if cancel_flag().load(Ordering::Relaxed) {
        Err("отменено пользователем".into())
    } else {
        Ok(())
    }
}

/// torch version from the installed venv (empty if the interpreter is not ready yet).
fn probe_versions() -> (Option<String>, Option<String>) {
    // The program may have been moved after installation — the venv remembers the old path.
    repair_venv_paths();
    let python = python_exe();
    if !python.exists() {
        return (None, None);
    }
    let mut cmd = Command::new(&python);
    cmd.args([
        "-c",
        "import sys, torch; print(sys.version.split()[0]); print(torch.__version__)",
    ]);
    cmd.env("PYTHONNOUSERSITE", "1");
    hide_console(&mut cmd);
    let Ok(out) = run_probe(cmd) else {
        return (None, None);
    };
    if !out.success {
        return (None, None);
    }
    let text = out.stdout;
    let mut lines = text.lines();
    let python = lines.next().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let torch = lines.next().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    (python, torch)
}

/// Cache of heavy probes: importing torch in the venv and walking the runtime directory take
/// seconds, while the training screen polls status every 2 seconds — without a cache
/// the poll spawned a python process on every tick and did not let presets load.
static PROBE_CACHE: Mutex<Option<(std::time::Instant, (Option<String>, Option<String>), u64)>> =
    Mutex::new(None);
const PROBE_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// One probe at a time: the training screen polls status every 2 seconds, and
/// before this marker every tick spawned a new python while torch was loading.
static PROBE_IN_FLIGHT: Mutex<Option<std::time::Instant>> = Mutex::new(None);
const PROBE_STALE: std::time::Duration = std::time::Duration::from_secs(180);

fn cached_probe() -> ((Option<String>, Option<String>), u64) {
    if let Ok(guard) = PROBE_CACHE.lock() {
        if let Some((at, versions, size_mb)) = guard.as_ref() {
            if at.elapsed() < PROBE_TTL {
                return (versions.clone(), *size_mb);
            }
        }
    }
    {
        let mut in_flight = PROBE_IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        if in_flight.is_some_and(|at| at.elapsed() < PROBE_STALE) {
            // A probe is already running — return the previous snapshot instead of a second process.
            drop(in_flight);
            if let Ok(cache) = PROBE_CACHE.lock() {
                if let Some((_, versions, size_mb)) = cache.as_ref() {
                    return (versions.clone(), *size_mb);
                }
            }
            return ((None, None), 0);
        }
        *in_flight = Some(std::time::Instant::now());
    }
    // A late "Cancel" click after "done" left the flag raised, and all
    // subsequent probes died as "cancelled" until restart. We clear a stale
    // flag before a new probe; during installation it must not be touched.
    let installing = STATE
        .lock()
        .map(|guard| guard.as_ref().is_some_and(|state| state.installing))
        .unwrap_or(false);
    if !installing {
        cancel_flag().store(false, Ordering::Relaxed);
    }
    let versions = probe_versions();
    let size_mb = dir_size_mb(&crate::paths::trainer_dir());
    *PROBE_IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner()) = None;
    if let Ok(mut guard) = PROBE_CACHE.lock() {
        *guard = Some((std::time::Instant::now(), versions.clone(), size_mb));
    }
    (versions, size_mb)
}

pub fn status() -> PyruntimeStatus {
    let state = STATE.lock().ok().and_then(|g| g.clone()).unwrap_or_default();
    let llama_ready = llama_cpp_dir().join("convert_lora_to_gguf.py").exists();
    let ((python, torch), size_mb) = cached_probe();
    PyruntimeStatus {
        installed: python.is_some() && torch.is_some() && llama_ready,
        python,
        torch,
        llama_cpp: llama_ready,
        size_mb,
        installing: state.installing,
        stage: state.stage,
        progress: state.progress,
        error: state.error,
    }
}

fn set_state(f: impl FnOnce(&mut InstallState)) {
    if let Ok(mut guard) = STATE.lock() {
        let state = guard.get_or_insert_with(InstallState::default);
        f(state);
    }
}

fn report(app: &AppHandle, stage: &str, progress: Option<u8>) {
    set_state(|s| {
        s.stage = Some(stage.to_string());
        s.progress = progress;
    });
    let _ = app.emit(
        "trainer-progress",
        serde_json::json!({ "stage": stage, "progress": progress }),
    );
}

/// Interpreter probe without a console window: from a GUI process, every such
/// probe would otherwise flash a cmd window.
fn python_runs_blocking(python: &Path, code: &str) -> bool {
    let mut cmd = Command::new(python);
    cmd.args(["-c", code]);
    hide_console(&mut cmd);
    run_probe(cmd).map(|out| out.success).unwrap_or(false)
}

/// Interpreter probe in `spawn_blocking`: `import torch` takes seconds and must not
/// hold a tokio runtime worker (V-54).
async fn python_runs(python: &Path, code: &str) -> bool {
    let python = python.to_path_buf();
    let code = code.to_string();
    tauri::async_runtime::spawn_blocking(move || python_runs_blocking(&python, &code))
        .await
        .unwrap_or(false)
}

#[cfg(windows)]
fn hide_console(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn hide_console(_cmd: &mut Command) {}

/// uv environment: everything inside the program folder, with no Windows registry writes.
fn uv_env(cmd: &mut Command) {
    let trainer = crate::paths::trainer_dir();
    cmd.env("UV_CACHE_DIR", crate::paths::data_dir().join("uv-cache"))
        .env("UV_PYTHON_INSTALL_DIR", trainer.join("python"))
        .env("UV_PYTHON_NO_REGISTRY", "1")
        .env("UV_NO_PROGRESS", "1")
        .env("PYTHONNOUSERSITE", "1")
        .env("PIP_DISABLE_PIP_VERSION_CHECK", "1")
        .env("PIP_CACHE_DIR", crate::paths::pip_cache_dir())
        .env("HF_HOME", crate::paths::hf_cache_dir())
        .env("TORCH_HOME", crate::paths::torch_cache_dir())
        .env("TMP", crate::paths::tmp_dir())
        .env("TEMP", crate::paths::tmp_dir());
}

fn run_uv_blocking(args: &[String]) -> Result<String, String> {
    let uv = uv_exe();
    if !uv.exists() {
        return Err("uv не установлен".into());
    }
    let mut cmd = Command::new(&uv);
    cmd.args(args);
    uv_env(&mut cmd);
    hide_console(&mut cmd);
    let out = run_cancellable(cmd).map_err(|e| format!("uv {}: {e}", args.join(" ")))?;
    if !out.success {
        let tail: String = out.stderr.lines().rev().take(6).collect::<Vec<_>>().join(" | ");
        return Err(format!("uv {} завершился с ошибкой: {tail}", args.join(" ")));
    }
    Ok(if out.stdout.trim().is_empty() { out.stderr } else { out.stdout })
}

/// uv step in `spawn_blocking` (V-54): multi-gigabyte `uv python install` and
/// `uv pip install torch` would hold a tokio runtime worker for minutes, taking it
/// away from async commands (`trainer_status`, `gpu_usage`, `get_comfy_status`),
/// which the UI polls every two seconds.
async fn run_uv(args: &[&str]) -> Result<String, String> {
    let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    tauri::async_runtime::spawn_blocking(move || run_uv_blocking(&owned))
        .await
        .map_err(|e| format!("шаг установки прерван: {e}"))?
}

/// Download the installation archive with the canonical downloader `paths::download_to_file`.
///
/// Earlier its own copy of the download rule lived here (V-53) without resuming via
/// `Range`, without marking the partial source, and without a "received N of M" check —
/// a truncated download ended up as "zip corrupted" instead of "not fully downloaded". Now
/// the common policy applies: a partial file is a resume point (on cancellation and error
/// it stays on disk), a foreign partial file is discarded by `<file>.total`.
/// Network: connect_timeout 30 s, stream silence — 60 s inside the downloader.
async fn download_to(url: &str, dest: &Path, app: &AppHandle, what: &str) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("не удалось подготовить загрузку {what}: {e}"))?;
    let label = format!("загрузка {what}");
    let progress_app = app.clone();
    let progress_label = label.clone();
    crate::paths::download_to_file(
        &client,
        url,
        dest,
        &label,
        crate::downloads::Flags::with_cancel(cancel_flag()),
        Arc::new(move |got, total, _status| {
            let percent = if total > 0 {
                Some(((got * 100 / total).min(100)) as u8)
            } else {
                None
            };
            report(&progress_app, &progress_label, percent);
        }),
    )
    .await
    .map(|_| ())
}

/// Install the fine-tuning runtime. Idempotent: already completed steps are skipped.
///
/// Entry point: the `trainer_install` command (`lib.rs`) starts it through
/// `tauri::async_runtime::spawn`, i.e. this is a task on a tokio worker; heavy
/// steps are additionally moved into `spawn_blocking` (V-54).
pub async fn install(app: AppHandle) -> Result<(), String> {
    {
        let mut guard = STATE.lock().map_err(|_| "состояние занято".to_string())?;
        let state = guard.get_or_insert_with(InstallState::default);
        if state.installing {
            return Err("установка уже идёт".into());
        }
        *state = InstallState {
            installing: true,
            ..InstallState::default()
        };
    }
    cancel_flag().store(false, Ordering::Relaxed);

    let result = install_inner(&app).await;
    // The cancellation flag was consumed by this run: without clearing it, status probes
    // (`probe_versions`) would die as "cancelled" already after the installation ends.
    cancel_flag().store(false, Ordering::Relaxed);
    // The probe before installation is cached for PROBE_TTL: without clearing it, the screen would
    // show "runtime not installed" for another minute over a ready environment.
    PROBE_CACHE.lock().map(|mut c| *c = None).ok();

    set_state(|s| {
        s.installing = false;
        match &result {
            Ok(()) => {
                s.stage = Some("рантайм дообучения готов".into());
                s.progress = Some(100);
                s.error = None;
            }
            Err(e) => {
                s.stage = None;
                s.error = Some(e.clone());
            }
        }
    });
    // The "Cancel installation" button is still visible until this state update —
    // a flag raised in this window must die together with the installation.
    cancel_flag().store(false, Ordering::Relaxed);
    match &result {
        Ok(()) => {
            let _ = app.emit("trainer-progress", serde_json::json!({ "stage": "готово", "progress": 100 }));
        }
        Err(e) => {
            let _ = app.emit("trainer-progress", serde_json::json!({ "stage": "ошибка", "error": e }));
        }
    }
    result
}

async fn install_inner(app: &AppHandle) -> Result<(), String> {
    let trainer = crate::paths::trainer_dir();
    std::fs::create_dir_all(&trainer).map_err(|e| e.to_string())?;

    if !uv_exe().exists() {
        ensure_not_cancelled()?;
        report(app, "загрузка uv", Some(0));
        let zip = trainer.join("uv.zip");
        let url = UV_ZIP_URL.replace("{VERSION}", UV_VERSION);
        download_to(&url, &zip, app, "uv").await?;
        ensure_not_cancelled()?;
        report(app, "распаковка uv", None);
        let zip_path = zip.clone();
        let dest = trainer.join("uv");
        let unpack =
            tauri::async_runtime::spawn_blocking(move || crate::paths::extract_zip(&zip_path, &dest, 0))
                .await
                .map_err(|e| format!("распаковка uv прервана: {e}"))?;
        if let Err(e) = unpack {
            // The archive is already fully on disk, i.e. not a resume point: a retry
            // must take a fresh file and not break on "zip corrupted" (V-53).
            let _ = std::fs::remove_file(&zip);
            return Err(e);
        }
        let _ = std::fs::remove_file(&zip);
        if !uv_exe().exists() {
            return Err("в архиве uv не нашлось uv.exe".into());
        }
    }

    if !crate::paths::trainer_dir().join("python").exists() {
        ensure_not_cancelled()?;
        report(app, "установка Python", None);
        run_uv(&["python", "install", PYTHON_VERSION]).await?;
    }

    // The venv may have remained from a previous program location: python.exe is
    // in place, but the uv trampoline does not find the interpreter (the path is baked into the exe when
    // the venv is created). We check by launching, not by file presence, and recreate it:
    // otherwise the installer considers the environment ready while the trainer silently does not work.
    let venv = trainer.join("venv");
    let venv_alive = python_exe().exists() && python_runs(&python_exe(), "import sys").await;
    if !venv_alive {
        ensure_not_cancelled()?;
        report(app, "пересоздание окружения", None);
        let _ = std::fs::remove_dir_all(&venv);
        run_uv(&[
            "venv",
            "--python",
            PYTHON_VERSION,
            "--relocatable",
            venv.to_string_lossy().as_ref(),
        ])
        .await?;
    }

    let python = python_exe();
    let python_arg = python.to_string_lossy().to_string();
    let has_torch = python_runs(&python, "import torch").await;

    if !has_torch {
        // sm_120 (RTX 50xx) requires cu128: cu124 wheels do not work on it.
        let index = if is_cuda() { TORCH_INDEX_CUDA } else { TORCH_INDEX_CPU };
        ensure_not_cancelled()?;
        report(app, "установка torch (это несколько гигабайт)", None);
        run_uv(&[
            "pip",
            "install",
            "--python",
            &python_arg,
            TORCH_PACKAGE,
            "--index-url",
            index,
        ])
        .await?;
    }

    // The whole TRAIN_PACKAGES, not part of it: a venv without datasets or truststore
    // was considered ready, and training failed already at dataset preparation.
    let has_stack = python_runs(
        &python,
        "import transformers, accelerate, peft, bitsandbytes, datasets, safetensors, sentencepiece, gguf, google.protobuf, truststore",
    )
    .await;
    if !has_stack {
        ensure_not_cancelled()?;
        report(app, "установка стека обучения", None);
        let mut args = vec!["pip", "install", "--python", python_arg.as_str()];
        args.extend_from_slice(TRAIN_PACKAGES);
        run_uv(&args).await?;
    }

    if !llama_cpp_dir().join("convert_lora_to_gguf.py").exists() {
        ensure_not_cancelled()?;
        report(app, "загрузка llama.cpp", Some(0));
        let zip = trainer.join("llama.cpp.zip");
        let url = LLAMA_CPP_ZIP.replace("{TAG}", LLAMA_CPP_TAG);
        download_to(&url, &zip, app, "llama.cpp").await?;
        ensure_not_cancelled()?;
        report(app, "распаковка llama.cpp", None);
        let zip_path = zip.clone();
        let dest = llama_cpp_dir();
        let unpack =
            tauri::async_runtime::spawn_blocking(move || crate::paths::extract_zip(&zip_path, &dest, 1))
                .await
                .map_err(|e| format!("распаковка llama.cpp прервана: {e}"))?;
        if let Err(e) = unpack {
            let _ = std::fs::remove_file(&zip);
            return Err(e);
        }
        let _ = std::fs::remove_file(&zip);
        if !llama_cpp_dir().join("convert_lora_to_gguf.py").exists() {
            return Err("в архиве llama.cpp нет convert_lora_to_gguf.py".into());
        }
    }

    // The final probe is also outside the worker: importing torch in a live venv takes seconds.
    let (python_version, torch_version) = tauri::async_runtime::spawn_blocking(probe_versions)
        .await
        .map_err(|e| format!("проверка окружения прервана: {e}"))?;
    if python_version.is_none() || torch_version.is_none() {
        return Err("окружение установлено, но python или torch не запускаются".into());
    }
    report(
        app,
        &format!(
            "готово: Python {}, torch {}",
            python_version.unwrap_or_default(),
            torch_version.unwrap_or_default()
        ),
        Some(100),
    );
    Ok(())
}

/// Cancel installation (V-56). Entry point: the `trainer_cancel` command (`lib.rs`),
/// main thread. Earlier the flag was seen only by the download chunk loop, and the
/// `uv python install`, `uv venv`, `uv pip install torch` steps ran to the end even
/// after the click — now every step reads the flag, and live child trees are killed
/// by pid, as in `image_trainer::cancel`.
pub fn cancel() {
    cancel_flag().store(true, Ordering::Relaxed);
    kill_running_steps();
}

fn is_cuda() -> bool {
    matches!(crate::gpu::gpu_info().backend, crate::gpu::GpuBackend::Cuda)
}

/// Environment for running training scripts: caches and temporary files inside the program folder.
pub fn training_env(cmd: &mut Command) {
    cmd.env("PYTHONNOUSERSITE", "1")
        .env("TOKENIZERS_PARALLELISM", "false")
        .env("HF_HOME", crate::paths::hf_cache_dir())
        .env("TORCH_HOME", crate::paths::torch_cache_dir())
        .env("HF_HUB_DISABLE_SYMLINKS_WARNING", "1")
        .env("TMP", crate::paths::tmp_dir())
        .env("TEMP", crate::paths::tmp_dir())
        .env("PYTHONIOENCODING", "utf-8");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shared lock for tests that launch processes: `cancel()` kills all live steps
    /// from `RUNNING_STEPS`, and a parallel process of another test would be its
    /// victim. Synchronization here is not a test luxury but a requirement of the runner's
    /// shared state.
    static TEST_STEPS: Mutex<()> = Mutex::new(());

    #[test]
    fn test_status_shape_without_runtime() {
        let _guard = TEST_STEPS.lock().unwrap_or_else(|e| e.into_inner());
        // On a clean machine there is no runtime: status must be valid and not crash.
        let status = status();
        assert!(!status.installed || status.torch.is_some());
        assert!(status.size_mb < u64::MAX);
    }

    #[test]
    fn test_paths_live_inside_app_folder() {
        assert!(crate::paths::trainer_dir().starts_with(crate::paths::runtimes_root()));
        assert!(crate::paths::training_dir().starts_with(crate::paths::data_dir()));
    }

    /// Program relocation: paths inside `pyvenv.cfg` are rebuilt for the new location,
    /// foreign lines are left untouched.
    #[test]
    fn test_reroot_pyvenv_follows_the_app() {
        let stale = "home = Z:\\Git\\Programs\\Cloufam\\runtime\\trainer\\python\\cpython-3.11-windows-x86_64-none\n\
                     implementation = CPython\n\
                     version_info = 3.11\n\
                     executable = Z:\\Git\\Programs\\Cloufam\\runtime\\trainer\\venv\\Scripts\\python.exe\n";
        let fixed = reroot_pyvenv(stale, std::path::Path::new("Z:\\AImodels\\Cloufam\\runtime\\trainer"));
        assert!(fixed.contains("home = Z:\\AImodels\\Cloufam\\runtime\\trainer\\python\\cpython-3.11-windows-x86_64-none"), "{fixed}");
        assert!(fixed.contains("executable = Z:\\AImodels\\Cloufam\\runtime\\trainer\\venv\\Scripts\\python.exe"), "{fixed}");
        assert!(fixed.contains("implementation = CPython"), "чужие строки не трогаем: {fixed}");

        // An already correct file stays byte for byte: otherwise a write on every poll.
        let fresh = reroot_pyvenv(&fixed, std::path::Path::new("Z:\\AImodels\\Cloufam\\runtime\\trainer"));
        assert_eq!(fresh, fixed);
    }

    /// Unicode in the path must not panic on slicing: to_lowercase() changes the
    /// byte length, and an offset from lower would slice the original string at a
    /// non-existent boundary.
    #[test]
    fn test_reroot_pyvenv_survives_multibyte_path() {
        let stale = "home = Z:\\Юзер\\Cloufam\\runtime\\trainer\\python\\cpython-3.11\n\
                     executable = Z:\\İstanbul\\runtime\\trainer\\venv\\Scripts\\python.exe\n";
        let fixed = reroot_pyvenv(stale, std::path::Path::new("Z:\\AImodels\\Cloufam\\runtime\\trainer"));
        assert!(fixed.contains("home = Z:\\AImodels\\Cloufam\\runtime\\trainer\\python\\cpython-3.11"), "{fixed}");
        assert!(
            fixed.contains("executable = Z:\\AImodels\\Cloufam\\runtime\\trainer\\venv\\Scripts\\python.exe"),
            "{fixed}"
        );
    }

    /// The size walk is depth-limited (V-55): a junction loop or a deep
    /// app relocation via links does not drive recursion to stack overflow.
    #[test]
    fn test_dir_size_stops_at_the_depth_limit() {
        let root = std::env::temp_dir().join(format!("lk-pysize-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("shallow")).unwrap();
        std::fs::write(root.join("shallow").join("near.bin"), vec![0u8; 3 * 1024 * 1024]).unwrap();
        let mut deep = root.clone();
        for _ in 0..DIR_SIZE_MAX_DEPTH {
            deep = deep.join("d");
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("deep.bin"), vec![0u8; 2 * 1024 * 1024]).unwrap();

        let mb = dir_size_mb(&root);
        let _ = std::fs::remove_dir_all(&root);
        // A file exactly at the depth cap is not counted: the top 3 MiB remain.
        assert_eq!(mb, 3);
    }

    /// Cancelling installation kills the live child step by pid (V-56): a process from
    /// the shared runner dies right after `cancel()`, not runs to the end.
    #[cfg(windows)]
    #[test]
    fn test_cancel_kills_the_running_step_by_pid() {
        let _guard = TEST_STEPS.lock().unwrap_or_else(|e| e.into_inner());
        let mut cmd = Command::new("ping");
        cmd.args(["-n", "30", "127.0.0.1"]);
        hide_console(&mut cmd);
        let started = std::time::Instant::now();
        let handle = std::thread::spawn(move || run_cancellable(cmd));
        // Let the step register itself: cancellation must hit a live process.
        std::thread::sleep(std::time::Duration::from_millis(500));
        cancel();
        let result = handle.join().expect("шаг завершился");
        cancel_flag().store(false, Ordering::Relaxed);

        let error = result.expect_err("отменённый шаг обязан завершиться ошибкой");
        assert!(error.contains("отменено"), "{error}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(15),
            "ping на 30 с должен умереть по pid, а не дожидаться конца: {:?}",
            started.elapsed()
        );
        assert!(
            RUNNING_STEPS.lock().map(|g| g.is_empty()).unwrap_or(true),
            "после отмены слот шагов пуст"
        );
    }

    /// A fast step returns its output and is removed from the accounting: the slot is empty, a repeated
    /// cancel does not touch something already finished.
    #[cfg(windows)]
    #[test]
    fn test_run_cancellable_collects_output_and_releases_the_slot() {
        let _guard = TEST_STEPS.lock().unwrap_or_else(|e| e.into_inner());
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "echo cloufam"]);
        hide_console(&mut cmd);
        let out = run_cancellable(cmd).expect("шаг");
        assert!(out.success);
        assert!(out.stdout.contains("cloufam"), "{}", out.stdout);
        assert!(RUNNING_STEPS.lock().map(|g| g.is_empty()).unwrap_or(true));
    }
}

