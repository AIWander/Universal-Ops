//! Universal-Ops ops — local execution hands for delegated coding agents.
//!
//! v0.1.0-alpha: install/uninstall subcommands wired; serve mode is scaffold-only.
//! See https://github.com/AIWander/Universal-Ops for status.

use anyhow::Result;

const SERVER_KEY: &str = "universal-ops";
const BINARY_NAME: &str = "ops";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let sub = args.get(1).map(|s| s.as_str());

    match sub {
        Some("--version") | Some("-V") => {
            println!("{} {}", BINARY_NAME, env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("--help") | Some("-h") => {
            print_help();
            Ok(())
        }
        Some("install") => install_common::install(SERVER_KEY, &args[2..]),
        Some("uninstall") => install_common::uninstall(SERVER_KEY, &args[2..]),
        Some("serve") | None => run_serve(),
        Some(other) => {
            eprintln!("Unknown subcommand: {}", other);
            print_help();
            std::process::exit(2);
        }
    }
}

fn print_help() {
    println!("Universal-Ops ops v{}", env!("CARGO_PKG_VERSION"));
    println!();
    println!("USAGE:");
    println!("  ops                              Run as MCP server (scaffold-only currently)");
    println!("  ops serve                        Same as above");
    println!("  ops install --target <host>      Register with host config as '{}'", SERVER_KEY);
    println!("  ops uninstall --target <host>    Unregister from host config");
    println!("  ops --version                    Print version");
    println!("  ops --help                       Print this help");
    println!();
    install_common::print_install_help(BINARY_NAME, SERVER_KEY);
    println!();
    println!("Repository: https://github.com/AIWander/Universal-Ops");
}

fn run_serve() -> Result<()> {
    eprintln!("ops (Universal-Ops) v{} — serve mode is scaffold-only.", env!("CARGO_PKG_VERSION"));
    eprintln!("Real tool surface lands in subsequent commits.");
    std::process::exit(2);
}
