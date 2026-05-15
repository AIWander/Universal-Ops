# Contributing to Universal-Ops

Thanks for thinking about contributing — this project is alpha and issues + PRs are very welcome.

Universal-Ops is a 3-binary Cargo workspace: `manager`, `ops`, `dashboard`. Each lives in its own crate under `crates/`.

## Quick start

```bash
git clone https://github.com/AIWander/Universal-Ops
cd Universal-Ops
cargo build --release --workspace
```

Requires Rust 1.75+.

## Ground rules

- Open an issue before a big change so we can discuss approach
- Keep PRs focused — one concern per PR (and ideally one crate per PR)
- Match existing code style (`cargo fmt` + `cargo clippy` clean)
- Add a test if you fix a bug or add behavior
- Update docs (README, doc comments) when behavior changes
- Cross-binary changes (e.g., a new breadcrumb field) should ship coordinated across all three crates in one PR

## Running checks locally

```bash
cargo fmt --check
cargo clippy --release --workspace -- -D warnings
cargo test --release --workspace
```

CI runs the same on x64 and ARM64.

## Reporting bugs

Use the [bug report template](.github/ISSUE_TEMPLATE/bug_report.yml). Include:

- Which binary (manager / ops / dashboard) is affected
- Windows version + architecture (x64 / ARM64)
- Host app (Claude Desktop / LM Studio / Cowork / Claude Code)
- Version output from the affected binary
- Steps to reproduce, expected vs actual behavior
- Relevant logs from `./.manager/logs/`, `./.ops/logs/`, or `./.dashboard/logs/`

## Reporting security issues

See [SECURITY.md](SECURITY.md). **Do not open a public issue** for security reports.

## Code of Conduct

By participating, you agree to the [Code of Conduct](CODE_OF_CONDUCT.md).

## License

Contributions are licensed under [MIT](LICENSE).
