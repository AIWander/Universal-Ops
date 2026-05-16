//! Persistent shell sessions — ported from local crate session.rs
//! Replaces psession_* tools with the richer session_* API.

use once_cell::sync::Lazy;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use uuid::Uuid;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

static SESSIONS: Lazy<Arc<Mutex<HashMap<String, PersistentSession>>>> =
    Lazy::new(|| Arc::new(Mutex::new(HashMap::new())));

struct PersistentSession {
    name: String,
    child: Child,
    output_buffer: Arc<Mutex<Vec<String>>>,
    cwd: String,
    env: HashMap<String, String>,
    history: Vec<String>,
    created_at: String,
}

impl Drop for PersistentSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn start_output_reader(stdout: std::process::ChildStdout, buffer: Arc<Mutex<Vec<String>>>) {
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(l) => {
                    buffer.lock().unwrap().push(l);
                }
                Err(_) => break,
            }
        }
    });
}

impl PersistentSession {
    fn new(name: &str, cwd: Option<&str>) -> Result<Self, String> {
        let working_dir = cwd.unwrap_or("C:\\").to_string();
        let mut cmd = Command::new("powershell");
        cmd.args(["-NoLogo", "-NoProfile", "-Command", "-"])
            .current_dir(&working_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn PowerShell: {}", e))?;
        let stdout = child.stdout.take().ok_or("Failed to take stdout")?;
        let output_buffer = Arc::new(Mutex::new(Vec::new()));
        start_output_reader(stdout, output_buffer.clone());
        thread::sleep(std::time::Duration::from_millis(100));

        Ok(Self {
            name: name.to_string(),
            child,
            output_buffer,
            cwd: working_dir,
            env: HashMap::new(),
            history: Vec::new(),
            created_at: chrono::Local::now().to_rfc3339(),
        })
    }

    fn run_command(&mut self, command: &str, timeout_secs: u64) -> Result<Value, String> {
        let marker = format!("__EXIT_{}__", &Uuid::new_v4().to_string()[..8]);
        let full_cmd = format!("{}; Write-Host '{}' $LASTEXITCODE\n", command, marker);

        let stdin = self
            .child
            .stdin
            .as_mut()
            .ok_or("No stdin available - session may be dead")?;
        stdin
            .write_all(full_cmd.as_bytes())
            .map_err(|e| format!("Write failed: {}", e))?;
        stdin.flush().map_err(|e| format!("Flush failed: {}", e))?;

        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(timeout_secs);
        let mut collected: Vec<String> = Vec::new();

        loop {
            if start.elapsed() > timeout {
                return Err(format!("Command timed out after {}s", timeout_secs));
            }
            {
                let mut buf = self.output_buffer.lock().unwrap();
                collected.append(&mut *buf);
            }

            let full_output = collected.join("\n");
            if full_output.contains(&marker) {
                let (clean_output, exit_code) = self.parse_output(&collected, &marker);
                self.history.push(command.to_string());
                self.maybe_update_cwd(command);
                return Ok(json!({
                    "success": exit_code == 0,
                    "session": self.name,
                    "cwd": self.cwd,
                    "output": clean_output.trim(),
                    "exit_code": exit_code
                }));
            }
            thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    fn parse_output(&self, lines: &[String], marker: &str) -> (String, i32) {
        let mut output_lines = Vec::new();
        let mut exit_code = 0;
        for line in lines {
            if line.contains(marker) {
                let parts: Vec<&str> = line.split(marker).collect();
                if parts.len() > 1 {
                    exit_code = parts[1].trim().parse().unwrap_or(0);
                }
            } else {
                output_lines.push(line.clone());
            }
        }
        (output_lines.join("\n"), exit_code)
    }

    fn maybe_update_cwd(&mut self, command: &str) {
        let cmd_lower = command.to_lowercase();
        let cmd_trimmed = cmd_lower.trim();
        let path = if cmd_trimmed.starts_with("cd ") {
            Some(command.trim()[3..].trim())
        } else if cmd_trimmed.starts_with("set-location ") {
            Some(command.trim()[13..].trim())
        } else {
            None
        };
        if let Some(p) = path {
            let clean_path = p.trim_matches(|c| c == '\'' || c == '"');
            let new_path = if std::path::Path::new(clean_path).is_absolute() {
                clean_path.to_string()
            } else {
                format!("{}\\{}", self.cwd, clean_path)
            };
            if let Ok(canonical) = std::fs::canonicalize(&new_path) {
                self.cwd = canonical.to_string_lossy().to_string();
            }
        }
    }

    fn is_alive(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(Some(_)) => false,
            Ok(None) => true,
            Err(_) => false,
        }
    }
}

pub fn get_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "session_create",
            "description": "Create a persistent shell session. Env vars and cwd persist across calls.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Session name (default: 'default')" },
                    "cwd": { "type": "string", "description": "Initial working directory" }
                }
            }
        }),
        json!({
            "name": "session_run",
            "description": "Run command in persistent session. Inherits env and cwd from session. Destructive commands require explicit user permission: pass `confirm: true` for service/firewall/scheduled-task/registry changes; pass `allow_destructive: true` for format/diskpart/account-deletion/bulk-system-delete operations. Catastrophic patterns are blocked unconditionally.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "Session name (default: 'default')" },
                    "command": { "type": "string", "description": "Command to execute" },
                    "allow_destructive": { "type": "boolean", "description": "Allow Tier-3 destructive operations. Requires explicit user permission." },
                    "confirm": { "type": "boolean", "description": "Allow Tier-2 state-changing operations. Requires user acknowledgment." }
                },
                "required": ["command"]
            }
        }),
        json!({
            "name": "session_cd",
            "description": "Change directory in session. Persists for subsequent commands.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "Session name" },
                    "path": { "type": "string", "description": "Directory to change to" }
                },
                "required": ["path"]
            }
        }),
        json!({
            "name": "session_set_env",
            "description": "Set environment variable in session.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "Session name" },
                    "key": { "type": "string", "description": "Variable name" },
                    "value": { "type": "string", "description": "Variable value" }
                },
                "required": ["key", "value"]
            }
        }),
        json!({
            "name": "session_get_env",
            "description": "Get environment variable(s) from session.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "Session name" },
                    "key": { "type": "string", "description": "Specific key (empty for all tracked vars)" }
                }
            }
        }),
        json!({
            "name": "session_list",
            "description": "List all active sessions with their state.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "session_destroy",
            "description": "Destroy a session and kill its PowerShell process.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "Session name to destroy" }
                },
                "required": ["session"]
            }
        }),
    ]
}

pub fn execute(name: &str, args: &Value) -> Value {
    match name {
        "session_create" => session_create(args),
        "session_run" => session_run(args),
        "session_cd" => session_cd(args),
        "session_set_env" => session_setenv(args),
        "session_get_env" => session_getenv(args),
        "session_list" => session_list(args),
        "session_destroy" => session_destroy(args),
        _ => json!({"error": format!("Unknown session tool: {}", name)}),
    }
}

fn session_create(args: &Value) -> Value {
    let name = args["name"].as_str().unwrap_or("default");
    let cwd = args["cwd"].as_str();
    let mut sessions = SESSIONS.lock().unwrap();
    if sessions.contains_key(name) {
        let session = sessions.get(name).unwrap();
        return json!({
            "exists": true, "session": name, "cwd": session.cwd.clone(),
            "history_count": session.history.len(),
            "message": format!("Session '{}' already exists - use session_run to reuse it", name)
        });
    }
    match PersistentSession::new(name, cwd) {
        Ok(session) => {
            let cwd_used = session.cwd.clone();
            sessions.insert(name.to_string(), session);
            json!({"success": true, "session": name, "cwd": cwd_used, "persistent": true})
        }
        Err(e) => json!({"success": false, "error": e}),
    }
}

fn session_run(args: &Value) -> Value {
    use crate::security::blocklist::{check, classify, log_audit, Guard};

    let allow_destructive = args
        .get("allow_destructive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let confirm = args
        .get("confirm")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");

    match check(cmd, allow_destructive, confirm) {
        Guard::Refuse {
            error_kind,
            tier,
            reason,
            matched,
            guidance,
        } => {
            let m = classify(cmd);
            log_audit("session_run", &m, "blocked", cmd);
            return serde_json::json!({
                "error": error_kind,
                "tier": tier,
                "reason": reason,
                "matched": matched,
                "guidance": guidance,
            });
        }
        Guard::Allow => {
            let m = classify(cmd);
            let outcome = match m.tier {
                crate::security::blocklist::Tier::Two => "allowed_with_confirm",
                crate::security::blocklist::Tier::Three => "allowed_with_destructive",
                _ => "",
            };
            if !outcome.is_empty() {
                log_audit("session_run", &m, outcome, cmd);
            }
        }
    }

    let session_name = args["session"].as_str().unwrap_or("default");
    let command = match args["command"].as_str() {
        Some(c) => c,
        None => return json!({"error": "command required"}),
    };
    let mut sessions = SESSIONS.lock().unwrap();
    if !sessions.contains_key(session_name) && session_name == "default" {
        match PersistentSession::new("default", None) {
            Ok(s) => {
                sessions.insert("default".to_string(), s);
            }
            Err(e) => return json!({"error": format!("Failed to create default session: {}", e)}),
        }
    }
    let session = match sessions.get_mut(session_name) {
        Some(s) => s,
        None => {
            return json!({"error": format!("Session '{}' not found. Create with session_create first.", session_name)})
        }
    };
    if !session.is_alive() {
        return json!({"error": "Session process has died", "hint": "Destroy and recreate the session"});
    }
    match session.run_command(command, 30) {
        Ok(result) => result,
        Err(e) => json!({"error": e, "session": session_name}),
    }
}

fn session_cd(args: &Value) -> Value {
    let session_name = args["session"].as_str().unwrap_or("default");
    let path = match args["path"].as_str() {
        Some(p) => p,
        None => return json!({"error": "path required"}),
    };
    let run_args = json!({"session": session_name, "command": format!("cd '{}'", path)});
    session_run(&run_args)
}

fn session_setenv(args: &Value) -> Value {
    let session_name = args["session"].as_str().unwrap_or("default");
    let key = match args["key"].as_str() {
        Some(k) => k,
        None => return json!({"error": "key required"}),
    };
    let value = match args["value"].as_str() {
        Some(v) => v,
        None => return json!({"error": "value required"}),
    };

    let run_args = json!({"session": session_name, "command": format!("$env:{}='{}'", key, value)});
    let result = session_run(&run_args);

    let mut sessions = SESSIONS.lock().unwrap();
    if let Some(session) = sessions.get_mut(session_name) {
        session.env.insert(key.to_string(), value.to_string());
    }
    result
}

fn session_getenv(args: &Value) -> Value {
    let session_name = args["session"].as_str().unwrap_or("default");
    let key = args["key"].as_str();
    match key {
        Some(k) if !k.is_empty() => {
            let run_args = json!({"session": session_name, "command": format!("$env:{}", k)});
            session_run(&run_args)
        }
        _ => {
            let sessions = SESSIONS.lock().unwrap();
            match sessions.get(session_name) {
                Some(s) => json!(s.env),
                None => json!({"error": format!("Session '{}' not found", session_name)}),
            }
        }
    }
}

fn session_list(args: &Value) -> Value {
    let _ = args;
    let mut sessions = SESSIONS.lock().unwrap();
    let list: Vec<Value> = sessions
        .iter_mut()
        .map(|(_, s)| {
            json!({
                "name": s.name, "cwd": s.cwd,
                "env_count": s.env.len(),
                "history_count": s.history.len(),
                "created_at": s.created_at,
                "alive": s.is_alive()
            })
        })
        .collect();
    json!({"sessions": list, "count": list.len()})
}

fn session_destroy(args: &Value) -> Value {
    let session_name = match args["session"].as_str() {
        Some(s) => s,
        None => return json!({"error": "session name required"}),
    };
    let mut sessions = SESSIONS.lock().unwrap();
    match sessions.remove(session_name) {
        Some(_) => json!({"success": true, "destroyed": session_name}),
        None => json!({"success": false, "error": format!("Session '{}' not found", session_name)}),
    }
}
