//! One running language server: the JSON-RPC conversation with it.
//!
//! The shape follows the MCP stdio connection (`mcp::conn`) — a thread reads
//! the server's output so that waiting for an answer can time out — with two
//! differences that LSP forces:
//!
//! - **Answers are matched to requests by id**, each request waiting on a
//!   channel of its own. A [`Client`] is therefore safe to use from several
//!   threads at once (it is `Sync`), which is what lets `symbols` calls run
//!   in a parallel batch; and a late answer to a request that gave up finds
//!   nobody waiting and is dropped.
//! - **The server asks things too** (`workspace/configuration`,
//!   `window/workDoneProgress/create`, `client/registerCapability`) and
//!   some servers wait for the answer, so the reading thread replies to
//!   every request itself: an empty answer to the ones that only need
//!   acknowledging, "method not found" to the rest.
//!
//! # Is it ready?
//!
//! A server answers from its first second, and for a while the answers are
//! *empty*: it has not loaded the project yet. An empty definition from a
//! server that is still indexing would be reported as "there is none", so a
//! query first waits ([`Client::wait_ready`]) until the server stops saying
//! it is busy: no `$/progress` work in flight (the client declares
//! `window.workDoneProgress`) and, for servers that send it, an
//! `experimental/serverStatus` saying `quiescent` (rust-analyzer). A server
//! that reports neither is taken to be ready after a short settling time.
//!
//! # Files
//!
//! A server reads the files it was told are open from the client, not from
//! disk. [`Client::sync`] sends a file's text with `didOpen` the first time
//! it is queried and again, whole, with `didChange` whenever the text
//! differs from what was last sent — the agent edits files between calls.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::frame::{self, Decoder};

/// How long a server that announces no work at all is given before it is
/// taken to be ready: the gap between `initialized` and its first
/// `$/progress`.
const SETTLE: Duration = Duration::from_millis(300);

/// Why a request got no usable answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Failure {
    /// Nothing came back in time. The server is still there.
    Timeout,
    /// The server is gone: its output closed, or stopped being LSP.
    Died,
    /// The server answered with an error; its message.
    Server(String),
}

/// The two ends of a server's stdio, and the process when there is one
/// (tests talk to a thread over pipes).
pub(crate) struct Transport {
    pub(crate) writer: Box<dyn Write + Send>,
    pub(crate) reader: Box<dyn Read + Send>,
    pub(crate) child: Option<Child>,
}

type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

/// What the reading thread and the requesting threads share.
#[derive(Default)]
struct State {
    /// Requests waiting for their answer, by id.
    pending: HashMap<u64, Sender<Result<Value, String>>>,
    /// `$/progress` tokens begun and not ended.
    working: HashSet<String>,
    /// The last `experimental/serverStatus`, from servers that send one.
    quiescent: Option<bool>,
    /// When the server last started or finished something.
    last_activity: Option<Instant>,
    /// The server's output ended.
    dead: bool,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    /// Signalled when readiness may have changed.
    changed: Condvar,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// What the server was last sent of a file.
struct Sent {
    version: i64,
    hash: u64,
}

/// A language server that has been initialized.
pub(crate) struct Client {
    writer: Writer,
    shared: Arc<Shared>,
    next_id: AtomicU64,
    /// The files open on the server.
    docs: Mutex<HashMap<PathBuf, Sent>>,
    child: Mutex<Option<Child>>,
}

impl Client {
    /// Take over `transport` and do the `initialize` handshake for the
    /// project at `root`, giving the server until `deadline` to answer.
    pub(crate) fn start(
        transport: Transport,
        root: &Path,
        options: Value,
        deadline: Instant,
    ) -> Result<Client, Failure> {
        let writer: Writer = Arc::new(Mutex::new(transport.writer));
        let shared = Arc::new(Shared::default());
        let (theirs, replies) = (Arc::clone(&shared), Arc::clone(&writer));
        thread::spawn(move || read_loop(transport.reader, &replies, &theirs));
        let client = Client {
            writer,
            shared,
            next_id: AtomicU64::new(1),
            docs: Mutex::new(HashMap::new()),
            child: Mutex::new(transport.child),
        };
        let uri = super::uri_of_path(root);
        let name = root.file_name().map_or_else(
            || "project".to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let params = json!({
            "processId": std::process::id(),
            "clientInfo": { "name": "atelier", "version": env!("CARGO_PKG_VERSION") },
            "rootUri": uri,
            "workspaceFolders": [{ "uri": uri, "name": name }],
            "initializationOptions": options,
            // Only what `symbols` uses. No `general.positionEncodings`:
            // positions are UTF-16, as the protocol defaults to. No
            // `publishDiagnostics`: diagnostics come from the project's own
            // checkers (see `context::diagnostics`), not from here.
            "capabilities": {
                "window": { "workDoneProgress": true },
                "workspace": {
                    "workspaceFolders": true,
                    "configuration": true,
                    "symbol": {},
                },
                "textDocument": {
                    "synchronization": {},
                    "definition": { "linkSupport": true },
                    "references": {},
                    "hover": { "contentFormat": ["plaintext", "markdown"] },
                    "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                },
                "experimental": { "serverStatusNotification": true },
            },
        });
        client.request("initialize", params, deadline)?;
        client.notify("initialized", json!({}))?;
        client.shared.state().last_activity = Some(Instant::now());
        Ok(client)
    }

    /// Whether the server's output has ended.
    pub(crate) fn is_dead(&self) -> bool {
        self.shared.state().dead
    }

    /// Send a request and wait for its answer until `deadline`.
    pub(crate) fn request(
        &self,
        method: &str,
        params: Value,
        deadline: Instant,
    ) -> Result<Value, Failure> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = channel();
        {
            // Checked and registered under one lock: the reading thread
            // either sees this request when the output closes, or this
            // sees that it already has.
            let mut state = self.shared.state();
            if state.dead {
                return Err(Failure::Died);
            }
            state.pending.insert(id, tx);
        }
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if self.send(&message).is_err() {
            self.shared.state().pending.remove(&id);
            return Err(Failure::Died);
        }
        let wait = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(message)) => Err(Failure::Server(message)),
            Err(RecvTimeoutError::Timeout) => {
                self.shared.state().pending.remove(&id);
                // Let the server stop working on it; whatever it still
                // sends for this id finds nobody waiting.
                let _ = self.notify("$/cancelRequest", json!({ "id": id }));
                Err(Failure::Timeout)
            }
            // The reading thread dropped the sender: the output closed.
            Err(RecvTimeoutError::Disconnected) => Err(Failure::Died),
        }
    }

    /// Send a notification.
    pub(crate) fn notify(&self, method: &str, params: Value) -> Result<(), Failure> {
        let message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.send(&message).map_err(|_| Failure::Died)
    }

    fn send(&self, message: &Value) -> std::io::Result<()> {
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        frame::write(writer.as_mut(), message)
    }

    /// Make the server's copy of the file at `path` be `text`: opened if it
    /// was not, replaced if it differs from what was last sent.
    pub(crate) fn sync(&self, path: &Path, language_id: &str, text: &str) -> Result<(), Failure> {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut h);
        let hash = h.finish();
        let uri = super::uri_of_path(path);
        // Held across the write, so two calls on one file cannot send
        // their versions out of order.
        let mut docs = self.docs.lock().unwrap_or_else(|e| e.into_inner());
        match docs.get_mut(path) {
            Some(sent) if sent.hash == hash => Ok(()),
            Some(sent) => {
                sent.version += 1;
                sent.hash = hash;
                // No range: the whole text, which every server takes
                // whatever kind of sync it asked for.
                self.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": uri, "version": sent.version },
                        "contentChanges": [{ "text": text }],
                    }),
                )
            }
            None => {
                docs.insert(path.to_path_buf(), Sent { version: 1, hash });
                self.notify(
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": language_id,
                            "version": 1,
                            "text": text,
                        },
                    }),
                )
            }
        }
    }

    /// Wait until the server says it is not busy (see the module docs), or
    /// `deadline`. `false` when it is still busy then. A dead server is
    /// "ready": the request that follows reports the death.
    pub(crate) fn wait_ready(&self, deadline: Instant) -> bool {
        let mut state = self.shared.state();
        loop {
            let now = Instant::now();
            let until = match (state.dead, state.quiescent) {
                (true, _) | (false, Some(true)) => return true,
                (false, Some(false)) => deadline,
                (false, None) if !state.working.is_empty() => deadline,
                (false, None) => {
                    let settled = state.last_activity.map_or(now, |at| at + SETTLE);
                    if now >= settled {
                        return true;
                    }
                    settled.min(deadline)
                }
            };
            if now >= deadline {
                return false;
            }
            let wait = until.saturating_duration_since(now);
            state = match self.shared.changed.wait_timeout(state, wait) {
                Ok((state, _)) => state,
                Err(e) => e.into_inner().0,
            };
        }
    }

    /// Stop the server and everything it started.
    pub(crate) fn kill(&self) {
        let child = self.child.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(mut child) = child {
            // A polite exit first costs nothing and lets the server remove
            // its lock files; the kill does not wait to see whether it
            // was heeded.
            let _ = self.notify("exit", Value::Null);
            kill_group(&mut child);
        }
        // Close our end, which the reading thread also holds: a server
        // without a process (a test's thread) sees its input end.
        *self.writer.lock().unwrap_or_else(|e| e.into_inner()) = Box::new(std::io::sink());
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Kill `child` and its process group — a server's helpers (rust-analyzer's
/// `cargo`, proc-macro servers) are in it, see `spawn` — the way the `bash`
/// tool stops a job: TERM, then KILL if it lingers.
fn kill_group(child: &mut Child) {
    #[cfg(unixlike)]
    {
        use std::process::Command;
        let group = format!("-{}", child.id());
        let _ = Command::new("kill").args(["-TERM", "--", &group]).status();
        let deadline = Instant::now() + Duration::from_millis(1000);
        while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        if matches!(child.try_wait(), Ok(None)) {
            let _ = Command::new("kill").args(["-KILL", "--", &group]).status();
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// The reading thread: decode the server's output until it ends, routing
/// answers to whoever waits for them. Then fail everything still waiting.
fn read_loop(mut reader: Box<dyn Read + Send>, writer: &Writer, shared: &Shared) {
    let mut decoder = Decoder::default();
    let mut buf = [0u8; 16 * 1024];
    'stream: loop {
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        decoder.push(&buf[..n]);
        loop {
            match decoder.next() {
                Ok(Some(body)) => {
                    // A body that is not JSON is skipped: the framing still
                    // says where the next one starts.
                    if let Ok(message) = serde_json::from_slice::<Value>(&body) {
                        receive(&message, writer, shared);
                    }
                }
                Ok(None) => break,
                Err(_) => break 'stream,
            }
        }
    }
    let mut state = shared.state();
    state.dead = true;
    // Dropping the senders wakes every waiting request.
    state.pending.clear();
    shared.changed.notify_all();
}

/// Act on one message from the server.
fn receive(message: &Value, writer: &Writer, shared: &Shared) {
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        // An answer. Ids are ours, so they are numbers.
        let Some(id) = message.get("id").and_then(Value::as_u64) else {
            return;
        };
        let Some(waiting) = shared.state().pending.remove(&id) else {
            return;
        };
        let outcome = match message.get("error") {
            Some(error) => Err(error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_string()),
            None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
        };
        let _ = waiting.send(outcome);
        return;
    };
    let params = &message["params"];
    let Some(id) = message.get("id") else {
        notified(method, params, shared);
        return;
    };
    let reply = match method {
        // One answer per item asked about; `null` is "no setting".
        "workspace/configuration" => {
            let items = params["items"].as_array().map_or(0, Vec::len);
            json!({ "jsonrpc": "2.0", "id": id, "result": vec![Value::Null; items] })
        }
        "window/workDoneProgress/create"
        | "client/registerCapability"
        | "client/unregisterCapability" => json!({ "jsonrpc": "2.0", "id": id, "result": null }),
        _ => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("{method} is not supported") },
        }),
    };
    let mut writer = writer.lock().unwrap_or_else(|e| e.into_inner());
    let _ = frame::write(writer.as_mut(), &reply);
}

/// A notification: only those that say whether the server is busy matter.
fn notified(method: &str, params: &Value, shared: &Shared) {
    match method {
        "$/progress" => {
            // A token is a string or a number; either way it names the work.
            let token = match &params["token"] {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let mut state = shared.state();
            match params["value"]["kind"].as_str() {
                Some("begin") => state.working.insert(token),
                Some("end") => state.working.remove(&token),
                _ => return,
            };
            state.last_activity = Some(Instant::now());
        }
        "experimental/serverStatus" => {
            let Some(quiescent) = params["quiescent"].as_bool() else {
                return;
            };
            shared.state().quiescent = Some(quiescent);
        }
        _ => return,
    }
    shared.changed.notify_all();
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{Act, FakeServer};
    use super::*;

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }

    fn start(server: &FakeServer) -> Client {
        Client::start(server.transport(), Path::new("/p"), Value::Null, soon()).unwrap()
    }

    #[test]
    fn the_handshake_is_initialize_then_initialized() {
        let server = FakeServer::new(|_, _| Act::Reply(json!({ "capabilities": {} })));
        let client = start(&server);
        // Answered once the server has read the notification before it.
        client.request("ping", Value::Null, soon()).unwrap();
        assert_eq!(server.methods(), ["initialize", "initialized", "ping"]);
        let init = server.params_of("initialize");
        assert_eq!(init["rootUri"], "file:///p");
        // UTF-16 positions: no other encoding is offered.
        assert!(init["capabilities"]["general"].is_null());
        assert!(
            init["capabilities"]["textDocument"]["publishDiagnostics"].is_null(),
            "diagnostics are not this client's business"
        );
        assert!(!client.is_dead());
    }

    #[test]
    fn concurrent_requests_each_get_their_own_answer() {
        // Answers come back in the reverse of the order asked.
        let server = FakeServer::new(|method, params| match method {
            "echo" => {
                let n = params["n"].as_u64().unwrap();
                thread::sleep(Duration::from_millis(40 * (4 - n)));
                Act::Reply(json!(n))
            }
            _ => Act::Reply(Value::Null),
        })
        .concurrent();
        let client = start(&server);
        thread::scope(|scope| {
            for n in 0..4u64 {
                let client = &client;
                scope.spawn(move || {
                    let got = client.request("echo", json!({ "n": n }), soon());
                    assert_eq!(got, Ok(json!(n)));
                });
            }
        });
    }

    #[test]
    fn a_timed_out_request_is_cancelled_and_the_client_stays_usable() {
        let server = FakeServer::new(|method, _| match method {
            "slow" => {
                thread::sleep(Duration::from_millis(300));
                Act::Reply(json!("late"))
            }
            _ => Act::Reply(json!("on time")),
        });
        let client = start(&server);
        let deadline = Instant::now() + Duration::from_millis(50);
        assert_eq!(
            client.request("slow", Value::Null, deadline),
            Err(Failure::Timeout)
        );
        // The late answer is not taken for this one's.
        assert_eq!(
            client.request("fast", Value::Null, soon()),
            Ok(json!("on time"))
        );
        assert!(server.methods().contains(&"$/cancelRequest".to_string()));
    }

    #[test]
    fn a_server_that_exits_fails_the_waiting_request_and_later_ones() {
        let server = FakeServer::new(|method, _| match method {
            "initialize" => Act::Reply(Value::Null),
            _ => Act::Exit,
        });
        let client = start(&server);
        assert_eq!(
            client.request("anything", Value::Null, soon()),
            Err(Failure::Died)
        );
        assert!(client.is_dead());
        assert_eq!(
            client.request("again", Value::Null, soon()),
            Err(Failure::Died)
        );
    }

    #[test]
    fn a_server_error_is_reported_with_its_message() {
        let server = FakeServer::new(|method, _| match method {
            "initialize" => Act::Reply(Value::Null),
            _ => Act::Error("content modified".to_string()),
        });
        let client = start(&server);
        assert_eq!(
            client.request("textDocument/hover", Value::Null, soon()),
            Err(Failure::Server("content modified".to_string()))
        );
    }

    #[test]
    fn requests_from_the_server_are_answered() {
        let server = FakeServer::new(|_, _| Act::Reply(Value::Null));
        let _client = start(&server);
        let config = server.ask(
            "workspace/configuration",
            json!({ "items": [{ "section": "a" }, { "section": "b" }] }),
        );
        assert_eq!(config["result"], json!([null, null]));
        let progress = server.ask("window/workDoneProgress/create", json!({ "token": "t" }));
        assert!(progress.get("error").is_none() && progress["result"].is_null());
        let edit = server.ask("workspace/applyEdit", json!({}));
        assert_eq!(edit["error"]["code"], -32601);
    }

    #[test]
    fn a_file_is_opened_once_and_resent_when_it_changes() {
        let server = FakeServer::new(|_, _| Act::Reply(Value::Null));
        let client = start(&server);
        let path = Path::new("/p/a.rs");
        client.sync(path, "rust", "fn a() {}").unwrap();
        client.sync(path, "rust", "fn a() {}").unwrap();
        client.sync(path, "rust", "fn b() {}").unwrap();
        // A request after them: by the time it is answered the server has
        // read the notifications before it.
        client.request("ping", Value::Null, soon()).unwrap();
        let sent: Vec<String> = (server.methods().into_iter())
            .filter(|m| m.starts_with("textDocument/"))
            .collect();
        assert_eq!(sent, ["textDocument/didOpen", "textDocument/didChange"]);
        let open = server.params_of("textDocument/didOpen");
        assert_eq!(open["textDocument"]["uri"], "file:///p/a.rs");
        assert_eq!(open["textDocument"]["languageId"], "rust");
        assert_eq!(open["textDocument"]["text"], "fn a() {}");
        let change = server.params_of("textDocument/didChange");
        assert_eq!(change["textDocument"]["version"], 2);
        assert_eq!(change["contentChanges"], json!([{ "text": "fn b() {}" }]));
    }

    #[test]
    fn readiness_waits_for_progress_to_end() {
        let server = FakeServer::new(|_, _| Act::Reply(Value::Null));
        let client = start(&server);
        server.tell(
            "$/progress",
            json!({ "token": "index", "value": { "kind": "begin" } }),
        );
        // Still indexing at the deadline.
        assert!(!client.wait_ready(Instant::now() + Duration::from_millis(500)));
        let teller = server.clone();
        let done = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            teller.tell(
                "$/progress",
                json!({ "token": "index", "value": { "kind": "end" } }),
            );
        });
        let started = Instant::now();
        assert!(client.wait_ready(soon()));
        // Not before the work ended, and after a settling time.
        assert!(started.elapsed() >= Duration::from_millis(100) + SETTLE);
        done.join().unwrap();
    }

    #[test]
    fn readiness_follows_server_status_when_one_is_sent() {
        let server = FakeServer::new(|_, _| Act::Reply(Value::Null));
        let client = start(&server);
        server.tell("experimental/serverStatus", json!({ "quiescent": false }));
        // Past the settling time, and still not ready: the server said so.
        assert!(!client.wait_ready(Instant::now() + SETTLE * 2));
        server.tell("experimental/serverStatus", json!({ "quiescent": true }));
        assert!(client.wait_ready(soon()));
    }

    #[test]
    fn a_silent_server_is_ready_after_the_settling_time() {
        let server = FakeServer::new(|_, _| Act::Reply(Value::Null));
        let client = start(&server);
        let started = Instant::now();
        assert!(client.wait_ready(soon()));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
