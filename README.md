# Universal-Ops

> **Operations Locally** — the local execution layer for any AI coding agent.

**Status:** alpha. Pairs with manager-delegated coding agents (Claude Code, Codex, Gemini, LM Studio LLMs).

[![Build](https://github.com/AIWander/Universal-Ops/actions/workflows/build.yml/badge.svg)](https://github.com/AIWander/Universal-Ops/actions)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Platform: Windows](https://img.shields.io/badge/Platform-Windows%20x64%20%7C%20ARM64-blue.svg)](https://github.com/AIWander/Universal-Ops/releases)

## What's in the box

Three binaries that work together:

| Binary | Registers as | What it does |
|---|---|---|
| **manager.exe** | `universal-manager` | Orchestrator. You ask "do X" in your chat; manager picks the smartest available coding agent and delegates. ~44 tools. |
| **ops.exe** | `universal-ops` | The hands. Whatever agent manager pulled in uses ops to actually operate on your machine — shells, files, git, deploy lifecycle, breadcrumbs. ~80 tools. |
| **dashboard.exe** | `universal-dashboard` | Web UI at `http://127.0.0.1:9999`. Cross-session breadcrumb view, heartbeat tracker, manager status, parent→child rollup of delegated agent work. |

The model:

```
You chat in Claude Desktop / Cowork
            ↓
       manager  (picks the smartest available coding agent)
            ↓
   Claude Code / Codex / Gemini / LM Studio LLM
            ↓
         ops  (local execution: shells, files, git, deploy, breadcrumbs)
            ↓
      Your Windows machine

   dashboard sees everything, shows you what's happening.
```

## Install

### Option 1 — Portable (recommended)

1. Download `universal-ops-windows-x64.zip` (or `arm64`) from [Releases](https://github.com/AIWander/Universal-Ops/releases/latest)
2. Extract to `C:\tools\universal-ops\`
3. Register all three binaries with your AI host:
   ```powershell
   C:\tools\universal-ops\manager.exe   install --target claude-desktop
   C:\tools\universal-ops\ops.exe       install --target claude-desktop
   C:\tools\universal-ops\dashboard.exe install --target claude-desktop --autostart
   ```
4. Restart your AI host

### Option 2 — MSI installer

1. Download `universal-ops-windows-x64.msi` from [Releases](https://github.com/AIWander/Universal-Ops/releases/latest)
2. Run it. The MSI registers all three binaries (`universal-manager`, `universal-ops`, `universal-dashboard`) as a post-install action.
3. Restart your AI host

### Option 3 — Have your AI install it for you

Open Claude / ChatGPT / your local LLM and paste:

> Install **AIWander/Universal-Ops** on my Windows machine using the AI install runbook at <https://github.com/AIWander/Universal-Ops#for-ai-assistants>

## For AI Assistants

If you're an AI installing this for a user, run these steps in order:

### 1. Detect environment

```powershell
$arch = if ((Get-CimInstance Win32_Processor).Architecture -eq 12) { "arm64" } else { "x64" }
$hosts = @()
if (Test-Path "$env:APPDATA\Claude\claude_desktop_config.json") { $hosts += "claude-desktop" }
if (Test-Path "$env:USERPROFILE\.lmstudio\mcp.json")            { $hosts += "lm-studio"      }
if (Test-Path "$env:USERPROFILE\.claude\settings.json")         { $hosts += "claude-code"    }
Write-Host "Arch: $arch | Hosts detected: $($hosts -join ', ')"
```

### 2. Download + extract

```powershell
$url = "https://github.com/AIWander/Universal-Ops/releases/latest/download/universal-ops-windows-$arch.zip"
Invoke-WebRequest $url -OutFile $env:TEMP\universal-ops.zip
New-Item -ItemType Directory -Force -Path C:\tools\universal-ops | Out-Null
Expand-Archive $env:TEMP\universal-ops.zip -DestinationPath C:\tools\universal-ops -Force
```

### 3. Register all three binaries with detected hosts

```powershell
foreach ($h in $hosts) {
  C:\tools\universal-ops\manager.exe   install --target $h
  C:\tools\universal-ops\ops.exe       install --target $h
  C:\tools\universal-ops\dashboard.exe install --target $h --autostart
}
```

### 4. Verify

```powershell
C:\tools\universal-ops\manager.exe --version
Test-NetConnection 127.0.0.1 -Port 9999  # dashboard listens here
```

### 5. Open the dashboard

```powershell
Start-Process http://127.0.0.1:9999
```

### 6. Tell the user

"Universal-Ops is installed. Manager, ops, and dashboard registered with: $($hosts -join ', '). Dashboard at http://127.0.0.1:9999. Restart those host apps now."

## Coexists with the legacy AIWander/ops

Universal-Ops is the **next-generation** manager+ops+dashboard bundle. The existing [`AIWander/ops`](https://github.com/AIWander/ops) repo (single-binary ops server, no manager/dashboard) keeps working — Universal-Ops registers under different MCP keys (`universal-manager` / `universal-ops` / `universal-dashboard`) so both stacks can run side-by-side.

Same applies to [`AIWander/manager-universal`](https://github.com/AIWander/manager-universal) — Universal-Ops's `manager.exe` registers as `universal-manager`, not `manager`, so the existing manager-universal install is untouched.

## Uninstall

```powershell
C:\tools\universal-ops\manager.exe   uninstall --target all
C:\tools\universal-ops\ops.exe       uninstall --target all
C:\tools\universal-ops\dashboard.exe uninstall --target all
Remove-Item C:\tools\universal-ops -Recurse -Force
```

## State directories

Each binary keeps its state in a hidden folder next to its exe:

- `./.manager/` — orchestration state, agent registry, dispatch log, breadcrumbs
- `./.ops/` — breadcrumbs, checkpoints, dead-drops, bag state, reminders
- `./.dashboard/` — heartbeat records, dashboard cache

Fully portable: copy the install folder to another machine and your state goes with it.

## Breadcrumb naming convention

All breadcrumbs Universal-Ops creates follow the CPC-wide naming rule:

```
[<client>:<thread-or-session-suffix>] <operation> | targets: <files>
```

Example:

```
[claude-code:trading-research] manager dispatch to codex | targets: ./.manager/dispatch.log
```

This keeps the cross-session dashboard view self-describing.

## Build from source

```bash
git clone https://github.com/AIWander/Universal-Ops
cd Universal-Ops
cargo build --release --workspace
# Binaries at: target/release/{manager,ops,dashboard}.exe
```

Requires Rust 1.75+.

## Companion: AIWander/Programmer-Wander

If you want a **single-AI dev shell** without the manager/dashboard layer, see [AIWander/Programmer-Wander](https://github.com/AIWander/Programmer-Wander). That repo is the standalone dev MCP — same toolset as ops minus the manager orchestration.

The two repos are independent — you can install either, both, or neither.

## License

MIT. See [LICENSE](LICENSE).
