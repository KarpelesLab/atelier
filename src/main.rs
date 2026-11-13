//! atelier — a minimal-TUI AI coding harness over OpenAI-compatible APIs.
//!
//! `main` wires the pieces together and hands control to the agent loop. The
//! plain stdin REPL ([`agent::repl`]) drives it until the inline TUI (M2,
//! `src/tui`) takes over.

mod agent;
mod agents;
mod auth;
mod claude;
mod cli;
mod commands;
mod config;
mod context;
mod coord;
mod doctor;
mod headless;
mod hooks;
mod js;
mod mcp;
mod memory;
mod oauth;
mod permissions;
mod provider;
mod review;
mod risk;
mod schema;
mod session;
mod settings;
mod shlex;
mod skills;
mod tools;
#[cfg(tui)]
mod tui;
mod update;
mod usage;

use std::process::ExitCode;

use anyhow::Result;

use config::Config;
use settings::Settings;

/// The exit status is part of the headless contract (see `headless`): 0 for
/// success, 1 for a failure, 2 for a usage error, then one per way a
/// `--print` run can end short of an answer.
fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("Error: {e:?}");
            ExitCode::from(headless::EXIT_ERROR)
        }
    }
}

fn run() -> Result<u8> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Anywhere among the options (what follows a bare `--` is a prompt).
    if args
        .iter()
        .take_while(|a| *a != "--")
        .any(|a| a == "--version" || a == "-V")
    {
        println!("{}", update::version_line());
        return Ok(0);
    }
    // `atelier update`: install the latest release now (see `update`).
    if args.first().map(String::as_str) == Some("update") {
        return update::run_command().map(|()| 0);
    }
    // `atelier coordinator [status|stop]`: the daemon that connects atelier
    // instances (they launch it themselves), or a word with it (see `coord`).
    if args.first().map(String::as_str) == Some("coordinator") {
        return coord::run_command(&args[1..]).map(|()| 0);
    }
    // `atelier permissions check …`: what the permission rules decide for a
    // call, and which rule decided (see `permissions`).
    if args.first().map(String::as_str) == Some("permissions") {
        return permissions::run_command(&args[1..]).map(|()| 0);
    }
    // `atelier doctor`: check the setup a session here would run with,
    // without starting one (see `doctor`).
    if args.first().map(String::as_str) == Some("doctor") {
        return Ok(doctor::run_command(&args[1..]));
    }
    // `atelier stats …`: totals of the usage log (see `usage`).
    if args.first().map(String::as_str) == Some("stats") {
        return usage::run_command(&args[1..]).map(|()| 0);
    }
    // `atelier mcp-serve <socket>`: the MCP server the Claude backend has
    // claude launch — a relay back to the atelier process that owns the
    // session (see `claude::relay`). Nothing else runs in that mode.
    #[cfg(unixlike)]
    if args.first().map(String::as_str) == Some("mcp-serve") {
        let socket = args
            .get(1)
            .ok_or_else(|| anyhow::anyhow!("usage: atelier mcp-serve <socket>"))?;
        return claude::relay(socket).map(|()| 0);
    }
    // `atelier review …` and `atelier debug prompt …`: runs of their own, on
    // the session set up below like any other. Their arguments are their
    // own (see `cli::subcommand`).
    let sub = match cli::subcommand(&args) {
        Ok(sub) => sub,
        Err(cli::Early::Help(text)) => {
            println!("{text}");
            return Ok(0);
        }
        Err(cli::Early::Usage(msg)) => {
            eprintln!("atelier: {msg}");
            return Ok(headless::EXIT_USAGE);
        }
    };
    // Everything else is flags (see `cli`): `--continue`/`-c` resumes the
    // saved session; `--print`/`-p` runs non-interactively and exits (see
    // `headless::run`). The two combine (`--continue --print ...`) to resume
    // history and then run a prompt against it.
    let cli = match cli::parse(if sub.is_some() { &[] } else { &args }) {
        Ok(cli) => cli,
        Err(msg) => {
            eprintln!("atelier: {msg}\n\n{}", cli::USAGE);
            return Ok(headless::EXIT_USAGE);
        }
    };
    if cli.help {
        println!("{}", cli::USAGE);
        return Ok(0);
    }
    // `--mode`, else `ATELIER_MODE`. A value that names no mode stops the
    // run: `plan` misspelt must not quietly become a mode that edits.
    let mode = match cli
        .mode
        .map_or_else(permissions::Mode::from_env, |m| Ok(Some(m)))
    {
        Ok(mode) => mode,
        Err(msg) => {
            eprintln!("atelier: {msg}");
            return Ok(headless::EXIT_USAGE);
        }
    };
    // Daily check for a newer release, in the background (official builds).
    update::spawn_background_check();
    // `--claude` selects the Claude Code backend (same as ATELIER_BACKEND=claude).
    let mut cfg = Config::from_env(cli.claude);
    let claude = matches!(cfg.backend, config::Backend::Claude { .. });
    let root = std::env::current_dir()?;
    let settings = Settings::load(&root).unwrap_or_else(|e| {
        eprintln!("warning: {e:#}; using empty settings");
        Settings::default()
    });

    // The model tables of `atelier.toml` — providers, a model per role
    // (`[defaults] model` among them, when ATELIER_MODEL isn't set),
    // variants — from the project's file over the user-global one. They
    // describe endpoints atelier calls, so none applies to the Claude
    // backend (where claude picks unless ATELIER_MODEL says). A user file
    // that can't be read is reported with the permission rules it also
    // holds.
    let user_settings = Settings::load_user().unwrap_or_default();
    for warning in cfg.apply_settings(&settings, &user_settings) {
        eprintln!("warning: {warning}");
    }

    let mut tools = tools::builtin_registry();
    // Join the other atelier instances on this machine, starting the
    // coordinator that connects them if this is the first one up.
    let coordinator = coord::start(&root, &cfg.model);
    if let Some(c) = &coordinator {
        coord::tools::register(&mut tools, c);
    }
    // Subagents run on atelier's own loop; claude has its own (see
    // `tools::task`).
    if !claude {
        tools.register(Box::new(tools::task::TaskTool::new(
            root.clone(),
            agents::global_dir(),
        )));
    }
    let context = context::default_providers(settings.diagnostics.clone());
    let mut session = agent::Session::new(cfg, root, tools, context, settings);
    if let Some(mode) = mode {
        session.set_mode(mode);
    }
    if let Some(c) = coordinator {
        session.attach_coordinator(c);
    }
    // Restore a prior Sign-in-with-ChatGPT session, if any (it routes
    // atelier's own loop, which the Claude backend doesn't use).
    if !claude && let Some(tokens) = auth::load() {
        session.use_chatgpt(tokens, None);
    }
    if cli.resume {
        eprintln!("{}", session.resume());
    }
    // Hooks: say which of the project's are waiting for approval (a headless
    // run can't ask, so it skips them), then run `session_start`.
    for line in session.start_hooks(cli.print.is_none() && sub.is_none()) {
        eprintln!("{line}");
    }

    match sub {
        Some(cli::Sub::Review(opts)) => {
            return Ok(finish(headless::review::run(session, opts)));
        }
        Some(cli::Sub::DebugPrompt(prompt)) => {
            return Ok(finish(headless::debug_prompt(session, prompt)));
        }
        None => {}
    }

    // Handled before the interactive TUI/REPL branch below, and unconditional
    // on the `tui` feature, so `--print` works in both build configs.
    if let Some(opts) = cli.print {
        return Ok(finish(headless::run(session, opts)));
    }

    #[cfg(tui)]
    let result = tui::run(session);
    #[cfg(not(tui))]
    let result = agent::repl(session);
    finish(result).map(|()| 0)
}

/// On the way out: stop the `bash` tool's background jobs, which would
/// otherwise outlive atelier.
fn finish<T>(result: T) -> T {
    tools::bash::kill_all();
    result
}
