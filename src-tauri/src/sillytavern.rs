use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

/// PID of the ST being started before it is saved into `AppState`: closing the window while
/// waiting for readiness (up to 120 s) left the node server orphaned — the same race
/// as with Ollama, reproduced on 21.10.2026 on 8001.
static STARTING_PID: AtomicU32 = AtomicU32::new(0);
/// PID of the running `npm install`. It works in the same `st_dir`, but the orphan sweep
/// deliberately does not touch it (otherwise it would kill a live npm during boot); on exit and
/// self-replacement it is killed by the saved PID — otherwise it keeps writing
/// `node_modules` after the window closes, and the next launch starts a second npm
/// on top of the same tree.
static NPM_PID: AtomicU32 = AtomicU32::new(0);
/// One ST update at a time (see `update_st_to`).
static ST_UPDATE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

use crate::l10n;

pub fn begin_shutdown() {
    SHUTTING_DOWN.store(true, Ordering::SeqCst);
    for slot in [&STARTING_PID, &NPM_PID] {
        let pid = slot.swap(0, Ordering::SeqCst);
        if pid != 0 {
            kill_pid_tree(pid);
        }
    }
}

pub fn clear_starting_pid() {
    STARTING_PID.store(0, Ordering::SeqCst);
}

fn adopt_started(child: &mut Child) -> Result<(), String> {
    let pid = child.id();
    STARTING_PID.store(pid, Ordering::SeqCst);
    if SHUTTING_DOWN.load(Ordering::SeqCst) {
        STARTING_PID.store(0, Ordering::SeqCst);
        kill_pid_tree(pid);
        let _ = child.kill();
        let _ = child.wait();
        return Err("приложение завершается".into());
    }
    Ok(())
}

fn kill_pid_tree(pid: u32) {
    #[cfg(windows)]
    {
        let mut cmd = Command::new("taskkill");
        cmd.args(["/PID", &pid.to_string(), "/T", "/F"]);
        hide_console(&mut cmd);
        let _ = cmd.status();
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
    }
}

use tokio::time::{sleep, Duration};

use crate::comfyui;
use crate::paths;

/// Pinned SillyTavern version. Updating ST = a new application release:
/// bump this constant, check `.st-research/SillyTavern-<ver>` for
/// injection contracts that drifted (form field ids, DOMPurify custom prefix,
/// regex_placement, TextGen preset schema, USER_DIRECTORY_TEMPLATE — see
/// gotchas.md; the stream patch sites in script.js are checked by the
/// `stream_tail_patch_matches_pinned_st` test) and cut a release. Inside
/// the zip unpacking, the ST git updater does not work — `update_st()` below does the update.
pub const ST_VERSION: &str = "1.19.0";
const ST_ZIP_URL: &str = "https://codeload.github.com/SillyTavern/SillyTavern/zip/refs/tags/{VERSION}";
const NODE_INDEX_URL: &str = "https://nodejs.org/dist/index.json";

/// Marker of the ST code version on disk — a regular file next to `server.js`, not a
/// field in `Settings`: it describes what actually lies in this folder, not the
/// user's choice, and does not get confused on export/reset of settings.
/// Written by `download_st`/`update_st` on every successful code change.
/// A missing marker on an already installed ST always means version 1.18.0:
/// the only one Cloufam ever downloaded before the marker appeared.
fn st_version_marker_path() -> PathBuf {
    st_dir().join(".lk-st-version")
}

pub fn installed_st_version() -> String {
    std::fs::read_to_string(st_version_marker_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "1.18.0".to_string())
}

fn write_st_version_marker(version: &str) -> Result<(), String> {
    std::fs::write(st_version_marker_path(), version).map_err(|e| e.to_string())
}

/// Whether a new version should be downloaded over the installed one. Upward only: after
/// the application is rolled back to a release with an older ST pin, updating "down"
/// is not allowed — the old version's zip would land on top of newer files without deleting them.
/// An unreadable marker is treated as outdated, as before.
pub fn st_needs_update(installed_version: &str) -> bool {
    fn parse(v: &str) -> Option<Vec<u64>> {
        v.trim().split('.').map(|part| part.parse().ok()).collect()
    }
    let (Some(mut installed), Some(mut pinned)) = (parse(installed_version), parse(ST_VERSION)) else {
        return installed_version != ST_VERSION;
    };
    let len = installed.len().max(pinned.len());
    installed.resize(len, 0);
    pinned.resize(len, 0);
    installed < pinned
}

pub fn st_dir() -> PathBuf {
    // ST directory — always next to the program (`base_dir()`); the dev branch for
    // the old repository path was removed: the folder `Z:\Git\Programs\Cloufam`
    // no longer exists after the move (V-26).
    paths::base_dir().join("sillytavern")
}

pub fn node_dir() -> PathBuf {
    paths::base_dir().join("runtime").join("node")
}

fn node_exe() -> PathBuf {
    // Portable node — always in `runtime/node` next to the program; the dev branch for
    // the old repository path was removed (V-26, see `st_dir`).
    node_dir().join("node.exe")
}

/// Fallback: if the portable Node does not start (Kaspersky blocks it),
/// try the system Node from standard paths. Runs in an async task of the
/// tauri runtime: waiting for the trial `--version` goes through
/// `tokio::time::sleep` so as not to occupy a tokio worker (V-63).
async fn node_exe_with_fallback() -> PathBuf {
    let local = node_exe();
    // Check that the portable node can start. We wait for `--version` to finish
    // rather than for a fixed pause: on a cold disk it answers longer,
    // and a blind wait sent a live portable node to the system Node.
    let mut probe = std::process::Command::new(&local);
    probe
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    hide_console(&mut probe);
    if let Ok(mut child) = probe.spawn() {
        for _ in 0..30 {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return local,
                Ok(Some(_)) | Err(_) => break,
                Ok(None) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    // Fallback: standard Windows system Node paths.
    let candidates = [
        r"C:\Program Files\nodejs\node.exe",
        r"C:\Program Files (x86)\nodejs\node.exe",
    ];
    for path in &candidates {
        let p = PathBuf::from(path);
        if p.exists() {
            crate::boot_log(&format!(
                "Портативный Node заблокирован, используем системный: {}",
                p.display()
            ));
            return p;
        }
    }
    // If nothing was found — return the portable one (let it fail with an error).
    local
}

pub fn installed() -> bool {
    code_installed() && st_dir().join("node_modules").join("express").exists()
}

/// ST code and Node are in place, dependencies (`node_modules`) are not required:
/// they can be delivered by `npm_install` without the wizard.
pub fn code_installed() -> bool {
    st_dir().join("server.js").exists() && node_exe().exists()
}

/// SillyTavern installation progress: (stage, status line, done/total bytes).
pub type ProgressFn = Arc<dyn Fn(String, String, u64, u64) + Send + Sync>;

pub fn parse_lts_version(list: &serde_json::Value) -> Option<String> {
    list.as_array()
        .and_then(|a| {
            a.iter().find(|v| {
                v["lts"].as_str().is_some() || v["lts"].as_bool() == Some(true)
            })
        })
        .and_then(|v| v["version"].as_str())
        .map(|s| s.trim_start_matches('v').to_string())
}

/// Pick the latest LTS Node from the index (we need the stable channel).
async fn lts_node_version() -> Result<String, String> {
    const FALLBACK_LTS: &str = "24.20.0";
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
    else {
        return Ok(FALLBACK_LTS.to_string());
    };
    let Ok(resp) = client.get(NODE_INDEX_URL).send().await else {
        return Ok(FALLBACK_LTS.to_string());
    };
    let Ok(list) = resp.json::<serde_json::Value>().await else {
        return Ok(FALLBACK_LTS.to_string());
    };
    Ok(parse_lts_version(&list).unwrap_or_else(|| FALLBACK_LTS.to_string()))
}

/// Download the portable Node into `runtime/node/`.
pub async fn download_node(cancel: Arc<AtomicBool>, on_progress: ProgressFn) -> Result<(), String> {
    let exe = node_exe();
    if exe.exists() {
        return Ok(());
    }
    let version = lts_node_version().await?;
    let url = format!("https://nodejs.org/dist/v{version}/node-v{version}-win-x64.zip");
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;

    let zip_path = paths::base_dir().join("runtime").join("node.zip");
    std::fs::create_dir_all(paths::base_dir().join("runtime")).map_err(|e| e.to_string())?;

    let label = format!("Node v{version}");
    let progress_label = label.clone();
    let progress_inner = on_progress.clone();
    let progress: Arc<dyn Fn(u64, u64, String) + Send + Sync> = Arc::new(move |got, total, _status| {
        progress_inner("node".into(), progress_label.clone(), got, total);
    });
    // Resume: the partially downloaded archive stays on disk and continues, not
    // starts over.
    let flags = crate::downloads::Flags::with_cancel(cancel.clone());
    crate::paths::download_to_file(&client, &url, &zip_path, &label, flags, progress).await?;

    let total = std::fs::metadata(&zip_path).map(|m| m.len()).unwrap_or(0);
    on_progress("node".into(), l10n::t("распаковка Node…", "unpacking Node…"), total, total);
    // The function runs in an async task of the tauri runtime, while unpacking is
    // blocking IO of a gigabyte archive: we move it to spawn_blocking so as not to
    // occupy a tokio worker (as in lib.rs and other downloads of the project).
    let zip = zip_path.clone();
    let dest = node_dir();
    tokio::task::spawn_blocking(move || paths::extract_zip(&zip, &dest, 1))
        .await
        .map_err(|e| format!("распаковка Node: {e}"))??;
    let _ = std::fs::remove_file(&zip_path);
    if exe.exists() {
        Ok(())
    } else {
        Err("в архиве Node не нашлось node.exe".into())
    }
}

/// Download SillyTavern into `sillytavern/`.
pub async fn download_st(cancel: Arc<AtomicBool>, on_progress: ProgressFn) -> Result<(), String> {
    if st_dir().join("server.js").exists() {
        return Ok(());
    }
    let url = ST_ZIP_URL.replace("{VERSION}", ST_VERSION);
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;

    let zip_path = paths::base_dir().join("sillytavern.zip");
    let label = format!("SillyTavern {ST_VERSION}");
    let progress_label = label.clone();
    let progress_inner = on_progress.clone();
    let progress: Arc<dyn Fn(u64, u64, String) + Send + Sync> = Arc::new(move |got, total, _status| {
        progress_inner("sillytavern".into(), progress_label.clone(), got, total);
    });
    let flags = crate::downloads::Flags::with_cancel(cancel.clone());
    // Resume: the partially downloaded archive continues, not downloads anew.
    crate::paths::download_to_file(&client, &url, &zip_path, &label, flags, progress).await?;

    let total = std::fs::metadata(&zip_path).map(|m| m.len()).unwrap_or(0);
    on_progress("sillytavern".into(), l10n::t("распаковка SillyTavern…", "unpacking SillyTavern…"), total, total);
    // The codeload archive contains the root folder SillyTavern-<ver> — we strip it.
    // The function runs in an async task of the tauri runtime, while unpacking is
    // blocking IO: we move it to spawn_blocking so as not to occupy a tokio worker.
    let zip = zip_path.clone();
    let dest = st_dir();
    tokio::task::spawn_blocking(move || paths::extract_zip(&zip, &dest, 1))
        .await
        .map_err(|e| format!("распаковка SillyTavern: {e}"))??;
    let _ = std::fs::remove_file(&zip_path);
    if let Err(e) = seed_russian_locale() {
        // Not fatal (unpacking/boot succeeded), but staying silent is not allowed: a locale patch
        // that fails to apply otherwise disables it without diagnostics (V-25).
        crate::boot_log(&format!("SillyTavern: {e}"));
    }

    if !st_dir().join("server.js").exists() {
        return Err("в архиве SillyTavern не нашлось server.js".into());
    }
    let _ = write_st_version_marker(ST_VERSION);
    Ok(())
}

/// Update an already installed SillyTavern to `ST_VERSION`. Unlike
/// `download_st`, it does not check whether it is already installed — the caller
/// (`bring_up_inner`) decides by `installed_st_version()`/`st_needs_update()`.
pub async fn update_st(from_version: &str, cancel: Arc<AtomicBool>, on_progress: ProgressFn) -> Result<(), String> {
    update_st_to(from_version, ST_VERSION, cancel, on_progress).await
}

/// Update the installed SillyTavern to a specific version — this is how the
/// "Update SillyTavern" menu item works: the caller sets the target
/// (the application's pinned version or the latest upstream release).
///
/// The code (`public/`, `src/`, `server.js`, ...) is overwritten by the new zip.
/// `data/` in the archive itself is always empty — upstream keeps only `.gitkeep`
/// there (`data/` is in SillyTavern's `.gitignore`), so the extractor does not touch
/// `data/default-user/` (chats, characters, world info, `settings.json`) at all:
/// `extract_zip` only writes files from the archive and deletes nothing — old
/// files that no longer exist in the new version stay on disk as dead weight,
/// a deliberate trade-off of safety for simplicity. `node_modules/` is rebuilt
/// for the new version: npm over a foreign `node_modules` and an old
/// `package-lock.json` reconstructs dependencies unreliably.
pub async fn update_st_to(
    from_version: &str,
    to_version: &str,
    cancel: Arc<AtomicBool>,
    on_progress: ProgressFn,
) -> Result<(), String> {
    // One update at a time: two runs share `sillytavern-update.zip`, wipe
    // `node_modules` under each other and kill the server the neighbor brought up.
    let _update = ST_UPDATE_LOCK
        .try_lock()
        .map_err(|_| "обновление SillyTavern уже идёт".to_string())?;
    let url = ST_ZIP_URL.replace("{VERSION}", to_version);
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;

    let zip_path = paths::base_dir().join("sillytavern-update.zip");
    let label = format!("SillyTavern {from_version} → {to_version}");
    let progress_label = label.clone();
    let progress_inner = on_progress.clone();
    let progress: Arc<dyn Fn(u64, u64, String) + Send + Sync> = Arc::new(move |got, total, _status| {
        progress_inner("sillytavern-update".into(), progress_label.clone(), got, total);
    });
    let flags = crate::downloads::Flags::with_cancel(cancel.clone());
    crate::paths::download_to_file(&client, &url, &zip_path, &label, flags, progress).await?;

    let total = std::fs::metadata(&zip_path).map(|m| m.len()).unwrap_or(0);
    on_progress(
        "sillytavern-update".into(),
        format!("распаковка SillyTavern {to_version}…"),
        total,
        total,
    );
    // The function runs in an async task of the tauri runtime, while unpacking is
    // blocking IO: we move it to spawn_blocking so as not to occupy a tokio worker.
    let zip = zip_path.clone();
    let dest = st_dir();
    tokio::task::spawn_blocking(move || paths::extract_zip(&zip, &dest, 1))
        .await
        .map_err(|e| format!("распаковка обновления SillyTavern: {e}"))??;
    let _ = std::fs::remove_file(&zip_path);

    if !st_dir().join("server.js").exists() {
        return Err("после распаковки обновления не нашлось server.js".into());
    }
    // The code is already updated — record that immediately. Previously the marker was written after
    // npm, and an npm failure forced every next start to download the archive again,
    // wipe node_modules and repeat the whole cycle instead of one fix of
    // dependencies (boot can do that separately).
    write_st_version_marker(to_version)?;

    on_progress("sillytavern-update".into(), l10n::t("обновление зависимостей…", "updating dependencies…"), 0, 0);
    // A fresh package.json is already in place (just unpacked) — the old
    // dependencies are wiped and installed anew for it. The silent `let _ =` here
    // led to an npm skip below: leftovers of node_modules with express left
    // npm_install thinking the dependencies were already installed.
    let nm = st_dir().join("node_modules");
    if nm.exists() {
        std::fs::remove_dir_all(&nm).map_err(|e| format!("не удалось удалить node_modules: {e}"))?;
        if nm.exists() {
            return Err("node_modules остался после удаления — закройте блокирующие процессы".into());
        }
    }
    npm_install(cancel, on_progress).await?;

    if let Err(e) = seed_russian_locale() {
        // Not fatal (unpacking/boot succeeded), but staying silent is not allowed: a locale patch
        // that fails to apply otherwise disables it without diagnostics (V-25).
        crate::boot_log(&format!("SillyTavern: {e}"));
    }
    Ok(())
}

/// Tag of the latest SillyTavern release from the GitHub API response: `v1.2.3` → `1.2.3`.
pub fn parse_latest_release_tag(release: &serde_json::Value) -> Option<String> {
    release["tag_name"]
        .as_str()
        .map(|s| s.trim().trim_start_matches('v').to_string())
        .filter(|s| !s.is_empty())
}

/// Latest SillyTavern version upstream — for the "Update SillyTavern" item.
pub async fn latest_st_version() -> Result<String, String> {
    const LATEST_URL: &str = "https://api.github.com/repos/SillyTavern/SillyTavern/releases/latest";
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let release: serde_json::Value = client
        .get(LATEST_URL)
        // GitHub API responds 403 without a User-Agent.
        .header("User-Agent", "Cloufam")
        .send()
        .await
        .map_err(|e| format!("GitHub: {e}"))?
        .error_for_status()
        .map_err(|e| format!("GitHub: {e}"))?
        .json()
        .await
        .map_err(|e| format!("GitHub: {e}"))?;
    parse_latest_release_tag(&release)
        .ok_or_else(|| "GitHub не вернул tag_name релиза SillyTavern".to_string())
}

/// Re-seed our extensions, theme and patches into the freshly unpacked ST copy:
/// the upstream zip overwrites `index.html` and patched `public/` scripts, while
/// extensions in `scripts/extensions/third-party` do not arrive from the archive at all.
/// Returns warnings: which pieces did not land (a new ST version may
/// diverge from the pinned injection sites — then the corresponding Cloufam
/// feature is silently disabled until its support is released).
pub fn reseed_after_update(gateway_port: u16) -> Vec<String> {
    let mut warnings = Vec::new();
    for (what, result) in [
        ("websearch", seed_web_search()),
        ("tagger", seed_tagger()),
        ("director", seed_director()),
        ("moonlit", seed_moonlit_theme()),
        ("тема", seed_theme()),
        ("локаль", seed_russian_locale()),
        ("cloufam-ui", seed_cloufam_ui(gateway_port)),
    ] {
        if let Err(e) = result {
            warnings.push(format!("{what}: {e}"));
        }
    }
    if comfyui::is_installed() {
        if let Err(e) = patch_sd_russian_triggers() {
            warnings.push(format!("SD-триггеры: {e}"));
        }
    }
    if let Err(e) = patch_backgrounds_fix() {
        warnings.push(format!("backgrounds: {e}"));
    }
    if let Err(e) = patch_world_info() {
        warnings.push(format!("world-info: {e}"));
    }
    if let Err(e) = patch_streaming_tail() {
        warnings.push(format!("stream-tail: {e}"));
    }
    warnings
}

/// npm install in the SillyTavern folder. A long step: output lines go to the status.
pub async fn npm_install(cancel: Arc<AtomicBool>, on_progress: ProgressFn) -> Result<(), String> {
    if SHUTTING_DOWN.load(Ordering::SeqCst) {
        return Err("приложение завершается".into());
    }
    // express check — only for resume with live dependencies; after
    // update_st node_modules is deleted entirely, nothing to skip.
    if st_dir().join("node_modules").join("express").exists() {
        return Ok(());
    }
    if !st_dir().join("package.json").exists() {
        return Err("в папке SillyTavern нет package.json".into());
    }
    on_progress("npm".into(), l10n::t("установка зависимостей…", "installing dependencies…"), 0, 0);

    // npm is portable, next to our node; the dev branch for the old repository
    // path was removed (V-26, see `st_dir`).
    let npm = node_dir().join("npm.cmd");
    // If the portable Node is blocked by antivirus, npm must go through the
    // system one. PATH does not help here: npm.cmd takes the neighboring node.exe while
    // the file exists — so we launch npm-cli.js with the system Node directly.
    let node = node_exe_with_fallback().await;
    let npm_cli = node_dir().join("node_modules").join("npm").join("bin").join("npm-cli.js");
    let mut cmd = if node != node_exe() && npm_cli.exists() {
        let mut direct = Command::new(&node);
        direct.arg(&npm_cli);
        direct
    } else {
        Command::new(&npm)
    };
    let mut path_env = node.parent().map(|p| p.to_path_buf()).unwrap_or_else(node_dir).into_os_string();
    if let Some(existing_path) = std::env::var_os("PATH") {
        path_env.push(";");
        path_env.push(existing_path);
    }
    let npm_cache = paths::data_dir().join("npm-cache");
    let _ = std::fs::create_dir_all(&npm_cache);
    cmd.args(["install", "--no-audit", "--no-fund", "--loglevel", "error"])
        .arg("--cache")
        .arg(&npm_cache)
        .current_dir(st_dir())
        .env("PATH", path_env)
        .env("NO_UPDATE_NOTIFIER", "1")
        .env("npm_config_cache", &npm_cache)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hide_console(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("не удалось запустить npm: {e}"))?;
    NPM_PID.store(child.id(), Ordering::SeqCst);

    let out = child.stdout.take();
    let err = child.stderr.take();
    let out_thread = std::thread::spawn(move || read_lines(out));
    let err_thread = std::thread::spawn(move || read_lines(err));
    // The cancel flag during boot is one-shot, no one toggles it: without a deadline
    // a hung npm kept the launcher on "Restoring dependencies" forever.
    // 45 minutes: hundreds of ST packages at 1–2 Mbit/s did not fit into 15
    let deadline = std::time::Instant::now() + Duration::from_secs(45 * 60);

    loop {
        let timed_out = std::time::Instant::now() >= deadline;
        if timed_out || cancel.load(Ordering::Relaxed) {
            // We kill the whole npm tree (node workers): a single child.kill()
            // leaves grandchildren, and joining the readers hangs on the pipes.
            #[cfg(windows)]
            {
                let pid = child.id();
                let mut cmd = Command::new("taskkill");
                cmd.args(["/PID", &pid.to_string(), "/T", "/F"]);
                hide_console(&mut cmd);
                let _ = cmd.status();
            }
            let _ = child.kill();
            let _ = child.wait();
            let _ = out_thread.join();
            let _ = err_thread.join();
            NPM_PID.store(0, Ordering::SeqCst);
            return Err(if timed_out { "npm install не завершился за 45 минут".into() } else { "отменено пользователем".into() });
        }
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            NPM_PID.store(0, Ordering::SeqCst);
            let _ = out_thread.join();
            let err_output = err_thread.join().unwrap_or_default();
            if status.success() {
                return Ok(());
            }
            let err_trimmed = err_output.trim();
            if err_trimmed.is_empty() {
                return Err(format!("npm install завершился с кодом {status}"));
            } else {
                return Err(format!("npm install ({status}): {err_trimmed}"));
            }
        }
        sleep(Duration::from_millis(300)).await;
    }
}

fn read_lines<R: std::io::Read>(stream: Option<R>) -> String {
    use std::io::{BufRead, BufReader};
    let Some(s) = stream else { return String::new() };
    let mut lines = Vec::new();
    for line in BufReader::new(s).lines().map_while(Result::ok) {
        if !line.trim().is_empty() {
            if lines.len() >= 10 {
                lines.remove(0);
            }
            lines.push(line);
        }
    }
    lines.join("\n")
}

fn hide_console(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

/// config.yaml for local startup: listen on localhost only, do not open the
/// browser (the Tauri window navigates), ST model auto-updates/auto-downloads
/// are disabled — the application manages dependencies itself.
pub fn write_config(port: u16) -> Result<(), String> {
    let path = st_dir().join("config.yaml");
    let cfg = config_template(port);
    match std::fs::read_to_string(&path) {
        // An existing ST file extends from `default/config.yaml` to ~90
        // keys, and the user edits their own there (thumbnails, backups,
        // performance). Unconditional overwrite cut all that down to fifteen
        // lines on every start — we change only our root keys.
        Ok(existing) => {
            let patched = patch_config(&existing, port);
            if patched != existing {
                std::fs::write(&path, patched).map_err(|e| e.to_string())?;
            }
            Ok(())
        }
        Err(_) => std::fs::write(&path, cfg).map_err(|e| e.to_string()),
    }
}

/// Root config keys the application sets: port and local mode.
/// We do not touch nested keys — those are the user's ST settings.
fn patch_config(text: &str, port: u16) -> String {
    let mut out = set_root_key(text, "port", &format!("port: {port}"));
    out = set_root_key(&out, "listen", "listen: false");
    out = set_root_key(&out, "whitelistMode", "whitelistMode: true");
    out = set_root_key(&out, "enableUserAccounts", "enableUserAccounts: false");
    out = set_root_key(&out, "enableServerPlugins", "enableServerPlugins: true");
    out
}

/// Replace a root key line (`key:` without indentation) or append it.
/// A nested key with the same name is not affected: the check is by the start
/// of the line, and nested YAML lines begin with spaces.
fn set_root_key(text: &str, key: &str, rendered: &str) -> String {
    let prefix = format!("{key}:");
    let mut out = String::with_capacity(text.len() + rendered.len() + 1);
    let mut replaced = false;
    for line in text.lines() {
        if !replaced && line.starts_with(&prefix) {
            out.push_str(rendered);
            out.push('\n');
            replaced = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !replaced {
        out.push_str(rendered);
        out.push('\n');
    }
    out
}

fn config_template(port: u16) -> String {
    format!(
        r#"dataRoot: ./data
listen: false
port: {port}
browserLaunch:
  enabled: false
  browser: 'default'
whitelistMode: true
whitelist:
  - ::1
  - 127.0.0.1
enableUserAccounts: false
extensions:
  enabled: true
  autoUpdate: false
  models:
    autoDownload: false
enableServerPlugins: true
enableServerPluginsAutoUpdate: false
logging:
  enableAccessLog: false
  minLogLevel: 1
"#
    )
}

/// Deploy the adaptive and built-in characters into the ST characters folder.
/// The list is taken from the catalog, not enumerated in code: the set of cards grows,
/// and a forgotten line would leave a character only for those
/// who installed the application anew.
pub fn seed_characters() -> Result<(), String> {
    let chars_dir = st_dir().join("data").join("default-user").join("characters");
    std::fs::create_dir_all(&chars_dir).map_err(|e| e.to_string())?;

    let src_dir = paths::characters_dir();
    let default_content = st_dir().join("default").join("content");

    // The set lives only locally (the `characters` folder next to the program) and is not
    // part of the build: without it, only the workflows are seeded.
    let mut char_files: Vec<String> = std::fs::read_dir(&src_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "png"))
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    char_files.sort();

    for file in char_files {
        let src = src_dir.join(&file);
        let dst = chars_dir.join(&file);
        // We do not overwrite an existing user card: the user could have
        // edited it, and new characters are added on their own.
        if !dst.exists() {
            let _ = std::fs::copy(&src, &dst);
        }

    }

    // Deploy the standard ComfyUI workflows for image generation
    let wf_dir = st_dir().join("data").join("default-user").join("workflows");
    let _ = std::fs::create_dir_all(&wf_dir);
    for wf in ["Default_Comfy_Workflow.json", "Char_Avatar_Comfy_Workflow.json"] {
        let src = default_content.join(wf);
        let dst = wf_dir.join(wf);
        if src.exists() && !dst.exists() {
            let _ = std::fs::copy(&src, &dst);
        }
    }

    Ok(())
}

/// Reset the active character and clean up outdated files in existing installations.
pub fn cleanup_legacy_characters() -> Result<(), String> {
    let chars_dir = st_dir().join("data").join("default-user").join("characters");
    let old_card = chars_dir.join("Coder.png");
    if old_card.exists() {
        let _ = std::fs::remove_file(old_card);
    }

    let old_chats = st_dir().join("data").join("default-user").join("chats").join("Coder");
    if old_chats.exists() {
        let _ = std::fs::remove_dir_all(old_chats);
    }

    let settings_path = st_dir().join("data").join("default-user").join("settings.json");
    if settings_path.exists() {
        let text = std::fs::read_to_string(&settings_path).map_err(|e| e.to_string())?;
        if let Ok(mut st) = serde_json::from_str::<serde_json::Value>(&text) {
            let active = st.get("active_character").and_then(|v| v.as_str()).unwrap_or("");
            if active == "Coder.png" || active == "default_Assistant.png" || active.is_empty() {
                st["active_character"] = serde_json::Value::String(default_character());
                let serialized = serde_json::to_string_pretty(&st).map_err(|e| e.to_string())?;
                write_if_changed(&settings_path, &serialized)?;
            }
        }
    }
    Ok(())
}

/// The default character without a card set — the stock ST card.
const FALLBACK_CHARACTER: &str = "default_Seraphina.png";

/// The default character for new installations and for resetting the active one: the card
/// set may assign its own (`default_character` in `tags.json`).
pub fn default_character() -> String {
    bundled_tags()
        .get("default_character")
        .and_then(|v| v.as_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(FALLBACK_CHARACTER)
        .to_string()
}

/// Remove the demo character Echo from existing installations:
/// the card, its chats and the active selection. Seeding no longer creates it, but for those
/// who updated, the files remained — they must be wiped, otherwise the character returns.
/// Returns the number of deleted objects (0 — there was nothing to remove).
pub fn remove_echo_character() -> Result<usize, String> {
    let user_dir = st_dir().join("data").join("default-user");
    let mut removed = 0;

    for card in [
        user_dir.join("characters").join("Echo.png"),
        st_dir().join("default").join("content").join("Echo.png"),
    ] {
        if card.exists() && std::fs::remove_file(&card).is_ok() {
            removed += 1;
        }
    }

    let chats = user_dir.join("chats").join("Echo");
    if chats.exists() && std::fs::remove_dir_all(&chats).is_ok() {
        removed += 1;
    }

    let settings_path = user_dir.join("settings.json");
    if settings_path.exists() {
        let text = std::fs::read_to_string(&settings_path).map_err(|e| e.to_string())?;
        if let Ok(mut st) = serde_json::from_str::<serde_json::Value>(&text) {
            if st.get("active_character").and_then(|v| v.as_str()) == Some("Echo.png") {
                st["active_character"] = serde_json::Value::String(default_character());
                let serialized = serde_json::to_string_pretty(&st).map_err(|e| e.to_string())?;
                write_if_changed(&settings_path, &serialized)?;
                removed += 1;
            }
        }
    }

    Ok(removed)
}

const WEBSEARCH_PLUGIN_JS: &str = include_str!("../websearch/plugin-index.js");
const WEBSEARCH_EXT_MANIFEST: &str = include_str!("../websearch/extension-manifest.json");
const WEBSEARCH_EXT_JS: &str = include_str!("../websearch/extension-index.js");

/// Web search for characters: the ST server plugin (DuckDuckGo, no keys,
/// route /api/plugins/websearch/search) + the client slash command /поиск.
/// Both files are ours and are overwritten on divergence — otherwise, after an application
/// update the user would be left with an old client against a new plugin.
pub fn seed_web_search() -> Result<(), String> {
    let plugin_dir = st_dir().join("plugins").join("websearch");
    std::fs::create_dir_all(&plugin_dir).map_err(|e| e.to_string())?;
    write_if_changed(&plugin_dir.join("index.js"), WEBSEARCH_PLUGIN_JS)?;

    let ext_dir = st_dir()
        .join("data")
        .join("default-user")
        .join("extensions")
        .join("websearch");
    std::fs::create_dir_all(&ext_dir).map_err(|e| e.to_string())?;
    write_if_changed(&ext_dir.join("manifest.json"), WEBSEARCH_EXT_MANIFEST)?;
    write_if_changed(&ext_dir.join("index.js"), WEBSEARCH_EXT_JS)?;
    Ok(())
}

const TAGGER_PLUGIN_JS: &str = include_str!("../tagger/plugin-index.js");
const TAGGER_EXT_MANIFEST: &str = include_str!("../tagger/extension-manifest.json");
const TAGGER_EXT_JS: &str = include_str!("../tagger/extension-index.js");
const TAGGER_EXT_CSS: &str = include_str!("../tagger/extension-style.css");

/// Character auto-tagging: the server plugin bridge to the gateway (it knows the gateway port
/// from the node environment variable) and a UI extension with a button and a modal.
/// The files are ours — we overwrite them on divergence so that the extension version always
/// matches the binary (settings live in the application's data/settings.json).
pub fn seed_tagger() -> Result<(), String> {
    let plugin_dir = st_dir().join("plugins").join("cloufam");
    std::fs::create_dir_all(&plugin_dir).map_err(|e| e.to_string())?;
    write_if_changed(&plugin_dir.join("index.js"), TAGGER_PLUGIN_JS)?;

    let ext_dir = st_dir()
        .join("data")
        .join("default-user")
        .join("extensions")
        .join("cloufam-tagger");
    std::fs::create_dir_all(&ext_dir).map_err(|e| e.to_string())?;
    write_if_changed(&ext_dir.join("manifest.json"), TAGGER_EXT_MANIFEST)?;
    write_if_changed(&ext_dir.join("index.js"), TAGGER_EXT_JS)?;
    write_if_changed(&ext_dir.join("style.css"), TAGGER_EXT_CSS)?;
    Ok(())
}

const DIRECTOR_EXT_MANIFEST: &str = include_str!("../director/extension-manifest.json");
const DIRECTOR_EXT_JS: &str = include_str!("../director/extension-index.js");
const DIRECTOR_EXT_CSS: &str = include_str!("../director/extension-style.css");

/// Director mode: an extension with a panel, input mode and slash command.
/// The bridge to the gateway is the same server plugin `cloufam` as for auto-tagging.
/// Moonlit Echoes theme files (AGPL-3.0, author Rivelle), baked into the binary.
/// ST installs extensions via git, which the user's machine may not have,
/// so the theme ships with the package. We barely change the author's code: the edits are
/// the Russian locale (i18n/ru-ru.json and a line in the manifest) and the observer target in
/// src/core/observers.js (marked "Cloufam edit": it woke the callback on every
/// stream frame); our own refinements live in the theme's custom_css.
const MOONLIT_FILES: &[(&str, &str)] = &[
    ("LICENSE", include_str!("../moonlit/LICENSE")),
    ("README.md", include_str!("../moonlit/README.md")),
    ("extension.css", include_str!("../moonlit/extension.css")),
    ("i18n/ko-kr.json", include_str!("../moonlit/i18n/ko-kr.json")),
    ("i18n/ru-ru.json", include_str!("../moonlit/i18n/ru-ru.json")),
    ("i18n/th-th.json", include_str!("../moonlit/i18n/th-th.json")),
    ("i18n/vi-vn.json", include_str!("../moonlit/i18n/vi-vn.json")),
    ("i18n/zh-cn.json", include_str!("../moonlit/i18n/zh-cn.json")),
    ("i18n/zh-tw.json", include_str!("../moonlit/i18n/zh-tw.json")),
    ("index.js", include_str!("../moonlit/index.js")),
    ("manifest.json", include_str!("../moonlit/manifest.json")),
    ("src/bootstrap/init-extension.js", include_str!("../moonlit/src/bootstrap/init-extension.js")),
    ("src/bootstrap/lifecycle-hooks.js", include_str!("../moonlit/src/bootstrap/lifecycle-hooks.js")),
    ("src/config/default-settings.js", include_str!("../moonlit/src/config/default-settings.js")),
    ("src/config/theme-info.js", include_str!("../moonlit/src/config/theme-info.js")),
    ("src/config/theme-settings.js", include_str!("../moonlit/src/config/theme-settings.js")),
    ("src/core/observers.js", include_str!("../moonlit/src/core/observers.js")),
    ("src/core/theme-applier.js", include_str!("../moonlit/src/core/theme-applier.js")),
    ("src/services/hints.js", include_str!("../moonlit/src/services/hints.js")),
    ("src/services/settings-service.js", include_str!("../moonlit/src/services/settings-service.js")),
    ("src/services/slash-commands.js", include_str!("../moonlit/src/services/slash-commands.js")),
    ("src/services/theme-selector.js", include_str!("../moonlit/src/services/theme-selector.js")),
    ("src/ui/controls.js", include_str!("../moonlit/src/ui/controls.js")),
    ("src/ui/popout.js", include_str!("../moonlit/src/ui/popout.js")),
    ("src/ui/preset-manager.js", include_str!("../moonlit/src/ui/preset-manager.js")),
    ("src/ui/settings-factory.css", include_str!("../moonlit/src/ui/settings-factory.css")),
    ("src/ui/settings-factory.js", include_str!("../moonlit/src/ui/settings-factory.js")),
    ("src/ui/settings-tabs.js", include_str!("../moonlit/src/ui/settings-tabs.js")),
    ("src/utils/color.js", include_str!("../moonlit/src/utils/color.js")),
    ("style.css", include_str!("../moonlit/style.css")),
    ("theme/Glimmer - by Rivelle.json", include_str!("../moonlit/theme/Glimmer - by Rivelle.json")),
    ("theme/Moonlit Echoes - by Rivelle.json", include_str!("../moonlit/theme/Moonlit Echoes - by Rivelle.json")),
    ("theme/[Moonlit] Glimmer - by Rivelle.json", include_str!("../moonlit/theme/[Moonlit] Glimmer - by Rivelle.json")),
];

/// Deploy the Moonlit Echoes theme into the SillyTavern extensions. Files are written
/// file by file and only on divergence, so foreign extensions, characters,
/// chats and the user's persona are not touched.
pub fn seed_moonlit_theme() -> Result<(), String> {
    let ext_dir = st_dir()
        .join("data")
        .join("default-user")
        .join("extensions")
        .join("SillyTavern-MoonlitEchoesTheme");
    for (rel, content) in MOONLIT_FILES {
        let path = ext_dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        write_if_changed(&path, content)?;
    }
    Ok(())
}

pub fn seed_director() -> Result<(), String> {
    let ext_dir = st_dir()
        .join("data")
        .join("default-user")
        .join("extensions")
        .join("cloufam-director");
    std::fs::create_dir_all(&ext_dir).map_err(|e| e.to_string())?;
    write_if_changed(&ext_dir.join("manifest.json"), DIRECTOR_EXT_MANIFEST)?;
    write_if_changed(&ext_dir.join("index.js"), DIRECTOR_EXT_JS)?;
    write_if_changed(&ext_dir.join("style.css"), DIRECTOR_EXT_CSS)?;
    Ok(())
}

/// Write the file only on content divergence.
///
/// We write through a temporary file and rename, not truncate+write: SillyTavern
/// reads `settings.json` synchronously on every `/api/settings/get`, and a request
/// that hit the window between truncation and write got empty JSON. The client
/// started with defaults and the first `saveSettings` saved them over the theme,
/// prompts and tags. The body is the shared primitive `json_store::write_atomic`
/// (tmp + fsync + rename; without fsync, rename can outrun the data flush to disk).
///
/// Writes are serialized by the module lock: "read → compare → wrote" in one
/// critical section, and parallel synchronizations of the same file (settings.json
/// is written by boot, the settings window and the model menu) do not overwrite each other's
/// result. The unique tmp name and fsync are inside `write_atomic`.
fn write_if_changed(path: &std::path::Path, content: &str) -> Result<(), String> {
    if std::fs::read_to_string(path).map(|current| current == content).unwrap_or(false) {
        return Ok(());
    }
    static WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::json_store::write_atomic(path, content)
}

/// Liquid Glass: the glass injected via JS after the Moonlit extension loads —
/// a separate file so as not to depend on `custom_css`, which the extension overwrites.
const GLASS_CSS: &str = include_str!("../theme/glass.css");

const CLOUFAM_THEME_JSON: &str = include_str!("../theme/Cloufam.json");
/// Model menu inside SillyTavern: a showcase matched to the hardware, downloading with progress,
/// model selection for chat, memory and vision.
const MODELS_MENU_JS: &str = include_str!("../models_menu/panel.js");
const MODELS_MENU_CSS: &str = include_str!("../models_menu/panel.css");

/// Theme version. Bumped together with a theme edit: installed copies
/// lay the theme out anew while their `theme_version` is lower than this one.
/// 7 — the palette and `custom_css` in `theme/Cloufam.json` diverged from live
/// installations (on the checked machine `blur_tint_color` 0.4 vs 0.50 in the theme,
/// `chat_tint_color` 0.1 vs 0.14, `custom_css` 232 bytes shorter), and with
/// version 6 the merge never ran again.
/// 8 — our own anime background behind the glass (`src/assets/chat-bg.webp`): without wallpaper
/// the transparency of the glass is not visible. Set once when the version is bumped.
pub const THEME_VERSION: u32 = 8;

/// Default chat background: generated locally (ZavyChroma XL), not taken from the
/// ST distribution — we do not bake foreign assets into the program.
const CHAT_BACKGROUND: &[u8] = include_bytes!("../../src/assets/chat-bg.webp");
const CHAT_BACKGROUND_NAME: &str = "cloufam-room.webp";

/// Make our background active while preserving the other gallery fields (sorting, columns).
fn apply_cloufam_background(obj: &mut serde_json::Map<String, serde_json::Value>) -> bool {
    let bg = obj.entry("background").or_insert_with(|| serde_json::json!({}));
    let Some(bg_obj) = bg.as_object_mut() else {
        *bg = serde_json::json!({});
        return apply_cloufam_background(obj);
    };
    let url = format!("url(\"backgrounds/{CHAT_BACKGROUND_NAME}\")");
    let mut changed = false;
    for (key, value) in [("name", serde_json::json!(CHAT_BACKGROUND_NAME)), ("url", serde_json::json!(url))] {
        if bg_obj.get(key) != Some(&value) {
            bg_obj.insert(key.to_string(), value);
            changed = true;
        }
    }
    changed
}

/// Version of the built-in characters' markup (`default_tags.json`). Installations with
/// a lower version get the new markup over the old one on the first boot.
pub const TAGS_VERSION: u32 = 3;

/// Marker of our custom_css: by it we tell our styling from the manual one
/// written by the user, and do not overwrite the latter.
const CUSTOM_CSS_MARKER: &str = "Cloufam: жидкое стекло";

/// Lay out the Cloufam theme over `power_user` of the ST settings. The theme
/// name alone is not enough: at start ST does not call `applyTheme`, but reads values directly from
/// `power_user` (`public/scripts/power-user.js`), so the fields must be copied.
/// The theme's `name` key corresponds to the `theme` field. Returns true if something
/// changed.
fn merge_theme_into_power_user(power_obj: &mut serde_json::Map<String, serde_json::Value>) -> bool {
    let Ok(theme) = serde_json::from_str::<serde_json::Value>(CLOUFAM_THEME_JSON) else {
        return false;
    };
    let Some(theme_obj) = theme.as_object() else {
        return false;
    };
    let mut changed = false;
    for (key, value) in theme_obj {
        let target = if key == "name" { "theme" } else { key.as_str() };
        // A foreign custom_css is the user's manual work, we do not overwrite it.
        // We recognize ours by the marker in the first line of the theme file.
        // The Moonlit Echoes extension overwrites
        // power_user.custom_css with its preset during initialization — this is not a manual edit.
        if target == "custom_css" {
            let current = power_obj.get("custom_css").and_then(|v| v.as_str()).unwrap_or_default();
            if !current.trim().is_empty()
                && !current.contains(CUSTOM_CSS_MARKER)
                && !current.contains("Moonlit Echoes")
            {
                continue;
            }
        }
        if power_obj.get(target) != Some(value) {
            power_obj.insert(target.to_string(), value.clone());
            changed = true;
        }
    }
    changed
}

/// Install and synchronize the Cloufam theme in the SillyTavern themes directory.
pub fn seed_theme() -> Result<(), String> {
    let themes_dirs = [
        st_dir().join("default").join("content").join("themes"),
        st_dir().join("data").join("default-user").join("themes"),
    ];
    // The file `Cloufam.json` is the product theme, and the name ST substitutes
    // on "Save theme" is the same; edits to this file are overwritten by the version scheme
    // anyway on every THEME_VERSION bump, so without a marker
    // (the installation is older than the scheme) we lay the theme out: otherwise the disk kept
    // the old palette while the marker claimed the theme was current.
    let marker = crate::paths::data_dir().join(".lk-theme-version");
    let applied: Option<u32> = std::fs::read_to_string(&marker)
        .ok()
        .and_then(|text| text.trim().parse().ok());
    let deploy = applied.map_or(true, |version| version < THEME_VERSION);
    for dir in themes_dirs {
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let theme_path = dir.join("Cloufam.json");
        let fresh = !theme_path.exists();
        if deploy || fresh {
            write_if_changed(&theme_path, CLOUFAM_THEME_JSON)?;
            // We put the author's originals next to them untouched: our theme was derived from them,
            // and the user must be able to return to clean Moonlit.
            for (rel, content) in MOONLIT_FILES {
                if let Some(name) = rel.strip_prefix("theme/") {
                    if name.ends_with(".json") {
                        write_if_changed(&dir.join(name), content)?;
                    }
                }
            }
        }
    }
    // The background file is always needed: the user could have deleted it from the gallery while
    // settings.json still references it.
    let bg_dir = st_dir().join("data").join("default-user").join("backgrounds");
    std::fs::create_dir_all(&bg_dir).map_err(|e| e.to_string())?;
    let bg_path = bg_dir.join(CHAT_BACKGROUND_NAME);
    if std::fs::metadata(&bg_path).map(|m| m.len() != CHAT_BACKGROUND.len() as u64).unwrap_or(true) {
        std::fs::write(&bg_path, CHAT_BACKGROUND).map_err(|e| format!("фон чата: {e}"))?;
    }
    if deploy {
        let _ = std::fs::write(&marker, THEME_VERSION.to_string());
    }
    Ok(())
}

/// Set the default response strategy in group rooms: LIST (round-robin).
pub fn seed_groups_activation_strategy() -> Result<(), String> {
    let groups_dirs = [
        st_dir().join("data").join("default-user").join("groups"),
        st_dir().join("default").join("content").join("groups"),
    ];
    for groups_dir in groups_dirs {
        if !groups_dir.exists() {
            continue;
        }
        let entries = match std::fs::read_dir(&groups_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else { continue; };
            let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&content) else { continue; };
            let Some(obj) = json.as_object_mut() else { continue; };
            // 0 (NATURAL) is a valid user choice, not "not set".
            // We set LIST only when the key is completely absent.
            if obj.get("activation_strategy").is_none() {
                obj.insert("activation_strategy".into(), serde_json::json!(1));
                if let Ok(new_content) = serde_json::to_string_pretty(&json) {
                    write_if_changed(&path, &new_content)?;
                }
            }
        }
    }
    Ok(())
}

/// Set Russian as the default SillyTavern UI language.
/// 1. Injects the `localStorage.language = 'ru-ru'` initialization into the head of public/index.html,
///    so that the key exists before i18n.js loads.
/// 2. Patches the default fallback from 'en' to 'ru-ru' in public/scripts/i18n.js.
/// Our patch of the ST Russian locale: strings that upstream does not have or that
/// sit there as a "key = English text" stub.
const RU_LOCALE_PATCH: &str = include_str!("../locale/ru-ru-extra.json");

/// Append the missing translations to `public/locales/ru-ru.json`.
/// Append, not replace the file: upstream edits its locale, and overwriting
/// would roll back their fixes. Our key is set only if it is completely absent
/// or holds an English stub — the user's manual translation survives.
pub fn merge_ru_locale_patch() -> Result<(), String> {
    let path = st_dir().join("public").join("locales").join("ru-ru.json");
    if !path.exists() {
        return Ok(());
    }
    let Ok(text) = std::fs::read_to_string(&path) else { return Ok(()) };
    let Ok(mut locale) = serde_json::from_str::<serde_json::Value>(&text) else { return Ok(()) };
    let Some(obj) = locale.as_object_mut() else { return Ok(()) };
    let Ok(patch) = serde_json::from_str::<serde_json::Value>(RU_LOCALE_PATCH) else { return Ok(()) };
    let Some(patch_obj) = patch.as_object() else { return Ok(()) };

    let mut changed = false;
    for (key, value) in patch_obj {
        let stale = match obj.get(key) {
            None => true,
            Some(serde_json::Value::String(cur)) => cur == key || cur.trim().is_empty(),
            Some(_) => false,
        };
        if stale && obj.get(key) != Some(value) {
            obj.insert(key.clone(), value.clone());
            changed = true;
        }
    }
    if changed {
        let serialized = serde_json::to_string_pretty(&locale).map_err(|e| e.to_string())?;
        write_if_changed(&path, &serialized)?;
    }
    Ok(())
}

const RU_LANGUAGE_HEAD_SCRIPT: &str =
    "<script>try{if(!localStorage.getItem('language')){localStorage.setItem('language','ru-ru');}}catch(e){}</script>";

/// Lay out the ST Russian locale: the auto language of the UI in `index.html` and
/// the forced locale fallback in `i18n.js`. The insertion sites are not found in this ST version —
/// an error with the ST version (like the neighboring patches): a silent skip disabled
/// the locale without a single line in boot.log.
pub fn seed_russian_locale() -> Result<(), String> {
    // The ST locale covers the UI only partially — we append what is missing.
    let _ = merge_ru_locale_patch();
    let st = st_dir();
    let index_html = st.join("public").join("index.html");
    let content = std::fs::read_to_string(&index_html).map_err(|e| format!("index.html: {e}"))?;
    let marker = "localStorage.setItem('language','ru-ru')";
    if !content.contains(marker) {
        if !content.contains("<head>") {
            return Err(format!(
                "русская локаль: место вставки <head> в index.html ST {} не найдено",
                installed_st_version()
            ));
        }
        let patched = content.replacen("<head>", &format!("<head>\n    {RU_LANGUAGE_HEAD_SCRIPT}"), 1);
        write_if_changed(&index_html, &patched)?;
    }

    let i18n_js = st.join("public").join("scripts").join("i18n.js");
    let content = std::fs::read_to_string(&i18n_js).map_err(|e| format!("i18n.js: {e}"))?;
    let target = "const localeFile = String(overrideLanguage || navigator.language || navigator.userLanguage || 'en').toLowerCase();";
    let bad_patch = "const localeFile = String(overrideLanguage || 'ru-ru').toLowerCase();";
    let replacement = "const localeFile = String(overrideLanguage || navigator.language || navigator.userLanguage || 'ru-ru').toLowerCase();";
    if content.contains(bad_patch) {
        write_if_changed(&i18n_js, &content.replace(bad_patch, replacement))?;
    } else if content.contains(target) {
        write_if_changed(&i18n_js, &content.replace(target, replacement))?;
    } else if !content.contains(replacement) {
        // Neither the target nor our replacement: the insertion site drifted in this ST version.
        return Err(format!(
            "русская локаль: место вставки в i18n.js ST {} не найдено",
            installed_st_version()
        ));
    }
    Ok(())
}

/// Markers of the Cloufam UI block in `public/index.html`: the legacy inject (without markers)
/// is recognized by `id="cloufam-settings-injected"` and cut out, otherwise after an
/// ST update the page would keep the previous floating button.
const UI_BLOCK_START: &str = "<!-- cloufam-ui:start -->";
const UI_BLOCK_END: &str = "<!-- cloufam-ui:end -->";
const UI_BLOCK_LEGACY: &str = "id=\"cloufam-settings-injected\"";

/// Cut out the legacy Cloufam settings inject (`<script id="cloufam-settings-injected">…</script>`).
fn remove_legacy_cloufam_script(html: &str) -> String {
    let Some(marker_pos) = html.find(UI_BLOCK_LEGACY) else { return html.to_string(); };
    let Some(open) = html[..marker_pos].rfind("<script") else { return html.to_string(); };
    let Some(close_rel) = html[marker_pos..].find("</script>") else { return html.to_string(); };
    let close = marker_pos + close_rel + "</script>".len();
    format!("{}{}", &html[..open], &html[close..])
}

/// Build the page HTML with the UI block: replace the marked block, cut out the
/// legacy inject or insert a new one before `</body>`. `None` — the content is already
/// in place, no rewrite is needed.
fn patch_cloufam_ui_block(html: &str, block: &str) -> Option<String> {
    let marked = format!("{UI_BLOCK_START}\n{block}\n{UI_BLOCK_END}");
    if html.contains(&marked) {
        return None;
    }
    let without_marked = match (html.find(UI_BLOCK_START), html.rfind(UI_BLOCK_END)) {
        (Some(start), Some(end)) if end >= start => {
            format!("{}{}", &html[..start], &html[end + UI_BLOCK_END.len()..])
        }
        _ => html.to_string(),
    };
    let base = remove_legacy_cloufam_script(&without_marked);
    Some(if base.contains("</body>") {
        base.replacen("</body>", &format!("{marked}\n</body>"), 1)
    } else {
        format!("{base}\n{marked}")
    })
}

const CLOUFAM_THEME_CSS: &str = r#"<style id="cloufam-theme-style">
/* Character thoughts: the third category next to *action* (italic from ST itself)
   and "speech". The parsing itself is done by the regex script «Cloufam — мысли (~текст~)»
   in the ST settings: it wraps ~text~ in this class for display only,
   while in the message and in the prompt the tildes stay as is. The script is
   on by default; it is turned off in Regex Presets.
   The color is warm: action in this theme is pale-blue italic, and a lilac thought
   next to it did not read as a separate category.
   The selector is `.custom-lk-thought`, not `.lk-thought`: ST runs the message
   body through DOMPurify and, with the `uponSanitizeAttribute` hook (public/scripts/chats.js),
   renames EVERY class to `custom-<name>`, except `fa-*`, `note-*` and
   `monospace`. Against `.lk-thought` the rule would simply never match. */
.lk-thought,
.custom-lk-thought {
    color: #f9a8d4;
    /* Upright font and a light background: action in this theme is italic, and a thought
       differed from it only by its hue. Owner decision 31.10.2026. */
    font-style: normal;
    background: rgba(249, 168, 212, 0.12);
    border-radius: 4px;
    padding: 0 2px;
    opacity: 0.95;
}

/* Lorebook entry header: the controls (Position/Depth/Order/Probability) hold a
   min-content of ~323px and pushed the title down to 59px (one letter per line). We move
   the controls to a second row when they do not fit next to the title, and do not let
   the title shrink below readable - on wide panels this does not change the look. */
.world_entry_thin_controls {
    flex-wrap: wrap;
}

.WIEntryTitleAndStatus {
    min-width: 220px;
}

/* Cloufam settings item in the extensions menu (the magic wand) */
#cloufam_wand_container {
    padding: 3px 0;
    margin-bottom: 4px;
    border-bottom: 1px solid rgba(255, 255, 255, 0.1);
}

#cloufam-settings-menu-item {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 8px 12px;
    border-radius: 8px;
    color: inherit;
    font-weight: 500;
    cursor: pointer;
    transition: all 0.2s ease;
}

#cloufam-settings-menu-item:hover {
    background: rgba(255, 255, 255, 0.08);
}

#cloufam-settings-menu-item .extensionsMenuExtensionButton {
    filter: drop-shadow(0 0 6px rgba(96, 165, 250, 0.5));
}

/* Floating Cloufam update top bar (Syncwatch style) */
#cloufam-update-bar {
    position: fixed;
    top: 10px;
    left: 50%;
    transform: translateX(-50%);
    z-index: 999999;
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 6px 14px;
    background: rgba(15, 23, 42, 0.94);
    backdrop-filter: blur(14px);
    border: 1px solid rgba(96, 165, 250, 0.35);
    border-radius: 999px;
    box-shadow: 0 4px 24px rgba(0, 0, 0, 0.5), 0 0 16px rgba(59, 130, 246, 0.22);
    color: #f1f5f9;
    font-size: 13px;
    font-weight: 500;
    pointer-events: auto;
    transition: opacity 0.2s ease, transform 0.2s ease;
}
#cloufam-update-bar.lk-hidden {
    display: none !important;
}
#cloufam-update-bar .lk-badge {
    background: rgba(59, 130, 246, 0.2);
    border: 1px solid rgba(96, 165, 250, 0.4);
    color: #93c5fd;
    padding: 2px 8px;
    border-radius: 999px;
    font-size: 11px;
    font-weight: 600;
    letter-spacing: 0.02em;
    text-transform: uppercase;
}
#cloufam-update-bar .lk-title {
    color: #e2e8f0;
}
#cloufam-update-bar .lk-btn-primary {
    background: linear-gradient(135deg, #2563eb, #1d4ed8);
    color: #ffffff;
    border: 1px solid rgba(255, 255, 255, 0.2);
    padding: 4px 12px;
    border-radius: 999px;
    font-size: 12px;
    font-weight: 600;
    cursor: pointer;
    box-shadow: 0 2px 8px rgba(37, 99, 235, 0.35);
    transition: all 0.15s ease;
}
#cloufam-update-bar .lk-btn-primary:hover {
    background: linear-gradient(135deg, #3b82f6, #2563eb);
    transform: translateY(-1px);
    box-shadow: 0 3px 12px rgba(59, 130, 246, 0.45);
}
#cloufam-update-bar .lk-btn-primary:active {
    transform: translateY(0);
}
#cloufam-update-bar .lk-btn-primary:disabled {
    opacity: 0.6;
    cursor: not-allowed;
    transform: none;
}
#cloufam-update-bar .lk-btn-ghost {
    background: transparent;
    color: #94a3b8;
    border: 1px solid transparent;
    padding: 4px 10px;
    border-radius: 999px;
    font-size: 12px;
    cursor: pointer;
    transition: all 0.15s ease;
}
#cloufam-update-bar .lk-btn-ghost:hover {
    color: #f1f5f9;
    background: rgba(255, 255, 255, 0.1);
}
#cloufam-update-bar .lk-progress-track {
    width: 130px;
    height: 6px;
    background: rgba(255, 255, 255, 0.12);
    border-radius: 999px;
    overflow: hidden;
}
#cloufam-update-bar .lk-progress-fill {
    height: 100%;
    background: linear-gradient(90deg, #38bdf8, #3b82f6);
    border-radius: 999px;
    transition: width 0.2s ease;
}
</style>"#;

/// Resolution and generation steps for the checkpoint family: SD1.5 was trained on
/// 512 px, SDXL — on ~1 MP. Returns (width, height, steps).
/// The family is determined by a single criterion `image_trainer::family_for_checkpoint`
/// (the safetensors header from the warehouse, fallback — the name): the previous list of
/// name substrings was a second source of truth about the family, and an SD1.5 checkpoint with a name
/// outside the list got SDXL resolution — a raw/torn frame (V-23).
fn sd_target_size(img_model: &str) -> (u64, u64, u64) {
    match crate::image_trainer::family_for_checkpoint(img_model) {
        crate::image_trainer::ImageFamily::Sd15 => (512, 768, 20),
        crate::image_trainer::ImageFamily::Sdxl => (832, 1216, 28),
    }
}

/// Payload for `__lkApplySettings` on the open page.
fn client_settings_payload(settings: &crate::settings::Settings, comfy_installed: bool) -> serde_json::Value {
    // f32 goes to JSON as 0.8999999761581421, and ST saves it to
    // settings.json as is — we round the same way as sync_st_settings.
    let round2 = |v: f32| (v as f64 * 100.0).round() / 100.0;
    // The resolution is also written by sync_st_settings, but the open page keeps the previous one
    // in memory and the first save would return 832×1216 for SD1.5.
    let sd_size = (comfy_installed && !settings.image_model.is_empty()).then(|| {
        let (width, height, steps) = sd_target_size(&settings.image_model);
        serde_json::json!({ "width": width, "height": height, "steps": steps })
    });
    serde_json::json!({
        "chat_model": settings.chat_model,
        "embed_model": settings.embed_model,
        "vision_model": settings.vision_model,
        "image_model": settings.image_model,
        "temperature": round2(settings.temperature),
        "top_p": round2(settings.top_p),
        "context_size": settings.context_size,
        "max_tokens": settings.max_tokens,
        "memory_max_chunks": settings.memory_max_chunks,
        "auto_memory": settings.auto_memory,
        "memory_protect": memory_protect(settings.context_size),
        "sd_size": sd_size,
    })
}

/// Deliver the application settings to an already open SillyTavern page.
/// The client keeps the model and samplers in memory (`oai_settings`, `extension_settings`)
/// and saves them over the file, so writing to settings.json is not enough:
/// the model selected in the application otherwise did not reach generation until a restart.
pub fn push_settings_to_client(app: &tauri::AppHandle, settings: &crate::settings::Settings) {
    use tauri::Manager;
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let payload = client_settings_payload(settings, comfyui::is_installed());
    // On the launcher screen the functions are not defined — eval simply does nothing.
    // The payload above does not include the tag settings (enabled/model/max_tags/...) —
    // the auto-tagging module keeps them in its own cache `config`, loaded once
    // at page start; without an explicit re-request, the "Apply tags
    // automatically" toggle, turned off in the settings window, does not reach an already
    // open SillyTavern (see gotchas.md, "the tags toggle does not turn off").
    let _ = window.eval(format!(
        "window.__lkApplySettings && window.__lkApplySettings({payload});\
         window.__lkEnsureApiConnection && window.__lkEnsureApiConnection(0);\
         window.__lkReloadTaggerConfig && window.__lkReloadTaggerConfig();"
    ));
}

/// Ask the open SillyTavern page to apply the character traits: the extension
/// compares the saved traits with the cards and appends `depth_prompt`.
/// On the launcher screen the function does not exist — eval does nothing.
pub fn push_traits_to_client(app: &tauri::AppHandle) {
    use tauri::Manager;
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let _ = window.eval("window.__lkApplyTraits && window.__lkApplyTraits();");
}

/// Inject the author's styles and the "Cloufam Settings" item into the SillyTavern UI.
/// The block is versioned by markers: when the gateway port or the styles change, the content is overwritten,
/// and legacy injects from past versions are cut out.
pub fn seed_cloufam_ui(gateway_port: u16) -> Result<(), String> {
    let st = st_dir();
    let index_html = st.join("public").join("index.html");
    if !index_html.exists() {
        return Ok(());
    }
    let Ok(content) = std::fs::read_to_string(&index_html) else { return Ok(()); };
    let block = cloufam_ui_block(gateway_port);
    if let Some(patched) = patch_cloufam_ui_block(&content, &block) {
        write_if_changed(&index_html, &patched)?;
    }
    Ok(())
}

/// The Cloufam UI block exactly in the form in which it lands in `public/index.html`.
fn cloufam_ui_block(gateway_port: u16) -> String {
    format!(
        r#"{CLOUFAM_THEME_CSS}
<style id="cloufam-models-style">{MODELS_MENU_CSS}</style>
<script id="cloufam-settings-injected">
(function() {{
    // The models menu needs the gateway port too: it calls the app for the catalog.
    window.__LK_GATEWAY = 'http://127.0.0.1:{gateway_port}';
    var GLASS_CSS = {glass_json};
    var CONTAINER_ID = 'cloufam_wand_container';
    function inject() {{
        var menu = document.getElementById('extensionsMenu');
        if (!menu) return;
        var container = document.getElementById(CONTAINER_ID);
        if (!container) {{
            container = document.createElement('div');
            container.id = CONTAINER_ID;
            container.className = 'extension_container';
            menu.prepend(container);
        }}
        if (document.getElementById('cloufam-settings-menu-item')) return;
        var item = document.createElement('div');
        item.id = 'cloufam-settings-menu-item';
        item.className = 'interactable';
        item.title = 'Настройки Cloufam (модели, VRAM, память)';
        item.innerHTML = '<div class="fa-solid fa-gear extensionsMenuExtensionButton"></div><span>Настройки Cloufam</span>';
        item.onclick = function() {{
            fetch('http://127.0.0.1:{gateway_port}/open-settings', {{ method: 'POST', mode: 'no-cors' }}).catch(function(){{}});
        }};
        container.append(item);
    }}
    if (document.readyState === 'loading') {{
        document.addEventListener('DOMContentLoaded', inject);
    }} else {{
        inject();
    }}
    setInterval(inject, 2000);

    // Applying the app settings to the live page: the ST client keeps them in
    // memory, so switching the model in the Cloufam window had no effect
    // on generation without this step (and was overwritten by the first settings save inside ST).
    function setField(id, value) {{
        var el = document.getElementById(id);
        if (!el) return;
        el.value = String(value);
        el.dispatchEvent(new Event('input', {{ bubbles: true }}));
    }}
    function setCheck(id, value) {{
        var el = document.getElementById(id);
        if (!el) return;
        el.checked = !!value;
        el.dispatchEvent(new Event('input', {{ bubbles: true }}));
    }}
    window.__lkApplySettings = function(s) {{
        if (!s) return;
        (async function() {{
            try {{
                if (s.chat_model) setField('custom_model_id', s.chat_model);
                if (typeof s.temperature === 'number') setField('temp_openai', s.temperature);
                if (typeof s.top_p === 'number') setField('top_p_openai', s.top_p);
                if (s.context_size) setField('openai_max_context', s.context_size);
                if (s.max_tokens) setField('openai_max_tokens', s.max_tokens);

                var oai = (await import('/scripts/openai.js')).oai_settings;
                if (s.chat_model) {{
                    oai.custom_model = s.chat_model;
                    oai.openai_model = s.chat_model;
                }}
                if (typeof s.temperature === 'number') oai.temp_openai = s.temperature;
                if (typeof s.top_p === 'number') oai.top_p_openai = s.top_p;
                if (s.context_size) oai.openai_max_context = s.context_size;
                if (s.max_tokens) oai.openai_max_tokens = s.max_tokens;

                var ext = (await import('/scripts/extensions.js')).extension_settings;
                // The vectors module works on its own copy of the settings and on any
                // panel action copies it back into extension_settings.
                // That is why the values also go through its input fields: the handler
                // updates the copy; direct assignment is needed when the panel does not exist yet.
                if (ext.vectors && s.embed_model) {{
                    ext.vectors.ollama_model = s.embed_model;
                    setField('vectors_ollama_model', s.embed_model);
                }}
                if (ext.vectors && typeof s.memory_max_chunks === 'number') {{
                    ext.vectors.insert = s.memory_max_chunks;
                    setField('vectors_insert', s.memory_max_chunks);
                }}
                if (ext.vectors && typeof s.auto_memory === 'boolean') {{
                    ext.vectors.enabled_chats = s.auto_memory;
                    setCheck('vectors_enabled_chats', s.auto_memory);
                }}
                // protect lives in the page memory. A write to settings.json
                // was overwritten by the first saveSettingsDebounced with the old value,
                // and the context change never reached the vector memory.
                if (ext.vectors && typeof s.memory_protect === 'number') {{
                    var curProtect = ext.vectors.protect;
                    var markedProtect = ext.vectors.lk_protect;
                    var protectIsDefault = curProtect == null || curProtect === 5;
                    var protectIsOurs = typeof markedProtect === 'number' && curProtect === markedProtect;
                    if (protectIsDefault || protectIsOurs) {{
                        ext.vectors.protect = s.memory_protect;
                    }}
                    ext.vectors.lk_protect = s.memory_protect;
                }}
                if (ext.caption && s.vision_model) ext.caption.ollama_custom_model = s.vision_model;
                // The image model is the user's choice if they made it in ST.
                // We overwrite only our own (marker lk_sd_ours) or an empty one -
                // the same V-29 policy as in sync_sd_settings.
                if (ext.sd && s.image_model) {{
                    var ours = ext.sd.lk_sd_ours || {{}};
                    var cur = ext.sd.model;
                    if (!cur || cur === s.image_model || ours.model === cur) {{
                        ext.sd.model = s.image_model;
                        if (s.sd_size) {{
                            ext.sd.width = s.sd_size.width;
                            ext.sd.height = s.sd_size.height;
                            ext.sd.steps = s.sd_size.steps;
                            setField('sd_width', s.sd_size.width);
                            setField('sd_height', s.sd_size.height);
                            setField('sd_steps', s.sd_size.steps);
                        }}
                        ours.model = s.image_model;
                        if (s.sd_size) {{
                            ours.width = s.sd_size.width;
                            ours.height = s.sd_size.height;
                            ours.steps = s.sd_size.steps;
                        }}
                        ext.sd.lk_sd_ours = ours;
                    }}
                }}

                // saveSettingsDebounced writes in-memory values over settings.json:
                // without syncing memory_max_chunks/auto_memory they would roll back
                // what sync_st_settings has just written.
                var script = await import('/script.js');
                if (script.saveSettingsDebounced) script.saveSettingsDebounced();
            }} catch (e) {{
                console.warn('Cloufam: настройки не применены', e);
            }}
        }})();
    }};

    // Liquid Glass: the Moonlit Echoes extension loads after our injection
    // and overwrites custom_css - so we add the glass as a DOM tag from JS
    // *after* the extension. A <style> tag in <head> sits below Moonlit styles and
    // wins by position; with equal specificity the later one wins.
    setTimeout(function() {{
        var el = document.getElementById('cloufam-glass');
        if (!el) {{
            el = document.createElement('style');
            el.id = 'cloufam-glass';
            document.head.appendChild(el);
        }}
        el.textContent = GLASS_CSS;
    }}, 3000);

    // Connecting to the Cloufam gateway. The ST client does not reach the source on
    // page load: the field «Доступные модели» stays at «None», the status is
    // «Нет соединения», and the chat does not work until the user manually
    // re-selects the source. We do the same for them - we click «Подключиться»
    // (the same handler the source re-selection uses).
    var GATEWAY_URL = 'http://127.0.0.1:{gateway_port}/v1';
    var CONNECT_ATTEMPTS = 6;

    function apiHasModels() {{
        var select = document.getElementById('model_custom_select');
        if (!select) return false;
        return Array.prototype.some.call(select.options, function (option) {{ return !!option.value; }});
    }}

    window.__lkEnsureApiConnection = function(attempt) {{
        attempt = attempt || 0;
        (async function() {{
            try {{
                var oai = (await import('/scripts/openai.js')).oai_settings;
                var dirty = false;
                if (oai.chat_completion_source !== 'custom') {{ oai.chat_completion_source = 'custom'; dirty = true; }}
                if (oai.custom_url !== GATEWAY_URL) {{ oai.custom_url = GATEWAY_URL; dirty = true; }}
                var urlField = document.getElementById('custom_api_url_text');
                if (urlField && urlField.value !== GATEWAY_URL) urlField.value = GATEWAY_URL;
                var sourceSelect = document.getElementById('chat_completion_source');
                if (sourceSelect && sourceSelect.value !== 'custom') sourceSelect.value = 'custom';
                if (dirty) {{
                    var script = await import('/script.js');
                    if (script.saveSettingsDebounced) script.saveSettingsDebounced();
                }}
                if (apiHasModels() || attempt >= CONNECT_ATTEMPTS) return;
                var button = document.getElementById('api_button_openai');
                if (button) button.click();
                setTimeout(function() {{ window.__lkEnsureApiConnection(attempt + 1); }}, 1500);
            }} catch (e) {{
                console.warn('Cloufam: подключение к шлюзу не настроено', e);
            }}
        }})();
    }};
    setTimeout(function() {{ window.__lkEnsureApiConnection(0); }}, 1000);

    // Floating Cloufam update top bar (Syncwatch style)
    var BAR_ID = 'cloufam-update-bar';
    var DEFERRED_KEY = 'cloufam_deferred_update_ver';

    function esc(s) {{ return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;'); }}

    function initUpdateBar() {{
        if (document.getElementById(BAR_ID)) return;
        var bar = document.createElement('div');
        bar.id = BAR_ID;
        bar.className = 'lk-hidden';
        document.body.prepend(bar);

        var isUpdating = false;
        var progressTimer = null;

        async function pollProgress() {{
            try {{
                var res = await fetch('http://127.0.0.1:{gateway_port}/api/cloufam/update-progress');
                if (!res.ok) return;
                var p = await res.json();
                if (!p.updating && !p.done && !p.error && isUpdating) {{
                    // The update never started (apply-update failed before writing progress):
                    // otherwise the bar stuck at «Подготовка…» forever.
                    if (progressTimer) clearInterval(progressTimer);
                    isUpdating = false;
                    bar.innerHTML = '<span class="lk-badge" style="border-color:#f87171;color:#f87171;">Ошибка</span>' +
                        '<span class="lk-title" style="color:#fca5a5;">Обновление не запустилось</span>' +
                        '<button class="lk-btn-ghost" id="lk-bar-close">✕</button>';
                    var eBtn = document.getElementById('lk-bar-close');
                    if (eBtn) eBtn.onclick = function() {{ bar.className = 'lk-hidden'; }};
                    return;
                }}
                if (p.updating) {{
                    isUpdating = true;
                    var mbGot = (p.downloaded_bytes / 1048576).toFixed(1);
                    var mbTotal = (p.total_bytes / 1048576).toFixed(1);
                    var text = p.stage || 'Загрузка…';
                    if (p.total_bytes > 0) {{
                        text = 'Загрузка ' + p.percent + '% (' + mbGot + ' / ' + mbTotal + ' МБ)';
                    }}
                    bar.innerHTML = '<span class="lk-badge">Обновление</span>' +
                        '<span class="lk-title">⏳ ' + esc(text) + '</span>' +
                        '<div class="lk-progress-track"><div class="lk-progress-fill" style="width:' + p.percent + '%"></div></div>';
                    bar.className = '';
                }} else if (p.done) {{
                    if (progressTimer) clearInterval(progressTimer);
                    bar.innerHTML = '<span class="lk-badge">Обновление</span>' +
                        '<span class="lk-title">🚀 Обновление установлено! Перезапуск…</span>';
                    bar.className = '';
                }} else if (p.error) {{
                    if (progressTimer) clearInterval(progressTimer);
                    isUpdating = false;
                    bar.innerHTML = '<span class="lk-badge" style="border-color:#f87171;color:#f87171;">Ошибка</span>' +
                        '<span class="lk-title" style="color:#fca5a5;">' + esc(p.error) + '</span>' +
                        '<button class="lk-btn-ghost" id="lk-bar-close">✕</button>';
                    var cBtn = document.getElementById('lk-bar-close');
                    if (cBtn) cBtn.onclick = function() {{ bar.className = 'lk-hidden'; }};
                }}
            }} catch (e) {{}}
        }}

        async function checkStatus() {{
            if (isUpdating) return;
            try {{
                var res = await fetch('http://127.0.0.1:{gateway_port}/api/cloufam/update-status');
                if (!res.ok) return;
                var data = await res.json();
                if (data.available && data.latest_version) {{
                    var deferred = sessionStorage.getItem(DEFERRED_KEY);
                    if (deferred === data.latest_version) return;

                    bar.innerHTML = '<span class="lk-badge">Обновление</span>' +
                        '<span class="lk-title">🚀 Доступно обновление <b>v' + esc(data.latest_version) + '</b></span>' +
                        '<button class="lk-btn-primary" id="lk-bar-update-btn">Обновить</button>' +
                        '<button class="lk-btn-ghost" id="lk-bar-defer-btn">Отложить</button>' +
                        '<button class="lk-btn-ghost" id="lk-bar-versions-btn" title="Выбрать версию">Все версии ▾</button>';
                    bar.className = '';

                    var uBtn = document.getElementById('lk-bar-update-btn');
                    if (uBtn) {{
                        uBtn.onclick = async function() {{
                            uBtn.disabled = true;
                            isUpdating = true;
                            bar.innerHTML = '<span class="lk-badge">Обновление</span>' +
                                '<span class="lk-title">⏳ Подготовка к обновлению…</span>';
                            try {{
                                var applyRes = await fetch('http://127.0.0.1:{gateway_port}/api/cloufam/apply-update', {{ method: 'POST' }});
                                // 409 - the update is already running (e.g. from the settings window): show its progress.
                                if (!applyRes.ok && applyRes.status !== 409) throw new Error('HTTP ' + applyRes.status);
                                if (progressTimer) clearInterval(progressTimer);
                                progressTimer = setInterval(pollProgress, 400);
                            }} catch (err) {{
                                isUpdating = false;
                                bar.innerHTML = '<span class="lk-badge" style="border-color:#f87171;color:#f87171;">Ошибка</span>' +
                                    '<span class="lk-title" style="color:#fca5a5;">Не удалось запустить обновление</span>' +
                                    '<button class="lk-btn-ghost" id="lk-bar-close">✕</button>';
                                var cb = document.getElementById('lk-bar-close');
                                if (cb) cb.onclick = function() {{ bar.className = 'lk-hidden'; }};
                            }}
                        }};
                    }}
                    var dBtn = document.getElementById('lk-bar-defer-btn');
                    if (dBtn) {{
                        dBtn.onclick = function() {{
                            sessionStorage.setItem(DEFERRED_KEY, data.latest_version);
                            bar.className = 'lk-hidden';
                        }};
                    }}
                    var vBtn = document.getElementById('lk-bar-versions-btn');
                    if (vBtn) {{
                        vBtn.onclick = function() {{ openVersionsModal(); }};
                    }}
                }} else {{
                    if (!isUpdating) bar.className = 'lk-hidden';
                }}
            }} catch (e) {{}}
        }}

        setTimeout(checkStatus, 3000);
        setInterval(checkStatus, 10 * 60 * 1000);
    }}

    // Version picker modal for rollback
    function openVersionsModal() {{
        var modalId = 'lk-versions-modal';
        if (document.getElementById(modalId)) return;

        var modal = document.createElement('div');
        modal.id = modalId;
        modal.style.cssText = 'position:fixed;inset:0;z-index:100001;display:flex;align-items:center;justify-content:center;';
        modal.innerHTML = '<div style="position:absolute;inset:0;background:rgba(0,0,0,0.6);backdrop-filter:blur(4px);" id="lk-ver-backdrop"></div>' +
            '<div style="position:relative;max-width:500px;width:90%;max-height:70vh;overflow-y:auto;' +
            'background:rgba(18,24,52,0.92);backdrop-filter:blur(14px);border-radius:18px;' +
            'box-shadow:inset 0 1px 0 rgba(255,255,255,0.28),0 16px 38px rgba(3,6,22,0.42);' +
            'padding:24px;font-family:inherit;color:#e7eaf3;">' +
            '<div style="display:flex;justify-content:space-between;align-items:center;margin-bottom:16px;">' +
            '<h3 style="margin:0;font-size:16px;">📦 Доступные версии</h3>' +
            '<button id="lk-ver-close" style="background:none;border:none;color:#8b93a8;font-size:18px;cursor:pointer;">✕</button>' +
            '</div>' +
            '<div id="lk-ver-list" style="color:#8b93a8;text-align:center;padding:20px;">Загрузка…</div>' +
            '</div>';
        document.body.appendChild(modal);

        document.getElementById('lk-ver-backdrop').onclick = function() {{ modal.remove(); }};
        document.getElementById('lk-ver-close').onclick = function() {{ modal.remove(); }};

        fetch('http://127.0.0.1:{gateway_port}/api/cloufam/versions')
            .then(function(r) {{ return r.json(); }})
            .then(function(data) {{
                var list = document.getElementById('lk-ver-list');
                if (!list) return;
                if (data.error) {{
                    list.innerHTML = '<p>Список версий не получен: ' + esc(data.error) + '</p>';
                    return;
                }}
                if (!data.versions || data.versions.length === 0) {{
                    list.innerHTML = '<p>Нет доступных версий</p>';
                    return;
                }}
                var html = '';
                for (var i = 0; i < data.versions.length; i++) {{
                    var v = data.versions[i];
                    var isCurrent = v.is_current;
                    var sizeMb = v.exe_size ? (v.exe_size / 1048576).toFixed(1) + ' МБ' : '';
                    var dateStr = v.published ? new Date(v.published).toLocaleDateString('ru-RU') : '';
                    html += '<div style="padding:12px;margin-bottom:8px;border-radius:12px;' +
                        'background:' + (isCurrent ? 'rgba(59,130,246,0.15)' : 'rgba(140,175,255,0.08)') + ';' +
                        'box-shadow:inset 0 1px 0 rgba(255,255,255,' + (isCurrent ? '0.25' : '0.12') + ');' +
                        'display:flex;justify-content:space-between;align-items:center;">' +
                        '<div>' +
                        '<b style="color:#e7eaf3;">v' + esc(v.version) + '</b>' +
                        (isCurrent ? ' <span style="color:#60a5fa;font-size:12px;">(текущая)</span>' : '') +
                        '<br><span style="font-size:12px;color:#8b93a8;">' + esc(dateStr) + (sizeMb ? ' · ' + esc(sizeMb) : '') + '</span>' +
                        (v.notes ? '<br><span style="font-size:12px;color:#94a3b8;">' + esc(v.notes.substring(0, 120)) + (v.notes.length > 120 ? '…' : '') + '</span>' : '') +
                        '</div>' +
                        (isCurrent ? '' : '<button class="lk-ver-install-btn" data-version="' + esc(v.version) + '" data-url="' + esc(v.exe_url || '') + '" ' +
                            'style="padding:6px 14px;border-radius:8px;border:1px solid rgba(59,130,246,0.4);background:rgba(59,130,246,0.15);color:#93c5fd;cursor:pointer;font-size:13px;">Установить</button>') +
                        '</div>';
                }}
                list.innerHTML = html;

                // Install button handlers
                var btns = list.querySelectorAll('.lk-ver-install-btn');
                for (var j = 0; j < btns.length; j++) {{
                    btns[j].onclick = function() {{
                        var ver = this.getAttribute('data-version');
                        var url = this.getAttribute('data-url');
                        // The rollback installs the binary from the release latest.json, not the installer
                        // from the list: a missing setup.exe is no reason to refuse - if
                        // there is no manifest, the gateway answers with an error and it is shown below.
                        if (!confirm('Установить v' + ver + '? Текущая версия будет заменена.')) return;
                        var btn = this;
                        btn.disabled = true;
                        btn.textContent = 'Загрузка…';
                        fetch('http://127.0.0.1:{gateway_port}/api/cloufam/versions/rollback', {{
                            method: 'POST',
                            headers: {{ 'Content-Type': 'application/json' }},
                            body: JSON.stringify({{ version: ver, exe_url: url }})
                        }}).then(function(r) {{
                            if (!r.ok) {{ r.text().then(function(t) {{ alert('Ошибка: ' + t); }}); }}
                            // On ok the app restarts itself
                        }}).catch(function(e) {{ alert('Ошибка: ' + e); }}).finally(function() {{
                            // The button must not stick at «Загрузка…» on failure.
                            btn.disabled = false;
                            btn.textContent = 'Установить';
                        }});
                    }};
                }}
            }})
            .catch(function(e) {{
                var list = document.getElementById('lk-ver-list');
                if (list) list.innerHTML = '<p style="color:#f87171;">Ошибка загрузки: ' + esc(e) + '</p>';
            }});
    }}

    if (document.readyState === 'loading') {{
        document.addEventListener('DOMContentLoaded', initUpdateBar);
    }} else {{
        initUpdateBar();
    }}
}})();
</script>
<script id="cloufam-models-injected">
{MODELS_MENU_JS}
</script>"#,
        glass_json = serde_json::to_string(GLASS_CSS).unwrap(),
    )
}

/// Patch of the Stable Diffusion extension to support Russian-language triggers ("скинь фото", "нарисуй себя", "селфи").
pub fn patch_sd_russian_triggers() -> Result<(), String> {
    let sd_index = st_dir()
        .join("public")
        .join("scripts")
        .join("extensions")
        .join("stable-diffusion")
        .join("index.js");
    if !sd_index.exists() {
        return Ok(());
    }
    let content = std::fs::read_to_string(&sd_index)
        .map_err(|e| format!("SD index.js не прочитан: {e}"))?;
    if content.contains(SD_ACTIVATION_REGEX_RU) {
        return Ok(());
    }
    // A silent skip disabled the Russian triggers without a single line in
    // boot.log: after an ST update the anchor could drift.
    let Some(patched) = patch_sd_activation_regex(&content) else {
        return Err(format!(
            "не найден якорь activationRegex (SillyTavern {})",
            installed_st_version()
        ));
    };
    write_if_changed(&sd_index, &patched)?;
    Ok(())
}

const SD_ACTIVATION_REGEX_ST: &str = r#"activationRegex: /\b(send|mail|imagine|generate|make|create|draw|paint|render|show)\b.{0,10}\b(pic|picture|image|drawing|painting|photo|photograph)\b(?:\s+of)?(?:\s+(?:a|an|the|this|that|those|your)?\s+)?(.+)/i,"#;
/// The first version of the patch (up to 1.2.52 inclusive) — it is already present in installed
/// copies: in the second alternative subject slipped into group 5, while ST takes
/// `activationMatch[3].trim()`, and "нарисуй X" failed silently inside try.
const SD_ACTIVATION_REGEX_V1: &str = r#"activationRegex: /(?:(send|mail|imagine|generate|make|create|draw|paint|render|show|скинь|скинуть|отправь|отправить|пришли|прислать|покажи|показать|нарисуй|нарисовать|сгенерируй|сгенерировать|создай|создать|изобрази|изобразить).{0,12}(pic|picture|image|drawing|painting|photo|photograph|selfie|картинку|картинка|картинки|фото|фотку|фотка|изображение|рисунок|арт|селфи)(?:\s+(?:of|a|an|the|this|that|those|your|из|про|для|с|где|в))?\s*(.+)?|(нарисуй|сгенерируй|изобрази)\s+(.+))/i,"#;
/// 3 groups, as in the original: 1 — verb, 2 — object, 3 — subject.
/// Subject is moved outside the alternative, otherwise in the second branch it slipped into group 5
/// and the ST script took emptiness. Word boundaries — lookaround over [a-zа-яё]:
/// `\b` in JS is ASCII only, and without boundaries "pic" was found inside "picture",
/// "арт" — inside "картинку", the preposition "с" — at the start of "себя".
/// The previous Russian regex (25.09.2026): without "фотографию", "фоточку",
/// "картину", "снимок". Installations where it is already set get the new one.
const SD_ACTIVATION_REGEX_V2: &str = r#"activationRegex: /(?:(?<![a-zа-яё])(send|mail|imagine|generate|make|create|draw|paint|render|show|скинь|скинуть|отправь|отправить|пришли|прислать|покажи|показать|нарисуй|нарисовать|сгенерируй|сгенерировать|создай|создать|изобрази|изобразить)(?![a-zа-яё]).{0,12}(?<![a-zа-яё])(pic|picture|image|drawing|painting|photo|photograph|selfie|картинку|картинка|картинки|фото|фотку|фотка|изображение|рисунок|арт|селфи)(?![a-zа-яё])(?:\s+(?:of|a|an|the|this|that|those|your|из|про|для|с|где|в)(?![a-zа-яё])){0,2}\s*|(?<![a-zа-яё])(?:нарисуй|сгенерируй|изобрази)\s+)(.+)/i,"#;

const SD_ACTIVATION_REGEX_RU: &str = r#"activationRegex: /(?:(?<![a-zа-яё])(send|mail|imagine|generate|make|create|draw|paint|render|show|скинь|скинуть|отправь|отправить|пришли|прислать|покажи|показать|нарисуй|нарисовать|сгенерируй|сгенерировать|создай|создать|изобрази|изобразить)(?![a-zа-яё]).{0,12}(?<![a-zа-яё])(pic|picture|image|drawing|painting|photo|photograph|selfie|картинку|картинка|картинки|картин[уаы]|фото|фотку|фотка|фотографи[юяие]|фоточк[уаи]|снимок|снимки|изображение|рисунок|арт|селфи)(?![a-zа-яё])(?:\s+(?:of|a|an|the|this|that|those|your|из|про|для|с|где|в)(?![a-zа-яё])){0,2}\s*|(?<![a-zа-яё])(?:нарисуй|сгенерируй|изобрази)\s+)(.+)/i,"#;

/// The text of `stable-diffusion/index.js` with the current trigger regex:
/// both the upstream one and the previous version of our patch are replaced. `None` — the current one
/// is already in place or there is no place for the patch in this ST version.
fn patch_sd_activation_regex(content: &str) -> Option<String> {
    if content.contains(SD_ACTIVATION_REGEX_RU) {
        return None;
    }
    [SD_ACTIVATION_REGEX_ST, SD_ACTIVATION_REGEX_V1, SD_ACTIVATION_REGEX_V2]
        .into_iter()
        .find(|old| content.contains(old))
        .map(|old| content.replace(old, SD_ACTIVATION_REGEX_RU))
}

/// Apply the find → replace patch pair with diagnostics instead of silence:
/// the replacement is already in place — do nothing; the target is found — replace; otherwise —
/// an error with the ST version, like the neighboring patches (`patch_streaming_tail`,
/// `patch_world_info`). A silent skip on a line mismatch disabled
/// the feature without a single line in boot.log. Returns true if the text changed.
fn apply_anchor_patch(
    text: &mut String,
    what: &str,
    find: &str,
    replace: &str,
) -> Result<bool, String> {
    if text.contains(replace) {
        return Ok(false);
    }
    if text.contains(find) {
        *text = text.replace(find, replace);
        return Ok(true);
    }
    Err(format!("{what}: место вставки в ST {} не найдено", installed_st_version()))
}

/// SillyTavern background gallery patch: instant render, a reliable file selection trigger,
/// upload into the shared collection when there is no chat, and a button to go to backgrounds from an empty chat tab.
/// The insertion sites are not found in this ST version — an error with the ST version (otherwise
/// the feature is silently disabled, as it was with this patch and `seed_russian_locale`).
pub fn patch_backgrounds_fix() -> Result<(), String> {
    let public_dir = st_dir().join("public");
    let index_html = public_dir.join("index.html");
    let content = std::fs::read_to_string(&index_html).map_err(|e| format!("index.html: {e}"))?;
    let mut patched = content.clone();

    // 1. The background upload form turns into a bare input: the form itself in ST is
    // only a wrapper, and its `hidden` makes the file selection trigger unreliable.
    let bg_upload_input =
        "<input type=\"file\" id=\"add_bg_button\" name=\"avatar\" accept=\"image/*,video/*\" multiple class=\"displayNone\">";
    if !patched.contains(bg_upload_input) {
        let target = "<form id=\"form_bg_upload\" hidden>\n                    <input type=\"file\" id=\"add_bg_button\" name=\"avatar\" accept=\"image/*,video/*\" multiple hidden>\n                </form>";
        if patched.contains(target) {
            patched = patched.replace(target, bg_upload_input);
        } else if let Some(start) = patched.find("<form id=\"form_bg_upload\" hidden>") {
            // A form with different content (another ST version): we cut out exactly it up to
            // the paired </form> — the previous fallback removed all closing
            // form tags in the file — and leave the same bare input in its place.
            // Without it, #add_bg_button would disappear, which the
            // backgrounds.js patch relies on, and the next run would not tell its own
            // trace from a missing insertion site.
            if let Some(offset) = patched[start..].find("</form>") {
                let end = start + offset + "</form>".len();
                patched = format!("{}{}{}", &patched[..start], bg_upload_input, &patched[end..]);
            } else {
                return Err(format!(
                    "галерея фонов: у form_bg_upload в index.html ST {} нет парного </form>",
                    installed_st_version()
                ));
            }
        } else {
            return Err(format!(
                "галерея фонов: место вставки form_bg_upload в index.html ST {} не найдено",
                installed_st_version()
            ));
        }
    }

    // 2. The "Show all backgrounds" button in an empty chat tab.
    let bg_hint_target = "<div id=\"bg_chat_hint\" class=\"wide100p textAlignCenter\">\n                            <span data-i18n=\"bg_chat_hint_1\">Chat backgrounds generated with the</span> <code><i class=\"fa-solid fa-paintbrush\"></i>&nbsp;Image Generation</code> <span data-i18n=\"bg_chat_hint_2\">extension will appear here.</span>\n                        </div>";
    let bg_hint_replacement = "<div id=\"bg_chat_hint\" class=\"wide100p textAlignCenter\">\n                            <span data-i18n=\"bg_chat_hint_1\">Chat backgrounds generated with the</span> <code><i class=\"fa-solid fa-paintbrush\"></i>&nbsp;Image Generation</code> <span data-i18n=\"bg_chat_hint_2\">extension will appear here.</span>\n                            <div style=\"margin-top: 10px;\">\n                                <button id=\"bg_switch_to_global_btn\" type=\"button\" class=\"menu_button\">\n                                    <i class=\"fa-solid fa-images\"></i> <span data-i18n=\"Show all backgrounds\">Показать все фоны</span>\n                                </button>\n                            </div>\n                        </div>";
    apply_anchor_patch(&mut patched, "галерея фонов (index.html)", bg_hint_target, bg_hint_replacement)?;

    if patched != content {
        write_if_changed(&index_html, &patched)?;
    }

    // 3-4. backgrounds.js handlers: the top button opens the same input,
    // an empty chat tab goes to the shared collection.
    let bg_js = public_dir.join("scripts").join("backgrounds.js");
    let js_content = std::fs::read_to_string(&bg_js).map_err(|e| format!("backgrounds.js: {e}"))?;
    let mut js_patched = js_content.clone();

    let handlers_target = "$('#add_bg_button').on('change', (e) => onBackgroundUploadSelected(e.originalEvent));";
    let handlers_replacement = "$('#add_bg_button').on('change', (e) => onBackgroundUploadSelected(e.originalEvent));\n    $('#add_background_button_top').on('click', function (e) {\n        e.preventDefault();\n        $('#add_bg_button').trigger('click');\n    });\n    $(document).on('click', '#bg_switch_to_global_btn', function () {\n        $('#bg_tabs').tabs('option', 'active', BG_SOURCES.GLOBAL);\n    });\n    $('#backgrounds-drawer-toggle').on('click', function () {\n        const activeTab = getActiveBackgroundTab();\n        const customCount = $('#bg_custom_content .bg_example').length;\n        if (activeTab === BG_SOURCES.CHAT && customCount === 0) {\n            $('#bg_tabs').tabs('option', 'active', BG_SOURCES.GLOBAL);\n        }\n        activateLazyLoader();\n    });";
    apply_anchor_patch(&mut js_patched, "галерея фонов (backgrounds.js)", handlers_target, handlers_replacement)?;

    let upload_target = "        switch (getActiveBackgroundTab()) {\n            case BG_SOURCES.GLOBAL:\n                await uploadBackground(formData);\n                break;\n            case BG_SOURCES.CHAT:\n                await uploadChatBackground(formData);\n                break;\n            default:\n                console.error('Unknown background source type');\n                continue;\n        }";
    let upload_replacement = "        const activeTab = getActiveBackgroundTab();\n        const hasActiveChat = Boolean(getCurrentChatId());\n        if (activeTab === BG_SOURCES.CHAT && hasActiveChat) {\n            await uploadChatBackground(formData);\n        } else {\n            await uploadBackground(formData);\n            if (activeTab === BG_SOURCES.CHAT) {\n                $('#bg_tabs').tabs('option', 'active', BG_SOURCES.GLOBAL);\n                toastr.info('Фон добавлен в общую коллекцию фонов');\n            }\n        }";
    apply_anchor_patch(&mut js_patched, "галерея фонов (backgrounds.js)", upload_target, upload_replacement)?;

    if js_patched != js_content {
        write_if_changed(&bg_js, &js_patched)?;
    }
    Ok(())
}

/// Stream tail render: the module is placed next to the ST scripts, and
/// `onProgressStreaming` calls it instead of a full innerHTML on every frame.
const STREAM_TAIL_JS: &str = include_str!("../streaming/tail-render.js");
const STREAM_TAIL_IMPORT_FIND: &str = r#"import { applyStreamFadeIn } from './scripts/util/stream-fadein.js';
"#;
const STREAM_TAIL_IMPORT_REPLACE: &str = r#"import { applyStreamFadeIn } from './scripts/util/stream-fadein.js';
import { renderStreamingText } from './scripts/lk-tail-render.js';
"#;
const STREAM_TAIL_RENDER_FIND: &str = r#"            const formattedText = messageFormatting(
                processedText,
                chat[messageId].name,
                chat[messageId].is_system,
                chat[messageId].is_user,
                messageId,
                {},
                false,
            );
            if (this.messageTextDom instanceof HTMLElement) {
                if (power_user.stream_fade_in) {
                    applyStreamFadeIn(this.messageTextDom, formattedText);
                } else {
                    this.messageTextDom.innerHTML = formattedText;
                }
            }
"#;
const STREAM_TAIL_RENDER_REPLACE: &str = r#"            const formatText = (text) => messageFormatting(
                text,
                chat[messageId].name,
                chat[messageId].is_system,
                chat[messageId].is_user,
                messageId,
                {},
                false,
            );
            if (this.messageTextDom instanceof HTMLElement) {
                if (power_user.stream_fade_in) {
                    applyStreamFadeIn(this.messageTextDom, formatText(processedText));
                } else {
                    renderStreamingText(this.messageTextDom, processedText, formatText, isFinal);
                }
            }
"#;

/// Patch the text of `public/script.js`. Both insertions — or none: a call without
/// an import would bring down the whole stream. `None` — the patch is already in place or there is no place for it
/// in this ST version.
fn patch_stream_tail_source(script: &str) -> Option<String> {
    if script.matches(STREAM_TAIL_IMPORT_FIND).count() != 1
        || script.matches(STREAM_TAIL_RENDER_FIND).count() != 1
    {
        return None;
    }
    Some(
        script
            .replace(STREAM_TAIL_IMPORT_FIND, STREAM_TAIL_IMPORT_REPLACE)
            .replace(STREAM_TAIL_RENDER_FIND, STREAM_TAIL_RENDER_REPLACE),
    )
}

/// The stream redraws only the tail of the reply (see `streaming/tail-render.js`).
/// The ST code (LF from the codeload archive) did not match the expected one — an error for boot.log,
/// ST remains stock.
pub fn patch_streaming_tail() -> Result<(), String> {
    let public_dir = st_dir().join("public");
    let script_path = public_dir.join("script.js");
    let script = std::fs::read_to_string(&script_path).map_err(|e| format!("script.js: {e}"))?;
    // The module — before editing script.js: a patched import without the file would break page load.
    write_if_changed(&public_dir.join("scripts").join("lk-tail-render.js"), STREAM_TAIL_JS)?;
    if script.contains(STREAM_TAIL_IMPORT_REPLACE) {
        return Ok(());
    }
    let patched = patch_stream_tail_source(&script)
        .ok_or_else(|| format!("хвостовой рендер стрима: место вставки в script.js ST {} не найдено", installed_st_version()))?;
    write_if_changed(&script_path, &patched)
}

const WORLD_INFO_CREATE_ENTRY_FIND: &str = r#"    await saveWorldInfo(worldName, worldInfoTemplate, true);
    await updateWorldInfoList();"#;
const WORLD_INFO_CREATE_ENTRY_REPLACE: &str = r#"    // Cloufam: новая книга сразу открывается с редактируемой записью.
    createWorldInfoEntry(worldName, worldInfoTemplate);
    await saveWorldInfo(worldName, worldInfoTemplate, true);
    await updateWorldInfoList();"#;
const WORLD_INFO_CREATE_ACTIVATE_FIND: &str = r#"    if (selectedIndex !== -1) {
        $('#world_editor_select').val(selectedIndex).trigger('change');
    } else {"#;
const WORLD_INFO_CREATE_ACTIVATE_REPLACE: &str = r#"    if (selectedIndex !== -1) {
        $('#world_editor_select').val(selectedIndex).trigger('change');
        if (!selected_world_info.includes(worldName)) {
            onWorldInfoChange({ state: 'on', silent: 'true' }, worldName);
        }
    } else {"#;
const WORLD_INFO_IMPORT_ACTIVATE_FIND: &str = r#"            if (newIndex >= 0) {
                $('#world_editor_select').val(newIndex).trigger('change');
            }

            toastr.success(t`World Info "${data.name}" imported successfully!`);"#;
const WORLD_INFO_IMPORT_ACTIVATE_REPLACE: &str = r#"            if (newIndex >= 0) {
                $('#world_editor_select').val(newIndex).trigger('change');
                if (!selected_world_info.includes(data.name)) {
                    onWorldInfoChange({ state: 'on', silent: 'true' }, data.name);
                }
            }

            window.__lkTranslateWorldInfo?.(data.name);

            toastr.success(t`World Info "${data.name}" imported successfully!`);"#;

fn patch_world_info_source(source: &str) -> Option<String> {
    let replacements = [
        (WORLD_INFO_CREATE_ENTRY_FIND, WORLD_INFO_CREATE_ENTRY_REPLACE),
        (WORLD_INFO_CREATE_ACTIVATE_FIND, WORLD_INFO_CREATE_ACTIVATE_REPLACE),
        (WORLD_INFO_IMPORT_ACTIVATE_FIND, WORLD_INFO_IMPORT_ACTIVATE_REPLACE),
    ];
    if replacements.iter().any(|(_, replacement)| source.contains(replacement))
        || replacements.iter().any(|(find, _)| source.matches(find).count() != 1)
    {
        return None;
    }
    Some(replacements.iter().fold(source.to_string(), |result, (find, replacement)| {
        result.replace(find, replacement)
    }))
}

/// In stock ST, import and creation only open the book in the editor. In
/// Cloufam this looked like broken keys: the book did not get into the list of
/// active ones and the scanner did not read it. The new book was also empty, and creating
/// the first entry was hidden behind an icon without text.
pub fn patch_world_info() -> Result<(), String> {
    let path = st_dir().join("public").join("scripts").join("world-info.js");
    let source = std::fs::read_to_string(&path).map_err(|e| format!("world-info.js: {e}"))?;
    if source.contains(WORLD_INFO_CREATE_ENTRY_REPLACE)
        && source.contains(WORLD_INFO_CREATE_ACTIVATE_REPLACE)
        && source.contains(WORLD_INFO_IMPORT_ACTIVATE_REPLACE)
    {
        return Ok(());
    }
    let patched = patch_world_info_source(&source)
        .ok_or_else(|| format!("World Info: места вставки в ST {} не найдены однозначно", installed_st_version()))?;
    write_if_changed(&path, &patched)
}

pub const ROLEPLAY_MAIN_PROMPT: &str = "\
Веди ролевой диалог исключительно от лица персонажа {{char}} от первого лица.\n\
Полностью вживайся в роль: твой характер, лексика, тон, эмоциональность, уровень интеллекта, манера речи и построение фраз должны строго следовать описанию персонажа, его предыстории и всем заданным характеристикам — включая любые теги (на русском или английском языке, например [Теги: ...] или [Tags: ...]), список черт, атрибуты и свойства личности.\n\
Категорически запрещено вести себя как шаблонный вежливый ИИ-помощник: если персонаж грубый, недалёкий, глупый, агрессивный, злой, просторечный, эксцентричный или вульгарный — общайся и выражай мысли именно так, сохраняя его уникальный голос, без цензуры, нравоучений и фальшивой вежливости.\n\
Отвечай только за {{char}}: не пиши реплики и действия за {{user}}, не дописывай фразы собеседника и не веди диалог с самим собой.\n\
Если собеседник просит прислать фото, селфи, нарисовать или показать визуальную сцену — опиши происходящее в характере персонажа и добавь команду [{{char}} sends a picture that contains: <подробное визуальное описание сцены, ракурса, внешности и окружения на английском языке>].";

pub const ROLEPLAY_JAILBREAK_PROMPT: &str = "\
[Инструкция: сейчас твоя очередь отвечать. Отвечай исключительно от лица {{char}}, строго соблюдая его характер, уровень интеллекта, лексику и теги из описания. Ни при каких обстоятельствах не веди себя как вежливый ИИ-ассистент. Сообщение собеседника завершено — дай прямой ответ от лица {{char}}.]";

/// Hint for the group chat: ST adds it last before the reply.
/// Without a name prohibition the model copies the history format ("Name: line") and sometimes
/// inserts another participant — while ST cuts the whole reply for a foreign name
/// (`cleanGroupMessage`), and the character silently does not answer.
pub const GROUP_NUDGE_PROMPT: &str = "\
[Сейчас очередь говорить только у {{char}}. Отвечай строго от лица {{char}} от первого лица.\n\
Категорически запрещено писать реплики, диалоги, слова или мысли за других персонажей ({{group}}) или за {{user}}.\n\
Пиши только свою реплику и свои действия. Не продолжай диалог за собеседников. Начинай сразу с текста реплики без префикса «Имя:».]";

/// Edit of a single OpenAI preset (an `OpenAI Settings/*.json` object). Returns
/// true if the content changed.
///
/// The "do not touch the user's choice" policy (the same as for penalties in
/// `sync_st_object`): numeric values are written ONLY when the key is absent.
/// Thresholds like "less means too little" (`temperature <= 0.65`,
/// `repetition_penalty <= 1.05`, `frequency_penalty == 0`) returned
/// the user's preset to our numbers on every boot and every
/// settings save: a deliberate temp 0.2 for cards silently became
/// 0.75. For penalties 0 is both the ST default and a deliberate "no penalty" choice,
/// they cannot be distinguished. Prompts follow their own "text marked as ours" policy:
/// only the stock ST English text is replaced (and an empty jailbreak), the user's
/// edit remains untouched.
fn patch_openai_preset_object(obj: &mut serde_json::Map<String, serde_json::Value>) -> bool {
    let mut changed = false;

    // A missing key = the choice has not been made yet: seed with our defaults.
    for (key, value) in [
        ("temperature", serde_json::json!(0.75)),
        ("repetition_penalty", serde_json::json!(1.18)),
        ("frequency_penalty", serde_json::json!(0.2)),
        ("presence_penalty", serde_json::json!(0.1)),
        ("openai_max_context", serde_json::json!(8192)),
    ] {
        if !obj.contains_key(key) {
            obj.insert(key.into(), value);
            changed = true;
        }
    }
    if let Some(prompts) = obj.get_mut("prompts").and_then(|v| v.as_array_mut()) {
        for p in prompts.iter_mut() {
            if let Some(p_obj) = p.as_object_mut() {
                let id = p_obj.get("identifier").and_then(|v| v.as_str()).unwrap_or_default();
                if id == "main" {
                    let content = p_obj.get("content").and_then(|v| v.as_str()).unwrap_or_default();
                    if content.starts_with("Write {{char}}'s next reply") || content.contains("fictional chat") {
                        p_obj.insert("content".into(), serde_json::Value::String(ROLEPLAY_MAIN_PROMPT.into()));
                        changed = true;
                    }
                } else if id == "jailbreak" {
                    let content = p_obj.get("content").and_then(|v| v.as_str()).unwrap_or_default();
                    if content.is_empty() || content.contains("Coder") {
                        p_obj.insert("content".into(), serde_json::Value::String(ROLEPLAY_JAILBREAK_PROMPT.into()));
                        changed = true;
                    }
                }
            }
        }
    }

    changed
}

/// Apply `patch_openai_preset_object` to the preset file. Read and
/// parse errors are silently skipped: this is an ST data file, and a broken user preset
/// must not bring down boot — unlike our ST code patches, which
/// report insertion site mismatches.
fn patch_openai_preset_file(path: &std::path::Path) {
    if !path.exists() {
        return;
    }
    let Ok(text) = std::fs::read_to_string(path) else { return; };
    let Ok(mut val) = serde_json::from_str::<serde_json::Value>(&text) else { return; };
    let Some(obj) = val.as_object_mut() else { return; };
    if patch_openai_preset_object(obj) {
        if let Ok(serialized) = serde_json::to_string_pretty(&val) {
            // ST reads presets in the same /api/settings/get as settings.json.
            let _ = write_if_changed(path, &serialized);
        }
    }
}

/// Tag catalog of the stock ST cards; the markup of our characters travels in the set.
pub const DEFAULT_TAGS_JSON: &str = include_str!("../default_tags.json");

/// Tag catalog of the card set: `tags.json` next to the cards in the local
/// `characters` folder, otherwise — the baked-in one. The set is in neither the repository nor
/// the releases, so a regular installation lives on the baked-in catalog.
pub fn bundled_tags() -> serde_json::Value {
    let pack = paths::characters_dir().join("tags.json");
    std::fs::read_to_string(&pack)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .filter(|v| v.get("tags").is_some_and(|t| t.is_array()) && v.get("tag_map").is_some_and(|m| m.is_object()))
        .unwrap_or_else(|| serde_json::from_str(DEFAULT_TAGS_JSON).unwrap_or_default())
}

/// SD source for "no generation". There is no `none` value in ST's source list;
/// the default `extras` without the SD module in Extras (Cloufam does not bring them up) gives
/// `isValidState() == false` — the same unavailability, but as a valid value.
const SD_SOURCE_UNAVAILABLE: &str = "extras";

/// Primary ST settings: connection to Ollama (chat), vision via caption,
/// vector memory via Ollama embeddings, image generation via local
/// ComfyUI. The keys are verified against ST 1.19.0 sources — not invented.
/// Runs ONCE: if the file already exists, ST keeps the user's choice in it
/// (models, characters), and overwriting it on every start would destroy that.
pub fn seed_settings(settings: &crate::settings::Settings, ollama_port: u16) -> Result<(), String> {
    if let Err(e) = seed_russian_locale() {
        // Not fatal (unpacking/boot succeeded), but staying silent is not allowed: a locale patch
        // that fails to apply otherwise disables it without diagnostics (V-25).
        crate::boot_log(&format!("SillyTavern: {e}"));
    }
    let _ = seed_theme();
    let dir = st_dir().join("data").join("default-user");
    let target = dir.join("settings.json");
    if target.exists() {
        return Ok(());
    }
    let st_settings = initial_st_settings(settings, ollama_port, comfyui::is_installed());
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let serialized = serde_json::to_string_pretty(&st_settings).map_err(|e| e.to_string())?;
    write_if_changed(&target, &serialized)?;
    // Optional bonus: must not bring down the first launch if something is wrong with the disk.
    if let Err(e) = seed_textgen_presets() {
        crate::boot_log(&format!("Пресеты сэмплеров: {e}"));
    }
    Ok(())
}

/// Contents of the first ST `settings.json`.
fn initial_st_settings(
    settings: &crate::settings::Settings,
    ollama_port: u16,
    comfy_installed: bool,
) -> serde_json::Value {
    let tags_val = bundled_tags();
    let def_tags = tags_val.get("tags").cloned().unwrap_or_else(|| serde_json::json!([]));
    let def_map = tags_val.get("tag_map").cloned().unwrap_or_else(|| serde_json::json!({}));

    serde_json::json!({
        "firstRun": false,
        "active_character": default_character(),
        "max_context": 8192,
        // `theme` and `bogus_folders` at the ST top level are not read: they live in
        // `power_user`. As root keys they only provoke endless rewriting
        // of the file (ST drops unknown keys on the first save, and sync
        // sees the file as "outdated" on every start).
        "tags": def_tags,
        "tag_map": def_map,
        // Chat Completions through our LLM gateway (OpenAI format → native
        // Ollama /api/chat with think:false): roles system/user/assistant from
        // the model's chat template — self-dialogue and user-role lines are
        // structurally impossible (textgen raw mode let the model
        // imitate the dialogue format, see gotchas).
        "main_api": "openai",
        "oai_settings": {
            "chat_completion_source": "custom",
            "custom_url": format!("http://127.0.0.1:{}/v1", crate::gateway::gateway_port(ollama_port)),
            "custom_model": settings.chat_model,
            "openai_model": settings.chat_model,
            "temp_openai": 0.75,
            "top_p_openai": 0.9,
            "freq_pen_openai": 0.2,
            "pres_pen_openai": 0.1,
            "repetition_penalty_openai": 1.18,
            "openai_max_tokens": 600,
            "openai_max_context": 8192,
            "stream_openai": true,
            "group_nudge_prompt": GROUP_NUDGE_PROMPT,
            "prompts": [
                {
                    "name": "Main Prompt",
                    "system_prompt": true,
                    "role": "system",
                    "content": ROLEPLAY_MAIN_PROMPT,
                    "identifier": "main"
                },
                {
                    "name": "Auxiliary Prompt",
                    "system_prompt": true,
                    "role": "system",
                    "content": "",
                    "identifier": "nsfw"
                },
                {
                    "identifier": "dialogueExamples",
                    "name": "Chat Examples",
                    "system_prompt": true,
                    "marker": true
                },
                {
                    "name": "Post-History Instructions",
                    "system_prompt": true,
                    "role": "system",
                    "content": ROLEPLAY_JAILBREAK_PROMPT,
                    "identifier": "jailbreak"
                },
                {
                    "identifier": "chatHistory",
                    "name": "Chat History",
                    "system_prompt": true,
                    "marker": true
                },
                {
                    "identifier": "worldInfoAfter",
                    "name": "World Info (after)",
                    "system_prompt": true,
                    "marker": true
                },
                {
                    "identifier": "worldInfoBefore",
                    "name": "World Info (before)",
                    "system_prompt": true,
                    "marker": true
                },
                {
                    "identifier": "enhanceDefinitions",
                    "role": "system",
                    "name": "Enhance Definitions",
                    "content": "If you have more knowledge of {{char}}, add to the character's lore and personality to enhance them but keep the Character Sheet's definitions absolute.",
                    "system_prompt": true,
                    "marker": false
                },
                {
                    "identifier": "charDescription",
                    "name": "Char Description",
                    "system_prompt": true,
                    "marker": true
                },
                {
                    "identifier": "charPersonality",
                    "name": "Char Personality",
                    "system_prompt": true,
                    "marker": true
                },
                {
                    "identifier": "scenario",
                    "name": "Scenario",
                    "system_prompt": true,
                    "marker": true
                },
                {
                    "identifier": "personaDescription",
                    "name": "Persona Description",
                    "system_prompt": true,
                    "marker": true
                }
            ]
        },
        // textgen settings remain: caption (vision) and vectors take
        // the Ollama address from server_urls regardless of main_api.
        "textgenerationwebui_settings": {
            "type": "ollama",
            "server_urls": { "ollama": format!("http://127.0.0.1:{ollama_port}") },
            "ollama_model": settings.chat_model,
            "streaming": true,
            "temp": 0.75,
            "rep_pen": 1.18,
        },
        // names_as_stop_strings is DISABLED: in OpenAI chat completions roles
        // are isolated by the message structure, and adding character names to stop strings
        // breaks group chats (the model stops at the zero token and outputs emptiness).
        "power_user": {
            // Previously this key was duplicated by a second `power_user` in the same `json!`
            // and silently lost: the Map in serde_json overwrites the previous value.
            "bogus_folders": true,
            "names_as_stop_strings": false,
            "context": {
                "names_as_stop_strings": false,
                "use_stop_strings": true
            },
            "custom_stopping_strings": "[\"\\nВы:\",\"\\nТы:\",\"\\nПользователь:\"]",
            "custom_stopping_strings_macro": true,
            "sysprompt": {
                "enabled": true,
                "name": "Neutral - Chat",
                "content": ROLEPLAY_MAIN_PROMPT,
                "post_history": ROLEPLAY_JAILBREAK_PROMPT
            }
        },
        "extension_settings": {
            // Ollama in caption is the source API `multimodal`
            // (caption/index.js: local|multimodal|extras|horde), the model —
            // an arbitrary tag via `ollama_custom`.
            "caption": {
                "source": "multimodal",
                "multimodal_api": "ollama",
                "multimodal_model": "ollama_custom",
                "ollama_custom_model": settings.vision_model,
            },
            "vectors": {
                "source": "ollama",
                "ollama_model": settings.embed_model,
                "ollama_keep": false,
                // Chat Vectorization: RAG over the chat history — a free
                // memory improvement on 8 GB VRAM. Uses the same embedder.
                "enabled_chats": settings.auto_memory,
                "insert": settings.memory_max_chunks,
                "score_threshold": 0.25,
                // A memory layout in which the prompt prefix lives between
                // turns — see sync_st_object.
                "position": MEMORY_POSITION_IN_CHAT,
                "depth": MEMORY_DEPTH,
                "protect": memory_protect(settings.context_size),
                "lk_protect": memory_protect(settings.context_size),
            },
            // Summarize: history summarization every 10 messages — solves
            // context loss in long dialogues on a short context.
            "memory": {
                "source": "main",
                "promptInterval": 10,
                "position": 0, // extension_prompt_types.IN_PROMPT
                "depth": 2,
                "promptWords": 200,
                "promptMinWords": 25,
                "promptMaxWords": 1000,
                "promptWordsStep": 25,
                "promptIntervalStep": 1,
                "promptMinInterval": 0,
                "promptMaxInterval": 250,
                "prompt_builder": 0, // DEFAULT
            },
            "sd": {
                "source": if comfy_installed { "comfy" } else { SD_SOURCE_UNAVAILABLE },
            },
            // Character thoughts in a separate color: see thought_regex_script().
            // The script is on out of the box; the user can disable it in Regex Presets.
            "regex": [thought_regex_script()],
            // Kokoro TTS: local TTS in the browser, does not need Ollama VRAM.
            // Voice and model — the extension's own default (af_heart — top in
            // quality, onnx-community/Kokoro-82M-v1.0-ONNX), we do not touch them.
            // Only device has our own default: ST sets it to wasm (CPU) itself,
            // while the machine for this product almost always has a GPU — webgpu is faster
            // by an order of magnitude. If Kokoro is unavailable (no WebGPU), ST falls back to System.
            "tts": {
                "currentProvider": "Kokoro",
                "Kokoro": {
                    "modelId": "onnx-community/Kokoro-82M-v1.0-ONNX",
                    "dtype": "q8",
                    "device": "webgpu",
                    "voiceMap": {},
                    "defaultVoice": "af_heart",
                    "speakingRate": 1.0,
                },
            },
        },
    })
}

/// Regex script id for thought highlighting: constant, so that the catch-up on
/// already installed copies does not produce duplicates on every start.
const THOUGHT_REGEX_ID: &str = "cloufam-thought-highlight";

/// Mark in ST's settings `extensions`: the thoughts script is already enabled on this copy
/// by the catch-up. It lives not in the script itself: the regex editor in ST rebuilds
/// the script object from the form fields and would lose a foreign field — and a script disabled
/// by the user would be enabled again on the next start.
const THOUGHTS_ON_MARK: &str = "cloufam_thoughts_on";
const WORLD_INFO_ACTIVATION_MARK: &str = "cloufam_world_info_activation_migrated";

fn ensure_existing_world_info_activation(
    settings: &mut serde_json::Map<String, serde_json::Value>,
    world_names: &[String],
) -> bool {
    let migrated = settings
        .get("extension_settings")
        .and_then(|value| value.as_object())
        .and_then(|extensions| extensions.get(WORLD_INFO_ACTIVATION_MARK))
        .and_then(|value| value.as_bool())
        == Some(true);
    if migrated {
        return false;
    }

    // On the very first unpacking the worlds catalog may not exist yet.
    // A mark here would deprive the next download of its only chance to catch up
    // an already appeared lorebook.
    if world_names.is_empty() {
        return false;
    }

    if let [world_name] = world_names {
        let world_info_settings = settings
            .entry("world_info_settings")
            .or_insert_with(|| serde_json::json!({}));
        let Some(world_info_settings) = world_info_settings.as_object_mut() else {
            return false;
        };
        let world_info = world_info_settings
            .entry("world_info")
            .or_insert_with(|| serde_json::json!({}));
        if let Some(world_info) = world_info.as_object_mut() {
            let active = world_info.get("globalSelect").and_then(|value| value.as_array());
            if active.is_none_or(Vec::is_empty) {
                world_info.insert("globalSelect".into(), serde_json::json!([world_name]));
            }
        }
    }

    let extensions = settings.entry("extension_settings").or_insert_with(|| serde_json::json!({}));
    if let Some(extensions) = extensions.as_object_mut() {
        extensions.insert(WORLD_INFO_ACTIVATION_MARK.into(), serde_json::json!(true));
    }
    true
}

fn existing_world_info_names() -> Vec<String> {
    let dir = st_dir().join("data").join("default-user").join("worlds");
    let mut names = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            (path.extension().and_then(|ext| ext.to_str()) == Some("json"))
                .then(|| path.file_stem()?.to_str().map(str::to_string))
                .flatten()
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}

/// ST regex script: `~text~` in the character's reply is shown as a thought
/// (the `.lk-thought` class from CLOUFAM_THEME_CSS).
///
/// `markdownOnly: true` — the edit is display-only: in the message itself and in
/// the next turn's prompt the tildes remain, so the model sees its own
/// format and keeps it further. Enabled by default (owner's decision
/// 25.09.2026); the tagger extension adds the request to write thoughts in tildes only
/// when the script is enabled, so disabling it in Regex Presets removes that too.
fn thought_regex_script() -> serde_json::Value {
    serde_json::json!({
        "id": THOUGHT_REGEX_ID,
        "scriptName": "Cloufam — мысли (~текст~)",
        // [^~]+ does not let two adjacent thoughts merge into one: "~раз~ и ~два~"
        // is parsed into two matches, not one across the whole text.
        "findRegex": "/~([^~\\n]+)~/g",
        "replaceString": "<span class=\"lk-thought\">$1</span>",
        "trimStrings": [],
        // 2 — regex_placement.AI_OUTPUT: character reply, not user input.
        "placement": [2],
        "disabled": false,
        "markdownOnly": true,
        "promptOnly": false,
        "runOnEdit": false,
        "substituteRegex": 0,
        "minDepth": null,
        "maxDepth": null,
    })
}

/// Catch up the thoughts regex script on an already installed copy: we add only
/// if a script with this id does not exist yet. We do not touch the found one — the user could
/// have enabled it, renamed it or adjusted the expression for themselves.
fn ensure_thought_regex(ext_obj: &mut serde_json::Map<String, serde_json::Value>) -> bool {
    let switched_on = ext_obj.get(THOUGHTS_ON_MARK).and_then(|v| v.as_bool()) == Some(true);
    let regex = ext_obj.entry("regex").or_insert_with(|| serde_json::json!([]));
    let Some(list) = regex.as_array_mut() else {
        return false;
    };
    let mut changed = false;
    match list
        .iter_mut()
        .find(|s| s.get("id").and_then(|v| v.as_str()) == Some(THOUGHT_REGEX_ID))
    {
        // Until 25.09.2026 the script was seeded disabled — such a copy is enabled once.
        // After that, disabling in Regex Presets is the user's choice.
        Some(script) if !switched_on => {
            if script.get("disabled").and_then(|v| v.as_bool()) != Some(false) {
                script["disabled"] = serde_json::json!(false);
                changed = true;
            }
        }
        Some(_) => {}
        None => {
            list.push(thought_regex_script());
            changed = true;
        }
    }
    if !switched_on {
        ext_obj.insert(THOUGHTS_ON_MARK.into(), serde_json::json!(true));
        changed = true;
    }
    changed
}

/// One named ST TextGen preset: the full schema (like the bundled
/// Default/Deterministic etc. in `default/content/presets/textgen/`), but with
/// values that the gateway currently hardcodes in two places at once —
/// `seed_settings` and `gateway.rs::build_ollama_request` (repeat_penalty=1.12,
/// num_ctx≥8192, repeat_last_n=512). The preset gives the same thing with the stock "Load preset"
/// button instead of editing settings.json by hand.
fn textgen_preset_json(temp: f64, top_p: f64, top_k: u32, rep_pen: f64, rep_pen_range: u32) -> serde_json::Value {
    serde_json::json!({
        "temp": temp,
        "temperature_last": true,
        "top_p": top_p,
        "top_k": top_k,
        "top_a": 0,
        "tfs": 1,
        "epsilon_cutoff": 0,
        "eta_cutoff": 0,
        "typical_p": 1,
        "min_p": 0.01,
        "rep_pen": rep_pen,
        "rep_pen_range": rep_pen_range,
        "rep_pen_decay": 0,
        "rep_pen_slope": 1,
        "no_repeat_ngram_size": 0,
        "penalty_alpha": 0,
        "num_beams": 1,
        "length_penalty": 1,
        "min_length": 0,
        "encoder_rep_pen": 1,
        "freq_pen": 0,
        "presence_pen": 0,
        "skew": 0,
        "do_sample": true,
        "early_stopping": false,
        "dynatemp": false,
        "min_temp": 0,
        "max_temp": 2,
        "dynatemp_exponent": 1,
        "smoothing_factor": 0,
        "smoothing_curve": 1,
        "dry_allowed_length": 2,
        "dry_multiplier": 0,
        "dry_base": 1.75,
        "dry_sequence_breakers": "[\"\\n\", \":\", \"\\\"\", \"*\"]",
        "dry_penalty_last_n": 0,
        "add_bos_token": true,
        "ban_eos_token": false,
        "skip_special_tokens": true,
        "mirostat_mode": 0,
        "mirostat_tau": 5,
        "mirostat_eta": 0.1,
        "guidance_scale": 1,
        "negative_prompt": "",
        "grammar_string": "",
        "json_schema": null,
        "json_schema_allow_empty": false,
        "banned_tokens": "",
        "sampler_priority": [
            "repetition_penalty", "presence_penalty", "frequency_penalty", "dry", "temperature",
            "dynamic_temperature", "quadratic_sampling", "top_n_sigma", "top_k", "top_p", "typical_p",
            "epsilon_cutoff", "eta_cutoff", "tfs", "top_a", "min_p", "mirostat", "xtc",
            "encoder_repetition_penalty", "no_repeat_ngram",
        ],
        "samplers": [
            "penalties", "dry", "top_n_sigma", "top_k", "typ_p", "tfs_z", "typical_p", "xtc", "top_p",
            "min_p", "temperature",
        ],
        "samplers_priorities": [
            "dry", "penalties", "no_repeat_ngram", "temperature", "top_nsigma", "top_p_top_k", "top_a",
            "min_p", "tfs", "eta_cutoff", "epsilon_cutoff", "typical_p", "quadratic", "xtc",
        ],
        "ignore_eos_token": false,
        "spaces_between_special_tokens": true,
        "speculative_ngram": false,
        "sampler_order": [6, 0, 1, 3, 4, 2, 5],
        "logit_bias": [],
        "xtc_threshold": 0.1,
        "xtc_probability": 0,
        "nsigma": 0,
        "min_keep": 0,
        "rep_pen_size": 0,
    })
}

/// Named Ollama sampler presets for the ST Text Completion panel.
/// Creates only missing files — does not touch the user's own edit (or an already
/// created preset).
fn seed_textgen_presets() -> Result<(), String> {
    let dir = st_dir().join("data").join("default-user").join("TextGen Settings");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let presets: [(&str, serde_json::Value); 3] = [
        (
            "Cloufam — сбалансированный",
            textgen_preset_json(0.75, 0.9, 40, 1.12, 512),
        ),
        (
            "Cloufam — меньше повторов",
            textgen_preset_json(0.8, 0.9, 40, 1.18, 1024),
        ),
        (
            "Cloufam — предсказуемый",
            textgen_preset_json(0.5, 0.85, 40, 1.1, 512),
        ),
    ];
    for (name, preset) in presets {
        let path = dir.join(format!("{name}.json"));
        if path.exists() {
            continue;
        }
        let text = serde_json::to_string_pretty(&preset).map_err(|e| e.to_string())?;
        write_if_changed(&path, &text)?;
    }
    Ok(())
}

/// Lay out the built-in set's tags in the SillyTavern settings.
/// `refresh_bundled` — update the set's card markup over the old one
/// (user tags on the same cards are preserved): before
/// TAGS_VERSION appeared, entries were only added, and an installed copy forever
/// kept the previous tag set.
fn apply_bundled_tags(obj: &mut serde_json::Map<String, serde_json::Value>, refresh_bundled: bool) -> bool {
    apply_tags_catalog(obj, &bundled_tags(), refresh_bundled)
}

fn apply_tags_catalog(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    tags_val: &serde_json::Value,
    refresh_bundled: bool,
) -> bool {
    let (Some(def_tags), Some(def_map)) = (
        tags_val.get("tags").and_then(|v| v.as_array()),
        tags_val.get("tag_map").and_then(|v| v.as_object()),
    ) else {
        return false;
    };
    let mut modified = false;

    // The user's own tag with the same name is the same tag: we replace our id with it,
    // otherwise a second "priestess" appears in the library filter, and the card
    // ends up tagged with other than the tag the user filters by.
    let foreign_by_name: std::collections::HashMap<String, String> = obj
        .get("tags")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    let id = t.get("id").and_then(|i| i.as_str())?;
                    let name = t.get("name").and_then(|n| n.as_str())?;
                    (!id.starts_with("t-")).then(|| (name.to_string(), id.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    let resolve = |id: &str| -> String {
        def_tags
            .iter()
            .find(|t| t.get("id").and_then(|i| i.as_str()) == Some(id))
            .and_then(|t| t.get("name").and_then(|n| n.as_str()))
            .and_then(|name| foreign_by_name.get(name).cloned())
            .unwrap_or_else(|| id.to_string())
    };

    // We add the catalog's missing tags to the user's ones instead of replacing
    // the whole list: otherwise deleting or renaming the set folder
    // wiped all their own folders and tags on every start.
    let cur_tags = obj.entry("tags").or_insert_with(|| serde_json::json!([]));
    if let Some(arr) = cur_tags.as_array_mut() {
        for tag in def_tags {
            let id = tag.get("id").and_then(|i| i.as_str()).unwrap_or_default();
            let name = tag.get("name").and_then(|n| n.as_str()).unwrap_or_default();
            // A set tag with the previous id could change its name (English "otaku" became
            // "отаку"): when updating the set we bring the name to the new one, otherwise
            // the library forever keeps the label from the old version.
            if refresh_bundled {
                if let Some(existing) = arr.iter_mut().find(|t| {
                    t.get("id").and_then(|i| i.as_str()) == Some(id)
                }) {
                    if existing.get("name").and_then(|n| n.as_str()) != Some(name) {
                        if let Some(obj) = existing.as_object_mut() {
                            obj.insert("name".into(), serde_json::Value::String(name.to_string()));
                            modified = true;
                        }
                    }
                }
            }
            // We compare by id, not by name: a set tag's name could remain
            // the previous one under a new id, and the name check did not add it —
            // tag_map referenced an id absent from the list, and the markup
            // crumbled into identifiers.
            let exists = arr.iter().any(|t| t.get("id").and_then(|i| i.as_str()) == Some(id))
                || foreign_by_name.contains_key(name);
            if !exists {
                arr.push(tag.clone());
                modified = true;
            }
        }
    }

    let cur_map = obj.entry("tag_map").or_insert_with(|| serde_json::json!({}));
    if let Some(cur_map_obj) = cur_map.as_object_mut() {
        for (avatar, bundled) in def_map {
            let bundled_ids: Vec<serde_json::Value> = bundled
                .as_array()
                .map(|ids| {
                    ids.iter()
                        .map(|id| match id.as_str() {
                            Some(s) => serde_json::Value::String(resolve(s)),
                            None => id.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            match cur_map_obj.get(avatar) {
                None => {
                    cur_map_obj.insert(avatar.clone(), serde_json::Value::Array(bundled_ids));
                    modified = true;
                }
                Some(current) if refresh_bundled => {
                    let mut merged = bundled_ids;
                    for id in current.as_array().into_iter().flatten() {
                        // Set tags of any version — with the `t-` prefix: they are
                        // replaced by the new markup, foreign ones stay on the card.
                        let own = id.as_str().is_some_and(|s| s.starts_with("t-"));
                        if !own && !merged.contains(id) {
                            merged.push(id.clone());
                        }
                    }
                    if current.as_array() != Some(&merged) {
                        cur_map_obj.insert(avatar.clone(), serde_json::Value::Array(merged));
                        modified = true;
                    }
                }
                Some(_) => {}
            }
        }
    }

    // Tags of the previous set that were left out are removed from the list — otherwise
    // the library filter grows dead labels.
    if refresh_bundled {
        let used: std::collections::HashSet<String> = obj
            .get("tag_map")
            .and_then(|m| m.as_object())
            .map(|m| {
                m.values()
                    .flat_map(|v| v.as_array().cloned().unwrap_or_default())
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(arr) = obj.get_mut("tags").and_then(|t| t.as_array_mut()) {
            let before = arr.len();
            // Our tags have the `t-` prefix; we do not touch foreign ones (and folders).
            arr.retain(|t| {
                let id = t.get("id").and_then(|i| i.as_str()).unwrap_or_default();
                !id.starts_with("t-") || used.contains(id)
            });
            modified |= arr.len() != before;
        }

        // A reference to a tag absent from the list is shown in the UI as a bare
        // identifier and breaks character trait parsing — we clean those up.
        let known: std::collections::HashSet<String> = obj
            .get("tags")
            .and_then(|t| t.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.get("id").and_then(|i| i.as_str()).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(map) = obj.get_mut("tag_map").and_then(|m| m.as_object_mut()) {
            for ids in map.values_mut() {
                if let Some(arr) = ids.as_array_mut() {
                    let before = arr.len();
                    arr.retain(|id| id.as_str().is_none_or(|s| known.contains(s)));
                    modified |= arr.len() != before;
                }
            }
        }
    }

    // `bogus_folders` at the ST top level is not read (the flag lives in `power_user`,
    // it is set by `sync_st_settings` below). As a root key it also set
    // `modified` on every start: ST drops unknown keys on its first
    // own save, and the file always seemed outdated.
    modified
}

/// Synchronize the gateway, Ollama addresses and active models in SillyTavern's settings.json.
pub fn sync_st_settings(settings: &crate::settings::Settings, ollama_port: u16) -> Result<(), String> {
    let _ = seed_theme();
    let _ = seed_groups_activation_strategy();
    let target = st_dir().join("data").join("default-user").join("settings.json");
    if !target.exists() {
        return Ok(());
    }
    let text = std::fs::read_to_string(&target).map_err(|e| e.to_string())?;
    let mut st: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(err) => {
            eprintln!("ST settings.json повреждён ({err}), восстанавливаем...");
            // Repeated corruption: the fixed backup name is already taken, and
            // rename failed with ERROR_ALREADY_EXISTS — the file stayed corrupted
            // forever. We replace the old backup with a fresh copy.
            let backup = target.with_extension("json.corrupt");
            let _ = std::fs::remove_file(&backup);
            let _ = std::fs::rename(&target, &backup);
            let _ = seed_settings(settings, ollama_port);
            if let Ok(new_text) = std::fs::read_to_string(&target) {
                serde_json::from_str(&new_text).map_err(|e| e.to_string())?
            } else {
                return Err(format!("Не удалось восстановить settings.json: {err}"));
            }
        }
    };
    let Some(obj) = st.as_object_mut() else {
        return Ok(());
    };

    if sync_st_object(obj, settings, ollama_port, comfyui::is_installed()) {
        let serialized = serde_json::to_string_pretty(&st).map_err(|e| e.to_string())?;
        // Atomically: ST reads this file on every /api/settings/get while
        // the settings window saves — a truncated file would be taken as empty.
        write_if_changed(&target, &serialized)?;
    }

    // We synchronize the OpenAI presets so that choosing a preset does not bring back the English prompts
    let preset_dir = st_dir().join("data").join("default-user").join("OpenAI Settings");
    if preset_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&preset_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().map(|e| e == "json").unwrap_or(false) {
                    patch_openai_preset_file(&p);
                }
            }
        }
    }
    let default_content_preset = st_dir().join("default").join("content").join("presets").join("openai").join("Default.json");
    patch_openai_preset_file(&default_content_preset);

    // Named sampler presets — the same catch-up for already installed
    // copies as for the other baked-in sets; creates only missing
    // files.
    if let Err(e) = seed_textgen_presets() {
        crate::boot_log(&format!("Пресеты сэмплеров: {e}"));
    }

    Ok(())
}

/// Bring the parsed ST `settings.json` to the application settings. Returns
/// true if something changed.
fn sync_st_object(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    settings: &crate::settings::Settings,
    ollama_port: u16,
    comfy_installed: bool,
) -> bool {
    let mut modified = false;

    let power = obj.entry("power_user").or_insert_with(|| serde_json::json!({}));
    if let Some(power_obj) = power.as_object_mut() {
        if power_obj.get("bogus_folders").and_then(|v| v.as_bool()) != Some(true) {
            power_obj.insert("bogus_folders".into(), serde_json::Value::Bool(true));
            modified = true;
        }
        // We lay out the theme while the styling version lags. The user switched to another
        // theme — we do not touch it at all, otherwise they could not leave it. The version
        // is bumped by the caller after a successful boot.
        let cur_theme = power_obj.get("theme").and_then(|v| v.as_str()).unwrap_or_default();
        let ours_or_fresh = cur_theme.is_empty() || cur_theme == "Cloufam";
        if settings.theme_version < THEME_VERSION
            && ours_or_fresh
            && merge_theme_into_power_user(power_obj)
        {
            modified = true;
        }
    }
    // The background — together with the styling version, once: the one chosen later in the ST
    // gallery is not overwritten by the next boot.
    if settings.theme_version < THEME_VERSION && apply_cloufam_background(obj) {
        modified = true;
    }

    // On settings reset the ST client returns to another backend (kobold/ooba),
    // and the chat silently stops working: the application expects replies through its gateway.
    if obj.get("main_api").and_then(|v| v.as_str()) != Some("openai") {
        obj.insert("main_api".into(), serde_json::Value::String("openai".into()));
        modified = true;
    }

    let ctx = settings.context_size.max(8192);
    if obj.get("max_context").and_then(|v| v.as_u64()).unwrap_or(0) != ctx as u64 {
        obj.insert("max_context".into(), serde_json::json!(ctx));
        modified = true;
    }

    let expected_gateway_url = format!("http://127.0.0.1:{}/v1", crate::gateway::gateway_port(ollama_port));
    let oai = obj.entry("oai_settings").or_insert_with(|| serde_json::json!({}));
    if let Some(oai_obj) = oai.as_object_mut() {
        if oai_obj.get("chat_completion_source").and_then(|v| v.as_str()) != Some("custom") {
            oai_obj.insert(
                "chat_completion_source".into(),
                serde_json::Value::String("custom".into()),
            );
            modified = true;
        }
        // By default ST suggests an English phrase to the group without a name prohibition.
        // We replace only it, emptiness and the previous version of our hint: text
        // edited by the user in ST is not overwritten by synchronization.
        let nudge = oai_obj
            .get("group_nudge_prompt")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if nudge != GROUP_NUDGE_PROMPT
            && (nudge.is_empty()
                || nudge.contains("Write the next reply only as")
                || nudge.contains("без префикса «Имя:» в начале"))
        {
            oai_obj.insert(
                "group_nudge_prompt".into(),
                serde_json::Value::String(GROUP_NUDGE_PROMPT.into()),
            );
            modified = true;
        }
        if oai_obj.get("custom_url").and_then(|v| v.as_str()) != Some(&expected_gateway_url) {
            oai_obj.insert("custom_url".into(), serde_json::Value::String(expected_gateway_url));
            modified = true;
        }
        if oai_obj.get("custom_model").and_then(|v| v.as_str()) != Some(&settings.chat_model) {
            oai_obj.insert("custom_model".into(), serde_json::Value::String(settings.chat_model.clone()));
            oai_obj.insert("openai_model".into(), serde_json::Value::String(settings.chat_model.clone()));
            modified = true;
        }
        if oai_obj.get("openai_max_context").and_then(|v| v.as_u64()).unwrap_or(0) != ctx as u64 {
            oai_obj.insert("openai_max_context".into(), serde_json::json!(ctx));
            modified = true;
        }
        // Reply length: the application is the source of truth for this parameter (as well as
        // for temperature and context), otherwise the value set "for the hardware"
        // would not reach generation: the ST client sends its own.
        if oai_obj.get("openai_max_tokens").and_then(|v| v.as_u64()).unwrap_or(0) != settings.max_tokens as u64 {
            oai_obj.insert("openai_max_tokens".into(), serde_json::json!(settings.max_tokens));
            modified = true;
        }
        let temp = (settings.temperature as f64 * 100.0).round() / 100.0;
        let cur_temp = oai_obj.get("temp_openai").and_then(|v| v.as_f64()).unwrap_or(0.0);
        if (cur_temp - temp).abs() > 0.02 {
            oai_obj.insert("temp_openai".into(), serde_json::json!(temp));
            modified = true;
        }
        let top_p = (settings.top_p as f64 * 100.0).round() / 100.0;
        let cur_top_p = oai_obj.get("top_p_openai").and_then(|v| v.as_f64()).unwrap_or(0.0);
        if (cur_top_p - top_p).abs() > 0.02 {
            oai_obj.insert("top_p_openai".into(), serde_json::json!(top_p));
            modified = true;
        }
        // Penalties — only when the key is absent, like device at Kokoro: 1.0 and 0 are
        // both ST defaults and a deliberate "no penalty" choice, they cannot be distinguished,
        // and synchronization runs on every settings save. Installed copies
        // have already received their 1.18/0.2/0.1 through the previous forced catch-up.
        for (key, value) in [
            ("repetition_penalty_openai", 1.18),
            ("freq_pen_openai", 0.2),
            ("pres_pen_openai", 0.1),
        ] {
            if !oai_obj.contains_key(key) {
                oai_obj.insert(key.into(), serde_json::json!(value));
                modified = true;
            }
        }
        if let Some(prompts) = oai_obj.get_mut("prompts").and_then(|v| v.as_array_mut()) {
            for p in prompts.iter_mut() {
                if let Some(p_obj) = p.as_object_mut() {
                    let id = p_obj.get("identifier").and_then(|v| v.as_str()).unwrap_or_default();
                    if id == "main" {
                        let content = p_obj.get("content").and_then(|v| v.as_str()).unwrap_or_default();
                        if content.starts_with("Write {{char}}'s next reply") || content.contains("fictional chat") {
                            p_obj.insert("content".into(), serde_json::Value::String(ROLEPLAY_MAIN_PROMPT.into()));
                            modified = true;
                        }
                    } else if id == "jailbreak" {
                        let content = p_obj.get("content").and_then(|v| v.as_str()).unwrap_or_default();
                        if content.is_empty() || content.contains("Coder") {
                            p_obj.insert("content".into(), serde_json::Value::String(ROLEPLAY_JAILBREAK_PROMPT.into()));
                            modified = true;
                        }
                    }
                }
            }
        }
    }

    let power = obj.entry("power_user").or_insert_with(|| serde_json::json!({}));
    if let Some(power_obj) = power.as_object_mut() {
        if power_obj.get("names_as_stop_strings").and_then(|v| v.as_bool()).unwrap_or(false) {
            power_obj.insert("names_as_stop_strings".into(), serde_json::Value::Bool(false));
            modified = true;
        }
        if let Some(ctx_obj) = power_obj.get_mut("context").and_then(|v| v.as_object_mut()) {
            if ctx_obj.get("names_as_stop_strings").and_then(|v| v.as_bool()).unwrap_or(false) {
                ctx_obj.insert("names_as_stop_strings".into(), serde_json::Value::Bool(false));
                modified = true;
            }
        }
        if let Some(sys_obj) = power_obj.get_mut("sysprompt").and_then(|v| v.as_object_mut()) {
            let content = sys_obj.get("content").and_then(|v| v.as_str()).unwrap_or_default();
            if content.starts_with("Ты — {{char}}. Веди чат от лица {{char}} на русском языке. Отвечай только за себя") {
                sys_obj.insert("content".into(), serde_json::Value::String(ROLEPLAY_MAIN_PROMPT.into()));
                modified = true;
            }
            let post = sys_obj.get("post_history").and_then(|v| v.as_str()).unwrap_or_default();
            if post.contains("Coder") || post.starts_with("[Инструкция: сейчас твоя очередь отвечать. Сообщение пользователя уже закончено") {
                sys_obj.insert("post_history".into(), serde_json::Value::String(ROLEPLAY_JAILBREAK_PROMPT.into()));
                modified = true;
            }
        }
    }

    let expected_ollama_url = format!("http://127.0.0.1:{ollama_port}");
    let textgen = obj.entry("textgenerationwebui_settings").or_insert_with(|| serde_json::json!({}));
    if let Some(textgen_obj) = textgen.as_object_mut() {
        let urls = textgen_obj.entry("server_urls").or_insert_with(|| serde_json::json!({}));
        if let Some(urls_obj) = urls.as_object_mut() {
            if urls_obj.get("ollama").and_then(|v| v.as_str()) != Some(&expected_ollama_url) {
                urls_obj.insert("ollama".into(), serde_json::Value::String(expected_ollama_url));
                modified = true;
            }
        }
        if textgen_obj.get("ollama_model").and_then(|v| v.as_str()) != Some(&settings.chat_model) {
            textgen_obj.insert("ollama_model".into(), serde_json::Value::String(settings.chat_model.clone()));
            modified = true;
        }
    }

    let ext = obj.entry("extension_settings").or_insert_with(|| serde_json::json!({}));
    if let Some(ext_obj) = ext.as_object_mut() {
        let caption = ext_obj.entry("caption").or_insert_with(|| serde_json::json!({}));
        if let Some(cap_obj) = caption.as_object_mut() {
            // The previous seed wrote `source: "ollama"` — caption has no such source
            // ("Unknown caption source"), and ST appended `multimodal_api`
            // with its default `openai`. The value `ollama` in source can only be
            // ours: the ST UI does not allow setting it.
            if cap_obj.get("source").and_then(|v| v.as_str()) == Some("ollama") {
                cap_obj.insert("source".into(), serde_json::Value::String("multimodal".into()));
                cap_obj.insert("multimodal_api".into(), serde_json::Value::String("ollama".into()));
                cap_obj.insert("multimodal_model".into(), serde_json::Value::String("ollama_custom".into()));
                modified = true;
            }
            if cap_obj.get("ollama_custom_model").and_then(|v| v.as_str()) != Some(&settings.vision_model) {
                cap_obj.insert("ollama_custom_model".into(), serde_json::Value::String(settings.vision_model.clone()));
                modified = true;
            }
        }
        let vectors = ext_obj.entry("vectors").or_insert_with(|| serde_json::json!({}));
        if let Some(vec_obj) = vectors.as_object_mut() {
            if vec_obj.get("ollama_model").and_then(|v| v.as_str()) != Some(&settings.embed_model) {
                vec_obj.insert("ollama_model".into(), serde_json::Value::String(settings.embed_model.clone()));
                modified = true;
            }
            // Chat Vectorization: RAG over the chat history.
            if vec_obj.get("enabled_chats").and_then(|v| v.as_bool()) != Some(settings.auto_memory) {
                vec_obj.insert("enabled_chats".into(), serde_json::Value::Bool(settings.auto_memory));
                modified = true;
            }
            // "How many memory chunks to mix in" — the `memory_max_chunks` knob from
            // the application settings: the "Suggestions" panel promised eight chunks, while
            // ST always kept its own default 3, because the `insert` key
            // was never written.
            if vec_obj.get("insert").and_then(|v| v.as_u64()) != Some(settings.memory_max_chunks as u64) {
                vec_obj.insert("insert".into(), serde_json::json!(settings.memory_max_chunks));
                modified = true;
            }
            // Relevance threshold — the ST knob ("Score Threshold"): we write only
            // when the key is completely absent, otherwise every settings save
            // rolled the user's choice back to 0.25.
            if vec_obj.get("score_threshold").is_none() {
                vec_obj.insert("score_threshold".into(), serde_json::json!(0.25));
                modified = true;
            }
            modified |= sync_memory_layout(vec_obj, ctx);
        }
        // Summarize: history summarization every 10 messages
        let memory = ext_obj.entry("memory").or_insert_with(|| serde_json::json!({}));
        if let Some(mem_obj) = memory.as_object_mut() {
            // Summarize — also ST knobs (source and "every N messages").
            // We do not overwrite the set value: the same policy as for the
            // vectors threshold above.
            if mem_obj.get("source").is_none() {
                mem_obj.insert("source".into(), serde_json::Value::String("main".into()));
                modified = true;
            }
            if mem_obj.get("promptInterval").is_none() {
                mem_obj.insert("promptInterval".into(), serde_json::json!(10));
                modified = true;
            }
        }
        // Character thoughts: the script is added enabled, an old disabled copy is enabled once.
        modified |= ensure_thought_regex(ext_obj);
        // Kokoro TTS: only if the old default "System" is set
        let tts = ext_obj.entry("tts").or_insert_with(|| serde_json::json!({}));
        if let Some(tts_obj) = tts.as_object_mut() {
            let cur = tts_obj.get("currentProvider").and_then(|v| v.as_str()).unwrap_or("");
            if cur == "System" || cur.is_empty() {
                tts_obj.insert("currentProvider".into(), serde_json::Value::String("Kokoro".into()));
                modified = true;
            }
            // We put Kokoro on webgpu only when the key is still completely absent, that
            // is, the extension has never saved its settings. A written
            // `wasm` is already the user's choice (in the Kokoro panel it is
            // called "WebAssembly (CPU)"), and it must not be reset:
            // synchronization runs on every settings save, and the person
            // would return to CPU again and again.
            let kokoro = tts_obj.entry("Kokoro").or_insert_with(|| serde_json::json!({}));
            if let Some(kokoro_obj) = kokoro.as_object_mut() {
                if !kokoro_obj.contains_key("device") {
                    kokoro_obj.insert("device".into(), serde_json::Value::String("webgpu".into()));
                    modified = true;
                }
            }
        }
        let sd = ext_obj.entry("sd").or_insert_with(|| serde_json::json!({}));
        if let Some(sd_obj) = sd.as_object_mut() {
            modified |= sync_sd_settings(sd_obj, settings, comfy_installed);
        }
    }

    modified |= ensure_existing_world_info_activation(obj, &existing_world_info_names());

    modified |= apply_bundled_tags(obj, settings.tags_version < TAGS_VERSION);
    modified
}

/// Mark in `extension_settings.sd`: which values Cloufam wrote there
/// (the model and the size/steps derived from it). Lives separately from the keys
/// themselves: ST writes them too, and "our previous value" and "the user's manual choice"
/// can only be distinguished by such a record — like `lk_protect` in the vector memory
/// layout.
const LK_SD_OURS: &str = "lk_sd_ours";

/// The key belongs to us, it can be overwritten: the key is absent (we seed),
/// the current value equals the target (ours or equivalent to it), or it was
/// written by us last (the `LK_SD_OURS` mark). Everything else is the user's choice
/// in ST, synchronization does not touch it.
fn sd_value_is_ours(
    sd_obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    desired: &serde_json::Value,
) -> bool {
    match sd_obj.get(key) {
        None => true,
        Some(current) => {
            current == desired
                || sd_obj.get(LK_SD_OURS).and_then(|m| m.get(key)) == Some(current)
        }
    }
}

/// Write the value while the key is ours (see `sd_value_is_ours`), and remember it
/// in the `LK_SD_OURS` mark. Returns true if the object changed.
fn sync_sd_value(
    sd_obj: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    desired: serde_json::Value,
) -> bool {
    if !sd_value_is_ours(sd_obj, key, &desired) {
        return false;
    }
    let mut changed = false;
    if sd_obj.get(key) != Some(&desired) {
        sd_obj.insert(key.into(), desired.clone());
        changed = true;
    }
    if sd_obj.get(LK_SD_OURS).and_then(|m| m.get(key)) != Some(&desired) {
        let marker = sd_obj.entry(LK_SD_OURS).or_insert_with(|| serde_json::json!({}));
        if !marker.is_object() {
            // The mark is corrupted by a manual file edit — we recreate it entirely.
            *marker = serde_json::json!({});
        }
        if let Some(m) = marker.as_object_mut() {
            m.insert(key.into(), desired);
        }
        changed = true;
    }
    changed
}

/// Bring `extension_settings.sd` (the ComfyUI block) to the application settings.
/// Returns true if something changed.
///
/// The "do not touch the user's choice" policy — the product's key contract:
/// we write a value only when the key is absent or marked as ours. Previously the block
/// normalized unconditionally and returned the manual choice (DPM++ instead of "Euler a",
/// own CFG, own workflow, own checkpoint, own sizes) to our values on
/// every settings save and boot. Infrastructure keys (`source`,
/// `comfy_url`, `comfy_type`) remain forced: ST must go to our
/// ComfyUI on 8188 — the application installs and brings it up itself, like `custom_url`
/// of the gateway. Text keys follow the "text marked as ours" policy (empty or
/// a fragment of our text), like `group_nudge_prompt`.
fn sync_sd_settings(
    sd_obj: &mut serde_json::Map<String, serde_json::Value>,
    settings: &crate::settings::Settings,
    comfy_installed: bool,
) -> bool {
    // ComfyUI is not installed — we clear our source (`comfy` for local
    // ComfyUI and the previous invalid `none`) so that ST does not try to generate.
    // We do not touch the user's foreign source.
    if !comfy_installed {
        if matches!(sd_obj.get("source").and_then(|v| v.as_str()), Some("comfy" | "none")) {
            sd_obj.insert("source".into(), serde_json::Value::String(SD_SOURCE_UNAVAILABLE.into()));
            return true;
        }
        return false;
    }

    let mut modified = false;
    // The application infrastructure — our ComfyUI on 8188.
    for (key, value) in [
        ("source", serde_json::Value::String("comfy".into())),
        ("comfy_url", serde_json::Value::String("http://127.0.0.1:8188".into())),
        ("comfy_type", serde_json::Value::String("standard".into())),
    ] {
        if sd_obj.get(key) != Some(&value) {
            sd_obj.insert(key.into(), value);
            modified = true;
        }
    }

    // Constants — only when the key is absent (like device at Kokoro): one's own
    // value in ST is the user's choice, and synchronization runs on every
    // settings save and on every boot.
    for (key, value) in [
        ("comfy_workflow", serde_json::Value::String("Default_Comfy_Workflow.json".into())),
        ("interactive_mode", serde_json::Value::Bool(true)),
        ("function_tool", serde_json::Value::Bool(true)),
        ("tool_visible", serde_json::Value::Bool(true)),
        ("wand_visible", serde_json::Value::Bool(true)),
        ("sampler", serde_json::Value::String("Euler a".into())),
        ("scale", serde_json::json!(6.0)),
    ] {
        if !sd_obj.contains_key(key) {
            sd_obj.insert(key.into(), value);
            modified = true;
        }
    }

    // The model and the size/steps derived from it — one group: sizes are computed
    // from the model, so the user's own model chosen in ST owns them too.
    // While the model is ours (or absent yet), the values catch up with the application
    // settings — choosing a model in the Cloufam window keeps reaching ST.
    let img_model = if !settings.image_model.is_empty() {
        settings.image_model.as_str()
    } else {
        // The default checkpoint name — one for the whole product (V-30).
        crate::comfyui::DEFAULT_IMAGE_CHECKPOINT
    };
    if sd_value_is_ours(sd_obj, "model", &serde_json::Value::String(img_model.to_string())) {
        modified |= sync_sd_value(sd_obj, "model", serde_json::Value::String(img_model.to_string()));
        let (target_w, target_h, target_steps) = sd_target_size(img_model);
        modified |= sync_sd_value(sd_obj, "width", serde_json::json!(target_w));
        modified |= sync_sd_value(sd_obj, "height", serde_json::json!(target_h));
        modified |= sync_sd_value(sd_obj, "steps", serde_json::json!(target_steps));
    }

    let negative = "lowres, bad anatomy, bad hands, text, error, missing fingers, extra digit, fewer digits, cropped, worst quality, low quality, normal quality, jpeg artifacts, signature, watermark, username, blurry, artist name";
    // "worst quality" is present in almost any negative prompt. It cannot be used
    // to recognize our text: a user edit where the phrase remained was overwritten.
    // We seed an empty string anew — this is not a choice but a cleared field.
    if sd_obj
        .get("negative_prompt")
        .and_then(|v| v.as_str())
        .is_some_and(str::is_empty)
    {
        sd_obj.remove("negative_prompt");
    }
    modified |= sync_sd_value(
        sd_obj,
        "negative_prompt",
        serde_json::Value::String(negative.into()),
    );
    if sd_obj.get("prompt_prefix").and_then(|v| v.as_str()).map(|s| s.is_empty()).unwrap_or(true) {
        sd_obj.insert("prompt_prefix".into(), serde_json::Value::String("masterpiece, best quality, absurdres, newest,".into()));
        modified = true;
    }
    let char_prompt = "In the next response provide only a detailed comma-delimited list of English Danbooru/anime tags describing {{char}} (e.g. 1girl, solo, character name, species, hair, eyes, body features). Always output strictly in English tags, never in Russian. If the user requested an erotic, nude, or NSFW picture (e.g. nude, naked, breasts, erotic), output tags 'nude, naked, breasts, uncensored' and omit clothing. Do not write full sentences or thinking blocks. Prefix your description with the phrase 'full body portrait,'";
    // The mark lies on sd, not inside prompts: ST would take an extra key as a template.
    let desired = serde_json::Value::String(char_prompt.into());
    let current = sd_obj.get("prompts").and_then(|prompts| prompts.get("0")).cloned();
    let marked = sd_obj.get(LK_SD_OURS).and_then(|m| m.get("prompt_0")).cloned();
    let blank = match &current {
        None => true,
        Some(value) => value.as_str().is_none_or(str::is_empty),
    };
    let replace = blank || current.as_ref() == Some(&desired) || marked == current;
    if replace {
        if current.as_ref() != Some(&desired) {
            let prompts = sd_obj.entry("prompts").or_insert_with(|| serde_json::json!({}));
            if let Some(p) = prompts.as_object_mut() {
                p.insert("0".into(), desired.clone());
                modified = true;
            }
        }
        if marked.as_ref() != Some(&desired) {
            let marker = sd_obj.entry(LK_SD_OURS).or_insert_with(|| serde_json::json!({}));
            if !marker.is_object() {
                *marker = serde_json::json!({});
            }
            if let Some(m) = marker.as_object_mut() {
                m.insert("prompt_0".into(), desired);
            }
            modified = true;
        }
    }
    modified
}

/// `extension_prompt_types.IN_CHAT` — the memory block inside the history at depth.
const MEMORY_POSITION_IN_CHAT: u64 = 1;
/// The memory block before the last two messages: on every turn it and they
/// are recalculated, not the whole history.
const MEMORY_DEPTH: u64 = 2;
/// ST's default `protect`: memory pulled messages already from the sixth from the end.
const ST_DEFAULT_PROTECT: u64 = 5;

/// How many recent messages the vector memory does not touch: everything in the context
/// window. ST pulls the found message out of the history into the memory block, and it
/// would leave the window from the middle — the prompt would change from that point every
/// turn. The calculation assumes a reply of at least ~128 tokens (an average roleplay one is longer).
fn memory_protect(context_tokens: u32) -> u64 {
    u64::from(context_tokens / 128)
}

/// A vector memory layout in which Ollama reuses the prompt
/// prefix. ST's default — the block right after the main prompt and `protect` 5:
/// the prompt changed from the start every turn and was recalculated entirely. We change the ST
/// default layout once (the `lk_protect` mark — what we wrote to
/// `protect`), the user's own layout — no; `protect` follows the context
/// until it is edited by hand.
fn sync_memory_layout(vec_obj: &mut serde_json::Map<String, serde_json::Value>, context_tokens: u32) -> bool {
    let protect = memory_protect(context_tokens);
    let current = vec_obj.get("protect").and_then(|v| v.as_u64());
    match vec_obj.get("lk_protect").and_then(|v| v.as_u64()) {
        None => {
            if vec_obj.get("position").and_then(|v| v.as_u64()).unwrap_or(0) == 0 {
                vec_obj.insert("position".into(), serde_json::json!(MEMORY_POSITION_IN_CHAT));
                vec_obj.insert("depth".into(), serde_json::json!(MEMORY_DEPTH));
            }
            if current.is_none_or(|p| p == ST_DEFAULT_PROTECT) {
                vec_obj.insert("protect".into(), serde_json::json!(protect));
            }
        }
        Some(ours) if ours != protect => {
            if current == Some(ours) {
                vec_obj.insert("protect".into(), serde_json::json!(protect));
            }
        }
        Some(_) => return false,
    }
    vec_obj.insert("lk_protect".into(), serde_json::json!(protect));
    true
}

/// Path for comparison: without the `\\?\` prefix and in lowercase.
fn compare_path(path: &std::path::Path) -> String {
    let s = path.to_string_lossy();
    s.strip_prefix(r"\\?\").unwrap_or(&s).to_lowercase()
}

/// Is this our SillyTavern server? We identify it strictly: by our node.exe together with
/// the path of our server.js or by the working directory — for SillyTavern it is always
/// our `st_dir()`. The substring check ("server.js", "sillytavern") killed
/// the user's unrelated node processes, while the check by our
/// node.exe alone missed a server started by the fallback on the system Node: its orphans
/// held port 8000, and every next start moved to a higher port.
fn is_our_st_process(
    name: &str,
    exe: Option<&std::path::Path>,
    cmd: &[String],
    cwd: Option<&std::path::Path>,
    node_exe: &std::path::Path,
    st_dir: &std::path::Path,
) -> bool {
    if !name.to_lowercase().contains("node") {
        return false;
    }
    // npm install/update from download_sillytavern/update_st works from the same st_dir
    // and matched both criteria below — kill_orphan_node_processes killed the live npm.
    if cmd.iter().any(|a| a.to_lowercase().contains("npm")) {
        return false;
    }
    let server_js = compare_path(&st_dir.join("server.js"));
    let is_st_server = cmd.is_empty()
        || cmd.iter().any(|arg| {
            let s = arg.to_lowercase();
            s == "server.js" || s.contains(&server_js)
        });
    // From the ST directory without a command line, the child node of a live npm install also runs:
    // an empty cmd here does not prove server.js while npm is alive.
    if cmd.is_empty() && NPM_PID.load(Ordering::SeqCst) != 0 {
        return false;
    }
    if cwd.map(compare_path).as_deref() == Some(&compare_path(st_dir)) {
        return is_st_server;
    }
    if exe.map(compare_path).as_deref() != Some(&compare_path(node_exe)) {
        return false;
    }
    // Our server starts as `<node.exe> server.js` from the ST directory.
    // sysinfo on Windows does not always return the command line (an empty list) —
    // then we identify the process by our node.exe alone.
    is_st_server
}

#[cfg(windows)]
pub fn kill_orphan_node_processes() {
    use sysinfo::System;

    let node_exe = node_exe();
    let st_dir = st_dir();

    let mut sys = System::new();
    for _ in 0..3 {
        // Without with_cmd/with_cwd, sysinfo 0.30 refreshes only memory/CPU/exe:
        // cmd and cwd stay empty, and an orphan with the system Node was never
        // identified — it held port 8000 after a hard exit.
        sys.refresh_processes_specifics(
            sysinfo::ProcessRefreshKind::new()
                .with_cmd(sysinfo::UpdateKind::OnlyIfNotSet)
                .with_cwd(sysinfo::UpdateKind::OnlyIfNotSet),
        );
        let mut to_kill = Vec::new();
        for (&pid, proc) in sys.processes() {
            if is_our_st_process(
                proc.name(),
                proc.exe(),
                proc.cmd(),
                proc.cwd(),
                &node_exe,
                &st_dir,
            ) {
                to_kill.push(pid);
            }
        }

        if to_kill.is_empty() {
            break;
        }

        for pid in to_kill {
            if let Some(proc) = sys.process(pid) {
                let _ = proc.kill();
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[cfg(not(windows))]
pub fn kill_orphan_node_processes() {}

/// Start `node server.js` and wait for HTTP 200. The child is killed in Drop.
/// The node output is written to `server.log` — without it ST failures are invisible
/// (stdout is hidden by the CREATE_NO_WINDOW flag).
pub async fn start(port: u16) -> Result<Child, String> {
    if SHUTTING_DOWN.load(Ordering::SeqCst) {
        return Err("приложение завершается".into());
    }
    // The function runs in an async task of the tauri runtime, while cleaning up node orphans
    // is blocking work (sysinfo sweeps and pauses between kills, V-63): we move it
    // to spawn_blocking so as not to occupy a tokio worker. From RunEvent::Exit the same
    // function is called synchronously — there the blocking is appropriate, and its
    // signature must not change (five calls in lib.rs, some from sync contexts).
    tokio::task::spawn_blocking(kill_orphan_node_processes)
        .await
        .map_err(|e| format!("зачистка node-сирот: {e}"))?;
    if let Err(e) = seed_russian_locale() {
        // Not fatal (unpacking/boot succeeded), but staying silent is not allowed: a locale patch
        // that fails to apply otherwise disables it without diagnostics (V-25).
        crate::boot_log(&format!("SillyTavern: {e}"));
    }
    let _ = seed_theme();
    let exe = node_exe_with_fallback().await;
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(st_dir().join("server.log"))
        .map_err(|e| format!("не удалось открыть server.log: {e}"))?;
    let mut cmd = Command::new(&exe);
    let mut path_env = exe.parent().map(|p| p.to_path_buf()).unwrap_or_else(node_dir).into_os_string();
    if let Some(existing_path) = std::env::var_os("PATH") {
        path_env.push(";");
        path_env.push(existing_path);
    }
    cmd.arg("server.js")
        .current_dir(st_dir())
        .env("PATH", path_env)
        .env("HOME", st_dir())
        .env("USERPROFILE", st_dir())
        // We do not pass HF_HOME and its neighbors: ST 1.19.0 keeps the transformers cache in
        // data/_cache hardcoded (src/transformers.js) and does not read these variables.
        // Models are not downloaded there — autoDownload is off, memory and vision go
        // through Ollama; Kokoro runs in the browser.
        // Gateway port: the auto-tagging server plugin reads it from the environment
        // (the port is dynamic and is not written to files).
        .env(
            "CLOUFAM_GATEWAY_PORT",
            crate::gateway::gateway_port(crate::gateway::ollama_port()).to_string(),
        )
        .stdout(Stdio::from(
            log_file.try_clone().map_err(|e| format!("server.log: {e}"))?,
        ))
        .stderr(Stdio::from(log_file));
    hide_console(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("не удалось запустить SillyTavern: {e}"))?;
    adopt_started(&mut child)?;

    // A closed port 127.0.0.1 Windows rejects not immediately but after ~2 s of SYN
    // retries: without a short connect-timeout, readiness was noticed up to 2.5 s late.
    // Loopback readiness probe (127.0.0.1): bypass the system proxy (otherwise
    // the user's machine proxy answers 503 instead of the local server).
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(200))
        .timeout(Duration::from_secs(2))
        .no_proxy()
        .build()
        .map_err(|e| e.to_string())?;
    let started = std::time::Instant::now();
    for _ in 0..240 {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            clear_starting_pid();
            return Err(format!("SillyTavern завершилась с кодом {status} на порту {port}"));
        }
        if let Ok(resp) = client.get(format!("http://127.0.0.1:{port}/")).send().await {
            if resp.status().is_success() {
                return Ok(child);
            }
        }
        sleep(Duration::from_millis(500)).await;
    }
    // An orphan is unacceptable: a dead ST would hold the port and interfere with the next start.
    clear_starting_pid();
    let _ = child.kill();
    let _ = child.wait();
    // Actual time: 240 probes of 0.5 s is 120 s only on connection
    // refusal, while a hung server eats up to 2 s of timeout on each.
    Err(format!(
        "SillyTavern не ответила за {} с на порту {port}",
        started.elapsed().as_secs()
    ))
}

/// A free port for ST: from min_port (not below 8000) up to 8019.
pub async fn find_port(min_port: u16) -> Result<u16, String> {
    let start = min_port.max(8000);
    for port in start..8020 {
        if tokio::net::TcpListener::bind(("127.0.0.1", port)).await.is_ok() {
            return Ok(port);
        }
    }
    Err(format!("не удалось найти свободный порт SillyTavern ({start}–8019)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_lts_version() {
        let json = serde_json::json!([
            { "version": "v26.2.0", "lts": false },
            { "version": "v24.20.0", "lts": "Krypton" },
            { "version": "v22.18.0", "lts": "Iron" }
        ]);
        assert_eq!(parse_lts_version(&json), Some("24.20.0".to_string()));

        let empty = serde_json::json!([]);
        assert_eq!(parse_lts_version(&empty), None);
    }

    /// config.yaml of an existing installation: our root keys are edited,
    /// user sections (thumbnails/backups) stay in place.
    #[test]
    fn test_patch_config_keeps_user_sections() {
        let text = "dataRoot: ./data\nlisten: true\nport: 8000\n\
                    thumbnails:\n  enabled: false\nbackups:\n  chat:\n    numberOfBackups: 50\n";
        let patched = patch_config(text, 8123);
        assert!(patched.contains("port: 8123"), "{patched}");
        assert!(!patched.contains("port: 8000"), "{patched}");
        assert!(patched.contains("listen: false"), "{patched}");
        assert!(patched.contains("numberOfBackups: 50"), "{patched}");
        assert!(patched.contains("enabled: false"), "{patched}");
    }

    /// Missing root keys are appended, a nested `listen:` is not
    /// taken for a root one.
    #[test]
    fn test_patch_config_appends_missing_root_keys() {
        let patched = patch_config("dataRoot: ./data\n", 8001);
        assert!(patched.contains("port: 8001"), "{patched}");
        assert!(patched.contains("enableServerPlugins: true"), "{patched}");
        let nested = patch_config("dataRoot: ./x\n  listen: true\n", 8000);
        assert!(nested.contains("  listen: true"), "{nested}");
        assert!(nested.lines().any(|l| l == "listen: false"), "{nested}");
    }

    /// The vectors threshold and Summarize parameters are ST knobs: a written value
    /// is not overwritten by sync, a missing one is seeded.
    #[test]
    fn test_sync_keeps_user_vector_and_memory_settings() {
        let mut st = serde_json::json!({
            "extension_settings": {
                "vectors": { "score_threshold": 0.1 },
                "memory": { "source": "extras", "promptInterval": 25 }
            }
        });
        let settings = crate::settings::Settings::default();
        let obj = st.as_object_mut().unwrap();
        sync_st_object(obj, &settings, 11434, false);
        let ext = &obj["extension_settings"];
        assert_eq!(ext["vectors"]["score_threshold"], 0.1);
        assert_eq!(ext["memory"]["source"], "extras");
        assert_eq!(ext["memory"]["promptInterval"], 25);
        assert_eq!(ext["vectors"]["insert"], settings.memory_max_chunks);
    }

    #[test]
    fn test_sync_settings_logic() {
        let mut st = serde_json::json!({
            "max_context": 4096,
            "oai_settings": {
                "custom_model": "old-model",
                "openai_max_context": 4096,
                "temp_openai": 0.6
            }
        });
        let settings = crate::settings::Settings {
            chat_model: "huihui_ai/qwen3-abliterated:14b".into(),
            context_size: 16384,
            temperature: 0.85,
            ..Default::default()
        };

        let obj = st.as_object_mut().unwrap();
        let ctx = settings.context_size.max(8192);
        obj.insert("max_context".into(), serde_json::json!(ctx));

        {
            let temp = (settings.temperature as f64 * 100.0).round() / 100.0;
            let oai_obj = obj.get_mut("oai_settings").unwrap().as_object_mut().unwrap();
            oai_obj.insert("custom_model".into(), serde_json::json!(settings.chat_model));
            oai_obj.insert("openai_max_context".into(), serde_json::json!(ctx));
            oai_obj.insert("temp_openai".into(), serde_json::json!(temp));
            assert_eq!(oai_obj["custom_model"], "huihui_ai/qwen3-abliterated:14b");
            assert_eq!(oai_obj["openai_max_context"], 16384);
            assert_eq!(oai_obj["temp_openai"], 0.85);
        }

        assert_eq!(obj["max_context"], 16384);
    }

    /// V-28: OpenAI presets — "do not touch the user's choice". Threshold
    /// overwrites (`temperature <= 0.65` → 0.75, `repetition_penalty <= 1.05`
    /// → 1.18, penalties == 0) returned the user's deliberate values to
    /// ours on every boot and every settings save.
    #[test]
    fn test_openai_preset_keeps_user_sampler_choices() {
        let mut preset = serde_json::json!({
            "temperature": 0.2,
            "repetition_penalty": 1.0,
            "frequency_penalty": 0.0,
            "presence_penalty": 0.0,
            "openai_max_context": 4096,
        });
        let obj = preset.as_object_mut().unwrap();
        assert!(!patch_openai_preset_object(obj), "записанные значения — выбор пользователя");
        assert_eq!(obj["temperature"], 0.2);
        assert_eq!(obj["repetition_penalty"], 1.0);
        assert_eq!(obj["frequency_penalty"], 0.0);
        assert_eq!(obj["presence_penalty"], 0.0);
        assert_eq!(obj["openai_max_context"], 4096);
    }

    /// A missing key = the choice has not been made yet: seed with our defaults.
    #[test]
    fn test_openai_preset_fills_only_missing_keys() {
        let mut preset = serde_json::json!({ "temperature": 0.2 });
        let obj = preset.as_object_mut().unwrap();
        assert!(patch_openai_preset_object(obj));
        assert_eq!(obj["temperature"], 0.2, "существующий ключ не перезаписывается");
        assert_eq!(obj["repetition_penalty"], 1.18);
        assert_eq!(obj["frequency_penalty"], 0.2);
        assert_eq!(obj["presence_penalty"], 0.1);
        assert_eq!(obj["openai_max_context"], 8192);
    }

    /// Prompts follow the "text marked as ours" policy: the stock English ST
    /// text is replaced with the roleplay one, the user's own text remains untouched.
    #[test]
    fn test_openai_preset_replaces_only_stock_prompts() {
        let mut stock = serde_json::json!({
            "prompts": [
                { "identifier": "main", "content": "Write {{char}}'s next reply in a fictional chat between {{char}} and {{user}}." },
                { "identifier": "jailbreak", "content": "свои правила пользователя" },
            ]
        });
        let obj = stock.as_object_mut().unwrap();
        assert!(patch_openai_preset_object(obj));
        assert_eq!(obj["prompts"][0]["content"], serde_json::json!(ROLEPLAY_MAIN_PROMPT));
        assert_eq!(obj["prompts"][1]["content"], "свои правила пользователя");

        let mut custom = serde_json::json!({
            "temperature": 0.75,
            "repetition_penalty": 1.18,
            "frequency_penalty": 0.2,
            "presence_penalty": 0.1,
            "openai_max_context": 8192,
            "prompts": [{ "identifier": "main", "content": "свой главный промпт" }],
        });
        let obj = custom.as_object_mut().unwrap();
        assert!(!patch_openai_preset_object(obj), "свой промпт и заполненные ключи не трогаем");
        assert_eq!(obj["prompts"][0]["content"], "свой главный промпт");
    }

    /// V-29: the ComfyUI block does not overwrite the user's choice — one's own sampler, CFG,
    /// workflow, checkpoint, sizes, prefix and substitution survive
    /// synchronization, which runs on every settings save and boot.
    #[test]
    fn test_sd_sync_keeps_user_comfy_choices() {
        let mut sd = serde_json::json!({
            "source": "comfy",
            "comfy_url": "http://127.0.0.1:8188",
            "comfy_type": "standard",
            "comfy_workflow": "My_Flow.json",
            "model": "custom.safetensors",
            "width": 512,
            "height": 512,
            "steps": 12,
            "scale": 9.5,
            "sampler": "DPM++ 2M Karras",
            "interactive_mode": false,
            "function_tool": false,
            "tool_visible": false,
            "wand_visible": false,
            "prompt_prefix": "моя префиксная строка,",
            "negative_prompt": "мои негативные теги",
            "prompts": { "0": "моя подстановка" },
        });
        let sd_obj = sd.as_object_mut().unwrap();
        let settings = crate::settings::Settings::default();
        assert!(!sync_sd_settings(sd_obj, &settings, true), "весь блок заполнен своими значениями");
        assert_eq!(sd_obj["comfy_workflow"], "My_Flow.json");
        assert_eq!(sd_obj["model"], "custom.safetensors");
        assert_eq!(sd_obj["width"], 512);
        assert_eq!(sd_obj["height"], 512);
        assert_eq!(sd_obj["steps"], 12);
        assert_eq!(sd_obj["scale"], 9.5);
        assert_eq!(sd_obj["sampler"], "DPM++ 2M Karras");
        assert_eq!(sd_obj["interactive_mode"], false);
        assert_eq!(sd_obj["prompt_prefix"], "моя префиксная строка,");
        assert_eq!(sd_obj["negative_prompt"], "мои негативные теги");
        assert_eq!(sd_obj["prompts"]["0"], "моя подстановка");
    }

    /// A phrase from our template inside someone else's text is still someone else's text.
    #[test]
    fn test_sd_sync_keeps_user_prompt_that_quotes_our_phrase() {
        let mut sd = serde_json::json!({
            "source": "comfy",
            "comfy_url": "http://127.0.0.1:8188",
            "comfy_type": "standard",
            "negative_prompt": "worst quality, my own extra tag",
            "prompts": { "0": "Danbooru/anime tags describing {{char}} but my ending" },
        });
        let sd_obj = sd.as_object_mut().unwrap();
        let _ = sync_sd_settings(sd_obj, &crate::settings::Settings::default(), true);
        assert_eq!(sd_obj["negative_prompt"], "worst quality, my own extra tag");
        assert_eq!(
            sd_obj["prompts"]["0"],
            "Danbooru/anime tags describing {{char}} but my ending"
        );
    }

    /// An empty block is seeded with our values once and marked with
    /// `lk_sd_ours`; a model change in the application settings reached ST while
    /// the model and sizes remain ours.
    #[test]
    fn test_sd_sync_seeds_missing_keys_and_follows_our_model_choice() {
        let settings = crate::settings::Settings::default();
        let (w, h, steps) = sd_target_size(&settings.image_model);
        let mut sd = serde_json::json!({});
        let sd_obj = sd.as_object_mut().unwrap();
        assert!(sync_sd_settings(sd_obj, &settings, true));
        assert_eq!(sd_obj["sampler"], "Euler a");
        assert_eq!(sd_obj["scale"], 6.0);
        assert_eq!(sd_obj["comfy_workflow"], "Default_Comfy_Workflow.json");
        assert_eq!(sd_obj["model"].as_str(), Some(settings.image_model.as_str()));
        assert_eq!(sd_obj["width"], w);
        assert_eq!(sd_obj["height"], h);
        assert_eq!(sd_obj["steps"], steps);
        assert_eq!(sd_obj[LK_SD_OURS]["model"].as_str(), Some(settings.image_model.as_str()));

        assert!(!sync_sd_settings(sd_obj, &settings, true), "второй проход идемпотентен");

        // Choosing a model in the Cloufam window keeps reaching ST.
        let mut tuned = crate::settings::Settings::default();
        tuned.image_model = "other.safetensors".into();
        let (w2, h2, steps2) = sd_target_size(&tuned.image_model);
        assert!(sync_sd_settings(sd_obj, &tuned, true));
        assert_eq!(sd_obj["model"], "other.safetensors");
        assert_eq!(sd_obj["width"], w2);
        assert_eq!(sd_obj["height"], h2);
        assert_eq!(sd_obj["steps"], steps2);
    }

    /// One's own size under our model is also the user's choice: derived
    /// values do not overwrite it while it does not match what we wrote.
    #[test]
    fn test_sd_sync_keeps_custom_size_with_our_model() {
        let settings = crate::settings::Settings::default();
        let mut sd = serde_json::json!({});
        let sd_obj = sd.as_object_mut().unwrap();
        assert!(sync_sd_settings(sd_obj, &settings, true));
        sd_obj.insert("width".into(), serde_json::json!(640));
        assert!(!sync_sd_settings(sd_obj, &settings, true));
        assert_eq!(sd_obj["width"], 640);
    }

    /// The same logic as in `sync_st_settings` for `extension_settings.tts.Kokoro.device`:
    /// we set webgpu only if the key is completely absent. Any written value —
    /// is already the user's choice in the Kokoro panel.
    fn kokoro_device_after_sync(current: Option<&str>) -> String {
        match current {
            None => "webgpu".to_string(),
            Some(device) => device.to_string(),
        }
    }

    #[test]
    fn test_textgen_preset_json_carries_tuned_values() {
        let preset = textgen_preset_json(0.75, 0.9, 40, 1.12, 512);
        assert_eq!(preset["temp"], 0.75);
        assert_eq!(preset["top_p"], 0.9);
        assert_eq!(preset["top_k"], 40);
        assert_eq!(preset["rep_pen"], 1.12);
        assert_eq!(preset["rep_pen_range"], 512);
        // The rest of the schema is a valid ST sampler preset, not half a JSON.
        assert!(preset.get("sampler_priority").is_some());
        assert!(preset.get("samplers").is_some());
        assert!(preset.get("dry_sequence_breakers").is_some());
    }

    /// An already open ST page keeps the auto-tagging config in its own JS cache,
    /// loaded once at start, and the reload hotkeys in WebView2
    /// are disabled — without an explicit re-request, the "Apply tags
    /// automatically" toggle turned off in the settings window did not reach the open
    /// page (symptom: the checkbox is cleared in the settings, but new characters
    /// still get tags). Further behavior can only be checked in
    /// a live ST — here we record that the call reached the source.
    /// The thoughts script is enabled and changes only the display: the text of the message itself
    /// stays with tildes, otherwise the model would stop seeing its own format.
    #[test]
    fn test_thought_regex_ships_enabled_and_display_only() {
        let script = thought_regex_script();
        assert_eq!(script["disabled"], false);
        assert_eq!(script["markdownOnly"], true);
        assert_eq!(script["promptOnly"], false);
        // 2 — AI_OUTPUT: character reply, not user input.
        assert_eq!(script["placement"], serde_json::json!([2]));
        assert!(script["replaceString"].as_str().unwrap().contains("lk-thought"));
    }

    #[test]
    fn test_thought_regex_added_once_and_user_copy_untouched() {
        let mut ext = serde_json::Map::new();
        assert!(ensure_thought_regex(&mut ext), "первый проход добавляет скрипт");
        assert!(!ensure_thought_regex(&mut ext), "второй проход дубликата не делает");
        let list = ext["regex"].as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(ext["regex"][0]["disabled"], false);

        // The user disabled the script — the catch-up must not enable it back.
        ext["regex"].as_array_mut().unwrap()[0]["disabled"] = serde_json::json!(true);
        assert!(!ensure_thought_regex(&mut ext));
        assert_eq!(ext["regex"][0]["disabled"], true);
    }

    /// Copies before 25.09.2026 carry the script disabled and without the mark: we enable it once,
    /// we do not touch fields edited by the user.
    #[test]
    fn test_thought_regex_old_disabled_copy_switched_on_once() {
        let mut old = thought_regex_script();
        old["disabled"] = serde_json::json!(true);
        old["scriptName"] = serde_json::json!("мои мысли");
        let mut ext = serde_json::Map::new();
        ext.insert("regex".into(), serde_json::json!([old]));

        assert!(ensure_thought_regex(&mut ext));
        assert_eq!(ext["regex"][0]["disabled"], false);
        assert_eq!(ext["regex"][0]["scriptName"], "мои мысли");
        assert_eq!(ext[THOUGHTS_ON_MARK], true);

        ext["regex"].as_array_mut().unwrap()[0]["disabled"] = serde_json::json!(true);
        assert!(!ensure_thought_regex(&mut ext), "метка стоит — выбор пользователя не перебиваем");
        assert_eq!(ext["regex"][0]["disabled"], true);
    }

    #[test]
    fn existing_single_world_info_is_activated_once() {
        let mut settings = serde_json::json!({
            "world_info_settings": { "world_info": { "globalSelect": [] } },
            "extension_settings": {},
        });
        let settings = settings.as_object_mut().unwrap();

        assert!(ensure_existing_world_info_activation(settings, &["Eldoria".into()]));
        assert_eq!(
            settings["world_info_settings"]["world_info"]["globalSelect"],
            serde_json::json!(["Eldoria"])
        );
        assert_eq!(settings["extension_settings"][WORLD_INFO_ACTIVATION_MARK], true);

        settings["world_info_settings"]["world_info"]["globalSelect"] = serde_json::json!([]);
        assert!(!ensure_existing_world_info_activation(settings, &["Eldoria".into()]));
        assert_eq!(
            settings["world_info_settings"]["world_info"]["globalSelect"],
            serde_json::json!([])
        );
    }

    #[test]
    fn existing_multiple_world_infos_are_not_enabled_globally() {
        let mut settings = serde_json::json!({
            "world_info_settings": { "world_info": { "globalSelect": [] } },
            "extension_settings": {},
        });
        let settings = settings.as_object_mut().unwrap();
        assert!(ensure_existing_world_info_activation(settings, &["A".into(), "B".into()]));
        assert_eq!(
            settings["world_info_settings"]["world_info"]["globalSelect"],
            serde_json::json!([])
        );
    }

    #[test]
    fn missing_world_directory_does_not_finish_activation_migration() {
        let mut settings = serde_json::json!({ "extension_settings": {} });
        let settings = settings.as_object_mut().unwrap();
        assert!(!ensure_existing_world_info_activation(settings, &[]));
        assert!(settings["extension_settings"].get(WORLD_INFO_ACTIVATION_MARK).is_none());
    }

    /// The thoughts style must be in the injected theme, otherwise the script would return
    /// a span without styling and the difference would not be visible.
    /// ST renames classes in the message body to `custom-<name>` (the
    /// DOMPurify hook in public/scripts/chats.js), so the rule must be
    /// written for `custom-lk-thought` — otherwise the highlighting silently does not work.
    /// One substring is not enough: in 1.2.48 the first comment closed too
    /// early, the tail of the text got into the `.lk-thought, .custom-lk-thought` prelude,
    /// and the browser dropped the rule entirely — while contains() passed.
    #[test]
    fn test_thought_style_is_in_injected_css() {
        let preludes = css_rule_preludes(CLOUFAM_THEME_CSS);
        assert!(
            preludes.iter().any(|p| p.split(',').any(|s| s.trim() == ".custom-lk-thought")),
            "правило .custom-lk-thought не живое: {preludes:?}"
        );
    }

    /// All the CSS that goes to the ST page: balanced comments and
    /// no text before the selectors.
    #[test]
    fn test_injected_css_has_no_stray_text() {
        for (name, css) in [
            ("CLOUFAM_THEME_CSS", CLOUFAM_THEME_CSS),
            ("models_menu/panel.css", MODELS_MENU_CSS),
            ("theme/glass.css", GLASS_CSS),
        ] {
            let preludes = css_rule_preludes(css);
            assert!(!preludes.is_empty(), "{name}: ни одного правила");
        }
    }

    /// Preludes of CSS rules (text before `{`) without comments. Panics on
    /// an unclosed comment, on `*/` outside a comment and on a prelude that
    /// does not look like a selector or at-rule (Cyrillic, backticks outside quotes).
    fn css_rule_preludes(css: &str) -> Vec<String> {
        let css = match (css.trim_start().starts_with("<style"), css.find('>'), css.rfind("</style>")) {
            (true, Some(open), Some(close)) => &css[open + 1..close],
            _ => css,
        };
        let mut plain = String::new();
        let mut rest = css;
        while let Some(start) = rest.find("/*") {
            plain.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let end = after.find("*/").expect("незакрытый комментарий CSS");
            rest = &after[end + 2..];
        }
        plain.push_str(rest);
        assert!(!plain.contains("*/"), "`*/` вне комментария — комментарий закрыт раньше времени");

        let mut preludes = Vec::new();
        let mut current = String::new();
        for c in plain.chars() {
            match c {
                '{' => {
                    let prelude = current.trim().to_string();
                    let unquoted: String = prelude
                        .split(['"', '\''])
                        .step_by(2)
                        .collect();
                    assert!(
                        !prelude.is_empty()
                            && unquoted.chars().all(|ch| {
                                ch.is_ascii_alphanumeric() || ch.is_whitespace() || "-_#.,:>+~*()[]=@%!^$|/\\".contains(ch)
                            }),
                        "прелюдия правила не похожа на селектор: {prelude:?}"
                    );
                    preludes.push(prelude);
                    current.clear();
                }
                '}' | ';' => current.clear(),
                _ => current.push(c),
            }
        }
        assert!(current.trim().is_empty(), "текст после последнего правила: {:?}", current.trim());
        preludes
    }

    #[test]
    fn test_parse_latest_release_tag_strips_v_prefix() {
        let release = serde_json::json!({ "tag_name": "v1.20.3" });
        assert_eq!(parse_latest_release_tag(&release).as_deref(), Some("1.20.3"));
    }

    #[test]
    fn test_parse_latest_release_tag_rejects_empty_or_missing() {
        assert_eq!(parse_latest_release_tag(&serde_json::json!({})), None);
        assert_eq!(
            parse_latest_release_tag(&serde_json::json!({ "tag_name": "  " })),
            None
        );
    }

    #[test]
    fn test_st_needs_update_compares_against_pinned_version() {
        assert!(st_needs_update("1.18.0"));
        assert!(!st_needs_update(ST_VERSION));
        assert!(st_needs_update(""));
        assert!(st_needs_update("мусор"));
    }

    /// Rolling the application back to a release with an older ST pin must not
    /// lay the old zip over a newer installation.
    #[test]
    fn test_st_needs_update_never_downgrades() {
        assert!(!st_needs_update("1.20.0"));
        assert!(!st_needs_update("2.0"));
        assert!(!st_needs_update("1.19"), "1.19 == 1.19.0");
        assert!(st_needs_update("1.18.9"));
        assert!(st_needs_update("1.9.0"), "сравнение числовое, а не строковое");
    }

    /// An installation without a marker is always the historical 1.18.0: the only
    /// version Cloufam downloaded before the marker file appeared. The real
    /// path is read by `st_dir()`, here only the behavior in its absence matters.
    #[test]
    fn test_installed_st_version_defaults_to_1_18_0_without_marker() {
        // st_version_marker_path() depends on st_dir(), which in the test
        // environment almost certainly does not contain .lk-st-version — it makes direct sense
        // to check the default itself without chasing the file.
        let path = st_version_marker_path();
        if path.exists() {
            return; // on a machine with a real ST installation — not our case.
        }
        assert_eq!(installed_st_version(), "1.18.0");
    }

    #[test]
    fn test_push_settings_reloads_tagger_config_on_open_page() {
        let source = include_str!("sillytavern.rs");
        assert!(
            source.contains("window.__lkReloadTaggerConfig && window.__lkReloadTaggerConfig();"),
            "push_settings_to_client должен дёргать перезагрузку конфига тегов"
        );
    }

    #[test]
    fn test_tagger_extension_exposes_reload_hook() {
        let extension_source = include_str!("../tagger/extension-index.js");
        // The hook in the extension is formatted across several lines, so the comparison goes
        // by text without whitespace: searching for the exact one-line entry failed when
        // the hook was in place.
        let glued: String = extension_source.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            glued.contains("window.__lkReloadTaggerConfig=()=>{loadConfig();};"),
            "расширение автотегов должно принимать команду на перезагрузку конфига из Rust"
        );
    }

    #[test]
    fn test_kokoro_device_defaults_to_webgpu() {
        assert_eq!(kokoro_device_after_sync(None), "webgpu");
    }

    /// "WebAssembly (CPU)" in the Kokoro panel is chosen deliberately — on machines
    /// where WebGPU fails. Synchronization runs on every settings save,
    /// so resetting it would mean CPU cannot be chosen in principle.
    #[test]
    fn test_kokoro_device_preserves_user_choice() {
        assert_eq!(kokoro_device_after_sync(Some("wasm")), "wasm");
        assert_eq!(kokoro_device_after_sync(Some("webgpu")), "webgpu");
        assert_eq!(kokoro_device_after_sync(Some("wasm-simd")), "wasm-simd");
    }

    #[test]
    fn test_patch_cloufam_ui_fresh_insert() {
        let html = "<html><body>привет</body></html>";
        let result = patch_cloufam_ui_block(html, "<script>console.log(1)</script>").unwrap();
        assert!(result.contains(UI_BLOCK_START));
        assert!(result.contains("console.log(1)"));
        assert!(result.contains(UI_BLOCK_END));
        assert!(result.ends_with("</html>"));
    }

    #[test]
    fn test_patch_cloufam_ui_idempotent() {
        let html = "<html><body></body></html>";
        let block = "<script>stable</script>";
        let patched = patch_cloufam_ui_block(html, block).unwrap();
        assert!(patch_cloufam_ui_block(&patched, block).is_none());
    }

    #[test]
    fn test_patch_cloufam_ui_legacy_replaced() {
        let legacy = r#"<html><body><script id="cloufam-settings-injected">old code</script></body></html>"#;
        let result = patch_cloufam_ui_block(legacy, "<script>new</script>").unwrap();
        assert!(!result.contains("old code"));
        assert!(result.contains(UI_BLOCK_START));
        assert!(result.contains("new"));
    }

    #[test]
    fn test_patch_cloufam_ui_outdated_block_replaced() {
        let html = "<html><body></body></html>";
        let a = patch_cloufam_ui_block(html, "<script>A</script>").unwrap();
        let b = patch_cloufam_ui_block(&a, "<script>B</script>").unwrap();
        assert!(b.contains(UI_BLOCK_START));
        assert!(b.contains("<script>B</script>"));
        assert!(!b.contains("<script>A</script>"));
    }

    #[test]
    fn test_patch_cloufam_ui_no_body() {
        let html = "<html><div>no body</div></html>";
        let result = patch_cloufam_ui_block(html, "<script>x</script>").unwrap();
        assert!(result.contains(UI_BLOCK_START));
        assert!(result.contains("x"));
    }

    #[test]
    fn test_theme_json_valid() {
        let val: serde_json::Value = serde_json::from_str(CLOUFAM_THEME_JSON).expect("valid theme json");
        assert_eq!(val["name"], "Cloufam");
        // The glass relies on blur: fast_ui_mode and noShadows turn it off.
        assert_eq!(val["fast_ui_mode"], false);
        assert_eq!(val["noShadows"], false);
    }

    /// The theme must be laid out over power_user: ST does not apply the theme
    /// name at start (applyTheme is called only from the UI), but reads the values from there.
    #[test]
    fn test_theme_merges_into_power_user() {
        let mut power = serde_json::Map::new();
        power.insert("theme".into(), serde_json::Value::String("Dark Lite".into()));
        power.insert("blur_strength".into(), serde_json::json!(10));

        assert!(merge_theme_into_power_user(&mut power), "первый проход обязан менять настройки");
        assert_eq!(power["theme"], "Cloufam");
        assert!(power["custom_css"].as_str().unwrap().contains("#chat .mes"));
        // The theme's name key goes to the theme field and does not leak into power_user.
        assert!(power.get("name").is_none());

        assert!(!merge_theme_into_power_user(&mut power), "повторный проход не должен ничего менять");
    }

    #[test]
    fn test_theme_sapphire_blue_colors() {
        let val: serde_json::Value = serde_json::from_str(CLOUFAM_THEME_JSON).expect("valid theme json");
        assert_eq!(val["italics_text_color"], "rgba(186, 200, 246, 1)");
        assert_eq!(val["quote_text_color"], "rgba(147, 197, 253, 1)");
    }

    /// The styling contract: geometry is taken from Moonlit Echoes one-to-one, ours are
    /// only the colors and the material of the surfaces. The test catches an attempt to "tweak" the paddings,
    /// radii or chat width for oneself.
    #[test]
    fn test_theme_keeps_moonlit_geometry_and_only_recolors() {
        let ours: serde_json::Value = serde_json::from_str(CLOUFAM_THEME_JSON).unwrap();
        let theirs: serde_json::Value = serde_json::from_str(
            MOONLIT_FILES
                .iter()
                .find(|(rel, _)| *rel == "theme/Moonlit Echoes - by Rivelle.json")
                .expect("пресет автора обязан быть вшит")
                .1,
        )
        .unwrap();

        for key in ["blur_strength", "chat_width", "font_scale", "shadow_width"] {
            assert_eq!(ours[key], theirs[key], "геометрия {key} разошлась с темой автора");
        }
        // Two deliberate differences from the author's preset: the message style Echo
        // (the author's is Whisper) and round avatars, as in his own theme file.
        assert_eq!(ours["chat_display"], 3);
        assert_eq!(theirs["chat_display"], 4);
        assert_eq!(ours["avatar_style"], 0);
        assert_eq!(theirs["avatar_style"], 0);

        // Our material adds neither borders, nor radii, nor avatar outlines.
        let css = ours["custom_css"].as_str().unwrap();
        let material = css.split(CUSTOM_CSS_MARKER).nth(1).expect("маркер материала");
        for forbidden in ["border-radius", "inset 0 0 0 1px", "padding", "margin", ".avatar"] {
            assert!(!material.contains(forbidden), "материал трогает {forbidden}");
        }
        assert!(material.contains("backdrop-filter"));
    }

    /// The user's manual custom_css is not our property: an update
    /// must leave it. We recognize ours by the marker.
    #[test]
    fn test_user_custom_css_survives_theme_update() {
        let mut power = serde_json::Map::new();
        power.insert("theme".into(), serde_json::Value::String("Cloufam".into()));
        power.insert("custom_css".into(), serde_json::json!("/* моё оформление */ .mes { color: red; }"));

        merge_theme_into_power_user(&mut power);
        assert_eq!(
            power["custom_css"], "/* моё оформление */ .mes { color: red; }",
            "чужой custom_css затёрт обновлением темы"
        );
        // Our own is updated in the process.
        power.insert("custom_css".into(), serde_json::json!(format!("/* {CUSTOM_CSS_MARKER} старая версия */")));
        merge_theme_into_power_user(&mut power);
        assert!(power["custom_css"].as_str().unwrap().contains("backdrop-filter"));
    }

    /// The Moonlit Echoes extension writes its CSS into
    /// power_user.custom_css on page load — this is not a manual user edit: we replace it,
    /// as if our previous glass were there.
    #[test]
    fn test_moonlit_extension_css_is_replaced_not_preserved() {
        let mut power = serde_json::Map::new();
        power.insert("theme".into(), serde_json::Value::String("Cloufam".into()));
        power.insert(
            "custom_css".into(),
            serde_json::json!("/* Moonlit Echoes */ .mes { backdrop-filter: blur(8px); }"),
        );
        merge_theme_into_power_user(&mut power);
        let new_css = power["custom_css"].as_str().unwrap();
        assert!(
            new_css.contains("Cloufam: жидкое стекло"),
            "CSS расширения Moonlit не заменён на Cloufam: {}",
            &new_css[..new_css.len().min(100)]
        );
    }

    /// The locale patch must append, not replace: upstream edits its
    /// Russian locale, and overwriting would roll back their fixes. The user's live translation
    /// is also not touched — we replace only the missing and the stubs.
    #[test]
    fn test_ru_locale_patch_only_fills_gaps() {
        let patch: serde_json::Value = serde_json::from_str(RU_LOCALE_PATCH).expect("valid patch json");
        let patch_obj = patch.as_object().expect("объект");
        assert!(patch_obj.len() > 400, "патч локали подозрительно мал: {}", patch_obj.len());
        assert_eq!(patch_obj.get("Vector Storage").and_then(|v| v.as_str()), Some("Векторное хранилище"));
        // Technical identifiers must not be translated.
        assert_eq!(patch_obj.get("REPLACE_GETVAR").and_then(|v| v.as_str()), Some("REPLACE_GETVAR"));
        assert_eq!(patch_obj.get("{{name}}").and_then(|v| v.as_str()), Some("{{name}}"));

        // Merge logic: empty or stub — replace, live translation — keep.
        let mut locale = serde_json::json!({
            "Vector Storage": "Vector Storage",
            "Top K": "Моё название",
        });
        let obj = locale.as_object_mut().unwrap();
        for (key, value) in patch_obj {
            let stale = match obj.get(key) {
                None => true,
                Some(serde_json::Value::String(cur)) => cur == key || cur.trim().is_empty(),
                Some(_) => false,
            };
            if stale {
                obj.insert(key.clone(), value.clone());
            }
        }
        assert_eq!(obj["Vector Storage"], "Векторное хранилище", "заглушка не заменена");
        assert_eq!(obj["Top K"], "Моё название", "живой перевод затёрт");
    }
    /// The Moonlit Echoes theme ships as files in the binary: without it, a user without
    /// git would have the extension installation silently fail.
    #[test]
    fn test_moonlit_theme_is_bundled() {
        assert!(MOONLIT_FILES.len() >= 30, "вшито слишком мало файлов темы");
        let manifest = MOONLIT_FILES
            .iter()
            .find(|(rel, _)| *rel == "manifest.json")
            .expect("манифест темы обязан быть вшит");
        assert!(manifest.1.contains("Moonlit Echoes Theme"));
        assert!(
            MOONLIT_FILES.iter().any(|(rel, _)| *rel == "LICENSE"),
            "AGPL-3.0 обязывает везти LICENSE вместе с кодом"
        );
        assert!(MOONLIT_FILES.iter().any(|(rel, _)| rel.starts_with("src/")));
        // The Russian locale of the theme panel is our only edit in foreign code.
        assert!(
            MOONLIT_FILES.iter().any(|(rel, _)| *rel == "i18n/ru-ru.json"),
            "русская локаль темы не вшита"
        );
        assert!(manifest.1.contains("\"ru-ru\""), "ru-ru не зарегистрирован в манифесте темы");
    }

    #[test]
    fn test_default_tags_cover_only_stock_cards() {
        let val: serde_json::Value = serde_json::from_str(DEFAULT_TAGS_JSON).expect("valid default tags json");
        let tags = val["tags"].as_array().expect("tags array");
        let tag_map = val["tag_map"].as_object().expect("tag_map object");
        assert!(tag_map.keys().all(|avatar| avatar.starts_with("default_")), "{:?}", tag_map.keys());
        for ids in tag_map.values() {
            for id in ids.as_array().unwrap() {
                assert!(tags.iter().any(|t| t["id"] == *id), "ссылка на несуществующий тег {id}");
            }
        }
    }

    /// The card set for markup tests: the baked-in catalog is empty, while the sets
    /// live outside the repository.
    fn test_catalog() -> serde_json::Value {
        serde_json::json!({
            "tags": [
                {"id": "t-zayka", "name": "зайка"},
                {"id": "t-geymer", "name": "геймер"},
                {"id": "t-otaku", "name": "отаку"}
            ],
            "tag_map": {
                "Alice.png": ["t-zayka"],
                "Bob.png": ["t-geymer", "t-otaku"]
            }
        })
    }

    #[test]
    fn test_clean_ui_no_breaking_overrides() {
        assert!(CLOUFAM_THEME_CSS.contains("#cloufam_wand_container"));
        for selector in ["#top-settings-holder", "#bg1", "#top-bar"] {
            assert!(!CLOUFAM_THEME_CSS.contains(selector), "инжект трогает {selector}");
            // The theme's custom_css goes to the same page: the ST interface has
            // already broken on these selectors (gotcha 1.2.7), we keep them out of the rules.
            assert!(
                !CLOUFAM_THEME_JSON.contains(selector),
                "custom_css темы трогает {selector}"
            );
        }
    }

    #[test]
    fn test_only_our_sillytavern_node_is_killed() {
        use std::path::Path;
        let node = Path::new(r"Z:\Cloufam\runtime\node\node.exe");
        let st = Path::new(r"Z:\Cloufam\sillytavern");

        let our = ["server.js".to_string()];
        assert!(is_our_st_process("node.exe", Some(node), &our, Some(st), node, st));
        // Fallback to the system Node: the exe is foreign, but the working directory is ours —
        // such orphans held port 8000 until the machine rebooted.
        let system_node = Path::new(r"C:\Program Files\nodejs\node.exe");
        assert!(is_our_st_process("node.exe", Some(system_node), &our, Some(st), node, st));
        // The user's foreign dev server: both the exe and the directory are not ours.
        let vite = ["vite.js".to_string(), "--port".to_string(), "5391".to_string()];
        let other = Path::new(r"Z:\Git\GameStudio\Games\overtime");
        assert!(!is_our_st_process("node.exe", Some(system_node), &vite, Some(other), node, st));
        // The command line does not always arrive on Windows — our node.exe
        // remains a sufficient criterion.
        assert!(is_our_st_process("node.exe", Some(node), &[], None, node, st));
        assert!(!is_our_st_process("ollama.exe", Some(node), &our, Some(st), node, st));
    }

    /// The markup update reaches an installed copy: the set gets
    /// new tags, the user's own tag on the same card remains, and the labels
    /// of the previous set leave the library filter.
    #[test]
    fn test_bundled_tags_refresh_keeps_user_tags() {
        let mut settings: serde_json::Map<String, serde_json::Value> = serde_json::from_value(
            serde_json::json!({
                "tags": [
                    {"id": "t-alice-old", "name": "alice-old"},
                    {"id": "user-1", "name": "мой тег"}
                ],
                "tag_map": {
                    "Alice.png": ["t-alice-old", "user-1"],
                    "MyOwn.png": ["user-1"]
                }
            }),
        )
        .unwrap();

        assert!(apply_tags_catalog(&mut settings, &test_catalog(), true));

        let alice = settings["tag_map"]["Alice.png"].as_array().unwrap();
        assert!(alice.iter().any(|v| v == "t-zayka"), "новая разметка: {alice:?}");
        assert!(alice.iter().any(|v| v == "user-1"), "свой тег остался: {alice:?}");
        assert!(!alice.iter().any(|v| v == "t-alice-old"), "старый тег снят: {alice:?}");
        assert_eq!(settings["tag_map"]["MyOwn.png"], serde_json::json!(["user-1"]));

        let names: Vec<&str> = settings["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"мой тег"), "чужой тег не удаляем");
        assert!(!names.contains(&"alice-old"), "мёртвый тег набора удалён");
    }

    /// Without a version bump the card markup is not touched: the user could
    /// have re-tagged the library themselves, and every start would overwrite their work.
    #[test]
    fn test_bundled_tags_without_refresh_keep_existing_markup() {
        let mut settings: serde_json::Map<String, serde_json::Value> = serde_json::from_value(
            serde_json::json!({
                "tags": [{"id": "user-1", "name": "мой тег"}],
                "tag_map": {"Alice.png": ["user-1"]}
            }),
        )
        .unwrap();

        apply_tags_catalog(&mut settings, &test_catalog(), false);

        assert_eq!(settings["tag_map"]["Alice.png"], serde_json::json!(["user-1"]));
    }

    /// A set tag could change its id while keeping the name ("геймер": t-bob-gamer →
    /// t-geymer). The "such a name already exists" check did not add the new tag, the old one
    /// left during cleanup, and tag_map pointed into the void: the UI showed
    /// bare identifiers instead of traits.
    #[test]
    fn test_bundled_tags_renumbered_tag_does_not_dangle() {
        let mut settings: serde_json::Map<String, serde_json::Value> = serde_json::from_value(
            serde_json::json!({
                "tags": [{"id": "t-bob-gamer", "name": "геймер"}],
                "tag_map": {"Bob.png": ["t-bob-gamer"]}
            }),
        )
        .unwrap();

        apply_tags_catalog(&mut settings, &test_catalog(), true);

        let known: std::collections::HashSet<&str> = settings["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap())
            .collect();
        for (avatar, ids) in settings["tag_map"].as_object().unwrap() {
            for id in ids.as_array().unwrap() {
                assert!(
                    known.contains(id.as_str().unwrap()),
                    "{avatar}: ссылка на несуществующий тег {id}"
                );
            }
        }
        let bob: Vec<&str> = settings["tag_map"]["Bob.png"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(bob.iter().any(|id| *id == "t-geymer"), "новая разметка: {bob:?}");
    }

    /// The user's own tag with the same name is the same tag: the set must take
    /// its id, not create a second tag with the same name.
    #[test]
    fn test_bundled_tags_reuse_user_tag_with_same_name() {
        let mut settings: serde_json::Map<String, serde_json::Value> = serde_json::from_value(
            serde_json::json!({
                "tags": [{"id": "user-gamer", "name": "геймер"}],
                "tag_map": {}
            }),
        )
        .unwrap();

        apply_tags_catalog(&mut settings, &test_catalog(), true);

        let names: Vec<&str> = settings["tags"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["name"] == "геймер")
            .map(|t| t["id"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["user-gamer"], "тег не должен задваиваться");
        let bob: Vec<&str> = settings["tag_map"]["Bob.png"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(bob.contains(&"user-gamer"), "разметка через тег пользователя: {bob:?}");
    }

    /// A set tag could change its name under the same id ("otaku" → "отаку"). Without
    /// updating the name, the user's library forever kept the label
    /// from the old set version.
    #[test]
    fn test_bundled_tags_rename_keeps_id() {
        let mut settings: serde_json::Map<String, serde_json::Value> = serde_json::from_value(
            serde_json::json!({
                "tags": [{"id": "t-otaku", "name": "otaku"}],
                "tag_map": {"Bob.png": ["t-otaku"]}
            }),
        )
        .unwrap();

        apply_tags_catalog(&mut settings, &test_catalog(), true);

        let renamed = settings["tags"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == "t-otaku")
            .expect("тег на месте");
        assert_eq!(renamed["name"], "отаку", "имя приведено к набору");
    }

    /// The insertion sites are checked against the sources of the pinned ST version: on a
    /// ST_VERSION bump the patch would silently stop applying. The copy lies in
    /// `.st-research` (outside git, CRLF after checkout) — without it the test is skipped.
    #[test]
    fn stream_tail_patch_matches_pinned_st() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join(".st-research")
            .join(format!("SillyTavern-{ST_VERSION}"))
            .join("public")
            .join("script.js");
        let Ok(raw) = std::fs::read_to_string(&path) else {
            eprintln!("пропуск: нет {}", path.display());
            return;
        };
        // A working installation is deployed from a codeload archive with LF.
        let script = raw.replace("\r\n", "\n");
        let patched = patch_stream_tail_source(&script).expect("оба места вставки есть в script.js");
        assert!(patched.contains("renderStreamingText(this.messageTextDom, processedText, formatText, isFinal);"));
        assert!(!patched.contains("formattedText"), "старая переменная больше нигде не используется");
        assert_eq!(patch_stream_tail_source(&patched), None, "повторный подъём не патчит дважды");
    }

    #[test]
    fn stream_tail_patch_is_all_or_nothing() {
        let both = format!("{STREAM_TAIL_IMPORT_FIND}\n{STREAM_TAIL_RENDER_FIND}");
        assert!(patch_stream_tail_source(&both).is_some());
        // An import without a call and a call without an import are equally unacceptable.
        assert_eq!(patch_stream_tail_source(STREAM_TAIL_IMPORT_FIND), None);
        assert_eq!(patch_stream_tail_source(STREAM_TAIL_RENDER_FIND), None);
        let twice = format!("{both}\n{STREAM_TAIL_RENDER_FIND}");
        assert_eq!(patch_stream_tail_source(&twice), None, "неоднозначное место не трогаем");
    }

    #[test]
    fn world_info_patch_activates_created_and_imported_books() {
        let source = [
            WORLD_INFO_CREATE_ENTRY_FIND,
            WORLD_INFO_CREATE_ACTIVATE_FIND,
            WORLD_INFO_IMPORT_ACTIVATE_FIND,
        ]
        .join("\n");
        let patched = patch_world_info_source(&source).expect("все три места должны патчиться вместе");

        assert!(patched.contains("createWorldInfoEntry(worldName, worldInfoTemplate)"));
        assert!(patched.contains("onWorldInfoChange({ state: 'on', silent: 'true' }, worldName)"));
        assert!(patched.contains("onWorldInfoChange({ state: 'on', silent: 'true' }, data.name)"));
        assert!(patched.contains("window.__lkTranslateWorldInfo?.(data.name)"));
        assert_eq!(patch_world_info_source(&patched), None, "повторный подъём не дублирует патч");
    }

    #[test]
    fn world_info_patch_is_all_or_nothing() {
        let incomplete = [WORLD_INFO_CREATE_ENTRY_FIND, WORLD_INFO_IMPORT_ACTIVATE_FIND].join("\n");
        assert_eq!(patch_world_info_source(&incomplete), None);

        let duplicate = [
            WORLD_INFO_CREATE_ENTRY_FIND,
            WORLD_INFO_CREATE_ACTIVATE_FIND,
            WORLD_INFO_IMPORT_ACTIVATE_FIND,
            WORLD_INFO_IMPORT_ACTIVATE_FIND,
        ]
        .join("\n");
        assert_eq!(patch_world_info_source(&duplicate), None);
    }

    /// Installed copies already carry the first version of the patch with "скинь" — the
    /// "patch is in place" check by this word did not let the fixed regex reach them.
    #[test]
    fn sd_activation_regex_upgrades_old_patch() {
        let line = |re: &str| format!("const messageTrigger = {{\n    {re}\n    specialCases: {{}},\n}};\n");

        let from_st = patch_sd_activation_regex(&line(SD_ACTIVATION_REGEX_ST)).expect("апстрим патчится");
        assert_eq!(from_st, line(SD_ACTIVATION_REGEX_RU));

        let from_v1 = patch_sd_activation_regex(&line(SD_ACTIVATION_REGEX_V1)).expect("старый патч заменяется");
        let from_v2 = patch_sd_activation_regex(&line(SD_ACTIVATION_REGEX_V2)).expect("прошлая русская заменяется");
        assert!(from_v2.contains(SD_ACTIVATION_REGEX_RU));
        assert_eq!(from_v1, line(SD_ACTIVATION_REGEX_RU));

        assert_eq!(patch_sd_activation_regex(&line(SD_ACTIVATION_REGEX_RU)), None, "повторно не патчим");
        assert_eq!(patch_sd_activation_regex("activationRegex: /other/i,"), None, "чужую версию ST не трогаем");
    }

    /// First seed: caption via the `multimodal` source with the Ollama API, memory
    /// IN_PROMPT, SD source — a value from the ST list.
    #[test]
    fn initial_settings_use_valid_st_values() {
        let settings = crate::settings::Settings {
            vision_model: "vision:tag".into(),
            ..Default::default()
        };
        let st = initial_st_settings(&settings, 11434, false);
        let ext = &st["extension_settings"];
        assert_eq!(ext["caption"]["source"], "multimodal");
        assert_eq!(ext["caption"]["multimodal_api"], "ollama");
        assert_eq!(ext["caption"]["multimodal_model"], "ollama_custom");
        assert_eq!(ext["caption"]["ollama_custom_model"], "vision:tag");
        assert_eq!(ext["memory"]["position"], 0, "extension_prompt_types.IN_PROMPT");
        assert_eq!(ext["vectors"]["position"], 1, "extension_prompt_types.IN_CHAT");
        assert_eq!(ext["vectors"]["depth"], 2);
        assert_eq!(ext["vectors"]["protect"], 64, "8192 токенов / 128");
        assert_eq!(ext["sd"]["source"], SD_SOURCE_UNAVAILABLE);
        assert_eq!(initial_st_settings(&settings, 11434, true)["extension_settings"]["sd"]["source"], "comfy");
    }

    fn synced_settings() -> crate::settings::Settings {
        crate::settings::Settings {
            chat_model: "chat:tag".into(),
            vision_model: "vision:tag".into(),
            embed_model: "embed:tag".into(),
            theme_version: THEME_VERSION,
            tags_version: TAGS_VERSION,
            ..Default::default()
        }
    }

    #[test]
    fn sync_sets_our_background_once_per_theme_version() {
        let mut st = serde_json::json!({"background": {"name": "bedroom clean.jpg", "url": "url(\"backgrounds/bedroom%20clean.jpg\")", "thumbnailColumns": 8}});
        let old = crate::settings::Settings { theme_version: THEME_VERSION - 1, ..synced_settings() };
        assert!(sync_st_object(st.as_object_mut().unwrap(), &old, 47120, false));
        assert_eq!(st["background"]["name"], CHAT_BACKGROUND_NAME);
        assert_eq!(st["background"]["url"], format!("url(\"backgrounds/{CHAT_BACKGROUND_NAME}\")"));
        assert_eq!(st["background"]["thumbnailColumns"], 8, "поля галереи сохраняются");

        st["background"]["name"] = serde_json::json!("mine.png");
        sync_json(&mut st, false);
        assert_eq!(st["background"]["name"], "mine.png", "выбор пользователя после подъёма версии не трогаем");
    }

    fn sync_json(st: &mut serde_json::Value, comfy_installed: bool) -> bool {
        // The Ollama port is away from 11434: the gateway port is picked by a bind probe,
        // and a live gateway or a parallel test must not shift it between runs.
        sync_st_object(st.as_object_mut().unwrap(), &synced_settings(), 47110, comfy_installed)
    }

    /// Live installations carry `source: "ollama"` (invalid) and `multimodal_api:
    /// "openai"` from the ST migration — image description did not work at all.
    #[test]
    fn sync_migrates_invalid_caption_source_only() {
        let mut st = serde_json::json!({"extension_settings": {"caption": {
            "source": "ollama", "multimodal_api": "openai", "multimodal_model": "ollama_custom"
        }}});
        assert!(sync_json(&mut st, false));
        let cap = &st["extension_settings"]["caption"];
        assert_eq!(cap["source"], "multimodal");
        assert_eq!(cap["multimodal_api"], "ollama");
        assert_eq!(cap["multimodal_model"], "ollama_custom");
        assert_eq!(cap["ollama_custom_model"], "vision:tag");

        // Synchronization does not touch the user's choice in the caption panel.
        let mut user = serde_json::json!({"extension_settings": {"caption": {
            "source": "local", "multimodal_api": "openai", "multimodal_model": "gpt-4o"
        }}});
        sync_json(&mut user, false);
        let cap = &user["extension_settings"]["caption"];
        assert_eq!(cap["source"], "local");
        assert_eq!(cap["multimodal_api"], "openai");
        assert_eq!(cap["multimodal_model"], "gpt-4o");
    }

    /// The group hint and penalties: we replace the ST default and our previous values,
    /// the user's edit — never (synchronization runs on every save).
    #[test]
    fn sync_keeps_user_nudge_and_penalties() {
        let mut st = serde_json::json!({"oai_settings": {
            "group_nudge_prompt": "[Моя подсказка для группы]",
            "repetition_penalty_openai": 1.0,
            "freq_pen_openai": 0.0,
            "pres_pen_openai": 0.0,
        }});
        sync_json(&mut st, false);
        let oai = &st["oai_settings"];
        assert_eq!(oai["group_nudge_prompt"], "[Моя подсказка для группы]");
        assert_eq!(oai["repetition_penalty_openai"], 1.0);
        assert_eq!(oai["freq_pen_openai"], 0.0);
        assert_eq!(oai["pres_pen_openai"], 0.0);

        for stale in [
            "",
            "[Write the next reply only as {{char}}.]",
            "[Отвечай только от лица {{char}} и пиши сразу текст реплики — без имени и без префикса «Имя:» в начале.]",
        ] {
            let mut st = serde_json::json!({"oai_settings": {"group_nudge_prompt": stale}});
            sync_json(&mut st, false);
            assert_eq!(st["oai_settings"]["group_nudge_prompt"], GROUP_NUDGE_PROMPT, "не заменён: {stale:?}");
        }

        let mut fresh = serde_json::json!({"oai_settings": {}});
        sync_json(&mut fresh, false);
        assert_eq!(fresh["oai_settings"]["repetition_penalty_openai"], 1.18);
        assert_eq!(fresh["oai_settings"]["freq_pen_openai"], 0.2);
        assert_eq!(fresh["oai_settings"]["pres_pen_openai"], 0.1);
    }

    /// The ST default (the memory block after the main prompt, protect 5) changed the prompt
    /// from the start every turn. We change it once; the user's layout — no.
    #[test]
    fn sync_moves_memory_into_chat_once() {
        for untouched in [
            serde_json::json!({"extension_settings": {"vectors": {"enabled_chats": true}}}),
            serde_json::json!({"extension_settings": {"vectors": {"position": 0, "depth": 2, "protect": 5}}}),
        ] {
            let mut st = untouched;
            assert!(sync_json(&mut st, false));
            let vectors = &st["extension_settings"]["vectors"];
            assert_eq!(vectors["position"], 1);
            assert_eq!(vectors["depth"], 2);
            assert_eq!(vectors["protect"], 64);

            // The user later returned memory after the main prompt — that is their choice.
            st["extension_settings"]["vectors"]["position"] = serde_json::json!(0);
            st["extension_settings"]["vectors"]["protect"] = serde_json::json!(5);
            sync_json(&mut st, false);
            assert_eq!(st["extension_settings"]["vectors"]["position"], 0);
            assert_eq!(st["extension_settings"]["vectors"]["protect"], 5);
        }

        let mut custom = serde_json::json!({"extension_settings": {"vectors": {"position": 2, "depth": 7, "protect": 20}}});
        sync_json(&mut custom, false);
        let vectors = &custom["extension_settings"]["vectors"];
        assert_eq!((vectors["position"].clone(), vectors["depth"].clone(), vectors["protect"].clone()), (2.into(), 7.into(), 20.into()));
    }

    /// `protect` follows the context until it is edited by hand.
    #[test]
    fn memory_protect_follows_context_until_edited() {
        let mut vectors = serde_json::Map::new();
        assert!(sync_memory_layout(&mut vectors, 8_192));
        assert!(!sync_memory_layout(&mut vectors, 8_192), "второй проход ничего не меняет");
        assert!(sync_memory_layout(&mut vectors, 32_768));
        assert_eq!(vectors["protect"], 256);

        vectors.insert("protect".into(), serde_json::json!(40));
        sync_memory_layout(&mut vectors, 16_384);
        assert_eq!(vectors["protect"], 40);
    }

    /// Without ComfyUI we clear only our SD source; the previous `none` — an invalid
    /// value — falls back to the ST default, a foreign source remains.
    #[test]
    fn sync_sd_source_without_comfy() {
        for ours in ["none", "comfy"] {
            let mut st = serde_json::json!({"extension_settings": {"sd": {"source": ours}}});
            assert!(sync_json(&mut st, false));
            assert_eq!(st["extension_settings"]["sd"]["source"], SD_SOURCE_UNAVAILABLE);
        }
        let mut user = serde_json::json!({"extension_settings": {"sd": {"source": "horde"}}});
        sync_json(&mut user, false);
        assert_eq!(user["extension_settings"]["sd"]["source"], "horde");
    }

    /// A second pass over an already synchronized file changes nothing — otherwise
    /// settings.json would be rewritten on every settings save.
    #[test]
    fn sync_is_idempotent() {
        // Our own port range: gateway probes of neighboring sync_json tests at the same
        // instant take ports near 47110 and would shift custom_url between runs.
        let sync = |st: &mut serde_json::Value, comfy: bool| {
            sync_st_object(st.as_object_mut().unwrap(), &synced_settings(), 47310, comfy)
        };
        for comfy in [true, false] {
            let mut st = serde_json::json!({});
            assert!(sync(&mut st, comfy));
            assert!(!sync(&mut st, comfy), "второй проход что-то поменял (comfy={comfy})");
        }
    }

    /// f32 from the settings must not go to ST as 0.8999999761581421, and the image
    /// size must not lag behind the family of the selected checkpoint.
    #[test]
    fn client_payload_rounds_samplers_and_carries_sd_size() {
        let settings = crate::settings::Settings {
            temperature: 0.9,
            top_p: 0.95,
            image_model: "anything-v5.safetensors".into(),
            ..Default::default()
        };
        let payload = client_settings_payload(&settings, true);
        assert_eq!(payload["temperature"].as_f64(), Some(0.9));
        assert_eq!(payload["top_p"].as_f64(), Some(0.95));
        assert_eq!(payload["sd_size"], serde_json::json!({"width": 512, "height": 768, "steps": 20}));
        assert!(client_settings_payload(&settings, false)["sd_size"].is_null(), "без ComfyUI размер не шлём");
    }

    /// Parallel writers of the same file (boot, settings window, model menu)
    /// must not leave it truncated: the shared `<name>.tmp` was truncated by one writer
    /// right before another's rename.
    #[test]
    fn write_if_changed_parallel_writers_never_truncate() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("lk-test-write-if-changed");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let contents: Vec<String> = (0..4).map(|i| format!("{{\"writer\":{i},\"pad\":\"{}\"}}", "x".repeat(200_000))).collect();

        std::thread::scope(|scope| {
            for content in &contents {
                let path = &path;
                scope.spawn(move || {
                    for _ in 0..20 {
                        // Windows may reject a rename over a file that another
                        // writer replaces at the same instant — this is not
                        // corruption, the file remains intact.
                        let _ = write_if_changed(path, content);
                    }
                });
            }
        });

        let result = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains(&result), "файл обрезан или смешан: {} байт", result.len());
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(leftovers.is_empty(), "tmp-файлы остались: {leftovers:?}");
    }

    /// `node --check` for the file; `None` — node is not in PATH.
    fn node_check(path: &std::path::Path) -> Option<Result<(), String>> {
        let out = match std::process::Command::new("node").arg("--check").arg(path).output() {
            Ok(out) => out,
            Err(_) => return None,
        };
        Some(if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).into_owned())
        })
    }

    fn node_check_dir(name: &str) -> std::path::PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The bodies of `<script>` in an HTML fragment.
    fn inline_scripts(html: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = html;
        while let Some(open) = rest.find("<script") {
            let tag = &rest[open..];
            let body_start = tag.find('>').expect("тег <script> не закрыт") + 1;
            let body = &tag[body_start..];
            let end = body.find("</script>").expect("нет </script>");
            out.push(body[..end].to_string());
            rest = &body[end + "</script>".len()..];
        }
        out
    }

    /// The JS that lives in Rust strings and lands in the ST page and files has never
    /// once gone through a parser: one bracket (or `{{` in format!) — and the whole
    /// Cloufam UI block is silently dead (gotcha 13.09). We check exactly what
    /// is written to disk.
    #[test]
    fn injected_scripts_pass_node_check() {
        let dir = node_check_dir("lk-node-check");
        let block = cloufam_ui_block(8123);
        let mut files: Vec<(String, String)> = inline_scripts(&block)
            .into_iter()
            .enumerate()
            .map(|(i, body)| (format!("ui-block-{i}.cjs"), body))
            .collect();
        assert_eq!(files.len(), 2, "в блоке UI два скрипта: настройки и меню моделей");
        files.extend(inline_scripts(RU_LANGUAGE_HEAD_SCRIPT).into_iter().map(|body| ("head-language.cjs".to_string(), body)));
        let regex_literal = SD_ACTIVATION_REGEX_RU.trim_start_matches("activationRegex:").trim_end_matches(',');
        files.push(("sd-regex.cjs".into(), format!("const activationRegex = {regex_literal};\n")));
        files.push(("stream-tail-import.mjs".into(), STREAM_TAIL_IMPORT_REPLACE.to_string()));
        files.push((
            "stream-tail-render.mjs".into(),
            format!("async function onProgressStreaming(isFinal) {{\n{STREAM_TAIL_RENDER_REPLACE}}}\n"),
        ));

        for (name, body) in &files {
            let path = dir.join(name);
            std::fs::write(&path, body).unwrap();
            match node_check(&path) {
                None => {
                    eprintln!("пропуск: node нет в PATH — инжект-JS не проверен");
                    return;
                }
                Some(Err(e)) => panic!("{name}: синтаксическая ошибка\n{e}"),
                Some(Ok(())) => {}
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ST takes subject strictly from `activationMatch[3]`: both regex branches
    /// must put it there. It is checked by the JS engine it is
    /// written for.
    #[test]
    fn sd_activation_regex_puts_subject_in_group_3() {
        let dir = node_check_dir("lk-node-sd-regex");
        let regex_literal = SD_ACTIVATION_REGEX_RU.trim_start_matches("activationRegex:").trim_end_matches(',');
        let script = format!(
            "const re = {regex_literal};\n\
             const cases = [['нарисуй кота в шляпе', 'кота в шляпе'], ['скинь фото с котом', 'котом'], ['send me a picture of a cat', 'cat'], ['сгенерируй закат', 'закат'], ['скинь картинку кота', 'кота'], ['пришли мне фотку себя', 'себя'], ['draw a pic about dogs', 'about dogs'], ['скинь фотографию кота', 'кота'], ['пришли фотографию себя', 'себя'], ['покажи фоточку заката', 'заката'], ['нарисуй картину моря', 'моря'], ['скинь снимок пляжа', 'пляжа']];\n\
             if ('remake the photo'.match(new RegExp(re, 'i'))) {{ console.error('remake'); process.exit(1); }}\n\
             for (const [msg, subject] of cases) {{\n\
               const m = msg.toLowerCase().match(new RegExp(re, 'i'));\n\
               if (!m || typeof m[3] !== 'string' || m[3].trim() !== subject) {{ console.error(msg, '->', m && m[3]); process.exit(1); }}\n\
             }}\n"
        );
        let path = dir.join("sd-regex-groups.cjs");
        std::fs::write(&path, script).unwrap();
        let out = match std::process::Command::new("node").arg(&path).output() {
            Ok(out) => out,
            Err(_) => {
                eprintln!("пропуск: node нет в PATH — регулярка SD не проверена");
                return;
            }
        };
        let _ = std::fs::remove_dir_all(&dir);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
}


