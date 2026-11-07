use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use tokio::time::{sleep, Duration};

use crate::{paths, settings::Settings};

/// Portable Ollama version that we download into the installation folder.
/// The `ollama-windows-amd64.zip` bundle includes CUDA libraries. The installed
/// version lives in a marker next to the exe — an update on top of it rides on that marker.
use crate::l10n;

pub const OLLAMA_VERSION: &str = "v0.40.3";
const OLLAMA_ZIP_URL: &str =
    "https://github.com/ollama/ollama/releases/download/{VERSION}/ollama-windows-amd64.zip";

/// Asset name of the runtime archive inside an Ollama release.
const OLLAMA_ZIP_NAME: &str = "ollama-windows-amd64.zip";

/// SHA-256 of the `OLLAMA_VERSION` archive as published with the release: the
/// `digest` GitHub reports for the asset (api.github.com, fetched 2026-10-17;
/// the release also carries a `sha256sum.txt` asset). Pinned here so the first
/// install is verified without any extra network lookup (V-24).
const OLLAMA_ZIP_SHA256: &str =
    "e5ecff02dfc7cae8ddf6d6b7a7ac5f727ffeedf4491f44ae75ff5b0820dbbbd5";

/// Per-release checksum list — the authoritative source for archives this code
/// downloads from that release (V-24).
const OLLAMA_SHA256SUM_URL: &str =
    "https://github.com/ollama/ollama/releases/download/{VERSION}/sha256sum.txt";

/// How many times the release checksum list is fetched before giving up (the
/// same fixed backoff as `registry::download_blob`).
const CHECKSUM_ATTEMPTS: u32 = 4;

/// Runtime version marker on disk — a regular file next to `ollama.exe`, not a
/// field in `Settings`: it describes what is really in the folder and doesn't get confused
/// by settings export/reset (the same scheme as `.lk-st-version` for ST).
fn ollama_version_marker_path() -> PathBuf {
    paths::runtime_dir().join(".lk-ollama-version")
}

fn write_ollama_version_marker(version: &str) -> Result<(), String> {
    std::fs::write(ollama_version_marker_path(), version).map_err(|e| e.to_string())
}

/// Version of the installed portable runtime: the marker, or for installs before
/// markers — the binary itself (`ollama.exe --version`). Empty string — the runtime
/// is missing or the binary doesn't answer: the updater fixes such an install forcibly.
pub async fn installed_ollama_version() -> String {
    let exe = paths::runtime_dir().join("ollama.exe");
    // A marker without a binary is a broken install (closing the app during
    // unpacking, antivirus quarantine). An empty version makes startup
    // re-download the runtime, otherwise `start` hit an unreachable wizard.
    if let Ok(text) = std::fs::read_to_string(ollama_version_marker_path()) {
        let version = text.trim().to_string();
        if !version.is_empty() && exe.exists() {
            return version;
        }
    }
    if !exe.exists() {
        return String::new();
    }
    // Starting a process blocks the thread, and this is called from async startup: move it
    // to the blocking task pool so as not to hold the executor.
    let output = tokio::task::spawn_blocking(move || {
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("--version");
        // Without CREATE_NO_WINDOW, the console ollama.exe from a GUI process opens its own window
        hide_console(&mut cmd);
        cmd.output()
    })
    .await;
    match output {
        Ok(Ok(out)) => parse_ollama_version_output(&format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )),
        _ => String::new(),
    }
}

/// The string `ollama --version` → "vX.Y.Z". Ollama prints it in two forms:
/// "ollama version is 0.40.3" (serve available) and "Warning: client version is
/// 0.40.3" (serve not responding) — we catch the common tail "version is ".
fn parse_ollama_version_output(text: &str) -> String {
    for line in text.lines() {
        if let Some(rest) = line.split("version is ").nth(1) {
            let version = rest.trim();
            if !version.is_empty() {
                return format!("v{version}");
            }
        }
    }
    String::new()
}

fn parse_version(v: &str) -> Option<Vec<u64>> {
    v.trim().trim_start_matches('v').split('.').map(|part| part.parse().ok()).collect()
}

/// Whether to update the runtime on top of the installed one. Upward only: rolling the app
/// back to a release with an older pin must not re-download Ollama
/// "downward". An unreadable/empty version counts as outdated — the update also fixes
/// a broken install.
pub fn ollama_needs_update(installed_version: &str) -> bool {
    match (parse_version(installed_version), parse_version(OLLAMA_VERSION)) {
        (Some(mut installed), Some(mut pinned)) => {
            let len = installed.len().max(pinned.len());
            installed.resize(len, 0);
            pinned.resize(len, 0);
            installed < pinned
        }
        _ => installed_version.trim() != OLLAMA_VERSION,
    }
}

/// KV cache type of the portable serve. q8_0 is half the size of f16 at the same prefill
/// (measurement 25.09.2026 on RTX 5080: KV 14B 1280 → 680 MiB at 8192, generation
/// 2–5% slower, answers on fixed prompts of the same quality) —
/// the freed memory goes to context (`models::context_budget`).
/// CUDA only: quantized V requires flash attention, and its support on
/// Vulkan (AMD/Intel) is impossible to test. Ollama enables flash attention
/// itself (`--flash-attn auto`), a separate OLLAMA_FLASH_ATTENTION is not needed.
pub fn kv_cache_type() -> Option<&'static str> {
    (crate::gpu::gpu_info().backend == crate::gpu::GpuBackend::Cuda).then_some("q8_0")
}

/// Readiness probes go only to 127.0.0.1, where a live server accepts
/// a connection instantly, while Windows rejects a closed port only after ~2 s
/// of SYN retries — exactly what every probe of a not-yet-started Ollama used to cost.
const PROBE_CONNECT_TIMEOUT: Duration = Duration::from_millis(200);

pub struct Runtime {
    pub client: crate::ollama_client::OllamaClient,
    /// Mutex — so stop() can be called on &self from the exit handler
    /// without taking Runtime out of tokio::Mutex.
    child: Option<std::sync::Mutex<Child>>,
}

/// PID of the just-spawned `ollama serve` before Runtime got into
/// `AppState`. Closing the window during `wait_ready` (up to 60 s) saw `None`
/// in state and left serve orphaned — reproduced 21.10.2026.
static STARTING_PID: AtomicU32 = AtomicU32::new(0);
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

/// Application exit: shut down the serve we started and don't allow spawning a new one.
pub fn begin_shutdown() {
    SHUTTING_DOWN.store(true, Ordering::SeqCst);
    let pid = STARTING_PID.swap(0, Ordering::SeqCst);
    if pid != 0 {
        #[cfg(windows)]
        kill_process_tree(pid);
        #[cfg(not(windows))]
        {
            // We don't build the tree on non-Windows: the root is enough.
            let _ = pid;
        }
    }
}

/// Runtime saved in `AppState` — then the regular `stop()` shuts it down.
pub fn clear_starting_pid() {
    STARTING_PID.store(0, Ordering::SeqCst);
}

/// Remember the pid right after spawn and refuse if the app is already
/// exiting (otherwise there is a window between spawn and writing to state).
fn adopt_started(child: &mut Child) -> Result<(), String> {
    let pid = child.id();
    STARTING_PID.store(pid, Ordering::SeqCst);
    if SHUTTING_DOWN.load(Ordering::SeqCst) {
        let _ = STARTING_PID.swap(0, Ordering::SeqCst);
        #[cfg(windows)]
        kill_process_tree(pid);
        let _ = child.kill();
        let _ = child.wait();
        return Err("приложение завершается".into());
    }
    Ok(())
}

impl Runtime {
    /// Shut down `ollama serve` — an orphaned runtime would hold models in VRAM
    /// and interfere with the next launch.
    pub fn stop(&self) {
        let Some(child) = self.child.as_ref() else {
            // A reused foreign serve: we have no children, nothing to shut down —
            // otherwise the one who started it would be left without a runtime.
            return;
        };
        let mut child = child.lock().unwrap_or_else(|e| e.into_inner());
        // The tree is collected before kill: after TerminateProcess the "parent →
        // child" link is lost, and the runner with llama-server would keep holding VRAM.
        #[cfg(windows)]
        kill_process_tree(child.id());
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Kill a process together with its whole subtree, bottom-up.
/// TerminateProcess on `ollama.exe` does NOT kill its children: runner and llama-server
/// keep hanging around holding VRAM, and the next launch would fail with OOM.
/// We hit strictly our own tree: a broad sweep over the runtime folder killed the runtime
/// of a neighboring app instance, and it waited for the dead serve all 60 seconds.
#[cfg(windows)]
fn kill_process_tree(root: u32) {
    use std::collections::HashMap;
    use sysinfo::{Pid, System};

    let mut sys = System::new();
    sys.refresh_processes();

    let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for (pid, proc) in sys.processes() {
        if let Some(parent) = proc.parent() {
            // A process older than the "parent" got it through a reused PID and
            // doesn't belong to our tree. A dead parent is not visible in the list —
            // we keep such a link: the runner of a crashed serve is ours anyway.
            let genuine = sys
                .process(parent)
                .is_none_or(|p| parent_is_older(p.start_time(), proc.start_time()));
            if genuine {
                children.entry(parent).or_default().push(*pid);
            }
        }
    }

    // Leaves first: a killed parent would stop holding the children, and they
    // would manage to pick up work (llama-server keeps the model in memory).
    for pid in process_tree(&children, Pid::from_u32(root)).into_iter().rev() {
        if let Some(proc) = sys.process(pid) {
            proc.kill();
        }
    }
}

/// Process subtree from the root, "top-down" order.
/// PIDs on Windows are reused, so we don't visit already-seen ones:
/// a cyclic parent↔child link would loop the traversal.
#[cfg(windows)]
fn process_tree(
    children: &std::collections::HashMap<sysinfo::Pid, Vec<sysinfo::Pid>>,
    root: sysinfo::Pid,
) -> Vec<sysinfo::Pid> {
    let mut tree = Vec::new();
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        if tree.contains(&pid) {
            continue;
        }
        tree.push(pid);
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids.iter().copied());
        }
    }
    tree
}

/// The "parent → child" link by PID is valid only if the parent is older:
/// Windows gives a dead process's PID to a new one, and it would look like the parent
/// of foreign processes. Zero time — failed to read it, consider the link valid.
#[cfg(windows)]
fn parent_is_older(parent_start: u64, child_start: u64) -> bool {
    parent_start == 0 || child_start == 0 || parent_start <= child_start
}

/// The path lies inside the `root` directory, not in a neighboring directory with the same
/// string prefix (`runtime-backup` must not match `runtime`):
/// after the root there must be a path separator. The comparison is string-based —
/// callers normalize case and strip `\\?\` before calling.
fn exe_inside_root(exe: &str, root: &str) -> bool {
    match exe.strip_prefix(root) {
        Some(rest) => rest.is_empty() || rest.starts_with('\\') || rest.starts_with('/'),
        None => false,
    }
}

/// Runtime processes left without a live parent: the app was killed hard
/// (Task Manager, antivirus, crash), and `ollama serve` outlived its app.
/// We don't touch live trees — a neighboring instance has its own runtime.
#[cfg(windows)]
fn kill_orphan_runtime_processes() {
    use sysinfo::System;

    let Ok(runtime_root) = crate::paths::runtime_dir().canonicalize() else {
        return;
    };
    let root_str = runtime_root.to_string_lossy();
    let clean_root = root_str.strip_prefix(r"\\?\").unwrap_or(&root_str).to_lowercase();

    let mut sys = System::new();
    for _ in 0..4 {
        sys.refresh_processes();
        let mut killed = false;
        for (_, proc) in sys.processes() {
            // A parent with a reused PID is younger than the orphan: it doesn't hold its
            // tree, and VRAM would remain occupied.
            let has_live_parent = proc
                .parent()
                .and_then(|p| sys.process(p))
                .is_some_and(|parent| parent_is_older(parent.start_time(), proc.start_time()));
            if has_live_parent {
                continue;
            }
            if let Some(exe) = proc.exe() {
                let exe_str = exe.to_string_lossy().to_lowercase();
                let clean_exe = exe_str.strip_prefix(r"\\?\").unwrap_or(&exe_str);
                if exe_inside_root(clean_exe, &clean_root) && proc.kill() {
                    killed = true;
                }
            }
        }
        if !killed {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Whether the port is listened to by our `ollama serve`: a process from the runtime folder with this
/// OLLAMA_HOST and the same model store. sysinfo doesn't expose the port owner,
/// so port and store are matched by the process environment — we set it ourselves.
fn own_serve_on(port: u16) -> Option<u32> {
    use sysinfo::{ProcessRefreshKind, System, UpdateKind};

    let Ok(runtime_root) = paths::runtime_dir().canonicalize() else {
        return None;
    };
    let root = comparable_path(&runtime_root);
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessRefreshKind::new().with_exe(UpdateKind::OnlyIfNotSet));
    let candidates: Vec<sysinfo::Pid> = sys
        .processes()
        .iter()
        .filter(|(_, proc)| {
            proc.exe()
                .is_some_and(|exe| exe_inside_root(&comparable_path(exe), &root))
        })
        .map(|(pid, _)| *pid)
        .collect();
    let store = paths::models_dir();
    candidates.into_iter().find(|&pid| {
        sys.refresh_process_specifics(pid, ProcessRefreshKind::new().with_environ(UpdateKind::Always));
        sys.process(pid)
            .is_some_and(|proc| serves_store(proc.environ(), port, &store))
    })
    .map(|pid| pid.as_u32())
}

/// Our serve's environment: the same port in OLLAMA_HOST and the same store in
/// OLLAMA_MODELS (see `spawn_portable`). The previous store is also foreign: models
/// downloaded into the current one are not seen by this serve.
fn serves_store(environ: &[String], port: u16, store: &std::path::Path) -> bool {
    let var = |name: &str| {
        environ.iter().find_map(|entry| {
            let (key, value) = entry.split_once('=')?;
            key.eq_ignore_ascii_case(name).then_some(value)
        })
    };
    let trim = |path: String| path.trim_end_matches(['\\', '/']).to_string();
    let host_ok = var("OLLAMA_HOST").is_some_and(|host| host.trim() == format!("127.0.0.1:{port}"));
    let store_ok = var("OLLAMA_MODELS").is_some_and(|dir| {
        trim(comparable_path(std::path::Path::new(dir.trim()))) == trim(comparable_path(store))
    });
    host_ok && store_ok
}

/// Path for comparison with `proc.exe()`: canonicalize on Windows adds `\\?\`,
/// while sysinfo returns a regular Win32 path (gotcha 05.09.2026).
fn comparable_path(path: &std::path::Path) -> String {
    let text = path.to_string_lossy().to_lowercase();
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_string()
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Runtime {
    /// Start Ollama and return a ready client. Order:
    /// dev mode (use_system_ollama) → wait for readiness on 11434 (or spawn
    /// `ollama` from PATH); otherwise — the portable exe from the installation folder with
    /// OLLAMA_HOST and OLLAMA_MODELS, increment the port if busy.
    pub async fn start(settings: &mut Settings) -> Result<Self, String> {
        if settings.ollama_port > u16::MAX - 20 {
            settings.ollama_port = 11434;
        }
        #[cfg(debug_assertions)]
        if settings.use_system_ollama {
            let base = "http://127.0.0.1:11434".to_string();
            // One quick probe: a full wait_ready before spawn would give a minute
            // of dead waiting on every cold start.
            if Self::probe(&base).await {
                return Ok(Self {
                    client: crate::ollama_client::OllamaClient::new(base)?,
                    child: None,
                });
            }
            // System Ollama is installed but not running — start it from PATH.
            if let Some(mut child) = Self::spawn_system() {
                if let Err(error) = adopt_started(&mut child) {
                    return Err(error);
                }
                if let Err(error) = Self::wait_ready(&base, &mut child).await {
                    clear_starting_pid();
                    // TerminateProcess doesn't remove children: runner/llama-server would hold VRAM
                    let pid = child.id();
                    let _ = tokio::task::spawn_blocking(move || kill_process_tree(pid)).await;
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
                return Ok(Self {
                    client: crate::ollama_client::OllamaClient::new(base)?,
                    child: Some(std::sync::Mutex::new(child)),
                });
            }
            return Err("Ollama не найден в системе и не запущен".into());
        }

        let exe = paths::runtime_dir().join("ollama.exe");
        if !exe.exists() {
            return Err("портативный Ollama не установлен — пройдите мастер настройки".into());
        }
        // After the last forced termination, orphaned llama-server processes could
        // hold VRAM — clean up before spawn, otherwise serve will fail with OOM. The sweep
        // scans all processes (sysinfo) and sleeps between passes: we give blocking
        // work to the pool so as not to occupy an async worker (V-63).
        #[cfg(windows)]
        if let Err(error) = tokio::task::spawn_blocking(kill_orphan_runtime_processes).await {
            crate::boot_log(&format!("очистка сирот рантайма не удалась: {error}"));
        }

        for attempt in 0..20 {
            let Some(port) = settings.ollama_port.checked_add(attempt as u16) else {
                break;
            };
            let base = format!("http://127.0.0.1:{port}");
            if Self::port_free(port).await {
                let mut child = Self::spawn_portable(&exe, port, &paths::models_dir())?;
                if let Err(error) = adopt_started(&mut child) {
                    return Err(error);
                }
                if let Err(error) = Self::wait_ready(&base, &mut child).await {
                    clear_starting_pid();
                    // TerminateProcess doesn't remove children: runner/llama-server would hold VRAM
                    let pid = child.id();
                    let _ = tokio::task::spawn_blocking(move || kill_process_tree(pid)).await;
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
                // The caller keeps the port on top of fresh settings: during
                // startup the user manages to change something on the screen, and
                // writing a whole snapshot from here would overwrite their edits.
                settings.ollama_port = port;
                return Ok(Self {
                    client: crate::ollama_client::OllamaClient::new(base)?,
                    child: Some(std::sync::Mutex::new(child)),
                });
            }
            // Port busy. Accept only our own serve with the same store: a foreign
            // Ollama (the Ollama for Windows service on 11434) reads its own
            // OLLAMA_MODELS, and it wouldn't see what was downloaded into our store.
            // The process sweep with environment reading is blocking work, not for a tokio worker (V-63)
            let Some(own_pid) = tokio::task::spawn_blocking(move || own_serve_on(port)).await.ok().flatten() else {
                continue;
            };
            // Ours — probe briefly (three probes of 2 s): a full wait_ready would hold
            // startup for 60 seconds on each port if serve hangs.
            let mut answered = false;
            for _ in 0..3 {
                if Self::probe(&base).await {
                    answered = true;
                    break;
                }
            }
            if answered {
                settings.ollama_port = port;
                return Ok(Self {
                    client: crate::ollama_client::OllamaClient::new(base)?,
                    child: None,
                });
            }
            // Our serve is silent. It cannot be left alive: the next port will raise
            // a second serve on the same store, and the first will keep holding VRAM.
            crate::boot_log(&format!("Ollama на порту {port} не отвечает — завершаю pid {own_pid}"));
            let _ = tokio::task::spawn_blocking(move || kill_process_tree(own_pid)).await;
        }
        Err("не удалось найти свободный порт для Ollama".into())
    }

    async fn port_free(port: u16) -> bool {
        tokio::net::TcpListener::bind(("127.0.0.1", port)).await.is_ok()
    }

    /// One request with a short timeout: is base answering right now.
    /// Also used externally: it decides whether an already started runtime
    /// can be reused instead of recreated.
    pub async fn probe(base: &str) -> bool {
        // base is always loopback Ollama (127.0.0.1): bypass the system proxy
        // (otherwise the user's machine proxy answers 503 instead of the local
        // server).
        let Ok(client) = reqwest::Client::builder()
            .connect_timeout(PROBE_CONNECT_TIMEOUT)
            .timeout(std::time::Duration::from_secs(2))
            .no_proxy()
            .build()
        else {
            return false;
        };
        match client.get(format!("{base}/api/tags")).send().await {
            Ok(resp) => resp.status().is_success(),
            Err(_) => false,
        }
    }

    async fn wait_ready(base: &str, child: &mut Child) -> Result<(), String> {
        // A hung serve holds its TCP connection forever: every request must
        // have its own timeout, otherwise polling never completes.
        // base is always loopback Ollama (127.0.0.1): bypass the system proxy
        // (otherwise the user's machine proxy answers 503 instead of the local
        // server).
        let client = reqwest::Client::builder()
            .connect_timeout(PROBE_CONNECT_TIMEOUT)
            .timeout(std::time::Duration::from_secs(2))
            .no_proxy()
            .build()
            .map_err(|e| e.to_string())?;
        // Wall-clock deadline: 120 iterations with a 2 s request timeout stretched to 5 minutes
        // with a silent serve, while the error text promised 60 seconds
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while std::time::Instant::now() < deadline {
            if let Ok(resp) = client.get(format!("{base}/api/tags")).send().await {
                if resp.status().is_success() {
                    return Ok(());
                }
            }
            // A crashed serve (antivirus, busy port, corrupted weights) is pointless
            // to wait a minute for: the reason is already in its log, show it immediately.
            if let Ok(Some(status)) = child.try_wait() {
                return Err(format!("Ollama завершилась с кодом {status}.{}", serve_log_hint()));
            }
            sleep(Duration::from_millis(500)).await;
        }
        Err(format!(
            "Ollama не ответил за 60 секунд на {base}.{}",
            serve_log_hint()
        ))
    }

    fn spawn_portable(
        exe: &std::path::Path,
        port: u16,
        models_dir: &std::path::Path,
    ) -> Result<Child, String> {
        let ollama_home = crate::paths::data_dir().join("ollama");
        let _ = std::fs::create_dir_all(&ollama_home);
        let mut cmd = Command::new(exe);
        cmd.arg("serve")
            .env("OLLAMA_HOST", format!("127.0.0.1:{port}"))
            .env("OLLAMA_MODELS", models_dir)
            .env("HOME", &ollama_home)
            .env("USERPROFILE", &ollama_home)
            // Without this, serve deletes `*-partial` and layers without a
            // manifest on every start — a download pause didn't survive an app restart.
            // `cleanup_stale_partials` removes unfinished data older than a week.
            .env("OLLAMA_NOPRUNE", "1");
        // Serve output goes to the log: the reason for failure (port, driver, antivirus
        // blocking) is visible only there, the process has no console window.
        match serve_log() {
            Some(log) => {
                let err = log.try_clone().ok();
                cmd.stdout(Stdio::from(log));
                match err {
                    Some(err) => cmd.stderr(Stdio::from(err)),
                    None => cmd.stderr(Stdio::null()),
                };
            }
            None => {
                cmd.stdout(Stdio::null()).stderr(Stdio::null());
            }
        }
        // AMD and Intel: they have no CUDA, and ROCm is not included — request
        // the Vulkan backend explicitly, otherwise the card silently stays idle.
        if matches!(
            crate::gpu::gpu_info().vendor,
            crate::gpu::GpuVendor::Amd | crate::gpu::GpuVendor::Intel
        ) {
            cmd.env("OLLAMA_VULKAN", "1");
        }
        if let Some(kv_type) = kv_cache_type() {
            cmd.env("OLLAMA_KV_CACHE_TYPE", kv_type);
        }
        hide_console(&mut cmd);
        cmd.spawn()
            .map_err(|e| format!("не удалось запустить ollama serve: {e}"))
    }

    /// Start the system ollama from PATH (a dev convenience). Error — None,
    /// the caller deals with the message itself.
    #[cfg(debug_assertions)]
    fn spawn_system() -> Option<Child> {
        let mut cmd = Command::new("ollama");
        cmd.arg("serve").stdout(Stdio::null()).stderr(Stdio::null());
        hide_console(&mut cmd);
        if let Ok(child) = cmd.spawn() {
            return Some(child);
        }
        // The app process PATH may not contain ollama — try the explicit
        // standard installation path.
        let exe = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|p| p.join("Programs").join("Ollama").join("ollama.exe"))?;
        let mut cmd = Command::new(exe);
        cmd.arg("serve").stdout(Stdio::null()).stderr(Stdio::null());
        hide_console(&mut cmd);
        cmd.spawn().ok()
    }
}

/// Log of `ollama serve` in the data folder. Rewritten on every startup:
/// we always analyze the last launch, and the file must not grow without limit.
fn serve_log_path() -> PathBuf {
    crate::paths::data_dir().join("ollama-serve.log")
}

fn serve_log() -> Option<std::fs::File> {
    std::fs::File::create(serve_log_path()).ok()
}

/// The tail of the failure message: where to look for details.
fn serve_log_hint() -> String {
    format!(" Подробности — {}", serve_log_path().display())
}

/// A console app (ollama.exe) gets a console window from Windows when spawned,
/// even if output is redirected to null. The user closes the empty
/// window — and kills the runtime with it. CREATE_NO_WINDOW removes the window.
#[cfg(windows)]
fn hide_console(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_console(_cmd: &mut Command) {}

/// Runtime download progress: (received, total, status).
pub type ProgressFn = Arc<dyn Fn(u64, u64, String) + Send + Sync>;

/// Expected SHA-256 of the runtime archive for `version` (V-24). For the
/// pinned release it is compiled in; other versions are checked against the
/// `sha256sum.txt` the release publishes. `None` — no checksum is available
/// for that version at all, and the install proceeds like it did before this
/// check existed (logged to boot.log by the caller).
async fn expected_zip_sha256(client: &reqwest::Client, version: &str) -> Option<String> {
    if version == OLLAMA_VERSION {
        return Some(OLLAMA_ZIP_SHA256.to_string());
    }
    fetch_release_sha256(client, version).await
}

/// `<sha256sum.txt>` of the release, parsed for our archive. Fetch faults
/// retry with the project's fixed backoff; a release without the file (404) or
/// without an entry yields `None` — there is simply no source to trust.
async fn fetch_release_sha256(client: &reqwest::Client, version: &str) -> Option<String> {
    let url = OLLAMA_SHA256SUM_URL.replace("{VERSION}", version);
    for attempt in 1..=CHECKSUM_ATTEMPTS {
        match client.get(&url).send().await {
            Ok(resp) if resp.status().as_u16() == 404 => return None,
            Ok(resp) if resp.status().is_success() => {
                return match resp.text().await {
                    Ok(text) => sha256sum_entry(&text, OLLAMA_ZIP_NAME),
                    Err(_) => None,
                };
            }
            _ => {}
        }
        if attempt < CHECKSUM_ATTEMPTS {
            sleep(Duration::from_secs(2)).await;
        }
    }
    None
}

/// One entry of a GNU `sha256sum` file: `<hex>  <file>` or `<hex> *<file>`.
fn sha256sum_entry(text: &str, file_name: &str) -> Option<String> {
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(hash), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        if name.trim_start_matches('*') == file_name
            && hash.len() == 64
            && hash.chars().all(|c| c.is_ascii_hexdigit())
        {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

/// Verify the downloaded archive against the release checksum before it is
/// extracted anywhere near the program folder (V-24). A mismatch deletes the
/// zip — it must not be extracted nor resumed from — and fails the install;
/// an unknown checksum is a logged gap, not a brick.
async fn verify_runtime_zip(
    client: &reqwest::Client,
    zip_path: &std::path::Path,
    version: &str,
    label: &str,
) -> Result<(), String> {
    let Some(expected) = expected_zip_sha256(client, version).await else {
        crate::boot_log(&format!(
            "{label}: у релиза {version} нет читаемой контрольной суммы — сверка пропущена"
        ));
        return Ok(());
    };
    let actual = sha256_of_file(zip_path.to_path_buf()).await?;
    if !actual.eq_ignore_ascii_case(&expected) {
        let _ = std::fs::remove_file(zip_path);
        let _ = std::fs::remove_file(paths::download_total_path(zip_path));
        return Err(format!(
            "{label}: контрольная сумма архива не совпала ({actual} вместо {expected}) — файл удалён, повторите загрузку"
        ));
    }
    Ok(())
}

/// SHA-256 of a file. The archive is gigabytes and hashing is blocking IO:
/// it runs in the blocking pool so the async worker is not stalled (V-63).
async fn sha256_of_file(path: PathBuf) -> Result<String, String> {
    tokio::task::spawn_blocking(move || -> Result<String, String> {
        use sha2::{Digest, Sha256};
        use std::io::Read;

        let mut file = std::fs::File::open(&path).map_err(|e| format!("архив не открыт: {e}"))?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let read = file
                .read(&mut buf)
                .map_err(|e| format!("архив не прочитан: {e}"))?;
            if read == 0 {
                break;
            }
            hasher.update(&buf[..read]);
        }
        Ok(format!("{:x}", hasher.finalize()))
    })
    .await
    .map_err(|e| format!("проверка контрольной суммы не запустилась: {e}"))?
}

/// Download portable Ollama into `runtime/ollama/` of the installation folder.
/// Idempotent: if the exe is already there — just Ok.
pub async fn download_runtime(
    cancel: Arc<AtomicBool>,
    on_progress: ProgressFn,
) -> Result<PathBuf, String> {
    let dir = paths::runtime_dir();
    let exe = dir.join("ollama.exe");
    if exe.exists() {
        // The presence of the exe doesn't mean unpacking finished: an interrupted
        // unzip left a broken binary, and the wizard got stuck forever on "Ollama
        // exited with code…". We recognize a working runtime by version.
        let installed = installed_ollama_version().await;
        if !installed.is_empty() {
            let _ = write_ollama_version_marker(&installed);
            return Ok(exe);
        }
        crate::boot_log("Рантайм Ollama: exe есть, но не запускается — перекачиваю");
    }

    let url = OLLAMA_ZIP_URL.replace("{VERSION}", OLLAMA_VERSION);
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;

    let runtime_root = paths::base_dir().join("runtime");
    std::fs::create_dir_all(&runtime_root).map_err(|e| e.to_string())?;
    let zip_path = runtime_root.join("ollama.zip");

    // Resume: the partially downloaded archive stays on disk, so a connection drop or
    // app exit doesn't start the 1.4 GB download over.
    let (have, known_total) = paths::download_partial(&zip_path);
    if have > 0 {
        on_progress(have, known_total, l10n::t("загрузка рантайма… (продолжаю)", "downloading the runtime… (resuming)"));
    } else {
        on_progress(0, 0, l10n::t("загрузка рантайма…", "downloading the runtime…"));
    }

    let flags = crate::downloads::Flags::with_cancel(cancel);
    paths::download_to_file(&client, &url, &zip_path, &l10n::t("загрузка рантайма", "downloading the runtime"), flags, on_progress.clone())
        .await?;
    verify_runtime_zip(&client, &zip_path, OLLAMA_VERSION, "загрузка рантайма").await?;

    let total = std::fs::metadata(&zip_path).map(|m| m.len()).unwrap_or(0);
    on_progress(total, total, "распаковка рантайма…".into());
    // Unzipping ~1.5 GB is blocking IO — the blocking pool, not the async
    // worker (V-63).
    tokio::task::spawn_blocking({
        let zip_path = zip_path.clone();
        let dir = dir.clone();
        move || paths::extract_zip(&zip_path, &dir, 0)
    })
    .await
    .map_err(|e| format!("распаковка рантайма не запустилась: {e}"))??;
    let _ = std::fs::remove_file(&zip_path);

    if exe.exists() {
        let _ = write_ollama_version_marker(OLLAMA_VERSION);
        on_progress(total, total, "рантайм готов".into());
        Ok(exe)
    } else {
        Err("в архиве не нашлось ollama.exe".into())
    }
}

/// Update portable Ollama on top of the installed one (flow like the ST update):
/// download the needed zip version with resume, shut down runtime orphans — otherwise a running
/// `ollama serve` holds the exe and unpacking fails with a sharing violation —
/// and unpack over `runtime/ollama`. Models in the store are not touched:
/// they are not here. Progress — (received, total, status).
pub async fn update_runtime_to(
    version: &str,
    cancel: Arc<AtomicBool>,
    on_progress: ProgressFn,
) -> Result<(), String> {
    let dir = paths::runtime_dir();
    let exe = dir.join("ollama.exe");
    let url = OLLAMA_ZIP_URL.replace("{VERSION}", version);
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;

    let runtime_root = paths::base_dir().join("runtime");
    std::fs::create_dir_all(&runtime_root).map_err(|e| e.to_string())?;
    let zip_path = runtime_root.join("ollama.zip");

    // Resume — the same as on first install: a connection drop doesn't start
    // the 1.5 GB download over, and download_to_file rejects a fragment of someone else's archive
    // by the source metadata.
    let (have, known_total) = paths::download_partial(&zip_path);
    if have > 0 {
        on_progress(have, known_total, "обновление Ollama… (продолжаю)".into());
    } else {
        on_progress(0, 0, "обновление Ollama…".into());
    }

    let flags = crate::downloads::Flags::with_cancel(cancel);
    paths::download_to_file(&client, &url, &zip_path, "обновление Ollama", flags, on_progress.clone())
        .await?;
    verify_runtime_zip(&client, &zip_path, version, "обновление Ollama").await?;

    on_progress(0, 0, "обновление Ollama: гашу службу…".into());
    // Orphan sweep scans every process (sysinfo) and sleeps between passes:
    // blocking work runs in the blocking pool, not on the async worker (V-63).
    if let Err(error) = tokio::task::spawn_blocking(kill_orphan_runtime_processes).await {
        crate::boot_log(&format!("очистка сирот рантайма не удалась: {error}"));
    }

    let total = std::fs::metadata(&zip_path).map(|m| m.len()).unwrap_or(0);
    on_progress(total, total, "обновление Ollama: распаковка…".into());
    // Unzipping ~1.5 GB is blocking IO — the blocking pool, not the async
    // worker (V-63).
    tokio::task::spawn_blocking({
        let zip_path = zip_path.clone();
        let dir = dir.clone();
        move || paths::extract_zip(&zip_path, &dir, 0)
    })
    .await
    .map_err(|e| format!("распаковка Ollama не запустилась: {e}"))??;
    let _ = std::fs::remove_file(&zip_path);

    if !exe.exists() {
        return Err("после обновления не нашлось ollama.exe".into());
    }
    write_ollama_version_marker(version)?;
    on_progress(total, total, "Ollama обновлён".into());
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use sysinfo::Pid;

    fn pid(n: u32) -> Pid {
        Pid::from_u32(n)
    }

    /// Only our own tree must be shut down: `ollama serve` → runner →
    /// llama-server. A broad sweep killed the runtime of a neighboring app
    /// instance, and it waited for the dead serve all 60 seconds.
    #[test]
    fn test_process_tree_covers_own_descendants_only() {
        let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
        children.insert(pid(100), vec![pid(200)]);
        children.insert(pid(200), vec![pid(300)]);
        children.insert(pid(999), vec![pid(1000)]);

        let tree = process_tree(&children, pid(100));
        assert_eq!(tree, vec![pid(100), pid(200), pid(300)]);
        assert!(!tree.contains(&pid(999)));
        assert!(!tree.contains(&pid(1000)));
    }

    #[test]
    fn test_process_tree_survives_recycled_pid_cycle() {
        let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
        children.insert(pid(100), vec![pid(200)]);
        children.insert(pid(200), vec![pid(100)]);

        let tree = process_tree(&children, pid(100));
        assert_eq!(tree, vec![pid(100), pid(200)]);
    }

    /// A parent younger than the child is a reused PID, not a parent.
    #[test]
    fn test_parent_younger_than_child_is_a_recycled_pid() {
        assert!(parent_is_older(100, 200));
        assert!(parent_is_older(200, 200), "одна секунда старта — связь верна");
        assert!(!parent_is_older(300, 200));
        assert!(parent_is_older(0, 200), "время не прочитано — не рвём связь");
    }

    /// Accept only our own serve: the same port and the same store. A system
    /// Ollama with its own OLLAMA_MODELS is foreign, even if it answers on the port.
    #[test]
    fn test_serves_store_matches_port_and_store() {
        let store = std::path::Path::new(r"Z:\AImodels\ollama");
        let ours = vec![
            "=C:=C:\\".to_string(),
            "OLLAMA_HOST=127.0.0.1:11434".to_string(),
            "ollama_models=z:\\aimodels\\ollama\\".to_string(),
        ];
        assert!(serves_store(&ours, 11434, store));
        assert!(!serves_store(&ours, 11435, store), "другой порт — другой serve");

        let system = vec![
            "OLLAMA_HOST=127.0.0.1:11434".to_string(),
            r"OLLAMA_MODELS=C:\Users\me\.ollama\models".to_string(),
        ];
        assert!(!serves_store(&system, 11434, store));
        assert!(!serves_store(&[], 11434, store), "окружение не прочитано — не наш");
    }

    /// Versions are compared numerically, not as strings: "0.40.10" is newer than "0.40.3".
    /// An unreadable version — the update fixes a broken install too.
    #[test]
    fn test_ollama_needs_update_compares_numerically() {
        assert!(ollama_needs_update("v0.33.1"), "более старая — обновляем");
        assert!(ollama_needs_update("0.32.15"), "без префикса v тоже читается");
        assert!(ollama_needs_update(""), "нет версии — чиним обновлением");
        assert!(!ollama_needs_update("v0.40.3"), "та же версия — не трогаем");
        assert!(!ollama_needs_update("v0.40.10"), "более новая — не откатываем вниз");
    }

    /// `ollama --version` prints the version in two forms: serve available —
    /// "ollama version is X", unavailable — "Warning: client version is X".
    #[test]
    fn test_ollama_version_output_is_parsed() {
        assert_eq!(parse_ollama_version_output("ollama version is 0.40.3\n"), "v0.40.3");
        let warning = "Warning: could not connect to a running Ollama instance\n\
                       Warning: client version is 0.40.3\n";
        assert_eq!(parse_ollama_version_output(warning), "v0.40.3");
        assert_eq!(parse_ollama_version_output("совсем другое"), "");
    }

    #[test]
    fn test_comparable_path_drops_verbatim_prefix() {
        assert_eq!(
            comparable_path(std::path::Path::new(r"\\?\Z:\AImodels\Cloufam\runtime\ollama")),
            comparable_path(std::path::Path::new(r"Z:\AImodels\Cloufam\runtime\ollama"))
        );
    }

    /// V-24: entries of a GNU `sha256sum` file are read for our archive only —
    /// both separator forms, hex normalized to lower case.
    #[test]
    fn test_sha256sum_entry_reads_gnu_format() {
        let text = "E5ECFF02DFC7CAE8DDF6D6B7A7AC5F727FFEEDF4491F44AE75FF5B0820DBBBD5  ollama-windows-amd64.zip\n\
                    2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae *other.zip\n";
        assert_eq!(
            sha256sum_entry(text, "ollama-windows-amd64.zip"),
            Some("e5ecff02dfc7cae8ddf6d6b7a7ac5f727ffeedf4491f44ae75ff5b0820dbbbd5".to_string())
        );
        assert_eq!(
            sha256sum_entry(text, "other.zip"),
            Some("2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae".to_string())
        );
        assert_eq!(sha256sum_entry(text, "missing.zip"), None);
        assert_eq!(sha256sum_entry("мусор без хэшей", "ollama-windows-amd64.zip"), None);
    }

    /// V-24: hashing runs on a file handle in the blocking pool and matches
    /// the published vector (SHA-256 of `abc`).
    #[tokio::test]
    async fn test_sha256_of_file_matches_reference_vector() {
        let dir = std::env::temp_dir().join("lk-runtime-sha-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("abc.bin");
        std::fs::write(&file, b"abc").unwrap();

        let hash = sha256_of_file(file.clone()).await.unwrap();
        assert_eq!(hash, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// V-24: a tampered archive is rejected and deleted before extraction; the
    /// pinned release is checked against the compiled-in checksum, so the test
    /// needs no network at all.
    #[tokio::test]
    async fn test_verify_runtime_zip_rejects_mismatch() {
        let dir = std::env::temp_dir().join("lk-runtime-verify-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let zip = dir.join("ollama.zip");
        std::fs::write(&zip, b"tampered bytes").unwrap();

        // Test-only client on the pinned-checksum path (no requests are sent):
        // bypass any system proxy so the test cannot depend on machine
        // network settings.
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let error = verify_runtime_zip(&client, &zip, OLLAMA_VERSION, "тест")
            .await
            .unwrap_err();

        assert!(error.contains("контрольная сумма"), "получено: {error}");
        assert!(!zip.exists(), "архив с неверной суммой удалён");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

