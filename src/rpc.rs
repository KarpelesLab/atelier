//! JSON-RPC 2.0 over stdio, one message per line: the wire under the two
//! protocols atelier speaks to a program that launched it — its own
//! ([`crate::serve`]) and the Agent Client Protocol ([`crate::acp`]).
//!
//! Both have the same shape: a reader thread owns stdin, a session thread
//! does the work, and either may write to stdout — answers, notifications,
//! and requests *of* the client, whose answers come back on stdin among the
//! client's own requests. What that takes is here, once: writing a message
//! whole ([`Wire::write`]), reading lines and telling a request from an
//! answer ([`Wire::read`]), and waiting for the answer to a request made of
//! the client ([`Wire::call`]) in a way the other thread can cut short.
//!
//! What a method *means* is the protocol's, and stays in its module.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{Map, Value, json};

/// The standard's error codes. A protocol adds its own next to them.
pub mod code {
    /// A line that is not JSON.
    pub const PARSE_ERROR: i64 = -32700;
    /// JSON, but not a request, a notification or an answer.
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
}

/// A request's `id`, to answer it with; `None` for a notification, which
/// is carried out but gets no answer.
pub type Id = Option<Value>;

/// A request that cannot be carried out: the JSON-RPC error to answer with.
pub struct Failure {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl Failure {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Failure {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn params(message: impl Into<String>) -> Self {
        Failure::new(code::INVALID_PARAMS, message)
    }
}

/// The answer to a request atelier made of the client.
pub enum Reply {
    /// The client's `result`.
    Result(Value),
    /// The client answered with an error (its `error` object).
    Error(Value),
    /// No answer will come.
    Cancelled(Why),
}

/// Why a request to the client was given up on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// The time allowed for an answer ran out.
    Timeout,
    /// The turn was interrupted, or the client is gone.
    Interrupted,
}

impl Why {
    pub fn name(self) -> &'static str {
        match self {
            Why::Timeout => "timeout",
            Why::Interrupted => "interrupted",
        }
    }
}

/// What a request of the client belongs to, which decides who can give up
/// on it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Part of the running turn — an approval, a question: interrupting the
    /// turn answers it ([`Wire::cancel_turn`]).
    Turn,
    /// Work the turn is in the middle of — a file the client is writing —
    /// which only the client's answer, or its going away, ends.
    Work,
}

/// Someone waiting for the client's answer.
struct Waiter {
    reply: Sender<Reply>,
    scope: Scope,
}

/// Lock a mutex, tolerating poison: the data is plain values, and a thread
/// that panicked holding it must not take the other one down in turn.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// This process's end of the conversation: stdout to write on, and the
/// requests made of the client that it has not answered yet.
pub struct Wire {
    /// The subcommand, for the lines logged to stderr (`atelier serve: …`).
    name: &'static str,
    pending: Mutex<HashMap<u64, Waiter>>,
    next_request: AtomicU64,
    /// The client is gone: nothing more can be asked of it.
    closed: AtomicBool,
}

impl Wire {
    pub fn new(name: &'static str) -> Self {
        Wire {
            name,
            pending: Mutex::new(HashMap::new()),
            next_request: AtomicU64::new(1),
            closed: AtomicBool::new(false),
        }
    }

    /// Write one message as a line on stdout, flushed so the client sees it
    /// now. Built first and written in one call under the lock, so messages
    /// from the two threads never interleave. A closed stdout isn't worth a
    /// panic: the client went away, and stdin's end says so soon enough.
    pub fn write(&self, message: &Value) {
        let line = format!("{message}\n");
        let mut out = io::stdout().lock();
        let _ = out.write_all(line.as_bytes());
        let _ = out.flush();
    }

    pub fn notify(&self, method: &str, params: Value) {
        self.write(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    pub fn respond(&self, id: &Id, result: Value) {
        if let Some(id) = id {
            self.write(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
        }
    }

    pub fn fail(&self, id: &Id, failure: Failure) {
        let Some(id) = id else {
            // A notification gets no answer, not even a refusal.
            self.log(&failure.message);
            return;
        };
        let mut error = json!({ "code": failure.code, "message": failure.message });
        if let Some(data) = failure.data {
            error["data"] = data;
        }
        self.write(&json!({ "jsonrpc": "2.0", "id": id, "error": error }));
    }

    /// A line for whoever reads the client's logs: stderr, never stdout.
    pub fn log(&self, line: &str) {
        eprintln!("atelier {}: {line}", self.name);
    }

    /// Whether the client is gone ([`close`](Self::close)).
    pub fn closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// The turn was interrupted: give up on what was asked on its behalf.
    pub fn cancel_turn(&self) {
        lock(&self.pending).retain(|_, waiter| {
            let of_turn = waiter.scope == Scope::Turn;
            if of_turn {
                let _ = waiter.reply.send(Reply::Cancelled(Why::Interrupted));
            }
            !of_turn
        });
    }

    /// The client is gone: nothing asked of it will be answered, and
    /// nothing more can be asked.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        for (_, waiter) in lock(&self.pending).drain() {
            let _ = waiter.reply.send(Reply::Cancelled(Why::Interrupted));
        }
    }

    /// Ask the client something and wait for its answer: for as long as
    /// `timeout` allows, until the client is gone, or — for a request of
    /// the [turn](Scope::Turn) — until the turn is interrupted. `stopped`
    /// says whether it already was; it is asked once the request is
    /// registered, so an interrupt either finds it there and cancels it, or
    /// came first and is seen here. Returns the answer, and with it the
    /// request's id if the client was sent it — for a caller that tells the
    /// client when it stops waiting.
    pub fn call(
        &self,
        method: &str,
        params: Value,
        scope: Scope,
        timeout: Option<Duration>,
        stopped: impl FnOnce() -> bool,
    ) -> (Option<u64>, Reply) {
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        lock(&self.pending).insert(id, Waiter { reply: tx, scope });
        if self.closed() || stopped() {
            lock(&self.pending).remove(&id);
            return (None, Reply::Cancelled(Why::Interrupted));
        }
        self.write(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let reply = match timeout {
            Some(limit) => rx
                .recv_timeout(limit)
                .unwrap_or(Reply::Cancelled(Why::Timeout)),
            None => rx.recv().unwrap_or(Reply::Cancelled(Why::Interrupted)),
        };
        lock(&self.pending).remove(&id);
        (Some(id), reply)
    }

    /// The reader thread's loop: every line of stdin, until it ends. A
    /// request or a notification is handed to `on_request` (method, id,
    /// params — `Null` when there are none); the client's answers go to
    /// whoever waits for them; what is neither is refused here.
    pub fn read(&self, mut on_request: impl FnMut(&str, &Id, &Value)) {
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
                self.incoming(text, &mut on_request);
            }
        }
    }

    /// One line from the client: a request, a notification, or the answer
    /// to a request of ours.
    fn incoming(&self, line: &str, on_request: &mut impl FnMut(&str, &Id, &Value)) {
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
                on_request(method, &id, object.get("params").unwrap_or(&Value::Null));
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
            self.log("dropped an answer to a request that is not pending");
            return;
        };
        let reply = match object.get("result") {
            Some(result) => Reply::Result(result.clone()),
            None => Reply::Error(object.get("error").cloned().unwrap_or(Value::Null)),
        };
        let _ = waiter.reply.send(reply);
    }
}

/// End the process after `grace`, whatever the session thread is doing: for
/// when nobody is left to wait for it. Returning from the protocol's `run`
/// before then ends the process the normal way, and this with it.
pub fn watchdog(grace: Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(grace);
        // What `main` does on the way out, which this exit skips.
        crate::tools::bash::kill_all();
        std::process::exit(0);
    });
}
