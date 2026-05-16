//! install-common — shared install/uninstall subcommand logic for Universal-Ops binaries.
//!
//! Each binary calls `install_common::install(server_key, args)` from its main.rs to
//! register itself with AI host configs (claude-desktop, claude-code, lm-studio).
//! Server key is the MCP-config key under which the binary registers
//! (e.g., "universal-manager", "universal-ops", "universal-dashboard").

use anyhow::{Context, Result, bail};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    ClaudeDesktop,
    ClaudeCode,
    LmStudio,
}

impl Target {
    pub fn all() -> &'static [Target] {
        &[Target::ClaudeDesktop, Target::ClaudeCode, Target::LmStudio]
    }

    pub fn parse(s: &str) -> Option<Vec<Target>> {
        match s {
            "claude-desktop" => Some(vec![Target::ClaudeDesktop]),
            "claude-code"    => Some(vec![Target::ClaudeCode]),
            "lm-studio"      => Some(vec![Target::LmStudio]),
            "all"            => Some(Target::all().to_vec()),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Target::ClaudeDesktop => "claude-desktop",
            Target::ClaudeCode    => "claude-code",
            Target::LmStudio      => "lm-studio",
        }
    }

    pub fn config_path(&self) -> Option<PathBuf> {
        match self {
            Target::ClaudeDesktop => {
                dirs::config_dir().map(|p| p.join("Claude").join("claude_desktop_config.json"))
            }
            Target::ClaudeCode => {
                dirs::home_dir().map(|p| p.join(".claude").join("settings.json"))
            }
            Target::LmStudio => {
                dirs::home_dir().map(|p| p.join(".lmstudio").join("mcp.json"))
            }
        }
    }
}

/// Install subcommand entry point. `server_key` is the MCP-config key
/// (e.g., "universal-manager"). `args` is everything after `install` on the CLI.
pub fn install(server_key: &str, args: &[String]) -> Result<()> {
    install_or_uninstall(server_key, args, false)
}

/// Uninstall subcommand entry point. Same calling convention as `install`.
pub fn uninstall(server_key: &str, args: &[String]) -> Result<()> {
    install_or_uninstall(server_key, args, true)
}

fn install_or_uninstall(server_key: &str, args: &[String], remove: bool) -> Result<()> {
    let target_str = parse_target_arg(args)
        .context("missing required --target <host> (one of: claude-desktop, claude-code, lm-studio, all)")?;

    let targets = Target::parse(&target_str)
        .with_context(|| format!("unknown target: '{}'. Valid: claude-desktop, claude-code, lm-studio, all", target_str))?;

    let exe_path = std::env::current_exe()
        .context("could not resolve current executable path")?;
    let exe_str = exe_path.to_string_lossy().to_string();

    let action = if remove { "uninstall" } else { "install" };
    println!("{} target(s): {}", action, targets.iter().map(|t| t.name()).collect::<Vec<_>>().join(", "));
    println!("server key: {}", server_key);
    println!("exe path:   {}", exe_str);
    println!();

    let mut any_success = false;
    let mut any_error = false;

    for target in &targets {
        match apply_target(*target, server_key, &exe_str, remove) {
            Ok(msg) => {
                println!("[OK]   {}: {}", target.name(), msg);
                any_success = true;
            }
            Err(e) => {
                println!("[SKIP] {}: {}", target.name(), e);
                if !is_skip_reason(&e) {
                    any_error = true;
                }
            }
        }
    }

    println!();
    if remove {
        println!("Restart any host apps that had {} loaded for changes to take effect.", server_key);
    } else if any_success {
        println!("Restart any registered host apps for the new tools to appear.");
    }

    if any_error && !any_success {
        std::process::exit(1);
    }
    Ok(())
}

fn parse_target_arg(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--target" | "-t" => {
                return args.get(i + 1).cloned();
            }
            s if s.starts_with("--target=") => {
                return Some(s[9..].to_string());
            }
            _ => i += 1,
        }
    }
    None
}

fn is_skip_reason(e: &anyhow::Error) -> bool {
    let msg = format!("{}", e);
    msg.contains("not detected") || msg.contains("already absent")
}

fn apply_target(target: Target, server_key: &str, exe_path: &str, remove: bool) -> Result<String> {
    let config_path = target.config_path()
        .with_context(|| format!("could not resolve config path for {}", target.name()))?;

    let parent = config_path.parent()
        .with_context(|| format!("config path has no parent: {}", config_path.display()))?;

    if !parent.exists() {
        bail!("host not detected (no {} directory)", parent.display());
    }

    let mut config = read_or_init_config(&config_path)?;
    let servers_map = ensure_mcp_servers_map(&mut config);

    if remove {
        if servers_map.remove(server_key).is_none() {
            bail!("entry already absent");
        }
    } else {
        servers_map.insert(server_key.to_string(), json!({
            "command": exe_path,
            "args": []
        }));
    }

    backup_if_exists(&config_path)?;
    write_config_pretty(&config_path, &config)?;

    Ok(format!("{}", config_path.display()))
}

fn read_or_init_config(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read failed: {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&text)
        .with_context(|| format!("invalid JSON in {}", path.display()))
}

fn ensure_mcp_servers_map(config: &mut Value) -> &mut serde_json::Map<String, Value> {
    let obj = config.as_object_mut().expect("top-level config must be a JSON object");
    if !obj.contains_key("mcpServers") {
        obj.insert("mcpServers".to_string(), json!({}));
    }
    obj.get_mut("mcpServers")
        .and_then(|v| v.as_object_mut())
        .expect("mcpServers should be an object after insertion")
}

fn backup_if_exists(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let backup = path.with_extension(format!("pre_{}.bak", ts));
    std::fs::copy(path, &backup)
        .with_context(|| format!("backup failed: {} -> {}", path.display(), backup.display()))?;
    Ok(())
}

fn write_config_pretty(path: &Path, config: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create parent dir: {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(config)
        .context("failed to serialize config to JSON")?;
    std::fs::write(path, text)
        .with_context(|| format!("write failed: {}", path.display()))?;
    Ok(())
}

/// Print standard help text for the install/uninstall subcommands.
/// Each binary calls this from its --help handler with its own server_key.
pub fn print_install_help(binary_name: &str, server_key: &str) {
    println!("INSTALL TARGETS for {} (registers as '{}'):", binary_name, server_key);
    println!("  claude-desktop    %APPDATA%\\Claude\\claude_desktop_config.json");
    println!("  claude-code       %USERPROFILE%\\.claude\\settings.json");
    println!("  lm-studio         %USERPROFILE%\\.lmstudio\\mcp.json");
    println!("  all               All detected hosts");
}
