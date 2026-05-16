//! Extraction stub - ops does not run the full extraction stack.
#![allow(dead_code)]
use anyhow::Result;
use serde_json::{json, Value};

pub async fn detect_triggers(_args: Value) -> Result<Value> {
    Ok(json!({"triggers": [], "note": "extraction not loaded in ops"}))
}

pub async fn auto_capture_project_signals(_args: Value) -> Result<Value> {
    Ok(json!({"accepted": 0, "evaluated": 0, "note": "extraction not loaded in ops"}))
}
