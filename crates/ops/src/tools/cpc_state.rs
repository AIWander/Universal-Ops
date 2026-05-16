//! state.json - Unified state file with additive writer attribution metadata.
#![allow(dead_code)]
//! Single source of truth read by dashboard and autonomous:boot.
//! Each tool updates only its own section via update_section().

use serde_json::{json, Value};
use std::fs;

use crate::agent_identity;

fn state_file() -> std::path::PathBuf {
    std::env::var("OPS_STATE_FILE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("LOCALAPPDATA")
                .map(|p| std::path::PathBuf::from(p).join("Ops").join("state.json"))
                .unwrap_or_else(|_| std::path::PathBuf::from("state.json"))
        })
}

/// Read full state, or create default if missing
pub fn read_state() -> Value {
    if let Ok(content) = fs::read_to_string(state_file()) {
        if let Ok(mut val) = serde_json::from_str::<Value>(&content) {
            ensure_state_schema(&mut val);
            return val;
        }
    }
    init_state()
}

/// Update a single top-level section atomically
pub fn update_section(section: &str, value: Value) {
    update_section_with_writer(section, value, None);
}

pub fn update_section_with_writer(section: &str, value: Value, args: Option<&Value>) {
    let mut state = read_state();
    {
        if let Some(obj) = state.as_object_mut() {
            obj.insert(section.to_string(), value);
        }
    }
    stamp_state_write(&mut state, Some(section), args);
    write_state(&state);
}

/// Update a nested field: update_nested("health", "ollama", value)
pub fn update_nested(section: &str, key: &str, value: Value) {
    update_nested_with_writer(section, key, value, None);
}

pub fn update_nested_with_writer(section: &str, key: &str, value: Value, args: Option<&Value>) {
    let mut state = read_state();
    {
        if let Some(obj) = state.as_object_mut() {
            let sec = obj.entry(section).or_insert_with(|| json!({}));
            if !sec.is_object() {
                *sec = json!({});
            }
            if let Some(sec_obj) = sec.as_object_mut() {
                sec_obj.insert(key.to_string(), value);
            }
        }
    }
    stamp_state_write(&mut state, Some(section), args);
    write_state(&state);
}

/// Write state atomically (temp file + rename) - public for multi-section updates
pub fn write_state(state: &Value) {
    let sf = state_file();
    let tmp = sf.with_extension("json.tmp");
    if let Ok(content) = serde_json::to_string_pretty(state) {
        if fs::write(&tmp, &content).is_ok() {
            if sf.exists() {
                let _ = fs::remove_file(&sf);
            }
            let _ = fs::rename(&tmp, &sf);
        }
    }
}

fn ensure_state_schema(state: &mut Value) {
    if !state.is_object() {
        *state = init_state();
        return;
    }

    let obj = state.as_object_mut().unwrap();
    obj.insert("_schema".to_string(), json!("OPS_STATE v1.1"));
    obj.entry("_updated".to_string())
        .or_insert_with(|| json!(chrono::Local::now().to_rfc3339()));

    let meta = obj.entry("_meta".to_string()).or_insert_with(|| json!({}));
    if !meta.is_object() {
        *meta = json!({});
    }
    if let Some(meta_obj) = meta.as_object_mut() {
        meta_obj
            .entry("last_writer".to_string())
            .or_insert(Value::Null);
        meta_obj
            .entry("sections".to_string())
            .or_insert_with(|| json!({}));
    }

    let operation = obj
        .entry("operation".to_string())
        .or_insert_with(|| json!({}));
    if !operation.is_object() {
        *operation = json!({});
    }
    if let Some(operation_obj) = operation.as_object_mut() {
        operation_obj
            .entry("active".to_string())
            .or_insert(Value::Null);
        operation_obj
            .entry("last_completed".to_string())
            .or_insert(Value::Null);
        operation_obj
            .entry("owner_agent".to_string())
            .or_insert(Value::Null);
        operation_obj
            .entry("last_writer".to_string())
            .or_insert(Value::Null);
    }

    let session = obj
        .entry("session".to_string())
        .or_insert_with(|| json!({}));
    if !session.is_object() {
        *session = json!({});
    }
    if let Some(session_obj) = session.as_object_mut() {
        session_obj.entry("id".to_string()).or_insert(Value::Null);
        session_obj
            .entry("started_at".to_string())
            .or_insert(Value::Null);
        session_obj
            .entry("topic".to_string())
            .or_insert(Value::Null);
        session_obj.entry("turns".to_string()).or_insert(json!(0));
        session_obj
            .entry("tools_used".to_string())
            .or_insert(json!(0));
        session_obj
            .entry("extractions_this_session".to_string())
            .or_insert(json!(0));
        session_obj
            .entry("calls_since_extraction_check".to_string())
            .or_insert(json!(0));
        session_obj
            .entry("current_agent".to_string())
            .or_insert(Value::Null);
        session_obj
            .entry("last_writer".to_string())
            .or_insert(Value::Null);
        session_obj
            .entry("contributors".to_string())
            .or_insert_with(|| json!([]));
        session_obj
            .entry("thread_id".to_string())
            .or_insert(Value::Null);
        session_obj
            .entry("client_session_id".to_string())
            .or_insert(Value::Null);
    }

    let tasks = obj.entry("tasks".to_string()).or_insert_with(|| json!({}));
    if !tasks.is_object() {
        *tasks = json!({});
    }
    if let Some(tasks_obj) = tasks.as_object_mut() {
        tasks_obj
            .entry("pending".to_string())
            .or_insert_with(|| json!([]));
        tasks_obj
            .entry("completed_today".to_string())
            .or_insert_with(|| json!([]));
        tasks_obj
            .entry("task_log".to_string())
            .or_insert_with(|| json!([]));
    }

    let health = obj.entry("health".to_string()).or_insert_with(|| json!({}));
    if !health.is_object() {
        *health = json!({});
    }
    if let Some(health_obj) = health.as_object_mut() {
        health_obj
            .entry("last_check".to_string())
            .or_insert(Value::Null);
        health_obj
            .entry("servers".to_string())
            .or_insert_with(|| json!({}));
        health_obj
            .entry("ollama".to_string())
            .or_insert_with(|| json!({"status": "unknown", "models": [], "ram_gb": null}));
    }

    let extractions = obj
        .entry("extractions".to_string())
        .or_insert_with(|| json!({}));
    if !extractions.is_object() {
        *extractions = json!({});
    }
    if let Some(extractions_obj) = extractions.as_object_mut() {
        extractions_obj
            .entry("recent".to_string())
            .or_insert_with(|| json!([]));
        extractions_obj
            .entry("today_count".to_string())
            .or_insert(json!(0));
        extractions_obj
            .entry("inbox_pending".to_string())
            .or_insert(json!(0));
    }

    let self_state = obj
        .entry("self_state".to_string())
        .or_insert_with(|| json!({}));
    if !self_state.is_object() {
        *self_state = json!({});
    }
    if let Some(self_state_obj) = self_state.as_object_mut() {
        self_state_obj
            .entry("current_goal".to_string())
            .or_insert(Value::Null);
        self_state_obj
            .entry("current_subgoal".to_string())
            .or_insert(Value::Null);
        self_state_obj
            .entry("blockers".to_string())
            .or_insert_with(|| json!([]));
        self_state_obj
            .entry("assumptions_in_force".to_string())
            .or_insert_with(|| json!([]));
        self_state_obj
            .entry("session_changes".to_string())
            .or_insert_with(|| json!([]));
        self_state_obj
            .entry("active_context".to_string())
            .or_insert_with(|| json!([]));
        self_state_obj
            .entry("mood_signal".to_string())
            .or_insert(Value::Null);
        self_state_obj
            .entry("delegation_state".to_string())
            .or_insert_with(|| json!([]));
        self_state_obj
            .entry("last_updated".to_string())
            .or_insert(Value::Null);
        self_state_obj
            .entry("last_writer".to_string())
            .or_insert(Value::Null);
    }

    let context = obj
        .entry("context".to_string())
        .or_insert_with(|| json!({}));
    if !context.is_object() {
        *context = json!({});
    }
    if let Some(context_obj) = context.as_object_mut() {
        context_obj
            .entry("platform".to_string())
            .or_insert(Value::Null);
        context_obj
            .entry("warm_topics".to_string())
            .or_insert_with(|| json!([]));
        context_obj
            .entry("key_decisions".to_string())
            .or_insert_with(|| json!([]));
        context_obj
            .entry("warnings".to_string())
            .or_insert_with(|| json!([]));
        context_obj
            .entry("key_decision_log".to_string())
            .or_insert_with(|| json!([]));
        context_obj
            .entry("warning_log".to_string())
            .or_insert_with(|| json!([]));
    }

    let errors = obj.entry("errors".to_string()).or_insert_with(|| json!({}));
    if !errors.is_object() {
        *errors = json!({});
    }
    if let Some(errors_obj) = errors.as_object_mut() {
        errors_obj
            .entry("recent".to_string())
            .or_insert_with(|| json!([]));
        errors_obj
            .entry("fallback_patterns".to_string())
            .or_insert_with(|| json!({}));
        errors_obj
            .entry("error_log".to_string())
            .or_insert_with(|| json!([]));
    }
}

fn stamp_state_write(state: &mut Value, section: Option<&str>, args: Option<&Value>) {
    ensure_state_schema(state);
    let writer = agent_identity::identity_json(args);
    let writer_actor = writer
        .get("actor")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let timestamp = chrono::Local::now().to_rfc3339();

    let obj = state.as_object_mut().unwrap();
    obj.insert("_updated".to_string(), json!(timestamp.clone()));

    if let Some(meta_obj) = obj.get_mut("_meta").and_then(|v| v.as_object_mut()) {
        meta_obj.insert("last_writer".to_string(), writer.clone());
        if let Some(section_name) = section {
            let sections = meta_obj
                .entry("sections".to_string())
                .or_insert_with(|| json!({}));
            if let Some(sections_obj) = sections.as_object_mut() {
                sections_obj.insert(
                    section_name.to_string(),
                    json!({
                        "writer": writer.clone(),
                        "updated_at": timestamp
                    }),
                );
            }
        }
    }

    if let Some(session_obj) = obj.get_mut("session").and_then(|v| v.as_object_mut()) {
        session_obj.insert("last_writer".to_string(), writer.clone());
        if matches!(
            section,
            Some("session") | Some("self_state") | Some("operation")
        ) {
            session_obj.insert("current_agent".to_string(), json!(writer_actor.clone()));
            if let Some(thread_id) = writer.get("thread_id").cloned() {
                if !thread_id.is_null() {
                    session_obj.insert("thread_id".to_string(), thread_id);
                }
            }
            if let Some(session_id) = writer.get("session_id").cloned() {
                if !session_id.is_null() {
                    session_obj.insert("client_session_id".to_string(), session_id);
                }
            }
            let contributors = session_obj
                .entry("contributors".to_string())
                .or_insert_with(|| json!([]));
            if let Some(arr) = contributors.as_array_mut() {
                let exists = arr
                    .iter()
                    .any(|item| item.as_str() == Some(writer_actor.as_str()));
                if !exists {
                    arr.push(json!(writer_actor.clone()));
                }
            }
        }
    }

    if let Some(operation_obj) = obj.get_mut("operation").and_then(|v| v.as_object_mut()) {
        operation_obj.insert("last_writer".to_string(), writer.clone());
        if section == Some("operation") {
            operation_obj.insert("owner_agent".to_string(), json!(writer_actor));
        }
    }

    if section == Some("self_state") {
        if let Some(self_state_obj) = obj.get_mut("self_state").and_then(|v| v.as_object_mut()) {
            self_state_obj.insert("last_writer".to_string(), writer);
        }
    }
}

/// Create default state file
fn init_state() -> Value {
    let mut state = json!({
        "_schema": "OPS_STATE v1.1",
        "_updated": chrono::Local::now().to_rfc3339(),
        "_meta": {
            "last_writer": null,
            "sections": {}
        },
        "operation": {
            "active": null,
            "owner_agent": null,
            "last_writer": null,
            "last_completed": null
        },
        "session": {
            "id": null,
            "started_at": null,
            "topic": null,
            "turns": 0,
            "tools_used": 0,
            "extractions_this_session": 0,
            "calls_since_extraction_check": 0,
            "current_agent": null,
            "last_writer": null,
            "contributors": [],
            "thread_id": null,
            "client_session_id": null
        },
        "tasks": {
            "pending": [],
            "completed_today": [],
            "task_log": []
        },
        "health": {
            "last_check": null,
            "servers": {},
            "ollama": {"status": "unknown", "models": [], "ram_gb": null}
        },
        "extractions": {
            "recent": [],
            "today_count": 0,
            "inbox_pending": 0
        },
        "self_state": {
            "current_goal": null,
            "current_subgoal": null,
            "blockers": [],
            "assumptions_in_force": [],
            "session_changes": [],
            "active_context": [],
            "mood_signal": null,
            "delegation_state": [],
            "last_updated": null,
            "last_writer": null
        },
        "context": {
            "platform": null,
            "warm_topics": [],
            "key_decisions": [],
            "warnings": [],
            "key_decision_log": [],
            "warning_log": []
        },
        "errors": {
            "recent": [],
            "fallback_patterns": {},
            "error_log": []
        }
    });
    ensure_state_schema(&mut state);
    write_state(&state);
    state
}

/// Increment tool call counter. Returns extraction hint every N calls.
const EXTRACTION_CHECK_INTERVAL: u64 = 5;

pub fn check_extraction_due(tool_name: &str) -> Option<Value> {
    let mut state = read_state();
    let session = state.get_mut("session").and_then(|s| s.as_object_mut());

    let session = session?;

    let tools_used = session
        .get("tools_used")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        + 1;
    session.insert("tools_used".to_string(), json!(tools_used));

    let calls = session
        .get("calls_since_extraction_check")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        + 1;

    let is_extraction_tool = matches!(
        tool_name,
        "post_turn" | "extraction_sweep" | "scan_output" | "extract" | "heuristics"
    );

    if is_extraction_tool {
        session.insert("calls_since_extraction_check".to_string(), json!(0));
        stamp_state_write(&mut state, Some("session"), None);
        write_state(&state);
        return None;
    }

    session.insert("calls_since_extraction_check".to_string(), json!(calls));
    stamp_state_write(&mut state, Some("session"), None);
    write_state(&state);

    if calls >= EXTRACTION_CHECK_INTERVAL {
        Some(json!({
            "_extraction_hint": {
                "due": true,
                "calls_since_last_check": calls,
                "action": "Run echo:heuristics on recent conversation or autonomous:post_turn to check for extractable patterns",
                "priority": if calls >= EXTRACTION_CHECK_INTERVAL * 2 { "high" } else { "normal" }
            }
        }))
    } else {
        None
    }
}

/// Update self-state block (Letta-style living session dossier)
pub fn update_self_state(args: &Value) -> Value {
    let mut state = read_state();
    let writer = agent_identity::identity_json(Some(args));
    let writer_actor = writer
        .get("actor")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let self_state = state
        .as_object_mut()
        .and_then(|obj| {
            obj.entry("self_state")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .cloned()
        })
        .unwrap_or_default();

    let mut new_state = serde_json::Map::from_iter(self_state);

    if let Some(goal) = args.get("current_goal") {
        new_state.insert("current_goal".to_string(), goal.clone());
    }
    if let Some(subgoal) = args.get("current_subgoal") {
        new_state.insert("current_subgoal".to_string(), subgoal.clone());
    }
    if let Some(blockers) = args.get("blockers") {
        new_state.insert("blockers".to_string(), blockers.clone());
    }
    if let Some(assumptions) = args.get("assumptions") {
        new_state.insert("assumptions_in_force".to_string(), assumptions.clone());
    }
    if let Some(changes) = args.get("session_changes") {
        let mut existing = new_state
            .get("session_changes")
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        if let Some(new_changes) = changes.as_array() {
            existing.extend(new_changes.iter().cloned());
        } else if let Some(s) = changes.as_str() {
            existing.push(json!({
                "what": s,
                "when": chrono::Local::now().format("%H:%M").to_string(),
                "actor": writer_actor
            }));
        }
        while existing.len() > 20 {
            existing.remove(0);
        }
        new_state.insert("session_changes".to_string(), json!(existing));
    }
    if let Some(ctx) = args.get("active_context") {
        new_state.insert("active_context".to_string(), ctx.clone());
    }
    if let Some(mood) = args.get("mood_signal") {
        new_state.insert("mood_signal".to_string(), mood.clone());
    }
    if let Some(delegation) = args.get("delegation_state") {
        new_state.insert("delegation_state".to_string(), delegation.clone());
    }

    new_state.insert(
        "last_updated".to_string(),
        json!(chrono::Local::now().to_rfc3339()),
    );
    new_state.insert("last_writer".to_string(), writer.clone());

    let new_state_value = Value::Object(new_state.clone());
    update_section_with_writer("self_state", new_state_value.clone(), Some(args));

    json!({
        "status": "ok",
        "self_state": new_state_value,
        "message": "Self-state updated"
    })
}

/// Read current self-state block
pub fn read_self_state() -> Value {
    let state = read_state();
    state.get("self_state").cloned().unwrap_or(json!({
        "current_goal": null,
        "current_subgoal": null,
        "blockers": [],
        "assumptions_in_force": [],
        "session_changes": [],
        "active_context": [],
        "mood_signal": null,
        "delegation_state": [],
        "last_updated": null,
        "last_writer": null
    }))
}
