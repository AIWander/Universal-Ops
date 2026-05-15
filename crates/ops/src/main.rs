//! Universal-Ops ops — local execution hands for delegated coding agents.
//!
//! v0.1.0-alpha: scaffold only. Real tool surface lands in subsequent commits.

fn main() {
    let version = env!("CARGO_PKG_VERSION");
    let args: Vec<String> = std::env::args().collect();

    match args.get(1).map(String::as_str) {
        Some("--version") | Some("-V") => {
            println!("ops {version}");
        }
        Some("install") => {
            eprintln!("install subcommand not yet implemented (scaffold v{version}).");
            std::process::exit(2);
        }
        Some("uninstall") => {
            eprintln!("uninstall subcommand not yet implemented (scaffold v{version}).");
            std::process::exit(2);
        }
        _ => {
            eprintln!("ops (Universal-Ops) v{version} — scaffold, not yet functional.");
            eprintln!("See https://github.com/AIWander/Universal-Ops for status.");
        }
    }
}
