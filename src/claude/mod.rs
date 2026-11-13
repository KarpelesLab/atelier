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
//! (`--permission-prompt-tool stdio`).
//!
//! # Permission requests
//!
//! A request for one of claude's own tools is judged like a call to
//! atelier's: its name and arguments are mapped to the tool the permission
//! rules know ([`rule_subject`]: `Bash` is `bash`, `Edit` is `edit`, …) and
//! put to the session's rules and mode ([`ToolHost::permit`], see
//! [`crate::permissions`]). So a `deny = ["bash(rm *)"]` stops claude's
//! `Bash` as it stops atelier's, plan mode refuses claude's `Write`, an
//! allow rule answers without a prompt, and "always" saves an atelier rule.
//! Only what the rules leave open reaches [`Ui::ask_approval`].
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
//! Messages the user types while a turn runs ([`Ui::take_queued`]) are handed
//! to claude while a tool is running: claude delivers queued messages at the
//! turn's next step, after the tool results, the way its own TUI does. Until
//! then they stay in atelier's queue, where Ctrl-C can still drop them (once
//! handed over, a stop delivers them along with the stop message). The turn
//! ends when every message sent during it has completed.
//!
//! **Background work is never killed unless the user explicitly asks.** That
//! rules out claude's own `interrupt` request, which stops background agents
//! along with the turn. Ctrl-C instead *preempts* the turn: atelier queues a
//! short stop message and asks claude to deliver it now (`interrupt` with
//! `send_now`), which ends the running turn the way a newly typed message
//! does and leaves background work alone. The same rule is why the process
//! is only ever dropped (which ends everything in it) with no background
//! tasks running, or on an explicit `/new force` / repeated quit.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::agent::Ui;
use crate::permissions;
use crate::provider::ToolSpec;
use crate::settings::Settings;

#[cfg(unixlike)]
mod host;
#[cfg(unixlike)]
pub use host::relay;

/// Appended to Claude Code's own system prompt so it knows about the
/// context block atelier prepends to user turns.
const APPEND_SYSTEM_PROMPT: &str = "\
The user is talking to you through atelier, a terminal frontend. A user \
message may begin with an <atelier-context> block that atelier gathered \
automatically just before sending (git status, diff, project layout, build \
diagnostics). Treat it as the current state of the project; the user did \
not type it.";

/// Added when atelier's tools are offered alongside claude's own.
const TOOLS_ALONGSIDE: &str = "\
atelier's own tools are available as `mcp__atelier__*` (file tools confined \
to the project with read-before-edit tracking, a sandboxed `node` scripting \
tool, and more); use them where they fit.";

/// Added when atelier's tools replace claude's overlapping built-ins.
const TOOLS_REPLACING: &str = "\
In this session your built-in file, shell and fetch tools (Read, Write, \
Edit, NotebookEdit, Bash, Monitor, WebFetch, …) are switched off; atelier's \
tools (`mcp__atelier__*`) replace them, and run with atelier's own \
approval policy. They take a `path` argument relative to the project root. \
- `read` instead of Read: text with line numbers, images (you see them), \
PDFs (`pages` for large ones), notebooks; files outside the project need \
the user's approval. \
- `write`, `edit`, `multiedit`, `apply_patch` (unified diff) instead of \
Write/Edit; `notebook_edit` instead of NotebookEdit. Read a file before \
editing it. \
- `bash` instead of Bash. For long-running commands (servers, watchers, big \
builds) pass `background: true`, then `bash_output` to read new output or \
wait for it to finish or print a pattern (`wait_ms`, `until`) — instead of \
Monitor — and `bash_kill` to stop it. \
- `grep`, `glob`, `ls`, `tree` to search and explore; `web_fetch` instead \
of WebFetch; `todo` for a task list; `node` for sandboxed JavaScript. \
- `ask` instead of AskUserQuestion, to put a question with options to the \
user.";

/// claude's built-ins that atelier's tools replace, switched off when those
/// are offered (unless the user keeps them). Names from several claude
/// versions; ones a given version lacks are ignored.
const REPLACED_BUILTINS: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "MultiEdit",
    "NotebookEdit",
    "NotebookRead",
    "Bash",
    "BashOutput",
    "KillBash",
    "KillShell",
    "Monitor",
    "Glob",
    "Grep",
    "LS",
    "WebFetch",
    "TodoWrite",
    "AskUserQuestion",
];

/// Delivered (preempting the running turn) when the user presses Ctrl-C.
const STOP_MESSAGE: &str = "\
[atelier] The user pressed Ctrl-C to stop this turn. Stop here: do not \
continue the interrupted work or start anything new. Background agents and \
tasks are still running; leave them alone. Reply with one short line saying \
where you stopped.";

/// Keep this much of the child's stderr for error messages.
const STDERR_TAIL: usize = 4096;

/// The `result` subtype claude ends a turn with when `--max-turns` ran out
/// (such a result carries no text, so the subtype is what gets reported).
const MAX_TURNS_RESULT: &str = "error_max_turns";

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
    /// Keep claude's built-ins that overlap atelier's tools, instead of
    /// switching them off.
    pub keep_builtins: bool,
    /// `--max-turns`: the most agentic turns claude takes per prompt before
    /// ending it with an `error_max_turns` result.
    pub max_turns: Option<u32>,
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
    #[cfg(unixlike)]
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
    /// Model requests (assistant messages reporting usage) not yet collected
    /// by [`Bridge::take_steps`].
    steps: u32,
    /// The final answer of the last turn that completed, until collected by
    /// [`Bridge::take_answer`] (for `turn_end` hooks).
    answer: Option<String>,
}

/// Progress of the turn [`Bridge::turn`] is waiting on.
struct TurnState {
    /// uuids of the user messages whose completion ends the turn: the user's
    /// own and those handed over mid-turn, or the stop message once Ctrl-C
    /// was pressed.
    awaiting: Vec<String>,
    /// Messages handed over mid-turn and not yet started, by uuid, so the
    /// transcript can show each one when claude delivers it.
    injected: HashMap<String, String>,
    /// Tool calls of this turn still running: queued messages are handed
    /// over only then (see the module docs).
    tools: HashSet<String>,
    /// An awaited message has started running (results seen before that
    /// belong to some other turn, e.g. one reporting a finished background
    /// agent).
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
    #[cfg(unixlike)]
    Call(host::ToolRequest),
}

/// Runs atelier's tools for claude: the session's tool registry and approval
/// policy, driven with the interface of whichever loop receives the call.
pub trait ToolHost {
    /// Whether a call claude wants to make with one of its own tools may
    /// run, under the session's permission rules and mode — asking the user
    /// when they say to. `Err` is the reason given to claude.
    fn permit(&mut self, req: permissions::Request, ui: &mut dyn Ui) -> Result<(), String>;
    /// Run tool `name` (asking approval if its policy says so), returning
    /// its output and whether it succeeded.
    fn call(
        &mut self,
        name: &str,
        arguments: Value,
        ui: &mut dyn Ui,
    ) -> (crate::tools::ToolOutput, bool);
}

/// How atelier's tools appear in claude: `mcp__atelier__<tool>`.
#[cfg(unixlike)]
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
        ]);
        if let Some(m) = launch.model {
            cmd.args(["--model", m]);
        }
        if let Some(id) = launch.resume {
            cmd.args(["--resume", id]);
        }
        if let Some(n) = launch.max_turns {
            cmd.args(["--max-turns", &n.to_string()]);
        }
        let mut mcp = mcp_config(settings);
        // atelier's own tools, unless a configured server already took the
        // name. claude may run them without asking: atelier applies its own
        // policy (confined file tools run, `bash` and the like ask).
        #[cfg(unixlike)]
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
        #[cfg(not(unixlike))]
        let (host, _) = (None::<()>, tools);
        // Only take claude's tools away when atelier's are there instead.
        let replacing = host.is_some() && !launch.keep_builtins;
        if replacing {
            cmd.arg("--disallowedTools").args(REPLACED_BUILTINS);
        }
        let prompt = match (host.is_some(), replacing) {
            (_, true) => format!("{APPEND_SYSTEM_PROMPT}\n\n{TOOLS_REPLACING}"),
            (true, false) => format!("{APPEND_SYSTEM_PROMPT}\n\n{TOOLS_ALONGSIDE}"),
            (false, _) => APPEND_SYSTEM_PROMPT.to_string(),
        };
        cmd.args(["--append-system-prompt", &prompt]);
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
            #[cfg(unixlike)]
            _host: host,
            stderr,
            session_id: launch.resume.map(str::to_owned),
            model: None,
            tool_names: HashMap::new(),
            background: 0,
            context: 0,
            output: 0,
            steps: 0,
            answer: None,
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

    /// Model requests claude made since the last call.
    pub fn take_steps(&mut self) -> u32 {
        std::mem::take(&mut self.steps)
    }

    /// The awaited turn's final answer, if it completed since the last call.
    pub fn take_answer(&mut self) -> Option<String> {
        self.answer.take()
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
            awaiting: vec![new_uuid()],
            injected: HashMap::new(),
            tools: HashSet::new(),
            started: false,
            stopping: false,
            error: None,
            done: false,
        };
        self.send_user(content, &st.awaiting[0])?;
        while !st.done {
            match self.incoming.recv_timeout(POLL) {
                Ok(inc) => self.receive(inc, ui, host, Some(&mut st))?,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => unreachable!("the bridge holds no sender"),
            }
            if !st.done && !st.stopping && ui.interrupted() {
                self.stop(&mut st, ui)?;
            }
            if !st.done && !st.stopping && !st.tools.is_empty() {
                self.inject(&mut st, ui)?;
            }
        }
        ui.turn_end();
        match st.error {
            // claude's own step limit (`--max-turns`), reported the way
            // atelier's loop reports its own.
            Some(e) if e == MAX_TURNS_RESULT => Err(crate::agent::StepLimit.into()),
            Some(e) => bail!("claude: {e}"),
            None => Ok(()),
        }
    }

    /// Render whatever the child produced while no turn was running — claude
    /// reporting back on background work. Returns whether anything arrived.
    #[cfg_attr(not(tui), allow(dead_code))]
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
            #[cfg(unixlike)]
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
                #[cfg(unixlike)]
                let name = name
                    .strip_prefix(TOOL_PREFIX)
                    .map_or(name.clone(), str::to_owned);
                ui.tool_start(&name, &input);
                if let Some(st) = turn {
                    st.tools.insert(id.clone());
                }
                self.tool_names.insert(id, name);
            }
            Event::ToolResult {
                id,
                content,
                is_error,
            } => {
                if let Some(st) = turn {
                    st.tools.remove(&id);
                }
                let name = self.tool_names.remove(&id).unwrap_or_default();
                ui.tool_end(&name, &content, !is_error);
            }
            Event::Context(n) => {
                self.context = n;
                self.steps += 1;
            }
            Event::Background(n) => self.background = n,
            Event::Permission(req) => {
                let answer = answer_permission(&req, ui, host);
                self.write(&answer)?;
            }
            Event::Lifecycle { uuid, state } => {
                if let Some(st) = turn
                    && st.awaiting.contains(&uuid)
                {
                    match state.as_str() {
                        "queued" => {}
                        "started" => {
                            st.started = true;
                            if let Some(text) = st.injected.remove(&uuid) {
                                ui.queued_delivered(&text);
                            }
                        }
                        // completed, cancelled, …
                        _ => {
                            st.awaiting.retain(|u| *u != uuid);
                            st.done = st.awaiting.is_empty();
                        }
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
                        } else {
                            self.answer = text;
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
        // claude delivers the handed-over messages it hasn't started along
        // with the stop message, so they're still awaited; the rest of the
        // turn ends as cancelled.
        st.awaiting.retain(|u| st.injected.contains_key(u));
        st.awaiting.push(id);
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

    /// Hand the user's queued messages to claude, which delivers them at the
    /// turn's next step.
    fn inject(&mut self, st: &mut TurnState, ui: &mut dyn Ui) -> Result<()> {
        for text in ui.take_queued() {
            let id = new_uuid();
            self.send_user(Value::from(text.as_str()), &id)?;
            st.awaiting.push(id.clone());
            st.injected.insert(id, text);
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
pub(crate) fn new_uuid() -> String {
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

/// claude's built-in tools as the permission rules know them: the atelier
/// tool that does the same thing, and whether it can modify something. The
/// arguments need no translation — claude's `command`, `url` and
/// `file_path`/`notebook_path` are read where atelier's own are (see
/// [`crate::tools::path_arg`]).
const BUILTIN_SUBJECTS: &[(&str, &str, bool)] = &[
    ("Bash", "bash", true),
    ("Edit", "edit", true),
    ("Write", "write", true),
    ("MultiEdit", "multiedit", true),
    ("NotebookEdit", "notebook_edit", true),
    ("Read", "read", false),
    ("NotebookRead", "read", false),
    ("Glob", "glob", false),
    ("Grep", "grep", false),
    ("LS", "ls", false),
    ("WebFetch", "web_fetch", false),
    ("WebSearch", "web_search", false),
    ("TodoWrite", "todo", false),
    ("BashOutput", "bash_output", false),
    ("KillBash", "bash_kill", false),
    ("KillShell", "bash_kill", false),
    ("AskUserQuestion", "ask", false),
];

/// The tool name a rule is matched against for claude's tool `name`, and
/// whether the tool can modify something. A built-in atelier has no
/// counterpart for (`Agent`, `Skill`, …) goes by its own name in lower case,
/// and an MCP tool by its name as it is (`mcp__server__tool`, the name
/// atelier's rules already use); both count as modifying, the safe answer
/// for a tool atelier knows nothing about.
fn rule_subject(name: &str) -> (String, bool) {
    if let Some((_, tool, mutates)) = BUILTIN_SUBJECTS.iter().find(|(n, ..)| *n == name) {
        (tool.to_string(), *mutates)
    } else if name.starts_with("mcp__") {
        (name.to_string(), true)
    } else {
        (name.to_ascii_lowercase(), true)
    }
}

/// Answer a `can_use_tool` request: the session's rules and mode decide,
/// and the user is asked only where they leave it open. claude asks because
/// its own rules want approval, so the call counts as one that needs it.
fn answer_permission(req: &PermissionRequest, ui: &mut dyn Ui, host: &mut dyn ToolHost) -> Value {
    let (tool, mutates) = rule_subject(&req.tool);
    let arguments = req.input.to_string();
    let verdict = host.permit(
        permissions::Request {
            tool: &tool,
            arguments: &arguments,
            needs_approval: true,
            mutates,
            hook_allowed: false,
            hooks: None,
        },
        ui,
    );
    permission_response(&req.request_id, verdict, &req.input)
}

/// The control response for a permission request: allow, or deny with the
/// reason (which claude shows its model).
fn permission_response(request_id: &str, verdict: Result<(), String>, input: &Value) -> Value {
    let response = match verdict {
        Ok(()) => json!({ "behavior": "allow", "updatedInput": input }),
        Err(reason) => json!({
            "behavior": "deny",
            // Tool results read `error: …`; here the denial is the frame.
            "message": reason.strip_prefix("error: ").unwrap_or(&reason),
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
        let allow = permission_response("r1", Ok(()), &input);
        assert_eq!(allow["type"], "control_response");
        assert_eq!(allow["response"]["subtype"], "success");
        assert_eq!(allow["response"]["request_id"], "r1");
        assert_eq!(allow["response"]["response"]["behavior"], "allow");
        assert_eq!(allow["response"]["response"]["updatedInput"], input);

        let deny = permission_response("r1", Err("error: refused by x.".into()), &input);
        assert_eq!(deny["response"]["request_id"], "r1");
        assert_eq!(deny["response"]["response"]["behavior"], "deny");
        assert_eq!(deny["response"]["response"]["message"], "refused by x.");
        assert!(deny["response"]["response"].get("updatedInput").is_none());
    }

    #[test]
    fn builtin_tools_map_to_rule_subjects() {
        for (name, tool, mutates) in [
            ("Bash", "bash", true),
            ("Edit", "edit", true),
            ("Write", "write", true),
            ("MultiEdit", "multiedit", true),
            ("NotebookEdit", "notebook_edit", true),
            ("Read", "read", false),
            ("NotebookRead", "read", false),
            ("Glob", "glob", false),
            ("Grep", "grep", false),
            ("LS", "ls", false),
            ("WebFetch", "web_fetch", false),
            ("WebSearch", "web_search", false),
            ("TodoWrite", "todo", false),
            ("BashOutput", "bash_output", false),
            ("KillShell", "bash_kill", false),
            // No counterpart: its own name in lower case, and modifying.
            ("Agent", "agent", true),
            ("ExitPlanMode", "exitplanmode", true),
            ("SomethingNew", "somethingnew", true),
            // Names are exact: this is not claude's `Bash`.
            ("bash", "bash", true),
            ("BASH", "bash", true),
            // MCP tools keep the name atelier's rules use for them.
            (
                "mcp__GitHub__create_issue",
                "mcp__GitHub__create_issue",
                true,
            ),
        ] {
            assert_eq!(rule_subject(name), (tool.to_string(), mutates), "{name}");
        }
        // Every mapped name is a tool atelier has, and agrees with it on
        // whether the tool modifies — so a rule or a mode means the same
        // thing for claude's tool as for atelier's.
        let registry = crate::tools::builtin_registry();
        for (name, tool, mutates) in BUILTIN_SUBJECTS {
            let own = registry
                .get(tool)
                .unwrap_or_else(|| panic!("{name} maps to unknown tool {tool}"));
            let args = match *tool {
                "bash" => json!({"command": "ls"}),
                _ => json!({"file_path": "/x", "url": "https://x"}),
            };
            assert_eq!(own.mutates(&args), *mutates, "{name} -> {tool}");
        }
        // Everything atelier switches off by default is mapped (or, for
        // `Monitor`, deliberately left to the safe default).
        for name in REPLACED_BUILTINS {
            assert!(
                *name == "Monitor" || BUILTIN_SUBJECTS.iter().any(|(n, ..)| n == name),
                "{name} has no rule subject"
            );
        }
    }

    /// A [`Ui`] that gives one answer to every prompt and counts them.
    struct Asker {
        answer: fn() -> crate::agent::Approval,
        asked: Vec<String>,
    }

    impl Asker {
        fn new(answer: fn() -> crate::agent::Approval) -> Self {
            let asked = Vec::new();
            Self { answer, asked }
        }
    }

    impl Ui for Asker {
        fn reasoning(&mut self, _: &str) {}
        fn content(&mut self, _: &str) {}
        fn tool_start(&mut self, _: &str, _: &str) {}
        fn tool_end(&mut self, _: &str, _: &str, _: bool) {}
        fn info(&mut self, _: &str) {}
        fn ask_approval(&mut self, tool: &str, _: &str) -> crate::agent::Approval {
            self.asked.push(tool.to_string());
            (self.answer)()
        }
        fn notice(&mut self, _: &str) {}
    }

    /// A [`ToolHost`] that judges permission requests as a session does —
    /// the real decision, under rules and a mode given here instead of read
    /// from the user's files.
    struct RuleHost {
        policy: permissions::Policy,
        settings: Settings,
        dir: std::path::PathBuf,
        mode: permissions::Mode,
    }

    impl RuleHost {
        fn new(name: &str, mode: permissions::Mode, allow: &[&str], deny: &[&str]) -> Self {
            let list = |l: &[&str]| l.iter().map(|s| s.to_string()).collect();
            let rules = crate::settings::Permissions {
                allow: list(allow),
                ask: Vec::new(),
                deny: list(deny),
            };
            let dir =
                std::env::temp_dir().join(format!("atelier-bridge-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self {
                // `~` in a rule is /home/me here.
                policy: permissions::Policy::new(
                    &rules,
                    &Default::default(),
                    Some("/home/me".into()),
                ),
                settings: Settings::default(),
                dir,
                mode,
            }
        }

        /// The `behavior` (and denial message) claude gets for a request.
        fn answer(&mut self, tool: &str, input: Value, ui: &mut Asker) -> (String, String) {
            let req = PermissionRequest {
                request_id: "r1".into(),
                tool: tool.into(),
                input: input.clone(),
            };
            let v = answer_permission(&req, ui, self);
            let r = &v["response"]["response"];
            if r["behavior"] == "allow" {
                assert_eq!(r["updatedInput"], input);
            }
            let text = |v: &Value| v.as_str().unwrap_or_default().to_string();
            (text(&r["behavior"]), text(&r["message"]))
        }
    }

    impl Drop for RuleHost {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl ToolHost for RuleHost {
        fn permit(&mut self, req: permissions::Request, ui: &mut dyn Ui) -> Result<(), String> {
            let (policy, dir) = (&self.policy, &self.dir);
            permissions::approve_with(policy, &mut self.settings, dir, self.mode, req, ui)
        }
        fn call(
            &mut self,
            name: &str,
            _: Value,
            _: &mut dyn Ui,
        ) -> (crate::tools::ToolOutput, bool) {
            panic!("unexpected tool call: {name}")
        }
    }

    use crate::agent::Approval;
    use permissions::Mode;

    #[test]
    fn deny_rules_stop_claudes_builtin_tools() {
        let deny = [
            "bash(rm *)",
            "read(~/.ssh/**)",
            "write(.github/**)",
            "agent",
        ];
        // Whatever the user would answer, and in every mode — yolo included.
        for mode in Mode::ALL {
            let mut host = RuleHost::new("deny", mode, &["bash"], &deny);
            let mut ui = Asker::new(|| Approval::Once);
            let workflow = host.dir.join(".github/workflows/ci.yml");
            for (tool, input) in [
                ("Bash", json!({"command": "rm -rf build"})),
                ("Bash", json!({"command": "ls && sudo rm x"})),
                ("Read", json!({"file_path": "/home/me/.ssh/id_rsa"})),
                ("Write", json!({"file_path": workflow, "content": "x"})),
                ("Agent", json!({"prompt": "do it"})),
            ] {
                let (behavior, message) = host.answer(tool, input, &mut ui);
                assert_eq!(behavior, "deny", "{tool} in {mode}");
                assert!(
                    message.starts_with("refused by the permission rule"),
                    "{tool} in {mode}: {message}"
                );
            }
            assert!(ui.asked.is_empty(), "{mode}: {:?}", ui.asked);
        }
    }

    #[test]
    fn allow_rules_answer_for_claudes_builtin_tools_without_a_prompt() {
        let allow = ["bash(git diff *)", "web_fetch(domain:docs.rs)"];
        let mut host = RuleHost::new("allow", Mode::AutoEdit, &allow, &[]);
        let mut ui = Asker::new(|| Approval::Deny);
        let diff = json!({"command": "git diff --stat"});
        assert_eq!(host.answer("Bash", diff, &mut ui).0, "allow");
        let fetch = json!({"url": "https://docs.rs/serde", "prompt": "summarize"});
        assert_eq!(host.answer("WebFetch", fetch, &mut ui).0, "allow");
        assert!(ui.asked.is_empty());

        // What no rule covers is put to the user, under atelier's name for
        // the tool, and their no is passed on.
        let push = json!({"command": "git diff; git push"});
        let (behavior, message) = host.answer("Bash", push, &mut ui);
        assert_eq!(behavior, "deny");
        assert_eq!(message, "the user denied permission to run this tool.");
        let elsewhere = json!({"url": "https://example.com"});
        assert_eq!(host.answer("WebFetch", elsewhere, &mut ui).0, "deny");
        // claude asked, so even a tool atelier would run unasked is put to
        // the user rather than waved through.
        let read = json!({"file_path": "/etc/hosts"});
        assert_eq!(host.answer("Read", read, &mut ui).0, "deny");
        assert_eq!(ui.asked, ["bash", "web_fetch", "read"]);
    }

    #[test]
    fn always_on_a_builtin_tool_saves_an_atelier_rule() {
        let mut host = RuleHost::new("always", Mode::AutoEdit, &[], &[]);
        let mut ui = Asker::new(|| Approval::Always);
        let test = json!({"command": "cargo test --lib"});
        assert_eq!(host.answer("Bash", test, &mut ui).0, "allow");
        assert_eq!(host.settings.permissions.allow, ["bash(cargo test *)"]);
        let saved = Settings::load(&host.dir).unwrap();
        assert_eq!(saved.permissions.allow, ["bash(cargo test *)"]);
    }

    #[test]
    fn modes_govern_claudes_builtin_tools() {
        let edits = [
            ("Bash", json!({"command": "make install"})),
            (
                "Edit",
                json!({"file_path": "a.rs", "old_string": "a", "new_string": "b"}),
            ),
            ("Write", json!({"file_path": "a.rs", "content": "x"})),
            ("MultiEdit", json!({"file_path": "a.rs", "edits": []})),
            (
                "NotebookEdit",
                json!({"notebook_path": "a.ipynb", "new_source": "x"}),
            ),
            ("Agent", json!({"prompt": "do it"})),
            ("mcp__gh__create_issue", json!({"title": "x"})),
        ];
        let reads = [
            ("Read", json!({"file_path": "/etc/hosts"})),
            ("Grep", json!({"pattern": "x"})),
            ("WebFetch", json!({"url": "https://example.com"})),
            ("WebSearch", json!({"query": "x"})),
        ];

        // plan: what can modify is refused with the plan-mode message, a
        // whole-tool allow rule notwithstanding, and nobody is asked; what
        // only reads is put to the user as usual.
        let mut host = RuleHost::new(
            "plan",
            Mode::Plan,
            &["bash", "write", "bash(git log *)"],
            &[],
        );
        let mut ui = Asker::new(|| Approval::Once);
        for (tool, input) in edits.clone() {
            let (behavior, message) = host.answer(tool, input, &mut ui);
            assert_eq!(behavior, "deny", "{tool}");
            let subject = rule_subject(tool).0;
            assert!(
                message.starts_with(&format!("plan mode: {subject} is not available")),
                "{tool}: {message}"
            );
        }
        assert!(ui.asked.is_empty(), "{:?}", ui.asked);
        for (tool, input) in reads.clone() {
            assert_eq!(host.answer(tool, input, &mut ui).0, "allow", "{tool}");
        }
        assert_eq!(ui.asked.len(), reads.len());
        // …and `Bash` runs what a pattern allow rule names.
        let log = json!({"command": "git log -3"});
        assert_eq!(host.answer("Bash", log, &mut ui).0, "allow");
        assert_eq!(ui.asked.len(), reads.len());

        // ask-edits and auto-edit: claude asked, so the user is asked.
        for mode in [Mode::AskEdits, Mode::AutoEdit] {
            let mut host = RuleHost::new("ask", mode, &[], &[]);
            let mut ui = Asker::new(|| Approval::Deny);
            for (tool, input) in edits.iter().chain(&reads).cloned() {
                assert_eq!(
                    host.answer(tool, input, &mut ui).0,
                    "deny",
                    "{tool} in {mode}"
                );
            }
            assert_eq!(ui.asked.len(), edits.len() + reads.len(), "{mode}");
        }

        // yolo: everything is allowed, unasked.
        let mut host = RuleHost::new("yolo", Mode::Yolo, &[], &[]);
        let mut ui = Asker::new(|| Approval::Deny);
        for (tool, input) in edits.iter().chain(&reads).cloned() {
            assert_eq!(host.answer(tool, input, &mut ui).0, "allow", "{tool}");
        }
        assert!(ui.asked.is_empty());
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
        /// Typed "while the turn runs"; handed over on the bridge's ask.
        queued: Vec<String>,
        delivered: Vec<String>,
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
        fn take_queued(&mut self) -> Vec<String> {
            std::mem::take(&mut self.queued)
        }
        fn queued_delivered(&mut self, text: &str) {
            self.delivered.push(text.to_string());
        }
    }

    /// Allows every permission request, offers no tools.
    struct NoTools;

    impl ToolHost for NoTools {
        fn permit(&mut self, _: permissions::Request, _: &mut dyn Ui) -> Result<(), String> {
            Ok(())
        }
        fn call(
            &mut self,
            name: &str,
            _: Value,
            _: &mut dyn Ui,
        ) -> (crate::tools::ToolOutput, bool) {
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
            keep_builtins: false,
            max_turns: None,
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

    /// A message typed mid-turn reaches the running turn: claude delivers it
    /// after the tool results, and the turn ends once both are done.
    #[test]
    #[ignore = "drives the real claude CLI on your account; run with --ignored"]
    fn live_queued_message_joins_the_turn() {
        let dir = std::env::temp_dir().join(format!("atelier-live-{}", new_uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        let launch = Launch {
            bin: "claude",
            model: Some("haiku"),
            resume: None,
            keep_builtins: false,
            max_turns: None,
        };
        let mut b = Bridge::spawn(&launch, &dir, &Settings::default(), Vec::new()).unwrap();
        let mut ui = LiveUi {
            queued: vec!["Also: end your final reply with the word BANANA.".into()],
            ..LiveUi::default()
        };
        b.turn(
            json!("Run `sleep 5; echo first` with the Bash tool, then reply with its output."),
            &mut ui,
            &mut NoTools,
        )
        .unwrap();
        assert_eq!(ui.delivered.len(), 1, "{:?}", ui.delivered);
        assert!(ui.text.contains("BANANA"), "{:?}", ui.text);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
