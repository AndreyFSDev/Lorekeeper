//! Launching text model fine-tuning: presets per GPU tier, a job runner
//! around `scripts/train_text_lora.py`, parsing of `@@LK@@` markers, cancellation.
//!
//! Presets rigidly tie the Hugging Face base and the Ollama tag: the adapter
//! is trained on HF weights and applied to the same weights from the registry, otherwise
//! the LoRA has no effect.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Instant;

use tauri::AppHandle;

use crate::gpu::HardwareTier;

#[derive(Debug, Clone, serde::Serialize)]
pub struct TrainPreset {
    pub id: String,
    pub name: String,
    pub description: String,
    pub base_model: String,
    pub ollama_tag: String,
    pub params_b: f32,
    /// Roughly how much disk the bf16 base takes (GB).
    pub download_gb: f32,
    pub min_tier: HardwareTier,
    pub recommended: bool,
}

fn preset(
    id: &str,
    base_model: &str,
    name: &str,
    description: &str,
    params_b: f32,
    min_tier: HardwareTier,
    recommended: bool,
) -> TrainPreset {
    TrainPreset {
        id: id.to_string(),
        name: name.to_string(),
        description: description.to_string(),
        base_model: base_model.to_string(),
        ollama_tag: format!("huihui_ai/qwen3-abliterated:{id}"),
        params_b,
        download_gb: params_b * 2.05,
        min_tier,
        recommended,
    }
}

/// Presets: the minimum tier is the one where the model really trains in 4 bits.
/// The "HF base ↔ Ollama tag" pairs are verified by requests to the registry and Hugging Face.
pub fn presets() -> Vec<TrainPreset> {
    vec![
        preset(
            "0.6b-v2",
            "huihui-ai/Huihui-Qwen3-0.6B-abliterated-v2",
            "Qwen 3 Abliterated 0.6B",
            "Пробная модель: обучение занимает минуты, удобно проверить датасет и весь путь целиком.",
            0.6,
            HardwareTier::Low,
            false,
        ),
        preset(
            "1.7b-v2",
            "huihui-ai/Huihui-Qwen3-1.7B-abliterated-v2",
            "Qwen 3 Abliterated 1.7B",
            "Лёгкая модель под 8 ГБ: быстро учится, годится для стиля и коротких реплик.",
            1.7,
            HardwareTier::Low,
            false,
        ),
        preset(
            "4b-v2",
            "huihui-ai/Huihui-Qwen3-4B-abliterated-v2",
            "Qwen 3 Abliterated 4B",
            "Оптимум для 8 ГБ: заметно умнее 1.7B, обучение идёт в 4 бита с запасом по памяти.",
            4.0,
            HardwareTier::Low,
            true,
        ),
        preset(
            "8b-v2",
            "huihui-ai/Huihui-Qwen3-8B-abliterated-v2",
            "Qwen 3 Abliterated 8B",
            "Основная ролевая модель: обучается от 12 ГБ VRAM, на 8 ГБ не помещается.",
            8.0,
            HardwareTier::Mid,
            true,
        ),
        preset(
            "14b-v2",
            "huihui-ai/Huihui-Qwen3-14B-abliterated-v2",
            "Qwen 3 Abliterated 14B",
            "Флагман: нужен тир 16 ГБ и больше времени, зато лучший язык и удержание характера.",
            14.0,
            HardwareTier::High,
            true,
        ),
    ]
}

pub fn preset_by_id(id: &str) -> Option<TrainPreset> {
    presets().into_iter().find(|p| p.id == id)
}

/// Training profile per tier: example length, batch, accumulation, LoRA rank.
/// On 8 GB the length is capped at 512 tokens — otherwise the activations leave no headroom.
pub fn tier_profile(tier: HardwareTier) -> Option<(u32, u32, u32, u32)> {
    match tier {
        HardwareTier::Minimal => None,
        HardwareTier::Low => Some((512, 1, 8, 16)),
        HardwareTier::Mid => Some((1024, 1, 8, 16)),
        HardwareTier::High => Some((2048, 1, 8, 16)),
    }
}

/// Example length for a run: the requested one, but not above the tier ceiling. One
/// function for both start and estimate — otherwise the estimate counted a different length.
pub fn training_seq(tier: HardwareTier, seq_override: Option<u32>) -> Option<u32> {
    let (tier_seq, _, _, _) = tier_profile(tier)?;
    Some(seq_override.unwrap_or(tier_seq).min(tier_seq))
}

/// Time-per-step estimate. Calibration from a live run in the app
/// (0.6B, seq 512, RTX 5080: 4.0 s/step at batch 1 and accumulation 8) plus
/// a linear count over parameters and length; a correction for the lower tiers.
/// The run itself shows the exact time — this is only "roughly how much".
pub fn estimate_seconds_per_step(tier: HardwareTier, params_b: f32, seq: u32) -> f64 {
    const STEP_OVERHEAD: f64 = 1.5;
    const SECONDS_PER_PARAM_TOKEN: f64 = 8.3e-12;
    let compute = SECONDS_PER_PARAM_TOKEN * (params_b as f64 * 1e9) * (seq as f64);
    let card_factor = match tier {
        HardwareTier::High => 1.0,
        HardwareTier::Mid => 1.5,
        HardwareTier::Low => 2.6,
        HardwareTier::Minimal => 6.0,
    };
    ((STEP_OVERHEAD + compute) * card_factor).max(0.2)
}

/// Fine-tuning availability on this machine: CUDA is required — on AMD (ROCm for Polaris
/// does not exist) and on the CPU there is nothing to train 4-bit LoRA with.
pub fn availability() -> Result<(), String> {
    let gpu = crate::gpu::gpu_info();
    if !matches!(gpu.backend, crate::gpu::GpuBackend::Cuda) {
        return Err("Дообучение доступно только на видеокартах NVIDIA (CUDA). На AMD и на процессоре его нет: 4-битное обучение требует CUDA.".into());
    }
    if tier_profile(gpu.tier).is_none() {
        return Err("Недостаточно видеопамяти для обучения: нужен тир Low (8 ГБ и больше).".into());
    }
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TrainingState {
    pub running: bool,
    pub run_id: Option<String>,
    pub preset_id: Option<String>,
    pub dataset: Option<String>,
    pub stage: String,
    pub step: u32,
    pub total: u32,
    pub loss: Option<f32>,
    pub epoch: Option<f32>,
    pub eta_seconds: Option<u64>,
    pub elapsed_seconds: u64,
    pub adapter_dir: Option<String>,
    pub log_tail: Vec<String>,
    pub error: Option<String>,
    pub finished: bool,
}

impl Default for TrainingState {
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
            epoch: None,
            eta_seconds: None,
            elapsed_seconds: 0,
            adapter_dir: None,
            log_tail: Vec::new(),
            error: None,
            finished: false,
        }
    }
}

struct Job {
    state: Arc<Mutex<TrainingState>>,
    child: Arc<Mutex<Option<Child>>>,
    /// pid of the training process. `0` — the preparation phase: the slot is already taken (A3-01),
    /// but the process is not spawned yet, nothing to kill — cancel() in this phase only
    /// raises the cancel flag (A3-02).
    pid: u32,
    cancel: Arc<AtomicBool>,
    started: std::time::Instant,
}

static JOB: Mutex<Option<Job>> = Mutex::new(None);

/// Shared lock for starting GPU jobs (text, images, WD14 captioning): the check
/// "no foreign job is running → take my slot" ran on different locks,
/// and between the check and the reservation both starts passed it at the same time.
pub(crate) static START_LOCK: Mutex<()> = Mutex::new(());

/// State of the current job. A poisoned lock is no reason to hand out a default
/// past a real run (A3-12): `into_inner` keeps the lock working.
pub fn status() -> TrainingState {
    let guard = JOB.lock().unwrap_or_else(|e| e.into_inner());
    let Some(job) = guard.as_ref() else {
        return TrainingState::default();
    };
    let mut state = job.state.lock().unwrap_or_else(|e| e.into_inner()).clone();
    state.elapsed_seconds = job.started.elapsed().as_secs();
    state
}

#[cfg(windows)]
fn hide_console(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn hide_console(_cmd: &mut Command) {}

/// Unique run identifier. `run-<unix-sec>` gave the same folder
/// to two starts within one second: configs, log and adapters of two runs
/// mixed in one directory (A3-01). Seconds + nanoseconds + counter
/// are unique within a process and sortable in time.
fn run_id_now() -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("run-{}-{:09}-{}", now.as_secs(), now.subsec_nanos(), n)
}

/// The training script is embedded in the binary and laid out to `data/training` next to
/// the runs: a built app has no project sources, while the script path
/// must always be the same one.
///
/// The write is tmp + rename (A3-01): a truncating write over the file was read
/// by a running python mid-read. After a successful layout the path is kept in
/// `OnceLock`: repeated starts do not re-read or rewrite the file that
/// another process is already working on.
fn script_path() -> Result<PathBuf, String> {
    static DEPLOYED: OnceLock<PathBuf> = OnceLock::new();
    if let Some(path) = DEPLOYED.get() {
        if path.is_file() {
            return Ok(path.clone());
        }
    }
    const SCRIPT: &str = include_str!("../scripts/train_text_lora.py");
    let dir = crate::paths::training_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join("train_text_lora.py");
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    if current != SCRIPT {
        let tmp = dir.join("train_text_lora.py.tmp");
        std::fs::write(&tmp, SCRIPT).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    }
    let _ = DEPLOYED.set(path.clone());
    Ok(path)
}

/// The number of examples and the mode (chat/text) of a dataset by its path from `dataset::resolve`.
/// The mode is the file's authoritative meta line (A3-05): the former `(0, "chat")` fallback
/// was fail-open and silently routed a text dataset into the chat template.
fn dataset_shape(dataset_path: &Path) -> Result<(usize, String), String> {
    crate::dataset::shape_of(dataset_path).map(|shape| (shape.examples, shape.mode))
}

/// How many recent runs we keep on disk; the rest are cleaned up together with
/// all checkpoints inside (A3-09).
pub(crate) const KEEP_RUNS: usize = 5;

/// Run directories to delete: the newest `keep` stay (A3-09).
/// Extracted as a pure function for a deterministic test — the mtime of directories created
/// in a row on NTFS is indistinguishable, while here the order is set explicitly.
fn prune_candidates(
    mut entries: Vec<(std::time::SystemTime, PathBuf)>,
    keep: usize,
) -> Vec<PathBuf> {
    entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    entries.into_iter().skip(keep).map(|(_, path)| path).collect()
}

/// Delete old runs (`run-*` directories with checkpoints inside).
fn prune_old_runs(keep: usize) {
    prune_runs_in(&crate::paths::training_runs_dir(), keep);
}

/// The same for any runs directory (images keep theirs in `image_runs`).
pub(crate) fn prune_runs_in(root: &std::path::Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let listed: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let modified = entry.metadata().and_then(|m| m.modified()).ok()?;
            Some((modified, entry.path()))
        })
        .collect();
    for path in prune_candidates(listed, keep) {
        let _ = std::fs::remove_dir_all(&path);
    }
}

/// Re-pipe the child process stream line by line into the channel. stdout and stderr
/// are read simultaneously: tqdm writes the progress bar to stderr every step, and while the stream
/// waited for stdout to end, an overflowing stderr pipe stopped python forever.
fn pump_lines(stream: impl Read + Send + 'static, tx: mpsc::Sender<String>) {
    std::thread::spawn(move || {
        // Bytes — with replacements: a non-UTF-8 line must not break reading,
        // otherwise the pipe stops draining and the process stalls again.
        for raw in BufReader::new(stream).split(b'\n').map_while(Result::ok) {
            let line = String::from_utf8_lossy(&raw);
            let _ = tx.send(line.trim_end_matches('\r').to_string());
        }
    });
}

/// Parsing of a `@@LK@@ {json}` marker from the embedded script; the reader stream only calls it.
///
/// `cancelled` — after a cancel request the status belongs to the cancel path (A3-02):
/// late markers of the dying process do not overwrite "cancelling…" back to
/// "training"; the numeric fields keep updating meanwhile.
fn apply_marker(s: &mut TrainingState, value: &serde_json::Value, started: Instant, cancelled: bool) {
    // The script's fail() prints the reason as a marker — without it the UI is left with
    // only "exited with code 1".
    if let Some(error) = value["error"].as_str() {
        if !cancelled {
            s.error = Some(error.to_string());
            s.stage = "ошибка".into();
        }
        return;
    }
    if let Some(stage) = value["stage"].as_str() {
        if !cancelled {
            s.stage = match stage {
                "loading" => "загрузка модели".into(),
                "dataset" => "подготовка датасета".into(),
                "model" => "подготовка LoRA".into(),
                "training" => "обучение".into(),
                "saving" => "сохранение адаптера".into(),
                "cancelled" => "отменено".into(),
                "done" => "готово".into(),
                other => other.to_string(),
            };
        }
    }
    if let Some(step) = value["step"].as_u64() {
        s.step = step as u32;
        if !cancelled {
            s.stage = "обучение".into();
            if let Some(eta) = eta_after_step(started, step, s.total) {
                s.eta_seconds = Some(eta);
            }
        }
    }
    if let Some(total) = value["total"].as_u64() {
        s.total = total as u32;
    }
    // Non-finite values (NaN/inf) do not get into the state: serializing them
    // into an event/command reply breaks the send, and the UI freezes (A3-03).
    if let Some(loss) = value["loss"].as_f64().filter(|v| v.is_finite()) {
        s.loss = Some(loss as f32);
    }
    if let Some(epoch) = value["epoch"].as_f64().filter(|v| v.is_finite()) {
        s.epoch = Some(epoch as f32);
    }
    if value["stage"].as_str() == Some("done") && !cancelled {
        s.finished = true;
    }
}

/// Log tail in the state: no more than 40 lines.
fn push_log(s: &mut TrainingState, line: &str) {
    let tail = &mut s.log_tail;
    if tail.len() >= 40 {
        tail.remove(0);
    }
    tail.push(line.to_string());
}

/// A line of script output into the state: a `@@LK@@ {json}` marker or a log line.
///
/// An invalid marker is not dropped silently (A3-03): after a parse failure
/// (e.g. `NaN` in loss yields non-JSON) the UI would otherwise freeze without a single
/// trace. The reason and a snippet of the marker settle in log_tail.
fn feed_line(s: &mut TrainingState, line: &str, started: Instant, cancelled: bool) {
    if let Some(payload) = line.strip_prefix("@@LK@@ ") {
        match serde_json::from_str::<serde_json::Value>(payload) {
            Ok(value) => apply_marker(s, &value, started, cancelled),
            Err(e) => {
                let short: String = payload.chars().take(160).collect();
                push_log(s, &format!("маркер скрипта не разобран ({e}): {short}"));
            }
        }
        return;
    }
    if line.trim().is_empty() {
        return;
    }
    push_log(s, line.trim());
}

/// Reserve the job slot under the first lock (A3-01): a placeholder with `running=true`
/// and its own cancel flag sits in `JOB` already during preparation, and a second start
/// is refused while preparation is running — the "checked → spawned" window used to
/// let a second python through, whose orphan would hold VRAM.
fn reserve_job(
    state: Arc<Mutex<TrainingState>>,
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

/// Release the slot reservation — only if it still holds our job.
fn release_slot(state: &Arc<Mutex<TrainingState>>) {
    let mut guard = JOB.lock().unwrap_or_else(|e| e.into_inner());
    if guard.as_ref().is_some_and(|job| Arc::ptr_eq(&job.state, state)) {
        *guard = None;
    }
}

/// Boundary of the preparation steps: a cancel before spawn must abort the work (A3-02),
/// otherwise the cancel button did nothing at all while python was not running yet.
fn ensure_not_cancelled(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err("отменено пользователем".into())
    } else {
        Ok(())
    }
}

/// A prepared run: everything needed to spawn python.
struct Prepared {
    run_id: String,
    run_dir: PathBuf,
    config_path: PathBuf,
    script: PathBuf,
    log_path: PathBuf,
}

/// Preparing a run under the slot reservation: checks, run directory, config and
/// script layout. Every step starts with a cancel check (A3-02).
fn prepare_run(
    state: &Arc<Mutex<TrainingState>>,
    cancel: &AtomicBool,
    preset_id: &str,
    dataset_name: &str,
    epochs: f32,
    seq_override: Option<u32>,
) -> Result<Prepared, String> {
    ensure_not_cancelled(cancel)?;
    let preset = preset_by_id(preset_id).ok_or_else(|| format!("неизвестный пресет: {preset_id}"))?;
    let gpu = crate::gpu::gpu_info();
    if gpu.tier < preset.min_tier {
        return Err(format!(
            "«{}» требует тир {:?}: на этой видеокарте не хватит видеопамяти.",
            preset.name, preset.min_tier
        ));
    }
    let dataset_path = crate::dataset::resolve(dataset_name)?;
    let (_, batch, grad_accum, lora_r) =
        tier_profile(gpu.tier).ok_or_else(|| "недостаточно видеопамяти для обучения".to_string())?;
    let seq = training_seq(gpu.tier, seq_override).unwrap_or_default();

    // Fail closed (A3-05): the mode comes from the dataset meta line; a mixed or
    // empty dataset is refused rather than silently becoming "chat" with lost records.
    let (examples, mode) = dataset_shape(&dataset_path)?;
    let steps_per_epoch = (examples as f32 / (batch * grad_accum) as f32).ceil().max(1.0);
    let total_steps = (steps_per_epoch * epochs).ceil() as u32;

    ensure_not_cancelled(cancel)?;
    let run_id = run_id_now();
    let run_dir = crate::paths::training_runs_dir().join(&run_id);
    std::fs::create_dir_all(&run_dir).map_err(|e| e.to_string())?;
    // Our directory was just created — the newest one; the fresh KEEP_RUNS stay,
    // the rest of the runs with all checkpoints inside go away (A3-09).
    prune_old_runs(KEEP_RUNS);
    let adapter_dir = run_dir.join("adapter");

    let config = serde_json::json!({
        "base_model": preset.base_model,
        "dataset": dataset_path.to_string_lossy(),
        "output_dir": adapter_dir.to_string_lossy(),
        "mode": mode,
        "max_seq_length": seq,
        "batch_size": batch,
        "grad_accum": grad_accum,
        "epochs": epochs,
        "learning_rate": 0.0002,
        "lora_r": lora_r,
        "lora_alpha": lora_r * 2,
        "save_steps": 1000,
        "seed": 42,
        "env": {},
    });
    let config_path = run_dir.join("train-config.json");
    std::fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap())
        .map_err(|e| e.to_string())?;

    ensure_not_cancelled(cancel)?;
    let script = script_path()?;
    {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        s.run_id = Some(run_id.clone());
        s.preset_id = Some(preset_id.to_string());
        s.dataset = Some(dataset_path.to_string_lossy().to_string());
        s.total = total_steps;
        s.adapter_dir = Some(adapter_dir.to_string_lossy().to_string());
        s.stage = "подготовка завершена".into();
    }
    Ok(Prepared {
        run_id,
        log_path: run_dir.join("train.log"),
        run_dir,
        config_path,
        script,
    })
}

/// Start training. Returns the run identifier.
///
/// Entry point: the `training_start` command (`lib.rs`), `spawn_blocking`.
/// The job slot is reserved before preparation and held until spawn (A3-01);
/// `training-progress` events are removed as dead (A3-11) — the UI
/// polls `status()`.
pub fn start(
    _app: AppHandle,
    preset_id: &str,
    dataset_name: &str,
    epochs: f32,
    seq_override: Option<u32>,
) -> Result<String, String> {
    availability()?;
    if !crate::pyruntime::status().installed {
        return Err("Рантайм дообучения не установлен: сначала нажмите «Установить рантайм».".into());
    }
    // The foreign-job check and our slot reservation — under one lock:
    // otherwise an image start managed to take the slot between them.
    let start_guard = crate::trainer::START_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Two trainers on one GPU are guaranteed to hit CUDA OOM.
    if crate::image_trainer::is_running() {
        return Err("Идёт обучение картинок — дождитесь окончания.".into());
    }

    let state = Arc::new(Mutex::new(TrainingState {
        running: true,
        preset_id: Some(preset_id.to_string()),
        stage: "подготовка".into(),
        ..TrainingState::default()
    }));
    let cancel = Arc::new(AtomicBool::new(false));
    let child_handle: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
    reserve_job(state.clone(), child_handle.clone(), cancel.clone())?;
    drop(start_guard);

    let prepared = match prepare_run(&state, &cancel, preset_id, dataset_name, epochs, seq_override)
    {
        Ok(prepared) => prepared,
        Err(e) => {
            release_slot(&state);
            return Err(e);
        }
    };
    let Prepared {
        run_id,
        run_dir,
        config_path,
        script,
        log_path,
    } = prepared;

    let mut cmd = Command::new(crate::pyruntime::python_exe());
    cmd.arg(&script)
        .arg("--config")
        .arg(&config_path)
        .current_dir(&run_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::pyruntime::training_env(&mut cmd);
    hide_console(&mut cmd);
    // Spawn and pid publication — in one JOB capture (A3-01): cancel() either sees the
    // pid and kills the tree, or (before spawn) raises the flag that is checked
    // right here. Neither ordering leaves an orphan.
    let (stdout, stderr) = {
        let mut guard = JOB.lock().unwrap_or_else(|e| e.into_inner());
        if cancel.load(Ordering::Relaxed) {
            if guard.as_ref().is_some_and(|job| Arc::ptr_eq(&job.state, &state)) {
                *guard = None;
            }
            return Err("отменено пользователем".into());
        }
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                if guard.as_ref().is_some_and(|job| Arc::ptr_eq(&job.state, &state)) {
                    *guard = None;
                }
                return Err(format!("не удалось запустить обучение: {e}"));
            }
        };
        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        if let Some(job) = guard.as_mut() {
            if Arc::ptr_eq(&job.state, &state) {
                job.pid = pid;
                *job.child.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
                let mut s = job.state.lock().unwrap_or_else(|e| e.into_inner());
                s.stage = "запуск".into();
            }
        }
        (stdout, stderr)
    };

    // Reading the output: `@@LK@@ {json}` markers are progress, other lines are the log.
    let reader_state = state.clone();
    let reader_cancel = cancel.clone();
    let started = std::time::Instant::now();
    std::thread::spawn(move || {
        use std::io::Write;
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .ok();
        let mut handle_line = |line: &str| {
            if let Some(log) = log.as_mut() {
                let _ = writeln!(log, "{line}");
            }
            let cancelled = reader_cancel.load(Ordering::Relaxed);
            let mut s = reader_state.lock().unwrap_or_else(|e| e.into_inner());
            feed_line(&mut s, line, started, cancelled);
        };

        let (tx, rx) = mpsc::channel::<String>();
        if let Some(out) = stdout {
            pump_lines(out, tx.clone());
        }
        if let Some(err) = stderr {
            pump_lines(err, tx.clone());
        }
        drop(tx);
        // The channel closes when both pipes reach EOF.
        for line in rx {
            handle_line(&line);
        }

        // Take the child out before wait(): cancel() takes the same lock and must not
        // hang while the process exit is being awaited. `waited` lives until the
        // end of the thread, that is, longer than running == true: the check in
        // cancel() relies on that.
        let mut waited = child_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let code = waited
            .as_mut()
            .and_then(|c| c.wait().ok())
            .and_then(|status| status.code());
        let mut s = reader_state.lock().unwrap_or_else(|e| e.into_inner());
        // The cancel flag is read under the state lock (A3-02): cancel() writes
        // "cancelling…" under the same lock, and reading before it raced with its
        // write — the final write overwrote the cancellation.
        let cancelled = reader_cancel.load(Ordering::Relaxed);
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
    });

    Ok(run_id)
}

/// Cancel the current training together with the process tree.
///
/// Entry point: the `training_cancel` command (`lib.rs`). Works in the preparation
/// phase too (A3-02): `pid == 0` — the process is not spawned yet, nothing to kill,
/// raising the flag that preparation checks at its boundaries is enough.
pub fn cancel() -> Result<(), String> {
    let guard = JOB.lock().unwrap_or_else(|e| e.into_inner());
    let Some(job) = guard.as_ref() else {
        return Err("обучение не запущено".into());
    };
    // Hold the state lock to the end: the wait thread clears running before
    // it drops the Child, and while the handle is open the OS will not give the pid to another
    // process. A finished run is not touched — its pid already belongs to someone else (an app
    // exit calls cancel always).
    let mut state = job.state.lock().unwrap_or_else(|e| e.into_inner());
    if !state.running {
        return Ok(());
    }
    job.cancel.store(true, Ordering::Relaxed);
    if job.pid != 0 {
        // Kill the tree by the saved pid: the child may already have been taken out of the slot
        // by the wait thread, and handle.kill() cannot reach it.
        #[cfg(windows)]
        {
            let mut cmd = Command::new("taskkill");
            cmd.args(["/PID", &job.pid.to_string(), "/T", "/F"]);
            hide_console(&mut cmd);
            let _ = cmd.status();
        }
        let mut child_guard = job.child.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(child) = child_guard.as_mut() {
            let _ = child.kill();
        }
    }
    state.stage = "отмена…".into();
    Ok(())
}

/// Forget a finished run so the UI returns to its initial state.
/// Also cleans old runs with checkpoints (A3-09).
pub fn clear() {
    let mut guard = JOB.lock().unwrap_or_else(|e| e.into_inner());
    let finished = guard
        .as_ref()
        .map(|job| !job.state.lock().unwrap_or_else(|e| e.into_inner()).running)
        .unwrap_or(false);
    if finished {
        *guard = None;
    }
    drop(guard);
    prune_old_runs(KEEP_RUNS);
}

/// ETA after step steps. step == 0 divides by zero (inf in the UI) —
/// count only after the first step, as in image_trainer.
fn eta_after_step(started: std::time::Instant, step: u64, total: u32) -> Option<u64> {
    if step == 0 {
        return None;
    }
    let speed = started.elapsed().as_secs_f64() / step as f64;
    let left = total.saturating_sub(step as u32) as f64;
    Some((left * speed) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shared lock of tests touching the global `JOB`: start race, cancel and
    /// slot cleanup share one state — in parallel they would flap.
    static TEST_JOB: Mutex<()> = Mutex::new(());

    #[test]
    fn test_presets_have_matching_base_and_tag() {
        for p in presets() {
            assert!(p.base_model.starts_with("huihui-ai/Huihui-Qwen3-"));
            assert!(p.base_model.ends_with("-abliterated-v2"));
            assert!(p.ollama_tag.starts_with("huihui_ai/qwen3-abliterated:"));
            // The size in the tag and in the repository name must match: the adapter
            // trains on HF weights and is applied to the same weights in Ollama.
            let size_from_tag = p.ollama_tag.rsplit(':').next().unwrap().trim_end_matches("-v2");
            let size_from_base = p
                .base_model
                .split('-')
                .find(|part| part.ends_with('B') || part.ends_with('b'))
                .unwrap()
                .to_lowercase();
            assert_eq!(size_from_tag, size_from_base, "пресет {}", p.id);
        }
    }

    #[test]
    fn test_tier_profiles() {
        assert!(tier_profile(HardwareTier::Minimal).is_none());
        assert_eq!(tier_profile(HardwareTier::Low).unwrap().0, 512);
        assert_eq!(tier_profile(HardwareTier::Mid).unwrap().0, 1024);
        assert_eq!(tier_profile(HardwareTier::High).unwrap().0, 2048);
    }

    #[test]
    fn test_8gb_card_gets_4b_presets_only() {
        let gpu =
            crate::gpu::build_vendor_info(crate::gpu::GpuVendor::Nvidia, "RTX 3060".into(), 8192);
        let available: Vec<TrainPreset> = presets()
            .into_iter()
            .filter(|p| p.min_tier <= gpu.tier)
            .collect();
        assert!(available.iter().any(|p| p.id == "4b-v2"));
        assert!(!available.iter().any(|p| p.id == "8b-v2"));
        assert!(!available.iter().any(|p| p.id == "14b-v2"));
    }

    #[test]
    fn test_estimates_grow_with_size_and_sequence() {
        let small = estimate_seconds_per_step(HardwareTier::Low, 0.6, 512);
        let big = estimate_seconds_per_step(HardwareTier::Low, 4.0, 512);
        let longer = estimate_seconds_per_step(HardwareTier::Low, 4.0, 1024);
        assert!(big > small);
        assert!(longer > big);
        let on_16gb = estimate_seconds_per_step(HardwareTier::High, 4.0, 512);
        assert!(on_16gb < big, "на старшей карте шаг должен быть быстрее");
    }

    #[test]
    fn test_eta_skips_divide_by_zero_on_first_step() {
        let started = std::time::Instant::now();
        assert_eq!(eta_after_step(started, 0, 100), None, "step==0 → без ETA, не inf");
        assert!(eta_after_step(started, 1, 100).is_some());
    }

    #[test]
    fn test_training_seq_clamps_to_tier() {
        assert_eq!(training_seq(HardwareTier::High, Some(4096)), Some(2048));
        assert_eq!(training_seq(HardwareTier::Low, None), Some(512));
        assert_eq!(training_seq(HardwareTier::Mid, Some(256)), Some(256));
        assert_eq!(training_seq(HardwareTier::Minimal, Some(512)), None);
    }

    /// Regression 22.09: the text dataset was found by a path string with \\?\ and
    /// went to python in chat mode. The form — in one pass over the file (A3-04),
    /// the mode fail closed (A3-05).
    #[test]
    fn test_dataset_shape_finds_resolved_text_dataset() {
        let dir = crate::paths::training_import_dir();
        std::fs::create_dir_all(&dir).unwrap();
        // Name with pid: the same test in a parallel cargo test process will not
        // delete the file from under this one.
        let name = format!("trainer-shape-probe-{}.jsonl", std::process::id());
        let file = dir.join(&name);
        std::fs::write(&file, "{\"text\":\"первый\"}\n{\"text\":\"второй\"}\n").unwrap();
        let shape = crate::dataset::resolve(&name).and_then(|path| dataset_shape(&path));
        let _ = std::fs::remove_file(&file);
        assert_eq!(shape.unwrap(), (2, "text".to_string()));
    }

    #[test]
    fn test_marker_error_and_stages() {
        let started = Instant::now();
        let mut state = TrainingState::default();
        apply_marker(&mut state, &serde_json::json!({"stage": "model", "trainable": 1}), started, false);
        assert_eq!(state.stage, "подготовка LoRA");
        apply_marker(&mut state, &serde_json::json!({"step": 3, "total": 10, "loss": 1.5}), started, false);
        assert_eq!((state.step, state.total, state.stage.as_str()), (3, 10, "обучение"));
        apply_marker(
            &mut state,
            &serde_json::json!({"error": "ошибка подготовки датасета: нет колонки"}),
            started,
            false,
        );
        assert_eq!(state.stage, "ошибка");
        assert_eq!(state.error.as_deref(), Some("ошибка подготовки датасета: нет колонки"));
        apply_marker(&mut state, &serde_json::json!({"stage": "cancelled"}), started, false);
        assert_eq!(state.stage, "отменено");
    }

    /// Late markers of the dying process do not overwrite "cancelling…" (A3-02):
    /// after a cancel request the status belongs to the cancel path, while the numbers
    /// keep updating.
    #[test]
    fn test_marker_does_not_overwrite_cancel_stage() {
        let started = Instant::now();
        let mut state = TrainingState {
            stage: "отмена…".into(),
            ..TrainingState::default()
        };
        apply_marker(&mut state, &serde_json::json!({"stage": "training"}), started, true);
        apply_marker(&mut state, &serde_json::json!({"step": 5, "total": 10}), started, true);
        assert_eq!(state.stage, "отмена…", "маркер не должен перетирать отмену");
        assert_eq!((state.step, state.total), (5, 10), "числа обновляются");
        apply_marker(&mut state, &serde_json::json!({"error": "поздняя ошибка"}), started, true);
        assert_eq!(state.stage, "отмена…");
        assert!(state.error.is_none());
    }

    /// An invalid marker is not dropped silently (A3-03): the reason and a snippet of the
    /// payload settle in log_tail, otherwise the UI freezes without traces.
    #[test]
    fn test_feed_line_logs_invalid_marker() {
        let started = Instant::now();
        let mut state = TrainingState::default();
        feed_line(&mut state, "@@LK@@ {\"loss\": NaN, \"step\": 1}", started, false);
        assert!(state.log_tail.iter().any(|l| l.contains("маркер скрипта не разобран")), "{:?}", state.log_tail);
        // A valid marker still works as before, ordinary lines go to the log.
        feed_line(&mut state, "@@LK@@ {\"step\": 2}", started, false);
        feed_line(&mut state, "обычная строка лога", started, false);
        assert_eq!(state.step, 2);
        assert!(state.log_tail.iter().any(|l| l == "обычная строка лога"));
    }

    /// A non-finite loss does not get into the state (A3-03): serializing it
    /// breaks sending the state to the UI.
    #[test]
    fn test_marker_drops_non_finite_numbers() {
        let started = Instant::now();
        let mut state = TrainingState::default();
        let value = serde_json::json!({"loss": f64::INFINITY, "epoch": 0.5});
        apply_marker(&mut state, &value, started, false);
        assert_eq!(state.loss, None, "inf/NaN обязан отбрасываться, а не портить состояние");
        assert_eq!(state.epoch, Some(0.5));
    }

    /// The run id is unique within the process (A3-01): `run-<unix-sec>` gave one
    /// folder to two starts within a second.
    #[test]
    fn test_run_ids_are_unique() {
        let first = run_id_now();
        let second = run_id_now();
        assert_ne!(first, second);
        assert!(first.starts_with("run-"), "{first}");
    }

    /// Start race (A3-01): the slot is reserved under the first lock, and exactly
    /// one of the parallel starts gets it.
    #[test]
    fn test_slot_reservation_rejects_double_start() {
        let _guard = TEST_JOB.lock().unwrap_or_else(|e| e.into_inner());
        let results: Vec<Result<(), String>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let state = Arc::new(Mutex::new(TrainingState {
                        running: true,
                        stage: "подготовка".into(),
                        ..TrainingState::default()
                    }));
                    let child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
                    let cancel = Arc::new(AtomicBool::new(false));
                    scope.spawn(move || reserve_job(state, child, cancel))
                })
                .collect();
            handles.into_iter().map(|h| h.join().expect("поток резерва")).collect()
        });
        let wins = results.iter().filter(|r| r.is_ok()).count();
        // Clear the slot so as not to affect other tests.
        let mut guard = JOB.lock().unwrap_or_else(|e| e.into_inner());
        *guard = None;
        drop(guard);
        assert_eq!(wins, 1, "слот обязан достаться ровно одному старту: {results:?}");
    }

    /// Cancel in the preparation phase (A3-02): pid is still 0 — nothing to kill, but the flag
    /// is set, and cancel() must not fail or invoke taskkill.
    #[test]
    fn test_cancel_in_preparation_phase_only_raises_the_flag() {
        let _guard = TEST_JOB.lock().unwrap_or_else(|e| e.into_inner());
        let state = Arc::new(Mutex::new(TrainingState {
            running: true,
            stage: "подготовка".into(),
            ..TrainingState::default()
        }));
        let cancel_signal = Arc::new(AtomicBool::new(false));
        reserve_job(state.clone(), Arc::new(Mutex::new(None)), cancel_signal.clone()).unwrap();
        let result = cancel();
        let flag = cancel_signal.load(Ordering::Relaxed);
        let stage = state.lock().unwrap_or_else(|e| e.into_inner()).stage.clone();
        let mut guard = JOB.lock().unwrap_or_else(|e| e.into_inner());
        *guard = None;
        drop(guard);
        assert!(result.is_ok());
        assert!(flag, "флаг отмены обязан подняться");
        assert_eq!(stage, "отмена…");
    }

    /// Pure selection of directories to delete (A3-09): the fresh `keep` stay,
    /// the old ones go with all checkpoints inside.
    #[test]
    fn test_prune_candidates_keep_the_newest() {
        let t = |secs: u64| std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        let entries = vec![
            (t(1), PathBuf::from("run-old")),
            (t(3), PathBuf::from("run-new")),
            (t(2), PathBuf::from("run-mid")),
        ];
        let removed = prune_candidates(entries, 2);
        assert_eq!(removed, vec![PathBuf::from("run-old")]);
    }

    /// The process writes more than the pipe buffer (64 KB) to stderr, and closes stdout
    /// only on exit: reading stdout to the end first and then stderr, it stalled.
    #[cfg(windows)]
    #[test]
    fn test_pump_drains_stderr_while_stdout_is_open() {
        let mut child = Command::new("cmd")
            .args(["/C", "for /L %i in (1,1,3000) do @echo stderr-line-%i-padding-padding-padding 1>&2"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("cmd");
        let (tx, rx) = mpsc::channel();
        pump_lines(child.stdout.take().unwrap(), tx.clone());
        pump_lines(child.stderr.take().unwrap(), tx);
        let mut count = 0;
        loop {
            match rx.recv_timeout(std::time::Duration::from_secs(30)) {
                Ok(line) if line.starts_with("stderr-line-") => count += 1,
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let _ = child.kill();
                    panic!("пайп stderr не опустошается");
                }
            }
        }
        let _ = child.wait();
        assert_eq!(count, 3000);
    }

    /// cancel() on a finished run must not hit taskkill on its pid:
    /// the OS may have given it to another process. A live run — is killed.
    #[cfg(windows)]
    #[test]
    fn test_cancel_kills_only_running_job() {
        let _guard = TEST_JOB.lock().unwrap_or_else(|e| e.into_inner());
        let spawn_bystander = || {
            Command::new("ping")
                .args(["-n", "30", "127.0.0.1"])
                .stdout(Stdio::null())
                .spawn()
                .expect("ping")
        };
        let install_job = |pid: u32, running: bool| {
            let state = Arc::new(Mutex::new(TrainingState {
                running,
                stage: "готово".into(),
                ..TrainingState::default()
            }));
            *JOB.lock().unwrap() = Some(Job {
                state: state.clone(),
                child: Arc::new(Mutex::new(None)),
                pid,
                cancel: Arc::new(AtomicBool::new(false)),
                started: Instant::now(),
            });
            state
        };

        let mut bystander = spawn_bystander();
        let finished = install_job(bystander.id(), false);
        let result = cancel();
        let survived = bystander.try_wait().unwrap().is_none();
        let _ = bystander.kill();
        let _ = bystander.wait();

        let mut target = spawn_bystander();
        let running = install_job(target.id(), true);
        let running_result = cancel();
        let killed = target.wait().map(|status| !status.success()).unwrap_or(false);
        *JOB.lock().unwrap() = None;

        assert!(result.is_ok());
        assert!(survived, "taskkill ушёл по pid завершённого прогона");
        assert_eq!(finished.lock().unwrap().stage, "готово");
        assert!(running_result.is_ok());
        assert!(killed, "живой прогон должен быть убит");
        assert_eq!(running.lock().unwrap().stage, "отмена…");
    }
}
