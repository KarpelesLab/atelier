//! atelier — a minimal-TUI AI coding harness over OpenAI-compatible APIs.
//!
//! `main` wires the pieces together and hands control to the agent loop. The
//! plain stdin REPL ([`agent::repl`]) drives it until the inline TUI (M2,
//! `src/tui`) takes over.

mod agent;
mod config;
mod context;
mod mcp;
mod provider;
mod tools;
#[cfg(feature = "tui")]
mod tui;

use anyhow::Result;

use config::Config;

fn main() -> Result<()> {
    let cfg = Config::from_env();
    let root = std::env::current_dir()?;

    let tools = tools::builtin_registry();
    let context = context::default_providers();
    let session = agent::Session::new(cfg, root, tools, context);

    #[cfg(feature = "tui")]
    {
        tui::run(session)
    }
    #[cfg(not(feature = "tui"))]
    {
        agent::repl(session)
    }
}
