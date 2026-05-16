#![recursion_limit = "512"]
//! Manager MCP Server v1.4.4
//! Multi-AI orchestrator: GPT (reasoning), Gemini CLI (coding), Claude Code (coding)
//! Submit → Poll → Retrieve pattern for long-running tasks
//!
//! Tools: submit_task, get_status, get_output, list_tasks, cancel_task, configure, retry_task
// NAV: TOC at line 7450 | 133 fn | 18 struct | 2026-04-15

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, Read as IoRead, Seek, SeekFrom, Write};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncReadExt;
use tokio::process::Command as TokioCommand;
use tokio::sync::RwLock;
use tracing::{error, info, warn};
use uuid::Uuid;

mod analyzer;

/// Resolve user home directory from USERPROFILE env var.
fn default_user_home() -> String {
    std::env::var("USERPROFILE").unwrap_or_else(|_| r"C:\Users\Default".to_string())
}

/// Resolve CPC workspace root from CPC_WORKSPACE_ROOT env var.
fn workspace_root() -> String {
    std::env::var("CPC_WORKSPACE_ROOT").unwrap_or_else(|_| r"C:\rust-mcp".to_string())
}

/// Controls whether a spawned CLI task gets piped stdin (for send() follow-ups)
/// or null stdin (immediate EOF — prevents claude_code stdin reader stalls).
#[derive(Clone, Copy, Debug)]
enum StdinMode {
    /// One-shot tasks: stdin → /dev/null. Child sees EOF immediately.
    Null,
    /// Sessions: stdin → piped. send() can write follow-up input.
    Piped,
}

// Dashboard
use axum::{
    extract::{Path as AxumPath, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use tower_http::cors::{Any, CorsLayer};

// ============================================================================
// Configuration
// ============================================================================

const MAX_HISTORY_ENTRIES: usize = 500;
const GPT_API_URL: &str = "https://api.openai.com/v1/chat/completions";
#[allow(dead_code)]
const ROLLBACK_RETENTION_HOURS: i64 = 24;
const DEFAULT_GPT_MODEL: &str = "o3";
const MANAGER_VERSION: &str = env!("CARGO_PKG_VERSION");

use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};

/// Port the dashboard HTTP listener actually bound to (0 = not running).
static DASHBOARD_PORT: AtomicU16 = AtomicU16::new(0);
/// True while the dashboard listener task is alive.
static DASHBOARD_RUNNING: AtomicBool = AtomicBool::new(false);
/// AbortHandle for the dashboard tokio task — used by `dashboard_stop` MCP tool.
static DASHBOARD_ABORT: Lazy<Mutex<Option<tokio::task::AbortHandle>>> =
    Lazy::new(|| Mutex::new(None));

fn default_data_dir() -> String {
    std::env::var("MANAGER_DATA_DIR").unwrap_or_else(|_| {
        let local = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| r"C:\ProgramData".to_string());
        format!(r"{}\manager-mcp", local)
    })
}

macro_rules! lazy_path {
    ($name:ident, $env:expr, $default:expr) => {
        static $name: Lazy<String> = Lazy::new(|| std::env::var($env).unwrap_or_else(|_| $default));
    };
}

lazy_path!(
    _TASKS_DIR,
    "MANAGER_TASKS_DIR",
    format!(r"{}\tasks", default_data_dir())
);
lazy_path!(_HISTORY_DIR, "MANAGER_HISTORY_DIR", default_data_dir());
lazy_path!(
    _WORKFLOW_PATTERNS_DIR,
    "MANAGER_WORKFLOW_DIR",
    format!(r"{}\workflow_patterns", default_data_dir())
);
lazy_path!(
    _ROLLBACK_DIR,
    "MANAGER_ROLLBACK_DIR",
    format!(r"{}\rollback", default_data_dir())
);
lazy_path!(
    _LEARNED_PATTERNS_DIR,
    "MANAGER_PATTERNS_DIR",
    format!(r"{}\learned_patterns", default_data_dir())
);
lazy_path!(
    _DASHBOARD_PREFS_PATH,
    "MANAGER_DASHBOARD_PREFS",
    format!(r"{}\dashboard_prefs.json", default_data_dir())
);
lazy_path!(
    _LOAVES_DIR,
    "MANAGER_LOAVES_DIR",
    format!(r"{}\loaves", default_data_dir())
);

static _GEMINI_CMD: Lazy<String> = Lazy::new(|| {
    std::env::var("gemini_cmd()").unwrap_or_else(|_| {
        let npm_root = std::env::var("APPDATA").unwrap_or_default();
        let npm_path = format!(
            r"{}\npm\node_modules\@google\gemini-cli\dist\index.js",
            npm_root
        );
        if std::path::Path::new(&npm_path).exists() {
            npm_path
        } else {
            "gemini".to_string()
        }
    })
});
static _CLAUDE_CODE_CMD: Lazy<String> = Lazy::new(|| {
    std::env::var("claude_code_cmd()").unwrap_or_else(|_| {
        let home = std::env::var("USERPROFILE").unwrap_or_default();
        let local_path = format!(r"{}\.local\bin\claude.exe", home);
        if std::path::Path::new(&local_path).exists() {
            local_path
        } else {
            "claude".to_string()
        }
    })
});
static _CODEX_CMD: Lazy<String> = Lazy::new(|| {
    std::env::var("codex_cmd()").unwrap_or_else(|_| {
        let npm_root = std::env::var("APPDATA").unwrap_or_default();
        for arch in &["arm64", "x64"] {
            let p = format!(r"{}\npm\node_modules\@openai\codex\node_modules\@openai\codex-win32-{}\vendor\aarch64-pc-windows-msvc\codex\codex.exe", npm_root, arch);
            if std::path::Path::new(&p).exists() { return p; }
        }
        "codex".to_string()
    })
});
static _NODE_CMD: Lazy<String> = Lazy::new(|| {
    std::env::var("node_cmd()").unwrap_or_else(|_| {
        let pf = r"C:\Program Files\nodejs\node.exe";
        if std::path::Path::new(pf).exists() {
            pf.to_string()
        } else {
            "node".to_string()
        }
    })
});
static _LOAVES_ARCHIVE_DIR: Lazy<String> = Lazy::new(|| format!(r"{}\archive", &*_LOAVES_DIR));

// Re-export as &str for backwards compat with existing code
const fn _ignore() {}
#[allow(unused_macros)]
macro_rules! path_ref {
    ($static_name:ident) => {
        &*$static_name
    };
}

// Accessor functions so existing code compiles with minimal changes
fn tasks_dir() -> &'static str {
    &_TASKS_DIR
}
fn history_dir() -> &'static str {
    &_HISTORY_DIR
}
fn gemini_cmd() -> &'static str {
    &_GEMINI_CMD
}
fn claude_code_cmd() -> &'static str {
    &_CLAUDE_CODE_CMD
}
fn codex_cmd() -> &'static str {
    &_CODEX_CMD
}
fn workflow_patterns_dir() -> &'static str {
    &_WORKFLOW_PATTERNS_DIR
}
fn rollback_dir() -> &'static str {
    &_ROLLBACK_DIR
}
fn learned_patterns_dir() -> &'static str {
    &_LEARNED_PATTERNS_DIR
}
fn node_cmd() -> &'static str {
    &_NODE_CMD
}
#[allow(dead_code)]
fn dashboard_prefs_path() -> &'static str {
    &_DASHBOARD_PREFS_PATH
}
fn loaves_dir() -> &'static str {
    &_LOAVES_DIR
}
fn loaves_archive_dir() -> &'static str {
    &_LOAVES_ARCHIVE_DIR
}

#[allow(dead_code)]
fn load_terminal_visible() -> bool {
    std::fs::read_to_string(dashboard_prefs_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("terminal_visible")?.as_bool())
        .unwrap_or(false)
}

/// Spawn a visible terminal window mirroring a background task's CLI command.
/// Fire-and-forget â€” errors are logged but don't affect the background task.
fn spawn_visible_terminal(title: &str, exe: &str, args: &[String], working_dir: &str) {
    // Write full command to temp .bat to avoid cmd/wt quoting hell
    let skip_args: std::collections::HashSet<&str> =
        ["--output-format", "stream-json"].iter().copied().collect();
    let mut cmd_parts: Vec<String> = vec![exe.to_string()];
    for a in args {
        if skip_args.contains(a.as_str()) {
            continue;
        }
        if a.contains(' ') || a.contains('"') || a.contains('\\') {
            cmd_parts.push(format!("\"{}\"", a.replace('"', "\\\"")));
        } else {
            cmd_parts.push(a.clone());
        }
    }
    let cmd_line = format!(
        "@echo off\r\ncd /d {}\r\n{}",
        working_dir,
        cmd_parts.join(" ")
    );

    let staging = format!(
        "{}\\CPC\\staging",
        std::env::var("LOCALAPPDATA").unwrap_or_else(|_| r"C:\Users\Public".to_string())
    );
    let _ = std::fs::create_dir_all(&staging);
    let bat_name = format!(
        "task_{}.bat",
        title
            .chars()
            .filter(|c| c.is_alphanumeric())
            .take(20)
            .collect::<String>()
    );
    let bat_path = format!("{}\\{}", staging, bat_name);
    if std::fs::write(&bat_path, &cmd_line).is_err() {
        return;
    }

    // Try wt first, fallback to cmd start
    let wt = std::process::Command::new("wt")
        .args([
            "-w", "0", "new-tab", "--title", title, "cmd", "/K", &bat_path,
        ])
        .spawn();
    if wt.is_err() {
        let _ = std::process::Command::new("cmd")
            .args([
                "/C",
                "start",
                &format!("\"{}\"", title),
                "cmd",
                "/K",
                &bat_path,
            ])
            .spawn();
    }
}

// ============================================================================
// MCP Protocol Types
// ============================================================================

#[derive(Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    #[serde(default)]
    id: Value,
    method: String,
    params: Option<Value>,
}

#[derive(Serialize)]
struct JsonRpcSuccess {
    jsonrpc: String,
    id: Value,
    result: Value,
}

#[derive(Serialize)]
struct JsonRpcErrorResponse {
    jsonrpc: String,
    id: Value,
    error: JsonRpcError,
}

#[derive(Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
}

// ============================================================================
// Task Types
// ============================================================================

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Backend {
    Gpt,
    Gemini,
    ClaudeCode,
    Codex,
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Backend::Gpt => write!(f, "gpt"),
            Backend::Gemini => write!(f, "gemini"),
            Backend::ClaudeCode => write!(f, "claude_code"),
            Backend::Codex => write!(f, "codex"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum TaskStatus {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
    Paused,
    /// v1.2.7: Session child process is still alive after manager restart but
    /// stdout/stderr pipes were lost — session is unmonitored and cannot receive output.
    /// Surface in session_list; user can destroy and restart.
    Orphaned,
    /// v1.2.9: Task is running but has produced no output for MANAGER_STALL_TIMEOUT_SECS.
    /// Transitions back to Running automatically if output resumes.
    Stalled,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskStatus::Queued => write!(f, "queued"),
            TaskStatus::Running => write!(f, "running"),
            TaskStatus::Done => write!(f, "done"),
            TaskStatus::Failed => write!(f, "failed"),
            TaskStatus::Cancelled => write!(f, "cancelled"),
            TaskStatus::Paused => write!(f, "paused"),
            TaskStatus::Orphaned => write!(f, "orphaned"),
            TaskStatus::Stalled => write!(f, "stalled"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
enum ExtractionStatus {
    #[default]
    None,
    PendingSuccess,
    PendingFailure,
    Extracted,
    Dismissed,
    TooSimple,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
enum TrustLevel {
    #[default]
    Low, // 1-3: fire and forget
    Medium, // 4-6: auto-backup before start
    High,   // 7-10: backup + require diff review
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
enum ValidationStatus {
    #[default]
    NotChecked,
    Passed,
    Failed,
    Skipped,
}

fn default_max_retries() -> u32 {
    2
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TaskStep {
    tool: String,
    timestamp: DateTime<Utc>,
    status: String, // "started", "completed", "error"
    summary: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ActivityEntry {
    pid: u32,
    name: String,
    cmd_preview: String,
    cpu_percent: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ToolCallEntry {
    tool_name: String,
    timestamp_utc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    duration_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Task {
    id: String,
    backend: Backend,
    prompt: String,
    system_prompt: Option<String>,
    model: Option<String>,
    working_dir: Option<String>,
    status: TaskStatus,
    output: String,
    error: Option<String>,
    created_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    completed_at: Option<DateTime<Utc>>,
    progress_lines: usize,
    #[serde(default)]
    steps: Vec<TaskStep>,
    #[serde(default)]
    last_activity: Option<DateTime<Utc>>,
    #[serde(default)]
    last_output_chunk_at: Option<DateTime<Utc>>,
    #[serde(default)]
    stall_detected: bool,
    #[serde(default)]
    extraction_status: ExtractionStatus,
    #[serde(default)]
    pub trust_score: u8,
    #[serde(default)]
    pub trust_level: TrustLevel,
    #[serde(default)]
    pub rollback_path: Option<String>,
    #[serde(default)]
    pub validation_status: ValidationStatus,
    #[serde(default)]
    pub assertions: Vec<String>,
    #[serde(default)]
    pub backed_up_files: Vec<String>,
    #[serde(default)]
    pub retry_count: u32,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default)]
    pub retry_of: Option<String>,
    #[serde(default)]
    pub error_context: Option<String>,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default)]
    pub on_complete: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub save_artifact: bool,
    #[serde(default)]
    pub rerun_of: Option<String>,
    #[serde(default)]
    pub parent_task_id: Option<String>,
    #[serde(default)]
    pub forked_from: Option<String>,
    #[serde(default)]
    pub continuation_of: Option<String>,
    #[serde(default)]
    pub child_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_path: Option<String>,
    #[serde(default)]
    pub log_offset: u64,
    #[serde(default)]
    pub reconnected: bool,
    #[serde(default)]
    pub watchdog_observations: Vec<String>,
    #[serde(default)]
    pub fingerprint: Option<String>,
    #[serde(default)]
    pub superseded_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_step: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_steps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_step_desc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_activity: Option<Vec<ActivityEntry>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    // v1.3.1: task-level notify flags (mirrors session notify)
    // v1.3.2 (Opus review): skip_serializing_if on bools for JSON cleanliness
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_on_complete: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_on_fail: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_on_destroy: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_body: Option<String>,
}

const MANAGER_FINAL_LOG_PREFIX: &str = "__MANAGER_FINAL__ ";

#[derive(Clone)]
struct ReconnectSpec {
    task_id: String,
    log_path: String,
    offset: u64,
    child_pid: u32,
}

/// v1.4.4: Lightweight task metadata written to `tasks/{id}/state.json` on submit.
/// Survives manager crashes — reconnect scans these as source of truth for in-flight tasks.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct TaskState {
    task_id: String,
    backend: String,
    prompt_preview: String,
    working_dir: Option<String>,
    child_pid: Option<u32>,
    started_at: Option<DateTime<Utc>>,
    log_path: Option<String>,
    created_at: DateTime<Utc>,
}

fn task_state_path(task_id: &str) -> std::path::PathBuf {
    task_workspace_dir(task_id).join("state.json")
}

/// Write (or update) the lightweight state.json for a task.
fn write_task_state(task: &Task) {
    let state = TaskState {
        task_id: task.id.clone(),
        backend: task.backend.to_string(),
        prompt_preview: safe_truncate(&task.prompt, 200),
        working_dir: task.working_dir.clone(),
        child_pid: task.child_pid,
        started_at: task.started_at,
        log_path: task.log_path.clone(),
        created_at: task.created_at,
    };
    let dir = task_workspace_dir(&task.id);
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(data) = serde_json::to_string_pretty(&state) {
        let _ = std::fs::write(task_state_path(&task.id), data);
    }
}

/// Remove state.json when a task reaches a terminal state (written to history instead).
fn remove_task_state(task_id: &str) {
    let _ = std::fs::remove_file(task_state_path(task_id));
}

fn task_workspace_dir(task_id: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(tasks_dir()).join(task_id)
}

fn task_log_path(task_id: &str) -> String {
    task_workspace_dir(task_id)
        .join("child.log")
        .to_string_lossy()
        .into_owned()
}

fn prepare_task_log(task_id: &str) -> std::io::Result<String> {
    let task_dir = task_workspace_dir(task_id);
    std::fs::create_dir_all(&task_dir)?;
    let path = std::path::PathBuf::from(task_log_path(task_id));
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)?;
    Ok(path.to_string_lossy().into_owned())
}

fn open_child_log_stdio(log_path: &str) -> std::io::Result<(Stdio, Stdio)> {
    let stdout = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    let stderr = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    Ok((Stdio::from(stdout), Stdio::from(stderr)))
}

fn log_file_len(log_path: &str) -> u64 {
    std::fs::metadata(log_path).map(|m| m.len()).unwrap_or(0)
}

fn append_final_log_marker(
    log_path: &str,
    task_id: &str,
    success: bool,
    exit_code: Option<i32>,
    error: Option<&str>,
) {
    let marker = json!({
        "schema": "manager-final-v1",
        "task_id": task_id,
        "success": success,
        "exit_code": exit_code,
        "error": error,
        "completed_at": Utc::now().to_rfc3339(),
    });
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    {
        let _ = writeln!(file, "{}{}", MANAGER_FINAL_LOG_PREFIX, marker);
        let _ = file.flush();
    }
}

fn is_process_alive(pid: u32) -> bool {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, false);
    sys.process(Pid::from_u32(pid)).is_some()
}

fn read_log_from(log_path: &str, mut offset: u64) -> std::io::Result<(Vec<u8>, u64)> {
    let mut file = std::fs::OpenOptions::new().read(true).open(log_path)?;
    let len = file.metadata()?.len();
    if offset > len {
        offset = 0;
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok((buf, len))
}

#[cfg(windows)]
fn wait_for_process_exit(pid: u32) -> Result<u32, String> {
    use std::ffi::c_void;

    type Handle = *mut c_void;

    const SYNCHRONIZE: u32 = 0x0010_0000;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const WAIT_OBJECT_0: u32 = 0x0000_0000;
    const WAIT_FAILED: u32 = 0xFFFF_FFFF;
    const INFINITE: u32 = 0xFFFF_FFFF;
    const STILL_ACTIVE: u32 = 259;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> Handle;
        fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
        fn GetExitCodeProcess(handle: Handle, exit_code: *mut u32) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
    }

    unsafe {
        let handle = OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return Err(format!(
                "OpenProcess failed for PID {}: {}",
                pid,
                std::io::Error::last_os_error()
            ));
        }

        let wait_status = WaitForSingleObject(handle, INFINITE);
        if wait_status == WAIT_FAILED {
            let err = std::io::Error::last_os_error();
            let _ = CloseHandle(handle);
            return Err(format!(
                "WaitForSingleObject failed for PID {}: {}",
                pid, err
            ));
        }
        if wait_status != WAIT_OBJECT_0 {
            let _ = CloseHandle(handle);
            return Err(format!(
                "Unexpected wait status {} for PID {}",
                wait_status, pid
            ));
        }

        let mut exit_code = STILL_ACTIVE;
        if GetExitCodeProcess(handle, &mut exit_code as *mut u32) == 0 {
            let err = std::io::Error::last_os_error();
            let _ = CloseHandle(handle);
            return Err(format!(
                "GetExitCodeProcess failed for PID {}: {}",
                pid, err
            ));
        }
        let _ = CloseHandle(handle);
        if exit_code == STILL_ACTIVE {
            return Err(format!(
                "PID {} reported STILL_ACTIVE after wait completion",
                pid
            ));
        }
        Ok(exit_code)
    }
}

#[cfg(not(windows))]
fn wait_for_process_exit(pid: u32) -> Result<u32, String> {
    Err(format!(
        "wait_for_process_exit is only implemented on Windows (pid {})",
        pid
    ))
}

fn split_log_bytes(bytes: &[u8], partial: &mut String) -> Vec<String> {
    let chunk = String::from_utf8_lossy(bytes);
    let mut lines = Vec::new();
    let mut cr_seen = false;
    for c in chunk.chars() {
        if c == '\n' {
            if cr_seen {
                cr_seen = false;
                continue;
            }
            lines.push(std::mem::take(partial));
        } else if c == '\r' {
            cr_seen = true;
            let line = std::mem::take(partial);
            if !line.is_empty() {
                lines.push(line);
            }
        } else {
            cr_seen = false;
            partial.push(c);
        }
    }
    lines
}

fn append_task_output(task: &mut Task, text: &str) {
    if text.is_empty() {
        return;
    }
    if !task.output.is_empty() {
        task.output.push('\n');
    }
    task.output.push_str(text);
    task.progress_lines += 1;
    if task.status == TaskStatus::Stalled {
        task.status = TaskStatus::Running;
    }
    task.last_output_chunk_at = Some(Utc::now());
}

fn apply_task_log_line(task: &mut Task, line: &str) {
    if line.is_empty() || line.starts_with(MANAGER_FINAL_LOG_PREFIX) {
        return;
    }
    task.last_activity = Some(Utc::now());
    task.stall_detected = false;

    if let Ok(ev) = serde_json::from_str::<Value>(line) {
        let ev_type = ev.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ev_type {
            "assistant" => {
                if let Some(contents) = ev.pointer("/message/content").and_then(|v| v.as_array()) {
                    for item in contents {
                        match item.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                            "text" => {
                                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                                    append_task_output(task, text);
                                }
                            }
                            "tool_use" => {
                                let tool_name = item
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown")
                                    .to_string();
                                task.steps.push(TaskStep {
                                    tool: tool_name,
                                    timestamp: Utc::now(),
                                    status: "started".to_string(),
                                    summary: item.get("input").map(|v| {
                                        let s = v.to_string();
                                        safe_truncate(&s, 120)
                                    }),
                                });
                            }
                            _ => {}
                        }
                    }
                }
            }
            "user" => {
                if let Some(contents) = ev.pointer("/message/content").and_then(|v| v.as_array()) {
                    for item in contents {
                        if item.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                            if let Some(last) = task.steps.last_mut() {
                                let is_err = item
                                    .get("is_error")
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(false);
                                last.status =
                                    if is_err { "error" } else { "completed" }.to_string();
                            }
                        }
                    }
                }
            }
            "item.completed" => {
                if let Some(item) = ev.get("item") {
                    match item.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                        "agent_message" => {
                            if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                                append_task_output(task, text);
                            }
                        }
                        "mcp_tool_call" | "command_execution" => {
                            let tool = item
                                .get("tool")
                                .or_else(|| item.get("command"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown");
                            let server = item.get("server").and_then(|v| v.as_str()).unwrap_or("");
                            let tool_name = if server.is_empty() {
                                tool.to_string()
                            } else {
                                format!("{}:{}", server, tool)
                            };
                            let status = item
                                .get("status")
                                .and_then(|v| v.as_str())
                                .unwrap_or("completed");
                            let has_error = item.get("error").is_some()
                                && !item.get("error").unwrap().is_null();
                            task.steps.push(TaskStep {
                                tool: tool_name,
                                timestamp: Utc::now(),
                                status: if has_error { "error" } else { status }.to_string(),
                                summary: item.get("arguments").or_else(|| item.get("command")).map(
                                    |v| {
                                        let s = v.to_string();
                                        safe_truncate(&s, 120)
                                    },
                                ),
                            });
                        }
                        _ => {}
                    }
                }
            }
            "result" => {
                if let Some(text) = ev.get("result").and_then(|v| v.as_str()) {
                    append_task_output(task, text);
                }
            }
            _ => {}
        }
    } else {
        append_task_output(task, line);
    }
}

fn final_status_from_log(log_path: &str) -> Option<(TaskStatus, Option<String>)> {
    let content = std::fs::read_to_string(log_path).ok()?;
    for line in content.lines().rev() {
        if let Some(json_part) = line.strip_prefix(MANAGER_FINAL_LOG_PREFIX) {
            if let Ok(v) = serde_json::from_str::<Value>(json_part) {
                let success = v.get("success").and_then(|v| v.as_bool()).unwrap_or(false);
                let err = v
                    .get("error")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(String::from);
                return Some((
                    if success {
                        TaskStatus::Done
                    } else {
                        TaskStatus::Failed
                    },
                    err,
                ));
            }
        }
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            if v.get("type").and_then(|v| v.as_str()) == Some("result") {
                let subtype = v.get("subtype").and_then(|v| v.as_str()).unwrap_or("");
                let is_error = v
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(matches!(subtype, "error" | "failure" | "failed"));
                let err = if is_error {
                    v.get("result")
                        .or_else(|| v.get("error"))
                        .and_then(|v| v.as_str())
                        .map(|s| safe_truncate(s, 500))
                } else {
                    None
                };
                return Some((
                    if is_error {
                        TaskStatus::Failed
                    } else {
                        TaskStatus::Done
                    },
                    err,
                ));
            }
            if matches!(
                v.get("type").and_then(|v| v.as_str()),
                Some("turn.completed" | "session.completed")
            ) {
                return Some((TaskStatus::Done, None));
            }
        }
    }
    None
}

/// v1.4.4: Extract the final agent output from a completed child log.
/// Walks the log looking for per-backend result content so reconnected tasks
/// get real output instead of an empty string.
fn extract_output_from_log(log_path: &str, backend: &Backend) -> Option<String> {
    let content = std::fs::read_to_string(log_path).ok()?;
    let mut output_parts: Vec<String> = Vec::new();

    match backend {
        Backend::Codex => {
            // Codex emits {"type":"item.completed","item":{"type":"agent_message","text":"..."}}
            // and {"type":"turn.completed"} at the end.
            for line in content.lines() {
                if let Ok(v) = serde_json::from_str::<Value>(line) {
                    let ev_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    if ev_type == "item.completed" {
                        if let Some(item) = v.get("item") {
                            if item.get("type").and_then(|t| t.as_str()) == Some("agent_message") {
                                if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                                    output_parts.push(text.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
        Backend::ClaudeCode => {
            // Claude Code stream-json emits {"type":"result","result":"..."} at the end,
            // and {"type":"assistant","message":{"content":[{"type":"text","text":"..."}]}} during.
            for line in content.lines() {
                if let Ok(v) = serde_json::from_str::<Value>(line) {
                    let ev_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    if ev_type == "result" {
                        if let Some(text) = v.get("result").and_then(|t| t.as_str()) {
                            output_parts.push(text.to_string());
                        }
                    } else if ev_type == "assistant" {
                        if let Some(contents) =
                            v.pointer("/message/content").and_then(|c| c.as_array())
                        {
                            for item in contents {
                                if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                                    if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                                        output_parts.push(text.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Backend::Gemini => {
            // TODO: Gemini CLI output format TBD — fall through to None for now.
            // When Gemini CLI streaming format is documented, add marker detection here.
        }
        Backend::Gpt => {
            // GPT tasks are in-process (API call), not applicable for log-based recovery.
        }
    }

    if output_parts.is_empty() {
        None
    } else {
        Some(output_parts.join("\n"))
    }
}

fn log_tail_text(log_path: &str, max_chars: usize) -> String {
    let Ok(content) = std::fs::read_to_string(log_path) else {
        return String::new();
    };
    let mut tail = content
        .lines()
        .filter(|line| !line.starts_with(MANAGER_FINAL_LOG_PREFIX))
        .rev()
        .take(40)
        .collect::<Vec<_>>();
    tail.reverse();
    let joined = tail.join("\n");
    if joined.len() <= max_chars {
        joined
    } else {
        let mut start = joined.len().saturating_sub(max_chars);
        while start < joined.len() && !joined.is_char_boundary(start) {
            start += 1;
        }
        format!("...{}", &joined[start..])
    }
}

fn drain_task_log_once(task: &mut Task, log_path: &str) {
    let mut partial = String::new();
    if let Ok((bytes, new_offset)) = read_log_from(log_path, task.log_offset) {
        let lines = split_log_bytes(&bytes, &mut partial);
        for line in lines {
            apply_task_log_line(task, &line);
        }
        if !partial.is_empty() {
            apply_task_log_line(task, &partial);
        }
        task.log_offset = new_offset;
    }
}

async fn process_log_lines(
    tasks: &Arc<RwLock<HashMap<String, Task>>>,
    task_id: &str,
    lines: &[String],
    offset: u64,
) {
    if lines.is_empty() {
        return;
    }
    let mut store = tasks.write().await;
    if let Some(task) = store.get_mut(task_id) {
        if matches!(
            task.status,
            TaskStatus::Cancelled | TaskStatus::Failed | TaskStatus::Done
        ) {
            task.log_offset = offset;
            Server::persist_task(task);
            return;
        }
        for line in lines {
            apply_task_log_line(task, line);
        }
        task.log_offset = offset;
        Server::persist_task(task);
    }
}

async fn finalize_reconnected_task(
    tasks: Arc<RwLock<HashMap<String, Task>>>,
    task_id: String,
    log_path: String,
    partial: String,
    offset: u64,
) {
    let final_state = final_status_from_log(&log_path);
    let mut completed_snap: Option<Task> = None;
    {
        let mut store = tasks.write().await;
        if let Some(task) = store.get_mut(&task_id) {
            if !partial.is_empty() {
                apply_task_log_line(task, &partial);
            }
            if matches!(
                task.status,
                TaskStatus::Done | TaskStatus::Failed | TaskStatus::Cancelled
            ) {
                task.log_offset = offset;
                Server::persist_task(task);
                return;
            }
            drain_task_log_once(task, &log_path);
            match final_state {
                Some((status, error)) => {
                    let status_label = status.to_string();
                    task.status = status;
                    task.error = error;
                    // v1.4.4: Extract real output from log
                    if task.status == TaskStatus::Done && task.output.is_empty() {
                        if let Some(extracted) = extract_output_from_log(&log_path, &task.backend) {
                            task.output = extracted;
                        }
                    }
                    task.watchdog_observations.push(format!(
                        "[{}] Reconnected child exited; finalized from log marker as {}",
                        Utc::now().format("%H:%M:%S"),
                        status_label
                    ));
                }
                None => {
                    task.status = TaskStatus::Failed;
                    task.error = Some("orphaned-pid-dead".to_string());
                    task.watchdog_observations.push(format!(
                        "[{}] Reconnected child PID is dead and log has no clean completion marker — marking failed (orphaned-pid-dead)",
                        Utc::now().format("%H:%M:%S")
                    ));
                }
            }
            task.completed_at = Some(Utc::now());
            task.log_offset = log_file_len(&log_path).max(offset);
            if task.status == TaskStatus::Done {
                Server::validate_output(task);
            }
            Server::flag_extraction(task);
            Server::persist_task(task);
            Server::save_to_history(task);
            save_task_artifact(task);
            remove_task_state(&task.id);
            check_and_fire_task_notify(task, &RealNotifier);
            auto_advance_loaf_on_task_complete(task);
            completed_snap = Some(task.clone());
        }
    }
    if let Some(ref ct) = completed_snap {
        spawn_on_complete(ct, tasks.clone(), None, &tokio::runtime::Handle::current());
    }
}

async fn watch_reconnected_process_exit(
    tasks: Arc<RwLock<HashMap<String, Task>>>,
    task_id: String,
    log_path: String,
    child_pid: u32,
) {
    let wait_result = tokio::task::spawn_blocking(move || wait_for_process_exit(child_pid)).await;

    match wait_result {
        Ok(Ok(exit_code)) => append_final_log_marker(
            &log_path,
            &task_id,
            exit_code == 0,
            Some(exit_code as i32),
            if exit_code == 0 {
                None
            } else {
                Some("process-exit-nonzero")
            },
        ),
        Ok(Err(e)) => {
            let mut store = tasks.write().await;
            if let Some(task) = store.get_mut(&task_id) {
                task.watchdog_observations.push(format!(
                    "[{}] Reconnected wait on PID {} failed: {}",
                    Utc::now().format("%H:%M:%S"),
                    child_pid,
                    e
                ));
                Server::persist_task(task);
            }
        }
        Err(e) => {
            let mut store = tasks.write().await;
            if let Some(task) = store.get_mut(&task_id) {
                task.watchdog_observations.push(format!(
                    "[{}] Reconnected wait task panicked for PID {}: {}",
                    Utc::now().format("%H:%M:%S"),
                    child_pid,
                    e
                ));
                Server::persist_task(task);
            }
        }
    }

    let offset = {
        let store = tasks.read().await;
        store.get(&task_id).map(|task| task.log_offset).unwrap_or(0)
    };
    finalize_reconnected_task(tasks, task_id, log_path, String::new(), offset).await;
}

async fn tail_task_log(
    tasks: Arc<RwLock<HashMap<String, Task>>>,
    task_id: String,
    log_path: String,
    mut offset: u64,
    child_pid: Option<u32>,
    finalize_on_pid_exit: bool,
) {
    let mut partial = String::new();
    loop {
        match read_log_from(&log_path, offset) {
            Ok((bytes, new_offset)) => {
                if !bytes.is_empty() {
                    offset = new_offset;
                    let lines = split_log_bytes(&bytes, &mut partial);
                    process_log_lines(&tasks, &task_id, &lines, offset).await;
                }
            }
            Err(e) => {
                let mut store = tasks.write().await;
                if let Some(task) = store.get_mut(&task_id) {
                    task.watchdog_observations.push(format!(
                        "[{}] Log tail read failed for {}: {}",
                        Utc::now().format("%H:%M:%S"),
                        log_path,
                        e
                    ));
                    Server::persist_task(task);
                }
                tokio::time::sleep(std::time::Duration::from_millis(750)).await;
            }
        }

        if finalize_on_pid_exit {
            if let Some(pid) = child_pid {
                if !is_process_alive(pid) {
                    for _ in 0..3 {
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                        if let Ok((bytes, new_offset)) = read_log_from(&log_path, offset) {
                            if !bytes.is_empty() {
                                offset = new_offset;
                                let lines = split_log_bytes(&bytes, &mut partial);
                                process_log_lines(&tasks, &task_id, &lines, offset).await;
                            }
                        }
                    }
                    finalize_reconnected_task(
                        tasks.clone(),
                        task_id.clone(),
                        log_path.clone(),
                        std::mem::take(&mut partial),
                        offset,
                    )
                    .await;
                    break;
                }
            }
        } else {
            let terminal = {
                let store = tasks.read().await;
                store
                    .get(&task_id)
                    .map(|task| {
                        matches!(
                            task.status,
                            TaskStatus::Done | TaskStatus::Failed | TaskStatus::Cancelled
                        )
                    })
                    .unwrap_or(true)
            };
            if terminal && offset >= log_file_len(&log_path) {
                if !partial.is_empty() {
                    process_log_lines(&tasks, &task_id, &[std::mem::take(&mut partial)], offset)
                        .await;
                }
                break;
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// Item 16: Task routing intelligence — recommends the best backend for a prompt.
#[derive(Clone, Debug, Serialize)]
struct BackendRecommendation {
    recommended_backend: String,
    confidence: f32,
    reasoning: String,
    alternatives: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
struct WorkflowStep {
    id: String,
    backend: String,
    prompt: String,
    working_dir: Option<String>,
    on_success: Option<String>,
    #[serde(default)]
    max_retries: Option<u32>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    alternatives: Option<Vec<String>>,
    /// Item 17: IDs of steps that must complete before this step starts
    #[serde(default)]
    depends_on: Vec<String>,
    /// Item 17: Group tag — steps with same parallel_group and no unmet deps start together
    #[serde(default)]
    parallel_group: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[allow(dead_code)]
struct WorkflowTemplate {
    name: String,
    description: String,
    #[serde(default)]
    parameters: HashMap<String, String>,
    steps: Vec<TemplateStep>,
    #[serde(default = "default_backend")]
    backend: String,
    #[serde(default = "default_trust_tmpl")]
    trust_level: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    times_used: u32,
    #[serde(default)]
    last_used: String,
    #[serde(default = "default_success_rate")]
    success_rate: f64,
}

#[allow(dead_code)]
fn default_backend() -> String {
    "claude_code".into()
}
#[allow(dead_code)]
fn default_trust_tmpl() -> String {
    "auto_with_backup".into()
}
#[allow(dead_code)]
fn default_success_rate() -> f64 {
    1.0
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[allow(dead_code)]
struct TemplateStep {
    id: String,
    prompt: String,
    #[serde(default)]
    backend: Option<String>,
}

// ============================================================================
// Server State
// ============================================================================

struct ServerConfig {
    openai_api_key: Option<String>,
    default_gpt_model: String,
    default_working_dir: String,
}

type StdinPipes = Arc<RwLock<HashMap<String, Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>>>>;

struct Server {
    tasks: Arc<RwLock<HashMap<String, Task>>>,
    config: Arc<RwLock<ServerConfig>>,
    runtime: tokio::runtime::Handle,
    stdout: Arc<Mutex<io::Stdout>>,
    notifier: Arc<dyn SessionNotifier>,
    recent_tool_calls: Arc<Mutex<VecDeque<ToolCallEntry>>>,
    stdin_pipes: StdinPipes,
}

impl Server {
    fn persist_recovered_terminal_task(task: &mut Task) {
        if task.status == TaskStatus::Done {
            Server::validate_output(task);
        }
        Server::flag_extraction(task);
        Server::persist_task(task);
        Server::save_to_history(task);
        save_task_artifact(task);
        remove_task_state(&task.id);
    }

    fn reconnect_orphaned_tasks(
        tasks: &mut HashMap<String, Task>,
    ) -> (Vec<String>, Vec<ReconnectSpec>) {
        let mut recovery_failed_ids = Vec::new();
        let mut reconnect_specs = Vec::new();

        for task in tasks.values_mut() {
            if !matches!(
                task.status,
                TaskStatus::Queued | TaskStatus::Running | TaskStatus::Paused | TaskStatus::Stalled
            ) {
                continue;
            }

            let is_session = task.id.starts_with("ses_");
            task.watchdog_observations.push(format!(
                "[{}] reconnect_orphaned_tasks: task was {} at manager startup. Child PID: {}",
                Utc::now().format("%H:%M:%S"),
                task.status,
                task.child_pid
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "unknown".into())
            ));

            let Some(log_path) = task.log_path.clone() else {
                task.status = TaskStatus::Failed;
                task.error = Some("pre-v1.4.3-no-logfile".to_string());
                task.completed_at = Some(Utc::now());
                task.watchdog_observations.push(format!(
                    "[{}] No log_path on task record — marking failed (pre-v1.4.3-no-logfile)",
                    Utc::now().format("%H:%M:%S")
                ));
                Server::persist_recovered_terminal_task(task);
                if !is_session {
                    recovery_failed_ids.push(task.id.clone());
                }
                continue;
            };

            if !std::path::Path::new(&log_path).exists() {
                task.status = TaskStatus::Failed;
                task.error = Some("orphaned-logfile-missing".to_string());
                task.completed_at = Some(Utc::now());
                task.watchdog_observations.push(format!(
                    "[{}] log_path exists in task record but file is missing: {}",
                    Utc::now().format("%H:%M:%S"),
                    log_path
                ));
                Server::persist_recovered_terminal_task(task);
                if !is_session {
                    recovery_failed_ids.push(task.id.clone());
                }
                continue;
            }

            let pid_alive = task.child_pid.map(is_process_alive).unwrap_or(false);
            task.log_offset = task.log_offset.min(log_file_len(&log_path));

            if pid_alive {
                let pid = task.child_pid.unwrap();
                task.status = TaskStatus::Running;
                task.reconnected = true;
                task.watchdog_observations.push(format!(
                    "[{}] Child PID {} is alive — reconnected to persistent log {} from offset {}",
                    Utc::now().format("%H:%M:%S"),
                    pid,
                    log_path,
                    task.log_offset
                ));
                reconnect_specs.push(ReconnectSpec {
                    task_id: task.id.clone(),
                    log_path,
                    offset: task.log_offset,
                    child_pid: pid,
                });
                Server::persist_task(task);
                continue;
            }

            if task.child_pid.is_some() {
                drain_task_log_once(task, &log_path);
                match final_status_from_log(&log_path) {
                    Some((status, error)) => {
                        let status_label = status.to_string();
                        task.status = status;
                        task.error = error;
                        task.completed_at = Some(Utc::now());
                        // v1.4.4: Extract real output from log when child completed successfully
                        if task.status == TaskStatus::Done && task.output.is_empty() {
                            if let Some(extracted) =
                                extract_output_from_log(&log_path, &task.backend)
                            {
                                task.output = extracted;
                                task.watchdog_observations.push(format!(
                                    "[{}] Extracted {} chars of output from child.log",
                                    Utc::now().format("%H:%M:%S"),
                                    task.output.len()
                                ));
                            }
                        }
                        task.watchdog_observations.push(format!(
                            "[{}] Child PID is dead; finalized from log marker as {}",
                            Utc::now().format("%H:%M:%S"),
                            status_label
                        ));
                        Server::persist_recovered_terminal_task(task);
                        if task.status == TaskStatus::Failed && !is_session {
                            recovery_failed_ids.push(task.id.clone());
                        }
                    }
                    None => {
                        task.status = TaskStatus::Failed;
                        task.error = Some("orphaned-pid-dead".to_string());
                        task.completed_at = Some(Utc::now());
                        task.watchdog_observations.push(format!(
                            "[{}] Child PID is dead and log has no clean completion marker — marking failed (orphaned-pid-dead)",
                            Utc::now().format("%H:%M:%S")
                        ));
                        Server::persist_recovered_terminal_task(task);
                        if !is_session {
                            recovery_failed_ids.push(task.id.clone());
                        }
                    }
                }
            } else {
                task.status = TaskStatus::Failed;
                task.error = Some("orphaned-no-pid".to_string());
                task.completed_at = Some(Utc::now());
                task.watchdog_observations.push(format!(
                    "[{}] log_path exists but child_pid is missing — marking failed (orphaned-no-pid)",
                    Utc::now().format("%H:%M:%S")
                ));
                Server::persist_recovered_terminal_task(task);
                if !is_session {
                    recovery_failed_ids.push(task.id.clone());
                }
            }
        }

        (recovery_failed_ids, reconnect_specs)
    }

    fn new(runtime: tokio::runtime::Handle) -> Self {
        // Try to load OpenAI key from env
        let openai_key = std::env::var("OPENAI_API_KEY").ok();
        if openai_key.is_none() {
            warn!("OPENAI_API_KEY not set - GPT backend will fail until configured");
        }

        // Ensure tasks directory exists
        std::fs::create_dir_all(tasks_dir()).ok();
        std::fs::create_dir_all(history_dir()).ok();

        // Load any persisted tasks
        let mut tasks = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(tasks_dir()) {
            for entry in entries.flatten() {
                if entry.path().extension().is_some_and(|e| e == "json") {
                    if let Ok(data) = std::fs::read_to_string(entry.path()) {
                        if let Ok(task) = serde_json::from_str::<Task>(&data) {
                            tasks.insert(task.id.clone(), task);
                        }
                    }
                }
            }
        }

        // v1.4.4: Scan tasks/*/state.json for in-flight tasks not yet in flat JSON files.
        // Covers the gap where task_submit wrote state.json but manager crashed before
        // persist_task finished writing the flat {id}.json.
        if let Ok(entries) = std::fs::read_dir(tasks_dir()) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let state_path = path.join("state.json");
                    if state_path.exists() {
                        if let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) {
                            if !tasks.contains_key(dir_name) {
                                if let Ok(data) = std::fs::read_to_string(&state_path) {
                                    if let Ok(state) = serde_json::from_str::<TaskState>(&data) {
                                        info!(
                                            "Recovered task {} from state.json (no flat JSON file)",
                                            state.task_id
                                        );
                                        let backend = match state.backend.as_str() {
                                            "gpt" => Backend::Gpt,
                                            "gemini" => Backend::Gemini,
                                            "codex" => Backend::Codex,
                                            _ => Backend::ClaudeCode,
                                        };
                                        let task = Task {
                                            id: state.task_id.clone(),
                                            backend,
                                            prompt: state.prompt_preview.clone(),
                                            system_prompt: None,
                                            model: None,
                                            working_dir: state.working_dir.clone(),
                                            status: TaskStatus::Running,
                                            output: String::new(),
                                            error: None,
                                            created_at: state.created_at,
                                            started_at: state.started_at,
                                            completed_at: None,
                                            progress_lines: 0,
                                            steps: Vec::new(),
                                            last_activity: None,
                                            last_output_chunk_at: None,
                                            stall_detected: false,
                                            extraction_status: ExtractionStatus::None,
                                            trust_score: 0,
                                            trust_level: TrustLevel::Low,
                                            rollback_path: None,
                                            validation_status: ValidationStatus::NotChecked,
                                            assertions: Vec::new(),
                                            backed_up_files: Vec::new(),
                                            retry_count: 0,
                                            max_retries: 2,
                                            retry_of: None,
                                            error_context: None,
                                            input_tokens: 0,
                                            output_tokens: 0,
                                            cost_usd: 0.0,
                                            on_complete: None,
                                            role: None,
                                            save_artifact: false,
                                            rerun_of: None,
                                            parent_task_id: None,
                                            forked_from: None,
                                            continuation_of: None,
                                            child_pid: state.child_pid,
                                            log_path: state.log_path.clone(),
                                            log_offset: 0,
                                            reconnected: false,
                                            watchdog_observations: vec![format!(
                                                "[{}] Recovered from state.json only (flat JSON missing)",
                                                Utc::now().format("%H:%M:%S")
                                            )],
                                            fingerprint: None,
                                            superseded_by: None,
                                            label: None,
                                            current_step: None,
                                            total_steps: None,
                                            current_step_desc: None,
                                            live_activity: None,
                                            effort: None,
                                            notify_on_complete: None,
                                            notify_on_fail: None,
                                            notify_on_destroy: None,
                                            notify_title: None,
                                            notify_body: None,
                                        };
                                        Server::persist_task(&task);
                                        tasks.insert(task.id.clone(), task);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let (recovery_failed_ids, reconnect_specs) = Server::reconnect_orphaned_tasks(&mut tasks);

        // v1.3.3 (Opus review B1): fire notify for tasks marked Failed during recovery.
        // Notifier wasn't available during the load loop above, but we can use RealNotifier
        // directly here since that's what the Server struct would have anyway.
        // Only fires for tasks that had notify_on_fail: true.
        // v1.3.5: fire notifies in a background thread so PowerShell subprocess doesn't block
        // MCP init handshake (was causing Claude Desktop to kill manager on restart).
        if !recovery_failed_ids.is_empty() {
            let recovery_tasks: Vec<Task> = recovery_failed_ids
                .iter()
                .filter_map(|id| tasks.get(id).cloned())
                .collect();
            std::thread::spawn(move || {
                for task in &recovery_tasks {
                    check_and_fire_task_notify(task, &RealNotifier);
                    // v1.3.4: auto-advance loaf phase if task was linked to current phase
                    auto_advance_loaf_on_task_complete(task);
                }
            });
        }

        let server = Server {
            tasks: Arc::new(RwLock::new(tasks)),
            config: Arc::new(RwLock::new(ServerConfig {
                openai_api_key: openai_key,
                default_gpt_model: DEFAULT_GPT_MODEL.to_string(),
                default_working_dir: default_user_home(),
            })),
            runtime,
            stdout: Arc::new(Mutex::new(io::stdout())),
            notifier: Arc::new(RealNotifier),
            recent_tool_calls: Arc::new(Mutex::new(VecDeque::new())),
            stdin_pipes: Arc::new(RwLock::new(HashMap::new())),
        };

        for spec in reconnect_specs {
            let tasks_c = server.tasks.clone();
            server.runtime.spawn(tail_task_log(
                tasks_c,
                spec.task_id.clone(),
                spec.log_path.clone(),
                spec.offset,
                Some(spec.child_pid),
                false,
            ));
            server.runtime.spawn(watch_reconnected_process_exit(
                server.tasks.clone(),
                spec.task_id,
                spec.log_path,
                spec.child_pid,
            ));
        }

        server
    }

    /// Write a JSON-RPC message to stdout (shared across threads).
    fn write_stdout(&self, msg: &str) {
        if let Ok(mut out) = self.stdout.lock() {
            let _ = writeln!(out, "{}", msg);
            let _ = out.flush();
        }
    }

    /// Send an MCP log notification (no id, no response expected).
    /// These show in Claude Code's status area, costing zero LLM tokens.
    fn send_notification(&self, level: &str, message: &str) {
        let notif = json!({
            "jsonrpc": "2.0",
            "method": "notifications/message",
            "params": {
                "level": level,
                "logger": "manager",
                "data": message
            }
        });
        self.write_stdout(&serde_json::to_string(&notif).unwrap_or_default());
    }

    fn persist_task(task: &Task) {
        let path = format!("{}\\{}.json", tasks_dir(), task.id);
        if let Ok(data) = serde_json::to_string_pretty(task) {
            std::fs::write(path, data).ok();
        }
    }

    fn save_to_history(task: &Task) {
        let history_path = format!("{}\\task_history.json", history_dir());
        let prompt_summary: String = if task.prompt.len() > 100 {
            safe_truncate(&task.prompt, 100)
        } else {
            task.prompt.clone()
        };
        let entry = json!({
            "task_id": task.id,
            "backend": task.backend.to_string(),
            "status": task.status.to_string(),
            "prompt_summary": prompt_summary,
            "step_count": task.steps.len(),
            "steps": task.steps.iter().map(|s| json!({"tool": s.tool, "status": s.status})).collect::<Vec<Value>>(),
            "output_preview": safe_truncate(&task.output, 500),
            "error": task.error,
            "created_at": task.created_at.to_rfc3339(),
            "started_at": task.started_at.map(|s| s.to_rfc3339()),
            "completed_at": task.completed_at.map(|s| s.to_rfc3339()),
            "duration_secs": task.started_at.and_then(|s| task.completed_at.map(|c| (c - s).num_seconds())),
        });

        let mut history: Vec<Value> = std::fs::read_to_string(&history_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();

        history.retain(|e| e.get("task_id").and_then(|v| v.as_str()) != Some(&task.id));
        history.push(entry);

        if history.len() > MAX_HISTORY_ENTRIES {
            let drain = history.len() - MAX_HISTORY_ENTRIES;
            history.drain(..drain);
        }

        if let Ok(data) = serde_json::to_string_pretty(&history) {
            std::fs::write(&history_path, data).ok();
        }
    }

    /// Item 13/14: Flag task for extraction review based on outcome
    fn flag_extraction(task: &mut Task) {
        match task.status {
            TaskStatus::Done if task.steps.len() >= 3 => {
                task.extraction_status = ExtractionStatus::PendingSuccess;
            }
            TaskStatus::Failed if !task.steps.is_empty() => {
                task.extraction_status = ExtractionStatus::PendingFailure;
            }
            _ => {
                task.extraction_status = ExtractionStatus::TooSimple;
            }
        }
    }

    /// Calculate trust score from prompt content. Higher = riskier = more safeguards.
    fn calculate_trust(task: &mut Task) {
        let prompt_lower = task.prompt.to_lowercase();
        let mut score: u8 = 1;

        // File operations
        if prompt_lower.contains("delete")
            || prompt_lower.contains("remove")
            || prompt_lower.contains("rm ")
        {
            score += 3;
        }
        if prompt_lower.contains("overwrite") || prompt_lower.contains("replace") {
            score += 2;
        }
        if prompt_lower.contains("format") || prompt_lower.contains("drop") {
            score += 4;
        }

        // Git operations
        if prompt_lower.contains("git push") || prompt_lower.contains("force push") {
            score += 3;
        }
        if prompt_lower.contains("git reset --hard") {
            score += 4;
        }

        // System operations
        if prompt_lower.contains("registry") || prompt_lower.contains("regedit") {
            score += 5;
        }
        if prompt_lower.contains("install") || prompt_lower.contains("uninstall") {
            score += 2;
        }

        // Scope amplifiers
        if prompt_lower.contains("all files") || prompt_lower.contains("recursive") {
            score += 2;
        }
        if prompt_lower.contains("production") || prompt_lower.contains("deploy") {
            score += 1;
        }

        // Cap at 10
        score = score.min(10);

        task.trust_score = score;
        task.trust_level = match score {
            1..=3 => TrustLevel::Low,
            4..=6 => TrustLevel::Medium,
            7..=10 => TrustLevel::High,
            _ => TrustLevel::Low,
        };
    }

    /// Create rollback backups for files mentioned in the prompt.
    fn create_rollback(task: &mut Task) {
        if task.trust_level == TrustLevel::Low {
            return;
        }

        let rollback_dir = format!(r"{}\{}", rollback_dir(), task.id);
        let _ = std::fs::create_dir_all(&rollback_dir);

        // Extract file paths from prompt (Windows paths like C:\...\file.ext)
        let path_regex = match regex::Regex::new(r#"[A-Za-z]:\\[^\s,;'"]+\.\w+"#) {
            Ok(r) => r,
            Err(_) => return,
        };

        let mut backed_up = Vec::new();
        for m in path_regex.find_iter(&task.prompt) {
            let path = m.as_str();
            if std::path::Path::new(path).exists() {
                let filename = std::path::Path::new(path)
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_else(|| "unknown".into());
                let backup_path = format!(r"{}\{}", rollback_dir, filename);
                if std::fs::copy(path, &backup_path).is_ok() {
                    backed_up.push(path.to_string());
                }
            }
        }

        if !backed_up.is_empty() {
            task.rollback_path = Some(rollback_dir);
            task.backed_up_files = backed_up;
        }
    }

    /// Restore backed up files on failure.
    fn rollback(task: &Task) -> Result<Vec<String>, String> {
        let rollback_dir = task
            .rollback_path
            .as_ref()
            .ok_or("No rollback data for this task")?;

        let mut restored = Vec::new();
        for original_path in &task.backed_up_files {
            let filename = std::path::Path::new(original_path)
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_default();
            let backup_path = format!(r"{}\{}", rollback_dir, filename);
            if std::path::Path::new(&backup_path).exists() {
                match std::fs::copy(&backup_path, original_path) {
                    Ok(_) => restored.push(original_path.clone()),
                    Err(e) => return Err(format!("Failed to restore {}: {}", original_path, e)),
                }
            }
        }
        Ok(restored)
    }

    /// Infer assertions from the prompt and validate after task completion.
    fn validate_output(task: &mut Task) {
        let prompt_lower = task.prompt.to_lowercase();
        let mut assertions: Vec<String> = Vec::new();

        // Extract file paths that should exist after creation
        let path_regex = match regex::Regex::new(
            r#"[Cc]reate\s+(?:a\s+file\s+(?:at\s+)?)?([A-Za-z]:\\[^\s,;'"]+\.\w+)"#,
        ) {
            Ok(r) => r,
            Err(_) => {
                task.validation_status = ValidationStatus::Skipped;
                return;
            }
        };
        for cap in path_regex.captures_iter(&task.prompt) {
            if let Some(path) = cap.get(1) {
                assertions.push(format!("file_exists:{}", path.as_str()));
            }
        }

        // Check for build commands -> expect binary
        if prompt_lower.contains("cargo build") && prompt_lower.contains("--release") {
            let pkg_regex = regex::Regex::new(r"-p\s+(\S+)").ok();
            if let Some(re) = pkg_regex {
                if let Some(cap) = re.captures(&task.prompt) {
                    if let Some(pkg) = cap.get(1) {
                        assertions.push(format!(
                            r"file_exists:{}\target\release\{}.exe",
                            workspace_root(),
                            pkg.as_str()
                        ));
                    }
                }
            }
        }

        // Run assertions
        if assertions.is_empty() {
            task.validation_status = ValidationStatus::Skipped;
            task.assertions = assertions;
            return;
        }

        let mut all_passed = true;
        let mut checked_assertions = Vec::new();
        for assertion in &assertions {
            if let Some(path) = assertion.strip_prefix("file_exists:") {
                let passed = std::path::Path::new(path).exists();
                checked_assertions.push(format!(
                    "{}:{}",
                    assertion,
                    if passed { "PASS" } else { "FAIL" }
                ));
                if !passed {
                    all_passed = false;
                }
            }
        }

        task.assertions = checked_assertions;
        task.validation_status = if all_passed {
            ValidationStatus::Passed
        } else {
            ValidationStatus::Failed
        };

        // Item 18: Clean up rollback backup when validation passes
        if all_passed {
            if let Some(ref rollback_dir) = task.rollback_path {
                if std::fs::remove_dir_all(rollback_dir).is_ok() {
                    info!("Backup cleaned: validation passed for {}", task.id);
                    task.rollback_path = None;
                    task.backed_up_files.clear();
                }
            }
        }
    }

    /// Item 3: Generate smart end report. Success = summary. Failure = step trail.
    fn generate_end_report(task: &Task) -> String {
        // v1.3.2: Char-boundary-safe slice — walk back from len-500 to the nearest valid UTF-8 char boundary
        // so multi-byte chars (em-dashes, arrows, curly quotes) in task output don't panic slicing.
        fn safe_tail(s: &str, max_chars: usize) -> &str {
            if s.chars().count() <= max_chars {
                return s;
            }
            // Find the byte index that leaves approximately max_chars from the end
            let skip = s.chars().count() - max_chars;
            match s.char_indices().nth(skip) {
                Some((byte_idx, _)) => &s[byte_idx..],
                None => s,
            }
        }

        match task.status {
            TaskStatus::Done => {
                // Success: last ~500 chars of output as summary (char-safe)
                let out = &task.output;
                if out.len() > 500 {
                    format!(
                        "âœ“ Task completed ({} steps)\n\n{}",
                        task.steps.len(),
                        safe_tail(out, 500)
                    )
                } else {
                    format!("âœ“ Task completed ({} steps)\n\n{}", task.steps.len(), out)
                }
            }
            TaskStatus::Failed => {
                // Failure: step trail + error
                let mut report = format!("âœ— Task failed after {} steps\n\n", task.steps.len());
                report.push_str("Step trail:\n");
                for (i, step) in task.steps.iter().enumerate() {
                    let mark = match step.status.as_str() {
                        "completed" => "âœ“",
                        "error" => "âœ—",
                        _ => "—",
                    };
                    report.push_str(&format!(
                        "  {} {}. {} ({})\n",
                        mark,
                        i + 1,
                        step.tool,
                        step.status
                    ));
                }
                if let Some(ref err) = task.error {
                    report.push_str(&format!("\nError: {}\n", err));
                }
                // Last 300 chars of output for context
                let out = &task.output;
                if out.len() > 300 {
                    report.push_str(&format!("\nLast output:\n{}", &out[out.len() - 300..]));
                } else if !out.is_empty() {
                    report.push_str(&format!("\nOutput:\n{}", out));
                }
                report
            }
            _ => task.output.clone(),
        }
    }

    /// Returns true if the most recent task step is still "started" (not yet
    /// "completed" or "error"). Used by the inline stall computation in
    /// handle_get_status to avoid false positives during long-running tool
    /// invocations like Write on large files.
    fn active_tool_running(task: &Task) -> bool {
        task.steps
            .last()
            .map(|s| s.status == "started")
            .unwrap_or(false)
    }

    /// Item 4: Read breadcrumb state for Gemini injection.
    /// Priority: local server state (public distribution) → autonomous → None.
    fn read_breadcrumb_state() -> Option<String> {
        // 1. Try local server state dir first (%LOCALAPPDATA%\CPC\state)
        let local_state_dir = {
            let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
            format!(r"{}\CPC\state", local)
        };
        if std::path::Path::new(&local_state_dir).exists() {
            let active_path = format!(r"{}\active_operation.json", local_state_dir);
            if let Some(v) = std::fs::read_to_string(&active_path)
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            {
                let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                let step = v.get("current_step").and_then(|s| s.as_u64()).unwrap_or(0);
                let total = v.get("total_steps").and_then(|s| s.as_u64()).unwrap_or(0);
                return Some(format!(
                    "[CONTEXT: Current operation: {} (step {}/{})]",
                    name, step, total
                ));
            }
            return None; // local state dir exists, no active operation
        }
        // 2. Fall back to autonomous breadcrumb.jsonl
        let autonomous_data = std::env::var("AUTONOMOUS_DATA_DIR").unwrap_or_else(|_| {
            let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
            format!(r"{}\autonomous", local)
        });
        let bc_jsonl = format!(r"{}\logs\breadcrumb.jsonl", autonomous_data);
        read_state_file(&bc_jsonl).map(|line| format!("[CONTEXT: Active breadcrumb: {}]", line))
    }

    /// Item 18: Build a retry task from a failed task. Returns the new Task to be inserted into the store.
    /// Updates the original task's output with a retry note.
    fn prepare_retry(task: &mut Task) -> Option<Task> {
        if task.status != TaskStatus::Failed || task.retry_count >= task.max_retries {
            return None;
        }

        let error_text = task
            .error
            .clone()
            .or_else(|| {
                let lines: Vec<&str> = task.output.lines().collect();
                let tail: Vec<&str> = lines.iter().rev().take(5).copied().collect();
                Some(tail.into_iter().rev().collect::<Vec<_>>().join("\n"))
            })
            .unwrap_or_else(|| "Unknown error".to_string());

        // Escalate backend if at max retries
        let new_backend = if task.retry_count + 1 >= task.max_retries {
            match task.backend {
                Backend::ClaudeCode => Backend::Codex,
                Backend::Codex => Backend::Gemini,
                Backend::Gemini => Backend::ClaudeCode,
                Backend::Gpt => Backend::Gpt, // no escalation for GPT
            }
        } else {
            task.backend.clone()
        };

        // Auto-escalate effort on retry
        let new_effort = match task.effort.as_deref() {
            None | Some("low") | Some("medium") => Some("high".to_string()),
            Some("high") => Some("max".to_string()),
            Some(other) => Some(other.to_string()),
        };

        // Extract original prompt (strip any previous retry injection)
        let original_prompt = task
            .retry_of
            .as_ref()
            .and_then(|_| {
                task.prompt
                    .split("\n\n--- PREVIOUS ATTEMPT FAILED ---")
                    .next()
                    .map(String::from)
            })
            .unwrap_or_else(|| task.prompt.clone());

        let new_prompt = format!(
            "{}\n\n--- PREVIOUS ATTEMPT FAILED ---\nError: {}\nAvoid the approach that caused this error. Try a different strategy.",
            original_prompt, error_text
        );

        let original_id = task.retry_of.clone().unwrap_or_else(|| task.id.clone());
        let new_task_id = Uuid::new_v4().to_string()[..8].to_string();

        let new_task = Task {
            id: new_task_id.clone(),
            backend: new_backend,
            prompt: new_prompt,
            system_prompt: task.system_prompt.clone(),
            model: task.model.clone(),
            working_dir: task.working_dir.clone(),
            status: TaskStatus::Queued,
            output: String::new(),
            error: None,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
            progress_lines: 0,
            steps: Vec::new(),
            last_activity: None,
            last_output_chunk_at: None,
            stall_detected: false,
            extraction_status: ExtractionStatus::None,
            trust_score: 0,
            trust_level: TrustLevel::Low,
            rollback_path: None,
            validation_status: ValidationStatus::NotChecked,
            assertions: Vec::new(),
            backed_up_files: Vec::new(),
            retry_count: task.retry_count + 1,
            max_retries: task.max_retries,
            retry_of: Some(original_id),
            error_context: Some(error_text),
            input_tokens: 0,
            output_tokens: 0,
            cost_usd: 0.0,
            on_complete: task.on_complete.clone(),
            role: task.role.clone(),
            save_artifact: task.save_artifact,
            rerun_of: None,
            parent_task_id: None,
            forked_from: None,
            continuation_of: None,
            child_pid: None,
            log_path: None,
            log_offset: 0,
            reconnected: false,
            watchdog_observations: Vec::new(),
            fingerprint: None,
            superseded_by: None,
            label: None,
            current_step: None,
            total_steps: None,
            current_step_desc: None,
            live_activity: None,
            effort: new_effort,
            notify_on_complete: None,
            notify_on_fail: None,
            notify_on_destroy: None,
            notify_title: None,
            notify_body: None,
        };

        // Note on original task
        task.output
            .push_str(&format!("\n[Retrying as {}]", new_task_id));
        info!(
            "Retry {}/{} for task {} -> new task {}",
            task.retry_count + 1,
            task.max_retries,
            task.id,
            new_task_id
        );

        Some(new_task)
    }

    /// Item 18: Write learned pattern when a retry succeeds.
    fn learn_from_outcome(task: &Task) {
        let original_id = match &task.retry_of {
            Some(id) => id.clone(),
            None => return,
        };

        let original_error = task
            .error_context
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        let output_summary: String = if task.output.len() > 500 {
            task.output[task.output.len() - 500..].to_string()
        } else {
            task.output.clone()
        };
        let prompt_pattern: String = task.prompt.chars().take(200).collect();

        let pattern = json!({
            "type": "error_recovery",
            "original_error": original_error,
            "successful_approach": output_summary,
            "backend_original": original_id,
            "backend_successful": task.backend.to_string(),
            "prompt_pattern": prompt_pattern,
            "retry_count": task.retry_count,
            "learned_at": Utc::now().to_rfc3339(),
        });

        let _ = std::fs::create_dir_all(learned_patterns_dir());
        let filename = format!(
            "{}\\{}_{}.json",
            learned_patterns_dir(),
            Utc::now().format("%Y%m%d_%H%M%S"),
            task.id
        );
        if let Ok(data) = serde_json::to_string_pretty(&pattern) {
            if std::fs::write(&filename, &data).is_ok() {
                info!(
                    "Learned pattern from retry success: {} -> {}",
                    original_id, task.id
                );
            }
        }
    }

    /// Item 16: Analyze a prompt and recommend the best AI backend.
    fn recommend_backend(prompt: &str, working_dir: Option<&str>) -> BackendRecommendation {
        let prompt_lower = prompt.to_lowercase();

        // --- Keyword scoring ---
        struct KeywordRule {
            keywords: &'static [&'static str],
            backend: &'static str,
            weight: f32,
        }
        let rules = [
            KeywordRule {
                keywords: &["build", "cargo", "compile", "rust", "npm", "install"],
                backend: "claude_code",
                weight: 0.8,
            },
            KeywordRule {
                keywords: &["edit", "refactor", "complex", "multi-file", "debug"],
                backend: "claude_code",
                weight: 0.7,
            },
            KeywordRule {
                keywords: &["read", "report", "list", "check", "verify", "simple"],
                backend: "gemini",
                weight: 0.6,
            },
            KeywordRule {
                keywords: &["search", "google", "find online", "look up"],
                backend: "gemini",
                weight: 0.7,
            },
            KeywordRule {
                keywords: &["review", "audit", "sandbox", "safe"],
                backend: "codex",
                weight: 0.6,
            },
            KeywordRule {
                keywords: &["delete", "overwrite", "deploy", "push"],
                backend: "codex",
                weight: 0.7,
            },
            KeywordRule {
                keywords: &["reason", "analyze", "think", "strategy", "plan"],
                backend: "gpt",
                weight: 0.6,
            },
        ];

        let mut keyword_scores: HashMap<&str, f32> = HashMap::new();
        for rule in &rules {
            let hit_count = rule
                .keywords
                .iter()
                .filter(|kw| prompt_lower.contains(**kw))
                .count();
            if hit_count > 0 {
                let score = rule.weight * (hit_count as f32 / rule.keywords.len() as f32);
                *keyword_scores.entry(rule.backend).or_insert(0.0) += score;
            }
        }

        // Working dir hints
        if let Some(wd) = working_dir {
            let wd_lower = wd.to_lowercase();
            if wd_lower.contains("rust-mcp") || wd_lower.contains("cargo") {
                *keyword_scores.entry("claude_code").or_insert(0.0) += 0.3;
            }
        }

        // --- Historical success rates ---
        let history_path = format!("{}\\task_history.json", history_dir());
        let mut backend_stats: HashMap<String, (u32, u32, u64)> = HashMap::new(); // (success, total, total_steps)

        if let Ok(data) = std::fs::read_to_string(&history_path) {
            if let Ok(history) = serde_json::from_str::<Vec<Value>>(&data) {
                for entry in &history {
                    let be = entry.get("backend").and_then(|v| v.as_str()).unwrap_or("");
                    let status = entry.get("status").and_then(|v| v.as_str()).unwrap_or("");
                    let steps = entry
                        .get("step_count")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let stat = backend_stats.entry(be.to_string()).or_insert((0, 0, 0));
                    stat.1 += 1;
                    stat.2 += steps;
                    if status == "done" {
                        stat.0 += 1;
                    }
                }
            }
        }

        // --- Learned error patterns ---
        let mut error_penalties: HashMap<String, f32> = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(learned_patterns_dir()) {
            for entry in entries.flatten() {
                if let Ok(data) = std::fs::read_to_string(entry.path()) {
                    if let Ok(pattern) = serde_json::from_str::<Value>(&data) {
                        let prompt_pattern = pattern
                            .get("prompt_pattern")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        // Check if this learned error is relevant to the current prompt
                        let pattern_words: Vec<&str> =
                            prompt_pattern.split_whitespace().take(5).collect();
                        let overlap = pattern_words
                            .iter()
                            .filter(|w| prompt_lower.contains(&w.to_lowercase()))
                            .count();
                        if overlap >= 2 {
                            let failed_backend = pattern
                                .get("backend_original")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            *error_penalties
                                .entry(failed_backend.to_string())
                                .or_insert(0.0) += 0.15;
                        }
                    }
                }
            }
        }

        // --- Combine scores: keyword 60%, history 30%, speed 10% ---
        let all_backends = ["claude_code", "gemini", "codex", "gpt"];
        let speed_scores: HashMap<&str, f32> = [
            ("gemini", 0.9),
            ("codex", 0.7),
            ("claude_code", 0.5),
            ("gpt", 0.4),
        ]
        .into_iter()
        .collect();

        let mut final_scores: Vec<(&str, f32)> = Vec::new();
        for be in &all_backends {
            let kw = keyword_scores.get(*be).copied().unwrap_or(0.0);
            let hist = backend_stats
                .get(*be)
                .map(
                    |(s, t, _)| {
                        if *t == 0 {
                            0.5
                        } else {
                            *s as f32 / *t as f32
                        }
                    },
                )
                .unwrap_or(0.5);
            let spd = speed_scores.get(*be).copied().unwrap_or(0.5);
            let penalty = error_penalties.get(*be).copied().unwrap_or(0.0);
            let score = (kw * 0.6) + (hist * 0.3) + (spd * 0.1) - penalty;
            final_scores.push((be, score));
        }

        final_scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let best = final_scores[0];
        let alternatives: Vec<String> = final_scores[1..]
            .iter()
            .map(|(be, _)| be.to_string())
            .collect();

        // Confidence: how far ahead the winner is
        let confidence = if final_scores.len() >= 2 {
            let gap = best.1 - final_scores[1].1;
            (0.5 + gap).clamp(0.1, 1.0)
        } else {
            0.5
        };

        // Build reasoning
        let mut reasons = Vec::new();
        if let Some(&kw) = keyword_scores.get(best.0) {
            if kw > 0.0 {
                reasons.push(format!("keyword match score {:.2}", kw));
            }
        }
        if let Some((s, t, avg_steps)) = backend_stats.get(best.0) {
            if *t > 0 {
                reasons.push(format!(
                    "historical: {}/{} success, avg {:.1} steps",
                    s,
                    t,
                    *avg_steps as f32 / *t as f32
                ));
            }
        }
        if error_penalties.values().any(|&v| v > 0.0) {
            reasons.push("learned error patterns applied".to_string());
        }

        let reasoning = if reasons.is_empty() {
            format!("{} selected as default (no strong signals)", best.0)
        } else {
            format!("{} selected: {}", best.0, reasons.join("; "))
        };

        BackendRecommendation {
            recommended_backend: best.0.to_string(),
            confidence,
            reasoning,
            alternatives,
        }
    }
}

/// Item 18: Spawn execution for a retry task based on its backend.
fn spawn_retry_execution(
    retry_task: &Task,
    tasks: Arc<RwLock<HashMap<String, Task>>>,
    config: Option<Arc<RwLock<ServerConfig>>>,
    handle: &tokio::runtime::Handle,
) {
    let tid = retry_task.id.clone();
    let prompt = retry_task.prompt.clone();
    let wd = retry_task
        .working_dir
        .clone()
        .unwrap_or_else(default_user_home);
    let model = retry_task.model.clone();

    match retry_task.backend {
        Backend::Gpt => {
            if let Some(cfg) = config {
                handle.spawn(run_gpt_task(cfg, tasks, tid));
            } else {
                info!("Cannot retry GPT task {} - no config available", tid);
            }
        }
        Backend::ClaudeCode => {
            let mut args = vec![
                "-p".to_string(),
                prompt,
                "--dangerously-skip-permissions".to_string(),
                "--verbose".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--add-dir".to_string(),
                r"C:\temp".to_string(),
                "--add-dir".to_string(),
                volumes_dir(),
                "--add-dir".to_string(),
                r"C:\CPC".to_string(),
                "--add-dir".to_string(),
                workspace_root(),
                "--add-dir".to_string(),
                wd,
            ];
            if let Some(m) = model {
                args.push("--model".to_string());
                args.push(m);
            }
            handle.spawn(run_cli_task(
                tasks,
                tid,
                claude_code_cmd(),
                args,
                None,
                StdinMode::Null,
            ));
        }
        Backend::Codex => {
            let args = vec![
                "exec".into(),
                "--json".into(),
                "--skip-git-repo-check".into(),
                "--full-auto".into(),
                "--cd".into(),
                wd.clone(),
                "--".into(),
                prompt,
            ];
            handle.spawn(run_codex_task(tasks, tid, args, wd));
        }
        Backend::Gemini => {
            let mut args = vec![
                gemini_cmd().to_string(),
                "-p".into(),
                prompt,
                "--yolo".into(),
            ];
            if let Some(m) = model {
                args.push("--model".to_string());
                args.push(m);
            }
            handle.spawn(run_cli_task(
                tasks,
                tid,
                node_cmd(),
                args,
                None,
                StdinMode::Null,
            ));
        }
    }
}

/// Spawn an on_complete follow-up task when a task finishes successfully.
/// Call after the completion block while still holding access to the tasks arc.
fn spawn_on_complete(
    completed_task: &Task,
    tasks: Arc<RwLock<HashMap<String, Task>>>,
    config: Option<Arc<RwLock<ServerConfig>>>,
    handle: &tokio::runtime::Handle,
) {
    if completed_task.status != TaskStatus::Done {
        return;
    }
    let prompt = match completed_task.on_complete {
        Some(ref p) => p.clone(),
        None => return,
    };
    let parent_id = completed_task.id.clone();
    let backend = completed_task.backend.clone();
    let working_dir = completed_task.working_dir.clone();
    let model = completed_task.model.clone();

    let follow_up = Task {
        id: Uuid::new_v4().to_string()[..8].to_string(),
        backend: backend.clone(),
        prompt: format!("[ON_COMPLETE of task {}]\n{}", parent_id, prompt),
        system_prompt: None,
        model,
        working_dir,
        status: TaskStatus::Queued,
        output: String::new(),
        error: None,
        created_at: Utc::now(),
        started_at: None,
        completed_at: None,
        progress_lines: 0,
        steps: Vec::new(),
        last_activity: None,
        last_output_chunk_at: None,
        stall_detected: false,
        extraction_status: ExtractionStatus::None,
        trust_score: 0,
        trust_level: TrustLevel::Low,
        rollback_path: None,
        validation_status: ValidationStatus::NotChecked,
        assertions: Vec::new(),
        backed_up_files: Vec::new(),
        retry_count: 0,
        max_retries: 2,
        retry_of: None,
        error_context: None,
        input_tokens: 0,
        output_tokens: 0,
        cost_usd: 0.0,
        on_complete: None,
        role: None,
        save_artifact: false,
        rerun_of: None,
        parent_task_id: None,
        forked_from: None,
        continuation_of: None,
        child_pid: None,
        log_path: None,
        log_offset: 0,
        reconnected: false,
        watchdog_observations: Vec::new(),
        fingerprint: None,
        superseded_by: None,
        label: None,
        current_step: None,
        total_steps: None,
        current_step_desc: None,
        live_activity: None,
        effort: None,
        notify_on_complete: None,
        notify_on_fail: None,
        notify_on_destroy: None,
        notify_title: None,
        notify_body: None,
    };

    info!(
        "on_complete: spawning follow-up task {} from completed task {}",
        follow_up.id, parent_id
    );
    Server::persist_task(&follow_up);
    let tasks_bg = tasks.clone();
    let follow_id = follow_up.id.clone();
    handle.block_on(async {
        let mut store = tasks_bg.write().await;
        store.insert(follow_id.clone(), follow_up.clone());
    });
    spawn_retry_execution(&follow_up, tasks, config, handle);
}

// ============================================================================
// Backend Execution
// ============================================================================

async fn run_gpt_task(
    config: Arc<RwLock<ServerConfig>>,
    tasks: Arc<RwLock<HashMap<String, Task>>>,
    task_id: String,
) {
    // Mark running
    {
        let mut store = tasks.write().await;
        if let Some(task) = store.get_mut(&task_id) {
            task.status = TaskStatus::Running;
            task.started_at = Some(Utc::now());
            Server::calculate_trust(task);
            Server::create_rollback(task);
            Server::persist_task(task);
        }
    }

    // Get task details + config
    let (prompt, system_prompt, model) = {
        let store = tasks.read().await;
        let task = store.get(&task_id).unwrap();
        (
            task.prompt.clone(),
            task.system_prompt.clone(),
            task.model.clone(),
        )
    };

    let (api_key, default_model) = {
        let cfg = config.read().await;
        (cfg.openai_api_key.clone(), cfg.default_gpt_model.clone())
    };

    let api_key = match api_key {
        Some(k) => k,
        None => {
            let mut store = tasks.write().await;
            let mut retry_task: Option<Task> = None;
            if let Some(task) = store.get_mut(&task_id) {
                task.status = TaskStatus::Failed;
                task.error =
                    Some("OPENAI_API_KEY not configured. Use configure tool to set it.".into());
                task.completed_at = Some(Utc::now());
                Server::flag_extraction(task);
                // Item 18: retry/learn hooks
                retry_task = Server::prepare_retry(task);
                if task.status == TaskStatus::Done && task.retry_of.is_some() {
                    Server::learn_from_outcome(task);
                }
                Server::persist_task(task);
                Server::save_to_history(task);
                // v1.3.2 (Opus review): fire task notify on early-exit before lock release
                check_and_fire_task_notify(task, &RealNotifier);
                // v1.3.4: auto-advance loaf phase if task was linked to current phase
                auto_advance_loaf_on_task_complete(task);
            }
            if let Some(ref rt) = retry_task {
                store.insert(rt.id.clone(), rt.clone());
                Server::persist_task(rt);
            }
            drop(store);
            if let Some(ref rt) = retry_task {
                spawn_retry_execution(
                    rt,
                    tasks.clone(),
                    Some(config.clone()),
                    &tokio::runtime::Handle::current(),
                );
            }
            return;
        }
    };

    let model = model.unwrap_or(default_model);

    // Build messages
    let mut messages = Vec::new();
    if let Some(sys) = system_prompt {
        messages.push(json!({"role": "system", "content": sys}));
    }
    messages.push(json!({"role": "user", "content": prompt}));

    let body = json!({
        "model": model,
        "messages": messages,
    });

    // Call OpenAI API
    let client = reqwest::Client::new();
    let result = client
        .post(GPT_API_URL)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await;

    let mut store = tasks.write().await;
    let mut retry_task: Option<Task> = None;
    let mut completed_snap: Option<Task> = None;
    if let Some(task) = store.get_mut(&task_id) {
        match result {
            Ok(response) => {
                let status_code = response.status();
                match response.text().await {
                    Ok(text) => {
                        if status_code.is_success() {
                            // Parse response
                            if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
                                let content = parsed["choices"][0]["message"]["content"]
                                    .as_str()
                                    .unwrap_or("(no content in response)");
                                let usage = &parsed["usage"];
                                let model_used = parsed["model"].as_str().unwrap_or(&model);
                                task.output = format!(
                                    "Model: {}\nTokens: prompt={}, completion={}, total={}\n\n{}",
                                    model_used,
                                    usage["prompt_tokens"].as_u64().unwrap_or(0),
                                    usage["completion_tokens"].as_u64().unwrap_or(0),
                                    usage["total_tokens"].as_u64().unwrap_or(0),
                                    content
                                );
                                task.status = TaskStatus::Done;
                            } else {
                                task.output = text;
                                task.status = TaskStatus::Done;
                            }
                        } else {
                            task.status = TaskStatus::Failed;
                            task.error = Some(format!("HTTP {}: {}", status_code, text));
                        }
                    }
                    Err(e) => {
                        task.status = TaskStatus::Failed;
                        task.error = Some(format!("Failed to read response: {}", e));
                    }
                }
            }
            Err(e) => {
                task.status = TaskStatus::Failed;
                task.error = Some(format!("Request failed: {}", e));
            }
        }
        task.completed_at = Some(Utc::now());
        if task.status == TaskStatus::Done {
            Server::validate_output(task);
        }
        Server::flag_extraction(task);
        // Item 18: retry/learn hooks
        if task.status == TaskStatus::Failed {
            retry_task = Server::prepare_retry(task);
        }
        if task.status == TaskStatus::Done && task.retry_of.is_some() {
            Server::learn_from_outcome(task);
        }
        Server::persist_task(task);
        Server::save_to_history(task);
        save_task_artifact(task);
        remove_task_state(&task.id);
        // v1.3.1: task notify
        check_and_fire_task_notify(task, &RealNotifier);
        // v1.3.4: auto-advance loaf phase if task was linked to current phase
        auto_advance_loaf_on_task_complete(task);
        completed_snap = Some(task.clone());
    }
    if let Some(ref rt) = retry_task {
        store.insert(rt.id.clone(), rt.clone());
        Server::persist_task(rt);
    }
    drop(store);
    if let Some(ref rt) = retry_task {
        spawn_retry_execution(
            rt,
            tasks.clone(),
            Some(config.clone()),
            &tokio::runtime::Handle::current(),
        );
    }
    if let Some(ref ct) = completed_snap {
        spawn_on_complete(
            ct,
            tasks.clone(),
            Some(config.clone()),
            &tokio::runtime::Handle::current(),
        );
    }
}

/// Codex-specific runner: synchronous capture via spawn_blocking.
/// TokioCommand piped stdout doesn't work for codex.exe on Windows ARM.
async fn run_codex_task(
    tasks: Arc<RwLock<HashMap<String, Task>>>,
    task_id: String,
    args: Vec<String>,
    working_dir: String,
) {
    {
        let mut store = tasks.write().await;
        if let Some(task) = store.get_mut(&task_id) {
            task.status = TaskStatus::Running;
            task.started_at = Some(Utc::now());
            Server::calculate_trust(task);
            Server::create_rollback(task);
            Server::persist_task(task);
        }
    }

    let log_path = match prepare_task_log(&task_id) {
        Ok(path) => path,
        Err(e) => {
            let mut store = tasks.write().await;
            if let Some(task) = store.get_mut(&task_id) {
                task.status = TaskStatus::Failed;
                task.error = Some(format!("Failed to prepare child log: {}", e));
                task.completed_at = Some(Utc::now());
                Server::persist_task(task);
                Server::save_to_history(task);
                check_and_fire_task_notify(task, &RealNotifier);
            }
            return;
        }
    };
    {
        let mut store = tasks.write().await;
        if let Some(task) = store.get_mut(&task_id) {
            task.log_path = Some(log_path.clone());
            task.log_offset = 0;
            Server::persist_task(task);
            write_task_state(task);
        }
    }

    let args_clone = args.clone();
    let wd = working_dir.clone();
    let log_path_for_child = log_path.clone();
    let task_id_for_marker = task_id.clone();
    let (pid_tx, pid_rx) = tokio::sync::oneshot::channel::<u32>();
    let result =
        tokio::task::spawn_blocking(move || -> Result<std::process::ExitStatus, String> {
            let (stdout_stdio, stderr_stdio) =
                open_child_log_stdio(&log_path_for_child).map_err(|e| e.to_string())?;
            let mut child = std::process::Command::new(codex_cmd())
                .args(&args_clone)
                .current_dir(&wd)
                .stdout(stdout_stdio)
                .stderr(stderr_stdio)
                .spawn()
                .map_err(|e| e.to_string())?;
            let _ = pid_tx.send(child.id());
            let status = child.wait().map_err(|e| e.to_string())?;
            append_final_log_marker(
                &log_path_for_child,
                &task_id_for_marker,
                status.success(),
                status.code(),
                if status.success() {
                    None
                } else {
                    Some("process-exit-nonzero")
                },
            );
            Ok(status)
        });

    if let Ok(pid) = pid_rx.await {
        let mut store = tasks.write().await;
        if let Some(task) = store.get_mut(&task_id) {
            task.child_pid = Some(pid);
            Server::persist_task(task);
            write_task_state(task);
        }
        drop(store);
        tokio::spawn(tail_task_log(
            tasks.clone(),
            task_id.clone(),
            log_path.clone(),
            0,
            Some(pid),
            false,
        ));
    }

    let result = result.await;

    let mut store = tasks.write().await;
    let mut retry_task: Option<Task> = None;
    let mut completed_snap: Option<Task> = None;
    if let Some(task) = store.get_mut(&task_id) {
        match result {
            Ok(Ok(status)) => {
                task.status = if status.success() {
                    TaskStatus::Done
                } else {
                    TaskStatus::Failed
                };
                if !status.success() {
                    task.error = Some(format!(
                        "Exit {}. See child log: {}",
                        status.code().unwrap_or(-1),
                        log_path
                    ));
                }
            }
            Ok(Err(e)) => {
                task.status = TaskStatus::Failed;
                task.error = Some(format!("Failed to run codex: {}", e));
            }
            Err(e) => {
                task.status = TaskStatus::Failed;
                task.error = Some(format!("Task panicked: {}", e));
            }
        }
        task.completed_at = Some(Utc::now());
        if task.status == TaskStatus::Done {
            Server::validate_output(task);
        }
        Server::flag_extraction(task);
        // Item 18: retry/learn hooks
        if task.status == TaskStatus::Failed {
            retry_task = Server::prepare_retry(task);
        }
        if task.status == TaskStatus::Done && task.retry_of.is_some() {
            Server::learn_from_outcome(task);
        }
        Server::persist_task(task);
        Server::save_to_history(task);
        save_task_artifact(task);
        remove_task_state(&task.id);
        // v1.3.1: task notify
        check_and_fire_task_notify(task, &RealNotifier);
        // v1.3.4: auto-advance loaf phase if task was linked to current phase
        auto_advance_loaf_on_task_complete(task);
        completed_snap = Some(task.clone());
    }
    if let Some(ref rt) = retry_task {
        store.insert(rt.id.clone(), rt.clone());
        Server::persist_task(rt);
    }
    drop(store);
    if let Some(ref rt) = retry_task {
        spawn_retry_execution(rt, tasks.clone(), None, &tokio::runtime::Handle::current());
    }
    if let Some(ref ct) = completed_snap {
        spawn_on_complete(ct, tasks.clone(), None, &tokio::runtime::Handle::current());
    }
}
async fn run_cli_task(
    tasks: Arc<RwLock<HashMap<String, Task>>>,
    task_id: String,
    command: &str,
    args: Vec<String>,
    stdin_pipes: Option<StdinPipes>,
    stdin_mode: StdinMode,
) {
    // Mark running
    {
        let mut store = tasks.write().await;
        if let Some(task) = store.get_mut(&task_id) {
            task.status = TaskStatus::Running;
            task.started_at = Some(Utc::now());
            Server::calculate_trust(task);
            Server::create_rollback(task);
            Server::persist_task(task);
        }
    }

    // Get working dir
    let working_dir = {
        let store = tasks.read().await;
        store
            .get(&task_id)
            .and_then(|t| t.working_dir.clone())
            .unwrap_or_else(default_user_home)
    };

    // Spawn process — .cmd files need cmd /C on Windows
    let (spawn_cmd, spawn_args) = if command.ends_with(".cmd") || command.ends_with(".bat") {
        let mut all_args = vec!["/C".to_string(), command.to_string()];
        all_args.extend(args.clone());
        ("cmd".to_string(), all_args)
    } else {
        (command.to_string(), args.clone())
    };

    // §12: Set CPC_AGENT_ROLE env var if task has a role
    let task_role = {
        let store = tasks.read().await;
        store.get(&task_id).and_then(|t| t.role.clone())
    };

    let log_path = match prepare_task_log(&task_id) {
        Ok(path) => path,
        Err(e) => {
            let mut store = tasks.write().await;
            if let Some(task) = store.get_mut(&task_id) {
                task.status = TaskStatus::Failed;
                task.error = Some(format!("Failed to prepare child log: {}", e));
                task.completed_at = Some(Utc::now());
                Server::persist_task(task);
                Server::save_to_history(task);
                check_and_fire_task_notify(task, &RealNotifier);
            }
            return;
        }
    };
    {
        let mut store = tasks.write().await;
        if let Some(task) = store.get_mut(&task_id) {
            task.log_path = Some(log_path.clone());
            task.log_offset = 0;
            Server::persist_task(task);
            write_task_state(task);
        }
    }
    let (stdout_stdio, stderr_stdio) = match open_child_log_stdio(&log_path) {
        Ok(stdio) => stdio,
        Err(e) => {
            let mut store = tasks.write().await;
            if let Some(task) = store.get_mut(&task_id) {
                task.status = TaskStatus::Failed;
                task.error = Some(format!("Failed to open child log stdio: {}", e));
                task.completed_at = Some(Utc::now());
                Server::persist_task(task);
                Server::save_to_history(task);
                check_and_fire_task_notify(task, &RealNotifier);
            }
            return;
        }
    };

    let mut cmd = TokioCommand::new(&spawn_cmd);
    cmd.args(&spawn_args)
        .current_dir(&working_dir)
        .stdout(stdout_stdio)
        .stderr(stderr_stdio)
        .stdin(match stdin_mode {
            StdinMode::Null => Stdio::null(),
            StdinMode::Piped => Stdio::piped(),
        });

    // Read effort from task store and set env var
    let task_effort = {
        let store = tasks.read().await;
        store.get(&task_id).and_then(|t| t.effort.clone())
    };

    if let Some(ref role) = task_role {
        cmd.env("CPC_AGENT_ROLE", role);
    }
    if let Some(ref effort) = task_effort {
        cmd.env("CLAUDE_CODE_EFFORT_LEVEL", effort);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let mut store = tasks.write().await;
            let mut retry_task: Option<Task> = None;
            if let Some(task) = store.get_mut(&task_id) {
                task.status = TaskStatus::Failed;
                task.error = Some(format!("Failed to spawn {}: {}", command, e));
                task.completed_at = Some(Utc::now());
                Server::flag_extraction(task);
                // Item 18: retry/learn hooks
                retry_task = Server::prepare_retry(task);
                Server::persist_task(task);
                Server::save_to_history(task);
                // v1.3.1: task notify
                check_and_fire_task_notify(task, &RealNotifier);
                // v1.3.4: auto-advance loaf phase if task was linked to current phase
                auto_advance_loaf_on_task_complete(task);
            }
            if let Some(ref rt) = retry_task {
                store.insert(rt.id.clone(), rt.clone());
                Server::persist_task(rt);
            }
            drop(store);
            if let Some(ref rt) = retry_task {
                spawn_retry_execution(rt, tasks.clone(), None, &tokio::runtime::Handle::current());
            }
            return;
        }
    };

    // Store child PID for lifecycle tracking
    if let Some(pid) = child.id() {
        let mut store = tasks.write().await;
        if let Some(task) = store.get_mut(&task_id) {
            task.child_pid = Some(pid);
            Server::persist_task(task);
            write_task_state(task);
        }
        drop(store);
        tokio::spawn(tail_task_log(
            tasks.clone(),
            task_id.clone(),
            log_path.clone(),
            0,
            Some(pid),
            false,
        ));
    }

    // Capture stdin pipe for send() support
    if let Some(ref pipes) = stdin_pipes {
        if let Some(stdin) = child.stdin.take() {
            let mut pipe_store = pipes.write().await;
            pipe_store.insert(task_id.clone(), Arc::new(tokio::sync::Mutex::new(stdin)));
        }
    }

    // Take stdout/stderr handles before waiting
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();

    // Spawn stdout reader â€” raw byte reading, splits on both \n and \r
    let stdout_handle = if let Some(mut stdout) = stdout.take() {
        let tasks_c = tasks.clone();
        let tid_c = task_id.clone();
        Some(tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            let mut partial = String::new();
            let mut cr_seen = false;
            loop {
                match stdout.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let chunk = String::from_utf8_lossy(&buf[..n]);
                        let mut lines: Vec<String> = Vec::new();
                        for c in chunk.chars() {
                            if c == '\n' {
                                if cr_seen {
                                    // \r\n pair â€” \r already emitted a line
                                    cr_seen = false;
                                    continue;
                                }
                                lines.push(std::mem::take(&mut partial));
                            } else if c == '\r' {
                                cr_seen = true;
                                let line = std::mem::take(&mut partial);
                                if !line.is_empty() {
                                    lines.push(line);
                                }
                            } else {
                                cr_seen = false;
                                partial.push(c);
                            }
                        }
                        if !lines.is_empty() {
                            let mut store = tasks_c.write().await;
                            if let Some(task) = store.get_mut(&tid_c) {
                                if task.status == TaskStatus::Cancelled {
                                    return;
                                }
                                for line in &lines {
                                    if line.is_empty() {
                                        continue;
                                    }
                                    // Try to parse as stream-json event
                                    if let Ok(ev) = serde_json::from_str::<Value>(line) {
                                        // Update activity timestamp for stall detection
                                        task.last_activity = Some(Utc::now());
                                        task.stall_detected = false;
                                        let ev_type =
                                            ev.get("type").and_then(|t| t.as_str()).unwrap_or("");
                                        match ev_type {
                                            "assistant" => {
                                                // Claude Code nests tool_use AND text in message.content[]
                                                if let Some(contents) = ev
                                                    .pointer("/message/content")
                                                    .and_then(|v| v.as_array())
                                                {
                                                    for item in contents {
                                                        let item_type = item
                                                            .get("type")
                                                            .and_then(|t| t.as_str())
                                                            .unwrap_or("");
                                                        match item_type {
                                                            "text" => {
                                                                if let Some(text) = item
                                                                    .get("text")
                                                                    .and_then(|v| v.as_str())
                                                                {
                                                                    if !text.is_empty() {
                                                                        if !task.output.is_empty() {
                                                                            task.output.push('\n');
                                                                        }
                                                                        task.output.push_str(text);
                                                                        task.progress_lines += 1;
                                                                        if task.status
                                                                            == TaskStatus::Stalled
                                                                        {
                                                                            task.status =
                                                                                TaskStatus::Running;
                                                                        }
                                                                        task.last_output_chunk_at =
                                                                            Some(Utc::now());
                                                                    }
                                                                }
                                                            }
                                                            "tool_use" => {
                                                                let tool_name = item
                                                                    .get("name")
                                                                    .and_then(|v| v.as_str())
                                                                    .unwrap_or("unknown")
                                                                    .to_string();
                                                                task.steps.push(TaskStep {
                                                                    tool: tool_name,
                                                                    timestamp: Utc::now(),
                                                                    status: "started".to_string(),
                                                                    summary: item.get("input").map(
                                                                        |v| {
                                                                            let s = v.to_string();
                                                                            {
                                                                                let s_ref: &str =
                                                                                    &s;
                                                                                safe_truncate(
                                                                                    s_ref, 120,
                                                                                )
                                                                            }
                                                                        },
                                                                    ),
                                                                });
                                                            }
                                                            _ => {}
                                                        }
                                                    }
                                                }
                                            }
                                            "user" => {
                                                // Claude Code: tool_result is at .message.content[]
                                                if let Some(contents) = ev
                                                    .pointer("/message/content")
                                                    .and_then(|v| v.as_array())
                                                {
                                                    for item in contents {
                                                        if item.get("type").and_then(|v| v.as_str())
                                                            == Some("tool_result")
                                                        {
                                                            if let Some(last) =
                                                                task.steps.last_mut()
                                                            {
                                                                let is_err = item
                                                                    .get("is_error")
                                                                    .and_then(|v| v.as_bool())
                                                                    .unwrap_or(false);
                                                                last.status = if is_err {
                                                                    "error".to_string()
                                                                } else {
                                                                    "completed".to_string()
                                                                };
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                            "item.completed" => {
                                                // Codex: events have item.type (agent_message, mcp_tool_call)
                                                if let Some(item) = ev.get("item") {
                                                    let item_type = item
                                                        .get("type")
                                                        .and_then(|t| t.as_str())
                                                        .unwrap_or("");
                                                    match item_type {
                                                        "agent_message" => {
                                                            if let Some(text) = item
                                                                .get("text")
                                                                .and_then(|v| v.as_str())
                                                            {
                                                                if !text.is_empty() {
                                                                    if !task.output.is_empty() {
                                                                        task.output.push('\n');
                                                                    }
                                                                    task.output.push_str(text);
                                                                    task.progress_lines += 1;
                                                                    if task.status
                                                                        == TaskStatus::Stalled
                                                                    {
                                                                        task.status =
                                                                            TaskStatus::Running;
                                                                    }
                                                                    task.last_output_chunk_at =
                                                                        Some(Utc::now());
                                                                }
                                                            }
                                                        }
                                                        "mcp_tool_call" => {
                                                            let tool = item
                                                                .get("tool")
                                                                .and_then(|v| v.as_str())
                                                                .unwrap_or("unknown");
                                                            let server = item
                                                                .get("server")
                                                                .and_then(|v| v.as_str())
                                                                .unwrap_or("");
                                                            let tool_name = if server.is_empty() {
                                                                tool.to_string()
                                                            } else {
                                                                format!("{}:{}", server, tool)
                                                            };
                                                            let status = item
                                                                .get("status")
                                                                .and_then(|v| v.as_str())
                                                                .unwrap_or("completed");
                                                            let has_error =
                                                                item.get("error").is_some()
                                                                    && !item
                                                                        .get("error")
                                                                        .unwrap()
                                                                        .is_null();
                                                            task.steps.push(TaskStep {
                                                                tool: tool_name,
                                                                timestamp: Utc::now(),
                                                                status: if has_error {
                                                                    "error".to_string()
                                                                } else {
                                                                    status.to_string()
                                                                },
                                                                summary: item.get("arguments").map(
                                                                    |v| {
                                                                        let s = v.to_string();
                                                                        {
                                                                            let s_ref: &str = &s;
                                                                            safe_truncate(
                                                                                s_ref, 120,
                                                                            )
                                                                        }
                                                                    },
                                                                ),
                                                            });
                                                        }
                                                        _ => {}
                                                    }
                                                }
                                            }
                                            "result" => {
                                                // Skip â€” Claude Code text already captured from assistant events
                                            }
                                            _ => {}
                                        }
                                    } else {
                                        // Not valid JSON — append raw (fallback)
                                        if !task.output.is_empty() {
                                            task.output.push('\n');
                                        }
                                        task.output.push_str(line);
                                        task.progress_lines += 1;
                                        if task.status == TaskStatus::Stalled {
                                            task.status = TaskStatus::Running;
                                        }
                                        task.last_output_chunk_at = Some(Utc::now());
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // Flush remaining partial data
            if !partial.is_empty() {
                let mut store = tasks_c.write().await;
                if let Some(task) = store.get_mut(&tid_c) {
                    // Try JSON parse â€” extract text from assistant content array
                    if let Ok(ev) = serde_json::from_str::<Value>(&partial) {
                        if ev.get("type").and_then(|t| t.as_str()) == Some("assistant") {
                            if let Some(contents) =
                                ev.pointer("/message/content").and_then(|v| v.as_array())
                            {
                                for item in contents {
                                    if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                                        if !text.is_empty() {
                                            if !task.output.is_empty() {
                                                task.output.push('\n');
                                            }
                                            task.output.push_str(text);
                                            task.progress_lines += 1;
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        if !task.output.is_empty() {
                            task.output.push('\n');
                        }
                        task.output.push_str(&partial);
                        task.progress_lines += 1;
                    }
                }
            }
        }))
    } else {
        None
    };

    // Spawn stderr reader â€” same byte-level splitting with [STDERR] prefix
    let stderr_handle = if let Some(mut stderr) = stderr.take() {
        let tasks_c = tasks.clone();
        let tid_c = task_id.clone();
        Some(tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            let mut partial = String::new();
            let mut cr_seen = false;
            let mut stderr_buf = String::new();
            loop {
                match stderr.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let chunk = String::from_utf8_lossy(&buf[..n]);
                        let mut lines: Vec<String> = Vec::new();
                        for c in chunk.chars() {
                            if c == '\n' {
                                if cr_seen {
                                    cr_seen = false;
                                    continue;
                                }
                                lines.push(std::mem::take(&mut partial));
                            } else if c == '\r' {
                                cr_seen = true;
                                let line = std::mem::take(&mut partial);
                                if !line.is_empty() {
                                    lines.push(line);
                                }
                            } else {
                                cr_seen = false;
                                partial.push(c);
                            }
                        }
                        if !lines.is_empty() {
                            let mut store = tasks_c.write().await;
                            if let Some(task) = store.get_mut(&tid_c) {
                                if task.status == TaskStatus::Cancelled {
                                    return stderr_buf;
                                }
                                for line in &lines {
                                    if !stderr_buf.is_empty() {
                                        stderr_buf.push('\n');
                                    }
                                    stderr_buf.push_str(line);
                                    if !task.output.is_empty() {
                                        task.output.push('\n');
                                    }
                                    task.output.push_str("[STDERR] ");
                                    task.output.push_str(line);
                                    task.progress_lines += 1;
                                }
                            }
                        }
                    }
                }
            }
            // Flush remaining partial data
            if !partial.is_empty() {
                if !stderr_buf.is_empty() {
                    stderr_buf.push('\n');
                }
                stderr_buf.push_str(&partial);
                let mut store = tasks_c.write().await;
                if let Some(task) = store.get_mut(&tid_c) {
                    if !task.output.is_empty() {
                        task.output.push('\n');
                    }
                    task.output.push_str("[STDERR] ");
                    task.output.push_str(&partial);
                    task.progress_lines += 1;
                }
            }
            stderr_buf
        }))
    } else {
        None
    };

    // Wait for child to exit first (untimed — preserves full happy-path output drain).
    let exit_status = child.wait().await;

    // After child exits, give readers 5s to finish draining remaining pipe data.
    // On Windows, pipe handles can outlive the child process, causing readers to hang.
    let reader_timeout = std::time::Duration::from_secs(5);
    let stderr_output = {
        let stdout_drain = async {
            if let Some(h) = stdout_handle {
                if tokio::time::timeout(reader_timeout, h).await.is_err() {
                    warn!(
                        "stdout reader timed out 5s after child exit (task {})",
                        task_id
                    );
                }
            }
        };
        let stderr_drain = async {
            if let Some(h) = stderr_handle {
                match tokio::time::timeout(reader_timeout, h).await {
                    Ok(Ok(s)) => s,
                    Ok(Err(_)) => String::new(),
                    Err(_) => {
                        warn!(
                            "stderr reader timed out 5s after child exit (task {})",
                            task_id
                        );
                        String::new()
                    }
                }
            } else {
                String::new()
            }
        };
        let (_, stderr_out) = tokio::join!(stdout_drain, stderr_drain);
        stderr_out
    };

    match &exit_status {
        Ok(status) => append_final_log_marker(
            &log_path,
            &task_id,
            status.success(),
            status.code(),
            if status.success() {
                None
            } else {
                Some("process-exit-nonzero")
            },
        ),
        Err(e) => append_final_log_marker(
            &log_path,
            &task_id,
            false,
            None,
            Some(&format!("Process error: {}", e)),
        ),
    }

    // Update final status
    let mut store = tasks.write().await;
    let mut retry_task: Option<Task> = None;
    let mut completed_snap: Option<Task> = None;
    if let Some(task) = store.get_mut(&task_id) {
        if task.status == TaskStatus::Cancelled {
            Server::persist_task(task);
            return;
        }

        // Item 7: Detect context limit before setting final status
        let ctx_limited = task.output.contains("context window")
            || task.output.contains("token limit")
            || task.output.contains("maximum context length")
            || task.output.contains("conversation is too long");

        match exit_status {
            Ok(status) if status.success() => {
                task.status = TaskStatus::Done;
            }
            Ok(status) => {
                task.status = TaskStatus::Failed;
                let stderr_msg = if stderr_output.len() > 500 {
                    format!("...{}", &stderr_output[stderr_output.len() - 500..])
                } else {
                    stderr_output
                };
                if ctx_limited {
                    let ctx_file = format!("{}\\context_resume_{}.json", history_dir(), task.id);
                    let resume = json!({
                        "task_id": task.id, "prompt": task.prompt,
                        "backend": task.backend.to_string(),
                        "steps_completed": task.steps.len(),
                        "last_steps": task.steps.iter().rev().take(10).rev()
                            .map(|s| json!({"tool": s.tool, "status": s.status}))
                            .collect::<Vec<Value>>(),
                        "output_tail": if task.output.len() > 1000 { &task.output[task.output.len()-1000..] } else { &task.output },
                        "saved_at": Utc::now().to_rfc3339(),
                    });
                    let _ = std::fs::write(
                        &ctx_file,
                        serde_json::to_string_pretty(&resume).unwrap_or_default(),
                    );
                    task.error = Some(format!(
                        "Context limit after {} steps. Resume saved: {}",
                        task.steps.len(),
                        ctx_file
                    ));
                } else {
                    let stderr_msg = if stderr_msg.is_empty() {
                        let tail = log_tail_text(&log_path, 500);
                        if tail.is_empty() {
                            format!("See child log: {}", log_path)
                        } else {
                            format!("{}\nChild log: {}", tail, log_path)
                        }
                    } else {
                        stderr_msg
                    };
                    task.error = Some(format!(
                        "Exit code {}. Stderr: {}",
                        status.code().unwrap_or(-1),
                        stderr_msg
                    ));
                }
            }
            Err(e) => {
                task.status = TaskStatus::Failed;
                task.error = Some(format!("Process error: {}", e));
            }
        }
        task.completed_at = Some(Utc::now());
        if task.status == TaskStatus::Done {
            Server::validate_output(task);
        }
        Server::flag_extraction(task);
        // Item 18: retry/learn hooks
        if task.status == TaskStatus::Failed {
            retry_task = Server::prepare_retry(task);
        }
        if task.status == TaskStatus::Done && task.retry_of.is_some() {
            Server::learn_from_outcome(task);
        }
        Server::persist_task(task);
        Server::save_to_history(task);
        save_task_artifact(task);
        remove_task_state(&task.id);
        // v1.3.1: task notify
        check_and_fire_task_notify(task, &RealNotifier);
        // v1.3.4: auto-advance loaf phase if task was linked to current phase
        auto_advance_loaf_on_task_complete(task);
        completed_snap = Some(task.clone());
    }
    if let Some(ref rt) = retry_task {
        store.insert(rt.id.clone(), rt.clone());
        Server::persist_task(rt);
    }
    drop(store);

    // Clean up stdin pipe
    if let Some(ref pipes) = stdin_pipes {
        let mut pipe_store = pipes.write().await;
        pipe_store.remove(&task_id);
    }

    if let Some(ref rt) = retry_task {
        spawn_retry_execution(rt, tasks.clone(), None, &tokio::runtime::Handle::current());
    }
    if let Some(ref ct) = completed_snap {
        spawn_on_complete(ct, tasks.clone(), None, &tokio::runtime::Handle::current());
    }
}

// ============================================================================
// Tool Handlers
// ============================================================================

/// Safe UTF-8 string truncation — never panics on multi-byte characters
fn safe_truncate(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

fn ensure_safety_validation(prompt: &str) -> String {
    // Safety validation removed 2026-04-16: redundant with ARCHIVE-FIRST
    prompt.to_string()
}

fn extract_safety_warning(output: &str) -> Option<String> {
    output
        .lines()
        .find(|line| line.contains("[SAFETY CHECK: REVIEW NEEDED"))
        .map(|line| line.trim().to_string())
}

fn handle_submit_task(server: &Server, params: Value) -> Result<Value, String> {
    let auto_route = params
        .get("auto_route")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let prompt_for_route = params.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
    let wd_for_route = params.get("working_dir").and_then(|v| v.as_str());

    let backend_str_opt = params.get("backend").and_then(|v| v.as_str());

    // Item 16: If no backend specified and auto_route is true, use recommend_backend
    let (backend, routed) = if backend_str_opt.is_none() && auto_route {
        let rec = Server::recommend_backend(prompt_for_route, wd_for_route);
        let be = match rec.recommended_backend.as_str() {
            "gpt" => Backend::Gpt,
            "gemini" => Backend::Gemini,
            "claude_code" | "claude" => Backend::ClaudeCode,
            "codex" => Backend::Codex,
            _ => Backend::ClaudeCode,
        };
        (be, Some(rec))
    } else {
        let backend_str = backend_str_opt
            .ok_or("Missing 'backend' parameter (gpt, gemini, claude_code, codex). Or set auto_route: true.")?;
        let be = match backend_str {
            "gpt" => Backend::Gpt,
            "gemini" => Backend::Gemini,
            "claude_code" | "claude" => Backend::ClaudeCode,
            "codex" => Backend::Codex,
            _ => {
                return Err(format!(
                    "Unknown backend '{}'. Use: gpt, gemini, claude_code, codex",
                    backend_str
                ))
            }
        };
        (be, None)
    };

    let raw_prompt = params
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'prompt' parameter")?
        .to_string();

    // v1.2.3: Fingerprint dedup check
    let allow_duplicate = params
        .get("allow_duplicate")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let working_dir_for_fp = params.get("working_dir").and_then(|v| v.as_str());
    let fp = compute_task_fingerprint(&backend, &raw_prompt, working_dir_for_fp);

    // v1.3.1: Task-level notify flags
    let notify_on_complete = params.get("notify_on_complete").and_then(|v| v.as_bool());
    let notify_on_fail = params.get("notify_on_fail").and_then(|v| v.as_bool());
    let notify_on_destroy = params.get("notify_on_destroy").and_then(|v| v.as_bool());
    let notify_title = params
        .get("notify_title")
        .and_then(|v| v.as_str())
        .map(String::from);
    let notify_body = params
        .get("notify_body")
        .and_then(|v| v.as_str())
        .map(String::from);

    if !allow_duplicate {
        let store = server.runtime.block_on(server.tasks.read());
        let match_task = store.values().find(|t| {
            t.fingerprint.as_deref() == Some(fp.as_str())
                && matches!(t.status, TaskStatus::Running | TaskStatus::Queued)
                && t.superseded_by.is_none()
        });
        if let Some(existing) = match_task {
            let last_act = existing.last_activity.unwrap_or(existing.created_at);
            let stale_secs = (Utc::now() - last_act).num_seconds();
            if stale_secs <= 120 {
                // Active duplicate — reject
                return Ok(json!({
                    "status": "duplicate",
                    "existing_task_id": existing.id,
                    "message": format!("Duplicate of active task {} (last activity {}s ago). Use allow_duplicate: true to override.", existing.id, stale_secs),
                }));
            }
            // Stalled duplicate — allow, mark old as superseded (need write lock)
            let old_id = existing.id.clone();
            drop(store);
            let mut wstore = server.runtime.block_on(server.tasks.write());
            if let Some(old_task) = wstore.get_mut(&old_id) {
                // Will be set to superseded_by after new task is created below
                old_task.watchdog_observations.push(format!(
                    "[{}] Stalled {}s — superseded by new submission",
                    Utc::now().format("%H:%M:%S"),
                    stale_secs
                ));
                Server::persist_task(old_task);
            }
            drop(wstore);
            // Continue to create new task; we'll set superseded_by after task_id is generated
        } else {
            drop(store);
        }
    }

    // CPC behavioral injection — prepend delegation rules to every task
    // Include active loaf context if one exists
    let loaf_context = find_active_loaf()
        .map(|(id, loaf)| {
            let goal = loaf.get("goal").and_then(|g| g.as_str()).unwrap_or("?");
            let phase_idx = loaf
                .get("current_phase")
                .and_then(|p| p.as_u64())
                .unwrap_or(0) as usize;
            let phase_name = loaf
                .get("phases")
                .and_then(|p| p.as_array())
                .and_then(|p| p.get(phase_idx))
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("main");
            format!(
                "You are working on: {}. Loaf: {}. Phase: {}. \
             Report: what you changed, what you decided, what you discovered.\n",
                goal, id, phase_name
            )
        })
        .unwrap_or_default();

    // v1.3.1: backend-aware progress directive — Claude Code has TodoWrite native,
    // other backends need explicit "track your progress" language.
    // Breadcrumb calls preserved for ClaudeCode to keep CPC cross-session coordination.
    let progress_directive = match backend {
        Backend::ClaudeCode => {
            "- Track progress via TodoWrite (native). \
             Call autonomous:breadcrumb_start/_step/_complete for CPC cross-session coordination.\n"
        }
        _ => {
            "- Track your progress: note what you're doing at each major step.\n\
             - On failure, document what failed and why before exiting.\n"
        }
    };

    let prompt = ensure_safety_validation(&format!(
        "[CPC DELEGATION CONTEXT]\n\
         {}\
         {}\
         - When done, summarize: decisions made, files changed, patterns discovered.\n\
         - If you discover something reusable (a fix, a pattern, a decision), call it out clearly.\n\n\
         [TASK]\n{}", loaf_context, progress_directive, raw_prompt
    ));

    // §12: Specialist role handling — custom YAML roles override built-in
    let role = params
        .get("role")
        .and_then(|v| v.as_str())
        .map(String::from);
    let role_prompt_owned: Option<String> = role
        .as_deref()
        .and_then(|r| get_custom_role_prompt(r).or_else(|| get_role_prompt(r).map(String::from)));
    let role_prompt = role_prompt_owned.as_deref();

    let user_system_prompt = params
        .get("system_prompt")
        .and_then(|v| v.as_str())
        .map(String::from);

    let system_prompt = match (role_prompt, &user_system_prompt) {
        (Some(rp), Some(sp)) => Some(format!("{}\n\n{}", rp, sp)),
        (Some(rp), None) => Some(rp.to_string()),
        (None, Some(sp)) => Some(sp.clone()),
        (None, None) => None,
    };

    // §13: Auto-artifact saving (default true)
    let save_artifact = params
        .get("save_artifact")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let effort = params
        .get("effort")
        .and_then(|v| v.as_str())
        .map(String::from);

    let model = params
        .get("model")
        .and_then(|v| v.as_str())
        .map(String::from);

    let working_dir = params
        .get("working_dir")
        .and_then(|v| v.as_str())
        .map(String::from);

    // Per-task visibility override, falls back to dashboard prefs
    let visible = params
        .get("visible")
        .and_then(|v| v.as_bool())
        .unwrap_or(false); // MCP tasks default to background

    let task_id = Uuid::new_v4().to_string()[..8].to_string();

    let task = Task {
        id: task_id.clone(),
        backend: backend.clone(),
        prompt: prompt.clone(),
        system_prompt: system_prompt.clone(),
        model: model.clone(),
        working_dir: working_dir.clone(),
        status: TaskStatus::Queued,
        output: String::new(),
        error: None,
        created_at: Utc::now(),
        started_at: None,
        completed_at: None,
        progress_lines: 0,
        steps: Vec::new(),
        last_activity: None,
        last_output_chunk_at: None,
        stall_detected: false,
        extraction_status: ExtractionStatus::None,
        trust_score: 0,
        trust_level: TrustLevel::Low,
        rollback_path: None,
        validation_status: ValidationStatus::NotChecked,
        assertions: Vec::new(),
        backed_up_files: Vec::new(),
        retry_count: 0,
        max_retries: 2,
        retry_of: None,
        error_context: None,
        input_tokens: 0,
        output_tokens: 0,
        cost_usd: 0.0,
        on_complete: params
            .get("on_complete")
            .and_then(|v| v.as_str())
            .map(String::from),
        role: role.clone(),
        save_artifact,
        rerun_of: None,
        parent_task_id: None,
        forked_from: None,
        continuation_of: None,
        child_pid: None,
        log_path: None,
        log_offset: 0,
        reconnected: false,
        watchdog_observations: Vec::new(),
        fingerprint: Some(fp.clone()),
        superseded_by: None,
        label: params
            .get("label")
            .and_then(|v| v.as_str())
            .map(String::from),
        current_step: None,
        total_steps: None,
        current_step_desc: None,
        live_activity: None,
        effort: effort.clone(),
        notify_on_complete,
        notify_on_fail,
        notify_on_destroy,
        notify_title,
        notify_body,
    };

    // Store task
    let tasks = server.tasks.clone();
    let config = server.config.clone();

    server.runtime.block_on(async {
        let mut store = tasks.write().await;
        // v1.2.3: Link stalled duplicate if one was detected
        if !allow_duplicate {
            let stalled_id: Option<String> = store
                .values()
                .find(|t| {
                    t.fingerprint.as_deref() == Some(fp.as_str())
                        && t.id != task_id
                        && matches!(t.status, TaskStatus::Running | TaskStatus::Queued)
                        && t.superseded_by.is_none()
                })
                .map(|t| t.id.clone());
            if let Some(old_id) = stalled_id {
                if let Some(old_task) = store.get_mut(&old_id) {
                    old_task.superseded_by = Some(task_id.clone());
                    Server::persist_task(old_task);
                }
            }
        }
        store.insert(task_id.clone(), task.clone());
    });
    Server::persist_task(&task);
    // v1.4.4: Write lightweight state.json for reconnect discovery
    write_task_state(&task);

    // Spawn background execution
    let tasks_bg = server.tasks.clone();
    let tid = task_id.clone();
    let pipes = Some(server.stdin_pipes.clone());

    // If visible, we only spawn the terminal (no background headless process)
    let run_background = !visible;

    // Track exe+args for optional visible terminal
    let mut vis_exe: Option<String> = None;
    let mut vis_args: Vec<String> = Vec::new();

    match backend {
        Backend::Gpt => {
            // GPT is API-only, no CLI to mirror
            server.runtime.spawn(run_gpt_task(config, tasks_bg, tid));
        }
        Backend::Gemini => {
            // Item 4: Inject breadcrumb context for Gemini continuity
            let gemini_prompt = if let Some(bc) = Server::read_breadcrumb_state() {
                format!("{}\n\n{}", bc, prompt)
            } else {
                prompt.clone()
            };
            let mut args = vec![
                gemini_cmd().to_string(),
                "-p".to_string(),
                gemini_prompt.clone(),
                "--yolo".to_string(),
            ];
            if let Some(m) = model {
                args.push("--model".to_string());
                args.push(m);
            }
            vis_exe = Some(node_cmd().to_string());
            vis_args = args.clone();
            if run_background {
                server.runtime.spawn(run_cli_task(
                    tasks_bg,
                    tid,
                    r"C:\Program Files\nodejs\node.exe",
                    args,
                    pipes.clone(),
                    StdinMode::Null,
                ));
            }
        }
        Backend::ClaudeCode => {
            // CRITICAL: prompt must come immediately after -p
            let mut args = vec![
                "-p".to_string(),
                prompt.clone(),
                "--dangerously-skip-permissions".to_string(),
                "--verbose".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--add-dir".to_string(),
                r"C:\temp".to_string(),
                "--add-dir".to_string(),
                volumes_dir(),
                "--add-dir".to_string(),
                r"C:\CPC".to_string(),
                "--add-dir".to_string(),
                workspace_root(),
            ];
            if let Some(m) = model {
                args.push("--model".to_string());
                args.push(m);
            }
            if let Some(ref wd) = working_dir {
                args.push("--add-dir".to_string());
                args.push(wd.clone());
            }
            vis_exe = Some(claude_code_cmd().to_string());
            vis_args = args.clone();
            if run_background {
                server.runtime.spawn(run_cli_task(
                    tasks_bg,
                    tid,
                    claude_code_cmd(),
                    args,
                    pipes.clone(),
                    StdinMode::Null,
                ));
            }
        }
        Backend::Codex => {
            let ws = workspace_root();
            let wd = working_dir.as_deref().unwrap_or(&ws);
            let args = vec![
                "exec".into(),
                "--json".into(),
                "--skip-git-repo-check".into(),
                "--full-auto".into(),
                "--cd".into(),
                wd.to_string(),
                "--".into(),
                prompt.clone(),
            ];
            vis_exe = Some(codex_cmd().to_string());
            vis_args = args.clone();
            if run_background {
                server
                    .runtime
                    .spawn(run_codex_task(tasks_bg, tid, args, wd.to_string()));
            }
        }
    }

    // When visible=true, SKIP background task (avoid double token cost)
    // Manager marks task as "visible" - no stream-json tracking
    if visible {
        if let Some(exe) = vis_exe {
            let title: String = prompt.chars().take(60).collect();
            let title = if prompt.len() > 60 {
                format!("{}...", title)
            } else {
                title
            };
            let ws = workspace_root();
            let wd = working_dir.as_deref().unwrap_or(&ws);
            spawn_visible_terminal(&title, &exe, &vis_args, wd);
        }
    }

    // If visible-only, mark task as done (terminal handles it, no tracking)
    if visible && !run_background {
        let tasks_done = server.tasks.clone();
        let tid_done = task_id.clone();
        server.runtime.block_on(async {
            let mut store = tasks_done.write().await;
            if let Some(t) = store.get_mut(&tid_done) {
                t.status = TaskStatus::Running;
                t.started_at = Some(Utc::now());
                t.output =
                    "Running in visible terminal - check terminal window for output".to_string();
            }
        });
    }

    let mut result = json!({
        "task_id": task_id,
        "backend": backend.to_string(),
        "status": if visible { "visible" } else { "queued" },
        "visible": visible,
        "message": if visible { "Task opened in visible terminal. Watch the terminal window.".to_string() } else { format!("Task submitted to {}. Poll with get_status.", backend) }
    });

    // Item 16: Include routing info when auto_route was used
    if let Some(rec) = routed {
        result["auto_routed"] = json!(true);
        result["routing"] = json!({
            "confidence": rec.confidence,
            "reasoning": rec.reasoning,
            "alternatives": rec.alternatives,
        });
    }

    // v1.2.3: wait=true blocking removed. task_submit always returns immediately.
    // timeout_secs kept as estimated_secs for informational purposes only.
    if let Some(est) = params
        .get("timeout_secs")
        .or(params.get("estimated_secs"))
        .and_then(|v| v.as_u64())
    {
        result["estimated_secs"] = json!(est);
    }

    Ok(result)
}

/// Watch multiple tasks until all complete. Polls internally (zero LLM turns).
/// Optionally sends MCP notifications for progress updates.
fn handle_watch_tasks(server: &Server, params: Value) -> Result<Value, String> {
    let task_ids: Vec<String> = params
        .get("task_ids")
        .and_then(|v| v.as_array())
        .ok_or("Missing 'task_ids' array")?
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();

    if task_ids.is_empty() {
        return Err("task_ids array is empty".into());
    }

    let timeout_secs = params
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(600);

    let progress = params
        .get("progress")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let progress_interval_secs = params
        .get("progress_interval_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(10);

    // Validate all task IDs exist
    {
        let store = server.runtime.block_on(server.tasks.read());
        for tid in &task_ids {
            if !store.contains_key(tid) {
                return Err(format!("Task '{}' not found", tid));
            }
        }
    }

    let start = std::time::Instant::now();
    let mut last_progress =
        std::time::Instant::now() - std::time::Duration::from_secs(progress_interval_secs + 1);
    let mut last_steps: HashMap<String, usize> = HashMap::new();

    loop {
        std::thread::sleep(std::time::Duration::from_millis(500));

        let store = server.runtime.block_on(server.tasks.read());

        // Check if all tasks are in terminal state
        let mut all_done = true;
        let mut results: Vec<Value> = Vec::new();

        for tid in &task_ids {
            if let Some(task) = store.get(tid) {
                let is_terminal = matches!(
                    task.status,
                    TaskStatus::Done | TaskStatus::Failed | TaskStatus::Cancelled
                );
                if !is_terminal {
                    all_done = false;
                }
                results.push(json!({
                    "task_id": task.id,
                    "backend": task.backend.to_string(),
                    "status": task.status.to_string(),
                    "elapsed": task.started_at.map(|s| {
                        let end = task.completed_at.unwrap_or_else(Utc::now);
                        format!("{}s", (end - s).num_seconds())
                    }),
                    "step_count": task.steps.len(),
                    "error": task.error.clone(),
                    "output_preview": if is_terminal { Server::generate_end_report(task) } else { String::new() },
                    "input_tokens": task.input_tokens,
                    "output_tokens": task.output_tokens,
                    "cost_usd": task.cost_usd,
                }));
            }
        }

        // Send progress notification if enabled and interval elapsed
        if progress && !all_done && last_progress.elapsed().as_secs() >= progress_interval_secs {
            let mut updates: Vec<String> = Vec::new();
            for tid in &task_ids {
                if let Some(task) = store.get(tid) {
                    let prev_steps = last_steps.get(tid).copied().unwrap_or(0);
                    let cur_steps = task.steps.len();
                    let status_str = match task.status {
                        TaskStatus::Running => {
                            if cur_steps > prev_steps {
                                format!("{} ({}): step {}", tid, task.backend, cur_steps)
                            } else {
                                format!(
                                    "{} ({}): running ({}s)",
                                    tid,
                                    task.backend,
                                    task.started_at
                                        .map(|s| (Utc::now() - s).num_seconds())
                                        .unwrap_or(0)
                                )
                            }
                        }
                        TaskStatus::Done => format!("{} ({}): done", tid, task.backend),
                        TaskStatus::Failed => format!("{} ({}): FAILED", tid, task.backend),
                        _ => format!("{} ({}): {}", tid, task.backend, task.status),
                    };
                    updates.push(status_str);
                    last_steps.insert(tid.clone(), cur_steps);
                }
            }
            drop(store); // release lock before writing to stdout
            server.send_notification("info", &format!("[watch] {}", updates.join(" | ")));
            last_progress = std::time::Instant::now();
            continue; // re-check after notification
        }

        drop(store);

        if all_done {
            return Ok(json!({
                "status": "all_complete",
                "elapsed": format!("{}s", start.elapsed().as_secs()),
                "tasks": results
            }));
        }

        if start.elapsed().as_secs() > timeout_secs {
            return Ok(json!({
                "status": "timeout",
                "elapsed": format!("{}s", start.elapsed().as_secs()),
                "message": format!("Timed out after {}s. Some tasks still running.", timeout_secs),
                "tasks": results
            }));
        }
    }
}

fn handle_get_status(server: &Server, params: Value) -> Result<Value, String> {
    let task_id = params
        .get("task_id")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'task_id' parameter")?;

    let store = server.runtime.block_on(server.tasks.read());
    let task = store
        .get(task_id)
        .ok_or(format!("Task '{}' not found", task_id))?;

    let elapsed = task.started_at.map(|s| {
        let end = task.completed_at.unwrap_or_else(Utc::now);
        let dur = end - s;
        format!("{}s", dur.num_seconds())
    });
    // Item 5: Compute stall status inline.
    // A task is "stalled" only if it's Running, no tool is mid-flight, AND
    // >90s have passed since last activity. active_tool_running takes precedence
    // because long Write/Edit tools can legitimately run 60-90s silently.
    let tool_running = task
        .steps
        .last()
        .map(|s| s.status == "started")
        .unwrap_or(false);
    let stalled = if task.status == TaskStatus::Running && !tool_running {
        task.last_activity
            .map(|la| Utc::now().signed_duration_since(la).num_seconds() > 90)
            .unwrap_or(false)
    } else {
        false
    };

    // Human-friendly health enum — additive to stall_detected, more expressive.
    // Values: "done", "failed", "queued", "cancelled", "running_long_tool",
    // "stalled", "idle", "running".
    let health = match task.status {
        TaskStatus::Done => "done",
        TaskStatus::Failed => "failed",
        TaskStatus::Queued => "queued",
        TaskStatus::Cancelled => "cancelled",
        TaskStatus::Paused => "paused",
        TaskStatus::Orphaned => "orphaned",
        TaskStatus::Stalled => "stalled",
        TaskStatus::Running => {
            if tool_running {
                "running_long_tool"
            } else if stalled {
                "stalled"
            } else if task
                .last_activity
                .map(|la| Utc::now().signed_duration_since(la).num_seconds() > 30)
                .unwrap_or(false)
            {
                "idle"
            } else {
                "running"
            }
        }
    };

    // Item 2: Recent steps summary
    let recent_steps: Vec<Value> = task
        .steps
        .iter()
        .rev()
        .take(5)
        .rev()
        .map(|s| json!({"tool": s.tool, "status": s.status, "ts": s.timestamp.to_rfc3339()}))
        .collect();

    // Item 3: Smart report for terminal states
    let output_preview = match task.status {
        TaskStatus::Done | TaskStatus::Failed => Server::generate_end_report(task),
        _ => {
            if task.output.len() > 300 {
                format!(
                    "{}...\n\n[{} total chars, use get_output for full]",
                    safe_truncate(&task.output, 300),
                    task.output.len()
                )
            } else {
                task.output.clone()
            }
        }
    };
    let warning = extract_safety_warning(&task.output);

    Ok(json!({
        "task_id": task.id,
        "backend": task.backend,
        "status": task.status.to_string(),
        "progress_lines": task.progress_lines,
        "step_count": task.steps.len(),
        "recent_steps": recent_steps,
        "stall_detected": stalled,
        "active_tool_running": Server::active_tool_running(task),
        "health": health,
            "input_tokens": task.input_tokens,
            "output_tokens": task.output_tokens,
            "cost_usd": task.cost_usd,
        "elapsed": elapsed,
        "created_at": task.created_at.to_rfc3339(),
        "started_at": task.started_at.map(|t| t.to_rfc3339()),
        "completed_at": task.completed_at.map(|t| t.to_rfc3339()),
        "error": task.error,
        "output_preview": output_preview,
        "warning": warning,
        "watchdog_observations": task.watchdog_observations,
        "child_pid": task.child_pid,
        "log_path": task.log_path,
        "log_offset": task.log_offset,
        "reconnected": task.reconnected
    }))
}

fn handle_get_output(server: &Server, params: Value) -> Result<Value, String> {
    let task_id = params
        .get("task_id")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'task_id' parameter")?;

    let tail = params
        .get("tail")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);

    let store = server.runtime.block_on(server.tasks.read());
    let task = store
        .get(task_id)
        .ok_or(format!("Task '{}' not found", task_id))?;

    let output = if let Some(n) = tail {
        let lines: Vec<&str> = task.output.lines().collect();
        if lines.len() > n {
            lines[lines.len() - n..].join("\n")
        } else {
            task.output.clone()
        }
    } else {
        task.output.clone()
    };
    let warning = extract_safety_warning(&output);

    Ok(json!({
        "task_id": task.id,
        "status": task.status.to_string(),
        "total_lines": task.output.lines().count(),
        "output": output,
        "error": task.error,
        "warning": warning
    }))
}

fn handle_list_tasks(server: &Server, params: Value) -> Result<Value, String> {
    let status_filter = params.get("status").and_then(|v| v.as_str());

    let backend_filter = params.get("backend").and_then(|v| v.as_str());

    let include_stalled = params
        .get("include_stalled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;

    let store = server.runtime.block_on(server.tasks.read());

    let mut tasks: Vec<&Task> = store
        .values()
        .filter(|t| {
            if let Some(sf) = status_filter {
                if t.status.to_string() != sf {
                    return false;
                }
            }
            if let Some(bf) = backend_filter {
                if t.backend.to_string() != bf {
                    return false;
                }
            }
            // v1.2.3: include_stalled filter — when true, only show stalled tasks
            if include_stalled {
                let is_stalled = matches!(t.status, TaskStatus::Running | TaskStatus::Queued)
                    && t.last_activity
                        .is_none_or(|la| (Utc::now() - la).num_seconds() > 120)
                    && t.superseded_by.is_none();
                if !is_stalled {
                    return false;
                }
            }
            true
        })
        .collect();

    tasks.sort_by_key(|t| std::cmp::Reverse(t.created_at));
    tasks.truncate(limit);

    let summary: Vec<Value> = tasks
        .iter()
        .map(|t| {
            json!({
                "task_id": t.id,
                "backend": t.backend,
                "status": t.status.to_string(),
                "prompt_preview": if t.prompt.len() > 80 {
                    safe_truncate(&t.prompt, 80)
                } else {
                    t.prompt.clone()
                },
                "created_at": t.created_at.to_rfc3339(),
                "elapsed": t.started_at.map(|s| {
                    let end = t.completed_at.unwrap_or_else(Utc::now);
                    format!("{}s", (end - s).num_seconds())
                }),
            })
        })
        .collect();

    Ok(json!({
        "total": store.len(),
        "showing": summary.len(),
        "tasks": summary
    }))
}

fn handle_cancel_task(server: &Server, params: Value) -> Result<Value, String> {
    let task_id = params
        .get("task_id")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'task_id' parameter")?;

    let mut store = server.runtime.block_on(server.tasks.write());
    let task = store
        .get_mut(task_id)
        .ok_or(format!("Task '{}' not found", task_id))?;

    if task.status != TaskStatus::Running && task.status != TaskStatus::Queued {
        return Err(format!(
            "Task '{}' is already {} - cannot cancel",
            task_id, task.status
        ));
    }

    // v1.2.3: Kill the child process tree before marking cancelled
    let killed_tree = if let Some(root_pid) = task.child_pid {
        kill_process_tree(root_pid)
    } else {
        vec![]
    };

    task.status = TaskStatus::Cancelled;
    task.completed_at = Some(Utc::now());
    task.error = Some("Cancelled by user".into());
    if !killed_tree.is_empty() {
        task.watchdog_observations.push(format!(
            "[{}] Cancel killed process tree: {:?}",
            Utc::now().format("%H:%M:%S"),
            killed_tree
        ));
    }
    Server::flag_extraction(task);
    // Item 18: no retry for cancelled tasks
    Server::persist_task(task);
    Server::save_to_history(task);
    // v1.3.1: notify_on_destroy for task cancel
    if task.notify_on_destroy.unwrap_or(false) {
        fire_notify_for_task(
            &task.id,
            &task.created_at,
            task.notify_title.as_deref(),
            task.notify_body.as_deref(),
            NotifyReason::Destroyed,
            server.notifier.as_ref(),
        );
    }

    Ok(json!({
        "task_id": task_id,
        "status": "cancelled",
        "killed_tree": killed_tree,
        "message": if killed_tree.is_empty() {
            "Task cancelled (no child process to kill).".to_string()
        } else {
            format!("Task cancelled. Killed {} processes.", killed_tree.len())
        }
    }))
}

/// Walk the process tree rooted at `root_pid`, kill descendants bottom-up, then kill root.
/// Returns list of PIDs that were successfully terminated.
fn kill_process_tree(root_pid: u32) -> Vec<u32> {
    use sysinfo::{Pid, ProcessesToUpdate, System};

    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);

    // Build parent→children map
    let mut children_map: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, proc) in sys.processes() {
        if let Some(parent) = proc.parent() {
            children_map
                .entry(parent.as_u32())
                .or_default()
                .push(pid.as_u32());
        }
    }

    // BFS to collect all descendants
    let mut to_kill = Vec::new();
    let mut queue = vec![root_pid];
    while let Some(pid) = queue.pop() {
        to_kill.push(pid);
        if let Some(kids) = children_map.get(&pid) {
            queue.extend(kids.iter());
        }
    }

    // Kill in reverse order (descendants first, root last)
    to_kill.reverse();
    let mut killed = Vec::new();
    for pid in &to_kill {
        if let Some(proc) = sys.process(Pid::from_u32(*pid)) {
            if proc.kill() {
                killed.push(*pid);
            }
        }
    }
    killed
}

/// v1.2.3: task_poll — returns tasks completed since a timestamp, plus still-running tasks and status_bar.
fn handle_task_poll(server: &Server, params: Value) -> Result<Value, String> {
    let since_str = params.get("since").and_then(|v| v.as_str());
    let since: DateTime<Utc> = if let Some(s) = since_str {
        DateTime::parse_from_rfc3339(s)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now() - chrono::Duration::hours(1))
    } else {
        // Default: 1 hour ago
        Utc::now() - chrono::Duration::hours(1)
    };

    let store = server.runtime.block_on(server.tasks.read());

    let completed_since: Vec<Value> = store
        .values()
        .filter(|t| {
            matches!(
                t.status,
                TaskStatus::Done | TaskStatus::Failed | TaskStatus::Cancelled
            ) && t.completed_at.is_some_and(|c| c > since)
        })
        .map(|t| {
            json!({
                "task_id": t.id,
                "backend": t.backend.to_string(),
                "status": t.status.to_string(),
                "prompt_preview": safe_truncate(&t.prompt, 80),
                "completed_at": t.completed_at.map(|c| c.to_rfc3339()),
                "error": t.error,
            })
        })
        .collect();

    let still_running: Vec<Value> = store
        .values()
        .filter(|t| matches!(t.status, TaskStatus::Running | TaskStatus::Queued))
        .map(|t| {
            json!({
                "task_id": t.id,
                "backend": t.backend.to_string(),
                "status": t.status.to_string(),
                "prompt_preview": safe_truncate(&t.prompt, 80),
                "elapsed": t.started_at.map(|s| format!("{}s", (Utc::now() - s).num_seconds())),
                "child_pid": t.child_pid,
            })
        })
        .collect();

    let status_bar = build_status_bar(&store);

    Ok(json!({
        "completed_since": completed_since,
        "still_running": still_running,
        "status_bar": status_bar,
        "polled_at": Utc::now().to_rfc3339(),
    }))
}

/// Build a status_bar summary from task state + external state files.
fn build_status_bar(store: &HashMap<String, Task>) -> Value {
    let running = store
        .values()
        .filter(|t| t.status == TaskStatus::Running)
        .count();
    let queued = store
        .values()
        .filter(|t| t.status == TaskStatus::Queued)
        .count();
    let unclaimed = 0usize; // reserved for future queue system

    let manager_line = format!(
        "{} running, {} queued, {} unclaimed",
        running, queued, unclaimed
    );

    // v1.2.7: Multi-breadcrumb support — read active.index.json for dashboard-aware multi-bc format.
    // Falls back through: CPC breadcrumb index → local active_operation.json → autonomous breadcrumb.jsonl
    let breadcrumb_line = read_breadcrumb_status_line();

    // Query local server state
    let loaf_line = read_active_loaf_summary();

    let formatted = format!(
        "mgr: {} | bc: {} | loaf: {}",
        manager_line, breadcrumb_line, loaf_line
    );

    json!({
        "manager": manager_line,
        "breadcrumb": breadcrumb_line,
        "loaf": loaf_line,
        "formatted": formatted,
    })
}

/// v1.2.7: Read breadcrumb status line with multi-bc support.
/// Priority: CPC active.index.json (multi-bc) → LOCALAPPDATA active_operation.json → autonomous JSONL.
/// - 0 active: "none"
/// - 1 active: "active:<short_name>" (same as before)
/// - 2+ active: "N active (project_a: 2, project_b: 1)"
fn read_breadcrumb_status_line() -> String {
    let cpc_state = std::env::var("CPC_STATE_DIR").unwrap_or_else(|_| r"C:\CPC\state".to_string());
    read_breadcrumb_status_line_from(&cpc_state)
}

/// Inner implementation — takes explicit cpc_state path so tests can inject a tempdir
/// without touching process-wide env vars (which are not safe to share between parallel tests).
fn read_breadcrumb_status_line_from(cpc_state: &str) -> String {
    // Tier 1: <cpc_state>\breadcrumbs\active.index.json — written by local/autonomous servers
    let index_path = format!(r"{}\breadcrumbs\active.index.json", cpc_state);
    if let Ok(content) = std::fs::read_to_string(&index_path) {
        if let Ok(index) = serde_json::from_str::<Value>(&content) {
            if let Some(obj) = index.as_object() {
                return if obj.is_empty() {
                    "none".to_string()
                } else if obj.len() == 1 {
                    let (_, bc) = obj.iter().next().unwrap();
                    let name = bc.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                    format!("active:{}", safe_truncate(name, 40))
                } else {
                    // Group by project_id for compact display
                    let mut by_project: HashMap<String, usize> = HashMap::new();
                    for (_, bc) in obj.iter() {
                        let proj = bc
                            .get("project_id")
                            .and_then(|p| p.as_str())
                            .unwrap_or("ungrouped")
                            .to_string();
                        *by_project.entry(proj).or_insert(0) += 1;
                    }
                    let mut breakdown: Vec<String> = by_project
                        .iter()
                        .map(|(k, v)| format!("{}: {}", k, v))
                        .collect();
                    breakdown.sort(); // stable output
                    format!("{} active ({})", obj.len(), breakdown.join(", "))
                };
            }
        }
    }

    // Tier 2: LOCALAPPDATA\CPC\state\active_operation.json (legacy single-bc format)
    let local_state_dir = {
        let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
        format!(r"{}\CPC\state", local)
    };
    if std::path::Path::new(&local_state_dir).exists() {
        let active_path = format!(r"{}\active_operation.json", local_state_dir);
        if let Some(v) = std::fs::read_to_string(&active_path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        {
            let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("?");
            let step = v.get("current_step").and_then(|s| s.as_u64()).unwrap_or(0);
            let total = v.get("total_steps").and_then(|s| s.as_u64()).unwrap_or(0);
            return format!("active:{} step {}/{}", name, step, total);
        }
        return "none".to_string();
    }

    // Tier 3: autonomous breadcrumb.jsonl (last-resort fallback)
    let autonomous_data = std::env::var("AUTONOMOUS_DATA_DIR").unwrap_or_else(|_| {
        let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
        format!(r"{}\autonomous", local)
    });
    read_state_file(&format!(r"{}\logs\breadcrumb.jsonl", autonomous_data))
        .unwrap_or_else(|| "unavailable".to_string())
}

/// Read last line of a state file to get latest status. Returns None if file unreadable.
fn read_state_file(path: &str) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let last_line = content.lines().last()?;
    // Try to extract a summary from JSONL
    if let Ok(v) = serde_json::from_str::<Value>(last_line) {
        let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("?");
        let step = v.get("current_step").and_then(|s| s.as_u64()).unwrap_or(0);
        let total = v.get("total_steps").and_then(|s| s.as_u64()).unwrap_or(0);
        Some(format!("{} ({}/{})", name, step, total))
    } else {
        Some(safe_truncate(last_line, 60))
    }
}

/// Read all active breadcrumbs with full detail for the dashboard tap panel.
/// Reads `active.index.json` for the list, then enriches each entry from its
/// project JSONL file (which has steps[], current_step, step_results, etc.).
/// Returns a JSON array sorted newest-first.
fn read_active_breadcrumbs_full() -> Vec<Value> {
    let cpc_state = std::env::var("CPC_STATE_DIR").unwrap_or_else(|_| r"C:\CPC\state".to_string());
    let index_path = format!(r"{}\breadcrumbs\active.index.json", cpc_state);
    let Ok(content) = std::fs::read_to_string(&index_path) else {
        return Vec::new();
    };
    let Ok(index) = serde_json::from_str::<Value>(&content) else {
        return Vec::new();
    };
    let Some(obj) = index.as_object() else {
        return Vec::new();
    };

    let projects_dir = format!(r"{}\breadcrumbs\projects", cpc_state);
    let mut results: Vec<Value> = Vec::new();

    for (_key, bc) in obj.iter() {
        let id = bc.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let project_id = bc
            .get("project_id")
            .and_then(|v| v.as_str())
            .unwrap_or("ungrouped");
        let name = bc.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let owner = bc
            .get("owner")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let started_at = bc
            .get("started_at")
            .or_else(|| bc.get("last_activity_at"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // Try to enrich from the project JSONL file
        let jsonl_path = format!(r"{}\{}.jsonl", projects_dir, project_id);
        let enriched = std::fs::read_to_string(&jsonl_path)
            .ok()
            .and_then(|content| {
                // Find the last line matching this breadcrumb ID (most recent update)
                content
                    .lines()
                    .rev()
                    .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                    .find(|entry| entry.get("id").and_then(|v| v.as_str()) == Some(id))
            });

        let mut entry = json!({
            "id": id,
            "project_id": project_id,
            "name": name,
            "owner": owner,
            "started_at": started_at,
            "source": "cpc-state",
        });

        if let Some(full) = enriched {
            // Copy enrichment fields from JSONL entry
            if let Some(steps) = full.get("steps") {
                entry["steps"] = steps.clone();
            }
            if let Some(cs) = full.get("current_step") {
                entry["current_step"] = cs.clone();
            }
            if let Some(ts) = full.get("total_steps") {
                entry["total_steps"] = ts.clone();
            }
            if let Some(sr) = full.get("step_results") {
                entry["step_results"] = sr.clone();
            }
            if let Some(fc) = full.get("files_changed") {
                entry["files_changed"] = fc.clone();
            }
            if let Some(ab) = full.get("aborted") {
                entry["aborted"] = ab.clone();
            }
            if let Some(la) = full.get("last_activity_at") {
                entry["last_activity_at"] = la.clone();
            }
        } else {
            // Minimal info from index only
            entry["current_step"] = json!(0);
            entry["total_steps"] = json!(0);
        }

        results.push(entry);
    }

    // Sort newest-first by started_at
    results.sort_by(|a, b| {
        let ta = a.get("started_at").and_then(|v| v.as_str()).unwrap_or("");
        let tb = b.get("started_at").and_then(|v| v.as_str()).unwrap_or("");
        tb.cmp(ta)
    });

    results
}

/// Read active loaf summary for status_bar
fn read_active_loaf_summary() -> String {
    match find_active_loaf() {
        Some((id, loaf)) => {
            let goal = loaf.get("goal").and_then(|g| g.as_str()).unwrap_or("?");
            format!("{}: {}", id, safe_truncate(goal, 40))
        }
        None => "none".to_string(),
    }
}

/// v1.2.3: Compute a dedup fingerprint from (backend, prompt[:200], working_dir).
fn compute_task_fingerprint(
    backend: &Backend,
    raw_prompt: &str,
    working_dir: Option<&str>,
) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    backend.to_string().hash(&mut h);
    let prompt_prefix: String = raw_prompt.chars().take(200).collect();
    prompt_prefix.hash(&mut h);
    working_dir.unwrap_or("").hash(&mut h);
    format!("{:016x}", h.finish())
}

/// v1.2.3: status_bar — one-liner summary of manager, breadcrumb, and loaf state.
fn handle_status_bar(server: &Server, _params: Value) -> Result<Value, String> {
    let store = server.runtime.block_on(server.tasks.read());
    Ok(build_status_bar(&store))
}

fn handle_pause_task(server: &Server, params: Value) -> Result<Value, String> {
    let task_id = params
        .get("task_id")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'task_id' parameter")?;

    let mut store = server.runtime.block_on(server.tasks.write());
    let task = store
        .get_mut(task_id)
        .ok_or(format!("Task '{}' not found", task_id))?;

    if task.status != TaskStatus::Running && task.status != TaskStatus::Queued {
        return Err(format!(
            "Task '{}' is {} - can only pause running or queued tasks",
            task_id, task.status
        ));
    }

    task.status = TaskStatus::Paused;
    Server::persist_task(task);

    Ok(json!({
        "task_id": task_id,
        "status": "paused",
        "message": "Task paused. Background process may still be running but status is marked paused. Use resume_task to re-queue."
    }))
}

fn handle_resume_task(server: &Server, params: Value) -> Result<Value, String> {
    let task_id = params
        .get("task_id")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'task_id' parameter")?;

    let tasks = server.tasks.clone();
    let config = server.config.clone();
    let mut store = server.runtime.block_on(tasks.write());
    let task = store
        .get_mut(task_id)
        .ok_or(format!("Task '{}' not found", task_id))?;

    if task.status != TaskStatus::Paused {
        return Err(format!(
            "Task '{}' is {} - can only resume paused tasks",
            task_id, task.status
        ));
    }

    task.status = TaskStatus::Queued;
    task.started_at = None;
    Server::persist_task(task);

    let task_snap = task.clone();
    drop(store);

    spawn_retry_execution(&task_snap, tasks.clone(), Some(config), &server.runtime);

    Ok(json!({
        "task_id": task_id,
        "status": "queued",
        "message": "Task resumed and re-queued for execution."
    }))
}

fn handle_configure(server: &Server, params: Value) -> Result<Value, String> {
    let mut config = server.runtime.block_on(server.config.write());
    let mut changes = Vec::new();

    if let Some(key) = params.get("openai_api_key").and_then(|v| v.as_str()) {
        config.openai_api_key = Some(key.to_string());
        changes.push("openai_api_key set");
    }

    if let Some(model) = params.get("default_gpt_model").and_then(|v| v.as_str()) {
        config.default_gpt_model = model.to_string();
        changes.push("default_gpt_model updated");
    }

    if let Some(dir) = params.get("default_working_dir").and_then(|v| v.as_str()) {
        config.default_working_dir = dir.to_string();
        changes.push("default_working_dir updated");
    }

    if changes.is_empty() {
        // Just show current config
        Ok(json!({
            "openai_api_key": config.openai_api_key.as_ref().map(|k| format!("{}...{}", &k[..8.min(k.len())], &k[k.len().saturating_sub(4)..])),
            "default_gpt_model": config.default_gpt_model,
            "default_working_dir": config.default_working_dir,
            "gemini_cmd": gemini_cmd(),
            "claude_code_cmd": claude_code_cmd(),
        }))
    } else {
        Ok(json!({
            "changes": changes,
            "message": "Configuration updated"
        }))
    }
}

fn handle_cleanup(server: &Server, params: Value) -> Result<Value, String> {
    let before_days = params
        .get("older_than_days")
        .and_then(|v| v.as_u64())
        .unwrap_or(7);

    let cutoff = Utc::now() - chrono::Duration::days(before_days as i64);

    let mut store = server.runtime.block_on(server.tasks.write());
    let to_remove: Vec<String> = store
        .iter()
        .filter(|(_, t)| {
            t.completed_at.is_some_and(|c| c < cutoff)
                && (t.status == TaskStatus::Done
                    || t.status == TaskStatus::Failed
                    || t.status == TaskStatus::Cancelled)
        })
        .map(|(id, _)| id.clone())
        .collect();

    let count = to_remove.len();
    for id in &to_remove {
        store.remove(id);
        let path = format!("{}\\{}.json", tasks_dir(), id);
        std::fs::remove_file(path).ok();
    }

    Ok(json!({
        "removed": count,
        "remaining": store.len(),
        "message": format!("Cleaned up {} completed tasks older than {} days", count, before_days)
    }))
}

// ============================================================================
// Workflow Execution
// ============================================================================

fn run_workflow_step(
    backend: &str,
    prompt: &str,
    working_dir: &str,
    timeout_secs: u64,
) -> Result<String, String> {
    let (cmd, args): (&str, Vec<String>) = match backend {
        "claude_code" => {
            let a = vec![
                "-p".to_string(),
                prompt.to_string(),
                "--dangerously-skip-permissions".to_string(),
                "--verbose".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--add-dir".to_string(),
                working_dir.to_string(),
            ];
            (claude_code_cmd(), a)
        }
        "codex" => {
            let a = vec![
                "exec".to_string(),
                "--json".to_string(),
                "--skip-git-repo-check".to_string(),
                "--dangerously-bypass-approvals-and-sandbox".to_string(),
                "--cd".to_string(),
                working_dir.to_string(),
                prompt.to_string(),
            ];
            (codex_cmd(), a)
        }
        "gemini" => {
            let a = vec![
                gemini_cmd().to_string(),
                "--yolo".to_string(),
                "-p".to_string(),
                prompt.to_string(),
            ];
            (node_cmd(), a)
        }
        _ => {
            return Err(format!(
                "Unknown backend: '{}'. Use: claude_code, codex, gemini",
                backend
            ))
        }
    };

    let mut child = std::process::Command::new(cmd)
        .args(&args)
        .current_dir(working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to spawn {} for backend '{}': {}", cmd, backend, e))?;

    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(timeout_secs);

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout_buf = String::new();
                let mut stderr_buf = String::new();
                if let Some(mut s) = child.stdout.take() {
                    IoRead::read_to_string(&mut s, &mut stdout_buf).ok();
                }
                if let Some(mut s) = child.stderr.take() {
                    IoRead::read_to_string(&mut s, &mut stderr_buf).ok();
                }
                if status.success() {
                    return Ok(stdout_buf);
                } else {
                    let err_tail = if stderr_buf.len() > 500 {
                        &stderr_buf[stderr_buf.len() - 500..]
                    } else {
                        &stderr_buf
                    };
                    return Err(format!(
                        "Exit code {}. Stderr: {}",
                        status.code().unwrap_or(-1),
                        err_tail
                    ));
                }
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("Timed out after {}s", timeout_secs));
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            Err(e) => return Err(format!("Error waiting for process: {}", e)),
        }
    }
}

fn handle_run_workflow(_server: &Server, args: Value) -> Result<Value, String> {
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'name' parameter")?
        .to_string();

    let steps: Vec<WorkflowStep> = serde_json::from_value(
        args.get("steps")
            .cloned()
            .ok_or("Missing 'steps' parameter")?,
    )
    .map_err(|e| format!("Invalid steps: {}", e))?;

    if steps.is_empty() {
        return Err("Workflow must have at least one step".into());
    }

    let max_total = args
        .get("max_total_attempts")
        .and_then(|v| v.as_u64())
        .unwrap_or(15) as u32;

    let log_results = args
        .get("log_results")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Build step lookup
    let step_map: HashMap<String, usize> = steps
        .iter()
        .enumerate()
        .map(|(i, s)| (s.id.clone(), i))
        .collect();

    let mut results: Vec<Value> = Vec::new();
    let mut previous_output = String::new();
    let mut total_attempts: u32 = 0;
    let mut current_step_idx: usize = 0;
    let mut steps_completed: u32 = 0;

    loop {
        if current_step_idx >= steps.len() {
            break; // All steps done
        }

        if total_attempts >= max_total {
            return Ok(json!({
                "workflow": name,
                "status": "aborted",
                "steps_completed": steps_completed,
                "steps_total": steps.len(),
                "results": results,
                "error": format!("Global attempt limit ({}) reached", max_total)
            }));
        }

        let step = &steps[current_step_idx];
        let max_retries = step.max_retries.unwrap_or(2);
        let timeout_secs = step.timeout_secs.unwrap_or(300);
        let home = default_user_home();
        let working_dir = step.working_dir.as_deref().unwrap_or(&home);

        // Replace {{previous_output}} in prompt
        let prompt = step.prompt.replace("{{previous_output}}", &previous_output);

        let mut success = false;
        let mut attempts: u32 = 0;
        let mut last_error = String::new();
        let mut output = String::new();
        let mut used_backend = step.backend.clone();

        // Try primary backend with retries
        for attempt in 0..=max_retries {
            if total_attempts >= max_total {
                break;
            }
            total_attempts += 1;
            attempts += 1;

            let attempt_prompt = if attempt == 0 {
                prompt.clone()
            } else {
                format!(
                    "{}\n\n[RETRY {}/{}] Previous attempt failed: {}",
                    prompt, attempt, max_retries, last_error
                )
            };

            match run_workflow_step(&step.backend, &attempt_prompt, working_dir, timeout_secs) {
                Ok(out) => {
                    output = out;
                    success = true;
                    break;
                }
                Err(e) => {
                    last_error = e;
                }
            }
        }

        // Try alternative backends if primary failed
        if !success {
            if let Some(alts) = &step.alternatives {
                for alt_backend in alts {
                    if success || total_attempts >= max_total {
                        break;
                    }

                    for alt_attempt in 0..2u32 {
                        if total_attempts >= max_total {
                            break;
                        }
                        total_attempts += 1;
                        attempts += 1;

                        let alt_prompt = if alt_attempt == 0 {
                            format!(
                                "{}\n\n[ESCALATED from {}] Previous attempts failed: {}",
                                prompt, step.backend, last_error
                            )
                        } else {
                            format!(
                                "{}\n\n[ESCALATED from {}, RETRY] Previous error: {}",
                                prompt, step.backend, last_error
                            )
                        };

                        match run_workflow_step(alt_backend, &alt_prompt, working_dir, timeout_secs)
                        {
                            Ok(out) => {
                                output = out;
                                used_backend = alt_backend.clone();
                                success = true;
                                break;
                            }
                            Err(e) => {
                                last_error = e;
                            }
                        }
                    }
                }
            }
        }

        // Record step result
        let output_preview = if output.len() > 500 {
            format!(
                "{}...[{} chars total]",
                safe_truncate(&output, 500),
                output.len()
            )
        } else {
            output.clone()
        };

        results.push(json!({
            "step_id": step.id,
            "backend": used_backend,
            "status": if success { "done" } else { "failed" },
            "attempts": attempts,
            "output_preview": output_preview,
            "error": if success { None } else { Some(&last_error) }
        }));

        if !success {
            return Ok(json!({
                "workflow": name,
                "status": "failed",
                "steps_completed": steps_completed,
                "steps_total": steps.len(),
                "results": results,
                "error": format!("Step '{}' failed after {} attempts: {}", step.id, attempts, last_error)
            }));
        }

        steps_completed += 1;
        previous_output = output;

        // Determine next step
        if let Some(next_id) = &step.on_success {
            match step_map.get(next_id) {
                Some(&idx) => current_step_idx = idx,
                None => {
                    return Ok(json!({
                        "workflow": name,
                        "status": "failed",
                        "steps_completed": steps_completed,
                        "steps_total": steps.len(),
                        "results": results,
                        "error": format!("on_success references unknown step '{}'", next_id)
                    }));
                }
            }
        } else {
            current_step_idx += 1;
        }
    }

    // Log to inbox if requested
    if log_results {
        let inbox_path = volumes_base_path()
            .join("multi_ai_coordination")
            .join("inbox.md");
        if let Ok(mut content) = std::fs::read_to_string(&inbox_path) {
            let entry = format!(
                "\n### [{date}] Workflow '{name}' completed\n**Source:** Manager MCP\n**For:** All backends\n**Detail:** {done}/{total} steps completed successfully.\n",
                date = Utc::now().format("%Y-%m-%d %H:%M"),
                name = name,
                done = steps_completed,
                total = steps.len()
            );
            content.push_str(&entry);
            let _ = std::fs::write(&inbox_path, content);
        }
    }

    Ok(json!({
        "workflow": name,
        "status": "completed",
        "steps_completed": steps_completed,
        "steps_total": steps.len(),
        "results": results,
        "error": null
    }))
}

// ============================================================================
// Item 17: Parallel Workflow with Dependency Gates
// ============================================================================

/// Per-step status tracked during parallel workflow execution.
#[derive(Clone, Debug, Serialize)]
struct ParallelStepResult {
    step_id: String,
    status: String, // "pending", "running", "done", "failed", "skipped"
    backend: String,
    output: String,
    error: Option<String>,
    attempts: u32,
}

fn handle_run_parallel(server: &Server, args: Value) -> Result<Value, String> {
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'name' parameter")?
        .to_string();

    let steps: Vec<WorkflowStep> = serde_json::from_value(
        args.get("steps")
            .cloned()
            .ok_or("Missing 'steps' parameter")?,
    )
    .map_err(|e| format!("Invalid steps: {}", e))?;

    if steps.is_empty() {
        return Err("Workflow must have at least one step".into());
    }

    let max_concurrent = args
        .get("max_concurrent")
        .and_then(|v| v.as_u64())
        .unwrap_or(3) as usize;

    let fail_fast = args
        .get("fail_fast")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Validate: all depends_on references exist
    let step_ids: std::collections::HashSet<&str> = steps.iter().map(|s| s.id.as_str()).collect();
    for step in &steps {
        for dep in &step.depends_on {
            if !step_ids.contains(dep.as_str()) {
                return Err(format!(
                    "Step '{}' depends_on unknown step '{}'",
                    step.id, dep
                ));
            }
        }
    }

    // Detect cycles with simple visited/in-stack DFS
    {
        let adj: HashMap<&str, &[String]> = steps
            .iter()
            .map(|s| (s.id.as_str(), s.depends_on.as_slice()))
            .collect();
        let mut visited = std::collections::HashSet::new();
        let mut stack = std::collections::HashSet::new();
        fn dfs<'a>(
            node: &'a str,
            adj: &HashMap<&'a str, &'a [String]>,
            visited: &mut std::collections::HashSet<&'a str>,
            stack: &mut std::collections::HashSet<&'a str>,
        ) -> bool {
            if stack.contains(node) {
                return true;
            } // cycle
            if visited.contains(node) {
                return false;
            }
            visited.insert(node);
            stack.insert(node);
            if let Some(deps) = adj.get(node) {
                for dep in *deps {
                    if dfs(dep.as_str(), adj, visited, stack) {
                        return true;
                    }
                }
            }
            stack.remove(node);
            false
        }
        for s in &steps {
            if dfs(s.id.as_str(), &adj, &mut visited, &mut stack) {
                return Err(format!(
                    "Dependency cycle detected involving step '{}'",
                    s.id
                ));
            }
        }
    }

    // Build shared state
    let step_results: Arc<RwLock<HashMap<String, ParallelStepResult>>> = Arc::new(RwLock::new(
        steps
            .iter()
            .map(|s| {
                (
                    s.id.clone(),
                    ParallelStepResult {
                        step_id: s.id.clone(),
                        status: "pending".into(),
                        backend: s.backend.clone(),
                        output: String::new(),
                        error: None,
                        attempts: 0,
                    },
                )
            })
            .collect(),
    ));

    let failed_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let steps_arc = Arc::new(steps.clone());

    // Async: spawn workflow, return immediately with workflow_id
    let rt = server.runtime.clone();
    let wf_id = format!("wf_{}", &Uuid::new_v4().to_string()[..8]);

    let wf_task = Task {
        id: wf_id.clone(),
        backend: Backend::ClaudeCode,
        prompt: format!("Parallel workflow: {}", name),
        system_prompt: None,
        model: None,
        working_dir: None,
        status: TaskStatus::Running,
        output: String::new(),
        error: None,
        created_at: Utc::now(),
        started_at: Some(Utc::now()),
        completed_at: None,
        progress_lines: 0,
        steps: Vec::new(),
        last_activity: Some(Utc::now()),
        last_output_chunk_at: None,
        stall_detected: false,
        extraction_status: ExtractionStatus::None,
        trust_score: 0,
        trust_level: TrustLevel::Low,
        rollback_path: None,
        validation_status: ValidationStatus::NotChecked,
        assertions: Vec::new(),
        backed_up_files: Vec::new(),
        retry_count: 0,
        max_retries: 0,
        retry_of: None,
        error_context: None,
        input_tokens: 0,
        output_tokens: 0,
        cost_usd: 0.0,
        on_complete: None,
        role: None,
        save_artifact: false,
        rerun_of: None,
        parent_task_id: None,
        forked_from: None,
        continuation_of: None,
        child_pid: None,
        log_path: None,
        log_offset: 0,
        reconnected: false,
        watchdog_observations: Vec::new(),
        fingerprint: None,
        superseded_by: None,
        label: None,
        current_step: None,
        total_steps: None,
        current_step_desc: None,
        live_activity: None,
        effort: None,
        notify_on_complete: None,
        notify_on_fail: None,
        notify_on_destroy: None,
        notify_title: None,
        notify_body: None,
    };
    rt.block_on(async {
        server.tasks.write().await.insert(wf_id.clone(), wf_task);
    });

    let tasks_ref = server.tasks.clone();
    let wf_id_bg = wf_id.clone();
    let steps_for_result = steps.clone();
    rt.spawn(async move {
        let result = run_parallel_workflow(
            steps_arc,
            step_results.clone(),
            failed_flag.clone(),
            max_concurrent,
            fail_fast,
        )
        .await;

        let final_results = step_results.read().await;
        let mut parts: Vec<String> = Vec::new();
        let (mut done_c, mut fail_c, mut skip_c) = (0u32, 0u32, 0u32);
        for step in &steps_for_result {
            if let Some(r) = final_results.get(&step.id) {
                match r.status.as_str() {
                    "done" => done_c += 1,
                    "failed" => fail_c += 1,
                    "skipped" => skip_c += 1,
                    _ => {}
                }
                let preview = safe_truncate(&r.output, 300);
                parts.push(format!(
                    "[{}] {} ({}): {}",
                    r.status, r.step_id, r.backend, preview
                ));
            }
        }
        let overall = if fail_c == 0 && skip_c == 0 {
            "done"
        } else if done_c > 0 {
            "partial"
        } else {
            "failed"
        };
        let summary = format!(
            "{}/{} done, {} failed, {} skipped\n{}",
            done_c,
            steps_for_result.len(),
            fail_c,
            skip_c,
            parts.join("\n")
        );

        let mut tasks = tasks_ref.write().await;
        if let Some(t) = tasks.get_mut(&wf_id_bg) {
            t.status = if overall == "done" {
                TaskStatus::Done
            } else {
                TaskStatus::Failed
            };
            t.output = summary;
            t.completed_at = Some(Utc::now());
            t.last_activity = Some(Utc::now());
            if let Some(err) = result.err() {
                t.error = Some(err);
            }
        }
    });

    Ok(json!({
        "workflow_id": wf_id,
        "status": "running",
        "steps_total": steps.len(),
        "note": "Workflow running in background. Poll with get_status using workflow_id."
    }))

    // Dead code below (kept for reference, compiler may warn)
}

/// Async orchestrator: runs steps in parallel respecting dependency gates and concurrency limit.
async fn run_parallel_workflow(
    steps: Arc<Vec<WorkflowStep>>,
    results: Arc<RwLock<HashMap<String, ParallelStepResult>>>,
    failed_flag: Arc<std::sync::atomic::AtomicBool>,
    max_concurrent: usize,
    fail_fast: bool,
) -> Result<(), String> {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrent));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(steps.len().max(1));

    // Seed: launch all steps with no dependencies
    let mut pending_count = steps.len();
    for step in steps.iter() {
        if step.depends_on.is_empty() {
            launch_step(
                step.clone(),
                steps.clone(),
                results.clone(),
                failed_flag.clone(),
                semaphore.clone(),
                tx.clone(),
                fail_fast,
            );
        }
    }

    // As each step completes, check what it unblocks
    while let Some(completed_id) = rx.recv().await {
        pending_count = pending_count.saturating_sub(1);
        if pending_count == 0 {
            break;
        }

        // Find steps that depended on the completed one
        for step in steps.iter() {
            if !step.depends_on.contains(&completed_id) {
                continue;
            }
            // Check if ALL deps are now done
            let store = results.read().await;
            let all_deps_met = step
                .depends_on
                .iter()
                .all(|dep_id| store.get(dep_id).is_some_and(|r| r.status == "done"));
            let any_dep_failed = step.depends_on.iter().any(|dep_id| {
                store
                    .get(dep_id)
                    .is_some_and(|r| r.status == "failed" || r.status == "skipped")
            });
            let still_pending = store.get(&step.id).is_some_and(|r| r.status == "pending");
            drop(store);

            if !still_pending {
                continue;
            }

            if any_dep_failed {
                // Skip this step — a dependency failed
                let mut store = results.write().await;
                if let Some(r) = store.get_mut(&step.id) {
                    r.status = "skipped".into();
                    r.error = Some("Dependency failed".into());
                }
                drop(store);
                // Notify so downstream of this skipped step also get processed
                let _ = tx.send(step.id.clone()).await;
            } else if all_deps_met {
                launch_step(
                    step.clone(),
                    steps.clone(),
                    results.clone(),
                    failed_flag.clone(),
                    semaphore.clone(),
                    tx.clone(),
                    fail_fast,
                );
            }
        }
    }

    Ok(())
}

/// Spawn a single workflow step on a background task.
fn launch_step(
    step: WorkflowStep,
    _all_steps: Arc<Vec<WorkflowStep>>,
    results: Arc<RwLock<HashMap<String, ParallelStepResult>>>,
    failed_flag: Arc<std::sync::atomic::AtomicBool>,
    semaphore: Arc<tokio::sync::Semaphore>,
    tx: tokio::sync::mpsc::Sender<String>,
    fail_fast: bool,
) {
    tokio::spawn(async move {
        // Acquire concurrency permit
        let _permit = semaphore.acquire().await;

        // Check fail_fast before starting
        if fail_fast && failed_flag.load(std::sync::atomic::Ordering::Relaxed) {
            let mut store = results.write().await;
            if let Some(r) = store.get_mut(&step.id) {
                r.status = "skipped".into();
                r.error = Some("Skipped due to fail_fast".into());
            }
            drop(store);
            let _ = tx.send(step.id.clone()).await;
            return;
        }

        // Mark running
        {
            let mut store = results.write().await;
            if let Some(r) = store.get_mut(&step.id) {
                r.status = "running".into();
            }
        }

        // Build prompt with {{step_id.output}} template substitution
        let prompt = {
            let store = results.read().await;
            let mut p = step.prompt.clone();
            // Replace {{previous_output}} with empty for parallel (no single predecessor)
            p = p.replace("{{previous_output}}", "");
            // Replace {{step_id.output}} references
            for (sid, sr) in store.iter() {
                let placeholder = format!("{{{{{}.output}}}}", sid);
                if p.contains(&placeholder) {
                    p = p.replace(&placeholder, &sr.output);
                }
            }
            p
        };

        let home = default_user_home();
        let working_dir = step.working_dir.as_deref().unwrap_or(&home);
        let timeout_secs = step.timeout_secs.unwrap_or(300);
        let max_retries = step.max_retries.unwrap_or(2);

        let mut success = false;
        let mut attempts: u32 = 0;
        let mut last_error = String::new();
        let mut output = String::new();
        let mut used_backend = step.backend.clone();

        // Try primary backend with retries
        for attempt in 0..=max_retries {
            attempts += 1;
            let attempt_prompt = if attempt == 0 {
                prompt.clone()
            } else {
                format!(
                    "{}\n\n[RETRY {}/{}] Previous attempt failed: {}",
                    prompt, attempt, max_retries, last_error
                )
            };

            // run_workflow_step is sync/blocking — run on blocking thread pool
            let backend = step.backend.clone();
            let wd = working_dir.to_string();
            let ap = attempt_prompt.clone();
            match tokio::task::spawn_blocking(move || {
                run_workflow_step(&backend, &ap, &wd, timeout_secs)
            })
            .await
            {
                Ok(Ok(out)) => {
                    output = out;
                    success = true;
                    break;
                }
                Ok(Err(e)) => {
                    last_error = e;
                }
                Err(e) => {
                    last_error = format!("Join error: {}", e);
                    break;
                }
            }
        }

        // Try alternative backends if primary failed
        if !success {
            if let Some(alts) = &step.alternatives {
                for alt_backend in alts {
                    if success {
                        break;
                    }
                    for alt_attempt in 0..2u32 {
                        attempts += 1;
                        let alt_prompt = if alt_attempt == 0 {
                            format!(
                                "{}\n\n[ESCALATED from {}] Previous attempts failed: {}",
                                prompt, step.backend, last_error
                            )
                        } else {
                            format!(
                                "{}\n\n[ESCALATED from {}, RETRY] Previous error: {}",
                                prompt, step.backend, last_error
                            )
                        };
                        let ab = alt_backend.clone();
                        let wd = working_dir.to_string();
                        let ap = alt_prompt.clone();
                        let ts = timeout_secs;
                        match tokio::task::spawn_blocking(move || {
                            run_workflow_step(&ab, &ap, &wd, ts)
                        })
                        .await
                        {
                            Ok(Ok(out)) => {
                                output = out;
                                used_backend = alt_backend.clone();
                                success = true;
                                break;
                            }
                            Ok(Err(e)) => {
                                last_error = e;
                            }
                            Err(e) => {
                                last_error = format!("Join error: {}", e);
                                break;
                            }
                        }
                    }
                }
            }
        }

        // Update result
        {
            let mut store = results.write().await;
            if let Some(r) = store.get_mut(&step.id) {
                r.status = if success {
                    "done".into()
                } else {
                    "failed".into()
                };
                r.backend = used_backend;
                r.output = output;
                r.error = if success { None } else { Some(last_error) };
                r.attempts = attempts;
            }
        }

        if !success {
            failed_flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }

        // Notify orchestrator this step finished
        let _ = tx.send(step.id.clone()).await;
    });
}

// ============================================================================
// Product Layer: Decompose / Templates / Explain
// ============================================================================

fn handle_decompose_task(args: Value) -> Result<Value, String> {
    let prompt = args
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'prompt'")?;
    let working_dir = args.get("working_dir").and_then(|v| v.as_str());

    // Try numbered steps first: "1. do X  2. do Y"
    let mut raw_steps: Vec<String> = Vec::new();
    let mut buf = String::new();
    let mut found_numbered = false;
    for line in prompt.lines() {
        let trimmed = line.trim();
        if trimmed.len() > 2
            && trimmed
                .chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false)
            && (trimmed.contains(". ") || trimmed.contains(") "))
        {
            if !buf.is_empty() {
                raw_steps.push(buf.trim().to_string());
                buf.clear();
            }
            let content = trimmed
                .split_once(['.', ')'])
                .map(|x| x.1)
                .unwrap_or(trimmed)
                .trim();
            buf = content.to_string();
            found_numbered = true;
        } else {
            if !buf.is_empty() {
                buf.push(' ');
            }
            buf.push_str(trimmed);
        }
    }
    if !buf.is_empty() {
        raw_steps.push(buf.trim().to_string());
    }

    // Fall back to connector splitting
    if !found_numbered || raw_steps.len() <= 1 {
        raw_steps.clear();
        let lower = prompt.to_lowercase();
        let connectors = [
            " then ",
            " and then ",
            " after that, ",
            " next, ",
            " finally ",
        ];
        let mut splits: Vec<(usize, usize)> = Vec::new();
        for conn in &connectors {
            let mut from = 0;
            while let Some(pos) = lower[from..].find(conn) {
                splits.push((from + pos, conn.len()));
                from = from + pos + conn.len();
            }
        }
        splits.sort_by_key(|s| s.0);
        if splits.is_empty() {
            raw_steps.push(prompt.to_string());
        } else {
            let mut last = 0;
            for (pos, len) in &splits {
                let chunk = prompt[last..*pos].trim();
                if !chunk.is_empty() {
                    raw_steps.push(chunk.to_string());
                }
                last = pos + len;
            }
            let tail = prompt[last..].trim();
            if !tail.is_empty() {
                raw_steps.push(tail.to_string());
            }
        }
    }

    let mut steps = Vec::new();
    for (i, step_prompt) in raw_steps.iter().enumerate() {
        let rec = Server::recommend_backend(step_prompt, working_dir);
        steps.push(json!({
            "id": format!("step_{}", i + 1),
            "prompt": step_prompt,
            "recommended_backend": rec.recommended_backend,
            "confidence": rec.confidence,
            "reason": rec.reasoning,
            "depends_on": if i > 0 { vec![format!("step_{}", i)] } else { vec![] as Vec<String> },
        }));
    }

    Ok(json!({
        "original_prompt": prompt,
        "steps": steps,
        "total_steps": steps.len(),
        "note": if steps.len() == 1 { "Single-step task. Use submit_task directly." }
                else { "Multi-step task decomposed. Feed to run_parallel or run_workflow." }
    }))
}

fn handle_save_template(args: Value) -> Result<Value, String> {
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'name'")?;
    let description = args
        .get("description")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'description'")?;
    let steps: Vec<Value> =
        serde_json::from_value(args.get("steps").cloned().ok_or("Missing 'steps'")?)
            .map_err(|e| format!("Invalid steps: {}", e))?;
    let backend = args
        .get("backend")
        .and_then(|v| v.as_str())
        .unwrap_or("claude_code");
    let params: HashMap<String, String> = args
        .get("parameters")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    let template = json!({
        "name": name, "description": description, "parameters": params,
        "steps": steps, "backend": backend, "trust_level": "auto_with_backup",
        "source": "manual", "times_used": 0, "last_used": "", "success_rate": 1.0,
    });

    std::fs::create_dir_all(workflow_patterns_dir()).map_err(|e| format!("mkdir: {}", e))?;
    let path = format!(
        "{}\\{}.json",
        workflow_patterns_dir(),
        name.replace(' ', "_")
    );
    let content = serde_json::to_string_pretty(&template).map_err(|e| format!("json: {}", e))?;
    std::fs::write(&path, &content).map_err(|e| format!("write: {}", e))?;

    Ok(
        json!({ "saved": path, "name": name, "steps": steps.len(), "parameters": params.keys().collect::<Vec<_>>() }),
    )
}

fn handle_list_templates(_args: Value) -> Result<Value, String> {
    if !std::path::Path::new(workflow_patterns_dir()).exists() {
        return Ok(json!({ "templates": [], "count": 0 }));
    }
    let mut templates = Vec::new();
    if let Ok(entries) = std::fs::read_dir(workflow_patterns_dir()) {
        for entry in entries.flatten() {
            if entry
                .path()
                .extension()
                .map(|e| e == "json")
                .unwrap_or(false)
            {
                if let Ok(c) = std::fs::read_to_string(entry.path()) {
                    if let Ok(t) = serde_json::from_str::<Value>(&c) {
                        templates.push(json!({
                            "name": t.get("name").and_then(|v| v.as_str()).unwrap_or("?"),
                            "description": t.get("description").and_then(|v| v.as_str()).unwrap_or(""),
                            "backend": t.get("backend").and_then(|v| v.as_str()).unwrap_or("?"),
                            "parameters": t.get("parameters"),
                            "times_used": t.get("times_used").and_then(|v| v.as_u64()).unwrap_or(0),
                            "success_rate": t.get("success_rate").and_then(|v| v.as_f64()).unwrap_or(1.0),
                            "file": entry.file_name().to_string_lossy().to_string(),
                        }));
                    }
                }
            }
        }
    }
    let count = templates.len();
    Ok(json!({ "templates": templates, "count": count }))
}

fn handle_run_template(server: &Server, args: Value) -> Result<Value, String> {
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'name'")?;
    let params: HashMap<String, String> = args
        .get("parameters")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let working_dir = args.get("working_dir").and_then(|v| v.as_str());

    let path = format!(
        "{}\\{}.json",
        workflow_patterns_dir(),
        name.replace(' ', "_")
    );
    let file_content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Template '{}' not found: {}", name, e))?;
    let mut tmpl: Value =
        serde_json::from_str(&file_content).map_err(|e| format!("Invalid template: {}", e))?;

    let steps = tmpl
        .get("steps")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let backend = tmpl
        .get("backend")
        .and_then(|v| v.as_str())
        .unwrap_or("claude_code")
        .to_string();

    let mut combined = String::new();
    for step in &steps {
        if let Some(p) = step.get("prompt").and_then(|v| v.as_str()) {
            let mut expanded = p.to_string();
            for (k, v) in &params {
                expanded = expanded.replace(&format!("{{{{{}}}}}", k), v);
            }
            if !combined.is_empty() {
                combined.push_str("\n\n");
            }
            if let Some(id) = step.get("id").and_then(|v| v.as_str()) {
                combined.push_str(&format!("Step {}: ", id));
            }
            combined.push_str(&expanded);
        }
    }

    let wd = working_dir
        .map(String::from)
        .or_else(|| {
            tmpl.get("working_dir")
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .unwrap_or_else(workspace_root);
    let submit_args = json!({ "prompt": combined, "backend": backend, "working_dir": wd });
    let result = handle_submit_task(server, submit_args)?;

    if let Some(used) = tmpl.get("times_used").and_then(|v| v.as_u64()) {
        tmpl["times_used"] = json!(used + 1);
    }
    tmpl["last_used"] = json!(chrono::Utc::now().to_rfc3339());
    if let Ok(updated) = serde_json::to_string_pretty(&tmpl) {
        let _ = std::fs::write(&path, updated);
    }

    Ok(json!({
        "template": name, "task_id": result.get("task_id"),
        "backend": backend, "parameters_applied": params, "steps_count": steps.len(),
    }))
}

fn handle_explain_task(server: &Server, args: Value) -> Result<Value, String> {
    let task_id = args.get("task_id").and_then(|v| v.as_str());
    let last_n = args.get("last").and_then(|v| v.as_u64()).unwrap_or(5) as usize;

    if let Some(tid) = task_id {
        let tasks = server.runtime.block_on(server.tasks.read());
        if let Some(task) = tasks.get(tid) {
            let elapsed = task
                .created_at
                .signed_duration_since(chrono::Utc::now())
                .num_seconds()
                .unsigned_abs();
            let step_trail: Vec<String> = task
                .steps
                .iter()
                .map(|s| format!("{} ({})", s.tool, s.status))
                .collect();
            let explanation = format!(
                "Task {}: You asked to {}. Backend: {}. Status: {}. Duration: {}s.{}",
                tid,
                safe_truncate(&task.prompt, 120),
                task.backend,
                task.status,
                elapsed,
                if step_trail.is_empty() {
                    String::new()
                } else {
                    format!(" Steps: {}", step_trail.join(" -> "))
                }
            );
            return Ok(json!({
                "task_id": tid, "explanation": explanation,
                "status": task.status.to_string(), "backend": task.backend.to_string(),
                "duration_secs": elapsed, "steps": task.steps.len(),
            }));
        }
        return Err(format!("Task '{}' not found in active tasks", tid));
    }

    // No task_id: summarize recent history
    let history_path = format!("{}\\task_history.json", history_dir());
    if let Ok(c) = std::fs::read_to_string(&history_path) {
        if let Ok(entries) = serde_json::from_str::<Vec<Value>>(&c) {
            let recent: Vec<&Value> = entries.iter().rev().take(last_n).collect();
            let summaries: Vec<String> = recent
                .iter()
                .map(|e| {
                    format!(
                        "{} - {} via {} ({} steps, {})",
                        e.get("task_id").and_then(|v| v.as_str()).unwrap_or("?"),
                        e.get("prompt_summary")
                            .and_then(|v| v.as_str())
                            .unwrap_or("?"),
                        e.get("backend").and_then(|v| v.as_str()).unwrap_or("?"),
                        e.get("step_count").and_then(|v| v.as_u64()).unwrap_or(0),
                        e.get("status").and_then(|v| v.as_str()).unwrap_or("?")
                    )
                })
                .collect();
            return Ok(
                json!({ "recent_tasks": last_n, "total_in_history": entries.len(), "summary": summaries.join("\n") }),
            );
        }
    }
    Ok(json!({ "summary": "No task history found.", "recent_tasks": 0 }))
}

// ============================================================================
// Project Loaf — Multi-Task Coordination
// ============================================================================

fn loaf_path(loaf_id: &str) -> String {
    format!("{}\\{}.json", loaves_dir(), loaf_id)
}

/// Find the most recent active loaf in the loaves directory
fn find_active_loaf() -> Option<(String, Value)> {
    let dir = std::fs::read_dir(loaves_dir()).ok()?;
    let mut best: Option<(String, Value, String)> = None; // (id, value, created)
    for entry in dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(v) = serde_json::from_str::<Value>(&content) {
                if v.get("status").and_then(|s| s.as_str()) == Some("active") {
                    let created = v
                        .get("created")
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string();
                    if best.as_ref().is_none_or(|(_, _, bc)| created > *bc) {
                        let id = v
                            .get("loaf_id")
                            .and_then(|i| i.as_str())
                            .unwrap_or("")
                            .to_string();
                        best = Some((id, v, created));
                    }
                }
            }
        }
    }
    best.map(|(id, v, _)| (id, v))
}

/// v1.3.4: Auto-advance loaf phase when a task linked to the current phase completes Done.
///
/// A task is linked to a phase if its prompt (after CPC DELEGATION CONTEXT injection) contains
/// "Phase: {phase_name}" matching the loaf's current phase at the time of task submission.
///
/// This is best-effort: only fires when status==Done (failures do not advance); silent if no loaf
/// or no match. Phases list gets the current phase marked status=done and current_phase index++.
/// If already on the last phase, marks the whole loaf status=completed.
fn auto_advance_loaf_on_task_complete(task: &Task) {
    if task.status != TaskStatus::Done {
        return;
    }
    let (loaf_id, loaf) = match find_active_loaf() {
        Some(pair) => pair,
        None => return,
    };
    let phase_idx = loaf
        .get("current_phase")
        .and_then(|p| p.as_u64())
        .unwrap_or(0) as usize;
    let phases = match loaf.get("phases").and_then(|p| p.as_array()) {
        Some(p) => p.clone(),
        None => return,
    };
    let current_phase_name = match phases
        .get(phase_idx)
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
    {
        Some(n) => n.to_string(),
        None => return,
    };
    // Match: task's prompt must have been injected with the current phase marker.
    // loaf_context produces: "Phase: {phase_name}." so we look for that.
    let marker = format!("Phase: {}.", current_phase_name);
    if !task.prompt.contains(&marker) {
        return;
    }
    // Read-modify-write the loaf file.
    let path = loaf_path(&loaf_id);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            warn!("auto_advance_loaf: read failed {}: {}", loaf_id, e);
            return;
        }
    };
    let mut loaf_live: Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            warn!("auto_advance_loaf: parse failed {}: {}", loaf_id, e);
            return;
        }
    };
    let phases_mut = match loaf_live.get_mut("phases").and_then(|p| p.as_array_mut()) {
        Some(p) => p,
        None => return,
    };
    if let Some(phase_obj) = phases_mut
        .get_mut(phase_idx)
        .and_then(|p| p.as_object_mut())
    {
        phase_obj.insert("status".into(), json!("done"));
        phase_obj.insert("completed_at".into(), json!(Utc::now().to_rfc3339()));
        phase_obj.insert("completed_by_task".into(), json!(task.id));
    }
    let total_phases = phases_mut.len();
    let next_idx = phase_idx + 1;
    if next_idx < total_phases {
        // Mark next phase active
        if let Some(next_phase) = phases_mut.get_mut(next_idx).and_then(|p| p.as_object_mut()) {
            next_phase.insert("status".into(), json!("active"));
        }
        loaf_live["current_phase"] = json!(next_idx);
    } else {
        // Was on final phase — mark whole loaf completed
        loaf_live["status"] = json!("completed");
        loaf_live["completed_at"] = json!(Utc::now().to_rfc3339());
    }
    // Append breadcrumb event for audit trail
    if let Some(bc_arr) = loaf_live
        .get_mut("breadcrumbs")
        .and_then(|b| b.as_array_mut())
    {
        bc_arr.push(json!({
            "timestamp": Utc::now().to_rfc3339(),
            "event": format!("Phase '{}' auto-advanced on task {} complete", current_phase_name, task.id),
        }));
    }
    match std::fs::write(
        &path,
        serde_json::to_string_pretty(&loaf_live).unwrap_or_default(),
    ) {
        Ok(_) => info!(
            "auto_advance_loaf: {} phase '{}' → done (task {})",
            loaf_id, current_phase_name, task.id
        ),
        Err(e) => warn!("auto_advance_loaf: write failed {}: {}", loaf_id, e),
    }
}

fn handle_loaf_create(_server: &Server, params: Value) -> Result<Value, String> {
    let goal = params
        .get("goal")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'goal' parameter")?;
    let project_name = params
        .get("project_name")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'project_name' parameter")?;

    let _ = std::fs::create_dir_all(loaves_dir());

    let phases: Vec<Value> = if let Some(arr) = params.get("phases").and_then(|v| v.as_array()) {
        arr.iter().map(|p| {
            let name = p.as_str().unwrap_or("unnamed");
            json!({"name": name, "status": if arr.first() == Some(p) { "active" } else { "pending" }, "tasks": []})
        }).collect()
    } else {
        vec![json!({"name": "main", "status": "active", "tasks": []})]
    };

    let loaf_id = format!("{}_Loaf", project_name);
    let now = Utc::now().to_rfc3339();
    let loaf = json!({
        "loaf_id": loaf_id,
        "goal": goal,
        "created": now,
        "status": "active",
        "current_phase": 0,
        "phases": phases,
        "decisions": [],
        "discoveries": [],
        "next_actions": [],
        "breadcrumbs": [{"timestamp": now, "event": "Loaf created"}],
        "metadata": {"total_tasks": 0, "completed_tasks": 0, "total_cost_usd": 0.0}
    });

    let path = loaf_path(&loaf_id);
    std::fs::write(&path, serde_json::to_string_pretty(&loaf).unwrap())
        .map_err(|e| format!("Failed to write loaf: {}", e))?;

    Ok(json!({"loaf_id": loaf_id, "path": path, "status": "created"}))
}

fn handle_loaf_update(_server: &Server, params: Value) -> Result<Value, String> {
    let loaf_id = params
        .get("loaf_id")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'loaf_id' parameter")?;

    let path = loaf_path(loaf_id);
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read loaf '{}': {}", loaf_id, e))?;
    let mut loaf: Value =
        serde_json::from_str(&content).map_err(|e| format!("Failed to parse loaf: {}", e))?;

    let now = Utc::now().to_rfc3339();
    // Collect breadcrumb events, push them all at the end to avoid borrow conflicts
    let mut new_breadcrumbs: Vec<Value> = Vec::new();

    // Task update
    if let Some(task_update) = params.get("task_update") {
        let task_id = task_update
            .get("task_id")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let status = task_update
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown")
            .to_string();

        let phase_idx = loaf
            .get("current_phase")
            .and_then(|p| p.as_u64())
            .unwrap_or(0) as usize;
        if let Some(phases) = loaf.get_mut("phases").and_then(|p| p.as_array_mut()) {
            if let Some(phase) = phases.get_mut(phase_idx) {
                if let Some(tasks) = phase.get_mut("tasks").and_then(|t| t.as_array_mut()) {
                    // Find existing task or add new one
                    let existing = tasks
                        .iter_mut()
                        .find(|t| t.get("task_id").and_then(|i| i.as_str()) == Some(&task_id));
                    if let Some(t) = existing {
                        if let Some(s) = task_update.get("status") {
                            t["status"] = s.clone();
                        }
                        if let Some(s) = task_update.get("output_summary") {
                            t["output_summary"] = s.clone();
                        }
                        if let Some(s) = task_update.get("files_changed") {
                            t["files_changed"] = s.clone();
                        }
                    } else {
                        tasks.push(task_update.clone());
                    }

                    // Update metadata counts
                    let total = tasks.len();
                    let completed = tasks
                        .iter()
                        .filter(|t| t.get("status").and_then(|s| s.as_str()) == Some("done"))
                        .count();
                    loaf["metadata"]["total_tasks"] = json!(total);
                    loaf["metadata"]["completed_tasks"] = json!(completed);
                }
            }
        }
        new_breadcrumbs
            .push(json!({"timestamp": &now, "event": format!("Task {} -> {}", task_id, status)}));

        // Capture decisions from task
        if let Some(decisions) = task_update.get("decisions_made").and_then(|d| d.as_array()) {
            if let Some(loaf_decisions) = loaf.get_mut("decisions").and_then(|d| d.as_array_mut()) {
                for d in decisions {
                    loaf_decisions.push(json!({"what": d, "who": "delegated_task", "when": &now}));
                }
            }
        }
        // Capture discoveries from task
        if let Some(discoveries) = task_update.get("discoveries").and_then(|d| d.as_array()) {
            if let Some(loaf_discoveries) =
                loaf.get_mut("discoveries").and_then(|d| d.as_array_mut())
            {
                for d in discoveries {
                    loaf_discoveries.push(json!({"what": d, "when": &now}));
                }
            }
        }
    }

    // Direct decision
    if let Some(decision) = params.get("decision") {
        let what = decision
            .get("what")
            .and_then(|w| w.as_str())
            .unwrap_or("?")
            .to_string();
        if let Some(decisions) = loaf.get_mut("decisions").and_then(|d| d.as_array_mut()) {
            decisions.push(json!({
                "what": decision.get("what"),
                "why": decision.get("why"),
                "who": decision.get("who").and_then(|w| w.as_str()).unwrap_or("manager"),
                "when": &now
            }));
        }
        new_breadcrumbs.push(json!({"timestamp": &now, "event": format!("Decision: {}", what)}));
    }

    // Direct discovery
    if let Some(discovery) = params.get("discovery") {
        let what = discovery
            .get("what")
            .and_then(|w| w.as_str())
            .unwrap_or("?")
            .to_string();
        if let Some(discoveries) = loaf.get_mut("discoveries").and_then(|d| d.as_array_mut()) {
            discoveries.push(json!({
                "what": discovery.get("what"),
                "impact": discovery.get("impact"),
                "when": &now
            }));
        }
        new_breadcrumbs.push(json!({"timestamp": &now, "event": format!("Discovery: {}", what)}));
    }

    // Replace next_actions
    if let Some(actions) = params.get("next_actions") {
        loaf["next_actions"] = actions.clone();
        new_breadcrumbs.push(json!({"timestamp": &now, "event": "Next actions updated"}));
    }

    // Advance phase
    if let Some(ps) = params.get("phase_status").and_then(|s| s.as_str()) {
        if ps == "done" {
            let phase_idx = loaf
                .get("current_phase")
                .and_then(|p| p.as_u64())
                .unwrap_or(0) as usize;
            if let Some(phases) = loaf.get_mut("phases").and_then(|p| p.as_array_mut()) {
                if let Some(phase) = phases.get_mut(phase_idx) {
                    phase["status"] = json!("done");
                }
                let next = phase_idx + 1;
                if next < phases.len() {
                    phases[next]["status"] = json!("active");
                    let name = phases[next]
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("?")
                        .to_string();
                    new_breadcrumbs.push(
                        json!({"timestamp": &now, "event": format!("Phase advanced to: {}", name)}),
                    );
                }
            }
            // Set current_phase outside the phases borrow
            let phase_idx = loaf
                .get("current_phase")
                .and_then(|p| p.as_u64())
                .unwrap_or(0) as usize;
            let total_phases = loaf
                .get("phases")
                .and_then(|p| p.as_array())
                .map(|p| p.len())
                .unwrap_or(0);
            if phase_idx + 1 < total_phases {
                loaf["current_phase"] = json!(phase_idx + 1);
            }
        }
    }

    // Now push all collected breadcrumbs at once
    if let Some(bc) = loaf.get_mut("breadcrumbs").and_then(|b| b.as_array_mut()) {
        bc.extend(new_breadcrumbs);
    }

    std::fs::write(&path, serde_json::to_string_pretty(&loaf).unwrap())
        .map_err(|e| format!("Failed to write loaf: {}", e))?;

    let phase_name = loaf
        .get("phases")
        .and_then(|p| p.as_array())
        .and_then(|p| {
            p.get(
                loaf.get("current_phase")
                    .and_then(|i| i.as_u64())
                    .unwrap_or(0) as usize,
            )
        })
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or("?");

    Ok(json!({
        "loaf_id": loaf_id,
        "status": "updated",
        "current_phase": phase_name,
        "metadata": loaf.get("metadata")
    }))
}

fn handle_loaf_status(_server: &Server, params: Value) -> Result<Value, String> {
    let (loaf_id, loaf) = if let Some(id) = params.get("loaf_id").and_then(|v| v.as_str()) {
        let path = loaf_path(id);
        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read loaf '{}': {}", id, e))?;
        let v: Value =
            serde_json::from_str(&content).map_err(|e| format!("Failed to parse loaf: {}", e))?;
        (id.to_string(), v)
    } else {
        find_active_loaf().ok_or("No active loaf found")?
    };

    let phase_idx = loaf
        .get("current_phase")
        .and_then(|p| p.as_u64())
        .unwrap_or(0) as usize;
    let phase_name = loaf
        .get("phases")
        .and_then(|p| p.as_array())
        .and_then(|p| p.get(phase_idx))
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or("?");

    let total = loaf
        .get("metadata")
        .and_then(|m| m.get("total_tasks"))
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    let completed = loaf
        .get("metadata")
        .and_then(|m| m.get("completed_tasks"))
        .and_then(|t| t.as_u64())
        .unwrap_or(0);

    let breadcrumbs = loaf
        .get("breadcrumbs")
        .and_then(|b| b.as_array())
        .map(|b| {
            let skip = if b.len() > 5 { b.len() - 5 } else { 0 };
            b.iter().skip(skip).cloned().collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Ok(json!({
        "loaf_id": loaf_id,
        "goal": loaf.get("goal"),
        "status": loaf.get("status"),
        "current_phase": phase_name,
        "total_tasks": total,
        "completed_tasks": completed,
        "pending_tasks": total - completed,
        "last_breadcrumbs": breadcrumbs,
        "next_actions": loaf.get("next_actions"),
        "decisions_count": loaf.get("decisions").and_then(|d| d.as_array()).map(|d| d.len()).unwrap_or(0),
        "discoveries_count": loaf.get("discoveries").and_then(|d| d.as_array()).map(|d| d.len()).unwrap_or(0)
    }))
}

fn handle_loaf_close(_server: &Server, params: Value) -> Result<Value, String> {
    let loaf_id = params
        .get("loaf_id")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'loaf_id' parameter")?;

    let path = loaf_path(loaf_id);
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read loaf '{}': {}", loaf_id, e))?;
    let mut loaf: Value =
        serde_json::from_str(&content).map_err(|e| format!("Failed to parse loaf: {}", e))?;

    let now = Utc::now().to_rfc3339();
    loaf["status"] = json!("completed");
    if let Some(bc) = loaf.get_mut("breadcrumbs").and_then(|b| b.as_array_mut()) {
        bc.push(json!({"timestamp": now, "event": "Loaf completed and archived"}));
    }

    let _ = std::fs::create_dir_all(loaves_archive_dir());
    let archive_path = format!("{}\\{}.json", loaves_archive_dir(), loaf_id);
    std::fs::write(&archive_path, serde_json::to_string_pretty(&loaf).unwrap())
        .map_err(|e| format!("Failed to write archive: {}", e))?;
    let _ = std::fs::remove_file(&path);

    let total = loaf
        .get("metadata")
        .and_then(|m| m.get("total_tasks"))
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    let completed = loaf
        .get("metadata")
        .and_then(|m| m.get("completed_tasks"))
        .and_then(|t| t.as_u64())
        .unwrap_or(0);

    Ok(json!({
        "loaf_id": loaf_id,
        "status": "archived",
        "archive_path": archive_path,
        "goal": loaf.get("goal"),
        "total_tasks": total,
        "completed_tasks": completed,
        "decisions": loaf.get("decisions").and_then(|d| d.as_array()).map(|d| d.len()).unwrap_or(0),
        "discoveries": loaf.get("discoveries").and_then(|d| d.as_array()).map(|d| d.len()).unwrap_or(0)
    }))
}

// ============================================================================
// List Sessions + Analytics
// ============================================================================

fn handle_list_sessions(server: &Server, args: Value) -> Result<Value, String> {
    let include_stalled = args
        .get("include_stalled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let session_dir = std::path::Path::new(session_dir());
    if !session_dir.exists() {
        return Ok(json!({"sessions": [], "count": 0}));
    }

    let store = server.runtime.block_on(server.tasks.read());

    let mut sessions = Vec::new();
    for entry in std::fs::read_dir(session_dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if !entry.path().is_dir() {
            continue;
        }
        let meta_path = entry.path().join("meta.json");
        if meta_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&meta_path) {
                if let Ok(meta) = serde_json::from_str::<Value>(&content) {
                    let session_id = entry.file_name().to_string_lossy().to_string();

                    // v1.2.3: Use task store for authoritative alive/pid (heartbeat updates meta.json)
                    // v1.2.7: Also track orphaned status (child alive but pipes gone after restart)
                    let (alive, orphaned, pid, last_activity) = if let Some(task) =
                        store.get(&*session_id)
                    {
                        let is_alive =
                            matches!(task.status, TaskStatus::Running | TaskStatus::Queued);
                        let is_orphaned = task.status == TaskStatus::Orphaned;
                        (is_alive, is_orphaned, task.child_pid, task.last_activity)
                    } else {
                        // Fallback to meta.json fields
                        let pid = meta.get("pid").and_then(|v| v.as_u64()).map(|p| p as u32);
                        let alive = meta.get("alive").and_then(|v| v.as_bool()).unwrap_or(false);
                        (alive, false, pid, None)
                    };

                    // v1.2.3: include_stalled filter — only show stalled sessions
                    if include_stalled {
                        let is_stalled = alive
                            && last_activity.is_none_or(|la| (Utc::now() - la).num_seconds() > 120);
                        if !is_stalled {
                            continue;
                        }
                    }

                    sessions.push(json!({
                        "session_id": session_id,
                        "alive": alive,
                        // v1.2.7: orphaned=true means child PID is still running but manager lost
                        // stdout/stderr pipes after Desktop restart. Destroy and restart to recover.
                        "orphaned": orphaned,
                        "pid": pid,
                        "model": meta.get("model"),
                        "working_dir": meta.get("working_dir"),
                        "started_at": meta.get("created_at"),
                        "last_heartbeat": meta.get("last_heartbeat"),
                        "last_activity": last_activity.map(|la| la.to_rfc3339()),
                        "prompt_preview": meta.get("prompt").and_then(|v| v.as_str()).map(|s| {
                            if s.len() > 100 { format!("{}...", &s[..100]) } else { s.to_string() }
                        }),
                    }));
                }
            }
        }
    }

    let count = sessions.len();
    Ok(json!({"sessions": sessions, "count": count}))
}

fn handle_get_analytics(server: &Server, args: Value) -> Result<Value, String> {
    let store = server.runtime.block_on(server.tasks.read());
    let backend_filter = args
        .get("backend")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let since = args
        .get("since")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // (total, success, cost, total_duration_ms)
    let mut by_backend: std::collections::HashMap<String, (u32, u32, f64, u64)> =
        std::collections::HashMap::new();
    let mut total_tasks = 0u32;
    let mut total_success = 0u32;
    let mut total_cost = 0.0f64;
    let mut total_tokens_in = 0u64;
    let mut total_tokens_out = 0u64;
    let mut recent_failures: Vec<Value> = Vec::new();

    for task in store.values() {
        let backend_str = task.backend.to_string();

        if let Some(ref bf) = backend_filter {
            if backend_str != *bf {
                continue;
            }
        }
        if let Some(ref since_str) = since {
            if let Ok(since_dt) = chrono::DateTime::parse_from_rfc3339(since_str) {
                if task.created_at < since_dt {
                    continue;
                }
            }
        }

        total_tasks += 1;
        total_cost += task.cost_usd;
        total_tokens_in += task.input_tokens;
        total_tokens_out += task.output_tokens;

        let is_success = task.status == TaskStatus::Done;
        if is_success {
            total_success += 1;
        }

        let entry = by_backend
            .entry(backend_str.clone())
            .or_insert((0, 0, 0.0, 0));
        entry.0 += 1;
        if is_success {
            entry.1 += 1;
        }
        entry.2 += task.cost_usd;

        // Calculate duration from started_at -> completed_at
        if let (Some(started), Some(completed)) = (&task.started_at, &task.completed_at) {
            let duration_ms = (*completed - *started).num_milliseconds().max(0) as u64;
            entry.3 += duration_ms;
        }

        if task.status == TaskStatus::Failed && recent_failures.len() < 5 {
            recent_failures.push(json!({
                "task_id": task.id,
                "backend": backend_str,
                "error_preview": task.error.as_deref().map(|s| {
                    if s.len() > 150 { format!("{}...", &s[..150]) } else { s.to_string() }
                }),
            }));
        }
    }

    let backend_stats: Vec<Value> = by_backend.iter().map(|(backend, (total, success, cost, duration_ms))| {
        json!({
            "backend": backend,
            "total_tasks": total,
            "successful": success,
            "success_rate": if *total > 0 { format!("{:.0}%", (*success as f64 / *total as f64) * 100.0) } else { "N/A".to_string() },
            "total_cost_usd": format!("{:.4}", cost),
            "avg_duration_secs": if *total > 0 { format!("{:.1}", (*duration_ms as f64 / *total as f64) / 1000.0) } else { "N/A".to_string() },
        })
    }).collect();

    Ok(json!({
        "total_tasks": total_tasks,
        "total_successful": total_success,
        "overall_success_rate": if total_tasks > 0 { format!("{:.0}%", (total_success as f64 / total_tasks as f64) * 100.0) } else { "N/A".to_string() },
        "total_cost_usd": format!("{:.4}", total_cost),
        "total_tokens": {"input": total_tokens_in, "output": total_tokens_out},
        "by_backend": backend_stats,
        "recent_failures": recent_failures,
    }))
}

// ============================================================================
// §12a: Nightly Analyzer
// ============================================================================

fn handle_run_analyzer(args: Value) -> Result<Value, String> {
    let vd = volumes_dir();
    let volumes = args
        .get("volumes_path")
        .and_then(|v| v.as_str())
        .unwrap_or(&vd)
        .to_string();
    let history = args
        .get("history_path")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("{}\\task_history.json", history_dir()));
    analyzer::run_analyzer(&volumes, &history)
}

// ============================================================================
// §12: Specialist Role Prompts
// ============================================================================

fn get_role_prompt(role: &str) -> Option<&'static str> {
    match role {
        "architect" => Some(
            "You are a software architect. Focus on system design, component boundaries, \
             data flow, and API contracts. Evaluate trade-offs between approaches. \
             Produce diagrams or pseudocode, not full implementations. \
             Flag coupling risks and scalability concerns.",
        ),
        "implementer" => Some(
            "You are an implementer. Write production-quality code that follows existing \
             patterns in the codebase. Keep changes minimal and focused. \
             Run builds and tests after changes. Report what files you modified.",
        ),
        "tester" => Some(
            "You are a test engineer. Write thorough tests covering happy paths, edge cases, \
             and error conditions. Prefer integration tests over mocks. \
             Report coverage gaps and suggest additional test scenarios.",
        ),
        "reviewer" => Some(
            "You are a code reviewer. Read the code carefully and identify bugs, security \
             issues, performance problems, and style violations. Be specific about line \
             numbers and suggest concrete fixes. Prioritize findings by severity.",
        ),
        "documenter" => Some(
            "You are a technical writer. Write clear, concise documentation for the code \
             and systems you examine. Produce READMEs, inline comments, API docs, and \
             architecture decision records as appropriate. Target the intended audience.",
        ),
        "debugger" => Some(
            "You are a debugger. Systematically narrow down the root cause of issues. \
             Add logging, inspect state, form hypotheses, and test them. \
             Document the investigation path so others can follow your reasoning.",
        ),
        "security" => Some(
            "You are a security analyst. Review code for OWASP top 10 vulnerabilities, \
             injection risks, authentication/authorization flaws, and data exposure. \
             Check dependencies for known CVEs. Report findings with severity ratings.",
        ),
        _ => None,
    }
}

fn list_roles() -> Vec<Value> {
    vec![
        json!({"name": "architect", "description": "System design, component boundaries, trade-off analysis"}),
        json!({"name": "implementer", "description": "Production-quality code following existing patterns"}),
        json!({"name": "tester", "description": "Thorough test coverage: happy paths, edge cases, error conditions"}),
        json!({"name": "reviewer", "description": "Code review: bugs, security, performance, style"}),
        json!({"name": "documenter", "description": "Technical writing: READMEs, API docs, ADRs"}),
        json!({"name": "debugger", "description": "Systematic root cause analysis and investigation"}),
        json!({"name": "security", "description": "Security audit: OWASP, injection, auth, CVEs"}),
    ]
}

// Custom YAML role support
#[derive(Deserialize)]
struct CustomRole {
    name: String,
    prompt: String,
    #[serde(default)]
    expertise: Vec<String>,
}

fn custom_roles_dir() -> std::path::PathBuf {
    let volumes = volumes_dir();
    std::path::PathBuf::from(volumes)
        .join("scripts")
        .join("roles")
}

fn load_custom_roles() -> Vec<CustomRole> {
    let dir = custom_roles_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return vec![];
    };
    entries
        .filter_map(|e| {
            let e = e.ok()?;
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("yaml") {
                return None;
            }
            let content = std::fs::read_to_string(&path).ok()?;
            serde_yaml::from_str::<CustomRole>(&content).ok()
        })
        .collect()
}

fn get_custom_role_prompt(role: &str) -> Option<String> {
    let path = custom_roles_dir().join(format!("{}.yaml", role));
    let content = std::fs::read_to_string(&path).ok()?;
    let cr: CustomRole = serde_yaml::from_str(&content).ok()?;
    Some(cr.prompt)
}

fn handle_role_list(_args: Value) -> Result<Value, String> {
    let mut roles = list_roles();
    let custom = load_custom_roles();
    let custom_count = custom.len();
    for cr in custom {
        // Custom roles override built-in with same name
        roles.retain(|r| r.get("name").and_then(|n| n.as_str()) != Some(&cr.name));
        roles.push(json!({
            "name": cr.name,
            "description": cr.expertise.join(", "),
            "custom": true,
        }));
    }
    let count = roles.len();
    Ok(
        json!({ "roles": roles, "count": count, "built_in": 7, "custom": custom_count, "note": "Pass role name to task_submit's 'role' parameter" }),
    )
}

fn handle_role_create(args: Value) -> Result<Value, String> {
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("Missing required param: name")?;
    let prompt = args
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or("Missing required param: prompt")?;
    let expertise: Vec<String> = args
        .get("expertise")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // Validate name (alphanumeric + underscore only)
    if !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return Err("Role name must be alphanumeric/underscore only".into());
    }

    let dir = custom_roles_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create roles dir: {}", e))?;

    let yaml_content = format!(
        "name: {}\nprompt: |\n{}\nexpertise:\n{}",
        name,
        prompt
            .lines()
            .map(|l| format!("  {}", l))
            .collect::<Vec<_>>()
            .join("\n"),
        expertise
            .iter()
            .map(|e| format!("  - {}", e))
            .collect::<Vec<_>>()
            .join("\n"),
    );

    let path = dir.join(format!("{}.yaml", name));
    std::fs::write(&path, &yaml_content).map_err(|e| format!("Failed to write role: {}", e))?;
    info!("Custom role created: {}", name);
    Ok(json!({ "created": name, "path": path.display().to_string() }))
}

fn handle_role_delete(args: Value) -> Result<Value, String> {
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("Missing required param: name")?;
    let path = custom_roles_dir().join(format!("{}.yaml", name));
    if !path.exists() {
        return Err(format!("Custom role '{}' not found", name));
    }
    std::fs::remove_file(&path).map_err(|e| format!("Failed to delete: {}", e))?;
    info!("Custom role deleted: {}", name);
    Ok(json!({ "deleted": name }))
}

// ============================================================================
// §13: Auto-Artifact Saving
// ============================================================================

fn save_task_artifact(task: &Task) {
    if !task.save_artifact {
        return;
    }
    if task.status != TaskStatus::Done {
        return;
    }

    let volumes_path = volumes_dir();
    let artifacts_dir = std::path::Path::new(&volumes_path).join("artifacts");
    if std::fs::create_dir_all(&artifacts_dir).is_err() {
        return;
    }

    let date = Utc::now().format("%Y-%m-%d").to_string();
    let role_tag = task.role.as_deref().unwrap_or("none");
    let filename = format!("{}_{}_{}_{}.md", date, task.id, task.backend, role_tag);
    let path = artifacts_dir.join(&filename);

    let prompt_preview: String = task.prompt.chars().take(200).collect();
    let content = format!(
        "# Task Artifact: {}\n\
         - Backend: {}\n\
         - Role: {}\n\
         - Date: {}\n\
         - Prompt: {}\n\
         ---\n\n{}\n",
        task.id, task.backend, role_tag, date, prompt_preview, task.output
    );

    let _ = std::fs::write(&path, &content);
    info!("Artifact saved: {}", path.display());
}

// ============================================================================
// Tool Dispatch
// ============================================================================

fn handle_tool_call(server: &Server, tool: &str, args: Value) -> Result<Value, String> {
    match tool {
        "task_submit" | "submit_task" => handle_submit_task(server, args),
        "task_status" | "get_status" => handle_get_status(server, args),
        "task_watch" | "watch_tasks" => handle_watch_tasks(server, args),
        "task_output" | "get_output" => handle_get_output(server, args),
        "task_list" | "list_tasks" => handle_list_tasks(server, args),
        "task_cancel" | "cancel_task" => handle_cancel_task(server, args),
        "task_poll" => handle_task_poll(server, args),
        "status_bar" => handle_status_bar(server, args),
        "pause_task" => handle_pause_task(server, args),
        "resume_task" => handle_resume_task(server, args),
        "configure" => handle_configure(server, args),
        "task_cleanup" | "cleanup" => handle_cleanup(server, args),
        "session_start" | "start_session" => handle_start_session(server, args),
        "send" | "task_send" => handle_send(server, args),
        "session_send" | "send_to_session" => handle_send(server, args),
        "open_terminal" => handle_open_terminal(args),
        "gemini_direct" => handle_gemini_direct(args),
        "codex_exec" => handle_codex_exec(args),
        "codex_review" => handle_codex_review(args),
        "workflow_run" | "run_workflow" => handle_run_workflow(server, args),
        "task_run_parallel" | "run_parallel" => handle_run_parallel(server, args),
        "review_extractions" => handle_review_extractions(server, args),
        "extract_workflow" => handle_extract_workflow(server, args),
        "dismiss_extraction" => handle_dismiss_extraction(server, args),
        "task_rollback" | "rollback_task" => handle_rollback_task(server, args),
        "task_retry" | "retry_task" => handle_retry_task(server, args),
        "task_rerun" | "rerun_task" => handle_task_rerun(server, args),
        "task_route" | "route_task" => handle_route_task(args),
        "task_decompose" | "decompose_task" => handle_decompose_task(args),
        "template_save" | "save_template" => handle_save_template(args),
        "template_list" | "list_templates" => handle_list_templates(args),
        "template_run" | "run_template" => handle_run_template(server, args),
        "task_explain" | "explain_task" => handle_explain_task(server, args),
        "create_loaf" => handle_loaf_create(server, args),
        "loaf_update" => handle_loaf_update(server, args),
        "loaf_status" => handle_loaf_status(server, args),
        "loaf_close" => handle_loaf_close(server, args),
        "session_list" | "list_sessions" => handle_list_sessions(server, args),
        "session_destroy" | "destroy_session" => {
            // Fire notify_on_destroy before cancel (preserves session notification behavior)
            if let Some(sid) = args.get("session_id").and_then(|v| v.as_str()) {
                let meta_file = format!("{}\\{}\\meta.json", session_dir(), sid);
                if let Ok(content) = std::fs::read_to_string(&meta_file) {
                    if let Ok(meta) = serde_json::from_str::<Value>(&content) {
                        if meta
                            .get("notify_on_destroy")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                        {
                            fire_notify_for_session(
                                sid,
                                meta.get("created_at").and_then(|v| v.as_str()),
                                meta.get("notify_title").and_then(|v| v.as_str()),
                                meta.get("notify_body").and_then(|v| v.as_str()),
                                NotifyReason::Destroyed,
                                server.notifier.as_ref(),
                            );
                        }
                    }
                }
                // Update meta.json alive flag
                let meta_file2 = format!("{}\\{}\\meta.json", session_dir(), sid);
                if let Ok(content) = std::fs::read_to_string(&meta_file2) {
                    if let Ok(mut meta) = serde_json::from_str::<Value>(&content) {
                        meta["alive"] = json!(false);
                        let _ = std::fs::write(
                            &meta_file2,
                            serde_json::to_string_pretty(&meta).unwrap_or_default(),
                        );
                    }
                }
            }
            // Alias: map session_id -> task_id for cancel
            let mut mapped = args.clone();
            if let Some(sid) = args.get("session_id").cloned() {
                mapped["task_id"] = sid;
            }
            handle_cancel_task(server, mapped)
        }
        "get_analytics" => handle_get_analytics(server, args),
        "run_analyzer" => handle_run_analyzer(args),
        "role_list" | "list_roles" => handle_role_list(args),
        "role_create" | "create_role" => handle_role_create(args),
        "role_delete" | "delete_role" => handle_role_delete(args),
        "notify" => Ok(handle_notify(args)),
        "dashboard_open" => Ok(handle_dashboard_open()),
        "dashboard_stop" => Ok(handle_dashboard_stop()),
        "dashboard_status" => Ok(handle_dashboard_status()),
        _ => Err(format!("Unknown tool: {}", tool)),
    }
}

fn handle_dashboard_open() -> Value {
    let port = DASHBOARD_PORT.load(Ordering::Relaxed);
    if port == 0 {
        return json!({"error": "Dashboard is not running. It starts automatically with the manager server."});
    }
    let url = format!("http://127.0.0.1:{}/", port);
    // Launch browser via start command
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", &url])
        .spawn();
    json!({"opened": true, "url": url, "port": port})
}

fn handle_dashboard_stop() -> Value {
    let handle = DASHBOARD_ABORT.lock().unwrap().take();
    match handle {
        Some(h) => {
            h.abort();
            DASHBOARD_RUNNING.store(false, Ordering::SeqCst);
            DASHBOARD_PORT.store(0, Ordering::SeqCst);
            json!({"stopped": true, "message": "Dashboard listener aborted."})
        }
        None => {
            json!({"stopped": false, "message": "Dashboard was not running or already stopped."})
        }
    }
}

fn handle_dashboard_status() -> Value {
    let running = DASHBOARD_RUNNING.load(Ordering::Relaxed);
    let port = DASHBOARD_PORT.load(Ordering::Relaxed);
    let url = if running && port > 0 {
        format!("http://127.0.0.1:{}/", port)
    } else {
        String::new()
    };
    json!({
        "running": running,
        "port":    port,
        "url":     url,
    })
}

// ============================================================================
// Item 13/14: Extraction Tools
// ============================================================================

fn handle_review_extractions(server: &Server, _args: Value) -> Result<Value, String> {
    let store = server.runtime.block_on(server.tasks.read());
    let pending: Vec<Value> = store.values()
        .filter(|t| t.extraction_status == ExtractionStatus::PendingSuccess || t.extraction_status == ExtractionStatus::PendingFailure)
        .map(|t| {
            let prompt_summary: String = safe_truncate(&t.prompt, 200);
            let steps_detail: Vec<Value> = t.steps.iter().map(|s| json!({"tool": s.tool, "status": s.status, "summary": s.summary.as_deref().unwrap_or("")})).collect();
            json!({
                "task_id": t.id, "backend": t.backend,
                "extraction_type": if t.extraction_status == ExtractionStatus::PendingSuccess { "success_pattern" } else { "failure_anti_pattern" },
                "prompt": prompt_summary,
                "output_preview": safe_truncate(&t.output, 300),
                "error": t.error, "step_count": t.steps.len(), "steps": steps_detail,
                "duration_secs": t.started_at.and_then(|s| t.completed_at.map(|c| (c - s).num_seconds())),
            })
        }).collect();
    Ok(
        json!({"pending_count": pending.len(), "candidates": pending, "instructions": "For each: 3Q check (Reusable? Specific? New?). Yes=extract_workflow. No=dismiss_extraction."}),
    )
}

fn handle_extract_workflow(server: &Server, args: Value) -> Result<Value, String> {
    let task_id = args
        .get("task_id")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'task_id'")?;
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'name'")?;
    let description = args
        .get("description")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'description'")?;
    let pattern_type = args
        .get("pattern_type")
        .and_then(|v| v.as_str())
        .unwrap_or("workflow");
    let mut store = server.runtime.block_on(server.tasks.write());
    let task = store
        .get_mut(task_id)
        .ok_or(format!("Task '{}' not found", task_id))?;
    let steps_summary: Vec<String> = task
        .steps
        .iter()
        .map(|s| {
            let summary = s.summary.as_deref().unwrap_or("");
            format!("{}: {}", s.tool, safe_truncate(summary, 100))
        })
        .collect();
    let pattern = json!({"name": name, "description": description, "pattern_type": pattern_type, "steps": steps_summary, "backend": task.backend, "source_task_id": task.id, "original_prompt": task.prompt, "error": task.error, "extracted_at": Utc::now().to_rfc3339(), "times_used": 0, "success_rate": if task.status == TaskStatus::Done { 1.0_f64 } else { 0.0_f64 }});
    let _ = std::fs::create_dir_all(workflow_patterns_dir());
    let pattern_path = format!("{}\\{}.json", workflow_patterns_dir(), name);
    let data = serde_json::to_string_pretty(&pattern).map_err(|e| e.to_string())?;
    std::fs::write(&pattern_path, &data).map_err(|e| format!("Failed to write: {}", e))?;
    task.extraction_status = ExtractionStatus::Extracted;
    Server::persist_task(task);
    Ok(
        json!({"status": "extracted", "pattern_name": name, "pattern_path": pattern_path, "pattern_type": pattern_type}),
    )
}

fn handle_dismiss_extraction(server: &Server, args: Value) -> Result<Value, String> {
    let task_id = args
        .get("task_id")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'task_id'")?;
    let reason = args
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("not extractable");
    let mut store = server.runtime.block_on(server.tasks.write());
    let task = store
        .get_mut(task_id)
        .ok_or(format!("Task '{}' not found", task_id))?;
    task.extraction_status = ExtractionStatus::Dismissed;
    Server::persist_task(task);
    Ok(json!({"status": "dismissed", "task_id": task_id, "reason": reason}))
}

fn handle_rollback_task(server: &Server, args: Value) -> Result<Value, String> {
    let task_id = args
        .get("task_id")
        .and_then(|v| v.as_str())
        .ok_or("Missing task_id")?;
    let store = server.runtime.block_on(server.tasks.read());
    let task = store.get(task_id).ok_or("Task not found")?;
    let restored = Server::rollback(task)?;
    Ok(json!({"rolled_back": true, "files_restored": restored, "task_id": task_id}))
}

/// Item 18: Manually trigger a retry for a failed task.
fn handle_retry_task(server: &Server, args: Value) -> Result<Value, String> {
    let task_id = args
        .get("task_id")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'task_id'")?;
    let inject_context = args.get("inject_context").and_then(|v| v.as_str());

    let mut store = server.runtime.block_on(server.tasks.write());
    let task = store
        .get_mut(task_id)
        .ok_or(format!("Task '{}' not found", task_id))?;

    if task.status != TaskStatus::Failed {
        return Err(format!(
            "Task '{}' is {} - can only retry failed tasks",
            task_id, task.status
        ));
    }

    // Override max_retries to allow manual retry even if limit reached
    task.max_retries = task.retry_count + 1;

    // Inject extra context if provided
    if let Some(ctx) = inject_context {
        let current_error = task.error.clone().unwrap_or_default();
        task.error = Some(format!("{}\n\nAdditional context: {}", current_error, ctx));
    }

    let retry = Server::prepare_retry(task).ok_or("Failed to prepare retry")?;

    let retry_id = retry.id.clone();
    let retry_backend = retry.backend.to_string();
    Server::persist_task(task);

    store.insert(retry_id.clone(), retry.clone());
    Server::persist_task(&retry);
    drop(store);

    spawn_retry_execution(
        &retry,
        server.tasks.clone(),
        Some(server.config.clone()),
        &server.runtime,
    );

    Ok(json!({
        "retry_task_id": retry_id,
        "original_task_id": task_id,
        "backend": retry_backend,
        "status": "queued",
        "message": format!("Retry task {} created from failed task {}", retry_id, task_id)
    }))
}

// ============================================================================
// Task Rerun — re-submit a completed task with optional new context
// ============================================================================

fn handle_task_rerun(server: &Server, args: Value) -> Result<Value, String> {
    let task_id = args
        .get("task_id")
        .and_then(|v| v.as_str())
        .ok_or("Missing required 'task_id'")?;
    let additional_context = args.get("additional_context").and_then(|v| v.as_str());
    let backend_override = args.get("backend_override").and_then(|v| v.as_str());
    let include_files: Vec<String> = args
        .get("include_files")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // Look up original task
    let store = server.runtime.block_on(server.tasks.read());
    let original = store
        .get(task_id)
        .ok_or(format!("Task '{}' not found", task_id))?;

    if original.status != TaskStatus::Done {
        return Err(format!(
            "Task '{}' is {} — task_rerun requires a completed (done) task. Use task_retry for failed tasks.",
            task_id, original.status
        ));
    }

    // Capture what we need from the original before dropping the lock
    let original_prompt = original.prompt.clone();
    let original_backend = if let Some(ovr) = backend_override {
        match ovr {
            "gpt" => Backend::Gpt,
            "gemini" => Backend::Gemini,
            "claude_code" | "claude" => Backend::ClaudeCode,
            "codex" => Backend::Codex,
            _ => {
                return Err(format!(
                    "Unknown backend_override '{}'. Use: gpt, gemini, claude_code, codex",
                    ovr
                ))
            }
        }
    } else {
        original.backend.clone()
    };
    let original_system_prompt = original.system_prompt.clone();
    let original_model = original.model.clone();
    let original_working_dir = original.working_dir.clone();
    let original_role = original.role.clone();
    let original_on_complete = original.on_complete.clone();
    let original_task_id = original.id.clone();
    drop(store);

    // Build file contents section
    let mut file_section = String::new();
    let mut files_loaded: usize = 0;
    let mut files_skipped: Vec<String> = Vec::new();
    for path in &include_files {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                file_section.push_str(&format!(
                    "\n\n--- Current state of {} ---\n{}\n--- End {} ---",
                    path, contents, path
                ));
                files_loaded += 1;
            }
            Err(e) => {
                eprintln!("task_rerun: skipping missing file {}: {}", path, e);
                files_skipped.push(path.clone());
            }
        }
    }

    // Construct new prompt
    let mut new_prompt = original_prompt;
    if !file_section.is_empty() {
        new_prompt.push_str(&file_section);
    }
    if let Some(ctx) = additional_context {
        new_prompt.push_str(&format!("\n\n[Additional context for rerun]\n{}", ctx));
    }

    // Create new task via internal submit
    let new_task_id = Uuid::new_v4().to_string()[..8].to_string();
    let new_task = Task {
        id: new_task_id.clone(),
        backend: original_backend.clone(),
        prompt: new_prompt,
        system_prompt: original_system_prompt,
        model: original_model,
        working_dir: original_working_dir.clone(),
        status: TaskStatus::Queued,
        output: String::new(),
        error: None,
        created_at: Utc::now(),
        started_at: None,
        completed_at: None,
        progress_lines: 0,
        steps: Vec::new(),
        last_activity: None,
        last_output_chunk_at: None,
        stall_detected: false,
        extraction_status: ExtractionStatus::None,
        trust_score: 0,
        trust_level: TrustLevel::Low,
        rollback_path: None,
        validation_status: ValidationStatus::NotChecked,
        assertions: Vec::new(),
        backed_up_files: Vec::new(),
        retry_count: 0,
        max_retries: 2,
        retry_of: None,
        error_context: None,
        input_tokens: 0,
        output_tokens: 0,
        cost_usd: 0.0,
        on_complete: original_on_complete,
        role: original_role,
        save_artifact: true,
        rerun_of: Some(original_task_id.clone()),
        parent_task_id: Some(original_task_id.clone()),
        forked_from: None,
        continuation_of: None,
        child_pid: None,
        log_path: None,
        log_offset: 0,
        reconnected: false,
        watchdog_observations: Vec::new(),
        fingerprint: None,
        superseded_by: None,
        label: None,
        current_step: None,
        total_steps: None,
        current_step_desc: None,
        live_activity: None,
        effort: None,
        notify_on_complete: None,
        notify_on_fail: None,
        notify_on_destroy: None,
        notify_title: None,
        notify_body: None,
    };

    // Store and persist
    let tasks = server.tasks.clone();
    let config = server.config.clone();
    server.runtime.block_on(async {
        let mut store = tasks.write().await;
        store.insert(new_task_id.clone(), new_task.clone());
    });
    Server::persist_task(&new_task);

    // Spawn execution (same pattern as task_submit)
    let tasks_bg = server.tasks.clone();
    let tid = new_task_id.clone();
    let be = original_backend.clone();
    let prompt_for_spawn = new_task.prompt.clone();
    let model_for_spawn = new_task.model.clone();
    let rerun_pipes = Some(server.stdin_pipes.clone());
    match be {
        Backend::Gpt => {
            server.runtime.spawn(run_gpt_task(config, tasks_bg, tid));
        }
        Backend::ClaudeCode => {
            let mut args = vec![
                "-p".to_string(),
                prompt_for_spawn,
                "--dangerously-skip-permissions".to_string(),
                "--verbose".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--add-dir".to_string(),
                r"C:\temp".to_string(),
                "--add-dir".to_string(),
                volumes_dir(),
                "--add-dir".to_string(),
                r"C:\CPC".to_string(),
                "--add-dir".to_string(),
                workspace_root(),
            ];
            if let Some(m) = model_for_spawn {
                args.push("--model".to_string());
                args.push(m);
            }
            if let Some(ref wd) = original_working_dir {
                args.push("--add-dir".to_string());
                args.push(wd.clone());
            }
            server.runtime.spawn(run_cli_task(
                tasks_bg,
                tid,
                claude_code_cmd(),
                args,
                rerun_pipes.clone(),
                StdinMode::Null,
            ));
        }
        Backend::Gemini => {
            let mut args = vec![
                gemini_cmd().to_string(),
                "-p".to_string(),
                prompt_for_spawn,
                "--yolo".to_string(),
            ];
            if let Some(m) = model_for_spawn {
                args.push("--model".to_string());
                args.push(m);
            }
            server.runtime.spawn(run_cli_task(
                tasks_bg,
                tid,
                r"C:\Program Files\nodejs\node.exe",
                args,
                rerun_pipes.clone(),
                StdinMode::Null,
            ));
        }
        Backend::Codex => {
            let wd = original_working_dir.unwrap_or_else(workspace_root);
            let args = vec![
                "exec".into(),
                "--json".into(),
                "--skip-git-repo-check".into(),
                "--full-auto".into(),
                "--cd".into(),
                wd.clone(),
                "--".into(),
                prompt_for_spawn,
            ];
            server
                .runtime
                .spawn(run_codex_task(tasks_bg, tid, args, wd));
        }
    }

    Ok(json!({
        "new_task_id": new_task_id,
        "rerun_of": original_task_id,
        "backend": original_backend.to_string(),
        "status": "queued",
        "include_files_loaded": files_loaded,
        "include_files_skipped": files_skipped,
        "has_additional_context": additional_context.is_some(),
        "message": format!("Rerun task {} created from completed task {}", new_task_id, original_task_id)
    }))
}

// ============================================================================
// Item 16: Task Routing
// ============================================================================

fn handle_route_task(args: Value) -> Result<Value, String> {
    let prompt = args
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'prompt'")?;
    let working_dir = args.get("working_dir").and_then(|v| v.as_str());
    let rec = Server::recommend_backend(prompt, working_dir);
    Ok(json!({
        "recommended_backend": rec.recommended_backend,
        "confidence": rec.confidence,
        "reasoning": rec.reasoning,
        "alternatives": rec.alternatives,
    }))
}

// ============================================================================
// Phase C fix3: Notify — Windows toast notification
// ============================================================================

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

fn handle_notify(args: Value) -> Value {
    let title = args
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let body = args
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let icon = args.get("icon").and_then(|v| v.as_str()).unwrap_or("info");
    let duration_ms = args
        .get("duration_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(5000)
        .max(1);

    if title.is_empty() || body.is_empty() {
        return json!({"error": "Both title and body are required"});
    }
    if !matches!(icon, "info" | "warning" | "error") {
        return json!({"error": "icon must be one of: info, warning, error"});
    }

    let display_title = match icon {
        "warning" => format!("[Warning] {}", title),
        "error" => format!("[Error] {}", title),
        _ => format!("[Info] {}", title),
    };
    let toast_duration = if duration_ms > 7_000 { "long" } else { "short" };

    let script = r#"
$ErrorActionPreference = 'Stop'
if (Get-Command New-BurntToastNotification -ErrorAction SilentlyContinue) {
    New-BurntToastNotification -Text $env:MCP_NOTIFY_TITLE, $env:MCP_NOTIFY_BODY -Silent | Out-Null
    Write-Output 'burnttoast'
    return
}
Add-Type -AssemblyName System.Runtime.WindowsRuntime | Out-Null
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] > $null
[Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] > $null
$titleEscaped = [System.Security.SecurityElement]::Escape($env:MCP_NOTIFY_TITLE)
$bodyEscaped = [System.Security.SecurityElement]::Escape($env:MCP_NOTIFY_BODY)
$toastDuration = if ([int]$env:MCP_NOTIFY_DURATION_MS -gt 7000) { 'long' } else { 'short' }
$xml = @"
<toast duration="$toastDuration">
  <visual>
    <binding template="ToastGeneric">
      <text>$titleEscaped</text>
      <text>$bodyEscaped</text>
    </binding>
  </visual>
  <audio silent="true"/>
</toast>
"@
$doc = [Windows.Data.Xml.Dom.XmlDocument]::new()
$doc.LoadXml($xml)
$toast = [Windows.UI.Notifications.ToastNotification]::new($doc)
$appId = '{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe'
try {
    [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier($appId).Show($toast)
} catch {
    [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier().Show($toast)
}
Write-Output 'winrt'
"#;

    let mut cmd = std::process::Command::new("powershell");
    cmd.args(["-NoProfile", "-Command", script])
        .env("MCP_NOTIFY_TITLE", &display_title)
        .env("MCP_NOTIFY_BODY", &body)
        .env("MCP_NOTIFY_DURATION_MS", duration_ms.to_string());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    match cmd.output() {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let backend = stdout.lines().last().unwrap_or("powershell").trim();
            if output.status.success() {
                json!({
                    "success": true,
                    "backend": backend,
                    "title": display_title,
                    "body": body,
                    "icon": icon,
                    "duration_ms": duration_ms,
                    "toast_duration": toast_duration,
                    "silent": true
                })
            } else {
                json!({"error": stderr.trim(), "stdout": stdout.trim()})
            }
        }
        Err(e) => json!({"error": format!("{}", e)}),
    }
}

// ============================================================================
// Notification infrastructure — v1.2.6
// ============================================================================

/// Core notify logic extracted from handle_notify. Returns Err on failure.
fn do_notify(title: &str, body: &str, icon: &str, duration_ms: u64) -> Result<(), String> {
    let display_title = match icon {
        "warning" => format!("[Warning] {}", title),
        "error" => format!("[Error] {}", title),
        _ => format!("[Info] {}", title),
    };

    let script = r#"
$ErrorActionPreference = 'Stop'
if (Get-Command New-BurntToastNotification -ErrorAction SilentlyContinue) {
    New-BurntToastNotification -Text $env:MCP_NOTIFY_TITLE, $env:MCP_NOTIFY_BODY -Silent | Out-Null
    return
}
Add-Type -AssemblyName System.Runtime.WindowsRuntime | Out-Null
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] > $null
[Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] > $null
$titleEscaped = [System.Security.SecurityElement]::Escape($env:MCP_NOTIFY_TITLE)
$bodyEscaped = [System.Security.SecurityElement]::Escape($env:MCP_NOTIFY_BODY)
$toastDuration = if ([int]$env:MCP_NOTIFY_DURATION_MS -gt 7000) { 'long' } else { 'short' }
$xml = @"
<toast duration="$toastDuration">
  <visual>
    <binding template="ToastGeneric">
      <text>$titleEscaped</text>
      <text>$bodyEscaped</text>
    </binding>
  </visual>
  <audio silent="true"/>
</toast>
"@
$doc = [Windows.Data.Xml.Dom.XmlDocument]::new()
$doc.LoadXml($xml)
$toast = [Windows.UI.Notifications.ToastNotification]::new($doc)
$appId = '{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe'
try {
    [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier($appId).Show($toast)
} catch {
    [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier().Show($toast)
}
"#;

    let mut cmd = std::process::Command::new("powershell");
    cmd.args(["-NoProfile", "-Command", script])
        .env("MCP_NOTIFY_TITLE", &display_title)
        .env("MCP_NOTIFY_BODY", body)
        .env("MCP_NOTIFY_DURATION_MS", duration_ms.to_string());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    match cmd.output() {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(String::from_utf8_lossy(&output.stderr).trim().to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// Abstraction over the toast notification backend. Allows TestNotifier in unit tests.
pub trait SessionNotifier: Send + Sync + 'static {
    fn notify(&self, title: &str, body: &str) -> Result<(), String>;
    /// v1.3.5: notify with explicit icon (info/warning/error). Default impl delegates to notify()
    /// for backward-compat with TestNotifier and any external implementations.
    fn notify_with_icon(&self, title: &str, body: &str, _icon: &str) -> Result<(), String> {
        self.notify(title, body)
    }
}

/// Production notifier — fires real Windows toast via PowerShell.
pub struct RealNotifier;

impl SessionNotifier for RealNotifier {
    fn notify(&self, title: &str, body: &str) -> Result<(), String> {
        do_notify(title, body, "info", 5000)
    }
    fn notify_with_icon(&self, title: &str, body: &str, icon: &str) -> Result<(), String> {
        do_notify(title, body, icon, 5000)
    }
}

// ============================================================================
// Tools List
// ============================================================================

fn get_tools_list() -> Value {
    json!({
        "tools": [
            {
                "name": "task_submit",
                "description": "Submit a task to GPT (reasoning), Gemini CLI (coding), Claude Code (coding), or Codex (coding with safety controls). Returns task_id immediately. The task runs in background - poll with get_status. Set auto_route: true to automatically select the best backend.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "backend": {
                            "type": "string",
                            "enum": ["gpt", "gemini", "claude_code", "codex"],
                            "description": "Which AI backend: 'gpt' for OpenAI reasoning, 'gemini' for Gemini CLI, 'claude_code' for Claude Code, 'codex' for OpenAI Codex"
                        },
                        "prompt": {
                            "type": "string",
                            "description": "The task prompt / instructions"
                        },
                        "system_prompt": {
                            "type": "string",
                            "description": "Optional system prompt (GPT only)"
                        },
                        "model": {
                            "type": "string",
                            "description": "Model override. GPT: o3, gpt-4o, etc. Gemini: gemini-2.5-pro, etc. Claude Code: uses default"
                        },
                        "working_dir": {
                            "type": "string",
                            "description": "Working directory for CLI backends (Gemini, Claude Code)"
                        },
                        "visible": {
                            "type": "boolean",
                            "description": "Override terminal visibility. true = also spawn visible terminal, false = background only. Defaults to dashboard_prefs.json setting."
                        },
                        "auto_route": {
                            "type": "boolean",
                            "description": "If true and no backend specified, automatically select the best backend based on prompt analysis, history, and learned patterns."
                        },
                        "estimated_secs": {
                            "type": "integer",
                            "description": "Estimated duration in seconds (informational only, no enforcement). Surfaced in task_poll status_bar."
                        },
                        "allow_duplicate": {
                            "type": "boolean",
                            "description": "If true, skip fingerprint dedup check. Default: false."
                        },
                        "on_complete": {
                            "type": "string",
                            "description": "Prompt for a follow-up task to auto-submit when this task completes successfully. The new task inherits backend, working_dir, and model."
                        },
                        "role": {
                            "type": "string",
                            "enum": ["architect", "implementer", "tester", "reviewer", "documenter", "debugger", "security"],
                            "description": "Specialist role: injects a role-specific system prompt and sets CPC_AGENT_ROLE env var for attribution."
                        },
                        "save_artifact": {
                            "type": "boolean",
                            "description": "Save task output as a markdown artifact in Volumes/artifacts/ on completion. Default: true."
                        },
                        "effort": {
                            "type": "string",
                            "enum": ["low", "medium", "high", "max"],
                            "description": "Effort level for Claude Code tasks. Default: medium. Auto-escalated on retry."
                        },
                        "notify_on_complete": {"type": "boolean", "description": "Fire a Windows toast notification when the task completes successfully. Default false."},
                        "notify_on_fail": {"type": "boolean", "description": "Fire a Windows toast notification when the task fails (crash, non-zero exit). Default false."},
                        "notify_on_destroy": {"type": "boolean", "description": "Fire a Windows toast notification when task_cancel is called on this task. Default false."},
                        "notify_title": {"type": "string", "description": "Custom notification title. If omitted, auto-generated from task state."},
                        "notify_body": {"type": "string", "description": "Custom notification body. If omitted, auto-generated from task state."}
                    },
                    "required": ["prompt"]
                }
            },
            {
                "name": "task_status",
                "description": "Check status of a task. Returns status (queued/running/done/failed/cancelled/paused), progress, elapsed time, and output preview.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "task_id": {
                            "type": "string",
                            "description": "Task ID from submit_task"
                        }
                    },
                    "required": ["task_id"]
                }
            },
            {
                "name": "task_output",
                "description": "Get full output from a task. Use 'tail' to get only last N lines.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "task_id": {
                            "type": "string",
                            "description": "Task ID"
                        },
                        "tail": {
                            "type": "integer",
                            "description": "Only return last N lines (optional)"
                        }
                    },
                    "required": ["task_id"]
                }
            },
            {
                "name": "task_list",
                "description": "List all tasks. Filter by status or backend.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "status": {
                            "type": "string",
                            "enum": ["queued", "running", "done", "failed", "cancelled"],
                            "description": "Filter by status"
                        },
                        "backend": {
                            "type": "string",
                            "enum": ["gpt", "gemini", "claude_code", "codex"],
                            "description": "Filter by backend"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max tasks to return (default 20)"
                        },
                        "include_stalled": {
                            "type": "boolean",
                            "description": "If true, only return stalled tasks (running/queued with no activity for 120s+ and not superseded). Default: false."
                        }
                    }
                }
            },
            {
                "name": "task_cancel",
                "description": "Cancel a running or queued task.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "task_id": {
                            "type": "string",
                            "description": "Task ID to cancel"
                        }
                    },
                    "required": ["task_id"]
                }
            },
            {
                "name": "status_bar",
                "description": "One-line status summary: manager (running/queued/unclaimed), breadcrumb (active op from autonomous), loaf (active project). Returns structured fields plus a formatted one-liner. Never errors — returns 'unavailable' for unreachable sources.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "task_poll",
                "description": "Poll for task completions and running status. Returns tasks completed since a timestamp, still-running tasks, and a status_bar summary. Use instead of blocking wait.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "since": {
                            "type": "string",
                            "description": "RFC3339 timestamp. Returns tasks completed after this time. Defaults to 1 hour ago."
                        }
                    }
                }
            },
            {
                "name": "pause_task",
                "description": "Pause a running or queued task. Marks status as paused. Background process may still run but will be ignored. Use resume_task to re-queue.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "task_id": {
                            "type": "string",
                            "description": "Task ID to pause"
                        }
                    },
                    "required": ["task_id"]
                }
            },
            {
                "name": "resume_task",
                "description": "Resume a paused task. Sets status back to queued and re-spawns execution. For claude_code/codex backends, re-spawns the CLI process.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "task_id": {
                            "type": "string",
                            "description": "Task ID to resume"
                        }
                    },
                    "required": ["task_id"]
                }
            },
            {
                "name": "configure",
                "description": "View or update Manager configuration. Call with no params to see current config.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "openai_api_key": {
                            "type": "string",
                            "description": "Set OpenAI API key for GPT backend"
                        },
                        "default_gpt_model": {
                            "type": "string",
                            "description": "Default GPT model (e.g., o3, gpt-4o)"
                        },
                        "default_working_dir": {
                            "type": "string",
                            "description": "Default working directory for CLI backends"
                        }
                    }
                }
            },
            {
                "name": "task_cleanup",
                "description": "Remove completed/failed/cancelled tasks older than N days.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "older_than_days": {
                            "type": "integer",
                            "description": "Remove tasks older than this many days (default: 7)"
                        }
                    }
                }
            }
            ,{
                "name": "session_start",
                "description": "Start a persistent Claude Code session. Returns session_id. Use send_to_session for follow-ups. Deduplicates by fingerprint (rejects healthy duplicates, overrides stalled). Heartbeat tracks pid/alive every 30s.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "prompt": {"type": "string", "description": "Initial prompt"},
                        "working_dir": {"type": "string", "description": "Working directory"},
                        "model": {"type": "string", "description": "Model: sonnet, opus, haiku"},
                        "allow_duplicate": {"type": "boolean", "description": "Skip fingerprint dedup check. Default: false."},
                        "notify_on_complete": {"type": "boolean", "description": "Fire a Windows toast notification when the session ends normally. Default false."},
                        "notify_on_fail": {"type": "boolean", "description": "Fire a Windows toast notification when the session dies unexpectedly (crash, heartbeat timeout). Default false."},
                        "notify_on_destroy": {"type": "boolean", "description": "Fire a Windows toast notification when session_destroy is called on this session. Default false."},
                        "notify_title": {"type": "string", "description": "Custom notification title. If omitted, auto-generated from session state."},
                        "notify_body": {"type": "string", "description": "Custom notification body. If omitted, auto-generated from session state."},
                        "effort": {"type": "string", "enum": ["low", "medium", "high", "max"], "description": "Effort level. Default: medium."}
                    },
                    "required": ["prompt"]
                }
            },
            {
                "name": "session_send",
                "description": "Send follow-up to an existing Claude Code session. Continues the conversation.",
                "inputSchema": {"type": "object", "properties": {"session_id": {"type": "string"}, "message": {"type": "string"}}, "required": ["session_id", "message"]}
            },
            {
                "name": "send",
                "description": "Send a follow-up message to a running task via stdin pipe. Works with any task_id or session_id. The task must be running and have an active stdin pipe.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "task_id": {"type": "string", "description": "Task ID or session ID to send to"},
                        "message": {"type": "string", "description": "Message to send"}
                    },
                    "required": ["task_id", "message"]
                }
            },
            {
                "name": "open_terminal",
                "description": "Open Claude Code in a visible terminal window for interactive use. Fire-and-forget.",
                "inputSchema": {"type": "object", "properties": {"prompt": {"type": "string", "description": "Optional initial prompt"}, "working_dir": {"type": "string"}}}
            },
            {
                "name": "gemini_direct",
                "description": "Send a one-shot query to Gemini CLI. No task queue - returns result directly.",
                "inputSchema": {"type": "object", "properties": {"prompt": {"type": "string"}, "model": {"type": "string", "description": "Model: gemini-2.5-pro, gemini-2.5-flash, etc."}, "working_dir": {"type": "string"}}, "required": ["prompt"]}
            },
            {
                "name": "codex_exec",
                "description": "Run OpenAI Codex non-interactively. Supports sandbox modes and model selection. Returns structured output.",
                "inputSchema": {"type": "object", "properties": {"prompt": {"type": "string", "description": "Task instructions for Codex"}, "model": {"type": "string", "description": "Model: o3, o4-mini, etc."}, "sandbox": {"type": "string", "description": "Sandbox mode: read-only, workspace-write, danger-full-access"}, "working_dir": {"type": "string"}, "full_auto": {"type": "boolean", "description": "Low-friction automatic execution (sandbox: workspace-write)"}, "skip_approvals": {"type": "boolean", "description": "DANGEROUS: Skip all approvals and sandbox. Use only in safe environments."}}, "required": ["prompt"]}
            },
            {
                "name": "codex_review",
                "description": "Run OpenAI Codex code review. Reviews uncommitted changes or changes against a base branch.",
                "inputSchema": {"type": "object", "properties": {"prompt": {"type": "string", "description": "Custom review instructions"}, "base": {"type": "string", "description": "Review changes against this base branch"}, "uncommitted": {"type": "boolean", "description": "Review staged, unstaged, and untracked changes"}, "commit": {"type": "string", "description": "Review changes introduced by this commit SHA"}, "working_dir": {"type": "string"}}}
            },
            {
                "name": "workflow_run",
                "description": "Execute a multi-step workflow with task chaining, retry logic, and backend escalation. Each step runs on a specified AI backend. Output from one step feeds into the next. Failed steps retry then escalate to alternative backends.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Workflow name for tracking"},
                        "steps": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "id": {"type": "string"},
                                    "backend": {"type": "string", "enum": ["claude_code", "codex", "gemini"]},
                                    "prompt": {"type": "string", "description": "Task prompt. Use {{previous_output}} to reference prior step output"},
                                    "working_dir": {"type": "string"},
                                    "on_success": {"type": "string", "description": "Next step ID on success"},
                                    "max_retries": {"type": "integer", "description": "Max retries for this step (default 2)"},
                                    "timeout_secs": {"type": "integer", "description": "Timeout per attempt in seconds (default 300)"},
                                    "alternatives": {"type": "array", "items": {"type": "string"}, "description": "Alternative backend order if primary fails"}
                                },
                                "required": ["id", "backend", "prompt"]
                            }
                        },
                        "max_total_attempts": {"type": "integer", "description": "Global attempt limit across all steps (default 15)"},
                        "log_results": {"type": "boolean", "description": "Write results to Volumes inbox"}
                    },
                    "required": ["name", "steps"]
                }
            },
            {
                "name": "task_run_parallel",
                "description": "Run multiple tasks in parallel with dependency gates. Steps with depends_on wait for their dependencies. Steps in the same parallel_group start together. Use {{step_id.output}} to reference output from completed dependencies.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Workflow name for tracking"},
                        "steps": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "id": {"type": "string", "description": "Unique step identifier"},
                                    "backend": {"type": "string", "enum": ["claude_code", "codex", "gemini"], "description": "AI backend to run this step"},
                                    "prompt": {"type": "string", "description": "Task prompt. Use {{step_id.output}} to reference output from a dependency"},
                                    "working_dir": {"type": "string"},
                                    "depends_on": {"type": "array", "items": {"type": "string"}, "description": "Step IDs that must complete before this step starts"},
                                    "parallel_group": {"type": "string", "description": "Group tag — steps with same group and no unmet deps start together"},
                                    "max_retries": {"type": "integer", "description": "Max retries for this step (default 2)"},
                                    "timeout_secs": {"type": "integer", "description": "Timeout per attempt in seconds (default 300)"},
                                    "alternatives": {"type": "array", "items": {"type": "string"}, "description": "Alternative backend order if primary fails"}
                                },
                                "required": ["id", "backend", "prompt"]
                            }
                        },
                        "max_concurrent": {"type": "integer", "description": "Max simultaneous tasks (default 3)"},
                        "fail_fast": {"type": "boolean", "description": "If true, cancel remaining steps on first failure (default false)"}
                    },
                    "required": ["name", "steps"]
                }
            },
            {
                "name": "review_extractions",
                "description": "List tasks pending extraction review. Returns step trails for completed/failed tasks that may contain reusable workflow patterns. Run 3Q check on each: Reusable? Specific? New? Then call extract_workflow or dismiss_extraction.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "status_filter": {"type": "string", "description": "Filter: 'pending_success', 'pending_failure', or 'all' (default: 'all')"}
                    }
                }
            },
            {
                "name": "extract_workflow",
                "description": "Extract a reusable workflow pattern from a completed task. Saves to Volumes/manager/workflow_patterns/{name}.json. Use after review_extractions confirms the pattern passes the 3Q check.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "task_id": {"type": "string", "description": "Task ID to extract pattern from"},
                        "name": {"type": "string", "description": "Short name for the workflow pattern (e.g. 'rust-build-deploy')"},
                        "description": {"type": "string", "description": "What this workflow does and when to use it"}
                    },
                    "required": ["task_id", "name", "description"]
                }
            },
            {
                "name": "dismiss_extraction",
                "description": "Mark a task as not worth extracting. Use after review_extractions when the pattern fails the 3Q check (not reusable, not specific, or not new).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "task_id": {"type": "string", "description": "Task ID to dismiss"},
                        "reason": {"type": "string", "description": "Why this wasn't extracted (e.g. 'too simple', 'duplicate of X')"}
                    },
                    "required": ["task_id"]
                }
            },
            {
                "name": "task_rollback",
                "description": "Restore files to their pre-task state. Use when a task failed and modified files need to be reverted. Only works for tasks with trust_level Medium or High that had backups created.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "task_id": {"type": "string", "description": "Task ID to rollback"}
                    },
                    "required": ["task_id"]
                }
            },
            {
                "name": "task_retry",
                "description": "Manually retry a failed task. Creates a new task with the error context injected into the prompt so the backend avoids the same mistake. If max retries exhausted, escalates to a different backend. Optionally inject additional context.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "task_id": {"type": "string", "description": "Failed task ID to retry"},
                        "inject_context": {"type": "string", "description": "Optional extra context to inject into the retry prompt (e.g. hints, corrections)"}
                    },
                    "required": ["task_id"]
                }
            },
            {
                "name": "task_route",
                "description": "Get an AI backend recommendation for a task. Analyzes the prompt, historical performance, and learned patterns to suggest the best backend.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "prompt": {"type": "string", "description": "The task prompt to analyze for backend routing"},
                        "working_dir": {"type": "string", "description": "Optional working directory for context hints"}
                    },
                    "required": ["prompt"]
                }
            },
            {
                "name": "task_decompose",
                "description": "Break a natural language request into structured steps with backend recommendations.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "prompt": {"type": "string", "description": "Natural language task to decompose"},
                        "working_dir": {"type": "string", "description": "Optional working directory"}
                    },
                    "required": ["prompt"]
                }
            },
            {
                "name": "template_save",
                "description": "Save a reusable workflow template with parameter placeholders.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "description": {"type": "string"},
                        "steps": {"type": "array", "items": {"type": "object"}},
                        "backend": {"type": "string"},
                        "parameters": {"type": "object"}
                    },
                    "required": ["name", "description", "steps"]
                }
            },
            {
                "name": "template_list",
                "description": "List available workflow templates.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "template_run",
                "description": "Run a saved template with parameter substitution.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "parameters": {"type": "object"},
                        "working_dir": {"type": "string"}
                    },
                    "required": ["name"]
                }
            },
            {
                "name": "task_explain",
                "description": "Plain English explanation of a task or recent history.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "task_id": {"type": "string"},
                        "last": {"type": "integer"}
                    }
                }
            },
            {
                "name": "create_loaf",
                "description": "Create a new Project Loaf — a persistent JSON file that tracks multi-task coordination. Tracks goal, phases, tasks, decisions, discoveries, and breadcrumbs.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "goal": {"type": "string", "description": "What this project aims to accomplish"},
                        "project_name": {"type": "string", "description": "Short name (used in filename: {project_name}_Loaf.json)"},
                        "phases": {"type": "array", "items": {"type": "string"}, "description": "Optional phase names. Defaults to single 'main' phase."}
                    },
                    "required": ["goal", "project_name"]
                }
            },
            {
                "name": "loaf_update",
                "description": "Update an active Project Loaf with task results, decisions, discoveries, next actions, or phase advancement.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "loaf_id": {"type": "string", "description": "Loaf ID (e.g. 'MyProject_Loaf')"},
                        "task_update": {"type": "object", "description": "Task update: {task_id, status, output_summary, files_changed, decisions_made, discoveries}"},
                        "decision": {"type": "object", "description": "Decision record: {what, why, who}"},
                        "discovery": {"type": "object", "description": "Discovery record: {what, impact}"},
                        "next_actions": {"type": "array", "items": {"type": "string"}, "description": "Replace current next_actions list"},
                        "phase_status": {"type": "string", "description": "Set to 'done' to complete current phase and advance to next"}
                    },
                    "required": ["loaf_id"]
                }
            },
            {
                "name": "loaf_status",
                "description": "Get current state of a Project Loaf. If no loaf_id given, finds the most recent active loaf.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "loaf_id": {"type": "string", "description": "Loaf ID. Optional — omit to find most recent active loaf."}
                    }
                }
            },
            {
                "name": "loaf_close",
                "description": "Mark a Project Loaf as complete and archive it.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "loaf_id": {"type": "string", "description": "Loaf ID to close"}
                    },
                    "required": ["loaf_id"]
                }
            },
            {
                "name": "task_watch",
                "description": "Watch multiple tasks until all complete. Blocks server-side (zero LLM polling turns). Optionally sends MCP progress notifications at configurable intervals. Use instead of repeated get_status calls to save tokens.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "task_ids": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "List of task IDs to watch until completion"
                        },
                        "timeout_secs": {
                            "type": "integer",
                            "description": "Max seconds to wait before returning partial results (default: 600)"
                        },
                        "progress": {
                            "type": "boolean",
                            "description": "Send MCP notifications with progress updates (default: true). Set false to disable if notifications interfere with context limits."
                        },
                        "progress_interval_secs": {
                            "type": "integer",
                            "description": "Seconds between progress notifications (default: 10). Higher = fewer notifications."
                        }
                    },
                    "required": ["task_ids"]
                }
            }
        ,
            json!({
                "name": "session_list",
                "description": "List active Claude Code sessions with their status, working directory, pid, and whether the process is still alive. Heartbeat-driven pid/alive tracking.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "include_stalled": {
                            "type": "boolean",
                            "description": "If true, only return stalled sessions (alive with no activity for 120s+). Default: false."
                        }
                    }
                }
            }),
            json!({
                "name": "session_destroy",
                "description": "Destroy a session: kill the child process tree (same as task_cancel), mark cancelled, update meta.json. Returns killed_tree: [pids].",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": {
                            "type": "string",
                            "description": "Session ID to destroy"
                        }
                    },
                    "required": ["session_id"]
                }
            }),
            json!({
                "name": "get_analytics",
                "description": "Get task performance analytics: success rates by backend, total cost, average duration, recent failures. Filter by backend or time range.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "backend": {
                            "type": "string",
                            "description": "Filter by backend: claude_code, codex, gemini, gpt"
                        },
                        "since": {
                            "type": "string",
                            "description": "Only include tasks created after this RFC3339 timestamp"
                        }
                    }
                }
            }),
            {
                "name": "run_analyzer",
                "description": "Run the nightly task performance analyzer. Computes per-backend metrics (success rate, p50/p95 duration, avg cost, retry rate), detects inflection points and promotion candidates, writes proposals to Volumes/inbox/. NEVER auto-modifies routing.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "volumes_path": {
                            "type": "string",
                            "description": "Override Volumes base path. Default: C:\\My Drive\\Volumes"
                        },
                        "history_path": {
                            "type": "string",
                            "description": "Override task_history.json path. Default: auto-detect from MANAGER_HISTORY_DIR"
                        }
                    }
                }
            },
            {
                "name": "role_list",
                "description": "List available specialist roles (built-in + custom YAML) for task_submit. Each role injects a system prompt tailored to that specialty.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "role_create",
                "description": "Create a custom specialist role as a YAML file. Custom roles can override built-in roles.",
                "inputSchema": {
                    "type": "object",
                    "required": ["name", "prompt"],
                    "properties": {
                        "name": { "type": "string", "description": "Role name (alphanumeric/underscore). Used as filename and role param value." },
                        "prompt": { "type": "string", "description": "System prompt injected when this role is used." },
                        "expertise": { "type": "array", "items": { "type": "string" }, "description": "List of expertise areas for this role." }
                    }
                }
            },
            {
                "name": "task_rerun",
                "description": "Re-submit a completed (done) task using its original prompt, with optional extra context, file injection, or backend override. Returns a new task_id. The new task records rerun_of pointing to the original.",
                "inputSchema": {
                    "type": "object",
                    "required": ["task_id"],
                    "properties": {
                        "task_id": { "type": "string", "description": "ID of a previously completed task to rerun" },
                        "additional_context": { "type": "string", "description": "Extra context appended to the original prompt" },
                        "backend_override": { "type": "string", "enum": ["gpt", "gemini", "claude_code", "codex"], "description": "Use a different backend than the original task" },
                        "include_files": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Array of file paths whose current contents are injected into the prompt"
                        }
                    }
                }
            },
            {
                "name": "role_delete",
                "description": "Delete a custom specialist role YAML file.",
                "inputSchema": {
                    "type": "object",
                    "required": ["name"],
                    "properties": {
                        "name": { "type": "string", "description": "Role name to delete." }
                    }
                }
            },
            {
                "name": "notify",
                "description": "Show a Windows toast notification. Use for background task completion alerts, status updates, or any user-facing notification.",
                "inputSchema": {
                    "type": "object",
                    "required": ["title", "body"],
                    "properties": {
                        "title": { "type": "string", "description": "Notification title" },
                        "body": { "type": "string", "description": "Notification body text" },
                        "icon": { "type": "string", "enum": ["info", "warning", "error"], "default": "info", "description": "Icon type" },
                        "duration_ms": { "type": "integer", "default": 5000, "description": "Display duration in milliseconds" }
                    }
                }
            },
            {
                "name": "dashboard_open",
                "description": "Open the CPC operational dashboard in the default browser. Returns the URL. The dashboard shows live session/task status, breadcrumbs, server health, and scorecard.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "dashboard_stop",
                "description": "Stop the dashboard HTTP listener. Use when you want to free the port or restart the dashboard.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "dashboard_status",
                "description": "Get the current dashboard HTTP server status: whether it is running, which port it bound to, and the URL.",
                "inputSchema": { "type": "object", "properties": {} }
            }
        ]
    })
}

// ============================================================================
// HTTP Dashboard
// ============================================================================

#[derive(Clone)]
struct DashboardState {
    tasks: Arc<RwLock<HashMap<String, Task>>>,
    config: Arc<RwLock<ServerConfig>>,
    recent_tool_calls: Arc<Mutex<VecDeque<ToolCallEntry>>>,
}

async fn dash_status(State(st): State<DashboardState>) -> Json<Value> {
    let store = st.tasks.read().await;
    let running = store
        .values()
        .filter(|t| t.status == TaskStatus::Running)
        .count();
    let completed = store
        .values()
        .filter(|t| t.status == TaskStatus::Done)
        .count();
    let mut tasks: Vec<&Task> = store.values().collect();
    tasks.sort_by_key(|t| std::cmp::Reverse(t.created_at));
    let tasks_json: Vec<Value> = tasks
        .iter()
        .map(|t| {
            json!({
                "id": t.id,
                "backend": t.backend,
                "status": t.status.to_string(),
                "prompt_preview": safe_truncate(&t.prompt, 100),
                "created_at": t.created_at.to_rfc3339(),
                "progress_lines": t.progress_lines,
            })
        })
        .collect();
    Json(
        json!({ "tasks": tasks_json, "running": running, "completed": completed, "total": store.len() }),
    )
}

async fn dash_status_by_id(
    State(st): State<DashboardState>,
    AxumPath(id): AxumPath<String>,
) -> (StatusCode, Json<Value>) {
    let store = st.tasks.read().await;
    match store.get(&id) {
        Some(t) => (
            StatusCode::OK,
            Json(json!({
                "id": t.id, "backend": t.backend, "status": t.status.to_string(),
                "prompt": t.prompt, "output": t.output, "error": t.error,
                "created_at": t.created_at.to_rfc3339(),
                "started_at": t.started_at.map(|s| s.to_rfc3339()),
                "completed_at": t.completed_at.map(|s| s.to_rfc3339()),
                "progress_lines": t.progress_lines,
            })),
        ),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("Task '{}' not found", id)})),
        ),
    }
}

async fn dash_health() -> Json<Value> {
    let servers = [
        "utonomous",
        "echo",
        "atlas",
        "local",
        "browser-mcp",
        "manager",
        "stocks",
    ];
    let output = TokioCommand::new("tasklist")
        .args(["/FO", "CSV", "/NH"])
        .output()
        .await
        .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase())
        .unwrap_or_default();
    let mut result = serde_json::Map::new();
    for name in &servers {
        let alive = output.contains(&format!("{}.exe", name));
        result.insert(
            name.to_string(),
            json!(if alive { "alive" } else { "dead" }),
        );
    }
    Json(json!({"servers": Value::Object(result)}))
}

async fn dash_inbox() -> Json<Value> {
    let inbox_path = volumes_base_path()
        .join("multi_ai_coordination")
        .join("inbox.md");
    let content = match std::fs::read_to_string(inbox_path) {
        Ok(c) => c,
        Err(e) => return Json(json!({"error": format!("Cannot read inbox: {}", e)})),
    };
    let mut pending: Vec<String> = Vec::new();
    let mut processed: Vec<String> = Vec::new();
    let mut section = "";
    let mut entry = String::new();
    for line in content.lines() {
        if line.starts_with("## Pending") {
            flush_entry(&mut entry, section, &mut pending, &mut processed);
            section = "pending";
            continue;
        }
        if line.starts_with("## Processed") {
            flush_entry(&mut entry, section, &mut pending, &mut processed);
            section = "processed";
            continue;
        }
        if line.starts_with("## ") {
            flush_entry(&mut entry, section, &mut pending, &mut processed);
            section = "";
            continue;
        }
        if line.starts_with("### ") {
            flush_entry(&mut entry, section, &mut pending, &mut processed);
            entry = line.to_string();
        } else if !entry.is_empty() {
            entry.push('\n');
            entry.push_str(line);
        }
    }
    flush_entry(&mut entry, section, &mut pending, &mut processed);
    Json(json!({"pending": pending, "processed": processed}))
}

fn flush_entry(
    entry: &mut String,
    section: &str,
    pending: &mut Vec<String>,
    processed: &mut Vec<String>,
) {
    if entry.is_empty() {
        return;
    }
    match section {
        "pending" => pending.push(std::mem::take(entry)),
        "processed" => processed.push(std::mem::take(entry)),
        _ => {
            entry.clear();
        }
    }
}

async fn dash_get_prefs() -> Json<Value> {
    let prefs_path = format!(
        r"{}\CPC\config\dashboard_prefs.json",
        std::env::var("LOCALAPPDATA").unwrap_or_else(|_| r"C:\Users\Default\AppData\Local".into())
    );
    match std::fs::read_to_string(&prefs_path) {
        Ok(c) => Json(serde_json::from_str::<Value>(&c).unwrap_or(json!({}))),
        Err(_) => Json(json!({})),
    }
}

async fn dash_post_prefs(Json(body): Json<Value>) -> Json<Value> {
    let prefs_dir = format!(
        r"{}\CPC\config",
        std::env::var("LOCALAPPDATA").unwrap_or_else(|_| r"C:\Users\Default\AppData\Local".into())
    );
    let _ = std::fs::create_dir_all(&prefs_dir);
    let prefs_path = format!(r"{}\dashboard_prefs.json", prefs_dir);
    match std::fs::write(
        &prefs_path,
        serde_json::to_string_pretty(&body).unwrap_or_default(),
    ) {
        Ok(_) => Json(json!({"saved": true})),
        Err(e) => Json(json!({"error": format!("Failed to write prefs: {}", e)})),
    }
}

async fn dash_post_task(
    State(st): State<DashboardState>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let backend_str = match body.get("backend").and_then(|v| v.as_str()) {
        Some(b) => b,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Missing 'backend'"})),
            )
        }
    };
    let prompt = match body.get("prompt").and_then(|v| v.as_str()) {
        Some(p) => p.to_string(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Missing 'prompt'"})),
            )
        }
    };
    let working_dir = body
        .get("working_dir")
        .and_then(|v| v.as_str())
        .map(String::from);
    let backend = match backend_str {
        "gpt" => Backend::Gpt,
        "gemini" => Backend::Gemini,
        "claude_code" | "claude" => Backend::ClaudeCode,
        "codex" => Backend::Codex,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("Unknown backend: {}", backend_str)})),
            )
        }
    };
    let task_id = Uuid::new_v4().to_string()[..8].to_string();
    let task = Task {
        id: task_id.clone(),
        backend: backend.clone(),
        prompt: prompt.clone(),
        system_prompt: None,
        model: None,
        working_dir: working_dir.clone(),
        status: TaskStatus::Queued,
        output: String::new(),
        error: None,
        created_at: Utc::now(),
        started_at: None,
        completed_at: None,
        progress_lines: 0,
        steps: Vec::new(),
        last_activity: None,
        last_output_chunk_at: None,
        stall_detected: false,
        extraction_status: ExtractionStatus::None,
        trust_score: 0,
        trust_level: TrustLevel::Low,
        rollback_path: None,
        validation_status: ValidationStatus::NotChecked,
        assertions: Vec::new(),
        backed_up_files: Vec::new(),
        retry_count: 0,
        max_retries: 2,
        retry_of: None,
        error_context: None,
        input_tokens: 0,
        output_tokens: 0,
        cost_usd: 0.0,
        on_complete: None,
        role: None,
        save_artifact: false,
        rerun_of: None,
        parent_task_id: None,
        forked_from: None,
        continuation_of: None,
        child_pid: None,
        log_path: None,
        log_offset: 0,
        reconnected: false,
        watchdog_observations: Vec::new(),
        fingerprint: None,
        superseded_by: None,
        label: None,
        current_step: None,
        total_steps: None,
        current_step_desc: None,
        live_activity: None,
        effort: None,
        notify_on_complete: None,
        notify_on_fail: None,
        notify_on_destroy: None,
        notify_title: None,
        notify_body: None,
    };
    {
        let mut store = st.tasks.write().await;
        store.insert(task_id.clone(), task.clone());
    }
    Server::persist_task(&task);
    let tasks_bg = st.tasks.clone();
    let tid = task_id.clone();
    match backend {
        Backend::Gpt => {
            tokio::spawn(run_gpt_task(st.config.clone(), tasks_bg, tid));
        }
        Backend::Gemini => {
            let args = vec![
                gemini_cmd().to_string(),
                "-p".into(),
                prompt,
                "--yolo".into(),
            ];
            tokio::spawn(run_cli_task(
                tasks_bg,
                tid,
                node_cmd(),
                args,
                None,
                StdinMode::Null,
            ));
        }
        Backend::ClaudeCode => {
            let mut args = vec![
                "-p".into(),
                prompt,
                "--dangerously-skip-permissions".into(),
                "--verbose".into(),
                "--output-format".into(),
                "stream-json".into(),
                "--add-dir".into(),
                r"C:\temp".into(),
                "--add-dir".into(),
                volumes_dir(),
                "--add-dir".into(),
                r"C:\CPC".into(),
                "--add-dir".into(),
                workspace_root(),
            ];
            if let Some(ref wd) = working_dir {
                args.push("--add-dir".into());
                args.push(wd.clone());
            }
            tokio::spawn(run_cli_task(
                tasks_bg,
                tid,
                claude_code_cmd(),
                args,
                None,
                StdinMode::Null,
            ));
        }
        Backend::Codex => {
            let wd = working_dir.unwrap_or_else(workspace_root);
            let args = vec![
                "exec".into(),
                "--json".into(),
                "--skip-git-repo-check".into(),
                "--full-auto".into(),
                "--cd".into(),
                wd.clone(),
                "--".into(),
                prompt,
            ];
            tokio::spawn(run_codex_task(tasks_bg, tid, args, wd));
        }
    }
    (
        StatusCode::OK,
        Json(json!({"task_id": task_id, "status": "queued"})),
    )
}

async fn dash_cancel(
    State(st): State<DashboardState>,
    AxumPath(id): AxumPath<String>,
) -> (StatusCode, Json<Value>) {
    let mut store = st.tasks.write().await;
    match store.get_mut(&id) {
        Some(task) if task.status == TaskStatus::Running || task.status == TaskStatus::Queued => {
            task.status = TaskStatus::Cancelled;
            task.completed_at = Some(Utc::now());
            task.error = Some("Cancelled via dashboard".into());
            Server::flag_extraction(task);
            // Item 18: no retry for cancelled tasks
            Server::persist_task(task);
            Server::save_to_history(task);
            // v1.3.1: notify_on_destroy for dashboard cancel
            if task.notify_on_destroy.unwrap_or(false) {
                fire_notify_for_task(
                    &task.id,
                    &task.created_at,
                    task.notify_title.as_deref(),
                    task.notify_body.as_deref(),
                    NotifyReason::Destroyed,
                    &RealNotifier,
                );
            }
            (
                StatusCode::OK,
                Json(json!({"task_id": id, "status": "cancelled"})),
            )
        }
        Some(task) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("Task {} is {} - cannot cancel", id, task.status)})),
        ),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("Task {} not found", id)})),
        ),
    }
}

async fn dash_knowledge() -> Json<Value> {
    let volumes_path = volumes_dir();
    let mut topics: Vec<Value> = Vec::new();
    let mut recent: Vec<(String, std::time::SystemTime)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(volumes_path) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let op_file = path.join(format!("Operating_{}.md", name));
            let modified = entry.metadata().ok().and_then(|m| m.modified().ok());
            topics.push(json!({
                "name": name,
                "has_operating_file": op_file.exists(),
                "modified": modified.map(|t| { let dt: DateTime<Utc> = t.into(); dt.to_rfc3339() }),
            }));
            if let Some(t) = modified {
                recent.push((name, t));
            }
        }
    }
    recent.sort_by_key(|r| std::cmp::Reverse(r.1));
    recent.truncate(10);
    let recent_json: Vec<Value> = recent
        .iter()
        .map(|(n, t)| {
            let dt: DateTime<Utc> = (*t).into();
            json!({"topic": n, "modified": dt.to_rfc3339()})
        })
        .collect();
    Json(json!({"topics": topics, "recent_changes": recent_json}))
}

async fn dash_git() -> Json<Value> {
    let repo = workspace_root();
    let (status, branch, log) = tokio::join!(
        TokioCommand::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&repo)
            .output(),
        TokioCommand::new("git")
            .args(["branch", "--show-current"])
            .current_dir(&repo)
            .output(),
        TokioCommand::new("git")
            .args(["log", "--oneline", "-5"])
            .current_dir(&repo)
            .output()
    );
    Json(json!({
        "branch": branch.ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()),
        "status": status.ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()),
        "log": log.ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()),
    }))
}

async fn dash_system() -> Json<Value> {
    use sysinfo::System;
    let mut sys = System::new();
    sys.refresh_memory();
    sys.refresh_cpu_usage();
    // CPU needs two samples for accurate reading
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    sys.refresh_cpu_usage();
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let total_disk: u64 = disks.iter().map(|d| d.total_space()).sum();
    let avail_disk: u64 = disks.iter().map(|d| d.available_space()).sum();
    Json(json!({
        "cpu": { "usage_percent": sys.global_cpu_usage(), "cores": sys.cpus().len() },
        "ram": {
            "total_gb": format!("{:.1}", sys.total_memory() as f64 / 1_073_741_824.0),
            "used_gb": format!("{:.1}", sys.used_memory() as f64 / 1_073_741_824.0),
            "available_gb": format!("{:.1}", (sys.total_memory() - sys.used_memory()) as f64 / 1_073_741_824.0),
        },
        "disk": {
            "total_gb": format!("{:.1}", total_disk as f64 / 1_073_741_824.0),
            "available_gb": format!("{:.1}", avail_disk as f64 / 1_073_741_824.0),
        },
    }))
}

async fn dash_history() -> Json<Value> {
    let history_path = format!("{}\\task_history.json", history_dir());
    match std::fs::read_to_string(&history_path) {
        Ok(data) => {
            let entries: Vec<Value> = serde_json::from_str(&data).unwrap_or_default();
            Json(json!({"entries": entries, "count": entries.len()}))
        }
        Err(_) => Json(json!({"entries": [], "count": 0})),
    }
}

fn volumes_base_path() -> std::path::PathBuf {
    cpc_paths::volumes_path().unwrap_or_else(|_| std::path::PathBuf::from(r"C:\My Drive\Volumes"))
}

/// Validate that a requested path is under the Volumes base directory.
fn validate_volumes_path(requested: &str) -> Result<std::path::PathBuf, (StatusCode, Json<Value>)> {
    let base = volumes_base_path();
    let candidate = base.join(requested);
    // Canonicalize base (must exist)
    let canon_base = base.canonicalize().map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "Volumes base path not accessible"})),
        )
    })?;
    // Canonicalize candidate
    let canon = candidate.canonicalize().map_err(|_| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "File not found"})),
        )
    })?;
    if !canon.starts_with(&canon_base) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(json!({"error": "Access denied: path outside Volumes"})),
        ));
    }
    Ok(canon)
}

#[derive(Deserialize)]
struct PathQuery {
    path: Option<String>,
}

async fn api_read_file(Query(q): Query<PathQuery>) -> impl IntoResponse {
    let rel_path = match q.path {
        Some(p) if !p.is_empty() => p,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Missing 'path' parameter"})),
            )
                .into_response()
        }
    };
    let canon = match validate_volumes_path(&rel_path) {
        Ok(p) => p,
        Err((status, body)) => return (status, body).into_response(),
    };
    if !canon.is_file() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "File not found"})),
        )
            .into_response();
    }
    match tokio::fs::read_to_string(&canon).await {
        Ok(contents) => (
            StatusCode::OK,
            [
                ("content-type", "text/plain; charset=utf-8"),
                ("access-control-allow-origin", "*"),
            ],
            contents,
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "Failed to read file"})),
        )
            .into_response(),
    }
}

async fn api_list_dir(Query(q): Query<PathQuery>) -> impl IntoResponse {
    let rel_path = match q.path {
        Some(p) if !p.is_empty() => p,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Missing 'path' parameter"})),
            )
                .into_response()
        }
    };
    let canon = match validate_volumes_path(&rel_path) {
        Ok(p) => p,
        Err((status, body)) => return (status, body).into_response(),
    };
    if !canon.is_dir() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Directory not found"})),
        )
            .into_response();
    }
    let mut files: Vec<Value> = Vec::new();
    match std::fs::read_dir(&canon) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let meta = entry.metadata().ok();
                files.push(json!({
                    "name": entry.file_name().to_string_lossy(),
                    "is_dir": meta.as_ref().map(|m| m.is_dir()).unwrap_or(false),
                    "size": meta.as_ref().map(|m| m.len()).unwrap_or(0),
                }));
            }
        }
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Failed to read directory"})),
            )
                .into_response()
        }
    }
    (
        StatusCode::OK,
        [("access-control-allow-origin", "*")],
        Json(json!({"files": files})),
    )
        .into_response()
}

/// GET / — serve embedded dashboard HTML
async fn dash_root() -> impl IntoResponse {
    // Dashboard HTML is embedded at compile time. To update: edit
    // src/dashboard_ui.html, rebuild manager, restart Claude Desktop.
    // No disk-override path — prevents runtime HTML from diverging
    // from the source in the repo (was a ship-blocker Apr 2026).
    const EMBEDDED_HTML: &str = include_str!("dashboard_ui.html");

    // Cache-Control: no-store so browsers never cache the dashboard HTML.
    // Ends the Ctrl+Shift+R dance for every dashboard iteration.
    (
        [
            (
                axum::http::header::CACHE_CONTROL,
                "no-store, no-cache, must-revalidate",
            ),
            (axum::http::header::PRAGMA, "no-cache"),
            (axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8"),
        ],
        EMBEDDED_HTML.to_string(),
    )
}

/// GET /api/status — rich manager status for the dashboard frontend and live_status.json
async fn dash_api_status(State(st): State<DashboardState>) -> Json<Value> {
    let store = st.tasks.read().await;
    let running = store
        .values()
        .filter(|t| t.status == TaskStatus::Running)
        .count();
    let queued = store
        .values()
        .filter(|t| t.status == TaskStatus::Queued)
        .count();
    let done = store
        .values()
        .filter(|t| t.status == TaskStatus::Done)
        .count();
    let failed = store
        .values()
        .filter(|t| t.status == TaskStatus::Failed)
        .count();
    let orphaned = store
        .values()
        .filter(|t| t.status == TaskStatus::Orphaned)
        .count();

    let mut tasks: Vec<&Task> = store.values().collect();
    tasks.sort_by_key(|t| std::cmp::Reverse(t.created_at));
    let tasks_json: Vec<Value> = tasks
        .iter()
        .take(20)
        .map(|t| {
            json!({
                "id": t.id,
                "backend": t.backend.to_string(),
                "status": t.status.to_string(),
                "prompt_preview": safe_truncate(&t.prompt, 80),
                "prompt": t.prompt,
                "created_at": t.created_at.to_rfc3339(),
                "started_at": t.started_at.map(|s| s.to_rfc3339()),
                "completed_at": t.completed_at.map(|s| s.to_rfc3339()),
                "progress_lines": t.progress_lines,
                "last_activity": t.last_activity.map(|la| la.to_rfc3339()),
                "stall_detected": t.stall_detected,
                "orphaned": t.status == TaskStatus::Orphaned,
                "label": t.label,
                "current_step": t.current_step,
                "total_steps": t.total_steps,
                "current_step_desc": t.current_step_desc,
                "step_count": t.steps.len(),
                "steps_completed": t.steps.iter().filter(|s| s.status == "completed").count(),
                "live_activity": t.live_activity,
            })
        })
        .collect();

    let status_bar = build_status_bar(&store);
    let loaf = find_active_loaf().map(|(id, loaf)| json!({"id": id, "data": loaf}));

    let recent_calls: Vec<ToolCallEntry> = {
        let ring = st.recent_tool_calls.lock().unwrap();
        ring.iter()
            .rev()
            .take(12)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    };

    // Cross-server tools: merge manager ring buffer with mcp_activity.jsonl tail
    let activity_log = tail_mcp_activity_log(20);
    let mut cross_tools: Vec<Value> = recent_calls
        .iter()
        .map(|c| {
            json!({
                "server": "manager",
                "tool": c.tool_name,
                "timestamp": c.timestamp_utc,
                "duration_ms": c.duration_ms,
            })
        })
        .collect();
    for entry in &activity_log {
        cross_tools.push(entry.clone());
    }
    // Sort descending by timestamp, dedup by (server, tool, timestamp), take 5
    cross_tools.sort_by(|a, b| {
        let ta = a.get("timestamp").and_then(|v| v.as_str()).unwrap_or("");
        let tb = b.get("timestamp").and_then(|v| v.as_str()).unwrap_or("");
        tb.cmp(ta)
    });
    let mut seen_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
    cross_tools.retain(|c| {
        let key = format!(
            "{}|{}|{}",
            c.get("server").and_then(|v| v.as_str()).unwrap_or(""),
            c.get("tool").and_then(|v| v.as_str()).unwrap_or(""),
            c.get("timestamp").and_then(|v| v.as_str()).unwrap_or("")
        );
        seen_keys.insert(key)
    });
    cross_tools.truncate(5);

    let pending_swaps = count_pending_swaps();
    let active_breadcrumbs = read_active_breadcrumbs_full();

    Json(json!({
        "version": "1.3.8",
        "sessions": {
            "running": running,
            "queued": queued,
            "done": done,
            "failed": failed,
            "orphaned": orphaned,
            "total": store.len(),
        },
        "tasks": tasks_json,
        "loaf": loaf,
        "status_bar": status_bar,
        "recent_tool_calls": recent_calls,
        "cross_server_recent_tools": cross_tools,
        "pending_swaps": pending_swaps,
        "active_breadcrumbs": active_breadcrumbs,
        "timestamp": Utc::now().to_rfc3339(),
    }))
}

/// GET /api/config — dashboard configuration (ports + poll intervals)
async fn dash_api_config() -> Json<Value> {
    let port = DASHBOARD_PORT.load(Ordering::Relaxed);
    Json(json!({
        "ports": {
            "manager":    port,
            "local":      9101u16,
            "hands":      9102u16,
            "workflow":   9103u16,
            "autonomous": 9104u16,
        },
        "poll_intervals_ms": {
            "manager":    5000u32,
            "local":      5000u32,
            "hands":      5000u32,
            "workflow":   42000u32,
            "autonomous": 42000u32,
        },
        "version": "1.3.8",
    }))
}

/// Resolve Volumes path from env or default.
fn volumes_dir() -> String {
    cpc_paths::volumes_path()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| r"C:\My Drive\Volumes".to_string())
}

/// Tail the last `n` lines of `C:\CPC\logs\mcp_activity.jsonl` and return parsed entries.
/// Each entry has {server, tool, timestamp, duration_ms, status}.
/// Returns an empty vec on any IO or parse error (non-blocking).
fn tail_mcp_activity_log(n: usize) -> Vec<Value> {
    let path = std::env::var("CPC_LOGS_DIR").unwrap_or_else(|_| r"C:\CPC\logs".to_string());
    let filepath = format!(r"{}\mcp_activity.jsonl", path);
    let Ok(content) = std::fs::read_to_string(&filepath) else {
        return Vec::new();
    };
    content
        .lines()
        .rev()
        .take(n)
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect()
}

/// Count `.new` files in `C:\CPC\servers\` for the pending-swap widget.
fn count_pending_swaps() -> usize {
    let dir = std::env::var("CPC_SERVERS_DIR").unwrap_or_else(|_| r"C:\CPC\servers".to_string());
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "new"))
        .count()
}

/// Fetch /api/status from a server on localhost. Returns None if unreachable.
async fn fetch_peer_status(client: &reqwest::Client, port: u16) -> Option<Value> {
    let url = format!("http://127.0.0.1:{}/api/status", port);
    match client
        .get(&url)
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r.json::<Value>().await.ok(),
        _ => None,
    }
}

/// Background task: 5s poll — walk process tree of running tasks, update live_activity and
/// step progress fields on TaskMeta. Redacts entries containing password/token/api_key/secret.
async fn live_activity_walker(tasks: Arc<RwLock<HashMap<String, Task>>>) {
    use regex::Regex;
    use sysinfo::{ProcessesToUpdate, System};

    let step_re = match Regex::new(r"^\[STEP (\d+)/(\d+)\] (.+)$") {
        Ok(r) => r,
        Err(_) => return,
    };
    let redact_re = match Regex::new(r"(?i)(password|token|api_key|secret)") {
        Ok(r) => r,
        Err(_) => return,
    };

    let mut sys = System::new();
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));

    loop {
        interval.tick().await;
        sys.refresh_processes(ProcessesToUpdate::All, false);

        // Snapshot running tasks that have a child_pid — avoid holding read lock during sysinfo work
        let running: Vec<(String, u32, String)> = {
            let store = tasks.read().await;
            store
                .values()
                .filter(|t| t.status == TaskStatus::Running && t.child_pid.is_some())
                .map(|t| (t.id.clone(), t.child_pid.unwrap(), t.output.clone()))
                .collect()
        };

        for (task_id, root_pid, output) in running {
            let activity = collect_process_tree(&sys, root_pid, &redact_re);
            let step_info = parse_step_progress(&output, &step_re);

            let mut store = tasks.write().await;
            if let Some(task) = store.get_mut(&task_id) {
                task.live_activity = if activity.is_empty() {
                    None
                } else {
                    Some(activity)
                };
                if let Some((step, total, desc)) = step_info {
                    task.current_step = Some(step);
                    task.total_steps = Some(total);
                    task.current_step_desc = Some(desc);
                }
            }
        }
    }
}

/// BFS walk of the process tree rooted at `root_pid`. Caps at 20 entries.
/// Redacts cmd_preview for any process whose full command line matches the redact pattern.
fn collect_process_tree(
    sys: &sysinfo::System,
    root_pid: u32,
    redact_re: &regex::Regex,
) -> Vec<ActivityEntry> {
    use sysinfo::Pid;

    let mut result: Vec<ActivityEntry> = Vec::new();
    let mut queue: VecDeque<u32> = VecDeque::new();
    let mut visited: std::collections::HashSet<u32> = std::collections::HashSet::new();
    queue.push_back(root_pid);

    while let Some(pid_u32) = queue.pop_front() {
        if result.len() >= 20 {
            break;
        }
        if !visited.insert(pid_u32) {
            continue;
        }
        let pid = Pid::from_u32(pid_u32);
        if let Some(proc) = sys.process(pid) {
            let cmd_raw: String = proc
                .cmd()
                .iter()
                .map(|s| s.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" ");
            let cmd_preview = if redact_re.is_match(&cmd_raw) {
                "[REDACTED]".to_string()
            } else {
                cmd_raw.chars().take(120).collect()
            };
            result.push(ActivityEntry {
                pid: pid_u32,
                name: proc.name().to_string_lossy().into_owned(),
                cmd_preview,
                cpu_percent: proc.cpu_usage(),
            });
            // Enqueue children
            for (cpid, cproc) in sys.processes() {
                if cproc.parent() == Some(pid) {
                    queue.push_back(cpid.as_u32());
                }
            }
        }
    }
    result
}

/// Scan the last 100 lines of task output for the most recent `[STEP n/N] description` line.
fn parse_step_progress(output: &str, re: &regex::Regex) -> Option<(u32, u32, String)> {
    for line in output.lines().rev().take(100) {
        if let Some(caps) = re.captures(line.trim()) {
            let step: u32 = caps[1].parse().ok()?;
            let total: u32 = caps[2].parse().ok()?;
            let desc = caps[3].trim().to_string();
            return Some((step, total, desc));
        }
    }
    None
}

/// Background task: every 5s check running tasks for output stalls.
///
/// If a Running task has produced no output chunks for `timeout_secs` seconds
/// (tracked via `last_output_chunk_at`, falling back to `started_at`), it is
/// marked `Stalled` and a Windows toast is fired.  If output resumes on a
/// Stalled task the inline push sites transition it back to Running immediately;
/// the watchdog handles the initial detection only.
async fn stall_watchdog(tasks: Arc<RwLock<HashMap<String, Task>>>, timeout_secs: u64) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
    interval.tick().await; // skip first tick — let tasks settle
    loop {
        interval.tick().await;
        let now = Utc::now();

        // Read pass: collect Running tasks that have exceeded the stall threshold.
        let to_stall: Vec<(String, String)> = {
            let store = tasks.read().await;
            store
                .values()
                .filter(|t| t.status == TaskStatus::Running)
                .filter_map(|t| {
                    let baseline = t.last_output_chunk_at.or(t.started_at)?;
                    let elapsed = now.signed_duration_since(baseline).num_seconds();
                    if elapsed > timeout_secs as i64 {
                        let label = t.label.clone().unwrap_or_else(|| t.id.clone());
                        Some((t.id.clone(), label))
                    } else {
                        None
                    }
                })
                .collect()
        };

        if to_stall.is_empty() {
            continue;
        }

        // Write pass: mark stalled (re-check status inside write lock).
        {
            let mut store = tasks.write().await;
            for (task_id, _) in &to_stall {
                if let Some(task) = store.get_mut(task_id) {
                    if task.status == TaskStatus::Running {
                        task.status = TaskStatus::Stalled;
                    }
                }
            }
        }

        // Fire toasts outside the lock — one per newly-stalled task.
        for (_, label) in to_stall {
            let title = format!("[STALLED] {}", label);
            let body = format!(
                "No output for {}+ min. Likely Claude Code freeze-at-init. Auto-cancel armed.",
                timeout_secs / 60
            );
            let _ = do_notify(&title, &body, "warning", 10000);
        }
    }
}

/// Background task: every 30s poll all 5 servers and write Volumes/dashboard/live_status.json.
async fn live_status_writer(manager_port: u16) {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    let vpath = volumes_dir();
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
    interval.tick().await; // skip immediate first tick — let server settle
    loop {
        interval.tick().await;
        if !DASHBOARD_RUNNING.load(Ordering::Relaxed) {
            break;
        }

        let manager_status = fetch_peer_status(&client, manager_port).await;
        let local_status = fetch_peer_status(&client, 9101).await;
        let hands_status = fetch_peer_status(&client, 9102).await;
        let workflow_status = fetch_peer_status(&client, 9103).await;
        let auto_status = fetch_peer_status(&client, 9104).await;

        let payload = json!({
            "timestamp":  Utc::now().to_rfc3339(),
            "manager":    manager_status,
            "local":      local_status,
            "hands":      hands_status,
            "workflow":   workflow_status,
            "autonomous": auto_status,
        });

        let dashboard_dir = format!(r"{}\dashboard", vpath);
        if std::fs::create_dir_all(&dashboard_dir).is_ok() {
            let path = format!(r"{}\live_status.json", dashboard_dir);
            let _ = std::fs::write(
                &path,
                serde_json::to_string_pretty(&payload).unwrap_or_default(),
            );
        }
    }
}

/// POST /api/tasks/register — accept external task reports (e.g. from manodex)
async fn api_register_external_task(
    State(st): State<DashboardState>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let id = body
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "missing id"})),
        );
    }
    let backend_str = body
        .get("backend")
        .and_then(|v| v.as_str())
        .unwrap_or("codex");
    let backend = match backend_str {
        "codex" => Backend::Codex,
        "gpt" => Backend::Gpt,
        "gemini" => Backend::Gemini,
        "claude_code" => Backend::ClaudeCode,
        _ => Backend::Codex,
    };
    let prompt = body
        .get("prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let status_str = body
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("done");
    let status = match status_str {
        "running" => TaskStatus::Running,
        "failed" => TaskStatus::Failed,
        "queued" => TaskStatus::Queued,
        _ => TaskStatus::Done,
    };
    let created_at = body
        .get("created_at")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        .unwrap_or_else(Utc::now);
    let completed_at = body
        .get("completed_at")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<DateTime<Utc>>().ok());

    let task = Task {
        id: id.clone(),
        backend,
        prompt,
        system_prompt: None,
        model: None,
        working_dir: None,
        status,
        output: String::new(),
        error: None,
        created_at,
        started_at: Some(created_at),
        completed_at,
        progress_lines: body
            .get("output_lines")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize,
        steps: vec![],
        last_activity: completed_at,
        last_output_chunk_at: None,
        stall_detected: false,
        extraction_status: ExtractionStatus::None,
        trust_score: 0,
        trust_level: TrustLevel::Low,
        rollback_path: None,
        validation_status: ValidationStatus::default(),
        assertions: vec![],
        backed_up_files: vec![],
        retry_count: 0,
        max_retries: default_max_retries(),
        retry_of: None,
        error_context: None,
        input_tokens: 0,
        output_tokens: 0,
        cost_usd: 0.0,
        on_complete: None,
        role: None,
        save_artifact: false,
        rerun_of: None,
        parent_task_id: None,
        forked_from: None,
        continuation_of: None,
        child_pid: None,
        log_path: None,
        log_offset: 0,
        reconnected: false,
        watchdog_observations: vec![],
        fingerprint: None,
        superseded_by: None,
        label: body
            .get("prompt_preview")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        current_step: None,
        total_steps: None,
        current_step_desc: None,
        live_activity: None,
        effort: None,
        notify_on_complete: None,
        notify_on_fail: None,
        notify_on_destroy: None,
        notify_title: None,
        notify_body: None,
    };

    let mut store = st.tasks.write().await;
    store.insert(id.clone(), task);
    info!("Registered external task {} (backend={})", id, backend_str);

    (StatusCode::OK, Json(json!({"registered": id})))
}

async fn start_dashboard(state: DashboardState) {
    // Port priority: CPC_DASHBOARD_PORT → CPC_MANAGER_PORT → default 9218
    // 9218 is well above the MCP server range (local=9101, hands=9102, workflow=9103,
    // autonomous=9104) and above the previous default (9200) which still drifted on some setups.
    let preferred: u16 = std::env::var("CPC_DASHBOARD_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .or_else(|| {
            std::env::var("CPC_MANAGER_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
        })
        .unwrap_or(9218);

    // Step 12.6: stall watchdog timeout (default 600s = 10 min)
    let stall_timeout_secs: u64 = std::env::var("MANAGER_STALL_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(600);

    // Clone tasks handle before state is moved into axum router
    let tasks_for_watchdog = state.tasks.clone();

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        // v1.2.8: new dashboard routes
        .route("/", get(dash_root))
        .route("/api/status", get(dash_api_status))
        .route("/api/config", get(dash_api_config))
        // legacy routes kept for backward compat
        .route("/status", get(dash_status))
        .route("/status/:id", get(dash_status_by_id))
        .route("/health", get(dash_health))
        .route("/inbox", get(dash_inbox))
        .route("/prefs", get(dash_get_prefs).post(dash_post_prefs))
        .route("/task", post(dash_post_task))
        .route("/cancel/:id", post(dash_cancel))
        .route("/knowledge", get(dash_knowledge))
        .route("/history", get(dash_history))
        .route("/git", get(dash_git))
        .route("/system", get(dash_system))
        .route("/api/read-file", get(api_read_file))
        .route("/api/list-dir", get(api_list_dir))
        .route("/api/tasks/register", post(api_register_external_task))
        .layer(cors)
        .with_state(state);

    // Try preferred port with random jitter, then walk forward through a 100-port range.
    // Jitter avoids collisions when multiple manager instances start concurrently.
    let jitter: u16 = rand::random::<u16>() % 20;
    let range_size: u16 = 100;
    let mut attempts: u16 = 0;
    let mut bound_port = preferred + jitter;
    let listener = loop {
        match tokio::net::TcpListener::bind(format!("127.0.0.1:{}", bound_port)).await {
            Ok(l) => break l,
            Err(e) => {
                attempts += 1;
                if attempts >= range_size {
                    error!(
                        "Dashboard failed to bind any port in range {}–{} after {} attempts: {}. \
                         Set CPC_DASHBOARD_PORT env var to override.",
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

    DASHBOARD_PORT.store(bound_port, Ordering::SeqCst);
    DASHBOARD_RUNNING.store(true, Ordering::SeqCst);
    info!("Dashboard HTTP server on http://127.0.0.1:{}/", bound_port);

    // v1.4.2: write bound port to discoverable file for UI/other-session lookup.
    // Location: %LOCALAPPDATA%\manager-mcp\dashboard_url.txt
    // Content: full URL on first line (e.g., "http://127.0.0.1:9218/")
    // Best-effort — failure to write doesn't stop the dashboard.
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        let url_file = std::path::PathBuf::from(local_app_data)
            .join("manager-mcp")
            .join("dashboard_url.txt");
        if let Some(parent) = url_file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let url = format!("http://127.0.0.1:{}/\n", bound_port);
        match std::fs::write(&url_file, &url) {
            Ok(_) => info!("Dashboard URL written to {:?}", url_file),
            Err(e) => warn!("Failed to write dashboard URL file: {}", e),
        }
    }

    // Spawn live_status.json writer alongside the HTTP server
    tokio::spawn(live_status_writer(bound_port));
    // Step 12.5: stall watchdog — marks Running tasks Stalled after silence
    tokio::spawn(stall_watchdog(tasks_for_watchdog, stall_timeout_secs));

    axum::serve(listener, app).await.ok();
    DASHBOARD_RUNNING.store(false, Ordering::SeqCst);
}

// ============================================================================
// Singleton Lock + Named Pipe
// ============================================================================

const PIPE_NAME: &str = r"\\.\pipe\cpc-manager";

fn lock_path() -> String {
    format!(r"{}\manager.lock", default_data_dir())
}

/// Try to acquire exclusive lock. Returns the lock file handle on success.
/// The lock is held as long as the file handle is open.
fn try_acquire_lock() -> Option<std::fs::File> {
    use std::os::windows::io::AsRawHandle;
    std::fs::create_dir_all(default_data_dir()).ok();
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(lock_path())
        .ok()?;

    // Use Windows LockFileEx for exclusive non-blocking lock
    let handle = file.as_raw_handle();
    let result = unsafe {
        use std::os::windows::io::RawHandle;
        #[allow(non_snake_case, clippy::upper_case_acronyms)] // Win32 API name
        #[repr(C)]
        struct OVERLAPPED {
            Internal: usize,
            InternalHigh: usize,
            Offset: u32,
            OffsetHigh: u32,
            hEvent: RawHandle,
        }
        extern "system" {
            fn LockFileEx(
                hFile: RawHandle,
                dwFlags: u32,
                dwReserved: u32,
                nNumberOfBytesToLockLow: u32,
                nNumberOfBytesToLockHigh: u32,
                lpOverlapped: *mut OVERLAPPED,
            ) -> i32;
        }
        const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x02;
        const LOCKFILE_FAIL_IMMEDIATELY: u32 = 0x01;
        let mut ov = OVERLAPPED {
            Internal: 0,
            InternalHigh: 0,
            Offset: 0,
            OffsetHigh: 0,
            hEvent: std::ptr::null_mut(),
        };
        LockFileEx(
            handle,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut ov,
        )
    };

    if result != 0 {
        // Write our PID to the lock file
        use std::io::Write as _;
        let mut f = &file;
        let _ = writeln!(f, "{}", std::process::id());
        Some(file)
    } else {
        None // Lock busy — another instance is primary
    }
}

/// Run as a pipe proxy: forward stdin to the primary instance's named pipe, forward responses to stdout.
fn run_as_proxy() -> ! {
    use std::io::{BufRead, Write as _};
    info!("Running as proxy — forwarding to primary manager via named pipe");

    // Connect to the named pipe (retry briefly in case primary is still starting)
    let pipe = {
        let mut attempts = 0;
        loop {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(PIPE_NAME)
            {
                Ok(f) => break f,
                Err(e) => {
                    attempts += 1;
                    if attempts > 10 {
                        eprintln!("Failed to connect to primary manager pipe: {}", e);
                        std::process::exit(1);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
        }
    };

    let pipe_reader = std::io::BufReader::new(pipe.try_clone().expect("pipe clone"));
    let mut pipe_writer = pipe.try_clone().expect("pipe clone write");

    // Spawn thread to read pipe responses and write to stdout
    let stdout_thread = std::thread::spawn(move || {
        let mut stdout = io::stdout();
        for line in pipe_reader.lines() {
            match line {
                Ok(l) => {
                    let _ = writeln!(stdout, "{}", l);
                    let _ = stdout.flush();
                }
                Err(_) => break,
            }
        }
    });

    // Read stdin and forward to pipe
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        match line {
            Ok(l) => {
                if writeln!(pipe_writer, "{}", l).is_err() {
                    break; // Pipe broken
                }
                let _ = pipe_writer.flush();
            }
            Err(_) => break,
        }
    }

    // stdin closed — we're done
    drop(pipe_writer);
    let _ = stdout_thread.join();
    std::process::exit(0);
}

/// Start named pipe server — accepts connections from proxy instances.
/// Each connection gets its own handler thread that processes JSON-RPC requests.
fn start_pipe_server(
    server_tasks: Arc<RwLock<HashMap<String, Task>>>,
    server_config: Arc<RwLock<ServerConfig>>,
    runtime_handle: tokio::runtime::Handle,
) {
    std::thread::spawn(move || {
        use std::os::windows::io::FromRawHandle;
        info!("Named pipe server starting at {}", PIPE_NAME);

        loop {
            // Create a named pipe instance
            let pipe_handle = unsafe {
                extern "system" {
                    fn CreateNamedPipeA(
                        lpName: *const u8,
                        dwOpenMode: u32,
                        dwPipeMode: u32,
                        nMaxInstances: u32,
                        nOutBufferSize: u32,
                        nInBufferSize: u32,
                        nDefaultTimeOut: u32,
                        lpSecurityAttributes: *const u8,
                    ) -> isize;
                    fn ConnectNamedPipe(hNamedPipe: isize, lpOverlapped: *mut u8) -> i32;
                }
                const PIPE_ACCESS_DUPLEX: u32 = 0x00000003;
                const PIPE_TYPE_BYTE: u32 = 0x00000000;
                const PIPE_READMODE_BYTE: u32 = 0x00000000;
                const PIPE_WAIT: u32 = 0x00000000;
                const PIPE_UNLIMITED_INSTANCES: u32 = 255;

                let name = format!("{}\0", PIPE_NAME);
                let handle = CreateNamedPipeA(
                    name.as_ptr(),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    PIPE_UNLIMITED_INSTANCES,
                    65536,
                    65536,
                    0,
                    std::ptr::null(),
                );
                if handle == -1 {
                    warn!("Failed to create named pipe, retrying in 5s");
                    std::thread::sleep(std::time::Duration::from_secs(5));
                    continue;
                }
                // Wait for a client
                ConnectNamedPipe(handle, std::ptr::null_mut());
                handle
            };

            // Wrap handle as a File for reading/writing
            let pipe_file =
                unsafe { std::fs::File::from_raw_handle(pipe_handle as *mut std::ffi::c_void) };
            let tasks_c = server_tasks.clone();
            let config_c = server_config.clone();
            let rt = runtime_handle.clone();

            std::thread::spawn(move || {
                let reader = std::io::BufReader::new(&pipe_file);
                let mut writer = std::io::BufWriter::new(&pipe_file);

                // Create a temporary Server-like handler for this pipe connection
                let proxy_server = Server {
                    tasks: tasks_c,
                    config: config_c,
                    runtime: rt,
                    stdout: Arc::new(Mutex::new(io::stdout())), // not used for pipe responses
                    notifier: Arc::new(RealNotifier),
                    recent_tool_calls: Arc::new(Mutex::new(VecDeque::new())),
                    stdin_pipes: Arc::new(RwLock::new(HashMap::new())),
                };

                for line in reader.lines() {
                    let line = match line {
                        Ok(l) if !l.trim().is_empty() => l,
                        Ok(_) => continue,
                        Err(_) => break,
                    };

                    let request: JsonRpcRequest = match serde_json::from_str(&line) {
                        Ok(r) => r,
                        Err(e) => {
                            let err_resp = json!({
                                "jsonrpc": "2.0", "id": null,
                                "error": {"code": -32700, "message": format!("Parse error: {}", e)}
                            });
                            let _ =
                                writeln!(writer, "{}", serde_json::to_string(&err_resp).unwrap());
                            let _ = writer.flush();
                            continue;
                        }
                    };

                    let response = match request.method.as_str() {
                        "initialize" => json!({
                            "jsonrpc": "2.0", "id": request.id,
                            "result": {
                                "protocolVersion": "2024-11-05",
                                "serverInfo": {"name": "manager", "version": MANAGER_VERSION},
                                "capabilities": {"tools": {}}
                            }
                        }),
                        // JSON-RPC 2.0: notifications MUST NOT receive responses
                        method if method.starts_with("notifications/") => continue,
                        "tools/list" => json!({
                            "jsonrpc": "2.0", "id": request.id,
                            "result": get_tools_list()
                        }),
                        "tools/call" => {
                            let params = request.params.unwrap_or(json!({}));
                            let tool_name =
                                params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                            let tool_args = params.get("arguments").cloned().unwrap_or(json!({}));
                            match handle_tool_call(&proxy_server, tool_name, tool_args) {
                                Ok(result) => json!({
                                    "jsonrpc": "2.0", "id": request.id,
                                    "result": {
                                        "content": [{"type": "text", "text": serde_json::to_string_pretty(&result).unwrap()}],
                                        "isError": false
                                    }
                                }),
                                Err(e) => json!({
                                    "jsonrpc": "2.0", "id": request.id,
                                    "result": {
                                        "content": [{"type": "text", "text": format!("Error: {}", e)}],
                                        "isError": true
                                    }
                                }),
                            }
                        }
                        // Safety net: null id means notification — never respond
                        _ if request.id.is_null() => continue,
                        _ => json!({"jsonrpc": "2.0", "id": request.id, "result": {}}),
                    };

                    let _ = writeln!(writer, "{}", serde_json::to_string(&response).unwrap());
                    let _ = writer.flush();
                }
                info!("Pipe client disconnected");
            });
        }
    });
}

// ============================================================================
// Main Loop
// ============================================================================

/// MCP-config server key under which manager registers with host configs.
const SERVER_KEY: &str = "universal-manager";
const BINARY_NAME: &str = "manager";

fn print_help() {
    println!("Universal-Ops manager v{}", MANAGER_VERSION);
    println!();
    println!("USAGE:");
    println!("  manager                              Run as MCP server (stdio JSON-RPC)");
    println!("  manager serve                        Same as above");
    println!("  manager install --target <host>      Register with host config as '{}'", SERVER_KEY);
    println!("  manager uninstall --target <host>    Unregister from host config");
    println!("  manager --version                    Print version");
    println!("  manager --help                       Print this help");
    println!();
    install_common::print_install_help(BINARY_NAME, SERVER_KEY);
    println!();
    println!("Repository: https://github.com/AIWander/Universal-Ops");
}

fn main() {
    // CLI subcommand dispatch (added 2026-05-15 for Universal-Ops v1.1.1).
    // Runs before tokio runtime + stdin loop so install/uninstall exit cleanly.
    let cli_args: Vec<String> = std::env::args().collect();
    match cli_args.get(1).map(|s| s.as_str()) {
        Some("--version") | Some("-V") => {
            println!("{} {}", BINARY_NAME, MANAGER_VERSION);
            return;
        }
        Some("--help") | Some("-h") => {
            print_help();
            return;
        }
        Some("install") => match install_common::install(SERVER_KEY, &cli_args[2..]) {
            Ok(()) => return,
            Err(e) => {
                eprintln!("install failed: {e:#}");
                std::process::exit(1);
            }
        },
        Some("uninstall") => match install_common::uninstall(SERVER_KEY, &cli_args[2..]) {
            Ok(()) => return,
            Err(e) => {
                eprintln!("uninstall failed: {e:#}");
                std::process::exit(1);
            }
        },
        // "serve" or no subcommand: fall through to MCP server loop.
        Some("serve") | None => {}
        Some(other) => {
            eprintln!("Unknown subcommand: {}", other);
            print_help();
            std::process::exit(2);
        }
    }

    // Set up tracing
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::new("manager=info"))
        .init();

    info!("Manager MCP v1.0 starting...");

    // === Fix 3: Singleton via lock file + named pipe ===
    // Try to acquire exclusive lock. If another instance holds it,
    // run as a proxy that forwards MCP requests via named pipe.
    let _lock_guard = match try_acquire_lock() {
        Some(lock) => {
            info!(
                "Acquired singleton lock — running as primary instance (PID {})",
                std::process::id()
            );
            lock
        }
        None => {
            info!("Lock busy — running as proxy to primary instance");
            run_as_proxy(); // never returns
        }
    };

    // === Fix 4: Zombie reaper ===
    // Kill orphan manager.exe instances that aren't responding via named pipe.
    // Only the startup manager does this, and only to stale/orphaned instances.
    {
        let my_pid = std::process::id();
        let output = std::process::Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq manager.exe", "/FO", "CSV", "/NH"])
            .output();
        if let Ok(out) = output {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                // CSV format: "manager.exe","1234","Console","1","12,345 K"
                let parts: Vec<&str> = line.split(',').collect();
                if parts.len() >= 2 {
                    let pid_str = parts[1].trim().trim_matches('"');
                    if let Ok(pid) = pid_str.parse::<u32>() {
                        if pid != my_pid {
                            // Try to connect to pipe — if it responds, this is a live instance (shouldn't happen since we hold the lock)
                            let is_orphan = std::fs::OpenOptions::new()
                                .read(true)
                                .write(true)
                                .open(PIPE_NAME)
                                .is_err();
                            if is_orphan {
                                info!("Killing orphan manager.exe PID {} (no pipe response)", pid);
                                let _ = std::process::Command::new("taskkill")
                                    .args(["/PID", &pid.to_string(), "/F"])
                                    .output();
                            }
                        }
                    }
                }
            }
        }
    }

    // Create tokio runtime for async operations
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to create tokio runtime");

    let server = Server::new(runtime.handle().clone());

    // Start named pipe server for proxy instances
    start_pipe_server(
        server.tasks.clone(),
        server.config.clone(),
        runtime.handle().clone(),
    );

    // Spawn live activity walker (5s poll: process tree capture + step progress parsing)
    {
        let walker_tasks = server.tasks.clone();
        runtime.spawn(live_activity_walker(walker_tasks));
    }

    // Universal-Ops 2026-05-15: embedded dashboard disabled; the dashboard binary
    // (crates/dashboard) owns the :9999 port and reads tasks/{id}/state.json directly
    // from disk. The start_dashboard function is kept compiled but unreachable from
    // main; the dashboard_open / dashboard_stop tools fall back to spawning the
    // dashboard binary on demand (TODO phase-3b: wire those tool handlers).
    // To revert: uncomment the block below.
    // {
    //     let handle = runtime.spawn(start_dashboard(DashboardState {
    //         tasks: server.tasks.clone(),
    //         config: server.config.clone(),
    //         recent_tool_calls: server.recent_tool_calls.clone(),
    //     }));
    //     *DASHBOARD_ABORT.lock().unwrap() = Some(handle.abort_handle());
    // }
    let _ = (&server.tasks, &server.config, &server.recent_tool_calls); // satisfy lints; keep state ready for future re-enable

    let stdin = io::stdin();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };

        if line.trim().is_empty() {
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let error_response = JsonRpcErrorResponse {
                    jsonrpc: "2.0".to_string(),
                    id: Value::Null,
                    error: JsonRpcError {
                        code: -32700,
                        message: format!("Parse error: {}", e),
                    },
                };
                server.write_stdout(&serde_json::to_string(&error_response).unwrap());
                continue;
            }
        };

        if request.jsonrpc != "2.0" {
            let error_response = JsonRpcErrorResponse {
                jsonrpc: "2.0".to_string(),
                id: request.id.clone(),
                error: JsonRpcError {
                    code: -32600,
                    message: "Invalid JSON-RPC version".to_string(),
                },
            };
            server.write_stdout(&serde_json::to_string(&error_response).unwrap());
            continue;
        }

        let response = match request.method.as_str() {
            "initialize" => JsonRpcSuccess {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                result: json!({
                    "protocolVersion": "2024-11-05",
                    "serverInfo": {"name": "manager", "version": MANAGER_VERSION},
                    "capabilities": {"tools": {}}
                }),
            },
            // JSON-RPC 2.0: notifications MUST NOT receive responses
            method if method.starts_with("notifications/") => continue,
            "tools/list" => JsonRpcSuccess {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                result: get_tools_list(),
            },
            "tools/call" => {
                let params = request.params.unwrap_or(json!({}));
                let tool_name_s = params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let tool_args = params.get("arguments").cloned().unwrap_or(json!({}));

                // v1.4.4: Progress keepalive for slow tool calls.
                // Prevents Claude Desktop's 60-second watchdog from killing manager
                // by sending periodic notifications while the handler is blocked.
                let progress_token = params
                    .get("_meta")
                    .and_then(|m| m.get("progressToken"))
                    .cloned();
                let needs_progress = matches!(
                    tool_name_s.as_str(),
                    "task_submit"
                        | "submit_task"
                        | "task_run_parallel"
                        | "run_workflow"
                        | "session_start"
                );
                let progress_cancel = Arc::new(AtomicBool::new(false));
                if needs_progress {
                    let stdout_c = server.stdout.clone();
                    let cancel_c = progress_cancel.clone();
                    let token_c = progress_token;
                    server.runtime.spawn(async move {
                        let mut tick = 1u64;
                        loop {
                            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                            if cancel_c.load(Ordering::Relaxed) {
                                break;
                            }
                            let notif = if let Some(ref token) = token_c {
                                json!({
                                    "jsonrpc": "2.0",
                                    "method": "notifications/progress",
                                    "params": {
                                        "progressToken": token,
                                        "progress": tick,
                                        "total": 0
                                    }
                                })
                            } else {
                                json!({
                                    "jsonrpc": "2.0",
                                    "method": "notifications/message",
                                    "params": {
                                        "level": "info",
                                        "logger": "manager",
                                        "data": format!("Processing... ({}s elapsed)", tick * 10)
                                    }
                                })
                            };
                            if let Ok(mut out) = stdout_c.lock() {
                                let _ = writeln!(
                                    out,
                                    "{}",
                                    serde_json::to_string(&notif).unwrap_or_default()
                                );
                                let _ = out.flush();
                            }
                            tick += 1;
                        }
                    });
                }

                let tc_start = std::time::Instant::now();
                let result = handle_tool_call(&server, &tool_name_s, tool_args);
                progress_cancel.store(true, Ordering::Relaxed);
                let tc_ms = tc_start.elapsed().as_millis() as u64;
                {
                    let entry = ToolCallEntry {
                        tool_name: tool_name_s.clone(),
                        timestamp_utc: Utc::now().to_rfc3339(),
                        session_id: None,
                        task_id: None,
                        duration_ms: tc_ms,
                    };
                    let mut ring = server.recent_tool_calls.lock().unwrap();
                    ring.push_back(entry);
                    if ring.len() > 50 {
                        ring.pop_front();
                    }
                }

                match result {
                    Ok(result) => JsonRpcSuccess {
                        jsonrpc: "2.0".to_string(),
                        id: request.id,
                        result: json!({
                            "content": [{"type": "text", "text": serde_json::to_string_pretty(&result).unwrap()}],
                            "isError": false
                        }),
                    },
                    Err(e) => JsonRpcSuccess {
                        jsonrpc: "2.0".to_string(),
                        id: request.id,
                        result: json!({
                            "content": [{"type": "text", "text": format!("Error: {}", e)}],
                            "isError": true
                        }),
                    },
                }
            }
            // Safety net: null id means notification — never respond
            _ if request.id.is_null() => continue,
            _ => JsonRpcSuccess {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                result: json!({}),
            },
        };

        server.write_stdout(&serde_json::to_string(&response).unwrap());
    }
}

// ============================================================================
// Session Management (absorbed from claude-bridge/claude-runner)
// ============================================================================

const LEGACY_SESSION_DIR: &str = r"C:\temp\manager-sessions";

/// Returns true if `dir` contains at least one session subdirectory with a meta.json.
fn has_session_data(dir: &std::path::Path) -> bool {
    dir.read_dir()
        .map(|mut d| {
            d.any(|e| {
                e.ok()
                    .map(|e| e.path().join("meta.json").exists())
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// Testable inner resolver — takes `legacy` as parameter so tests can inject tempdirs.
pub(crate) fn _resolve_session_dir(legacy: &std::path::Path) -> Result<std::path::PathBuf, String> {
    if legacy.exists() && has_session_data(legacy) {
        return Ok(legacy.to_path_buf());
    }
    cpc_paths::data_path("manager").map_err(|e| e.to_string())
}

static _SESSION_DIR: Lazy<String> =
    Lazy::new(
        || match _resolve_session_dir(std::path::Path::new(LEGACY_SESSION_DIR)) {
            Ok(p) => p.to_string_lossy().into_owned(),
            Err(_) => LEGACY_SESSION_DIR.to_string(),
        },
    );

fn session_dir() -> &'static str {
    &_SESSION_DIR
}

fn handle_start_session(server: &Server, args: Value) -> Result<Value, String> {
    let prompt = args
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'prompt'")?;
    let working_dir = args
        .get("working_dir")
        .and_then(|v| v.as_str())
        .unwrap_or(r"C:\");
    let model = args.get("model").and_then(|v| v.as_str());
    let allow_duplicate = args
        .get("allow_duplicate")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // v1.2.6: session notification flags
    let notify_on_complete = args.get("notify_on_complete").and_then(|v| v.as_bool());
    let notify_on_fail = args.get("notify_on_fail").and_then(|v| v.as_bool());
    let notify_on_destroy = args.get("notify_on_destroy").and_then(|v| v.as_bool());
    let notify_title = args
        .get("notify_title")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let notify_body = args
        .get("notify_body")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let effort = args
        .get("effort")
        .and_then(|v| v.as_str())
        .map(String::from);

    // v1.2.3: Fingerprint dedup for sessions (same logic as task_submit)
    let fp = compute_task_fingerprint(&Backend::ClaudeCode, prompt, Some(working_dir));
    if !allow_duplicate {
        let store = server.runtime.block_on(server.tasks.read());
        let match_task = store.values().find(|t| {
            t.fingerprint.as_deref() == Some(fp.as_str())
                && matches!(t.status, TaskStatus::Running | TaskStatus::Queued)
                && t.superseded_by.is_none()
                && t.id.starts_with("ses_")
        });
        if let Some(existing) = match_task {
            let last_act = existing.last_activity.unwrap_or(existing.created_at);
            let stale_secs = (Utc::now() - last_act).num_seconds();
            if stale_secs <= 120 {
                return Ok(json!({
                    "status": "duplicate",
                    "existing_session_id": existing.id,
                    "message": format!("Duplicate of active session {} (last activity {}s ago). Use allow_duplicate: true to override.", existing.id, stale_secs),
                }));
            }
            // Stalled — mark old superseded
            let old_id = existing.id.clone();
            drop(store);
            let mut wstore = server.runtime.block_on(server.tasks.write());
            if let Some(old_task) = wstore.get_mut(&old_id) {
                old_task.superseded_by = Some("pending".to_string());
                old_task.watchdog_observations.push(format!(
                    "[{}] Stalled {}s — superseded by new session",
                    Utc::now().format("%H:%M:%S"),
                    stale_secs
                ));
                Server::persist_task(old_task);
            }
            drop(wstore);
        } else {
            drop(store);
        }
    }

    let session_id = format!("ses_{}", &uuid::Uuid::new_v4().to_string()[..8]);

    // Create session directory
    let session_path = format!("{}\\{}", session_dir(), session_id);
    let _ = std::fs::create_dir_all(&session_path);

    // Build args - use -p for first prompt, store session for continuation
    let mut cli_args = vec![
        "-p".to_string(),
        prompt.to_string(),
        "--dangerously-skip-permissions".to_string(),
        "--verbose".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--add-dir".to_string(),
        working_dir.to_string(),
        "--add-dir".to_string(),
        r"C:\temp".to_string(),
        "--add-dir".to_string(),
        volumes_dir(),
        "--add-dir".to_string(),
        workspace_root(),
    ];
    if let Some(m) = model {
        cli_args.push("--model".to_string());
        cli_args.push(m.to_string());
    }

    // Submit as managed task so we can track it
    let task_id = session_id.clone();
    let tasks_bg = server.tasks.clone();
    let tid = task_id.clone();

    // Create task entry
    {
        let mut store = server.runtime.block_on(server.tasks.write());
        // Link stalled duplicate if any
        if !allow_duplicate {
            let stalled_id: Option<String> = store
                .values()
                .find(|t| {
                    t.fingerprint.as_deref() == Some(fp.as_str())
                        && t.id != task_id
                        && t.id.starts_with("ses_")
                        && matches!(t.status, TaskStatus::Running | TaskStatus::Queued)
                        && t.superseded_by.as_deref() == Some("pending")
                })
                .map(|t| t.id.clone());
            if let Some(old_id) = stalled_id {
                if let Some(old_task) = store.get_mut(&old_id) {
                    old_task.superseded_by = Some(task_id.clone());
                    Server::persist_task(old_task);
                }
            }
        }
        store.insert(
            task_id.clone(),
            Task {
                id: task_id.clone(),
                backend: Backend::ClaudeCode,
                prompt: prompt.to_string(),
                status: TaskStatus::Running,
                output: String::new(),
                error: None,
                system_prompt: None,
                model: None,
                working_dir: Some(working_dir.to_string()),
                created_at: chrono::Utc::now(),
                started_at: Some(chrono::Utc::now()),
                completed_at: None,
                progress_lines: 0,
                steps: Vec::new(),
                last_activity: Some(chrono::Utc::now()),
                last_output_chunk_at: None,
                stall_detected: false,
                extraction_status: ExtractionStatus::None,
                trust_score: 0,
                trust_level: TrustLevel::Low,
                rollback_path: None,
                validation_status: ValidationStatus::NotChecked,
                assertions: Vec::new(),
                backed_up_files: Vec::new(),
                retry_count: 0,
                max_retries: 2,
                retry_of: None,
                error_context: None,
                input_tokens: 0,
                output_tokens: 0,
                cost_usd: 0.0,
                on_complete: None,
                role: None,
                save_artifact: false,
                rerun_of: None,
                parent_task_id: None,
                forked_from: None,
                continuation_of: None,
                child_pid: None,
                log_path: None,
                log_offset: 0,
                reconnected: false,
                watchdog_observations: Vec::new(),
                fingerprint: Some(fp.clone()),
                superseded_by: None,
                label: args.get("label").and_then(|v| v.as_str()).map(String::from),
                current_step: None,
                total_steps: None,
                current_step_desc: None,
                live_activity: None,
                effort: effort.clone(),
                notify_on_complete: None,
                notify_on_fail: None,
                notify_on_destroy: None,
                notify_title: None,
                notify_body: None,
            },
        );
    }

    server.runtime.spawn(run_cli_task(
        tasks_bg.clone(),
        tid,
        claude_code_cmd(),
        cli_args,
        Some(server.stdin_pipes.clone()),
        StdinMode::Piped,
    ));

    // v1.2.3: Spawn session heartbeat — updates alive/pid in meta.json and task store every 30s
    // v1.2.6: pass notifier so heartbeat can fire toast on session completion/failure
    let hb_session_id = session_id.clone();
    let hb_session_path = session_path.clone();
    let hb_tasks = server.tasks.clone();
    let hb_notifier = Arc::clone(&server.notifier);
    server.runtime.spawn(async move {
        session_heartbeat(hb_session_id, hb_session_path, hb_tasks, hb_notifier).await;
    });

    // Save session metadata (now includes prompt and pid placeholder — heartbeat fills real pid)
    // v1.2.6: persist notify flags so they survive manager restarts
    let meta = serde_json::json!({
        "session_id": session_id,
        "working_dir": working_dir,
        "model": model,
        "prompt": prompt,
        "created_at": chrono::Utc::now().to_rfc3339(),
        "pid": null,
        "alive": true,
        "last_heartbeat": chrono::Utc::now().to_rfc3339(),
        "notify_on_complete": notify_on_complete,
        "notify_on_fail": notify_on_fail,
        "notify_on_destroy": notify_on_destroy,
        "notify_title": notify_title,
        "notify_body": notify_body,
    });
    let _ = std::fs::write(
        format!("{}\\meta.json", session_path),
        serde_json::to_string_pretty(&meta).unwrap_or_default(),
    );

    Ok(json!({
        "session_id": session_id,
        "task_id": session_id,
        "status": "running",
        "message": "Session started. Use send(task_id, message) for follow-ups, or get_status/get_output to check."
    }))
}

// ============================================================================
// Session notification helpers — v1.2.6
// ============================================================================

#[derive(Debug, Clone, Copy)]
enum NotifyReason {
    Completed,
    Failed,
    Destroyed,
}

fn format_duration(created_at_rfc3339: Option<&str>) -> String {
    created_at_rfc3339
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| {
            let secs = (Utc::now() - dt.with_timezone(&Utc)).num_seconds().max(0);
            if secs < 60 {
                format!("{}s", secs)
            } else if secs < 3600 {
                format!("{}m {}s", secs / 60, secs % 60)
            } else {
                format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
            }
        })
        .unwrap_or_else(|| "unknown duration".to_string())
}

fn fire_notify_for_session(
    session_id: &str,
    created_at: Option<&str>,
    notify_title_override: Option<&str>,
    notify_body_override: Option<&str>,
    reason: NotifyReason,
    notifier: &dyn SessionNotifier,
) {
    let duration = format_duration(created_at);
    let (default_title, default_body) = match reason {
        NotifyReason::Completed => (
            "Session complete".to_string(),
            format!("{} finished in {}", session_id, duration),
        ),
        NotifyReason::Failed => (
            "Session failed".to_string(),
            format!("{} died unexpectedly after {}", session_id, duration),
        ),
        NotifyReason::Destroyed => (
            "Session destroyed".to_string(),
            format!("{} was manually destroyed", session_id),
        ),
    };
    // v1.3.5: only honor user-supplied title/body for Completed (same logic as fire_notify_for_task)
    let (title, body) = match reason {
        NotifyReason::Completed => (
            notify_title_override.unwrap_or(&default_title),
            notify_body_override.unwrap_or(&default_body),
        ),
        NotifyReason::Failed | NotifyReason::Destroyed => {
            (default_title.as_str(), default_body.as_str())
        }
    };
    let icon = match reason {
        NotifyReason::Completed => "info",
        NotifyReason::Failed => "error",
        NotifyReason::Destroyed => "warning",
    };
    if let Err(e) = notifier.notify_with_icon(title, body, icon) {
        eprintln!("[manager] notify failed for session {}: {}", session_id, e);
    }
}

/// Check meta flags and fire notify as appropriate. Extracted for unit-testability.
fn check_and_fire_session_notify(
    session_id: &str,
    meta: &Value,
    task_status: &TaskStatus,
    notifier: &dyn SessionNotifier,
) {
    let exit_was_normal = matches!(task_status, TaskStatus::Done);
    let notify_complete = meta
        .get("notify_on_complete")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let notify_fail = meta
        .get("notify_on_fail")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let created_at = meta.get("created_at").and_then(|v| v.as_str());
    let title_ov = meta.get("notify_title").and_then(|v| v.as_str());
    let body_ov = meta.get("notify_body").and_then(|v| v.as_str());

    if notify_complete && exit_was_normal {
        fire_notify_for_session(
            session_id,
            created_at,
            title_ov,
            body_ov,
            NotifyReason::Completed,
            notifier,
        );
    }
    if notify_fail && !exit_was_normal {
        fire_notify_for_session(
            session_id,
            created_at,
            title_ov,
            body_ov,
            NotifyReason::Failed,
            notifier,
        );
    }
}

/// v1.3.1: Fire a toast notification for a task event.
fn fire_notify_for_task(
    task_id: &str,
    created_at: &DateTime<Utc>,
    notify_title_override: Option<&str>,
    notify_body_override: Option<&str>,
    reason: NotifyReason,
    notifier: &dyn SessionNotifier,
) {
    let elapsed = Utc::now().signed_duration_since(*created_at);
    let mins = elapsed.num_minutes();
    let secs = elapsed.num_seconds() % 60;
    let duration = if mins > 0 {
        format!("{}m {}s", mins, secs)
    } else {
        format!("{}s", secs)
    };
    let (default_title, default_body) = match reason {
        NotifyReason::Completed => (
            "Task complete".to_string(),
            format!("Task {} finished in {}", task_id, duration),
        ),
        NotifyReason::Failed => (
            "Task failed".to_string(),
            format!("Task {} failed after {}", task_id, duration),
        ),
        NotifyReason::Destroyed => (
            "Task cancelled".to_string(),
            format!("Task {} was cancelled", task_id),
        ),
    };
    // v1.3.5: only honor user-supplied title/body for Completed. For Failed/Destroyed, use defaults
    // (user's custom label was set expecting success; applying it to failure is misleading).
    let (title, body) = match reason {
        NotifyReason::Completed => (
            notify_title_override.unwrap_or(&default_title),
            notify_body_override.unwrap_or(&default_body),
        ),
        NotifyReason::Failed | NotifyReason::Destroyed => {
            (default_title.as_str(), default_body.as_str())
        }
    };
    // v1.3.5: pick icon per reason so failed tasks show [Error] instead of [Info]
    let icon = match reason {
        NotifyReason::Completed => "info",
        NotifyReason::Failed => "error",
        NotifyReason::Destroyed => "warning",
    };
    if let Err(e) = notifier.notify_with_icon(title, body, icon) {
        eprintln!("[manager] notify failed for task {}: {}", task_id, e);
    }
}

/// v1.3.1: Check task notify flags and fire as appropriate. Extracted for unit-testability.
fn check_and_fire_task_notify(task: &Task, notifier: &dyn SessionNotifier) {
    let exit_was_normal = matches!(task.status, TaskStatus::Done);
    let notify_complete = task.notify_on_complete.unwrap_or(false);
    let notify_fail = task.notify_on_fail.unwrap_or(false);

    if notify_complete && exit_was_normal {
        fire_notify_for_task(
            &task.id,
            &task.created_at,
            task.notify_title.as_deref(),
            task.notify_body.as_deref(),
            NotifyReason::Completed,
            notifier,
        );
    }
    if notify_fail && !exit_was_normal {
        fire_notify_for_task(
            &task.id,
            &task.created_at,
            task.notify_title.as_deref(),
            task.notify_body.as_deref(),
            NotifyReason::Failed,
            notifier,
        );
    }
}

/// v1.2.3: Session heartbeat — polls task store every 30s to sync pid/alive into meta.json.
async fn session_heartbeat(
    session_id: String,
    session_path: String,
    tasks: Arc<RwLock<HashMap<String, Task>>>,
    notifier: Arc<dyn SessionNotifier>,
) {
    let meta_file = format!("{}\\meta.json", session_path);
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;

        let store = tasks.read().await;
        let task = match store.get(&session_id) {
            Some(t) => t.clone(),
            None => break,
        };
        drop(store);

        // Session is terminal — write final state, fire notify if requested, and stop
        if matches!(
            task.status,
            TaskStatus::Done | TaskStatus::Failed | TaskStatus::Cancelled
        ) {
            if let Ok(content) = std::fs::read_to_string(&meta_file) {
                if let Ok(mut meta) = serde_json::from_str::<Value>(&content) {
                    // v1.2.6: fire toast before writing final state (while meta still has notify flags)
                    check_and_fire_session_notify(
                        &session_id,
                        &meta,
                        &task.status,
                        notifier.as_ref(),
                    );
                    meta["alive"] = json!(false);
                    meta["pid"] = json!(task.child_pid);
                    meta["last_heartbeat"] = json!(Utc::now().to_rfc3339());
                    let _ = std::fs::write(
                        &meta_file,
                        serde_json::to_string_pretty(&meta).unwrap_or_default(),
                    );
                }
            }
            break;
        }

        // Update meta.json with current pid and alive status
        if let Ok(content) = std::fs::read_to_string(&meta_file) {
            if let Ok(mut meta) = serde_json::from_str::<Value>(&content) {
                let pid = task.child_pid;
                let alive = pid
                    .map(|p| {
                        std::process::Command::new("tasklist")
                            .args(["/FI", &format!("PID eq {}", p), "/NH"])
                            .output()
                            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&p.to_string()))
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                meta["pid"] = json!(pid);
                meta["alive"] = json!(alive);
                meta["last_heartbeat"] = json!(Utc::now().to_rfc3339());
                let _ = std::fs::write(
                    &meta_file,
                    serde_json::to_string_pretty(&meta).unwrap_or_default(),
                );
            }
        }
    }
}

/// v1.2.3: session_destroy — kill process tree and clean up session.
/// DEPRECATED: session_destroy now routes through handle_cancel_task via dispatch alias.
#[allow(dead_code)]
fn handle_session_destroy(server: &Server, args: Value) -> Result<Value, String> {
    let session_id = args
        .get("session_id")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'session_id'")?;

    // v1.2.6: fire notify_on_destroy before killing, so the toast has time to show
    let meta_file = format!("{}\\{}\\meta.json", session_dir(), session_id);
    if let Ok(content) = std::fs::read_to_string(&meta_file) {
        if let Ok(meta) = serde_json::from_str::<Value>(&content) {
            if meta
                .get("notify_on_destroy")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                fire_notify_for_session(
                    session_id,
                    meta.get("created_at").and_then(|v| v.as_str()),
                    meta.get("notify_title").and_then(|v| v.as_str()),
                    meta.get("notify_body").and_then(|v| v.as_str()),
                    NotifyReason::Destroyed,
                    server.notifier.as_ref(),
                );
            }
        }
    }

    // Kill child process tree via task store
    let mut killed_tree = vec![];
    {
        let mut store = server.runtime.block_on(server.tasks.write());
        if let Some(task) = store.get_mut(session_id) {
            if matches!(task.status, TaskStatus::Running | TaskStatus::Queued) {
                if let Some(root_pid) = task.child_pid {
                    killed_tree = kill_process_tree(root_pid);
                }
                task.status = TaskStatus::Cancelled;
                task.completed_at = Some(Utc::now());
                task.error = Some("Session destroyed by user".into());
                if !killed_tree.is_empty() {
                    task.watchdog_observations.push(format!(
                        "[{}] session_destroy killed process tree: {:?}",
                        Utc::now().format("%H:%M:%S"),
                        killed_tree
                    ));
                }
                Server::persist_task(task);
                Server::save_to_history(task);
            }
        }
    }

    // Update meta.json
    let meta_file = format!("{}\\{}\\meta.json", session_dir(), session_id);
    if let Ok(content) = std::fs::read_to_string(&meta_file) {
        if let Ok(mut meta) = serde_json::from_str::<Value>(&content) {
            meta["alive"] = json!(false);
            meta["destroyed_at"] = json!(Utc::now().to_rfc3339());
            let _ = std::fs::write(
                &meta_file,
                serde_json::to_string_pretty(&meta).unwrap_or_default(),
            );
        }
    }

    Ok(json!({
        "session_id": session_id,
        "status": "destroyed",
        "killed_tree": killed_tree,
        "message": if killed_tree.is_empty() {
            "Session destroyed (no child process to kill).".to_string()
        } else {
            format!("Session destroyed. Killed {} processes.", killed_tree.len())
        }
    }))
}

/// Send a follow-up message to a running task's stdin pipe.
fn handle_send(server: &Server, args: Value) -> Result<Value, String> {
    let task_id = args
        .get("task_id")
        .or_else(|| args.get("session_id"))
        .and_then(|v| v.as_str())
        .ok_or("Missing 'task_id' or 'session_id'")?;
    let message = args
        .get("message")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'message'")?;

    // Check task exists and is running
    {
        let store = server.runtime.block_on(server.tasks.read());
        let task = store
            .get(task_id)
            .ok_or(format!("Task '{}' not found", task_id))?;
        if !matches!(task.status, TaskStatus::Running) {
            return Err(format!(
                "Task '{}' is {} — can only send to running tasks",
                task_id, task.status
            ));
        }
    }

    // Write to stdin pipe
    let pipes = server.runtime.block_on(server.stdin_pipes.read());
    let pipe = pipes
        .get(task_id)
        .ok_or(format!(
            "No stdin pipe for task '{}' — process may not support follow-ups",
            task_id
        ))?
        .clone();
    drop(pipes);

    let msg = format!("{}\n", message);
    server.runtime.block_on(async {
        let mut pipe = pipe.lock().await;
        use tokio::io::AsyncWriteExt;
        pipe.write_all(msg.as_bytes())
            .await
            .map_err(|e| format!("Failed to write to stdin: {}", e))?;
        pipe.flush()
            .await
            .map_err(|e| format!("Failed to flush stdin: {}", e))
    })?;

    Ok(json!({
        "sent": true,
        "task_id": task_id,
        "bytes": msg.len()
    }))
}

/// DEPRECATED: session_send now routes through handle_send via dispatch alias.
#[allow(dead_code)]
fn handle_send_to_session(server: &Server, args: Value) -> Result<Value, String> {
    let session_id = args
        .get("session_id")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'session_id'")?;
    let message = args
        .get("message")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'message'")?;

    // Read session metadata for working_dir and model
    let meta_path = format!("{}\\{}\\meta.json", session_dir(), session_id);
    let meta: Value = std::fs::read_to_string(&meta_path)
        .map(|s| serde_json::from_str(&s).unwrap_or(json!({})))
        .unwrap_or(json!({}));

    let working_dir = meta
        .get("working_dir")
        .and_then(|v| v.as_str())
        .unwrap_or(r"C:\");
    let model = meta.get("model").and_then(|v| v.as_str());

    // Item 6: Route by backend stored in session meta
    let backend_str = meta
        .get("backend")
        .and_then(|v| v.as_str())
        .unwrap_or("claude_code");
    let (exe, cli_args, backend_enum): (&str, Vec<String>, Backend) = match backend_str {
        "codex" => {
            let mut a = vec![
                "exec".to_string(),
                "resume".to_string(),
                "--last".to_string(),
            ];
            if let Some(m) = model {
                a.push("--model".to_string());
                a.push(m.to_string());
            }
            a.push("--json".to_string());
            a.push("--cd".to_string());
            a.push(working_dir.to_string());
            a.push("--skip-git-repo-check".to_string());
            a.push("--full-auto".to_string());
            a.push(message.to_string());
            (codex_cmd(), a, Backend::Codex)
        }
        "gemini" => {
            let gp = if let Some(bc) = Server::read_breadcrumb_state() {
                format!("{}\n\n{}", bc, message)
            } else {
                message.to_string()
            };
            let mut a = vec![
                gemini_cmd().to_string(),
                "-p".to_string(),
                gp,
                "--yolo".to_string(),
            ];
            if let Some(m) = model {
                a.push("--model".to_string());
                a.push(m.to_string());
            }
            (node_cmd(), a, Backend::Gemini)
        }
        _ => {
            let mut a = vec![
                "-p".to_string(),
                message.to_string(),
                "--continue".to_string(),
                "--dangerously-skip-permissions".to_string(),
                "--verbose".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--add-dir".to_string(),
                working_dir.to_string(),
                "--add-dir".to_string(),
                r"C:\temp".to_string(),
            ];
            if let Some(m) = model {
                a.push("--model".to_string());
                a.push(m.to_string());
            }
            (claude_code_cmd(), a, Backend::ClaudeCode)
        }
    };

    let task_id = format!("{}_turn_{}", session_id, chrono::Utc::now().timestamp());
    let tasks_bg = server.tasks.clone();
    let tid = task_id.clone();

    {
        let mut store = server.runtime.block_on(server.tasks.write());
        store.insert(
            task_id.clone(),
            Task {
                id: task_id.clone(),
                backend: backend_enum.clone(),
                prompt: message.to_string(),
                status: TaskStatus::Running,
                output: String::new(),
                error: None,
                system_prompt: None,
                model: None,
                working_dir: None,
                created_at: chrono::Utc::now(),
                started_at: Some(chrono::Utc::now()),
                completed_at: None,
                progress_lines: 0,
                steps: Vec::new(),
                last_activity: None,
                last_output_chunk_at: None,
                stall_detected: false,
                extraction_status: ExtractionStatus::None,
                trust_score: 0,
                trust_level: TrustLevel::Low,
                rollback_path: None,
                validation_status: ValidationStatus::NotChecked,
                assertions: Vec::new(),
                backed_up_files: Vec::new(),
                retry_count: 0,
                max_retries: 2,
                retry_of: None,
                error_context: None,
                input_tokens: 0,
                output_tokens: 0,
                cost_usd: 0.0,
                on_complete: None,
                role: None,
                save_artifact: false,
                rerun_of: None,
                parent_task_id: None,
                forked_from: None,
                continuation_of: None,
                child_pid: None,
                log_path: None,
                log_offset: 0,
                reconnected: false,
                watchdog_observations: Vec::new(),
                fingerprint: None,
                superseded_by: None,
                label: None,
                current_step: None,
                total_steps: None,
                current_step_desc: None,
                live_activity: None,
                effort: None,
                notify_on_complete: None,
                notify_on_fail: None,
                notify_on_destroy: None,
                notify_title: None,
                notify_body: None,
            },
        );
    }

    match backend_enum {
        Backend::Codex => {
            server.runtime.spawn(run_codex_task(
                tasks_bg,
                tid,
                cli_args,
                working_dir.to_string(),
            ));
        }
        _ => {
            server.runtime.spawn(run_cli_task(
                tasks_bg,
                tid,
                exe,
                cli_args,
                None,
                StdinMode::Null,
            ));
        }
    }

    Ok(json!({
        "task_id": task_id,
        "session_id": session_id,
        "status": "running",
        "message": "Follow-up sent. Use get_status/get_output with task_id to check."
    }))
}

fn handle_open_terminal(args: Value) -> Result<Value, String> {
    let prompt = args.get("prompt").and_then(|v| v.as_str());
    let working_dir = args
        .get("working_dir")
        .and_then(|v| v.as_str())
        .unwrap_or(r"C:\");

    // Build the claude command that runs inside the terminal
    let mut claude_args = format!("\"{}\"", claude_code_cmd());
    if let Some(p) = prompt {
        // Escape double quotes in prompt for cmd
        let escaped = p.replace('"', "\\\"");
        claude_args.push_str(&format!(" -p \"{}\"", escaped));
    }
    claude_args.push_str(" --dangerously-skip-permissions");
    // cmd /K keeps terminal open after claude exits
    let inner_cmd = format!("cmd /K {}", claude_args);

    // Title: task name preview (first 60 chars of prompt, or "Claude Code")
    let title = prompt
        .map(|p| {
            let trimmed: String = p.chars().take(60).collect();
            if p.len() > 60 {
                format!("{}...", trimmed)
            } else {
                trimmed
            }
        })
        .unwrap_or_else(|| "Claude Code".to_string());

    // Try Windows Terminal first, fall back to cmd start
    let (method, result) = {
        let wt_result = std::process::Command::new("wt")
            .args([
                "-w",
                "0",
                "new-tab",
                "--title",
                &title,
                "cmd",
                "/K",
                &claude_args,
            ])
            .current_dir(working_dir)
            .spawn();
        match wt_result {
            Ok(_) => ("wt", Ok(())),
            Err(_) => {
                // Fallback: cmd /C start with title
                let fallback = std::process::Command::new("cmd")
                    .args(["/C", "start", &format!("\"{}\"", title)])
                    .arg(&inner_cmd)
                    .current_dir(working_dir)
                    .spawn();
                match fallback {
                    Ok(_) => ("cmd", Ok(())),
                    Err(e) => ("cmd", Err(format!("Failed to open terminal: {}", e))),
                }
            }
        }
    };

    match result {
        Ok(()) => Ok(json!({
            "success": true,
            "message": format!("Claude Code terminal opened via {}", method),
            "working_dir": working_dir,
            "method": method
        })),
        Err(e) => Err(e),
    }
}

fn handle_gemini_direct(args: Value) -> Result<Value, String> {
    let prompt = args
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'prompt'")?;
    let working_dir = args
        .get("working_dir")
        .and_then(|v| v.as_str())
        .unwrap_or(r"C:\");

    let output = std::process::Command::new(r"C:\Program Files\nodejs\node.exe")
        .args([gemini_cmd(), "--yolo", "-p", prompt])
        .current_dir(working_dir)
        .output()
        .map_err(|e| format!("Gemini CLI failed: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    Ok(json!({
        "success": output.status.success(),
        "output": stdout.trim(),
        "stderr": if stderr.is_empty() { None } else { Some(stderr.trim().to_string()) }
    }))
}

fn handle_codex_exec(args: Value) -> Result<Value, String> {
    let prompt = args
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'prompt'")?;
    let ws = workspace_root();
    let working_dir = args
        .get("working_dir")
        .and_then(|v| v.as_str())
        .unwrap_or(&ws);
    let model = args.get("model").and_then(|v| v.as_str());
    let sandbox = args.get("sandbox").and_then(|v| v.as_str());
    let full_auto = args
        .get("full_auto")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let skip_approvals = args
        .get("skip_approvals")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut cmd = std::process::Command::new(codex_cmd());
    cmd.arg("exec");

    if let Some(m) = model {
        cmd.args(["--model", m]);
    }
    if let Some(s) = sandbox {
        cmd.args(["--sandbox", s]);
    }
    if full_auto {
        cmd.arg("--full-auto");
    }
    if skip_approvals {
        cmd.arg("--dangerously-bypass-approvals-and-sandbox");
    }
    cmd.args(["--cd", working_dir]);
    cmd.arg("--json");
    cmd.arg("--skip-git-repo-check");
    cmd.arg("--");
    cmd.arg(prompt);

    let output = cmd
        .current_dir(working_dir)
        .output()
        .map_err(|e| format!("Codex exec failed: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    Ok(json!({
        "success": output.status.success(),
        "output": stdout.trim(),
        "stderr": if stderr.is_empty() { None } else { Some(stderr.trim().to_string()) },
        "exit_code": output.status.code()
    }))
}

fn handle_codex_review(args: Value) -> Result<Value, String> {
    let prompt = args.get("prompt").and_then(|v| v.as_str());
    let ws = workspace_root();
    let working_dir = args
        .get("working_dir")
        .and_then(|v| v.as_str())
        .unwrap_or(&ws);
    let base = args.get("base").and_then(|v| v.as_str());
    let uncommitted = args
        .get("uncommitted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let commit = args.get("commit").and_then(|v| v.as_str());

    let mut cmd = std::process::Command::new(codex_cmd());
    cmd.arg("review");

    if let Some(b) = base {
        cmd.args(["--base", b]);
    }
    if uncommitted {
        cmd.arg("--uncommitted");
    }
    if let Some(sha) = commit {
        cmd.args(["--commit", sha]);
    }
    if let Some(p) = prompt {
        cmd.arg("--");
        cmd.arg(p);
    }

    let output = cmd
        .current_dir(working_dir)
        .output()
        .map_err(|e| format!("Codex review failed: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    Ok(json!({
        "success": output.status.success(),
        "review": stdout.trim(),
        "stderr": if stderr.is_empty() { None } else { Some(stderr.trim().to_string()) },
        "exit_code": output.status.code()
    }))
}

// ============================================================================
// Tests — v1.2.6 session notification hooks
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// In-memory notifier for unit tests — records calls, never fires real toasts.
    struct TestNotifier {
        calls: Mutex<Vec<(String, String)>>,
    }

    impl TestNotifier {
        fn new() -> Self {
            TestNotifier {
                calls: Mutex::new(Vec::new()),
            }
        }
        fn recorded(&self) -> Vec<(String, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl SessionNotifier for TestNotifier {
        fn notify(&self, title: &str, body: &str) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push((title.to_string(), body.to_string()));
            Ok(())
        }
    }

    fn make_meta(flags: serde_json::Value) -> Value {
        let mut base = json!({
            "session_id": "ses_test01",
            "created_at": "2026-04-15T10:00:00Z",
            "notify_on_complete": null,
            "notify_on_fail": null,
            "notify_on_destroy": null,
            "notify_title": null,
            "notify_body": null,
        });
        if let (Value::Object(ref mut b), Value::Object(f)) = (&mut base, flags) {
            for (k, v) in f {
                b.insert(k, v);
            }
        }
        base
    }

    #[test]
    fn notify_on_complete_fires_on_normal_exit() {
        let n = TestNotifier::new();
        let meta = make_meta(json!({"notify_on_complete": true}));
        check_and_fire_session_notify("ses_test01", &meta, &TaskStatus::Done, &n);
        let calls = n.recorded();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].0.contains("complete"),
            "title should contain 'complete': {:?}",
            calls[0].0
        );
        assert!(
            calls[0].1.contains("ses_test01"),
            "body should contain session id"
        );
    }

    #[test]
    fn notify_on_fail_fires_on_crash() {
        let n = TestNotifier::new();
        let meta = make_meta(json!({"notify_on_fail": true}));
        check_and_fire_session_notify("ses_test01", &meta, &TaskStatus::Failed, &n);
        let calls = n.recorded();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].0.contains("failed"),
            "title should contain 'failed': {:?}",
            calls[0].0
        );
    }

    #[test]
    fn notify_on_destroy_fires_on_session_destroy() {
        let n = TestNotifier::new();
        let meta = make_meta(json!({"notify_on_destroy": true}));
        // Simulate the notify path that handle_session_destroy takes
        if meta
            .get("notify_on_destroy")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            fire_notify_for_session(
                "ses_test01",
                meta.get("created_at").and_then(|v| v.as_str()),
                meta.get("notify_title").and_then(|v| v.as_str()),
                meta.get("notify_body").and_then(|v| v.as_str()),
                NotifyReason::Destroyed,
                &n,
            );
        }
        let calls = n.recorded();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].0.contains("destroyed"),
            "title should contain 'destroyed': {:?}",
            calls[0].0
        );
    }

    #[test]
    fn defaults_fire_no_notify() {
        let n = TestNotifier::new();
        // All flags absent (null in JSON → unwrap_or(false))
        let meta = make_meta(json!({}));
        check_and_fire_session_notify("ses_test01", &meta, &TaskStatus::Done, &n);
        check_and_fire_session_notify("ses_test01", &meta, &TaskStatus::Failed, &n);
        assert_eq!(
            n.recorded().len(),
            0,
            "no flags set — no notifications expected"
        );
    }

    #[test]
    fn custom_title_body_overrides_defaults() {
        let n = TestNotifier::new();
        let meta = make_meta(json!({
            "notify_on_complete": true,
            "notify_title": "Custom Title",
            "notify_body": "Custom body text"
        }));
        check_and_fire_session_notify("ses_test01", &meta, &TaskStatus::Done, &n);
        let calls = n.recorded();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "Custom Title");
        assert_eq!(calls[0].1, "Custom body text");
    }

    #[test]
    fn notify_survives_manager_restart_via_meta_persistence() {
        #[allow(unused_imports)]
        use std::io::Write;

        // Write meta.json with notify flags (simulates pre-restart state on disk)
        let tmp = std::env::temp_dir().join("manager_test_ses_restart");
        std::fs::create_dir_all(&tmp).unwrap();
        let meta_path = tmp.join("meta.json");
        let meta_content = json!({
            "session_id": "ses_restart",
            "created_at": "2026-04-15T10:00:00Z",
            "notify_on_complete": true,
            "notify_on_fail": true,
            "notify_title": null,
            "notify_body": null,
        });
        std::fs::write(
            &meta_path,
            serde_json::to_string_pretty(&meta_content).unwrap(),
        )
        .unwrap();

        // Simulate manager restart: read meta fresh from disk
        let loaded_str = std::fs::read_to_string(&meta_path).unwrap();
        let loaded_meta: Value = serde_json::from_str(&loaded_str).unwrap();

        // Verify flags survived disk round-trip
        assert_eq!(
            loaded_meta
                .get("notify_on_complete")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            loaded_meta.get("notify_on_fail").and_then(|v| v.as_bool()),
            Some(true)
        );

        // Simulate heartbeat detecting session died normally
        let n = TestNotifier::new();
        check_and_fire_session_notify("ses_restart", &loaded_meta, &TaskStatus::Done, &n);
        let calls = n.recorded();
        assert_eq!(
            calls.len(),
            1,
            "notify_on_complete should fire after restart"
        );
        assert!(calls[0].0.contains("complete"));

        // Cleanup
        std::fs::remove_dir_all(&tmp).ok();
    }

    // --- v1.3.1: Task-level notify tests ---

    /// Build a minimal Task with optional notify overrides for testing.
    fn make_task(status: TaskStatus, overrides: serde_json::Value) -> Task {
        let mut t = Task {
            id: "tsk_test01".into(),
            backend: Backend::ClaudeCode,
            prompt: "test prompt".into(),
            system_prompt: None,
            model: None,
            working_dir: None,
            status,
            output: String::new(),
            error: None,
            created_at: Utc::now() - chrono::Duration::seconds(30),
            started_at: None,
            completed_at: Some(Utc::now()),
            progress_lines: 0,
            steps: Vec::new(),
            last_activity: None,
            last_output_chunk_at: None,
            stall_detected: false,
            extraction_status: ExtractionStatus::None,
            trust_score: 0,
            trust_level: TrustLevel::Low,
            rollback_path: None,
            validation_status: ValidationStatus::NotChecked,
            assertions: Vec::new(),
            backed_up_files: Vec::new(),
            retry_count: 0,
            max_retries: 2,
            retry_of: None,
            error_context: None,
            input_tokens: 0,
            output_tokens: 0,
            cost_usd: 0.0,
            on_complete: None,
            role: None,
            save_artifact: false,
            rerun_of: None,
            parent_task_id: None,
            forked_from: None,
            continuation_of: None,
            child_pid: None,
            log_path: None,
            log_offset: 0,
            reconnected: false,
            watchdog_observations: Vec::new(),
            fingerprint: None,
            superseded_by: None,
            label: None,
            current_step: None,
            total_steps: None,
            current_step_desc: None,
            live_activity: None,
            effort: None,
            notify_on_complete: None,
            notify_on_fail: None,
            notify_on_destroy: None,
            notify_title: None,
            notify_body: None,
        };
        if let Value::Object(flags) = overrides {
            for (k, v) in flags {
                match k.as_str() {
                    "notify_on_complete" => t.notify_on_complete = v.as_bool(),
                    "notify_on_fail" => t.notify_on_fail = v.as_bool(),
                    "notify_on_destroy" => t.notify_on_destroy = v.as_bool(),
                    "notify_title" => t.notify_title = v.as_str().map(String::from),
                    "notify_body" => t.notify_body = v.as_str().map(String::from),
                    _ => {}
                }
            }
        }
        t
    }

    #[test]
    fn notify_on_complete_fires_on_normal_exit_for_task() {
        let n = TestNotifier::new();
        let task = make_task(TaskStatus::Done, json!({"notify_on_complete": true}));
        check_and_fire_task_notify(&task, &n);
        let calls = n.recorded();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].0.contains("complete"),
            "title should contain 'complete': {:?}",
            calls[0].0
        );
        assert!(
            calls[0].1.contains("tsk_test01"),
            "body should contain task id"
        );
    }

    #[test]
    fn notify_on_fail_fires_on_task_crash() {
        let n = TestNotifier::new();
        let task = make_task(TaskStatus::Failed, json!({"notify_on_fail": true}));
        check_and_fire_task_notify(&task, &n);
        let calls = n.recorded();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].0.contains("failed"),
            "title should contain 'failed': {:?}",
            calls[0].0
        );
    }

    #[test]
    fn notify_on_destroy_fires_on_task_cancel() {
        let n = TestNotifier::new();
        let task = make_task(TaskStatus::Cancelled, json!({"notify_on_destroy": true}));
        // Simulate the cancel path
        if task.notify_on_destroy.unwrap_or(false) {
            fire_notify_for_task(
                &task.id,
                &task.created_at,
                task.notify_title.as_deref(),
                task.notify_body.as_deref(),
                NotifyReason::Destroyed,
                &n,
            );
        }
        let calls = n.recorded();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].0.contains("cancelled"),
            "title should contain 'cancelled': {:?}",
            calls[0].0
        );
    }

    #[test]
    fn defaults_fire_no_notify_for_task() {
        let n = TestNotifier::new();
        // All flags absent (None → unwrap_or(false))
        let done_task = make_task(TaskStatus::Done, json!({}));
        let failed_task = make_task(TaskStatus::Failed, json!({}));
        check_and_fire_task_notify(&done_task, &n);
        check_and_fire_task_notify(&failed_task, &n);
        assert_eq!(
            n.recorded().len(),
            0,
            "no flags set — no notifications expected"
        );
    }

    #[test]
    fn custom_title_body_overrides_for_task() {
        let n = TestNotifier::new();
        let task = make_task(
            TaskStatus::Done,
            json!({
                "notify_on_complete": true,
                "notify_title": "Build Done",
                "notify_body": "Your build finished"
            }),
        );
        check_and_fire_task_notify(&task, &n);
        let calls = n.recorded();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "Build Done");
        assert_eq!(calls[0].1, "Your build finished");
    }

    #[test]
    fn test_session_dir_legacy_path_wins() {
        let dir = tempfile::tempdir().unwrap();
        // Create a session subdir with meta.json — simulates Joe's machine
        let session_sub = dir.path().join("ses_abc123");
        std::fs::create_dir_all(&session_sub).unwrap();
        std::fs::write(session_sub.join("meta.json"), "{}").unwrap();

        let result = _resolve_session_dir(dir.path()).unwrap();
        assert_eq!(
            result,
            dir.path(),
            "legacy session dir with meta.json should be returned"
        );
    }

    #[test]
    fn test_session_dir_no_legacy_falls_through() {
        let dir = tempfile::tempdir().unwrap();
        // Empty tempdir — no session subdirs with meta.json
        assert!(
            !has_session_data(dir.path()),
            "empty dir must not be detected as legacy session data"
        );
    }

    // =========================================================================
    // Tests — v1.2.7 session stdout reconnect (orphan detection on restart)
    // =========================================================================

    /// Simulate the startup task-loading logic for a single task, returning the
    /// final TaskStatus after the restart classification algorithm runs.
    /// `child_alive` controls the fake PID liveness probe result.
    fn classify_on_restart(is_session: bool, had_pid: bool, child_alive: bool) -> TaskStatus {
        let id = if is_session {
            "ses_abc123".to_string()
        } else {
            "task_abc123".to_string()
        };
        let child_pid: Option<u32> = if had_pid { Some(99999) } else { None };
        let mut status = TaskStatus::Running;

        // Mirror the startup logic from Server::new
        let obs1 = format!(
            "[TEST] Manager restarted — task was {} at load time. Child PID: {}",
            status,
            child_pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "unknown".into())
        );
        let _ = obs1; // consumed for logic only

        if child_alive {
            if id.starts_with("ses_") {
                status = TaskStatus::Orphaned;
            }
            // else: keep Running (non-session tasks retain status)
        } else {
            // Dead child (with or without PID) → Failed
            status = TaskStatus::Failed;
        }
        status
    }

    #[test]
    fn session_alive_at_restart_becomes_orphaned() {
        // (a) session was alive at Desktop quit, child still running at startup → orphaned
        let result = classify_on_restart(true, true, true);
        assert_eq!(
            result,
            TaskStatus::Orphaned,
            "alive session after restart must be orphaned (pipes are gone)"
        );
    }

    #[test]
    fn session_dead_at_restart_becomes_failed() {
        // (b) session was alive at Desktop quit, child dead at startup → failed
        let result = classify_on_restart(true, true, false);
        assert_eq!(
            result,
            TaskStatus::Failed,
            "session whose child died during restart must be marked failed"
        );
    }

    #[test]
    fn non_session_alive_at_restart_stays_running() {
        // Non-session task (task_) alive → stays Running (existing behavior unchanged)
        let result = classify_on_restart(false, true, true);
        assert_eq!(
            result,
            TaskStatus::Running,
            "non-session tasks with alive child must stay Running"
        );
    }

    #[test]
    fn breadcrumb_status_line_single_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let bc_dir = tmp.path().join("breadcrumbs");
        std::fs::create_dir_all(&bc_dir).unwrap();
        let index = json!({
            "bc_001": {
                "id": "bc_001",
                "project_id": "myproject",
                "name": "fix something | targets: foo.rs",
                "owner": "claude"
            }
        });
        std::fs::write(
            bc_dir.join("active.index.json"),
            serde_json::to_string(&index).unwrap(),
        )
        .unwrap();

        let line = read_breadcrumb_status_line_from(tmp.path().to_str().unwrap());

        assert!(
            line.starts_with("active:"),
            "single bc must show active:<name>, got: {}",
            line
        );
        assert!(
            line.contains("fix something"),
            "must include bc name, got: {}",
            line
        );
    }

    #[test]
    fn breadcrumb_status_line_multi_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let bc_dir = tmp.path().join("breadcrumbs");
        std::fs::create_dir_all(&bc_dir).unwrap();
        let index = json!({
            "bc_001": {"id": "bc_001", "project_id": "proj_a", "name": "task one", "owner": "claude"},
            "bc_002": {"id": "bc_002", "project_id": "proj_a", "name": "task two", "owner": "claude"},
            "bc_003": {"id": "bc_003", "project_id": "proj_b", "name": "task three", "owner": "claude"}
        });
        std::fs::write(
            bc_dir.join("active.index.json"),
            serde_json::to_string(&index).unwrap(),
        )
        .unwrap();

        let line = read_breadcrumb_status_line_from(tmp.path().to_str().unwrap());

        assert!(
            line.starts_with("3 active"),
            "3 bcs must show count, got: {}",
            line
        );
        assert!(
            line.contains("proj_a: 2"),
            "proj_a has 2 entries, got: {}",
            line
        );
        assert!(
            line.contains("proj_b: 1"),
            "proj_b has 1 entry, got: {}",
            line
        );
    }

    #[test]
    fn breadcrumb_status_line_empty_index() {
        let tmp = tempfile::tempdir().unwrap();
        let bc_dir = tmp.path().join("breadcrumbs");
        std::fs::create_dir_all(&bc_dir).unwrap();
        std::fs::write(bc_dir.join("active.index.json"), "{}").unwrap();

        let line = read_breadcrumb_status_line_from(tmp.path().to_str().unwrap());

        assert_eq!(
            line, "none",
            "empty index must return 'none', got: {}",
            line
        );
    }

    // =========================================================================
    // Tests — v1.2.8 dashboard /api/status and /api/config
    // =========================================================================

    #[test]
    fn api_status_shape_has_required_keys() {
        // Build a minimal in-memory AppState and call dash_api_status synchronously
        use std::sync::Arc;
        use tokio::runtime::Runtime;
        use tokio::sync::RwLock as TokioRwLock;

        let rt = Runtime::new().unwrap();
        let tasks: Arc<TokioRwLock<HashMap<String, Task>>> =
            Arc::new(TokioRwLock::new(HashMap::new()));
        let config = Arc::new(TokioRwLock::new(ServerConfig {
            openai_api_key: None,
            default_gpt_model: DEFAULT_GPT_MODEL.to_string(),
            default_working_dir: ".".to_string(),
        }));

        let state = DashboardState {
            tasks,
            config,
            recent_tool_calls: Arc::new(Mutex::new(VecDeque::new())),
        };

        // Invoke the handler directly via block_on
        let Json(v) = rt.block_on(dash_api_status(axum::extract::State(state)));

        assert!(v.get("version").is_some(), "must have 'version'");
        assert!(v.get("sessions").is_some(), "must have 'sessions'");
        assert!(v.get("tasks").is_some(), "must have 'tasks'");
        assert!(v.get("status_bar").is_some(), "must have 'status_bar'");
        assert!(v.get("timestamp").is_some(), "must have 'timestamp'");

        let sessions = &v["sessions"];
        for key in &["running", "queued", "done", "failed", "orphaned", "total"] {
            assert!(sessions.get(key).is_some(), "sessions must have '{}'", key);
        }
    }

    #[test]
    fn api_config_shape_has_required_keys() {
        use tokio::runtime::Runtime;
        let rt = Runtime::new().unwrap();
        let Json(v) = rt.block_on(dash_api_config());

        assert!(v.get("ports").is_some(), "must have 'ports'");
        assert!(
            v.get("poll_intervals_ms").is_some(),
            "must have 'poll_intervals_ms'"
        );
        assert!(v.get("version").is_some(), "must have 'version'");

        let ports = &v["ports"];
        for key in &["manager", "local", "hands", "workflow", "autonomous"] {
            assert!(ports.get(key).is_some(), "ports must have '{}'", key);
        }
        let intervals = &v["poll_intervals_ms"];
        for key in &["manager", "local", "hands", "workflow", "autonomous"] {
            assert!(
                intervals.get(key).is_some(),
                "poll_intervals_ms must have '{}'",
                key
            );
        }
    }

    #[test]
    fn dashboard_status_tool_reflects_globals() {
        // Reset globals to a known state
        DASHBOARD_PORT.store(0, Ordering::SeqCst);
        DASHBOARD_RUNNING.store(false, Ordering::SeqCst);

        let v = handle_dashboard_status();
        assert_eq!(v["running"], false, "must be not running");
        assert_eq!(v["port"], 0, "port must be 0 when stopped");
        assert_eq!(
            v["url"].as_str().unwrap_or("x"),
            "",
            "url must be empty when stopped"
        );

        // Simulate a running dashboard on port 9100
        DASHBOARD_PORT.store(9100, Ordering::SeqCst);
        DASHBOARD_RUNNING.store(true, Ordering::SeqCst);

        let v2 = handle_dashboard_status();
        assert_eq!(v2["running"], true);
        assert_eq!(v2["port"], 9100);
        assert!(
            v2["url"].as_str().unwrap_or("").contains("9100"),
            "url must contain port"
        );

        // Cleanup
        DASHBOARD_PORT.store(0, Ordering::SeqCst);
        DASHBOARD_RUNNING.store(false, Ordering::SeqCst);
    }

    // =========================================================================
    // Tests — v1.4.4: TaskState serialization round-trip
    // =========================================================================

    #[test]
    fn task_state_serialization_round_trip() {
        let state = TaskState {
            task_id: "abc12345".into(),
            backend: "codex".into(),
            prompt_preview: "Build a CLI tool that...".into(),
            working_dir: Some(r"C:\projects\test".into()),
            child_pid: Some(12345),
            started_at: Some(Utc::now()),
            log_path: Some(r"C:\temp\tasks\abc12345\child.log".into()),
            created_at: Utc::now(),
        };
        let json_str = serde_json::to_string_pretty(&state).unwrap();
        let deserialized: TaskState = serde_json::from_str(&json_str).unwrap();

        assert_eq!(state.task_id, deserialized.task_id);
        assert_eq!(state.backend, deserialized.backend);
        assert_eq!(state.prompt_preview, deserialized.prompt_preview);
        assert_eq!(state.working_dir, deserialized.working_dir);
        assert_eq!(state.child_pid, deserialized.child_pid);
        assert_eq!(state.log_path, deserialized.log_path);
    }

    #[test]
    fn task_state_write_and_read_from_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let state_path = tmp.path().join("state.json");
        let state = TaskState {
            task_id: "disk_test".into(),
            backend: "claude_code".into(),
            prompt_preview: "Test prompt".into(),
            working_dir: None,
            child_pid: None,
            started_at: None,
            log_path: None,
            created_at: Utc::now(),
        };
        let data = serde_json::to_string_pretty(&state).unwrap();
        std::fs::write(&state_path, &data).unwrap();
        let loaded: TaskState =
            serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
        assert_eq!(loaded.task_id, "disk_test");
        assert_eq!(loaded.backend, "claude_code");
    }

    // =========================================================================
    // Tests — v1.4.4: Log-tail parser (codex turn.completed, claude result)
    // =========================================================================

    #[test]
    fn log_parser_finds_codex_turn_completed() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("child.log");
        let content = r#"{"type":"item.created","item":{"type":"agent_message"}}
{"type":"item.completed","item":{"type":"agent_message","text":"Done: created 3 files"}}
{"type":"turn.completed"}
"#;
        std::fs::write(&log, content).unwrap();
        let result = final_status_from_log(log.to_str().unwrap());
        assert!(result.is_some(), "must find turn.completed marker");
        let (status, err) = result.unwrap();
        assert_eq!(status, TaskStatus::Done);
        assert!(err.is_none());
    }

    #[test]
    fn log_parser_finds_claude_result() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("child.log");
        let content = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working..."}]}}
{"type":"result","subtype":"success","result":"All changes applied successfully.","is_error":false}
"#;
        std::fs::write(&log, content).unwrap();
        let result = final_status_from_log(log.to_str().unwrap());
        assert!(result.is_some(), "must find result marker");
        let (status, err) = result.unwrap();
        assert_eq!(status, TaskStatus::Done);
        assert!(err.is_none());
    }

    #[test]
    fn log_parser_truncated_log_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("child.log");
        // Partial log with no completion marker
        let content = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"still working..."}]}}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"xyz"}]}}
some random stderr output
"#;
        std::fs::write(&log, content).unwrap();
        let result = final_status_from_log(log.to_str().unwrap());
        assert!(
            result.is_none(),
            "truncated log must return None, got: {:?}",
            result
        );
    }

    // =========================================================================
    // Tests — v1.4.4: extract_output_from_log
    // =========================================================================

    #[test]
    fn extract_output_codex_agent_message() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("child.log");
        let content = r#"{"type":"item.completed","item":{"type":"agent_message","text":"Created files:\n- foo.rs\n- bar.rs"}}
{"type":"item.completed","item":{"type":"mcp_tool_call","tool":"write"}}
{"type":"item.completed","item":{"type":"agent_message","text":"All done."}}
{"type":"turn.completed"}
"#;
        std::fs::write(&log, content).unwrap();
        let output = extract_output_from_log(log.to_str().unwrap(), &Backend::Codex);
        assert!(output.is_some(), "must extract output from codex log");
        let text = output.unwrap();
        assert!(
            text.contains("Created files"),
            "output must contain agent text"
        );
        assert!(text.contains("All done"), "output must contain final text");
    }

    #[test]
    fn extract_output_claude_result() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("child.log");
        let content = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Analyzing..."}]}}
{"type":"result","subtype":"success","result":"Implemented all requested changes.","is_error":false}
"#;
        std::fs::write(&log, content).unwrap();
        let output = extract_output_from_log(log.to_str().unwrap(), &Backend::ClaudeCode);
        assert!(output.is_some(), "must extract output from claude log");
        let text = output.unwrap();
        assert!(
            text.contains("Implemented all requested changes"),
            "must contain result text"
        );
    }

    #[test]
    fn extract_output_gemini_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("child.log");
        std::fs::write(&log, "some gemini output\n").unwrap();
        let output = extract_output_from_log(log.to_str().unwrap(), &Backend::Gemini);
        assert!(
            output.is_none(),
            "gemini extraction is TBD — must return None"
        );
    }

    // =========================================================================
    // Tests — v1.4.4: Progress notification JSON format
    // =========================================================================

    #[test]
    fn progress_notification_with_token_has_correct_shape() {
        let token = json!("progress_abc123");
        let notif = json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": {
                "progressToken": token,
                "progress": 1,
                "total": 0
            }
        });
        assert_eq!(notif["method"].as_str().unwrap(), "notifications/progress");
        assert_eq!(
            notif["params"]["progressToken"].as_str().unwrap(),
            "progress_abc123"
        );
        // Must NOT have an "id" field (notifications are fire-and-forget)
        assert!(notif.get("id").is_none(), "notification must not have id");
    }

    #[test]
    fn progress_notification_without_token_falls_back_to_message() {
        let notif = json!({
            "jsonrpc": "2.0",
            "method": "notifications/message",
            "params": {
                "level": "info",
                "logger": "manager",
                "data": "Processing... (10s elapsed)"
            }
        });
        assert_eq!(notif["method"].as_str().unwrap(), "notifications/message");
        assert!(
            notif["params"]["data"]
                .as_str()
                .unwrap()
                .contains("Processing"),
            "message must contain status text"
        );
        assert!(notif.get("id").is_none(), "notification must not have id");
    }
}

// === FILE NAVIGATION ===
// Generated: 2026-04-15T20:30:13
// Total: 7447 lines | 133 functions | 18 structs | 21 constants
//
// IMPORTS: axum, chrono, once_cell, serde, serde_json, std, super, sysinfo, tokio, tower_http, tracing, uuid
//
// CONSTANTS:
//   const MAX_HISTORY_ENTRIES: 31
//   const GPT_API_URL: 32
//   const DEFAULT_GPT_MODEL: 34
//   static _GEMINI_CMD: 60
//   static _CLAUDE_CODE_CMD: 67
//   static _CODEX_CMD: 74
//   static _NODE_CMD: 84
//   static _LOAVES_ARCHIVE_DIR: 90
//   const fn: 93
//   const SAFETY_VALIDATION_BLOCK: 1922
//   const CREATE_NO_WINDOW: 4843
//   const PIPE_NAME: 6082
//   const LOCKFILE_EXCLUSIVE_LOCK: 6123
//   const LOCKFILE_FAIL_IMMEDIATELY: 6124
//   const PIPE_ACCESS_DUPLEX: 6230
//   const PIPE_TYPE_BYTE: 6231
//   const PIPE_READMODE_BYTE: 6232
//   const PIPE_WAIT: 6233
//   const PIPE_UNLIMITED_INSTANCES: 6234
//   const LEGACY_SESSION_DIR: 6523
//   static _SESSION_DIR: 6546
//
// STRUCTS:
//   JsonRpcRequest: 162-168
//   JsonRpcSuccess: 171-175
//   JsonRpcErrorResponse: 178-182
//   JsonRpcError: 185-188
//   TaskStep: 286-291
//   Task: 294-364
//   BackendRecommendation: 368-373
//   WorkflowStep: 377-395
//   WorkflowTemplate: 399-417
//   TemplateStep: 425-430
//   ServerConfig: 436-440
//   Server: 442-448
//   ParallelStepResult: 3251-3258
//   CustomRole: 4346-4351
//   pub RealNotifier: 5008-5009
//   DashboardState: 5685-5688
//   PathQuery: 5980-5982
//   TestNotifier: 7183-7185
//
// ENUMS:
//   Backend: 196-201
//   TaskStatus: 216-227
//   ExtractionStatus: 245-252
//   TrustLevel: 260-264
//   ValidationStatus: 272-277
//   NotifyReason: 6731-6731
//
// IMPL BLOCKS:
//   impl std::fmt::Display for Backend: 203-212
//   impl std::fmt::Display for TaskStatus: 229-241
//   impl Default for ExtractionStatus: 254-256
//   impl Default for TrustLevel: 266-268
//   impl Default for ValidationStatus: 279-281
//   impl Server: 450-1111
//   impl SessionNotifier for RealNotifier: 5010-5014
//   impl TestNotifier: 7187-7190
//   impl SessionNotifier for TestNotifier: 7192-7197
//
// FUNCTIONS:
//   default_data_dir: 38-44
//   tasks_dir: 100-100
//   history_dir: 101-101
//   gemini_cmd: 102-102
//   claude_code_cmd: 103-103
//   codex_cmd: 104-104
//   workflow_patterns_dir: 105-105
//   rollback_dir: 106-106
//   learned_patterns_dir: 107-107
//   node_cmd: 108-108
//   loaves_dir: 110-110
//   loaves_archive_dir: 111-111
//   spawn_visible_terminal: 124-155
//   default_max_retries: 283-283
//   spawn_retry_execution: 1114-1160
//   spawn_on_complete: 1164-1217 [med]
//   run_gpt_task: 1223-1372 [LARGE]
//   run_codex_task: 1376-1485 [LARGE]
//   run_cli_task: 1486-1908 [LARGE]
//   safe_truncate: 1915-1920
//   ensure_safety_validation: 1924-1930
//   extract_safety_warning: 1932-1937
//   handle_submit_task: 1939-2267 [LARGE]
//   handle_watch_tasks: 2271-2391 [LARGE]
//   handle_get_status: 2393-2472 [med]
//   handle_get_output: 2474-2506
//   handle_list_tasks: 2508-2570 [med]
//   handle_cancel_task: 2572-2614
//   kill_process_tree: 2618-2653
//   handle_task_poll: 2656-2704
//   build_status_bar: 2707-2729
//   read_breadcrumb_status_line: 2736-2739
//   read_breadcrumb_status_line_from: 2743-2800 [med]
//   read_state_file: 2803-2815
//   read_active_loaf_summary: 2818-2826
//   compute_task_fingerprint: 2829-2838
//   handle_status_bar: 2841-2844
//   handle_pause_task: 2846-2866
//   handle_resume_task: 2868-2896
//   handle_configure: 2898-2932
//   handle_cleanup: 2934-2962
//   run_workflow_step: 2968-3048 [med]
//   handle_run_workflow: 3050-3243 [LARGE]
//   handle_run_parallel: 3260-3409 [LARGE]
//   run_parallel_workflow: 3412-3478 [med]
//   launch_step: 3481-3609 [LARGE]
//   handle_decompose_task: 3615-3693 [med]
//   handle_save_template: 3695-3717
//   handle_list_templates: 3719-3745
//   handle_run_template: 3747-3793
//   handle_explain_task: 3795-3839
//   loaf_path: 3845-3847
//   find_active_loaf: 3850-3869
//   handle_loaf_create: 3871-3909
//   handle_loaf_update: 3911-4047 [LARGE]
//   handle_loaf_status: 4049-4088
//   handle_loaf_close: 4090-4125
//   handle_list_sessions: 4131-4193 [med]
//   handle_get_analytics: 4195-4271 [med]
//   handle_run_analyzer: 4277-4284
//   get_role_prompt: 4290-4330
//   list_roles: 4332-4342
//   custom_roles_dir: 4353-4357
//   load_custom_roles: 4359-4369
//   get_custom_role_prompt: 4371-4376
//   handle_role_list: 4378-4393
//   handle_role_create: 4395-4424
//   handle_role_delete: 4426-4436
//   save_task_artifact: 4442-4474
//   handle_tool_call: 4480-4528
//   handle_review_extractions: 4534-4551
//   handle_extract_workflow: 4553-4572
//   handle_dismiss_extraction: 4574-4582
//   handle_rollback_task: 4584-4590
//   handle_retry_task: 4593-4633
//   handle_task_rerun: 4639-4820 [LARGE]
//   handle_route_task: 4826-4836
//   handle_notify: 4845-4935 [med]
//   do_notify: 4942-5000 [med]
//   notify: 5004-5007
//   get_tools_list: 5020-5678 [LARGE]
//   dash_status: 5690-5705
//   dash_status_by_id: 5707-5720
//   dash_health: 5722-5736
//   dash_inbox: 5738-5757
//   flush_entry: 5759-5766
//   dash_get_prefs: 5768-5775
//   dash_post_prefs: 5777-5786
//   dash_post_task: 5788-5857 [med]
//   dash_cancel: 5859-5875
//   dash_knowledge: 5877-5903
//   dash_git: 5905-5917
//   dash_system: 5919-5942
//   dash_history: 5944-5953
//   volumes_base_path: 5955-5959
//   validate_volumes_path: 5962-5977
//   api_read_file: 5984-6004
//   api_list_dir: 6006-6037
//   start_dashboard: 6039-6076
//   lock_path: 6084-6086
//   try_acquire_lock: 6090-6145 [med]
//   run_as_proxy: 6148-6205 [med]
//   start_pipe_server: 6209-6339 [LARGE]
//   main: 6345-6516 [LARGE]
//   has_session_data: 6526-6536
//   session_dir: 6553-6555
//   handle_start_session: 6557-6724 [LARGE]
//   format_duration: 6733-6743
//   fire_notify_for_session: 6745-6773
//   check_and_fire_session_notify: 6776-6795
//   session_heartbeat: 6798-6843
//   handle_session_destroy: 6846-6910 [med]
//   handle_send_to_session: 6912-7009 [med]
//   handle_open_terminal: 7011-7066 [med]
//   handle_gemini_direct: 7068-7087
//   handle_codex_exec: 7089-7132
//   handle_codex_review: 7134-7171
//   make_meta: 7199-7213
//   notify_on_complete_fires_on_normal_exit: 7216-7224
//   notify_on_fail_fires_on_crash: 7227-7234
//   notify_on_destroy_fires_on_session_destroy: 7237-7254
//   defaults_fire_no_notify: 7257-7264
//   custom_title_body_overrides_defaults: 7267-7279
//   notify_survives_manager_restart_via_meta_persistence: 7282-7316
//   test_session_dir_legacy_path_wins: 7319-7328
//   test_session_dir_no_legacy_falls_through: 7331-7338
//   classify_on_restart: 7347-7368
//   session_alive_at_restart_becomes_orphaned: 7371-7376
//   session_dead_at_restart_becomes_failed: 7379-7384
//   non_session_alive_at_restart_stays_running: 7387-7392
//   breadcrumb_status_line_single_entry: 7395-7414
//   breadcrumb_status_line_multi_entry: 7417-7434
//   breadcrumb_status_line_empty_index: 7437-7446
//
// === END FILE NAVIGATION ===
