//! Nightly analyzer for manager task metrics.
//! Reads task_history.json, computes per-backend metrics, detects inflection points,
//! writes proposals to Volumes/inbox/ for human review.
//!
//! Scheduled via Windows Task Scheduler at 03:45 daily.
//! NEVER auto-modifies routing logic — proposals only.

use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use std::collections::HashMap;

// ============================================================================
// Metrics structs
// ============================================================================

#[derive(Debug, Default, Clone)]
pub struct BackendMetrics {
    pub total: u32,
    pub successful: u32,
    pub failed: u32,
    pub success_rate: f64,
    pub durations_ms: Vec<u64>,
    pub p50_duration_ms: u64,
    pub p95_duration_ms: u64,
    pub total_cost: f64,
    pub avg_cost: f64,
    pub retry_count: u32,
    pub retry_rate: f64,
    pub failure_categories: HashMap<String, u32>,
}

impl BackendMetrics {
    fn finalize(&mut self) {
        if self.total > 0 {
            self.success_rate = self.successful as f64 / self.total as f64;
            self.avg_cost = self.total_cost / self.total as f64;
            self.retry_rate = self.retry_count as f64 / self.total as f64;
        }
        self.durations_ms.sort_unstable();
        let n = self.durations_ms.len();
        if n > 0 {
            self.p50_duration_ms = self.durations_ms[n / 2];
            self.p95_duration_ms = self.durations_ms[(n as f64 * 0.95) as usize]
                .min(*self.durations_ms.last().unwrap());
        }
    }
}

// ============================================================================
// Core analyzer
// ============================================================================

/// Run the nightly analyzer. Returns JSON summary + writes proposals file.
pub fn run_analyzer(volumes_path: &str, history_path: &str) -> Result<Value, String> {
    let history = load_history(history_path)?;
    let now = Utc::now();
    let seven_days_ago = now - Duration::days(7);
    let fourteen_days_ago = now - Duration::days(14);

    let recent: Vec<&Value> = history
        .iter()
        .filter(|e| {
            parse_created_at(e)
                .map(|dt| dt >= seven_days_ago)
                .unwrap_or(false)
        })
        .collect();

    let prev_week: Vec<&Value> = history
        .iter()
        .filter(|e| {
            parse_created_at(e)
                .map(|dt| dt >= fourteen_days_ago && dt < seven_days_ago)
                .unwrap_or(false)
        })
        .collect();

    let current_metrics = compute_metrics(&recent);
    let prev_metrics = compute_metrics(&prev_week);
    let mut proposals: Vec<Value> = Vec::new();

    // --- Inflection detection: success rate dropped ≥15% WoW ---
    for (backend, metrics) in &current_metrics {
        if let Some(prev) = prev_metrics.get(backend) {
            if prev.total >= 3
                && metrics.total >= 3
                && prev.success_rate > 0.0
                && metrics.success_rate < prev.success_rate - 0.15
            {
                let drop = prev.success_rate - metrics.success_rate;
                let top_failures: Vec<String> = top_n_keys(&metrics.failure_categories, 3);
                proposals.push(json!({
                    "type": "inflection_point",
                    "severity": "warning",
                    "backend": backend,
                    "current_success_rate": format!("{:.1}%", metrics.success_rate * 100.0),
                    "previous_success_rate": format!("{:.1}%", prev.success_rate * 100.0),
                    "drop_pct": format!("{:.1}%", drop * 100.0),
                    "top_failure_categories": top_failures,
                    "recommendation": format!(
                        "{} success rate dropped {:.1}% week-over-week. Top failures: {}. Investigate.",
                        backend, drop * 100.0, top_failures.join(", ")
                    ),
                }));
            }
        }
    }

    // --- Promotion detection: backend Y outperforms default for task class Z ---
    let task_type_metrics = compute_by_task_type(&recent);
    for (task_type, backend_map) in &task_type_metrics {
        if backend_map.len() < 2 {
            continue;
        }
        let mut sorted: Vec<(&String, &BackendMetrics)> = backend_map.iter().collect();
        sorted.sort_by(|a, b| {
            b.1.success_rate
                .partial_cmp(&a.1.success_rate)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        if let (Some(best), Some(runner)) = (sorted.first(), sorted.get(1)) {
            if best.1.total >= 3 && best.1.success_rate >= runner.1.success_rate + 0.20 {
                proposals.push(json!({
                    "type": "promotion",
                    "severity": "info",
                    "task_type": task_type,
                    "recommended_backend": best.0,
                    "success_rate": format!("{:.1}%", best.1.success_rate * 100.0),
                    "runner_up": runner.0,
                    "runner_up_rate": format!("{:.1}%", runner.1.success_rate * 100.0),
                    "sample_size": best.1.total,
                    "recommendation": format!(
                        "{} outperforms {} for '{}' tasks ({:.0}% vs {:.0}%, n={})",
                        best.0, runner.0, task_type,
                        best.1.success_rate * 100.0, runner.1.success_rate * 100.0,
                        best.1.total
                    ),
                }));
            }
        }
    }

    // --- Cost anomaly: backend cost spiked ≥50% WoW ---
    for (backend, metrics) in &current_metrics {
        if let Some(prev) = prev_metrics.get(backend) {
            if prev.total >= 3
                && metrics.total >= 3
                && prev.avg_cost > 0.001
                && metrics.avg_cost > prev.avg_cost * 1.5
            {
                proposals.push(json!({
                    "type": "cost_anomaly",
                    "severity": "info",
                    "backend": backend,
                    "current_avg_cost": format!("${:.4}", metrics.avg_cost),
                    "previous_avg_cost": format!("${:.4}", prev.avg_cost),
                    "recommendation": format!(
                        "{} avg cost increased {:.0}% WoW (${:.4} → ${:.4})",
                        backend, ((metrics.avg_cost / prev.avg_cost) - 1.0) * 100.0,
                        prev.avg_cost, metrics.avg_cost
                    ),
                }));
            }
        }
    }

    // --- Write proposals ---
    let date_str = now.format("%Y-%m-%d").to_string();
    let proposals_path = format!(
        "{}\\inbox\\manager_analyzer_proposals_{}.md",
        volumes_path, date_str
    );

    let report = build_report(&current_metrics, &proposals, &date_str, recent.len());

    if let Some(parent) = std::path::Path::new(&proposals_path).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&proposals_path, &report)
        .map_err(|e| format!("Failed to write proposals: {}", e))?;

    Ok(json!({
        "status": "completed",
        "proposals_count": proposals.len(),
        "proposals_path": proposals_path,
        "tasks_analyzed": recent.len(),
        "backends_analyzed": current_metrics.len(),
        "metrics": metrics_to_json(&current_metrics),
    }))
}

// ============================================================================
// Helpers
// ============================================================================

fn load_history(path: &str) -> Result<Vec<Value>, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("Failed to read history: {}", e))?;
    serde_json::from_str(&content).map_err(|e| format!("Failed to parse history: {}", e))
}

fn parse_created_at(entry: &Value) -> Option<DateTime<Utc>> {
    entry
        .get("created_at")
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

fn compute_metrics(tasks: &[&Value]) -> HashMap<String, BackendMetrics> {
    let mut map: HashMap<String, BackendMetrics> = HashMap::new();

    for task in tasks {
        let backend = task
            .get("backend")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let status = task.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let duration_ms = task
            .get("duration_secs")
            .and_then(|v| v.as_i64())
            .map(|s| (s * 1000) as u64);
        let cost = task.get("cost_usd").and_then(|v| v.as_f64()).unwrap_or(0.0);

        let m = map.entry(backend).or_default();
        m.total += 1;
        if status == "done" {
            m.successful += 1;
        }
        if status == "failed" {
            m.failed += 1;
            let error = task
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let category = categorize_error(error);
            *m.failure_categories.entry(category).or_insert(0) += 1;
        }
        if let Some(d) = duration_ms {
            m.durations_ms.push(d);
        }
        m.total_cost += cost;

        // Detect retries by checking step_count or retry_of field
        let step_count = task.get("step_count").and_then(|v| v.as_u64()).unwrap_or(0);
        if step_count > 5 {
            m.retry_count += 1;
        } // heuristic: many steps = likely retry
    }

    for m in map.values_mut() {
        m.finalize();
    }
    map
}

fn compute_by_task_type(tasks: &[&Value]) -> HashMap<String, HashMap<String, BackendMetrics>> {
    // Classify tasks by type from prompt keywords
    let mut type_map: HashMap<String, Vec<&Value>> = HashMap::new();
    for task in tasks {
        let task_type = classify_task_type(task);
        type_map.entry(task_type).or_default().push(task);
    }

    let mut result: HashMap<String, HashMap<String, BackendMetrics>> = HashMap::new();
    for (task_type, type_tasks) in &type_map {
        let refs: Vec<&Value> = type_tasks.to_vec();
        result.insert(task_type.clone(), compute_metrics(&refs));
    }
    result
}

fn classify_task_type(task: &Value) -> String {
    let prompt = task
        .get("prompt_summary")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    if prompt.contains("build") || prompt.contains("cargo") || prompt.contains("compile") {
        "build".to_string()
    } else if prompt.contains("fix") || prompt.contains("bug") || prompt.contains("error") {
        "fix".to_string()
    } else if prompt.contains("refactor") || prompt.contains("clean") {
        "refactor".to_string()
    } else if prompt.contains("test") || prompt.contains("verify") {
        "test".to_string()
    } else if prompt.contains("deploy") || prompt.contains("release") || prompt.contains("ship") {
        "deploy".to_string()
    } else if prompt.contains("research")
        || prompt.contains("investigate")
        || prompt.contains("explore")
    {
        "research".to_string()
    } else {
        "general".to_string()
    }
}

fn categorize_error(error: &str) -> String {
    let lower = error.to_lowercase();
    if lower.contains("timeout") || lower.contains("timed out") {
        "timeout".to_string()
    } else if lower.contains("rate limit") || lower.contains("429") {
        "rate_limit".to_string()
    } else if lower.contains("compile") || lower.contains("cargo") || lower.contains("build") {
        "build_failure".to_string()
    } else if lower.contains("auth") || lower.contains("api key") || lower.contains("401") {
        "auth_failure".to_string()
    } else if lower.contains("not found") || lower.contains("404") {
        "not_found".to_string()
    } else if lower.contains("stall") || lower.contains("watchdog") {
        "stall".to_string()
    } else {
        "other".to_string()
    }
}

fn top_n_keys(map: &HashMap<String, u32>, n: usize) -> Vec<String> {
    let mut sorted: Vec<(&String, &u32)> = map.iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(a.1));
    sorted.into_iter().take(n).map(|(k, _)| k.clone()).collect()
}

fn metrics_to_json(metrics: &HashMap<String, BackendMetrics>) -> Value {
    let entries: Vec<Value> = metrics
        .iter()
        .map(|(backend, m)| {
            json!({
                "backend": backend,
                "total_tasks": m.total,
                "success_rate": format!("{:.1}%", m.success_rate * 100.0),
                "p50_duration_secs": format!("{:.1}", m.p50_duration_ms as f64 / 1000.0),
                "p95_duration_secs": format!("{:.1}", m.p95_duration_ms as f64 / 1000.0),
                "avg_cost_usd": format!("${:.4}", m.avg_cost),
                "retry_rate": format!("{:.1}%", m.retry_rate * 100.0),
            })
        })
        .collect();
    json!(entries)
}

fn build_report(
    metrics: &HashMap<String, BackendMetrics>,
    proposals: &[Value],
    date: &str,
    task_count: usize,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Manager Analyzer Report — {}\n\n", date));
    out.push_str(&format!("Tasks analyzed (last 7 days): {}\n\n", task_count));

    out.push_str("## Per-Backend Metrics\n\n");
    out.push_str("| Backend | Tasks | Success | p50 | p95 | Avg Cost | Retry Rate |\n");
    out.push_str("|---------|-------|---------|-----|-----|----------|------------|\n");
    for (backend, m) in metrics {
        out.push_str(&format!(
            "| {} | {} | {:.1}% | {:.1}s | {:.1}s | ${:.4} | {:.1}% |\n",
            backend,
            m.total,
            m.success_rate * 100.0,
            m.p50_duration_ms as f64 / 1000.0,
            m.p95_duration_ms as f64 / 1000.0,
            m.avg_cost,
            m.retry_rate * 100.0
        ));
    }

    if proposals.is_empty() {
        out.push_str(
            "\n## Proposals\n\nNo proposals generated — all metrics within normal ranges.\n",
        );
    } else {
        out.push_str(&format!("\n## Proposals ({})\n\n", proposals.len()));
        for (i, p) in proposals.iter().enumerate() {
            let ptype = p.get("type").and_then(|v| v.as_str()).unwrap_or("unknown");
            let severity = p.get("severity").and_then(|v| v.as_str()).unwrap_or("info");
            let rec = p
                .get("recommendation")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let marker = match severity {
                "warning" => "⚠️",
                _ => "ℹ️",
            };
            out.push_str(&format!(
                "### {}. {} [{}] {}\n\n",
                i + 1,
                marker,
                ptype,
                severity
            ));
            out.push_str(&format!("{}\n\n", rec));
        }
    }

    out.push_str("---\n*Generated by manager-mcp nightly analyzer. Proposals only — no auto-modifications.*\n");
    out
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_task(
        backend: &str,
        status: &str,
        created_at: &str,
        duration_secs: i64,
        error: Option<&str>,
    ) -> Value {
        let mut v = json!({
            "task_id": format!("task_{}", uuid::Uuid::new_v4()),
            "backend": backend,
            "status": status,
            "prompt_summary": "test task",
            "step_count": 3,
            "created_at": created_at,
            "duration_secs": duration_secs,
            "cost_usd": 0.05,
        });
        if let Some(e) = error {
            v.as_object_mut()
                .unwrap()
                .insert("error".to_string(), json!(e));
        }
        v
    }

    #[test]
    fn test_inflection_detection() {
        let now = Utc::now();
        let recent_date = (now - Duration::days(1)).to_rfc3339();
        let prev_date = (now - Duration::days(10)).to_rfc3339();

        // Previous week: 10 tasks, 9 success (90%)
        let mut history: Vec<Value> = (0..9)
            .map(|_| make_task("claude_code", "done", &prev_date, 60, None))
            .collect();
        history.push(make_task(
            "claude_code",
            "failed",
            &prev_date,
            30,
            Some("timeout"),
        ));

        // Current week: 10 tasks, 5 success (50%) — 40% drop
        history.extend((0..5).map(|_| make_task("claude_code", "done", &recent_date, 60, None)));
        history.extend((0..5).map(|_| {
            make_task(
                "claude_code",
                "failed",
                &recent_date,
                30,
                Some("build failure"),
            )
        }));

        let tmp = std::env::temp_dir().join("test_analyzer_history.json");
        std::fs::write(&tmp, serde_json::to_string(&history).unwrap()).unwrap();

        let volumes_tmp = std::env::temp_dir().join("test_analyzer_volumes");
        std::fs::create_dir_all(volumes_tmp.join("inbox")).ok();

        let result = run_analyzer(volumes_tmp.to_str().unwrap(), tmp.to_str().unwrap()).unwrap();

        assert!(
            result
                .get("proposals_count")
                .and_then(|v| v.as_u64())
                .unwrap()
                >= 1
        );
        let proposals_path = result
            .get("proposals_path")
            .and_then(|v| v.as_str())
            .unwrap();
        let report = std::fs::read_to_string(proposals_path).unwrap();
        assert!(report.contains("inflection_point"));

        // Cleanup
        std::fs::remove_file(&tmp).ok();
        std::fs::remove_dir_all(&volumes_tmp).ok();
    }

    #[test]
    fn test_compute_metrics_empty() {
        let tasks: Vec<&Value> = vec![];
        let metrics = compute_metrics(&tasks);
        assert!(metrics.is_empty());
    }

    #[test]
    fn test_classify_task_type() {
        assert_eq!(
            classify_task_type(&json!({"prompt_summary": "cargo build"})),
            "build"
        );
        assert_eq!(
            classify_task_type(&json!({"prompt_summary": "fix the bug"})),
            "fix"
        );
        assert_eq!(
            classify_task_type(&json!({"prompt_summary": "hello world"})),
            "general"
        );
    }

    #[test]
    fn test_categorize_error() {
        assert_eq!(categorize_error("request timed out after 30s"), "timeout");
        assert_eq!(categorize_error("cargo build failed"), "build_failure");
        assert_eq!(categorize_error("401 unauthorized"), "auth_failure");
        assert_eq!(categorize_error("something weird"), "other");
    }
}
