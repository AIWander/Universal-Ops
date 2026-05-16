//! Transform utilities — ported from local crate transforms.rs

use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::path::Path;

pub fn get_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "transform_grep",
            "description": "Search files for pattern, return matching lines with context.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File or directory" },
                    "pattern": { "type": "string", "description": "Search pattern (regex)" },
                    "context": { "type": "integer", "description": "Lines of context (default 0)" },
                    "recursive": { "type": "boolean", "description": "Search subdirs (default false)" }
                },
                "required": ["path", "pattern"]
            }
        }),
        json!({
            "name": "transform_extract_lines",
            "description": "Extract specific line range from file.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path" },
                    "start": { "type": "integer", "description": "Start line (1-indexed)" },
                    "end": { "type": "integer", "description": "End line (inclusive, -1 for EOF)" }
                },
                "required": ["path", "start"]
            }
        }),
        json!({
            "name": "transform_diff_file",
            "description": "Compare two files, return diff. Saves loading both files into chat.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file_a": { "type": "string", "description": "First file path" },
                    "file_b": { "type": "string", "description": "Second file path" }
                },
                "required": ["file_a", "file_b"]
            }
        }),
        json!({
            "name": "transform_find_replace",
            "description": "Find/replace in file. Saves reading entire file into chat.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path" },
                    "find": { "type": "string", "description": "Text or regex to find" },
                    "replace": { "type": "string", "description": "Replacement text" },
                    "regex": { "type": "boolean", "description": "Use regex (default false)" }
                },
                "required": ["path", "find", "replace"]
            }
        }),
        json!({
            "name": "transform_json_format",
            "description": "Pretty-print JSON with proper indentation.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "json_string": { "type": "string", "description": "JSON to format" }
                },
                "required": ["json_string"]
            }
        }),
        json!({
            "name": "transform_hash_file",
            "description": "Compute file checksum (SHA256 via PowerShell).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path" },
                    "algorithm": { "type": "string", "description": "md5 or sha256 (default sha256)" }
                },
                "required": ["path"]
            }
        }),
        json!({
            "name": "transform_file_stats",
            "description": "Get file/directory stats without reading content.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to analyze" },
                    "recursive": { "type": "boolean", "description": "Include subdirs (default false)" }
                },
                "required": ["path"]
            }
        }),
    ]
}

pub fn execute(name: &str, args: &Value) -> Value {
    match name {
        "transform_grep" => grep(args),
        "transform_extract_lines" => extract_lines(args),
        "transform_diff_file" | "transform_diff_files" => diff_files(args),
        "transform_find_replace" => find_replace(args),
        "transform_json_format" => json_format(args),
        "transform_hash_file" => hash_file(args),
        "transform_file_stats" => file_stats(args),
        _ => json!({"error": format!("Unknown transform: {}", name)}),
    }
}

fn grep(args: &Value) -> Value {
    let path = match args["path"].as_str() {
        Some(s) => s,
        None => return json!({"error": "path required"}),
    };
    let pattern = match args["pattern"].as_str() {
        Some(s) => s,
        None => return json!({"error": "pattern required"}),
    };
    let context = args["context"].as_u64().unwrap_or(0) as usize;

    let re = match regex::Regex::new(pattern) {
        Ok(r) => r,
        Err(e) => return json!({"error": format!("Invalid regex: {}", e)}),
    };

    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => return json!({"error": format!("Can't read: {}", e)}),
    };

    let lines: Vec<&str> = content.lines().collect();
    let mut matches: Vec<Value> = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        if re.is_match(line) {
            let start = i.saturating_sub(context);
            let end = (i + context + 1).min(lines.len());
            let context_lines: Vec<String> = lines[start..end]
                .iter()
                .enumerate()
                .map(|(j, l)| format!("{}: {}", start + j + 1, l))
                .collect();
            matches.push(json!({
                "line": i + 1,
                "match": line,
                "context": context_lines
            }));
        }
    }

    json!({"path": path, "pattern": pattern, "matches": matches, "count": matches.len()})
}

fn extract_lines(args: &Value) -> Value {
    let path = match args["path"].as_str() {
        Some(s) => s,
        None => return json!({"error": "path required"}),
    };
    let start = args["start"].as_i64().unwrap_or(1) as usize;
    let end = args["end"].as_i64().unwrap_or(-1);

    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return json!({"error": format!("Can't open: {}", e)}),
    };

    let reader = BufReader::new(file);
    let lines: Vec<String> = reader
        .lines()
        .enumerate()
        .filter_map(|(i, line)| {
            let line_num = i + 1;
            let in_range = line_num >= start && (end < 0 || line_num <= end as usize);
            if in_range {
                line.ok()
            } else {
                None
            }
        })
        .collect();

    json!({"path": path, "start": start, "end": if end < 0 { "EOF".to_string() } else { end.to_string() }, "lines": lines, "count": lines.len()})
}

fn diff_files(args: &Value) -> Value {
    let file_a = match args["file_a"].as_str() {
        Some(s) => s,
        None => return json!({"error": "file_a required"}),
    };
    let file_b = match args["file_b"].as_str() {
        Some(s) => s,
        None => return json!({"error": "file_b required"}),
    };

    let content_a = match std::fs::read_to_string(file_a) {
        Ok(c) => c,
        Err(e) => return json!({"error": format!("Can't read {}: {}", file_a, e)}),
    };
    let content_b = match std::fs::read_to_string(file_b) {
        Ok(c) => c,
        Err(e) => return json!({"error": format!("Can't read {}: {}", file_b, e)}),
    };

    let lines_a: Vec<&str> = content_a.lines().collect();
    let lines_b: Vec<&str> = content_b.lines().collect();

    let mut diff_lines: Vec<String> = Vec::new();
    let max_len = lines_a.len().max(lines_b.len());
    let mut changes = 0;

    for i in 0..max_len {
        let a = lines_a.get(i);
        let b = lines_b.get(i);
        match (a, b) {
            (Some(la), Some(lb)) if la != lb => {
                diff_lines.push(format!("{}:- {}", i + 1, la));
                diff_lines.push(format!("{}:+ {}", i + 1, lb));
                changes += 1;
            }
            (Some(la), None) => {
                diff_lines.push(format!("{}:- {}", i + 1, la));
                changes += 1;
            }
            (None, Some(lb)) => {
                diff_lines.push(format!("{}:+ {}", i + 1, lb));
                changes += 1;
            }
            _ => {}
        }
    }

    json!({"diff": diff_lines.join("\n"), "changes": changes, "lines_a": lines_a.len(), "lines_b": lines_b.len()})
}

fn find_replace(args: &Value) -> Value {
    let path = match args["path"].as_str() {
        Some(s) => s,
        None => return json!({"error": "path required"}),
    };
    let find = match args["find"].as_str() {
        Some(s) => s,
        None => return json!({"error": "find required"}),
    };
    let replace = match args["replace"].as_str() {
        Some(s) => s,
        None => return json!({"error": "replace required"}),
    };
    let use_regex = args["regex"].as_bool().unwrap_or(false);

    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => return json!({"error": format!("Can't read: {}", e)}),
    };

    let (new_content, count) = if use_regex {
        match regex::Regex::new(find) {
            Ok(re) => {
                let matches = re.find_iter(&content).count();
                (re.replace_all(&content, replace).to_string(), matches)
            }
            Err(e) => return json!({"error": format!("Invalid regex: {}", e)}),
        }
    } else {
        let count = content.matches(find).count();
        (content.replace(find, replace), count)
    };

    if count > 0 {
        if let Err(e) = std::fs::write(path, &new_content) {
            return json!({"error": format!("Can't write: {}", e)});
        }
    }

    json!({"path": path, "replacements": count})
}

fn json_format(args: &Value) -> Value {
    let json_string = match args["json_string"].as_str() {
        Some(s) => s,
        None => return json!({"error": "json_string required"}),
    };
    match serde_json::from_str::<Value>(json_string) {
        Ok(parsed) => json!({"formatted": serde_json::to_string_pretty(&parsed).unwrap()}),
        Err(e) => json!({"error": format!("Invalid JSON: {}", e)}),
    }
}

fn hash_file(args: &Value) -> Value {
    let path = match args["path"].as_str() {
        Some(s) => s,
        None => return json!({"error": "path required"}),
    };
    let algorithm = args["algorithm"].as_str().unwrap_or("sha256");
    let algo_upper = algorithm.to_uppercase();

    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);

    let result = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "(Get-FileHash -Path '{}' -Algorithm {}).Hash",
                path, algo_upper
            ),
        ])
        .output();

    match result {
        Ok(output) => {
            let hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
            json!({"path": path, "algorithm": algorithm, "hash": hash, "size": size})
        }
        Err(e) => json!({"error": format!("Hash failed: {}", e)}),
    }
}

fn file_stats(args: &Value) -> Value {
    let path = match args["path"].as_str() {
        Some(s) => s,
        None => return json!({"error": "path required"}),
    };
    let recursive = args["recursive"].as_bool().unwrap_or(false);

    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => return json!({"error": format!("Can't stat: {}", e)}),
    };

    if meta.is_file() {
        json!({"type": "file", "path": path, "size": meta.len(), "size_human": format_size(meta.len())})
    } else {
        let mut total_size: u64 = 0;
        let mut file_count: u64 = 0;
        let mut dir_count: u64 = 0;
        walk_dir(
            Path::new(path),
            recursive,
            &mut total_size,
            &mut file_count,
            &mut dir_count,
        );
        json!({
            "type": "directory", "path": path,
            "files": file_count, "directories": dir_count,
            "total_size": total_size, "total_size_human": format_size(total_size),
            "recursive": recursive
        })
    }
}

fn walk_dir(p: &Path, recursive: bool, total: &mut u64, files: &mut u64, dirs: &mut u64) {
    if let Ok(entries) = std::fs::read_dir(p) {
        for entry in entries.flatten() {
            if let Ok(m) = entry.metadata() {
                if m.is_file() {
                    *total += m.len();
                    *files += 1;
                } else if m.is_dir() {
                    *dirs += 1;
                    if recursive {
                        walk_dir(&entry.path(), recursive, total, files, dirs);
                    }
                }
            }
        }
    }
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}
