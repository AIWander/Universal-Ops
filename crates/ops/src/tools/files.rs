//! File I/O tools — ported from local crate raw.rs

use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

pub fn get_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "read_file",
            "description": "Read file with smart options: search for pattern, get specific lines, or auto-truncate large files.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Full file path" },
                    "search": { "type": "string", "description": "Optional: grep for pattern, return matching lines" },
                    "lines": { "type": "string", "description": "Optional: line range like '50:100'" },
                    "max_kb": { "type": "integer", "description": "Max KB to return (default 100KB)" }
                },
                "required": ["path"]
            }
        }),
        json!({
            "name": "write_file",
            "description": "Write file, return confirmation only",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Full file path" },
                    "content": { "type": "string", "description": "Content to write" }
                },
                "required": ["path", "content"]
            }
        }),
        json!({
            "name": "append_file",
            "description": "Append to file, return confirmation only",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Full file path" },
                    "content": { "type": "string", "description": "Content to append" }
                },
                "required": ["path", "content"]
            }
        }),
        json!({
            "name": "list_dir",
            "description": "List directory contents as tree",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Directory path" },
                    "depth": { "type": "integer", "description": "Depth to traverse (default 2)" }
                },
                "required": ["path"]
            }
        }),
        json!({
            "name": "tail_file",
            "description": "Return last N lines of a file plus current byte offset. Pass since_bytes from a previous call to get only NEW content (delta polling).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path to tail" },
                    "lines": { "type": "integer", "description": "Number of lines to return (default 50)" },
                    "since_bytes": { "type": "integer", "description": "Byte offset from previous call. 0 = read from end (default)." }
                },
                "required": ["path"]
            }
        }),
    ]
}

pub fn execute(name: &str, args: &Value) -> Value {
    match name {
        "read_file" => read_file(args),
        "write_file" => write_file(args),
        "append_file" => append_file(args),
        "list_dir" => list_dir(args),
        "tail_file" => tail_file(args),
        _ => json!({"error": format!("Unknown file tool: {}", name)}),
    }
}

fn read_file(args: &Value) -> Value {
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let search = args.get("search").and_then(|v| v.as_str());
    let lines_param = args.get("lines").and_then(|v| v.as_str());
    let max_kb = args.get("max_kb").and_then(|v| v.as_i64()).unwrap_or(100);

    let file_path = Path::new(path);
    if !file_path.exists() {
        return json!(format!("[ERROR] File not found: {}", path));
    }

    let file_size = match fs::metadata(path) {
        Ok(m) => m.len(),
        Err(e) => return json!(format!("[ERROR] {}", e)),
    };
    let file_kb = file_size / 1024;

    if let Some(pattern) = search {
        return read_search(path, pattern);
    }
    if let Some(range) = lines_param {
        return read_lines(path, range);
    }

    match fs::read_to_string(path) {
        Ok(content) => {
            if file_kb > max_kb as u64 {
                let chars_limit = (max_kb * 1024) as usize;
                let truncated: String = content.chars().take(chars_limit).collect();
                let total_lines = content.lines().count();
                let shown_lines = truncated.lines().count();
                json!(format!(
                    "{}\n\n[TRUNCATED: {}KB file, showed {}/{} lines. Use search='pattern' or lines='start:end' for specific content]",
                    truncated, file_kb, shown_lines, total_lines
                ))
            } else {
                json!(content)
            }
        }
        Err(e) => json!(format!("[ERROR] {}", e)),
    }
}

fn read_search(path: &str, pattern: &str) -> Value {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return json!(format!("[ERROR] {}", e)),
    };
    let reader = BufReader::new(file);
    let pattern_lower = pattern.to_lowercase();
    let mut matches: Vec<String> = Vec::new();
    let mut total_lines = 0;

    for (i, line) in reader.lines().enumerate() {
        total_lines = i + 1;
        if let Ok(text) = line {
            if text.to_lowercase().contains(&pattern_lower) {
                matches.push(format!("{}:{}", i + 1, text));
            }
        }
        if matches.len() >= 100 {
            matches.push("[...truncated at 100 matches]".to_string());
            break;
        }
    }

    if matches.is_empty() {
        json!(format!(
            "[NO MATCHES] '{}' not found in {} lines",
            pattern, total_lines
        ))
    } else {
        json!(format!(
            "[{} matches in {} lines]\n{}",
            matches.len(),
            total_lines,
            matches.join("\n")
        ))
    }
}

fn read_lines(path: &str, range: &str) -> Value {
    let parts: Vec<&str> = range.split(':').collect();
    if parts.len() != 2 {
        return json!("[ERROR] lines format: 'start:end' e.g. '50:100'");
    }
    let start: usize = parts[0].parse().unwrap_or(1);
    let end: usize = parts[1].parse().unwrap_or(50);
    if start < 1 || end < start {
        return json!("[ERROR] Invalid line range");
    }

    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return json!(format!("[ERROR] {}", e)),
    };
    let reader = BufReader::new(file);
    let mut result: Vec<String> = Vec::new();
    let mut total_lines = 0;

    for (i, line) in reader.lines().enumerate() {
        let line_num = i + 1;
        total_lines = line_num;
        if line_num >= start && line_num <= end {
            if let Ok(text) = line {
                result.push(format!("{}:{}", line_num, text));
            }
        }
        if line_num > end {
            break;
        }
    }

    json!(format!(
        "[Lines {}-{} of {}]\n{}",
        start,
        end.min(total_lines),
        total_lines,
        result.join("\n")
    ))
}

fn write_file(args: &Value) -> Value {
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
    if let Some(parent) = Path::new(path).parent() {
        let _ = fs::create_dir_all(parent);
    }
    match fs::write(path, content) {
        Ok(_) => json!(format!("written: {}", path)),
        Err(e) => json!(format!("[ERROR] {}", e)),
    }
}

fn append_file(args: &Value) -> Value {
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
    use std::io::Write;
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        Ok(mut file) => match file.write_all(content.as_bytes()) {
            Ok(_) => json!(format!("appended: {}", path)),
            Err(e) => json!(format!("[ERROR] {}", e)),
        },
        Err(e) => json!(format!("[ERROR] {}", e)),
    }
}

fn list_dir(args: &Value) -> Value {
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let depth = args.get("depth").and_then(|v| v.as_i64()).unwrap_or(2) as usize;
    let mut output = Vec::new();
    list_recursive(Path::new(path), depth, 0, &mut output);
    json!(output.join("\n"))
}

fn list_recursive(base: &Path, max_depth: usize, current_depth: usize, output: &mut Vec<String>) {
    if current_depth > max_depth {
        return;
    }
    let entries = match fs::read_dir(base) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut items: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    items.sort_by_key(|e| e.file_name());
    for entry in items.iter().take(100) {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let prefix = "  ".repeat(current_depth);
        if path.is_dir() {
            output.push(format!("{}{}/", prefix, name));
            if current_depth < max_depth {
                list_recursive(&path, max_depth, current_depth + 1, output);
            }
        } else {
            output.push(format!("{}{}", prefix, name));
        }
    }
}

fn tail_file(args: &Value) -> Value {
    let path = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return json!({"error": "Missing 'path' parameter"}),
    };
    let max_lines = args.get("lines").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
    let since_bytes = args
        .get("since_bytes")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let mut file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return json!({"error": format!("Cannot open file: {}", e)}),
    };
    let total_bytes = match file.metadata() {
        Ok(m) => m.len(),
        Err(e) => return json!({"error": format!("Cannot read metadata: {}", e)}),
    };

    if since_bytes > 0 {
        if since_bytes >= total_bytes {
            return json!({ "lines": [], "byte_offset": total_bytes, "total_bytes": total_bytes, "new_content": false });
        }
        if let Err(e) = file.seek(SeekFrom::Start(since_bytes)) {
            return json!({"error": format!("Seek failed: {}", e)});
        }
        let mut new_data = String::new();
        if let Err(e) = file.read_to_string(&mut new_data) {
            return json!({"error": format!("Read failed: {}", e)});
        }
        let lines: Vec<&str> = new_data.lines().collect();
        let tail: Vec<&str> = if lines.len() > max_lines {
            lines[lines.len() - max_lines..].to_vec()
        } else {
            lines
        };
        return json!({ "lines": tail, "byte_offset": total_bytes, "total_bytes": total_bytes, "new_content": true });
    }

    let read_size: u64 = (64 * 1024).min(total_bytes);
    let start_pos = total_bytes.saturating_sub(read_size);
    if let Err(e) = file.seek(SeekFrom::Start(start_pos)) {
        return json!({"error": format!("Seek failed: {}", e)});
    }
    let mut buf = String::new();
    if let Err(e) = file.read_to_string(&mut buf) {
        return json!({"error": format!("Read failed: {}", e)});
    }
    let lines: Vec<&str> = buf.lines().collect();
    let tail: Vec<&str> = if lines.len() > max_lines {
        lines[lines.len() - max_lines..].to_vec()
    } else {
        lines
    };
    json!({ "lines": tail, "byte_offset": total_bytes, "total_bytes": total_bytes, "new_content": true })
}
