//! Config backup and validation tools — pure Rust implementation.
//! No external scripts required. Works on any clean install.

use chrono::Local;
use serde_json::{json, Value};
use std::path::PathBuf;

fn config_path() -> Option<PathBuf> {
    std::env::var("APPDATA").ok().map(|appdata| {
        PathBuf::from(appdata)
            .join("Claude")
            .join("claude_desktop_config.json")
    })
}

pub fn get_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "config_backup",
            "description": "Backup claude_desktop_config.json with a timestamp before editing.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "config_validate",
            "description": "Validate claude_desktop_config.json: parse JSON and check structure.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
    ]
}

pub fn execute(name: &str, _args: &Value) -> Value {
    match name {
        "config_backup" => {
            let src = match config_path() {
                Some(p) => p,
                None => {
                    return json!({"success": false, "error": "APPDATA environment variable not set"})
                }
            };
            if !src.exists() {
                return json!({"success": false, "error": format!("Config not found: {}", src.display())});
            }
            let ts = Local::now().format("%Y%m%d_%H%M%S");
            let parent = match src.parent() {
                Some(p) => p.to_path_buf(),
                None => {
                    return json!({"success": false, "error": "Cannot determine config directory"})
                }
            };
            let backup = parent.join(format!("claude_desktop_config.backup-{}.json", ts));
            match std::fs::copy(&src, &backup) {
                Ok(_) => json!({"success": true, "backup_path": backup.to_string_lossy()}),
                Err(e) => json!({"success": false, "error": format!("Backup failed: {}", e)}),
            }
        }
        "config_validate" => {
            let path = match config_path() {
                Some(p) => p,
                None => {
                    return json!({"success": false, "valid": false, "error": "APPDATA environment variable not set"})
                }
            };
            if !path.exists() {
                return json!({"success": false, "valid": false, "error": format!("Config not found: {}", path.display())});
            }
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    return json!({"success": false, "valid": false, "error": format!("Read error: {}", e)})
                }
            };
            match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(val) => {
                    let has_mcp_servers = val.get("mcpServers").is_some();
                    let server_count = val
                        .get("mcpServers")
                        .and_then(|s| s.as_object())
                        .map(|o| o.len())
                        .unwrap_or(0);
                    json!({
                        "success": true,
                        "valid": true,
                        "has_mcp_servers": has_mcp_servers,
                        "server_count": server_count,
                        "path": path.to_string_lossy()
                    })
                }
                Err(e) => {
                    json!({"success": false, "valid": false, "error": format!("JSON parse error: {}", e)})
                }
            }
        }
        _ => json!({"error": format!("Unknown config tool: {}", name)}),
    }
}
