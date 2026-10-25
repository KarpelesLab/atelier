//! atelier — a minimal-TUI AI coding harness over OpenAI-compatible APIs.
//!
//! `main` wires the pieces together and hands control to the agent loop. The
//! plain stdin REPL ([`agent::repl`]) drives it until the inline TUI (M2,
//! `src/tui`) takes over.

mod agent;
mod auth;
mod claude;
mod config;
mod context;
mod headless;
mod js;
mod mcp;
mod provider;
mod review;
mod risk;
mod session;
mod settings;
mod shlex;
mod tools;
#[cfg(feature = "tui")]
mod tui;
mod update;

use anyhow::Result;

use config::Config;
use settings::Settings;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `atelier mcp-serve <socket>`: the MCP server the Claude backend has
    // claude launch — a relay back to the atelier process that owns the
    // session (see `claude::relay`). Nothing else runs in that mode.
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("{}", update::version_line());
        return Ok(());
    }
    // `atelier update`: install the latest release now (see `update`).
    if args.first().map(String::as_str) == Some("update") {
        return update::run_command();
    }
    #[cfg(unix)]
    if args.first().map(String::as_str) == Some("mcp-serve") {
        let socket = args
            .get(1)
            .ok_or_else(|| anyhow::anyhow!("usage: atelier mcp-serve <socket>"))?;
        return claude::relay(socket);
    }
    // `--claude` selects the Claude Code backend (same as ATELIER_BACKEND=claude).
    // Daily check for a newer release, in the background (official builds).
    update::spawn_background_check();
    let mut cfg = Config::from_env(args.iter().any(|a| a == "--claude"));
    let claude = matches!(cfg.backend, config::Backend::Claude { .. });
    let root = std::env::current_dir()?;
    let settings = Settings::load(&root).unwrap_or_else(|e| {
        eprintln!("warning: {e:#}; using empty settings");
        Settings::default()
    });

    // `[defaults] model` fills in when ATELIER_MODEL isn't set in the env.
    // It names a model on the configured endpoint, so it doesn't apply to
    // the Claude backend (where claude picks unless ATELIER_MODEL says).
    if !claude
        && std::env::var_os("ATELIER_MODEL").is_none()
        && let Some(m) = &settings.defaults.model
    {
        cfg.model = m.clone();
    }

    // Minimal flag handling: `--continue`/`-c` resumes the saved session;
    // `--print`/`-p` runs one prompt non-interactively and exits (see
    // `headless::run`). The two combine (`--continue --print ...`) to resume
    // history and then run a single prompt against it.
    let resume = args.iter().any(|a| a == "--continue" || a == "-c");
    let print_at = args.iter().position(|a| a == "--print" || a == "-p");

    let tools = tools::builtin_registry();
    let context = context::default_providers();
    let mut session = agent::Session::new(cfg, root, tools, context, settings);
    // Restore a prior Sign-in-with-ChatGPT session, if any (it routes
    // atelier's own loop, which the Claude backend doesn't use).
    if !claude && let Some(tokens) = auth::load() {
        session.use_chatgpt(tokens, None);
    }
    if resume {
        eprintln!("{}", session.resume());
    }

    // Handled before the interactive TUI/REPL branch below, and unconditional
    // on the `tui` feature, so `--print` works in both build configs.
    if let Some(idx) = print_at {
        let prompt = match headless::prompt_from_args(&args[idx + 1..]) {
            Some(p) => p,
            None => headless::read_stdin_prompt()?,
        };
        return finish(headless::run(session, &prompt));
    }

    #[cfg(feature = "tui")]
    let result = tui::run(session);
    #[cfg(not(feature = "tui"))]
    let result = agent::repl(session);
    finish(result)
}

/// On the way out: stop the `bash` tool's background jobs, which would
/// otherwise outlive atelier.
fn finish(result: Result<()>) -> Result<()> {
    tools::bash::kill_all();
    result
}
