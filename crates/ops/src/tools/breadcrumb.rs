//! Breadcrumb — multi-step operation tracking.
//! No lite version. This is the full implementation.
// NAV: TOC at line 572 | 22 fn | 2 struct | 2026-04-08

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::agent_identity::{self, WriterIdentity};
use crate::config::get_config;

#[derive(Serialize, Deserialize, Clone)]
struct Operation {
    #[serde(default)]
    id: String,
    name: String,
    steps: Vec<String>,
    current_step: usize,
    total_steps: usize,
    started_at: String,
    step_results: Vec<StepResult>,
    files_changed: Vec<String>,
    #[serde(default)]
    transcript_offset: usize,
    #[serde(default)]
    owner: WriterIdentity,
    #[serde(default)]
    last_writer: WriterIdentity,
}

#[derive(Serialize, Deserialize, Clone)]
struct StepResult {
    step: String,
    result: String,
    completed_at: String,
    files: Vec<String>,
    #[serde(default)]
    writer: WriterIdentity,
}

/// Resolve breadcrumb root directory using priority order:
/// 1. OPS_BREADCRUMBS_DIR env var (matches manager-universal's read path)
/// 2. OPS_BREADCRUMB_PATH env var (legacy - kept for backward compat)
/// 3. %LOCALAPPDATA%\CPC\ops-data\logs (matches manager-universal's default for OPS_BREADCRUMBS_DIR)
/// 4. {exe_parent}\state\breadcrumbs\
fn breadcrumb_root() -> PathBuf {
    if let Ok(p) = std::env::var("OPS_BREADCRUMBS_DIR") {
        let path = PathBuf::from(p);
        let _ = std::fs::create_dir_all(&path);
        return path;
    }
    if let Ok(p) = std::env::var("OPS_BREADCRUMB_PATH") {
        let path = PathBuf::from(p);
        let _ = std::fs::create_dir_all(&path);
        return path;
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let path = PathBuf::from(local)
            .join("CPC")
            .join("ops-data")
            .join("logs");
        let _ = std::fs::create_dir_all(&path);
        return path;
    }
    let fallback = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("state").join("breadcrumbs")))
        .unwrap_or_else(|| PathBuf::from("state/breadcrumbs"));
    let _ = std::fs::create_dir_all(&fallback);
    fallback
}

fn active_path() -> PathBuf {
    let p = breadcrumb_root().join("active");
    let _ = std::fs::create_dir_all(&p);
    p.join("active_operation.json")
}

fn completed_dir() -> PathBuf {
    breadcrumb_root().join("completed")
}

fn checkpoint_path() -> PathBuf {
    breadcrumb_root().join("breadcrumb_checkpoint.json")
}

fn breadcrumb_log_path() -> PathBuf {
    breadcrumb_root().join("breadcrumb.jsonl")
}

fn save_checkpoint(op: &Operation) {
    let checkpoint = serde_json::json!({
        "operation_name": op.name,
        "resume_from_step": op.current_step,
        "total_steps": op.total_steps,
        "owner": op.owner,
        "last_writer": op.last_writer,
        "completed_steps": op.step_results.iter().map(|s| {
            serde_json::json!({"step": s.step, "result": s.result, "completed_at": s.completed_at, "writer": s.writer})
        }).collect::<Vec<_>>(),
        "files_modified": op.files_changed,
        "last_checkpoint": chrono::Local::now().to_rfc3339(),
    });
    let _ = std::fs::write(
        checkpoint_path(),
        serde_json::to_string_pretty(&checkpoint).unwrap_or_default(),
    );
}

fn clear_checkpoint() {
    let _ = std::fs::remove_file(checkpoint_path());
}

fn load_active() -> Option<Operation> {
    let path = active_path();
    if path.exists() {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
    } else {
        None
    }
}

fn save_active(op: &Operation) -> Result<()> {
    let path = active_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(op)?)?;
    Ok(())
}

fn clear_active() {
    let _ = std::fs::remove_file(active_path());
}

fn safe_operation_slug(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    let mut last_was_sep = false;

    for ch in name.chars() {
        let safe = ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_');
        if safe {
            slug.push(ch);
            last_was_sep = false;
        } else if !last_was_sep {
            slug.push('_');
            last_was_sep = true;
        }
    }

    let trimmed = slug.trim_matches('_');
    if trimmed.is_empty() {
        "operation".to_string()
    } else {
        trimmed.to_string()
    }
}

fn log_event(event: &str, op: &Operation, payload: Value) {
    // status field maps event names to values manager-universal can consume:
    // "complete" -> "complete", "abort" -> "aborted", all others -> "active"
    let status = match event {
        "complete" => "complete",
        "abort" => "aborted",
        _ => "active",
    };
    let entry = json!({
        "id": op.id,
        "status": status,
        "event": event,
        "name": op.name,
        "owner": op.owner,
        "writer": op.last_writer,
        "current_step": op.current_step,
        "total_steps": op.total_steps,
        "started_at": op.started_at,
        "timestamp": chrono::Local::now().to_rfc3339(),
        "payload": payload
    });

    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(breadcrumb_log_path())
        .and_then(|mut f| {
            use std::io::Write;
            writeln!(f, "{}", entry)
        });
}

fn sync_operation_state(op: Option<&Operation>, completed: Option<Value>, args: Option<&Value>) {
    let mut operation_state = crate::tools::cpc_state::read_state()
        .get("operation")
        .cloned()
        .unwrap_or_else(|| json!({}));

    if !operation_state.is_object() {
        operation_state = json!({});
    }

    if let Some(operation_obj) = operation_state.as_object_mut() {
        operation_obj.insert(
            "active".to_string(),
            op.map(|active| {
                json!({
                    "name": active.name,
                    "current_step": active.current_step,
                    "total_steps": active.total_steps,
                    "started_at": active.started_at,
                    "owner": active.owner,
                    "last_writer": active.last_writer
                })
            })
            .unwrap_or(Value::Null),
        );

        if let Some(active) = op {
            operation_obj.insert("owner_agent".to_string(), json!(active.owner.actor));
            operation_obj.insert(
                "last_writer".to_string(),
                serde_json::to_value(&active.last_writer).unwrap_or(Value::Null),
            );
        } else {
            operation_obj.insert("owner_agent".to_string(), Value::Null);
        }

        if let Some(last_completed) = completed {
            operation_obj.insert("last_completed".to_string(), last_completed);
        }
    }

    crate::tools::cpc_state::update_section_with_writer("operation", operation_state, args);
}

fn fingerprint_path() -> PathBuf {
    let root = breadcrumb_root();
    let learning = root
        .parent()
        .map(|p| p.join("learning"))
        .unwrap_or_else(|| root.join("learning"));
    let _ = std::fs::create_dir_all(&learning);
    learning.join("process_fingerprints.jsonl")
}

fn classify_task_type(name: &str) -> &'static str {
    let lower = name.to_lowercase();
    let checks: &[(&[&str], &str)] = &[
        (&["browser", "web", "scrape"], "browser_automation"),
        (&["build", "cargo", "deploy"], "build_deploy"),
        (&["extract", "learn", "insight"], "extraction"),
        (&["file", "edit", "write", "read"], "file_operations"),
        (&["git", "commit", "push"], "git_operations"),
        (&["delegate", "task", "codex"], "delegation"),
        (&["research", "search", "query"], "research"),
        (&["voice", "speak", "listen"], "voice"),
        (&["install", "config", "setup"], "configuration"),
    ];
    for (keywords, task_type) in checks {
        if keywords.iter().any(|kw| lower.contains(kw)) {
            return task_type;
        }
    }
    "general"
}

fn save_fingerprint(op: &Operation, duration_secs: f64, summary: &str) -> Result<()> {
    let task_type = classify_task_type(&op.name);

    // Extract tool sequence from step names/results
    let tool_sequence: Vec<String> = op
        .step_results
        .iter()
        .map(|sr| {
            // Try to extract a tool-like name from the step name
            sr.step.clone()
        })
        .collect();

    // Count dead ends: steps with error/failed/retry in result
    let dead_ends = op
        .step_results
        .iter()
        .filter(|sr| {
            let r = sr.result.to_lowercase();
            r.contains("error") || r.contains("failed") || r.contains("retry")
        })
        .count();

    // Count backtracks: same step name appearing more than once
    let mut step_counts: HashMap<&str, usize> = HashMap::new();
    for sr in &op.step_results {
        *step_counts.entry(sr.step.as_str()).or_insert(0) += 1;
    }
    let backtrack_count: usize = step_counts
        .values()
        .filter(|&&c| c > 1)
        .map(|c| c - 1)
        .sum();

    let fingerprint = json!({
        "task_type": task_type,
        "operation_name": op.name,
        "tool_sequence": tool_sequence,
        "total_steps": op.step_results.len(),
        "dead_ends": dead_ends,
        "backtrack_count": backtrack_count,
        "duration_seconds": duration_secs,
        "outcome": "completed",
        "summary": summary,
        "files_changed_count": op.files_changed.len(),
        "owner": op.owner.actor,
        "timestamp": chrono::Local::now().to_rfc3339()
    });

    let path = fingerprint_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    use std::io::Write;
    writeln!(file, "{}", fingerprint)?;
    Ok(())
}

#[allow(dead_code)]
pub async fn get_efficiency_baseline(args: Value) -> Result<Value> {
    let task_type = args
        .get("task_type")
        .and_then(|v| v.as_str())
        .unwrap_or("general");
    let path = fingerprint_path();
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => {
            return Ok(json!({ "status": "no_data", "message": "No fingerprints recorded yet" }))
        }
    };

    let mut matching: Vec<Value> = content
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|v| v.get("task_type").and_then(|t| t.as_str()) == Some(task_type))
        .collect();

    if matching.len() < 2 {
        return Ok(json!({
            "status": "insufficient_data",
            "task_type": task_type,
            "count": matching.len(),
            "message": "Need at least 2 fingerprints for a baseline"
        }));
    }

    let count = matching.len() as f64;
    let avg_steps = matching
        .iter()
        .filter_map(|v| v.get("total_steps").and_then(|s| s.as_f64()))
        .sum::<f64>()
        / count;
    let avg_duration = matching
        .iter()
        .filter_map(|v| v.get("duration_seconds").and_then(|s| s.as_f64()))
        .sum::<f64>()
        / count;

    // Best sequence: completed outcome with lowest steps
    matching.sort_by(|a, b| {
        let sa = a
            .get("total_steps")
            .and_then(|s| s.as_u64())
            .unwrap_or(u64::MAX);
        let sb = b
            .get("total_steps")
            .and_then(|s| s.as_u64())
            .unwrap_or(u64::MAX);
        sa.cmp(&sb)
    });
    let best = matching
        .iter()
        .find(|v| v.get("outcome").and_then(|o| o.as_str()) == Some("completed"));
    let best_sequence = best
        .and_then(|v| v.get("tool_sequence"))
        .cloned()
        .unwrap_or(json!([]));

    Ok(json!({
        "status": "ok",
        "task_type": task_type,
        "count": matching.len(),
        "avg_steps": (avg_steps * 10.0).round() / 10.0,
        "avg_duration_seconds": (avg_duration * 10.0).round() / 10.0,
        "best_sequence": best_sequence
    }))
}

pub async fn start(args: Value) -> Result<Value> {
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("unnamed");
    let steps: Vec<String> = args
        .get("steps")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    if load_active().is_some() {
        anyhow::bail!("Operation already in progress. Complete or abort it first.");
    }

    let writer = agent_identity::identity_from_args(Some(&args));
    let unix_ts = chrono::Local::now().timestamp();
    let op_id = format!("bc_{}_{}", unix_ts, safe_operation_slug(name));
    let op = Operation {
        id: op_id,
        name: name.to_string(),
        steps: steps.clone(),
        current_step: 0,
        total_steps: steps.len(),
        started_at: chrono::Local::now().to_rfc3339(),
        step_results: Vec::new(),
        files_changed: Vec::new(),
        transcript_offset: crate::tools::transcripts::transcript_len(),
        owner: writer.clone(),
        last_writer: writer,
    };

    save_active(&op)?;
    log_event("start", &op, json!({ "steps": steps }));
    sync_operation_state(Some(&op), None, Some(&args));

    let auto_capture = crate::tools::extraction::auto_capture_project_signals(json!({
        "operation": name,
        "stage": "start",
        "transcript_entries": crate::tools::transcripts::transcript_recent(8)
    }))
    .await
    .unwrap_or(json!({ "accepted": 0, "extracted": 0 }));

    Ok(json!({
        "status": "started",
        "name": name,
        "steps": steps,
        "total_steps": steps.len(),
        "auto_capture": auto_capture
    }))
}

pub async fn step(args: Value) -> Result<Value> {
    let result_text = args.get("result").and_then(|v| v.as_str()).unwrap_or("");
    let files: Vec<String> = args
        .get("files_changed")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let mut op = load_active().ok_or_else(|| anyhow::anyhow!("No active operation"))?;
    let step_writer = agent_identity::identity_from_args(Some(&args));

    let step_name = op
        .steps
        .get(op.current_step)
        .cloned()
        .unwrap_or_else(|| format!("step_{}", op.current_step + 1));

    op.step_results.push(StepResult {
        step: step_name.clone(),
        result: result_text.to_string(),
        completed_at: chrono::Local::now().to_rfc3339(),
        files: files.clone(),
        writer: step_writer.clone(),
    });
    op.last_writer = step_writer;
    op.files_changed.extend(files);
    op.current_step += 1;
    let transcript_entries = crate::tools::transcripts::transcript_since(op.transcript_offset);
    op.transcript_offset += transcript_entries.len();

    save_active(&op)?;
    save_checkpoint(&op);
    sync_operation_state(Some(&op), None, Some(&args));
    log_event(
        "step",
        &op,
        json!({
            "step": step_name,
            "result": result_text,
            "files_changed": op.files_changed
        }),
    );

    let auto_capture = crate::tools::extraction::auto_capture_project_signals(json!({
        "operation": op.name,
        "stage": "step",
        "step": op.step_results.last().map(|s| s.step.clone()).unwrap_or_default(),
        "result": result_text,
        "files_changed": op.step_results.last().map(|s| s.files.clone()).unwrap_or_default(),
        "transcript_entries": transcript_entries
    }))
    .await
    .unwrap_or(json!({ "accepted": 0, "extracted": 0 }));

    let remaining = op.total_steps.saturating_sub(op.current_step);

    Ok(json!({
        "step_completed": step_name,
        "current": op.current_step,
        "total": op.total_steps,
        "remaining": remaining,
        "next_step": op.steps.get(op.current_step),
        "auto_capture": auto_capture
    }))
}

pub async fn complete(args: Value) -> Result<Value> {
    let summary = args.get("summary").and_then(|v| v.as_str()).unwrap_or("");

    let mut op = load_active().ok_or_else(|| anyhow::anyhow!("No active operation"))?;
    op.last_writer = agent_identity::identity_from_args(Some(&args));
    let completed_at = chrono::Local::now().to_rfc3339();
    let duration_secs = chrono::DateTime::parse_from_rfc3339(&completed_at)
        .ok()
        .zip(chrono::DateTime::parse_from_rfc3339(&op.started_at).ok())
        .map(|(end, start)| (end - start).num_milliseconds() as f64 / 1000.0)
        .unwrap_or(0.0);
    let transcript_entries = crate::tools::transcripts::transcript_since(op.transcript_offset);
    op.transcript_offset += transcript_entries.len();

    // Archive to completed_ops
    let dir = completed_dir();
    std::fs::create_dir_all(&dir)?;
    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let filename = format!("{}_{}.json", safe_operation_slug(&op.name), ts);
    std::fs::write(
        dir.join(&filename),
        serde_json::to_string_pretty(&json!({
            "operation": op,
            "summary": summary,
            "completed_at": chrono::Local::now().to_rfc3339()
        }))?,
    )?;

    log_event(
        "complete",
        &op,
        json!({
            "summary": summary,
            "steps_completed": op.step_results.len(),
            "files_changed": op.files_changed
        }),
    );
    sync_operation_state(
        None,
        Some(json!({
            "name": op.name,
            "operation_id": format!("{}_{}", safe_operation_slug(&op.name).to_lowercase(), ts),
            "summary": summary,
            "completed_at": completed_at,
            "files_modified": op.files_changed,
            "files_created": [],
            "duration_secs": duration_secs,
            "owner": op.owner,
            "writer": op.last_writer
        })),
        Some(&args),
    );

    let auto_capture = crate::tools::extraction::auto_capture_project_signals(json!({
        "operation": op.name,
        "stage": "complete",
        "summary": summary,
        "files_changed": op.files_changed,
        "transcript_entries": transcript_entries
    }))
    .await
    .unwrap_or(json!({ "accepted": 0, "extracted": 0 }));

    // Save process fingerprint for learning (non-fatal)
    let _ = save_fingerprint(&op, duration_secs, summary);

    clear_active();
    clear_checkpoint();

    Ok(json!({
        "status": "completed",
        "name": op.name,
        "steps_completed": op.step_results.len(),
        "files_changed": op.files_changed,
        "EXTRACT_NOW": true,
        "note": "Review work for extraction-worthy insights (3Q gate: Reusable? Specific? New?)",
        "auto_capture": auto_capture
    }))
}

pub async fn abort(args: Value) -> Result<Value> {
    let reason = args.get("reason").and_then(|v| v.as_str()).unwrap_or("");
    let mut op = load_active();
    if let Some(active) = op.as_mut() {
        active.last_writer = agent_identity::identity_from_args(Some(&args));
        log_event("abort", active, json!({ "reason": reason }));
    }
    sync_operation_state(None, None, Some(&args));
    clear_active();
    clear_checkpoint();

    Ok(json!({
        "status": "aborted",
        "name": op.as_ref().map(|o| o.name.as_str()).unwrap_or("none"),
        "reason": reason,
        "steps_completed": op.as_ref().map(|o| o.step_results.len()).unwrap_or(0)
    }))
}

pub async fn status(_args: Value) -> Result<Value> {
    match load_active() {
        Some(op) => {
            let last_activity = op
                .step_results
                .last()
                .map(|s| s.completed_at.clone())
                .unwrap_or_else(|| op.started_at.clone());

            // Check if modified files still exist
            let files_verified = op
                .files_changed
                .iter()
                .all(|f| std::path::Path::new(f).exists());

            let completed_summaries: Vec<Value> = op
                .step_results
                .iter()
                .map(|s| json!({"step": s.step, "result": s.result}))
                .collect();

            let remaining_steps: Vec<&String> = op.steps[op.current_step..].iter().collect();

            Ok(json!({
                "active": true,
                "name": op.name,
                "current_step": op.current_step,
                "total_steps": op.total_steps,
                "started_at": op.started_at,
                "last_activity": last_activity,
                "next_step": op.steps.get(op.current_step),
                "remaining_steps": remaining_steps,
                "completed_steps_summary": completed_summaries,
                "files_changed": op.files_changed,
                "files_verified": files_verified,
                "files_at_risk": op.files_changed,
                "recovery_available": op.current_step > 0,
                "resume_from_step": op.current_step
            }))
        }
        None => Ok(json!({ "active": false })),
    }
}

pub async fn backup(_args: Value) -> Result<Value> {
    let op = load_active();
    if let Some(op) = &op {
        let backup_dir = get_config().backup_dir.join("breadcrumbs");
        std::fs::create_dir_all(&backup_dir)?;
        let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let path = backup_dir.join(format!("breadcrumb_backup_{}.json", ts));
        std::fs::write(&path, serde_json::to_string_pretty(op)?)?;
        Ok(json!({ "status": "backed_up", "path": path.to_string_lossy() }))
    } else {
        Ok(json!({ "status": "nothing_to_backup" }))
    }
}

// === FILE NAVIGATION ===
// Generated: 2026-04-08T14:12:45
// Total: 569 lines | 22 functions | 2 structs | 0 constants
//
// IMPORTS: anyhow, crate, serde, serde_json, std
//
// STRUCTS:
//   Operation: 14-28
//   StepResult: 31-38
//
// FUNCTIONS:
//   active_path: 40-42
//   completed_dir: 44-46
//   checkpoint_path: 48-50
//   breadcrumb_log_path: 52-54
//   save_checkpoint: 56-70
//   clear_checkpoint: 72-74
//   load_active: 76-85
//   save_active: 87-94
//   clear_active: 96-98
//   safe_operation_slug: 100-121
//   log_event: 123-144
//   sync_operation_state: 146-188
//   fingerprint_path: 190-192
//   classify_task_type: 194-213
//   save_fingerprint: 215-264
//   pub +get_efficiency_baseline: 266-318 [med]
//   pub +start: 320-362
//   pub +step: 364-423 [med]
//   pub +complete: 425-497 [med]
//   pub +abort: 499-516
//   pub +status: 518-555
//   pub +backup: 557-569
//
// === END FILE NAVIGATION ===
