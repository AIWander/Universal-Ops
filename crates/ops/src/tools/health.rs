//! Health -- server health, checkpoints, git rollback.

use anyhow::Result;
use serde_json::{json, Value};

use crate::config::get_config;

pub async fn health_check(_args: Value) -> Result<Value> {
    let cfg = get_config();
    let data_dir_exists = cfg.data_dir.exists();
    Ok(json!({
        "server": "ops",
        "status": if data_dir_exists { "healthy" } else { "degraded" },
        "data_dir": cfg.data_dir.to_string_lossy(),
        "data_dir_exists": data_dir_exists,
        "timestamp": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
    }))
}

pub async fn health_report(_args: Value) -> Result<Value> {
    health_check(json!({})).await
}

pub async fn status(args: Value) -> Result<Value> {
    let topic = args.get("topic").and_then(|v| v.as_str());
    match topic {
        Some(t) => {
            Ok(json!({ "topic": t, "status": "ok", "note": "Status files not available in ops" }))
        }
        None => health_check(json!({})).await,
    }
}

pub async fn checkpoint_save(args: Value) -> Result<Value> {
    let default_data = json!({});
    let data = args.get("data").unwrap_or(&default_data);
    let cfg = get_config();
    let path = cfg.data_dir.join("checkpoint.json");
    let checkpoint = json!({ "data": data, "saved_at": chrono::Utc::now().to_rfc3339() });
    std::fs::write(&path, serde_json::to_string_pretty(&checkpoint)?)?;
    Ok(json!({ "status": "saved", "path": path.to_string_lossy() }))
}

pub async fn checkpoint_load(_args: Value) -> Result<Value> {
    let cfg = get_config();
    let path = cfg.data_dir.join("checkpoint.json");
    if path.exists() {
        let content = std::fs::read_to_string(&path)?;
        let checkpoint: Value = serde_json::from_str(&content)?;
        Ok(json!({ "status": "loaded", "checkpoint": checkpoint }))
    } else {
        Ok(json!({ "status": "no_checkpoint" }))
    }
}

pub async fn checkpoint_clear(_args: Value) -> Result<Value> {
    let cfg = get_config();
    let path = cfg.data_dir.join("checkpoint.json");
    let _ = std::fs::remove_file(&path);
    Ok(json!({ "status": "cleared" }))
}

pub async fn git_rollback(args: Value) -> Result<Value> {
    let commit = args.get("commit").and_then(|v| v.as_str()).unwrap_or("");
    let repo_dir = std::env::var("OPS_DEV_DIR").map_err(|_| {
        anyhow::anyhow!("OPS_DEV_DIR not set; this tool requires an explicit dev directory")
    })?;
    let output = std::process::Command::new("git")
        .args(["reset", "--hard", commit])
        .current_dir(&repo_dir)
        .output()?;
    Ok(json!({
        "status": if output.status.success() { "rolled_back" } else { "failed" },
        "commit": commit,
        "output": String::from_utf8_lossy(&output.stdout).trim()
    }))
}
