//! atelier — a minimal-TUI AI coding harness over OpenAI-compatible APIs.
//!
//! `main` wires the pieces together and hands control to the agent loop. The
//! plain stdin REPL ([`agent::repl`]) drives it until the inline TUI (M2,
//! `src/tui`) takes over.

mod agent;
mod config;
mod context;
mod headless;
mod js;
mod mcp;
mod provider;
mod risk;
mod session;
mod settings;
mod shlex;
mod tools;
#[cfg(feature = "tui")]
mod tui;

use anyhow::Result;

use config::Config;
use settings::Settings;

fn main() -> Result<()> {
    let cfg = Config::from_env();
    let root = std::env::current_dir()?;
    let settings = Settings::load(&root).unwrap_or_else(|e| {
        eprintln!("warning: {e:#}; using empty settings");
        Settings::default()
    });

    // Minimal flag handling: `--continue`/`-c` resumes the saved session;
    // `--print`/`-p` runs one prompt non-interactively and exits (see
    // `headless::run`). The two combine (`--continue --print ...`) to resume
    // history and then run a single prompt against it.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let resume = args.iter().any(|a| a == "--continue" || a == "-c");
    let print_at = args.iter().position(|a| a == "--print" || a == "-p");

    let tools = tools::builtin_registry();
    let context = context::default_providers();
    let mut session = agent::Session::new(cfg, root, tools, context, settings);
    if resume {
        let n = session.resume();
        eprintln!("resumed session ({n} message(s))");
    }

    // Handled before the interactive TUI/REPL branch below, and unconditional
    // on the `tui` feature, so `--print` works in both build configs.
    if let Some(idx) = print_at {
        let prompt = match headless::prompt_from_args(&args[idx + 1..]) {
            Some(p) => p,
            None => headless::read_stdin_prompt()?,
        };
        return headless::run(session, &prompt);
    }

    #[cfg(feature = "tui")]
    {
        tui::run(session)
    }
    #[cfg(not(feature = "tui"))]
    {
        agent::repl(session)
    }
}
