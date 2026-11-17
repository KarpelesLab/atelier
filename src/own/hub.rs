//! The owner's socket: what a session's owner answers to the atelier that
//! was started next to it, and — for an owner that is the terminal
//! interface — the session itself, shared with the remotes attached to it.
//!
//! JSON-RPC 2.0, one message per line, as everywhere else in atelier (see
//! [`crate::rpc`], `docs/protocol.md`). Two methods are every owner's:
//!
//! | method          | params        | result                                   |
//! |-----------------|---------------|------------------------------------------|
//! | `owner/status`  | —             | who the owner is and what it is doing    |
//! | `session/yield` | `{by}`        | `{}`: it will stop, save and exit        |
//!
//! A thread accepts connections and one thread serves each; the session's
//! own thread is never waited for, so an owner in the middle of a turn
//! answers as fast as an idle one.
//!
//! # Remotes
//!
//! A connection that says `initialize` is a *remote*: a second front end of
//! the owner's live session, speaking the embedding protocol of
//! [`crate::serve`] — the same methods, events and error codes — with the
//! one difference that the session is not its own. This module is that
//! protocol's server for a session several front ends share; the session
//! itself stays where it was, with the interface that owns it (the *front*,
//! [`Hub::serve_remotes`]), and the two meet here:
//!
//! - **What the session reports goes to everybody.** The front calls
//!   [`Hub::event`] next to drawing each thing itself, and every remote is
//!   sent the notification. The events of the turn in progress are kept
//!   ([`Replay`]), so a remote that attaches in the middle of one is sent
//!   it from its start; what came before that it asks for
//!   (`session/history`), answered from the copy the front leaves here
//!   whenever the session comes to rest ([`Hub::publish`]).
//! - **What a remote asks is answered here when it can be** — the status,
//!   the history, steering and interrupting a turn (noted for the front) —
//!   by the connection's own thread, turn or no turn. What needs the
//!   session (a prompt, a slash command, a query, a setting) is left in the
//!   [inbox](Hub::inbox) for the front, which looks there wherever it looks
//!   at its keyboard, does it as if it had been typed, and answers
//!   ([`Hub::respond`]).
//! - **What the session asks is put to everybody**, and whoever answers
//!   first has answered: [`Hub::request`] sends an `approval/request` or a
//!   `question/request` to every remote and hands the front a channel their
//!   answers arrive on, to wait on next to its own keyboard;
//!   [`Hub::settle`] then tells the others (`request/cancelled`, reason
//!   `answered`, and by whom).
//! - **A command's terminal** is shown to the remotes that asked for it
//!   ([`Hub::command_event`]), and what they type reaches the command
//!   through the queue the front already polls for its own keys
//!   ([`Hub::command_input`]). What is typed is passed on and kept nowhere:
//!   no event repeats it, to the owner's terminal or to another remote.
//!
//! A remote that goes away takes nothing with it: the turn it started goes
//! on, and what it had not answered is still everybody else's to answer.
//!
//! # Yielding
//!
//! `session/yield` is the first step of a take-over (see
//! [`super::takeover`]). The owner notes who asked ([`Hub::yielding`], which
//! every interface looks at where it waits), wakes what has to be woken
//! ([`Hub::on_yield`]), and from there ends as it would on a quit: the turn
//! in progress is interrupted, the session saved and dropped, background
//! work stopped. An interface that has not let go after
//! [`YIELD_GRACE`](super::YIELD_GRACE) — a tool that will not return, a
//! model that has not answered — is not waited for: background work is
//! stopped and the process ends there. The remotes are told at once
//! (`session/closed`) and let go.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::Record;
use crate::rpc::{self, Failure, Id, lock};
use crate::serve::{self, code, history::Page, query::Query};
use crate::tools::terminal::{CommandEvent, CommandInput};

/// How long a newcomer waits for an owner's answer.
const ANSWER: Duration = Duration::from_secs(2);
/// How long a remote that does not read may hold up what it is sent before
/// it is let go.
const WRITE: Duration = Duration::from_secs(5);
/// The most of a turn's events kept for a remote that attaches during it.
const REPLAY_BYTES: usize = 2 << 20;

type Hook = Box<dyn Fn() + Send + Sync>;

/// What a remote asked for that the front has to do, or to know.
pub(crate) enum Inbound {
    /// A remote attached.
    Joined { remote: u64, name: String },
    /// A remote went away.
    Left { remote: u64 },
    /// `turn/start`: a prompt to send as it is. Answered by
    /// [`Hub::begin_turn`], or refused with [`Hub::fail`].
    Prompt {
        remote: u64,
        id: Id,
        text: String,
        images: Vec<String>,
    },
    /// `command/run`: a line as if typed. `start`: when it is a prompt, or
    /// stands for one, the turn is to be started rather than handed back.
    Line {
        remote: u64,
        id: Id,
        line: String,
        start: bool,
    },
    /// `turn/steer`, already answered: a message for the running turn.
    Steer { remote: u64, text: String },
    /// `turn/interrupt`, already answered.
    Interrupt { remote: u64 },
    /// A query (`models/list`, …), to answer from the session.
    Query { remote: u64, id: Id, query: Query },
    /// `settings/get` or `settings/set`.
    Settings {
        remote: u64,
        id: Id,
        set: bool,
        params: Value,
    },
}

/// A remote's answer to a request put to all ([`Hub::request`]).
pub(crate) enum Answered {
    Result(Value),
    Error,
}

/// A request put to the remotes: its id, and their answers as they come,
/// each with the remote it is from.
pub(crate) struct Asked {
    pub id: u64,
    pub answers: Receiver<(u64, Answered)>,
}

/// The session as the front last left it, for what is answered here.
#[derive(Default)]
pub(crate) struct Snapshot {
    /// `initialize`'s `session`.
    pub describe: Value,
    /// `session/status`, less what is live.
    pub status: Value,
    /// The conversation's items (`session/history`).
    pub history: Vec<Value>,
    pub stored: bool,
    pub claude_session: Option<String>,
}

/// Who did something, as events name them: the owner's own terminal, or a
/// remote by its number.
fn origin(by: Option<u64>) -> String {
    match by {
        None => "terminal".to_string(),
        Some(remote) => format!("remote-{remote}"),
    }
}

/// One connection's way out.
struct Remote {
    id: u64,
    out: Mutex<UnixStream>,
    /// It asked for the running command's terminal (`initialize`).
    terminal: AtomicBool,
    gone: AtomicBool,
}

impl Remote {
    /// Write one message. A remote that cannot be written to — gone, or
    /// not reading — is let go of: its reader sees the socket close.
    fn send(&self, line: &str) {
        if self.gone.load(Ordering::Relaxed) {
            return;
        }
        let mut out = lock(&self.out);
        if out.write_all(format!("{line}\n").as_bytes()).is_err() {
            self.gone.store(true, Ordering::Relaxed);
            let _ = out.shutdown(std::net::Shutdown::Both);
        }
    }

    fn respond(&self, id: &Id, result: Value) {
        if let Some(id) = id {
            self.send(&json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string());
        }
    }

    fn fail(&self, id: &Id, failure: Failure) {
        let Some(id) = id else { return };
        let mut error = json!({ "code": failure.code, "message": failure.message });
        if let Some(data) = failure.data {
            error["data"] = data;
        }
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "error": error }).to_string());
    }

    fn close(&self) {
        self.gone.store(true, Ordering::Relaxed);
        let _ = lock(&self.out).shutdown(std::net::Shutdown::Both);
    }
}

/// What the session is busy with, as far as remotes are concerned.
#[derive(Clone, Copy)]
enum Running {
    Turn(u64),
    /// A command, a query, or a remote's request the front has yet to
    /// look at.
    Busy,
}

fn busy(running: Running) -> Failure {
    match running {
        Running::Turn(turn) => Failure {
            code: code::TURN_RUNNING,
            message: format!(
                "turn {turn} is running: steer it with turn/steer, or stop it with turn/interrupt"
            ),
            data: Some(json!({ "turn": turn })),
        },
        Running::Busy => Failure::new(code::TURN_RUNNING, "a command is running"),
    }
}

/// The running turn's notifications so far, for a remote that attaches in
/// the middle of it.
#[derive(Default)]
struct Replay {
    /// `turn/started`, kept whatever else is dropped.
    started: Option<String>,
    /// Each line, and whether it is for remotes with a terminal only.
    lines: VecDeque<(String, bool)>,
    bytes: usize,
    /// The oldest lines were dropped to stay under [`REPLAY_BYTES`].
    cut: bool,
}

impl Replay {
    fn push(&mut self, line: &str, terminal: bool) {
        self.bytes += line.len();
        self.lines.push_back((line.to_string(), terminal));
        while self.bytes > REPLAY_BYTES {
            let Some((dropped, _)) = self.lines.pop_front() else {
                break;
            };
            self.bytes -= dropped.len();
            self.cut = true;
        }
    }
}

/// A request the remotes have been sent and nobody has answered yet.
struct Pending {
    line: String,
    answers: Sender<(u64, Answered)>,
}

/// What remotes typed to the running command, and who of all front ends
/// has taken its keyboard (0 is the owner's own terminal).
#[derive(Default)]
struct Keys {
    queue: Vec<(u64, CommandInput)>,
    holders: BTreeSet<u64>,
    /// The end of the command's output that stops inside a character.
    partial: Vec<u8>,
}

/// This process's ownership of the session: the lock, the socket, and what
/// the interfaces told it about themselves.
pub(super) struct Hub {
    record: Record,
    /// Held for as long as the process lives: this is the ownership.
    _lock: File,
    socket: Option<PathBuf>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// A turn is in progress (as the session itself says, whoever drives).
    turn: bool,
    /// Who took the session over.
    yielding: Option<String>,
    agents: Option<Box<dyn Fn() -> usize + Send + Sync>>,
    client: Option<String>,
    on_yield: Vec<Arc<Hook>>,
    on_forced_exit: Vec<Arc<Hook>>,

    /// The interface takes remotes ([`Hub::serve_remotes`]).
    front: bool,
    remotes: BTreeMap<u64, Arc<Remote>>,
    connections: u64,
    running: Option<Running>,
    turns: u64,
    snapshot: Snapshot,
    inbox: VecDeque<Inbound>,
    pending: BTreeMap<u64, Pending>,
    requests: u64,
    replay: Replay,
    /// Steering messages accepted and not yet delivered.
    queued: usize,
    /// While a command runs on a terminal.
    command: Option<Keys>,
}

impl State {
    /// The remotes a message goes to.
    fn targets(&self, terminal: bool) -> Vec<Arc<Remote>> {
        let wanted = |r: &&Arc<Remote>| !terminal || r.terminal.load(Ordering::Relaxed);
        self.remotes.values().filter(wanted).cloned().collect()
    }
}

/// Open the owner's socket for the process `pid`. `None` — said once on
/// stderr — when there is nowhere to put it: the session is then owned all
/// the same, and can be taken over by signal only.
pub(super) fn bind(pid: u32) -> Option<(UnixListener, PathBuf)> {
    let dir = crate::coord::runtime_dir()?;
    sweep(&dir);
    match crate::listen::bind_unix(&dir.join(format!("session-{pid}.sock"))) {
        Ok(bound) => Some(bound),
        Err(e) => {
            eprintln!("warning: this session has no control socket: {e:#}");
            None
        }
    }
}

/// Take away the sockets of owners that are gone: one that was killed
/// could not remove its own, and the next atelier in its project, which
/// would, may never come.
fn sweep(dir: &std::path::Path) {
    use std::os::unix::fs::FileTypeExt;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let pid = name.to_str().and_then(|n| {
            n.strip_prefix("session-")?
                .strip_suffix(".sock")?
                .parse()
                .ok()
        });
        let Some(pid) = pid else { continue };
        if entry.file_type().is_ok_and(|t| t.is_socket()) && super::proc::start_token(pid).is_none()
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Connect to the socket the owner `record` describes.
pub(super) fn connect(record: &Record) -> Result<UnixStream> {
    let Some(path) = &record.socket else {
        bail!("the owner has no control socket");
    };
    let Some((short, _handle)) = crate::coord::short_socket(std::path::Path::new(path)) else {
        bail!("{path} is too long for a socket path");
    };
    UnixStream::connect(short).with_context(|| format!("connecting to {path}"))
}

/// Ask the owner `record` describes one thing, and wait for its answer.
pub(super) fn ask(record: &Record, method: &str, params: Value) -> Result<Value> {
    let stream = connect(record)?;
    stream.set_read_timeout(Some(ANSWER))?;
    stream.set_write_timeout(Some(ANSWER))?;
    let request = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    (&stream).write_all(format!("{request}\n").as_bytes())?;
    let mut answer = String::new();
    BufReader::new(&stream).read_line(&mut answer)?;
    let answer: Value = serde_json::from_str(&answer).context("the owner's answer")?;
    if let Some(e) = answer.get("error") {
        bail!("{}", e["message"].as_str().unwrap_or("error"));
    }
    Ok(answer["result"].clone())
}

fn notification(method: &str, params: Value) -> String {
    json!({ "jsonrpc": "2.0", "method": method, "params": params }).to_string()
}

/// What a connection's thread does about a request.
enum Accepted {
    /// Answer now.
    Reply(Value),
    /// The front answers, or it was answered already.
    Later,
    /// Answer, and close the connection.
    Bye(Value),
}

impl Hub {
    pub(super) fn start(
        record: Record,
        lock: File,
        socket: Option<(UnixListener, PathBuf)>,
    ) -> Arc<Hub> {
        let (listener, path) = socket.unzip();
        let hub = Arc::new(Hub {
            record,
            _lock: lock,
            socket: path,
            state: Mutex::new(State::default()),
        });
        if let Some(listener) = listener {
            let accepting = Arc::clone(&hub);
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let hub = Arc::clone(&accepting);
                    std::thread::spawn(move || hub.connection(stream));
                }
            });
        }
        hub
    }

    /// One connection, to its end.
    fn connection(self: Arc<Self>, stream: UnixStream) {
        let Ok(out) = stream.try_clone() else {
            return;
        };
        let _ = out.set_write_timeout(Some(WRITE));
        let id = {
            let mut state = lock(&self.state);
            state.connections += 1;
            state.connections
        };
        let remote = Arc::new(Remote {
            id,
            out: Mutex::new(out),
            terminal: AtomicBool::new(false),
            gone: AtomicBool::new(false),
        });
        let mut attached = false;
        let null = Some(Value::Null);
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                remote.fail(&null, Failure::new(rpc::code::PARSE_ERROR, "parse error"));
                continue;
            };
            let id: Id = message.get("id").cloned();
            let Some(method) = message["method"].as_str() else {
                // An answer to something asked of the remotes, or nothing.
                if message.get("result").is_some() || message.get("error").is_some() {
                    self.answered(&remote, &message);
                } else {
                    let why = "neither a request nor an answer";
                    remote.fail(
                        &id.or(null.clone()),
                        Failure::new(rpc::code::INVALID_REQUEST, why),
                    );
                }
                continue;
            };
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            match self.accept(&remote, &mut attached, method, &id, &params) {
                Ok(Accepted::Reply(result)) => remote.respond(&id, result),
                Ok(Accepted::Later) => {}
                Ok(Accepted::Bye(result)) => {
                    remote.respond(&id, result);
                    break;
                }
                Err(failure) => remote.fail(&id, failure),
            }
        }
        if attached {
            self.detach(&remote);
        }
        remote.close();
    }

    /// Decide what to do about a request, without doing anything slow.
    fn accept(
        self: &Arc<Self>,
        remote: &Arc<Remote>,
        attached: &mut bool,
        method: &str,
        id: &Id,
        params: &Value,
    ) -> Result<Accepted, Failure> {
        if !(params.is_null() || params.is_object()) {
            return Err(Failure::params("\"params\" must be an object"));
        }
        // Every owner's, attached or not.
        match method {
            "owner/status" => return Ok(Accepted::Reply(self.status())),
            "session/yield" => {
                let by = params["by"].as_str().unwrap_or("another process");
                self.yield_to(by);
                let grace = super::YIELD_GRACE.as_millis() as u64;
                return Ok(Accepted::Reply(json!({ "grace_ms": grace })));
            }
            "initialize" => return self.attach(remote, attached, id, params),
            _ => {}
        }
        if !*attached {
            return Err(Failure::new(code::NOT_INITIALIZED, "initialize first"));
        }
        let mut state = lock(&self.state);
        let from = remote.id;
        let free = |state: &State| state.running.map_or(Ok(()), |r| Err(busy(r)));
        match method {
            "turn/start" => {
                let text = serve::string_param(params, "text")?;
                let images = match &params["images"] {
                    Value::Null => Vec::new(),
                    Value::Array(list) => list
                        .iter()
                        .map(|v| v.as_str().map(str::to_string))
                        .collect::<Option<Vec<String>>>()
                        .ok_or_else(|| Failure::params("\"images\" must be strings"))?,
                    _ => return Err(Failure::params("\"images\" must be an array")),
                };
                free(&state)?;
                state.running = Some(Running::Busy);
                state.inbox.push_back(Inbound::Prompt {
                    remote: from,
                    id: id.clone(),
                    text,
                    images,
                });
                Ok(Accepted::Later)
            }
            "command/run" => {
                let line = serve::string_param(params, "line")?;
                let start = match &params["start"] {
                    Value::Null => false,
                    Value::Bool(start) => *start,
                    _ => return Err(Failure::params("\"start\" must be true or false")),
                };
                free(&state)?;
                state.running = Some(Running::Busy);
                state.inbox.push_back(Inbound::Line {
                    remote: from,
                    id: id.clone(),
                    line,
                    start,
                });
                Ok(Accepted::Later)
            }
            "turn/steer" => {
                let text = serve::string_param(params, "text")?;
                let Some(Running::Turn(turn)) = state.running else {
                    let why = "no turn is running: start one with turn/start";
                    return Err(Failure::new(code::NO_TURN, why));
                };
                state.queued += 1;
                state.inbox.push_back(Inbound::Steer { remote: from, text });
                Ok(Accepted::Reply(
                    json!({ "turn": turn, "queued": state.queued }),
                ))
            }
            "turn/interrupt" => {
                let Some(Running::Turn(turn)) = state.running else {
                    return Err(Failure::new(code::NO_TURN, "no turn is running"));
                };
                state.inbox.push_back(Inbound::Interrupt { remote: from });
                Ok(Accepted::Reply(json!({ "turn": turn })))
            }
            "command/input" => {
                let input = serve::command_input(params)?;
                let Some(keys) = state.command.as_mut() else {
                    let why = "no command is running that can be typed to";
                    return Err(Failure::new(code::NO_COMMAND, why));
                };
                keys.queue.extend(input.into_iter().map(|i| (from, i)));
                Ok(Accepted::Reply(json!({})))
            }
            "session/status" => {
                let mut status = state.snapshot.status.clone();
                status["turn"] = match state.running {
                    Some(Running::Turn(turn)) => Value::from(turn),
                    _ => Value::Null,
                };
                status["busy"] = Value::Bool(state.running.is_some());
                status["queued"] = Value::from(state.queued);
                status["remotes"] = Value::from(state.remotes.len());
                Ok(Accepted::Reply(status))
            }
            // From the copy the front left when the session last came to
            // rest: while a turn runs, that is the conversation up to it,
            // and the turn itself is what the events are.
            "session/history" => {
                let page = Page::parse(params)?;
                let mut result = page.cut(state.snapshot.history.clone());
                result["stored"] = Value::Bool(state.snapshot.stored);
                if let Some(id) = &state.snapshot.claude_session {
                    result["claude_session"] = Value::from(id.as_str());
                }
                Ok(Accepted::Reply(result))
            }
            "settings/get" | "settings/set" => {
                free(&state)?;
                state.running = Some(Running::Busy);
                state.inbox.push_back(Inbound::Settings {
                    remote: from,
                    id: id.clone(),
                    set: method == "settings/set",
                    params: params.clone(),
                });
                Ok(Accepted::Later)
            }
            // Leaving is all a remote's `shutdown` is: the session is not
            // its to end.
            "shutdown" => Ok(Accepted::Bye(json!({}))),
            _ => {
                let Some(query) = Query::parse(method, params)? else {
                    return Err(Failure::new(
                        rpc::code::METHOD_NOT_FOUND,
                        format!("unknown method '{method}'"),
                    ));
                };
                free(&state)?;
                state.running = Some(Running::Busy);
                state.inbox.push_back(Inbound::Query {
                    remote: from,
                    id: id.clone(),
                    query,
                });
                Ok(Accepted::Later)
            }
        }
    }

    /// `initialize`: the connection becomes a remote. Answered, brought up
    /// to date and added to the remotes under one lock, so that it misses
    /// nothing and is sent nothing twice.
    fn attach(
        &self,
        remote: &Arc<Remote>,
        attached: &mut bool,
        id: &Id,
        params: &Value,
    ) -> Result<Accepted, Failure> {
        if *attached {
            let why = "initialize was already called";
            return Err(Failure::new(code::ALREADY_INITIALIZED, why));
        }
        if !(params["protocol"].is_null() || params["protocol"].as_u64().is_some()) {
            return Err(Failure::params("\"protocol\" must be a version number"));
        }
        let terminal = match &params["terminal"] {
            Value::Null => false,
            Value::Bool(terminal) => *terminal,
            _ => return Err(Failure::params("\"terminal\" must be true or false")),
        };
        let mut state = lock(&self.state);
        if !state.front {
            return Err(Failure::new(
                code::NO_REMOTE,
                format!(
                    "the session here is run by {} (pid {}), which takes no remote",
                    describe_kind(self.record.kind),
                    self.record.pid
                ),
            ));
        }
        if state.yielding.is_some() {
            let why = "the session is being taken over";
            return Err(Failure::new(code::NO_REMOTE, why));
        }
        remote.terminal.store(terminal, Ordering::Relaxed);
        let turn = match state.running {
            Some(Running::Turn(turn)) => Value::from(turn),
            _ => Value::Null,
        };
        remote.respond(
            id,
            json!({
                "protocol": serve::PROTOCOL,
                "atelier": { "version": env!("CARGO_PKG_VERSION") },
                "session": state.snapshot.describe,
                "capabilities": {
                    "steer": true,
                    "interrupt": true,
                    "approvals": true,
                    "questions": true,
                    "images": true,
                    "commands": true,
                    "history": true,
                    "file_changes": true,
                    "command_input": crate::tools::terminal::supported(),
                    "queries": serve::query::METHODS,
                    "approval_timeout": Value::Null,
                    "shared": true,
                },
                "shared": {
                    "remote": remote.id,
                    "owner": {
                        "pid": self.record.pid,
                        "kind": self.record.kind.name(),
                        "tty": self.record.tty,
                    },
                    "busy": state.running.is_some(),
                    "turn": turn,
                    "remotes": state.remotes.len() + 1,
                },
            }),
        );
        // The turn in progress, from its start; then what is waiting for
        // an answer.
        if let Some(started) = &state.replay.started {
            remote.send(started);
            if state.replay.cut {
                let text = "(the beginning of this turn is not shown: it was too long to keep)";
                let mut params = json!({ "text": text });
                params["turn"] = turn;
                remote.send(&notification("event/notice", params));
            }
            for (line, for_terminal) in &state.replay.lines {
                if !for_terminal || terminal {
                    remote.send(line);
                }
            }
        }
        for pending in state.pending.values() {
            remote.send(&pending.line);
        }
        state.remotes.insert(remote.id, Arc::clone(remote));
        let name = match params["client"]["name"].as_str() {
            Some(name) => format!(
                "{name} {}",
                params["client"]["version"].as_str().unwrap_or("?")
            ),
            None => "a client".to_string(),
        };
        state.inbox.push_back(Inbound::Joined {
            remote: remote.id,
            name,
        });
        *attached = true;
        Ok(Accepted::Later)
    }

    /// A remote went away: whatever it held is let go.
    fn detach(&self, remote: &Remote) {
        let mut state = lock(&self.state);
        if state.remotes.remove(&remote.id).is_none() {
            return;
        }
        if let Some(keys) = state.command.as_mut()
            && keys.holders.contains(&remote.id)
        {
            keys.queue.push((remote.id, CommandInput::Detach));
        }
        state.inbox.push_back(Inbound::Left { remote: remote.id });
    }

    /// A remote's answer to a request put to all: for whoever waits.
    fn answered(&self, remote: &Remote, message: &Value) {
        let state = lock(&self.state);
        let pending = message["id"].as_u64().and_then(|id| state.pending.get(&id));
        if let Some(pending) = pending {
            let answer = match message.get("result") {
                Some(result) => Answered::Result(result.clone()),
                None => Answered::Error,
            };
            let _ = pending.answers.send((remote.id, answer));
        }
    }

    /// Who this owner is and what it has going.
    fn status(&self) -> Value {
        let jobs = crate::tools::bash::job_list();
        let state = lock(&self.state);
        json!({
            "pid": self.record.pid,
            "kind": self.record.kind.name(),
            "since": self.record.since,
            "tty": self.record.tty,
            "version": self.record.version,
            "busy": state.turn || matches!(state.running, Some(Running::Turn(_))),
            "jobs": jobs.iter().filter(|j| j.running).count(),
            "agents": state.agents.as_ref().map_or(0, |count| count()),
            "client": state.client,
            "remote": state.front,
            "remotes": state.remotes.len(),
        })
    }

    /// The session was asked for by `by`: note it, wake the interface, and
    /// see to it that the process ends even if the interface never lets go.
    fn yield_to(self: &Arc<Self>, by: &str) {
        let hooks = {
            let mut state = lock(&self.state);
            if state.yielding.is_some() {
                return;
            }
            state.yielding = Some(by.to_string());
            state.on_yield.clone()
        };
        self.close_remotes("taken_over", Some(by));
        for hook in hooks {
            hook();
        }
        let hub = Arc::clone(self);
        let by = by.to_string();
        std::thread::spawn(move || {
            std::thread::sleep(super::YIELD_GRACE);
            let hooks = lock(&hub.state).on_forced_exit.clone();
            for hook in hooks {
                hook();
            }
            eprintln!("{}", super::taken_over_line(&by));
            hub.close();
            // What `main` does on the way out, which this exit skips.
            crate::tools::bash::kill_all();
            crate::lsp::kill_all();
            std::process::exit(0);
        });
    }

    /// Tell the remotes the session is over for them, and let them go.
    fn close_remotes(&self, reason: &str, by: Option<&str>) {
        let remotes = std::mem::take(&mut lock(&self.state).remotes);
        let mut closed = json!({ "reason": reason });
        if let Some(by) = by {
            closed["by"] = Value::from(by);
        }
        let line = notification("session/closed", closed);
        for remote in remotes.values() {
            remote.send(&line);
            remote.close();
        }
    }

    pub(super) fn yielding(&self) -> Option<String> {
        lock(&self.state).yielding.clone()
    }

    pub(super) fn on_yield(&self, hook: Hook) {
        lock(&self.state).on_yield.push(Arc::new(hook));
    }

    pub(super) fn on_forced_exit(&self, hook: Hook) {
        lock(&self.state).on_forced_exit.push(Arc::new(hook));
    }

    pub(super) fn set_turn(&self, running: bool) {
        lock(&self.state).turn = running;
    }

    pub(super) fn watch_agents(&self, count: Box<dyn Fn() -> usize + Send + Sync>) {
        lock(&self.state).agents = Some(count);
    }

    pub(super) fn note_client(&self, name: &str) {
        lock(&self.state).client = Some(name.to_string());
    }

    /// The process is on its way out: the remotes are told, the socket
    /// goes now, the lock when the process does. The record stays, true
    /// until then and stale after.
    pub(super) fn close(&self) {
        self.close_remotes("ended", None);
        if let Some(path) = &self.socket {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// A kind of run, in the words of a refusal.
fn describe_kind(kind: super::Kind) -> &'static str {
    match kind {
        super::Kind::Terminal => "a terminal interface that is not up yet",
        super::Kind::Repl => "the line REPL",
        super::Kind::Print => "a --print run",
        super::Kind::Serve => "atelier serve",
        super::Kind::Acp => "atelier acp",
    }
}

/// The front's side: what the interface that owns the session does here.
impl Hub {
    /// The interface takes remotes from now on: it will look at the
    /// [inbox](Self::inbox) and report through [`event`](Self::event).
    pub(super) fn serve_remotes(&self) {
        lock(&self.state).front = true;
    }

    /// How many remotes are attached.
    pub(super) fn remotes(&self) -> usize {
        lock(&self.state).remotes.len()
    }

    /// Leave the session as it is now, for what is answered without it.
    pub(super) fn publish(&self, snapshot: Snapshot) {
        lock(&self.state).snapshot = snapshot;
    }

    /// What remotes asked for since last looked.
    pub(super) fn inbox(&self) -> Vec<Inbound> {
        lock(&self.state).inbox.drain(..).collect()
    }

    fn remote(&self, id: u64) -> Option<Arc<Remote>> {
        lock(&self.state).remotes.get(&id).cloned()
    }

    pub(super) fn respond(&self, remote: u64, id: &Id, result: Value) {
        if let Some(remote) = self.remote(remote) {
            remote.respond(id, result);
        }
    }

    pub(super) fn fail(&self, remote: u64, id: &Id, failure: Failure) {
        if let Some(remote) = self.remote(remote) {
            remote.fail(id, failure);
        }
    }

    /// The refusal for a request that found the session busy after all.
    pub(super) fn fail_busy(&self, remote: u64, id: &Id) {
        let running = lock(&self.state).running.unwrap_or(Running::Busy);
        self.fail(remote, id, busy(running));
    }

    /// What a remote asked for is done, and it was not a turn: the session
    /// is free again.
    pub(super) fn release(&self) {
        let mut state = lock(&self.state);
        if matches!(state.running, Some(Running::Busy)) {
            state.running = None;
        }
    }

    /// A turn starts, with the prompt `text` from `by`. Returns its id.
    /// `answer` is the remote's request that started it, when one did: it
    /// is answered — with the turn's id added — before anybody hears of
    /// the turn.
    pub(super) fn begin_turn(
        &self,
        text: &str,
        by: Option<u64>,
        answer: Option<(&Id, Value)>,
    ) -> u64 {
        let (turn, started, targets) = {
            let mut state = lock(&self.state);
            state.turns += 1;
            let turn = state.turns;
            state.running = Some(Running::Turn(turn));
            state.queued = 0;
            let started = json!({ "turn": turn, "text": text, "from": origin(by) });
            let started = notification("turn/started", started);
            state.replay = Replay {
                started: Some(started.clone()),
                ..Replay::default()
            };
            (turn, started, state.targets(false))
        };
        if let (Some(remote), Some((id, mut result))) = (by, answer) {
            result["turn"] = Value::from(turn);
            if let Some(remote) = targets.iter().find(|r| r.id == remote) {
                remote.respond(id, result);
            }
        }
        for remote in targets {
            remote.send(&started);
        }
        turn
    }

    /// The turn is over: `ended` is `turn/ended`'s params, `snapshot` the
    /// session after it. One step, so that a remote attaching now gets the
    /// turn either as events or as history, never both.
    pub(super) fn end_turn(&self, ended: Value, snapshot: Snapshot) {
        let targets = {
            let mut state = lock(&self.state);
            state.running = None;
            state.queued = 0;
            state.replay = Replay::default();
            state.snapshot = snapshot;
            state.targets(false)
        };
        let line = notification("turn/ended", ended);
        for remote in targets {
            remote.send(&line);
        }
    }

    /// Something the session reports, for every remote: `event/<kind>` of
    /// the running turn (kept for whoever attaches during it), of a
    /// remote's command, or — between the two — `event/idle`. `terminal`:
    /// only for remotes that show a command's terminal.
    pub(super) fn event(&self, kind: &str, mut params: Value, terminal: bool) {
        let (line, targets) = {
            let mut state = lock(&self.state);
            let line = match state.running {
                Some(Running::Turn(turn)) => {
                    params["turn"] = Value::from(turn);
                    notification(&format!("event/{kind}"), params)
                }
                Some(Running::Busy) => notification(&format!("event/{kind}"), params),
                None if state.remotes.is_empty() => return,
                None => {
                    params["kind"] = Value::from(kind);
                    notification("event/idle", params)
                }
            };
            if matches!(state.running, Some(Running::Turn(_))) {
                state.replay.push(&line, terminal);
                if kind == "delivered" {
                    state.queued = state.queued.saturating_sub(1);
                }
            }
            (line, state.targets(terminal))
        };
        for remote in targets {
            remote.send(&line);
        }
    }

    /// Put `method` (`approval/request`, `question/request`) to every
    /// remote, now and as they attach, until it is [settled](Self::settle).
    pub(super) fn request(&self, method: &str, mut params: Value) -> Asked {
        let (tx, answers) = mpsc::channel();
        let (id, line, targets) = {
            let mut state = lock(&self.state);
            state.requests += 1;
            let id = state.requests;
            if let Some(Running::Turn(turn)) = state.running {
                params["turn"] = Value::from(turn);
            }
            let line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
            let line = line.to_string();
            let pending = Pending {
                line: line.clone(),
                answers: tx,
            };
            state.pending.insert(id, pending);
            (id, line, state.targets(false))
        };
        for remote in targets {
            remote.send(&line);
        }
        Asked { id, answers }
    }

    /// The request `id` has its answer, from `by`: nobody else is asked
    /// any more, and all are told (`request/cancelled`, with `how` it went).
    pub(super) fn settle(&self, id: u64, by: Option<u64>, how: Value) {
        let targets = {
            let mut state = lock(&self.state);
            if state.pending.remove(&id).is_none() {
                return;
            }
            state.targets(false)
        };
        let mut cancelled = json!({ "id": id, "reason": "answered", "by": origin(by) });
        if let (Some(fields), Some(more)) = (cancelled.as_object_mut(), how.as_object()) {
            fields.extend(more.clone());
        }
        let line = notification("request/cancelled", cancelled);
        for remote in targets {
            remote.send(&line);
        }
    }

    /// What the running command has to tell, for the remotes that show
    /// its terminal. Its start opens the queue `command/input` fills.
    pub(super) fn command_event(&self, event: &CommandEvent) {
        match event {
            CommandEvent::Started {
                command,
                interactive,
            } => {
                lock(&self.state).command = Some(Keys::default());
                let started = json!({ "command": command, "interactive": interactive });
                self.event("command_started", started, true);
            }
            CommandEvent::Output(bytes) => {
                // Whole characters only; the start of one waits for its end.
                let data: Vec<u8> = {
                    let mut state = lock(&self.state);
                    let Some(keys) = state.command.as_mut() else {
                        return;
                    };
                    keys.partial.extend_from_slice(bytes);
                    let whole = match std::str::from_utf8(&keys.partial) {
                        Err(e) if e.error_len().is_none() => e.valid_up_to(),
                        _ => keys.partial.len(),
                    };
                    keys.partial.drain(..whole).collect()
                };
                if !data.is_empty() {
                    let data = String::from_utf8_lossy(&data);
                    self.event("command_output", json!({ "data": data }), true);
                }
            }
            CommandEvent::InputWanted { prompt } => {
                self.event("input_wanted", json!({ "prompt": prompt }), true);
            }
            CommandEvent::Ended => {
                lock(&self.state).command = None;
                self.event("command_ended", json!({}), true);
            }
        }
    }

    /// What the command is to be given: `local`, the owner's own keys, and
    /// what the remotes typed. Several front ends may hold the keyboard at
    /// once; the command is told it is taken by the first and given back
    /// by the last.
    pub(super) fn command_input(&self, local: Vec<CommandInput>) -> Vec<CommandInput> {
        let mut state = lock(&self.state);
        let Some(keys) = state.command.as_mut() else {
            return local;
        };
        let remote = std::mem::take(&mut keys.queue);
        if local.is_empty() && remote.is_empty() {
            return local;
        }
        let all = local.into_iter().map(|i| (0, i)).chain(remote);
        let mut out = Vec::new();
        for (who, input) in all {
            match input {
                CommandInput::Attach => {
                    if keys.holders.insert(who) && keys.holders.len() == 1 {
                        out.push(CommandInput::Attach);
                    }
                }
                CommandInput::Detach => {
                    if keys.holders.remove(&who) && keys.holders.is_empty() {
                        out.push(CommandInput::Detach);
                    }
                }
                other => out.push(other),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub() -> Arc<Hub> {
        let path = std::env::temp_dir().join(format!(
            "atelier-hub-{}-{:?}.lock",
            std::process::id(),
            std::thread::current().id()
        ));
        let lock = File::create(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let record = Record {
            pid: std::process::id(),
            started: String::new(),
            since: 0,
            kind: super::super::Kind::Terminal,
            tty: None,
            socket: None,
            version: String::new(),
        };
        Hub::start(record, lock, None)
    }

    #[test]
    fn a_turns_events_are_kept_up_to_a_size_and_its_start_always() {
        let mut replay = Replay {
            started: Some("start".into()),
            ..Replay::default()
        };
        let line = "x".repeat(REPLAY_BYTES / 4);
        for _ in 0..3 {
            replay.push(&line, false);
        }
        assert_eq!((replay.lines.len(), replay.cut), (3, false));
        replay.push(&line, false);
        replay.push(&line, true);
        // The oldest went; what is kept fits, and its start is still there.
        assert!(replay.cut);
        assert_eq!(replay.lines.len(), 4);
        assert!(replay.bytes <= REPLAY_BYTES);
        assert_eq!(replay.started.as_deref(), Some("start"));
        assert!(replay.lines.back().unwrap().1);
    }

    #[test]
    fn events_outside_a_turn_are_kept_for_nobody() {
        let hub = hub();
        hub.event("info", json!({"text": "nobody hears this"}), false);
        assert!(lock(&hub.state).replay.lines.is_empty());
        let turn = hub.begin_turn("hello", None, None);
        assert_eq!(turn, 1);
        hub.event("text", json!({"text": "hi"}), false);
        hub.event("delivered", json!({"text": "more"}), false);
        {
            let state = lock(&hub.state);
            let started: Value =
                serde_json::from_str(state.replay.started.as_ref().unwrap()).unwrap();
            assert_eq!(
                started["params"],
                json!({"turn": 1, "text": "hello", "from": "terminal"})
            );
            let first: Value = serde_json::from_str(&state.replay.lines[0].0).unwrap();
            assert_eq!(first["method"], "event/text");
            assert_eq!(first["params"], json!({"turn": 1, "text": "hi"}));
            assert_eq!(state.replay.lines.len(), 2);
        }
        hub.end_turn(json!({"turn": 1}), Snapshot::default());
        let state = lock(&hub.state);
        assert!(state.replay.started.is_none() && state.replay.lines.is_empty());
        assert!(state.running.is_none());
    }

    /// Several front ends may hold a command's keyboard: the command is
    /// told it is taken by the first and given back by the last, and gets
    /// every key in between.
    #[test]
    fn the_keyboard_is_the_commands_while_anybody_holds_it() {
        let hub = hub();
        // Without a command there is nothing to merge with.
        assert!(hub.command_input(Vec::new()).is_empty());
        hub.command_event(&CommandEvent::Started {
            command: "read x",
            interactive: false,
        });
        let typed = |hub: &Hub, who: u64, input: CommandInput| {
            lock(&hub.state)
                .command
                .as_mut()
                .unwrap()
                .queue
                .push((who, input));
        };
        let names = |inputs: Vec<CommandInput>| -> Vec<String> {
            inputs
                .iter()
                .map(|i| match i {
                    CommandInput::Attach => "attach".to_string(),
                    CommandInput::Detach => "detach".to_string(),
                    CommandInput::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
                    CommandInput::Resize { .. } => "resize".to_string(),
                })
                .collect()
        };
        typed(&hub, 2, CommandInput::Attach);
        typed(&hub, 2, CommandInput::Bytes(b"a".to_vec()));
        // The owner's terminal takes it too: the command already knows.
        let out = hub.command_input(vec![CommandInput::Attach]);
        assert_eq!(names(out), ["attach", "a"]);
        typed(&hub, 2, CommandInput::Detach);
        assert!(names(hub.command_input(Vec::new())).is_empty());
        let out = hub.command_input(vec![
            CommandInput::Bytes(b"b".to_vec()),
            CommandInput::Detach,
        ]);
        assert_eq!(names(out), ["b", "detach"]);
        // Given back twice is given back once.
        assert!(names(hub.command_input(vec![CommandInput::Detach])).is_empty());
        hub.command_event(&CommandEvent::Ended);
        assert!(lock(&hub.state).command.is_none());
    }

    #[test]
    fn a_request_is_pending_until_it_is_settled() {
        let hub = hub();
        let asked = hub.request("approval/request", json!({"tool": "bash"}));
        assert_eq!(asked.id, 1);
        assert!(lock(&hub.state).pending.contains_key(&1));
        hub.settle(asked.id, Some(2), json!({"decision": "once"}));
        assert!(lock(&hub.state).pending.is_empty());
        // Settled once.
        hub.settle(asked.id, None, json!({"decision": "deny"}));
        assert_eq!(origin(Some(2)), "remote-2");
        assert_eq!(origin(None), "terminal");
    }
}
