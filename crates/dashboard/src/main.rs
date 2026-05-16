//! Universal-Ops dashboard — cross-session breadcrumb viewer and heartbeat tracker.
//!
//! Phase 3a port from manager-mcp v1.4.4 (commit 0ccb053):
//!   - Extracted axum HTTP server skeleton + route table
//!   - Inlined the embedded dashboard_ui.html (next to this main.rs)
//!   - Filesystem-backed handlers (breadcrumbs, inbox) work standalone
//!   - In-memory-task handlers (status/cancel/post_task) are STUBBED — they
//!     used to share manager's `Arc<RwLock<HashMap<String, Task>>>` directly;
//!     in the cross-binary model they must call manager over HTTP/named-pipe.
//!     See `TODO(phase-3b)` markers below.
//!
//! Listens on `CPC_DASHBOARD_PORT` (default 9999 per Universal-Ops README;
//! manager's v1.4.4 default was 9218 — port choice is a 3b decision).
//!
//! Source-of-truth for the original code: `crates/manager/src/main.rs`
//! functions `start_dashboard`, `dash_*`, `api_*` (lines ~8687-9985).

use axum::{
    extract::{Path as AxumPath, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_http::cors::{Any, CorsLayer};
use tracing::{error, info, warn};

/// Resolve the manager's tasks directory the same way manager v1.4.4 does
/// (cpc_paths::data_path("manager")/tasks, overridable via MANAGER_TASKS_DIR).
/// Phase 3b blocker 1: dashboard reads tasks/{id}/state.json directly from
/// disk so READ handlers (status, status_by_id, history) work without
/// manager-side IPC. WRITE handlers (post_task, cancel, post_prefs,
/// register_external_task) still need IPC and stay 501 until Phase 3c.
fn manager_tasks_dir() -> PathBuf {
    if let Ok(env) = std::env::var("MANAGER_TASKS_DIR") {
        return PathBuf::from(env);
    }
    cpc_paths::data_path("manager")
        .map(|p| p.join("tasks"))
        .unwrap_or_else(|_| PathBuf::from(r"C:\CPC\data\manager\tasks"))
}

/// Read one tasks/{id}/state.json and return its parsed JSON (or None on error).
fn read_task_state(task_id: &str) -> Option<Value> {
    let path = manager_tasks_dir().join(task_id).join("state.json");
    let s = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str::<Value>(&s).ok()
}

/// Iterate all tasks/*/state.json files and return their parsed contents.
fn list_task_states() -> Vec<Value> {
    let dir = manager_tasks_dir();
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else { return out; };
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) { continue; }
        let state_path = entry.path().join("state.json");
        if let Ok(s) = std::fs::read_to_string(&state_path) {
            if let Ok(v) = serde_json::from_str::<Value>(&s) {
                out.push(v);
            }
        }
    }
    out
}

const DASHBOARD_VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_PORT: u16 = 9999;
const SERVER_KEY: &str = "universal-dashboard";
const BINARY_NAME: &str = "dashboard";

/// Dashboard-local state. Phase 3a: filesystem-only; no in-memory task store.
/// Phase 3b adds an `Arc<reqwest::Client>` (or named-pipe client) for talking to manager.
#[derive(Clone)]
struct DashboardState {
    /// Cached recent activity, read from `CPC_STATE.json` and the breadcrumb log.
    /// Populated by a background refresher (3b).
    activity: Arc<RwLock<Vec<Value>>>,
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--version") | Some("-V") => {
            println!("{BINARY_NAME} {DASHBOARD_VERSION}");
            return;
        }
        Some("--help") | Some("-h") => {
            print_help();
            return;
        }
        Some("install") => match install_common::install(SERVER_KEY, &args[2..]) {
            Ok(()) => return,
            Err(e) => {
                eprintln!("install failed: {e:#}");
                std::process::exit(1);
            }
        },
        Some("uninstall") => match install_common::uninstall(SERVER_KEY, &args[2..]) {
            Ok(()) => return,
            Err(e) => {
                eprintln!("uninstall failed: {e:#}");
                std::process::exit(1);
            }
        },
        _ => {}
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let state = DashboardState {
        activity: Arc::new(RwLock::new(Vec::new())),
    };

    start_dashboard(state).await;
}

fn print_help() {
    println!("Universal-Ops dashboard v{}", DASHBOARD_VERSION);
    println!();
    println!("USAGE:");
    println!("  dashboard                              Run web UI on http://127.0.0.1:{} (CPC_DASHBOARD_PORT to override)", DEFAULT_PORT);
    println!("  dashboard serve                        Same as above");
    println!("  dashboard install --target <host>      Register with host config as '{}'", SERVER_KEY);
    println!("  dashboard uninstall --target <host>    Unregister from host config");
    println!("  dashboard --version                    Print version");
    println!("  dashboard --help                       Print this help");
    println!();
    install_common::print_install_help(BINARY_NAME, SERVER_KEY);
    println!();
    println!("Repository: https://github.com/AIWander/Universal-Ops");
}

async fn start_dashboard(state: DashboardState) {
    // README documents port 9999; honor CPC_DASHBOARD_PORT override for parity with manager v1.4.4.
    let preferred: u16 = std::env::var("CPC_DASHBOARD_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT);

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        // Filesystem-backed (work standalone in 3a)
        .route("/", get(dash_root))
        .route("/health", get(dash_health))
        .route("/inbox", get(dash_inbox))
        .route("/knowledge", get(dash_knowledge))
        .route("/git", get(dash_git))
        .route("/system", get(dash_system))
        .route("/api/read-file", get(api_read_file))
        .route("/api/list-dir", get(api_list_dir))
        // Breadcrumb-backed (read from CPC_STATE.json / breadcrumb.jsonl)
        .route("/api/status", get(dash_api_status))
        .route("/api/config", get(dash_api_config))
        // Manager-backed (TODO(phase-3b): forward to manager over IPC)
        .route("/status", get(dash_status))
        .route("/status/:id", get(dash_status_by_id))
        .route("/prefs", get(dash_get_prefs).post(dash_post_prefs))
        .route("/task", post(dash_post_task))
        .route("/cancel/:id", post(dash_cancel))
        .route("/history", get(dash_history))
        .route("/api/tasks/register", post(api_register_external_task))
        .layer(cors)
        .with_state(state);

    let mut attempts: u16 = 0;
    let range_size: u16 = 100;
    let mut bound_port = preferred;
    let listener = loop {
        match tokio::net::TcpListener::bind(format!("127.0.0.1:{}", bound_port)).await {
            Ok(l) => break l,
            Err(e) => {
                attempts += 1;
                if attempts >= range_size {
                    error!(
                        "Dashboard failed to bind any port in {}-{} after {} attempts: {}",
                        preferred,
                        preferred + range_size - 1,
                        attempts,
                        e
                    );
                    return;
                }
                let failed_port = bound_port;
                bound_port = if bound_port >= preferred + range_size - 1 {
                    preferred
                } else {
                    bound_port + 1
                };
                if attempts <= 3 {
                    warn!(
                        "Dashboard port {} busy: {} — trying {}",
                        failed_port, e, bound_port
                    );
                }
            }
        }
    };

    info!("Dashboard HTTP server on http://127.0.0.1:{}/", bound_port);

    // Write URL discovery file (parity with manager v1.4.4 behavior).
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        let url_file = std::path::PathBuf::from(local_app_data)
            .join("universal-ops")
            .join("dashboard_url.txt");
        if let Some(parent) = url_file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let url = format!("http://127.0.0.1:{}/\n", bound_port);
        let _ = std::fs::write(&url_file, &url);
    }

    axum::serve(listener, app).await.ok();
}

// ============================================================================
// Filesystem-backed handlers (work standalone in 3a)
// ============================================================================

/// Serves the embedded dashboard UI HTML.
async fn dash_root() -> impl IntoResponse {
    static HTML: &str = include_str!("dashboard_ui.html");
    ([("content-type", "text/html; charset=utf-8")], HTML)
}

/// Lightweight liveness probe.
async fn dash_health() -> Json<Value> {
    Json(json!({
        "ok": true,
        "binary": "universal-dashboard",
        "version": DASHBOARD_VERSION,
    }))
}

async fn dash_inbox() -> Json<Value> {
    let inbox_path = volumes_base_path()
        .join("multi_ai_coordination")
        .join("inbox.md");
    match std::fs::read_to_string(&inbox_path) {
        Ok(content) => Json(json!({"raw": content, "path": inbox_path.display().to_string()})),
        Err(e) => Json(json!({"error": format!("Cannot read inbox: {}", e)})),
    }
}

async fn dash_knowledge() -> Json<Value> {
    Json(json!({"todo": "phase-3b: catalog summary from CATALOG.md"}))
}

async fn dash_git() -> Json<Value> {
    Json(json!({"todo": "phase-3b: git status of current workspace"}))
}

async fn dash_system() -> Json<Value> {
    Json(json!({"todo": "phase-3b: cpu/mem/disk via sysinfo"}))
}

#[derive(Debug, Deserialize)]
struct PathQuery {
    path: String,
}

fn volumes_base_path() -> std::path::PathBuf {
    let user = std::env::var("USERPROFILE").unwrap_or_else(|_| r"C:\Users\Default".to_string());
    std::path::PathBuf::from(format!(r"{}\My Drive\Volumes", user))
}

fn validate_volumes_path(
    requested: &str,
) -> Result<std::path::PathBuf, (StatusCode, Json<Value>)> {
    let base = volumes_base_path();
    let p = std::path::PathBuf::from(requested);
    let canon = p
        .canonicalize()
        .or_else(|_| Ok::<_, std::io::Error>(p.clone()))
        .unwrap();
    if !canon.starts_with(&base) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(json!({"error": "path outside Volumes"})),
        ));
    }
    Ok(canon)
}

async fn api_read_file(Query(q): Query<PathQuery>) -> impl IntoResponse {
    let path = match validate_volumes_path(&q.path) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match std::fs::read_to_string(&path) {
        Ok(content) => Json(json!({"path": path.display().to_string(), "content": content}))
            .into_response(),
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("{}", e)})),
        )
            .into_response(),
    }
}

async fn api_list_dir(Query(q): Query<PathQuery>) -> impl IntoResponse {
    let path = match validate_volumes_path(&q.path) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    match std::fs::read_dir(&path) {
        Ok(rd) => {
            let entries: Vec<Value> = rd
                .filter_map(|e| e.ok())
                .map(|e| {
                    json!({
                        "name": e.file_name().to_string_lossy(),
                        "is_dir": e.file_type().map(|t| t.is_dir()).unwrap_or(false),
                    })
                })
                .collect();
            Json(json!({"path": path.display().to_string(), "entries": entries}))
                .into_response()
        }
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("{}", e)})),
        )
            .into_response(),
    }
}

// ============================================================================
// Breadcrumb-backed handlers (read CPC_STATE.json + breadcrumb.jsonl)
// ============================================================================

/// Top-level dashboard status: active breadcrumbs, recent activity, peer health.
/// In 3a, reads disk; in 3b, also queries manager's `/api/status` for live tasks.
async fn dash_api_status(State(_st): State<DashboardState>) -> Json<Value> {
    let breadcrumbs = read_active_breadcrumbs();
    Json(json!({
        "binary": "universal-dashboard",
        "version": DASHBOARD_VERSION,
        "breadcrumbs": breadcrumbs,
        "manager": null,  // TODO(phase-3b): GET http://127.0.0.1:<manager-port>/api/status
    }))
}

async fn dash_api_config() -> Json<Value> {
    Json(json!({
        "version": DASHBOARD_VERSION,
        "default_port": DEFAULT_PORT,
        "volumes_base": volumes_base_path().display().to_string(),
    }))
}

fn read_active_breadcrumbs() -> Vec<Value> {
    let cpc_state_path = volumes_base_path()
        .join("system")
        .join("CPC_STATE.json");
    let raw = match std::fs::read_to_string(&cpc_state_path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let parsed: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    parsed
        .get("active_breadcrumbs")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

// ============================================================================
// Manager-backed handlers — stubbed; TODO(phase-3b)
// ============================================================================

async fn dash_status(State(_st): State<DashboardState>) -> Json<Value> {
    // Phase 3b: read tasks/{id}/state.json files directly from disk.
    // Manager v1.4.4 writes one state.json per in-flight task; on terminal
    // state the file is removed (task is moved to history).
    let states = list_task_states();
    let running = states.iter().filter(|s| {
        s.get("status").and_then(|v| v.as_str())
            .map(|st| st != "completed" && st != "failed" && st != "cancelled")
            .unwrap_or(false)
    }).count();
    let total = states.len();
    Json(json!({
        "tasks": states,
        "running": running,
        "completed": total - running,
        "total": total,
        "source": "tasks/{id}/state.json filesystem scan",
    }))
}

async fn dash_status_by_id(
    State(_st): State<DashboardState>,
    AxumPath(id): AxumPath<String>,
) -> (StatusCode, Json<Value>) {
    // Phase 3b: read tasks/{id}/state.json directly.
    match read_task_state(&id) {
        Some(v) => (StatusCode::OK, Json(v)),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "id": id,
                "error": "task not found in tasks/{id}/state.json (may be completed and moved to history, or never submitted)",
            })),
        ),
    }
}

async fn dash_get_prefs() -> Json<Value> {
    Json(json!({"note": "phase-3a stub — manager owns prefs file"}))
}

async fn dash_post_prefs(Json(_body): Json<Value>) -> Json<Value> {
    Json(json!({"ok": false, "note": "phase-3a stub — wire to manager in 3b"}))
}

async fn dash_post_task(
    State(_st): State<DashboardState>,
    Json(_body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({"error": "phase-3a stub — POST /task forwards to manager in 3b"})),
    )
}

async fn dash_cancel(
    State(_st): State<DashboardState>,
    AxumPath(id): AxumPath<String>,
) -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "id": id,
            "error": "phase-3a stub — wire to manager IPC in 3b",
        })),
    )
}

async fn dash_history() -> Json<Value> {
    // Phase 3b: history lives in manager's _HISTORY_DIR (default same as data_dir).
    // Look for tasks/*/history.json or {id}.history.json patterns. For now scan
    // tasks/ for entries whose state.json marks them terminal (status in
    // completed/failed/cancelled) — manager removes state.json on terminal, so
    // this returns empty if manager v1.4.4 is doing its job. The true history
    // store is a separate concern (read flat JSON files under data_dir root).
    // Phase 3c: hook into _HISTORY_DIR's flat file scanner.
    let history_dir = std::env::var("MANAGER_HISTORY_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(|| cpc_paths::data_path("manager").ok())
        .unwrap_or_else(|| PathBuf::from(r"C:\CPC\data\manager"));
    let mut history = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&history_dir) {
        for entry in entries.flatten().take(200) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(s) = std::fs::read_to_string(&path) {
                    if let Ok(v) = serde_json::from_str::<Value>(&s) {
                        history.push(v);
                    }
                }
            }
        }
    }
    Json(json!({
        "history": history,
        "count": history.len(),
        "source": format!("flat JSON scan of {}", history_dir.display()),
    }))
}

async fn api_register_external_task(
    State(_st): State<DashboardState>,
    Json(_body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({"error": "phase-3a stub — external task registration arrives in 3b"})),
    )
}
