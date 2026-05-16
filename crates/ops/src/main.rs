mod agent_identity;
mod config;
pub mod security;
mod tools;

use chrono::Local;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

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
// Server State
// ============================================================================

struct Server {}

impl Server {
    fn new() -> Self {
        Server {}
    }
}

// ============================================================================
// Tool Definitions
// ============================================================================

fn get_tools_list() -> Value {
    let mut all_tools: Vec<serde_json::Value> = tools::get_definitions();
    let ops_native_json = serde_json::json!([
        {
            "name": "mcp_rebuild",
            "description": "Rebuild an MCP server with backup. Backs up exe, kills process, runs cargo build.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {"type": "string", "description": "Server name (e.g., 'local', 'ops')"}
                },
                "required": ["target"]
            }
        },
        {
            "name": "server_health",
            "description": "Check which MCP servers are alive. Returns process status for all registered servers.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "servers": {"type": "array", "items": {"type": "string"}, "description": "Specific servers to check (default: all)"}
                }
            }
        },
        {
            "name": "tool_fallback",
            "description": "Look up fallback tool when primary is unavailable. Returns equivalent tool name from fallback map.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tool": {"type": "string", "description": "Full tool name (e.g., 'ops:breadcrumb_step')"}
                },
                "required": ["tool"]
            }
        },
        {
            "name": "deploy_preflight",
            "description": "Pre-kill safety checks before deploying/rebuilding an MCP server.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {"type": "string", "description": "Server name to deploy"}
                },
                "required": ["target"]
            }
        },
        {
            "name": "deploy_smoke_test",
            "description": "Validate MCP server binaries before packaging. Checks each expected exe exists and is non-zero bytes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "exe_dir": {"type": "string", "description": "Directory containing server .exe files"},
                    "expected": {"type": "array", "items": {"type": "string"}, "description": "List of expected exe names."}
                },
                "required": ["exe_dir"]
            }
        },
        {
            "name": "powershell",
            "description": "Execute PowerShell. Most commands run freely. Destructive commands require explicit user permission: pass `confirm: true` for service/firewall/scheduled-task/registry changes; pass `allow_destructive: true` for format/diskpart/account-deletion/bulk-system-delete operations. Catastrophic patterns are blocked unconditionally.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "PowerShell command to execute"},
                    "timeout_secs": {"type": "integer", "description": "Timeout in seconds (default: 30)"},
                    "allow_destructive": {"type": "boolean", "description": "Allow Tier-3 destructive operations (format, diskpart, account deletion). Requires explicit user permission."},
                    "confirm": {"type": "boolean", "description": "Allow Tier-2 state-changing operations (services, firewall, scheduled tasks, registry). Requires user acknowledgment."}
                },
                "required": ["command"]
            }
        },
        {
            "name": "bash",
            "description": "Execute a bash command via Git Bash. Most commands run freely. Destructive commands require explicit user permission: pass `confirm: true` for service/firewall/scheduled-task/registry changes; pass `allow_destructive: true` for format/account-deletion/bulk-system-delete/dd-to-device operations. Catastrophic patterns (raw disk writes, fork bombs, system-root recursive delete, curl-pipe-shell from network, BitLocker disable) are blocked unconditionally. Working directory defaults to the caller's cwd, or %LOCALAPPDATA%\\Ops.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The bash command to execute. Multi-line commands supported."},
                    "working_dir": {"type": "string", "description": "Optional working directory. Default: current process cwd, falling back to %LOCALAPPDATA%/Ops."},
                    "timeout_secs": {"type": "integer", "description": "Timeout in seconds. Default: 30. Maximum: 600.", "default": 30},
                    "allow_destructive": {"type": "boolean", "description": "Bypass Tier 3 destructive-command guards. Default: false.", "default": false},
                    "confirm": {"type": "boolean", "description": "Bypass Tier 2 state-changing-command guards. Default: false.", "default": false}
                },
                "required": ["command"]
            }
        },
        {
            "name": "archive_create",
            "description": "Create zip/tar/tar.gz archive.",
            "inputSchema": {"type": "object", "properties": {"output": {"type": "string"}, "paths": {"type": "array", "items": {"type": "string"}}, "format": {"type": "string"}}, "required": ["output", "paths"]}
        },
        {
            "name": "archive_extract",
            "description": "Extract zip/tar/tar.gz archive.",
            "inputSchema": {"type": "object", "properties": {"archive": {"type": "string"}, "destination": {"type": "string"}}, "required": ["archive"]}
        },
        {
            "name": "md2docx",
            "description": "Convert Markdown to DOCX via pandoc.",
            "inputSchema": {"type": "object", "properties": {"input": {"type": "string"}, "output": {"type": "string"}}, "required": ["input", "output"]}
        },
        {
            "name": "search_file",
            "description": "Search files by name or content.",
            "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}, "pattern": {"type": "string"}, "search_type": {"type": "string", "description": "files/content"}}, "required": ["path", "pattern"]}
        },
        {
            "name": "system_info",
            "description": "Get OS, CPU, RAM, disk info.",
            "inputSchema": {"type": "object", "properties": {}}
        }
    ]);
    let ops_native: Vec<serde_json::Value> =
        ops_native_json.as_array().cloned().unwrap_or_default();
    all_tools.extend(ops_native);
    serde_json::json!({ "tools": all_tools })
}

// ============================================================================
// mcp_rebuild
// ============================================================================

fn handle_mcp_rebuild(_server: &Server, params: Value) -> Result<Value, String> {
    use std::process::Command;
    use std::thread;
    use std::time::Duration;

    let target = params
        .get("target")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'target' parameter")?;

    let rust_mcp_dir = match std::env::var("OPS_DEV_DIR").map(PathBuf::from) {
        Ok(p) => p,
        Err(_) => {
            return Err(
                "OPS_DEV_DIR not set; this tool requires an explicit dev directory".to_string(),
            )
        }
    };

    let target_dir = rust_mcp_dir.join(target);
    let exe_name = format!("{}.exe", target);
    let exe_path = rust_mcp_dir.join("target").join("release").join(&exe_name);
    let backup_dir = rust_mcp_dir.join("backups");

    if !target_dir.exists() {
        return Err(format!("Target '{}' not found at {:?}", target, target_dir));
    }

    fs::create_dir_all(&backup_dir).map_err(|e| format!("Cannot create backup dir: {}", e))?;

    let backup_path = if exe_path.exists() {
        let timestamp = Local::now().format("%Y%m%d_%H%M%S");
        let backup_name = format!("{}_{}.exe", target, timestamp);
        let backup_path = backup_dir.join(&backup_name);
        fs::copy(&exe_path, &backup_path).map_err(|e| format!("Backup failed: {}", e))?;
        Some(backup_path)
    } else {
        None
    };

    let kill_result = Command::new("taskkill")
        .args(["/F", "/IM", &exe_name])
        .output();
    let process_killed = match kill_result {
        Ok(output) => output.status.success(),
        Err(_) => false,
    };

    thread::sleep(Duration::from_secs(3));

    let cargo_path = std::env::var("USERPROFILE")
        .map(|p| {
            PathBuf::from(p)
                .join(".cargo")
                .join("bin")
                .join("cargo.exe")
        })
        .unwrap_or_else(|_| PathBuf::from("cargo"));

    let build_output = Command::new(&cargo_path)
        .args(["build", "--release"])
        .current_dir(&target_dir)
        .output()
        .map_err(|e| format!("Cargo failed to start: {}", e))?;

    let build_success = build_output.status.success();
    let build_stderr = String::from_utf8_lossy(&build_output.stderr).to_string();

    let new_exe_exists = exe_path.exists();
    let new_exe_size = if new_exe_exists {
        fs::metadata(&exe_path).map(|m| m.len()).ok()
    } else {
        None
    };

    Ok(json!({
        "target": target,
        "backup_path": backup_path.map(|p| p.display().to_string()),
        "process_killed": process_killed,
        "build_success": build_success,
        "new_exe_exists": new_exe_exists,
        "new_exe_size_bytes": new_exe_size,
        "build_stderr_preview": build_stderr.chars().take(500).collect::<String>(),
        "message": if build_success {
            format!("Successfully rebuilt {}. Restart Claude Desktop to use new version.", target)
        } else {
            "Build failed. Check build_stderr_preview.".to_string()
        }
    }))
}

// ============================================================================
// Health / Fallback / Preflight Tools
// ============================================================================

/// Embedded default fallback map used when no file is present at OPS_FALLBACK_MAP / the default path.
/// Provides a minimal working set so tool_fallback, server_health, and deploy_preflight work
/// on a clean install without any extra configuration.
const DEFAULT_FALLBACK_MAP: &str = r#"{
  "servers": {
    "ops": {"process": "ops.exe", "mirror": null, "critical": false}
  },
  "equivalents": {},
  "fallback_chains": {},
  "deploy_sequence": {}
}"#;

fn fallback_map_path() -> PathBuf {
    if let Ok(p) = std::env::var("OPS_FALLBACK_MAP") {
        return PathBuf::from(p);
    }
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Ops")
        .join("tool_fallback_map.json")
}

fn is_process_running(name: &str) -> bool {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("IMAGENAME eq {}", name), "/NH"])
        .output();
    match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).contains(name),
        Err(_) => false,
    }
}

fn load_fallback_map() -> Result<Value, String> {
    let path = fallback_map_path();
    let content = if path.exists() {
        fs::read_to_string(&path)
            .map_err(|e| format!("Cannot read fallback map at {}: {}", path.display(), e))?
    } else {
        DEFAULT_FALLBACK_MAP.to_string()
    };
    serde_json::from_str(&content).map_err(|e| format!("Invalid JSON in fallback map: {}", e))
}

fn handle_server_health(_server: &Server, params: Value) -> Result<Value, String> {
    let map = load_fallback_map()?;
    let filter: Option<Vec<String>> = params
        .get("servers")
        .and_then(|s| serde_json::from_value(s.clone()).ok());
    let servers = map
        .get("servers")
        .and_then(|s| s.as_object())
        .ok_or("No servers in fallback map")?;

    let mut results = serde_json::Map::new();
    let mut alive_count = 0u32;
    let mut dead_count = 0u32;

    for (name, config) in servers {
        if let Some(ref f) = filter {
            if !f.iter().any(|s| s == name) {
                continue;
            }
        }
        let process = config
            .get("process")
            .and_then(|p| p.as_str())
            .unwrap_or("unknown");
        let alive = is_process_running(process);
        if alive {
            alive_count += 1;
        } else {
            dead_count += 1;
        }
        let mirror = config
            .get("mirror")
            .and_then(|m| m.as_str())
            .unwrap_or("none");
        let critical = config
            .get("critical")
            .and_then(|c| c.as_bool())
            .unwrap_or(false);
        results.insert(
            name.clone(),
            json!({
                "alive": alive, "process": process, "mirror": mirror, "critical": critical
            }),
        );
    }
    Ok(json!({
        "servers": results,
        "summary": { "alive": alive_count, "dead": dead_count, "total": alive_count + dead_count }
    }))
}

fn handle_tool_fallback(_server: &Server, params: Value) -> Result<Value, String> {
    let tool = params
        .get("tool")
        .and_then(|t| t.as_str())
        .ok_or("tool parameter required")?;
    let map = load_fallback_map()?;

    if let Some(equiv) = map
        .get("equivalents")
        .and_then(|e| e.get(tool))
        .and_then(|v| v.as_str())
    {
        return Ok(
            json!({ "tool": tool, "fallback": equiv, "type": "equivalent", "note": "Direct mirror tool available" }),
        );
    }
    if let Some(chain) = map
        .get("fallback_chains")
        .and_then(|c| c.get(tool))
        .and_then(|v| v.as_array())
    {
        let fallbacks: Vec<&str> = chain.iter().filter_map(|v| v.as_str()).collect();
        return Ok(json!({ "tool": tool, "fallbacks": fallbacks, "type": "chain" }));
    }
    if let Some(equivs) = map.get("equivalents").and_then(|e| e.as_object()) {
        for (key, val) in equivs {
            if key.starts_with('_') {
                continue;
            }
            if val.as_str() == Some(tool) {
                return Ok(json!({ "tool": tool, "fallback": key, "type": "reverse_equivalent" }));
            }
        }
    }
    Ok(json!({ "tool": tool, "fallback": null, "type": "none", "note": "No fallback registered" }))
}

fn handle_preflight_deploy(_server: &Server, params: Value) -> Result<Value, String> {
    let target = params
        .get("target")
        .and_then(|t| t.as_str())
        .ok_or("target parameter required")?;
    let map = load_fallback_map()?;

    let deploy_seq = map.get("deploy_sequence").and_then(|d| d.get(target));
    let pre_kill_steps = deploy_seq
        .and_then(|d| d.get("pre_kill"))
        .and_then(|p| p.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
        .unwrap_or_default();
    let post_restart_steps = deploy_seq
        .and_then(|d| d.get("post_restart"))
        .and_then(|p| p.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
        .unwrap_or_default();

    let server_config = map.get("servers").and_then(|s| s.get(target));
    let mirror_name = server_config
        .and_then(|s| s.get("mirror"))
        .and_then(|m| m.as_str());
    let mirror_alive = if let Some(mirror) = mirror_name {
        let mirror_process = map
            .get("servers")
            .and_then(|s| s.get(mirror))
            .and_then(|s| s.get("process"))
            .and_then(|p| p.as_str())
            .unwrap_or("unknown");
        is_process_running(mirror_process)
    } else {
        false
    };
    let critical = server_config
        .and_then(|s| s.get("critical"))
        .and_then(|c| c.as_bool())
        .unwrap_or(false);
    let safe = if critical && mirror_name.is_some() {
        mirror_alive
    } else {
        true
    };

    let mut warnings = Vec::new();
    if critical && !mirror_alive && mirror_name.is_some() {
        warnings.push(format!(
            "BLOCK: Mirror '{}' is DOWN. Cannot safely kill critical server '{}'.",
            mirror_name.unwrap_or("unknown"),
            target
        ));
    }

    Ok(json!({
        "target": target, "safe_to_deploy": safe, "mirror": mirror_name,
        "mirror_alive": mirror_alive, "critical": critical,
        "pre_kill_steps": pre_kill_steps, "post_restart_steps": post_restart_steps,
        "warnings": warnings
    }))
}

fn handle_smoke_test(_server: &Server, params: Value) -> Result<Value, String> {
    let exe_dir = params
        .get("exe_dir")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'exe_dir' parameter")?;

    let expected: Vec<String> = params
        .get("expected")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let dir = PathBuf::from(exe_dir);
    let mut missing: Vec<String> = Vec::new();
    let mut sizes: HashMap<String, u64> = HashMap::new();
    let mut found = 0u32;

    for exe in &expected {
        let path = dir.join(exe);
        match fs::metadata(&path) {
            Ok(meta) => {
                let size = meta.len();
                sizes.insert(exe.clone(), size);
                if size == 0 {
                    missing.push(format!("{} (zero bytes)", exe));
                } else {
                    found += 1;
                }
            }
            Err(_) => {
                missing.push(exe.clone());
            }
        }
    }

    Ok(json!({
        "pass": missing.is_empty(),
        "total_expected": expected.len(),
        "found": found,
        "missing": missing,
        "sizes": sizes,
        "exe_dir": exe_dir
    }))
}

fn handle_powershell(_server: &Server, params: Value) -> Result<Value, String> {
    use crate::security::blocklist::{check, classify, log_audit, Guard};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    let allow_destructive = params
        .get("allow_destructive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let confirm = params
        .get("confirm")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let cmd = params.get("command").and_then(|v| v.as_str()).unwrap_or("");

    match check(cmd, allow_destructive, confirm) {
        Guard::Refuse {
            error_kind,
            tier,
            reason,
            matched,
            guidance,
        } => {
            let m = classify(cmd);
            log_audit("powershell", &m, "blocked", cmd);
            return Ok(json!({
                "error": error_kind,
                "tier": tier,
                "reason": reason,
                "matched": matched,
                "guidance": guidance,
            }));
        }
        Guard::Allow => {
            let m = classify(cmd);
            let outcome = match m.tier {
                crate::security::blocklist::Tier::Two => "allowed_with_confirm",
                crate::security::blocklist::Tier::Three => "allowed_with_destructive",
                _ => "",
            };
            if !outcome.is_empty() {
                log_audit("powershell", &m, outcome, cmd);
            }
        }
    }

    let command = params
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'command' parameter")?
        .to_string();

    let timeout = params
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(30);

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", &command])
            .output();
        let _ = tx.send(result);
    });

    let output = rx
        .recv_timeout(Duration::from_secs(timeout))
        .map_err(|_| format!("PowerShell command timed out after {} seconds", timeout))?
        .map_err(|e| format!("Failed to execute powershell: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    let mut result = json!({
        "success": output.status.success(),
        "stdout": stdout.trim(),
        "exit_code": output.status.code()
    });
    if !stderr.trim().is_empty() {
        result["stderr"] = json!(stderr.trim());
    }
    Ok(result)
}

fn resolve_bash() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("OPS_BASH_PATH") {
        let pb = std::path::PathBuf::from(&p);
        if pb.exists() {
            return Ok(pb);
        }
        return Err(format!(
            "OPS_BASH_PATH set to {} but file does not exist",
            p
        ));
    }
    let mut candidates = vec![
        PathBuf::from(r"C:\Program Files\Git\bin\bash.exe"),
        PathBuf::from(r"C:\Program Files (x86)\Git\bin\bash.exe"),
    ];
    if let Ok(local_appdata) = std::env::var("LOCALAPPDATA") {
        candidates.push(
            PathBuf::from(local_appdata)
                .join("Programs")
                .join("Git")
                .join("bin")
                .join("bash.exe"),
        );
    }
    for c in &candidates {
        if c.exists() {
            return Ok(c.clone());
        }
    }
    if let Ok(path_env) = std::env::var("PATH") {
        for dir in path_env.split(';') {
            let candidate = PathBuf::from(dir).join("bash.exe");
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }
    Err("Git Bash not found. Install Git for Windows from https://git-scm.com/download/win or set OPS_BASH_PATH.".to_string())
}

fn resolve_working_dir(passed: Option<&str>) -> PathBuf {
    if let Some(p) = passed {
        let pb = PathBuf::from(p);
        if pb.is_dir() {
            return pb;
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        return cwd;
    }
    if let Some(local) = dirs::data_local_dir() {
        return local.join("Ops");
    }
    PathBuf::from(".")
}

fn handle_bash(_server: &Server, params: Value) -> Result<Value, String> {
    use crate::security::blocklist::{check, classify, log_audit, Guard};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    let allow_destructive = params
        .get("allow_destructive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let confirm = params
        .get("confirm")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let cmd = params.get("command").and_then(|v| v.as_str()).unwrap_or("");

    match check(cmd, allow_destructive, confirm) {
        Guard::Refuse {
            error_kind,
            tier,
            reason,
            matched,
            guidance,
        } => {
            let m = classify(cmd);
            log_audit("bash", &m, "blocked", cmd);
            return Ok(json!({
                "error": error_kind,
                "tier": tier,
                "reason": reason,
                "matched": matched,
                "guidance": guidance,
            }));
        }
        Guard::Allow => {
            let m = classify(cmd);
            let outcome = match m.tier {
                crate::security::blocklist::Tier::Two => "allowed_with_confirm",
                crate::security::blocklist::Tier::Three => "allowed_with_destructive",
                _ => "",
            };
            if !outcome.is_empty() {
                log_audit("bash", &m, outcome, cmd);
            }
        }
    }

    let bash_bin = resolve_bash()?;

    let command = params
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'command' parameter")?
        .to_string();

    let working_dir = resolve_working_dir(params.get("working_dir").and_then(|v| v.as_str()));

    let timeout = params
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(30)
        .min(600);

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = std::process::Command::new(&bash_bin)
            .args(["-c", &command])
            .current_dir(&working_dir)
            .output();
        let _ = tx.send(result);
    });

    let output = rx
        .recv_timeout(Duration::from_secs(timeout))
        .map_err(|_| format!("Bash command timed out after {} seconds", timeout))?
        .map_err(|e| format!("Failed to execute bash: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    let mut result = json!({
        "success": output.status.success(),
        "stdout": stdout.trim(),
        "exit_code": output.status.code()
    });
    if !stderr.trim().is_empty() {
        result["stderr"] = json!(stderr.trim());
    }
    Ok(result)
}

fn handle_archive_create(_server: &Server, params: Value) -> Result<Value, String> {
    let output = params["output"].as_str().ok_or("output required")?;
    let paths = params["paths"].as_array().ok_or("paths required")?;
    let format = params["format"].as_str().unwrap_or("zip");
    let path_list: Vec<&str> = paths.iter().filter_map(|p| p.as_str()).collect();
    let ps_cmd = match format {
        "zip" => format!(
            "Compress-Archive -Path '{}' -DestinationPath '{}' -Force; Write-Output 'Created: {}'",
            path_list.join("','"),
            output,
            output
        ),
        _ => format!(
            "tar -cf '{}' {}; Write-Output 'Created: {}'",
            output,
            path_list.join(" "),
            output
        ),
    };
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps_cmd])
        .output()
        .map_err(|e| e.to_string())?;
    Ok(
        json!({"success": out.status.success(), "stdout": String::from_utf8_lossy(&out.stdout).trim().to_string(), "stderr": String::from_utf8_lossy(&out.stderr).trim().to_string()}),
    )
}

fn handle_archive_extract(_server: &Server, params: Value) -> Result<Value, String> {
    let archive = params["archive"].as_str().ok_or("archive required")?;
    let dest = params["destination"].as_str().unwrap_or(".");
    let ext = archive.to_lowercase();
    let ps_cmd = if ext.ends_with(".zip") {
        format!("Expand-Archive -Path '{}' -DestinationPath '{}' -Force; Write-Output 'Extracted to: {}'",
            archive, dest, dest)
    } else {
        format!(
            "tar -xf '{}' -C '{}' 2>&1; Write-Output 'Extracted to: {}'",
            archive, dest, dest
        )
    };
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps_cmd])
        .output()
        .map_err(|e| e.to_string())?;
    Ok(
        json!({"success": out.status.success(), "stdout": String::from_utf8_lossy(&out.stdout).trim().to_string(), "stderr": String::from_utf8_lossy(&out.stderr).trim().to_string()}),
    )
}

fn handle_md2docx(_server: &Server, params: Value) -> Result<Value, String> {
    let input = params["input"].as_str().ok_or("input required")?;
    let output = params["output"].as_str().ok_or("output required")?;
    let ps_cmd = format!(
        "pandoc '{}' -o '{}' 2>&1; if ($?) {{ Write-Output 'Converted: {} -> {}' }}",
        input, output, input, output
    );
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps_cmd])
        .output()
        .map_err(|e| e.to_string())?;
    Ok(
        json!({"success": out.status.success(), "stdout": String::from_utf8_lossy(&out.stdout).trim().to_string(), "stderr": String::from_utf8_lossy(&out.stderr).trim().to_string()}),
    )
}

fn handle_search_files(_server: &Server, params: Value) -> Result<Value, String> {
    let path = params["path"].as_str().ok_or("path required")?;
    let pattern = params["pattern"].as_str().ok_or("pattern required")?;
    let search_type = params["search_type"].as_str().unwrap_or("files");
    let ps_cmd = match search_type {
        "content" => format!(
            "Get-ChildItem -Path '{}' -Recurse -File -EA SilentlyContinue | Select-String -Pattern '{}' | Select-Object -First 50 | ForEach-Object {{ \"$($_.Path):$($_.LineNumber): $($_.Line.Trim())\" }}",
            path, pattern),
        _ => format!(
            "Get-ChildItem -Path '{}' -Recurse -File -Filter '*{}*' -EA SilentlyContinue | Select-Object -First 50 | ForEach-Object {{ $_.FullName }}",
            path, pattern),
    };
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps_cmd])
        .output()
        .map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let results: Vec<&str> = stdout.lines().collect();
    Ok(json!({"matches": results, "count": results.len(), "search_type": search_type}))
}

fn handle_system_info(_server: &Server, _params: Value) -> Result<Value, String> {
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command",
            "Get-CimInstance Win32_OperatingSystem | ForEach-Object { Write-Output ('OS: ' + $_.Caption + ' ' + $_.Version + ' | RAM: ' + [math]::Round($_.FreePhysicalMemory/1MB,1).ToString() + '/' + [math]::Round($_.TotalVisibleMemorySize/1MB,1).ToString() + ' GB free') }; Get-CimInstance Win32_Processor | Select-Object -First 1 | ForEach-Object { Write-Output ('CPU: ' + $_.Name + ' (' + $_.NumberOfCores + ' cores)') }; Get-CimInstance Win32_LogicalDisk -Filter 'DriveType=3' | ForEach-Object { Write-Output ('Disk: ' + $_.DeviceID + ' ' + [math]::Round($_.FreeSpace/1GB,1).ToString() + '/' + [math]::Round($_.Size/1GB,1).ToString() + 'GB') }"
        ])
        .output().map_err(|e| e.to_string())?;
    Ok(json!({"info": String::from_utf8_lossy(&out.stdout).trim().to_string()}))
}

fn handle_tool_call(server: &Server, name: &str, params: Value) -> Result<Value, String> {
    match name {
        "mcp_rebuild" => handle_mcp_rebuild(server, params),
        "server_health" => handle_server_health(server, params),
        "tool_fallback" => handle_tool_fallback(server, params),
        "deploy_preflight" | "preflight_deploy" => handle_preflight_deploy(server, params),
        "deploy_smoke_test" | "smoke_test" => handle_smoke_test(server, params),
        "powershell" => handle_powershell(server, params),
        "bash" => handle_bash(server, params),
        "archive_create" => handle_archive_create(server, params),
        "archive_extract" => handle_archive_extract(server, params),
        "md2docx" => handle_md2docx(server, params),
        "search_file" | "search_files" => handle_search_files(server, params),
        "system_info" => handle_system_info(server, params),
        _ => {
            if let Some(result) = tools::execute(name, &params) {
                Ok(result)
            } else {
                Err(format!("Unknown tool: {}", name))
            }
        }
    }
}

// ============================================================================
// Main Loop
// ============================================================================

fn main() {
    // Orphan-process prevention: kill this process tree when parent dies.
    if let Err(e) = cpc_paths::process::ensure_kill_on_parent_death() {
        eprintln!("[warn] job-object setup failed: {e}");
    }

    // Handle subcommands BEFORE entering the MCP stdio loop.
    // Without this, `ops.exe --version` hangs waiting for JSON-RPC input.
    let argv: Vec<String> = std::env::args().collect();
    const SERVER_KEY: &str = "universal-ops";
    match argv.get(1).map(|s| s.as_str()) {
        Some("--version") | Some("-V") => {
            println!("ops {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Some("--help") | Some("-h") => {
            println!("Universal-Ops ops v{}", env!("CARGO_PKG_VERSION"));
            println!();
            println!("USAGE:");
            println!("  ops                              Run as MCP server over stdio (default)");
            println!("  ops install --target <host>      Register with host config as '{}'", SERVER_KEY);
            println!("  ops uninstall --target <host>    Unregister from host config");
            println!("  ops --version                    Print version");
            println!();
            install_common::print_install_help("ops", SERVER_KEY);
            println!();
            println!("Repository: https://github.com/AIWander/Universal-Ops");
            return;
        }
        Some("install") => {
            std::process::exit(match install_common::install(SERVER_KEY, &argv[2..]) {
                Ok(()) => 0,
                Err(e) => { eprintln!("install failed: {}", e); 1 }
            });
        }
        Some("uninstall") => {
            std::process::exit(match install_common::uninstall(SERVER_KEY, &argv[2..]) {
                Ok(()) => 0,
                Err(e) => { eprintln!("uninstall failed: {}", e); 1 }
            });
        }
        _ => { /* fall through to MCP stdio loop */ }
    }

    let server = Server::new();
    let stdin = io::stdin();
    let mut stdout = io::stdout();

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
                let _ = writeln!(
                    stdout,
                    "{}",
                    serde_json::to_string(&error_response).unwrap()
                );
                let _ = stdout.flush();
                continue;
            }
        };

        if request.jsonrpc != "2.0" {
            let error_response = JsonRpcErrorResponse {
                jsonrpc: "2.0".to_string(),
                id: request.id.clone(),
                error: JsonRpcError {
                    code: -32600,
                    message: format!(
                        "Invalid JSON-RPC version: expected '2.0', got '{}'",
                        request.jsonrpc
                    ),
                },
            };
            let _ = writeln!(
                stdout,
                "{}",
                serde_json::to_string(&error_response).unwrap()
            );
            let _ = stdout.flush();
            continue;
        }

        let response = match request.method.as_str() {
            "initialize" => {
                agent_identity::set_from_initialize(request.params.as_ref());
                JsonRpcSuccess {
                    jsonrpc: "2.0".to_string(),
                    id: request.id,
                    result: json!({
                        "protocolVersion": "2024-11-05",
                        "serverInfo": {"name": "ops", "version": "0.3.0"},
                        "capabilities": {"tools": {}}
                    }),
                }
            }
            "notifications/initialized" => continue,
            "tools/list" => JsonRpcSuccess {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                result: get_tools_list(),
            },
            "tools/call" => {
                let params = request.params.unwrap_or(json!({}));
                let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let tool_args = params.get("arguments").cloned().unwrap_or(json!({}));

                match handle_tool_call(&server, tool_name, tool_args) {
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
            _ => JsonRpcSuccess {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                result: json!({}),
            },
        };

        let _ = writeln!(stdout, "{}", serde_json::to_string(&response).unwrap());
        let _ = stdout.flush();
    }
}
