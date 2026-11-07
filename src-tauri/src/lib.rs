#![recursion_limit = "256"]

mod bonds;
mod catalog;
mod changelog;
mod comfyui;
mod dataset;
mod director;
mod downloads;
mod engine;
mod gateway;
mod gpu;
mod guide;
mod image_trainer;
mod json_store;
mod l10n;
mod md_import;
mod models;
mod ollama_client;
mod paths;
mod pyruntime;
mod recommend;
mod registry;
mod repetition;
mod runtime;
mod settings;
mod sillytavern;
mod system;
mod character_evolution;
mod relationships;
mod tag_influence;
mod tagger;
mod rpg;
mod text_lora;
mod traits;
mod trainer;
mod translation;
mod window_state;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ollama_client::OllamaClient;
use settings::Settings;
use tauri::{Emitter, Manager};

/// Application state. The runtime and ST come up lazily; until
/// navigation to ST the frontend shows a splash screen with the status.
struct AppState {
    client: Mutex<Option<OllamaClient>>,
    runtime: Mutex<Option<runtime::Runtime>>,
    /// Runtime boot — one at a time: parallel calls (startup boot
    /// and the wizard button) found the port free at the same time and
    /// started two `ollama serve` processes, after which the gateway went to the second one's port.
    runtime_lock: tokio::sync::Mutex<()>,
    st_child: Mutex<Option<std::process::Child>>,
    settings: Mutex<Settings>,
    cancel_download: Arc<AtomicBool>,
    /// Model download queue: its state survives the window, so
    /// the download continues in the background, and the settings show it when opened.
    downloads: Arc<downloads::Queue>,
    vision_downloading: AtomicBool,
    st_port: std::sync::atomic::AtomicU16,
    boot_status: Mutex<String>,
    /// Boot progress 0..=100. Only the SillyTavern download step computes it,
    /// the others reset it to None: a bar left at the previous value lies.
    boot_progress: Mutex<Option<u8>>,
    boot_error: Mutex<Option<String>>,
    boot_ready: AtomicBool,
    boot_in_progress: AtomicBool,
    st_opened: AtomicBool,
    /// Context ceiling for the active model and free VRAM (0 —
    /// unknown): we do not take more than the user-requested context.
    context_ceiling: std::sync::atomic::AtomicU32,
    /// Whether the active model fits entirely in VRAM.
    model_fits: AtomicBool,
    /// The character currently being chatted with in SillyTavern (the card avatar).
    /// The ST extension reports it on chat change; the gateway uses it to mix
    /// the character's live state into the prompt.
    active_character: Mutex<String>,
    /// The user's persona: a character has a separate attitude toward each
    /// interlocutor, so state is kept per "character + persona" pair.
    active_persona: Mutex<String>,
}

/// WebView2 behaves like a browser: context menu, F5/Ctrl+R, zoom.
/// For a desktop app that is a foreign layer — we remove it.
#[cfg(windows)]
fn make_native(window: &tauri::WebviewWindow) {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2Settings3, ICoreWebView2Settings5, ICoreWebView2Settings6,
    };
    use windows_core::Interface;

    let res = window.with_webview(|webview| unsafe {
        let core = match webview.controller().CoreWebView2() {
            Ok(c) => c,
            Err(e) => return eprintln!("CoreWebView2 недоступен: {e}"),
        };
        let settings = match core.Settings() {
            Ok(s) => s,
            Err(e) => return eprintln!("настройки webview недоступны: {e}"),
        };
        fn warn(name: &str, r: windows_core::Result<()>) {
            if let Err(e) = r {
                eprintln!("webview: не удалось отключить {name}: {e}");
            }
        }
        warn("контекстное меню", settings.SetAreDefaultContextMenusEnabled(false));
        warn("зум", settings.SetIsZoomControlEnabled(false));
        warn("строку состояния", settings.SetIsStatusBarEnabled(false));
        #[cfg(not(debug_assertions))]
        warn("devtools", settings.SetAreDevToolsEnabled(false));
        match settings.cast::<ICoreWebView2Settings3>() {
            Ok(s) => warn("горячие клавиши браузера", s.SetAreBrowserAcceleratorKeysEnabled(false)),
            Err(e) => eprintln!("webview: нет ICoreWebView2Settings3: {e}"),
        }
        match settings.cast::<ICoreWebView2Settings5>() {
            Ok(s) => warn("зум щипком", s.SetIsPinchZoomEnabled(false)),
            Err(e) => eprintln!("webview: нет ICoreWebView2Settings5: {e}"),
        }
        match settings.cast::<ICoreWebView2Settings6>() {
            Ok(s) => warn("навигацию свайпом", s.SetIsSwipeNavigationEnabled(false)),
            Err(e) => eprintln!("webview: нет ICoreWebView2Settings6: {e}"),
        }
    });
    if let Err(e) = res {
        eprintln!("не удалось настроить webview: {e}");
    }
}

#[cfg(not(windows))]
fn make_native(_window: &tauri::WebviewWindow) {}

/// Set the boot status: the state is the source of truth for frontend polling,
/// the event is only a fast update (it can be missed, the state cannot).
fn set_boot_status(app: &tauri::AppHandle, status: &str) {
    set_boot_progress(app, status, None);
}

/// Same, but with boot progress where the step actually computes it
/// (SillyTavern update). The other steps call `set_boot_status`,
/// which zeroes the bar: progress that outlived a step change would look
/// 90% done at the first "Checking Ollama…" word.
fn set_boot_progress(app: &tauri::AppHandle, status: &str, progress: Option<u8>) {
    let state = app.state::<Arc<AppState>>().inner().clone();
    *state.boot_status.lock().unwrap_or_else(|e| e.into_inner()) = status.to_string();
    *state.boot_progress.lock().unwrap_or_else(|e| e.into_inner()) = progress;
    let _ = app.emit("boot-status", status.to_string());
}

/// Append a line to data\boot.log: events may not reach the frontend,
/// the log is the only place where the exact boot failure point is visible.
pub fn boot_log(msg: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths::data_dir().join("boot.log"))
    {
        use std::io::Write;
        let _ = writeln!(f, "[{}] {msg}", local_time_str());
    }
}

#[cfg(windows)]
fn local_time_str() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let t = unsafe { GetLocalTime() };
    format!("{:02}:{:02}:{:02}", t.wHour, t.wMinute, t.wSecond)
}

#[cfg(not(windows))]
fn local_time_str() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default()
}

/// Reset `boot_in_progress` on any boot exit, including a panic:
/// without it the flag stays true and a retry (`retry_boot`) is impossible.
struct BootFlagGuard(Arc<AppState>);

impl Drop for BootFlagGuard {
    fn drop(&mut self) {
        self.0.boot_in_progress.store(false, Ordering::Relaxed);
    }
}

/// Full boot after setup: Ollama → SillyTavern → ready.
/// The frontend performs window navigation from `boot_state` — missed events and
/// the startup navigation race are thus excluded.
async fn bring_up(state: Arc<AppState>, app: tauri::AppHandle) -> Result<(), String> {
    state.boot_ready.store(false, Ordering::Relaxed);
    *state.boot_error.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let result = bring_up_inner(state.clone(), app.clone()).await;
    match result {
        Ok(()) => {
            state.boot_ready.store(true, Ordering::Relaxed);
            set_boot_status(&app, &l10n::t("Готово — открываем SillyTavern…", "Ready — opening SillyTavern…"));
            // The new build actually came up — the previous version's backup is no
            // longer needed. Without this mark, `.exe.old` would have to be erased right
            // at startup (which the old cleanup_old_updates did), losing the way
            // back before it was known whether the update works.
            confirm_update_healthy();
            Ok(())
        }
        Err(e) => {
            boot_log(&format!("ОШИБКА: {e}"));
            *state.boot_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(e.clone());
            set_boot_status(&app, &format!("{} {e}", l10n::t("Ошибка запуска:", "Launch failed:")));
            // Rolling the version back automatically here is NOT allowed: boot also fails for
            // reasons unrelated to the build (the wizard is not completed and ST
            // is not installed, an orphan holds the Ollama port, an antivirus held
            // the portable node — all of this is in gotchas.md). Silently returning
            // the user to the old version because of a busy port is worse than showing
            // an error. Our job is not to lose the way back: the backup stays on
            // disk until the boot succeeds. The "All versions" button lives
            // in SillyTavern, which a build that failed to come up never reaches —
            // so we write to the log how to return manually.
            if has_update_backup() {
                let exe = std::env::current_exe()
                    .ok()
                    .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                    .unwrap_or_else(|| "Cloufam.exe".into());
                boot_log(&format!(
                    "подъём не удался на версии, которая ещё ни разу не поднималась; предыдущая \
                     версия сохранена рядом с программой как {exe}.old (удаляется только после \
                     успешного запуска). Вернуться на неё: закройте Cloufam, переименуйте \
                     {exe} (например, в {exe}.bad), а у {exe}.old уберите окончание .old"
                ));
            }
            Err(e)
        }
    }
}

async fn bring_up_inner(state: Arc<AppState>, app: tauri::AppHandle) -> Result<(), String> {
    boot_log("старт подъёма");
    sillytavern::kill_orphan_node_processes();

    // The model store — before starting Ollama: the path from settings (or the default store)
    // and a one-time move of what was already downloaded into the program folder. Otherwise Ollama
    // starts on an empty store and begins downloading models again.
    {
        let configured = state.settings.lock().unwrap_or_else(|e| e.into_inner()).models_root.clone();
        if !configured.trim().is_empty() {
            paths::set_models_root(std::path::PathBuf::from(configured.trim()));
        }
        if !state.settings.lock().unwrap_or_else(|e| e.into_inner()).models_moved {
            match models::migrate_to_shared_root() {
                Ok(actions) => {
                    for action in actions {
                        boot_log(&format!("Склад моделей: {action}"));
                    }
                    let saved = update_settings(&state, |s| {
                        s.models_moved = true;
                        if s.models_root.trim().is_empty() {
                            s.models_root = paths::models_root().to_string_lossy().to_string();
                        }
                        Ok(())
                    });
                    if let Err(e) = saved {
                        boot_log(&format!("Склад моделей: настройки не сохранены: {e}"));
                    }
                }
                Err(e) => boot_log(&format!("Склад моделей: перенос не удался: {e}")),
            }
        } else {
            // The one-time move is already marked for these installs, but it did not
            // pick up the store used before 1.2.3 (blobs/manifests right in <base>\models) back then.
            match models::adopt_nested_legacy_store() {
                Ok(actions) => {
                    for action in actions {
                        boot_log(&format!("Склад моделей: {action}"));
                    }
                }
                Err(e) => boot_log(&format!("Склад моделей: старое хранилище не подхвачено: {e}")),
            }
        }
        boot_log(&format!("Склад моделей: {}", paths::models_root().display()));
    }

    // Leftovers from past failures: partially downloaded blobs and image checkpoints. Fresh
    // files are left alone — another process may still be downloading them.
    let freed = models::cleanup_stale_partials();
    if freed > 0 {
        boot_log(&format!(
            "Недокачанные файлы удалены: {:.1} ГБ",
            freed as f64 / 1024.0 / 1024.0 / 1024.0
        ));
    }

    // Ollama.
    {
        set_boot_status(&app, &l10n::t("Проверяем Ollama…", "Checking Ollama…"));
        let settings = state.settings.lock().unwrap_or_else(|e| e.into_inner()).clone();
        boot_log(&format!(
            "use_system_ollama={}, порт={}",
            settings.use_system_ollama, settings.ollama_port
        ));
        // The runtime on disk may lag behind the pinned version: update it in place
        // before starting serve — the same scheme as SillyTavern below. System
        // Ollama is a foreign runtime, we leave it alone.
        if !settings.use_system_ollama {
            let installed = runtime::installed_ollama_version().await;
            if runtime::ollama_needs_update(&installed) {
                let shown = if installed.is_empty() { "неизвестная версия".to_string() } else { installed.clone() };
                set_boot_status(
                    &app,
                    &format!("{} {shown} → {}…", l10n::t("Обновляем Ollama", "Updating Ollama"), runtime::OLLAMA_VERSION),
                );
                boot_log(&format!("Ollama: обновление {shown} -> {}", runtime::OLLAMA_VERSION));
                let app_for_progress = app.clone();
                let on_progress: runtime::ProgressFn = Arc::new(move |_got, _total, status| {
                    set_boot_status(&app_for_progress, &status);
                });
                let cancel = Arc::new(AtomicBool::new(false));
                match runtime::update_runtime_to(runtime::OLLAMA_VERSION, cancel, on_progress).await {
                    Ok(()) => boot_log("Ollama: обновлена"),
                    // A network failure or a busy exe does not break boot: we stay
                    // on the installed version, and the next start tries again.
                    Err(e) => boot_log(&format!(
                        "Ollama: обновление не удалось ({e}) — продолжаем с установленной {shown}"
                    )),
                }
            }
        }
        ensure_runtime(&state).await?;
    }
    set_boot_status(&app, &l10n::t("Ollama запущен", "Ollama is running"));
    boot_log("Ollama готов");
    refresh_context_ceiling(&state).await;

    // LLM gateway for ST Chat Completions (OpenAI format → native /api/chat with think:false)
    let gateway_ollama_port = ollama_port_from_url(
        &state
            .client
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|c| c.base_url.clone())
            .unwrap_or_default(),
    )
    .unwrap_or(11434);
    gateway::start(gateway_ollama_port, app.clone()).await?;

    // Background download of vision, if the model is missing — does not block boot.
    spawn_vision_download(state.clone(), app.clone());

    // SillyTavern. The gate is code and Node only: without node_modules the install
    // is repaired below, while the `installed()` check here locked boot hard
    // after a failed npm update (the wizard is not shown when setup_done is set).
    if !sillytavern::code_installed() {
        return Err("SillyTavern не установлена — пройдите мастер настройки".into());
    }
    // The ST version on disk may lag behind the one pinned in the app: the Cloufam
    // updater replaces only its own exe, not the ST code inside, and without this
    // check every existing install would stay on the first-run version
    // forever. It runs before the seed_*/patch_* below — they re-inject the theme,
    // extensions and locale on top of the fresh code.
    {
        let installed_st_version = sillytavern::installed_st_version();
        if sillytavern::st_needs_update(&installed_st_version) {
            set_boot_status(
                &app,
                &format!("{} {installed_st_version} → {}…", l10n::t("Обновляем SillyTavern", "Updating SillyTavern"), sillytavern::ST_VERSION),
            );
            boot_log(&format!(
                "SillyTavern: обновление {installed_st_version} -> {}",
                sillytavern::ST_VERSION
            ));
            let app_for_progress = app.clone();
            let on_progress: sillytavern::ProgressFn = Arc::new(move |_step, status, got, total| {
                // The share lives in the progress field, not in the label: the label is read
                // by humans, the bar by machine, and "(37%)" in the text breaks it on
                // steps without a counter.
                let share = if total > 0 {
                    let percent = (got as f64 / total as f64 * 100.0).round();
                    Some(percent.clamp(0.0, 100.0) as u8)
                } else {
                    None
                };
                set_boot_progress(&app_for_progress, &status, share);
            });
            // A separate cancel flag: on this screen (a regular boot, not the
            // wizard) there is no "Cancel" button for this particular step.
            // An update error does not break boot: the removed dependencies are fixed
            // by the block below, and the update is retried on the next start
            // (the version marker is written only after full success).
            match sillytavern::update_st(
                &installed_st_version,
                Arc::new(AtomicBool::new(false)),
                on_progress,
            )
            .await
            {
                Ok(()) => {
                    boot_log(&format!("SillyTavern: обновлена до {}", sillytavern::ST_VERSION))
                }
                Err(e) => boot_log(&format!(
                    "Обновление SillyTavern: {e} — продолжаем с установленной {installed_st_version}"
                )),
            }
        }
    }
    // Code is in place, dependencies are missing: the update above wiped node_modules and failed on
    // npm, or they were removed by hand. We install them again; an error — a clear reason on
    // the boot screen instead of three empty starts, "Retry" tries again.
    if !sillytavern::installed() {
        set_boot_status(&app, &l10n::t("Восстанавливаем зависимости SillyTavern…", "Restoring SillyTavern dependencies…"));
        boot_log("SillyTavern: нет node_modules — npm install");
        let app_for_progress = app.clone();
        let on_progress: sillytavern::ProgressFn = Arc::new(move |_step, status, _got, _total| {
            set_boot_status(&app_for_progress, &status);
        });
        sillytavern::npm_install(Arc::new(AtomicBool::new(false)), on_progress)
            .await
            .map_err(|e| format!("Зависимости SillyTavern не установлены: {e}"))?;
        boot_log("SillyTavern: зависимости восстановлены");
    }
    let client_port = state
        .client
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|c| c.base_url.clone())
        .unwrap_or_default();
    let current_ollama_port = ollama_port_from_url(&client_port).unwrap_or(11434);

    // Settings for this hardware: the first run on it or a GPU replacement.
    // Do it before starting SillyTavern — sync_st_settings inside applies
    // the tuned values to ST, and the server comes up with them already.
    if let Err(e) = auto_tune(&app, &state).await {
        boot_log(&format!("Подбор под железо: {e}"));
    }

    // One-time reset of the active character on existing installs.
    let migrate = !state.settings.lock().unwrap_or_else(|e| e.into_inner()).echo_migrated;
    if migrate {
        if let Err(e) = sillytavern::cleanup_legacy_characters() {
            boot_log(&format!("SillyTavern: cleanup_legacy_characters: {e}"));
        }
        let _ = update_settings(&state, |s| {
            s.coder_removed = true;
            s.echo_migrated = true;
            Ok(())
        });
    }
    // One-time removal of the demo character Echo.
    if !state.settings.lock().unwrap_or_else(|e| e.into_inner()).echo_removed {
        match sillytavern::remove_echo_character() {
            Ok(0) => {}
            Ok(count) => boot_log(&format!("SillyTavern: персонаж Echo удалён ({count})")),
            Err(e) => boot_log(&format!("SillyTavern: удаление Echo: {e}")),
        }
        let _ = update_settings(&state, |s| {
            s.echo_removed = true;
            Ok(())
        });
    }
    // Auto-tags became an option instead of a coercion: on existing installs
    // the default was "enabled", and tags were added to cards without asking.
    if !state.settings.lock().unwrap_or_else(|e| e.into_inner()).tagger_opt_in_migrated {
        let _ = update_settings(&state, |s| {
            crate::settings::migrate_tagger_opt_in(s);
            Ok(())
        });
    }
    // Adaptive and built-in characters
    if let Err(e) = sillytavern::seed_characters() {
        boot_log(&format!("SillyTavern: seed_characters: {e}"));
    }

    // Web search (ST plugin + slash command) — before the server starts.
    // An inaccessible file (antivirus, permissions) must not bring down the whole boot.
    if let Err(e) = sillytavern::seed_web_search() {
        boot_log(&format!("SillyTavern: seed_web_search: {e}"));
    }
    // Character auto-tags: a bridge plugin into the gateway + the ST UI extension.
    if let Err(e) = sillytavern::seed_tagger() {
        boot_log(&format!("SillyTavern: seed_tagger: {e}"));
    }
    // Director mode: an extension with narrator notes.
    if let Err(e) = sillytavern::seed_director() {
        boot_log(&format!("SillyTavern: seed_director: {e}"));
    }
    // The Moonlit Echoes theme: shipped in the bundle because the standard ST
    // extension install goes through git, which may not exist on the user's machine.
    if let Err(e) = sillytavern::seed_moonlit_theme() {
        boot_log(&format!("SillyTavern: seed_moonlit_theme: {e}"));
    }
    // The custom Cloufam theme (obsidian glassmorphism)
    if let Err(e) = sillytavern::seed_theme() {
        boot_log(&format!("SillyTavern: seed_theme: {e}"));
    }
    // ComfyUI models — into the shared store via a junction (if ComfyUI is installed).
    match models::link_comfy_models() {
        Ok(action) if !action.is_empty() => boot_log(&format!("Склад моделей: {action}")),
        Ok(_) => {}
        Err(e) => boot_log(&format!("Склад моделей: ComfyUI: {e}")),
    }
    // The archive of an interrupted install is useless when ComfyUI is already there: it is not
    // resumed (the install does not start) and takes up to 2 GB.
    let freed = comfyui::discard_installer_archive();
    if freed > 0 {
        boot_log(&format!(
            "Архив установщика ComfyUI удалён: {:.0} МБ",
            freed as f64 / 1024.0 / 1024.0
        ));
    }
    // The active image model may have disappeared from disk (deleted by hand, the folder
    // was moved) — otherwise ST is configured for a nonexistent file. We check
    // after binding the store: before that the checkpoint folder may still be empty.
    {
        let current = state.settings.lock().unwrap_or_else(|e| e.into_inner()).image_model.clone();
        if let Some(next) = comfyui::resolve_active_model(&current) {
            boot_log(&format!(
                "Модель картинок: {current} не найдена на диске, активной стала {next}"
            ));
            let _ = update_settings(&state, |s| {
                s.image_model = next;
                Ok(())
            });
        }
    }
    if let Err(e) = sillytavern::seed_cloufam_ui(gateway::gateway_port(current_ollama_port)) {
        boot_log(&format!("SillyTavern: seed_cloufam_ui: {e}"));
    }
    // The SD extension patch: Russian triggers only if ComfyUI is installed,
    // otherwise the extension must not react to generation commands.
    if comfyui::is_installed() {
        if let Err(e) = sillytavern::patch_sd_russian_triggers() {
            boot_log(&format!("SillyTavern: SD-триггеры: {e}"));
        }
    }
    if let Err(e) = sillytavern::patch_backgrounds_fix() {
        boot_log(&format!("SillyTavern: {e}"));
    }
    if let Err(e) = sillytavern::patch_world_info() {
        boot_log(&format!("SillyTavern: {e}"));
    }
    if let Err(e) = sillytavern::patch_streaming_tail() {
        boot_log(&format!("SillyTavern: {e}"));
    }

    // Starting the local ComfyUI — genuinely in the background: `start_comfy` inside waits for
    // `pip install` and unconditionally sleeps for two seconds, and because of that SillyTavern
    // did not begin to start until the optional image generation
    // finished. Boot does not wait for it and learns about a refusal from the log.
    if state.settings.lock().unwrap_or_else(|e| e.into_inner()).auto_launch_comfy && comfyui::is_installed() {
        tauri::async_runtime::spawn_blocking(|| {
            if let Err(e) = comfyui::start_comfy(8188) {
                boot_log(&format!("ComfyUI start: {e}"));
            }
        });
    }

    set_boot_status(&app, &l10n::t("Запускаем SillyTavern…", "Starting SillyTavern…"));

    let mut started_child = None;
    let mut actual_st_port = 0;
    let mut last_err = String::new();
    let mut next_min_port = 8000;
    // Theme and markup versions are recorded only if deployment succeeded: otherwise
    // the applied mark stays forever while the tags and theme themselves remain old.
    let mut settings_synced = false;

    for attempt in 0..3 {
        let st_port = sillytavern::find_port(next_min_port).await?;
        next_min_port = st_port + 1;
        sillytavern::write_config(st_port)?;
        let effective = with_effective_context(&state, &state.settings.lock().unwrap_or_else(|e| e.into_inner()));
        sillytavern::seed_settings(&effective, current_ollama_port)?;
        match sillytavern::sync_st_settings(&effective, current_ollama_port) {
            Ok(()) => settings_synced = true,
            Err(e) => boot_log(&format!("SillyTavern: sync_st_settings: {e}")),
        }
        boot_log(&format!("старт SillyTavern на порту {st_port} (попытка {})", attempt + 1));
        match sillytavern::start(st_port).await {
            Ok(child) => {
                actual_st_port = st_port;
                started_child = Some(child);
                break;
            }
            Err(e) => {
                boot_log(&format!("SillyTavern не запустилась на {st_port}: {e}"));
                last_err = e;
                sillytavern::kill_orphan_node_processes();
            }
        }
    }

    let child = started_child.ok_or_else(|| format!("Не удалось запустить SillyTavern после 3 попыток: {last_err}"))?;
    state.st_port.store(actual_st_port, Ordering::SeqCst);
    *state.st_child.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
    sillytavern::clear_starting_pid();
    boot_log("SillyTavern отвечает");

    // The theme was deployed into power_user above (sync_st_settings) — record the styling
    // version. Only after a successful boot: otherwise a failed start would consume
    // the theme application, and the user would be left with the old look.
    if settings_synced && state.settings.lock().unwrap_or_else(|e| e.into_inner()).theme_version < sillytavern::THEME_VERSION {
        let _ = update_settings(&state, |s| {
            s.theme_version = sillytavern::THEME_VERSION;
            Ok(())
        });
        boot_log(&format!(
            "SillyTavern: оформление обновлено до версии {}",
            sillytavern::THEME_VERSION
        ));
    }

    // Character markup was deployed by the same sync_st_settings — record the version
    // after a successful boot, so a failed start does not consume the tag update.
    if settings_synced && state.settings.lock().unwrap_or_else(|e| e.into_inner()).tags_version < sillytavern::TAGS_VERSION {
        let _ = update_settings(&state, |s| {
            s.tags_version = sillytavern::TAGS_VERSION;
            Ok(())
        });
        boot_log(&format!(
            "SillyTavern: теги персонажей обновлены до версии {}",
            sillytavern::TAGS_VERSION
        ));
    }

    Ok(())
}

/// Bring up Ollama, reusing the already running runtime.
/// Assigning a new `Runtime` dropped the previous one, and its Drop kills `ollama serve`
/// along with its children: a repeated boot (wizard, "Retry" button) left
/// the app with a dead Ollama. A dead runtime is killed explicitly before spawning a new one,
/// otherwise a broad orphan sweep would kill the serve just started.
async fn ensure_runtime(state: &Arc<AppState>) -> Result<(), String> {
    let _serialized = state.runtime_lock.lock().await;
    let previous = { state.runtime.lock().unwrap_or_else(|e| e.into_inner()).take() };
    let alive = match previous.as_ref() {
        Some(rt) => runtime::Runtime::probe(&rt.client.base_url).await,
        None => false,
    };
    if let Some(rt) = previous {
        if alive {
            *state.client.lock().unwrap_or_else(|e| e.into_inner()) = Some(rt.client.clone());
            *state.runtime.lock().unwrap_or_else(|e| e.into_inner()) = Some(rt);
            runtime::clear_starting_pid();
            return Ok(());
        }
        rt.stop();
        // between rt.stop() and creating the new runtime, state.client holds a dead
        // OllamaClient — concurrent gateway requests get connection refused.
        *state.client.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
    let mut settings = state.settings.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let rt = runtime::Runtime::start(&mut settings).await?;
    // We write only the port back to settings: the Ollama boot lasts up to a minute, and the
    // screen is already open all that time. Writing a whole snapshot rolled back what
    // the user had managed to save (model, context) — they saw "saved"
    // and half a minute later the setting silently reverted.
    {
        let mut current = state.settings.lock().unwrap_or_else(|e| e.into_inner());
        if current.ollama_port != settings.ollama_port {
            current.ollama_port = settings.ollama_port;
            let _ = current.save();
        }
    }
    *state.client.lock().unwrap_or_else(|e| e.into_inner()) = Some(rt.client.clone());
    *state.runtime.lock().unwrap_or_else(|e| e.into_inner()) = Some(rt);
    runtime::clear_starting_pid();
    Ok(())
}

/// The key of the last VRAM log entry: the settings screen recomputes the budget
/// every 10 seconds, and the same situation must not flood boot.log.
static LAST_VRAM_NOTE: Mutex<String> = Mutex::new(String::new());

/// Context ceiling between recomputations: a raw measurement fluctuates, and every new
/// num_ctx value reloads the model in Ollama.
static CONTEXT_CEILING: Mutex<models::CeilingTracker> = Mutex::new(models::CeilingTracker::new());

/// Recompute the context ceiling for the active model and free VRAM.
/// Returns the computed budget — the gateway needs it only as a number in the state,
/// while the settings screen also needs the figures for the hint.
async fn refresh_context_ceiling(state: &Arc<AppState>) -> Option<models::ContextBudget> {
    let client = state.client.lock().unwrap_or_else(|e| e.into_inner()).clone()?;
    let (model, requested, embed) = {
        let s = state.settings.lock().unwrap_or_else(|e| e.into_inner());
        // Memory is enabled — the embedder will live in the same GPU between turns
        // (`ext.vectors.enabled_chats`), and it needs a reserve in the ceiling.
        let embed = (s.auto_memory && !s.embed_model.trim().is_empty())
            .then(|| s.embed_model.trim().to_string());
        (s.chat_model.clone(), s.context_size, embed)
    };
    let budget = models::context_budget(&client, &model, embed.as_deref()).await;
    // While we were computing (seconds, requests to Ollama), the model may have changed: the old
    // result must not override the new model's ceiling. The next call computes it.
    let current_model = state
        .settings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .chat_model
        .clone();
    if current_model != model {
        return None;
    }
    // Could not compute (nvidia-smi or /api/show failure) — keep the previous value:
    // "no limit" would raise num_ctx and reload the model.
    let (ceiling, fits) = {
        let mut tracker = CONTEXT_CEILING.lock().unwrap_or_else(|e| e.into_inner());
        match &budget {
            Some(budget) => tracker.update(&model, budget),
            None => tracker.fallback(&model),
        }
    };
    state.context_ceiling.store(ceiling, Ordering::Relaxed);
    state.model_fits.store(fits, Ordering::Relaxed);
    let Some(budget) = budget else { return None };
    // The dedup key — without floating values: free VRAM changes
    // from poll to poll, and the settings screen triggers a recompute every 10 seconds.
    // Comparing whole strings wrote a new entry to the log on every poll and
    // buried the boot stages — the only failure diagnostics.
    let (key, note) = if !fits {
        (
            format!("fit:{model}"),
            format!(
                "Видеопамять: {model} не помещается (свободно {} МБ, нужно ≈{} МБ) — закройте приложения, использующие GPU, или возьмите модель меньше",
                budget.free_mb, budget.needed_mb
            ),
        )
    } else if ceiling < requested {
        (
            format!("ceiling:{model}:{requested}"),
            format!(
                "Контекст: {requested} токенов не помещаются в видеопамять для {model} — используется {ceiling}"
            ),
        )
    } else {
        (String::new(), String::new())
    };
    if !key.is_empty() {
        let mut last = LAST_VRAM_NOTE.lock().unwrap_or_else(|e| e.into_inner());
        if *last != key {
            boot_log(&note);
            *last = key;
        }
    } else {
        LAST_VRAM_NOTE.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
    Some(budget)
}

/// Settings with the context trimmed to the VRAM ceiling: we take neither more
/// than requested nor less (the ceiling already accounts for the minimum).
fn with_effective_context(state: &AppState, settings: &Settings) -> Settings {
    let ceiling = state.context_ceiling.load(Ordering::Relaxed);
    let mut s = settings.clone();
    s.context_size = models::chat_context(s.context_size, ceiling);
    s
}

fn ollama_port_from_url(url: &str) -> Option<u16> {
    let clean = url.trim();
    if clean.is_empty() {
        return None;
    }
    let with_scheme = if clean.contains("://") {
        clean.to_string()
    } else {
        format!("http://{clean}")
    };
    reqwest::Url::parse(&with_scheme).ok()?.port()
}

/// Background download of the vision model. All work is inside the task: the installed()
/// check (a request to Ollama) must not block the app boot.
/// The outcome goes out as the `vision-download-done` event on all paths: without it the line
/// "vision will download in the background" hangs forever, and a failure is not visible at all.
fn spawn_vision_download(state: Arc<AppState>, app: tauri::AppHandle) {
    if state.vision_downloading.swap(true, Ordering::Relaxed) {
        return;
    }
    let vision = state.settings.lock().unwrap_or_else(|e| e.into_inner()).vision_model.clone();
    // We do not download a model outside this hardware's showcase: it will not run anyway
    // and weighs several gigabytes.
    let vision_tags: Vec<String> = catalog::visible(&gpu::gpu_info())
        .into_iter()
        .filter(|m| m.category == "vision")
        .map(|m| m.tag)
        .collect();
    if !models::is_model_installed(&vision_tags, &vision) {
        boot_log(&format!(
            "Зрение: модель {vision} не подходит этому железу — докачка пропущена"
        ));
        state.vision_downloading.store(false, Ordering::Relaxed);
        emit_vision_result(&app, false, &vision, &l10n::t("модель зрения вне витрины этого железа", "the vision model is beyond what this hardware can run"));
        return;
    }
    let state_arc = state.clone();
    tauri::async_runtime::spawn(async move {
        let Some(client) = state_arc.client.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
            state_arc.vision_downloading.store(false, Ordering::Relaxed);
            emit_vision_result(&app, false, &vision, &l10n::t("Ollama ещё не запущена", "Ollama is not running yet"));
            return;
        };
        if models::is_model_installed(&models::installed(&client).await, &vision) {
            state_arc.vision_downloading.store(false, Ordering::Relaxed);
            emit_vision_result(&app, true, &vision, "");
            return;
        }
        // The download uses the same queue as manual downloads: it is visible in
        // the settings, and can be paused and cancelled.
        let title = model_title(&vision);
        start_downloads(&app, &state_arc, vec![downloads::Task::ollama(&vision, title)]);
        state_arc.downloads.wait_for(std::slice::from_ref(&vision)).await;
        state_arc.vision_downloading.store(false, Ordering::Relaxed);
        match state_arc.downloads.outcome(&vision) {
            Some(Ok(())) => emit_vision_result(&app, true, &vision, ""),
            Some(Err(error)) => {
                boot_log(&format!("Зрение: модель {vision} не скачана: {error}"));
                emit_vision_result(&app, false, &vision, &error);
            }
            None => emit_vision_result(&app, false, &vision, &l10n::t("загрузка не запускалась", "the download never started")),
        }
    });
}

fn emit_vision_result(app: &tauri::AppHandle, ok: bool, model: &str, error: &str) {
    let _ = app.emit(
        "vision-download-done",
        serde_json::json!({
            "ok": ok,
            "model": model,
            "error": if error.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(error.to_string()) },
        }),
    );
}

// ---------- Commands ----------

#[derive(serde::Serialize)]
struct BootState {
    status: String,
    error: Option<String>,
    ready: bool,
    port: u16,
    /// Boot progress 0..=100, if the current step computes it.
    #[serde(default)]
    progress: Option<u8>,
}

#[derive(serde::Serialize)]
struct SetupState {
    has_gpu: bool,
    gpu_name: Option<String>,
    vram_mb: Option<u64>,
    free_space_gb: f64,
    runtime_installed: bool,
    st_installed: bool,
    setup_done: bool,
    recommended_chat_model: String,
    /// Hardware: vendor, VRAM, tier, available acceleration backend and explanations.
    hardware: gpu::GpuInfo,
    /// The Ollama model showcase, already filtered for this hardware.
    models: Vec<catalog::CatalogModel>,
    /// Whether image generation is available on this machine at all.
    images_available: bool,
    /// The model store the install will use.
    models_root: String,
    settings: Settings,
}

/// A candidate folder for the model store, for the wizard.
#[derive(serde::Serialize)]
struct ModelStore {
    path: String,
    /// How many Ollama models are already in the folder.
    models: usize,
    /// Free space on the folder's volume, GB; -1 — could not determine.
    free_gb: f64,
    current: bool,
}

/// Free space on the volume of a folder that may not exist yet: measured at
/// the nearest existing parent.
fn store_free_gb(path: &std::path::Path) -> f64 {
    path.ancestors()
        .find(|dir| dir.exists())
        .and_then(gpu::free_space_bytes)
        .map(|bytes| bytes as f64 / 1024.0 / 1024.0 / 1024.0)
        .unwrap_or(-1.0)
}

fn describe_store(path: &std::path::Path) -> ModelStore {
    ModelStore {
        path: path.to_string_lossy().to_string(),
        models: paths::count_ollama_models(&paths::ollama_store_in(path)),
        free_gb: store_free_gb(path),
        current: path == paths::models_root(),
    }
}

/// Stores to choose from in the wizard: the current one and discovered folders that already hold models.
/// Directory scanning is blocking: a synchronous Tauri 2 command would run on the main thread
/// and freeze the window, hence async + spawn_blocking.
#[tauri::command]
async fn model_stores() -> Result<Vec<ModelStore>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        paths::model_store_candidates()
            .iter()
            .map(|path| describe_store(path))
            .filter(|store| store.current || store.models > 0)
            .collect()
    })
    .await
    .map_err(|e| e.to_string())
}

/// Information about a folder the user selected, before making it the store.
#[tauri::command]
async fn inspect_model_store(path: String) -> Result<ModelStore, String> {
    let path = std::path::PathBuf::from(path.trim());
    if !path.is_absolute() {
        return Err(l10n::t("Нужен полный путь к папке", "A full folder path is required"));
    }
    tauri::async_runtime::spawn_blocking(move || describe_store(&path))
        .await
        .map_err(|e| e.to_string())
}

/// Make a folder the model store. Only before Ollama starts: a running
/// `ollama serve` holds the previous `OLLAMA_MODELS`, and downloads would go elsewhere.
#[tauri::command]
async fn set_models_root(state: tauri::State<'_, Arc<AppState>>, path: String) -> Result<ModelStore, String> {
    let path = std::path::PathBuf::from(path.trim());
    if !path.is_absolute() {
        return Err(l10n::t("Нужен полный путь к папке", "A full folder path is required"));
    }
    if state.client.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
        return Err(l10n::t(
            "Ollama уже запущена: смените папку моделей в мастере после перезапуска приложения",
            "Ollama is already running: change the models folder in the setup wizard after restarting the app",
        ));
    }
    paths::create_model_store_dirs(&path).map_err(|e| {
        format!("{} {}: {e}", l10n::t("Папка недоступна для записи", "The folder is not writable"), path.display())
    })?;
    update_settings(state.inner(), |s| {
        s.models_root = path.to_string_lossy().to_string();
        Ok(())
    })?;
    paths::set_models_root(path.clone());
    tauri::async_runtime::spawn_blocking(move || describe_store(&path))
        .await
        .map_err(|e| e.to_string())
}

/// Current boot state. The frontend polls it while showing the splash screen:
/// `boot-status` events may be missed if they fired before the listener
/// was registered.
#[tauri::command]
fn boot_state(state: tauri::State<'_, Arc<AppState>>) -> BootState {
    BootState {
        status: state.boot_status.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        error: state.boot_error.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        ready: state.boot_ready.load(Ordering::Relaxed),
        port: state.st_port.load(Ordering::Relaxed),
        progress: *state.boot_progress.lock().unwrap_or_else(|e| e.into_inner()),
    }
}

/// Accept the UI locale from the frontend: Rust window strings (boot statuses,
/// model showcase, guide) are served in the UI language (see `l10n`).
/// Sent at the start of every window and on every language change; until the first
/// call Russian is in effect.
#[tauri::command]
fn set_locale(app: tauri::AppHandle, lang: String) {
    l10n::set_locale(&lang);
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        match tray_menu(&app) {
            Ok(menu) => {
                if let Err(e) = tray.set_menu(Some(menu)) {
                    boot_log(&format!("Трей: меню не обновлено ({e})"));
                }
            }
            Err(e) => boot_log(&format!("Трей: меню не собрано ({e})")),
        }
    }
}

/// Retry the boot from the error screen — without restarting the app.
#[tauri::command]
async fn retry_boot(
    state: tauri::State<'_, Arc<AppState>>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    if state.boot_in_progress.swap(true, Ordering::Relaxed) {
        return Err("запуск уже выполняется".to_string());
    }
    let state_arc = state.inner().clone();
    tauri::async_runtime::spawn(async move {
        let _guard = BootFlagGuard(state_arc.clone());
        let _ = bring_up(state_arc.clone(), app).await;
    });
    Ok(())
}

#[tauri::command]
async fn setup_state(state: tauri::State<'_, Arc<AppState>>) -> Result<SetupState, String> {
    let settings = state.settings.lock().unwrap_or_else(|e| e.into_inner()).clone();
    // The wizard shows the smaller of the two volumes: the runtime and SillyTavern download into
    // the program folder, models — into the model store; lying about "100 GB free"
    // on one of them is not acceptable.
    let free = match (
        gpu::free_space_bytes(&paths::models_dir()),
        gpu::free_space_bytes(&paths::base_dir()),
    ) {
        (Some(models_free), Some(app_free)) => Some(models_free.min(app_free)),
        (models_free, app_free) => models_free.or(app_free),
    }
    .map(|b| b as f64 / 1024.0 / 1024.0 / 1024.0)
    .unwrap_or(-1.0);
    let gpu = gpu::gpu_info();
    // The model showcase is built for the hardware: the user sees only what
    // this machine can handle (see catalog.rs).
    let models = catalog::visible(&gpu);
    let recommended_chat_model = catalog::recommended_chat_model(&gpu);
    Ok(SetupState {
        has_gpu: gpu.has_gpu,
        gpu_name: gpu.name.clone(),
        vram_mb: gpu.vram_mb,
        free_space_gb: free,
        runtime_installed: paths::runtime_dir().join("ollama.exe").exists(),
        st_installed: sillytavern::installed(),
        setup_done: settings.setup_done,
        recommended_chat_model,
        hardware: gpu.clone(),
        models,
        images_available: catalog::image_ceiling(&gpu).is_some(),
        models_root: paths::models_root().to_string_lossy().to_string(),
        settings,
    })
}

#[tauri::command]
async fn download_runtime(
    state: tauri::State<'_, Arc<AppState>>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    state.cancel_download.store(false, Ordering::Relaxed);
    let cancel = state.cancel_download.clone();
    let on_progress: runtime::ProgressFn = Arc::new(move |got, total, status| {
        let _ = app.emit(
            "runtime-download-progress",
            serde_json::json!({ "completed": got, "total": total, "status": status }),
        );
    });
    runtime::download_runtime(cancel, on_progress).await.map(|_| ())
}

#[tauri::command]
async fn cancel_download(state: tauri::State<'_, Arc<AppState>>) -> Result<(), String> {
    state.cancel_download.store(true, Ordering::Relaxed);
    Ok(())
}

/// Cancel the ComfyUI install without touching the wizard downloads.
#[tauri::command]
fn cancel_comfy_install() {
    comfyui::install_cancel_flag().store(true, Ordering::Relaxed);
}

/// Model name for the queue row: from this hardware's showcase, otherwise — the tag itself.
fn model_title(tag: &str) -> String {
    catalog::visible(&gpu::gpu_info())
        .into_iter()
        .find(|m| models::is_model_installed(&[m.tag.clone()], tag))
        .map(|m| format!("{} ({})", m.name, m.category))
        .unwrap_or_else(|| tag.to_string())
}

/// Checkpoint name for the queue row: from the image showcase, otherwise — the file name.
fn image_model_title(filename: &str) -> String {
    let active = String::new();
    comfyui::get_curated_models(&active, &gpu::gpu_info())
        .into_iter()
        .find(|m| m.filename.eq_ignore_ascii_case(filename))
        .map(|m| m.name)
        .unwrap_or_else(|| filename.to_string())
}

/// Enqueue downloads and start the queue if it was idle. The backend does the work:
/// the window can be closed or you can switch to SillyTavern — progress is not lost,
/// and the settings reopened show the same queue.
fn start_downloads(app: &tauri::AppHandle, state: &Arc<AppState>, tasks: Vec<downloads::Task>) {
    if tasks.is_empty() {
        return;
    }
    if !state.downloads.enqueue(tasks) {
        // The queue is busy: the task went to the end, and the window must learn about it
        // immediately — when paused there are no progress ticks, so the event would not come otherwise.
        state.downloads.publish(app);
        return;
    }
    let queue = state.downloads.clone();
    let handle = app.clone();
    let app_state = state.clone();
    let item_handle = app.clone();
    tauri::async_runtime::spawn(async move {
        downloads::run(queue, handle, move |task, flags| {
            let app_state = app_state.clone();
            let app = item_handle.clone();
            async move { download_task(app_state, app, task, flags).await }
        })
        .await;
    });
}

/// Download one queue item: an Ollama model or an image checkpoint.
async fn download_task(
    state: Arc<AppState>,
    app: tauri::AppHandle,
    task: downloads::Task,
    flags: downloads::FlagsRef,
) -> Result<(), String> {
    match task.kind {
        downloads::Kind::Ollama => {
            let client = state
                .client
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .ok_or_else(|| "Ollama ещё не запущена".to_string())?;
            let progress = ollama_progress(state.downloads.clone(), app.clone(), task.id.clone());
            models::download(&client, &task.id, flags, progress).await
        }
        downloads::Kind::Image => {
            let progress = image_progress(state.downloads.clone(), app.clone(), task.id.clone());
            comfyui::download_image_model(&task.id, flags, progress)
                .await
                .map(|_| ())
        }
    }
}

/// Model progress: first into the download queue (shown by the settings screen and,
/// through the gateway, the model menu in SillyTavern), then as a throttled event for
/// the first-run wizard: its final statuses are state changes, so they are not lost.
fn ollama_progress(
    queue: Arc<downloads::Queue>,
    app: tauri::AppHandle,
    model: String,
) -> downloads::ProgressFn {
    let throttle = std::sync::Mutex::new(downloads::ProgressThrottle::new());
    Arc::new(move |completed, total, status, speed, eta| {
        queue.tick(&app, &model, completed, total, &status, speed, eta);
        let due = throttle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .due(completed, total, &status);
        if !due {
            return;
        }
        let _ = app.emit(
            "model-download-progress",
            serde_json::json!({
                "model": model,
                "completed": completed,
                "total": total,
                "status": status,
                "speed_bytes_per_sec": speed,
                "eta_sec": eta,
            }),
        );
    })
}

/// Checkpoint progress: speed and eta are computed by the queue itself (the transport
/// does not send them).
fn image_progress(
    queue: Arc<downloads::Queue>,
    app: tauri::AppHandle,
    filename: String,
) -> Arc<dyn Fn(u64, u64, String) + Send + Sync> {
    Arc::new(move |completed, total, status| {
        queue.tick(&app, &filename, completed, total, &status, 0, 0);
    })
}

/// Response of a command that awaited the download: the queue item has already finished.
fn download_outcome(state: &Arc<AppState>, id: &str) -> Result<(), String> {
    if let Some(result) = state.downloads.outcome(id) {
        return result;
    }
    // A pause is not a failure: the item stayed in the queue and resumes on the button.
    if state.downloads.snapshot().paused {
        return Err("загрузка приостановлена — продолжите её в очереди".into());
    }
    Err("загрузка не завершена — повторите".into())
}

#[tauri::command]
async fn download_model(
    state: tauri::State<'_, Arc<AppState>>,
    app: tauri::AppHandle,
    model: String,
) -> Result<(), String> {
    let state_arc = state.inner().clone();
    let title = model_title(&model);
    start_downloads(&app, &state_arc, vec![downloads::Task::ollama(&model, title)]);
    state_arc.downloads.wait_for(std::slice::from_ref(&model)).await;
    download_outcome(&state_arc, &model)
}

/// What is downloading now: a queue snapshot for the settings screen. A reopened
/// window sees both progress and pause from it.
#[tauri::command]
fn downloads_state(state: tauri::State<'_, Arc<AppState>>) -> downloads::Snapshot {
    state.downloads.snapshot()
}

#[tauri::command]
fn downloads_pause(app: tauri::AppHandle, state: tauri::State<'_, Arc<AppState>>) {
    state.downloads.pause(&app);
}

#[tauri::command]
fn downloads_resume(app: tauri::AppHandle, state: tauri::State<'_, Arc<AppState>>) {
    state.downloads.resume(&app);
}

#[tauri::command]
fn downloads_cancel(app: tauri::AppHandle, state: tauri::State<'_, Arc<AppState>>) {
    state.downloads.cancel(&app);
}

/// Pause, resume and cancel of one queue item: the buttons sit on each
/// row, and previously any of them controlled the whole queue.
#[tauri::command]
fn downloads_pause_task(app: tauri::AppHandle, state: tauri::State<'_, Arc<AppState>>, id: String) {
    state.downloads.pause_task(&app, &id);
}

#[tauri::command]
fn downloads_resume_task(app: tauri::AppHandle, state: tauri::State<'_, Arc<AppState>>, id: String) {
    state.downloads.resume_task(&app, &id);
}

#[tauri::command]
fn downloads_cancel_task(app: tauri::AppHandle, state: tauri::State<'_, Arc<AppState>>, id: String) {
    state.downloads.cancel_task(&app, &id);
}

#[tauri::command]
async fn start_runtime(state: tauri::State<'_, Arc<AppState>>) -> Result<(), String> {
    ensure_runtime(state.inner()).await
}

#[tauri::command]
async fn installed_models(state: tauri::State<'_, Arc<AppState>>) -> Result<Vec<String>, String> {
    let client = state
        .client
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .ok_or_else(|| "Ollama ещё не запущен".to_string())?;
    Ok(models::installed(&client).await)
}

/// Machine snapshot for tuning settings: hardware, downloaded models, image
/// checkpoints and the live context ceiling.
async fn recommend_input(state: &Arc<AppState>) -> recommend::Input {
    // Clone the client on a separate line: a temporary MutexGuard lives to the end of
    // the expression and prevents the future from being Send across await.
    let client = state.client.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let installed_models = match client {
        Some(client) => models::installed(&client).await,
        None => Vec::new(),
    };
    recommend::Input {
        hardware: gpu::gpu_info(),
        system: system_info_blocking().await,
        installed_models,
        checkpoints: comfyui::scan_local_checkpoints(),
        context_ceiling: state.context_ceiling.load(Ordering::Relaxed),
    }
}

/// System information via a blocking thread (A1-05): `system::info` blocks
/// (sysinfo + disk probes) and must not run on the main thread.
async fn system_info_blocking() -> system::SystemInfo {
    match tauri::async_runtime::spawn_blocking(system::info).await {
        Ok(info) => info,
        Err(e) => {
            crate::boot_log(&format!("Сведения о системе не собраны: {e}"));
            system::SystemInfo::default()
        }
    }
}

fn recommend_report(state: &Arc<AppState>, input: &recommend::Input) -> recommend::Report {
    let settings = state.settings.lock().unwrap_or_else(|e| e.into_inner()).clone();
    recommend::Report {
        hardware: recommend::summary(&input.hardware),
        system: input.system.clone(),
        matches: recommend::fingerprint_matches(&settings.tuned_for, &input.hardware),
        tuned_for: settings.tuned_for.clone(),
        items: recommend::plan(&settings, input),
    }
}

/// Whole-system parameters: CPU, memory, disk space.
#[tauri::command]
async fn system_info() -> system::SystemInfo {
    system_info_blocking().await
}

#[derive(serde::Serialize, serde::Deserialize)]
struct TraitTag {
    tag: String,
    intensity: u8,
}

#[derive(serde::Serialize)]
struct TraitCharacter {
    avatar: String,
    name: String,
    /// Tags managed by Cloufam: its own from the store, or if none —
    /// the ones set in SillyTavern.
    tags: Vec<TraitTag>,
    /// Character trait tags from SillyTavern (excluding brand and generation folders):
    /// the "take tags from SillyTavern" button.
    st_tags: Vec<String>,
    no_repeat: bool,
    /// Behavior chaos, 0..100.
    chaos: u8,
    /// Live state is enabled: it changes by itself as the conversation goes.
    live: bool,
    /// State toward the current interlocutor — exactly what the gateway mixes into the prompt.
    state: traits::LiveState,
    /// Bonds with other characters (the manual layer): this same list goes into the card
    /// note block and serves as the target picker for a new bond.
    bonds: Vec<bonds::Bond>,
    fragment: String,
}

/// Intensity of a trait that has not been tuned yet: the same as
/// `traits::card_fragment` substitutes into the card note. Otherwise the first
/// "Save" in the traits panel changed the prompt for every tag from 50 to 60.
const UNCONFIGURED_TAG_INTENSITY: u8 = 50;

/// Characters and their tags with tuned intensity: tags come from SillyTavern
/// (the auto-tagger puts them there), intensity — from our own settings.
#[tauri::command]
async fn traits_state(state: tauri::State<'_, Arc<AppState>>) -> Result<Vec<TraitCharacter>, String> {
    // Runs on a Tauri async IPC worker (not the main thread). A poisoned mutex
    // is not a reason to lose the persona: the `into_inner()` policy (V-13).
    let persona = state.active_persona.lock().unwrap_or_else(|e| e.into_inner()).clone();
    // Reading ST cards and settings — not on the main thread: a synchronous command
    // ran on it and froze both windows for the duration of the scan.
    tauri::async_runtime::spawn_blocking(move || traits_snapshot(&persona))
        .await
        .map_err(|e| format!("черты персонажей не прочитаны: {e}"))
}

fn traits_snapshot(persona: &str) -> Vec<TraitCharacter> {
    let store = traits::load();
    let bond_store = bonds::load();
    // ST's settings.json (~140 KB) is parsed once for all, not once per card.
    let st_tag_map: std::collections::HashMap<String, Vec<String>> =
        traits::st_tagged_characters().into_iter().collect();
    let mut avatars: Vec<(String, String)> = traits::character_list();
    for avatar in store.keys() {
        if !avatars.iter().any(|(a, _)| a == avatar) {
            avatars.push((avatar.clone(), traits::display_name(avatar)));
        }
    }
    avatars
        .into_iter()
        .map(|(avatar, name)| {
            let saved = store.get(&avatar).cloned().unwrap_or_default();
            let saved_bonds = bond_store.get(&avatar).cloned().unwrap_or_default();
            let st_tags = st_tag_map.get(&avatar).cloned().unwrap_or_default();
            // Brand and generation folders do not participate in traits: once
            // saved (including by the "Take tags from SillyTavern" button), they
            // settle into `intensity` forever, and `card_fragment` no longer looks
            // at SillyTavern tags.
            let trait_tags: Vec<String> = st_tags
                .iter()
                .filter(|tag| crate::tag_influence::is_character_trait(tag))
                .cloned()
                .collect();
            // Our own tag list takes precedence: the user could remove extra ones and add
            // their own, and the list must not revert to the SillyTavern state.
            let source: Vec<String> = if saved.intensity.is_empty() {
                trait_tags.clone()
            } else {
                saved.intensity.keys().cloned().collect()
            };
            let tags: Vec<TraitTag> = source
                .into_iter()
                .map(|tag| TraitTag {
                    intensity: saved.intensity.get(&tag).copied().unwrap_or(UNCONFIGURED_TAG_INTENSITY),
                    tag,
                })
                .collect();
            TraitCharacter {
                // The preview shows exactly what the gateway carries into the card
                // (`gateway.rs`: `card_fragment`), not half of it: for
                // a character with untuned traits the preview had one line about
                // repeats, while the model received the list for all tags.
                fragment: card_text_for(&saved, &st_tags, &saved_bonds),
                no_repeat: saved.no_repeat,
                chaos: saved.chaos,
                live: saved.live,
                state: saved.relation(persona),
                st_tags: trait_tags,
                bonds: saved_bonds,
                avatar,
                name,
                tags,
            }
        })
        .collect()
}

/// Card note text: traits, then bonds below. The single glue point,
/// so the preview, the command response and what actually goes into the card do not diverge.
fn card_text_for(
    entry: &traits::CharacterTraits,
    st_tags: &[String],
    saved_bonds: &[bonds::Bond],
) -> String {
    bonds::join(
        &traits::card_fragment(entry, st_tags),
        &bonds::block(saved_bonds),
    )
}

/// Character trait tags from SillyTavern: they supply proportions for the note when
/// there are no traits of our own yet (`traits::card_fragment`).
fn st_tags_for(avatar: &str) -> Vec<String> {
    traits::st_tagged_characters()
        .into_iter()
        .find(|(tagged, _)| tagged == avatar)
        .map(|(_, tags)| tags)
        .unwrap_or_default()
}

/// The key goes into JSON and is matched against ST card names — a path and excessive
/// length are of no use here.
fn check_avatar(avatar: &str) -> Result<(), String> {
    if avatar.is_empty()
        || avatar.len() > 200
        || avatar.contains('/')
        || avatar.contains('\\')
        || avatar.contains("..")
    {
        return Err("Некорректное имя карточки".into());
    }
    Ok(())
}

/// Save the character's traits and carry them to the open SillyTavern: the extension
/// in ST appends the text to the card's `depth_prompt`.
///
/// Threading: async Tauri command; the traits.json write (tmp+fsync+rename) and
/// the reads of ST tags/settings run in `spawn_blocking` so the main thread —
/// which owns both windows — is never frozen by disk work (V-19).
#[tauri::command]
async fn traits_save(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
    avatar: String,
    tags: Vec<TraitTag>,
    no_repeat: bool,
    chaos: Option<u8>,
    live: Option<bool>,
    // State values, if the user adjusted them by hand in the panel.
    live_state: Option<traits::LiveState>,
) -> Result<String, String> {
    // The key goes into JSON and is matched against ST card names — a path is of no use here.
    check_avatar(&avatar)?;
    // Poisoned mutex is not a reason to lose the persona (`into_inner()` policy, V-13).
    let persona = state.active_persona.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let text = tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
        let saved = traits::update(|store| {
            let entry = store.entry(avatar.clone()).or_default();
            entry.no_repeat = no_repeat;
            if let Some(chaos) = chaos {
                entry.chaos = chaos.min(100);
            }
            if let Some(live) = live {
                entry.live = live;
            }
            if let Some(state) = live_state {
                entry.set_state(&persona, state);
            }
            entry.intensity = tags
                .into_iter()
                .map(|t| (t.tag, t.intensity.min(100)))
                .filter(|(tag, _)| !tag.trim().is_empty())
                .collect();
            // The entry stays even when empty: its presence means "Cloufam owns the card",
            // and the extension removes the previous note from the card based on it.
            entry.clone()
        })?;
        // Outside the closure: `bonds::load` and ST tag reading take their own resources, and
        // the std traits lock is not reentrant.
        Ok(card_text_for(&saved, &st_tags_for(&avatar), &bonds::bonds_of(&avatar)))
    })
    .await
    .map_err(|e| format!("черты персонажа не сохранены: {e}"))??;
    sillytavern::push_traits_to_client(&app);
    Ok(text)
}

/// Save the character's bonds with other characters and carry them to the card —
/// the same way as traits: the extension in SillyTavern appends the block to
/// `depth_prompt`.
///
/// Threading: async Tauri command; the bonds.json write (tmp+fsync+rename) and
/// the character/ST tag reads run in `spawn_blocking` so the main thread —
/// which owns both windows — is never frozen by disk work (V-19 class).
#[tauri::command]
async fn bonds_save(
    app: tauri::AppHandle,
    avatar: String,
    bonds: Vec<bonds::Bond>,
) -> Result<String, String> {
    check_avatar(&avatar)?;
    let text = tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
        let known: std::collections::BTreeSet<String> = traits::character_list()
            .into_iter()
            .map(|(card, _)| card)
            .collect();
        // The target must be a catalog card: a bond with a made-up name is something the model
        // would start inventing rather than gossiping about.
        let saved = crate::bonds::set(&avatar, bonds, &known)?;
        let entry = traits::load().remove(&avatar).unwrap_or_default();
        Ok(card_text_for(&entry, &st_tags_for(&avatar), &saved))
    })
    .await
    .map_err(|e| format!("связи персонажа не сохранены: {e}"))??;
    sillytavern::push_traits_to_client(&app);
    Ok(text)
}

/// Return the character's state to an even keel (after a quarrel or a strange scene).
/// We also clear the attitude entry toward the current interlocutor: otherwise the reset "does not hold" —
/// the next reply reads the previous attitude from `relations` and everything comes back.
///
/// Threading: async Tauri command; the traits.json write and ST tag reads run in
/// `spawn_blocking` — a sync command did this on the main thread and froze both
/// windows on every reset (V-19).
#[tauri::command]
async fn traits_reset_state(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
    avatar: String,
) -> Result<String, String> {
    check_avatar(&avatar)?;
    // Poisoned mutex is not a reason to lose the persona (`into_inner()` policy, V-13).
    let persona = state.active_persona.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let text = tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
        let saved = traits::update(|store| {
            let entry = store.entry(avatar.clone()).or_default();
            entry.reset_state(&persona);
            entry.clone()
        })?;
        Ok(card_text_for(&saved, &st_tags_for(&avatar), &bonds::bonds_of(&avatar)))
    })
    .await
    .map_err(|e| format!("состояние персонажа не сброшено: {e}"))??;
    sillytavern::push_traits_to_client(&app);
    Ok(text)
}

/// What this machine can handle and what of that is not yet set in the settings.
#[tauri::command]
async fn recommended_settings(state: tauri::State<'_, Arc<AppState>>) -> Result<recommend::Report, String> {
    let input = recommend_input(state.inner()).await;
    Ok(recommend_report(state.inner(), &input))
}

/// Apply recommendations: `keys` — only the selected settings, empty — all.
/// The path is the same as the "Save" button: recompute the context ceiling,
/// sync SillyTavern, write to disk.
#[tauri::command]
async fn apply_recommended_settings(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
    keys: Option<Vec<String>>,
) -> Result<Settings, String> {
    let input = recommend_input(state.inner()).await;
    // Snapshot — after await: the plan is computed from the settings it will be applied over.
    let current = state.settings.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let planned = recommend::plan(&current, &input);
    let chosen: Vec<recommend::Recommendation> = match &keys {
        Some(keys) if !keys.is_empty() => planned
            .into_iter()
            .filter(|item| keys.iter().any(|k| k == &item.key))
            .collect(),
        _ => planned,
    };
    if chosen.is_empty() {
        return Err("Менять нечего: всё рекомендуемое уже выставлено.".into());
    }
    let titles: Vec<String> = chosen.iter().map(|item| item.title.clone()).collect();
    let fingerprint = recommend::fingerprint(&input.hardware);
    let next = apply_settings_with(&app, |s| {
        *s = recommend::apply(s, &chosen, Some(&fingerprint))?;
        Ok(())
    })
    .await?;
    boot_log(&format!("Подбор под железо: применено — {}", titles.join(", ")));
    Ok(next)
}

/// What of the recommended for this hardware is not downloaded yet. We do not download anything ourselves:
/// the UI shows the list, the user starts the download — it is an option,
/// not an obligation (they may have their own set of models and their own internet).
#[derive(serde::Serialize)]
struct RecommendedDownload {
    /// ollama — the chat/vision/memory model, image — the image checkpoint.
    kind: String,
    id: String,
    title: String,
    size: String,
}

#[tauri::command]
async fn recommended_downloads(state: tauri::State<'_, Arc<AppState>>) -> Result<Vec<RecommendedDownload>, String> {
    // Without a running Ollama the installed list is empty, and all recommended models
    // would look missing — they must not be offered for download.
    let client = state.client.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let installed_models = match client {
        Some(client) => models::installed(&client).await,
        None => return Err("Ollama ещё не запущена — список моделей появится после подъёма.".into()),
    };
    let gpu = gpu::gpu_info();
    let mut list = Vec::new();
    let embed_model = state.settings.lock().unwrap_or_else(|e| e.into_inner()).embed_model.clone();
    // Memory is not switched by tuning while the selected model is on disk: the vectors
    // were built by it. The recommended embedder would then be an extra gigabyte to download.
    let embed_ready = models::is_model_installed(&installed_models, &embed_model);

    // Ollama models: those recommended by the catalog for this hardware tier.
    for model in catalog::visible(&gpu).into_iter().filter(|m| m.recommended) {
        if models::is_model_installed(&installed_models, &model.tag) {
            continue;
        }
        if model.category == "embed" && embed_ready {
            continue;
        }
        list.push(RecommendedDownload {
            kind: "ollama".into(),
            id: model.tag.clone(),
            title: format!("{} ({})", model.name, model.category),
            size: model.size.trim_start_matches('~').to_string(),
        });
    }

    // Image checkpoint: recommended by the showcase and missing from disk.
    let active = state.settings.lock().unwrap_or_else(|e| e.into_inner()).image_model.clone();
    if let Some(model) = comfyui::get_curated_models(&active, &gpu)
        .into_iter()
        .find(|m| m.recommended && !m.is_installed)
    {
        list.push(RecommendedDownload {
            kind: "image".into(),
            id: model.filename.clone(),
            title: model.name.clone(),
            size: model.size.trim_start_matches('~').to_string(),
        });
    }
    Ok(list)
}

/// Download the recommended ones in turn. A failure of one model does not cancel
/// the others: at the end we return the list of what failed. A user
/// cancellation stops the whole queue — they expect the downloads to stop,
/// not the next one to start by itself.
#[tauri::command]
async fn download_recommended(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<Vec<String>, String> {
    let list = recommended_downloads(state.clone()).await?;
    let tasks: Vec<downloads::Task> = list
        .into_iter()
        .map(|item| {
            if item.kind == "image" {
                downloads::Task::image(&item.id, item.title)
            } else {
                downloads::Task::ollama(&item.id, item.title)
            }
        })
        .collect();
    if tasks.is_empty() {
        return Ok(Vec::new());
    }
    // The failure list is collected from our own items: the queue may have contained someone else's
    // downloads (the model menu in SillyTavern, a background vision download), and their failures
    // do not belong in this response.
    let state_arc = state.inner().clone();
    start_downloads(&app, &state_arc, tasks.clone());
    let ids: Vec<String> = tasks.iter().map(|task| task.id.clone()).collect();
    state_arc.downloads.wait_for(&ids).await;
    // A cancelled item is named explicitly: listing "the rest were not downloaded"
    // as a separate line is no longer needed — cancellation closes each item itself.
    let mut failed = Vec::new();
    for task in tasks {
        if let Some(Err(error)) = state_arc.downloads.outcome(&task.id) {
            failed.push(format!("{}: {error}", task.title));
        }
    }
    Ok(failed)
}

/// The first run on this hardware or a GPU replacement: we bring the settings to
/// the recommendations ourselves. User edits after tuning are not overwritten —
/// the hardware fingerprint is recorded in the settings, and tuning does not run again.
/// If there are no models yet, we do not write the fingerprint: tuning will return when
/// there is something to choose from.
///
/// Threading: async fn, runs on the Tauri async runtime during boot
/// (`bring_up_inner`); settings writes go through the guarded `update_settings`
/// and never touch the main thread.
///
/// Policy (V-58) lives in `recommend::auto_tune_plan`: `AutoTunePlan::applicable`
/// never contains items that lower user values (`lowers_user_value`), and a
/// suspicious hardware probe (a laptop's dGPU powered down → `nvidia-smi` fails,
/// DXGI reports the iGPU) yields `None` and no write at all. Lowering items stay
/// suggestions in the `recommended_settings` report — the user confirms them
/// there; they are only named in boot.log here.
async fn auto_tune(app: &tauri::AppHandle, state: &Arc<AppState>) -> Result<(), String> {
    let input = recommend_input(state).await;
    let current = state.settings.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if recommend::fingerprint_matches(&current.tuned_for, &input.hardware) {
        return Ok(());
    }
    // Lowering and disabling are never applied silently (V-58): the suggestions
    // stay in the report — we name them in the log too, the only trace of the pass.
    let Some(plan) = recommend::auto_tune_plan(&current.tuned_for, &current, &input) else {
        // The fingerprint was checked above, so the hardware probe is suspicious
        // (`recommend::detection_suspect`): we touch neither the values nor the fingerprint —
        // one boot with false hardware does not overwrite the settings, and the next one
        // does not bring them back (V-58). Raw suggestions in this branch are computed
        // directly: there is no plan for it.
        let held: Vec<String> = recommend::plan(&current, &input)
            .into_iter()
            .filter(|item| item.lowers_user_value)
            .map(|item| item.title)
            .collect();
        boot_log(
            "Подбор под железо: пропущен — детект железа подозрителен (неудачная проба \
             видеокарты?). Настройки не тронуты, отпечаток не записан",
        );
        log_held_recommendations(&held);
        return Ok(());
    };
    if !plan.applicable.is_empty() {
        let titles: Vec<String> = plan.applicable.iter().map(|item| item.title.clone()).collect();
        // `plan.stamp` is false while no models are downloaded: tuning must
        // return when there is something to choose from.
        let stamp = plan.stamp.then_some(plan.fingerprint.as_str());
        apply_settings_with(app, |s| {
            *s = recommend::apply(s, &plan.applicable, stamp)?;
            Ok(())
        })
        .await?;
        boot_log(&format!("Подбор под железо: применено — {}", titles.join(", ")));
    } else if plan.stamp {
        // Models exist and everything is already as recommended — mark the hardware, otherwise
        // the next start would overwrite a manual edit made after this.
        update_settings(state, |s| {
            s.tuned_for = plan.fingerprint.clone();
            Ok(())
        })?;
    }
    // The plan's suggestions are the same lowering items (V-58): we name them in the log.
    let held: Vec<String> = plan
        .offered
        .iter()
        .map(|item| item.title.clone())
        .collect();
    log_held_recommendations(&held);
    Ok(())
}

/// Lowering tuning items — only a report suggestion; in the boot log
/// we keep their list so the pass is not invisible (V-58).
fn log_held_recommendations(held: &[String]) {
    if !held.is_empty() {
        boot_log(&format!(
            "Подбор под железо: предложено без авто-применения (понизило бы ручные значения) — {}. \
             Применить: окно настроек, раздел «Что эта машина потянет»",
            held.join(", ")
        ));
    }
}

#[tauri::command]
async fn background_vision_download(
    state: tauri::State<'_, Arc<AppState>>,
    app: tauri::AppHandle,
) -> Result<bool, String> {
    spawn_vision_download(state.inner().clone(), app);
    Ok(true)
}

/// SillyTavern install: Node → ST itself → npm install. Progress is reported by the
/// `st-setup-progress` {step, status, completed, total}.
#[tauri::command]
async fn download_sillytavern(
    state: tauri::State<'_, Arc<AppState>>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    state.cancel_download.store(false, Ordering::Relaxed);
    let cancel = state.cancel_download.clone();
    let on_progress: sillytavern::ProgressFn = Arc::new(move |step, status, got, total| {
        let _ = app.emit(
            "st-setup-progress",
            serde_json::json!({ "step": step, "status": status, "completed": got, "total": total }),
        );
    });
    sillytavern::download_node(cancel.clone(), on_progress.clone()).await?;
    sillytavern::download_st(cancel.clone(), on_progress.clone()).await?;
    sillytavern::npm_install(cancel, on_progress).await
}

/// Wizard finale: setup_done and a full boot (Ollama + ST + navigation).
#[tauri::command]
async fn finalize_setup(
    state: tauri::State<'_, Arc<AppState>>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    // The same flag as at startup and "Retry": two parallel boots would
    // run the model store move, tuning and settings writes at the same time.
    if state.boot_in_progress.swap(true, Ordering::Relaxed) {
        return Err("запуск уже выполняется".to_string());
    }
    let _guard = BootFlagGuard(state.inner().clone());
    update_settings(state.inner(), |s| {
        s.setup_done = true;
        Ok(())
    })?;
    let state_arc = state.inner().clone();
    let handle = app.clone();
    let result = bring_up(state_arc, handle).await;
    if let Err(e) = result {
        // setup_done is already saved: on the next start the boot repeats by itself.
        return Err(format!("SillyTavern: {e}"));
    }
    Ok(())
}

#[tauri::command]
fn get_settings(state: tauri::State<'_, Arc<AppState>>) -> Settings {
    state.settings.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

#[tauri::command]
async fn save_settings(app: tauri::AppHandle, settings: Settings) -> Result<(), String> {
    apply_settings(&app, settings).await
}

/// Apply the settings snapshot from the window: recompute the context ceiling,
/// synchronize the ST file and carry the values to the open page.
/// Fields the window does not edit are taken from memory (`keep_backend_fields`).
pub async fn apply_settings(app: &tauri::AppHandle, settings: Settings) -> Result<(), String> {
    apply_settings_with(app, |current| {
        let merged = keep_backend_fields(settings, current);
        *current = merged;
        Ok(())
    })
    .await
    .map(|_| ())
}

/// Change settings under the lock and apply: one path for the app window,
/// the model menu in SillyTavern, hardware tuning and model deletion.
pub(crate) async fn apply_settings_with(
    app: &tauri::AppHandle,
    change: impl FnOnce(&mut Settings) -> Result<(), String>,
) -> Result<Settings, String> {
    use tauri::Manager;
    let state = app.state::<Arc<AppState>>().inner().clone();
    let client_port = state
        .client
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|c| c.base_url.clone())
        .unwrap_or_default();
    let port = ollama_port_from_url(&client_port).unwrap_or(11434);
    // Memory changes BEFORE recomputing the context ceiling: refresh_context_ceiling
    // reads chat_model from state.settings — with the old model the ceiling lies
    // (the new model's KV cache does not fit → OOM → fallback to CPU, 100× slower).
    // save() — inside, before syncing with ST: if the write fails, the ST client does not
    // receive values that are not on disk, and memory stays as before.
    let applied = update_settings(&state, |s| {
        change(s)?;
        Ok(s.clone())
    })?;
    refresh_context_ceiling(&state).await;
    let effective = with_effective_context(&state, &applied);
    // Sync with ST is secondary: a locked or corrupted ST
    // settings.json must not cancel the already saved Cloufam settings.
    let st_err = sillytavern::sync_st_settings(&effective, port).err();
    sillytavern::push_settings_to_client(app, &effective);
    if let Some(e) = st_err {
        boot_log(&format!("SillyTavern: sync_st_settings: {e}"));
    }
    Ok(applied)
}

/// Change settings under the lock and save; memory changes only after
/// a successful write. "Clone → change → save → write outside the lock" lost edits
/// made in parallel (boot, window and gateway write the same settings).
fn update_settings<R>(
    state: &AppState,
    change: impl FnOnce(&mut Settings) -> Result<R, String>,
) -> Result<R, String> {
    let mut guard = state.settings.lock().unwrap_or_else(|e| e.into_inner());
    let mut next = guard.clone();
    let result = change(&mut next)?;
    next.save()?;
    *guard = next;
    Ok(result)
}

/// The window sends the whole settings snapshot — the one it loaded some time earlier.
/// Fields absent from the window are written by the backend (Ollama port, theme and tag versions,
/// tuning fingerprint, the journal read from ST, the model store, the image model —
/// it has its own command), and a stale snapshot would roll them back on any
/// "Save". The list is checked against the Settings.svelte/Wizard.svelte forms — the test
/// `test_window_does_not_edit_backend_fields` keeps it honest.
fn keep_backend_fields(mut incoming: Settings, current: &Settings) -> Settings {
    incoming.ollama_port = current.ollama_port;
    incoming.use_system_ollama = current.use_system_ollama;
    incoming.setup_done = current.setup_done;
    incoming.coder_removed = current.coder_removed;
    incoming.echo_migrated = current.echo_migrated;
    incoming.echo_removed = current.echo_removed;
    incoming.tagger_opt_in_migrated = current.tagger_opt_in_migrated;
    incoming.models_root = current.models_root.clone();
    incoming.models_moved = current.models_moved;
    incoming.theme_version = current.theme_version;
    incoming.tags_version = current.tags_version;
    incoming.image_model = current.image_model.clone();
    incoming.auto_launch_comfy = current.auto_launch_comfy;
    incoming.auto_memory = current.auto_memory;
    incoming.memory_max_chunks = current.memory_max_chunks;
    // `top_k` removed from Settings (V-44: no UI, no consumer); legacy keys stay ignored on load.
    incoming.tuned_for = current.tuned_for.clone();
    incoming.changelog_version_seen = current.changelog_version_seen.clone();
    incoming.models_upgrade_dismissed = current.models_upgrade_dismissed.clone();
    incoming
}

/// Open the SillyTavern folder (data, characters, chats) in Explorer.
#[tauri::command]
fn open_st_folder() -> Result<(), String> {
    #[cfg(windows)]
    {
        std::process::Command::new("explorer")
            .arg(sillytavern::st_dir())
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Reset the setting: bring the wizard back (to repair after manual ST edits).
#[tauri::command]
async fn reset_setup(state: tauri::State<'_, Arc<AppState>>) -> Result<(), String> {
    update_settings(state.inner(), |s| {
        s.setup_done = false;
        Ok(())
    })
}

/// Delete a model from disk to free space.
#[tauri::command]
async fn delete_model(
    state: tauri::State<'_, Arc<AppState>>,
    app: tauri::AppHandle,
    model: String,
) -> Result<(), String> {
    let client = state
        .client
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .ok_or_else(|| "Ollama ещё не запущен".to_string())?;
    models::delete(&client, &model).await?;

    // If the active model was deleted, automatically switch the settings to a remaining one.
    let installed = models::installed(&client).await;
    let affected = {
        let mut probe = state.settings.lock().unwrap_or_else(|e| e.into_inner()).clone();
        forget_deleted_model(&mut probe, &model, &installed)
    };
    if affected {
        apply_settings_with(&app, |s| {
            forget_deleted_model(s, &model, &installed);
            Ok(())
        })
        .await?;
    }
    Ok(())
}

/// Whether a model looks like a chat model — the same rule as `isLikelyChatModel` in
/// the frontend (src/lib/models.ts): embeddings, vision, speech and image models do not qualify for chat.
fn is_likely_chat_model(tag: &str) -> bool {
    const NOT_CHAT: [&str; 7] = ["embed", "bge", "nomic", "minicpm", "whisper", "tts", "stable-diffusion"];
    let lower = tag.to_lowercase();
    if NOT_CHAT.iter().any(|needle| lower.contains(needle)) {
        return false;
    }
    // `vl` — only as a separate tag word (qwen3-vl-…), not as letters inside a name.
    !lower.split(['/', ':', '_', '-']).any(|part| part == "vl")
}

/// Remove a deleted model from settings: main roles move to the remaining
/// model of the same role, assistants — to "same as main". Returns whether there was anything to change.
fn forget_deleted_model(s: &mut Settings, model: &str, installed: &[String]) -> bool {
    let mut changed = false;

    if models::is_model_installed(&[s.chat_model.clone()], model) {
        let next_chat = installed
            .iter()
            .find(|m| is_likely_chat_model(m))
            .cloned()
            .unwrap_or_default();
        s.chat_model = next_chat;
        changed = true;
    }
    if models::is_model_installed(&[s.embed_model.clone()], model) {
        let next_embed = installed
            .iter()
            .find(|m| m.contains("embed") || m.contains("bge") || m.contains("nomic"))
            .cloned()
            .unwrap_or_default();
        s.embed_model = next_embed;
        changed = true;
    }
    if models::is_model_installed(&[s.vision_model.clone()], model) {
        // We look for vision in the catalog: the qwen3.5 line is multimodal, its tags do not
        // contain "vl", and the old heuristic would leave the field empty.
        let next_vision = installed
            .iter()
            .find(|m| {
                crate::catalog::catalog()
                    .iter()
                    .any(|c| c.tag == **m && c.category == "vision")
                    || m.contains("vl")
                    || m.contains("minicpm")
            })
            .cloned()
            .unwrap_or_default();
        s.vision_model = next_vision;
        changed = true;
    }
    // Explicitly selected assistant models: an empty string in their contract means
    // "same as main/auto". Without a reset the deleted model would stay in
    // the settings forever, and background evaluation and the tagger would silently stop
    // working — the error is visible only in boot.log.
    if !s.evaluator_model.is_empty()
        && models::is_model_installed(&[s.evaluator_model.clone()], model)
    {
        s.evaluator_model = String::new();
        changed = true;
    }
    if !s.tagger_model.is_empty()
        && models::is_model_installed(&[s.tagger_model.clone()], model)
    {
        s.tagger_model = String::new();
        changed = true;
    }
    changed
}

/// Move roles from the old model to the new one: all settings that pointed to
/// `from` begin to point to `to`. Returns whether there was anything to change.
fn retarget_model(s: &mut Settings, from: &str, to: &str) -> bool {
    let mut changed = false;
    for field in [
        &mut s.chat_model,
        &mut s.vision_model,
        &mut s.embed_model,
        &mut s.evaluator_model,
        &mut s.tagger_model,
    ] {
        if models::is_model_installed(&[field.clone()], from) {
            *field = to.to_string();
            changed = true;
        }
    }
    changed
}

/// Dismissed "Later" model upgrade pairs: from-tags comma-separated.
fn parse_dismissed(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// State of the model upgrade flow for the settings window.
#[derive(serde::Serialize)]
struct UpgradeState {
    /// "Old → new" pairs worth upgrading (excluding dismissed ones).
    offers: Vec<catalog::UpgradeOffer>,
    /// What the user has already hidden with the "Later" button.
    dismissed: Vec<String>,
}

/// Which installed models of the previous line are worth upgrading. Empty — nothing
/// to upgrade (or everything is already upgraded: the suggestion disappears by itself).
#[tauri::command]
async fn models_upgrade_state(
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<UpgradeState, String> {
    let client = state
        .client
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .ok_or_else(|| "Ollama ещё не запущен".to_string())?;
    let installed = models::installed(&client).await;
    let dismissed = parse_dismissed(
        &state.settings.lock().unwrap_or_else(|e| e.into_inner()).models_upgrade_dismissed,
    );
    let offers = catalog::upgrade_offers(&installed)
        .into_iter()
        .filter(|offer| !dismissed.contains(&offer.from))
        .collect();
    Ok(UpgradeState { offers, dismissed })
}

/// Upgrade a model per the map rule: download the new one, move roles from the old
/// to it and only then delete the old one. Deletion happens strictly after a successful
/// download — a network failure does not leave the user without a model.
#[tauri::command]
async fn models_upgrade_apply(
    state: tauri::State<'_, Arc<AppState>>,
    app: tauri::AppHandle,
    from: String,
    to: String,
) -> Result<(), String> {
    let rule = catalog::MODEL_UPGRADES
        .iter()
        .find(|r| r.from == from && r.to == to)
        .ok_or_else(|| format!("нет правила обновления {from} → {to}"))?;
    let state_arc = state.inner().clone();
    let to_tag = rule.to.to_string();
    let client = state_arc
        .client
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .ok_or_else(|| "Ollama ещё не запущен".to_string())?;

    // 1. The new model — through the usual download queue with its progress.
    //    Already downloaded (by another role or manually) — no download needed, only
    //    the role switch and deletion of the old one remain.
    let already = models::is_model_installed(&models::installed(&client).await, &to_tag);
    if !already {
        let title = model_title(&to_tag);
        start_downloads(&app, &state_arc, vec![downloads::Task::ollama(&to_tag, title)]);
        state_arc.downloads.wait_for(std::slice::from_ref(&to_tag)).await;
        download_outcome(&state_arc, &to_tag)?;
    }

    // 2. Roles have moved — deleting the old one will not disable anything now.
    apply_settings_with(&app, |s| {
        retarget_model(s, rule.from, rule.to);
        Ok(())
    })
    .await?;

    // 3. Delete the old one (a live Ollama is needed).
    models::delete(&client, rule.from).await?;
    boot_log(&format!("Модели: обновлена {from} → {to}, старая удалена"));
    Ok(())
}

/// "Later": the pair is hidden from suggestions; new upgrade map rules
/// (`catalog::MODEL_UPGRADES`) will show up again.
#[tauri::command]
async fn models_upgrade_dismiss(app: tauri::AppHandle, from: String) -> Result<(), String> {
    apply_settings_with(&app, |s| {
        let mut dismissed = parse_dismissed(&s.models_upgrade_dismissed);
        if !dismissed.contains(&from) {
            dismissed.push(from.clone());
            s.models_upgrade_dismissed = dismissed.join(",");
        }
        Ok(())
    })
    .await?;
    Ok(())
}

/// Open the folder with model weights in Windows Explorer.
#[tauri::command]
fn open_models_folder() -> Result<(), String> {
    #[cfg(windows)]
    {
        std::process::Command::new("explorer")
            .arg(paths::models_dir())
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Open a link from the "About" window in the default browser. The addresses are a
/// fixed list: the command does not open an arbitrary URL from the page.
#[tauri::command]
fn open_about_link(target: String) -> Result<(), String> {
    let url = match target.as_str() {
        "repo" => "https://github.com/AndreyFSDev/Cloufam",
        "license" => "https://www.gnu.org/licenses/agpl-3.0.html",
        _ => return Err(format!("неизвестная ссылка: {target}")),
    };
    #[cfg(windows)]
    {
        std::process::Command::new("explorer")
            .arg(url)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// State of the fine-tuning runtime and availability of fine-tuning on this machine.
/// The command is async: the version probe launches python with a torch import, and on
/// the main thread it froze the UI for seconds.
#[derive(serde::Serialize)]
pub struct TrainerInfo {
    pub available: bool,
    pub reason: Option<String>,
    pub runtime: pyruntime::PyruntimeStatus,
    /// Epoch step divisor (`batch × grad_accum` from `trainer::tier_profile`):
    /// the UI computes "N steps · ≈time" with this field, not with its own copy of the numbers (V-50).
    pub step_divisor: u32,
}

#[tauri::command]
async fn trainer_status() -> TrainerInfo {
    let availability = trainer::availability();
    let step_divisor = trainer::tier_profile(crate::gpu::gpu_info().tier)
        .map(|(_, batch, grad_accum, _)| batch * grad_accum)
        .unwrap_or(8);
    // Importing torch in the venv is blocking work for seconds: in an async command
    // it held a tokio worker, and a hung probe accumulated processes on every tick.
    let runtime = tauri::async_runtime::spawn_blocking(pyruntime::status)
        .await
        .unwrap_or_default();
    TrainerInfo {
        available: availability.is_ok(),
        reason: availability.err(),
        runtime,
        step_divisor,
    }
}

#[tauri::command]
async fn trainer_install(app: tauri::AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn(async move {
        if let Err(e) = pyruntime::install(app.clone()).await {
            eprintln!("установка рантайма дообучения: {e}");
        }
    });
    Ok(())
}

#[tauri::command]
fn trainer_cancel() {
    pyruntime::cancel();
}

#[tauri::command]
fn training_presets() -> Vec<trainer::TrainPreset> {
    trainer::presets()
}

/// Time estimate per step for a "preset + sample length" pair: the formula lives in
/// `trainer.rs`, so calibration is in one place and not duplicated in the UI.
#[tauri::command]
fn training_estimate(preset_id: String, max_seq_length: Option<u32>) -> Result<f64, String> {
    let preset = trainer::preset_by_id(&preset_id)
        .ok_or_else(|| format!("неизвестный пресет: {preset_id}"))?;
    let tier = crate::gpu::gpu_info().tier;
    // The same length trainer::start will take: a request above the tier ceiling is trimmed.
    let seq = trainer::training_seq(tier, max_seq_length).unwrap_or(512);
    Ok(trainer::estimate_seconds_per_step(tier, preset.params_b, seq))
}

/// Parsing a large file — not for the main thread (it also serves the window).
#[tauri::command]
async fn training_import(path: String) -> Result<dataset::DatasetInfo, String> {
    tauri::async_runtime::spawn_blocking(move || dataset::import(std::path::Path::new(&path)))
        .await
        .map_err(|e| e.to_string())?
}

/// Threading: async Tauri command; `dataset::list()` walks the import folder on
/// disk, so it runs in `spawn_blocking` instead of the main thread (V-19).
#[tauri::command]
async fn training_datasets() -> Result<Vec<dataset::DatasetInfo>, String> {
    tauri::async_runtime::spawn_blocking(dataset::list)
        .await
        .map_err(|e| format!("датасеты не прочитаны: {e}"))
}

/// Threading: async Tauri command; `trainer::start` validates the dataset on
/// disk and spawns the python process, so it runs in `spawn_blocking` — a sync
/// command did all of this on the main thread (V-19).
#[tauri::command]
async fn training_start(
    app: tauri::AppHandle,
    preset_id: String,
    dataset: String,
    epochs: f32,
    max_seq_length: Option<u32>,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        trainer::start(app, &preset_id, &dataset, epochs.clamp(1.0, 20.0), max_seq_length)
    })
    .await
    .map_err(|e| format!("обучение не запущено: {e}"))?
}

#[tauri::command]
fn training_state() -> trainer::TrainingState {
    trainer::status()
}

/// Threading: async Tauri command; `cancel()` kills the process tree (taskkill),
/// `clear()` cleans the run directories — blocking, not for the main thread (A3-08).
#[tauri::command]
async fn training_cancel() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(trainer::cancel)
        .await
        .map_err(|e| format!("отмена не дошла: {e}"))?
}

#[tauri::command]
async fn training_clear() {
    let _ = tauri::async_runtime::spawn_blocking(trainer::clear).await;
}

/// The export takes minutes (conversion to GGUF and `ollama create`), so it is
/// async. The base is installed by the app's download queue: it has a direct
/// registry download when DNS fails, whereas `ollama create` would pull the base itself and
/// fail with "no such host".
#[tauri::command]
async fn training_export(
    state: tauri::State<'_, Arc<AppState>>,
    app: tauri::AppHandle,
    preset_id: String,
    adapter_dir: String,
    name: String,
) -> Result<String, String> {
    let preset = trainer::preset_by_id(&preset_id)
        .ok_or_else(|| format!("неизвестный пресет: {preset_id}"))?;
    text_lora::checked_adapter_dir(&adapter_dir)?;
    let state_arc = state.inner().clone();
    let base = preset.ollama_tag;
    // The download queue is sequential: a queued base would wait for someone else's download
    // (or forever — a paused queue), although it does not need to be installed.
    let client = state_arc.client.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let base_installed = match client {
        Some(client) => models::is_model_installed(&models::installed(&client).await, &base),
        None => false,
    };
    if !base_installed {
        start_downloads(&app, &state_arc, vec![downloads::Task::ollama(&base, model_title(&base))]);
        state_arc.downloads.wait_for(std::slice::from_ref(&base)).await;
        download_outcome(&state_arc, &base)?;
    }
    tauri::async_runtime::spawn_blocking(move || {
        text_lora::export(&app, &preset_id, &adapter_dir, &name)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
fn training_open_import_folder() -> Result<(), String> {
    #[cfg(windows)]
    {
        std::fs::create_dir_all(paths::training_import_dir()).map_err(|e| e.to_string())?;
        std::process::Command::new("explorer")
            .arg(paths::training_import_dir())
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// LoRA fine-tuning for image models: presets are checkpoints on disk,
/// training runs on the kohya sd-scripts stack from `runtime\trainer\venv-sd`.
/// Threading: async Tauri command; the preset list comes from scanning
/// checkpoints on disk (safetensors headers included), so it runs in
/// `spawn_blocking` instead of the main thread (same class as V-52).
#[tauri::command]
async fn image_training_presets() -> Result<Vec<image_trainer::ImageTrainPreset>, String> {
    tauri::async_runtime::spawn_blocking(image_trainer::presets)
        .await
        .map_err(|e| format!("пресеты не прочитаны: {e}"))
}

/// Run time estimate: calibration lives in `image_trainer.rs`, the UI
/// only displays it in a convenient form.
///
/// Threading: async Tauri command; resolving the preset rescans the checkpoint
/// store (reads safetensors headers of every file) and the UI calls this on
/// every keystroke in the «steps» field — `spawn_blocking`, never the main
/// thread (V-52). A module-level cache for the scan itself is a separate change
/// in `image_trainer.rs`.
#[tauri::command]
async fn image_training_estimate(preset_id: String, steps: u32) -> Result<f64, String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<f64, String> {
        let preset = image_trainer::preset_by_id(&preset_id)
            .ok_or_else(|| format!("неизвестный пресет: {preset_id}"))?;
        Ok(image_trainer::estimate_total_seconds(
            crate::gpu::gpu_info().tier,
            preset.family_kind(),
            preset.resolution,
            steps,
        ))
    })
    .await
    .map_err(|e| format!("оценка времени не посчитана: {e}"))?
}

/// Threading: async Tauri command; `image_trainer::datasets()` copies/reads
/// dataset folders, so it runs in `spawn_blocking` (V-19).
#[tauri::command]
async fn image_training_datasets() -> Result<Vec<image_trainer::ImageDatasetInfo>, String> {
    tauri::async_runtime::spawn_blocking(image_trainer::datasets)
        .await
        .map_err(|e| format!("датасеты не прочитаны: {e}"))
}

/// Copying the image folder — not for the main thread (it also serves the window).
#[tauri::command]
async fn image_training_import(path: String) -> Result<image_trainer::ImageDatasetInfo, String> {
    tauri::async_runtime::spawn_blocking(move || image_trainer::import(std::path::Path::new(&path)))
        .await
        .map_err(|e| e.to_string())?
}

/// WD14 tagging takes minutes per hundred images: the command is async and moves
/// the work to a separate thread, otherwise the main thread would wait for it entirely.
#[tauri::command]
async fn image_training_caption(dataset: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || image_trainer::caption_dataset(&dataset))
        .await
        .map_err(|e| e.to_string())?
}

/// Threading: async Tauri command; `image_trainer::start` checks the dataset on
/// disk and spawns the kohya python process, so it runs in `spawn_blocking` — a
/// sync command did this on the main thread (V-19).
#[tauri::command]
async fn image_training_start(
    app: tauri::AppHandle,
    preset_id: String,
    dataset: String,
    repeats: u32,
    name: String,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        image_trainer::start(app, &preset_id, &dataset, repeats, &name)
    })
    .await
    .map_err(|e| format!("обучение картинок не запущено: {e}"))?
}

/// The state is async: it recomputes environment readiness (checkpoint headers
/// on disk), and the screen polls it every two seconds.
#[tauri::command]
async fn image_training_state() -> image_trainer::ImageTrainingState {
    // status() reads checkpoint headers and (once a minute) tries importing
    // onnxruntime — this is blocking work, not for a tokio worker.
    tauri::async_runtime::spawn_blocking(image_trainer::status)
        .await
        .unwrap_or_default()
}

#[tauri::command]
fn image_training_cancel() -> Result<(), String> {
    image_trainer::cancel()
}

#[tauri::command]
fn image_training_clear() {
    image_trainer::clear();
}

#[tauri::command]
fn image_training_open_folder(folder: String) -> Result<(), String> {
    image_trainer::open_folder(&folder)
}

/// The full chain of error causes: without it network failures look identical
/// ("error sending request"), and it is impossible to tell what exactly happened — DNS, TLS or
/// a proxy.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut parts = vec![error.to_string()];
    let mut source = error.source();
    while let Some(inner) = source {
        let text = inner.to_string();
        if parts.last().map(|last| last != &text).unwrap_or(true) {
            parts.push(text);
        }
        source = inner.source();
    }
    parts.join(" → ")
}

/// Create a Cloufam shortcut on the desktop (for a portable copy and in case
/// the installer did not create a shortcut).
#[tauri::command]
async fn create_desktop_shortcut() -> Result<String, String> {
    // PowerShell with `.output()` is a blocking call of 0.5–2 s; in a synchronous
    // command it froze the settings window.
    tauri::async_runtime::spawn_blocking(|| create_desktop_shortcut_blocking(true))
        .await
        .map_err(|e| format!("ярлык: {e}"))?
}

/// A desktop shortcut — once per install. The button in settings overwrites
/// an existing one; the app start does not. The absence of the file does not distinguish "never was" from
/// "deleted by hand", so the fact of creation is remembered by a marker in data.
fn ensure_desktop_shortcut() {
    let marker = paths::data_dir().join(".desktop-shortcut");
    if marker.exists() {
        return;
    }
    std::thread::spawn(move || match create_desktop_shortcut_blocking(false) {
        Ok(path) => {
            let _ = std::fs::write(&marker, &path);
            boot_log(&format!("Ярлык на рабочем столе: {path}"));
        }
        Err(e) => boot_log(&format!("Ярлык на рабочем столе не создан: {e}")),
    });
}

fn create_desktop_shortcut_blocking(overwrite: bool) -> Result<String, String> {
    #[cfg(windows)]
    {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let exe_text = ps_single_quoted(&exe.display().to_string());
        let dir_text = ps_single_quoted(&exe.parent().map(|p| p.display().to_string()).unwrap_or_default());
        let keep_existing = if overwrite {
            ""
        } else {
            "if (Test-Path -LiteralPath $p) { Write-Output $p; exit 0 };"
        };
        let script = format!(
            "$d=[Environment]::GetFolderPath('Desktop');$p=Join-Path $d 'Cloufam.lnk';{keep_existing}\
             $s=(New-Object -ComObject WScript.Shell).CreateShortcut($p);\
             $s.TargetPath='{exe_text}';$s.WorkingDirectory='{dir_text}';$s.IconLocation='{exe_text},0';$s.Description='Cloufam';$s.Save();\
             Write-Output $p"
        );
        let mut cmd = std::process::Command::new("powershell");
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        let out = cmd.output().map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(format!(
                "не удалось создать ярлык: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    #[cfg(not(windows))]
    Err("ярлыки создаются только в Windows".into())
}

/// Text for a PowerShell line in single quotes: an apostrophe in the profile path
/// (`C:\Users\O'Brien`) would otherwise close the string and break — or append to — the script.
fn ps_single_quoted(text: &str) -> String {
    text.replace('\'', "''")
}

/// Open the program root folder in Explorer.
#[tauri::command]
fn open_app_folder() -> Result<(), String> {
    #[cfg(windows)]
    {
        std::process::Command::new("explorer")
            .arg(paths::base_dir())
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Isolation check line: an external path that must not exist, or
/// an internal directory that must exist.
#[derive(serde::Serialize)]
pub struct IsolationEntry {
    pub label: String,
    pub path: String,
    pub exists: bool,
    pub size_bytes: u64,
    /// true — a path outside the program folder (existence = leak).
    pub outside: bool,
}

fn dir_size_inner(path: &std::path::Path, depth: u32) -> u64 {
    if depth == 0 {
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => dir_size_inner(&entry.path(), depth - 1),
            Ok(kind) if kind.is_file() => entry.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

fn dir_size(path: &std::path::Path) -> u64 {
    // Junction/symlink loops on Windows (node_modules, .venv) cause
    // infinite recursion → stack overflow. We limit the traversal depth.
    dir_size_inner(path, 20)
}

/// The "everything in one folder" check: what ended up outside the program directory.
/// Scanning the model store and the program folder (tens of gigabytes) is not on the main
/// thread: a synchronous command ran on it and froze both windows.
#[tauri::command]
async fn isolation_report() -> Result<Vec<IsolationEntry>, String> {
    tauri::async_runtime::spawn_blocking(isolation_entries)
        .await
        .map_err(|e| format!("проверка изоляции не выполнена: {e}"))
}

fn isolation_entries() -> Vec<IsolationEntry> {
    let mut entries: Vec<(String, std::path::PathBuf, bool)> = vec![
        (
            "Кэш pip".into(),
            std::env::var_os("LOCALAPPDATA")
                .map(|p| std::path::PathBuf::from(p).join("pip"))
                .unwrap_or_default(),
            true,
        ),
        (
            "Кэш Hugging Face".into(),
            std::env::var_os("USERPROFILE")
                .map(|p| std::path::PathBuf::from(p).join(".cache").join("huggingface"))
                .unwrap_or_default(),
            true,
        ),
        (
            "Состояние окна Tauri (старое)".into(),
            std::env::var_os("APPDATA")
                .map(|p| std::path::PathBuf::from(p).join("app.localchatbot.desktop"))
                .unwrap_or_default(),
            true,
        ),
        (
            "Каталог программы".into(),
            paths::base_dir(),
            false,
        ),
        ("Кэш pip приложения".into(), paths::pip_cache_dir(), false),
        (
            "Склад моделей (Ollama, ComfyUI, HF)".into(),
            paths::models_root(),
            false,
        ),
        ("Модели Ollama".into(), paths::models_dir(), false),
        ("Кэш HF приложения".into(), paths::hf_cache_dir(), false),
        ("Временные файлы".into(), paths::tmp_dir(), false),
    ];

    entries.retain(|(_, path, _)| !path.as_os_str().is_empty());

    let mut result: Vec<IsolationEntry> = entries
        .into_iter()
        .map(|(label, path, outside)| IsolationEntry {
            label,
            exists: path.exists(),
            size_bytes: dir_size(&path),
            path: path.display().to_string(),
            outside,
        })
        .collect();
    result.sort_by(|a, b| b.outside.cmp(&a.outside));
    result
}

/// Open or focus the separate window for model and environment settings.
/// The command is async: window creation goes to the main thread, while a synchronous
/// command would run on it too and hang the app (the "Settings
/// Cloufam" item in SillyTavern and the Ctrl+, hotkey).
#[tauri::command]
async fn open_settings_window(app: tauri::AppHandle) -> Result<(), String> {
    show_settings_window(&app);
    Ok(())
}

/// User guide (data embedded in the binary). Runs on the IPC
/// Tauri thread: data is static, no IO or locks, the call is thread-safe.
#[tauri::command]
fn guide_data() -> Vec<guide::GuideSection> {
    guide::guide()
}

#[tauri::command]
fn mark_st_opened(state: tauri::State<Arc<AppState>>) {
    state.st_opened.store(true, Ordering::Relaxed);
}

#[tauri::command]
fn is_st_opened(state: tauri::State<Arc<AppState>>) -> bool {
    state.st_opened.load(Ordering::Relaxed)
}

/// What the loaded model is actually considered to be. With an unsupported
/// GPU, Ollama silently falls back to CPU, so the only honest source is `/api/ps`.
#[derive(serde::Serialize)]
pub struct GpuUsage {
    pub loaded: bool,
    pub model: Option<String>,
    pub size_vram_mb: u64,
    pub size_total_mb: u64,
    pub on_gpu: bool,
    /// How many context tokens actually fit in VRAM (0 — unknown).
    pub context_ceiling: u32,
    /// Whether the model fits entirely in VRAM.
    pub model_fits: bool,
    /// Free on the GPU now, the budget for our model, the required minimum and the total (MB).
    pub vram_free_mb: u64,
    pub vram_budget_mb: u64,
    pub vram_needed_mb: u64,
    pub vram_total_mb: u64,
}

#[tauri::command]
async fn gpu_usage(state: tauri::State<'_, Arc<AppState>>) -> Result<GpuUsage, String> {
    // The settings screen asks for the status on open and via the "Refresh" button —
    // we also recompute the context ceiling: VRAM may have been freed.
    let budget = refresh_context_ceiling(state.inner()).await;
    let ceiling = state.context_ceiling.load(Ordering::Relaxed);
    let fits = state.model_fits.load(Ordering::Relaxed);
    let vram_free_mb = budget.map(|b| b.free_mb).unwrap_or(0);
    let vram_budget_mb = budget.map(|b| b.budget_mb).unwrap_or(0);
    let vram_needed_mb = budget.map(|b| b.needed_mb).unwrap_or(0);
    let vram_total_mb = budget.map(|b| b.total_mb).unwrap_or(0);
    let empty = GpuUsage {
        loaded: false,
        model: None,
        size_vram_mb: 0,
        size_total_mb: 0,
        on_gpu: false,
        context_ceiling: ceiling,
        model_fits: fits,
        vram_free_mb,
        vram_budget_mb,
        vram_needed_mb,
        vram_total_mb,
    };
    let port = gateway::ollama_port();
    // Loopback Ollama /api/ps (127.0.0.1): bypass the system proxy (otherwise
    // the user's machine proxy answers 503 instead of the local server).
    // One client for all polls (every 10 s): a new builder means a new pool and TLS config per call
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let client = match CLIENT.get() {
        Some(client) => client,
        None => {
            let built = reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_millis(200))
                .timeout(std::time::Duration::from_secs(5))
                .no_proxy()
                .build()
                .map_err(|e| e.to_string())?;
            CLIENT.get_or_init(|| built)
        }
    };
    let resp = match client.get(format!("http://127.0.0.1:{port}/api/ps")).send().await {
        Ok(resp) => resp,
        Err(_) => return Ok(empty),
    };
    let body: serde_json::Value = match resp.json().await {
        Ok(body) => body,
        Err(_) => return Ok(empty),
    };
    let Some(first) = body["models"].as_array().and_then(|arr| arr.first()) else {
        return Ok(empty);
    };
    let size_vram = first["size_vram"].as_u64().unwrap_or(0);
    Ok(GpuUsage {
        loaded: true,
        model: first["name"].as_str().map(|s| s.to_string()),
        size_vram_mb: size_vram / (1024 * 1024),
        size_total_mb: first["size"].as_u64().unwrap_or(0) / (1024 * 1024),
        on_gpu: size_vram > 0,
        context_ceiling: ceiling,
        model_fits: fits,
        vram_free_mb,
        vram_budget_mb,
        vram_needed_mb,
        vram_total_mb,
    })
}

/// List of image generation models (ComfyUI checkpoints) for this hardware.
///
/// Threading: async Tauri command; scanning the checkpoint folder is disk work,
/// so it runs in `spawn_blocking` instead of the main thread (V-19).
#[tauri::command]
async fn get_image_models(state: tauri::State<'_, Arc<AppState>>) -> Result<Vec<comfyui::ImageModelInfo>, String> {
    let active = state.settings.lock().unwrap_or_else(|e| e.into_inner()).image_model.clone();
    tauri::async_runtime::spawn_blocking(move || comfyui::get_curated_models(&active, &gpu::gpu_info()))
        .await
        .map_err(|e| format!("модели картинок не прочитаны: {e}"))
}

/// Selecting the active image generation model.
///
/// Threading: async Tauri command; the settings write (tmp+fsync+rename) and the
/// re-sync of ST's ~140 KB settings.json run in `spawn_blocking` — a sync
/// command did both on the main thread (V-19).
#[tauri::command]
async fn set_image_model(
    model: String,
    state: tauri::State<'_, Arc<AppState>>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let s = update_settings(&state, |s| {
            s.image_model = model;
            Ok(s.clone())
        })?;
        push_image_settings(&app, &state, &s);
        Ok(())
    })
    .await
    .map_err(|e| format!("модель картинок не выбрана: {e}"))?
}

/// Carry the image model change to SillyTavern — the file and the open page.
/// The context ceiling does not depend on it, so no recomputation. A failed
/// ST sync goes only to the log: the settings are already saved, and the model file may already be deleted.
fn push_image_settings(app: &tauri::AppHandle, state: &AppState, s: &Settings) {
    let effective = with_effective_context(state, s);
    if let Err(e) = sillytavern::sync_st_settings(&effective, s.ollama_port) {
        boot_log(&format!("SillyTavern: sync_st_settings: {e}"));
    }
    sillytavern::push_settings_to_client(app, &effective);
}

/// ComfyUI status (running / port / path). Not on the main thread: the port probe
/// with ComfyUI off always waits its 600 ms, and the window froze for that time.
#[tauri::command]
async fn get_comfy_status() -> Result<comfyui::ComfyStatus, String> {
    tauri::async_runtime::spawn_blocking(|| comfyui::get_status(8188))
        .await
        .map_err(|e| format!("статус ComfyUI не получен: {e}"))
}

/// Starting the ComfyUI process. A synchronous command would freeze the window: inside
/// there is a `pip install` wait and a two-second pause before the port probe.
#[tauri::command]
async fn start_comfy_server() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(|| comfyui::start_comfy(8188))
        .await
        .map_err(|e| format!("не удалось запустить ComfyUI: {e}"))?
}

/// Installing ComfyUI into the app directory.
#[tauri::command]
async fn install_comfyui(
    state: tauri::State<'_, Arc<AppState>>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    if catalog::image_ceiling(&gpu::gpu_info()).is_none() {
        return Err(
            "Генерация картинок требует дискретной видеокарты: на этой машине она недоступна".into(),
        );
    }
    let cancel = comfyui::install_cancel_flag();
    let app_clone = app.clone();
    let on_progress = Arc::new(move |got: u64, total: u64, status: String| {
        let _ = app_clone.emit(
            "comfy-setup-progress",
            serde_json::json!({ "completed": got, "total": total, "status": status }),
        );
    });
    comfyui::install_comfyui(cancel, on_progress).await?;
    // The same as the app boot does: without this a fresh ComfyUI did not see
    // the checkpoint store until a restart, and ST stayed on sd.source "none".
    match tauri::async_runtime::spawn_blocking(models::link_comfy_models).await {
        Ok(Ok(action)) if !action.is_empty() => boot_log(&format!("Склад моделей: {action}")),
        Ok(Ok(_)) => {}
        Ok(Err(e)) => boot_log(&format!("Склад моделей: ComfyUI: {e}")),
        Err(e) => boot_log(&format!("Склад моделей: ComfyUI: {e}")),
    }
    if let Err(e) = sillytavern::patch_sd_russian_triggers() {
        boot_log(&format!("SillyTavern: SD-триггеры: {e}"));
    }
    let settings = state.settings.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let effective = with_effective_context(&state, &settings);
    if let Err(e) = sillytavern::sync_st_settings(&effective, settings.ollama_port) {
        boot_log(&format!("SillyTavern: после установки ComfyUI: {e}"));
    }
    sillytavern::push_settings_to_client(&app, &effective);
    Ok(())
}

/// Downloading an image generation model checkpoint into the ComfyUI directory.
#[tauri::command]
async fn download_image_model(
    filename: String,
    state: tauri::State<'_, Arc<AppState>>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let state_arc = state.inner().clone();
    let title = image_model_title(&filename);
    start_downloads(&app, &state_arc, vec![downloads::Task::image(&filename, title)]);
    state_arc.downloads.wait_for(std::slice::from_ref(&filename)).await;
    download_outcome(&state_arc, &filename)
}

/// Delete the ComfyUI image generation checkpoint file from disk.
#[tauri::command]
async fn delete_image_model(
    state: tauri::State<'_, Arc<AppState>>,
    app: tauri::AppHandle,
    filename: String,
) -> Result<(), String> {
    comfyui::delete_image_model(&filename).await?;

    let active = state.settings.lock().unwrap_or_else(|e| e.into_inner()).image_model.clone();
    if !active.eq_ignore_ascii_case(&filename) {
        return Ok(());
    }
    let remaining = comfyui::scan_local_checkpoints();
    // Only an image model can become the next one: otherwise the audio checkpoint
    // from the same folder would get `image_model`, and generation would break silently.
    let next = remaining
        .iter()
        .find(|(name, _)| !name.eq_ignore_ascii_case(&filename) && comfyui::is_image_checkpoint(name))
        .map(|(name, _)| name.clone())
        .unwrap_or_default();
    let s = update_settings(state.inner(), |s| {
        // While the deletion was running, another model could be selected — we leave it alone.
        if s.image_model.eq_ignore_ascii_case(&filename) {
            s.image_model = next;
        }
        Ok(s.clone())
    })?;
    // An open ST keeps the deleted `sd.model` in memory and would write it back.
    push_image_settings(&app, &state, &s);
    Ok(())
}

#[derive(serde::Serialize, Clone)]
pub struct AppUpdateInfo {
    pub available: bool,
    pub current_version: String,
    pub latest_version: String,
    pub notes: Option<String>,
    /// Address and minisign signature of the exe itself: `make-release` puts them in the manifest.
    pub exe_url: Option<String>,
    pub exe_signature: Option<String>,
}

#[derive(serde::Serialize, Clone)]
pub struct AppUpdateProgress {
    pub stage: String,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub percent: u8,
    pub speed_bps: u64,
}

fn is_newer_version(remote: &str, current: &str) -> bool {
    // SemVer order: 1.0.35-rc.1 < 1.0.35 (the final is newer than its RC, and
    // an RC tester gets the final), 1.0.36-rc.1 > 1.0.35, and prereleases of one
    // version compare field by field: alpha < beta < rc, rc.2 < rc.10.
    let parse = |v: &str| -> Option<(Vec<u32>, String)> {
        let v = v.trim_start_matches('v');
        // Build metadata (`+build.7`) does not participate in the comparison — SemVer 2.0
        // §10: `1.2.58+build` and `1.2.58` are equal, and an update to the "same" version
        // with a different build suffix is not offered (V-27). They are cut off first:
        // otherwise `+build` fell into prerelease and was treated as a string, which made
        // "1.2.58" look newer than "1.2.58+build".
        let v = v.split_once('+').map(|(core, _)| core).unwrap_or(v);
        // prerelease = the suffix after the first non-digit/non-dot character
        let (core, pre) = match v.find(|c: char| !c.is_ascii_digit() && c != '.') {
            Some(i) => (&v[..i], v[i..].trim_start_matches('-')),
            None => (v, ""),
        };
        let mut out = Vec::new();
        for p in core.split('.') {
            if p.is_empty() {
                return None;
            }
            out.push(p.parse::<u32>().ok()?);
        }
        Some((out, pre.to_string()))
    };
    let (Some((mut r, r_pre)), Some((mut c, c_pre))) = (parse(remote), parse(current)) else {
        return false;
    };
    while r.len() < c.len() {
        r.push(0);
    }
    while c.len() < r.len() {
        c.push(0);
    }
    for (a, b) in r.iter().zip(c.iter()) {
        if a > b {
            return true;
        }
        if a < b {
            return false;
        }
    }
    prerelease_cmp(&r_pre, &c_pre) == std::cmp::Ordering::Greater
}

/// Comparing prereleases of one version per SemVer: without a suffix — older than any
/// suffix; dot-separated fields: numbers — numerically and below words, words — by text.
fn prerelease_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering as Cmp;
    match (a.is_empty(), b.is_empty()) {
        (true, true) => return Cmp::Equal,
        (true, false) => return Cmp::Greater,
        (false, true) => return Cmp::Less,
        (false, false) => {}
    }
    let mut left = a.split('.');
    let mut right = b.split('.');
    loop {
        let (x, y) = match (left.next(), right.next()) {
            (None, None) => return Cmp::Equal,
            (None, Some(_)) => return Cmp::Less,
            (Some(_), None) => return Cmp::Greater,
            (Some(x), Some(y)) => (x, y),
        };
        let order = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(nx), Ok(ny)) => nx.cmp(&ny),
            (Ok(_), Err(_)) => Cmp::Less,
            (Err(_), Ok(_)) => Cmp::Greater,
            (Err(_), Err(_)) => x.cmp(y),
        };
        if order != Cmp::Equal {
            return order;
        }
    }
}

/// Update check through the GitHub Releases manifest (the check core).
pub async fn check_app_update_core(current_version: &str) -> Result<AppUpdateInfo, String> {
    // A timeout with margin: the first request includes DNS, the TLS handshake and a
    // CDN redirect; four seconds were not enough for that.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;

    let resp = client
        .get("https://github.com/AndreyFSDev/cloufam-releases/releases/latest/download/latest.json")
        .send()
        .await
        .map_err(|e| format!("Не удалось проверить обновления: {}", error_chain(&e)))?;

    if !resp.status().is_success() {
        return Err(format!("Сервер обновлений вернул HTTP {}", resp.status()));
    }

    let val: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let latest_version = val
        .get("version")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Неверный формат манифеста обновлений".to_string())?
        .to_string();
    let notes = val
        .get("notes")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let available = is_newer_version(&latest_version, current_version);

    Ok(AppUpdateInfo {
        available,
        current_version: current_version.to_string(),
        latest_version,
        notes,
        exe_url: val
            .get("exe")
            .and_then(|exe| exe.get("url"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        exe_signature: val
            .get("exe")
            .and_then(|exe| exe.get("signature"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    })
}

/// Verify the minisign signature of the downloaded update with the updater key from the config.
fn verify_update_signature(app: &tauri::AppHandle, signature_b64: &str, data: &[u8]) -> Result<(), String> {
    let key_b64 = app
        .config()
        .plugins
        .0
        .get("updater")
        .and_then(|updater| updater.get("pubkey"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "в конфигурации нет публичного ключа апдейтера".to_string())?;
    verify_signature(key_b64, signature_b64, data)
}

/// Parsing the key and signature separately from the configuration: the key is public and lives
/// in `tauri.conf.json` next to the update endpoint.
fn verify_signature(public_key_b64: &str, signature_b64: &str, data: &[u8]) -> Result<(), String> {
    use base64::Engine;
    // Both the key and the signature are stored in the config and the manifest as base64 over
    // the minisign text — tauri-plugin-updater parses them the same way.
    let decode = |value: &str, what: &str| -> Result<String, String> {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(value)
            .map_err(|e| format!("{what} не разобран: {e}"))?;
        String::from_utf8(raw).map_err(|e| format!("{what} не текст: {e}"))
    };
    let key_text = decode(public_key_b64, "ключ апдейтера")?;
    let signature_text = decode(signature_b64, "подпись обновления")?;
    let public_key = minisign_verify::PublicKey::decode(&key_text)
        .map_err(|e| format!("ключ апдейтера не принят: {e}"))?;
    let signature = minisign_verify::Signature::decode(&signature_text)
        .map_err(|e| format!("подпись обновления не разобрана: {e}"))?;
    public_key
        .verify(data, &signature, false)
        .map_err(|e| format!("подпись обновления не сошлась: {e}"))
}

/// Update check through the GitHub Releases manifest (a Tauri command).
#[tauri::command]
async fn check_app_update(app: tauri::AppHandle) -> Result<AppUpdateInfo, String> {
    let current_version = app.package_info().version.to_string();
    check_app_update_core(&current_version).await
}

/// Native self-update of the app binary in the install location with a restart (the core).
pub async fn apply_app_update_core<F>(
    app: &tauri::AppHandle,
    on_progress: F,
) -> Result<(), String>
where
    F: FnMut(AppUpdateProgress) + Send + 'static,
{
    let current_version = app.package_info().version.to_string();
    let info = check_app_update_core(&current_version).await?;
    if !info.available {
        return Err(format!(
            "У вас уже установлена актуальная версия ({current_version})"
        ));
    }

    let download_url = info.exe_url.clone().unwrap_or_else(|| {
        format!(
            "https://github.com/AndreyFSDev/cloufam-releases/releases/download/v{}/Cloufam.exe",
            info.latest_version
        )
    });
    // The signature is mandatory: without it, an exe swapped along the path would reach the user
    // as a regular update, and there would be no way to verify it.
    let signature = info.exe_signature.clone().ok_or_else(|| {
        "в манифесте обновления нет подписи exe — обновление не установлено".to_string()
    })?;

    install_exe_release(app, &download_url, &signature, on_progress).await
}

/// fsync of the finished file. The handle is write-opened: FlushFileBuffers on Windows
/// requires GENERIC_WRITE, and on a file opened read-only sync_all
/// fails with "Access denied" — update and rollback would never work.
fn sync_to_disk(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::OpenOptions::new().write(true).open(path)?.sync_all()
}

/// Put the release binary in place of the running one: download, verify the size and
/// minisign signature, swap it by rename and come back via restart.
/// The shared part of update and version rollback — rollback used to write `fs::write`
/// straight over the running exe (on Windows that is always access denied) and took
/// the NSIS installer instead of the binary, and without signature verification at that.
pub async fn install_exe_release<F>(
    app: &tauri::AppHandle,
    download_url: &str,
    signature: &str,
    mut on_progress: F,
) -> Result<(), String>
where
    F: FnMut(AppUpdateProgress) + Send + 'static,
{
    // Installs happen one at a time: the window button, the top bar in SillyTavern and
    // the version rollback write to the same `Cloufam.exe.new`, and two swaps launched
    // at once would leave a gluing of two downloads instead of the app.
    static INSTALL_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    let _serialized = INSTALL_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let current_exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe_dir = current_exe
        .parent()
        .ok_or_else(|| "Не удалось определить каталог приложения".to_string())?;
    let new_exe = exe_dir.join("Cloufam.exe.new");
    let old_exe = current_exe.with_extension("exe.old");

    // The timeout is on reading, not on the whole request: otherwise a slow link cuts
    // the download in the middle (16 MB wait for at least 54 KB/s).
    let client = reqwest::Client::builder()
        .read_timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| e.to_string())?;

    on_progress(AppUpdateProgress {
        stage: "Подключение к серверу загрузки…".into(),
        downloaded_bytes: 0,
        total_bytes: 0,
        percent: 0,
        speed_bps: 0,
    });

    let resp = client
        .get(download_url)
        .send()
        .await
        .map_err(|e| format!("Ошибка загрузки обновления: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("Ошибка загрузки обновления: HTTP {}", resp.status()));
    }

    let total = resp.content_length().unwrap_or(0);
    if let Some(free) = crate::gpu::free_space_bytes(&exe_dir) {
        // Margin for unpacking/swapping — the update is placed next to the current exe.
        let needed = total.saturating_add(64 * 1024 * 1024);
        if total > 0 && free < needed {
            return Err(format!(
                "Недостаточно места для обновления: нужно {} МБ, свободно {} МБ",
                needed / (1024 * 1024),
                free / (1024 * 1024)
            ));
        }
    }
    // An unfinished download must not leave .exe.new on disk: otherwise
    // the next update start stumbles over occupied space.
    let downloaded: Result<(), String> = async {
        use futures_util::StreamExt;
        use tokio::io::AsyncWriteExt;

        let mut file = tokio::fs::File::create(&new_exe)
            .await
            .map_err(|e| format!("Не удалось создать временный файл обновления: {e}"))?;

        let mut stream = resp.bytes_stream();
        let mut got: u64 = 0;
        let mut last_speed_time = std::time::Instant::now();
        let mut bytes_since_speed = 0u64;
        let mut current_speed = 0u64;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| format!("Ошибка сетевого потока: {e}"))?;
            file.write_all(&chunk)
                .await
                .map_err(|e| format!("Ошибка записи (возможно, кончилось место): {e}"))?;
            got += chunk.len() as u64;
            bytes_since_speed += chunk.len() as u64;

            let elapsed = last_speed_time.elapsed();
            if elapsed >= std::time::Duration::from_millis(400) {
                current_speed = (bytes_since_speed as f64 / elapsed.as_secs_f64()) as u64;
                bytes_since_speed = 0;
                last_speed_time = std::time::Instant::now();
            }

            let pct = if total > 0 {
                ((got as f64 / total as f64) * 100.0).round().min(99.0) as u8
            } else {
                0
            };

            on_progress(AppUpdateProgress {
                stage: "Загрузка обновления…".into(),
                downloaded_bytes: got,
                total_bytes: total,
                percent: pct,
                speed_bps: current_speed,
            });
        }
        file.flush().await.map_err(|e| e.to_string())?;
        Ok(())
    }
    .await;

    if let Err(e) = downloaded {
        let _ = std::fs::remove_file(&new_exe);
        return Err(e);
    }

    // The swap only after the checks: the body size matched the declared one, and the signature
    // matched the updater key. Otherwise a corrupt or foreign exe would take the place
    // of the working one.
    if total > 0 {
        let written = std::fs::metadata(&new_exe).map(|m| m.len()).unwrap_or(0);
        if written != total {
            let _ = std::fs::remove_file(&new_exe);
            return Err(format!(
                "Обновление скачано не полностью ({written} из {total} байт) — повторите позже"
            ));
        }
    }
    {
        let bytes = std::fs::read(&new_exe).map_err(|e| format!("Обновление не прочитано: {e}"))?;
        if let Err(e) = verify_update_signature(app, signature, &bytes) {
            let _ = std::fs::remove_file(&new_exe);
            return Err(e);
        }
    }
    // rename without fsync: after a power failure the new exe could end up empty/truncated
    // already in the place of the working one. We flush the bytes to disk before the swap.
    sync_to_disk(&new_exe).map_err(|e| format!("Обновление не сброшено на диск: {e}"))?;

    on_progress(AppUpdateProgress {
        stage: "Установка обновления…".into(),
        downloaded_bytes: total,
        total_bytes: total,
        percent: 100,
        speed_bps: 0,
    });

    if let Err(e) = std::fs::remove_file(&old_exe) {
        if old_exe.exists() {
            return Err(format!(
                "Файл {} заблокирован другим процессом ({e}) — закройте его и повторите",
                old_exe.display()
            ));
        }
    }
    std::fs::rename(&current_exe, &old_exe)
        .map_err(|e| format!("Не удалось переименовать текущий exe: {e}"))?;
    if let Err(e) = std::fs::rename(&new_exe, &current_exe) {
        // If the rollback also failed, no app remains on disk —
        // the user must learn about it from the error text.
        return Err(match std::fs::rename(&old_exe, &current_exe) {
            Ok(_) => format!("Не удалось установить новый exe: {e}"),
            Err(rb) => format!(
                "Не удалось установить новый exe: {e}; откат тоже не удался ({rb}). \
                 Восстановите {} вручную из {}",
                current_exe.display(),
                old_exe.display()
            ),
        });
    }

    for mirror in ["Cloufam.exe", "local-chatbot.exe"] {
        let path = exe_dir.join(mirror);
        if path.exists() && !is_same_exe_name(&current_exe, mirror) {
            if let Err(e) = std::fs::copy(&current_exe, &path) {
                boot_log(&format!("не удалось обновить {}: {e}", path.display()));
            }
        }
    }

    let state = app.state::<Arc<AppState>>();
    // Runs on the async worker of `apply_app_update_core`, right before
    // `process::exit(0)`. A poisoned mutex must not leave children alive:
    // `into_inner()` policy (V-20) — otherwise `ollama serve`/node survive the
    // self-replacement as orphans.
    let rt_guard = state.runtime.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(rt) = rt_guard.as_ref() {
        rt.stop();
    }
    // st_child is killed in RunEvent::Exit, but process::exit() does not trigger
    // the Tauri event handler — so we kill it explicitly here.
    runtime::begin_shutdown();
    sillytavern::begin_shutdown();
    let mut child = state.st_child.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(c) = child.as_mut() {
        let _ = c.kill();
        // The wait is bounded: with a failed TerminateProcess, an unbounded wait
        // hung the update after the exe had already been swapped.
        for _ in 0..20 {
            if c.try_wait().ok().flatten().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    // Fine-tuning has its own python processes: without cancel() they are orphaned on
    // self-unload (this code would no longer get to run in RunEvent::Exit).
    let _ = trainer::cancel();
    let _ = image_trainer::cancel();
    // Runtime steps (uv/python/probe) live separately from the trainers: without this
    // a hung probe survived the exe self-replacement.
    let _ = crate::pyruntime::cancel();
    sillytavern::kill_orphan_node_processes();
    // A late node spawn from a still-alive boot task after the first sweep.
    std::thread::sleep(std::time::Duration::from_millis(300));
    sillytavern::kill_orphan_node_processes();
    runtime::begin_shutdown();
    comfyui::kill_comfy_child();

    // We ask the new process to wait before starting: single-instance does not distinguish
    // versions, and a child launched right after spawn would see our still-alive
    // mutex, silently exit — the app would close and not come back.
    // The delay is done by the new process itself (the `--after-update` flag), not by the
    // `cmd` wrapper: nested quotes in its command broke, and there was no restart.
    #[cfg(windows)]
    {
        let mut cmd = std::process::Command::new(&current_exe);
        cmd.arg("--after-update").current_dir(exe_dir);
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
        if let Err(e) = cmd.spawn() {
            log_restart_failure(&e);
        }
    }
    #[cfg(not(windows))]
    {
        if let Err(e) = std::process::Command::new(&current_exe)
            .arg("--after-update")
            .current_dir(exe_dir)
            .spawn()
        {
            log_restart_failure(&e);
        }
    }

    std::process::exit(0);
}

/// In a release build there is no console (`windows_subsystem`), and eprintln! went
/// nowhere: the app closed silently, and the reason was not left anywhere.
fn log_restart_failure(error: &std::io::Error) {
    boot_log(&format!(
        "Обновление установлено, но перезапуск не удался: {error} — запустите Cloufam вручную"
    ));
}

/// Whether this is the same file as the running exe: the directory is shared, and Windows
/// does not distinguish case — the installer drops `cloufam.exe`, and a path comparison
/// as is would copy the running exe onto itself.
fn is_same_exe_name(current_exe: &std::path::Path, name: &str) -> bool {
    current_exe
        .file_name()
        .map(|n| n.to_string_lossy().eq_ignore_ascii_case(name))
        .unwrap_or(false)
}

/// Native self-update of the app binary in the install location with a restart (a Tauri command).
#[tauri::command]
async fn apply_app_update(
    app: tauri::AppHandle,
    on_progress: tauri::ipc::Channel<AppUpdateProgress>,
) -> Result<(), String> {
    apply_app_update_core(&app, move |p| {
        let _ = on_progress.send(p);
    })
    .await
}

/// Garbage from an interrupted install — it will be downloaded again on the next attempt,
/// so it is always removed, not only after a health check.
fn cleanup_stale_update_artifacts() {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let _ = std::fs::remove_file(dir.join("Cloufam.exe.new"));
        }
    }
}

/// Backups of the previous version (`.exe.old`) are the only way back if
/// the new build does not come up. Unlike `.exe.new`, they cannot be deleted
/// unconditionally at startup: `install_exe_release` creates `.exe.old` and immediately
/// restarts, while the old `cleanup_old_updates()` erased the backup after as little as
/// 2 seconds (the `--after-update` pause) — before `bring_up` (Ollama +
/// SillyTavern) even got to try coming up on the new build.
fn old_exe_backup_paths() -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            paths.push(exe.with_extension("exe.old"));
            paths.push(dir.join("Cloufam.exe.old"));
            paths.push(dir.join("local-chatbot.exe.old"));
        }
    }
    paths
}

/// The boot succeeded — the previous version is no longer needed.
fn confirm_update_healthy() {
    for path in old_exe_backup_paths() {
        let _ = std::fs::remove_file(path);
    }
}

/// Whether a backup of the previous version lies next to it — that is, we are running on an update
/// that has never come up successfully.
fn has_update_backup() -> bool {
    old_exe_backup_paths().iter().any(|p| p.exists())
}

/// Opening or focusing the settings window.
/// Window creation goes to the main thread: from the IPC thread or the WebView2 gateway
/// it comes up "dead" (the window exists, the page does not load), while a synchronous call
/// from the main thread hung the app outright.
pub fn show_settings_window(app: &tauri::AppHandle) {
    show_settings_window_tab(app, None);
}

/// Open settings directly on the needed tab: from SillyTavern the "Traits and
/// character" item leads straight to the sliders, not to the default tab. The tab
/// is passed both as an event (the window is already open) and in the address (the window is created anew).
pub fn show_settings_window_tab(app: &tauri::AppHandle, tab: Option<String>) {
    let handle = app.clone();
    if let Err(e) = app.run_on_main_thread(move || show_settings_window_main(&handle, tab)) {
        boot_log(&format!("Окно настроек: не удалось запланировать открытие ({e})"));
    }
}

fn show_settings_window_main(app: &tauri::AppHandle, tab: Option<String>) {
    use tauri::Emitter;
    let state = app.state::<Arc<AppState>>();
    let st_opened = state.st_opened.load(Ordering::Relaxed);

    // Before SillyTavern is opened, the main window is the settings and launcher screen.
    // If SillyTavern has not been opened yet, we show and focus the main window.
    if !st_opened {
        if let Some(main_window) = app.get_webview_window("main") {
            if let Some(tab) = tab.as_deref() {
                let _ = main_window.emit("cloufam-tab", tab);
            }
            bring_to_front(&main_window);
            return;
        }
    }

    // After SillyTavern is opened, we open or focus the separate native settings window.
    if let Some(window) = app.get_webview_window("settings") {
        if let Some(tab) = tab.as_deref() {
            let _ = window.emit("cloufam-tab", tab);
        }
        bring_to_front(&window);
    } else {
        let hash = match tab.as_deref() {
            Some(tab) if !tab.is_empty() => format!("index.html#settings/{tab}"),
            _ => "index.html#settings".to_string(),
        };
        let builder = tauri::WebviewWindowBuilder::new(
            app,
            "settings",
            tauri::WebviewUrl::App(hash.into()),
        )
        .title(l10n::t("Cloufam — Настройки моделей и среды", "Cloufam — Model and app settings"))
        .inner_size(980.0, 720.0)
        .min_inner_size(840.0, 560.0)
        .resizable(true)
        .decorations(false)
        .shadow(true);

        // Browser arguments must match the main window: WebView2 does not allow
        // creating environments with different options in one process, and the second window
        // came up "dead" — a blank page that answers nothing.
        let builder = match app
            .config()
            .app
            .windows
            .first()
            .and_then(|w| w.additional_browser_args.clone())
        {
            Some(args) => builder.additional_browser_args(&args),
            None => builder,
        };

        match builder.build() {
            Ok(window) => {
                make_native(&window);
                bring_to_front(&window);
                boot_log("Окно настроек: открыто отдельным окном");
            }
            // A silent failure left the user without the settings window.
            Err(e) => boot_log(&format!("Окно настроек: не удалось создать ({e})")),
        }
    }
}

/// Open or focus the guide window (the "📖 Guide" item in the tray).
/// Window creation goes to the main thread: from the tray handler or the IPC
/// WebView2 thread it comes up "dead" (the same trap as with the settings window above).
pub fn show_guide_window(app: &tauri::AppHandle) {
    let handle = app.clone();
    if let Err(e) = app.run_on_main_thread(move || show_guide_window_main(&handle)) {
        boot_log(&format!("Руководство: не удалось запланировать открытие ({e})"));
    }
}

fn show_guide_window_main(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("guide") {
        bring_to_front(&window);
        return;
    }

    let builder = tauri::WebviewWindowBuilder::new(
        app,
        "guide",
        tauri::WebviewUrl::App("index.html#guide".into()),
    )
    .title(l10n::t("Cloufam — Руководство", "Cloufam — Guide"))
    .inner_size(1000.0, 720.0)
    .min_inner_size(840.0, 560.0)
    .resizable(true)
    .decorations(false)
    .shadow(true);

    // Browser arguments must match the main window: WebView2 does not allow
    // creating environments with different options in one process.
    let builder = match app
        .config()
        .app
        .windows
        .first()
        .and_then(|w| w.additional_browser_args.clone())
    {
        Some(args) => builder.additional_browser_args(&args),
        None => builder,
    };

    match builder.build() {
        Ok(window) => {
            make_native(&window);
            bring_to_front(&window);
            boot_log("Руководство: окно открыто");
        }
        Err(e) => boot_log(&format!("Руководство: не удалось создать окно ({e})")),
    }
}

/// Update the installed SillyTavern to `version` (the "Update SillyTavern" item
/// in the menu). Stops the ST server, rewrites the code, re-seeds extensions and patches,
/// brings ST back up on the same port (otherwise the open page would stay on
/// a dead address). Patches that failed to apply go into the response and boot.log — the new ST version
/// may diverge from the Cloufam insertion points pinned in code.
pub async fn update_sillytavern_to(
    app: tauri::AppHandle,
    version: String,
) -> Result<serde_json::Value, String> {
    let state = app.state::<Arc<AppState>>();
    let from = sillytavern::installed_st_version();
    if from == version {
        return Ok(serde_json::json!({ "ok": true, "from": from, "to": version, "changed": false }));
    }
    // The update writes over the files the server runs from — stop ST.
    // Runs on the async IPC worker; a poisoned mutex must not leave ST running
    // over files that are about to be rewritten (`into_inner()` policy, V-20).
    // Scoped block: the std mutex is not reentrant, and the restart below takes
    // `st_child` again.
    {
        let mut guard = state.st_child.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut child) = guard.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    sillytavern::kill_orphan_node_processes();

    let cancel = Arc::new(AtomicBool::new(false));
    let progress: sillytavern::ProgressFn = Arc::new(|_id, status, _got, _total| {
        boot_log(&format!("SillyTavern: {status}"));
    });
    let update_result = sillytavern::update_st_to(&from, &version, cancel, progress).await;
    let warnings = if update_result.is_ok() {
        // Re-seeding makes sense only over an intact unpacking; on failure it
        // would return a list of "did not apply" against the old tree.
        let warnings = sillytavern::reseed_after_update(gateway::gateway_port(gateway::ollama_port()));
        for warning in &warnings {
            boot_log(&format!("SillyTavern: после обновления не легло: {warning}"));
        }
        warnings
    } else {
        Vec::new()
    };

    // The same port as the stopped ST: the page in the window points to it.
    let st_port = state.st_port.load(Ordering::SeqCst);
    let mut restarted = false;
    let mut start_err = String::new();
    if sillytavern::write_config(st_port).is_ok() {
        match sillytavern::start(st_port).await {
            Ok(child) => {
                *state.st_child.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
                sillytavern::clear_starting_pid();
                restarted = true;
                boot_log(&format!(
                    "SillyTavern: перезапущена на {st_port} после обновления {from} -> {version}"
                ));
            }
            Err(e) => start_err = e,
        }
    }
    if !restarted {
        boot_log(&format!("SillyTavern: после обновления не запустилась: {start_err}"));
    }
    match update_result {
        Ok(()) => Ok(serde_json::json!({
            "ok": true,
            "from": from,
            "to": version,
            "changed": true,
            "restarted": restarted,
            "warnings": warnings,
        })),
        // An access/network error used to leave ST off with node_modules removed:
        // the page in the window pointed at a dead port. Now it is
        // at least restarted (if the tree survived), and this is visible in the text.
        Err(e) => Err(if restarted {
            format!("{e} SillyTavern перезапущена на {st_port}.")
        } else {
            format!("{e} SillyTavern не запустилась: {start_err}")
        }),
    }
}

/// Show the window above the others. Windows forbids a background process from
/// taking focus, so a window opened from the tray quietly stayed behind
/// the SillyTavern window: the user saw that "nothing opened". A short
/// always-on-top raises the window, after which it can be lowered.
fn bring_to_front(window: &tauri::WebviewWindow) {
    let _ = window.show();
    let _ = window.unminimize();
    let _ = window.set_always_on_top(true);
    let _ = window.set_focus();
    let handle = window.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(400));
        let _ = handle.set_always_on_top(false);
    });
}

const TRAY_ID: &str = "main-tray";

/// Tray exit: the main window's close button hides it to the tray, and this flag
/// distinguishes a real exit from closing the window.
static QUITTING: AtomicBool = AtomicBool::new(false);
/// The tray was created: only then does the close button hide the window. Without a tray there is no "Exit" item,
/// and a hidden window would leave Ollama/ST/ComfyUI running invisibly.
static TRAY_OK: AtomicBool = AtomicBool::new(false);

/// The tray menu in the current UI language; rebuilt on `set_locale`.
fn tray_menu(app: &tauri::AppHandle) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};

    let open_item = MenuItem::with_id(app, "open", l10n::t("Открыть Cloufam", "Open Cloufam"), true, None::<&str>)?;
    let settings_item =
        MenuItem::with_id(app, "settings", l10n::t("Настройки моделей", "Model settings"), true, None::<&str>)?;
    let folder_item =
        MenuItem::with_id(app, "models_folder", l10n::t("Папка с моделями", "Models folder"), true, None::<&str>)?;
    let guide_item = MenuItem::with_id(app, "guide", l10n::t("Руководство", "Guide"), true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit_item = MenuItem::with_id(app, "quit", l10n::t("Выход", "Quit"), true, None::<&str>)?;

    Menu::with_items(
        app,
        &[&open_item, &settings_item, &folder_item, &guide_item, &sep, &quit_item],
    )
}

/// Bring up the main window (launcher or SillyTavern) — from the tray and a second launch.
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        bring_to_front(&window);
    }
}

/// Initializing the system tray with a context menu.
fn setup_tray(app: &tauri::AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    let mut tray_builder = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&tray_menu(app)?)
        .show_menu_on_left_click(false)
        .tooltip("Cloufam");

    if let Some(icon) = app.default_window_icon() {
        tray_builder = tray_builder.icon(icon.clone());
    }

    tray_builder
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_main_window(app),
            "settings" => {
                show_settings_window(app);
            }
            "models_folder" => {
                let _ = std::process::Command::new("explorer")
                    .arg(paths::models_dir())
                    .spawn();
            }
            "guide" => {
                show_guide_window(app);
            }
            "quit" => {
                QUITTING.store(true, Ordering::SeqCst);
                if let Some(window) = app.get_webview_window("main") {
                    window_state::save(&window);
                }
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| match event {
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }
            | TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } => show_main_window(tray.app_handle()),
            _ => {}
        })
        .build(app)?;

    Ok(())
}

/// Init script for all pages: the Windows language for "auto" mode and the window buttons
/// on the SillyTavern page (it has none of its own, and the window has no system frame).
fn shell_init_plugin() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    let script = format!(
        "window.__LK_SYSTEM_LOCALE__ = {:?};
{}",
        l10n::system_locale_tag(),
        include_str!("../shell/st-window-controls.js")
    );
    tauri::plugin::Builder::new("lk-shell").js_init_script(script).build()
}

pub fn run() {
    // Until the first `set_locale` from the frontend (the tray is built before the window) —
    // the Windows language, not hard-coded Russian.
    l10n::set_locale(l10n::system_locale_tag());

    // The update launches us right after the old process exits, and
    // single-instance checks the mutex before the window is created: without this pause
    // the new copy would take the old one for alive and exit.
    if std::env::args().any(|arg| arg == "--after-update") {
        std::thread::sleep(std::time::Duration::from_millis(2000));
    }

    #[cfg(windows)]
    {
        let webview_dir = paths::data_dir().join("webview");
        let _ = std::fs::create_dir_all(&webview_dir);
        std::env::set_var("WEBVIEW2_USER_DATA_FOLDER", webview_dir);
    }

    // The model store from settings — before ensure_dirs: otherwise the Ollama and
    // cache directories were created under the default root, and the wizard measured free
    // space on the wrong disk.
    let settings = Settings::load();
    if !settings.models_root.trim().is_empty() {
        paths::set_models_root(std::path::PathBuf::from(settings.models_root.trim()));
    }
    // An unavailable store (a disconnected disk) must not make startup fail silently:
    // the program directories are created before the store, and the boot will show the reason.
    if let Err(e) = paths::ensure_dirs() {
        boot_log(&format!("Каталоги данных: {e}"));
    }

    let state = Arc::new(AppState {
        client: Mutex::new(None),
        runtime: Mutex::new(None),
        runtime_lock: tokio::sync::Mutex::new(()),
        st_child: Mutex::new(None),
        settings: Mutex::new(settings),
        cancel_download: Arc::new(AtomicBool::new(false)),
        downloads: downloads::Queue::new(),
        vision_downloading: AtomicBool::new(false),
        st_port: std::sync::atomic::AtomicU16::new(8000),
        boot_status: Mutex::new(l10n::t("Запускаем…", "Starting…")),
        boot_progress: Mutex::new(None),
        boot_error: Mutex::new(None),
        boot_ready: AtomicBool::new(false),
        boot_in_progress: AtomicBool::new(false),
        st_opened: AtomicBool::new(false),
        context_ceiling: std::sync::atomic::AtomicU32::new(0),
        model_fits: AtomicBool::new(true),
        active_character: Mutex::new(String::new()),
        active_persona: Mutex::new(String::new()),
    });

    tauri::Builder::default()
        .plugin(shell_init_plugin())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app);
        }))
        .manage(state.clone())
        .manage(engine::EngineState::default())
        // We save the window geometry on CloseRequested: by RunEvent::Exit the window
        // is already destroyed, and get_webview_window returns None.
        .on_window_event(|window, event| {
            if window.label() != "main" {
                return;
            }
            // The close button hides the window to the tray: Ollama and SillyTavern keep
            // running, and a real exit is the "Exit" item in the tray menu.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if !QUITTING.load(Ordering::SeqCst) && !TRAY_OK.load(Ordering::SeqCst) {
                    // The same exit as the tray item: the children are killed in RunEvent::Exit
                    if let Some(main) = window.app_handle().get_webview_window("main") {
                        window_state::save(&main);
                    }
                    QUITTING.store(true, Ordering::SeqCst);
                    window.app_handle().exit(0);
                    return;
                }
                if !QUITTING.load(Ordering::SeqCst) {
                    api.prevent_close();
                    if let Some(main) = window.app_handle().get_webview_window("main") {
                        window_state::save(&main);
                    }
                    let _ = window.hide();
                    return;
                }
            }
            if matches!(
                event,
                tauri::WindowEvent::CloseRequested { .. } | tauri::WindowEvent::Destroyed
            ) {
                crate::boot_log("Закрытие окна: сохраняю геометрию");
                if let Some(main) = window.app_handle().get_webview_window("main") {
                    window_state::save(&main);
                }
                crate::boot_log("Закрытие окна: геометрия сохранена");
            }
        })
        .setup(move |app| {
            // Destructive cleanup — only after single-instance: the plugin terminates
            // the second instance during initialization, before this hook. Otherwise a double
            // click on the shortcut erased the partially downloaded Cloufam.exe.new and data\tmp
            // of a running pip/ComfyUI/fine-tuning.
            cleanup_stale_update_artifacts();
            paths::cleanup_tmp();

            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_title(&format!("Cloufam v{}", env!("CARGO_PKG_VERSION")));
                make_native(&window);
                window_state::restore(&window);
            }

            let handle = app.handle().clone();
            match setup_tray(&handle) {
                Ok(()) => TRAY_OK.store(true, Ordering::SeqCst),
                Err(e) => boot_log(&format!("Ошибка создания системного трея: {e}; крестик закрывает приложение")),
            }
            ensure_desktop_shortcut();

            let setup_done = state.settings.lock().unwrap_or_else(|e| e.into_inner()).setup_done;
            if setup_done {
                let state_arc = state.clone();
                state_arc.boot_in_progress.store(true, Ordering::Relaxed);
                tauri::async_runtime::spawn(async move {
                    let _guard = BootFlagGuard(state_arc.clone());
                    let _ = bring_up(state_arc.clone(), handle).await;
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            boot_state,
            set_locale,
            model_stores,
            inspect_model_store,
            set_models_root,
            retry_boot,
            setup_state,
            download_runtime,
            cancel_download,
            cancel_comfy_install,
            start_runtime,
            download_model,
            downloads_state,
            downloads_pause,
            downloads_resume,
            downloads_cancel,
            downloads_pause_task,
            downloads_resume_task,
            downloads_cancel_task,
            installed_models,
            background_vision_download,
            download_sillytavern,
            finalize_setup,
            get_settings,
            save_settings,
            open_st_folder,
            reset_setup,
            delete_model,
            open_models_folder,
            open_about_link,
            engine::engine_list_characters,
            engine::engine_import_character,
            engine::engine_delete_character,
            engine::engine_avatar,
            engine::engine_background,
            engine::engine_set_background,
            engine::engine_get_lorebook,
            engine::engine_save_lorebook,
            engine::engine_import_lorebook,
            engine::engine_list_chats,
            engine::engine_create_chat,
            engine::engine_load_chat,
            engine::engine_delete_chat,
            engine::engine_cancel,
            engine::engine_send,
            open_app_folder,
            isolation_report,
            open_settings_window,
            guide_data,
            models_upgrade_state,
            models_upgrade_apply,
            models_upgrade_dismiss,
            mark_st_opened,
            is_st_opened,
            get_image_models,
            gpu_usage,
            set_image_model,
            recommended_settings,
            apply_recommended_settings,
            recommended_downloads,
            download_recommended,
            system_info,
            traits_state,
            traits_save,
            traits_reset_state,
            bonds_save,
            get_comfy_status,
            start_comfy_server,
            install_comfyui,
            download_image_model,
            delete_image_model,
            check_app_update,
            apply_app_update,
            trainer_status,
            trainer_install,
            trainer_cancel,
            training_presets,
            training_estimate,
            training_import,
            training_datasets,
            training_start,
            training_state,
            training_cancel,
            training_clear,
            training_export,
            training_open_import_folder,
            image_training_presets,
            image_training_estimate,
            image_training_datasets,
            image_training_import,
            image_training_caption,
            image_training_start,
            image_training_state,
            image_training_cancel,
            image_training_clear,
            image_training_open_folder,
            create_desktop_shortcut,
            // The settings-window tabs list (V-45): one source — gateway.rs.
            gateway::settings_tabs,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                // Window geometry — into data/window.json, before the window disappears.
                crate::boot_log("Выход: сохраняю геометрию окна");
                if let Some(window) = app.get_webview_window("main") {
                    window_state::save(&window);
                }
                // We kill child processes: otherwise node/ollama would outlive the window.
                // Runs on the Tauri main thread inside RunEvent::Exit. A poisoned
                // mutex must not leave children alive (`into_inner()` policy,
                // V-20): `if let Ok(lock)` skipped the kill and orphaned Ollama.
                crate::boot_log("Выход: гашу дочерние процессы");
                // A serve being started right now (wait_ready up to 60 s) is not yet
                // in state.runtime — we kill it by pid and forbid spawning.
                runtime::begin_shutdown();
                // The same for ST: a node started seconds before exit is not yet in
                // state.st_child and outlived the window (reproduced on 8001).
                sillytavern::begin_shutdown();
                let state = app.state::<Arc<AppState>>();
                let mut child = state.st_child.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(child) = child.as_mut() {
                    let _ = child.kill();
                    // A bounded wait: a hung node must not hold the exit.
                    for _ in 0..20 {
                        match child.try_wait() {
                            Ok(Some(_)) => break,
                            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
                            Err(_) => break,
                        }
                    }
                }
                drop(child);
                let rt_guard = state.runtime.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(rt) = rt_guard.as_ref() {
                    rt.stop();
                }
                drop(rt_guard);
                sillytavern::kill_orphan_node_processes();
                // The "late spawn" race: the boot task manages to spawn node
                // between the first sweep and process exit. A short pause and
                // a second sweep catch it (reproduced on 8001 after an update).
                std::thread::sleep(std::time::Duration::from_millis(300));
                sillytavern::kill_orphan_node_processes();
                runtime::begin_shutdown();
                comfyui::kill_comfy_child();
                // Runtime install/probe steps (pyruntime) must also
                // die: a python probe without a timeout would outlive the window (an orphan
                // in the install folder, smoke of 18.10.2026).
                pyruntime::cancel();
                let _ = trainer::cancel();
                let _ = image_trainer::cancel();
                crate::boot_log("Выход: дочерние процессы погашены");
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_exe_backup_paths_point_at_exe_old_variants() {
        let paths = old_exe_backup_paths();
        assert!(!paths.is_empty());
        assert!(
            paths.iter().any(|p| p.to_string_lossy().ends_with("exe.old")),
            "{paths:?}"
        );
    }

    /// A panic in boot must not jam `boot_in_progress` forever:
    /// otherwise `retry_boot` answers "startup is already running" forever.
    #[test]
    fn test_boot_flag_guard_clears_flag_even_after_panic() {
        let state = Arc::new(AppState {
            client: Mutex::new(None),
            runtime: Mutex::new(None),
            runtime_lock: tokio::sync::Mutex::new(()),
            st_child: Mutex::new(None),
            settings: Mutex::new(Settings::default()),
            cancel_download: Arc::new(AtomicBool::new(false)),
            downloads: downloads::Queue::new(),
            vision_downloading: AtomicBool::new(false),
            st_port: std::sync::atomic::AtomicU16::new(8000),
            boot_status: Mutex::new(String::new()),
            boot_progress: Mutex::new(None),
            boot_error: Mutex::new(None),
            boot_ready: AtomicBool::new(false),
            boot_in_progress: AtomicBool::new(true),
            st_opened: AtomicBool::new(false),
            context_ceiling: std::sync::atomic::AtomicU32::new(0),
            model_fits: AtomicBool::new(true),
            active_character: Mutex::new(String::new()),
            active_persona: Mutex::new(String::new()),
        });
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = BootFlagGuard(state.clone());
            panic!("паника в bring_up");
        }));
        assert!(!state.boot_in_progress.load(Ordering::Relaxed));
    }

    /// A successful boot removes the previous version's backup, an unsuccessful one leaves it.
    /// That is the whole point of the mechanism: before this fix `.exe.old` was erased
    /// unconditionally at startup, that is, the way back disappeared before it became
    /// known whether the new build comes up at all.
    ///
    /// Both cases in one test on purpose: previously these were two tests, and
    /// "no-op without a backup" deleted the very paths the second one had just
    /// created — on a parallel cargo run `has_update_backup()` either falsely
    /// fired or did not.
    #[test]
    fn confirm_update_healthy_removes_existing_backup() {
        for path in old_exe_backup_paths() {
            let _ = std::fs::remove_file(&path);
        }
        // There is no backup — it must not panic.
        assert!(!has_update_backup());
        confirm_update_healthy();

        let backup = std::env::current_exe().unwrap().with_extension("exe.old");
        std::fs::write(&backup, b"prev").unwrap();
        assert!(has_update_backup(), "бэкап должен опознаваться");
        confirm_update_healthy();
        assert!(!backup.exists(), "после успешного подъёма бэкап снимается");
        assert!(!has_update_backup());
    }

    /// Auto-rollback was removed deliberately: boot also fails for external reasons
    /// (the wizard is not completed, the Ollama port is held by an orphan, an antivirus held
    /// node), and a silent return to the previous version because of a busy port is worse
    /// than the error shown. The string is assembled from pieces, otherwise `include_str!`
    /// would find itself.
    #[test]
    fn boot_failure_path_has_no_auto_rollback() {
        let source = include_str!("lib.rs");
        let needle = concat!("fn rollback_", "failed_update");
        assert!(!source.contains(needle), "подъём не должен подменять exe сам");
    }

    #[test]
    fn test_updater_pubkey_from_config_is_accepted() {
        // The key the releases are signed with must parse as minisign: otherwise
        // the exe signature check rejects any update.
        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let key = conf["plugins"]["updater"]["pubkey"].as_str().unwrap();
        let error = verify_signature(key, "не подпись вовсе", b"data").unwrap_err();
        assert!(error.contains("подпись"), "ошибка не о подписи: {error}");
    }

    /// The signature in the manifest is base64 over the minisign text (like the key in
    /// the config). Parsing without stripping base64 crashed the live-copy update
    /// install: "Invalid encoding in minisign data". The fixture is signed with the same
    /// key as the releases — payload `cloufam-update-test`.
    #[test]
    fn test_update_signature_is_verified_with_base64_layer() {
        const SIGNATURE_B64: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVTUVpyRmMyWk1zUTJKQkFyakQyL0NYWFdCQXpMU20xTEh5WmtmcVB6MkRtaHVVWGxoR1MvZFVCRzRJdktKOElQeGFVMVpGS3drQnh4VTRzbnNVdTVxdy96QjZIM051ZHdVPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzg5MjM5OTYyCWZpbGU6cGF5bG9hZC5iaW4KTWlwWXF2dHFMS3hwaHlFcWd4bmFrZ1RlVTN2MTRNNW5vc1RzTnoxR2NoSHBDR0dZam0xcjBLbHkwWnhQMXBVSldFUHNZbjFmS0FUME15R3V0RlJVQWc9PQo=";
        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let key = conf["plugins"]["updater"]["pubkey"].as_str().unwrap();

        verify_signature(key, SIGNATURE_B64, b"cloufam-update-test")
            .expect("подпись релизного ключа должна приниматься");
        let error = verify_signature(key, SIGNATURE_B64, b"tampered exe").unwrap_err();
        assert!(error.contains("не сошлась"), "получено: {error}");
    }

    #[test]
    fn test_ollama_port_from_url() {
        assert_eq!(ollama_port_from_url("http://127.0.0.1:11434"), Some(11434));
        assert_eq!(ollama_port_from_url("http://127.0.0.1:11434/"), Some(11434));
        assert_eq!(ollama_port_from_url("http://127.0.0.1:11434/v1"), Some(11434));
        assert_eq!(ollama_port_from_url("http://127.0.0.1:11434/api/chat"), Some(11434));
        assert_eq!(ollama_port_from_url("127.0.0.1:8080"), Some(8080));
        assert_eq!(ollama_port_from_url("http://localhost:9000///"), Some(9000));
        assert_eq!(ollama_port_from_url("invalid-url"), None);
        assert_eq!(ollama_port_from_url("http://127.0.0.1"), None);
        assert_eq!(ollama_port_from_url("hostname:abc"), None);
        assert_eq!(ollama_port_from_url(""), None);
    }

    #[test]
    fn test_is_newer_version() {
        assert!(is_newer_version("1.0.35", "1.0.34"));
        assert!(is_newer_version("v1.0.35", "1.0.34"));
        assert!(is_newer_version("1.1.0", "1.0.34"));
        assert!(is_newer_version("2.0.0", "1.99.99"));
        assert!(!is_newer_version("1.0.34", "1.0.34"));
        assert!(!is_newer_version("1.0.33", "1.0.34"));
        assert!(!is_newer_version("0.9.9", "1.0.0"));
        // Segments with non-digits (1.0.35-rc.1) must not turn into 1.0.1.
        assert!(is_newer_version("1.0.35-rc.1", "1.0.34"));
        assert!(!is_newer_version("1.0.34", "1.0.35-rc.1"));
        assert!(!is_newer_version("1.0.35-rc.1", "1.0.35-rc.1"));
        assert!(!is_newer_version("1.0.35-rc.1", "1.0.35"));
        // The final is newer than its RC: an RC tester gets the final build.
        assert!(is_newer_version("1.0.35", "1.0.35-rc.1"));
        // Prerelease of one version — by SemVer fields.
        assert!(is_newer_version("1.0.35-rc.2", "1.0.35-rc.1"));
        assert!(is_newer_version("1.0.35-rc.10", "1.0.35-rc.2"));
        assert!(is_newer_version("1.0.35-beta.1", "1.0.35-alpha.3"));
        assert!(is_newer_version("1.0.35-rc.1", "1.0.35-beta.9"));
        assert!(!is_newer_version("1.0.35-alpha", "1.0.35-beta"));
        assert!(!is_newer_version("1.0.35-rc", "1.0.35-rc.1"));
        // Different length: 1.0.35.1 > 1.0.35.
        assert!(is_newer_version("1.0.35.1", "1.0.35"));
        assert!(!is_newer_version("1.0.35", "1.0.35.1"));
    }

    /// Build metadata (`+build.7`) per SemVer 2.0 §10 does not participate in the comparison:
    /// versions differing only in the build suffix are equal, and an update to
    /// the "same" version with a different suffix is not offered (V-27).
    #[test]
    fn test_is_newer_version_ignores_build_metadata() {
        assert!(!is_newer_version("1.2.58+build.7", "1.2.58"));
        assert!(!is_newer_version("1.2.58", "1.2.58+build.7"));
        assert!(!is_newer_version("1.2.58+build.2", "1.2.58+build.7"));
        assert!(is_newer_version("1.2.59+build.7", "1.2.58"));
        assert!(!is_newer_version("1.2.58+build.7", "1.2.59"));
        // Build metadata does not override the prerelease order.
        assert!(!is_newer_version("1.2.59-rc.1+build.7", "1.2.59"));
        assert!(is_newer_version("1.2.59+build.7", "1.2.59-rc.1"));
        assert!(is_newer_version("1.2.59-rc.2+build.7", "1.2.59-rc.1+build.9"));
    }

    /// The preview must show the same text that goes into the card. Previously
    /// `traits_save` returned `traits::fragment`, while the gateway carried `card_fragment`:
    /// for a character without tuned traits the preview kept one line about
    /// repeats, while the model received proportions for all tags.
    #[test]
    fn test_card_text_for_equals_the_gateway_note() {
        let entry = traits::CharacterTraits::default();
        let st_tags = vec!["Энергичность".to_string()];
        let text = card_text_for(&entry, &st_tags, &[]);
        assert_eq!(text, traits::card_fragment(&entry, &st_tags));
        assert!(text.contains("Энергичность: 50%"), "{text}");
    }

    #[test]
    fn test_card_text_for_appends_bonds_below_traits() {
        let entry = traits::CharacterTraits {
            intensity: [("Ласковость".to_string(), 60u8)].into_iter().collect(),
            ..Default::default()
        };
        let bond = bonds::Bond {
            target: "Gawr_Gura.png".to_string(),
            kind: "сестра".to_string(),
            strength: 70,
            ..Default::default()
        };
        let text = card_text_for(&entry, &[], &[bond.clone()]);
        assert!(text.contains("Ласковость: 60%"), "{text}");
        assert!(text.contains("\n\nОтношения с другими персонажами"), "{text}");
        assert!(text.contains("Gawr Gura: сестра, 70%"), "{text}");
        // Without bonds the block disappears entirely — the card returns to traits.
        assert_eq!(card_text_for(&entry, &[], &[]), traits::fragment(&entry));
    }

    fn test_state(settings: Settings) -> AppState {
        AppState {
            client: Mutex::new(None),
            runtime: Mutex::new(None),
            runtime_lock: tokio::sync::Mutex::new(()),
            st_child: Mutex::new(None),
            settings: Mutex::new(settings),
            cancel_download: Arc::new(AtomicBool::new(false)),
            downloads: downloads::Queue::new(),
            vision_downloading: AtomicBool::new(false),
            st_port: std::sync::atomic::AtomicU16::new(8000),
            boot_status: Mutex::new(String::new()),
            boot_progress: Mutex::new(None),
            boot_error: Mutex::new(None),
            boot_ready: AtomicBool::new(false),
            boot_in_progress: AtomicBool::new(false),
            st_opened: AtomicBool::new(false),
            context_ceiling: std::sync::atomic::AtomicU32::new(0),
            model_fits: AtomicBool::new(true),
            active_character: Mutex::new(String::new()),
            active_persona: Mutex::new(String::new()),
        }
    }

    /// An error inside the change touches neither memory nor disk.
    #[test]
    fn test_update_settings_keeps_memory_on_error() {
        let state = test_state(Settings { tuned_for: "было".into(), ..Settings::default() });
        let result: Result<(), String> = update_settings(&state, |s| {
            s.tuned_for = "стало".into();
            Err("отказ".into())
        });
        assert!(result.is_err());
        assert_eq!(state.settings.lock().unwrap().tuned_for, "было");
    }

    /// Fields that `keep_backend_fields` takes from memory: we change each field in the window snapshot
    /// one at a time and see whether the edit survived.
    fn backend_owned_fields() -> Vec<String> {
        let base = serde_json::to_value(Settings::default()).unwrap();
        let mut owned = Vec::new();
        for (key, value) in base.as_object().unwrap() {
            let mut edited = base.clone();
            edited[key] = match value {
                serde_json::Value::Bool(b) => serde_json::json!(!b),
                serde_json::Value::Number(n) if n.is_u64() => serde_json::json!(n.as_u64().unwrap() + 1),
                serde_json::Value::Number(n) => serde_json::json!(n.as_f64().unwrap() + 0.5),
                serde_json::Value::String(s) => serde_json::json!(format!("{s}-правка")),
                other => panic!("неожиданный тип поля {key}: {other}"),
            };
            let incoming: Settings = serde_json::from_value(edited).unwrap();
            let merged = serde_json::to_value(keep_backend_fields(incoming, &Settings::default())).unwrap();
            if merged[key] == base[key] {
                owned.push(key.clone());
            }
        }
        owned
    }

    /// A stale window snapshot does not roll back what the backend writes, while the user's
    /// edits from the window go through.
    #[test]
    fn test_window_snapshot_keeps_backend_fields() {
        let owned = backend_owned_fields();
        for field in ["tuned_for", "changelog_version_seen", "ollama_port", "theme_version", "tags_version", "models_moved", "image_model", "tagger_opt_in_migrated", "models_upgrade_dismissed"] {
            assert!(owned.iter().any(|f| f == field), "{field} должно браться из памяти: {owned:?}");
        }
        for field in ["chat_model", "embed_model", "vision_model", "context_size", "temperature", "max_tokens", "tagger_enabled", "auto_translate_imports", "evaluator_model", "director_prompt"] {
            assert!(!owned.iter().any(|f| f == field), "{field} редактируется в окне: {owned:?}");
        }
    }

    /// A field taken from memory must not be editable by the window: otherwise its
    /// edit in the window would silently disappear on save.
    #[test]
    fn test_window_does_not_edit_backend_fields() {
        let window = [include_str!("../../src/Settings.svelte"), include_str!("../../src/Wizard.svelte")];
        for field in backend_owned_fields() {
            let assignment = format!(".{field} =");
            for source in window {
                assert!(!source.contains(&format!("settings.{field}}}")), "окно привязывает поле {field}");
                let assigned = source
                    .match_indices(&assignment)
                    .any(|(at, _)| !source[at + assignment.len()..].starts_with('='));
                assert!(!assigned, "окно присваивает поле {field}");
            }
        }
    }

    /// After a chat model is deleted, the chat does not become an embedder or a vision model.
    #[test]
    fn test_deleted_chat_model_falls_back_to_chat_model() {
        let installed: Vec<String> = [
            "bge-m3:latest",
            "huihui_ai/qwen3-vl-abliterated:8b-instruct-q4_K_M",
            "nomic-embed-text:latest",
            "huihui_ai/qwen3-abliterated:8b",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let mut s = Settings {
            chat_model: "huihui_ai/qwen3-abliterated:14b".into(),
            embed_model: "bge-m3:latest".into(),
            ..Settings::default()
        };
        assert!(forget_deleted_model(&mut s, "huihui_ai/qwen3-abliterated:14b", &installed));
        assert_eq!(s.chat_model, "huihui_ai/qwen3-abliterated:8b");
        assert_eq!(s.embed_model, "bge-m3:latest");

        let only_helpers = &installed[..3];
        let mut s = Settings { chat_model: "huihui_ai/qwen3-abliterated:8b".into(), ..Settings::default() };
        assert!(forget_deleted_model(&mut s, "huihui_ai/qwen3-abliterated:8b", only_helpers));
        assert_eq!(s.chat_model, "", "без чат-моделей роль остаётся пустой");

        let mut s = Settings { chat_model: "huihui_ai/qwen3-abliterated:8b".into(), ..Settings::default() };
        assert!(!forget_deleted_model(&mut s, "some-other:latest", &installed));

        assert!(is_likely_chat_model("cloufam-devlin:latest"));
        assert!(!is_likely_chat_model("huihui_ai/qwen3-vl-abliterated:2b-instruct-q4_K_M"));
    }

    /// The model upgrade flow: roles move from the old model to the new one,
    /// other settings are not touched.
    #[test]
    fn test_retarget_model_switches_all_roles() {
        let mut s = Settings {
            chat_model: "huihui_ai/qwen3-abliterated:14b-v2".into(),
            vision_model: "huihui_ai/qwen3.5-abliterated:9b".into(),
            evaluator_model: "huihui_ai/qwen3-abliterated:14b-v2".into(),
            tagger_model: String::new(),
            ..Settings::default()
        };
        assert!(retarget_model(&mut s, "huihui_ai/qwen3-abliterated:14b-v2", "huihui_ai/qwen3.5-abliterated:9b"));
        assert_eq!(s.chat_model, "huihui_ai/qwen3.5-abliterated:9b");
        assert_eq!(s.evaluator_model, "huihui_ai/qwen3.5-abliterated:9b");
        assert_eq!(s.vision_model, "huihui_ai/qwen3.5-abliterated:9b", "чужая роль не меняется");
        assert_eq!(s.tagger_model, "", "«как основная» остаётся пустой");

        // Nothing pointed to the old model — nothing to change.
        let mut s = Settings { chat_model: "huihui_ai/qwen3.5-abliterated:4B".into(), ..Settings::default() };
        assert!(!retarget_model(&mut s, "huihui_ai/qwen3-abliterated:14b-v2", "huihui_ai/qwen3.5-abliterated:9b"));
    }

    /// "Later" writes from-tags comma-separated and reads them back without garbage.
    #[test]
    fn test_dismissed_list_roundtrips() {
        assert!(parse_dismissed("").is_empty());
        assert!(parse_dismissed(" , ,").is_empty());
        let list = parse_dismissed("a:1, b:2 ,");
        assert_eq!(list, vec!["a:1".to_string(), "b:2".to_string()]);
    }

    /// Untuned traits are shown with the same intensity with which
    /// `card_fragment` carries them into the prompt: saving without edits does not change the prompt.
    #[test]
    fn test_unconfigured_tag_intensity_matches_card_fragment() {
        let st_tags = vec!["озорная".to_string()];
        let mut configured = traits::CharacterTraits::default();
        configured.intensity.insert("озорная".into(), UNCONFIGURED_TAG_INTENSITY);
        assert_eq!(
            traits::card_fragment(&traits::CharacterTraits::default(), &st_tags),
            traits::fragment(&configured)
        );
    }

    #[test]
    fn test_ps_single_quoted_escapes_apostrophe() {
        assert_eq!(ps_single_quoted(r"C:\Users\O'Brien\Cloufam.exe"), r"C:\Users\O''Brien\Cloufam.exe");
        assert_eq!(ps_single_quoted(r"C:\Cloufam"), r"C:\Cloufam");
    }

    /// The installer drops `cloufam.exe`: the `Cloufam.exe` mirror is the same file.
    #[test]
    fn test_sync_to_disk_flushes_an_existing_file() {
        let path = std::env::temp_dir().join(format!("lk-sync-{}.bin", std::process::id()));
        std::fs::write(&path, b"exe").unwrap();
        sync_to_disk(&path).expect("fsync готового файла");
        assert_eq!(std::fs::read(&path).unwrap(), b"exe", "содержимое не тронуто");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_mirror_exe_name_is_case_insensitive() {
        let exe = std::path::Path::new(r"C:\Program Files\Cloufam\cloufam.exe");
        assert!(is_same_exe_name(exe, "Cloufam.exe"));
        assert!(!is_same_exe_name(exe, "local-chatbot.exe"));
    }

    fn test_cpu_gpu() -> gpu::GpuInfo {
        gpu::GpuInfo {
            has_gpu: false,
            vendor: gpu::GpuVendor::Unknown,
            name: None,
            vram_mb: None,
            backend: gpu::GpuBackend::Cpu,
            tier: gpu::HardwareTier::Minimal,
            notes: Vec::new(),
        }
    }

    fn test_recommend_input(
        hardware: gpu::GpuInfo,
        installed: &[&str],
        checkpoints: &[(&str, u64)],
    ) -> recommend::Input {
        recommend::Input {
            hardware,
            system: system::SystemInfo {
                cpu: "тестовый процессор".into(),
                cpu_cores: 8,
                cpu_threads: 16,
                ram_total_mb: 32768,
                ram_free_mb: 16384,
                disk_free_gb: 500.0,
            },
            installed_models: installed.iter().map(|s| s.to_string()).collect(),
            checkpoints: checkpoints.iter().map(|(n, s)| (n.to_string(), *s)).collect(),
            context_ceiling: 0,
        }
    }

    /// V-58: auto-tuning (`lib::auto_tune`) applies only what does not
    /// lower the user's values. A false tier (a laptop's dGPU powered down →
    /// `nvidia-smi` fails, DXGI reports the iGPU) used to lower
    /// `max_tokens` to 400, `memory_max_chunks` to 3 and turn off ComfyUI autolaunch
    /// over manual values in one run. Contract lib.rs ↔ recommend.rs: `auto_tune`
    /// writes exactly `AutoTunePlan::items` (there is no lowering there) and sets the fingerprint
    /// only from `plan.stamp`; lowering stays a report suggestion.
    #[test]
    fn test_auto_tune_never_lowers_user_values() {
        // An honest CPU → CUDA switch, but the reply length is set by hand above the tier
        // target: the lowering item is visible in the report and does not enter auto-application.
        let s = Settings { max_tokens: 1500, ..Settings::default() };
        let high = test_recommend_input(
            gpu::build_vendor_info(gpu::GpuVendor::Nvidia, "NVIDIA GeForce RTX".into(), 16303),
            &["huihui_ai/qwen3.5-abliterated:9b"],
            &[],
        );
        let report = recommend::plan(&s, &high);
        assert!(
            report
                .iter()
                .any(|item| item.key == recommend::SettingKey::MaxTokens && item.lowers_user_value),
            "пункт понижения показывается в отчёте"
        );

        let plan = recommend::auto_tune_plan("0|Cpu", &s, &high).expect("CPU→CUDA — честное железо");
        assert!(plan.applicable.iter().all(|item| !item.lowers_user_value), "авто-применение не понижает");
        assert!(
            plan.applicable.iter().all(|item| item.key != recommend::SettingKey::MaxTokens),
            "понижение не применяется молча"
        );

        // lib::auto_tune writes exactly plan.applicable and sets the fingerprint from plan.stamp.
        let applied =
            recommend::apply(&s, &plan.applicable, plan.stamp.then_some(plan.fingerprint.as_str())).unwrap();
        assert_eq!(applied.max_tokens, 1500, "ручное значение не перетёрто");
        assert_eq!(
            applied.tuned_for,
            if plan.stamp {
                plan.fingerprint.clone()
            } else {
                s.tuned_for.clone()
            },
            "отпечаток пишется только по plan.stamp"
        );
    }

    /// V-58: a failed GPU probe yields no plan at all — a false tier does not
    /// overwrite settings and does not write a false fingerprint.
    #[test]
    fn test_auto_tune_skips_a_suspicious_gpu_probe() {
        let s = Settings {
            max_tokens: 200,
            memory_max_chunks: 8,
            auto_launch_comfy: true,
            ..Settings::default()
        };
        // The previous fingerprint is High under CUDA, while now only the iGPU is visible or
        // nothing at all: a laptop's dGPU powered down, not a GPU replacement.
        for probe in [
            test_cpu_gpu(),
            gpu::build_vendor_info(gpu::GpuVendor::Intel, "Intel Iris Xe".into(), 1024),
        ] {
            let input = test_recommend_input(probe, &[], &[]);
            assert!(
                recommend::auto_tune_plan("3|Cuda", &s, &input).is_none(),
                "ложный тир не должен давать план"
            );
        }
        // The first tuning for the hardware (no fingerprint) — tuning proceeds.
        let input = test_recommend_input(test_cpu_gpu(), &[], &[]);
        assert!(recommend::auto_tune_plan("", &s, &input).is_some());
    }
}

