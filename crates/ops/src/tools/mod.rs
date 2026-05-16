//! Core tool modules ported from autonomous.
//! Covers: breadcrumbs, health, reminders, dead drop, bag/tag.
//! Extended: files, transforms, sessions, utils, sqlite, config_ops.

pub mod bagtag;
pub mod breadcrumb;
pub mod cpc_state;
pub mod health;
pub mod reminder;
// Stubs — ops doesn't carry the full knowledge stack
pub mod extraction;
pub mod transcripts;
// Ported from local crate
pub mod config_ops;
pub mod files;
pub mod sessions;
pub mod sqlite;
pub mod utils;
pub mod xforms;

use once_cell::sync::Lazy;
use serde_json::{json, Value};
use tokio::runtime::Runtime;

/// Shared async runtime for all core tool calls.
static RT: Lazy<Runtime> = Lazy::new(|| Runtime::new().expect("core tools tokio runtime"));

fn tool_def(name: &str, description: &str, schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": schema
    })
}

/// All tool definitions for the core set.
pub fn get_definitions() -> Vec<Value> {
    let mut defs = vec![
        // === SYSTEM ===
        tool_def(
            "status",
            "Check system or topic status.",
            json!({
                "type": "object",
                "properties": { "topic": { "type": "string" } }
            }),
        ),
        // === BREADCRUMBS ===
        tool_def(
            "breadcrumb_start",
            "Start tracked operation with planned steps.",
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "steps": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["name", "steps"]
            }),
        ),
        tool_def(
            "breadcrumb_step",
            "Log step completion, auto-advances to next.",
            json!({
                "type": "object",
                "properties": {
                    "result": { "type": "string" },
                    "files_changed": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["result"]
            }),
        ),
        tool_def(
            "breadcrumb_complete",
            "Mark operation complete, trigger extraction review.",
            json!({
                "type": "object",
                "properties": { "summary": { "type": "string" } }
            }),
        ),
        tool_def(
            "breadcrumb_abort",
            "Abort current operation with reason.",
            json!({
                "type": "object",
                "properties": { "reason": { "type": "string" } },
                "required": ["reason"]
            }),
        ),
        tool_def(
            "breadcrumb_status",
            "Get current operation status and progress.",
            json!({
                "type": "object", "properties": {}
            }),
        ),
        tool_def(
            "breadcrumb_backup",
            "Snapshot breadcrumb state before irreversible ops.",
            json!({
                "type": "object", "properties": {}
            }),
        ),
        // === HEALTH & RECOVERY ===
        tool_def(
            "system_health_check",
            "Check server health and update dashboard.",
            json!({
                "type": "object", "properties": {}
            }),
        ),
        tool_def(
            "system_health_report",
            "Get current health dashboard.",
            json!({
                "type": "object", "properties": {}
            }),
        ),
        tool_def(
            "checkpoint_save",
            "Save working memory state (survives context compaction).",
            json!({
                "type": "object",
                "properties": { "data": { "type": "object" } },
                "required": ["data"]
            }),
        ),
        tool_def(
            "checkpoint_load",
            "Load last checkpoint.",
            json!({
                "type": "object", "properties": {}
            }),
        ),
        tool_def(
            "checkpoint_clear",
            "Clear checkpoint after task completion.",
            json!({
                "type": "object", "properties": {}
            }),
        ),
        tool_def(
            "git_rollback",
            "Rollback rust-mcp repo to a previous commit.",
            json!({
                "type": "object",
                "properties": { "commit": { "type": "string" } },
                "required": ["commit"]
            }),
        ),
        // === REMINDERS ===
        tool_def(
            "reminder_add",
            "Create reminder with natural language time parsing.",
            json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" },
                    "due": { "type": "string", "description": "Natural language: 'tomorrow 9am', 'in 2 hours', ISO8601" },
                    "recurring": { "type": "string", "enum": ["daily", "weekly", "monthly"] }
                },
                "required": ["text", "due"]
            }),
        ),
        tool_def(
            "reminder_list",
            "List reminders with optional filter.",
            json!({
                "type": "object",
                "properties": {
                    "filter": { "type": "string", "enum": ["pending", "completed", "overdue", "all"], "default": "pending" }
                }
            }),
        ),
        tool_def(
            "reminder_complete",
            "Mark reminder completed. Auto-creates next if recurring.",
            json!({
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"]
            }),
        ),
        tool_def(
            "reminder_delete",
            "Permanently remove a reminder.",
            json!({
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"]
            }),
        ),
        tool_def(
            "reminder_check_due",
            "Return all reminders that are due now or overdue.",
            json!({
                "type": "object", "properties": {}
            }),
        ),
        tool_def(
            "reminder_add_recurring",
            "Add recurring reminder (daily/weekly/monthly).",
            json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" },
                    "schedule": { "type": "string" },
                    "time": { "type": "string", "description": "Time of day, e.g. '9:00 AM'" }
                },
                "required": ["text", "schedule"]
            }),
        ),
        tool_def(
            "reminder_add_scheduled",
            "Create Windows Task Scheduler reminder.",
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "time": { "type": "string" },
                    "message": { "type": "string" }
                },
                "required": ["name", "time", "message"]
            }),
        ),
        tool_def(
            "reminder_list_scheduled",
            "List Windows Task Scheduler CPC reminders.",
            json!({
                "type": "object", "properties": {}
            }),
        ),
        tool_def(
            "reminder_delete_scheduled",
            "Delete Windows Task Scheduler reminder by name.",
            json!({
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            }),
        ),
        tool_def(
            "system_time_check",
            "Check elapsed time and re-surface reminders if 3+ hours passed.",
            json!({
                "type": "object", "properties": {}
            }),
        ),
        // === DEAD DROP (inter-agent messaging) ===
        tool_def(
            "dead_drop_leave",
            "Leave message in dead drop for other AI agents to find at boot.",
            json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string" },
                    "from": { "type": "string", "default": "ops" },
                    "priority": { "type": "string", "enum": ["low", "normal", "high"], "default": "normal" }
                },
                "required": ["message"]
            }),
        ),
        tool_def(
            "dead_drop_check",
            "Check dead drop for unread messages.",
            json!({
                "type": "object", "properties": {}
            }),
        ),
        tool_def(
            "dead_drop_clear",
            "Mark dead drop messages as read.",
            json!({
                "type": "object",
                "properties": { "id": { "type": "string", "description": "Optional: clear specific message. Omit to clear all." } }
            }),
        ),
        // === BAG / TAG (clipboard scratchpad) ===
        tool_def(
            "bag_tag",
            "Tag items into the in-memory bag for later retrieval.",
            json!({
                "type": "object", "properties": {}
            }),
        ),
        tool_def(
            "bag_read",
            "Read current bag contents.",
            json!({
                "type": "object", "properties": {}
            }),
        ),
        tool_def(
            "bag_clear",
            "Clear the bag.",
            json!({
                "type": "object", "properties": {}
            }),
        ),
    ];

    // Append definitions from new ported modules
    defs.extend(files::get_definitions());
    defs.extend(xforms::get_definitions());
    defs.extend(sessions::get_definitions());
    defs.extend(utils::get_definitions());
    defs.extend(sqlite::get_definitions());
    defs.extend(config_ops::get_definitions());

    defs
}

/// Sync dispatch — wraps async tool functions via the shared runtime.
pub fn execute(name: &str, args: &Value) -> Option<Value> {
    let a = args.clone();
    match name {
        // System
        "status" => Some(RT.block_on(health::status(a)).unwrap_or_else(err_val)),

        // Breadcrumbs
        "breadcrumb_start" => Some(RT.block_on(breadcrumb::start(a)).unwrap_or_else(err_val)),
        "breadcrumb_step" => Some(RT.block_on(breadcrumb::step(a)).unwrap_or_else(err_val)),
        "breadcrumb_complete" => Some(RT.block_on(breadcrumb::complete(a)).unwrap_or_else(err_val)),
        "breadcrumb_abort" => Some(RT.block_on(breadcrumb::abort(a)).unwrap_or_else(err_val)),
        "breadcrumb_status" => Some(RT.block_on(breadcrumb::status(a)).unwrap_or_else(err_val)),
        "breadcrumb_backup" => Some(RT.block_on(breadcrumb::backup(a)).unwrap_or_else(err_val)),

        // Health
        "system_health_check" => Some(RT.block_on(health::health_check(a)).unwrap_or_else(err_val)),
        "system_health_report" => Some(
            RT.block_on(health::health_report(a))
                .unwrap_or_else(err_val),
        ),
        "checkpoint_save" => Some(
            RT.block_on(health::checkpoint_save(a))
                .unwrap_or_else(err_val),
        ),
        "checkpoint_load" => Some(
            RT.block_on(health::checkpoint_load(a))
                .unwrap_or_else(err_val),
        ),
        "checkpoint_clear" => Some(
            RT.block_on(health::checkpoint_clear(a))
                .unwrap_or_else(err_val),
        ),
        "git_rollback" => Some(RT.block_on(health::git_rollback(a)).unwrap_or_else(err_val)),

        // Reminders + dead drop
        "reminder_add" => Some(
            RT.block_on(reminder::add_reminder(a))
                .unwrap_or_else(err_val),
        ),
        "reminder_list" => Some(
            RT.block_on(reminder::list_reminders(a))
                .unwrap_or_else(err_val),
        ),
        "reminder_complete" => Some(
            RT.block_on(reminder::complete_reminder(a))
                .unwrap_or_else(err_val),
        ),
        "reminder_delete" => Some(
            RT.block_on(reminder::delete_reminder(a))
                .unwrap_or_else(err_val),
        ),
        "reminder_check_due" => Some(RT.block_on(reminder::check_due(a)).unwrap_or_else(err_val)),
        "reminder_add_recurring" => Some(
            RT.block_on(reminder::add_recurring(a))
                .unwrap_or_else(err_val),
        ),
        "reminder_add_scheduled" => Some(
            RT.block_on(reminder::add_scheduled(a))
                .unwrap_or_else(err_val),
        ),
        "reminder_list_scheduled" => Some(
            RT.block_on(reminder::list_scheduled(a))
                .unwrap_or_else(err_val),
        ),
        "reminder_delete_scheduled" => Some(
            RT.block_on(reminder::delete_scheduled(a))
                .unwrap_or_else(err_val),
        ),
        "system_time_check" => Some(RT.block_on(reminder::time_check(a)).unwrap_or_else(err_val)),
        "dead_drop_leave" => Some(
            RT.block_on(reminder::dead_drop_leave(a))
                .unwrap_or_else(err_val),
        ),
        "dead_drop_check" => Some(
            RT.block_on(reminder::dead_drop_check(a))
                .unwrap_or_else(err_val),
        ),
        "dead_drop_clear" => Some(
            RT.block_on(reminder::dead_drop_clear(a))
                .unwrap_or_else(err_val),
        ),

        // Bag/tag
        "bag_tag" => Some(RT.block_on(bagtag::bag_tag(a)).unwrap_or_else(err_val)),
        "bag_read" => Some(RT.block_on(bagtag::bag_read(a)).unwrap_or_else(err_val)),
        "bag_clear" => Some(RT.block_on(bagtag::bag_clear(a)).unwrap_or_else(err_val)),

        // File I/O (ported from local)
        "read_file" | "write_file" | "append_file" | "list_dir" | "tail_file" => {
            Some(files::execute(name, &a))
        }

        // Transforms (ported from local)
        "transform_grep"
        | "transform_extract_lines"
        | "transform_diff_file"
        | "transform_diff_files"
        | "transform_find_replace"
        | "transform_json_format"
        | "transform_hash_file"
        | "transform_file_stats" => Some(xforms::execute(name, &a)),

        // Sessions
        "session_create" | "session_run" | "session_cd" | "session_set_env" | "session_get_env"
        | "session_list" | "session_destroy" => Some(sessions::execute(name, &a)),

        // Utilities (ported from local)
        "clipboard_read" | "clipboard_write" | "notify" | "kill_process" | "list_process"
        | "port_check" => Some(utils::execute(name, &a)),

        // SQLite
        "sqlite_query" => Some(sqlite::execute(name, &a)),

        // Config ops (ported from local)
        "config_backup" | "config_validate" => Some(config_ops::execute(name, &a)),

        _ => None,
    }
}

fn err_val(e: anyhow::Error) -> Value {
    json!({"error": e.to_string()})
}
