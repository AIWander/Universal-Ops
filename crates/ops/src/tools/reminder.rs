//! Reminders — natural language time, recurring, Windows Task Scheduler, dead drop.
//! Ported from reminder-rs, fully native.

use anyhow::Result;
use chrono::{DateTime, Datelike, Duration, Local, NaiveTime, Utc, Weekday};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use uuid::Uuid;

use crate::config::get_config;

#[derive(Serialize, Deserialize, Clone)]
struct Reminder {
    id: String,
    text: String,
    due: String,
    created: String,
    completed: Option<String>,
    recurring: Option<String>,
    status: String,
}

#[derive(Serialize, Deserialize)]
struct ReminderStore {
    reminders: Vec<Reminder>,
}

#[derive(Serialize, Deserialize, Clone)]
struct DeadDropMessage {
    id: String,
    date: String,
    message: String,
    from: String,
    priority: String,
    created: String,
    read: bool,
}

fn reminders_path() -> PathBuf {
    get_config().data_dir.join("reminders.json")
}

fn dead_drop_path() -> PathBuf {
    get_config().data_dir.join("dead_drop.json")
}

fn load_reminders() -> ReminderStore {
    let path = reminders_path();
    if path.exists() {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(ReminderStore {
                reminders: Vec::new(),
            })
    } else {
        ReminderStore {
            reminders: Vec::new(),
        }
    }
}

fn save_reminders(store: &ReminderStore) -> Result<()> {
    let path = reminders_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(store)?)?;
    Ok(())
}

fn parse_due(input: &str) -> Result<DateTime<Utc>> {
    let s = input.trim().to_lowercase();
    let now = Local::now();

    // ISO8601 passthrough
    if let Ok(dt) = DateTime::parse_from_rfc3339(input) {
        return Ok(dt.with_timezone(&Utc));
    }

    // Relative: "in X hours/minutes/days"
    if s.starts_with("in ") {
        let parts: Vec<&str> = s
            .strip_prefix("in ")
            .unwrap_or("")
            .split_whitespace()
            .collect();
        if parts.len() >= 2 {
            if let Ok(n) = parts[0].parse::<i64>() {
                let unit = parts[1];
                let dur = match unit {
                    u if u.starts_with("min") => Duration::minutes(n),
                    u if u.starts_with("hour") => Duration::hours(n),
                    u if u.starts_with("day") => Duration::days(n),
                    u if u.starts_with("week") => Duration::weeks(n),
                    _ => Duration::hours(n),
                };
                return Ok((now + dur).with_timezone(&Utc));
            }
        }
    }

    // "tomorrow" / "tomorrow at 9am"
    if s.starts_with("tomorrow") {
        let tomorrow = now + Duration::days(1);
        let time = extract_time_from_str(&s).unwrap_or(NaiveTime::from_hms_opt(9, 0, 0).unwrap());
        let dt = tomorrow.date_naive().and_time(time);
        return Ok(dt.and_local_timezone(Local).unwrap().with_timezone(&Utc));
    }

    // "next monday/tuesday/etc"
    let weekdays = [
        ("monday", Weekday::Mon),
        ("tuesday", Weekday::Tue),
        ("wednesday", Weekday::Wed),
        ("thursday", Weekday::Thu),
        ("friday", Weekday::Fri),
        ("saturday", Weekday::Sat),
        ("sunday", Weekday::Sun),
    ];
    for (name, wd) in &weekdays {
        if s.contains(name) {
            let mut target = now + Duration::days(1);
            while target.weekday() != *wd {
                target += Duration::days(1);
            }
            let time =
                extract_time_from_str(&s).unwrap_or(NaiveTime::from_hms_opt(9, 0, 0).unwrap());
            let dt = target.date_naive().and_time(time);
            return Ok(dt.and_local_timezone(Local).unwrap().with_timezone(&Utc));
        }
    }

    // Fallback: 1 hour from now
    Ok((now + Duration::hours(1)).with_timezone(&Utc))
}

fn extract_time_from_str(s: &str) -> Option<NaiveTime> {
    // Look for patterns like "9am", "9:30am", "9:30 pm", "14:00"
    let re = regex::Regex::new(r"(\d{1,2}):?(\d{2})?\s*(am|pm)?").ok()?;
    if let Some(caps) = re.captures(s) {
        let mut hour: u32 = caps.get(1)?.as_str().parse().ok()?;
        let min: u32 = caps
            .get(2)
            .map(|m| m.as_str().parse().unwrap_or(0))
            .unwrap_or(0);
        if let Some(ampm) = caps.get(3) {
            if ampm.as_str() == "pm" && hour < 12 {
                hour += 12;
            }
            if ampm.as_str() == "am" && hour == 12 {
                hour = 0;
            }
        }
        return NaiveTime::from_hms_opt(hour, min, 0);
    }
    None
}

pub async fn add_reminder(args: Value) -> Result<Value> {
    let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
    let due_str = args
        .get("due")
        .and_then(|v| v.as_str())
        .unwrap_or("in 1 hour");
    let recurring = args
        .get("recurring")
        .and_then(|v| v.as_str())
        .map(String::from);

    let due = parse_due(due_str)?;
    let reminder = Reminder {
        id: Uuid::new_v4().to_string()[..8].to_string(),
        text: text.to_string(),
        due: due.to_rfc3339(),
        created: Utc::now().to_rfc3339(),
        completed: None,
        recurring,
        status: "pending".to_string(),
    };

    let mut store = load_reminders();
    store.reminders.push(reminder.clone());
    save_reminders(&store)?;

    Ok(json!({
        "id": reminder.id,
        "text": reminder.text,
        "due": reminder.due,
        "recurring": reminder.recurring,
        "status": "created"
    }))
}

pub async fn list_reminders(args: Value) -> Result<Value> {
    let filter = args
        .get("filter")
        .and_then(|v| v.as_str())
        .unwrap_or("pending");
    let store = load_reminders();
    let now = Utc::now();

    let filtered: Vec<&Reminder> = store
        .reminders
        .iter()
        .filter(|r| match filter {
            "pending" => r.status == "pending",
            "completed" => r.status == "completed",
            "overdue" => {
                r.status == "pending"
                    && DateTime::parse_from_rfc3339(&r.due)
                        .map(|d| d < now)
                        .unwrap_or(false)
            }
            "all" => true,
            _ => r.status == "pending",
        })
        .collect();

    let json_reminders: Vec<Value> = filtered.iter().map(|r| json!({
        "id": r.id, "text": r.text, "due": r.due, "status": r.status, "recurring": r.recurring
    })).collect();

    Ok(json!({ "reminders": json_reminders, "count": json_reminders.len() }))
}

pub async fn complete_reminder(args: Value) -> Result<Value> {
    let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let mut store = load_reminders();

    let mut completed_text = String::new();
    let mut next_due = None;

    for r in &mut store.reminders {
        if r.id == id {
            r.status = "completed".to_string();
            r.completed = Some(Utc::now().to_rfc3339());
            completed_text = r.text.clone();

            // Handle recurring
            if let Some(ref schedule) = r.recurring {
                let current_due =
                    DateTime::parse_from_rfc3339(&r.due).unwrap_or_else(|_| Utc::now().into());
                let next = match schedule.as_str() {
                    "daily" => current_due + Duration::days(1),
                    "weekly" => current_due + Duration::weeks(1),
                    "monthly" => current_due + Duration::days(30),
                    _ => current_due + Duration::days(1),
                };
                next_due = Some((r.text.clone(), next.to_rfc3339(), schedule.clone()));
            }
            break;
        }
    }

    // Create next occurrence if recurring
    if let Some((text, due, schedule)) = next_due {
        store.reminders.push(Reminder {
            id: Uuid::new_v4().to_string()[..8].to_string(),
            text,
            due,
            created: Utc::now().to_rfc3339(),
            completed: None,
            recurring: Some(schedule),
            status: "pending".to_string(),
        });
    }

    save_reminders(&store)?;
    Ok(json!({ "status": "completed", "id": id, "text": completed_text }))
}

pub async fn delete_reminder(args: Value) -> Result<Value> {
    let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let mut store = load_reminders();
    store.reminders.retain(|r| r.id != id);
    save_reminders(&store)?;
    Ok(json!({ "status": "deleted", "id": id }))
}

pub async fn check_due(_args: Value) -> Result<Value> {
    let store = load_reminders();
    let now = Utc::now();

    let due: Vec<Value> = store
        .reminders
        .iter()
        .filter(|r| r.status == "pending")
        .filter(|r| {
            DateTime::parse_from_rfc3339(&r.due)
                .map(|d| d <= now)
                .unwrap_or(false)
        })
        .map(|r| json!({ "id": r.id, "text": r.text, "due": r.due }))
        .collect();

    Ok(json!({ "due": due, "count": due.len() }))
}

pub async fn add_recurring(args: Value) -> Result<Value> {
    let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
    let schedule = args
        .get("schedule")
        .and_then(|v| v.as_str())
        .unwrap_or("daily");
    let time = args
        .get("time")
        .and_then(|v| v.as_str())
        .unwrap_or("9:00 AM");

    add_reminder(json!({
        "text": text,
        "due": format!("tomorrow at {}", time),
        "recurring": schedule
    }))
    .await
}

pub async fn add_scheduled(args: Value) -> Result<Value> {
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("Ops_Reminder");
    let time = args.get("time").and_then(|v| v.as_str()).unwrap_or("09:00");
    let message = args.get("message").and_then(|v| v.as_str()).unwrap_or("");

    let task_name = format!("Ops_{}", name.replace(' ', "_"));
    let cmd = format!(
        "schtasks /Create /TN \"{}\" /TR \"msg * /TIME:10 {}\" /SC ONCE /ST {} /F",
        task_name,
        message.replace('"', "'"),
        time
    );

    let output = std::process::Command::new("cmd")
        .args(["/C", &cmd])
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    Ok(json!({ "task_name": task_name, "time": time, "output": stdout.trim() }))
}

pub async fn list_scheduled(_args: Value) -> Result<Value> {
    let output = std::process::Command::new("cmd")
        .args(["/C", "schtasks /Query /FO LIST /V /TN \\Ops_*"])
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(json!({ "tasks": stdout.trim() }))
}

pub async fn delete_scheduled(args: Value) -> Result<Value> {
    let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let cmd = format!("schtasks /Delete /TN \"{}\" /F", name);
    let output = std::process::Command::new("cmd")
        .args(["/C", &cmd])
        .output()?;
    Ok(
        json!({ "status": "deleted", "name": name, "output": String::from_utf8_lossy(&output.stdout).trim() }),
    )
}

pub async fn time_check(_args: Value) -> Result<Value> {
    let due = check_due(json!({})).await?;
    let count = due.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
    Ok(json!({
        "current_time": Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        "overdue_count": count,
        "reminders": due.get("due")
    }))
}

// === Dead Drop ===

fn load_dead_drop() -> Vec<DeadDropMessage> {
    let path = dead_drop_path();
    if path.exists() {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    } else {
        Vec::new()
    }
}

fn save_dead_drop(msgs: &[DeadDropMessage]) -> Result<()> {
    let path = dead_drop_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(msgs)?)?;
    Ok(())
}

pub async fn dead_drop_leave(args: Value) -> Result<Value> {
    let message = args.get("message").and_then(|v| v.as_str()).unwrap_or("");
    let from = args
        .get("from")
        .and_then(|v| v.as_str())
        .unwrap_or("autonomous");
    let priority = args
        .get("priority")
        .and_then(|v| v.as_str())
        .unwrap_or("normal");

    let msg = DeadDropMessage {
        id: Uuid::new_v4().to_string()[..8].to_string(),
        date: Local::now().format("%Y-%m-%d").to_string(),
        message: message.to_string(),
        from: from.to_string(),
        priority: priority.to_string(),
        created: Utc::now().to_rfc3339(),
        read: false,
    };

    let mut msgs = load_dead_drop();
    msgs.push(msg.clone());
    save_dead_drop(&msgs)?;

    Ok(json!({ "status": "left", "id": msg.id }))
}

pub async fn dead_drop_check(_args: Value) -> Result<Value> {
    let msgs = load_dead_drop();
    let unread: Vec<Value> = msgs.iter()
        .filter(|m| !m.read)
        .map(|m| json!({ "id": m.id, "message": m.message, "from": m.from, "priority": m.priority, "date": m.date }))
        .collect();
    Ok(json!({ "messages": unread, "count": unread.len() }))
}

pub async fn dead_drop_clear(args: Value) -> Result<Value> {
    let specific_id = args.get("id").and_then(|v| v.as_str());
    let mut msgs = load_dead_drop();

    match specific_id {
        Some(id) => {
            for m in &mut msgs {
                if m.id == id {
                    m.read = true;
                }
            }
        }
        None => {
            for m in &mut msgs {
                m.read = true;
            }
        }
    }

    save_dead_drop(&msgs)?;
    Ok(json!({ "status": "cleared" }))
}
