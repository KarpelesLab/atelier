//! The owner's socket: what a session's owner answers to the atelier that
//! was started next to it.
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
//! stopped and the process ends there.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::Record;
use crate::rpc::{self, lock};

/// How long a newcomer waits for an owner's answer.
const ANSWER: Duration = Duration::from_secs(2);

type Hook = Box<dyn Fn() + Send + Sync>;

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
    /// A turn is in progress.
    turn: bool,
    /// Who took the session over.
    yielding: Option<String>,
    agents: Option<Box<dyn Fn() -> usize + Send + Sync>>,
    client: Option<String>,
    on_yield: Vec<Arc<Hook>>,
    on_forced_exit: Vec<Arc<Hook>>,
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

/// Ask the owner `record` describes one thing, and wait for its answer.
pub(super) fn ask(record: &Record, method: &str, params: Value) -> Result<Value> {
    let Some(path) = &record.socket else {
        bail!("the owner has no control socket");
    };
    let Some((short, _handle)) = crate::coord::short_socket(std::path::Path::new(path)) else {
        bail!("{path} is too long for a socket path");
    };
    let stream = UnixStream::connect(short).with_context(|| format!("connecting to {path}"))?;
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
        let Ok(mut out) = stream.try_clone() else {
            return;
        };
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { break };
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let (Some(method), id) = (message["method"].as_str(), &message["id"]) else {
                continue;
            };
            let answer = match self.answer(method, &message["params"]) {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Err(failure) => json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": { "code": failure.code, "message": failure.message },
                }),
            };
            if !id.is_null() && out.write_all(format!("{answer}\n").as_bytes()).is_err() {
                break;
            }
        }
    }

    fn answer(self: &Arc<Self>, method: &str, params: &Value) -> Result<Value, rpc::Failure> {
        match method {
            "owner/status" => Ok(self.status()),
            "session/yield" => {
                let by = params["by"].as_str().unwrap_or("another process");
                self.yield_to(by);
                Ok(json!({ "grace_ms": super::YIELD_GRACE.as_millis() as u64 }))
            }
            _ => Err(rpc::Failure::new(
                rpc::code::METHOD_NOT_FOUND,
                format!("unknown method '{method}'"),
            )),
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
            "busy": state.turn,
            "jobs": jobs.iter().filter(|j| j.running).count(),
            "agents": state.agents.as_ref().map_or(0, |count| count()),
            "client": state.client,
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

    /// The process is on its way out: the socket goes now, the lock when
    /// the process does. The record stays, true until then and stale after.
    pub(super) fn close(&self) {
        if let Some(path) = &self.socket {
            let _ = std::fs::remove_file(path);
        }
    }
}
