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

use anyhow::{Context, Result};
use serde_json::Value;

use crate::config::Config;
use crate::context::{ContextItem, ContextProvider};
use crate::mcp::{self, HttpServer, StdioServer};
use crate::provider::{self, Completion, Message, StreamEvent, ToolCall};
use crate::settings::{HttpServerConfig, McpServerConfig, Settings};
use crate::tools::{FileState, ToolCtx, ToolRegistry};

const SYSTEM_PROMPT: &str = "\
You are atelier, an AI coding assistant operating inside a project directory. \
You have tools to read and modify files and run commands; use them to inspect \
the project and make changes rather than guessing. Keep responses concise. \
When you have completed the request, stop calling tools and give a short final \
answer.";

/// Token budget for per-turn injected context (git status, diff, layout,
/// diagnostics), prioritized and truncated to fit.
const CONTEXT_TOKEN_BUDGET: usize = 2_000;

/// Default context-size threshold (last request's total tokens) past which older
/// history is compacted into a summary. Override with `ATELIER_CONTEXT_LIMIT`.
const DEFAULT_CONTEXT_LIMIT: u32 = 8_000;

/// When compacting, keep at least this many of the most recent messages intact.
const COMPACT_KEEP_RECENT: usize = 6;

/// Don't bother compacting until the history has at least this many messages.
const COMPACT_MIN_MESSAGES: usize = 10;

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
    "  /mcp add <name> <command> [args...]   add a stdio MCP server",
    "  /mcp add <name> <http(s)://url> [H: V] add an HTTP MCP server",
    "  /mcp remove <name>                    remove an MCP server",
    "  /new                                  start a fresh conversation (clear saved session)",
    "  /image <path>                         attach an image to your next message (vision)",
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
        "/new" => {
            session.new_conversation();
            ui.info("started a new conversation (previous session cleared)");
        }
        "/image" => match args.first() {
            Some(path) => match session.stage_image(path) {
                Ok(n) => ui.info(&format!(
                    "attached image ({n} staged) — it will be sent with your next message"
                )),
                Err(e) => ui.info(&format!("error: {e:#}")),
            },
            None => ui.info("usage: /image <path>"),
        },
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
                ui.info("       /mcp add <name> <http(s)://url> [Header: Value ...]");
                return;
            }
            let name = rest[0];
            let target = rest[1];
            // A URL second argument means an HTTP (Streamable) server; otherwise
            // it's a command to launch over stdio.
            let result = if target.starts_with("http://") || target.starts_with("https://") {
                let headers: Vec<String> = rest[2..].iter().map(|s| s.to_string()).collect();
                session.mcp_add_http(name, target, headers)
            } else {
                let extra: Vec<String> = rest[2..].iter().map(|s| s.to_string()).collect();
                session.mcp_add(name, target, extra)
            };
            match result {
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
    /// Images (data URLs) staged by `/image`, attached to the next user turn.
    pending_images: Vec<String>,
    /// Rolling summary of older turns that have been compacted out of `history`.
    summary: Option<String>,
    /// Compact when the last request's total tokens exceed this.
    compact_threshold: u32,
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
        let compact_threshold = std::env::var("ATELIER_CONTEXT_LIMIT")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(DEFAULT_CONTEXT_LIMIT);
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
            pending_images: Vec::new(),
            summary: None,
            compact_threshold,
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

    /// Load a previously-saved conversation for this project (for `--continue`).
    /// Returns the number of messages restored.
    pub fn resume(&mut self) -> usize {
        let data = crate::session::load(&self.root);
        self.summary = data.summary;
        self.history = data.messages;
        self.history.len()
    }

    /// Start a fresh conversation, discarding history and the saved session.
    pub fn new_conversation(&mut self) {
        self.history.clear();
        self.summary = None;
        self.pending_images.clear();
        let _ = crate::session::clear(&self.root);
    }

    /// Stage an image file to attach to the next user message (for `/image`).
    /// Returns the number of images now staged. The path is the user's own, so
    /// it isn't confined to the project root.
    pub fn stage_image(&mut self, path: &str) -> Result<usize> {
        let bytes = std::fs::read(path).with_context(|| format!("reading image {path:?}"))?;
        let mime = image_mime(path);
        let data_url = format!("data:{mime};base64,{}", base64_encode(&bytes));
        self.pending_images.push(data_url);
        Ok(self.pending_images.len())
    }

    /// Persist the current conversation (summary + history) to disk (best-effort).
    fn persist(&self) {
        let _ = crate::session::save(&self.root, self.summary.as_deref(), &self.history);
    }

    /// Connect every MCP server in the settings and register its tools.
    /// Returns human-readable status lines (one per server) for the caller to
    /// display; a failed server is reported but does not abort the others.
    pub fn connect_configured_mcp(&mut self) -> Vec<String> {
        let mut status = Vec::new();
        for s in self.settings.mcp.clone() {
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
        for s in self.settings.mcp_http.clone() {
            match mcp::connect_http(&to_http(&s)) {
                Ok(tools) => {
                    let n = tools.len();
                    for t in tools {
                        self.tools.register(t);
                    }
                    status.push(format!("mcp: connected '{}' (http, {n} tool(s))", s.name));
                }
                Err(e) => status.push(format!("mcp: failed to connect '{}': {e:#}", s.name)),
            }
        }
        status
    }

    /// One display line per configured MCP server (stdio and HTTP).
    pub fn mcp_list(&self) -> Vec<String> {
        let stdio = self.settings.mcp.iter().map(|m| {
            if m.args.is_empty() {
                format!("{} → {}", m.name, m.command)
            } else {
                format!("{} → {} {}", m.name, m.command, m.args.join(" "))
            }
        });
        let http = self
            .settings
            .mcp_http
            .iter()
            .map(|m| format!("{} → {} (http)", m.name, m.url));
        stdio.chain(http).collect()
    }

    /// Whether any configured MCP server (stdio or HTTP) already uses `name`.
    fn mcp_name_taken(&self, name: &str) -> bool {
        self.settings.mcp.iter().any(|m| m.name == name)
            || self.settings.mcp_http.iter().any(|m| m.name == name)
    }

    /// Connect a new stdio MCP server, register its tools, and persist it.
    pub fn mcp_add(&mut self, name: &str, command: &str, args: Vec<String>) -> Result<usize> {
        if self.mcp_name_taken(name) {
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

    /// Connect a new HTTP (Streamable) MCP server, register its tools, persist
    /// it. `headers` are `"Name: Value"` strings sent on every request.
    pub fn mcp_add_http(&mut self, name: &str, url: &str, headers: Vec<String>) -> Result<usize> {
        if self.mcp_name_taken(name) {
            anyhow::bail!("an MCP server named '{name}' already exists");
        }
        let cfg = HttpServerConfig {
            name: name.to_string(),
            url: url.to_string(),
            headers,
        };
        let tools = mcp::connect_http(&to_http(&cfg))?;
        let n = tools.len();
        for t in tools {
            self.tools.register(t);
        }
        self.settings.mcp_http.push(cfg);
        self.settings.save(&self.root)?;
        Ok(n)
    }

    /// Remove a configured MCP server (stdio or HTTP) and drop its live tools.
    /// Returns whether a server by that name existed.
    pub fn mcp_remove(&mut self, name: &str) -> Result<bool> {
        let before = self.settings.mcp.len() + self.settings.mcp_http.len();
        self.settings.mcp.retain(|m| m.name != name);
        self.settings.mcp_http.retain(|m| m.name != name);
        if self.settings.mcp.len() + self.settings.mcp_http.len() == before {
            return Ok(false);
        }
        self.tools.remove_prefix(&format!("mcp__{name}__"));
        self.settings.save(&self.root)?;
        Ok(true)
    }

    /// Run one user turn to completion, executing tool calls until the model
    /// stops requesting them. Progress is reported through `ui`.
    pub fn send(&mut self, user_input: &str, ui: &mut dyn Ui) -> Result<()> {
        if self.pending_images.is_empty() {
            self.history.push(Message::user(user_input));
        } else {
            let images = std::mem::take(&mut self.pending_images);
            self.history
                .push(Message::user_with_images(user_input, images));
        }
        let context_msg = self.gather_context();

        loop {
            // Assemble the request: a single leading system message (prompt +
            // fresh context — some servers reject a second system message), then
            // the conversation history.
            let mut messages = Vec::with_capacity(self.history.len() + 1);
            // One leading system message (some servers reject a second): the
            // prompt, then the rolling summary of compacted turns, then fresh
            // per-turn context.
            let mut system = SYSTEM_PROMPT.to_string();
            if let Some(s) = &self.summary {
                system.push_str("\n\nSummary of earlier conversation:\n");
                system.push_str(s);
            }
            if let Some(ctx) = &context_msg {
                system.push_str("\n\n");
                system.push_str(ctx);
            }
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
                self.maybe_compact(ui);
                self.persist();
                return Ok(());
            }

            for call in &completion.tool_calls {
                let call_args: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
                if self.needs_approval(&call.name, &call_args) {
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
    fn needs_approval(&self, name: &str, args: &Value) -> bool {
        if self.auto_approve || self.allow.contains(name) {
            return false;
        }
        self.tools
            .get(name)
            .map(|t| t.requires_approval(args))
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

    /// If the context has grown past the threshold, summarize the older part of
    /// the history into the rolling summary and drop it from `history`.
    fn maybe_compact(&mut self, ui: &mut dyn Ui) {
        if self.usage_ctx <= self.compact_threshold || self.history.len() < COMPACT_MIN_MESSAGES {
            return;
        }
        let Some(split) = self.compact_split_point() else {
            return;
        };
        let excerpt = render_excerpt(self.summary.as_deref(), &self.history[..split]);
        match self.summarize(&excerpt) {
            Ok(summary) => {
                self.summary = Some(summary);
                self.history.drain(0..split);
                // Avoid re-compacting immediately; the next real turn recomputes.
                self.usage_ctx = 0;
                ui.info(&format!(
                    "compacted {split} earlier message(s) into a summary"
                ));
            }
            Err(e) => ui.notice(&format!("compaction failed: {e:#}")),
        }
    }

    /// Choose where to split history for compaction: summarize everything before
    /// the returned index, keep the rest. The kept suffix must start at a `user`
    /// message so the request stays well-formed (no orphan tool result). Returns
    /// `None` if there's nothing sensible to compact.
    fn compact_split_point(&self) -> Option<usize> {
        split_point(&self.history, COMPACT_KEEP_RECENT)
    }

    /// Summarize a rendered conversation excerpt via the provider (no tools).
    fn summarize(&self, excerpt: &str) -> Result<String> {
        let messages = vec![
            Message::system(
                "You compress a conversation transcript into a concise summary. Preserve concrete \
                 facts, decisions, file paths touched, and any open tasks, as short bullet points. \
                 No preamble or commentary.",
            ),
            Message::user(format!("Summarize this conversation excerpt:\n\n{excerpt}")),
        ];
        let completion = provider::stream_chat(&self.cfg, &messages, &[], |_| {})?;
        let s = completion.content.trim().to_string();
        if s.is_empty() {
            anyhow::bail!("model returned an empty summary");
        }
        Ok(s)
    }

    /// Gather per-turn context into a single injected block, or `None`.
    ///
    /// Providers are prioritized and truncated to a token budget so a large diff
    /// or layout can't crowd out the conversation.
    fn gather_context(&self) -> Option<String> {
        let items: Vec<ContextItem> = self
            .context
            .iter()
            .filter_map(|p| p.gather(&self.root))
            .collect();
        crate::context::render_budgeted(items, CONTEXT_TOKEN_BUDGET)
    }
}

/// Pick a compaction split index: summarize `history[..i]`, keep `history[i..]`,
/// where `i` is the first `user` message at or after `len - keep` (so the kept
/// suffix starts cleanly and no tool result is orphaned). `None` if there's
/// nothing sensible to compact.
fn split_point(history: &[Message], keep: usize) -> Option<usize> {
    let n = history.len();
    let target = n.saturating_sub(keep);
    if target == 0 {
        return None;
    }
    let mut i = target;
    while i < n && history[i].role != "user" {
        i += 1;
    }
    if i == 0 || i >= n { None } else { Some(i) }
}

/// Render history messages (and any prior summary) into a plain-text excerpt for
/// summarization.
fn render_excerpt(prev_summary: Option<&str>, msgs: &[Message]) -> String {
    let mut s = String::new();
    if let Some(p) = prev_summary {
        s.push_str("Earlier summary:\n");
        s.push_str(p);
        s.push_str("\n\n");
    }
    for m in msgs {
        if !m.content.is_empty() {
            s.push_str(&format!("{}: {}\n", m.role, m.content));
        }
        for c in &m.tool_calls {
            s.push_str(&format!("assistant called {}({})\n", c.name, c.arguments));
        }
    }
    s
}

/// Guess an image MIME type from a file extension (defaults to PNG).
fn image_mime(path: &str) -> &'static str {
    match path
        .rsplit('.')
        .next()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        _ => "image/png",
    }
}

/// Standard base64 encoding (no line breaks). Small hand-rolled encoder to avoid
/// a dependency, used for `data:` image URLs.
fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn to_stdio(cfg: &McpServerConfig) -> StdioServer {
    StdioServer {
        name: cfg.name.clone(),
        command: cfg.command.clone(),
        args: cfg.args.clone(),
    }
}

fn to_http(cfg: &HttpServerConfig) -> HttpServer {
    let headers = cfg
        .headers
        .iter()
        .filter_map(|h| h.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    HttpServer {
        name: cfg.name.clone(),
        url: cfg.url.clone(),
        headers,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_point_finds_user_boundary() {
        // 12 messages, alternating user/assistant. keep=6 -> target index 6,
        // which is a user message (even indices are users here).
        let mut h = Vec::new();
        for i in 0..12 {
            h.push(if i % 2 == 0 {
                Message::user(format!("u{i}"))
            } else {
                Message::assistant(format!("a{i}"))
            });
        }
        let i = split_point(&h, 6).expect("should find a split");
        assert_eq!(h[i].role, "user");
        assert!((6..12).contains(&i));
    }

    #[test]
    fn split_point_advances_past_non_user() {
        // target lands on an assistant/tool run; must advance to the next user.
        let h = vec![
            Message::user("u0"),
            Message::assistant("a1"),
            Message::assistant("a2"), // target (keep=2 -> target 1) region is non-user
            Message::user("u3"),
        ];
        let i = split_point(&h, 2).expect("split");
        assert_eq!(h[i].role, "user");
        assert_eq!(i, 3);
    }

    #[test]
    fn split_point_none_when_short() {
        let h = vec![Message::user("u0"), Message::assistant("a1")];
        assert!(split_point(&h, 6).is_none());
    }

    #[test]
    fn split_point_none_when_no_user_in_tail() {
        let h = vec![
            Message::user("u0"),
            Message::assistant("a1"),
            Message::assistant("a2"),
        ];
        // keep=1 -> target 2 (assistant), no user after -> None.
        assert!(split_point(&h, 1).is_none());
    }

    #[test]
    fn render_excerpt_includes_summary_and_roles() {
        let h = vec![Message::user("hi"), Message::assistant("hello")];
        let s = render_excerpt(Some("prior summary"), &h);
        assert!(s.contains("Earlier summary:\nprior summary"));
        assert!(s.contains("user: hi"));
        assert!(s.contains("assistant: hello"));
    }

    #[test]
    fn base64_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
    }

    #[test]
    fn image_mime_from_extension() {
        assert_eq!(image_mime("a.jpg"), "image/jpeg");
        assert_eq!(image_mime("a.JPEG"), "image/jpeg");
        assert_eq!(image_mime("a.gif"), "image/gif");
        assert_eq!(image_mime("a.png"), "image/png");
        assert_eq!(image_mime("noext"), "image/png");
    }
}
