//! `atelier serve`: a session driven by another program, as JSON-RPC 2.0
//! over stdio.
//!
//! atelier's own interface is an input line, a status strip and scrollback,
//! on purpose. Anything richer — an editor integration, a web page, another
//! agent — lives outside this repository and talks to atelier through this
//! protocol, which is the whole reference for it together with
//! `docs/protocol.md`. It is `--print --output-format stream-json`
//! ([`crate::headless`]) grown the other half of a conversation: the same
//! events (their payloads are built once, in [`crate::events`]), plus what a
//! one-way stream cannot do — answer approval prompts, steer or interrupt a
//! running turn, run slash commands.
//!
//! One message per line, both ways; stdout carries protocol messages and
//! nothing else (logs go to stderr). One session per process.
//!
//! # Two threads
//!
//! The structure is the TUI's ([`crate::tui`]) with the pipe in place of the
//! keyboard: whoever owns the [`Session`] is busy for as long as a turn
//! runs, so someone else has to keep listening.
//!
//! - The **session thread** (the caller of [`run`]) owns the [`Session`]. It
//!   takes [`Job`]s off a channel one at a time — the handshake, a turn, a
//!   slash command, the shutdown — and, between jobs, polls for what the
//!   session produces on its own (messages from other instances, background
//!   jobs that ended), as the TUI's idle prompt does. Everything a job
//!   reports goes through [`ServeUi`], which writes one notification per
//!   [`Ui`] callback.
//! - The **reader thread** owns stdin. It answers at once whatever needs no
//!   session — a turn's id, `turn/steer` (a shared queue the turn drains
//!   between steps, like lines typed mid-turn), `turn/interrupt` (a flag the
//!   turn polls, like Ctrl-C), `session/status` (a snapshot the session
//!   thread publishes) — queues the rest as jobs, and hands the client's
//!   answers to whoever is waiting for them.
//!
//! That last part is why the session never reads stdin itself: an approval
//! prompt blocks the session thread inside [`Ui::ask_approval`] on a channel,
//! and the reader thread, still free, routes the client's answer to it — or
//! cancels it when the turn is interrupted. So a turn waiting on an answer
//! can still be steered, interrupted, or asked for its status.
//!
//! What is running ([`State::running`]) is decided by the reader thread
//! when it accepts a request and cleared by the session thread *before* it
//! reports the end (`turn/ended`, a command's result), so a client that has
//! seen the end can start the next thing without a race.
//!
//! # Leaving
//!
//! `shutdown` is a job, so it waits its turn and the session is dropped the
//! normal way. When the client is gone (EOF on stdin) or forces the
//! shutdown while a turn runs, the turn is interrupted first — and since a
//! turn only notices that at its next streamed chunk or tool boundary (the
//! same limit Ctrl-C has), a watchdog ends the process if it has not let go
//! after [`GRACE`].

use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::agent::{self, Approval, Dispatch, Session, StepLimit, Ui};
use crate::events;
use crate::permissions::{Decision, Policy};
use crate::settings::Permissions;
use crate::tools::ask::{Answer, Question};

/// The protocol version this build speaks (`initialize` reports it).
const PROTOCOL: u64 = 1;

/// How often the idle session looks for output of its own (see `idle`).
const IDLE_POLL: Duration = Duration::from_millis(200);

/// How long an interrupted turn has to let go once the client is gone or
/// has forced a shutdown, before the process ends regardless.
const GRACE: Duration = Duration::from_secs(5);

/// JSON-RPC error codes. The first five are the standard's; the rest are
/// this protocol's own, in the range the standard leaves to applications.
/// They are part of the contract (`docs/protocol.md`): never renumber.
mod code {
    /// A line that is not JSON.
    pub const PARSE_ERROR: i64 = -32700;
    /// JSON, but not a request, a notification or an answer.
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    /// A turn or a command is running, and this needs the session idle.
    pub const TURN_RUNNING: i64 = -32001;
    /// A request other than `initialize` came first.
    pub const NOT_INITIALIZED: i64 = -32002;
    /// `turn/steer` or `turn/interrupt` with no turn to act on.
    pub const NO_TURN: i64 = -32003;
    /// `shutdown` would stop background work; `force` goes ahead.
    pub const BACKGROUND_RUNNING: i64 = -32004;
    /// `initialize`, a second time.
    pub const ALREADY_INITIALIZED: i64 = -32005;
}

/// `atelier serve`, as asked for on the command line (see `cli`).
#[derive(Debug, PartialEq, Eq)]
pub struct Options {
    /// `--approval-timeout`: how long an `approval/request` or a
    /// `question/request` waits for the client's answer before it counts as
    /// "deny" / "no one to ask". `None` waits for as long as it takes: the
    /// client is usually showing the prompt to a person.
    pub approval_timeout: Option<Duration>,
}

/// A request's `id`, to answer it with; `None` for a notification, which
/// is carried out but gets no answer.
type Id = Option<Value>;

/// Serve `session` to the program on the other end of stdin/stdout until it
/// asks to stop or goes away, and return the exit status: 0, since how each
/// turn went was reported to the client as it happened.
pub fn run(session: Session, opts: Options) -> u8 {
    let shared = Arc::new(Shared {
        cwd: std::env::current_dir().unwrap_or_default(),
        approval_timeout: opts.approval_timeout,
        state: Mutex::new(State {
            phase: Phase::New,
            running: None,
            turns: 0,
            status: Value::Null,
            queue: VecDeque::new(),
        }),
        started: Condvar::new(),
        stop: AtomicBool::new(false),
        closed: AtomicBool::new(false),
        pending: Mutex::new(HashMap::new()),
        next_request: AtomicU64::new(1),
    });
    let (jobs_tx, jobs_rx) = mpsc::channel();
    let reader = Arc::clone(&shared);
    std::thread::spawn(move || reader.read_requests(&jobs_tx));
    Server {
        shared,
        session,
        ready: false,
    }
    .serve(&jobs_rx);
    0
}

/// Work for the session thread, in the order the client asked for it.
enum Job {
    Initialize {
        id: Id,
        client: Value,
    },
    Turn {
        turn: u64,
        text: String,
        images: Vec<String>,
        max_turns: Option<u32>,
    },
    Command {
        id: Id,
        line: String,
    },
    Shutdown {
        id: Id,
        force: bool,
    },
    /// stdin ended: the client is gone.
    Eof,
}

/// Where the handshake stands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Nothing but `initialize` is accepted.
    New,
    /// `initialize` is being carried out (servers are connecting). The
    /// reader thread waits this out, so requests a client wrote right behind
    /// its `initialize` are served in order rather than refused.
    Starting,
    Ready,
}

/// What the session thread is busy with.
#[derive(Clone, Copy)]
enum Running {
    Turn(u64),
    Command,
}

/// What both threads need to agree on, under one lock.
struct State {
    phase: Phase,
    running: Option<Running>,
    /// Turns started so far; the last one's id.
    turns: u64,
    /// The session as of the last time its thread was free to look (see
    /// [`Server::publish`]): what `session/status` answers with while the
    /// session itself is busy running a turn.
    status: Value,
    /// Messages for the running turn (`turn/steer`), until it takes them.
    queue: VecDeque<String>,
}

/// The answer to a request atelier made of the client.
enum Reply {
    /// The client's `result`.
    Result(Value),
    /// The client answered with an error: it cannot ask anyone.
    Error,
    /// No answer will come.
    Cancelled(Why),
}

/// Why a request to the client was given up on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Why {
    /// `--approval-timeout` ran out.
    Timeout,
    /// The turn was interrupted, or the client is gone.
    Interrupted,
}

impl Why {
    fn name(self) -> &'static str {
        match self {
            Why::Timeout => "timeout",
            Why::Interrupted => "interrupted",
        }
    }
}

/// A request that cannot be carried out: the JSON-RPC error to answer with.
struct Failure {
    code: i64,
    message: String,
    data: Option<Value>,
}

impl Failure {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Failure {
            code,
            message: message.into(),
            data: None,
        }
    }

    fn params(message: impl Into<String>) -> Self {
        Failure::new(code::INVALID_PARAMS, message)
    }

    /// The session is not idle.
    fn busy(running: Running) -> Self {
        match running {
            Running::Turn(turn) => Failure {
                code: code::TURN_RUNNING,
                message: format!(
                    "turn {turn} is running: steer it with turn/steer, or stop it with turn/interrupt"
                ),
                data: Some(json!({ "turn": turn })),
            },
            Running::Command => Failure::new(code::TURN_RUNNING, "a command is running"),
        }
    }
}

/// What the reader thread does about a request it accepted: answer now,
/// queue a job (which answers later, unless the answer is already here), or
/// both — in that order, so a turn's id reaches the client before the
/// turn's first event.
#[derive(Default)]
struct Accepted {
    reply: Option<Value>,
    job: Option<Job>,
    /// A forced shutdown: the process ends even if the session thread
    /// never gets to the job.
    watchdog: bool,
}

/// Lock a mutex, tolerating poison: the data is plain values, and a thread
/// that panicked holding it must not take the other one down in turn.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// State shared by the two threads.
struct Shared {
    /// The project root, for the session's description and for matching
    /// permission rules.
    cwd: PathBuf,
    approval_timeout: Option<Duration>,
    state: Mutex<State>,
    /// Signalled when the handshake is done (see [`Phase::Starting`]).
    started: Condvar,
    /// Set by `turn/interrupt`, read by the running turn through
    /// [`Ui::interrupted`], cleared when a turn is accepted.
    stop: AtomicBool,
    /// The client is gone: nothing more can be asked of it.
    closed: AtomicBool,
    /// Requests made of the client that it has not answered, by id.
    pending: Mutex<HashMap<u64, Sender<Reply>>>,
    next_request: AtomicU64,
}

impl Shared {
    /// Write one message as a line on stdout, flushed so the client sees it
    /// now. Built first and written in one call under the lock, so messages
    /// from the two threads never interleave. A closed stdout isn't worth a
    /// panic: the client went away, and stdin's end says so soon enough.
    fn write(&self, message: &Value) {
        let line = format!("{message}\n");
        let mut out = io::stdout().lock();
        let _ = out.write_all(line.as_bytes());
        let _ = out.flush();
    }

    fn notify(&self, method: &str, params: Value) {
        self.write(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    fn respond(&self, id: &Id, result: Value) {
        if let Some(id) = id {
            self.write(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
        }
    }

    fn fail(&self, id: &Id, failure: Failure) {
        let Some(id) = id else {
            // A notification gets no answer, not even a refusal.
            eprintln!("atelier serve: {}", failure.message);
            return;
        };
        let mut error = json!({ "code": failure.code, "message": failure.message });
        if let Some(data) = failure.data {
            error["data"] = data;
        }
        self.write(&json!({ "jsonrpc": "2.0", "id": id, "error": error }));
    }

    /// Give up on every request the client has not answered.
    fn cancel_pending(&self, why: Why) {
        for (_, waiter) in lock(&self.pending).drain() {
            let _ = waiter.send(Reply::Cancelled(why));
        }
    }

    /// The reader thread: every line of stdin, until it ends.
    fn read_requests(&self, jobs: &Sender<Job>) {
        let mut stdin = io::stdin().lock();
        let mut line = Vec::new();
        loop {
            line.clear();
            match stdin.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            // Bytes that aren't UTF-8 are a malformed line, not the end of
            // the stream: they fail to parse and are answered as such.
            let text = String::from_utf8_lossy(&line);
            let text = text.trim();
            if !text.is_empty() {
                self.incoming(text, jobs);
            }
        }
        // The client is gone: a shutdown it can no longer be asked to
        // confirm. Whatever is running is stopped, as far as it can be.
        self.closed.store(true, Ordering::Relaxed);
        self.stop.store(true, Ordering::Relaxed);
        self.cancel_pending(Why::Interrupted);
        let _ = jobs.send(Job::Eof);
        watchdog();
    }

    /// One line from the client: a request, a notification, or the answer
    /// to a request of ours.
    fn incoming(&self, line: &str, jobs: &Sender<Job>) {
        let null = Some(Value::Null);
        let message: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                let why = format!("parse error: {e}");
                return self.fail(&null, Failure::new(code::PARSE_ERROR, why));
            }
        };
        let invalid = |id: &Id, why: &str| {
            self.fail(id, Failure::new(code::INVALID_REQUEST, why));
        };
        let Some(object) = message.as_object() else {
            return invalid(
                &null,
                "a message must be a JSON object (batches are not supported)",
            );
        };
        let id = object.get("id").cloned();
        match object.get("method") {
            Some(Value::String(method)) => {
                let params = object.get("params").unwrap_or(&Value::Null);
                match self.accept(method, &id, params) {
                    Ok(accepted) => {
                        if let Some(reply) = accepted.reply {
                            self.respond(&id, reply);
                        }
                        if let Some(job) = accepted.job {
                            let _ = jobs.send(job);
                        }
                        if accepted.watchdog {
                            watchdog();
                        }
                    }
                    Err(failure) => self.fail(&id, failure),
                }
            }
            Some(_) => invalid(&id.or(null), "\"method\" must be a string"),
            None if object.contains_key("result") || object.contains_key("error") => {
                self.answered(object);
            }
            None => invalid(
                &id.or(null),
                "neither a request (\"method\") nor an answer (\"result\" or \"error\")",
            ),
        }
    }

    /// The client's answer to a request of ours: hand it to whoever waits.
    /// An answer nobody waits for (the request timed out or was cancelled
    /// meanwhile) is dropped; JSON-RPC has no way to refuse an answer.
    fn answered(&self, object: &Map<String, Value>) {
        let waiter = object
            .get("id")
            .and_then(Value::as_u64)
            .and_then(|id| lock(&self.pending).remove(&id));
        let Some(waiter) = waiter else {
            eprintln!("atelier serve: dropped an answer to a request that is not pending");
            return;
        };
        let reply = match object.get("result") {
            Some(result) => Reply::Result(result.clone()),
            None => Reply::Error,
        };
        let _ = waiter.send(reply);
    }

    /// Decide what to do about the request `method`, without doing anything
    /// slow: the state is checked and changed under its lock, the answer is
    /// written by the caller once the lock is released.
    fn accept(&self, method: &str, id: &Id, params: &Value) -> Result<Accepted, Failure> {
        if !(params.is_null() || params.is_object()) {
            return Err(Failure::params("\"params\" must be an object"));
        }
        let mut state = lock(&self.state);
        while state.phase == Phase::Starting {
            state = self.started.wait(state).unwrap_or_else(|p| p.into_inner());
        }
        if method == "initialize" {
            if state.phase != Phase::New {
                let why = "initialize was already called";
                return Err(Failure::new(code::ALREADY_INITIALIZED, why));
            }
            if !(params["protocol"].is_null() || params["protocol"].as_u64().is_some()) {
                return Err(Failure::params("\"protocol\" must be a version number"));
            }
            state.phase = Phase::Starting;
            let client = params["client"].clone();
            return Ok(Accepted {
                job: Some(Job::Initialize {
                    id: id.clone(),
                    client,
                }),
                ..Accepted::default()
            });
        }
        if state.phase != Phase::Ready {
            let why = "initialize first";
            return Err(Failure::new(code::NOT_INITIALIZED, why));
        }
        match method {
            "turn/start" => {
                let text = string_param(params, "text")?;
                let images = match &params["images"] {
                    Value::Null => Vec::new(),
                    Value::Array(list) => list
                        .iter()
                        .map(|v| v.as_str().map(str::to_string))
                        .collect::<Option<Vec<String>>>()
                        .ok_or_else(|| Failure::params("\"images\" must be strings"))?,
                    _ => return Err(Failure::params("\"images\" must be an array")),
                };
                let max_turns = match &params["max_turns"] {
                    Value::Null => None,
                    v => Some(
                        v.as_u64()
                            .and_then(|n| u32::try_from(n).ok())
                            .filter(|&n| n > 0)
                            .ok_or_else(|| {
                                Failure::params("\"max_turns\" must be a positive number")
                            })?,
                    ),
                };
                if let Some(running) = state.running {
                    return Err(Failure::busy(running));
                }
                state.turns += 1;
                let turn = state.turns;
                state.running = Some(Running::Turn(turn));
                // Here rather than on the session thread: an interrupt that
                // arrives before the turn has begun must still stop it.
                self.stop.store(false, Ordering::Relaxed);
                Ok(Accepted {
                    reply: Some(json!({ "turn": turn })),
                    job: Some(Job::Turn {
                        turn,
                        text,
                        images,
                        max_turns,
                    }),
                    ..Accepted::default()
                })
            }
            "turn/steer" => {
                let text = string_param(params, "text")?;
                let Some(Running::Turn(turn)) = state.running else {
                    let why = "no turn is running: start one with turn/start";
                    return Err(Failure::new(code::NO_TURN, why));
                };
                // Under the state lock, which the turn's end also takes: a
                // message accepted here is delivered or handed back in
                // `turn/ended`, never lost between the two.
                state.queue.push_back(text);
                Ok(Accepted {
                    reply: Some(json!({ "turn": turn, "queued": state.queue.len() })),
                    ..Accepted::default()
                })
            }
            "turn/interrupt" => {
                let Some(Running::Turn(turn)) = state.running else {
                    return Err(Failure::new(code::NO_TURN, "no turn is running"));
                };
                self.stop.store(true, Ordering::Relaxed);
                // A turn waiting on an answer is stopped there: the answer
                // is "deny".
                self.cancel_pending(Why::Interrupted);
                Ok(Accepted {
                    reply: Some(json!({ "turn": turn })),
                    ..Accepted::default()
                })
            }
            "command/run" => {
                let line = string_param(params, "line")?;
                if let Some(running) = state.running {
                    return Err(Failure::busy(running));
                }
                state.running = Some(Running::Command);
                Ok(Accepted {
                    job: Some(Job::Command {
                        id: id.clone(),
                        line,
                    }),
                    ..Accepted::default()
                })
            }
            "session/status" => {
                let mut status = state.status.clone();
                status["turn"] = match state.running {
                    Some(Running::Turn(turn)) => Value::from(turn),
                    _ => Value::Null,
                };
                status["busy"] = Value::Bool(state.running.is_some());
                status["queued"] = Value::from(state.queue.len());
                Ok(Accepted {
                    reply: Some(status),
                    ..Accepted::default()
                })
            }
            "shutdown" => {
                let force = match &params["force"] {
                    Value::Null => false,
                    Value::Bool(force) => *force,
                    _ => return Err(Failure::params("\"force\" must be true or false")),
                };
                if let Some(running) = state.running {
                    if !force {
                        return Err(Failure::busy(running));
                    }
                    self.stop.store(true, Ordering::Relaxed);
                    self.cancel_pending(Why::Interrupted);
                }
                Ok(Accepted {
                    job: Some(Job::Shutdown {
                        id: id.clone(),
                        force,
                    }),
                    watchdog: force,
                    ..Accepted::default()
                })
            }
            _ => Err(Failure::new(
                code::METHOD_NOT_FOUND,
                format!("unknown method '{method}'"),
            )),
        }
    }
}

/// A required, non-empty string parameter.
fn string_param(params: &Value, name: &str) -> Result<String, Failure> {
    match params[name].as_str() {
        Some(s) if !s.trim().is_empty() => Ok(s.to_string()),
        _ => Err(Failure::params(format!(
            "\"{name}\" must be a non-empty string"
        ))),
    }
}

/// End the process after [`GRACE`], whatever the session thread is doing:
/// for when nobody is left to wait for it. Returning from [`run`] before
/// then ends the process the normal way, and this with it.
fn watchdog() {
    std::thread::spawn(|| {
        std::thread::sleep(GRACE);
        // What `main` does on the way out, which this exit skips.
        crate::tools::bash::kill_all();
        std::process::exit(0);
    });
}

/// The session thread's side: the session and what it serves it to.
struct Server {
    shared: Arc<Shared>,
    session: Session,
    /// The handshake is done: the client may be sent events.
    ready: bool,
}

impl Server {
    /// Carry out jobs until one of them ends the session.
    fn serve(mut self, jobs: &Receiver<Job>) {
        loop {
            match jobs.recv_timeout(IDLE_POLL) {
                Ok(Job::Initialize { id, client }) => self.initialize(&id, &client),
                Ok(Job::Turn {
                    turn,
                    text,
                    images,
                    max_turns,
                }) => self.turn(turn, &text, &images, max_turns),
                Ok(Job::Command { id, line }) => self.command(&id, &line),
                Ok(Job::Shutdown { id, force }) => {
                    if self.shutdown(&id, force) {
                        return;
                    }
                }
                Ok(Job::Eof) | Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => self.idle(),
            }
        }
    }

    /// Publish the session as `session/status` reports it, for the reader
    /// thread to answer with while this one is busy.
    fn publish(&self) {
        let status = self.status();
        lock(&self.shared.state).status = status;
    }

    fn status(&self) -> Value {
        let (context_tokens, output_tokens) = self.session.usage();
        let mut status = self.describe();
        status["usage"] = json!({
            "context_tokens": context_tokens,
            "output_tokens": output_tokens,
        });
        status["context_limit"] = Value::from(self.session.context_limit());
        status["background"] = Value::from(self.session.background_tasks());
        status
    }

    /// What the session is: where, which model through which backend, in
    /// which permission mode.
    fn describe(&self) -> Value {
        // The label names the model actually asked (`chatgpt:<model>` when
        // signed in with ChatGPT), which the configured one may not be.
        let model = self.session.backend_label();
        let backend = if self.session.is_claude() {
            "claude"
        } else if model.starts_with("chatgpt:") {
            "chatgpt"
        } else {
            "openai"
        };
        json!({
            "cwd": self.shared.cwd.display().to_string(),
            "model": model,
            "backend": backend,
            "mode": self.session.mode().name(),
        })
    }

    /// `initialize`: connect the configured MCP servers (so the tools the
    /// answer lists are the ones the model will be offered), describe the
    /// session, and open the gate for everything else.
    fn initialize(&mut self, id: &Id, client: &Value) {
        if let Some(name) = client["name"].as_str() {
            let version = client["version"].as_str().unwrap_or("?");
            eprintln!("atelier serve: client {name} {version}");
        }
        let connected = self.session.connect_configured_mcp();
        let mut session = self.describe();
        session["tools"] = Value::from(self.session.tool_names());
        let result = json!({
            "protocol": PROTOCOL,
            "atelier": { "version": env!("CARGO_PKG_VERSION") },
            "session": session,
            "capabilities": {
                "steer": true,
                "interrupt": true,
                "approvals": true,
                "questions": true,
                "images": true,
                "commands": true,
                "approval_timeout": self.shared.approval_timeout.map(|t| t.as_secs()),
            },
        });
        {
            let mut state = lock(&self.shared.state);
            state.phase = Phase::Ready;
            state.status = self.status();
            // Answered before the reader thread is let go (it needs this
            // lock), so the handshake's result precedes the answer to
            // anything sent behind it.
            self.shared.respond(id, result);
        }
        self.ready = true;
        self.shared.started.notify_all();
        // What connecting had to say, now that the client may be told.
        let mut ui = ServeUi::new(&self.shared, &self.session, Scope::Idle);
        for line in connected {
            ui.info(&line);
        }
    }

    /// Between jobs: output the session produces on its own — messages from
    /// other atelier instances, background jobs that ended, claude reporting
    /// on background work — goes out as `event/idle`.
    fn idle(&mut self) {
        if !self.ready {
            return;
        }
        let mut ui = ServeUi::new(&self.shared, &self.session, Scope::Idle);
        self.session.poll_background(&mut ui);
        self.session.poll_peers(&mut ui);
        if let Some(notice) = crate::update::take_notice() {
            ui.notice(&notice);
        }
        self.publish();
    }

    /// The job is over: the session is free again. Returns the steering
    /// messages the turn never took. Called *before* the end is reported, so
    /// a client that reads the report can start the next thing at once.
    fn release(&self) -> Vec<String> {
        let status = self.status();
        let mut state = lock(&self.shared.state);
        state.running = None;
        state.status = status;
        state.queue.drain(..).collect()
    }

    /// `turn/start`: one prompt to its end, reported as `turn/ended`.
    fn turn(&mut self, turn: u64, text: &str, images: &[String], max_turns: Option<u32>) {
        let started = Instant::now();
        let output_before = self.session.usage().1;
        self.session.set_max_steps(max_turns);
        let mut ui = ServeUi::new(&self.shared, &self.session, Scope::Turn(turn));
        // An image that can't be read ends the turn before the model is
        // asked anything: sending the prompt without it would answer a
        // different question.
        let (result, steps) = match self.session.stage_images(images) {
            Ok(_) => {
                let result = self.session.send(text, &mut ui);
                (result, self.session.steps())
            }
            Err(e) => (Err(e), 0),
        };
        let (status, error) = match &result {
            Ok(()) if ui.stopped => ("interrupted", None),
            Ok(()) => ("completed", None),
            Err(e) if e.is::<StepLimit>() => ("max_turns", Some(e.to_string())),
            Err(e) => ("error", Some(format!("{e:#}"))),
        };
        let (context_tokens, output_after) = self.session.usage();
        let mut ended = json!({
            "turn": turn,
            "status": status,
            "steps": steps,
            "duration_ms": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "usage": {
                "context_tokens": context_tokens,
                "output_tokens": output_after.saturating_sub(output_before),
            },
        });
        if let Some(error) = error {
            ended["error"] = Value::from(error);
        }
        let undelivered = self.release();
        if !undelivered.is_empty() {
            ended["undelivered"] = Value::from(undelivered);
        }
        self.shared.notify("turn/ended", ended);
    }

    /// `command/run`: a slash command, with its output as `event/info`. What
    /// an interface would do next is left to the client: a line that is a
    /// prompt (or expands into one) is handed back rather than sent.
    fn command(&mut self, id: &Id, line: &str) {
        let mut ui = ServeUi::new(&self.shared, &self.session, Scope::Command);
        let result = match agent::dispatch(&mut self.session, line, &mut ui) {
            Dispatch::Handled => json!({ "handled": true, "kind": "handled" }),
            // Leaving is the client's call: `shutdown`.
            Dispatch::Quit => json!({ "handled": false, "kind": "quit" }),
            Dispatch::Prompt => json!({ "handled": false, "kind": "prompt", "text": line.trim() }),
            Dispatch::Expanded(text) => {
                // The expansion's `model:` is passed along; the client decides
                // whether to start a turn with it.
                json!({
                    "handled": false,
                    "kind": "expanded",
                    "text": text.prompt,
                    "model": text.model,
                })
            }
            // The settings screen is the terminal interface's.
            Dispatch::Config => json!({ "handled": false, "kind": "config" }),
        };
        self.release();
        self.shared.respond(id, result);
    }

    /// `shutdown`: whether to leave. Exiting ends claude and the background
    /// work in it, so with work running it takes `force` — the rule the
    /// interactive exit has, with the confirmation in the client's hands.
    fn shutdown(&mut self, id: &Id, force: bool) -> bool {
        let background = self.session.background_tasks();
        if background > 0 && !force {
            let failure = Failure {
                code: code::BACKGROUND_RUNNING,
                message: format!(
                    "{background} background task(s) are running and would be stopped; \
                     pass \"force\": true to shut down anyway"
                ),
                data: Some(json!({ "background": background })),
            };
            self.shared.fail(id, failure);
            return false;
        }
        self.shared.respond(id, json!({}));
        true
    }
}

/// What a [`ServeUi`] is reporting on, which decides the envelope.
#[derive(Clone, Copy)]
enum Scope {
    /// A turn: `event/<kind>` with the turn's id.
    Turn(u64),
    /// A slash command: `event/<kind>`, no turn.
    Command,
    /// Nothing the client asked for: `event/idle`, the kind inside.
    Idle,
}

/// The [`Ui`] of a served session: each callback is a notification, and the
/// two that need an answer are requests the session thread waits on.
struct ServeUi<'a> {
    shared: &'a Shared,
    scope: Scope,
    /// The project's permission rules as the job began, to name the rule
    /// behind an approval prompt.
    permissions: Permissions,
    /// The loop was told to stop (it asked, and the answer was yes).
    stopped: bool,
}

impl<'a> ServeUi<'a> {
    /// The interface for one job. It borrows only what the two threads
    /// share, so the session stays free to be driven through it.
    fn new(shared: &'a Shared, session: &Session, scope: Scope) -> Self {
        ServeUi {
            shared,
            scope,
            permissions: session.settings().permissions.clone(),
            stopped: false,
        }
    }

    fn emit(&self, kind: &str, mut params: Value) {
        match self.scope {
            Scope::Turn(turn) => {
                params["turn"] = Value::from(turn);
                self.shared.notify(&format!("event/{kind}"), params);
            }
            Scope::Command => self.shared.notify(&format!("event/{kind}"), params),
            Scope::Idle => {
                params["kind"] = Value::from(kind);
                self.shared.notify("event/idle", params);
            }
        }
    }

    /// Ask the client something and wait for its answer — for as long as
    /// `--approval-timeout` allows, or until the turn is interrupted. When
    /// the request is given up on after it was sent, the client is told
    /// (`request/cancelled`) so it can take its prompt down.
    fn request(&mut self, method: &str, mut params: Value) -> Reply {
        let shared = self.shared;
        if let Scope::Turn(turn) = self.scope {
            params["turn"] = Value::from(turn);
        }
        let id = shared.next_request.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        lock(&shared.pending).insert(id, tx);
        // Checked once the request is registered: an interrupt either finds
        // it there and cancels it, or came first and is seen here.
        if shared.closed.load(Ordering::Relaxed) || self.interrupted() {
            lock(&shared.pending).remove(&id);
            return Reply::Cancelled(Why::Interrupted);
        }
        shared.write(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let reply = match shared.approval_timeout {
            Some(limit) => rx
                .recv_timeout(limit)
                .unwrap_or(Reply::Cancelled(Why::Timeout)),
            None => rx.recv().unwrap_or(Reply::Cancelled(Why::Interrupted)),
        };
        lock(&shared.pending).remove(&id);
        if let Reply::Cancelled(why) = &reply {
            shared.notify(
                "request/cancelled",
                json!({ "id": id, "reason": why.name() }),
            );
        }
        reply
    }

    /// The `ask` rule that makes this call prompt, if it is a rule that
    /// does (rather than the tool's own default, or the mode).
    fn asking_rule(&self, tool: &str, arguments: &str) -> Option<String> {
        let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
        let verdict = Policy::load(&self.permissions).decide(tool, &args, &self.shared.cwd);
        match verdict.decision {
            Decision::Ask => verdict.by.map(|by| by.rule),
            _ => None,
        }
    }
}

impl Ui for ServeUi<'_> {
    fn reasoning(&mut self, text: &str) {
        self.emit("reasoning", events::text(text));
    }

    fn content(&mut self, text: &str) {
        self.emit("text", events::text(text));
    }

    fn tool_start(&mut self, name: &str, arguments: &str) {
        self.emit("tool_start", events::tool_start(name, arguments));
    }

    fn tool_end(&mut self, name: &str, result: &str, ok: bool) {
        self.emit("tool_end", events::tool_end(name, result, ok));
    }

    fn info(&mut self, text: &str) {
        self.emit("info", events::text(text));
    }

    fn activity(&mut self, text: &str) {
        self.emit("activity", events::text(text));
    }

    fn notice(&mut self, text: &str) {
        self.emit("notice", events::text(text));
    }

    fn subconscious(&mut self, text: &str) {
        self.emit("review", events::text(text));
    }

    /// `approval/request`. Anything but a clear yes is a no: an answer that
    /// can't be read, an error, a timeout, an interrupt.
    fn ask_approval(&mut self, tool: &str, arguments: &str) -> Approval {
        let mut params = json!({
            "tool": tool,
            "arguments": events::arguments(arguments),
            "risk": crate::risk::signals(tool, arguments),
        });
        if let Some(rule) = self.asking_rule(tool, arguments) {
            params["rule"] = Value::from(rule);
        }
        match self.request("approval/request", params) {
            Reply::Result(answer) => match answer["decision"].as_str() {
                Some("once") => Approval::Once,
                Some("always") => Approval::Always,
                Some("deny") => Approval::Deny,
                _ => {
                    self.notice(
                        "approval/request: the answer needs \"decision\": \"once\", \
                         \"always\" or \"deny\"; taken as deny",
                    );
                    Approval::Deny
                }
            },
            Reply::Error | Reply::Cancelled(_) => Approval::Deny,
        }
    }

    /// `question/request`. A client that answers with an error, or not in
    /// time, leaves no one to ask, and the model is told to decide for
    /// itself; an interrupt or an answer that doesn't fit the question
    /// counts as the user declining to answer.
    fn ask_user(&mut self, question: &Question) -> Option<Answer> {
        match self.request("question/request", events::question(question)) {
            Reply::Result(answer) if answer["declined"] == true => Some(Answer::Declined),
            Reply::Result(answer) => match events::answer(&answer, question) {
                Ok(answer) => Some(answer),
                Err(why) => {
                    self.notice(&format!("question/request: {why}; taken as declined"));
                    Some(Answer::Declined)
                }
            },
            Reply::Cancelled(Why::Interrupted) => Some(Answer::Declined),
            Reply::Error | Reply::Cancelled(Why::Timeout) => None,
        }
    }

    fn take_queued(&mut self) -> Vec<String> {
        match self.scope {
            Scope::Turn(_) => lock(&self.shared.state).queue.drain(..).collect(),
            Scope::Command | Scope::Idle => Vec::new(),
        }
    }

    fn queued_delivered(&mut self, text: &str) {
        self.emit("delivered", events::text(text));
    }

    fn interrupted(&mut self) -> bool {
        // The flag is a turn's: a command that follows an interrupted turn
        // is not itself interrupted.
        let stop = matches!(self.scope, Scope::Turn(_)) && self.shared.stop.load(Ordering::Relaxed);
        self.stopped |= stop;
        stop
    }
}
