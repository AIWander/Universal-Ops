//! Utility tools — clipboard, notify, process, port check
//! Ported from local crate raw.rs

use serde_json::{json, Value};
use std::process::Command;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

pub fn get_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "clipboard_read",
            "description": "Read from Windows clipboard",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "clipboard_write",
            "description": "Write to Windows clipboard",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content": { "type": "string", "description": "Content to copy" }
                },
                "required": ["content"]
            }
        }),
        json!({
            "name": "notify",
            "description": "Show a Windows toast notification.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "Notification title" },
                    "body": { "type": "string", "description": "Notification body" },
                    "icon": { "type": "string", "enum": ["info", "warning", "error"], "default": "info" },
                    "duration_ms": { "type": "integer", "description": "Duration ms (default 5000)", "default": 5000 }
                },
                "required": ["title", "body"]
            }
        }),
        json!({
            "name": "kill_process",
            "description": "Kill process by PID",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pid": { "type": "integer", "description": "Process ID to kill" }
                },
                "required": ["pid"]
            }
        }),
        json!({
            "name": "list_process",
            "description": "List processes, optionally filtered by name",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "filter": { "type": "string", "description": "Filter by process name (optional)" }
                }
            }
        }),
        json!({
            "name": "port_check",
            "description": "Test TCP connectivity to a host:port. Returns whether the port is open.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "host": { "type": "string", "description": "Host (default: 127.0.0.1)" },
                    "port": { "type": "integer", "description": "Port number" },
                    "timeout_ms": { "type": "integer", "description": "Timeout ms (default 2000)" }
                },
                "required": ["port"]
            }
        }),
    ]
}

pub fn execute(name: &str, args: &Value) -> Value {
    match name {
        "clipboard_read" => clipboard_read(args),
        "clipboard_write" => clipboard_write(args),
        "notify" => notify(args),
        "kill_process" => kill_process(args),
        "list_process" => list_process(args),
        "port_check" => port_check(args),
        _ => json!({"error": format!("Unknown util tool: {}", name)}),
    }
}

fn clipboard_read(_args: &Value) -> Value {
    match arboard::Clipboard::new().and_then(|mut cb| cb.get_text()) {
        Ok(text) => json!(text),
        Err(e) => json!(format!("[ERROR] {}", e)),
    }
}

fn clipboard_write(args: &Value) -> Value {
    let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
    match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(content.to_string())) {
        Ok(()) => json!(format!("copied: {} chars", content.len())),
        Err(e) => json!(format!("[ERROR] {}", e)),
    }
}

fn notify(args: &Value) -> Value {
    let title = args
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let body = args
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let icon = args.get("icon").and_then(|v| v.as_str()).unwrap_or("info");
    let duration_ms = args
        .get("duration_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(5000)
        .max(1);

    if title.is_empty() || body.is_empty() {
        return json!({"error": "Both title and body are required"});
    }

    let display_title = match icon {
        "warning" => format!("[Warning] {title}"),
        "error" => format!("[Error] {title}"),
        _ => format!("[Info] {title}"),
    };

    let script = r#"
$ErrorActionPreference = 'Stop'
if (Get-Command New-BurntToastNotification -ErrorAction SilentlyContinue) {
    New-BurntToastNotification -Text $env:MCP_NOTIFY_TITLE, $env:MCP_NOTIFY_BODY -Silent | Out-Null
    Write-Output 'burnttoast'
    return
}
Add-Type -AssemblyName System.Runtime.WindowsRuntime | Out-Null
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] > $null
[Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] > $null
$titleEscaped = [System.Security.SecurityElement]::Escape($env:MCP_NOTIFY_TITLE)
$bodyEscaped = [System.Security.SecurityElement]::Escape($env:MCP_NOTIFY_BODY)
$xml = "<toast><visual><binding template='ToastGeneric'><text>$titleEscaped</text><text>$bodyEscaped</text></binding></visual><audio silent='true'/></toast>"
$doc = [Windows.Data.Xml.Dom.XmlDocument]::new()
$doc.LoadXml($xml)
$toast = [Windows.UI.Notifications.ToastNotification]::new($doc)
$appId = '{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe'
try { [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier($appId).Show($toast) }
catch { [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier().Show($toast) }
Write-Output 'winrt'
"#;

    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("powershell");
        c.args(["-NoProfile", "-Command", script])
            .env("MCP_NOTIFY_TITLE", &display_title)
            .env("MCP_NOTIFY_BODY", body)
            .env("MCP_NOTIFY_DURATION_MS", duration_ms.to_string())
            .creation_flags(CREATE_NO_WINDOW);
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = Command::new("powershell");
        c.args(["-NoProfile", "-Command", script])
            .env("MCP_NOTIFY_TITLE", &display_title)
            .env("MCP_NOTIFY_BODY", body)
            .env("MCP_NOTIFY_DURATION_MS", duration_ms.to_string());
        c
    };

    match cmd.output() {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            if output.status.success() {
                let backend = stdout.lines().last().unwrap_or("powershell").trim();
                json!({"success": true, "backend": backend, "title": display_title, "body": body})
            } else {
                json!({"error": stderr.trim(), "stdout": stdout.trim()})
            }
        }
        Err(e) => json!({"error": format!("{}", e)}),
    }
}

fn kill_process(args: &Value) -> Value {
    let pid = args.get("pid").and_then(|v| v.as_i64()).unwrap_or(0);

    #[cfg(windows)]
    let output = Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    #[cfg(not(windows))]
    let output = Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .output();

    match output {
        Ok(out) => {
            if out.status.success() {
                json!(format!("killed: {}", pid))
            } else {
                json!(format!(
                    "[ERROR] {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ))
            }
        }
        Err(e) => json!(format!("[ERROR] {}", e)),
    }
}

fn list_process(args: &Value) -> Value {
    let filter = args.get("filter").and_then(|v| v.as_str()).unwrap_or("");
    let ps_cmd = if filter.is_empty() {
        "Get-Process | Select-Object ProcessName, Id | Format-Table -Auto".to_string()
    } else {
        format!("Get-Process | Where-Object {{ $_.ProcessName -like '*{}*' }} | Select-Object ProcessName, Id | Format-Table -Auto", filter)
    };

    #[cfg(windows)]
    let output = Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps_cmd])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    #[cfg(not(windows))]
    let output = Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps_cmd])
        .output();

    match output {
        Ok(out) => json!(String::from_utf8_lossy(&out.stdout).trim().to_string()),
        Err(e) => json!(format!("[ERROR] {}", e)),
    }
}

fn port_check(args: &Value) -> Value {
    let host = args
        .get("host")
        .and_then(|v| v.as_str())
        .unwrap_or("127.0.0.1");
    let port = match args.get("port").and_then(|v| v.as_u64()) {
        Some(p) => p as u16,
        None => return json!({"error": "port required"}),
    };
    let timeout_ms = args
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(2000);

    let addr = format!("{}:{}", host, port);
    let socket_addr: std::net::SocketAddr = match addr.parse() {
        Ok(a) => a,
        Err(e) => return json!({"error": format!("Invalid address {}: {}", addr, e)}),
    };

    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_millis(timeout_ms);

    match std::net::TcpStream::connect_timeout(&socket_addr, timeout) {
        Ok(_) => {
            json!({"open": true, "host": host, "port": port, "connect_time_ms": start.elapsed().as_millis()})
        }
        Err(e) => {
            json!({"open": false, "host": host, "port": port, "error": e.to_string(), "elapsed_ms": start.elapsed().as_millis()})
        }
    }
}
