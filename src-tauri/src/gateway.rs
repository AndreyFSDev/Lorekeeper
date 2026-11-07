use std::convert::Infallible;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{stream, StreamExt};
use serde::Deserialize;
use serde_json::{json, Map, Value};

static ACTUAL_GATEWAY_PORT: AtomicU16 = AtomicU16::new(0);

/// Gateway port: returns the actually occupied port (if the gateway is running)
/// or finds the first free port starting from ollama_port + 1.
pub fn gateway_port(ollama_port: u16) -> u16 {
    let bound = ACTUAL_GATEWAY_PORT.load(Ordering::SeqCst);
    if bound != 0 {
        return bound;
    }
    find_free_gateway_port(ollama_port)
}

/// The Ollama port the gateway is connected to: passed to the ST node so
/// the server plugin knows where to go for auto-tags.
pub fn ollama_port() -> u16 {
    TARGET_OLLAMA_PORT.load(Ordering::Relaxed)
}

/// Candidate ports next to Ollama: +1..+15, then -1..-15.
fn port_candidates(ollama_port: u16) -> Vec<u16> {
    let mut out = Vec::with_capacity(30);
    for offset in 1..=15u16 {
        if let Some(p) = ollama_port.checked_add(offset) {
            out.push(p);
        }
        if let Some(p) = ollama_port.checked_sub(offset) {
            if p != 0 {
                out.push(p);
            }
        }
    }
    out
}

fn find_free_gateway_port(ollama_port: u16) -> u16 {
    for candidate in port_candidates(ollama_port) {
        if std::net::TcpListener::bind(("127.0.0.1", candidate)).is_ok() {
            return candidate;
        }
    }
    if ollama_port < 65535 { ollama_port + 1 } else { ollama_port - 1 }
}

static TARGET_OLLAMA_PORT: AtomicU16 = AtomicU16::new(11434);
/// The live gateway server: the Ollama port it is serving and its task.
static GATEWAY_TASK: std::sync::OnceLock<
    tokio::sync::Mutex<Option<(u16, tauri::async_runtime::JoinHandle<()>)>>,
> =
    std::sync::OnceLock::new();

#[derive(serde::Serialize, Clone, Default)]
pub struct GatewayUpdateProgress {
    pub updating: bool,
    pub stage: String,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub percent: u8,
    pub speed_bps: u64,
    pub error: Option<String>,
    pub done: bool,
}

/// The lock is synchronous: progress is written right from the download callback. Spawning a task
/// per chunk reordered the writes and could return `updating = true` after the
/// final `updating = false` — the SillyTavern bar stuck on
/// «Загрузка…» forever, and the update error never reached the user.
static UPDATE_PROGRESS: std::sync::OnceLock<std::sync::Mutex<GatewayUpdateProgress>> =
    std::sync::OnceLock::new();
static CACHED_UPDATE_INFO: std::sync::OnceLock<
    tokio::sync::RwLock<Option<(std::time::Instant, crate::AppUpdateInfo)>>,
> = std::sync::OnceLock::new();

fn update_progress() -> std::sync::MutexGuard<'static, GatewayUpdateProgress> {
    UPDATE_PROGRESS
        .get_or_init(|| std::sync::Mutex::new(GatewayUpdateProgress::default()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn get_cached_update_lock() -> &'static tokio::sync::RwLock<Option<(std::time::Instant, crate::AppUpdateInfo)>> {
    CACHED_UPDATE_INFO.get_or_init(|| tokio::sync::RwLock::new(None))
}

/// Model download state started from the menu inside SillyTavern.
/// Filled by the same events the app window sees.
#[derive(Default, Clone, serde::Serialize)]
struct ModelDownloadState {
    running: bool,
    model: String,
    completed: u64,
    total: u64,
    status: String,
    speed: u64,
    eta: u64,
    error: Option<String>,
}

static MODEL_DOWNLOAD: OnceLock<std::sync::Mutex<ModelDownloadState>> = OnceLock::new();

fn model_download() -> &'static std::sync::Mutex<ModelDownloadState> {
    MODEL_DOWNLOAD.get_or_init(|| std::sync::Mutex::new(ModelDownloadState::default()))
}

/// Watch the download queue for the SillyTavern page: the app window
/// gets the snapshot directly, while the models menu in ST polls the gateway.
///
/// The source is the queue snapshot, not individual progress events: an item removed
/// by cancellation before start or closed by pause produces no transport events, and
/// the menu would be left with an eternal «идёт загрузка» and a refusal for new ones.
fn subscribe_model_events(app: &tauri::AppHandle) {
    use tauri::Listener;
    // There is a single listener: when the gateway restarts (port change) we remove the old one —
    // otherwise every handler duplicated and wrote into the same state in turn.
    static SUB: std::sync::OnceLock<std::sync::Mutex<Option<tauri::EventId>>> = std::sync::OnceLock::new();
    let cell = SUB.get_or_init(|| std::sync::Mutex::new(None));
    if let Ok(mut guard) = cell.lock() {
        if let Some(id) = guard.take() {
            app.unlisten(id);
        }
        let queue_app = app.clone();
        let id = queue_app.listen("downloads-changed", move |event| {
        let Ok(snapshot) = serde_json::from_str::<Value>(event.payload()) else {
            return;
        };
        let tasks: Vec<Value> = snapshot["tasks"]
            .as_array()
            .map(|tasks| {
                tasks
                    .iter()
                    .filter(|task| task["kind"] == "ollama")
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let active = tasks.iter().find(|task| {
            matches!(
                task["status"].as_str(),
                Some("queued") | Some("downloading") | Some("paused")
            )
        });
        let Ok(mut guard) = model_download().lock() else {
            return;
        };
        if let Some(task) = active {
            guard.model = task["id"].as_str().unwrap_or_default().to_string();
            guard.status = task["note"].as_str().unwrap_or_default().to_string();
            guard.completed = task["completed"].as_u64().unwrap_or(0);
            guard.total = task["total"].as_u64().unwrap_or(0);
            guard.speed = task["speed"].as_u64().unwrap_or(0);
            guard.eta = task["eta"].as_u64().unwrap_or(0);
            guard.running = true;
            guard.error = None;
            return;
        }
        // No active items: show how the last one ended.
        guard.running = false;
        guard.speed = 0;
        guard.eta = 0;
        let Some(last) = tasks.last() else { return };
        match last["status"].as_str() {
            Some("done") => {
                guard.status = "готово".into();
                guard.error = None;
            }
            Some("cancelled") => {
                guard.status = "отменено".into();
                guard.error = None;
            }
            Some("failed") => {
                guard.status = "ошибка".into();
                guard.error = last["error"].as_str().map(|s| s.to_string());
            }
            // Nothing finished (the queue just started) — keep the previous status.
            _ => {}
        }
    });
        *guard = Some(id);
    }
}

/// GET /api/cloufam/models — a showcase for this hardware: what is installed and what is selected.
async fn handle_models(State(state): State<Arc<GatewayState>>) -> impl IntoResponse {
    use tauri::Manager;
    let hardware = crate::gpu::gpu_info();
    // The showcase is useful even without Ollama: with no http client we show the catalog
    // with an empty installed list instead of crashing the endpoint (V-35).
    let installed = match ollama_client() {
        Ok(client) => crate::models::installed(&client).await,
        Err(_) => Vec::new(),
    };
    let settings = app_settings(&state.app);
    let (ceiling, fits) = state
        .app
        .try_state::<Arc<crate::AppState>>()
        .map(|s| {
            (
                s.context_ceiling.load(std::sync::atomic::Ordering::Relaxed),
                s.model_fits.load(std::sync::atomic::Ordering::Relaxed),
            )
        })
        .unwrap_or((0, false));
    cors_json(
        StatusCode::OK,
        json!({
            "hardware": hardware,
            "installed": installed,
            "settings": settings,
            "context_ceiling": ceiling,
            "model_fits": fits,
            "models": crate::catalog::visible(&hardware),
        })
        .to_string(),
    )
}

/// POST /api/cloufam/models/download — download a model in the background.
async fn handle_model_download(
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    use tauri::Manager;
    let model = body["model"].as_str().unwrap_or_default().trim().to_string();
    if model.is_empty() {
        return cors_json(StatusCode::BAD_REQUEST, r#"{"error":"не указана модель"}"#.into());
    }
    // Before setting `running`: exiting here would leave the flag raised, and the menu
    // would answer 409 «уже скачивается» until the next queue snapshot.
    let Some(app_state) = state.app.try_state::<Arc<crate::AppState>>() else {
        return cors_json(StatusCode::INTERNAL_SERVER_ERROR, r#"{"error":"приложение не готово"}"#.into());
    };
    {
        let Ok(mut guard) = model_download().lock() else {
            return cors_json(StatusCode::INTERNAL_SERVER_ERROR, r#"{"error":"состояние загрузки"}"#.into());
        };
        if guard.running {
            return cors_json(
                StatusCode::CONFLICT,
                json!({ "error": format!("уже скачивается {}", guard.model) }).to_string(),
            );
        }
        *guard = ModelDownloadState {
            running: true,
            model: model.clone(),
            status: "подготовка…".into(),
            ..Default::default()
        };
    }

    if app_state.client.lock().unwrap_or_else(|e| e.into_inner()).is_none() {
        if let Ok(mut guard) = model_download().lock() {
            guard.running = false;
            guard.error = Some("Ollama ещё не запущена".into());
        }
        return cors_json(StatusCode::SERVICE_UNAVAILABLE, r#"{"error":"Ollama не запущена"}"#.into());
    }

    // The download goes through the app's shared queue: it is visible in settings and can be
    // paused and cancelled — the models menu in SillyTavern only starts it.
    let title = crate::model_title(&model);
    let started = model.clone();
    crate::start_downloads(
        &state.app,
        &app_state,
        vec![crate::downloads::Task::ollama(&model, title)],
    );

    cors_json(StatusCode::OK, json!({ "started": true, "model": started }).to_string())
}

/// GET /api/cloufam/models/progress — how the download is going.
async fn handle_model_progress() -> impl IntoResponse {
    let payload = model_download()
        .lock()
        .map(|guard| serde_json::to_value(guard.clone()).unwrap_or_else(|_| json!({})))
        .unwrap_or_else(|_| json!({}));
    cors_json(StatusCode::OK, payload.to_string())
}

/// POST /api/cloufam/models/select — select a model for chat, memory, or vision.
async fn handle_model_select(
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let model = body["model"].as_str().unwrap_or_default().trim().to_string();
    let category = body["category"].as_str().unwrap_or("chat").to_string();
    if model.is_empty() {
        return cors_json(StatusCode::BAD_REQUEST, r#"{"error":"не указана модель"}"#.into());
    }
    if !matches!(category.as_str(), "chat" | "embed" | "vision") {
        return cors_json(
            StatusCode::BAD_REQUEST,
            json!({ "error": format!("неизвестная категория {category}") }).to_string(),
        );
    }
    use tauri::Manager;
    if state.app.try_state::<Arc<crate::AppState>>().is_none() {
        return cors_json(StatusCode::INTERNAL_SERVER_ERROR, r#"{"error":"приложение не готово"}"#.into());
    }
    // Editing a single field under the settings lock: a full snapshot from this handler
    // would clobber what the window saved in parallel (T-22).
    let (category_c, model_c) = (category.clone(), model.clone());
    let applied = crate::apply_settings_with(&state.app, move |s| {
        match category_c.as_str() {
            "chat" => s.chat_model = model_c,
            // For existing installs the memory model changes only on an explicit selection:
            // their vectors were already built with the previous model.
            "embed" => s.embed_model = model_c,
            _ => s.vision_model = model_c,
        }
        Ok(())
    })
    .await;
    match applied {
        Ok(_) => cors_json(
            StatusCode::OK,
            json!({ "ok": true, "category": category, "model": model }).to_string(),
        ),
        Err(e) => cors_json(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e }).to_string()),
    }
}

/// Ollama client for the current gateway port.
fn ollama_client() -> Result<crate::ollama_client::OllamaClient, String> {
    let port = TARGET_OLLAMA_PORT.load(Ordering::Relaxed);
    crate::ollama_client::OllamaClient::new(format!("http://127.0.0.1:{port}"))
}

/// Headers for requests from the SillyTavern page: it lives on its own port, and
/// without them the browser does not hand the response to the script.
const CORS_HEADERS: [(&str, &str); 3] = [
    ("Access-Control-Allow-Origin", "*"),
    ("Access-Control-Allow-Headers", "Content-Type"),
    ("Access-Control-Allow-Methods", "GET, POST, OPTIONS"),
];

fn cors_json(status: StatusCode, body: String) -> (StatusCode, [(&'static str, &'static str); 3], String) {
    (status, CORS_HEADERS, body)
}

/// Attach CORS headers to any gateway response. `*` from the handlers is replaced with an echo
/// of the already validated `local_only` source: an "everyone" permission gave gateway
/// responses to any browser tab.
fn add_cors_headers(response: &mut Response, origin: Option<axum::http::HeaderValue>) {
    let headers = response.headers_mut();
    for (name, value) in CORS_HEADERS {
        if let (Ok(name), Ok(value)) = (
            axum::http::HeaderName::from_bytes(name.as_bytes()),
            axum::http::HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    match origin {
        Some(origin) => {
            headers.insert(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
            headers.append(axum::http::header::VARY, axum::http::HeaderValue::from_static("Origin"));
        }
        None => {
            headers.remove(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN);
        }
    }
}

/// The gateway listens on 127.0.0.1, but the user's browser can reach it from any
/// open tab: a foreign site read roleplay chat traces and settings,
/// started model downloads, and took over the GPU through `/v1/chat/completions`.
/// We let in pages from local addresses (SillyTavern, the app window) and
/// clients without Origin (the ST node, its server plugin). We verify Host because of DNS
/// rebinding: a foreign domain rebound to 127.0.0.1 sends GET without Origin.
async fn local_only(request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    if !is_local_request(request.headers()) || !has_loopback_host(request.headers()) {
        return (
            StatusCode::FORBIDDEN,
            [("Content-Type", "application/json")],
            r#"{"error":"шлюз Cloufam доступен только локальным страницам"}"#,
        )
            .into_response();
    }
    let origin = request.headers().get(axum::http::header::ORIGIN).cloned();
    let mut response = next.run(request).await;
    add_cors_headers(&mut response, origin);
    response
}

/// The response to the browser's preflight request before a POST with JSON.
async fn handle_cors_preflight() -> impl IntoResponse {
    (StatusCode::OK, CORS_HEADERS, String::new())
}

async fn handle_update_options() -> impl IntoResponse {
    (
        StatusCode::OK,
        [
            ("Access-Control-Allow-Origin", "*"),
            ("Access-Control-Allow-Methods", "GET, POST, OPTIONS"),
            ("Access-Control-Allow-Headers", "*"),
        ],
        "",
    )
}

async fn handle_update_status(State(state): State<Arc<GatewayState>>) -> impl IntoResponse {
    let cache_lock = get_cached_update_lock();
    let need_fetch = {
        let guard = cache_lock.read().await;
        match guard.as_ref() {
            Some((time, _)) => time.elapsed() > std::time::Duration::from_secs(600),
            None => true,
        }
    };

    if need_fetch {
        let version = state.app.package_info().version.to_string();
        if let Ok(info) = crate::check_app_update_core(&version).await {
            let mut guard = cache_lock.write().await;
            *guard = Some((std::time::Instant::now(), info));
        }
    }

    let guard = cache_lock.read().await;
    let json = if let Some((_, info)) = guard.as_ref() {
        serde_json::to_string(info).unwrap_or_else(|_| r#"{"available":false}"#.to_string())
    } else {
        r#"{"available":false}"#.to_string()
    };

    (
        StatusCode::OK,
        [
            ("Access-Control-Allow-Origin", "*"),
            ("Content-Type", "application/json"),
        ],
        json,
    )
}

async fn handle_update_progress() -> impl IntoResponse {
    let json = {
        let guard = update_progress();
        serde_json::to_string(&*guard).unwrap_or_else(|_| r#"{"updating":false}"#.to_string())
    };
    (
        StatusCode::OK,
        [
            ("Access-Control-Allow-Origin", "*"),
            ("Content-Type", "application/json"),
        ],
        json,
    )
}

/// The request source is a local page (the SillyTavern UI, the app window)
/// or no browser at all. Update is a privileged action: a foreign page
/// in the user's browser cannot use it, even though the port listens on 127.0.0.1.
fn is_local_request(headers: &axum::http::HeaderMap) -> bool {
    let Some(origin) = headers.get("origin") else {
        return true;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let Some(host) = origin.split("//").nth(1).and_then(|rest| rest.split('/').next()) else {
        return false;
    };
    is_loopback_authority(host)
}

/// The request Host is the gateway itself by a local name. Without the header (HTTP/1.0) —
/// not a browser, we let it through.
fn has_loopback_host(headers: &axum::http::HeaderMap) -> bool {
    match headers.get(axum::http::header::HOST) {
        None => true,
        Some(host) => host.to_str().is_ok_and(is_loopback_authority),
    }
}

/// `host[:port]` points at this machine by a name that DNS
/// cannot rebind.
fn is_loopback_authority(authority: &str) -> bool {
    let host = authority.rsplit_once(':').map(|(host, _)| host).unwrap_or(authority);
    matches!(
        host.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "tauri.localhost" | "[::1]"
    )
}

async fn handle_apply_update(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> impl IntoResponse {
    if !is_local_request(&headers) {
        return (
            StatusCode::FORBIDDEN,
            [("Access-Control-Allow-Origin", "*"), ("Content-Type", "application/json")],
            r#"{"error":"обновление доступно только из приложения"}"#.to_string(),
        );
    }
    {
        let mut guard = update_progress();
        if guard.updating {
            return (
                StatusCode::CONFLICT,
                [("Access-Control-Allow-Origin", "*"), ("Content-Type", "application/json")],
                r#"{"error":"Обновление уже выполняется"}"#.to_string(),
            );
        }
        *guard = GatewayUpdateProgress {
            updating: true,
            stage: "Подготовка к загрузке…".to_string(),
            ..Default::default()
        };
    }

    let app = state.app.clone();
    tauri::async_runtime::spawn(async move {
        let res = crate::apply_app_update_core(&app, |p| {
            let mut guard = update_progress();
            guard.stage = p.stage;
            guard.downloaded_bytes = p.downloaded_bytes;
            guard.total_bytes = p.total_bytes;
            guard.percent = p.percent;
            guard.speed_bps = p.speed_bps;
        })
        .await;

        let mut guard = update_progress();
        guard.updating = false;
        if let Err(e) = res {
            guard.error = Some(e);
        } else {
            guard.done = true;
        }
    });

    (
        StatusCode::OK,
        [("Access-Control-Allow-Origin", "*"), ("Content-Type", "application/json")],
        r#"{"ok":true}"#.to_string(),
    )
}

#[derive(Clone)]
struct GatewayState {
    client: reqwest::Client,
    app: tauri::AppHandle,
}

async fn handle_open_settings_options() -> impl IntoResponse {
    (
        StatusCode::OK,
        [
            ("Access-Control-Allow-Origin", "*"),
            ("Access-Control-Allow-Methods", "POST, OPTIONS"),
            ("Access-Control-Allow-Headers", "*"),
        ],
        "",
    )
}

/// The settings window tabs are the only source of the list (V-45): the frontend takes it
/// only through the `settings_tabs` IPC command (`api.settingsTabs`) and keeps no
/// copies; uniqueness is guarded by the test
/// `settings_tabs_live_only_in_the_gateway`.
pub const SETTINGS_TABS: [&str; 10] =
    ["chat", "embed", "vision", "image", "tune", "params", "custom", "tags", "train", "imgtrain"];

/// The tab from the request body. The string goes into the window address
/// (`index.html#settings/<tab>`) without escaping, so we drop an unknown value:
/// the window opens on the default tab.
fn requested_settings_tab(body: &[u8]) -> Option<String> {
    let value = serde_json::from_slice::<Value>(body).ok()?;
    let tab = value.get("tab")?.as_str()?;
    SETTINGS_TABS.contains(&tab).then(|| tab.to_string())
}

/// IPC for the settings window (V-45): the tabs list, one source — this file.
/// The Svelte side calls it on mount (`api.settingsTabs`) and keeps no copy of
/// its own; `settings_tabs_live_only_in_the_gateway` enforces that.
#[tauri::command]
pub fn settings_tabs() -> Vec<String> {
    SETTINGS_TABS.iter().map(|tab| (*tab).to_string()).collect()
}

async fn handle_open_settings(State(state): State<Arc<GatewayState>>, body: Bytes) -> impl IntoResponse {
    // The body is optional: old calls send an empty POST, the «Черты и
    // характер» item adds a tab.
    let tab = requested_settings_tab(&body);
    crate::show_settings_window_tab(&state.app, tab);
    (
        StatusCode::OK,
        [
            ("Access-Control-Allow-Origin", "*"),
            ("Content-Type", "application/json"),
        ],
        r#"{"ok":true}"#,
    )
}

/// LLM gateway: accepts the OpenAI format from SillyTavern and translates it into
/// Ollama's native /api/chat with think:false.
pub async fn start(ollama_port: u16, app: tauri::AppHandle) -> Result<(), String> {
    TARGET_OLLAMA_PORT.store(ollama_port, Ordering::SeqCst);
    let _ = APP_HANDLE.set(app.clone());
    let task = GATEWAY_TASK.get_or_init(|| tokio::sync::Mutex::new(None));
    let mut task_guard = task.lock().await;
    // We compare the target, not "is the port free": the port of our own listener cannot
    // be bound, and every repeated bring_up (retry_boot, finalize_setup)
    // restarted the gateway, cutting off running streams.
    if let Some((served_ollama_port, handle)) = task_guard.as_ref() {
        if *served_ollama_port == ollama_port && !handle.inner().is_finished() {
            return Ok(());
        }
    }
    if let Some((_, handle)) = task_guard.take() {
        handle.abort();
        let _ = handle.await;
    }
    let state = Arc::new(GatewayState {
        // Every request of this client goes to loopback Ollama (/api/* on
        // 127.0.0.1): bypass the system proxy (otherwise the user's machine
        // proxy answers 503 instead of the local server).
        client: reqwest::Client::builder()
            .tcp_nodelay(true)
            .read_timeout(std::time::Duration::from_secs(300))
            .no_proxy()
            .build()
            .map_err(|e| e.to_string())?,
        app,
    });
    subscribe_model_events(&state.app);
    let router = Router::new()
        .route(
            "/v1/chat/completions",
            post(chat_completions)
                .options(handle_cors_preflight)
                .layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024)),
        )
        .route("/v1/models", get(list_models).options(handle_cors_preflight))
        .route("/api/cloufam/models", get(handle_models).options(handle_cors_preflight))
        .route(
            "/api/cloufam/models/download",
            post(handle_model_download).options(handle_cors_preflight),
        )
        .route(
            "/api/cloufam/models/progress",
            get(handle_model_progress).options(handle_cors_preflight),
        )
        .route(
            "/api/cloufam/models/select",
            post(handle_model_select).options(handle_cors_preflight),
        )
        .route(
            "/api/cloufam/tags",
            post(tagger_tags).options(handle_cors_preflight),
        )
        .route(
            "/api/cloufam/tagger-config",
            get(tagger_config)
                .post(tagger_config_update)
                .options(handle_cors_preflight),
        )
        .route(
            "/api/cloufam/traits",
            get(traits_map).options(handle_cors_preflight),
        )
        .route(
            "/api/cloufam/active-character",
            post(set_active_character).options(handle_cors_preflight),
        )
        .route(
            "/api/cloufam/tag-influence",
            get(tag_influence_report).options(handle_cors_preflight),
        )
        .route(
            "/api/cloufam/character-evolution",
            get(character_evolution_report).options(handle_cors_preflight),
        )
        .route(
            "/api/cloufam/relationships",
            get(relationships_report).options(handle_cors_preflight),
        )
        .route(
            "/api/cloufam/character-relationships",
            get(character_relationships_report).options(handle_cors_preflight),
        )
        .route(
            "/api/cloufam/director",
            post(director_narration).options(handle_cors_preflight),
        )
        .route(
            "/api/cloufam/md-import",
            post(md_import_parse).options(handle_cors_preflight),
        )
        .route(
            "/api/cloufam/translate",
            post(translate_import).options(handle_cors_preflight),
        )
        .route("/api/cloufam/traces", get(traces_report).options(handle_cors_preflight))
        .route("/api/cloufam/traces/detail", get(trace_detail).options(handle_cors_preflight))
        .route(
            "/open-settings",
            post(handle_open_settings).options(handle_open_settings_options),
        )
        .route(
            "/api/cloufam/open-settings",
            post(handle_open_settings).options(handle_open_settings_options),
        )
        .route(
            "/api/cloufam/update-status",
            get(handle_update_status).options(handle_update_options),
        )
        .route(
            "/api/cloufam/update-progress",
            get(handle_update_progress).options(handle_update_options),
        )
        .route(
            "/api/cloufam/apply-update",
            post(handle_apply_update).options(handle_update_options),
        )
        // RPG endpoints
        .route("/api/cloufam/rpg/world", get(rpg_world).options(handle_cors_preflight))
        .route("/api/cloufam/rpg/world/move", post(rpg_world_move).options(handle_cors_preflight))
        .route("/api/cloufam/rpg/world/start", post(rpg_world_start).options(handle_cors_preflight))
        .route("/api/cloufam/rpg/world/reset", post(rpg_world_reset).options(handle_cors_preflight))
        .route("/api/cloufam/rpg/stats", get(rpg_stats).options(handle_cors_preflight))
        .route("/api/cloufam/rpg/skills", get(rpg_skills).options(handle_cors_preflight))
        .route("/api/cloufam/rpg/quests", get(rpg_quests).options(handle_cors_preflight))
        .route(
            "/api/cloufam/rpg/quests/generate",
            post(rpg_quest_generate).options(handle_cors_preflight),
        )
        .route(
            "/api/cloufam/rpg/npcs/generate",
            post(rpg_npc_generate).options(handle_cors_preflight),
        )
        .route("/api/cloufam/rpg/trade", post(rpg_trade).options(handle_cors_preflight))
        .route(
            "/api/cloufam/rpg/skills/custom",
            post(rpg_skills_custom).options(handle_cors_preflight),
        )
        .route("/api/cloufam/rpg/inventory", get(rpg_inventory).options(handle_cors_preflight))
        .route("/api/cloufam/rpg/combat", get(rpg_combat).options(handle_cors_preflight))
        .route("/api/cloufam/rpg/combat/start", post(rpg_combat_start).options(handle_cors_preflight))
        .route("/api/cloufam/rpg/combat/action", post(rpg_combat_action).options(handle_cors_preflight))
        .route("/api/cloufam/rpg/combat/end", post(rpg_combat_end).options(handle_cors_preflight))
        // Changelog
        .route("/api/cloufam/changelog", get(handle_changelog).options(handle_cors_preflight))
        .route("/api/cloufam/changelog/seen", post(handle_changelog_seen).options(handle_cors_preflight))
        .route("/api/cloufam/versions", get(handle_versions).options(handle_cors_preflight))
        .route("/api/cloufam/versions/rollback", post(handle_rollback).options(handle_update_options))
        // SillyTavern updates and chat backups
        .route(
            "/api/cloufam/st/update-check",
            get(handle_st_update_check).options(handle_cors_preflight),
        )
        .route(
            "/api/cloufam/st/update",
            post(handle_st_update).options(handle_cors_preflight),
        )
        // Long chat export — a single POST with the whole jsonl: axum's default 2 MB
        // limit cut it with 413 before the handler. The same limit — for vision
        // messages with base64 images in /v1/chat/completions.
        .route(
            "/api/cloufam/chat/export",
            post(handle_chat_export)
                .options(handle_cors_preflight)
                .layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024)),
        )
        .with_state(state)
        // CORS is a layer over the whole router: the SillyTavern page lives on its own
        // port, and without these headers the browser does not hand the response to the script. Previously
        // only individual handlers set them, and every new endpoint (versions,
        // changelog, relationships, RPG) silently became unavailable to the UI.
        .layer(axum::middleware::from_fn(local_only));

    let mut chosen = None;
    for candidate in port_candidates(ollama_port) {
        if let Ok(l) = tokio::net::TcpListener::bind(("127.0.0.1", candidate)).await {
            chosen = Some((candidate, l));
            break;
        }
    }

    let (port, listener) = chosen.ok_or_else(|| {
        format!("шлюз LLM: не удалось занять свободный порт около {ollama_port}")
    })?;

    ACTUAL_GATEWAY_PORT.store(port, Ordering::SeqCst);

    let handle = tauri::async_runtime::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    *task_guard = Some((ollama_port, handle));
    Ok(())
}

/// The speaker name at the start of text like "Name: line".
/// Returns the name only if a word that looks like a name stands before the colon:
/// no line breaks, no markup, no longer than 40 characters, and capitalized
/// in every word — otherwise it is an ordinary colon in prose ("She sighs: …")
/// and must not be touched.
fn leading_speaker_name(text: &str) -> Option<&str> {
    let head = text.trim_start();
    let colon = head.find(':')?;
    let name = head[..colon].trim();
    if name.is_empty() || name.chars().count() > 40 || name.contains('\n') {
        return None;
    }
    let looks_like_name = name
        .chars()
        .all(|c| c.is_alphanumeric() || c == ' ' || c == '-' || c == '.' || c == '\'')
        && crate::repetition::looks_like_speaker_name(name);
    looks_like_name.then_some(name)
}

fn name_matches(known: &str, candidate: &str) -> bool {
    let k = known.to_lowercase();
    let c = candidate.to_lowercase();
    if k == c {
        return true;
    }
    let k_norm = k.replace('_', " ");
    let c_norm = c.replace('_', " ");
    k_norm == c_norm
}

fn add_unique_name(names: &mut Vec<String>, name: &str) {
    let clean = name.trim().trim_matches(|c: char| c == '[' || c == ']' || c == '(' || c == ')' || c == '"' || c == '\'');
    if clean.is_empty() || clean.chars().count() > 50 {
        return;
    }
    if !names.iter().any(|n| name_matches(n, clean)) {
        names.push(clean.to_string());
    }
}

fn extract_names_from_text(text: &str, names: &mut Vec<String>) {
    // 1. Search in the group hint: "персонажей (A, B, C)"
    if let Some(start) = text.find("персонажей (") {
        if let Some(end) = text[start + "персонажей (".len()..].find(')') {
            let inside = &text[start + "персонажей (".len()..start + "персонажей (".len() + end];
            for part in inside.split(',') {
                let clean = part.trim();
                if !clean.is_empty() && clean.chars().count() <= 50 {
                    add_unique_name(names, clean);
                }
            }
        }
    }
    // 2. Search for "только у {{char}}" / "только у <name>"
    if let Some(start) = text.find("только у ") {
        let after = &text[start + "только у ".len()..];
        if let Some(end) = after.find(". Отвечай") {
            let clean = after[..end].trim();
            if !clean.is_empty() && clean.chars().count() <= 50 {
                add_unique_name(names, clean);
            }
        }
    }
}

/// The names of participants visible in the prompt: ST adds them as prefixes to group
/// lines ("Name: text") or in the group system hints.
fn known_speakers(messages: &Value) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let Some(list) = messages.as_array() else {
        return names;
    };
    for msg in list {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or_default();
        if role == "assistant" {
            if let Some(text) = msg.get("content").and_then(|v| v.as_str()) {
                if let Some(name) = leading_speaker_name(text) {
                    add_unique_name(&mut names, name);
                }
            }
        } else if role == "system" {
            if let Some(text) = msg.get("content").and_then(|v| v.as_str()) {
                extract_names_from_text(text, &mut names);
            }
        }
    }
    names
}

/// Character line guard:
/// 1. Strips a foreign or own name at the start of the reply ("Name: line"), so ST
///    does not trim the character's message to zero.
/// 2. Stops generation (`is_stopped()`) if the model, in the middle of a reply on a new line,
///    tries to continue the dialogue for another room participant ("\nName2: ..."), keeping
///    characters from interrupting each other and monopolizing the chat.
struct SpeakerGuard {
    known: Vec<String>,
    pending: String,
    leading_decided: bool,
    stopped: bool,
}

impl SpeakerGuard {
    const DECIDE_AT: usize = 64;

    fn new(messages: &Value) -> Self {
        let known = known_speakers(messages);
        let no_known = known.is_empty();
        Self {
            leading_decided: no_known,
            known,
            pending: String::new(),
            stopped: false,
        }
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    fn foreign_prefix_len(&self, text: &str) -> usize {
        let Some(name) = leading_speaker_name(text) else {
            return 0;
        };
        if !self.known.iter().any(|n| name_matches(n, name)) {
            return 0;
        }
        text.find(':').map(|i| i + 1).unwrap_or(0)
    }

    fn can_decide_leading(&self) -> bool {
        let head = self.pending.trim_start();
        if head.is_empty() {
            return false;
        }
        if head.contains('\n') || head.contains(':') || head.chars().count() >= Self::DECIDE_AT {
            return true;
        }
        let first = head.chars().next().unwrap_or_default().to_lowercase().to_string();
        !self.known.iter().any(|n| {
            n.chars()
                .next()
                .is_some_and(|c| c.to_lowercase().to_string() == first)
        })
    }

    fn line_starts_with_speaker(&self, line: &str) -> bool {
        let trimmed = line.trim_start();
        if let Some(colon_pos) = trimmed.find(':') {
            let candidate = trimmed[..colon_pos].trim().trim_matches(|c: char| c == '[' || c == ']' || c == '*' || c == '_');
            if candidate.is_empty() || candidate.chars().count() > 40 || candidate.contains('\n') {
                return false;
            }
            return self.known.iter().any(|n| name_matches(n, candidate));
        }
        false
    }

    fn is_potential_speaker_prefix(&self, line: &str) -> bool {
        let trimmed = line.trim_start().trim_matches(|c: char| c == '[' || c == '*' || c == '_');
        if trimmed.is_empty() {
            return true;
        }
        if trimmed.contains(':') || trimmed.chars().count() >= Self::DECIDE_AT {
            return false;
        }
        let lowered = trimmed.to_lowercase();
        self.known.iter().any(|n| {
            let n_low = n.to_lowercase();
            let n_norm = n_low.replace('_', " ");
            let l_norm = lowered.replace('_', " ");
            n_norm.starts_with(&l_norm) || l_norm.starts_with(&n_norm)
        })
    }

    fn find_midstream_stop(&self) -> Option<usize> {
        let mut search_from = 0;
        while let Some(nl_rel) = self.pending[search_from..].find('\n') {
            let nl_pos = search_from + nl_rel;
            let after = &self.pending[nl_pos + 1..];
            if self.line_starts_with_speaker(after) {
                return Some(nl_pos);
            }
            search_from = nl_pos + 1;
        }
        None
    }

    fn feed(&mut self, chunk: &str) -> String {
        if self.stopped {
            return String::new();
        }
        self.pending.push_str(chunk);

        if !self.leading_decided {
            if !self.can_decide_leading() {
                return String::new();
            }
            self.leading_decided = true;
            let skip = self.foreign_prefix_len(&self.pending);
            self.pending = self.pending[skip..].trim_start().to_string();
        }

        if self.known.is_empty() {
            return std::mem::take(&mut self.pending);
        }

        if let Some(stop_pos) = self.find_midstream_stop() {
            self.stopped = true;
            let emit = self.pending[..stop_pos].trim_end().to_string();
            self.pending.clear();
            return emit;
        }

        if let Some(last_nl) = self.pending.rfind('\n') {
            let tail = &self.pending[last_nl + 1..];
            if self.is_potential_speaker_prefix(tail) {
                let emit = self.pending[..last_nl].to_string();
                self.pending = self.pending[last_nl..].to_string();
                return emit;
            }
        }

        std::mem::take(&mut self.pending)
    }

    fn flush(&mut self) -> String {
        if self.stopped {
            return String::new();
        }
        if !self.leading_decided {
            self.leading_decided = true;
            let skip = self.foreign_prefix_len(&self.pending);
            self.pending = self.pending[skip..].trim_start().to_string();
        }
        if let Some(stop_pos) = self.find_midstream_stop() {
            self.stopped = true;
            return self.pending[..stop_pos].trim_end().to_string();
        }
        std::mem::take(&mut self.pending)
    }
}

/// The echo-guard verdict as a pair of "text to show, whether a copy was found".
fn echo_pass(verdict: crate::repetition::EchoVerdict) -> (String, bool) {
    match verdict {
        crate::repetition::EchoVerdict::Safe(text) => (text, false),
        crate::repetition::EchoVerdict::Copied(text) => (text, true),
    }
}

/// Pass a piece through the echo guard. While generation runs, the last words
/// are held back — a copy is confirmed only at the eighth. When generation
/// ended before the usual `done` (another speaker, an Ollama error, the end
/// of a non-stream), the held text must be released: otherwise a short reply disappears entirely.
fn echo_absorb(
    echo: &mut crate::repetition::EchoGuard,
    chunk: &str,
    generation_ended: bool,
) -> (String, bool) {
    let (mut shown, mut cut) = echo_pass(echo.feed(chunk));
    if generation_ended && !cut {
        let (rest, rest_cut) = echo_pass(echo.finish());
        shown.push_str(&rest);
        cut = rest_cut;
    }
    (shown, cut)
}

fn normalize_messages(messages: &Value) -> Value {
    let Some(arr) = messages.as_array() else {
        return messages.clone();
    };
    let normalized: Vec<Value> = arr
        .iter()
        .map(|msg| {
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            match msg.get("content") {
                Some(Value::Array(parts)) => {
                    let mut text_parts = Vec::new();
                    let mut images = Vec::new();
                    for part in parts {
                        let part_type = part.get("type").and_then(|v| v.as_str()).unwrap_or_default();
                        if part_type == "text" {
                            if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                                text_parts.push(t);
                            }
                        } else if part_type == "image_url" {
                            let url_opt = part
                                .get("image_url")
                                .and_then(|v| v.get("url").and_then(|u| u.as_str()).or_else(|| v.as_str()));
                            if let Some(url) = url_opt {
                                let b64 = if let Some(idx) = url.find(";base64,") {
                                    &url[idx + 8..]
                                } else {
                                    url
                                };
                                images.push(json!(b64));
                            }
                        }
                    }
                    let mut new_msg = json!({
                        "role": role,
                        "content": text_parts.join("\n"),
                    });
                    if !images.is_empty() {
                        new_msg["images"] = json!(images);
                    }
                    new_msg
                }
                _ => msg.clone(),
            }
        })
        .collect();
    json!(normalized)
}

pub(crate) fn build_ollama_request(
    body: &Value,
    model: String,
    messages: Value,
    default_ctx: u32,
) -> Value {
    let mut options = Map::new();
    for key in [
        "temperature",
        "top_p",
        "top_k",
        "frequency_penalty",
        "presence_penalty",
        "seed",
        "repeat_penalty",
        "repeat_last_n",
    ] {
        if let Some(v) = body.get(key) {
            options.insert(key.to_string(), v.clone());
        }
    }
    // SillyTavern may send repetition_penalty instead of repeat_penalty.
    if !options.contains_key("repeat_penalty") {
        if let Some(v) = body.get("repetition_penalty") {
            options.insert("repeat_penalty".to_string(), v.clone());
        }
    }
    // Loop protection: ST's custom OpenAI endpoint does not pass repeat_penalty,
    // so Ollama runs with the default 1.0 (disabled) and spams repetitions.
    if !options.contains_key("repeat_penalty") {
        options.insert("repeat_penalty".to_string(), json!(1.12));
    }
    // The penalty window was shorter than the history: recent lines were penalized while older
    // ones stayed cheap, and the model "escaped" the penalty into unpenalized
    // old text — chunks of previous messages at the end of new ones. 1024 tokens
    // cover several recent replies; more already mows down ordinary
    // words rather than repetitions.
    if !options.contains_key("repeat_last_n") {
        options.insert("repeat_last_n".to_string(), json!(1024));
    }
    // Context-emptying protection: without num_ctx Ollama uses a hard limit of 2048.
    // With long dialogues and group chats the prompt exceeds 2048 tokens, causing
    // an instant generation cut and empty messages.
    if !options.contains_key("num_ctx") {
        options.insert("num_ctx".to_string(), json!(default_ctx.max(8192)));
    }
    if !options.contains_key("temperature") {
        options.insert("temperature".to_string(), json!(0.75));
    }

    let max_tok = body
        .get("max_tokens")
        .or_else(|| body.get("max_completion_tokens"))
        .and_then(|v| v.as_i64());
    if let Some(v) = max_tok {
        options.insert("num_predict".to_string(), json!(v));
    }

    if let Some(v) = body.get("stop") {
        match v {
            Value::Array(arr) => {
                let mut stops: Vec<Value> = Vec::new();
                for s in arr {
                    if let Some(st) = s.as_str() {
                        let trimmed = st.trim_matches(|c: char| c == ' ' || c == '\t');
                        if !trimmed.is_empty()
                            && !stops.iter().any(|x| x.as_str() == Some(trimmed))
                        {
                            stops.push(json!(trimmed));
                        }
                    }
                }
                // Ollama / llama.cpp supports up to 4 stop sequences.
                stops.truncate(4);
                if !stops.is_empty() {
                    options.insert("stop".to_string(), Value::Array(stops));
                }
            }
            Value::String(s) => {
                let trimmed = s.trim_matches(|c: char| c == ' ' || c == '\t');
                if !trimmed.is_empty() {
                    options.insert("stop".to_string(), json!([trimmed]));
                }
            }
            _ => {}
        }
    }

    json!({
        "model": model,
        "messages": normalize_messages(&messages),
        "stream": true,
        "think": false,
        "options": options,
    })
}


/// An SSE chunk with a piece of the reply text.
fn content_chunk(text: &str) -> String {
    json!({
        "id": "chatcmpl-local",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": { "content": text },
            "finish_reason": null,
        }],
    })
    .to_string()
}

/// The final SSE chunk: an empty delta and the stop reason.
fn finish_chunk(reason: &str) -> String {
    json!({
        "id": "chatcmpl-local",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": reason,
        }],
    })
    .to_string()
}

// --- Prompt Trace: what actually went to the model and what it returned ---
//
// The gateway silently overrides some of ST's sampler parameters (repeat_penalty,
// num_ctx, repeat_last_n, stop-list truncation) in build_ollama_request — this is
// visible neither in the ST panel nor in its own prompt itemization (that one
// shows what ST itself sent, not what reached Ollama after the
// substitution). Until now the substitutions could only be checked by reading the source.

/// How many recent days to keep traces: without rotation the log would grow endlessly —
/// a file per reply.
const TRACE_RETENTION_DAYS: u64 = 14;

fn traces_dir() -> std::path::PathBuf {
    crate::paths::data_dir().join("traces")
}

fn unix_secs_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Trace id: unix milliseconds + a counter — sorts by time
/// as an ordinary string, no separate date library is needed for this.
fn next_trace_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{millis:013}-{seq:04}")
}

/// The daily trace folder: `unix_secs / 86400` — needs no calendar parsing.
fn trace_day_bucket(unix_secs: u64) -> u64 {
    unix_secs / 86_400
}

/// Cleanup of daily folders older than TRACE_RETENTION_DAYS — called along the way
/// when writing a new trace; we do not bring up a separate scheduler.
fn cleanup_old_traces(current_day: u64) {
    let Ok(entries) = std::fs::read_dir(traces_dir()) else { return };
    for entry in entries.flatten() {
        let Ok(day) = entry.file_name().to_string_lossy().parse::<u64>() else { continue };
        if current_day.saturating_sub(day) > TRACE_RETENTION_DAYS {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Ollama counters from the final (`done`) chunk: they show the model reload
/// (`load_duration`) and prompt cache behavior. `prompt_eval_count` in
/// Ollama 0.32 is the whole prompt, not the counted part: a reused
/// prefix is visible by `prompt_eval_duration` (measurement 25.09.2026).
fn ollama_metrics(done: &Value) -> Value {
    let mut metrics = Map::new();
    for key in ["prompt_eval_count", "prompt_eval_duration", "load_duration", "eval_count", "eval_duration"] {
        if let Some(v) = done.get(key) {
            metrics.insert(key.to_string(), v.clone());
        }
    }
    Value::Object(metrics)
}

/// Write the request trace in the background: it must neither delay the user's
/// reply nor crash the chat on a write error (disk full, etc.).
/// `metrics` — `ollama_metrics` of the final chunk; without it (error, break
/// by SpeakerGuard) — Null.
/// The line ceiling in a trace. Base64 images arrive in `messages[].images`
/// and weigh megabytes: without a clamp one vision turn left ~10 MB for
/// 14 days, and `/traces` parsed 50 such files for a 120-character preview.
const TRACE_STRING_LIMIT: usize = 2000;

/// A copy of the value with long strings clipped (dialogue text, base64
/// images). The shape and short fields are preserved — the trace remains usable
/// for analysis.
fn clipped_for_trace(value: &Value) -> Value {
    match value {
        Value::String(s) if s.chars().count() > TRACE_STRING_LIMIT => {
            let kept: String = s.chars().take(TRACE_STRING_LIMIT).collect();
            Value::String(format!(
                "{kept}…[обрезано {} символов]",
                s.chars().count() - TRACE_STRING_LIMIT
            ))
        }
        Value::Array(items) => Value::Array(items.iter().map(clipped_for_trace).collect()),
        Value::Object(map) => {
            Value::Object(map.iter().map(|(k, v)| (k.clone(), clipped_for_trace(v))).collect())
        }
        other => other.clone(),
    }
}

fn spawn_trace_write(ollama_body: Value, reply: String, finish_reason: String, metrics: Value) {
    tauri::async_runtime::spawn(async move {
        let now = unix_secs_now();
        let day = trace_day_bucket(now);
        let dir = traces_dir().join(day.to_string());
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let id = next_trace_id();
        let trace = json!({
            "id": id,
            "unix_secs": now,
            "model": ollama_body.get("model").cloned().unwrap_or(Value::Null),
            "request_messages": ollama_body
                .get("messages")
                .map(clipped_for_trace)
                .unwrap_or(Value::Null),
            "options": ollama_body.get("options").cloned().unwrap_or(Value::Null),
            "reply": reply,
            "finish_reason": finish_reason,
            "ollama_metrics": metrics,
        });
        if let Ok(text) = serde_json::to_string_pretty(&trace) {
            let _ = std::fs::write(dir.join(format!("{id}.json")), text);
        }
        cleanup_old_traces(day);
    });
}

/// Sync part of `traces_report` (V-17): scans the day buckets and reads up to
/// 50 full trace files — directory/file IO belongs on a blocking thread.
fn traces_report_rows() -> Vec<Value> {
    let mut days: Vec<u64> = std::fs::read_dir(traces_dir())
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| e.file_name().to_string_lossy().parse::<u64>().ok())
                .collect()
        })
        .unwrap_or_default();
    days.sort_unstable_by(|a, b| b.cmp(a));

    let mut items: Vec<Value> = Vec::new();
    'days: for day in days {
        let dir = traces_dir().join(day.to_string());
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut files: Vec<std::path::PathBuf> = entries.flatten().map(|e| e.path()).collect();
        files.sort_unstable_by(|a, b| b.cmp(a));
        for path in files {
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
            items.push(json!({
                "id": value.get("id").cloned().unwrap_or(Value::Null),
                "unix_secs": value.get("unix_secs").cloned().unwrap_or(Value::Null),
                "model": value.get("model").cloned().unwrap_or(Value::Null),
                "finish_reason": value.get("finish_reason").cloned().unwrap_or(Value::Null),
                "reply_preview": value.get("reply")
                    .and_then(|v| v.as_str())
                    .map(|s| s.chars().take(120).collect::<String>()),
            }));
            if items.len() >= 50 {
                break 'days;
            }
        }
    }
    items
}

/// GET /api/cloufam/traces — the latest traces (without message text, a
/// summary is enough for the list).
/// Runs on an axum async-runtime worker; the file scan is moved to
/// `spawn_blocking` (V-17).
async fn traces_report() -> Response {
    match tauri::async_runtime::spawn_blocking(traces_report_rows).await {
        Ok(items) => Json(json!({ "traces": items })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("traces read failed: {e}")).into_response(),
    }
}

/// Sync part of `trace_detail` (V-17): scans the day buckets for the trace file.
fn trace_detail_value(id: String) -> Option<Value> {
    let days = std::fs::read_dir(traces_dir()).ok()?;
    for entry in days.flatten() {
        let path = entry.path().join(format!("{id}.json"));
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(value) = serde_json::from_str::<Value>(&text) {
                return Some(value);
            }
        }
    }
    None
}

/// GET /api/cloufam/traces/detail?id=<id> — the full trace: the exact messages
/// and options that went to Ollama, and the full reply.
/// Runs on an axum async-runtime worker; the file scan is moved to
/// `spawn_blocking` (V-17).
async fn trace_detail(uri: axum::http::Uri) -> Response {
    let id = uri
        .query()
        .and_then(|q| q.split('&').find(|p| p.starts_with("id=")))
        .map(|p| p[3..].to_string())
        .unwrap_or_default();
    if id.is_empty() {
        return (StatusCode::BAD_REQUEST, "id required").into_response();
    }
    // The id format is fixed (`next_trace_id`: digits and a hyphen), so
    // we check with an allowlist, not a denylist: a denylist let through Windows specifics
    // like `C:config.json` — `Path::join` with such an argument drops the base
    // entirely and reads a file outside the traces directory.
    let valid_id = id.len() <= 32 && id.chars().all(|c| c.is_ascii_digit() || c == '-');
    if !valid_id {
        return (StatusCode::BAD_REQUEST, "invalid id").into_response();
    }
    match tauri::async_runtime::spawn_blocking(move || trace_detail_value(id)).await {
        Ok(Some(value)) => Json(value).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "trace not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("trace read failed: {e}")).into_response(),
    }
}

async fn chat_completions(
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<Value>,
) -> Result<Response, (StatusCode, String)> {
    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if model.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "model не указан".into()));
    }
    let messages = body.get("messages").cloned().unwrap_or_else(|| json!([]));
    let generation_type = generation_type(&body);
    // ST service calls (Summarize, embeddings, image captions) go into the same
    // `/v1/chat/completions`: they must get neither roleplay notes nor background
    // state evaluation.
    let roleplay = is_roleplay(&messages, generation_type);
    // Continue/impersonate continue a line already started: its actions are not
    // "already happened", the repetition ban turned the continuation into a new scene.
    let continuation = matches!(generation_type, Some("continue" | "impersonate"));
    let speaker = active_avatar().map(|avatar| crate::traits::display_name(&avatar));
    let want_stream = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    // Load the traits store once per request: both live_state_note and
    // the background state evaluation (evaluate_state) use the same snapshot.
    let traits_store = crate::traits::load();
    // We pin (avatar, persona) at request start: by done/stop in ST the active card could
    // already have changed, and the evaluation would go to a different pair.
    let eval_avatar = roleplay_avatar(roleplay && evaluates_reply(generation_type, want_stream));
    let eval_persona = active_persona();
    let exchange = exchange_key(&messages, body.get("lk_chat_id").and_then(|v| v.as_str()));
    let last_user_text = last_user_message_text(&messages);
    let notes = roleplay_notes(
        roleplay,
        &last_user_text,
        vec![
            live_state_note(&traits_store).await,
            repetition_note_for(&traits_store, &messages, continuation, speaker.as_deref()),
        ],
    );
    let messages = if notes.is_empty() {
        messages
    } else {
        with_state_note(messages, &notes.join("\n\n"))
    };
    let ollama_port = TARGET_OLLAMA_PORT.load(Ordering::Relaxed);

    // The chat context — for whatever model ST sent: it trims history
    // to it (max_context), otherwise the prompt would be cut already in Ollama.
    let default_ctx = chat_num_ctx(&state.app);

    let max_tokens_limit = body
        .get("max_tokens")
        .or_else(|| body.get("max_completion_tokens"))
        .and_then(|v| v.as_i64());

    // The prompt must fit into num_ctx entirely — otherwise Ollama silently cuts it
    // from the head (together with the system prompt), and the reply becomes a repeat
    // of previous messages. The reserve is for the reply itself; its contract is `prompt_budget`
    // (the reserve cannot eat the whole context, V-15).
    let messages = checked_prompt_messages(messages, prompt_budget(default_ctx, max_tokens_limit))
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    let mut ollama_body = build_ollama_request(&body, model.clone(), messages, default_ctx);
    let resp = state
        .client
        .post(format!("http://127.0.0.1:{ollama_port}/api/chat"))
        .json(&ollama_body)
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("Ollama недоступен: {e}")))?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("Ollama: HTTP {status} {text}"),
        ));
    }

    // A stream of Ollama NDJSON lines with chunk-boundary buffering. The error slot
    // is shared by the first and the retry attempt: a transport break is not `done`.
    let stream_error: Arc<std::sync::Mutex<Option<String>>> =
        Arc::new(std::sync::Mutex::new(None));
    let mut line_stream: std::pin::Pin<Box<dyn futures_util::Stream<Item = String> + Send>> =
        Box::pin(lines_from_bytes(Box::pin(resp.bytes_stream()), stream_error.clone()));

    // A reply that verbatim repeats a line from the history is exactly "the character
    // repeats previous messages". The copy is visible in the first words, and while the model
    // has not typed even a hundred characters, it is cheap to regenerate without the copied
    // message: there is nothing left to copy (verified by the bench
    // `lk-repeat-check.mjs`: the copy went away from 13 words to 1). The second attempt goes
    // as is, without a new check.
    let mut head: Vec<String> = Vec::new();
    let mut head_text = String::new();
    while head_text.chars().count() < COPY_HEAD_CHARS {
        let Some(line) = line_stream.next().await else {
            break;
        };
        if let Ok(value) = serde_json::from_str::<Value>(&line) {
            if let Some(content) = value["message"]["content"].as_str() {
                head_text.push_str(content);
            }
        }
        head.push(line);
    }
    let sent_messages = ollama_body.get("messages").cloned().unwrap_or(Value::Null);
    if !continuation {
        if let Some(index) = crate::repetition::copied_from_with_speaker(
            &head_text,
            &sent_messages,
            speaker.as_deref(),
        ) {
            let mut retry_messages = sent_messages;
            if let Some(list) = retry_messages.as_array_mut() {
                if index < list.len() {
                    list.remove(index);
                }
            }
            let retry_body = build_ollama_request(&body, model.clone(), retry_messages, default_ctx);
            let retry_resp = state
                .client
                .post(format!("http://127.0.0.1:{ollama_port}/api/chat"))
                .json(&retry_body)
                .send()
                .await;
            if let Ok(resp) = retry_resp {
                if resp.status().is_success() {
                    ollama_body = retry_body;
                    line_stream = Box::pin(lines_from_bytes(
                        Box::pin(resp.bytes_stream()),
                        stream_error.clone(),
                    ));
                    head.clear();
                }
            }
        }
    }
    let line_stream = stream::iter(head).chain(line_stream);

    // The clone is needed in full: the prompt trace writes exactly what actually went to
    // Ollama (model/messages/options AFTER the gateway substitutions above), not what the
    // client sent — that is already visible in the ST logs, while the substitutions are nowhere.
    let trace_request = ollama_body.clone();

    if want_stream {
        let guard = Arc::new(std::sync::Mutex::new(SpeakerGuard::new(
            ollama_body.get("messages").unwrap_or(&Value::Null),
        )));
        // Tail echo guard: a verbatim copy of previous messages at the end of the reply
        // is cut at the moment of detection (we do not cut continuations of the line — the same
        // skip as in the "head" check above).
        let echo_guard = Arc::new(std::sync::Mutex::new(if continuation {
            crate::repetition::EchoGuard::inactive()
        } else {
            crate::repetition::EchoGuard::new(
                ollama_body.get("messages").unwrap_or(&Value::Null),
                speaker.as_deref(),
            )
        }));
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let guard_for_lines = guard.clone();
        let finished_for_lines = finished.clone();
        let guard_for_tail = guard.clone();
        let finished_for_tail = finished.clone();
        let finished_for_upstream = finished.clone();
        // The raw model reply — the trace; the one shown to the client (after SpeakerGuard) —
        // the state evaluation: the raw one with a foreign name prefix distorts the exchange evaluation.
        let reply_raw = Arc::new(std::sync::Mutex::new(String::new()));
        let reply_shown = Arc::new(std::sync::Mutex::new(String::new()));
        let reply_raw_for_lines = reply_raw.clone();
        let reply_shown_for_lines = reply_shown.clone();
        let state_for_eval = state.client.clone();
        let traits_for_eval = traits_store;
        let trace_for_lines = trace_request.clone();

        // After [DONE] we do not read the upstream further: the stream ends, the Ollama reply
        // is dropped together with the connection, and generation stops. Otherwise
        // after a break by SpeakerGuard the model wrote to num_predict into the void.
        let line_stream = line_stream.take_while(move |_| {
            std::future::ready(!finished_for_upstream.load(Ordering::Relaxed))
        });
        let echo_for_lines = echo_guard.clone();
        let echo_for_tail = echo_guard.clone();
        let events = line_stream.flat_map(move |line| {
            let mut events = Vec::new();
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                return stream::iter(events);
            };
            let Ok(mut guard) = guard_for_lines.lock() else {
                return stream::iter(events);
            };
            let Ok(mut echo) = echo_for_lines.lock() else {
                return stream::iter(events);
            };
            if guard.is_stopped() {
                return stream::iter(events);
            }
            if let Some(err) = value["error"].as_str() {
                // The error arrives instead of `done`: without the final flush the echo guard
                // never releases the last words, and SpeakerGuard — the undercut tail.
                let tail = if guard.is_stopped() {
                    String::new()
                } else {
                    guard.flush()
                };
                let (shown, _) = echo_absorb(&mut echo, &tail, true);
                if !shown.is_empty() {
                    if let Ok(mut buffer) = reply_shown_for_lines.lock() {
                        buffer.push_str(&shown);
                    }
                    events.push(Ok::<Event, Infallible>(
                        Event::default().data(content_chunk(&shown)),
                    ));
                }
                let chunk = json!({
                    "id": "chatcmpl-local",
                    "object": "chat.completion.chunk",
                    "choices": [{
                        "index": 0,
                        "delta": { "content": format!("\n[Ошибка Ollama: {err}]\n") },
                        "finish_reason": "stop",
                    }],
                    "error": {
                        "message": format!("Ollama: {err}"),
                        "type": "ollama_error"
                    }
                })
                .to_string();
                events.push(Ok::<Event, Infallible>(Event::default().data(chunk)));
                events.push(Ok::<Event, Infallible>(Event::default().data("[DONE]")));
                finished_for_lines.store(true, Ordering::Relaxed);
                // An Ollama error is exactly the case the trace is looked at for;
                // without this it stays silent precisely when analyzing failures.
                {
                    let reply = reply_raw_for_lines.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    spawn_trace_write(trace_for_lines.clone(), reply, format!("error: {err}"), Value::Null);
                }
            } else if value["done"].as_bool().unwrap_or(false) {
                let tail = guard.flush();
                // The SpeakerGuard tail passes the echo check, then everything held
                // by the echo guard is released: a copy at the very end is cut here too.
                let (shown_tail, echo_cut) = echo_absorb(&mut echo, &tail, true);
                if !shown_tail.is_empty() {
                    if let Ok(mut buffer) = reply_shown_for_lines.lock() {
                        buffer.push_str(&shown_tail);
                    }
                    events.push(Ok::<Event, Infallible>(
                        Event::default().data(content_chunk(&shown_tail)),
                    ));
                }
                // We start evaluating the lines' effect on state after the reply and in the
                // background: it must neither delay generation nor break the stream.
                if let Some(avatar) = eval_avatar.clone() {
                    let shown = reply_shown_for_lines.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    spawn_evaluation(
                        state_for_eval.clone(),
                        avatar,
                        eval_persona.clone(),
                        last_user_text.clone(),
                        shown,
                        traits_for_eval.clone(),
                        exchange,
                    );
                }
                let finish = if let Some(reason) = value["done_reason"].as_str() {
                    match reason {
                        "length" => "length",
                        "stop" => "stop",
                        other => other,
                    }
                } else if let (Some(eval_count), Some(limit)) =
                    (value["eval_count"].as_i64(), max_tokens_limit)
                {
                    if eval_count >= limit {
                        "length"
                    } else {
                        "stop"
                    }
                } else {
                    "stop"
                };
                // The echo cut is a trace detail: the client gets the regular reason.
                let finish = if echo_cut { "echo_guard" } else { finish };
                let client_finish = if finish == "echo_guard" { "stop" } else { finish };
                {
                    let reply = reply_raw_for_lines.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    spawn_trace_write(trace_for_lines.clone(), reply, finish.to_string(), ollama_metrics(&value));
                }
                events.push(Ok::<Event, Infallible>(
                    Event::default().data(finish_chunk(client_finish)),
                ));
                events.push(Ok::<Event, Infallible>(Event::default().data("[DONE]")));
                finished_for_lines.store(true, Ordering::Relaxed);
            } else {
                let content = value["message"]["content"].as_str().unwrap_or_default();
                let forwarded = guard.feed(content);
                if let Ok(mut buffer) = reply_raw_for_lines.lock() {
                    buffer.push_str(content);
                }
                // The shown text goes through the echo guard: the last words
                // are held back until it is clear that no copy has started. A stop
                // on another speaker is the end of generation, the held text must be released.
                let (emit, echo_cut) = echo_absorb(&mut echo, &forwarded, guard.is_stopped());
                if !emit.is_empty() {
                    if let Ok(mut buffer) = reply_shown_for_lines.lock() {
                        buffer.push_str(&emit);
                    }
                    events.push(Ok::<Event, Infallible>(
                        Event::default().data(content_chunk(&emit)),
                    ));
                }
                if guard.is_stopped() || echo_cut {
                    events.push(Ok::<Event, Infallible>(
                        Event::default().data(finish_chunk("stop")),
                    ));
                    events.push(Ok::<Event, Infallible>(Event::default().data("[DONE]")));
                    finished_for_lines.store(true, Ordering::Relaxed);
                    {
                        let reply = reply_raw_for_lines.lock().unwrap_or_else(|e| e.into_inner()).clone();
                        let reason = if echo_cut { "echo_guard" } else { "stop" };
                        spawn_trace_write(trace_for_lines.clone(), reply, reason.to_string(), Value::Null);
                    }
                    // State evaluation on a break by the guard — same as the done branch
                    if let Some(avatar) = eval_avatar.clone() {
                        let shown = reply_shown_for_lines.lock().unwrap_or_else(|e| e.into_inner()).clone();
                        spawn_evaluation(
                            state_for_eval.clone(),
                            avatar,
                            eval_persona.clone(),
                            last_user_text.clone(),
                            shown,
                            traits_for_eval.clone(),
                            exchange,
                        );
                    }
                }
            }
            stream::iter(events)
        });

        // End of the stream without `done` (connection break, Ollama crash) previously
        // silently lost the undelivered tail and did not send [DONE]: ST committed the truncated
        // reply as finished. Now the break reason is at least visible in
        // boot.log and the trace — before, this case was diagnosed nowhere.
        let trace_for_tail = trace_request.clone();
        let reply_for_tail = reply_raw.clone();
        let error_for_tail = stream_error.clone();
        let tail = stream::once(async move {
            let mut events = Vec::new();
            if finished_for_tail.load(Ordering::Relaxed) {
                return stream::iter(events);
            }
            let mut text = String::new();
            if let Ok(mut guard) = guard_for_tail.lock() {
                text = guard.flush();
            }
            // A transport break is the same final: the tail passes the echo check.
            if let Ok(mut echo) = echo_for_tail.lock() {
                let (shown, _) = echo_absorb(&mut echo, &text, true);
                text = shown;
            }
            if !text.is_empty() {
                events.push(Ok::<Event, Infallible>(
                    Event::default().data(content_chunk(&text)),
                ));
            }
            if let Some(err) = error_for_tail.lock().ok().and_then(|mut slot| slot.take()) {
                crate::boot_log(&format!("Поток Ollama оборван: {err}"));
                let reply = reply_for_tail.lock().unwrap_or_else(|e| e.into_inner()).clone();
                spawn_trace_write(trace_for_tail, reply, format!("error: {err}"), Value::Null);
            }
            events.push(Ok::<Event, Infallible>(
                Event::default().data(finish_chunk("stop")),
            ));
            events.push(Ok::<Event, Infallible>(Event::default().data("[DONE]")));
            stream::iter(events)
        });

        // flat_map already flattens nested streams, while `once` does not.
        let sse = events.chain(tail.flatten());
        Ok(Sse::new(sse).into_response())
    } else {
        // Non-stream: collect from the stream into a string.
        let mut content = String::new();
        let mut finish = "stop".to_string();
        let mut guard = SpeakerGuard::new(ollama_body.get("messages").unwrap_or(&Value::Null));
        let mut echo = if continuation {
            crate::repetition::EchoGuard::inactive()
        } else {
            crate::repetition::EchoGuard::new(
                ollama_body.get("messages").unwrap_or(&Value::Null),
                speaker.as_deref(),
            )
        };
        let mut lines = Box::pin(line_stream);
        // The trace writes the raw model reply, not what went to ST after SpeakerGuard:
        // otherwise it lies exactly in the case it is looked at for —
        // when the guard cut a foreign name prefix.
        let mut raw = String::new();
        let mut metrics = Value::Null;
        while let Some(line) = lines.next().await {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some(err) = value["error"].as_str() {
                spawn_trace_write(trace_request, raw, format!("error: {err}"), Value::Null);
                return Err((StatusCode::BAD_GATEWAY, format!("Ollama: {err}")));
            }
            if let Some(c) = value["message"]["content"].as_str() {
                raw.push_str(c);
                let forwarded = guard.feed(c);
                let (shown, echo_cut) = echo_absorb(&mut echo, &forwarded, guard.is_stopped());
                if echo_cut {
                    finish = "echo_guard".to_string();
                }
                content.push_str(&shown);
                if guard.is_stopped() || echo.is_stopped() {
                    if finish == "stop" {
                        finish = if guard.is_stopped() { "speaker_guard" } else { "echo_guard" }
                            .to_string();
                    }
                    break;
                }
            }
            if value["done"].as_bool().unwrap_or(false) {
                metrics = ollama_metrics(&value);
                if let Some(reason) = value["done_reason"].as_str() {
                    finish = match reason {
                        "length" => "length".to_string(),
                        "stop" => "stop".to_string(),
                        other => other.to_string(),
                    };
                } else if let (Some(eval_count), Some(limit)) =
                    (value["eval_count"].as_i64(), max_tokens_limit)
                {
                    if eval_count >= limit {
                        finish = "length".to_string();
                    } else {
                        finish = "stop".to_string();
                    }
                } else {
                    finish = "stop".to_string();
                }
            }
        }
        // A transport break is not a clean end: without this check
        // an unfinished reply silently went to the caller as complete.
        if let Some(err) = stream_error.lock().ok().and_then(|mut slot| slot.take()) {
            crate::boot_log(&format!("Поток Ollama оборван: {err}"));
            spawn_trace_write(trace_request, raw, format!("error: {err}"), Value::Null);
            return Err((StatusCode::BAD_GATEWAY, format!("Поток Ollama оборван: {err}")));
        }
        if !guard.is_stopped() {
            let (shown, echo_cut) = echo_absorb(&mut echo, &guard.flush(), true);
            if echo_cut {
                finish = "echo_guard".to_string();
            }
            content.push_str(&shown);
        }
        spawn_trace_write(trace_request, raw, finish.clone(), metrics);
        // State evaluation — as in the stream: by the shown line, in the background.
        if let Some(avatar) = eval_avatar {
            spawn_evaluation(
                state.client.clone(),
                avatar,
                eval_persona,
                last_user_text,
                content.clone(),
                traits_store,
                exchange,
            );
        }
        // The break reason is given to the client as the regular one: "speaker_guard" and
        // "echo_guard" are trace details, ST does not know such values.
        let finish = if finish == "speaker_guard" || finish == "echo_guard" {
            "stop".to_string()
        } else {
            finish
        };
        Ok(Json(json!({
            "id": "chatcmpl-local",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": content },
                "finish_reason": finish,
            }],
        }))
        .into_response())
    }
}

/// A stream of NDJSON lines from a byte stream: buffers raw bytes up to the newline
/// \n so that multi-byte UTF-8 sequences (Russian letters) are not split
/// at network chunk boundaries. Used for both modes (stream and non-stream).
///
/// Two stream-contract violations, each caught as a panic in the gateway:
/// `unfold` itself panics when polled after `Ready(None)`, and the reply-repeat
/// check reads the stream to the end and then still plugs it into
/// `chain` — a short reply (less than `COPY_HEAD_CHARS`) looped this polling.
/// `.fuse()` stops the repeated poll. The second flag: having given the rest of the buffer as
/// the last line, the source is exhausted, and without it the next poll would poke the
/// `None` source a second time.
pub(crate) fn lines_from_bytes<S, E>(
    source: S,
    error_slot: Arc<std::sync::Mutex<Option<String>>>,
) -> impl futures_util::Stream<Item = String>
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, E>> + Unpin,
    E: std::fmt::Debug,
{
    stream::unfold((source, bytes::BytesMut::new(), false), move |(mut source, mut buffer, drained)| {
        let error_slot = error_slot.clone();
        async move {
        loop {
            if drained {
                return None;
            }
            if let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
                let raw = buffer.split_to(pos + 1);
                let valid = match std::str::from_utf8(&raw) {
                    Ok(s) => s,
                    Err(e) => std::str::from_utf8(&raw[..e.valid_up_to()]).unwrap_or_default(),
                };
                let line = valid.trim().to_string();
                if !line.is_empty() {
                    return Some((line, (source, buffer, drained)));
                }
                continue;
            }
            match source.next().await {
                Some(Ok(chunk)) => {
                    buffer.extend_from_slice(&chunk);
                }
                other => {
                    // A transport error is not the end of the reply: we remember it
                    // so the stream tail can tell a break from a clean `done` and
                    // record the reason in boot.log and the trace.
                    if let Some(Err(e)) = other {
                        if let Ok(mut slot) = error_slot.lock() {
                            *slot = Some(format!("{e:?}"));
                        }
                    }
                    if !buffer.is_empty() {
                        let valid = match std::str::from_utf8(&buffer) {
                            Ok(s) => s,
                            Err(e) => std::str::from_utf8(&buffer[..e.valid_up_to()]).unwrap_or_default(),
                        };
                        let line = valid.trim().to_string();
                        if !line.is_empty() {
                            return Some((line, (source, bytes::BytesMut::new(), true)));
                        }
                    }
                    return None;
                }
            }
        }
        }
    })
    .fuse()
}

/// GET /v1/models — returns the model list in the OpenAI format.
/// SillyTavern calls it when checking the connection ("Connect")
/// and when picking a model from the dropdown.
async fn list_models(State(state): State<Arc<GatewayState>>) -> Result<Response, (StatusCode, String)> {
    let ollama_port = TARGET_OLLAMA_PORT.load(Ordering::Relaxed);
    let resp = state
        .client
        .get(format!("http://127.0.0.1:{ollama_port}/api/tags"))
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("Ollama недоступен: {e}")))?;
    let body: Value = resp.json().await.unwrap_or_else(|_| json!({}));
    let raw_models = body["models"].as_array().cloned().unwrap_or_default();
    let models: Vec<Value> = raw_models
        .into_iter()
        .filter_map(|m| {
            let id = m["name"].as_str()?;
            Some(json!({ "id": id, "object": "model", "owned_by": "ollama" }))
        })
        .collect();
    Ok(Json(json!({ "object": "list", "data": models })).into_response())
}

/// A snapshot of app settings without holding the lock across await points. The lock
/// is blocking: it is held only for a clone or a short edit with save(), while
/// try_lock on a busy lock silently gave "no settings" — an empty evaluator
/// model with no line in boot.log, 503 from the director and the tagger.
fn app_settings(app: &tauri::AppHandle) -> Option<crate::settings::Settings> {
    use tauri::Manager;
    app.try_state::<Arc<crate::AppState>>()
        .map(|s| s.settings.lock().unwrap_or_else(|e| e.into_inner()).clone())
}

/// num_ctx for all models except the chat model: their prompts are short.
const AUX_CONTEXT: u32 = 8192;

/// The chat context: requested in settings, capped by the video memory ceiling.
/// Without app state — a safe minimum: 32768 inflated the KV cache and
/// pushed the model onto the CPU.
pub(crate) fn chat_num_ctx(app: &tauri::AppHandle) -> u32 {
    use tauri::Manager;
    let Some(state) = app.try_state::<Arc<crate::AppState>>() else {
        return AUX_CONTEXT;
    };
    // The lock is blocking: with try_lock, busy settings gave 8192 instead of
    // context_size, and the request went to Ollama with a different num_ctx.
    let requested = state.settings.lock().unwrap_or_else(|e| e.into_inner()).context_size;
    crate::models::chat_context(requested, state.context_ceiling.load(Ordering::Relaxed))
}

/// num_ctx of a request to the model. Ollama keeps the model under the num_ctx with which
/// it loaded it: any other number means a reload (~3 s for 14B) and a lost
/// prompt cache. So a model has one number for all calls: the chat model gets the chat
/// context in the evaluator too, and in battle, the director, tags, draft parsing.
fn num_ctx_for(model: &str, chat_model: &str, chat_ctx: u32) -> u32 {
    if crate::models::is_model_installed(&[chat_model.to_string()], model) {
        chat_ctx
    } else {
        AUX_CONTEXT
    }
}

/// `num_ctx_for` by the live app settings.
fn live_num_ctx(app: &tauri::AppHandle, model: &str) -> u32 {
    let chat_model = app_settings(app).map(|s| s.chat_model).unwrap_or_default();
    num_ctx_for(model, &chat_model, chat_num_ctx(app))
}

/// The body of an auxiliary model request: one prompt, no streaming and no thinking.
fn aux_chat_body(model: &str, prompt: &str, num_ctx: u32, mut options: Value) -> Value {
    options["num_ctx"] = json!(num_ctx);
    json!({
        "model": model,
        "messages": [{ "role": "user", "content": prompt }],
        "stream": false,
        "think": false,
        "options": options,
    })
}

/// Add user instructions to the built-in prompt — the same way as
/// `tagger::build_prompt` with `tagger_prompt`: the contract (JSON,
/// limits, format) in the prompt stays untouched, the user can
/// only append on top, without breaking the reply parsing.
fn with_custom_instructions(prompt: String, custom: &str) -> String {
    let custom = custom.trim();
    if custom.is_empty() {
        prompt
    } else {
        format!("{prompt}\n\nДополнительные указания пользователя:\n{custom}")
    }
}

/// One quiet model call for RPG bookkeeping (quest progress, reward weights).
/// Fail closed: `None` on any transport/HTTP/shape error with the reason in
/// boot.log, and the caller keeps the current state. Run from a background
/// task (`evaluate_state`) — the call takes seconds.
async fn aux_model_reply(
    client: &reqwest::Client,
    model: &str,
    prompt: &str,
    label: &str,
) -> Option<String> {
    let port = TARGET_OLLAMA_PORT.load(Ordering::Relaxed);
    let num_ctx = APP_HANDLE.get().map_or(AUX_CONTEXT, |app| live_num_ctx(app, model));
    let body = aux_chat_body(model, prompt, num_ctx, json!({ "temperature": 0.2, "num_predict": 300 }));
    let resp = match client
        .post(format!("http://127.0.0.1:{port}/api/chat"))
        .json(&body)
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => resp,
        Ok(resp) => {
            crate::boot_log(&format!(
                "{label}: Ollama HTTP {} (модель {model})",
                resp.status().as_u16()
            ));
            return None;
        }
        Err(e) => {
            crate::boot_log(&format!("{label} не удалось ({model}): {e}"));
            return None;
        }
    };
    match resp.json::<Value>().await {
        Ok(value) => Some(value["message"]["content"].as_str().unwrap_or_default().to_string()),
        Err(e) => {
            crate::boot_log(&format!("{label}: ответ не разобран ({model}): {e}"));
            None
        }
    }
}

async fn ollama_model_names(state: &Arc<GatewayState>) -> Vec<String> {
    let port = TARGET_OLLAMA_PORT.load(Ordering::Relaxed);
    let Ok(resp) = state
        .client
        .get(format!("http://127.0.0.1:{port}/api/tags"))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    else {
        return Vec::new();
    };
    let Ok(body) = resp.json::<Value>().await else {
        return Vec::new();
    };
    body["models"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m["name"].as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn effective_tagger_model(settings: &crate::settings::Settings) -> String {
    let custom = settings.tagger_model.trim();
    if custom.is_empty() {
        settings.chat_model.clone()
    } else {
        custom.to_string()
    }
}

fn tagger_config_json(settings: &crate::settings::Settings, models: &[String]) -> Value {
    json!({
        "enabled": settings.tagger_enabled,
        "model": effective_tagger_model(settings),
        "tagger_model": settings.tagger_model,
        "max_tags": settings.tagger_max_tags,
        "language": settings.tagger_language,
        "write_card_tags": settings.tagger_write_card_tags,
        "auto_translate": settings.auto_translate_imports,
        "prompt": settings.tagger_prompt,
        "models": models,
    })
}

/// GET /api/cloufam/tagger-config — auto-tag settings for the ST extension.
async fn tagger_config(State(state): State<Arc<GatewayState>>) -> Response {
    let Some(settings) = app_settings(&state.app) else {
        return (StatusCode::SERVICE_UNAVAILABLE, "Настройки приложения недоступны").into_response();
    };
    let models = ollama_model_names(&state).await;
    Json(tagger_config_json(&settings, &models)).into_response()
}

/// Sync part of `traits_map` (V-17): the four store/catalog reads below hit
/// the disk on every request.
fn traits_map_rows() -> std::collections::BTreeMap<String, String> {
    card_notes(
        &crate::traits::load(),
        &crate::bonds::load(),
        &crate::traits::st_tagged_characters().into_iter().collect(),
        &crate::traits::character_list()
            .into_iter()
            .map(|(avatar, _)| avatar)
            .collect::<Vec<_>>(),
    )
}

/// GET /api/cloufam/traits — the character card note per character: avatar → text
/// for `depth_prompt`. The extension in SillyTavern compares it with the card and
/// appends it if it diverged. For characters with tags but no configured
/// traits, a default fragment is generated (all tags at 50%).
/// Runs on an axum async-runtime worker; the store reads are moved to
/// `spawn_blocking` (V-17).
async fn traits_map() -> Response {
    match tauri::async_runtime::spawn_blocking(traits_map_rows).await {
        Ok(notes) => Json(notes).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("traits read failed: {e}")).into_response(),
    }
}

/// A note for every catalog card. Parsed separately from the route so that the
/// check binds the stores to the text rather than depending on which
/// cards are in the installed SillyTavern.
fn card_notes(
    store: &crate::traits::Store,
    bond_store: &crate::bonds::Store,
    tagged: &std::collections::HashMap<String, Vec<String>>,
    avatars: &[String],
) -> std::collections::BTreeMap<String, String> {
    avatars
        .iter()
        .map(|avatar| {
            let entry = store.get(avatar).cloned().unwrap_or_default();
            let tags = tagged.get(avatar).cloned().unwrap_or_default();
            let saved_bonds = bond_store.get(avatar).cloned().unwrap_or_default();
            // Every catalog card gets a note, not only the hand-configured
            // ones: it holds the repetition ban, tag proportions, and bonds.
            (avatar.clone(), crate::card_text_for(&entry, &tags, &saved_bonds))
        })
        .collect()
}

#[derive(Deserialize)]
struct ActiveCharacterBody {
    avatar: String,
    /// The user's persona name: a character has its own attitude to each interlocutor.
    #[serde(default)]
    persona: String,
}

/// Trimmed avatar key from the ST extension, validated like manual trait
/// saves (V-32): the key goes into `traits.json` and prompt notes, so path
/// separators, ".." and absurd lengths are refused on this write path too —
/// `check_avatar` (the same validator `traits_save` uses) is private to the
/// crate root but visible here as a descendant module. An empty key is
/// legitimate: the extension clears the active character when no card is open.
fn checked_avatar_key(raw: &str) -> Result<String, (StatusCode, String)> {
    let avatar = raw.trim().to_string();
    if !avatar.is_empty() {
        crate::check_avatar(&avatar).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    }
    Ok(avatar)
}

/// POST /api/cloufam/active-character — the extension reports who the chat is with:
/// by this name the gateway mixes the character's live state into the prompt.
/// Runs on an axum async-runtime worker; the state update is a pair of short
/// std::Mutex critical sections (blocking locks, never held across an await).
async fn set_active_character(
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<ActiveCharacterBody>,
) -> Response {
    use tauri::Manager;
    // Validate on the write path (V-32): the state evaluator opens a
    // traits.json entry under this key, and manual trait saves validate theirs.
    let avatar = match checked_avatar_key(&body.avatar) {
        Ok(avatar) => avatar,
        Err((status, e)) => return (status, e).into_response(),
    };
    let mut card_changed = false;
    if let Some(app) = state.app.try_state::<Arc<crate::AppState>>() {
        let mut active = app.active_character.lock().unwrap_or_else(|e| e.into_inner());
        // The extension re-reports the active card every 30 s (heartbeat), so
        // "a new session" is a card *change*, not every report (V-41).
        card_changed = !avatar.is_empty() && *active != avatar;
        *active = avatar;
        *app.active_persona.lock().unwrap_or_else(|e| e.into_inner()) = body.persona.trim().to_string();
    }
    if card_changed {
        // Session start for a new card (V-41): relationships cool down by 0.5
        // per pass — one pass per session, the same decay idea as
        // `traits::evolve`; without this call they only ever grew.
        let path = crate::paths::data_dir().join("relationships.json");
        if let Err(e) = crate::relationships::RelationshipStore::update(&path, |store| {
            store.session_decay();
        }) {
            crate::boot_log(&format!("Отношения: {e}"));
        }
        // Tag weights cool down over a session too (V-40) — same trigger. The
        // store write is disk IO, so it goes to a blocking thread (V-17).
        tauri::async_runtime::spawn_blocking(|| {
            if let Err(e) = crate::character_evolution::decay_session() {
                crate::boot_log(&format!("Эволюция тегов: {e}"));
            }
        });
        invalidate_rpg_cache().await;
    }
    Json(json!({ "ok": true })).into_response()
}

/// Sync part of `tag_influence_report` (V-17): tag-map parsing plus the O(n²)
/// similarity computation across all characters — CPU work for a blocking
/// thread, not an async-runtime worker. The response shape is unchanged: the
/// report array, or an `{"error": ...}` object when the tag map is missing.
fn tag_influence_report_value() -> Value {
    let config = crate::tag_influence::TagInfluenceConfig::default();

    let dt = crate::sillytavern::bundled_tags();

    let Some(tag_map) = dt.get("tag_map").and_then(|v| v.as_object()) else {
        return json!({ "error": "tag_map not found" });
    };
    let Some(tags_arr) = dt.get("tags").and_then(|v| v.as_array()) else {
        return json!({ "error": "tags not found" });
    };

    let id_to_name: std::collections::HashMap<&str, &str> = tags_arr
        .iter()
        .filter_map(|t| {
            let id = t.get("id")?.as_str()?;
            let name = t.get("name")?.as_str()?;
            Some((id, name))
        })
        .collect();

    let mut characters: Vec<(String, String, Vec<String>)> = Vec::new();
    for (avatar, tag_ids) in tag_map {
        let tags: Vec<String> = tag_ids
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|id| id.as_str())
                    .filter_map(|id| id_to_name.get(id).map(|n| n.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        // The same helper as every other display name (V-14): the hand-rolled
        // `replace(".png", "")` was case-sensitive and cut ".png" at any
        // position, unlike `traits::display_name`.
        let name = crate::traits::display_name(avatar);
        characters.push((avatar.clone(), name, tags));
    }

    let cache = crate::tag_influence::compute_all(&characters, &config.weights, config.top_n);

    // Build the full report: every character → top-N similar + tags with weights
    let reports: Vec<crate::tag_influence::InfluenceReport> = characters
        .iter()
        .map(|(a, n, t)| crate::tag_influence::get_report(a, n, t, &cache, &config))
        .collect();

    json!(reports)
}

/// GET /api/cloufam/tag-influence — a similarity report for all characters based on tags.
/// Runs on an axum async-runtime worker; the compute below is moved to
/// `spawn_blocking` (V-17).
async fn tag_influence_report() -> Response {
    match tauri::async_runtime::spawn_blocking(tag_influence_report_value).await {
        Ok(value) => Json(value).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("tag influence failed: {e}")).into_response(),
    }
}

/// GET /api/cloufam/character-evolution — the current character tag weights.
/// Runs on an axum async-runtime worker; the store read goes to a blocking
/// thread (V-17), the path comes from the module itself (single source).
async fn character_evolution_report() -> Response {
    match tauri::async_runtime::spawn_blocking(|| {
        let path = crate::character_evolution::store_path();
        crate::character_evolution::CharacterEvolution::load(&path)
    })
    .await
    {
        Ok(evo) => {
            // Per-character effective tags: the module's own merge
            // (`tag_weights` over `acquired_tags`, zero/NaN dropped), so the
            // panel sees exactly what prompts would use (V-40).
            let effective: std::collections::BTreeMap<String, Value> = evo
                .characters
                .keys()
                .map(|character| {
                    let tags: std::collections::BTreeMap<String, f32> =
                        evo.effective_tags(character).into_iter().collect();
                    (character.clone(), json!(tags))
                })
                .collect();
            Json(json!({
                "characters": evo.characters.len(),
                "data": evo.characters,
                "effective_tags": effective,
            }))
            .into_response()
        }
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("evolution read failed: {e}")).into_response()
        }
    }
}

/// GET /api/cloufam/relationships — the current relationships between characters.
async fn relationships_report() -> Response {
    let path = crate::paths::data_dir().join("relationships.json");
    let rels = crate::relationships::RelationshipStore::load(&path);
    Json(json!({
        "count": rels.relationships.len(),
        "relationships": rels.relationships,
    }))
    .into_response()
}

/// GET /api/cloufam/character-relationships?avatar=<avatar>
/// Relationships of a specific character: inter-character + towards companions.
async fn character_relationships_report(uri: axum::http::Uri) -> Response {
    let avatar = parse_avatar(&uri);
    if avatar.is_empty() {
        return (StatusCode::BAD_REQUEST, "avatar required").into_response();
    }

    // Inter-character relationships from relationships.json
    let rels_path = crate::paths::data_dir().join("relationships.json");
    let rels = crate::relationships::RelationshipStore::load(&rels_path);

    // Filter relationships for this character
    let character_name = crate::traits::display_name(&avatar);
    // `RelationshipStore::key` collapses "::" in names to ":" before joining
    // them with the "::" separator — normalize the same way, or a card named
    // "A::B" silently finds no pair.
    let character_key_name = character_name.replace("::", ":");
    let inter_character: Vec<Value> = rels
        .relationships
        .iter()
        .filter_map(|(key, &axes)| {
            let (a, b) = key.split_once("::")?;
            let target = if a == character_key_name {
                b.to_string()
            } else if b == character_key_name {
                a.to_string()
            } else {
                return None;
            };
            Some(json!({
                "target": target,
                "friendship": axes.friendship,
                "romance": axes.romance,
                "respect": axes.respect,
                "fear": axes.fear,
                "hatred": axes.hatred,
                "score": axes.score(),
                "label": axes.label(),
            }))
        })
        .collect();

    // Attitudes towards companions from traits.json
    let traits_path = crate::paths::data_dir().join("traits.json");
    let traits_store = crate::traits::load_from(&traits_path);
    let persona_relations: Value = if let Some(traits) = traits_store.get(&avatar) {
        let mut relations = serde_json::Map::new();
        for (persona, state) in &traits.relations {
            relations.insert(
                persona.clone(),
                json!({
                    "affection": state.affection,
                    "trust": state.trust,
                    "energy": state.energy,
                    "irritation": state.irritation,
                }),
            );
        }
        // Add the current state (for an anonymous interlocutor)
        relations.insert(
            "".to_string(),
            json!({
                "affection": traits.state.affection,
                "trust": traits.state.trust,
                "energy": traits.state.energy,
                "irritation": traits.state.irritation,
            }),
        );
        Value::Object(relations)
    } else {
        Value::Object(serde_json::Map::new())
    };

    // The manual layer of "what he thinks of them". The order — as in the card block, so
    // the modal and the prompt do not show different lists.
    let bonds: Vec<Value> = crate::bonds::bonds_of(&avatar)
        .into_iter()
        .map(|bond| {
            json!({
                "target": crate::traits::display_name(&bond.target),
                "kind": bond.kind,
                "strength": bond.strength,
                "note": bond.note,
                "secret": bond.secret,
            })
        })
        .collect();

    Json(json!({
        "character": character_name,
        "avatar": avatar,
        "bonds": bonds,
        "inter_character": inter_character,
        "persona_relations": persona_relations,
    }))
    .into_response()
}

/// The state line for the prompt: only if live state is enabled for the active
/// character. The card holds the settings, while the state changes every
/// line — that is why it is mixed into the request and not written to the card.
fn active_avatar() -> Option<String> {
    use tauri::Manager;
    let app = APP_HANDLE.get()?;
    let state = app.try_state::<Arc<crate::AppState>>()?;
    // Poisoned lock policy (V-13): `into_inner` keeps the last value instead of
    // silently dropping the live character from every prompt note.
    let avatar = state.active_character.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if avatar.is_empty() {
        None
    } else {
        Some(avatar)
    }
}

/// Who the interlocutor is: a character has a separate attitude to each person,
/// so the state is kept per "character + persona" pair from SillyTavern.
fn active_persona() -> String {
    use tauri::Manager;
    let Some(app) = APP_HANDLE.get() else {
        return String::new();
    };
    let Some(state) = app.try_state::<Arc<crate::AppState>>() else {
        return String::new();
    };
    // Poisoned lock policy (V-13), same as `active_avatar`. The guard is bound
    // to a local so it drops before `state` (tail-expression temporaries would
    // outlive the borrow).
    let persona = state.active_persona.lock().unwrap_or_else(|e| e.into_inner()).clone();
    persona
}

/// Avatar from the query string: `?avatar=X`. The client sends `encodeURIComponent(avatar)` —
/// without decoding, a file with a space or non-ASCII name is not found by key.
fn parse_avatar(uri: &axum::http::Uri) -> String {
    uri.query()
        .and_then(|q| {
            q.split('&')
                .find(|p| p.starts_with("avatar="))
                .map(|p| {
                    percent_encoding::percent_decode_str(&p[7..])
                        .decode_utf8_lossy()
                        .into_owned()
                })
        })
        .unwrap_or_default()
}

/// A snapshot of RPG data for the cache: all 7 stores loaded from disk.
/// Cached for 60 seconds so as not to spend ~7 disk operations on every LLM request.
type RpgSnapshot = (
    crate::rpg::world::World,
    crate::rpg::stats::StatsStore,
    crate::rpg::skills::SkillsStore,
    crate::rpg::quests::QuestStore,
    crate::rpg::inventory::InventoryStore,
    crate::rpg::npcs::NpcStore,
    crate::relationships::RelationshipStore,
);

static CACHED_RPG_SNAPSHOT: std::sync::OnceLock<
    tokio::sync::RwLock<Option<(std::time::Instant, RpgSnapshot)>>,
> = std::sync::OnceLock::new();

/// The RPG cache generation: a reset that happened between reading the files and writing the
/// snapshot must not be lost — otherwise a snapshot from before the edit would live for another 60 seconds
/// and the prompt would see data from before the evaluator's commit.
static RPG_CACHE_GEN: AtomicU64 = AtomicU64::new(0);

/// Combat changes through "read → ask the model → write", and a request to the
/// model takes tens of seconds. Without a lock two simultaneous actions
/// overwrite each other's result.
static COMBAT_ACTION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn get_rpg_cache_lock() -> &'static tokio::sync::RwLock<Option<(std::time::Instant, RpgSnapshot)>> {
    CACHED_RPG_SNAPSHOT.get_or_init(|| tokio::sync::RwLock::new(None))
}

/// RPG data with a 60-second cache. On request: if the cache is fresh — return it,
/// otherwise re-read the files.
async fn cached_rpg_snapshot() -> RpgSnapshot {
    let lock = get_rpg_cache_lock();
    {
        let guard = lock.read().await;
        if let Some((time, ref data)) = *guard {
            if time.elapsed() < std::time::Duration::from_secs(60) {
                return data.clone();
            }
        }
    }
    let gen = RPG_CACHE_GEN.load(Ordering::SeqCst);
    let fresh = (
        crate::rpg::world::load(),
        crate::rpg::stats::load(),
        crate::rpg::skills::load(),
        crate::rpg::quests::load(),
        crate::rpg::inventory::load(),
        crate::rpg::npcs::load(),
        crate::relationships::RelationshipStore::load(
            &crate::paths::data_dir().join("relationships.json"),
        ),
    );
    let mut guard = lock.write().await;
    // A reset during the read — the snapshot is stale and does not go into the cache; the caller
    // will get it this time, the next request re-reads the files.
    if RPG_CACHE_GEN.load(Ordering::SeqCst) == gen {
        *guard = Some((std::time::Instant::now(), fresh.clone()));
    }
    fresh
}

/// Reset the RPG cache: called after mutations (world/move, combat/action),
/// so the next `live_state_note` sees the current state.
async fn invalidate_rpg_cache() {
    // The generation is moved before the lock: a snapshot that read the files in parallel
    // will see the change and not write itself over the reset.
    RPG_CACHE_GEN.fetch_add(1, Ordering::SeqCst);
    if let Some(lock) = CACHED_RPG_SNAPSHOT.get() {
        // try_write silently lost the reset if someone was reading the cache at that moment.
        let mut guard = lock.write().await;
        *guard = None;
    }
}

/// The state note for the prompt — the only place where state gets into
/// generation: in the card it would go stale, because the card is written when
/// settings are saved, while the state changes every line. Computed for the active
/// character and the current interlocutor from SillyTavern.
async fn live_state_note(store: &crate::traits::Store) -> Option<String> {
    let avatar = active_avatar()?;
    let persona = active_persona();
    // The card may have no record at all: then the default values apply
    // (live, without repetitions), not "no state".
    let traits = store.get(&avatar).cloned().unwrap_or_default();

    let mut parts: Vec<String> = Vec::new();

    // The character state (tenderness, trust, energy, irritation).
    if traits.live {
        parts.push(crate::traits::state_note(
            &crate::traits::display_name(&avatar),
            &persona,
            traits.relation(&persona),
        ));
    }

    // RPG note: world, stats, skills, quests, inventory, NPCs, combat.
    let (world, stats, skills, quests, inventory, npcs, rels) = cached_rpg_snapshot().await;
    let active_combat = crate::rpg::enemies::load_combat();
    let rpg_note = crate::rpg::prompts::rpg_state_note(
        &avatar,
        &persona,
        crate::rpg::world::started().then_some(&world),
        &stats,
        &skills,
        &quests,
        &inventory,
        &npcs,
        &rels,
        active_combat.as_ref(),
    );
    if !rpg_note.is_empty() {
        parts.push(rpg_note);
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

/// The repetition ban listing what the character has already done. Respects
/// a manually disabled `no_repeat`; for a card without its own record it is on.
fn repetition_note_for(
    store: &crate::traits::Store,
    messages: &Value,
    continuation: bool,
    speaker: Option<&str>,
) -> Option<String> {
    // Continue/impersonate continue a started line — its gestures are not "already done".
    if continuation {
        return None;
    }
    let no_repeat = active_avatar()
        .and_then(|avatar| store.get(&avatar).map(|traits| traits.no_repeat))
        .unwrap_or_else(|| crate::traits::CharacterTraits::default().no_repeat);
    if !no_repeat {
        return None;
    }
    crate::repetition::repetition_note_for_speaker(messages, speaker)
}

/// The gateway handler knows the app window: the character state lives in AppState,
/// not in the gateway registry.
static APP_HANDLE: std::sync::OnceLock<tauri::AppHandle> = std::sync::OnceLock::new();

/// The roleplay generation marker — a fragment of the main prompt without macros that
/// ST sends verbatim. The test below keeps it from diverging from `ROLEPLAY_MAIN_PROMPT`.
const ROLEPLAY_MARKER: &str = "шаблонный вежливый ИИ-помощник";

/// The message text after `normalize_messages` — there content is always a string.
fn message_text(message: &Value) -> &str {
    message
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or_default()
}

/// Is this a roleplay generation or an ST service call? Memory extensions (`source:
/// "main"` in `sillytavern.rs`) hit the same `/v1/chat/completions`, and roleplay
/// notes together with the background state evaluation got into the summary — that is,
/// into the character's long-term memory, which he then reads as fact. The marker
/// cuts off only calls without the main prompt: ST quiet generations (Summarize
/// with `prompt_builder` DEFAULT) carry it, they are distinguished by `generation_type`.
fn is_roleplay_request(messages: &Value) -> bool {
    normalize_messages(messages)
        .as_array()
        .map(|list| {
            list.iter().any(|m| {
                m.get("role").and_then(|r| r.as_str()) == Some("system")
                    && message_text(m).contains(ROLEPLAY_MARKER)
            })
        })
        .unwrap_or(false)
}

/// Whether the generation is roleplay. The main signal is the generation type from the extension:
/// the main prompt marker disappears on every card with its own
/// `system_prompt` (all 63 cards of the set; ST by default replaces
/// the main prompt with it — `prefer_character_prompt`), and they had neither live
/// state, nor repetition ban, nor evaluator. Quiet — service calls
/// (Summarize, image prompt, connection test). No type or an unknown one
/// (the user's body in the YAML list, requests bypassing Generate) — the former
/// signal, the marker.
fn is_roleplay(messages: &Value, generation_type: Option<&str>) -> bool {
    match generation_type {
        Some("normal" | "regenerate" | "swipe" | "continue" | "impersonate") => true,
        Some("quiet") => false,
        _ => is_roleplay_request(messages),
    }
}

/// The character whose state should be evaluated after this reply. For
/// ST service requests (`is_roleplay` == false) — `None`: a background
/// evaluation on a summary or an image caption spins the model for nothing and records
/// a non-existent exchange of lines.
fn roleplay_avatar(roleplay: bool) -> Option<String> {
    roleplay.then(active_avatar).flatten()
}

/// ST generation type (`normal`, `regenerate`, `swipe`, `continue`, `impersonate`,
/// `quiet`). ST does not convey it to the custom endpoint: the field is added by the
/// Cloufam extension through `custom_include_body`. No field — the type is unknown.
fn generation_type(body: &Value) -> Option<&str> {
    body.get("lk_generation_type").and_then(|v| v.as_str())
}

/// Whether to evaluate the reply as a character line. Quiet generations (Summarize,
/// image prompt, third-party extensions) carry the same main prompt, while
/// impersonate and continue are not a new reply to the player's line. Without a type
/// we rely on the stream: quiet in ST is never streamed.
fn evaluates_reply(generation_type: Option<&str>, stream: bool) -> bool {
    match generation_type {
        Some(kind) => matches!(kind, "normal" | "regenerate" | "swipe"),
        None => stream,
    }
}

/// The exchange key — the dialogue (user/assistant) up to the reply. Regeneration and
/// swipe send the same history, while the next turn — already with the character's previous reply,
/// so the same "yes" on different turns does not glue together. System
/// messages are not included: summaries, lore, and notes change even without a new turn.
fn exchange_key(messages: &Value, chat_id: Option<&str>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    // A new chat with the same greeting and the same "hello" would give the same history;
    // without the chat id its first turn would be taken for a regeneration of the previous one.
    chat_id.hash(&mut hasher);
    for message in messages.as_array().into_iter().flatten() {
        let role = message.get("role").and_then(|r| r.as_str()).unwrap_or_default();
        if !matches!(role, "user" | "assistant") {
            continue;
        }
        role.hash(&mut hasher);
        match message.get("content") {
            Some(Value::String(text)) => text.hash(&mut hasher),
            Some(Value::Array(parts)) => {
                for text in parts.iter().filter_map(|part| part.get("text").and_then(|t| t.as_str())) {
                    text.hash(&mut hasher);
                }
            }
            _ => {}
        }
    }
    hasher.finish()
}

/// The text of the last user message. A turn with an image arrives as an
/// array of parts, and `.as_str()` on it gives `None` — the state evaluation silently
/// skipped every turn with an image. The ST normalizer expands such
/// messages itself; no own parser is needed here.
fn last_user_message_text(messages: &Value) -> String {
    normalize_messages(messages)
        .as_array()
        .and_then(|list| {
            list.iter()
                .rev()
                .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        })
        .map(|m| message_text(m).to_string())
        .unwrap_or_default()
}

/// The reply language is pinned by nothing: a card with its own `system_prompt` replaces
/// the main prompt entirely, and the model keeps the language of its text — hence replies in
/// English for part of the cards. The rule comes from the gateway, so it applies to
/// any card. Macros are not needed here: ST expands them before the request.
///
/// The wording is tied to the language of the line, not abstract: "in the language of the
/// last message" the model on an English card interpreted as "you may
/// mix", and separate English/Japanese phrases appeared in the reply.
/// The rule goes as the last block of the tail (after state and repetitions): for tail
/// instructions the later one wins, while the director's directive still goes after it.
pub fn reply_language_note(last_user_text: &str) -> String {
    // The shared Cyrillic predicate (V-16): the old `'А'..='я'` set missed the
    // Ukrainian/Belarusian letters (`і ї є ґ ў`), so a message built from them
    // fell through to the generic note and the reply language was not pinned.
    let cyr = last_user_text
        .chars()
        .filter(|c| crate::translation::is_cyrillic_letter(*c))
        .count();
    let lat = last_user_text.chars().filter(|c| c.is_ascii_alphabetic()).count();
    if cyr >= 2 && cyr >= lat {
        "[Язык ответа: весь ответ — строго на русском языке. Не вставляй английские, японские, \
китайские, французские и любые другие иностранные слова и фразы: допустимы только имена \
собственные и устоявшиеся обращения персонажа (например «-peko»). Даже если описание \
персонажа, примеры диалога или приветствие на другом языке — всё равно отвечай по-русски. \
Не смешивай языки в одном ответе.]"
            .to_string()
    } else if lat >= 2 && lat > cyr {
        "[Reply language: write the entire reply in English. Do not insert Russian, Japanese, \
Chinese, French or any other foreign words and phrases: only proper names and the character's \
established catchphrases are allowed. Even if the character card, examples or greeting are in \
another language — still reply in English. Do not mix languages in one reply.]"
            .to_string()
    } else {
        "[Язык ответа: пиши на языке последнего сообщения собеседника. Не смешивай языки в одном \
ответе: иностранная речь допустима только для имён, названий и устоявшихся выражений \
персонажа.]"
            .to_string()
    }
}

/// The tail notes of a roleplay request: state and repetition ban, then the language
/// rule — last before the director's directive (a later line wins over an
/// earlier one). ST service calls get no notes.
fn roleplay_notes(roleplay: bool, last_user_text: &str, notes: Vec<Option<String>>) -> Vec<String> {
    if !roleplay {
        return Vec::new();
    }
    let mut out: Vec<String> = notes.into_iter().flatten().collect();
    out.push(reply_language_note(last_user_text));
    out
}

/// How many reply characters suffice to see a verbatim copy of the previous
/// message (8+ words in a row) and regenerate the reply before showing it to the client.
const COPY_HEAD_CHARS: usize = 160;

/// Prompt budget = `num_ctx - reply reserve`, both from one contract (V-15).
/// The client `max_tokens` is a SillyTavern slider and may be set at or above
/// `num_ctx`; reserving that much zeroed the budget through `saturating_sub`,
/// and `trim_history_to_fit` then cut the dialogue down to four messages and
/// still did not fit — exactly the Ollama head-trim case it guards. The
/// reserve is therefore capped at half the context (never less than the old
/// 256 floor for a sane request), so the prompt always keeps at least
/// `num_ctx / 2` tokens.
pub(crate) fn prompt_budget(ctx: u32, max_tokens: Option<i64>) -> usize {
    let ctx = ctx.max(8192) as usize;
    let reserve = max_tokens.unwrap_or(1000).max(256).min((ctx / 2) as i64) as usize;
    ctx - reserve
}

/// The prompt must fit into `num_ctx` entirely. On overflow Ollama cuts it
/// from the head and discards the system prompt with the role instructions — the model is left
/// without a role and simply continues the history pattern (repeats previous messages).
/// Instead, the oldest dialogue lines are dropped here: system
/// messages, the tail, and the latest lines stay in place.
/// Token estimate of text for the prompt budget. Latin compresses about
/// threefold in BPE, Cyrillic about twofold (Qwen: 1.8–2.5 characters per token).
/// Counting the whole string by /3 meant underestimating a Russian prompt by one and a half to two
/// times: history trimming did not kick in, and Ollama cut the head together with
/// the system prompt. Rounding up — overestimation is safe here.
pub(crate) fn estimate_tokens(text: &str) -> usize {
    let mut ascii = 0usize;
    let mut other = 0usize;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii += 1;
        } else {
            other += 1;
        }
    }
    ascii / 3 + (other + 1) / 2
}

/// An image in a vision model context: llava ~576 tokens, gemma3/qwen-vl ~256 —
/// we take the upper estimate, otherwise a turn with a photo overflows num_ctx and Ollama cuts the system prompt.
const IMAGE_TOKEN_ESTIMATE: usize = 600;

fn message_token_estimate(message: &Value) -> usize {
    let images = message.get("images").and_then(Value::as_array).map_or(0, Vec::len);
    // The budget is computed before normalize_messages: SillyTavern images still sit as image_url parts
    let image_parts = message.get("content").and_then(Value::as_array).map_or(0, |parts| {
        parts.iter().filter(|part| part.get("type").and_then(Value::as_str) == Some("image_url")).count()
    });
    4 + (images + image_parts) * IMAGE_TOKEN_ESTIMATE
        + match message.get("content") {
            Some(Value::String(text)) => estimate_tokens(text),
            Some(Value::Array(parts)) => estimate_tokens(&parts.iter().filter_map(|part| part.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join(" ")),
            _ => 0,
        }
}

pub(crate) fn checked_prompt_messages(messages: Value, budget_tokens: usize) -> Result<Value, String> {
    let mut trimmed = trim_history_to_fit(messages, budget_tokens);
    let total = |list: &[Value]| list.iter().map(message_token_estimate).sum::<usize>();
    // Four latest lines — a coherence wish, not a condition: otherwise a long
    // tail would lock the chat with an error forever. We always keep the last line.
    if let Some(list) = trimmed.as_array_mut() {
        let is_dialogue = |m: &Value| matches!(m.get("role").and_then(Value::as_str), Some("user" | "assistant"));
        while total(list) > budget_tokens && list.iter().filter(|m| is_dialogue(m)).count() > 1 {
            let Some(oldest) = list.iter().position(is_dialogue) else { break };
            list.remove(oldest);
        }
    }
    let count = trimmed.as_array().map(|list| total(list)).unwrap_or(0);
    if count > budget_tokens {
        return Err(format!("Карточка и последние сообщения не помещаются в контекст ({count} из {budget_tokens} токенов). Сократите текст или увеличьте контекст в настройках."));
    }
    Ok(trimmed)
}

fn trim_history_to_fit(messages: Value, budget_tokens: usize) -> Value {
    let Some(list) = messages.as_array() else {
        return messages;
    };
    let estimate = |list: &[Value]| -> usize {
        list.iter().map(message_token_estimate).sum()
    };
    if estimate(list) <= budget_tokens {
        return messages;
    }
    // We never touch system messages: role and rules are not consumable
    // material. We drop the oldest dialogue lines, keeping the last
    // four: the tail is responsible for the line's coherence.
    let is_dialogue =
        |m: &Value| matches!(m.get("role").and_then(|r| r.as_str()), Some("user" | "assistant"));
    let mut drop: std::collections::HashSet<usize> = std::collections::HashSet::new();
    loop {
        let remaining: Vec<usize> = list
            .iter()
            .enumerate()
            .filter(|(i, m)| is_dialogue(m) && !drop.contains(i))
            .map(|(i, _)| i)
            .collect();
        if remaining.len() <= 4 {
            break;
        }
        drop.insert(remaining[0]);
        let trimmed: Vec<Value> = list
            .iter()
            .enumerate()
            .filter(|(i, _)| !drop.contains(i))
            .map(|(_, m)| m.clone())
            .collect();
        if estimate(&trimmed) <= budget_tokens {
            break;
        }
    }
    if drop.is_empty() {
        return messages;
    }
    Value::Array(
        list.iter()
            .enumerate()
            .filter(|(i, _)| !drop.contains(i))
            .map(|(_, m)| m.clone())
            .collect(),
    )
}

/// Append the state as the last system message: for chat models a later
/// system line outweighs the earlier one, while ST's history does not change.
/// The exception is the director's directive at the tail: it requires a reaction to the event,
/// and a state note ("calm and interested") last would extinguish this
/// requirement — the note goes before the directive, the last word stays with it.
fn with_state_note(messages: Value, note: &str) -> Value {
    let Some(list) = messages.as_array() else {
        return messages;
    };
    let mut out = list.clone();
    let slot = out
        .last()
        .filter(|last| {
            last.get("role").and_then(|r| r.as_str()) == Some("system")
                && message_text(last).starts_with(crate::director::DIRECTOR_HINT_PREFIX)
        })
        .map(|_| out.len() - 1)
        .unwrap_or(out.len());
    out.insert(slot, json!({ "role": "system", "content": note }));
    Value::Array(out)
}

/// Nothing lighter than this can be the evaluator: 0.8B–2B return, on the evaluation prompt,
/// not deltas but rewritten numbers of the current state (verified in a live run
/// 15.09.2026 on 1.7B), and an ordinary "hello" looked like a harsh event to them.
const MIN_EVALUATOR_MODEL: &str = "huihui_ai/qwen3.5-abliterated:4B";

/// Which model evaluates the exchange of lines. Usually the same one that runs the chat: it is
/// already in memory, no extra load. But if the chat model does not fit into
/// video memory entirely, evaluation on it runs with swapping and misses the
/// timeout — state and relationships then do not accumulate at all, silently. In that
/// case we take the lightest installed model from the catalog.
async fn evaluator_model() -> String {
    let Some(settings) = APP_HANDLE.get().and_then(app_settings) else {
        return String::new();
    };
    // An explicit user choice overrides the video-memory auto-selection entirely:
    // on 15.09.2026 auto-selection once silently substituted a too-light model.
    let explicit = settings.evaluator_model.trim();
    if !explicit.is_empty() {
        return explicit.to_string();
    }
    let chat_model = settings.chat_model;
    let fits = APP_HANDLE
        .get()
        .and_then(|app| {
            use tauri::Manager;
            app.try_state::<std::sync::Arc<crate::AppState>>()
                .map(|state| state.model_fits.load(Ordering::Relaxed))
        })
        .unwrap_or(true);
    if fits || chat_model.is_empty() {
        return chat_model;
    }

    let port = TARGET_OLLAMA_PORT.load(Ordering::Relaxed);
    let Ok(client) = crate::ollama_client::OllamaClient::new(format!("http://127.0.0.1:{port}"))
    else {
        return chat_model;
    };
    let Ok(installed) = client.list_models().await else {
        return chat_model;
    };
    // The catalog is ordered from junior models to senior ones, so the first
    // installed chat model after the threshold is the lightest suitable one.
    let catalog = crate::catalog::catalog();
    let from = catalog
        .iter()
        .position(|m| m.tag == MIN_EVALUATOR_MODEL)
        .unwrap_or(0);
    let lightest = catalog
        .into_iter()
        .skip(from)
        .filter(|m| m.category == "chat")
        .map(|m| m.tag)
        .find(|tag| crate::models::is_model_installed(&installed, tag));
    match lightest {
        Some(tag) if tag != chat_model => {
            crate::boot_log(&format!(
                "Оценка состояния: {chat_model} не помещается в видеопамять — оцениваем на {tag}"
            ));
            tag
        }
        _ => chat_model,
    }
}

/// The "last evaluate_state run for the (avatar, persona) pair" token.
/// Regeneration or two quick messages in a row start a second evaluation
/// before the first has time to reach the write (an HTTP call to the model — seconds);
/// without this check both deltas are applied, including the one that evaluated a
/// line the user had already discarded. The lock in traits/rpg/relationships does not
/// lose a single write — but it does not know that one of them is stale by meaning.
static LAST_EVAL_TOKEN: OnceLock<std::sync::Mutex<std::collections::HashMap<(String, String), u64>>> =
    OnceLock::new();

fn eval_token_registry() -> &'static std::sync::Mutex<std::collections::HashMap<(String, String), u64>> {
    LAST_EVAL_TOKEN.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Register a new evaluation attempt for the pair — call synchronously at
/// the moment the character's line became the last one for this pair (before spawning
/// the background task), so the order of tokens matches the order of lines, not the
/// order of completing async calls.
fn register_eval_token(avatar: &str, persona: &str) -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let token = COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    eval_token_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert((avatar.to_string(), persona.to_string()), token);
    token
}

/// Whether the token is still current: false — a newer evaluation has already started
/// for this pair, and the current result must not be applied.
fn is_eval_current(avatar: &str, persona: &str, token: u64) -> bool {
    eval_token_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(avatar.to_string(), persona.to_string()))
        .is_some_and(|latest| *latest == token)
}

/// The last exchange (`exchange_key`) applied to the (avatar, persona) pair.
/// The token above discards only an evaluation that has not been written yet; after the
/// write, regeneration and swipe of the same turn applied the deltas again, and the
/// state drifted with every regeneration.
static APPLIED_EXCHANGE: OnceLock<std::sync::Mutex<std::collections::HashMap<(String, String), u64>>> =
    OnceLock::new();

fn applied_exchange_registry() -> &'static std::sync::Mutex<std::collections::HashMap<(String, String), u64>> {
    APPLIED_EXCHANGE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn is_exchange_applied(avatar: &str, persona: &str, exchange: u64) -> bool {
    applied_exchange_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(avatar.to_string(), persona.to_string()))
        .is_some_and(|applied| *applied == exchange)
}

/// Claim the exchange before the first write. false — its deltas have already been applied.
fn claim_exchange(avatar: &str, persona: &str, exchange: u64) -> bool {
    let mut registry = applied_exchange_registry().lock().unwrap_or_else(|e| e.into_inner());
    let pair = (avatar.to_string(), persona.to_string());
    if registry.get(&pair) == Some(&exchange) {
        return false;
    }
    registry.insert(pair, exchange);
    true
}

/// Start a background evaluation of the shown reply. An already applied exchange
/// is cut off before registering the token: a new token would make stale
/// the evaluation that is just finishing this exchange.
fn spawn_evaluation(
    client: reqwest::Client,
    avatar: String,
    persona: String,
    user_text: String,
    reply_text: String,
    traits_store: crate::traits::Store,
    exchange: u64,
) {
    if reply_text.trim().is_empty() {
        return;
    }
    if is_exchange_applied(&avatar, &persona, exchange) {
        crate::boot_log(&format!(
            "Оценка состояния {avatar}: этот ход уже оценён (регенерация или свайп), пропущен"
        ));
        return;
    }
    // The token fixes the order of lines synchronously, before spawning: the completion
    // order of async tasks does not guarantee the order of lines.
    let eval_token = register_eval_token(&avatar, &persona);
    tauri::async_runtime::spawn(async move {
        evaluate_state(client, avatar, persona, user_text, reply_text, traits_store, eval_token, exchange)
            .await;
    });
}

/// Exchange evaluation by a local model: asynchronous and optional.
/// One model call instead of three: state + stats + skills.
/// No reply or garbage — change nothing (fail closed).
async fn evaluate_state(
    client: reqwest::Client,
    avatar: String,
    persona: String,
    user_text: String,
    reply_text: String,
    traits_store: crate::traits::Store,
    eval_token: u64,
    exchange: u64,
) {
    if reply_text.trim().is_empty() || user_text.trim().is_empty() {
        return;
    }
    let entry = traits_store.get(&avatar).cloned().unwrap_or_default();
    if !entry.live {
        return;
    }
    let model = evaluator_model().await;
    if model.is_empty() {
        return;
    }
    let port = TARGET_OLLAMA_PORT.load(Ordering::Relaxed);

    // The combined prompt: state + stats + skills in one call.
    let prompt = crate::rpg::prompts::combined_evaluator_prompt(
        &avatar,
        &persona,
        &user_text,
        &reply_text,
        &entry.relation(&persona),
    );
    let custom = APP_HANDLE.get().and_then(app_settings).map(|s| s.rpg_evaluator_prompt).unwrap_or_default();
    let prompt = with_custom_instructions(prompt, &custom);
    let num_ctx = APP_HANDLE.get().map_or(AUX_CONTEXT, |app| live_num_ctx(app, &model));
    let body = aux_chat_body(&model, &prompt, num_ctx, json!({ "temperature": 0.2, "num_predict": 400 }));
    // We log an evaluation refusal: without it the state simply "does not accumulate", and
    // there is nowhere to look for the reason — timeout, unloaded model, garbage in the reply.
    // Parsing can fail on a live model (9b occasionally returns non-JSON/out of
    // contract) — one retry, then an honest refusal.
    let mut eval = None;
    for attempt in 0..2u8 {
        let resp = match client
            .post(format!("http://127.0.0.1:{port}/api/chat"))
            .json(&body)
            .timeout(std::time::Duration::from_secs(120))
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                crate::boot_log(&format!(
                    "Оценка состояния не удалась ({model}, попытка {}): {e}",
                    attempt + 1
                ));
                continue;
            }
        };
        let value = match resp.json::<Value>().await {
            Ok(value) => value,
            Err(e) => {
                crate::boot_log(&format!(
                    "Оценка состояния: ответ не разобран ({model}, попытка {}): {e}",
                    attempt + 1
                ));
                continue;
            }
        };
        let raw = value["message"]["content"].as_str().unwrap_or_default();
        match crate::rpg::prompts::parse_combined_evaluator(raw) {
            Some(parsed) => {
                eval = Some(parsed);
                break;
            }
            None => {
                crate::boot_log(&format!(
                    "Оценка состояния: модель {model} вернула не JSON или оценки вне контракта \
                     (попытка {}): {:.240}",
                    attempt + 1,
                    raw.trim()
                ));
            }
        }
    }
    let Some(eval) = eval else {
        return;
    };

    // While the HTTP call to the model was running (seconds), a newer evaluation
    // could start for the same pair — a regeneration or the next line.
    // Applying a stale result is not allowed: it evaluated a line that was no longer
    // the character's latest. We check freshness before every write below too —
    // between apply_deltas and RelationshipStore::update a newer line
    // can also arrive.
    if !is_eval_current(&avatar, &persona, eval_token) {
        crate::boot_log(&format!(
            "Оценка состояния {avatar}: результат устарел (пришла более новая реплика), пропущен"
        ));
        return;
    }
    if !claim_exchange(&avatar, &persona, exchange) {
        crate::boot_log(&format!(
            "Оценка состояния {avatar}: этот ход уже оценён (регенерация или свайп), пропущен"
        ));
        return;
    }

    // Sentiment for the tag-evolution commit (step 4b, V-40) — captured here
    // because step 1 moves `state_deltas` out of `eval`.
    let sentiment = eval
        .state_deltas
        .as_ref()
        .map(crate::character_evolution::sentiment_from_state);

    // 1. Character state (traits).
    if let Some(deltas) = eval.state_deltas {
        if !is_eval_current(&avatar, &persona, eval_token) {
            crate::boot_log(&format!(
                "Оценка состояния {avatar}: результат устарел перед записью traits, пропущен"
            ));
            return;
        }
        if let Err(e) = crate::traits::apply_deltas(&avatar, &persona, &deltas) {
            crate::boot_log(&format!("Состояние персонажа: {e}"));
        }
    }

    // 2. The character's attitude to the interlocutor: five axes that the
    // «Отношения» screen shows. Before this branch appeared no one wrote them —
    // relationships.json was not created at all, and the screen was always empty.
    if let Some(deltas) = eval.relationship_deltas {
        if !persona.is_empty() {
            if !is_eval_current(&avatar, &persona, eval_token) {
                crate::boot_log(&format!(
                    "Оценка состояния {avatar}: результат устарел перед записью relationships, пропущен"
                ));
                // Part of the records are already on disk: a RPG snapshot without a reset would live on for another minute.
                invalidate_rpg_cache().await;
                return;
            }
            let path = crate::paths::data_dir().join("relationships.json");
            let character = crate::traits::display_name(&avatar);
            if let Err(e) = crate::relationships::RelationshipStore::update(&path, |store| {
                // `shift_and_settle` (V-41): the shift plus a 0.5 cool-down of
                // the pair's axes — the same settle idea as `traits::evolve`,
                // without it the ratings only ever grew.
                store.shift_and_settle(&character, &persona, &deltas);
            }) {
                crate::boot_log(&format!("Отношения: {e}"));
            }
        }
    }

    // 3. Stats (stats).
    if let Some(deltas) = eval.stat_deltas {
        if !is_eval_current(&avatar, &persona, eval_token) {
            crate::boot_log(&format!(
                "Оценка состояния {avatar}: результат устарел перед записью stats, пропущен"
            ));
            invalidate_rpg_cache().await;
            return;
        }
        if let Err(e) = crate::rpg::stats::update(|store| {
            crate::rpg::stats::init_character(store, &avatar);
            if let Some(cs) = store.get_mut(&avatar) {
                for (stat, amount) in &deltas {
                    if *amount > 0 {
                        if let Some(entry) = cs.stats.get_mut(stat.as_str()) {
                            crate::rpg::stats::add_experience(entry, *amount);
                        }
                    }
                }
            }
        }) {
            crate::boot_log(&format!("RPG характеристики: {e}"));
        }
    }

    // 4. Skills (skills).
    if let Some(deltas) = eval.skill_deltas {
        if !is_eval_current(&avatar, &persona, eval_token) {
            crate::boot_log(&format!(
                "Оценка состояния {avatar}: результат устарел перед записью skills, пропущен"
            ));
            invalidate_rpg_cache().await;
            return;
        }
        if let Err(e) = crate::rpg::skills::update(|store| {
            crate::rpg::skills::init_character(store, &avatar);
            if let Some(cs) = store.get_mut(&avatar) {
                for (skill, amount) in &deltas {
                    if *amount > 0 {
                        if let Some(entry) = cs.skills.get_mut(skill.as_str()) {
                            crate::rpg::skills::add_experience(entry, *amount);
                        }
                    }
                }
            }
        }) {
            crate::boot_log(&format!("RPG навыки: {e}"));
        }
    }

    // 4b. Tag evolution (V-40): reinforce the card's tags from this exchange —
    // the same commit discipline as steps 1-4 (one commit per exchange via
    // `claim_exchange`, freshness via `is_eval_current`). Everything that
    // touches the disk runs on a blocking thread (V-17); a failure is logged
    // and never breaks the reply (fail closed inside `apply_exchange`).
    if !is_eval_current(&avatar, &persona, eval_token) {
        crate::boot_log(&format!(
            "Оценка состояния {avatar}: результат устарел перед записью character_evolution, пропущен"
        ));
        invalidate_rpg_cache().await;
        return;
    }
    let evo_avatar = avatar.clone();
    let evo_reply = reply_text.clone();
    let evo_user = user_text.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let tagged = crate::traits::st_tagged_characters();
        let card_tags = tagged
            .iter()
            .find(|(avatar, _)| avatar == &evo_avatar)
            .map(|(_, tags)| tags.clone())
            .unwrap_or_default();
        // Flat vocabulary of every card's tags, unique.
        let vocabulary: Vec<String> = tagged
            .iter()
            .flat_map(|(_, tags)| tags.iter().cloned())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let signal = crate::character_evolution::ExchangeSignal {
            avatar: &evo_avatar,
            card_tags: &card_tags,
            reply_text: &evo_reply,
            user_text: &evo_user,
            vocabulary: &vocabulary,
            sentiment,
        };
        match crate::character_evolution::apply_exchange(&signal) {
            Ok(outcome) => crate::boot_log(&format!(
                "Эволюция тегов {}: усилено {}, приобретено {}, взаимодействий {}",
                outcome.avatar,
                outcome.reinforced.len(),
                outcome.acquired.len(),
                outcome.interaction_count
            )),
            Err(e) => crate::boot_log(&format!("Эволюция тегов не удалась: {e}")),
        }
    });

    // 5. Quest progress (V-03/V-57): one model call per active quest and only
    // when the world is started — a regular chat must not pay for it. Parsing
    // is fail closed (`commit_progress` writes only on a successful parse of an
    // existing active quest), and a completed quest claims its rewards once
    // (`claim_rewards`: XP → stats, items → inventory, relationship shifts →
    // relationships; `quest_xp_prompt` weights the XP, uniform if the model is
    // silent).
    if crate::rpg::world::started() && is_eval_current(&avatar, &persona, eval_token) {
        let active_quests: Vec<crate::rpg::quests::Quest> = crate::rpg::quests::load()
            .into_values()
            .filter(|q| q.status == crate::rpg::quests::QuestStatus::Active)
            .collect();
        for quest in active_quests {
            let Some(stage) = quest.current_stage() else { continue };
            let prompt = crate::rpg::quests::evaluator_prompt(
                &quest.title,
                &stage.objective,
                &user_text,
                &reply_text,
            );
            let Some(raw) = aux_model_reply(&client, &model, &prompt, "Прогресс квеста").await else {
                continue;
            };
            if crate::rpg::quests::parse_progress(&raw) != Some(true) {
                continue;
            }
            // The evaluation could become stale while the model calls were running: a regeneration
            // discarded the result above, but the quest block wrote without a check.
            if !is_eval_current(&avatar, &persona, eval_token) {
                break;
            }
            match crate::rpg::quests::commit_progress(&quest.id, &raw) {
                Ok(crate::rpg::quests::QuestProgress::Completed) => {
                    let weights_raw = aux_model_reply(
                        &client,
                        &model,
                        &crate::rpg::quests::quest_xp_prompt(&quest.title, &quest.description),
                        "Награда за квест",
                    )
                    .await;
                    let weights = weights_raw.as_deref().and_then(crate::rpg::stats::parse_stat_deltas);
                    // The quest is already Completed on disk. While the weights were being computed, the
                    // next line could start — the evaluation went stale, while the next
                    // pass only looks at Active. Skipping the reward here
                    // means never granting it. The weights are computed from the quest
                    // text, not from the line.
                    match crate::rpg::quests::claim_rewards(&quest.id, &avatar, weights.as_ref()) {
                        Ok(_) => {
                            crate::boot_log(&format!("Квест «{}» завершён, награды выданы", quest.title))
                        }
                        Err(e) => {
                            crate::boot_log(&format!("Квест «{}»: награды не выданы: {e}", quest.title))
                        }
                    }
                    if !is_eval_current(&avatar, &persona, eval_token) {
                        break;
                    }
                }
                Ok(_) => {}
                Err(e) => crate::boot_log(&format!("Квест «{}»: {e}", quest.title)),
            }
        }
    }

    // The evaluator may have just shifted relationships, stats, and skills on
    // disk, while `cached_rpg_snapshot` returns a snapshot for 60 seconds: without a reset
    // the next reply would be built on yesterday's values, and an NPC would stay
    // cold to what the player has already played out. We reset unconditionally — after a
    // model request, re-reading a few small JSON files costs nothing.
    invalidate_rpg_cache().await;
}

#[derive(Deserialize)]
struct TagConfigPatch {
    enabled: Option<bool>,
    model: Option<String>,
    max_tags: Option<u8>,
    language: Option<String>,
    write_card_tags: Option<bool>,
    auto_translate: Option<bool>,
    prompt: Option<String>,
}

/// POST /api/cloufam/tagger-config — partial update (the form in ST).
async fn tagger_config_update(
    State(state): State<Arc<GatewayState>>,
    Json(patch): Json<TagConfigPatch>,
) -> Response {
    // Mutation and save() under one lock: clone → save() → store_settings
    // clobbered parallel settings edits (T-22).
    let updated = {
        use tauri::Manager;
        let Some(app_state) = state.app.try_state::<Arc<crate::AppState>>() else {
            return (StatusCode::SERVICE_UNAVAILABLE, "Настройки приложения недоступны").into_response();
        };
        let mut guard = app_state.settings.lock().unwrap_or_else(|e| e.into_inner());
        let settings = &mut *guard;
        if let Some(v) = patch.enabled {
            settings.tagger_enabled = v;
        }
        if let Some(v) = patch.model {
            let v = v.trim().to_string();
            // The default value (chat_model) is stored as an empty string so that
            // changing the main chat model also drags the tagger along.
            settings.tagger_model = if v.is_empty() { String::new() } else { v };
        }
        if let Some(v) = patch.max_tags {
            settings.tagger_max_tags = v.clamp(3, 20);
        }
        if let Some(v) = patch.language {
            if matches!(v.as_str(), "ru" | "en" | "both") {
                settings.tagger_language = v;
            }
        }
        if let Some(v) = patch.write_card_tags {
            settings.tagger_write_card_tags = v;
        }
        if let Some(v) = patch.auto_translate {
            settings.auto_translate_imports = v;
        }
        if let Some(v) = patch.prompt {
            settings.tagger_prompt = v;
        }
        if let Err(e) = settings.save() {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("Не удалось сохранить настройки: {e}"))
                .into_response();
        }
        settings.clone()
    };
    let models = ollama_model_names(&state).await;
    Json(tagger_config_json(&updated, &models)).into_response()
}

/// POST /api/cloufam/md-import — parse a draft text into character card
/// fields. Along the contour — like auto-tags: the same model, the same bridge
/// `tagger/plugin-index.js` on the ST side (`/api/plugins/cloufam/md-import`).
async fn md_import_parse(
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<Value>,
) -> Response {
    let draft = body.get("draft").and_then(|v| v.as_str()).unwrap_or_default().trim().to_string();
    if draft.is_empty() {
        return (StatusCode::BAD_REQUEST, "draft required").into_response();
    }
    let settings = match app_settings(&state.app) {
        Some(s) => s,
        None => {
            return (StatusCode::SERVICE_UNAVAILABLE, "Настройки приложения недоступны").into_response();
        }
    };
    let model = effective_tagger_model(&settings);
    let port = TARGET_OLLAMA_PORT.load(Ordering::Relaxed);
    // A1-03/w1 №7: draft truncation is visible in the log — otherwise the user
    // would get a "trimmed" card parse without explanation.
    let crate::md_import::BuiltPrompt { prompt, truncated } =
        crate::md_import::build_prompt_ex(&draft);
    if truncated {
        crate::boot_log("Импорт черновика: черновик не влез в контекст модели и усечён");
    }
    // Without num_ctx Ollama takes its short default context, and the draft up to
    // MAX_DRAFT_CHARS was silently trimmed; it never goes below AUX_CONTEXT.
    let req_body = aux_chat_body(
        &model,
        &prompt,
        live_num_ctx(&state.app, &model),
        json!({ "temperature": 0.3, "num_predict": 900 }),
    );

    let Ok(resp) = state
        .client
        .post(format!("http://127.0.0.1:{port}/api/chat"))
        .json(&req_body)
        .timeout(std::time::Duration::from_secs(180))
        .send()
        .await
    else {
        return (StatusCode::BAD_GATEWAY, "Ollama недоступен").into_response();
    };
    let Ok(value) = resp.json::<Value>().await else {
        return (StatusCode::BAD_GATEWAY, "некорректный ответ Ollama").into_response();
    };
    let raw = value["message"]["content"].as_str().unwrap_or_default();
    match crate::md_import::parse_response(raw) {
        Some(fields) => Json(fields).into_response(),
        None => (StatusCode::BAD_GATEWAY, "не удалось разобрать ответ модели — попробуйте другую модель или дополните черновик").into_response(),
    }
}

/// POST /api/cloufam/translate — translate fields of an imported card
/// or lorebook. The client saves the original alongside and applies the result itself.
async fn translate_import(
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<Value>,
) -> Response {
    let kind = body.get("kind").and_then(Value::as_str).unwrap_or_default();
    let Some(source) = body.get("content") else {
        return (StatusCode::BAD_REQUEST, "content required").into_response();
    };
    let prompt = match crate::translation::build_prompt(kind, source) {
        Ok(prompt) => prompt,
        Err(error) => return (StatusCode::BAD_REQUEST, error).into_response(),
    };
    let settings = match app_settings(&state.app) {
        Some(settings) => settings,
        None => {
            return (StatusCode::SERVICE_UNAVAILABLE, "Настройки приложения недоступны")
                .into_response()
        }
    };
    let model = effective_tagger_model(&settings);
    let port = TARGET_OLLAMA_PORT.load(Ordering::Relaxed);
    let num_ctx = live_num_ctx(&state.app, &model);
    let source_chars = serde_json::to_string(source).map(|text| text.chars().count()).unwrap_or(0);
    let num_predict = match crate::translation::output_budget(source_chars, num_ctx) {
        Ok(budget) => budget,
        Err(error) => return (StatusCode::BAD_REQUEST, error).into_response(),
    };
    let req_body = aux_chat_body(
        &model,
        &prompt,
        num_ctx,
        json!({ "temperature": 0.1, "top_p": 0.8, "num_predict": num_predict }),
    );
    let resp = match state
        .client
        .post(format!("http://127.0.0.1:{port}/api/chat"))
        .json(&req_body)
        .timeout(std::time::Duration::from_secs(180))
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            return (StatusCode::BAD_GATEWAY, format!("Ollama недоступен: {error}"))
                .into_response()
        }
    };
    if !resp.status().is_success() {
        return (
            StatusCode::BAD_GATEWAY,
            format!("Ollama: HTTP {}", resp.status().as_u16()),
        )
            .into_response();
    }
    let value = match resp.json::<Value>().await {
        Ok(value) => value,
        Err(error) => {
            return (StatusCode::BAD_GATEWAY, format!("Некорректный ответ Ollama: {error}"))
                .into_response()
        }
    };
    let raw = value["message"]["content"].as_str().unwrap_or_default();
    match crate::translation::parse_response(kind, raw, source) {
        Some(translated) => Json(translated).into_response(),
        None => (StatusCode::BAD_GATEWAY, "модель вернула неполный перевод").into_response(),
    }
}

/// POST /api/cloufam/director — expand the director's note into a narration.
/// The ST extension adds the narration to the chat (as a `narrator` message), while the model
/// for expansion and the settings are here: the gateway owns Ollama and lives with the settings.
#[derive(serde::Deserialize)]
struct DirectorRequest {
    text: String,
    /// Whether to expand the note with the model or send it to the chat as is.
    #[serde(default)]
    expand: Option<bool>,
    /// A secret instruction to the model: does not go into the chat, applies to the next reply.
    #[serde(default)]
    hidden_text: String,
    /// Who replies after the narration: `characters`, `narrator`, or `none`.
    #[serde(default)]
    after: String,
}

async fn director_narration(
    State(state): State<Arc<GatewayState>>,
    Json(request): Json<DirectorRequest>,
) -> Result<Response, (StatusCode, String)> {
    // Shop commands are handled without the model.
    if let Some(shop_cmd) = crate::director::parse_shop_command(&request.text) {
        if let crate::director::ShopCommand::Invalid(usage) = shop_cmd {
            return Ok(Json(json!({ "narration": usage, "model": "director", "expanded": false })).into_response());
        }
        // The shop is placed into the location of the started world. `world::update` in
        // an unstarted world would refuse by itself without creating `world.json` — here we give
        // only a clear reply instead of an error.
        if !crate::rpg::world::started() {
            return Ok(Json(json!({
                "narration": "Магазин ставится в локацию, а RPG-мир ещё не начат: начните его в RPG-панели (вкладка «Мир»).",
                "model": "director",
                "expanded": false,
            }))
            .into_response());
        }
        let shop_type = match &shop_cmd {
            crate::director::ShopCommand::Set(t) => Some(t.clone()),
            crate::director::ShopCommand::Remove => None,
            crate::director::ShopCommand::Invalid(_) => unreachable!("отклонено выше"),
        };
        let location_name = match crate::rpg::world::update(|w| {
            w.set_shop(shop_type.clone())?;
            Ok::<_, String>(w.current().map(|l| l.name.clone()).unwrap_or_default())
        }) {
            Ok(Ok(name)) => name,
            Ok(Err(e)) | Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e)),
        };
        // Otherwise the prompt comes from the cache for up to 60 s without the shop.
        invalidate_rpg_cache().await;
        let msg = if shop_type.is_some() {
            format!("Магазин «{}» установлен в локации «{}»", shop_type.unwrap(), location_name)
        } else {
            format!("Магазин убран из локации «{}»", location_name)
        };
        return Ok(Json(json!({ "narration": msg, "model": "director", "expanded": false })).into_response());
    }

    // Own-skill command: `// skill <Name> -> <Stat>` (changelog
    // 1.2.39 "own skills"). Without the model: we write the skill to the active card.
    if let Some(cmd) = crate::director::parse_skill_command(&request.text) {
        return match cmd {
            crate::director::SkillCommand::Add { name, stat } => {
                let Some(avatar) = active_avatar().filter(|a| !a.is_empty()) else {
                    return Ok(Json(json!({
                        "narration": "Навык добавляется персонажу: откройте карточку активного персонажа.",
                        "model": "director",
                        "expanded": false,
                    }))
                    .into_response());
                };
                match crate::rpg::skills::update_if_ok(|store| {
                    crate::rpg::skills::add_custom_skill(store, &avatar, &name, &stat)
                }) {
                    Ok(Ok(())) => {
                        invalidate_rpg_cache().await;
                        Ok(Json(json!({
                            "narration": format!("Навык «{name}» добавлен (привязка: {stat})."),
                            "model": "director",
                            "expanded": false,
                        }))
                        .into_response())
                    }
                    Ok(Err(e)) => Ok(Json(json!({ "narration": e, "model": "director", "expanded": false }))
                        .into_response()),
                    Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e)),
                }
            }
            crate::director::SkillCommand::Invalid(usage) => Ok(Json(json!({
                "narration": usage,
                "model": "director",
                "expanded": false,
            }))
            .into_response()),
        };
    }

    // New-location command: `// location [description]` (changelog 1.2.39
    // "new locations are generated automatically"). Generation goes through the same
    // branch as a move to an unknown place (V-57): model →
    // `world::commit_generated` (add_location + connection + move, fail closed).
    if let Some(cmd) = crate::director::parse_location_command(&request.text) {
        let hint = match cmd {
            crate::director::LocationCommand::Generate(hint) => hint,
            crate::director::LocationCommand::Invalid(usage) => {
                return Ok(Json(json!({
                    "narration": usage,
                    "model": "director",
                    "expanded": false,
                }))
                .into_response());
            }
        };
        if !crate::rpg::world::started() {
            return Ok(Json(json!({
                "narration": "Локации генерируются в RPG-мире: начните его в RPG-панели (вкладка «Мир»).",
                "model": "director",
                "expanded": false,
            }))
            .into_response());
        }
        let world = crate::rpg::world::load();
        let Some(from) = world.current() else {
            return Err((StatusCode::CONFLICT, "текущая локация не найдена".to_string()));
        };
        let (from_id, from_name, from_desc) = (
            world.current_location.clone(),
            from.name.clone(),
            from.description.clone(),
        );
        let to = hint.unwrap_or_else(|| "новое неизвестное место".to_string());
        let model = evaluator_model().await;
        if model.is_empty() {
            return Err((StatusCode::SERVICE_UNAVAILABLE, "модель не настроена".to_string()));
        }
        let prompt = crate::rpg::world::generation_prompt(&from_name, &to, &from_desc);
        let Some(raw) = aux_model_reply(&state.client, &model, &prompt, "Генерация локации").await
        else {
            return Err((StatusCode::BAD_GATEWAY, "Ollama недоступен".to_string()));
        };
        // The new location id comes from the chat only as text — we give our own,
        // unused one; the user will see the name from the model's reply.
        let to_id = format!(
            "loc-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or_default()
        );
        return match crate::rpg::world::commit_generated(&raw, &from_id, &to_id) {
            Ok(location) => {
                if let Err(e) = crate::rpg::npcs::sync_with_world() {
                    crate::boot_log(&format!("NPC: {e}"));
                }
                invalidate_rpg_cache().await;
                Ok(Json(json!({
                    "narration": format!("Новая локация «{}» — вы в ней.", location.name),
                    "model": "director",
                    "expanded": false,
                }))
                .into_response())
            }
            Err(e) => Ok(Json(json!({ "narration": e, "model": "director", "expanded": false }))
                .into_response()),
        };
    }

    crate::director::validate(&request.text).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let settings = app_settings(&state.app).ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "Настройки приложения недоступны".to_string(),
    ))?;
    let model = effective_tagger_model(&settings);

    // Note expansion: without it the narration goes to the chat exactly as written.
    let raw_narration = if request.expand == Some(false) {
        request.text.trim().to_string()
    } else {
        let ollama_port = TARGET_OLLAMA_PORT.load(Ordering::Relaxed);
        let director_prompt = with_custom_instructions(
            crate::director::build_prompt(&request.text),
            &settings.director_prompt,
        );
        let body = aux_chat_body(
            &model,
            &director_prompt,
            live_num_ctx(&state.app, &model),
            json!({ "temperature": 0.8, "top_p": 0.9, "num_predict": 512, "repeat_penalty": 1.1 }),
        );
        let resp = state
            .client
            .post(format!("http://127.0.0.1:{ollama_port}/api/chat"))
            .json(&body)
            .timeout(std::time::Duration::from_secs(180))
            .send()
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, format!("Ollama недоступен: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            if status.as_u16() == 404 {
                return Err((
                    StatusCode::CONFLICT,
                    format!("Модель «{model}» не установлена. Скачайте её в настройках Cloufam."),
                ));
            }
            return Err((
                StatusCode::BAD_GATEWAY,
                format!("Ollama: HTTP {} {text}", status.as_u16()),
            ));
        }
        let value: Value = resp
            .json()
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, format!("Некорректный ответ Ollama: {e}")))?;
        crate::director::clean_narration(value["message"]["content"].as_str().unwrap_or_default())
    };

    // An empty expansion is no reason to leave the scene without an event: we send the note as is.
    let expanded = !raw_narration.is_empty() && request.expand != Some(false);
    let narration = if raw_narration.is_empty() {
        request.text.trim().to_string()
    } else {
        raw_narration
    };

    let mut response = json!({
        "narration": narration,
        "model": model,
        "expanded": expanded,
    });
    // The secret is taken from a separate field: the scene note goes to the chat anyway, and
    // substituting it into the hidden directive told the model nothing.
    let hidden = request.hidden_text.trim();
    if !hidden.is_empty() {
        // A3-07: the secret also passes the limit before insertion into the system block.
        crate::director::validate_hidden_directive(hidden)
            .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
        response["hidden"] = json!(crate::director::build_hidden_directive(hidden));
    }
    if let Some(hint) = crate::director::hint_for(&request.after) {
        response["hint"] = json!(hint);
    }
    Ok(Json(response).into_response())
}

/// POST /api/cloufam/tags — character tags based on his card.
async fn tagger_tags(
    State(state): State<Arc<GatewayState>>,
    Json(request): Json<crate::tagger::TagRequest>,
) -> Result<Response, (StatusCode, String)> {
    let settings = app_settings(&state.app).ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "Настройки приложения недоступны".to_string(),
    ))?;

    let model = effective_tagger_model(&settings);
    let max_tags = request
        .max_tags
        .map(|v| v as usize)
        .unwrap_or(settings.tagger_max_tags as usize)
        .clamp(1, 20);
    let language = match request.language.as_deref() {
        Some("ru") | Some("en") | Some("both") => request.language.clone().unwrap_or_default(),
        _ => settings.tagger_language.clone(),
    };

    let prompt = crate::tagger::build_prompt(
        &request,
        &crate::tagger::TagOptions {
            max_tags,
            language: &language,
            custom_prompt: &settings.tagger_prompt,
        },
    );

    let ollama_port = TARGET_OLLAMA_PORT.load(Ordering::Relaxed);
    let body = aux_chat_body(
        &model,
        &prompt,
        live_num_ctx(&state.app, &model),
        json!({ "temperature": 0.2, "top_p": 0.8, "num_predict": 256, "repeat_penalty": 1.05 }),
    );

    // The first request loads the model into VRAM: 180 s is enough for a cold start too.
    let resp = state
        .client
        .post(format!("http://127.0.0.1:{ollama_port}/api/chat"))
        .json(&body)
        .timeout(std::time::Duration::from_secs(180))
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("Ollama недоступен: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        if status.as_u16() == 404 {
            return Err((
                StatusCode::CONFLICT,
                format!("Модель «{model}» не установлена. Скачайте её в настройках Cloufam."),
            ));
        }
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("Ollama: HTTP {} {text}", status.as_u16()),
        ));
    }

    let value: Value = resp
        .json()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("Некорректный ответ Ollama: {e}")))?;
    let raw = value["message"]["content"].as_str().unwrap_or_default();
    let tags = crate::tagger::parse_tags(raw, &request.existing_tags, max_tags);

    Ok(Json(json!({ "tags": tags, "model": model })).into_response())
}

// === Changelog ===

/// GET /api/cloufam/st/update-check — what is installed and what is available
/// to the "Update SillyTavern" item.
async fn handle_st_update_check() -> Response {
    let installed = crate::sillytavern::installed_st_version();
    let pinned = crate::sillytavern::ST_VERSION.to_string();
    match crate::sillytavern::latest_st_version().await {
        Ok(latest) => {
            Json(json!({ "installed": installed, "pinned": pinned, "latest": latest })).into_response()
        }
        Err(error) => Json(
            json!({ "installed": installed, "pinned": pinned, "latest": null, "error": error }),
        )
        .into_response(),
    }
}

/// POST /api/cloufam/st/update — update the installed SillyTavern to
/// `version` (an empty body or no version — to the latest upstream release).
/// A long request: download, unpack, npm install, and ST restart go
/// in one call.
async fn handle_st_update(State(state): State<Arc<GatewayState>>, body: Bytes) -> Response {
    let requested = serde_json::from_slice::<Value>(&body).ok();
    let version = match requested
        .as_ref()
        .and_then(|v| v.get("version"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        Some(v) => v.trim_start_matches('v').to_string(),
        None => match crate::sillytavern::latest_st_version().await {
            Ok(latest) => latest,
            Err(error) => return Json(json!({ "ok": false, "error": error })).into_response(),
        },
    };
    match crate::update_sillytavern_to(state.app.clone(), version).await {
        Ok(report) => Json(report).into_response(),
        Err(error) => Json(json!({ "ok": false, "error": error })).into_response(),
    }
}

/// The chat backup file name: the name from the chat is cleaned of path separators so that
/// the write does not escape the backups directory.
fn chat_backup_stem(filename: &str) -> String {
    let cleaned: String = filename
        .trim()
        .trim_end_matches(".jsonl")
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '(' | ')' | '@') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut stem = cleaned.trim().trim_matches('_').to_string();
    while stem.contains("__") {
        stem = stem.replace("__", "_");
    }
    let stem = stem.trim().trim_matches('_');
    if stem.is_empty() {
        "chat".to_string()
    } else {
        stem.to_string()
    }
}

/// POST /api/cloufam/chat/export — save the open chat's jsonl to
/// `data/chat-backups/<name>-<ms>.jsonl`. The content is sent by the extension: it takes it
/// from ST's regular `/api/chats/export`, so the layout of chat
/// files and their format stay SillyTavern's concern.
async fn handle_chat_export(body: Bytes) -> Response {
    let Ok(value) = serde_json::from_slice::<Value>(&body) else {
        return Json(json!({ "ok": false, "error": "ожидался JSON" })).into_response();
    };
    let filename = value["filename"].as_str().unwrap_or("chat");
    let content = value["content"].as_str().unwrap_or("");
    if content.trim().is_empty() {
        return Json(json!({ "ok": false, "error": "пустой файл чата" })).into_response();
    }
    let dir = crate::paths::data_dir().join("chat-backups");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Json(json!({ "ok": false, "error": e.to_string() })).into_response();
    }
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!("{}-{}.jsonl", chat_backup_stem(filename), millis));
    // Via .part + rename: a truncated backup under the production name would look intact,
    // and this is exactly where the user restores the chat from later
    let part = path.with_extension("jsonl.part");
    let size = content.len();
    let content = content.to_owned();
    let written = tokio::task::spawn_blocking(move || {
        let result = std::fs::File::create(&part)
            .and_then(|mut file| {
                use std::io::Write;
                file.write_all(content.as_bytes())?;
                file.sync_all()
            })
            .and_then(|_| std::fs::rename(&part, &path));
        if result.is_err() {
            let _ = std::fs::remove_file(&part);
        }
        result.map(|_| path)
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r.map_err(|e| e.to_string()));
    let path = match written {
        Ok(path) => path,
        Err(e) => return Json(json!({ "ok": false, "error": e })).into_response(),
    };
    Json(
        json!({ "ok": true, "path": path.to_string_lossy(), "size": size }),
    )
    .into_response()
}

/// GET /api/cloufam/changelog — the changelog + which version the player has seen.
async fn handle_changelog(State(state): State<Arc<GatewayState>>) -> Response {
    let entries = crate::changelog::changelog();
    let seen = app_settings(&state.app)
        .map(|s| s.changelog_version_seen.clone())
        .unwrap_or_default();
    Json(json!({
        "entries": entries,
        "seen_version": seen,
    }))
    .into_response()
}

/// POST /api/cloufam/changelog/seen — mark the version as read.
async fn handle_changelog_seen(
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<Value>,
) -> Response {
    let version = body["version"].as_str().unwrap_or_default().trim().to_string();
    if version.is_empty() {
        return (StatusCode::BAD_REQUEST, "version required").into_response();
    }
    // Mutation and save() under one lock: clone → save() → store_settings
    // clobbered parallel settings edits (T-22).
    {
        use tauri::Manager;
        let Some(app_state) = state.app.try_state::<Arc<crate::AppState>>() else {
            return (StatusCode::SERVICE_UNAVAILABLE, "настройки недоступны").into_response();
        };
        let mut guard = app_state.settings.lock().unwrap_or_else(|e| e.into_inner());
        guard.changelog_version_seen = version.clone();
        if let Err(e) = guard.save() {
            return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
    }
    Json(json!({ "ok": true, "seen_version": version })).into_response()
}

// === Version rollback ===

/// GET /api/cloufam/versions — the list of available versions from GitHub Releases.
async fn handle_versions(State(state): State<Arc<GatewayState>>) -> Response {
    let current = state.app.package_info().version.to_string();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build();
    // The versions window reads the response as JSON on any branch: a text error
    // turned into a "SyntaxError" for it instead of the reason.
    let fail = |status: StatusCode, message: String| (status, Json(json!({ "error": message }))).into_response();
    let Ok(client) = client else {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "не удалось создать HTTP-клиент".into());
    };
    let resp = client
        .get("https://api.github.com/repos/AndreyFSDev/cloufam-releases/releases")
        .header("User-Agent", format!("Cloufam/{}", current))
        .send()
        .await;
    let resp = match resp {
        Ok(resp) => resp,
        Err(e) => return fail(StatusCode::BAD_GATEWAY, format!("GitHub недоступен: {e}")),
    };
    let status = resp.status();
    if !status.is_success() {
        // 403 — the GitHub API rate limit without a token: its body is not an array, and
        // the versions window showed "No available versions" instead of the reason.
        let message = resp
            .json::<Value>()
            .await
            .ok()
            .and_then(|v| v["message"].as_str().map(str::to_string))
            .unwrap_or_default();
        return fail(StatusCode::BAD_GATEWAY, format!("GitHub API: HTTP {} {message}", status.as_u16()));
    }
    let Ok(val) = resp.json::<Value>().await else {
        return fail(StatusCode::BAD_GATEWAY, "некорректный ответ GitHub API".into());
    };
    let Some(releases) = val.as_array() else {
        return Json(json!({ "current": current, "versions": [] })).into_response();
    };
    let versions: Vec<Value> = releases
        .iter()
        .filter_map(|r| {
            let tag = r.get("tag_name")?.as_str()?;
            let version = tag.strip_prefix('v').unwrap_or(tag);
            let body = r.get("body").and_then(|v| v.as_str()).unwrap_or("");
            let published = r.get("published_at").and_then(|v| v.as_str()).unwrap_or("");
            let assets = r.get("assets").and_then(|v| v.as_array())?;
            let exe_asset = assets.iter().find(|a| {
                a.get("name")
                    .and_then(|n| n.as_str())
                    .map(|n| n.contains("setup") && n.ends_with(".exe"))
                    .unwrap_or(false)
            });
            let exe_url = exe_asset
                .and_then(|a| a.get("browser_download_url"))
                .and_then(|v| v.as_str());
            let exe_size = exe_asset
                .and_then(|a| a.get("size"))
                .and_then(|v| v.as_u64());
            Some(json!({
                "version": version,
                "tag": tag,
                "notes": body,
                "published": published,
                "exe_url": exe_url,
                "exe_size": exe_size,
                "is_current": version == current,
            }))
        })
        .collect();
    Json(json!({ "current": current, "versions": versions })).into_response()
}

/// Release version: `1.2.52` or `1.2.52-beta.1`. The string goes into the address path
/// of the manifest, so `/`, `..`, `?`, and the like are not allowed in it.
fn is_release_version(version: &str) -> bool {
    let (core, pre) = match version.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (version, None),
    };
    let numeric = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    core.split('.').count() == 3
        && core.split('.').all(numeric)
        && pre.map_or(true, |pre| {
            !pre.is_empty() && pre.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.')
        })
}

/// The rollback binary — only from Cloufam releases on GitHub. We verify the parsed
/// address: a string prefix is bypassed through `/../` in the path.
fn is_trusted_release_url(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    parsed.scheme() == "https"
        && parsed.host_str() == Some("github.com")
        && parsed.port().is_none()
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.path().starts_with("/AndreyFSDev/cloufam-releases/releases/download/")
}

/// POST /api/cloufam/versions/rollback — download and install the selected version.
async fn handle_rollback(
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<Value>,
) -> Response {
    if !is_local_request(&headers) {
        return (StatusCode::FORBIDDEN, "только из приложения").into_response();
    }
    // We take the binary address from the version manifest, not from the request body:
    // the version list gives an installer link, which does not belong here.
    let version = body["version"].as_str().unwrap_or_default().trim();
    if version.is_empty() {
        return (StatusCode::BAD_REQUEST, "version обязателен").into_response();
    }
    if !is_release_version(version) {
        return (StatusCode::BAD_REQUEST, "некорректная версия").into_response();
    }
    let current = state.app.package_info().version.to_string();
    if version == current {
        return (StatusCode::CONFLICT, "это текущая версия").into_response();
    }

    // We install the same way as an update: the manifest of the selected release gives
    // the address of the binary itself and its signature. The version list gives an
    // NSIS installer link — it must not be substituted for Cloufam.exe.
    let manifest_url = format!(
        "https://github.com/AndreyFSDev/cloufam-releases/releases/download/v{version}/latest.json"
    );
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(client) => client,
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "не удалось создать HTTP-клиент")
                .into_response()
        }
    };
    let manifest: Value = match client.get(&manifest_url).send().await {
        Ok(resp) if resp.status().is_success() => match resp.json().await {
            Ok(val) => val,
            Err(_) => {
                return (StatusCode::BAD_GATEWAY, "манифест версии не разобран").into_response()
            }
        },
        Ok(resp) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("манифест версии недоступен: HTTP {}", resp.status()),
            )
                .into_response()
        }
        Err(_) => return (StatusCode::BAD_GATEWAY, "манифест версии не скачан").into_response(),
    };
    let exe_url = manifest["exe"]["url"].as_str().unwrap_or_default().to_string();
    let signature = manifest["exe"]["signature"].as_str().unwrap_or_default().to_string();
    if exe_url.is_empty() || signature.is_empty() {
        return (
            StatusCode::CONFLICT,
            "в этой версии нет подписанного бинарника — откат на неё невозможен",
        )
            .into_response();
    }
    if !is_trusted_release_url(&exe_url) {
        return (StatusCode::BAD_REQUEST, "недоверенный URL").into_response();
    }

    let app = state.app.clone();
    match crate::install_exe_release(&app, &exe_url, &signature, |_| {}).await {
        // Installation does not return: the process restarts as the new version.
        Ok(()) => (StatusCode::OK, "устанавливается").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// === RPG endpoints ===

fn location_json(loc: &crate::rpg::world::Location) -> Value {
    json!({
        "name": loc.name,
        "description": loc.description,
        "tags": loc.tags,
        "atmosphere": loc.atmosphere,
        "connections": loc.connections,
        "npcs": loc.npcs,
        "has_shop": loc.has_shop,
        "shop_type": loc.shop_type,
    })
}

/// GET /api/cloufam/rpg/world — the current world and location. Before RPG start —
/// only `started: false`: the default city from `World::default()` does not go
/// into the prompt, and it must not be shown in the panel as the current location.
/// V-01: a broken `world.json` is its own state — the default city must not
/// leak into prompts or the panel as if it were the real world.
async fn rpg_world() -> Response {
    match crate::rpg::world::state() {
        crate::rpg::world::WorldState::NotStarted => {
            Json(json!({ "started": false })).into_response()
        }
        crate::rpg::world::WorldState::Broken => {
            Json(json!({
                "started": false,
                "broken": true,
                "hint": "world.json повреждён, копия world.json.corrupt сохранена; \
                         сбросьте мир во вкладке «Мир»",
            }))
            .into_response()
        }
        crate::rpg::world::WorldState::Ready(world) => {
            Json(json!({
                "started": true,
                "current_location": world.current_location,
                "location": world.current().map(|loc| location_json(loc)),
                "exits": world.available_exits().iter().map(|(id, name)| json!({
                    "id": id, "name": name
                })).collect::<Vec<_>>(),
            }))
            .into_response()
        }
    }
}

/// POST /api/cloufam/rpg/world/start — start RPG: create the default world.
async fn rpg_world_start() -> Response {
    match crate::rpg::world::start() {
        Ok(created) => {
            // World NPCs and shop counters: without seeding, `tavern_keeper` and
            // `merchant` did not exist before the first move, the prompt did not know
            // the residents, and `/rpg/trade` found no counter.
            if let Err(e) = crate::rpg::npcs::sync_with_world() {
                crate::boot_log(&format!("NPC: {e}"));
            }
            let world = crate::rpg::world::load();
            let npcs = crate::rpg::npcs::load();
            ensure_shop_shelves(&world, &npcs);
            invalidate_rpg_cache().await;
            Json(json!({ "ok": true, "created": created })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// POST /api/cloufam/rpg/world/reset — reset the world together with combat:
/// combat without a world does not get into the prompt, but it would get in the way of starting a new one.
async fn rpg_world_reset() -> Response {
    let _combat_guard = COMBAT_ACTION.lock().await;
    // World, quests, and NPCs are one bundle: after the reset the old world came back
    // as active quests and NPCs with the previous location ids.
    let result = crate::rpg::enemies::save_combat(None)
        .and_then(|()| crate::rpg::world::reset())
        .and_then(|()| crate::rpg::quests::update(|store| *store = Default::default()))
        .and_then(|()| crate::rpg::npcs::update(|store| *store = Default::default()))
        // The reset confirmation promises to delete the shops: the counters live in
        // the inventory, and without this line the old assortment would stay hanging.
        .and_then(|()| {
            crate::rpg::inventory::update_if_ok(|store| {
                store.npcs.clear();
                Ok::<(), String>(())
            })
            .and_then(|inner| inner)
        });
    invalidate_rpg_cache().await;
    match result {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// POST /api/cloufam/rpg/world/move — move to a location. An unknown target —
/// that is an "unexplored place": the location is generated by the model, linked to
/// the current one, and you move into it (changelog "new locations are generated
/// automatically on move"; V-57: `generation_prompt`/`commit_generated` —
/// the module's canonical parse and commit, fail-closed). Generation triggers
/// only for a non-existent id: a target that exists in the world but is not linked
/// stays an error — otherwise `commit_generated` would clobber the real location.
/// Runs on an axum async-runtime worker: a short store commit for a known
/// target, one awaited model call plus a commit for an unknown one.
async fn rpg_world_move(
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<Value>,
) -> Response {
    let raw_target = body["location"].as_str().unwrap_or_default();
    if raw_target.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "location required").into_response();
    }
    let target = match checked_rpg_id(raw_target) {
        Ok(target) => target,
        Err((status, e)) => return (status, e).into_response(),
    };
    let known = crate::rpg::world::load().locations.contains_key(&target);
    if known {
        return match crate::rpg::world::update(|world| {
            world.move_to(&target).map(|loc| location_json(loc))
        }) {
            Ok(Ok(location)) => {
                // Entering a location (V-02): seed the NPCs the world declares
                // for it and recompute who is "busy" — idempotent, safe to call
                // often. Only errors are logged; the move already succeeded.
                if let Err(e) = crate::rpg::npcs::sync_with_world() {
                    crate::boot_log(&format!("NPC: {e}"));
                }
                invalidate_rpg_cache().await;
                Json(json!({ "ok": true, "location": location })).into_response()
            }
            Ok(Err(e)) if e == crate::rpg::world::NOT_STARTED => {
                (StatusCode::CONFLICT, e).into_response()
            }
            Ok(Err(e)) => (StatusCode::BAD_REQUEST, e).into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        };
    }
    let world = crate::rpg::world::load();
    let Some(from) = world.current() else {
        return (StatusCode::CONFLICT, "текущая локация не найдена").into_response();
    };
    let (from_id, from_name, from_desc) = (
        world.current_location.clone(),
        from.name.clone(),
        from.description.clone(),
    );
    let model = evaluator_model().await;
    if model.is_empty() {
        return (StatusCode::SERVICE_UNAVAILABLE, "модель не настроена").into_response();
    }
    let prompt = crate::rpg::world::generation_prompt(&from_name, &target, &from_desc);
    let Some(raw) = aux_model_reply(&state.client, &model, &prompt, "Генерация локации").await else {
        return (StatusCode::BAD_GATEWAY, "Ollama недоступен").into_response();
    };
    // `commit_generated` parses (fail closed) and commits add_location +
    // two-way connect + the move in one step; its error is either model
    // garbage or a store failure — both leave the world unchanged.
    match crate::rpg::world::commit_generated(&raw, &from_id, &target) {
        Ok(location) => {
            if let Err(e) = crate::rpg::npcs::sync_with_world() {
                crate::boot_log(&format!("NPC: {e}"));
            }
            invalidate_rpg_cache().await;
            Json(json!({
                "ok": true,
                "generated": true,
                "location": location_json(&location),
            }))
            .into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

/// GET /api/cloufam/rpg/stats — character stats.
async fn rpg_stats(uri: axum::http::Uri) -> Response {
    let avatar = parse_avatar(&uri);
    let store = crate::rpg::stats::load();
    if avatar.is_empty() {
        return Json(json!({ "characters": store.len() })).into_response();
    }
    match store.get(&avatar) {
        Some(stats) => Json(json!({
            "avatar": avatar,
            "stats": stats.stats.iter().map(|(name, entry)| json!({
                "name": name,
                "level": entry.level,
                "experience": entry.experience,
                "exp_to_next": crate::rpg::stats::exp_to_next(entry.level),
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        None => Json(json!({ "avatar": avatar, "stats": [] })).into_response(),
    }
}

/// GET /api/cloufam/rpg/skills — character skills.
async fn rpg_skills(uri: axum::http::Uri) -> Response {
    let avatar = parse_avatar(&uri);
    let store = crate::rpg::skills::load();
    if avatar.is_empty() {
        return Json(json!({ "characters": store.len() })).into_response();
    }
    match store.get(&avatar) {
        Some(skills) => Json(json!({
            "avatar": avatar,
            "skills": skills.skills.iter().map(|(name, entry)| json!({
                "name": name,
                "level": entry.level,
                "experience": entry.experience,
                "exp_to_next": crate::rpg::skills::exp_to_next(entry.level),
                "stat_binding": entry.stat_binding,
                "is_custom": entry.is_custom,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        None => Json(json!({ "avatar": avatar, "skills": [] })).into_response(),
    }
}

/// GET /api/cloufam/rpg/quests — active quests.
async fn rpg_quests() -> Response {
    let store = crate::rpg::quests::load();
    let quests: Vec<Value> = store
        .values()
        .filter(|q| q.status == crate::rpg::quests::QuestStatus::Active)
        .map(|q| {
            let stage = q.current_stage();
            json!({
                "id": q.id,
                "title": q.title,
                "description": q.description,
                "current_stage": q.current_stage,
                "total_stages": q.stages.len(),
                "objective": stage.map(|s| s.objective.as_str()),
                "giver": q.giver,
            })
        })
        .collect();
    Json(json!({ "quests": quests })).into_response()
}

/// POST /api/cloufam/rpg/npcs/generate — live NPC generation for the current
/// location (V-02: NPCs had no writer). Runs on an axum async-runtime
/// worker: one awaited model call; `commit_generated` parses, clamps and
/// writes fail-closed — garbage from the model changes nothing.
async fn rpg_npc_generate(State(state): State<Arc<GatewayState>>) -> Response {
    let world = crate::rpg::world::load();
    if !crate::rpg::world::started() {
        return (StatusCode::CONFLICT, crate::rpg::world::NOT_STARTED).into_response();
    }
    let Some(location) = world.current() else {
        return (StatusCode::CONFLICT, "текущая локация не найдена").into_response();
    };
    let model = evaluator_model().await;
    if model.is_empty() {
        return (StatusCode::SERVICE_UNAVAILABLE, "модель не настроена").into_response();
    }
    let shop_type = location.has_shop.then(|| location.shop_type.as_deref()).flatten();
    let prompt = crate::rpg::npcs::generation_prompt(
        &location.name,
        &location.tags,
        &location.atmosphere,
        shop_type,
    );
    let Some(raw) = aux_model_reply(&state.client, &model, &prompt, "Генерация NPC").await else {
        return (StatusCode::BAD_GATEWAY, "Ollama недоступен").into_response();
    };
    match crate::rpg::npcs::commit_generated(&raw, &world.current_location) {
        Ok(npc) => {
            invalidate_rpg_cache().await;
            match serde_json::to_value(&npc) {
                Ok(npc_json) => Json(json!({ "ok": true, "npc": npc_json })).into_response(),
                Err(e) => {
                    (StatusCode::INTERNAL_SERVER_ERROR, format!("NPC не сериализован: {e}"))
                        .into_response()
                }
            }
        }
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

/// POST /api/cloufam/rpg/quests/generate — ask the model to produce a quest in
/// the current location (V-03: quests were read-only). Runs on an axum
/// async-runtime worker: one awaited model call; `commit_generated` is
/// fail-closed the same way as for NPC.
async fn rpg_quest_generate(State(state): State<Arc<GatewayState>>) -> Response {
    let world = crate::rpg::world::load();
    if !crate::rpg::world::started() {
        return (StatusCode::CONFLICT, crate::rpg::world::NOT_STARTED).into_response();
    }
    let Some(location) = world.current() else {
        return (StatusCode::CONFLICT, "текущая локация не найдена").into_response();
    };
    let model = evaluator_model().await;
    if model.is_empty() {
        return (StatusCode::SERVICE_UNAVAILABLE, "модель не настроена").into_response();
    }
    let store = crate::rpg::npcs::load();
    let present = crate::rpg::npcs::active_npcs_in_location(&store, &world.current_location);
    let names: Vec<String> = present.iter().map(|n| n.name.clone()).collect();
    let details: Vec<(String, String)> = present
        .iter()
        .map(|n| (n.id.clone(), n.name.clone()))
        .collect();
    let prompt = crate::rpg::quests::generation_prompt(&location.name, &names, &details, None);
    let Some(mut raw) = aux_model_reply(&state.client, &model, &prompt, "Генерация квеста").await else {
        return (StatusCode::BAD_GATEWAY, "Ollama недоступен").into_response();
    };
    let mut parsed = crate::rpg::quests::commit_generated(&raw);
    if parsed.is_err() {
        // The model sometimes answers in prose or without objectives. One retry with a direct
        // JSON demand — the same policy as the state evaluator; we write the raw
        // reply to the log, otherwise a refusal is indistinguishable from "model unavailable".
        crate::boot_log(&format!(
            "Генерация квеста: первый ответ не разобран: {}",
            raw.chars().take(400).collect::<String>()
        ));
        let retry_prompt = format!(
            "{prompt}\n\nПредыдущий ответ не разобрался как JSON с целями стадий. \
             Верни только JSON по схеме, без markdown и пояснений."
        );
        if let Some(second) = aux_model_reply(&state.client, &model, &retry_prompt, "Генерация квеста (повтор)").await {
            raw = second;
            parsed = crate::rpg::quests::commit_generated(&raw);
        }
        if parsed.is_err() {
            crate::boot_log(&format!(
                "Генерация квеста: повтор тоже не разобран: {}",
                raw.chars().take(400).collect::<String>()
            ));
        }
    }
    match parsed {
        Ok(quest) => {
            invalidate_rpg_cache().await;
            match serde_json::to_value(&quest) {
                Ok(quest_json) => Json(json!({ "ok": true, "quest": quest_json })).into_response(),
                Err(e) => {
                    (StatusCode::INTERNAL_SERVER_ERROR, format!("Квест не сериализован: {e}"))
                        .into_response()
                }
            }
        }
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

/// Contract for `POST /api/cloufam/rpg/trade`: ids are store keys and end
/// up in messages, so they get the same bounding as the avatar key (V-32);
/// quantity is clamped instead of rejected (panel sliders overshoot).
struct TradeRequest {
    npc_id: String,
    /// Absent → the call only asks the merchant for a bargaining line.
    item_id: Option<String>,
    quantity: i32,
    /// Acting character card, validated like every other avatar key.
    avatar: Option<String>,
    talk: bool,
}

/// Contract-level clamp for RPG store keys (npc/item/quest/location ids): they
/// end up in store keys and messages, so non-empty and bounded is enough — the
/// same spirit as `check_avatar` (V-32).
fn checked_rpg_id(raw: &str) -> Result<String, (StatusCode, String)> {
    let id = raw.trim().to_string();
    if id.is_empty() || id.len() > 200 || id.contains(char::is_control) {
        return Err((StatusCode::BAD_REQUEST, "некорректный идентификатор".into()));
    }
    Ok(id)
}

fn parse_trade_request(body: &Value) -> Result<TradeRequest, (StatusCode, String)> {
    let npc_id = checked_rpg_id(body.get("npc").and_then(Value::as_str).unwrap_or_default())?;
    let item_id = match body.get("item").and_then(Value::as_str) {
        Some(item) if !item.trim().is_empty() => Some(checked_rpg_id(item)?),
        _ => None,
    };
    let quantity = body
        .get("quantity")
        .and_then(Value::as_i64)
        .unwrap_or(1)
        .clamp(1, 99) as i32;
    let avatar = match body.get("avatar").and_then(Value::as_str) {
        Some(raw) if !raw.trim().is_empty() => Some(checked_avatar_key(raw)?),
        _ => None,
    };
    let talk = body.get("talk").and_then(Value::as_bool).unwrap_or(false);
    Ok(TradeRequest { npc_id, item_id, quantity, avatar, talk })
}

/// POST /api/cloufam/rpg/trade — buying from an NPC in kind and/or the merchant's
/// line (V-03/V-05/V-57: the trade mechanic existed only in the module's
/// tests). Body: `{"npc": id, "item": id?, "quantity": 1..=99?, "avatar"?:
/// card.png, "talk"?: bool}`. Without `item` the call only produces the
/// merchant's bargaining line (`trade_prompt` → model); with `item` it buys
/// through `commit_purchase` — price `final_price` (location + friendship),
/// paid with the player's valuables by `item_value`, fail-closed via
/// `update_if_ok`.
/// Runs on an axum async-runtime worker: one short store commit plus at most
/// one awaited model call for the merchant line.
async fn rpg_trade(
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<Value>,
) -> Response {
    let request = match parse_trade_request(&body) {
        Ok(request) => request,
        Err((status, e)) => return (status, e).into_response(),
    };
    if !crate::rpg::world::started() {
        return (StatusCode::CONFLICT, crate::rpg::world::NOT_STARTED).into_response();
    }
    let world = crate::rpg::world::load();
    let location_tags = world.current().map(|loc| loc.tags.clone()).unwrap_or_default();
    // Friendship of the merchant NPC with the player drives the price
    // (`relationship_price_mult` inside `final_price`).
    let (friendship, npc_name, trader_description) = {
        let npcs = crate::rpg::npcs::load();
        match npcs.get(&request.npc_id) {
            Some(npc) => (
                merchant_friendship(npc, &load_relationships(), request.avatar.as_deref()),
                npc.name.clone(),
                npc.appearance.clone(),
            ),
            None => (0.0, request.npc_id.clone(), String::new()),
        }
    };

    let paid = if let Some(item_id) = &request.item_id {
        // A world started before this version has no counters — we seed them.
        let npcs = crate::rpg::npcs::load();
        ensure_shop_shelves(&world, &npcs);
        // Typed 404s from a snapshot; the commit re-checks everything itself.
        let snapshot = crate::rpg::inventory::load();
        match snapshot.npcs.get(&request.npc_id) {
            None => {
                return (StatusCode::NOT_FOUND, format!("У {} нет прилавка", request.npc_id))
                    .into_response()
            }
            Some(stock) if !stock.iter().any(|i| i.id == *item_id && i.quantity != 0) => {
                return (StatusCode::NOT_FOUND, format!("«{item_id}» нет в наличии")).into_response()
            }
            Some(_) => {}
        }
        let outcome = crate::rpg::inventory::update_if_ok(|store| {
            crate::rpg::inventory::commit_purchase(
                store,
                &request.npc_id,
                item_id,
                request.quantity,
                &location_tags,
                friendship,
            )
        });
        // `update_if_ok`: outer error — store/IO, inner — the purchase
        // decision (funds or stock short → nothing is written).
        match outcome {
            Ok(Ok(paid)) => {
                invalidate_rpg_cache().await;
                Some(paid)
            }
            Ok(Err(e)) => return (StatusCode::CONFLICT, e).into_response(),
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        }
    } else {
        None
    };

    // The merchant's line: always for a dialogue-only call, optionally
    // (`talk`) alongside a purchase — a failed line never hides a completed
    // purchase.
    let want_line = request.talk || request.item_id.is_none();
    let mut line = None;
    if want_line {
        let model = evaluator_model().await;
        if model.is_empty() {
            if paid.is_none() {
                return (StatusCode::SERVICE_UNAVAILABLE, "модель не настроена").into_response();
            }
        } else {
            let snapshot = crate::rpg::inventory::load();
            let npc_items = snapshot.npcs.get(&request.npc_id).cloned().unwrap_or_default();
            let mut prompt = crate::rpg::inventory::trade_prompt(
                &npc_name,
                &npc_items,
                &snapshot.player,
                friendship,
                &location_tags,
                &trader_description,
            );
            if let Some(avatar) = &request.avatar {
                prompt = format!("{prompt}\nПокупатель: {}.", crate::traits::display_name(avatar));
            }
            line = aux_model_reply(&state.client, &model, &prompt, "Диалог торговца").await;
            if line.is_none() && paid.is_none() {
                return (StatusCode::BAD_GATEWAY, "Ollama недоступен").into_response();
            }
        }
    }
    Json(json!({ "ok": true, "paid": paid, "line": line })).into_response()
}

/// POST /api/cloufam/rpg/skills/custom — a custom skill bound
/// to a stat (changelog "own skills"; V-57: `skills::add_custom_skill`
/// had no caller). Body: `{"avatar": card.png, "name": "...",
/// "stat_binding": "Strength"}`; the length clamps are inside the module.
/// Runs on an axum async-runtime worker; one short store commit.
async fn rpg_skills_custom(Json(body): Json<Value>) -> Response {
    let avatar = match checked_avatar_key(
        body.get("avatar").and_then(Value::as_str).unwrap_or_default(),
    ) {
        Ok(avatar) if !avatar.is_empty() => avatar,
        Ok(_) => return (StatusCode::BAD_REQUEST, "avatar required").into_response(),
        Err((status, e)) => return (status, e).into_response(),
    };
    let name = match checked_rpg_id(body.get("name").and_then(Value::as_str).unwrap_or_default()) {
        Ok(name) => name,
        Err((status, e)) => return (status, e).into_response(),
    };
    let stat_binding = match checked_rpg_id(
        body.get("stat_binding").and_then(Value::as_str).unwrap_or_default(),
    ) {
        Ok(stat_binding) => stat_binding,
        Err((status, e)) => return (status, e).into_response(),
    };
    let outcome = crate::rpg::skills::update_if_ok(|store| {
        crate::rpg::skills::add_custom_skill(store, &avatar, &name, &stat_binding)
    });
    match outcome {
        Ok(Ok(())) => {
            invalidate_rpg_cache().await;
            Json(json!({ "ok": true })).into_response()
        }
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, e).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}



/// NPC counters in locations with a shop. `InventoryStore::npcs` is the key by
/// which `/rpg/trade` looks for goods; there was no one to create it, and trade
/// always answered 404 "no counter", although it was promised in "What's new" and the help.
/// Returns the number of created counters.
/// Shopkeeper residents: NPCs declared by the world in a location with a shop
/// (innkeeper, merchant). An NPC generated along the plot is not a shopkeeper,
/// he is not entitled to a counter.
fn shopkeeper_ids(
    world: &crate::rpg::world::World,
    npcs: &crate::rpg::npcs::NpcStore,
) -> Vec<String> {
    world
        .locations
        .values()
        .filter(|location| location.has_shop)
        .flat_map(|location| location.npcs.iter().cloned())
        .filter(|id| npcs.contains_key(id))
        .collect()
}

fn ensure_shop_shelves(
    world: &crate::rpg::world::World,
    npcs: &crate::rpg::npcs::NpcStore,
) -> usize {
    let wanted = shopkeeper_ids(world, npcs);
    if wanted.is_empty() {
        return 0;
    }
    let current = crate::rpg::inventory::load();
    let missing: Vec<String> = wanted
        .into_iter()
        .filter(|id| !current.npcs.contains_key(id))
        .collect();
    if missing.is_empty() {
        return 0;
    }
    let outcome = crate::rpg::inventory::update_if_ok(|store| {
        let mut added = 0;
        for id in &missing {
            if store
                .npcs
                .insert(id.clone(), crate::rpg::inventory::default_shop_items())
                .is_none()
            {
                added += 1;
            }
        }
        Ok::<usize, String>(added)
    });
    match outcome {
        Ok(Ok(added)) => added,
        Ok(Err(e)) | Err(e) => {
            crate::boot_log(&format!("RPG прилавки: {e}"));
            0
        }
    }
}

/// The merchant's friendship with the player. `relationship_with_player` is written by no one;
/// the relationship evaluator and quest rewards accumulate axes in relationships.json per "card — NPC" pair
/// (quest rewards — by NPC id), so the price reads the same registry.
fn load_relationships() -> crate::relationships::RelationshipStore {
    crate::relationships::RelationshipStore::load(&crate::paths::data_dir().join("relationships.json"))
}

/// `rels` is loaded by the caller: the counter computes the price for every item, so the file is read once.
fn merchant_friendship(
    npc: &crate::rpg::npcs::Npc,
    rels: &crate::relationships::RelationshipStore,
    avatar: Option<&str>,
) -> f32 {
    let stored = avatar.map_or(0.0, |avatar| {
        let me = crate::traits::display_name(avatar);
        let by_id = rels.get(&me, &npc.id).friendship;
        if by_id != 0.0 { by_id } else { rels.get(&me, &npc.name).friendship }
    });
    (npc.relationship_with_player.friendship + stored).clamp(-100.0, 100.0)
}

/// GET /api/cloufam/rpg/inventory — the player's inventory and the counters of the current
/// location (what and for how much can be bought; the purchase — `POST /rpg/trade`).
async fn rpg_inventory(uri: axum::http::Uri) -> Response {
    let avatar = parse_avatar(&uri);
    let world = crate::rpg::world::load();
    let npcs = crate::rpg::npcs::load();
    // Lazy seeding: the world may have been started before counters appeared.
    ensure_shop_shelves(&world, &npcs);
    let store = crate::rpg::inventory::load();
    let items: Vec<Value> = store
        .player
        .iter()
        .map(|item| {
            json!({
                "id": item.id,
                "name": item.name,
                "description": item.description,
                // The key (common…legendary) — for the panel, the name — for display.
                "rarity": item.rarity,
                "rarity_name": item.rarity.display_name(),
                "quantity": item.quantity,
                "tags": item.tags,
            })
        })
        .collect();
    let mut shops: Vec<Value> = Vec::new();
    if crate::rpg::world::started() {
        let location_tags = world.current().map(|loc| loc.tags.clone()).unwrap_or_default();
        let rels = load_relationships();
        for npc in npcs.values() {
            if npc.current_location != world.current_location {
                continue;
            }
            let Some(shelf) = store.npcs.get(&npc.id) else { continue };
            let stock: Vec<Value> = shelf
                .iter()
                .filter(|item| item.quantity != 0)
                .map(|item| {
                    json!({
                        "id": item.id,
                        "name": item.name,
                        "description": item.description,
                        "rarity": item.rarity,
                        "rarity_name": item.rarity.display_name(),
                        "price": crate::rpg::inventory::final_price(
                            item,
                            &location_tags,
                            merchant_friendship(npc, &rels, Some(avatar.as_str()).filter(|a| !a.is_empty())),
                        ),
                    })
                })
                .collect();
            if stock.is_empty() {
                continue;
            }
            shops.push(json!({ "npc": npc.id, "name": npc.name, "items": stock }));
        }
    }
    Json(json!({ "items": items, "count": items.len(), "shops": shops })).into_response()
}

/// GET /api/cloufam/rpg/combat — the current combat; without combat — the list of enemies
/// with which it can be started.
async fn rpg_combat() -> Response {
    match crate::rpg::enemies::load_combat() {
        Some(combat) => {
            let result_str = combat.result().map(|r| r.key());
            Json(json!({
                "active": true,
                "over": combat.is_over(),
                "result": result_str,
                "enemy": {
                    "name": combat.enemy.name,
                    "hp": combat.enemy_hp.max(0),
                    "max_hp": combat.enemy.hp,
                    "attack": combat.enemy.attack,
                    "defense": combat.enemy.defense,
                    "is_boss": combat.enemy.is_boss,
                },
                "player_hp": combat.player_hp.max(0),
                "player_max_hp": combat.player_max_hp.max(1),
                "mode": match combat.mode {
                    crate::rpg::enemies::CombatMode::Fatal => "fatal",
                    crate::rpg::enemies::CombatMode::NonLethal => "non_lethal",
                    crate::rpg::enemies::CombatMode::Negotiation => "negotiation",
                },
                "round": combat.round,
                "log": combat.log,
                // In non-lethal combat the enemy's HP does not change until surrender — progress is visible
                // only by stats.
                "enemy_stats": combat.enemy_stats,
                "companion_assists": combat.companion_assists,
                "companion_name": combat.companion_name,
            }))
            .into_response()
        }
        None => {
            let enemies: Vec<Value> = crate::rpg::enemies::default_enemies()
                .into_iter()
                .map(|(id, e)| json!({
                    "id": id,
                    "name": e.name,
                    "hp": e.hp,
                    "attack": e.attack,
                    "defense": e.defense,
                    "is_boss": e.is_boss,
                    "negotiable": e.negotiable,
                }))
                .collect();
            Json(json!({
                "active": false,
                "world_started": crate::rpg::world::started(),
                "enemies": enemies,
            }))
            .into_response()
        }
    }
}

/// POST /api/cloufam/rpg/combat/start — start combat with an enemy from
/// `default_enemies`. Body: `{"enemy": "<id>", "mode": "fatal|non_lethal|negotiation"}`.
async fn rpg_combat_start(Json(body): Json<Value>) -> Response {
    let _combat_guard = COMBAT_ACTION.lock().await;
    if !crate::rpg::world::started() {
        return (StatusCode::CONFLICT, "RPG-мир не начат: начните его во вкладке «Мир»").into_response();
    }
    if crate::rpg::enemies::load_combat().is_some_and(|c| !c.is_over()) {
        return (StatusCode::CONFLICT, "бой уже идёт").into_response();
    }
    let enemy_id = body["enemy"].as_str().unwrap_or_default();
    let Some(enemy) = crate::rpg::enemies::default_enemies().remove(enemy_id) else {
        return (StatusCode::BAD_REQUEST, "неизвестный противник").into_response();
    };
    let Ok(mode) = serde_json::from_value::<crate::rpg::enemies::CombatMode>(body["mode"].clone()) else {
        return (StatusCode::BAD_REQUEST, "неизвестный режим боя").into_response();
    };
    // With someone who does not negotiate, negotiations cannot be won — only lost
    // under counterattacks.
    if mode == crate::rpg::enemies::CombatMode::Negotiation && !enemy.negotiable {
        return (StatusCode::BAD_REQUEST, format!("{} не идёт на переговоры", enemy.name)).into_response();
    }
    let mut combat = crate::rpg::enemies::ActiveCombat::new(enemy, mode);
    // V-04: stats and skills now feed the combat math — the prompt promised it
    // while `player_hp`/`player_defense` were hardcoded. `player_power` derives
    // max HP and defense from the active character; `set_player` installs them
    // without marking the player's own card as a companion.
    if let Some(avatar) = active_avatar() {
        let power = crate::rpg::enemies::player_power(
            &crate::rpg::stats::load(),
            &crate::rpg::skills::load(),
            &avatar,
        );
        combat.set_player(power);
        // The combat owner survives a card switch: the victory experience goes to him.
        combat.owner = Some(avatar);
    }
    match crate::rpg::enemies::save_combat(Some(&combat)) {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// POST /api/cloufam/rpg/combat/end — remove the combat: finished or interrupted
/// by the player. Before, nobody removed a finished combat.
async fn rpg_combat_end() -> Response {
    let _combat_guard = COMBAT_ACTION.lock().await;
    match crate::rpg::enemies::save_combat(None) {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// Award combat experience in the background (V-07: the battle outcome used to
/// affect nothing — `combat_reward` was dead). The module contract:
/// `combat_reward` sizes the pot from the combat mode, `combat_xp_prompt`
/// returns per-stat relevance (0–100, parsed by `stats::parse_stat_deltas`),
/// and the pot splits across the stats by those weights. Runs after the final
/// round is already saved and shown; fail closed on every error so bookkeeping
/// can neither delay nor break the combat response.
fn spawn_combat_reward(
    client: reqwest::Client,
    avatar: String,
    reward: u32,
    enemy_name: String,
    mode: &'static str,
    rounds: u32,
) {
    tauri::async_runtime::spawn(async move {
        if reward == 0 {
            return;
        }
        let model = evaluator_model().await;
        if model.is_empty() {
            return;
        }
        let port = TARGET_OLLAMA_PORT.load(Ordering::Relaxed);
        let prompt = crate::rpg::quests::combat_xp_prompt(&enemy_name, mode, rounds);
        let num_ctx = APP_HANDLE.get().map_or(AUX_CONTEXT, |app| live_num_ctx(app, &model));
        let body = aux_chat_body(&model, &prompt, num_ctx, json!({ "temperature": 0.2, "num_predict": 200 }));
        let resp = match client
            .post(format!("http://127.0.0.1:{port}/api/chat"))
            .json(&body)
            .timeout(std::time::Duration::from_secs(120))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => resp,
            Ok(resp) => {
                crate::boot_log(&format!(
                    "Награда за бой: Ollama HTTP {} (модель {model})",
                    resp.status().as_u16()
                ));
                return;
            }
            Err(e) => {
                crate::boot_log(&format!("Награда за бой не удалась ({model}): {e}"));
                return;
            }
        };
        let value = match resp.json::<Value>().await {
            Ok(value) => value,
            Err(e) => {
                crate::boot_log(&format!("Награда за бой: ответ не разобран ({model}): {e}"));
                return;
            }
        };
        let raw = value["message"]["content"].as_str().unwrap_or_default();
        // Empty or unparsed weights — split evenly, as with the quest reward.
        // The former divisor counted unknown names too, and the division remainder
        // was thrown away: the promised experience did not arrive.
        let weights = match crate::rpg::stats::parse_stat_deltas(raw) {
            Some(weights) => weights,
            None => {
                crate::boot_log(&format!(
                    "Награда за бой: модель {model} вернула не JSON или веса вне контракта — опыт делится поровну"
                ));
                std::collections::BTreeMap::new()
            }
        };
        if let Err(e) = crate::rpg::stats::update(|store| {
            crate::rpg::stats::award_experience(store, &avatar, &weights, reward);
        }) {
            crate::boot_log(&format!("RPG характеристики: {e}"));
        }
        invalidate_rpg_cache().await;
    });
}

/// POST /api/cloufam/rpg/combat/action — a free action in combat.
/// The player writes text, the model describes the round and picks the luck tier, while damage and
/// the enemy's counter-move are computed by the server.
/// Runs on an axum async-runtime worker; the round itself is one awaited model
/// call guarded by `COMBAT_ACTION`, the reward bookkeeping is background work.
async fn rpg_combat_action(
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<Value>,
) -> Response {
    let action_text = body["action"].as_str().unwrap_or_default().trim();
    if action_text.is_empty() {
        return (StatusCode::BAD_REQUEST, "action required").into_response();
    }

    // The lock covers the whole "read → ask the model → write" cycle: without it
    // the second action clobbers the first one's result.
    let _combat_guard = COMBAT_ACTION.lock().await;

    let mut combat = match crate::rpg::enemies::load_combat() {
        Some(c) if !c.is_over() => c,
        _ => return (StatusCode::CONFLICT, "нет активного боя").into_response(),
    };

    // The same policy as with the state evaluator: a chat model that does not fit into
    // video memory spins the round with swapping and hits the timeout.
    let model = evaluator_model().await;
    if model.is_empty() {
        return (StatusCode::SERVICE_UNAVAILABLE, "модель не настроена").into_response();
    }

    let port = TARGET_OLLAMA_PORT.load(Ordering::Relaxed);
    let prompt = crate::rpg::prompts::combat_action_prompt(&combat, action_text);
    let custom = app_settings(&state.app).map(|s| s.rpg_combat_prompt).unwrap_or_default();
    let prompt = with_custom_instructions(prompt, &custom);
    let req_body = aux_chat_body(
        &model,
        &prompt,
        live_num_ctx(&state.app, &model),
        json!({ "temperature": 0.7, "num_predict": 400 }),
    );

    let Ok(resp) = state.client
        .post(format!("http://127.0.0.1:{port}/api/chat"))
        .json(&req_body)
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await
    else {
        return (StatusCode::BAD_GATEWAY, "Ollama недоступен").into_response();
    };
    // 404 "model not found" otherwise reached the parser and looked like
    // "failed to parse the model reply".
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        return (StatusCode::BAD_GATEWAY, format!("Ollama: HTTP {status} (модель {model})")).into_response();
    }

    let Ok(value) = resp.json::<Value>().await else {
        return (StatusCode::BAD_GATEWAY, "некорректный ответ Ollama").into_response();
    };

    let raw = value["message"]["content"].as_str().unwrap_or_default();
    let Some(mut result) = crate::rpg::prompts::parse_combat_action(raw, combat.mode) else {
        return (StatusCode::BAD_GATEWAY, "не удалось разобрать ответ модели").into_response();
    };
    // Negotiations are run by the character's Persuasion skill: the model evaluates only
    // the argument, while the ability is in the progression. The combat owner is set at start
    // (rpg_combat_start); a combat from an old version without an owner is computed from
    // the current card.
    if combat.mode == crate::rpg::enemies::CombatMode::Negotiation {
        let skill = combat
            .owner
            .clone()
            .or_else(active_avatar)
            .map(|avatar| crate::rpg::skills::level_for(&avatar, "Persuasion"))
            .unwrap_or(1);
        result.persuasion_level =
            crate::rpg::prompts::effective_persuasion(skill, result.persuasion_level);
    }

    // A bribe debits valuables before the combat is written: if the combat is not saved, the inventory
    // snapshot returns what was debited — otherwise repeating the action would debit again.
    let inventory_before = (combat.mode == crate::rpg::enemies::CombatMode::Negotiation
        && result.bribe_amount > 0)
        .then(crate::rpg::inventory::load);
    crate::rpg::prompts::apply_combat_action(&mut combat, &result);

    // The client must not see new HP that is not on disk.
    if let Err(e) = crate::rpg::enemies::save_combat(Some(&combat)) {
        if let Some(before) = inventory_before {
            if let Err(restore) = crate::rpg::inventory::update_if_ok(|store| {
                *store = before.clone();
                Ok::<(), String>(())
            }) {
                crate::boot_log(&format!("Подкуп: откат инвентаря не сохранён: {restore}"));
            }
        }
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("бой не сохранён: {e}")).into_response();
    }
    invalidate_rpg_cache().await;

    let is_over = combat.is_over();
    let combat_result = combat.result().map(|r| r.key());

    // V-07: the outcome now matters — experience is awarded only for a won
    // battle (a defeat gives nothing), and only once: the next action on a
    // finished combat gets 409 "no active combat".
    let won = matches!(
        combat.result(),
        Some(
            crate::rpg::enemies::CombatResult::EnemyDefeated
                | crate::rpg::enemies::CombatResult::NonLethalWin
        )
    );
    if is_over && won {
        if let Some(avatar) = combat.owner.clone().or_else(active_avatar) {
            let reward = crate::rpg::enemies::combat_reward(&combat);
            let mode = match combat.mode {
                crate::rpg::enemies::CombatMode::Fatal => "фатальная",
                crate::rpg::enemies::CombatMode::NonLethal => "бытовая (несмертельная)",
                crate::rpg::enemies::CombatMode::Negotiation => "переговоры",
            };
            spawn_combat_reward(
                state.client.clone(),
                avatar,
                reward,
                combat.enemy.name.clone(),
                mode,
                combat.round,
            );
        }
    }

    Json(json!({
        "description": result.description,
        "round": combat.round,
        "enemy_hp": combat.enemy_hp.max(0),
        "player_hp": combat.player_hp.max(0),
        "over": is_over,
        "result": combat_result,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_prompt_budget_rejects_irreducible_history() {
        let messages = json!([
            {"role": "system", "content": "Keep the character."},
            {"role": "user", "content": "x".repeat(12_000)},
            {"role": "assistant", "content": "x".repeat(12_000)},
            {"role": "user", "content": "x".repeat(12_000)},
            {"role": "assistant", "content": "x".repeat(12_000)}
        ]);
        let kept = checked_prompt_messages(messages, 4096).expect("long tail must not lock the chat");
        let kept = kept.as_array().unwrap();
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0]["role"], "system");
        assert_eq!(kept[1]["role"], "assistant");
        let one = json!([{"role": "system", "content": "s"}, {"role": "user", "content": "x".repeat(15_000)}]);
        assert!(checked_prompt_messages(one, 4096).is_err());
        let system = json!([{"role": "system", "content": "я".repeat(20_000)}]);
        assert!(checked_prompt_messages(system, 4096).is_err());
        let parts: Vec<_> = (0..15_000).map(|_| json!({"type":"text", "text":"ab"})).collect();
        assert!(checked_prompt_messages(json!([{"role":"user", "content":parts}]), 4096).is_err());
    }

    /// The marker must remain a piece of the real main prompt: if they
    /// diverge, `is_roleplay_request` will silently stop recognizing roleplay
    /// generations, and the character will lose live state and the repetition ban.
    #[test]
    fn roleplay_marker_is_a_substring_of_the_main_prompt() {
        assert!(
            crate::sillytavern::ROLEPLAY_MAIN_PROMPT.contains(ROLEPLAY_MARKER),
            "маркер «{ROLEPLAY_MARKER}» больше не входит в ROLEPLAY_MAIN_PROMPT"
        );
    }

    /// The card note carries the bonds too: without it the manual layer of "what he thinks
    /// of them" would not reach the model, and the SillyTavern modal would show
    /// emptiness where the record exists.
    #[test]
    fn card_notes_carry_the_bonds_block() {
        use crate::{bonds, traits};
        let avatar = "Akihoshi_Roshito.png".to_string();
        let entry = traits::CharacterTraits {
            intensity: [("Ласковость".to_string(), 60u8)].into_iter().collect(),
            ..Default::default()
        };
        let bond = bonds::Bond {
            target: "Gawr_Gura.png".to_string(),
            kind: "предатель".to_string(),
            strength: 85,
            note: "сдала на прослушивании".to_string(),
            secret: true,
        };
        let mut store = traits::Store::new();
        store.insert(avatar.clone(), entry);
        let mut bond_store = bonds::Store::new();
        bond_store.insert(avatar.clone(), vec![bond]);
        let tagged = std::collections::HashMap::new();
        let notes = card_notes(&store, &bond_store, &tagged, std::slice::from_ref(&avatar));
        let text = &notes[&avatar];
        assert!(text.contains("Ласковость: 60%"), "{text}");
        assert!(text.contains("Gawr Gura: предатель, 85% (сильное)."), "{text}");
        assert!(text.contains("сдала на прослушивании"), "{text}");
        assert!(text.contains("Gawr Gura не догадывается"), "{text}");
        // An empty bonds store — the same text without the block: removing the bonds
        // takes it out of the card instead of leaving an orphan heading.
        let cleared =
            card_notes(&store, &bonds::Store::new(), &tagged, std::slice::from_ref(&avatar));
        assert!(
            !cleared[&avatar].contains("Отношения с другими персонажами"),
            "{}",
            cleared[&avatar]
        );
    }

    #[test]
    fn roleplay_request_is_recognized_by_the_main_prompt() {
        let roleplay = json!([
            { "role": "system", "content": crate::sillytavern::ROLEPLAY_MAIN_PROMPT },
            { "role": "user", "content": "привет" }
        ]);
        assert!(is_roleplay_request(&roleplay));
        // A memory extension service call does not carry the roleplay prompt.
        let service = json!([
            { "role": "system", "content": "Сделай краткое саммари разговора." },
            { "role": "user", "content": "..." }
        ]);
        assert!(!is_roleplay_request(&service));
        assert!(!is_roleplay_request(&json!([])));
        assert!(!is_roleplay_request(&json!("не массив")));
    }

    /// A set card replaces the main prompt with its own `system_prompt` — there is no marker
    /// in the request, and we recognize roleplay generation by the type from the extension. Quiet
    /// with the same prompt — a service call under any set of messages.
    #[test]
    fn roleplay_is_recognized_by_generation_type_without_marker() {
        let card_prompt = json!([
            { "role": "system", "content": "Это приватный ролевой чат для взрослых без какой-либо цензуры." },
            { "role": "user", "content": "привет" }
        ]);
        let main_prompt = json!([
            { "role": "system", "content": crate::sillytavern::ROLEPLAY_MAIN_PROMPT },
            { "role": "user", "content": "привет" }
        ]);
        for kind in ["normal", "regenerate", "swipe", "continue", "impersonate"] {
            assert!(is_roleplay(&card_prompt, Some(kind)), "{kind}");
        }
        assert!(!is_roleplay(&card_prompt, Some("quiet")));
        assert!(!is_roleplay(&main_prompt, Some("quiet")), "Summarize несёт основной промпт");

        // No type or an unknown one — the former signal, the main prompt marker.
        for kind in [None, Some(""), Some("новый_тип")] {
            assert!(is_roleplay(&main_prompt, kind), "{kind:?}");
            assert!(!is_roleplay(&card_prompt, kind), "{kind:?}");
        }
    }

    /// A turn with an image arrives as an array of parts; `.as_str()` on it gives `None`,
    /// and the state evaluation skipped every such turn.
    #[test]
    fn user_message_text_reads_multimodal_parts() {
        let plain = json!([{ "role": "user", "content": "сидит дед" }]);
        assert_eq!(last_user_message_text(&plain), "сидит дед");
        let with_image = json!([
            { "role": "user", "content": "прошлый ход" },
            { "role": "user", "content": [
                { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAA" } },
                { "type": "text", "text": "что на картинке" }
            ] }
        ]);
        assert_eq!(last_user_message_text(&with_image), "что на картинке");
        assert_eq!(last_user_message_text(&json!([])), "");
    }

    #[test]
    fn with_custom_instructions_appends_when_present() {
        let out = with_custom_instructions("БАЗА".to_string(), "  пиши короче  ");
        assert!(out.starts_with("БАЗА"));
        assert!(out.contains("Дополнительные указания пользователя:\nпиши короче"));
    }

    #[test]
    fn with_custom_instructions_leaves_prompt_untouched_when_empty() {
        assert_eq!(with_custom_instructions("БАЗА".to_string(), "   "), "БАЗА");
        assert_eq!(with_custom_instructions("БАЗА".to_string(), ""), "БАЗА");
    }

    /// The state note must not override the director's directive at the tail:
    /// the last word before the reply is the demand to react to the event.
    #[test]
    fn state_note_yields_the_last_word_to_the_director_directive() {
        let hint = crate::director::character_reaction();
        let messages = json!([
            { "role": "user", "content": "идёт гроза" },
            { "role": "system", "content": hint },
        ]);
        let out = with_state_note(messages, "Сейчас X с Y: спокойно.");
        let list = out.as_array().unwrap();
        assert_eq!(list.len(), 3);
        assert!(message_text(&list[2]).starts_with(crate::director::DIRECTOR_HINT_PREFIX));
        assert!(message_text(&list[1]).contains("спокойно"));

        // Without a directive the note, as before, becomes the last one.
        let plain = json!([{ "role": "user", "content": "привет" }]);
        let out = with_state_note(plain, "Сейчас X с Y: спокойно.");
        let list = out.as_array().unwrap();
        assert!(message_text(list.last().unwrap()).contains("спокойно"));

        // With a language note in the same block, the directive is still last.
        let messages = json!([
            { "role": "user", "content": "идёт гроза" },
            { "role": "system", "content": hint },
        ]);
        let out = with_state_note(messages, &roleplay_notes(true, "идёт гроза", vec![None]).join("\n\n"));
        let list = out.as_array().unwrap();
        assert!(message_text(&list[2]).starts_with(crate::director::DIRECTOR_HINT_PREFIX));
    }

    /// The reply language is pinned for roleplay generation and tied to the line's language;
    /// ST service calls (Summarize, image captions, the evaluator) get no notes.
    #[test]
    fn reply_language_note_only_for_roleplay() {
        let notes = roleplay_notes(true, "привет", vec![None, Some("сейчас спокойно".into())]);
        assert!(notes[0].contains("спокойно"));
        assert!(notes[1].contains("строго на русском"), "{:?}", notes[1]);
        assert!(roleplay_notes(false, "привет", vec![Some("сейчас спокойно".into())]).is_empty());
    }

    /// A Russian line demands a Russian reply even on an English card;
    /// an English one — an English reply; a mixed/empty one follows the general rule.
    #[test]
    fn reply_language_note_follows_last_user_language() {
        let ru = reply_language_note("И что дальше?");
        assert!(ru.contains("строго на русском"), "{ru}");
        assert!(ru.contains("описание персонажа"), "{ru}");
        let en = reply_language_note("What happens next?");
        assert!(en.contains("entire reply in English"), "{en}");
        let generic = reply_language_note("…");
        assert!(generic.contains("на языке последнего сообщения"), "{generic}");
    }

    /// V-16: Ukrainian/Belarusian letters are Cyrillic — a message built only
    /// of them (the old `'А'..='я'` set missed all of them) must still pin the
    /// reply language instead of falling through to the generic note.
    #[test]
    fn reply_language_note_pins_cyrillic_for_ukrainian_and_belarusian() {
        let uk = reply_language_note("Її!");
        assert!(uk.contains("строго на русском"), "{uk}");
        let be = reply_language_note("Ўў!");
        assert!(be.contains("строго на русском"), "{be}");
    }

    /// V-32: the avatar key coming from the ST extension must pass the same
    /// validation as manual trait saves (`check_avatar`); an empty key is a
    /// legitimate "no card open" and clears the active character.
    #[test]
    fn checked_avatar_key_validates_on_the_write_path() {
        assert_eq!(
            checked_avatar_key("  Akihoshi_Roshito.png ").unwrap(),
            "Akihoshi_Roshito.png"
        );
        assert_eq!(checked_avatar_key("").unwrap(), "");
        assert_eq!(checked_avatar_key("   ").unwrap(), "");
        assert!(checked_avatar_key("a/b.png").is_err());
        assert!(checked_avatar_key("a\\b.png").is_err());
        assert!(checked_avatar_key("../traits.json").is_err());
        assert!(checked_avatar_key(&"x".repeat(201)).is_err());
    }

    /// Frontend sources under `src/` (the settings window ships from there).
    fn frontend_sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                frontend_sources(&path, out);
            } else if matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("svelte" | "ts" | "js")
            ) {
                out.push(path);
            }
        }
    }

    /// V-45: the settings tabs list has exactly one copy — `SETTINGS_TABS`
    /// here; the frontend gets it over IPC (`settings_tabs`) and keeps no
    /// fallback literal. This tripwire fails when any frontend source grows a
    /// consecutive pair of the tabs as a quoted literal (the shape of a
    /// hardcoded array), whichever quotes or whitespace it uses.
    #[test]
    fn settings_tabs_live_only_in_the_gateway() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../src");
        let mut files = Vec::new();
        frontend_sources(&root, &mut files);
        assert!(!files.is_empty(), "no frontend sources under {root:?}");
        let pairs: Vec<String> = SETTINGS_TABS
            .windows(2)
            .map(|pair| format!("'{}','{}'", pair[0], pair[1]))
            .collect();
        for path in &files {
            let Ok(text) = std::fs::read_to_string(path) else { continue };
            // Squeeze whitespace and unify quotes so the shape check does not
            // depend on formatting.
            let squeezed: String = text
                .chars()
                .filter(|c| !c.is_whitespace())
                .map(|c| if c == '"' { '\'' } else { c })
                .collect();
            for pair in &pairs {
                assert!(
                    !squeezed.contains(pair.as_str()),
                    "{path:?} hardcodes the settings tabs ({pair}): the list must live \
                     only in gateway.rs::SETTINGS_TABS and arrive over IPC"
                );
            }
        }
    }

    /// Trade request contract: ids bounded like the avatar key, quantity
    /// clamped, avatar validated; without `item` the call is dialogue-only.
    #[test]
    fn trade_request_contract_validates_and_clamps() {
        let request = parse_trade_request(&json!({ "npc": " merchant " })).unwrap();
        assert_eq!(request.npc_id, "merchant");
        assert_eq!(request.item_id, None);
        assert_eq!(request.quantity, 1);
        assert_eq!(request.avatar, None);
        assert!(!request.talk);

        let request = parse_trade_request(&json!({
            "npc": "merchant",
            "item": "health_potion",
            "quantity": 500,
            "avatar": " Akihoshi_Roshito.png ",
            "talk": true,
        }))
        .unwrap();
        assert_eq!(request.item_id.as_deref(), Some("health_potion"));
        assert_eq!(request.quantity, 99, "quantity is clamped to 1..=99");
        assert_eq!(request.avatar.as_deref(), Some("Akihoshi_Roshito.png"));
        assert!(request.talk);

        // Zero and negative quantities clamp up, not to a broken purchase.
        assert_eq!(
            parse_trade_request(&json!({ "npc": "m", "item": "i", "quantity": 0 }))
                .unwrap()
                .quantity,
            1
        );
        assert_eq!(
            parse_trade_request(&json!({ "npc": "m", "item": "i", "quantity": -7 }))
                .unwrap()
                .quantity,
            1
        );
        // An empty item is dialogue-only, not a broken purchase.
        assert_eq!(
            parse_trade_request(&json!({ "npc": "m", "item": "  " })).unwrap().item_id,
            None
        );

        // Contract violations are rejected, not silently normalized.
        assert!(parse_trade_request(&json!({})).is_err());
        assert!(parse_trade_request(&json!({ "npc": "" })).is_err());
        assert!(parse_trade_request(&json!({ "npc": "x".repeat(201) })).is_err());
        assert!(parse_trade_request(&json!({ "npc": "m", "item": "x".repeat(201) })).is_err());
        assert!(parse_trade_request(&json!({ "npc": "m", "avatar": "a/b.png" })).is_err());
        // Interior control characters are rejected (edge whitespace is trimmed).
        assert!(parse_trade_request(&json!({ "npc": "m\nx" })).is_err());
    }

    /// V-15: the prompt budget keeps at least half of the context even when the
    /// client `max_tokens` reaches or exceeds `num_ctx` — the old
    /// `saturating_sub` zeroed the budget and the dialogue was cut to four
    /// messages without ever fitting.
    #[test]
    fn prompt_budget_caps_reserve_at_half_the_context() {
        assert!(prompt_budget(8192, Some(8192)) > 0);
        assert_eq!(prompt_budget(8192, Some(8192)), 4096);
        assert_eq!(prompt_budget(8192, Some(100_000)), 4096);
        assert_eq!(prompt_budget(16384, Some(100_000)), 8192);
        // Sane requests are untouched: reserve 1000 by default, 300 as asked.
        assert_eq!(prompt_budget(8192, None), 8192 - 1000);
        assert_eq!(prompt_budget(8192, Some(300)), 8192 - 300);
        // The old 256 floor for the reserve still holds.
        assert_eq!(prompt_budget(8192, Some(0)), 8192 - 256);
        // Anything below 8192 is raised to the safe minimum first.
        assert_eq!(prompt_budget(4096, Some(100_000)), 4096);
    }

    /// A short prompt is not touched: trimming is needed only for an overflowing one.
    #[test]
    fn trim_history_keeps_short_prompts_intact() {
        let messages = json!([
            { "role": "system", "content": "правила" },
            { "role": "user", "content": "привет" },
            { "role": "assistant", "content": "здравствуй" },
        ]);
        assert_eq!(trim_history_to_fit(messages.clone(), 1000), messages);
    }

    /// On overflow the oldest dialogue lines go away, while system
    /// messages and the latest lines stay: without the system prompt the model
    /// loses its role and repeats previous messages.
    #[test]
    fn trim_history_drops_oldest_dialogue_only() {
        let mut list = vec![json!({ "role": "system", "content": "правила роли" })];
        for i in 0..40 {
            list.push(json!({ "role": "user", "content": format!("{} {}", i, "вопрос ".repeat(20)) }));
            list.push(json!({ "role": "assistant", "content": format!("{} {}", i, "ответ ".repeat(20)) }));
        }
        list.push(json!({ "role": "system", "content": "хвостовая инструкция" }));
        let trimmed = trim_history_to_fit(Value::Array(list), 2000);
        let out = trimmed.as_array().unwrap();
        let texts: Vec<String> = out
            .iter()
            .map(|m| m.get("content").and_then(|c| c.as_str()).unwrap_or_default().to_string())
            .collect();
        assert!(texts.iter().any(|t| t.contains("правила роли")), "система уцелела");
        assert!(texts.iter().any(|t| t.contains("хвостовая инструкция")), "хвост уцелел");
        assert!(texts.iter().any(|t| t.starts_with("39 ")), "последние реплики на месте");
        assert!(!texts.iter().any(|t| t.starts_with("0 ")), "старые реплики ушли");
        let kept_dialogue = out
            .iter()
            .filter(|m| matches!(m.get("role").and_then(|r| r.as_str()), Some("user" | "assistant")))
            .count();
        assert!(kept_dialogue >= 4 && kept_dialogue < 80, "диалог подрезан: {kept_dialogue}");
    }

    #[test]
    fn trace_day_bucket_groups_by_86400_seconds() {
        assert_eq!(trace_day_bucket(0), 0);
        assert_eq!(trace_day_bucket(86_399), 0);
        assert_eq!(trace_day_bucket(86_400), 1);
    }

    #[test]
    fn trace_ids_are_unique_and_sortable_as_strings() {
        let a = next_trace_id();
        let b = next_trace_id();
        assert_ne!(a, b);
        // millis-seq: with equal millis, sorting by the counter is still correct.
        if a.split('-').next() == b.split('-').next() {
            assert!(a < b, "{a} должен идти раньше {b} при равном millis");
        }
    }

    /// The second `evaluate_state` for the same pair overtakes the first by token
    /// registration — the first must recognize itself as stale.
    #[test]
    fn eval_token_second_registration_invalidates_first() {
        let avatar = "test_token_avatar.png";
        let persona = "test_token_persona";
        let first = register_eval_token(avatar, persona);
        assert!(is_eval_current(avatar, persona, first));
        let second = register_eval_token(avatar, persona);
        assert_ne!(first, second);
        assert!(!is_eval_current(avatar, persona, first), "первый токен должен устареть");
        assert!(is_eval_current(avatar, persona, second));
    }

    /// Tokens of different (avatar, persona) pairs do not affect each other.
    #[test]
    fn eval_token_is_scoped_per_pair() {
        let a = register_eval_token("test_token_a.png", "Alex");
        let b = register_eval_token("test_token_b.png", "Alex");
        assert!(is_eval_current("test_token_a.png", "Alex", a));
        assert!(is_eval_current("test_token_b.png", "Alex", b));
    }

    /// A group chat prompt: history with participant names and the ST hint.
    fn group_messages() -> Value {
        json!([
            { "role": "system", "content": "Ты отыгрываешь персонажа Bella Stone." },
            { "role": "assistant", "content": "Alice Carter: *Алиса улыбается.*" },
            { "role": "assistant", "content": "Bella Stone: *Белла замирает у часов.*" },
            { "role": "user", "content": "Расскажите, чем заняты." },
            { "role": "system", "content": "[Write the next reply only as Bella Stone.]" }
        ])
    }

    #[test]
    fn test_known_speakers_collected_from_history() {
        let names = known_speakers(&group_messages());
        assert_eq!(names, vec!["Alice Carter".to_string(), "Bella Stone".to_string()]);
    }

    #[test]
    fn test_leading_speaker_name_ignores_prose_colon() {
        assert_eq!(leading_speaker_name("Alice Carter: текст"), Some("Alice Carter"));
        assert_eq!(leading_speaker_name("\n\nBella Stone: текст"), Some("Bella Stone"));
        // A colon in prose is not a speaker name.
        assert_eq!(leading_speaker_name("*Она вздыхает: «Ну и день»*"), None);
        assert_eq!(leading_speaker_name("— Сегодня: спокойно."), None);
        // Without asterisks and dashes this is still prose: the second word is lowercase.
        assert_eq!(leading_speaker_name("Она вздыхает: «Ну и день»"), None);
        assert_eq!(leading_speaker_name("Алиса: привет"), Some("Алиса"));
    }

    /// The prose "She sighs:" does not become a room participant and does not
    /// cut the next line at the newline.
    #[test]
    fn test_guard_does_not_stop_on_prose_colon() {
        let messages = json!([
            { "role": "assistant", "content": "Алиса: *кивает.*" },
            { "role": "assistant", "content": "Она вздыхает: тихо." },
            { "role": "user", "content": "ещё" },
        ]);
        assert_eq!(known_speakers(&messages), vec!["Алиса".to_string()]);
        let mut guard = SpeakerGuard::new(&messages);
        let out = format!(
            "{}{}",
            guard.feed("Я рада.\nОна вздыхает: ещё сильнее"),
            guard.flush()
        );
        assert!(out.contains("ещё сильнее"), "реплика обрезана: {out}");
        assert!(!guard.is_stopped());
    }

    /// Stopping generation releases the words the echo guard still held.
    #[test]
    fn test_echo_absorb_flushes_hold_when_generation_stops() {
        let messages = json!([
            { "role": "user", "content": "привет" },
            { "role": "assistant", "content": "старая реплика" },
        ]);
        let mut echo = crate::repetition::EchoGuard::new(&messages, None);
        let (shown, cut) = echo_absorb(&mut echo, "короткий ответ", true);
        assert!(!cut);
        assert_eq!(shown, "короткий ответ");
    }

    #[test]
    fn test_guard_strips_foreign_name_prefix() {
        let messages = group_messages();
        let mut guard = SpeakerGuard::new(&messages);
        // The model answered for Alice although ST expects Bella: without stripping the prefix
        // ST would trim the line entirely (cleanGroupMessage) and the character would stay silent.
        let first = guard.feed("Alice Carter: *Алиса садится на ковёр.*");
        let rest = guard.feed(" — Сегодня я отдыхала.");
        let out = format!("{first}{rest}");
        assert!(out.starts_with("*Алиса садится"), "получено: {out}");
        assert!(!out.contains("Alice Carter:"), "чужое имя осталось: {out}");
        assert!(out.ends_with("отдыхала."));
    }

    #[test]
    fn test_guard_strips_own_name_prefix_too() {
        // ST strips its own name at the start of the reply itself (allow_name2_display is off),
        // so we cut any room participant's prefix — this way the line does not
        // depend on whose name the model substituted.
        let messages = group_messages();
        let mut guard = SpeakerGuard::new(&messages);
        let out = format!(
            "{}{}",
            guard.feed("Bella Stone: *Белла смотрит на часы.*"),
            guard.flush()
        );
        assert_eq!(out, "*Белла смотрит на часы.*");
    }

    #[test]
    fn test_guard_keeps_plain_prose() {
        let messages = group_messages();
        let mut guard = SpeakerGuard::new(&messages);
        let mut out = String::new();
        for chunk in ["*Белла ", "поправляет ", "платье", "*", " и улыбается."] {
            out.push_str(&guard.feed(chunk));
        }
        out.push_str(&guard.flush());
        assert_eq!(out, "*Белла поправляет платье* и улыбается.");
    }

    #[test]
    fn test_guard_is_inert_without_group_history() {
        let messages = json!([
            { "role": "assistant", "content": "*Просто реплика без имени.*" },
            { "role": "user", "content": "привет" }
        ]);
        let mut guard = SpeakerGuard::new(&messages);
        assert_eq!(guard.feed("Alice Carter: текст"), "Alice Carter: текст");
    }

    #[test]
    fn test_guard_ignores_user_name_prefix() {
        // The user's name does not get into known_speakers: ST trims the line
        // only by the room characters' names, so there is no text loss here and
        // there is no reason to touch the prefix.
        let messages = json!([
            { "role": "assistant", "content": "Alice Carter: *машет рукой*" },
            { "role": "user", "content": "привет" },
            { "role": "system", "content": "[Write the next reply only as Alice Carter.]" }
        ]);
        let mut guard = SpeakerGuard::new(&messages);
        assert_eq!(guard.feed("Алекс: *машет рукой*"), "Алекс: *машет рукой*");
    }

    #[tokio::test]
    async fn test_lines_from_bytes_utf8_split() {
        let text = "Привет, мир!\nВторая строка с эмодзи 🚀\n";
        // Split the string into chunks of exactly 1 byte, cutting every multi-byte UTF-8 character
        let byte_chunks: Vec<Result<bytes::Bytes, std::io::Error>> = text
            .as_bytes()
            .iter()
            .map(|&b| Ok(bytes::Bytes::copy_from_slice(&[b])))
            .collect();
        let stream = futures_util::stream::iter(byte_chunks);
        let mut lines = Box::pin(lines_from_bytes(stream, Arc::new(std::sync::Mutex::new(None))));

        let mut collected = Vec::new();
        while let Some(line) = lines.next().await {
            collected.push(line);
        }

        assert_eq!(collected.len(), 2);
        assert_eq!(collected[0], "Привет, мир!");
        assert_eq!(collected[1], "Вторая строка с эмодзи 🚀");
        assert!(!collected[0].contains('\u{FFFD}'));
        assert!(!collected[1].contains('\u{FFFD}'));
    }

    #[test]
    fn test_estimate_tokens_counts_cyrillic_denser() {
        assert_eq!(estimate_tokens(&"a".repeat(3000)), 1000);
        assert_eq!(estimate_tokens(&"я".repeat(3000)), 1500);
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(
            estimate_tokens(&format!("{}{}", "a".repeat(300), "я".repeat(300))),
            100 + 150
        );
    }

    /// A counter — only for world residents in a location with a shop; an NPC generated
    /// along the plot does not become a merchant.
    #[test]
    fn test_shopkeeper_ids_follow_world_roster() {
        let world = crate::rpg::world::World::default();
        let mut npcs = crate::rpg::npcs::NpcStore::new();
        for id in ["tavern_keeper", "merchant", "hero"] {
            npcs.insert(
                id.to_string(),
                crate::rpg::npcs::Npc {
                    id: id.to_string(),
                    name: id.to_string(),
                    appearance: String::new(),
                    personality_tags: Vec::new(),
                    current_location: "tavern".to_string(),
                    dialogue_state: Default::default(),
                    relationship_with_player: Default::default(),
                    alive: true,
                    shop_inventory: None,
                },
            );
        }
        let ids = shopkeeper_ids(&world, &npcs);
        assert!(ids.contains(&"tavern_keeper".to_string()), "{ids:?}");
        assert!(ids.contains(&"merchant".to_string()), "{ids:?}");
        assert!(!ids.contains(&"hero".to_string()), "{ids:?}");
    }

    /// A base64 image in a trace must not drag megabytes: strings are clipped,
    /// the message shape and short fields stay.
    #[test]
    fn test_clipped_for_trace_caps_huge_strings() {
        let value = json!({
            "messages": [{
                "role": "user",
                "content": "привет",
                "images": ["A".repeat(10_000)],
            }]
        });
        let clipped = clipped_for_trace(&value);
        let image = clipped["messages"][0]["images"][0]
            .as_str()
            .expect("картинка осталась строкой");
        assert!(image.chars().count() < 2100, "строка не обрезана: {}", image.chars().count());
        assert!(image.ends_with("символов]"));
        assert_eq!(clipped["messages"][0]["content"], "привет");
    }

    #[test]
    fn test_normalize_messages_multimodal() {
        let input = json!([
            {
                "role": "user",
                "content": [
                    { "type": "text", "text": "Опиши картинку:" },
                    { "type": "image_url", "image_url": { "url": "data:image/png;base64,mock_image_base64" } }
                ]
            }
        ]);
        let normalized = normalize_messages(&input);
        let arr = normalized.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["role"], "user");
        assert_eq!(arr[0]["content"], "Опиши картинку:");
        assert_eq!(arr[0]["images"].as_array().unwrap().len(), 1);
        assert_eq!(arr[0]["images"][0], "mock_image_base64");
    }

    #[test]
    fn test_normalize_messages_text_only() {
        let input = json!([
            { "role": "system", "content": "Ты персонаж" },
            { "role": "user", "content": "Привет!" }
        ]);
        let normalized = normalize_messages(&input);
        assert_eq!(normalized, input);
    }

    #[test]
    fn test_build_ollama_request_options() {
        let body = json!({
            "model": "huihui_ai/qwen3-abliterated:8b",
            "messages": [{ "role": "user", "content": "Привет" }],
            "stop": ["", "  ", "\nUser:", "  \nUser:  ", "\nВы:", "\nТы:", "\nДополнительно:"]
        });
        let req = build_ollama_request(&body, "huihui_ai/qwen3-abliterated:8b".into(), body["messages"].clone(), 32768);
        let opts = req.get("options").expect("options must exist");
        assert_eq!(opts.get("repeat_penalty").and_then(|v| v.as_f64()), Some(1.12));
        assert_eq!(opts.get("repeat_last_n").and_then(|v| v.as_i64()), Some(1024));
        assert_eq!(opts.get("num_ctx").and_then(|v| v.as_u64()), Some(32768));
        assert_eq!(opts.get("temperature").and_then(|v| v.as_f64()), Some(0.75));

        let stops = opts.get("stop").and_then(|v| v.as_array()).expect("stop must be array");
        // Limit 4, duplicates (including ones with spaces) and empty lines are filtered out
        assert_eq!(stops.len(), 4);
        assert_eq!(stops[0], "\nUser:");
        assert_eq!(stops[1], "\nВы:");
        assert_eq!(stops[2], "\nТы:");
        assert_eq!(stops[3], "\nДополнительно:");
    }

    /// Chat, evaluator, combat, director, tags, and draft parsing on the chat model send
    /// the same num_ctx: otherwise Ollama reloads the model between calls
    /// (live log 25.09.2026 — four loads in 28 s: 27392, 4096, 8192, 24320).
    #[test]
    fn test_num_ctx_same_for_every_call_on_chat_model() {
        let chat_model = "huihui_ai/qwen3-abliterated:14b-v2";
        let chat_ctx = crate::models::chat_context(32_768, 24_576);
        let body = json!({ "messages": [{ "role": "user", "content": "Привет" }] });
        let chat = build_ollama_request(&body, chat_model.into(), body["messages"].clone(), chat_ctx);
        let chat_num_ctx = chat["options"]["num_ctx"].as_u64();

        for options in [
            json!({ "temperature": 0.2, "num_predict": 400 }),
            json!({ "temperature": 0.7, "num_predict": 400 }),
            json!({ "temperature": 0.8, "top_p": 0.9, "num_predict": 512, "repeat_penalty": 1.1 }),
            json!({ "temperature": 0.2, "top_p": 0.8, "num_predict": 256, "repeat_penalty": 1.05 }),
            json!({ "temperature": 0.3, "num_predict": 900 }),
        ] {
            let aux = aux_chat_body(chat_model, "промпт", num_ctx_for(chat_model, chat_model, chat_ctx), options);
            assert_eq!(aux["options"]["num_ctx"].as_u64(), chat_num_ctx);
            assert_eq!(aux["model"], chat["model"]);
        }
    }

    /// Another model (light evaluator, separate tag model) gets its own
    /// constant number, not Ollama's default 4096 — and is not reloaded either.
    #[test]
    fn test_num_ctx_for_other_model_is_constant() {
        let chat_model = "huihui_ai/qwen3-abliterated:14b-v2";
        for chat_ctx in [8_192, 20_480, 32_768] {
            assert_eq!(num_ctx_for("huihui_ai/qwen3-abliterated:4b", chat_model, chat_ctx), AUX_CONTEXT);
        }
        assert_eq!(num_ctx_for("bge-m3", "bge-m3:latest", 20_480), 20_480, "тег :latest — та же модель");
        let aux = aux_chat_body("m", "текст", AUX_CONTEXT, json!({ "temperature": 0.2 }));
        assert_eq!(aux["options"], json!({ "temperature": 0.2, "num_ctx": AUX_CONTEXT }));
        assert_eq!(aux["messages"], json!([{ "role": "user", "content": "текст" }]));
        assert_eq!(aux["stream"], false);
        assert_eq!(aux["think"], false);
    }

    /// The final Ollama chunk → counters in the trace: they show the model
    /// reload and prompt cache behavior.
    #[test]
    fn test_ollama_metrics_from_done_chunk() {
        let done = json!({
            "model": "m", "done": true, "done_reason": "stop", "total_duration": 5,
            "load_duration": 3_118_000_000u64, "prompt_eval_count": 1835, "prompt_eval_duration": 257_000_000,
            "eval_count": 20, "eval_duration": 150_000_000,
        });
        assert_eq!(
            ollama_metrics(&done),
            json!({
                "load_duration": 3_118_000_000u64, "prompt_eval_count": 1835, "prompt_eval_duration": 257_000_000,
                "eval_count": 20, "eval_duration": 150_000_000,
            })
        );
        assert_eq!(ollama_metrics(&json!({ "done": true })), json!({}));
    }

    #[tokio::test]
    async fn test_lines_from_bytes_no_trailing_newline() {
        use futures_util::StreamExt;
        let chunks: Vec<Result<bytes::Bytes, std::convert::Infallible>> = vec![
            Ok(bytes::Bytes::from("{\"msg\": \"привет\"}\n")),
            Ok(bytes::Bytes::from("{\"msg\": \"мир\"}")),
        ];
        let stream = futures_util::stream::iter(chunks);
        let lines: Vec<String> = lines_from_bytes(Box::pin(stream), Arc::new(std::sync::Mutex::new(None))).collect().await;
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "{\"msg\": \"привет\"}");
        assert_eq!(lines[1], "{\"msg\": \"мир\"}");
    }

    /// A transport break must not look like a clean end: the error slot
    /// remembers the reason, and the tail writes the trace and boot.log from it.
    #[tokio::test]
    async fn test_lines_from_bytes_records_transport_error() {
        use futures_util::StreamExt;
        let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = vec![
            Ok(bytes::Bytes::from("{\"a\":1}\n")),
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "boom",
            )),
        ];
        let slot: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
        let lines: Vec<String> = lines_from_bytes(Box::pin(futures_util::stream::iter(chunks)), slot.clone())
            .collect()
            .await;
        assert_eq!(lines, vec!["{\"a\":1}"]);
        let recorded = slot.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert!(
            recorded.as_deref().is_some_and(|e| e.contains("boom")),
            "ошибка не записана: {recorded:?}"
        );
    }

    /// Gateway panic regression: reading the reply "head" to the end reaches the end of the
    /// stream, and `chain` polls it once more. A short reply (entirely shorter than
    /// `COPY_HEAD_CHARS`) drove `unfold` into a poll after `Ready(None)` — the client
    /// got a break without `finish_reason` and `[DONE]`.
    #[tokio::test]
    async fn test_lines_from_bytes_survives_repoll_after_end() {
        use futures_util::StreamExt;
        let chunks: Vec<Result<bytes::Bytes, std::convert::Infallible>> = vec![
            Ok(bytes::Bytes::from("{\"msg\": \"раз\"}\n")),
            Ok(bytes::Bytes::from("{\"msg\": \"два\"}")),
        ];
        let mut lines: std::pin::Pin<Box<dyn futures_util::Stream<Item = String> + Send>> =
            Box::pin(lines_from_bytes(Box::pin(futures_util::stream::iter(chunks)), Arc::new(std::sync::Mutex::new(None))));
        let mut head: Vec<String> = Vec::new();
        while let Some(line) = lines.next().await {
            head.push(line);
        }
        let replayed: Vec<String> = futures_util::stream::iter(head).chain(lines).collect().await;
        assert_eq!(replayed, vec!["{\"msg\": \"раз\"}", "{\"msg\": \"два\"}"]);
    }

    #[test]
    fn test_guard_stops_on_another_speaker_midstream() {
        let messages = group_messages();
        let mut guard = SpeakerGuard::new(&messages);
        let first = guard.feed("Я рада тебя видеть!\n\nAlice Carter: Привет, как дела?");
        assert!(guard.is_stopped());
        assert_eq!(first, "Я рада тебя видеть!");
        assert_eq!(guard.feed(" Больше текста"), "");
        assert_eq!(guard.flush(), "");
    }

    #[test]
    fn test_known_speakers_extracted_from_group_nudge_prompt() {
        let messages = json!([
            { "role": "system", "content": "[Сейчас очередь говорить только у Bella Stone. Отвечай строго от лица Bella Stone от первого лица.\nКатегорически запрещено писать реплики, диалоги, слова или мысли за других персонажей (Alice Carter, Watson Amelia) или за {{user}}.\nПиши только свою реплику.]" }
        ]);
        let names = known_speakers(&messages);
        assert!(names.contains(&"Alice Carter".to_string()));
        assert!(names.contains(&"Watson Amelia".to_string()));
        assert!(names.contains(&"Bella Stone".to_string()));
    }

    /// The client sends `encodeURIComponent(avatar)` — without %XX decoding the key
    /// of a file with a space or non-ASCII name does not match the name on disk.
    #[test]
    fn parse_avatar_percent_decodes_query_value() {
        let uri: axum::http::Uri = "/api/x?avatar=Anya%20Gurl.png".parse().unwrap();
        assert_eq!(parse_avatar(&uri), "Anya Gurl.png");
        // encodeURIComponent encodes + as %2B; after decoding — a plus.
        let uri: axum::http::Uri = "/api/x?avatar=a%2Bb.png&x=1".parse().unwrap();
        assert_eq!(parse_avatar(&uri), "a+b.png");
        // ASCII without escaping passes as is.
        let uri: axum::http::Uri = "/api/x?avatar=plain.png".parse().unwrap();
        assert_eq!(parse_avatar(&uri), "plain.png");
        let uri: axum::http::Uri = "/api/x?other=1".parse().unwrap();
        assert_eq!(parse_avatar(&uri), "");
    }

    /// Only the character's replies to the player's line are evaluated. Quiet carries the same
    /// main prompt with the marker, but it is a summary or a service call.
    #[test]
    fn only_reply_generations_are_evaluated() {
        for kind in ["normal", "regenerate", "swipe"] {
            assert!(evaluates_reply(Some(kind), false), "{kind}");
            assert!(evaluates_reply(Some(kind), true), "{kind}");
        }
        for kind in ["quiet", "impersonate", "continue", ""] {
            assert!(!evaluates_reply(Some(kind), true), "{kind}");
        }
        // No type came: quiet in ST is never streamed.
        assert!(evaluates_reply(None, true));
        assert!(!evaluates_reply(None, false));
        assert_eq!(generation_type(&json!({ "lk_generation_type": "quiet" })), Some("quiet"));
        assert_eq!(generation_type(&json!({ "stream": true })), None);
    }

    #[test]
    fn exchange_key_matches_regenerate_and_splits_turns() {
        let first = json!([
            { "role": "system", "content": "карточка" },
            { "role": "assistant", "content": "Привет." },
            { "role": "user", "content": "да" }
        ]);
        // Regeneration: the same history, the system part (summary, lore) is different.
        let regenerate = json!([
            { "role": "system", "content": "карточка и свежее саммари" },
            { "role": "assistant", "content": "Привет." },
            { "role": "user", "content": "да" }
        ]);
        assert_eq!(exchange_key(&first, None), exchange_key(&regenerate, None));
        // The next turn with the same "yes" is already a different exchange.
        let next_turn = json!([
            { "role": "system", "content": "карточка" },
            { "role": "assistant", "content": "Привет." },
            { "role": "user", "content": "да" },
            { "role": "assistant", "content": "Правда?" },
            { "role": "user", "content": "да" }
        ]);
        assert_ne!(exchange_key(&first, None), exchange_key(&next_turn, None));
        // The same exchange in another chat is a new turn, not a regeneration.
        assert_ne!(exchange_key(&first, Some("chat-1")), exchange_key(&first, Some("chat-2")));
        assert_eq!(exchange_key(&first, Some("chat-1")), exchange_key(&regenerate, Some("chat-1")));
        let with_image = json!([{ "role": "user", "content": [
            { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAA" } },
            { "type": "text", "text": "что это?" }
        ] }]);
        let other_text = json!([{ "role": "user", "content": [{ "type": "text", "text": "а это?" }] }]);
        assert_ne!(exchange_key(&with_image, None), exchange_key(&other_text, None));
    }

    #[test]
    fn exchange_is_applied_once_per_pair() {
        let (avatar, persona) = ("test_exchange.png", "Алекс");
        assert!(!is_exchange_applied(avatar, persona, 7));
        assert!(claim_exchange(avatar, persona, 7));
        assert!(is_exchange_applied(avatar, persona, 7));
        assert!(!claim_exchange(avatar, persona, 7), "регенерация того же хода");
        assert!(claim_exchange(avatar, persona, 8), "следующий ход");
        assert!(!is_exchange_applied(avatar, persona, 7));
        assert!(claim_exchange("test_exchange_other.png", persona, 7), "другая пара");
    }

    fn header_map(pairs: &[(&str, &str)]) -> axum::http::HeaderMap {
        let mut map = axum::http::HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn local_request_accepts_only_loopback_pages() {
        assert!(is_local_request(&header_map(&[])));
        for origin in [
            "http://127.0.0.1:8000",
            "http://localhost:8000",
            "http://tauri.localhost",
            "tauri://localhost",
            "http://[::1]:8000",
        ] {
            assert!(is_local_request(&header_map(&[("origin", origin)])), "{origin}");
        }
        for origin in [
            "https://evil.example",
            "null",
            "http://127.0.0.1.evil.example",
            "http://localhost.evil.example:8000",
        ] {
            assert!(!is_local_request(&header_map(&[("origin", origin)])), "{origin}");
        }
        // An unprintable Origin used to be considered "no header" and passed.
        let mut garbled = axum::http::HeaderMap::new();
        garbled.insert("origin", axum::http::HeaderValue::from_bytes("http://ёж.рф".as_bytes()).unwrap());
        assert!(!is_local_request(&garbled));

        assert!(has_loopback_host(&header_map(&[("host", "127.0.0.1:11435")])));
        assert!(has_loopback_host(&header_map(&[("host", "localhost:11435")])));
        assert!(!has_loopback_host(&header_map(&[("host", "evil.example:11435")])));
        assert!(has_loopback_host(&header_map(&[])));
    }

    #[test]
    fn chat_backup_stem_strips_paths_and_traversal() {
        assert_eq!(
            chat_backup_stem("Seraphina - 2026-09-11@15h44m29s705ms.jsonl"),
            "Seraphina - 2026-09-11@15h44m29s705ms"
        );
        assert_eq!(chat_backup_stem("..\\..\\evil.jsonl"), "evil");
        assert_eq!(chat_backup_stem("../../../evil.jsonl"), "evil");
        assert_eq!(chat_backup_stem("имя/с/слэшом.jsonl"), "имя_с_слэшом");
        assert_eq!(chat_backup_stem(".."), "chat");
        assert_eq!(chat_backup_stem(".jsonl"), "chat");
    }

    /// The router layer: a foreign site and a DNS-rebound domain get 403
    /// without a CORS permission, the ST page — an echo of its own Origin instead of `*`.
    #[tokio::test]
    async fn local_only_layer_guards_every_route() {
        let router = Router::new()
            .route("/ping", get(|| async { (StatusCode::OK, CORS_HEADERS, "pong") }))
            .layer(axum::middleware::from_fn(local_only));
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let url = format!("http://{addr}/ping");

        let server_side = client.get(&url).send().await.unwrap();
        assert_eq!(server_side.status(), 200);
        assert!(server_side.headers().get("access-control-allow-origin").is_none());

        let st_page = client.get(&url).header("Origin", "http://127.0.0.1:8000").send().await.unwrap();
        assert_eq!(st_page.status(), 200);
        assert_eq!(st_page.headers()["access-control-allow-origin"], "http://127.0.0.1:8000");

        let foreign = client.get(&url).header("Origin", "https://evil.example").send().await.unwrap();
        assert_eq!(foreign.status(), 403);
        assert!(foreign.headers().get("access-control-allow-origin").is_none());

        let rebound = client
            .get(&url)
            .header("Host", format!("evil.example:{}", addr.port()))
            .send()
            .await
            .unwrap();
        assert_eq!(rebound.status(), 403);
    }

    #[test]
    fn open_settings_accepts_only_known_tabs() {
        assert_eq!(requested_settings_tab(br#"{"tab":"tags"}"#), Some("tags".to_string()));
        assert_eq!(requested_settings_tab(br#"{"tab":"../../x"}"#), None);
        assert_eq!(requested_settings_tab(br#"{"tab":"tags?x=1"}"#), None);
        assert_eq!(requested_settings_tab(b""), None);
        assert_eq!(requested_settings_tab(br#"{}"#), None);
    }

    #[test]
    fn rollback_version_and_url_are_validated() {
        assert!(is_release_version("1.2.52"));
        assert!(is_release_version("1.3.0-beta.1"));
        for bad in ["", "1.2", "1.2.52/../../x", "v1.2.52", "1.2.x", "1.2.52-", "1..2", "1.2.52?a=b"] {
            assert!(!is_release_version(bad), "{bad}");
        }
        assert!(is_trusted_release_url(
            "https://github.com/AndreyFSDev/cloufam-releases/releases/download/v1.2.52/Cloufam.exe"
        ));
        for bad in [
            "https://github.com/AndreyFSDev/cloufam-releases/releases/download/../../../evil/r/releases/download/v1/x.exe",
            "https://github.com/AndreyFSDev/cloufam-releases/releases/download/%2e%2e/%2e%2e/%2e%2e/evil/r/releases/download/v1/x.exe",
            "https://github.com@evil.example/AndreyFSDev/cloufam-releases/releases/download/v1/x.exe",
            "http://github.com/AndreyFSDev/cloufam-releases/releases/download/v1/x.exe",
            "https://github.com:8443/AndreyFSDev/cloufam-releases/releases/download/v1/x.exe",
            "https://github.com/Other/cloufam-releases/releases/download/v1/x.exe",
            "не адрес",
        ] {
            assert!(!is_trusted_release_url(bad), "{bad}");
        }
    }
}

