# Security Policy

## Reporting a vulnerability

If you find a security issue in Universal-Ops (manager / ops / dashboard), email **josephwander@gmail.com** directly. **Do not** open a public GitHub issue.

We aim to:
- Acknowledge within 72 hours
- Triage within 7 days
- Ship a fix or mitigation for high-severity issues within 14 days

## In scope

- The `manager.exe`, `ops.exe`, and `dashboard.exe` binaries
- The `install` / `uninstall` subcommands and their config-file edits
- State directories: `./.manager/`, `./.ops/`, `./.dashboard/` (permissions, contents)
- The dashboard's local HTTP server on `127.0.0.1:9999`
- Cross-binary IPC (breadcrumb store, dead drops, checkpoints)
- Shipping artifacts on the Releases page (zip / MSI)

## Out of scope

- Third-party MCP host apps (Claude Desktop, LM Studio, Cowork, Claude Code)
- Third-party coding agents that manager delegates to (Claude Code, Codex, Gemini, LM Studio LLMs)
- The user's host operating system
- Issues in Rust, Cargo, or third-party crates (report those upstream)

## Disclosure

After a fix lands, we'll credit the reporter (with permission) in release notes.
