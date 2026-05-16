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

## Authenticate each backend (one-time setup)

Manager doesn't store API keys or OAuth tokens itself — **each delegated coding agent owns its own credentials**. Before manager can dispatch to a backend, you authenticate that backend once via its own CLI. Run these once on your machine; manager picks up the authenticated sessions automatically.

### Codex (ChatGPT-backed)

```powershell
# Install Codex CLI first: https://github.com/openai/codex
codex login
```

Codex starts a local OAuth callback server (default `http://localhost:1455`) and opens your browser to `auth.openai.com`. Sign in with your ChatGPT account, pick a workspace if you have multiple, click **Continue** on the consent page, close the browser tab. The terminal will print `Successfully logged in`. Subsequent `codex` invocations (including those manager makes) reuse the session.

### Claude Code

```powershell
# Install Claude Code first: https://github.com/anthropics/claude-code
claude --setup
```

OAuth flow in browser → sign in with your Anthropic account → done. Claude Code is now ready to be invoked as a subprocess by manager.

### Gemini CLI

```powershell
# Install Gemini CLI first: https://github.com/google-gemini/gemini-cli
gemini auth login
```

### LM Studio (local LLM, beta)

Open LM Studio → **Local Server** in the sidebar → load a model → **Start Server**. Manager auto-detects an LM Studio server on `http://localhost:1234`. No auth required for local-only models. *Note:* model behavior varies — some local LLMs handle tool calls cleanly, others don't. Manager flags this when it detects an LM Studio backend.

### Verify which backends manager can reach

```powershell
C:\tools\universal-ops\manager.exe backends list
```

Manager prints the status of each known backend (authenticated / not configured / unreachable). If a backend isn't authenticated, manager skips it when picking the "smartest available." Re-run any of the login commands above to refresh credentials.

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

## Manual delegation (until v1 ships)

Universal-Ops v0.1.0-alpha is **scaffold-only** — `manager.exe` doesn't yet pick backends or dispatch tasks, `ops.exe` doesn't yet expose its tool surface, `dashboard.exe` doesn't yet serve UI. While we build the real implementation, here's how to delegate manually with the underlying CLIs you've already authenticated:

1. **Authenticate each backend** you want available (see [Authenticate each backend](#authenticate-each-backend-one-time-setup) above)
2. **Pick a backend** based on your task — Codex for ChatGPT-style coding, Claude Code for Anthropic, Gemini for Google, LM Studio for offline
3. **Compose your prompt** — describe the task, paste relevant code, point at file paths
4. **Run the CLI directly**:
   ```powershell
   codex "Implement a Rust function that does X..."
   # or: claude "Refactor src/foo.rs to ..."
   # or: gemini "Read src/lib.rs and add tests for ..."
   ```
5. **Review the output** before letting any agent run commands or modify files

When v1 ships, `manager.exe dispatch "your task"` will:
- Auto-detect which backends are authenticated and reachable
- Pick the smartest one for the task type (or let you `--backend codex` etc.)
- Stream output back to your AI client via MCP
- Track the operation as a breadcrumb for the dashboard
- Roll up child-agent progress under the parent session

The token storage stays exactly the same — **manager never owns credentials**, the delegated CLIs do. v1 is purely additive UX.

Implementation iterates here in the open. PRs welcome — see [CONTRIBUTING.md](CONTRIBUTING.md).

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
