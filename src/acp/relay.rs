//! Sessions beyond the process's own: each in an `atelier acp` of its own,
//! started by this one and spoken to on its behalf.
//!
//! A process has one session, in one directory (see the module docs of
//! [`super`]), and an editor may ask for another — a second thread, in the
//! same project or another. One in a directory this connection already has
//! a session in is refused: a directory has one session. One in another
//! directory is opened by a **child**: the same program with the same
//! arguments, given the client's `initialize` and then its `session/new`,
//! which it carries out as any `atelier acp` does — claiming the directory
//! with the coordinator, or being refused it, or taking it over with
//! `--take-over`. The child's session is its directory's for as long as
//! the child lives, which is until the connection ends.
//!
//! Between the client and a child this process is a pipe that knows where
//! each message goes:
//!
//! - a request or notification of the client's that names a child's
//!   session (`sessionId`) is passed to that child as it came, and its
//!   answer back as it comes; `$/cancel_request` goes to every child, since
//!   it names a request and not a session;
//! - a request the child makes of the client (`session/request_permission`,
//!   `fs/*`, `elicitation/create`) is passed on under an id of this
//!   process's — `atelier-relay-<child>:<the child's id>` — so that it cannot
//!   be taken for one of this process's own, and the client's answer goes
//!   back to the child under its own id;
//! - the child's notifications go to the client untouched: they already
//!   name the child's session.
//!
//! A child that ends — taken over by another atelier, or gone wrong — takes
//! its session with it: what was asked of it and not answered fails, and
//! the session id is unknown from then on. When the client goes, each
//! child's input is closed, and each ends as `atelier acp` does.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::rpc::{self, Failure, Id, Wire, lock};

/// What the ids of a child's requests start with, as the client sees them.
const PREFIX: &str = "atelier-relay-";
/// The id of the `initialize` this process sends a child, whose answer the
/// client never sees.
const INITIALIZE: &str = "atelier-relay-initialize";

/// The children of one connection.
pub(super) struct Relay {
    /// How to start a child: the program and its arguments.
    program: PathBuf,
    args: Vec<String>,
    children: Mutex<Vec<Arc<Child>>>,
    next: AtomicU64,
}

/// One child, and its one session.
struct Child {
    no: u64,
    /// The directory, by its real path.
    dir: PathBuf,
    /// Its input; `None` once closed.
    stdin: Mutex<Option<ChildStdin>>,
    /// The session id, once the child has answered `session/new` with one.
    session: Mutex<Option<String>>,
    /// The client's requests passed to it and not answered yet (their ids,
    /// as JSON).
    pending: Mutex<HashSet<String>>,
    /// The `session/new` it was started for.
    opening: Value,
    process: Mutex<Option<std::process::Child>>,
}

impl Child {
    fn send(&self, message: &Value) -> bool {
        let mut stdin = lock(&self.stdin);
        let Some(pipe) = stdin.as_mut() else {
            return false;
        };
        let sent = pipe
            .write_all(format!("{message}\n").as_bytes())
            .and_then(|()| pipe.flush());
        if sent.is_err() {
            *stdin = None;
        }
        sent.is_ok()
    }

    fn close(&self) {
        lock(&self.stdin).take();
    }

    fn has_session(&self, id: &str) -> bool {
        lock(&self.session).as_deref() == Some(id)
    }
}

impl Relay {
    pub(super) fn new(program: PathBuf, args: Vec<String>) -> Relay {
        Relay {
            program,
            args,
            children: Mutex::new(Vec::new()),
            next: AtomicU64::new(1),
        }
    }

    /// The session a child has open in `dir`, or is opening there (`None`
    /// for its id then).
    pub(super) fn session_in(&self, dir: &std::path::Path) -> Option<Option<String>> {
        let children = lock(&self.children);
        let child = children.iter().find(|c| c.dir == dir)?;
        Some(lock(&child.session).clone())
    }

    /// Start a child for `session/new` (request `id`, `params` as the client
    /// sent them) in `dir`, and hand it `initialize` (the client's, as it
    /// was sent) and the request. Its answer reaches the client from the
    /// child's own thread.
    pub(super) fn open(
        self: &Arc<Self>,
        wire: &Arc<Wire>,
        initialize: &Value,
        id: &Id,
        params: &Value,
        dir: PathBuf,
    ) {
        let spawned = Command::new(&self.program)
            .args(&self.args)
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn();
        let mut process = match spawned {
            Ok(process) => process,
            Err(e) => {
                let why = format!(
                    "could not start an atelier for the session in {}: {e}",
                    dir.display()
                );
                return wire.fail(id, Failure::new(rpc::code::INTERNAL_ERROR, why));
            }
        };
        let stdout = process.stdout.take().expect("piped stdout");
        let opening = id.clone().unwrap_or(Value::Null);
        let child = Arc::new(Child {
            no: self.next.fetch_add(1, Ordering::Relaxed),
            dir,
            stdin: Mutex::new(process.stdin.take()),
            session: Mutex::new(None),
            pending: Mutex::new(HashSet::from([opening.to_string()])),
            opening: opening.clone(),
            process: Mutex::new(Some(process)),
        });
        lock(&self.children).push(Arc::clone(&child));
        let hello = json!({
            "jsonrpc": "2.0", "id": INITIALIZE, "method": "initialize", "params": initialize,
        });
        let new =
            json!({ "jsonrpc": "2.0", "id": opening, "method": "session/new", "params": params });
        child.send(&hello);
        child.send(&new);
        let (relay, wire) = (Arc::clone(self), Arc::clone(wire));
        std::thread::spawn(move || relay.read_child(&wire, &child, stdout));
    }

    /// A request or notification of the client's for a child's session:
    /// passed to it. `false` when it names no child's session.
    pub(super) fn route(&self, wire: &Wire, method: &str, id: &Id, params: &Value) -> bool {
        let message = match id {
            Some(id) => json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
            None => json!({ "jsonrpc": "2.0", "method": method, "params": params }),
        };
        if method == "$/cancel_request" {
            for child in lock(&self.children).iter() {
                child.send(&message);
            }
            // This process may have the request too.
            return false;
        }
        let Some(session) = params["sessionId"].as_str() else {
            return false;
        };
        let child = lock(&self.children)
            .iter()
            .find(|c| c.has_session(session))
            .cloned();
        let Some(child) = child else {
            return false;
        };
        if let Some(id) = id {
            lock(&child.pending).insert(id.to_string());
        }
        if !child.send(&message) {
            let why = "the atelier that had this session has ended";
            if let Some(key) = id.as_ref().map(Value::to_string) {
                lock(&child.pending).remove(&key);
            }
            wire.fail(id, Failure::new(rpc::code::INTERNAL_ERROR, why));
        }
        true
    }

    /// The client's answer to a request a child made: back to that child,
    /// under the id it gave. `false` when it is not one.
    pub(super) fn answer(&self, answer: &Map<String, Value>) -> bool {
        let Some(relayed) = answer.get("id").and_then(Value::as_str) else {
            return false;
        };
        let Some((no, id)) = relayed
            .strip_prefix(PREFIX)
            .and_then(|rest| rest.split_once(':'))
        else {
            return false;
        };
        let id: Value = serde_json::from_str(id).unwrap_or(Value::Null);
        let mut answer = answer.clone();
        answer.insert("id".to_string(), id);
        let child = lock(&self.children)
            .iter()
            .find(|c| c.no.to_string() == no)
            .cloned();
        // A child that is gone has nobody waiting.
        if let Some(child) = child {
            child.send(&Value::Object(answer));
        }
        true
    }

    /// Everything a child writes, until it ends.
    fn read_child(&self, wire: &Wire, child: &Child, stdout: std::process::ChildStdout) {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            let Ok(Value::Object(mut message)) = serde_json::from_str::<Value>(&line) else {
                wire.log(&format!("dropped a line from a session's atelier: {line}"));
                continue;
            };
            let id = message.get("id").cloned();
            match (message.contains_key("method"), id) {
                // A request of the client's: under an id of this process's.
                (true, Some(id)) => {
                    let relayed = format!("{PREFIX}{}:{id}", child.no);
                    message.insert("id".to_string(), Value::from(relayed));
                }
                (true, None) => {}
                (false, Some(id)) if id == INITIALIZE => continue,
                (false, Some(id)) => {
                    lock(&child.pending).remove(&id.to_string());
                    if id == child.opening {
                        match message.get("result").and_then(|r| r["sessionId"].as_str()) {
                            Some(session) => *lock(&child.session) = Some(session.to_string()),
                            // Refused: the child has nothing more to do,
                            // and the directory is not this connection's
                            // when the client hears so (it may try again).
                            None => {
                                child.close();
                                lock(&self.children).retain(|c| c.no != child.no);
                            }
                        }
                    }
                }
                (false, None) => {}
            }
            wire.write(&Value::Object(message));
        }
        child.close();
        // Its session is no more: nothing waits on it.
        let unanswered: Vec<String> = lock(&child.pending).drain().collect();
        for id in unanswered {
            let id = serde_json::from_str(&id).ok();
            let why = "the atelier that had this session has ended (taken over by another, \
                       or it failed: see the log)";
            wire.fail(&id, Failure::new(rpc::code::INTERNAL_ERROR, why));
        }
        lock(&self.children).retain(|c| c.no != child.no);
        if let Some(mut process) = lock(&child.process).take() {
            let _ = process.wait();
        }
    }

    /// The client is gone: close every child's input, for it to end as
    /// `atelier acp` does when its client goes.
    pub(super) fn close(&self) {
        for child in lock(&self.children).iter() {
            child.close();
        }
    }

    /// Give the children until `within` from now to have ended.
    pub(super) fn wait(&self, within: Duration) {
        let deadline = Instant::now() + within;
        while !lock(&self.children).is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// The answer to a `session/new` in `dir`, where this connection has a
/// session already (`session`: its id, `None` while it is being opened).
pub(super) fn already_open(dir: &std::path::Path, session: Option<&str>) -> Failure {
    let what = match session {
        Some(id) => format!("is already open on this connection, as {id}"),
        None => "is being opened on this connection".to_string(),
    };
    let mut failure = Failure::new(
        super::code::SESSION_LIMIT,
        format!(
            "the session of {} {what}: a directory has one session (prompt that one)",
            dir.display()
        ),
    );
    if let Some(id) = session {
        failure.data = Some(json!({ "sessionId": id }));
    }
    failure
}
