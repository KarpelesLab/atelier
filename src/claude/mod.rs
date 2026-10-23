//! Claude backend: atelier as a frontend for the user's own Claude Code CLI.
//!
//! In Claude mode atelier doesn't call a model API itself. It launches the
//! `claude` binary the user has already installed and signed in to —
//! unmodified — in headless stream-json mode, and acts as its interface:
//! each user turn is written to the child's stdin as a JSON line, and the
//! child's event stream (text/thinking deltas, tool calls and results,
//! permission requests, the end-of-turn result) is rendered through the same
//! [`Ui`] as the OpenAI-compatible loop. atelier never sees credentials;
//! authentication is whatever the user's `claude` is configured with.
//!
//! The agent loop, built-in tools and compaction are Claude Code's. atelier
//! contributes its configured MCP servers (`--mcp-config`), its per-turn
//! project context (prepended to each user turn), and the UI — including
//! permission prompts, which arrive as `can_use_tool` control requests
//! (`--permission-prompt-tool stdio`) and are answered through
//! [`Ui::ask_approval`].
//!
//! One child process lives for the whole conversation (stream-json input
//! accepts any number of turns). It is restarted with `--resume <session>`
//! when it has to be relaunched (MCP config changed, or it died).
//!
//! # Turns, background work, and stopping
//!
//! Background subagents and shells live inside the child, and finishing ones
//! make claude run turns of its own (to report back) whenever it likes — so
//! the output stream is read continuously by a thread, a turn is tracked by
//! the `uuid` atelier gives its user message (`command_lifecycle` events say
//! when that message's turn has completed), and output arriving between
//! turns is rendered by [`Bridge::pump`].
//!
//! **Background work is never killed unless the user explicitly asks.** That
//! rules out claude's own `interrupt` request, which stops background agents
//! along with the turn. Ctrl-C instead *preempts* the turn: atelier queues a
//! short stop message and asks claude to deliver it now (`interrupt` with
//! `send_now`), which ends the running turn the way a newly typed message
//! does and leaves background work alone. The same rule is why the process
//! is only ever dropped (which ends everything in it) with no background
//! tasks running, or on an explicit `/new force` / repeated quit.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::agent::{Approval, Ui};
use crate::provider::ToolSpec;
use crate::settings::Settings;

#[cfg(unix)]
mod host;
#[cfg(unix)]
pub use host::relay;

/// Appended to Claude Code's own system prompt so it knows about the
/// context block atelier prepends to user turns.
const APPEND_SYSTEM_PROMPT: &str = "\
The user is talking to you through atelier, a terminal frontend. A user \
message may begin with an <atelier-context> block that atelier gathered \
automatically just before sending (git status, diff, project layout, build \
diagnostics). Treat it as the current state of the project; the user did \
not type it. atelier's own tools are available as `mcp__atelier__*` (file \
tools confined to the project, with read-before-edit tracking; a sandboxed \
`node` scripting tool; and more); use them where they fit.";

/// Delivered (preempting the running turn) when the user presses Ctrl-C.
const STOP_MESSAGE: &str = "\
[atelier] The user pressed Ctrl-C to stop this turn. Stop here: do not \
continue the interrupted work or start anything new. Background agents and \
tasks are still running; leave them alone. Reply with one short line saying \
where you stopped.";

/// Keep this much of the child's stderr for error messages.
const STDERR_TAIL: usize = 4096;

/// How often a waiting turn checks whether the user asked to stop.
const POLL: Duration = Duration::from_millis(50);

/// How to launch the CLI.
pub struct Launch<'a> {
    /// The `claude` executable.
    pub bin: &'a str,
    /// `--model`, when the user picked one; otherwise Claude Code's default.
    pub model: Option<&'a str>,
    /// Claude session to continue (`--resume`).
    pub resume: Option<&'a str>,
}

/// A running `claude` child speaking stream-json.
pub struct Bridge {
    child: Child,
    stdin: Option<ChildStdin>,
    /// Everything the event loop acts on: the child's output lines (read
    /// continuously by a thread) and calls to atelier's tools (from the MCP
    /// host).
    incoming: Receiver<Incoming>,
    /// Serves atelier's tools to claude; `None` when there are none to offer
    /// (or no Unix sockets).
    #[cfg(unix)]
    _host: Option<host::Host>,
    stderr: Arc<Mutex<String>>,
    /// Claude's session id, from the `init` event (stable across turns).
    session_id: Option<String>,
    /// Model actually serving the session, from the `init` event.
    model: Option<String>,
    /// tool_use id -> tool name, so results can be labeled.
    tool_names: HashMap<String, String>,
    /// Background tasks (subagents, background shells, monitors) running in
    /// the child, per its latest `background_tasks_changed`.
    background: usize,
    /// Context size at the latest model request.
    context: u32,
    /// Output tokens not yet collected by [`Bridge::take_output`].
    output: u64,
}

/// Progress of the turn [`Bridge::turn`] is waiting on.
struct TurnState {
    /// uuid of the user message whose completion ends the turn: the user's
    /// own, or the stop message once Ctrl-C was pressed.
    awaiting: String,
    /// `awaiting` has started running (results seen before that belong to
    /// some other turn, e.g. one reporting a finished background agent).
    started: bool,
    /// Ctrl-C was pressed and the stop message sent.
    stopping: bool,
    /// The failure reported by the awaited turn's `result`, if any.
    error: Option<String>,
    done: bool,
}

/// What the bridge's event loop receives.
pub enum Incoming {
    /// One line of the child's stdout.
    Line(String),
    /// The child's stdout closed: it exited.
    Closed,
    /// claude called one of atelier's tools.
    #[cfg(unix)]
    Call(host::ToolRequest),
}

/// Runs atelier's tools for claude: the session's tool registry and approval
/// policy, driven with the interface of whichever loop receives the call.
pub trait ToolHost {
    /// Skip every approval prompt (claude's permission requests included).
    fn auto_approve(&self) -> bool;
    /// Run tool `name` (asking approval if its policy says so), returning
    /// `(result_text, ok)`.
    fn call(&mut self, name: &str, arguments: Value, ui: &mut dyn Ui) -> (String, bool);
}

/// How atelier's tools appear in claude: `mcp__atelier__<tool>`.
#[cfg(unix)]
const TOOL_PREFIX: &str = "mcp__atelier__";

impl Bridge {
    /// Spawn the CLI in `root`, wired to the MCP servers in `settings` and
    /// offering atelier's `tools`.
    pub fn spawn(
        launch: &Launch,
        root: &Path,
        settings: &Settings,
        tools: Vec<ToolSpec>,
    ) -> Result<Self> {
        let (tx, incoming) = mpsc::channel();
        let mut cmd = Command::new(launch.bin);
        cmd.args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--permission-prompt-tool",
            "stdio",
            "--append-system-prompt",
            APPEND_SYSTEM_PROMPT,
        ]);
        if let Some(m) = launch.model {
            cmd.args(["--model", m]);
        }
        if let Some(id) = launch.resume {
            cmd.args(["--resume", id]);
        }
        let mut mcp = mcp_config(settings);
        // atelier's own tools, unless a configured server already took the
        // name. claude may run them without asking: atelier applies its own
        // policy (confined file tools run, `bash` and the like ask).
        #[cfg(unix)]
        let host = if tools.is_empty()
            || mcp
                .as_ref()
                .is_some_and(|c| !c["mcpServers"][host::SERVER_NAME].is_null())
        {
            None
        } else {
            let h = host::Host::start(tools, tx.clone())?;
            mcp.get_or_insert_with(|| json!({ "mcpServers": {} }))["mcpServers"]
                [host::SERVER_NAME] = h.mcp_entry()?;
            cmd.args(["--allowedTools", &format!("mcp__{}", host::SERVER_NAME)]);
            Some(h)
        };
        #[cfg(not(unix))]
        let _ = tools;
        if let Some(cfg) = mcp {
            cmd.args(["--mcp-config", &cfg.to_string()]);
        }
        let mut child = cmd
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| {
                format!(
                    "launching '{}' (is Claude Code installed? set ATELIER_CLAUDE_BIN to its path)",
                    launch.bin
                )
            })?;
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().context("claude stdout")?);
        std::thread::spawn(move || {
            for line in stdout.lines() {
                let Ok(line) = line else { break };
                if tx.send(Incoming::Line(line)).is_err() {
                    return;
                }
            }
            let _ = tx.send(Incoming::Closed);
        });
        // Drain stderr in the background so the child can't block on a full
        // pipe; keep the tail for error reports.
        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(mut pipe) = child.stderr.take() {
            let sink = Arc::clone(&stderr);
            std::thread::spawn(move || {
                let mut buf = [0u8; 1024];
                while let Ok(n) = pipe.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let mut s = sink.lock().unwrap_or_else(|e| e.into_inner());
                    s.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if s.len() > STDERR_TAIL {
                        let cut = s.len() - STDERR_TAIL;
                        let cut = (cut..s.len()).find(|&i| s.is_char_boundary(i)).unwrap_or(0);
                        s.drain(..cut);
                    }
                }
            });
        }
        Ok(Self {
            child,
            stdin,
            incoming,
            #[cfg(unix)]
            _host: host,
            stderr,
            session_id: launch.resume.map(str::to_owned),
            model: None,
            tool_names: HashMap::new(),
            background: 0,
            context: 0,
            output: 0,
        })
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Background tasks currently running in the child.
    pub fn background(&self) -> usize {
        self.background
    }

    /// Context size (tokens) as of the latest model request.
    pub fn context(&self) -> u32 {
        self.context
    }

    /// Output tokens generated since the last call.
    pub fn take_output(&mut self) -> u64 {
        std::mem::take(&mut self.output)
    }

    /// Whether the child is still running.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Send one user turn (`content`: a string or an array of content blocks)
    /// and render the child's events until that turn completes.
    ///
    /// While waiting, `ui.interrupted()` is polled; when the user asks to
    /// stop, the turn is preempted (see the module docs) and this returns
    /// once claude has acknowledged the stop — without an error.
    pub fn turn(&mut self, content: Value, ui: &mut dyn Ui, host: &mut dyn ToolHost) -> Result<()> {
        let mut st = TurnState {
            awaiting: new_uuid(),
            started: false,
            stopping: false,
            error: None,
            done: false,
        };
        self.send_user(content, &st.awaiting)?;
        while !st.done {
            match self.incoming.recv_timeout(POLL) {
                Ok(inc) => self.receive(inc, ui, host, Some(&mut st))?,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => unreachable!("the bridge holds no sender"),
            }
            if !st.done && !st.stopping && ui.interrupted() {
                self.stop(&mut st, ui)?;
            }
        }
        ui.turn_end();
        match st.error {
            Some(e) => bail!("claude: {e}"),
            None => Ok(()),
        }
    }

    /// Render whatever the child produced while no turn was running — claude
    /// reporting back on background work. Returns whether anything arrived.
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    pub fn pump(&mut self, ui: &mut dyn Ui, host: &mut dyn ToolHost) -> Result<bool> {
        let mut any = false;
        loop {
            match self.incoming.try_recv() {
                Ok(inc) => {
                    any = true;
                    self.receive(inc, ui, host, None)?;
                }
                Err(TryRecvError::Empty) => return Ok(any),
                Err(TryRecvError::Disconnected) => unreachable!("the bridge holds no sender"),
            }
        }
    }

    /// Render output until no background task is running (and claude has
    /// finished reporting on them). For one-shot runs, which must not end —
    /// and take the child with them — while background work is in flight.
    pub fn wait_background(&mut self, ui: &mut dyn Ui, host: &mut dyn ToolHost) -> Result<()> {
        // Done once nothing is running and the stream has been quiet for a
        // moment: the report on a finished task is a turn of its own, and
        // that report can start more work (a subagent resumed by its own
        // finished monitor, say), so "count hit zero" alone isn't the end.
        let quiet = Duration::from_millis(1500);
        loop {
            let wait = if self.background > 0 { POLL } else { quiet };
            match self.incoming.recv_timeout(wait) {
                Ok(inc) => self.receive(inc, ui, host, None)?,
                Err(RecvTimeoutError::Timeout) if self.background == 0 => return Ok(()),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => unreachable!("the bridge holds no sender"),
            }
        }
    }

    /// Act on one incoming item; `turn` is the turn being waited on, if any.
    fn receive(
        &mut self,
        inc: Incoming,
        ui: &mut dyn Ui,
        host: &mut dyn ToolHost,
        mut turn: Option<&mut TurnState>,
    ) -> Result<()> {
        match inc {
            Incoming::Line(line) => {
                for ev in parse_line(&line) {
                    self.handle(ev, ui, host, turn.as_deref_mut())?;
                }
                Ok(())
            }
            Incoming::Closed => bail!("claude exited{}", self.stderr_note()),
            #[cfg(unix)]
            Incoming::Call(req) => {
                // The call already shows as claude's tool use; this just runs it.
                let _ = req.reply.send(host.call(&req.name, req.arguments, ui));
                Ok(())
            }
        }
    }

    /// Act on one event; `turn` is the turn being waited on, if any.
    fn handle(
        &mut self,
        ev: Event,
        ui: &mut dyn Ui,
        host: &mut dyn ToolHost,
        turn: Option<&mut TurnState>,
    ) -> Result<()> {
        match ev {
            Event::Init { session_id, model } => {
                self.session_id = Some(session_id);
                if model.is_some() {
                    self.model = model;
                }
            }
            Event::Text(t) => ui.content(&t),
            Event::Thinking(t) => ui.reasoning(&t),
            Event::ToolUse { id, name, input } => {
                // atelier's own tools read as themselves, not `mcp__atelier__…`.
                #[cfg(unix)]
                let name = name
                    .strip_prefix(TOOL_PREFIX)
                    .map_or(name.clone(), str::to_owned);
                ui.tool_start(&name, &input);
                self.tool_names.insert(id, name);
            }
            Event::ToolResult {
                id,
                content,
                is_error,
            } => {
                let name = self.tool_names.remove(&id).unwrap_or_default();
                ui.tool_end(&name, &content, !is_error);
            }
            Event::Context(n) => self.context = n,
            Event::Background(n) => self.background = n,
            Event::Permission(req) => {
                let answer = self.decide(&req, ui, host.auto_approve());
                self.write(&answer)?;
            }
            Event::Lifecycle { uuid, state } => {
                if let Some(st) = turn
                    && uuid == st.awaiting
                {
                    match state.as_str() {
                        "queued" => {}
                        "started" => st.started = true,
                        _ => st.done = true,
                    }
                }
            }
            Event::Result {
                is_error,
                text,
                output_tokens,
                empty,
            } => {
                self.output += output_tokens;
                match turn {
                    Some(st) if st.started => {
                        if is_error {
                            st.error = Some(text.unwrap_or_else(|| "turn failed".into()));
                        }
                    }
                    // Another turn ended: one we preempted, or claude
                    // reporting on background work. Separate it visually;
                    // the bare summary claude emits when everything has
                    // settled (`num_turns: 0`) has nothing to separate.
                    _ if !empty => ui.turn_end(),
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// Ctrl-C: preempt the running turn with the stop message. The awaited
    /// turn becomes the stop message's, so `turn` returns once claude has
    /// stopped and acknowledged.
    fn stop(&mut self, st: &mut TurnState, ui: &mut dyn Ui) -> Result<()> {
        let id = new_uuid();
        self.send_user(Value::from(STOP_MESSAGE), &id)?;
        self.write(&json!({
            "type": "control_request",
            "request_id": new_uuid(),
            "request": { "subtype": "interrupt", "send_now": true, "message_uuid": id },
        }))?;
        st.awaiting = id;
        st.started = false;
        st.stopping = true;
        st.error = None;
        if self.background > 0 {
            ui.info(&format!(
                "{} background task(s) keep running",
                self.background
            ));
        }
        Ok(())
    }

    fn send_user(&mut self, content: Value, uuid: &str) -> Result<()> {
        self.write(&json!({
            "type": "user",
            "uuid": uuid,
            "message": { "role": "user", "content": content },
            "parent_tool_use_id": null,
            "session_id": self.session_id.clone().unwrap_or_default(),
        }))
    }

    /// Answer a `can_use_tool` request.
    fn decide(&self, req: &PermissionRequest, ui: &mut dyn Ui, auto_approve: bool) -> Value {
        let args = req.input.to_string();
        let approval = if auto_approve {
            Approval::Once
        } else {
            // Claude's Bash takes the same `command` argument as ours.
            let risk_name = if req.tool == "Bash" {
                "bash"
            } else {
                req.tool.as_str()
            };
            for signal in crate::risk::signals(risk_name, &args) {
                ui.info(&format!("⚠ {signal}"));
            }
            ui.ask_approval(&req.tool, &args)
        };
        permission_response(&req.request_id, approval, &req.input, &req.suggestions)
    }

    fn write(&mut self, v: &Value) -> Result<()> {
        let stdin = self.stdin.as_mut().context("claude stdin closed")?;
        writeln!(stdin, "{v}")
            .and_then(|_| stdin.flush())
            .with_context(|| format!("writing to claude{}", self.stderr_note()))
    }

    fn stderr_note(&self) -> String {
        let s = self.stderr.lock().unwrap_or_else(|e| e.into_inner());
        let s = s.trim();
        if s.is_empty() {
            String::new()
        } else {
            format!(": {s}")
        }
    }
}

impl Drop for Bridge {
    /// Ends the child and everything running in it, background agents
    /// included. Callers only drop a bridge with no background work, or on
    /// the user's explicit request (see the module docs).
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A random (v4-format) uuid, without pulling in a crate: std's per-instance
/// random hasher keys, mixed with a counter and the clock.
fn new_uuid() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let word = || {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        h.write_u128(nanos);
        h.finish()
    };
    let n = (u128::from(word()) << 64) | u128::from(word());
    // Version 4, RFC 4122 variant.
    let n = (n & !(0xf << 76) | (0x4 << 76)) & !(0x3 << 62) | (0x2 << 62);
    let h = format!("{n:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// Build the `--mcp-config` JSON for atelier's configured servers, or `None`
/// if there are none.
fn mcp_config(settings: &Settings) -> Option<Value> {
    let mut servers = serde_json::Map::new();
    for s in &settings.mcp {
        servers.insert(
            s.name.clone(),
            json!({ "type": "stdio", "command": s.command, "args": s.args }),
        );
    }
    for s in &settings.mcp_http {
        let headers: serde_json::Map<String, Value> = s
            .headers
            .iter()
            .filter_map(|h| h.split_once(':'))
            .map(|(k, v)| (k.trim().to_string(), Value::from(v.trim())))
            .collect();
        servers.insert(
            s.name.clone(),
            json!({ "type": "http", "url": s.url, "headers": headers }),
        );
    }
    if servers.is_empty() {
        None
    } else {
        Some(json!({ "mcpServers": servers }))
    }
}

/// The user-message content for a turn: plain text, or image blocks followed
/// by the text when `images` (data URLs, as staged by `/image`) are attached.
pub fn user_content(text: &str, images: &[String]) -> Value {
    if images.is_empty() {
        return Value::from(text);
    }
    let mut blocks: Vec<Value> = images
        .iter()
        .filter_map(|url| {
            let (mime, data) = url.strip_prefix("data:")?.split_once(";base64,")?;
            Some(json!({
                "type": "image",
                "source": { "type": "base64", "media_type": mime, "data": data },
            }))
        })
        .collect();
    blocks.push(json!({ "type": "text", "text": text }));
    Value::Array(blocks)
}

/// The control response for a permission request.
fn permission_response(
    request_id: &str,
    approval: Approval,
    input: &Value,
    suggestions: &Value,
) -> Value {
    let response = match approval {
        Approval::Once => json!({ "behavior": "allow", "updatedInput": input }),
        Approval::Always => {
            let mut r = json!({ "behavior": "allow", "updatedInput": input });
            if suggestions.as_array().is_some_and(|a| !a.is_empty()) {
                r["updatedPermissions"] = suggestions.clone();
            }
            r
        }
        Approval::Deny => json!({
            "behavior": "deny",
            "message": "The user denied permission to run this tool.",
        }),
    };
    json!({
        "type": "control_response",
        "response": { "subtype": "success", "request_id": request_id, "response": response },
    })
}

#[derive(Debug, PartialEq)]
struct PermissionRequest {
    request_id: String,
    tool: String,
    input: Value,
    suggestions: Value,
}

/// What one line of the CLI's stream-json output means to the frontend.
#[derive(Debug, PartialEq)]
enum Event {
    Init {
        session_id: String,
        model: Option<String>,
    },
    Text(String),
    Thinking(String),
    ToolUse {
        id: String,
        name: String,
        input: String,
    },
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
    },
    /// Context size (tokens) as of the latest model request.
    Context(u32),
    /// Number of background tasks now running.
    Background(usize),
    Permission(PermissionRequest),
    /// A user message's progress: `queued`, `started`, then a final state
    /// (`completed`, or another when it didn't run to completion).
    Lifecycle {
        uuid: String,
        state: String,
    },
    Result {
        is_error: bool,
        text: Option<String>,
        output_tokens: u64,
        /// claude's closing summary once all work has settled
        /// (`num_turns: 0`), rather than the end of an actual turn.
        empty: bool,
    },
}

/// Parse one output line. Unknown or irrelevant lines yield nothing.
///
/// Only top-level activity is surfaced: events from inside a subagent
/// (`parent_tool_use_id` set) are skipped — the subagent shows as its one
/// `Task`/`Agent` tool call. Text is taken from the partial-message deltas;
/// the complete `assistant` message is used only for tool calls and usage, so
/// nothing is printed twice.
fn parse_line(line: &str) -> Vec<Event> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let s = |v: &Value| v.as_str().map(str::to_owned);
    if !v["parent_tool_use_id"].is_null() {
        return Vec::new();
    }
    match v["type"].as_str() {
        Some("system") if v["subtype"] == "init" => s(&v["session_id"])
            .map(|session_id| Event::Init {
                session_id,
                model: s(&v["model"]),
            })
            .into_iter()
            .collect(),
        Some("system") if v["subtype"] == "background_tasks_changed" => v["tasks"]
            .as_array()
            .map(|t| Event::Background(t.len()))
            .into_iter()
            .collect(),
        Some("command_lifecycle") => match (s(&v["command_uuid"]), s(&v["state"])) {
            (Some(uuid), Some(state)) => vec![Event::Lifecycle { uuid, state }],
            _ => Vec::new(),
        },
        Some("stream_event") => {
            let delta = &v["event"]["delta"];
            if v["event"]["type"] != "content_block_delta" {
                return Vec::new();
            }
            match delta["type"].as_str() {
                Some("text_delta") => s(&delta["text"]).map(Event::Text).into_iter().collect(),
                Some("thinking_delta") => s(&delta["thinking"])
                    .map(Event::Thinking)
                    .into_iter()
                    .collect(),
                _ => Vec::new(),
            }
        }
        Some("assistant") => {
            let msg = &v["message"];
            let mut out: Vec<Event> = blocks(&msg["content"])
                .filter(|b| b["type"] == "tool_use")
                .filter_map(|b| {
                    Some(Event::ToolUse {
                        id: s(&b["id"])?,
                        name: s(&b["name"])?,
                        input: b["input"].to_string(),
                    })
                })
                .collect();
            let u = &msg["usage"];
            let ctx = [
                "input_tokens",
                "cache_creation_input_tokens",
                "cache_read_input_tokens",
            ]
            .iter()
            .filter_map(|k| u[*k].as_u64())
            .sum::<u64>();
            if ctx > 0 {
                out.push(Event::Context(u32::try_from(ctx).unwrap_or(u32::MAX)));
            }
            out
        }
        Some("user") => blocks(&v["message"]["content"])
            .filter(|b| b["type"] == "tool_result")
            .filter_map(|b| {
                Some(Event::ToolResult {
                    id: s(&b["tool_use_id"])?,
                    content: result_text(&b["content"]),
                    is_error: b["is_error"].as_bool().unwrap_or(false),
                })
            })
            .collect(),
        Some("control_request") if v["request"]["subtype"] == "can_use_tool" => {
            let r = &v["request"];
            let (Some(request_id), Some(tool)) = (s(&v["request_id"]), s(&r["tool_name"])) else {
                return Vec::new();
            };
            vec![Event::Permission(PermissionRequest {
                request_id,
                tool,
                input: r["input"].clone(),
                suggestions: r["permission_suggestions"].clone(),
            })]
        }
        Some("result") => vec![Event::Result {
            is_error: v["is_error"].as_bool().unwrap_or(false),
            text: s(&v["result"]).or_else(|| s(&v["subtype"])),
            output_tokens: v["usage"]["output_tokens"].as_u64().unwrap_or(0),
            empty: v["num_turns"].as_u64() == Some(0),
        }],
        _ => Vec::new(),
    }
}

fn blocks(content: &Value) -> impl Iterator<Item = &Value> {
    content.as_array().into_iter().flatten()
}

/// A tool result's content as text: either a plain string or an array of
/// content blocks, of which the text ones are joined (images are noted).
fn result_text(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    blocks(content)
        .map(|b| match b["type"].as_str() {
            Some("text") => b["text"].as_str().unwrap_or_default().to_string(),
            Some(other) => format!("[{other}]"),
            None => String::new(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{HttpServerConfig, McpServerConfig};

    #[test]
    fn parses_init() {
        let ev = parse_line(
            r#"{"type":"system","subtype":"init","session_id":"abc","model":"claude-haiku-5-5","tools":[]}"#,
        );
        assert_eq!(
            ev,
            vec![Event::Init {
                session_id: "abc".into(),
                model: Some("claude-haiku-5-5".into())
            }]
        );
    }

    #[test]
    fn parses_text_and_thinking_deltas() {
        let text = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}},"parent_tool_use_id":null}"#;
        assert_eq!(parse_line(text), vec![Event::Text("hi".into())]);
        let think = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}},"parent_tool_use_id":null}"#;
        assert_eq!(parse_line(think), vec![Event::Thinking("hmm".into())]);
        let json = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{"}},"parent_tool_use_id":null}"#;
        assert!(parse_line(json).is_empty());
    }

    #[test]
    fn skips_subagent_events() {
        let text = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}},"parent_tool_use_id":"toolu_1"}"#;
        assert!(parse_line(text).is_empty());
    }

    #[test]
    fn assistant_message_yields_tool_uses_and_context_not_text() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"x"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}],"usage":{"input_tokens":2,"cache_creation_input_tokens":10,"cache_read_input_tokens":100,"output_tokens":5}},"parent_tool_use_id":null}"#;
        assert_eq!(
            parse_line(line),
            vec![
                Event::ToolUse {
                    id: "t1".into(),
                    name: "Bash".into(),
                    input: r#"{"command":"ls"}"#.into()
                },
                Event::Context(112),
            ]
        );
    }

    #[test]
    fn parses_tool_results_string_and_blocks() {
        let line = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"t1","type":"tool_result","content":"ok","is_error":false},{"tool_use_id":"t2","type":"tool_result","content":[{"type":"text","text":"a"},{"type":"image"}],"is_error":true}]},"parent_tool_use_id":null}"#;
        assert_eq!(
            parse_line(line),
            vec![
                Event::ToolResult {
                    id: "t1".into(),
                    content: "ok".into(),
                    is_error: false
                },
                Event::ToolResult {
                    id: "t2".into(),
                    content: "a\n[image]".into(),
                    is_error: true
                },
            ]
        );
    }

    #[test]
    fn parses_permission_request() {
        let line = r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Write","input":{"file_path":"/x"},"permission_suggestions":[{"type":"setMode","mode":"acceptEdits","destination":"session"}],"tool_use_id":"t1"}}"#;
        let mut evs = parse_line(line);
        let (Some(Event::Permission(req)), None) = (evs.pop(), evs.pop()) else {
            panic!("expected one permission event");
        };
        assert_eq!(req.request_id, "r1");
        assert_eq!(req.tool, "Write");
        assert_eq!(req.input, json!({"file_path": "/x"}));
        assert!(req.suggestions.is_array());
    }

    #[test]
    fn parses_result() {
        let ok = r#"{"type":"result","subtype":"success","is_error":false,"result":"done","num_turns":2,"usage":{"output_tokens":42}}"#;
        assert_eq!(
            parse_line(ok),
            vec![Event::Result {
                is_error: false,
                text: Some("done".into()),
                output_tokens: 42,
                empty: false,
            }]
        );
        let err = r#"{"type":"result","subtype":"error_max_turns","is_error":true}"#;
        assert_eq!(
            parse_line(err),
            vec![Event::Result {
                is_error: true,
                text: Some("error_max_turns".into()),
                output_tokens: 0,
                empty: false,
            }]
        );
        let settled = r#"{"type":"result","subtype":"success","is_error":false,"num_turns":0}"#;
        assert!(matches!(
            parse_line(settled)[..],
            [Event::Result { empty: true, .. }]
        ));
    }

    #[test]
    fn parses_lifecycle_and_background() {
        let lc = r#"{"type":"command_lifecycle","command_uuid":"u1","state":"started","uuid":"x","session_id":"s"}"#;
        assert_eq!(
            parse_line(lc),
            vec![Event::Lifecycle {
                uuid: "u1".into(),
                state: "started".into()
            }]
        );
        let bg = r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"a"},{"task_id":"b"}]}"#;
        assert_eq!(parse_line(bg), vec![Event::Background(2)]);
        let none = r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#;
        assert_eq!(parse_line(none), vec![Event::Background(0)]);
    }

    #[test]
    fn uuids_are_v4_and_distinct() {
        let a = new_uuid();
        let b = new_uuid();
        assert_ne!(a, b);
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            [8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('4'));
        assert!(matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'));
    }

    #[test]
    fn ignores_garbage_and_unknown() {
        assert!(parse_line("not json").is_empty());
        assert!(parse_line(r#"{"type":"rate_limit_event"}"#).is_empty());
    }

    #[test]
    fn permission_responses() {
        let input = json!({"command": "ls"});
        let sugg = json!([{"type": "setMode", "mode": "acceptEdits", "destination": "session"}]);
        let once = permission_response("r1", Approval::Once, &input, &sugg);
        assert_eq!(once["type"], "control_response");
        assert_eq!(once["response"]["request_id"], "r1");
        assert_eq!(once["response"]["response"]["behavior"], "allow");
        assert_eq!(once["response"]["response"]["updatedInput"], input);
        assert!(
            once["response"]["response"]
                .get("updatedPermissions")
                .is_none()
        );

        let always = permission_response("r1", Approval::Always, &input, &sugg);
        assert_eq!(always["response"]["response"]["updatedPermissions"], sugg);

        let deny = permission_response("r1", Approval::Deny, &input, &sugg);
        assert_eq!(deny["response"]["response"]["behavior"], "deny");
    }

    #[test]
    fn user_content_with_and_without_images() {
        assert_eq!(user_content("hi", &[]), json!("hi"));
        let c = user_content("look", &["data:image/png;base64,AAAA".into()]);
        assert_eq!(
            c,
            json!([
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
                {"type": "text", "text": "look"},
            ])
        );
    }

    #[test]
    fn mcp_config_from_settings() {
        assert!(mcp_config(&Settings::default()).is_none());
        let mut s = Settings::default();
        s.mcp.push(McpServerConfig {
            name: "fs".into(),
            command: "srv".into(),
            args: vec!["-x".into()],
        });
        s.mcp_http.push(HttpServerConfig {
            name: "web".into(),
            url: "https://e.x/mcp".into(),
            headers: vec!["Authorization: Bearer t".into()],
        });
        let c = mcp_config(&s).unwrap();
        assert_eq!(
            c["mcpServers"]["fs"],
            json!({"type": "stdio", "command": "srv", "args": ["-x"]})
        );
        assert_eq!(
            c["mcpServers"]["web"],
            json!({"type": "http", "url": "https://e.x/mcp", "headers": {"Authorization": "Bearer t"}})
        );
    }

    /// Records what a turn rendered; asks to stop once enough answer text
    /// has streamed.
    #[derive(Default)]
    struct LiveUi {
        text: String,
        tools: Vec<String>,
        stop_after: Option<usize>,
    }

    impl Ui for LiveUi {
        fn reasoning(&mut self, _: &str) {}
        fn content(&mut self, t: &str) {
            self.text.push_str(t);
        }
        fn tool_start(&mut self, name: &str, _: &str) {
            self.tools.push(name.to_string());
        }
        fn tool_end(&mut self, _: &str, _: &str, _: bool) {}
        fn info(&mut self, _: &str) {}
        fn ask_approval(&mut self, _: &str, _: &str) -> Approval {
            Approval::Once
        }
        fn notice(&mut self, _: &str) {}
        fn interrupted(&mut self) -> bool {
            self.stop_after.is_some_and(|n| self.text.len() > n)
        }
    }

    /// Auto-approves, offers no tools.
    struct NoTools;

    impl ToolHost for NoTools {
        fn auto_approve(&self) -> bool {
            true
        }
        fn call(&mut self, name: &str, _: Value, _: &mut dyn Ui) -> (String, bool) {
            panic!("unexpected tool call: {name}")
        }
    }

    /// The property Ctrl-C exists for: stopping a turn leaves background
    /// agents running. (claude's own `interrupt` would kill them; it drops
    /// the background count to 0 before the interrupted turn's result.)
    #[test]
    #[ignore = "drives the real claude CLI on your account; run with --ignored"]
    fn live_stop_spares_background_agents() {
        let dir = std::env::temp_dir().join(format!("atelier-live-{}", new_uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        let launch = Launch {
            bin: "claude",
            model: Some("haiku"),
            resume: None,
        };
        let mut b = Bridge::spawn(&launch, &dir, &Settings::default(), Vec::new()).unwrap();
        let mut ui = LiveUi {
            stop_after: Some(300),
            ..LiveUi::default()
        };
        b.turn(
            json!(
                "Do these two things in order. 1) Launch a subagent with the Agent tool and \
                 run_in_background=true; its task: 'Use the Monitor tool to wait until 20 \
                 seconds have passed (until-loop on a timestamp), then report the word \
                 FINISHED.' 2) Then, without using any tools, write a detailed 1500-word \
                 essay about the history of lighthouses."
            ),
            &mut ui,
            &mut NoTools,
        )
        .unwrap();
        assert!(
            ui.tools.iter().any(|t| t == "Agent"),
            "tools: {:?}",
            ui.tools
        );
        // Stopped well short of 1500 words (the stop turn's reply included).
        assert!(ui.text.len() < 4000, "turn ran on: {} bytes", ui.text.len());
        assert!(
            b.background() > 0,
            "background agent did not survive the stop"
        );

        // The agent then finishes on its own, and the session keeps working.
        b.wait_background(&mut ui, &mut NoTools).unwrap();
        assert_eq!(b.background(), 0);
        let mut after = LiveUi::default();
        b.turn(json!("Reply with exactly: ok"), &mut after, &mut NoTools)
            .unwrap();
        assert!(after.text.to_lowercase().contains("ok"), "{:?}", after.text);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
