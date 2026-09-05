//! Agent loop: conversation state and turn orchestration.
//!
//! A [`Session`] owns the conversation and drives the tool-call loop: gather
//! context, call the provider, and while the model requests tools, execute them
//! (sandboxed, via the [`ToolRegistry`]) and feed the results back — repeating
//! until a turn produces no tool calls.
//!
//! Rendering goes through the [`Ui`] trait so the loop stays independent of the
//! terminal interface (the `tui` module supplies one; [`repl`] uses a plain
//! stdout one). Nothing here may depend on `tui`.

use std::collections::HashSet;
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::Result;
use serde_json::Value;

use crate::config::Config;
use crate::context::{ContextItem, ContextProvider};
use crate::mcp::{self, StdioServer};
use crate::provider::{self, Completion, Message, StreamEvent, ToolCall};
use crate::settings::{McpServerConfig, Settings};
use crate::tools::{FileState, ToolCtx, ToolRegistry};

const SYSTEM_PROMPT: &str = "\
You are atelier, an AI coding assistant operating inside a project directory. \
You have tools to read and modify files and run commands; use them to inspect \
the project and make changes rather than guessing. Keep responses concise. \
When you have completed the request, stop calling tools and give a short final \
answer.";

/// How the loop reports progress. Implemented by the TUI and by the plain REPL.
pub trait Ui {
    /// A chunk of the model's reasoning ("thinking").
    fn reasoning(&mut self, text: &str);
    /// A chunk of the model's answer.
    fn content(&mut self, text: &str);
    /// A tool is about to run, with its raw JSON arguments.
    fn tool_start(&mut self, name: &str, arguments: &str);
    /// A tool finished; `ok` is false when it returned an error result.
    fn tool_end(&mut self, name: &str, result: &str, ok: bool);
    /// The turn is complete (one full assistant response).
    fn turn_end(&mut self) {}
    /// Informational output from a command (e.g. `/models`, `/mcp`): printed to
    /// the scrollback, not part of a model turn.
    fn info(&mut self, text: &str);
    /// Ask the user to approve a side-effecting tool call before it runs.
    fn ask_approval(&mut self, tool: &str, arguments: &str) -> Approval;
    /// An out-of-band notice (errors, status). Used by the TUI.
    #[allow(dead_code)]
    fn notice(&mut self, text: &str);
}

/// The user's answer to a tool-approval prompt.
pub enum Approval {
    /// Run this once.
    Once,
    /// Run this and remember the tool as always-allowed (persisted).
    Always,
    /// Refuse; the model is told the user denied it.
    Deny,
}

/// What a line of input turned out to be, once [`dispatch`] has looked at it.
pub enum Dispatch {
    /// Not a command — send it to the model as a prompt.
    Prompt,
    /// A command that was handled in place.
    Handled,
    /// The user asked to quit.
    Quit,
}

const HELP: &[&str] = &[
    "commands:",
    "  /help                                 show this help",
    "  /models                               list models offered by the endpoint",
    "  /mcp                                  list configured MCP servers",
    "  /mcp add <name> <command> [args...]   add and connect an MCP server",
    "  /mcp remove <name>                    remove an MCP server",
    "  /clear                                (no-op: scrollback is append-only)",
    "  /quit                                 exit (also Ctrl-D on an empty input)",
];

/// Interpret one line of input: run it if it is a slash command (rendering
/// output through `ui`), otherwise report that it is a prompt for the caller to
/// send. Shared by the REPL and the TUI so commands behave identically in both.
pub fn dispatch(session: &mut Session, line: &str, ui: &mut dyn Ui) -> Dispatch {
    let trimmed = line.trim();
    if !trimmed.starts_with('/') {
        return Dispatch::Prompt;
    }
    let tokens = match crate::shlex::split(trimmed) {
        Ok(t) => t,
        Err(e) => {
            ui.info(&format!("parse error: {e}"));
            return Dispatch::Handled;
        }
    };
    let Some(cmd) = tokens.first().map(String::as_str) else {
        return Dispatch::Handled;
    };
    let args: Vec<&str> = tokens[1..].iter().map(String::as_str).collect();
    match cmd {
        "/quit" | "/exit" => return Dispatch::Quit,
        "/help" => {
            for l in HELP {
                ui.info(l);
            }
        }
        "/models" => match provider::list_models(session.config()) {
            Ok(models) if models.is_empty() => ui.info("(no models returned)"),
            Ok(models) => {
                for m in models {
                    ui.info(&m);
                }
            }
            Err(e) => ui.info(&format!("error: {e:#}")),
        },
        "/mcp" => dispatch_mcp(session, &args, ui),
        "/clear" => ui.info("(scrollback is append-only — nothing to clear)"),
        other => ui.info(&format!("unknown command '{other}' — /help for commands")),
    }
    Dispatch::Handled
}

fn dispatch_mcp(session: &mut Session, args: &[&str], ui: &mut dyn Ui) {
    match args.first().copied() {
        None => {
            let list = session.mcp_list();
            if list.is_empty() {
                ui.info("no MCP servers configured. add one:");
                ui.info("  /mcp add <name> <command> [args...]");
            } else {
                ui.info("configured MCP servers:");
                for l in list {
                    ui.info(&format!("  {l}"));
                }
            }
        }
        Some("add") => {
            let rest = &args[1..];
            if rest.len() < 2 {
                ui.info("usage: /mcp add <name> <command> [args...]");
                return;
            }
            let name = rest[0];
            let command = rest[1];
            let extra: Vec<String> = rest[2..].iter().map(|s| s.to_string()).collect();
            match session.mcp_add(name, command, extra) {
                Ok(n) => ui.info(&format!(
                    "added MCP server '{name}' — {n} tool(s) now available"
                )),
                Err(e) => ui.info(&format!("error: {e:#}")),
            }
        }
        Some("remove") | Some("rm") => {
            let Some(name) = args.get(1) else {
                ui.info("usage: /mcp remove <name>");
                return;
            };
            match session.mcp_remove(name) {
                Ok(true) => ui.info(&format!("removed MCP server '{name}'")),
                Ok(false) => ui.info(&format!("no MCP server named '{name}'")),
                Err(e) => ui.info(&format!("error: {e:#}")),
            }
        }
        Some(other) => {
            ui.info(&format!(
                "unknown /mcp subcommand '{other}' — use: add, remove, or no args to list"
            ));
        }
    }
}

/// A conversation with its tools and context providers.
pub struct Session {
    cfg: Config,
    root: PathBuf,
    tools: ToolRegistry,
    context: Vec<Box<dyn ContextProvider>>,
    settings: Settings,
    /// Tools approved for the rest of this session (seeded from settings, grown
    /// by "always allow").
    allow: HashSet<String>,
    /// Skip approval prompts entirely (e.g. `ATELIER_APPROVE=all` for headless
    /// runs).
    auto_approve: bool,
    /// Prompt+completion tokens of the most recent request (the live context
    /// size), and the cumulative completion tokens generated this session.
    usage_ctx: u32,
    usage_out: u64,
    history: Vec<Message>,
    fstate: FileState,
}

impl Session {
    pub fn new(
        cfg: Config,
        root: PathBuf,
        tools: ToolRegistry,
        context: Vec<Box<dyn ContextProvider>>,
        settings: Settings,
    ) -> Self {
        let allow: HashSet<String> = settings.permissions.allow.iter().cloned().collect();
        let auto_approve = matches!(
            std::env::var("ATELIER_APPROVE").as_deref(),
            Ok("all" | "yes" | "1")
        );
        Self {
            cfg,
            root,
            tools,
            context,
            settings,
            allow,
            auto_approve,
            usage_ctx: 0,
            usage_out: 0,
            history: Vec::new(),
            fstate: FileState::new(),
        }
    }

    /// A short token-usage summary (`<ctx> ctx · <out> out`), or `None` if the
    /// endpoint hasn't reported usage.
    pub fn usage_summary(&self) -> Option<String> {
        if self.usage_ctx == 0 && self.usage_out == 0 {
            return None;
        }
        Some(format!("{} ctx · {} out", self.usage_ctx, self.usage_out))
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Connect every MCP server in the settings and register its tools.
    /// Returns human-readable status lines (one per server) for the caller to
    /// display; a failed server is reported but does not abort the others.
    pub fn connect_configured_mcp(&mut self) -> Vec<String> {
        let servers = self.settings.mcp.clone();
        let mut status = Vec::new();
        for s in servers {
            match mcp::connect_stdio(&to_stdio(&s)) {
                Ok(tools) => {
                    let n = tools.len();
                    for t in tools {
                        self.tools.register(t);
                    }
                    status.push(format!("mcp: connected '{}' ({n} tool(s))", s.name));
                }
                Err(e) => status.push(format!("mcp: failed to connect '{}': {e:#}", s.name)),
            }
        }
        status
    }

    /// One display line per configured MCP server.
    pub fn mcp_list(&self) -> Vec<String> {
        self.settings
            .mcp
            .iter()
            .map(|m| {
                if m.args.is_empty() {
                    format!("{} → {}", m.name, m.command)
                } else {
                    format!("{} → {} {}", m.name, m.command, m.args.join(" "))
                }
            })
            .collect()
    }

    /// Connect a new MCP server, register its tools, and persist it. Returns the
    /// number of tools it advertised. Nothing is saved if the connection fails.
    pub fn mcp_add(&mut self, name: &str, command: &str, args: Vec<String>) -> Result<usize> {
        if self.settings.mcp.iter().any(|m| m.name == name) {
            anyhow::bail!("an MCP server named '{name}' already exists");
        }
        let cfg = McpServerConfig {
            name: name.to_string(),
            command: command.to_string(),
            args,
        };
        let tools = mcp::connect_stdio(&to_stdio(&cfg))?;
        let n = tools.len();
        for t in tools {
            self.tools.register(t);
        }
        self.settings.mcp.push(cfg);
        self.settings.save(&self.root)?;
        Ok(n)
    }

    /// Remove a configured MCP server (and drop its live tools). Returns whether
    /// a server by that name existed.
    pub fn mcp_remove(&mut self, name: &str) -> Result<bool> {
        let before = self.settings.mcp.len();
        self.settings.mcp.retain(|m| m.name != name);
        if self.settings.mcp.len() == before {
            return Ok(false);
        }
        self.tools.remove_prefix(&format!("mcp__{name}__"));
        self.settings.save(&self.root)?;
        Ok(true)
    }

    /// Run one user turn to completion, executing tool calls until the model
    /// stops requesting them. Progress is reported through `ui`.
    pub fn send(&mut self, user_input: &str, ui: &mut dyn Ui) -> Result<()> {
        self.history.push(Message::user(user_input));
        let context_msg = self.gather_context();

        loop {
            // Assemble the request: a single leading system message (prompt +
            // fresh context — some servers reject a second system message), then
            // the conversation history.
            let mut messages = Vec::with_capacity(self.history.len() + 1);
            let system = match &context_msg {
                Some(ctx) => format!("{SYSTEM_PROMPT}\n\n{ctx}"),
                None => SYSTEM_PROMPT.to_string(),
            };
            messages.push(Message::system(system));
            messages.extend(self.history.iter().cloned());

            let specs = self.tools.specs();
            let completion: Completion =
                provider::stream_chat(&self.cfg, &messages, &specs, |ev| match ev {
                    StreamEvent::Reasoning(t) => ui.reasoning(t),
                    StreamEvent::Content(t) => ui.content(t),
                })?;

            if let Some(u) = &completion.usage {
                self.usage_ctx = u.total_tokens;
                self.usage_out += u64::from(u.completion_tokens);
            }

            self.history.push(Message::assistant_tool_calls(
                completion.content.clone(),
                completion.tool_calls.clone(),
            ));

            if completion.tool_calls.is_empty() {
                ui.turn_end();
                return Ok(());
            }

            for call in &completion.tool_calls {
                if self.needs_approval(&call.name) {
                    for signal in crate::risk::signals(&call.name, &call.arguments) {
                        ui.info(&format!("⚠ {signal}"));
                    }
                    match ui.ask_approval(&call.name, &call.arguments) {
                        Approval::Once => {}
                        Approval::Always => self.grant_always(&call.name),
                        Approval::Deny => {
                            let msg =
                                "error: the user denied permission to run this tool.".to_string();
                            ui.tool_end(&call.name, &msg, false);
                            self.history
                                .push(Message::tool_result(call.id.clone(), msg));
                            continue;
                        }
                    }
                }
                ui.tool_start(&call.name, &call.arguments);
                let (result, ok) = self.exec_tool(call);
                ui.tool_end(&call.name, &result, ok);
                self.history
                    .push(Message::tool_result(call.id.clone(), result));
            }
        }
    }

    /// Whether a tool call must be approved before running: the tool declares it
    /// needs approval (unconfined — `bash`, MCP tools), it isn't already
    /// allowed, and we're not in auto-approve mode. Tools confined to the
    /// project directory (all file tools) never prompt.
    fn needs_approval(&self, name: &str) -> bool {
        if self.auto_approve || self.allow.contains(name) {
            return false;
        }
        self.tools
            .get(name)
            .map(|t| t.requires_approval())
            .unwrap_or(false)
    }

    /// Remember a tool as always-allowed for this session and persist it.
    fn grant_always(&mut self, name: &str) {
        self.allow.insert(name.to_string());
        if !self.settings.permissions.allow.iter().any(|n| n == name) {
            self.settings.permissions.allow.push(name.to_string());
            let _ = self.settings.save(&self.root);
        }
    }

    /// Execute a single tool call, returning `(result_text, ok)`.
    fn exec_tool(&mut self, call: &ToolCall) -> (String, bool) {
        let args: Value = if call.arguments.trim().is_empty() {
            Value::Null
        } else {
            match serde_json::from_str(&call.arguments) {
                Ok(v) => v,
                Err(e) => return (format!("error: invalid arguments JSON: {e}"), false),
            }
        };
        let Some(tool) = self.tools.get(&call.name) else {
            return (format!("error: unknown tool '{}'", call.name), false);
        };
        let mut ctx = ToolCtx {
            project_root: &self.root,
            fstate: &mut self.fstate,
        };
        match tool.call(&mut ctx, args) {
            Ok(s) => (s, true),
            Err(e) => (format!("error: {e:#}"), false),
        }
    }

    /// Gather per-turn context into a single injected block, or `None`.
    fn gather_context(&self) -> Option<String> {
        let mut items: Vec<ContextItem> = self
            .context
            .iter()
            .filter_map(|p| p.gather(&self.root))
            .collect();
        if items.is_empty() {
            return None;
        }
        items.sort_by_key(|b| std::cmp::Reverse(b.priority));
        let mut s = String::from("Current project context:\n");
        for it in items {
            s.push_str(&format!("\n## {}\n{}\n", it.title, it.body));
        }
        Some(s)
    }
}

fn to_stdio(cfg: &McpServerConfig) -> StdioServer {
    StdioServer {
        name: cfg.name.clone(),
        command: cfg.command.clone(),
        args: cfg.args.clone(),
    }
}

/// A plain stdout [`Ui`]: reasoning dimmed, answer normal, tools annotated.
pub struct StdoutUi {
    in_reasoning: bool,
}

const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

impl StdoutUi {
    pub fn new() -> Self {
        Self {
            in_reasoning: false,
        }
    }
    fn end_reasoning(&mut self) {
        if self.in_reasoning {
            print!("{RESET}");
            println!();
            self.in_reasoning = false;
        }
    }
}

impl Default for StdoutUi {
    fn default() -> Self {
        Self::new()
    }
}

impl Ui for StdoutUi {
    fn reasoning(&mut self, text: &str) {
        if !self.in_reasoning {
            print!("{DIM}");
            self.in_reasoning = true;
        }
        print!("{text}");
        io::stdout().flush().ok();
    }
    fn content(&mut self, text: &str) {
        self.end_reasoning();
        print!("{text}");
        io::stdout().flush().ok();
    }
    fn tool_start(&mut self, name: &str, arguments: &str) {
        self.end_reasoning();
        println!("{DIM}⚙ {name} {arguments}{RESET}");
    }
    fn tool_end(&mut self, _name: &str, result: &str, ok: bool) {
        let mark = if ok { "✓" } else { "✗" };
        let preview: String = result.chars().take(200).collect();
        println!("{DIM}{mark} {preview}{RESET}");
    }
    fn turn_end(&mut self) {
        self.end_reasoning();
        println!();
    }
    fn info(&mut self, text: &str) {
        self.end_reasoning();
        println!("{text}");
    }
    fn ask_approval(&mut self, tool: &str, arguments: &str) -> Approval {
        self.end_reasoning();
        let args: String = arguments.chars().take(200).collect();
        print!("allow tool '{tool}' {args}? [y]once / [a]lways / [n]o: ");
        io::stdout().flush().ok();
        let mut line = String::new();
        if io::stdin().read_line(&mut line).is_err() {
            return Approval::Deny;
        }
        match line.trim().chars().next() {
            Some('y') | Some('Y') => Approval::Once,
            Some('a') | Some('A') => Approval::Always,
            _ => Approval::Deny,
        }
    }
    fn notice(&mut self, text: &str) {
        eprintln!("{text}");
    }
}

/// A minimal stdin REPL driving a [`Session`] with [`StdoutUi`].
///
/// Used for headless runs and until the inline TUI (M2) lands.
pub fn repl(mut session: Session) -> Result<()> {
    eprintln!(
        "atelier — model {} @ {}",
        session.cfg.model, session.cfg.base_url
    );
    eprintln!("type a message, or /help for commands.\n");
    for line in session.connect_configured_mcp() {
        eprintln!("{line}");
    }

    let stdin = io::stdin();
    loop {
        print!("› ");
        io::stdout().flush().ok();
        let mut input = String::new();
        if stdin.read_line(&mut input)? == 0 {
            break; // EOF (Ctrl-D)
        }
        let line = input.trim();
        if line.is_empty() {
            continue;
        }
        let mut ui = StdoutUi::new();
        match dispatch(&mut session, line, &mut ui) {
            Dispatch::Quit => break,
            Dispatch::Handled => {}
            Dispatch::Prompt => {
                if let Err(e) = session.send(line, &mut ui) {
                    eprintln!("error: {e:#}");
                } else if let Some(s) = session.usage_summary() {
                    eprintln!("{DIM}[{s}]{RESET}");
                }
            }
        }
    }
    Ok(())
}
