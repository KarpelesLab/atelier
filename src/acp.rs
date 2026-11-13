//! `atelier acp`: the session as an agent of the Agent Client Protocol.
//!
//! ACP (<https://agentclientprotocol.com>) is how editors — Zed, the
//! JetBrains IDEs, Neovim plugins — drive coding agents: the editor launches
//! the agent as a subprocess and speaks JSON-RPC 2.0 with it over stdio.
//! This module is that agent, for **protocol version 1**: with it an editor
//! gets atelier without atelier growing a UI. `docs/acp.md` says what is
//! supported and how to set an editor up.
//!
//! It is a second front end next to [`crate::serve`], atelier's own
//! embedding protocol, not a translation of it: the same two threads over
//! the same wire ([`crate::rpc`]), with a [`Ui`] that speaks ACP's
//! vocabulary ([`present`]) instead of atelier's events.
//!
//! # Two threads
//!
//! - The **session thread** (the caller of [`run`]) owns the [`Session`]
//!   once there is one, and takes [`Job`]s off a channel one at a time:
//!   open the session, run a prompt, switch the mode. What a prompt reports
//!   goes through [`AcpUi`] as `session/update` notifications; its answer —
//!   the stop reason — is the response to `session/prompt`, sent when the
//!   turn is over.
//! - The **reader thread** owns stdin. It answers `initialize` itself,
//!   checks and queues the rest, hands the client's answers to whoever
//!   waits for them, and carries out `session/cancel` at once — which is
//!   why the session never reads stdin: a turn blocked on a permission
//!   request is cancelled from here.
//!
//! # One session
//!
//! ACP lets a connection hold several sessions, each with its own working
//! directory. atelier's session is the process's: its tools, the status
//! helpers and the sandbox go by the process's current directory. So
//! `session/new` changes to the directory the client names and opens the
//! session there, and a second `session/new` is refused
//! ([`code::SESSION_LIMIT`]). Editors start an agent process per thread, or
//! can be made to; nothing is lost but the sharing of one process.
//!
//! That is also why the session is opened here rather than by `main`:
//! which directory it belongs to is not known until the client says.
//!
//! # Cancelling
//!
//! Prompts are queued, each with a sequence number, and `session/cancel`
//! marks every prompt accepted so far — the running one, which sees it at
//! its next streamed chunk or tool boundary (the same limit Ctrl-C has),
//! and any still waiting, which then end as `cancelled` without starting.
//! A prompt sent after the cancel is not affected by it.

mod present;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};

use crate::agent::{self, Approval, Dispatch, Session, StepLimit, Ui};
use crate::events;
use crate::mcp;
use crate::permissions::Mode;
use crate::rpc::{self, Failure, Id, Reply, Scope as Asked, Wire, lock};
use crate::settings::Permissions;
use crate::tools::ask::{Answer, Question};
use crate::tools::files::{self, FileAccess};

/// The protocol version this build speaks: ACP's major version, the only
/// one there is a stable specification of.
const PROTOCOL: u64 = 1;

/// How often the idle session looks for output of its own (see `idle`).
const IDLE_POLL: Duration = Duration::from_millis(200);

/// How long an interrupted turn has to let go once the client is gone,
/// before the process ends regardless.
const GRACE: Duration = Duration::from_secs(5);

/// How long the client has to read or write a file for a tool. A turn can
/// be cancelled out of a permission request, but not out of a write that
/// may be half done; a client that never answers must not hold it forever.
const FILE_TIMEOUT: Duration = Duration::from_secs(60);

/// Error codes beyond JSON-RPC's own ([`rpc::code`]).
mod code {
    /// ACP's "resource not found": a session id this connection never gave.
    pub const NOT_FOUND: i64 = -32002;
    /// atelier's own, outside the ranges JSON-RPC and ACP reserve: this
    /// process already has its one session.
    pub const SESSION_LIMIT: i64 = 1001;
}

/// `atelier acp`, as asked for on the command line (see `cli`).
#[derive(Debug, PartialEq, Eq)]
pub struct Options {
    /// `--max-turns`: the most model requests one prompt may make before it
    /// ends with the stop reason `max_turn_requests`. `None` is no limit.
    pub max_turns: Option<u32>,
}

/// Speak ACP with the program on the other end of stdin/stdout until it
/// goes away, and return the exit status: 0, since how each prompt went was
/// reported to the client as it happened. `open` sets up a session in the
/// current directory (see the module docs for why that is not done yet).
pub fn run(open: impl FnMut() -> anyhow::Result<Session>, opts: Options) -> u8 {
    let shared = Arc::new(Shared {
        wire: Wire::new("acp"),
        state: Mutex::new(State::default()),
        next_call: AtomicU64::new(1),
    });
    let (jobs_tx, jobs_rx) = mpsc::channel();
    let reader = Arc::clone(&shared);
    std::thread::spawn(move || reader.read_requests(&jobs_tx));
    Agent {
        shared,
        open,
        max_turns: opts.max_turns,
        current: None,
    }
    .serve(&jobs_rx);
    0
}

/// Work for the session thread, in the order the client asked for it.
enum Job {
    NewSession {
        id: Id,
        cwd: PathBuf,
        servers: Vec<mcp::Server>,
    },
    Prompt {
        id: Id,
        seq: u64,
        prompt: present::Prompt,
    },
    SetMode {
        id: Id,
        mode: Mode,
    },
    /// stdin ended: the client is gone.
    Eof,
}

/// What the client said it can do, in `initialize`. Everything it did not
/// say is something it cannot.
#[derive(Clone, Copy, Default)]
struct Client {
    /// `fs.readTextFile`: it serves `fs/read_text_file`.
    read_file: bool,
    /// `fs.writeTextFile`: it serves `fs/write_text_file`.
    write_file: bool,
    /// `elicitation.form`: it can put a form to the user.
    form: bool,
    /// `session.notices`: it shows `notice` updates.
    notices: bool,
}

/// Where the process's one session stands.
#[derive(Default, PartialEq, Eq)]
enum Opened {
    #[default]
    No,
    /// `session/new` was accepted and is being carried out.
    Opening,
    /// Open, under this id.
    Yes(String),
}

/// A prompt that was accepted and has not been answered yet.
struct Turn {
    seq: u64,
    /// The request's id, to recognize a `$/cancel_request` for it.
    id: Id,
    cancelled: bool,
}

/// What both threads need to agree on, under one lock.
#[derive(Default)]
struct State {
    /// `initialize` was answered; nothing else is accepted before.
    client: Option<Client>,
    session: Opened,
    /// The prompts accepted and not yet answered, oldest first: the first
    /// is the one running, if any is.
    turns: Vec<Turn>,
    /// Prompts accepted so far; the last one's sequence number.
    accepted: u64,
}

/// What the reader thread does about a request it accepted: answer now, or
/// queue a job that answers later.
#[derive(Default)]
struct Accepted {
    reply: Option<Value>,
    job: Option<Job>,
}

/// State shared by the two threads (and by the tools, through
/// [`ClientFiles`]).
struct Shared {
    /// stdout, and the requests made of the client that it has not answered.
    wire: Wire,
    state: Mutex<State>,
    /// Numbers the tool calls of the session, for their ids.
    next_call: AtomicU64,
}

impl Shared {
    /// A `session/update` notification.
    fn update(&self, session: &str, update: Value) {
        let params = json!({ "sessionId": session, "update": update });
        self.wire.notify("session/update", params);
    }

    /// Something for the user that is not part of the conversation — a
    /// server that failed to connect, a background job that ended: a
    /// `notice` when the client shows those, the log otherwise.
    fn note(&self, session: &str, client: Client, severity: &str, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if !client.notices {
            return self.wire.log(text);
        }
        // A notice is a title and an optional description: the first line,
        // and the rest.
        let (title, rest) = text.split_once('\n').unwrap_or((text, ""));
        let mut notice = json!({ "sessionUpdate": "notice", "severity": severity, "title": title });
        if !rest.trim().is_empty() {
            notice["description"] = Value::from(rest.trim());
        }
        self.update(session, notice);
    }

    /// Whether prompt `seq` was cancelled.
    fn cancelled(&self, seq: u64) -> bool {
        let state = lock(&self.state);
        state.turns.iter().any(|t| t.seq == seq && t.cancelled)
    }

    /// The reader thread: every line of stdin, until it ends.
    fn read_requests(&self, jobs: &Sender<Job>) {
        self.wire
            .read(|method, id, params| match self.accept(method, id, params) {
                Ok(accepted) => {
                    if let Some(reply) = accepted.reply {
                        self.wire.respond(id, reply);
                    }
                    if let Some(job) = accepted.job {
                        let _ = jobs.send(job);
                    }
                }
                Err(failure) => self.wire.fail(id, failure),
            });
        // The client is gone: whatever is running is stopped, as far as it
        // can be, and the session thread is told to leave.
        for turn in &mut lock(&self.state).turns {
            turn.cancelled = true;
        }
        self.wire.close();
        let _ = jobs.send(Job::Eof);
        rpc::watchdog(GRACE);
    }

    /// Decide what to do about the request `method`, without doing anything
    /// slow: the state is checked and changed under its lock, the answer is
    /// written by the caller once the lock is released.
    fn accept(&self, method: &str, id: &Id, params: &Value) -> Result<Accepted, Failure> {
        if !(params.is_null() || params.is_object()) {
            return Err(Failure::params("\"params\" must be an object"));
        }
        let mut state = lock(&self.state);
        if method == "initialize" {
            if state.client.is_some() {
                let why = "initialize was already called";
                return Err(Failure::new(rpc::code::INVALID_REQUEST, why));
            }
            if params["protocolVersion"].as_u64().is_none() {
                return Err(Failure::params("\"protocolVersion\" must be a number"));
            }
            let caps = &params["clientCapabilities"];
            state.client = Some(Client {
                read_file: caps["fs"]["readTextFile"] == true,
                write_file: caps["fs"]["writeTextFile"] == true,
                // Each mode is advertised by an object of its own; an empty
                // `elicitation` advertises none.
                form: caps["elicitation"]["form"].is_object(),
                notices: caps["session"]["notices"].is_object(),
            });
            if let Some(name) = params["clientInfo"]["name"].as_str() {
                let version = params["clientInfo"]["version"].as_str().unwrap_or("?");
                self.wire.log(&format!("client {name} {version}"));
            }
            return Ok(Accepted {
                reply: Some(hello()),
                ..Accepted::default()
            });
        }
        if state.client.is_none() {
            let why = "initialize first";
            return Err(Failure::new(rpc::code::INVALID_REQUEST, why));
        }
        match method {
            "session/new" => {
                let cwd = params["cwd"].as_str().map(PathBuf::from);
                let Some(cwd) = cwd.filter(|cwd| cwd.is_absolute()) else {
                    return Err(Failure::params("\"cwd\" must be an absolute path"));
                };
                let servers = mcp_servers(&params["mcpServers"])?;
                if state.session != Opened::No {
                    let why = "this atelier process already has its session: atelier serves \
                               one session per process (start another `atelier acp` for another)";
                    return Err(Failure::new(code::SESSION_LIMIT, why));
                }
                state.session = Opened::Opening;
                let id = id.clone();
                Ok(Accepted {
                    job: Some(Job::NewSession { id, cwd, servers }),
                    ..Accepted::default()
                })
            }
            "session/prompt" => {
                session_named(&state, params)?;
                let Some(blocks) = params["prompt"].as_array() else {
                    return Err(Failure::params("\"prompt\" must be an array of content"));
                };
                let prompt = present::prompt(blocks).map_err(Failure::params)?;
                state.accepted += 1;
                let seq = state.accepted;
                state.turns.push(Turn {
                    seq,
                    id: id.clone(),
                    cancelled: false,
                });
                let id = id.clone();
                Ok(Accepted {
                    job: Some(Job::Prompt { id, seq, prompt }),
                    ..Accepted::default()
                })
            }
            "session/cancel" => {
                session_named(&state, params)?;
                for turn in &mut state.turns {
                    turn.cancelled = true;
                }
                // A turn waiting on the user is stopped there: the client
                // owes those requests a "cancelled" answer, and is not
                // waited for.
                self.wire.cancel_turn();
                Ok(Accepted::default())
            }
            // The protocol's general way to give up on a request: for a
            // prompt it is `session/cancel` for that prompt alone. For
            // anything else there is nothing to stop — the answer is
            // already on its way.
            "$/cancel_request" => {
                let wanted = Some(params["requestId"].clone());
                if let Some(at) = state.turns.iter().position(|t| t.id == wanted) {
                    state.turns[at].cancelled = true;
                    if at == 0 {
                        self.wire.cancel_turn();
                    }
                }
                Ok(Accepted::default())
            }
            "session/set_mode" => {
                session_named(&state, params)?;
                let mode = params["modeId"].as_str().map(Mode::parse);
                let Some(Ok(mode)) = mode else {
                    let modes: Vec<&str> = Mode::ALL.iter().map(|m| m.name()).collect();
                    let why = format!("\"modeId\" must be one of {}", modes.join(", "));
                    return Err(Failure::params(why));
                };
                let id = id.clone();
                Ok(Accepted {
                    job: Some(Job::SetMode { id, mode }),
                    ..Accepted::default()
                })
            }
            _ => Err(Failure::new(
                rpc::code::METHOD_NOT_FOUND,
                format!("unknown method '{method}'"),
            )),
        }
    }
}

/// The answer to `initialize`: the version, and what this agent really does.
///
/// The version is 1 whatever the client asked for: the same when it asked
/// for 1, and "the latest this agent supports" when it asked for another,
/// which is what the specification has an agent answer in both cases.
fn hello() -> Value {
    json!({
        "protocolVersion": PROTOCOL,
        "agentCapabilities": {
            // The saved session (`--continue`) has no id a client could
            // name, and replaying it is not implemented.
            "loadSession": false,
            "promptCapabilities": { "image": true, "audio": false, "embeddedContext": true },
            "mcpCapabilities": { "http": true, "sse": false },
        },
        "agentInfo": {
            "name": "atelier",
            "title": "atelier",
            "version": env!("CARGO_PKG_VERSION"),
        },
        // The model backends authenticate out of band (an API key in the
        // environment, `/login`, claude's own sign-in).
        "authMethods": [],
    })
}

/// Check that `params` names the open session: a request for any other is
/// answered "not found", with the id that was asked for.
fn session_named(state: &State, params: &Value) -> Result<(), Failure> {
    let Some(asked) = params["sessionId"].as_str() else {
        return Err(Failure::params("\"sessionId\" must be a string"));
    };
    match &state.session {
        Opened::Yes(id) if id == asked => Ok(()),
        _ => Err(Failure {
            code: code::NOT_FOUND,
            message: format!("no session '{asked}' on this connection"),
            data: Some(json!({ "sessionId": asked })),
        }),
    }
}

/// The `mcpServers` of `session/new` as servers atelier's MCP layer
/// connects: stdio ones (no `type`) and `http` ones. `sse` was not
/// advertised and is refused.
fn mcp_servers(list: &Value) -> Result<Vec<mcp::Server>, Failure> {
    let entries = match list {
        Value::Null => return Ok(Vec::new()),
        Value::Array(entries) => entries,
        _ => return Err(Failure::params("\"mcpServers\" must be an array")),
    };
    let mut servers = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        let bad = |what: &str| Failure::params(format!("mcpServers[{i}]: {what}"));
        let text = |name: &str| {
            let value = entry[name].as_str().filter(|v| !v.is_empty());
            value.ok_or_else(|| bad(&format!("\"{name}\" must be a non-empty string")))
        };
        // `env` and `headers`: lists of `{name, value}`.
        let pairs = |name: &str| {
            let pair = |p: &Value| Some((p["name"].as_str()?.into(), p["value"].as_str()?.into()));
            let list = entry[name].as_array().map(Vec::as_slice).unwrap_or(&[]);
            let pairs: Option<Vec<(String, String)>> = list.iter().map(pair).collect();
            pairs.ok_or_else(|| bad(&format!("\"{name}\" must be a list of name and value")))
        };
        // The name becomes part of tool names (`mcp__<server>__<tool>`),
        // which model APIs hold to letters, digits, `_` and `-`.
        let name: String = text("name")?
            .chars()
            .map(|c| match c {
                c if c.is_ascii_alphanumeric() || c == '-' || c == '_' => c,
                _ => '_',
            })
            .collect();
        let options = mcp::ServerOptions::default();
        servers.push(match entry["type"].as_str() {
            None => {
                let strings = |v: &Value| v.as_str().map(str::to_string);
                let args = entry["args"].as_array().map(Vec::as_slice).unwrap_or(&[]);
                let args: Option<Vec<String>> = args.iter().map(strings).collect();
                mcp::Server::Stdio(mcp::StdioServer {
                    name,
                    command: text("command")?.to_string(),
                    args: args.ok_or_else(|| bad("\"args\" must be strings"))?,
                    env: pairs("env")?,
                    options,
                })
            }
            Some("http") => mcp::Server::Http(mcp::HttpServer {
                name,
                url: text("url")?.to_string(),
                headers: pairs("headers")?,
                options,
                client_id: None,
                // What the client sends in `headers` is the sign-in; none of
                // atelier's own stored tokens go to a server it named.
                tokens: None,
            }),
            Some(other) => {
                return Err(bad(&format!(
                    "the \"{other}\" transport is not supported (stdio and http are)"
                )));
            }
        });
    }
    Ok(servers)
}

/// The built-in slash commands worth offering in an editor: those whose
/// whole effect is on the session or is text to show. Left out are the ones
/// that are the terminal's (`/config`, `/clear`, `/quit`, `/image`: the
/// editor has its own) or need a browser on this machine at a moment the
/// editor does not expect (`/login`). Any of them still runs if typed.
const COMMANDS: &[(&str, &str, Option<&str>)] = &[
    (
        "compact",
        "Shrink the conversation: prune old tool output, summarize older turns",
        Some("instructions for the summary (optional)"),
    ),
    (
        "context",
        "Show where the context window goes, in tokens",
        None,
    ),
    (
        "usage",
        "Show this session's model requests, tokens and cost",
        None,
    ),
    (
        "model",
        "Show or switch the active model",
        Some("provider/model#variant (optional)"),
    ),
    (
        "mode",
        "Show or switch how much runs without asking",
        Some("plan | ask-edits | auto-edit | yolo (optional)"),
    ),
    (
        "goal",
        "Keep the agent working until a condition holds",
        Some("the condition, or clear (optional)"),
    ),
    (
        "init",
        "Have the agent write or improve this repository's AGENTS.md",
        Some("guidance (optional)"),
    ),
    (
        "memory",
        "List what the agent remembers, or forget one memory",
        Some("forget <name> (optional)"),
    ),
    ("skills", "List the skills the agent can load", None),
    (
        "agents",
        "List the subagents and the ones running in the background",
        Some("stop <id> | clean [id] (optional)"),
    ),
    ("tools", "List the available tools", None),
    (
        "mcp",
        "List, add or remove MCP servers",
        Some("add <name> <command…> | remove <name> (optional)"),
    ),
    ("permissions", "List the permission rules in force", None),
    (
        "trust",
        "Accept what this project's atelier.toml runs and grants",
        Some("show | revoke (optional)"),
    ),
    ("new", "Start a fresh conversation in this session", None),
    ("help", "List every command", None),
];

/// The modes of [`Mode`] as ACP session modes, with the current one.
fn modes(current: Mode) -> Value {
    let title = |mode: Mode| match mode {
        Mode::Plan => "Plan",
        Mode::AskEdits => "Ask before edits",
        Mode::AutoEdit => "Auto-edit",
        Mode::Yolo => "Yolo",
    };
    let available: Vec<Value> = Mode::ALL
        .iter()
        .map(|&m| json!({ "id": m.name(), "name": title(m), "description": m.summary() }))
        .collect();
    json!({ "currentModeId": current.name(), "availableModes": available })
}

/// The process's session, once `session/new` opened it.
struct Current {
    session: Session,
    /// The id the client knows it by.
    id: String,
    /// The directory it was opened in.
    root: PathBuf,
    /// The command list as last advertised, to say so when it changes.
    commands: Value,
}

/// The session thread's side.
struct Agent<F> {
    shared: Arc<Shared>,
    /// Sets up a session in the current directory.
    open: F,
    max_turns: Option<u32>,
    current: Option<Current>,
}

impl<F: FnMut() -> anyhow::Result<Session>> Agent<F> {
    /// Carry out jobs until the client is gone.
    fn serve(mut self, jobs: &Receiver<Job>) {
        loop {
            match jobs.recv_timeout(IDLE_POLL) {
                Ok(Job::NewSession { id, cwd, servers }) => self.new_session(&id, &cwd, servers),
                Ok(Job::Prompt { id, seq, prompt }) => self.prompt(&id, seq, &prompt),
                Ok(Job::SetMode { id, mode }) => {
                    // Accepted only with a session open (`session_named`).
                    if let Some(current) = &mut self.current {
                        current.session.set_mode(mode);
                    }
                    self.shared.wire.respond(&id, json!({}));
                }
                Ok(Job::Eof) | Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => self.idle(),
            }
        }
    }

    fn client(&self) -> Client {
        lock(&self.shared.state).client.unwrap_or_default()
    }

    /// `session/new`: open the session in `cwd`, connect the MCP servers —
    /// the configured ones (if the project is trusted) and the client's —
    /// and answer with its id and modes. The commands follow.
    fn new_session(&mut self, id: &Id, cwd: &Path, servers: Vec<mcp::Server>) {
        let (mut session, root) = match self.open_in(cwd) {
            Ok(opened) => opened,
            Err(failure) => {
                // Nothing was opened: the client may try again.
                lock(&self.shared.state).session = Opened::No;
                return self.shared.wire.fail(id, failure);
            }
        };
        session.set_max_steps(self.max_turns);
        // What a terminal session prints as it starts: the project settings
        // waiting to be trusted, hooks that have a problem, each server.
        let mut notes = session.start_hooks(false);
        notes.extend(session.connect_configured_mcp());
        notes.extend(session.attach_mcp(servers));
        let client = self.client();
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let session_id = format!("sess_{nanos:x}{:x}", std::process::id());
        if client.read_file || client.write_file {
            files::install(Box::new(ClientFiles {
                shared: Arc::clone(&self.shared),
                session: session_id.clone(),
                client,
            }));
        }
        let result = json!({ "sessionId": session_id, "modes": modes(session.mode()) });
        // Open before it is answered, so the client can use the id at once.
        lock(&self.shared.state).session = Opened::Yes(session_id.clone());
        self.shared.wire.respond(id, result);
        let mut current = Current {
            session,
            id: session_id,
            root,
            commands: Value::Null,
        };
        advertise(&self.shared, &mut current);
        for note in notes {
            self.shared.note(&current.id, client, "info", &note);
        }
        self.current = Some(current);
    }

    /// Change to `cwd` and set a session up there.
    fn open_in(&mut self, cwd: &Path) -> Result<(Session, PathBuf), Failure> {
        std::env::set_current_dir(cwd)
            .map_err(|e| Failure::params(format!("\"cwd\": {}: {e}", cwd.display())))?;
        let internal = |e: String| Failure::new(rpc::code::INTERNAL_ERROR, e);
        // As the session will see it (symlinks resolved, as `main` has it).
        let root = std::env::current_dir().map_err(|e| internal(e.to_string()))?;
        let session = (self.open)().map_err(|e| internal(format!("{e:#}")))?;
        Ok((session, root))
    }

    /// Between jobs: output the session produces on its own — messages from
    /// other atelier instances, background jobs that ended.
    fn idle(&mut self) {
        let client = self.client();
        let Some(current) = &mut self.current else {
            return;
        };
        let mut ui = AcpUi::new(&self.shared, current, client, Scope::Idle, 0);
        current.session.poll_background(&mut ui);
        current.session.poll_peers(&mut ui);
        if let Some(notice) = crate::update::take_notice() {
            ui.notice(&notice);
        }
    }

    /// `session/prompt`: a slash command, or a turn of the conversation, to
    /// its end — the stop reason is the answer.
    fn prompt(&mut self, id: &Id, seq: u64, prompt: &present::Prompt) {
        let client = self.client();
        let shared = Arc::clone(&self.shared);
        // Accepted only with a session open (`session_named`).
        let Some(current) = &mut self.current else {
            return;
        };
        let mode = current.session.mode();
        let outcome = match shared.cancelled(seq) {
            // Cancelled while it waited its turn: it never starts.
            true => Ok("cancelled"),
            false => run_prompt(&shared, current, client, seq, prompt),
        };
        // What a command may have changed, that the client shows.
        if current.session.mode() != mode {
            let update = json!({
                "sessionUpdate": "current_mode_update",
                "currentModeId": current.session.mode().name(),
            });
            shared.update(&current.id, update);
        }
        advertise(&shared, current);
        let (used, _) = current.session.usage();
        if used > 0 {
            let update = json!({
                "sessionUpdate": "usage_update",
                "used": used,
                "size": current.session.context_limit(),
            });
            shared.update(&current.id, update);
        }
        // The prompt is over before its answer says so: a client that has
        // the answer can cancel only what comes next.
        lock(&shared.state).turns.retain(|t| t.seq != seq);
        match outcome {
            Ok(stop) => shared.wire.respond(id, json!({ "stopReason": stop })),
            Err(why) => {
                let failure = Failure::new(rpc::code::INTERNAL_ERROR, why);
                shared.wire.fail(id, failure);
            }
        }
    }
}

/// Run one prompt and return its stop reason, or why it failed.
fn run_prompt(
    shared: &Shared,
    current: &mut Current,
    client: Client,
    seq: u64,
    prompt: &present::Prompt,
) -> Result<&'static str, String> {
    let mut ui = AcpUi::new(shared, current, client, Scope::Command, seq);
    let session = &mut current.session;
    // A line that starts with a slash is a command, as at the terminal.
    let expanded = if prompt.text.trim_start().starts_with('/') {
        match agent::dispatch(session, &prompt.text, &mut ui) {
            Dispatch::Expanded(expansion) => Some(expansion),
            // `dispatch` only says so of a line with no slash.
            Dispatch::Prompt => Some(prompt.text.clone().into()),
            Dispatch::Handled => None,
            Dispatch::Quit => {
                ui.info("leaving is the editor's: close the thread to end this session");
                None
            }
            Dispatch::Config => {
                ui.info("the settings screen is the terminal interface's: edit atelier.toml");
                None
            }
        }
    } else {
        Some(prompt.text.clone().into())
    };
    ui.flush_command();
    let result = match &expanded {
        None => Ok(()),
        Some(expansion) => {
            ui.scope = Scope::Turn;
            // An image that can't be taken ends the turn before the model
            // is asked anything: without it the prompt is another question.
            let staged = session.stage_images(&prompt.images);
            staged.and_then(|_| session.send_expansion(expansion, &mut ui))
        }
    };
    ui.settle();
    match result {
        // Cancelled, however the turn then ended: the client must be able
        // to tell a cancellation it asked for from a failure.
        _ if ui.stopped || ui.interrupted() => Ok("cancelled"),
        Ok(()) => Ok("end_turn"),
        Err(e) if e.is::<StepLimit>() => Ok("max_turn_requests"),
        Err(e) => Err(format!("{e:#}")),
    }
}

/// Tell the client which slash commands there are, when the list is not
/// the one it has: at the start, and after a command that changed it (an
/// MCP server added, a project trusted).
fn advertise(shared: &Shared, current: &mut Current) {
    let builtin = COMMANDS.iter().map(|&(name, description, hint)| {
        (
            name.to_string(),
            description.to_string(),
            hint.map(str::to_string),
        )
    });
    let extra = current.session.extra_commands().into_iter();
    let extra = extra.map(|c| (c.name, c.description, c.hint));
    let commands: Vec<Value> = builtin
        .chain(extra)
        .map(|(name, description, hint)| {
            let mut command = json!({ "name": name, "description": description });
            if let Some(hint) = hint {
                command["input"] = json!({ "hint": hint });
            }
            command
        })
        .collect();
    let commands = Value::from(commands);
    if commands != current.commands {
        let update = json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": commands,
        });
        shared.update(&current.id, update);
        current.commands = commands;
    }
}

/// What an [`AcpUi`] is reporting on, which decides where plain lines go.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// A slash command: its lines are its output, shown as the answer.
    Command,
    /// A turn: the model's text is the answer, and lines from atelier are
    /// notices beside it.
    Turn,
    /// Nothing the client asked for.
    Idle,
}

/// How far a tool call the client was told about has got.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// The user was asked, and said this (`true`: go ahead).
    Asked(bool),
    Running,
}

/// A tool call that has been announced and has not ended.
struct Call {
    id: String,
    tool: String,
    stage: Stage,
    /// The change it makes, shown from the start (see [`present::diffs`]).
    diffs: Vec<Value>,
    /// Lines of progress so far (a subagent's steps).
    progress: String,
}

/// The [`Ui`] of an ACP session: each callback is a `session/update`, and
/// the two that need an answer are requests the session thread waits on.
struct AcpUi<'a> {
    shared: &'a Shared,
    client: Client,
    /// The session's id and directory.
    session: String,
    root: PathBuf,
    /// The project's permission rules as the job began, to know when
    /// "always" cannot be offered.
    permissions: Permissions,
    scope: Scope,
    /// The prompt this reports on, to ask whether it was cancelled.
    seq: u64,
    /// The tool calls announced and not ended, oldest first.
    calls: Vec<Call>,
    /// A command's output so far (see [`flush_command`](Self::flush_command)).
    lines: Vec<String>,
    /// The loop was told to stop (it asked, and the answer was yes).
    stopped: bool,
}

impl<'a> AcpUi<'a> {
    /// The interface for one job. It copies what it needs of the session,
    /// which stays free to be driven through it.
    fn new(shared: &'a Shared, current: &Current, client: Client, scope: Scope, seq: u64) -> Self {
        AcpUi {
            shared,
            client,
            session: current.id.clone(),
            root: current.root.clone(),
            permissions: current.session.settings().permissions.clone(),
            scope,
            seq,
            calls: Vec::new(),
            lines: Vec::new(),
            stopped: false,
        }
    }

    fn update(&self, update: Value) {
        self.shared.update(&self.session, update);
    }

    /// A piece of the answer (or of the reasoning behind it).
    fn chunk(&self, kind: &str, text: &str) {
        self.update(json!({ "sessionUpdate": kind, "content": present::text(text) }));
    }

    /// A command's output is lines of plain text, often in columns
    /// (`/context`, `/usage`), and a client renders the answer as Markdown,
    /// which would run them together. So they are collected and sent as one
    /// message: a single line as it is, several as a code block.
    fn flush_command(&mut self) {
        let lines = std::mem::take(&mut self.lines);
        match lines.as_slice() {
            [] => {}
            [line] => self.chunk("agent_message_chunk", &format!("{line}\n\n")),
            lines => {
                let block = format!("```\n{}\n```\n\n", lines.join("\n"));
                self.chunk("agent_message_chunk", &block);
            }
        }
    }

    /// How a tool call is described to the client: the fields `tool_call`
    /// and the `toolCall` of a permission request share.
    fn describe(&self, id: &str, tool: &str, args: &Value, status: &str) -> (Value, Vec<Value>) {
        let mut call = json!({
            "toolCallId": id,
            "title": present::title(tool, args),
            "name": tool,
            "kind": present::kind(tool),
            "status": status,
            "rawInput": args,
        });
        let locations = present::locations(tool, args, &self.root);
        if !locations.is_empty() {
            call["locations"] = Value::from(locations);
        }
        let diffs = present::diffs(tool, args, &self.root);
        if !diffs.is_empty() {
            call["content"] = Value::from(diffs.clone());
        }
        (call, diffs)
    }

    /// Announce a tool call (`tool_call`) and keep track of it.
    fn announce(&mut self, tool: &str, mut call: Value, diffs: Vec<Value>, stage: Stage) {
        let id = call["toolCallId"].as_str().unwrap_or_default().to_string();
        call["sessionUpdate"] = Value::from("tool_call");
        self.update(call);
        self.calls.push(Call {
            id,
            tool: tool.to_string(),
            stage,
            diffs,
            progress: String::new(),
        });
    }

    fn next_call_id(&self) -> String {
        let n = self.shared.next_call.fetch_add(1, Ordering::Relaxed);
        format!("call_{n}")
    }

    /// The end of the job: no call may be left looking as if it still ran.
    /// One is left open when what was approved was not a tool call of the
    /// loop — a command file's shell line (see `agent::dispatch_custom`).
    fn settle(&mut self) {
        for call in std::mem::take(&mut self.calls) {
            let status = match call.stage {
                Stage::Asked(false) => "failed",
                Stage::Asked(true) | Stage::Running => "completed",
            };
            self.update(json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": call.id,
                "status": status,
            }));
        }
    }

    /// Ask the client something on behalf of the running job and wait for
    /// the answer, or for the job to be cancelled.
    fn request(&mut self, method: &str, params: Value) -> Reply {
        let shared = self.shared;
        let stopped = || self.interrupted();
        shared
            .wire
            .call(method, params, Asked::Turn, None, stopped)
            .1
    }
}

impl Ui for AcpUi<'_> {
    fn reasoning(&mut self, text: &str) {
        self.chunk("agent_thought_chunk", text);
    }

    fn content(&mut self, text: &str) {
        self.chunk("agent_message_chunk", text);
    }

    /// The call starts running: `in_progress`, on the call the user was
    /// asked about if there was one, on a new one otherwise.
    fn tool_start(&mut self, name: &str, arguments: &str) {
        let asked = |c: &&mut Call| c.tool == name && c.stage == Stage::Asked(true);
        if let Some(call) = self.calls.iter_mut().find(asked) {
            call.stage = Stage::Running;
            let id = call.id.clone();
            return self.update(json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": id,
                "status": "in_progress",
            }));
        }
        let args = events::arguments(arguments);
        let (call, diffs) = self.describe(&self.next_call_id(), name, &args, "in_progress");
        self.announce(name, call, diffs, Stage::Running);
    }

    /// The call ended: `completed` or `failed`, with what it produced. A
    /// call that was refused before it started (a deny rule, plan mode, a
    /// hook) ends without having been announced, and is announced ended.
    fn tool_end(&mut self, name: &str, result: &str, ok: bool) {
        let status = if ok { "completed" } else { "failed" };
        // Calls that ran together end in the order they started.
        let Some(at) = self.calls.iter().position(|c| c.tool == name) else {
            return self.update(json!({
                "sessionUpdate": "tool_call",
                "toolCallId": self.next_call_id(),
                "title": present::title(name, &Value::Null),
                "name": name,
                "kind": present::kind(name),
                "status": status,
                "content": [present::output(result)],
            }));
        };
        let call = self.calls.remove(at);
        // `content` replaces what the call had: the diff stays with a
        // change that was made, and goes with one that was not.
        let mut content = if ok { call.diffs } else { Vec::new() };
        content.push(present::output(result));
        self.update(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": call.id,
            "status": status,
            "content": content,
        }));
        // The todo list is the agent's plan: say what it is now.
        if name == "todo"
            && ok
            && let Some(plan) = present::plan(&self.root)
        {
            self.update(plan);
        }
    }

    fn info(&mut self, text: &str) {
        match self.scope {
            Scope::Command => self.lines.push(text.to_string()),
            Scope::Turn | Scope::Idle => {
                self.shared.note(&self.session, self.client, "info", text);
            }
        }
    }

    /// A subagent's progress belongs to the `task` call that started it:
    /// the lines so far are that call's content while it runs.
    fn activity(&mut self, text: &str) {
        let running = |c: &&mut Call| c.stage == Stage::Running;
        let Some(call) = self.calls.iter_mut().rev().find(running) else {
            return self.shared.wire.log(text);
        };
        call.progress.push_str(text.trim_end());
        call.progress.push('\n');
        let update = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": call.id,
            "content": [present::text_content(&call.progress)],
        });
        self.update(update);
    }

    /// Something went wrong beside the answer. A client that shows notices
    /// gets one; for one that doesn't, the specification's fallback: the
    /// conversation, where the user is looking.
    fn notice(&mut self, text: &str) {
        if self.client.notices || self.scope == Scope::Idle {
            return self
                .shared
                .note(&self.session, self.client, "warning", text);
        }
        self.flush_command();
        self.chunk("agent_message_chunk", &format!("\n\n{}\n\n", text.trim()));
    }

    /// The parallel reviewer's note is commentary on the work, not the
    /// answer: a thought.
    fn subconscious(&mut self, text: &str) {
        self.chunk(
            "agent_thought_chunk",
            &format!("\n\n[review] {}\n\n", text.trim()),
        );
    }

    /// `session/request_permission`, for the call announced with it as
    /// `pending`. Anything but a clear yes is a no: an option that was not
    /// offered, an error, a cancelled turn.
    fn ask_approval(&mut self, tool: &str, arguments: &str) -> Approval {
        // What a command printed so far is what the user needs to have
        // read before being asked.
        self.flush_command();
        let args = events::arguments(arguments);
        let (mut call, diffs) = self.describe(&self.next_call_id(), tool, &args, "pending");
        let mut content = diffs.clone();
        for signal in crate::risk::signals(tool, arguments) {
            content.push(present::text_content(&format!("⚠ {signal}")));
        }
        let mut options = vec![json!({
            "optionId": "allow-once", "name": "Allow once", "kind": "allow_once",
        })];
        // "Always" saves the narrowest permission rule that lets this call
        // through. Where a rule asks every time, no saved rule would
        // change that, so it is not offered — and the rule is named.
        match crate::serve::asking_rule(&self.permissions, &self.root, tool, arguments) {
            Some(rule) => {
                let why = format!("the rule `{rule}` asks before this runs");
                content.push(present::text_content(&why));
            }
            None => options.push(json!({
                "optionId": "allow-always",
                "name": "Always allow calls like this one (saves a rule to atelier.toml)",
                "kind": "allow_always",
            })),
        }
        options.push(json!({
            "optionId": "reject-once", "name": "Reject", "kind": "reject_once",
        }));
        if !content.is_empty() {
            call["content"] = Value::from(content);
        }
        self.announce(tool, call.clone(), diffs, Stage::Asked(false));
        let params = json!({ "sessionId": self.session, "toolCall": call, "options": options });
        let approval = match self.request("session/request_permission", params) {
            Reply::Result(answer) => {
                let outcome = &answer["outcome"];
                let chosen = outcome["optionId"]
                    .as_str()
                    .filter(|_| outcome["outcome"] == "selected");
                match chosen {
                    Some("allow-once") => Approval::Once,
                    Some("allow-always") => Approval::Always,
                    _ => Approval::Deny,
                }
            }
            Reply::Error(_) | Reply::Cancelled(_) => Approval::Deny,
        };
        if let Some(call) = self.calls.last_mut() {
            call.stage = Stage::Asked(!matches!(approval, Approval::Deny));
        }
        approval
    }

    /// The `ask` tool's question, as a form elicitation — the protocol's
    /// way to ask the user for structured input — where the client offers
    /// that. Where it doesn't there is no one to ask, and the model is told
    /// to decide for itself, as in a headless run.
    fn ask_user(&mut self, question: &Question) -> Option<Answer> {
        if !self.client.form {
            return None;
        }
        let mut params = json!({
            "sessionId": self.session,
            "mode": "form",
            "message": question.question,
            "requestedSchema": present::question_form(question),
        });
        if let Some(call) = self.calls.iter().rev().find(|c| c.tool == "ask") {
            params["toolCallId"] = Value::from(call.id.clone());
        }
        match self.request("elicitation/create", params) {
            Reply::Result(answer) if answer["action"] == "accept" => {
                let given = present::question_answer(&answer["content"]);
                match events::answer(&given, question) {
                    Ok(answer) => Some(answer),
                    Err(why) => {
                        self.notice(&format!("elicitation/create: {why}; taken as declined"));
                        Some(Answer::Declined)
                    }
                }
            }
            // Declined, dismissed, or the turn was cancelled.
            Reply::Result(_) | Reply::Cancelled(_) => Some(Answer::Declined),
            Reply::Error(_) => None,
        }
    }

    fn interrupted(&mut self) -> bool {
        // A prompt can be cancelled; what the session does on its own
        // between prompts cannot.
        let stop = self.scope != Scope::Idle && self.shared.cancelled(self.seq);
        self.stopped |= stop;
        stop
    }
}

/// The editor as the place files are read from and written to (see
/// [`crate::tools::files`]): `fs/read_text_file` and `fs/write_text_file`,
/// each used only if the client said it serves it. This is how the agent
/// sees a buffer that is not saved yet, and how its edits show up in the
/// editor as they are made.
struct ClientFiles {
    shared: Arc<Shared>,
    session: String,
    client: Client,
}

impl ClientFiles {
    fn call(&self, method: &str, mut params: Value) -> Reply {
        params["sessionId"] = Value::from(self.session.clone());
        let timeout = Some(FILE_TIMEOUT);
        let wire = &self.shared.wire;
        wire.call(method, params, Asked::Work, timeout, || false).1
    }
}

impl FileAccess for ClientFiles {
    /// The client's text for the file, or the disk's when it has none to
    /// give: a path outside its workspace, a file that is not text, one
    /// that does not exist (the disk then says so in the usual words).
    fn read(&self, path: &Path) -> Option<io::Result<String>> {
        if !self.client.read_file {
            return None;
        }
        let params = json!({ "path": path.display().to_string() });
        match self.call("fs/read_text_file", params) {
            Reply::Result(result) => result["content"].as_str().map(|text| Ok(text.to_string())),
            Reply::Error(_) | Reply::Cancelled(_) => None,
        }
    }

    /// Through the client or not at all: a write it refuses is not made
    /// behind its back on the disk.
    fn write(&self, path: &Path, text: &str) -> Option<io::Result<()>> {
        if !self.client.write_file {
            return None;
        }
        let params = json!({ "path": path.display().to_string(), "content": text });
        Some(match self.call("fs/write_text_file", params) {
            Reply::Result(_) => Ok(()),
            Reply::Error(error) => {
                let why = error["message"].as_str().unwrap_or("no reason given");
                Err(io::Error::other(format!(
                    "the editor refused the write: {why}"
                )))
            }
            Reply::Cancelled(_) => Err(io::Error::other("the editor did not answer the write")),
        })
    }
}
