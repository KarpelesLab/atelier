//! Integration tests for `atelier serve`: the embedding protocol (JSON-RPC
//! 2.0 over stdio, see `docs/protocol.md`), spoken to the real binary.
//!
//! Each test plays the client: it spawns `atelier serve` against a scripted
//! mock of an OpenAI-compatible server (the same idea as `tests/e2e.rs`,
//! whose helpers are copied here in their smallest form), writes requests
//! on the child's stdin and reads messages off its stdout.
//!
//! Nothing here sleeps to let something happen. The client waits *for a
//! message* — a response, a notification, a request from atelier — and the
//! protocol supplies the points to wait at: an `approval/request` holds a
//! turn still until it is answered, and the mock can hold a response open
//! until the test lets it go. Every wait has a deadline, so a missing
//! message fails the test (with the child's stderr) instead of hanging it.
//!
//! The reader also checks the stdout contract on every line of every test:
//! anything that is not a JSON-RPC message is a failure.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};

/// How long any single wait may take before the test fails.
const DEADLINE: Duration = Duration::from_secs(30);

// Error codes of the protocol (`docs/protocol.md`).
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const TURN_RUNNING: i64 = -32001;
const NOT_INITIALIZED: i64 = -32002;
const NO_TURN: i64 = -32003;
const ALREADY_INITIALIZED: i64 = -32005;

// ---------------------------------------------------------------- the mock

/// Answers the Nth chat request (0-based), given its body, by writing the
/// HTTP response to the connection — in one go or, to hold a turn open, in
/// pieces.
type Responder = Arc<dyn Fn(usize, &str, &mut TcpStream) + Send + Sync>;

/// A background mock OpenAI-compatible server: one request per connection,
/// answered in order by the [`Responder`]. Records every request body.
struct Mock {
    port: u16,
    bodies: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Mock {
    /// A mock whose Nth response is whatever `script` returns for it.
    fn scripted(script: impl Fn(usize, &str) -> Vec<u8> + Send + Sync + 'static) -> Mock {
        Mock::start(Arc::new(move |n, body, stream| {
            let _ = stream.write_all(&script(n, body));
        }))
    }

    fn start(responder: Responder) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let port = listener.local_addr().expect("local addr").port();
        // Non-blocking accept so the loop can see the stop flag.
        listener.set_nonblocking(true).expect("non-blocking");
        let stop = Arc::new(AtomicBool::new(false));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let (stop_thread, bodies_thread) = (Arc::clone(&stop), Arc::clone(&bodies));
        let handle = thread::spawn(move || {
            let counter = AtomicUsize::new(0);
            while !stop_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let n = counter.fetch_add(1, Ordering::SeqCst);
                        let _ = handle_conn(stream, n, &responder, &bodies_thread);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Mock {
            port,
            bodies,
            stop,
            handle: Some(handle),
        }
    }

    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    /// The body of the Nth request atelier sent.
    fn body(&self, n: usize) -> String {
        let bodies = self.bodies.lock().unwrap();
        assert!(
            n < bodies.len(),
            "only {} request(s) were made",
            bodies.len()
        );
        bodies[n].clone()
    }

    fn requests(&self) -> usize {
        self.bodies.lock().unwrap().len()
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Read one HTTP request (headers, then the `Content-Length` body), record
/// it, and let the responder answer it.
fn handle_conn(
    stream: TcpStream,
    n: usize,
    responder: &Responder,
    bodies: &Mutex<Vec<String>>,
) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut content_length = 0usize;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 || line.trim_end().is_empty() {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;
    let body = String::from_utf8_lossy(&body).into_owned();
    bodies.lock().unwrap().push(body.clone());

    responder(n, &body, &mut writer);
    writer.flush()?;
    // Close-delimited: the provider reads the SSE stream until EOF.
    let _ = writer.shutdown(Shutdown::Write);
    Ok(())
}

/// What opens every streamed response.
const SSE_HEAD: &str = "HTTP/1.1 200 OK\r\n\
     Content-Type: text/event-stream\r\n\
     Cache-Control: no-cache\r\n\
     Connection: close\r\n\
     \r\n";

/// One SSE frame carrying `text` as a piece of the answer.
fn text_frame(text: &str) -> String {
    let chunk = json!({"choices": [{"index": 0, "delta": {"content": text}}]});
    format!("data: {chunk}\n\n")
}

/// A whole streamed response: the frames, then the end marker.
fn sse(frames: &[String]) -> Vec<u8> {
    format!("{SSE_HEAD}{}data: [DONE]\n\n", frames.concat()).into_bytes()
}

/// A streamed plain answer.
fn answer(text: &str) -> Vec<u8> {
    sse(&[text_frame(text)])
}

/// A streamed call of `tool` with the given arguments.
fn tool_call(id: &str, tool: &str, arguments: Value) -> Vec<u8> {
    let chunk = json!({"choices": [{"index": 0, "delta": {"tool_calls": [{
        "index": 0, "id": id, "type": "function",
        "function": {"name": tool, "arguments": arguments.to_string()},
    }]}}]});
    sse(&[format!("data: {chunk}\n\n")])
}

// -------------------------------------------------------------- the client

/// A unique temp directory to use as the served project.
fn unique_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "atelier-serve-{name}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// The client end of an `atelier serve` process.
struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    /// Messages off the child's stdout, parsed; `None` once it has ended.
    incoming: Receiver<Option<Value>>,
    /// Messages read while waiting for another one, not yet asked for.
    backlog: VecDeque<Value>,
    /// Everything received, in order.
    log: Vec<Value>,
    /// The child's stderr so far, for failure messages.
    stderr: Arc<Mutex<String>>,
    next_id: u64,
}

impl Client {
    /// Spawn `atelier serve <args>` in `cwd` against the mock, with nothing
    /// of the outer environment or the real home leaking in. Auto-approve
    /// is off: approvals are the client's to answer.
    fn spawn(mock: &Mock, cwd: &Path, args: &[&str]) -> Client {
        Client::spawn_env(mock, cwd, args, &[])
    }

    /// [`spawn`](Self::spawn), with these variables set (`Some`) or taken
    /// out (`None`) on top of the environment it gives the child.
    fn spawn_env(mock: &Mock, cwd: &Path, args: &[&str], env: &[(&str, Option<&str>)]) -> Client {
        let mut command = Command::new(env!("CARGO_BIN_EXE_atelier"));
        command
            .arg("serve")
            .args(args)
            .current_dir(cwd)
            .env("ATELIER_BASE_URL", mock.base_url())
            .env("ATELIER_MODEL", "mock")
            .env("ATELIER_NO_COORDINATOR", "1")
            .env("XDG_STATE_HOME", cwd.with_extension("state"))
            .env("XDG_CONFIG_HOME", cwd.join(".no-user-config"))
            .env("XDG_DATA_HOME", cwd.with_extension("data"))
            .env("HOME", cwd.with_extension("home"))
            .env_remove("ATELIER_APPROVE")
            .env_remove("ATELIER_TRUST_HOOKS")
            .env_remove("ATELIER_API_KEY")
            .env_remove("ATELIER_TRACE")
            .env_remove("ATELIER_DEBUG")
            .env_remove("ATELIER_BACKEND")
            .env_remove("ATELIER_CONTEXT_LIMIT")
            .env_remove("ATELIER_REVIEW")
            .env_remove("ATELIER_MODE")
            .env_remove("ATELIER_PARALLEL_TOOLS")
            .env_remove("ATELIER_SEARCH_KEY")
            .env_remove("ATELIER_TRUST_PROJECT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in env {
            match value {
                Some(value) => command.env(key, value),
                None => command.env_remove(key),
            };
        }
        let mut child = command.spawn().expect("spawn atelier serve");

        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, incoming) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                // The stdout contract: protocol messages, one per line, and
                // nothing else.
                let message: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|e| panic!("stdout line is not JSON ({e}): {line:?}"));
                assert_eq!(message["jsonrpc"], "2.0", "not JSON-RPC: {line}");
                if tx.send(Some(message)).is_err() {
                    return;
                }
            }
            let _ = tx.send(None);
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let mut child_stderr = child.stderr.take().expect("piped stderr");
        let sink = Arc::clone(&stderr);
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = child_stderr.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        });

        Client {
            stdin: child.stdin.take(),
            child,
            incoming,
            backlog: VecDeque::new(),
            log: Vec::new(),
            stderr,
            next_id: 0,
        }
    }

    /// A served session past its handshake.
    fn ready(mock: &Mock, cwd: &Path, args: &[&str]) -> Client {
        Client::ready_env(mock, cwd, args, &[])
    }

    /// [`ready`](Self::ready), with `env` added to the child's environment.
    fn ready_with(mock: &Mock, cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Client {
        let env: Vec<(&str, Option<&str>)> = env.iter().map(|(k, v)| (*k, Some(*v))).collect();
        Client::ready_env(mock, cwd, args, &env)
    }

    /// [`ready`](Self::ready), with the environment changed as
    /// [`spawn_env`](Self::spawn_env) does.
    fn ready_env(mock: &Mock, cwd: &Path, args: &[&str], env: &[(&str, Option<&str>)]) -> Client {
        let mut client = Client::spawn_env(mock, cwd, args, env);
        client.ok(
            "initialize",
            json!({"client": {"name": "test", "version": "0"}}),
        );
        client
    }

    fn fail(&self, why: &str) -> ! {
        panic!(
            "{why}\n--- received ---\n{}\n--- stderr ---\n{}",
            self.log
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
            self.stderr.lock().unwrap()
        );
    }

    /// Write one raw line to the child.
    fn write_line(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{line}").expect("write to atelier");
        stdin.flush().expect("flush");
    }

    /// Send a request; returns its id.
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = json!(format!("c{}", self.next_id));
        self.write_line(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string(),
        );
        id
    }

    /// Answer a request atelier made.
    fn reply(&mut self, request: &Value, result: Value) {
        let id = &request["id"];
        self.write_line(&json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string());
    }

    /// The next message off the wire, or failure at the deadline.
    fn recv(&mut self, waiting_for: &str) -> Value {
        match self.incoming.recv_timeout(DEADLINE) {
            Ok(Some(message)) => {
                self.log.push(message.clone());
                message
            }
            Ok(None) => self.fail(&format!("atelier exited while waiting for {waiting_for}")),
            Err(_) => self.fail(&format!("timed out waiting for {waiting_for}")),
        }
    }

    /// The first message matching `wanted` that has not been taken yet:
    /// one already read while waiting for something else, or the next to
    /// arrive. Messages passed over stay available.
    fn take(&mut self, what: &str, wanted: impl Fn(&Value) -> bool) -> Value {
        if let Some(i) = self.backlog.iter().position(&wanted) {
            return self.backlog.remove(i).unwrap();
        }
        loop {
            let message = self.recv(what);
            if wanted(&message) {
                return message;
            }
            self.backlog.push_back(message);
        }
    }

    /// The response to request `id`.
    fn response(&mut self, id: &Value) -> Value {
        self.take(&format!("the response to {id}"), |m| {
            m.get("method").is_none() && &m["id"] == id
        })
    }

    /// The next notification named `method`.
    fn notification(&mut self, method: &str) -> Value {
        let message = self.take(method, |m| m["method"] == method && m.get("id").is_none());
        message["params"].clone()
    }

    /// The next request atelier makes named `method` (id and params).
    fn asked(&mut self, method: &str) -> Value {
        self.take(method, |m| m["method"] == method && m.get("id").is_some())
    }

    /// Call a method that must succeed; returns its result.
    fn ok(&mut self, method: &str, params: Value) -> Value {
        let id = self.request(method, params);
        let response = self.response(&id);
        if response.get("result").is_none() {
            self.fail(&format!("{method} failed: {response}"));
        }
        response["result"].clone()
    }

    /// Call a method that must fail with `code`; returns the error object.
    fn err(&mut self, method: &str, params: Value, code: i64) -> Value {
        let id = self.request(method, params);
        let response = self.response(&id);
        if response["error"]["code"] != code {
            self.fail(&format!("{method}: expected error {code}, got {response}"));
        }
        response["error"].clone()
    }

    /// Start a turn; returns its id.
    fn start_turn(&mut self, text: &str) -> Value {
        self.ok("turn/start", json!({"text": text}))["turn"].clone()
    }

    /// The `turn/ended` notification of `turn`.
    fn turn_ended(&mut self, turn: &Value) -> Value {
        let ended = self.notification("turn/ended");
        assert_eq!(&ended["turn"], turn, "{ended}");
        ended
    }

    /// The methods of everything received so far, in order (responses as
    /// `"response"`).
    fn methods(&self) -> Vec<String> {
        self.log
            .iter()
            .map(|m| m["method"].as_str().unwrap_or("response").to_string())
            .collect()
    }

    /// Every `event/text` chunk received so far, joined.
    fn text(&self) -> String {
        self.log
            .iter()
            .filter(|m| m["method"] == "event/text")
            .filter_map(|m| m["params"]["text"].as_str())
            .collect()
    }

    /// Wait for the process to end on its own (after `shutdown`, or once
    /// its stdin is closed) and return how it did.
    fn exited(mut self) -> ExitStatus {
        loop {
            match self.incoming.recv_timeout(DEADLINE) {
                Ok(Some(message)) => self.log.push(message),
                Ok(None) => break,
                Err(_) => self.fail("timed out waiting for atelier to exit"),
            }
        }
        self.child.wait().expect("wait for atelier")
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------- the tests

/// The handshake comes first: anything before it is refused with its own
/// code, and it describes the session it opened. Then the errors a client
/// can cause without a turn: a second handshake, an unknown method, bad
/// parameters — each with a stable code, none of them fatal.
#[test]
fn initialize_handshake_and_request_errors() {
    let mock = Mock::scripted(|_, _| answer("unused"));
    let dir = unique_dir("init");
    let mut client = Client::spawn(&mock, &dir, &[]);

    let refused = client.err("session/status", json!({}), NOT_INITIALIZED);
    assert!(refused["message"].as_str().unwrap().contains("initialize"));
    client.err("turn/start", json!({"text": "hi"}), NOT_INITIALIZED);
    client.err("initialize", json!({"protocol": "one"}), INVALID_PARAMS);

    let hello = client.ok(
        "initialize",
        json!({"client": {"name": "test", "version": "0"}, "protocol": 1}),
    );
    assert_eq!(hello["protocol"], 1);
    assert_eq!(hello["atelier"]["version"], env!("CARGO_PKG_VERSION"));
    let session = &hello["session"];
    assert_eq!(session["model"], "mock");
    assert_eq!(session["backend"], "openai");
    assert_eq!(session["mode"], "auto-edit");
    assert!(
        Path::new(session["cwd"].as_str().unwrap()).ends_with(dir.file_name().unwrap()),
        "{session}"
    );
    let tools: Vec<&str> = session["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        tools.contains(&"read") && tools.contains(&"bash"),
        "{tools:?}"
    );
    assert_eq!(hello["capabilities"]["steer"], true);
    assert_eq!(hello["capabilities"]["approval_timeout"], Value::Null);

    client.err("initialize", json!({}), ALREADY_INITIALIZED);
    client.err("turn/rewind", json!({}), METHOD_NOT_FOUND);
    client.err("turn/start", json!({}), INVALID_PARAMS);
    client.err(
        "turn/start",
        json!({"text": "x", "images": "a.png"}),
        INVALID_PARAMS,
    );
    client.err(
        "turn/start",
        json!({"text": "x", "max_turns": 0}),
        INVALID_PARAMS,
    );
    client.err("turn/steer", json!({"text": "x"}), NO_TURN);
    client.err("turn/interrupt", json!({}), NO_TURN);

    let status = client.ok("session/status", json!({}));
    assert_eq!(status["model"], "mock");
    assert_eq!(status["mode"], "auto-edit");
    assert_eq!(status["busy"], false);
    assert_eq!(status["turn"], Value::Null);
    assert_eq!(status["background"], 0);
    assert_eq!(status["usage"]["output_tokens"], 0);
    // None of that asked the model anything.
    assert_eq!(mock.requests(), 0);
}

/// A turn with a tool call: `turn/start` answers at once with the turn's
/// id, the turn's events follow in order carrying that id, and
/// `turn/ended` closes it.
#[test]
fn a_turn_streams_its_tool_call_and_answer_then_ends() {
    let mock = Mock::scripted(|n, _| match n {
        0 => tool_call(
            "call_1",
            "write",
            json!({"path": "out.txt", "content": "written by tool"}),
        ),
        _ => answer("done"),
    });
    let dir = unique_dir("turn");
    let mut client = Client::ready(&mock, &dir, &[]);

    let turn = client.start_turn("write the file");
    assert_eq!(turn, 1);
    let ended = client.turn_ended(&turn);
    assert_eq!(ended["status"], "completed", "{ended}");
    assert_eq!(ended["steps"], 2);
    assert!(ended["duration_ms"].is_u64());
    assert!(ended["usage"]["context_tokens"].is_u64());
    assert!(ended.get("error").is_none() && ended.get("undelivered").is_none());

    let methods = client.methods();
    let events: Vec<&str> = methods
        .iter()
        .map(String::as_str)
        .skip_while(|m| *m != "event/tool_start")
        .collect();
    assert_eq!(
        events,
        [
            "event/tool_start",
            "event/file_change",
            "event/tool_end",
            "event/text",
            "turn/ended"
        ],
        "{methods:?}"
    );
    let event = |method: &str| {
        let found = client.log.iter().find(|m| m["method"] == method);
        found.unwrap()["params"].clone()
    };
    let started = event("event/tool_start");
    assert_eq!(started["turn"], 1);
    assert_eq!(started["name"], "write");
    assert_eq!(started["arguments"]["path"], "out.txt");
    let finished = event("event/tool_end");
    assert_eq!(finished["ok"], true, "{finished}");
    assert_eq!(finished["turn"], 1);
    assert_eq!(event("event/text"), json!({"turn": 1, "text": "done"}));
    assert_eq!(
        std::fs::read_to_string(dir.join("out.txt")).unwrap(),
        "written by tool"
    );

    // The session is free again, and counts turns.
    let status = client.ok("session/status", json!({}));
    assert_eq!(status["busy"], false);
    assert_eq!(client.start_turn("again"), 2);
    client.turn_ended(&json!(2));
}

/// With auto-approve off, a call that needs approval becomes a request to
/// the client, and the client's decision is the one carried out: `once`
/// runs the call, `deny` gives the model the refusal instead. A rule that
/// asks is named in the request.
#[test]
fn approvals_are_asked_of_the_client_and_obeyed() {
    let mock = Mock::scripted(|n, _| match n {
        0 => tool_call("call_1", "bash", json!({"command": "echo one > a.txt"})),
        2 => tool_call(
            "call_2",
            "write",
            json!({"path": "b.txt", "content": "two"}),
        ),
        _ => answer("done"),
    });
    let dir = unique_dir("approval");
    std::fs::write(
        dir.join("atelier.toml"),
        "[permissions]\nask = [\"write\"]\n",
    )
    .unwrap();
    let mut client = Client::ready(&mock, &dir, &[]);

    // `once`: the shell command runs.
    let turn = client.start_turn("run it");
    let asked = client.asked("approval/request");
    let params = &asked["params"];
    assert_eq!(params["turn"], 1);
    assert_eq!(params["tool"], "bash");
    assert_eq!(params["arguments"]["command"], "echo one > a.txt");
    assert!(params["risk"].is_array(), "{params}");
    assert!(params.get("rule").is_none(), "{params}");
    // The turn waits for the answer: it is still the running one.
    assert_eq!(client.ok("session/status", json!({}))["turn"], 1);
    assert!(!dir.join("a.txt").exists());
    client.reply(&asked, json!({"decision": "once"}));
    assert_eq!(client.turn_ended(&turn)["status"], "completed");
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap().trim(),
        "one"
    );
    assert!(!mock.body(1).contains("denied permission"));

    // `deny`: the write does not happen, and the model is told why.
    let turn = client.start_turn("write it");
    let asked = client.asked("approval/request");
    assert_eq!(asked["params"]["tool"], "write");
    assert_eq!(asked["params"]["rule"], "write", "{asked}");
    client.reply(&asked, json!({"decision": "deny"}));
    assert_eq!(client.turn_ended(&turn)["status"], "completed");
    assert!(!dir.join("b.txt").exists());
    assert!(
        mock.body(3)
            .contains("the user denied permission to run this tool"),
        "{}",
        mock.body(3)
    );
    let refused = client
        .log
        .iter()
        .rfind(|m| m["method"] == "event/tool_end")
        .unwrap();
    assert_eq!(refused["params"]["ok"], false);
    assert_eq!(refused["params"]["turn"], 2);
}

/// An approval nobody answers is a denial once `--approval-timeout` runs
/// out, and the client is told the request is no longer open.
#[test]
fn an_unanswered_approval_times_out_as_a_denial() {
    let mock = Mock::scripted(|n, _| match n {
        0 => tool_call("call_1", "bash", json!({"command": "echo one > a.txt"})),
        _ => answer("done"),
    });
    let dir = unique_dir("timeout");
    let mut client = Client::ready(&mock, &dir, &["--approval-timeout", "1"]);

    let turn = client.start_turn("run it");
    let asked = client.asked("approval/request");
    let cancelled = client.notification("request/cancelled");
    assert_eq!(cancelled, json!({"id": asked["id"], "reason": "timeout"}));
    assert_eq!(client.turn_ended(&turn)["status"], "completed");
    assert!(!dir.join("a.txt").exists());
    assert!(mock.body(1).contains("the user denied permission"));
    // An answer that comes too late is dropped, and breaks nothing.
    client.reply(&asked, json!({"decision": "once"}));
    assert_eq!(client.ok("session/status", json!({}))["busy"], false);
    assert!(!dir.join("a.txt").exists());
}

/// With the approval reviewer on, the call a client leaves unanswered is not
/// simply refused: once `--approval-timeout` runs out the reviewer judges
/// it, and its verdict is an `event/approval`. (The client was asked first —
/// `when` is "unattended", and a client is somebody to ask.)
#[test]
fn an_approval_the_client_leaves_open_goes_to_the_reviewer() {
    let mock = Mock::scripted(|_, body| {
        if body.contains("You are atelier's approval reviewer") {
            return answer(
                &json!({"decision": "allow", "risk": "low", "reason": "writes one file"})
                    .to_string(),
            );
        }
        // A prompt is answered with the call, the call's result with "done".
        let request: Value = serde_json::from_str(body).expect("a JSON body");
        let last = request["messages"].as_array().and_then(|m| m.last());
        match last.is_some_and(|m| m["role"] == "user") {
            true => tool_call("call_1", "bash", json!({"command": "echo one > a.txt"})),
            false => answer("done"),
        }
    });
    let dir = unique_dir("reviewer");
    let user = dir.join(".no-user-config/atelier");
    std::fs::create_dir_all(&user).unwrap();
    std::fs::write(
        user.join("atelier.toml"),
        "[permissions.reviewer]\nenabled = true\n",
    )
    .unwrap();
    let mut client = Client::ready(&mock, &dir, &["--approval-timeout", "1"]);

    let turn = client.start_turn("run it");
    let asked = client.asked("approval/request");
    let cancelled = client.notification("request/cancelled");
    assert_eq!(cancelled, json!({"id": asked["id"], "reason": "timeout"}));
    let reviewed = client.notification("event/approval");
    assert_eq!(reviewed["by"], "reviewer", "{reviewed}");
    assert_eq!(reviewed["decision"], "allowed");
    assert_eq!(reviewed["risk"], "low");
    assert_eq!(reviewed["reason"], "writes one file");
    assert_eq!(reviewed["tool"], "bash");
    assert_eq!(reviewed["arguments"]["command"], "echo one > a.txt");
    assert_eq!(reviewed["turn"], 1);
    assert_eq!(client.turn_ended(&turn)["status"], "completed");
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap().trim(),
        "one"
    );

    // An answer the client does give is the answer: the reviewer, which
    // would allow, is not consulted about a refusal.
    let before = mock.requests();
    let turn = client.start_turn("again");
    let asked = client.asked("approval/request");
    client.reply(&asked, json!({"decision": "deny"}));
    assert_eq!(client.turn_ended(&turn)["status"], "completed");
    let reviews = (before..mock.requests())
        .filter(|&n| mock.body(n).contains("You are atelier's approval reviewer"))
        .count();
    assert_eq!(reviews, 0);
}

/// `turn/steer` puts a message into the running turn: the model gets it at
/// its next step, and `event/delivered` says when. (The approval request
/// is what holds the turn still while the message is queued.)
#[test]
fn a_steering_message_reaches_the_running_turn() {
    let mock = Mock::scripted(|n, _| match n {
        0 => tool_call("call_1", "bash", json!({"command": "echo first-step"})),
        _ => answer("done"),
    });
    let dir = unique_dir("steer");
    let mut client = Client::ready(&mock, &dir, &[]);

    let turn = client.start_turn("begin");
    let asked = client.asked("approval/request");
    let queued = client.ok("turn/steer", json!({"text": "and use tabs, not spaces"}));
    assert_eq!(queued, json!({"turn": 1, "queued": 1}));
    assert_eq!(client.ok("session/status", json!({}))["queued"], 1);
    client.reply(&asked, json!({"decision": "once"}));

    let delivered = client.notification("event/delivered");
    assert_eq!(
        delivered,
        json!({"turn": 1, "text": "and use tabs, not spaces"})
    );
    let ended = client.turn_ended(&turn);
    assert_eq!(ended["status"], "completed");
    assert!(ended.get("undelivered").is_none(), "{ended}");
    // Same turn, next request: the tool result, then the message.
    assert_eq!(mock.requests(), 2);
    let followup = mock.body(1);
    assert!(followup.contains("first-step"), "{followup}");
    assert!(followup.contains("and use tabs, not spaces"), "{followup}");
    assert!(!mock.body(0).contains("and use tabs"));
}

/// `turn/interrupt` stops a turn whose response is still streaming: the
/// turn ends as `interrupted` without reading the rest, and the session
/// goes on. While the turn runs, a second `turn/start` is refused.
#[test]
fn interrupt_ends_a_turn_held_open_by_the_model() {
    // The first response stops after one chunk until the test lets it go.
    let (release, gate) = mpsc::channel::<()>();
    let gate = Mutex::new(gate);
    let mock = Mock::start(Arc::new(move |n, _body, stream| {
        if n > 0 {
            let _ = stream.write_all(&answer("after"));
            return;
        }
        let _ = stream.write_all(format!("{SSE_HEAD}{}", text_frame("first ")).as_bytes());
        let _ = stream.flush();
        let _ = gate.lock().unwrap().recv_timeout(DEADLINE);
        let rest = [text_frame("second "), text_frame("third")].concat();
        let _ = stream.write_all(format!("{rest}data: [DONE]\n\n").as_bytes());
    }));
    let dir = unique_dir("interrupt");
    let mut client = Client::ready(&mock, &dir, &[]);

    let turn = client.start_turn("go on");
    let first = client.notification("event/text");
    assert_eq!(first, json!({"turn": 1, "text": "first "}));
    let busy = client.err("turn/start", json!({"text": "another"}), TURN_RUNNING);
    assert_eq!(busy["data"]["turn"], 1);
    client.err("command/run", json!({"line": "/mode plan"}), TURN_RUNNING);
    client.err("shutdown", json!({}), TURN_RUNNING);

    assert_eq!(client.ok("turn/interrupt", json!({})), json!({"turn": 1}));
    // The stop is seen at the stream's next chunk, as Ctrl-C is.
    release.send(()).unwrap();
    let ended = client.turn_ended(&turn);
    assert_eq!(ended["status"], "interrupted", "{ended}");
    assert!(!client.text().contains("third"), "{}", client.text());
    client.err("turn/interrupt", json!({}), NO_TURN);

    // The conversation continues, and knows where it was cut.
    let turn = client.start_turn("continue");
    assert_eq!(client.turn_ended(&turn)["status"], "completed");
    assert!(client.text().ends_with("after"));
    assert!(mock.body(1).contains("[stopped by the user]"));
}

/// The `ask` tool's question is a request to the client, and its answer is
/// the tool's result.
#[test]
fn a_question_is_put_to_the_client_and_its_answer_returned() {
    let mock = Mock::scripted(|n, _| match n {
        0 => tool_call(
            "call_1",
            "ask",
            json!({
                "question": "Which one?",
                "options": [{"label": "Alpha", "description": "the first"}, {"label": "Beta"}],
            }),
        ),
        _ => answer("ok"),
    });
    let dir = unique_dir("question");
    let mut client = Client::ready(&mock, &dir, &[]);

    let turn = client.start_turn("choose");
    let asked = client.asked("question/request");
    assert_eq!(
        asked["params"],
        json!({
            "turn": 1,
            "question": "Which one?",
            "options": [{"label": "Alpha", "description": "the first"}, {"label": "Beta"}],
            "multi_select": false,
            "allow_other": true,
        })
    );
    client.reply(&asked, json!({"labels": ["Beta"]}));
    assert_eq!(client.turn_ended(&turn)["status"], "completed");
    assert!(
        mock.body(1)
            .contains(r#"The user chose the option \"Beta\"."#),
        "{}",
        mock.body(1)
    );
}

/// `command/run` runs a slash command between turns, its output arriving as
/// `event/info`; here it switches to plan mode, which the next turn's write
/// then runs into. A line that is no command is handed back as a prompt.
#[test]
fn a_command_switches_to_plan_mode_and_the_next_write_is_refused() {
    let mock = Mock::scripted(|n, _| match n {
        0 => tool_call(
            "call_1",
            "write",
            json!({"path": "out.txt", "content": "nope"}),
        ),
        _ => answer("here is the plan"),
    });
    let dir = unique_dir("command");
    let mut client = Client::ready(&mock, &dir, &[]);

    let result = client.ok("command/run", json!({"line": "/mode plan"}));
    assert_eq!(result, json!({"handled": true, "kind": "handled"}));
    let info = client.notification("event/info");
    assert!(
        info["text"].as_str().unwrap().starts_with("mode: plan"),
        "{info}"
    );
    assert!(info.get("turn").is_none(), "{info}");
    assert_eq!(client.ok("session/status", json!({}))["mode"], "plan");

    let prompt = client.ok("command/run", json!({"line": "just some words"}));
    assert_eq!(
        prompt,
        json!({"handled": false, "kind": "prompt", "text": "just some words"})
    );
    assert_eq!(mock.requests(), 0);

    let turn = client.start_turn("change things");
    assert_eq!(client.turn_ended(&turn)["status"], "completed");
    assert!(!dir.join("out.txt").exists());
    let refused = client.notification("event/tool_end");
    assert_eq!(refused["ok"], false);
    // Nothing was written, so nothing is said to have changed.
    assert!(!client.methods().contains(&"event/file_change".to_string()));
    assert!(
        refused["output"].as_str().unwrap().contains("plan mode"),
        "{refused}"
    );
    assert!(mock.body(0).contains("PLAN MODE"));
    assert!(mock.body(1).contains("plan mode: write is not available"));
}

/// A line that is not JSON, or not a message, gets an error with a null id
/// and the session carries on.
#[test]
fn malformed_lines_are_answered_and_survived() {
    let mock = Mock::scripted(|_, _| answer("fine"));
    let dir = unique_dir("malformed");
    let mut client = Client::ready(&mock, &dir, &[]);

    let null_error = |client: &mut Client| {
        let message = client.take("an error with a null id", |m| m["id"].is_null());
        message["error"]["code"].as_i64().unwrap()
    };
    client.write_line("this is not json");
    assert_eq!(null_error(&mut client), PARSE_ERROR);
    client.write_line("{\"jsonrpc\": \"2.0\", \"id\": 1, \"method\": ");
    assert_eq!(null_error(&mut client), PARSE_ERROR);
    client.write_line("[1, 2]");
    assert_eq!(null_error(&mut client), INVALID_REQUEST);
    // Blank lines separate nothing.
    client.write_line("");
    // An object that is neither a request nor an answer keeps its id.
    client.write_line(r#"{"jsonrpc": "2.0", "id": "odd"}"#);
    let odd = client.response(&json!("odd"));
    assert_eq!(odd["error"]["code"], INVALID_REQUEST);
    // An answer to a request that was never made is dropped.
    client.write_line(r#"{"jsonrpc": "2.0", "id": 999, "result": {}}"#);

    let turn = client.start_turn("still there?");
    assert_eq!(client.turn_ended(&turn)["status"], "completed");
    assert_eq!(client.text(), "fine");
}

/// `shutdown` answers and the process exits cleanly.
#[test]
fn shutdown_exits_cleanly() {
    let mock = Mock::scripted(|_, _| answer("unused"));
    let dir = unique_dir("shutdown");
    let mut client = Client::ready(&mock, &dir, &[]);

    client.err("shutdown", json!({"force": "yes"}), INVALID_PARAMS);
    assert_eq!(client.ok("shutdown", json!({})), json!({}));
    let status = client.exited();
    assert_eq!(status.code(), Some(0));
}

/// The client going away (EOF on stdin) is a shutdown too.
#[test]
fn end_of_input_shuts_down() {
    let mock = Mock::scripted(|_, _| answer("unused"));
    let dir = unique_dir("eof");
    let mut client = Client::ready(&mock, &dir, &[]);

    drop(client.stdin.take());
    let status = client.exited();
    assert_eq!(status.code(), Some(0));
}

/// Requests written right behind `initialize`, without waiting for its
/// result, are served after it rather than refused.
#[test]
fn requests_sent_behind_initialize_wait_for_it() {
    let mock = Mock::scripted(|_, _| answer("unused"));
    let dir = unique_dir("pipelined");
    let mut client = Client::spawn(&mock, &dir, &[]);

    let hello = client.request("initialize", json!({}));
    let status = client.request("session/status", json!({}));
    // In order: nothing is read between the two but the handshake's result.
    assert_eq!(client.recv("the handshake's result")["id"], hello);
    assert_eq!(client.response(&status)["result"]["busy"], false);
}

/// Images ride with `turn/start`, as paths or `data:` URLs; one that cannot
/// be read ends the turn with an error before the model is asked anything,
/// and leaves nothing attached to the next turn.
#[test]
fn images_are_attached_or_the_turn_fails() {
    let mock = Mock::scripted(|_, _| answer("a cat"));
    let dir = unique_dir("images");
    let mut client = Client::ready(&mock, &dir, &[]);
    let picture = "data:image/png;base64,aGVsbG8=";

    let with_images = |client: &mut Client, images: Value| {
        let params = json!({"text": "what is this?", "images": images});
        client.ok("turn/start", params)["turn"].clone()
    };
    let turn = with_images(&mut client, json!([picture, "missing.png"]));
    let ended = client.turn_ended(&turn);
    assert_eq!(ended["status"], "error", "{ended}");
    assert!(
        ended["error"].as_str().unwrap().contains("missing.png"),
        "{ended}"
    );
    assert_eq!(ended["steps"], 0);
    assert_eq!(mock.requests(), 0);

    let turn = client.start_turn("never mind the picture");
    assert_eq!(client.turn_ended(&turn)["status"], "completed");
    assert!(!mock.body(0).contains(picture));

    let turn = with_images(&mut client, json!([picture]));
    assert_eq!(client.turn_ended(&turn)["status"], "completed");
    assert!(mock.body(1).contains(picture), "{}", mock.body(1));
}

/// `session/history` gives the conversation back as items — the prompt, each
/// tool call with its result, the answer — without the context atelier put
/// around them, a page at a time; it waits for the session to be idle; and
/// a session resumed with `--continue` has the same history to show.
#[test]
fn history_is_the_conversation_so_far_and_survives_continue() {
    let mock = Mock::scripted(|n, _| match n {
        0 => tool_call(
            "call_1",
            "write",
            json!({"path": "out.txt", "content": "written by tool"}),
        ),
        1 => answer("done"),
        2 => tool_call("call_2", "bash", json!({"command": "echo hi"})),
        _ => answer("as you wish"),
    });
    let dir = unique_dir("history");
    let mut client = Client::ready(&mock, &dir, &[]);

    // Nothing yet.
    let empty = client.ok("session/history", json!({}));
    assert_eq!(
        empty,
        json!({"items": [], "first": 0, "total": 0, "more": false, "stored": true})
    );
    client.err("session/history", json!({"limit": 0}), INVALID_PARAMS);
    client.err("session/history", json!({"before": "x"}), INVALID_PARAMS);

    let turn = client.start_turn("write the file");
    assert_eq!(client.turn_ended(&turn)["status"], "completed");
    let history = client.ok("session/history", json!({}));
    let expected = json!([
        {"index": 0, "type": "user", "text": "write the file", "images": 0},
        {
            "index": 1,
            "type": "tool",
            "id": "call_1",
            "name": "write",
            "arguments": {"path": "out.txt", "content": "written by tool"},
            "ok": true,
            "output": "wrote 15 bytes to out.txt",
        },
        {"index": 2, "type": "assistant", "text": "done"},
    ]);
    assert_eq!(history["items"], expected, "{history}");
    assert_eq!(history["total"], 3);
    assert_eq!(history["more"], false);
    assert_eq!(history["stored"], true);

    // A page is the end of the list; `before` walks back from it.
    let last = client.ok("session/history", json!({"limit": 1}));
    assert_eq!(last["items"], json!([expected[2]]));
    assert_eq!(last["first"], 2);
    assert_eq!(last["more"], true);
    let earlier = client.ok("session/history", json!({"limit": 5, "before": 2}));
    assert_eq!(earlier["items"], json!([expected[0], expected[1]]));
    assert_eq!(earlier["more"], false);

    // While a turn runs — here, waiting on an approval — the session is
    // not there to be asked.
    let turn = client.start_turn("run it");
    let asked = client.asked("approval/request");
    let busy = client.err("session/history", json!({}), TURN_RUNNING);
    assert_eq!(busy["data"]["turn"], 2);
    client.reply(&asked, json!({"decision": "deny"}));
    assert_eq!(client.turn_ended(&turn)["status"], "completed");
    let history = client.ok("session/history", json!({}));
    assert_eq!(history["total"], 6, "{history}");
    let refused = &history["items"][4];
    assert_eq!(refused["name"], "bash");
    assert_eq!(refused["ok"], false, "{refused}");
    assert!(
        refused["output"].as_str().unwrap().contains("denied"),
        "{refused}"
    );

    client.ok("shutdown", json!({}));
    assert_eq!(client.exited().code(), Some(0));

    // Another process, the same conversation.
    let mut resumed = Client::ready(&mock, &dir, &["--continue"]);
    let again = resumed.ok("session/history", json!({}));
    assert_eq!(again, history);
    // Without `--continue` the session starts empty.
    drop(resumed);
    let mut fresh = Client::ready(&mock, &dir, &[]);
    assert_eq!(fresh.ok("session/history", json!({}))["total"], 0);
}

/// A tool that writes a file is followed by `event/file_change`: the file,
/// what became of it, and the change as a unified diff — between the
/// call's start and its end. A call that fails before it writes says
/// nothing.
#[test]
fn an_edit_and_a_new_file_are_reported_as_diffs() {
    let mock = Mock::scripted(|n, _| match n {
        0 => tool_call("call_1", "read", json!({"path": "src/a.txt"})),
        1 => tool_call(
            "call_2",
            "edit",
            json!({"path": "src/a.txt", "old_string": "two", "new_string": "2"}),
        ),
        2 => tool_call(
            "call_3",
            "write",
            json!({"path": "new.txt", "content": "fresh\n"}),
        ),
        // Not read first: refused by the tool, nothing written.
        3 => tool_call(
            "call_4",
            "edit",
            json!({"path": "other.txt", "old_string": "x", "new_string": "y"}),
        ),
        // The same text again: written, and nothing changed.
        4 => tool_call(
            "call_5",
            "write",
            json!({"path": "new.txt", "content": "fresh\n"}),
        ),
        _ => answer("done"),
    });
    let dir = unique_dir("diff");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/a.txt"), "one\ntwo\nthree\n").unwrap();
    std::fs::write(dir.join("other.txt"), "x\n").unwrap();
    let mut client = Client::ready(&mock, &dir, &[]);

    let turn = client.start_turn("change things");
    assert_eq!(client.turn_ended(&turn)["status"], "completed");
    assert_eq!(
        std::fs::read_to_string(dir.join("src/a.txt")).unwrap(),
        "one\n2\nthree\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("other.txt")).unwrap(),
        "x\n"
    );

    let changes: Vec<Value> = client
        .log
        .iter()
        .filter(|m| m["method"] == "event/file_change")
        .map(|m| m["params"].clone())
        .collect();
    assert_eq!(changes.len(), 2, "{changes:?}");

    let edited = &changes[0];
    assert_eq!(edited["turn"], 1);
    assert_eq!(edited["tool"], "edit");
    assert_eq!(edited["kind"], "modified");
    assert_eq!(edited["relative_path"], "src/a.txt");
    let path = Path::new(edited["path"].as_str().unwrap());
    assert!(
        path.is_absolute() && path.ends_with("src/a.txt"),
        "{edited}"
    );
    assert_eq!(
        edited["diff"],
        "--- a/src/a.txt\n+++ b/src/a.txt\n@@ -1,3 +1,3 @@\n one\n-two\n+2\n three\n"
    );
    assert_eq!(edited["added"], 1);
    assert_eq!(edited["removed"], 1);
    assert!(edited.get("truncated").is_none() && edited.get("binary").is_none());

    let created = &changes[1];
    assert_eq!(created["tool"], "write");
    assert_eq!(created["kind"], "created");
    assert_eq!(created["relative_path"], "new.txt");
    assert_eq!(
        created["diff"],
        "--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+fresh\n"
    );

    // Each change sits inside its call.
    let methods = client.methods();
    let around: Vec<&str> = methods
        .iter()
        .map(String::as_str)
        .filter(|m| m.starts_with("event/tool_") || *m == "event/file_change")
        .collect();
    assert_eq!(
        around,
        [
            "event/tool_start",
            "event/tool_end",
            "event/tool_start",
            "event/file_change",
            "event/tool_end",
            "event/tool_start",
            "event/file_change",
            "event/tool_end",
            "event/tool_start",
            "event/tool_end",
            "event/tool_start",
            "event/tool_end",
        ]
    );
    let failed: Vec<&Value> = client
        .log
        .iter()
        .filter(|m| m["method"] == "event/tool_end" && m["params"]["ok"] == false)
        .collect();
    assert_eq!(failed.len(), 1, "{failed:?}");
}

// --------------------------------------------------------------- settings

/// The user-global `atelier.toml` of a session served in `dir`: under the
/// `XDG_CONFIG_HOME` the client gives it, never the real one.
fn user_settings(dir: &Path) -> PathBuf {
    dir.join(".no-user-config")
        .join("atelier")
        .join("atelier.toml")
}

/// The entry of `settings/get` for `key`.
fn setting<'a>(settings: &'a Value, key: &str) -> &'a Value {
    let list = settings["settings"].as_array().expect("a list of settings");
    list.iter()
        .find(|s| s["key"] == key)
        .unwrap_or_else(|| panic!("no setting '{key}' in {settings}"))
}

/// The value in force and its source, of an entry of `settings/get`.
fn held(entry: &Value) -> (Value, &str) {
    (
        entry["value"].clone(),
        entry["source"].as_str().expect("a source"),
    )
}

/// `settings/get` lists every setting with its value and where it comes
/// from — a flag, the environment, the project's file, the user's, the
/// built-in default — says which values do not count and why (an untrusted
/// project's, a key in the wrong file), describes each setting well enough
/// to draw a form, and lets no key through.
#[test]
fn settings_are_reported_with_where_each_value_comes_from() {
    let mock = Mock::scripted(|_, _| answer("unused"));
    let dir = unique_dir("settings-get");
    std::fs::write(
        dir.join("atelier.toml"),
        r#"
[defaults]
context_limit = 4000
mode = "yolo"

[[mcp_http]]
name = "web"
url = "http://127.0.0.1:1/mcp"
headers = ["Authorization: Bearer sk-secret-header"]

[local]
model = "someone/elses-model"
"#,
    )
    .unwrap();
    let user = user_settings(&dir);
    std::fs::create_dir_all(user.parent().unwrap()).unwrap();
    std::fs::write(
        &user,
        r#"
[models]
small = "tiny"

[[provider]]
name = "hosted"
base_url = "http://127.0.0.1:1/v1"
api_key_command = "echo sk-secret-command"
headers = { "X-Token" = "sk-secret-table" }
"#,
    )
    .unwrap();
    let env = [("ATELIER_API_KEY", Some("sk-secret-env"))];
    let mut client = Client::ready_env(&mock, &dir, &[], &env);

    let settings = client.ok("settings/get", json!({}));
    assert_eq!(
        settings["files"]["project"],
        json!({"path": dir.join("atelier.toml"), "exists": true})
    );
    assert_eq!(
        settings["files"]["user"],
        json!({"path": user, "exists": true})
    );
    assert_eq!(
        settings["trust"],
        json!({"state": "untrusted", "active": false})
    );

    // The environment over everything.
    let model = setting(&settings, "defaults.model");
    assert_eq!(held(model), (json!("mock"), "env"));
    assert_eq!(model["layers"][0]["name"], "ATELIER_MODEL");
    let last = model["layers"].as_array().unwrap().last().unwrap();
    assert_eq!(last["source"], "default");
    // The project's file, the user's, the default.
    let limit = setting(&settings, "defaults.context_limit");
    assert_eq!(held(limit), (json!(4000), "project"));
    let small = setting(&settings, "models.small");
    assert_eq!(held(small), (json!("tiny"), "user"));
    let review = setting(&settings, "review.enabled");
    assert_eq!(held(review), (json!(false), "default"));

    // An untrusted project does not get to start in yolo, nor to contact
    // its servers: the values are there, marked as not counting.
    let mode = setting(&settings, "defaults.mode");
    assert_eq!(held(mode), (json!("auto-edit"), "default"));
    let ignored = &mode["layers"][0];
    assert_eq!(ignored["source"], "project");
    assert_eq!(ignored["value"], "yolo");
    let why = ignored["ignored"].as_str().unwrap();
    assert!(why.contains("not trusted"), "{mode}");
    let servers = setting(&settings, "mcp_http");
    assert!(servers["value"].is_null(), "{servers}");
    assert!(servers["layers"][0]["ignored"].is_string(), "{servers}");
    assert_eq!(
        servers["layers"][0]["value"][0]["headers"],
        json!(["Authorization"])
    );
    // What only the user's own file may say.
    let local = setting(&settings, "local.model");
    assert!(local["value"].is_null(), "{local}");
    let why = local["layers"][0]["ignored"].as_str().unwrap();
    assert!(why.contains("user-global file only"), "{local}");

    // Enough to draw a form with.
    assert_eq!(mode["type"], "string");
    assert_eq!(
        mode["values"],
        json!(["plan", "ask-edits", "auto-edit", "yolo"])
    );
    assert_eq!(mode["settable"], false);
    assert_eq!(limit["type"], "integer");
    assert_eq!(limit["label"], "context limit");
    assert_eq!(limit["settable"], true);
    assert_eq!(limit["scopes"], json!(["project"]));
    assert_eq!(model["scopes"], json!(["user", "project"]));
    for entry in settings["settings"].as_array().unwrap() {
        let described = entry["description"].as_str().is_some_and(|d| !d.is_empty());
        assert!(described, "{entry}");
        assert!(entry["settable"].is_boolean(), "{entry}");
        assert!(entry["layers"].is_array(), "{entry}");
    }

    // Keys: that there is one, and where — never the key.
    let provider = &setting(&settings, "provider")["value"][0];
    assert_eq!(provider["api_key_command"], true);
    assert_eq!(provider["headers"], json!(["X-Token"]));
    let key = &settings["keys"][0];
    assert_eq!(key["name"], "ATELIER_API_KEY");
    assert_eq!(key["set"], true);
    assert_eq!(key["source"], "env");
    assert_eq!(settings["keys"][1]["set"], false);
    assert!(!settings.to_string().contains("sk-secret"), "{settings}");

    // A mode given on the command line is told from one configured.
    drop(client);
    let mut client = Client::ready(&mock, &dir, &["--mode", "plan"]);
    let settings = client.ok("settings/get", json!({}));
    assert_eq!(
        held(setting(&settings, "defaults.mode")),
        (json!("plan"), "flag")
    );
    // …and one switched during the session from both.
    client.ok("command/run", json!({"line": "/mode ask-edits"}));
    let settings = client.ok("settings/get", json!({}));
    assert_eq!(
        held(setting(&settings, "defaults.mode")),
        (json!("ask-edits"), "session")
    );
}

/// `settings/set` writes the user's own file by default and the project's
/// when asked, puts the value into effect at once where it is the one that
/// counts, and the next `settings/get` — and the next session — has it.
#[test]
fn a_setting_is_written_applied_and_read_back() {
    let mock = Mock::scripted(|_, _| answer("unused"));
    let dir = unique_dir("settings-set");
    let user = user_settings(&dir);
    // No ATELIER_MODEL: the files decide the model.
    let env = [("ATELIER_MODEL", None)];
    let mut client = Client::ready_env(&mock, &dir, &[], &env);
    assert!(!user.exists());

    // The user's file, created for the occasion; in effect at once.
    let set = client.ok(
        "settings/set",
        json!({"key": "defaults.model", "value": "next-model"}),
    );
    assert_eq!(set["scope"], "user");
    assert_eq!(set["applies"], "now");
    assert_eq!(set["file"], json!(user));
    assert_eq!(set["setting"]["source"], "user");
    let written = std::fs::read_to_string(&user).unwrap();
    assert!(written.contains("model = \"next-model\""), "{written}");
    assert!(!dir.join("atelier.toml").exists());
    assert_eq!(
        client.ok("session/status", json!({}))["model"],
        "next-model"
    );
    let settings = client.ok("settings/get", json!({}));
    assert_eq!(
        held(setting(&settings, "defaults.model")),
        (json!("next-model"), "user")
    );

    // The project's file, for what is read from there.
    let set = client.ok(
        "settings/set",
        json!({"key": "defaults.context_limit", "value": 1234, "scope": "project"}),
    );
    assert_eq!(set["scope"], "project");
    assert_eq!(set["applies"], "now");
    assert_eq!(set["file"], json!(dir.join("atelier.toml")));
    let project = std::fs::read_to_string(dir.join("atelier.toml")).unwrap();
    assert!(project.contains("context_limit = 1234"), "{project}");
    // Applying the model did not carry it into the project's file.
    assert!(!project.contains("next-model"), "{project}");
    assert_eq!(
        client.ok("session/status", json!({}))["context_limit"],
        1234
    );
    let set = client.ok(
        "settings/set",
        json!({"key": "review.enabled", "value": true, "scope": "project"}),
    );
    assert_eq!(set["setting"]["value"], true);
    let settings = client.ok("settings/get", json!({}));
    assert_eq!(
        held(setting(&settings, "defaults.context_limit")),
        (json!(1234), "project")
    );
    assert_eq!(setting(&settings, "review.enabled")["source"], "project");

    // `null` takes the key out; the running session keeps what it has.
    let set = client.ok(
        "settings/set",
        json!({"key": "defaults.context_limit", "value": null, "scope": "project"}),
    );
    assert_eq!(set["applies"], "next_start");
    let project = std::fs::read_to_string(dir.join("atelier.toml")).unwrap();
    assert!(!project.contains("context_limit"), "{project}");
    assert_eq!(set["setting"]["source"], "session");
    assert_eq!(
        client.ok("session/status", json!({}))["context_limit"],
        1234
    );

    // The next session starts with what was written.
    drop(client);
    let mut client = Client::spawn_env(&mock, &dir, &[], &env);
    let hello = client.ok("initialize", json!({}));
    assert_eq!(hello["session"]["model"], "next-model");
    let status = client.ok("session/status", json!({}));
    assert_eq!(status["context_limit"], 8000);
    drop(client);

    // Where something stronger decides, the value is written and the
    // answer says what stands in its way.
    let mut client = Client::ready(&mock, &dir, &[]);
    let set = client.ok(
        "settings/set",
        json!({"key": "defaults.model", "value": "later-model"}),
    );
    assert_eq!(set["applies"], "overridden");
    assert_eq!(set["by"]["source"], "env");
    assert_eq!(set["by"]["name"], "ATELIER_MODEL");
    assert_eq!(client.ok("session/status", json!({}))["model"], "mock");
    let written = std::fs::read_to_string(&user).unwrap();
    assert!(written.contains("later-model"), "{written}");
}

/// What `settings/set` refuses, each as invalid params and without a file
/// written: a key that is none, a value of the wrong type, a setting that
/// is not the client's to change — above all one gated by trust or reserved
/// to the user's own file, at project scope — and a file the setting is not
/// read from. And, like a command, both methods wait for the turn to end.
#[test]
fn settings_that_cannot_be_set_are_refused() {
    let mock = Mock::scripted(|n, _| match n {
        0 => tool_call("call_1", "bash", json!({"command": "echo hi"})),
        _ => answer("done"),
    });
    let dir = unique_dir("settings-refused");
    let mut client = Client::ready(&mock, &dir, &[]);

    let refused = [
        json!({"key": "no.such", "value": 1}),
        json!({"value": 1}),
        json!({"key": "defaults.model"}),
        json!({"key": "defaults.model", "value": 7}),
        json!({"key": "defaults.model", "value": ""}),
        json!({"key": "defaults.model", "value": "m", "scope": "everywhere"}),
        json!({"key": "defaults.context_limit", "value": "many", "scope": "project"}),
        json!({"key": "defaults.context_limit", "value": 0, "scope": "project"}),
        json!({"key": "review.enabled", "value": "yes", "scope": "project"}),
        // Read from the project's file only: the user's would not count.
        json!({"key": "defaults.context_limit", "value": 9000}),
        json!({"key": "defaults.context_limit", "value": 9000, "scope": "user"}),
        // Gated by trust, or the user's own file's alone: at no scope.
        json!({"key": "defaults.approve", "value": true, "scope": "project"}),
        json!({"key": "defaults.approve", "value": true}),
        json!({"key": "defaults.mode", "value": "yolo", "scope": "project"}),
        json!({"key": "local.model", "value": "x", "scope": "project"}),
        json!({"key": "sandbox.root", "value": "/", "scope": "project"}),
        json!({"key": "permissions.allow", "value": ["bash"], "scope": "project"}),
        json!({"key": "provider", "value": [], "scope": "project"}),
        json!({"key": "search.key_env", "value": "HOME"}),
    ];
    for params in refused {
        let error = client.err("settings/set", params.clone(), INVALID_PARAMS);
        assert!(error["message"].is_string(), "{params}: {error}");
    }
    assert!(!dir.join("atelier.toml").exists());
    assert!(!user_settings(&dir).exists());

    // A turn held at its approval prompt: the settings wait for it.
    let turn = client.start_turn("run it");
    let asked = client.asked("approval/request");
    client.err("settings/get", json!({}), TURN_RUNNING);
    let set = json!({"key": "review.enabled", "value": true, "scope": "project"});
    client.err("settings/set", set.clone(), TURN_RUNNING);
    client.reply(&asked, json!({"decision": "deny"}));
    client.turn_ended(&turn);
    assert!(!dir.join("atelier.toml").exists());
    assert_eq!(client.ok("settings/set", set)["applies"], "now");
}

// ------------------------------------------------------------- the queries

/// The answer of a `GET /models`: the ids, as an OpenAI-compatible server
/// lists them.
fn model_list(ids: &[&str]) -> Vec<u8> {
    let data: Vec<Value> = ids.iter().map(|id| json!({"id": id})).collect();
    let body = json!({"object": "list", "data": data}).to_string();
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// The element of the array `list` whose `key` is `value`.
fn entry<'a>(list: &'a Value, key: &str, value: &str) -> &'a Value {
    let found = list.as_array().and_then(|all| {
        all.iter()
            .find(|e| e[key].as_str().is_some_and(|v| v == value))
    });
    found.unwrap_or_else(|| panic!("no entry with {key} = {value} in {list}"))
}

/// The queries are advertised in the handshake, and each needs the session
/// idle: while a turn runs they are refused like `command/run` (a bad
/// parameter still being a bad parameter). Between turns `jobs/list` says
/// nothing runs in the background, and `usage/get` counts the requests the
/// turn made.
#[test]
fn queries_are_advertised_and_wait_for_the_session_to_be_idle() {
    let mock = Mock::scripted(|n, _| match n {
        0 => tool_call("call_1", "bash", json!({"command": "echo one > a.txt"})),
        _ => answer("done"),
    });
    let dir = unique_dir("queries");
    let mut client = Client::spawn(&mock, &dir, &[]);
    let hello = client.ok("initialize", json!({}));
    let queries = &hello["capabilities"]["queries"];
    assert_eq!(
        queries,
        &json!([
            "models/list",
            "mcp/list",
            "peers/list",
            "usage/get",
            "memory/list",
            "commands/list",
            "jobs/list"
        ])
    );

    let usage = client.ok("usage/get", json!({}));
    assert_eq!(usage["backend"], "openai");
    assert_eq!(usage["by_model"], json!([]));
    assert_eq!(usage["total"]["requests"], 0);

    // The approval prompt holds the turn still.
    let turn = client.start_turn("run it");
    let asked = client.asked("approval/request");
    for method in queries.as_array().unwrap() {
        let refused = client.err(method.as_str().unwrap(), json!({}), TURN_RUNNING);
        assert_eq!(refused["data"]["turn"], turn, "{refused}");
    }
    client.err("models/list", json!({"fetch": "no"}), INVALID_PARAMS);
    assert_eq!(client.ok("session/status", json!({}))["busy"], true);
    client.reply(&asked, json!({"decision": "deny"}));
    assert_eq!(client.turn_ended(&turn)["status"], "completed");

    let jobs = client.ok("jobs/list", json!({}));
    assert_eq!(
        jobs,
        json!({"jobs": [], "agents": [], "claude_background": 0})
    );
    let usage = client.ok("usage/get", json!({}));
    let model = entry(&usage["by_model"], "model", "env/mock");
    assert_eq!(model["requests"], 2, "{usage}");
    assert_eq!(usage["total"]["requests"], 2);
    // No `[[price]]` names the model.
    assert_eq!(usage["total"]["cost"], Value::Null);
    assert_eq!(usage["total"]["priced_requests"], 0);
    assert_eq!(mock.requests(), 2);
    // A query leaves the session free for the next thing.
    assert_eq!(client.ok("session/status", json!({}))["busy"], false);
}

/// `models/list`: the model in use, each provider with the models it offers
/// (or why it could not be asked) and where its key comes from — never the
/// key — the model per role and the variants. `fetch: false` leaves the
/// providers alone, and `/model` shows in the next answer.
#[test]
fn models_list_describes_providers_roles_and_variants() {
    let mock = Mock::scripted(|_, body| match body.is_empty() {
        true => model_list(&["alpha", "beta"]),
        false => answer("unused"),
    });
    let dir = unique_dir("models");
    let user = dir.join(".no-user-config/atelier");
    std::fs::create_dir_all(&user).unwrap();
    std::fs::write(
        user.join("atelier.toml"),
        "[[provider]]\nname = \"second\"\nbase_url = \"http://127.0.0.1:1/v1\"\n\
         api_key_env = \"SERVE_TEST_KEY\"\n\
         headers = { \"x-extra\" = \"header-secret\" }\n\n\
         [models]\nsmall = \"second/tiny#high\"\n\n\
         [variant.fast]\ntemperature = 0.25\n",
    )
    .unwrap();
    let env = [("SERVE_TEST_KEY", "sk-very-secret")];
    let mut client = Client::ready_with(&mock, &dir, &[], &env);

    let models = client.ok("models/list", json!({}));
    assert_eq!(models["backend"], "openai");
    assert_eq!(
        models["current"],
        json!({"label": "mock", "model": "mock", "provider": "env", "id": "mock",
               "variant": null, "options": {}})
    );
    let first = entry(&models["providers"], "name", "env");
    assert_eq!(first["models"], json!(["alpha", "beta"]));
    assert_eq!(first["error"], Value::Null);
    assert_eq!(first["default"], true);
    assert_eq!(first["base_url"], mock.base_url());
    assert_eq!(first["key"], "none");
    let second = entry(&models["providers"], "name", "second");
    assert_eq!(second["models"], Value::Null);
    assert!(second["error"].is_string(), "{second}");
    assert_eq!(second["default"], false);
    assert_eq!(second["key"], "$SERVE_TEST_KEY (set)");

    let main = entry(&models["roles"], "role", "main");
    assert_eq!(main["provider"], "env");
    assert_eq!(main["id"], "mock");
    let small = entry(&models["roles"], "role", "small");
    assert_eq!(
        small,
        &json!({"role": "small", "model": "second/tiny#high", "provider": "second",
                "id": "tiny", "options": {"reasoning_effort": "high"}, "own": true})
    );
    // Not set: it follows the role it falls back to.
    let summary = entry(&models["roles"], "role", "summary");
    assert_eq!(summary["id"], "tiny");
    assert_eq!(entry(&models["roles"], "role", "subagent")["own"], false);
    let fast = entry(&models["variants"], "name", "fast");
    assert_eq!(fast["options"], json!({"temperature": 0.25}));
    entry(&models["variants"], "name", "high");

    let text = models.to_string();
    assert!(!text.contains("sk-very-secret"), "{text}");
    assert!(!text.contains("header-secret"), "{text}");

    // Without asking the providers: the same, less what they offer.
    let asked = mock.requests();
    client.ok("command/run", json!({"line": "/model second/tiny#fast"}));
    let models = client.ok("models/list", json!({"fetch": false}));
    assert_eq!(mock.requests(), asked);
    assert_eq!(
        models["current"],
        json!({"label": "second/tiny#fast", "model": "second/tiny#fast",
               "provider": "second", "id": "tiny", "variant": "fast",
               "options": {"temperature": 0.25}})
    );
    let first = entry(&models["providers"], "name", "env");
    assert_eq!(first["models"], Value::Null);
    assert_eq!(first["error"], Value::Null);
}

/// `mcp/list`: each configured server with where it stands — connected
/// (and its tools), disabled, failed and why — and how it is reached,
/// without the headers it is sent. In a project that is not trusted no
/// server is started, and the list says so.
#[test]
fn mcp_list_reports_each_server_and_its_tools() {
    let mock = Mock::scripted(|_, _| answer("unused"));
    let dir = unique_dir("mcp");
    // atelier itself is the server: its own tools over MCP (`atelier mcp`).
    let atelier = env!("CARGO_BIN_EXE_atelier");
    std::fs::write(
        dir.join("atelier.toml"),
        format!(
            "[[mcp]]\nname = \"self\"\ncommand = '{atelier}'\nargs = [\"mcp\"]\n\n\
             [[mcp]]\nname = \"broken\"\ncommand = '{}'\n\n\
             [[mcp]]\nname = \"off\"\ncommand = \"never-run\"\nenabled = false\n\n\
             [[mcp_http]]\nname = \"remote\"\nurl = \"http://127.0.0.1:1/mcp\"\n\
             headers = [\"Authorization: Bearer sk-header-secret\"]\n",
            dir.join("no-such-server").display()
        ),
    )
    .unwrap();

    let trusted = [("ATELIER_TRUST_PROJECT", "1")];
    let mut client = Client::ready_with(&mock, &dir, &[], &trusted);
    let listed = client.ok("mcp/list", json!({}));
    let servers = &listed["servers"];
    assert_eq!(servers.as_array().unwrap().len(), 4, "{listed}");

    let own = entry(servers, "name", "self");
    assert_eq!(own["status"], "connected", "{own}");
    assert_eq!(own["error"], Value::Null);
    assert_eq!(own["transport"], "stdio");
    assert_eq!(own["command"], atelier);
    assert_eq!(own["args"], json!(["mcp"]));
    assert_eq!(own["enabled"], true);
    assert_eq!(own["configured"], true);
    let read = entry(&own["tools"], "name", "mcp__self__read");
    assert_eq!(read["tool"], "read");
    // Whether they are advertised one by one depends on how many there are.
    assert!(read["deferred"].is_boolean(), "{read}");
    assert!(!read["description"].as_str().unwrap().is_empty(), "{read}");

    let broken = entry(servers, "name", "broken");
    assert_eq!(broken["status"], "failed", "{broken}");
    assert!(broken["error"].is_string(), "{broken}");
    assert_eq!(broken["tools"], json!([]));
    let off = entry(servers, "name", "off");
    assert_eq!(off["status"], "disabled");
    assert_eq!(off["enabled"], false);
    let remote = entry(servers, "name", "remote");
    assert_eq!(remote["status"], "failed", "{remote}");
    assert_eq!(remote["transport"], "http");
    assert_eq!(remote["url"], "http://127.0.0.1:1/mcp");
    assert!(!listed.to_string().contains("sk-header-secret"), "{listed}");

    // `/mcp` prints the same servers, as it always did.
    client.ok("command/run", json!({"line": "/mcp"}));
    let line = format!("  self → {atelier} mcp");
    client.take("the server's line", |m| {
        m["params"]["text"] == line.as_str()
    });
    client.ok("shutdown", json!({}));
    assert!(client.exited().success());

    let mut client = Client::ready(&mock, &dir, &[]);
    let listed = client.ok("mcp/list", json!({}));
    for name in ["self", "broken", "remote"] {
        let server = entry(&listed["servers"], "name", name);
        assert_eq!(server["status"], "untrusted", "{server}");
        assert_eq!(server["tools"], json!([]));
    }
    assert_eq!(
        entry(&listed["servers"], "name", "off")["status"],
        "disabled"
    );
}

/// `memory/list`, `commands/list` and `peers/list`: the notes and the
/// instruction files, the commands and the skills — each with the file it
/// comes from — and, with the coordinator off, no peers.
#[test]
fn memory_commands_and_peers_are_listed_as_data() {
    let mock = Mock::scripted(|_, _| answer("unused"));
    let dir = unique_dir("lists");
    let write = |path: &str, text: &str| {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    write("AGENTS.md", "Use tabs.\n");
    write(
        ".atelier/memory/build.md",
        "---\nname: build\ndescription: how to build it\ntype: reference\n---\nRun make.\n",
    );
    write(
        ".atelier/commands/fix.md",
        "---\ndescription: fix an issue\nargument-hint: <issue>\nmodel: big\n---\nFix $1.\n",
    );
    // A file cannot replace a built-in command, so it is not one to list.
    write(".atelier/commands/help.md", "Never reached.\n");
    write(
        ".atelier/skills/deploy/SKILL.md",
        "---\nname: deploy\ndescription: ship a release\n---\nSteps.\n",
    );
    write(".atelier/skills/empty/SKILL.md", "no front matter\n");
    let mut client = Client::ready(&mock, &dir, &[]);

    let memory = client.ok("memory/list", json!({}));
    let project = entry(&memory["scopes"], "scope", "project");
    assert!(
        Path::new(project["dir"].as_str().unwrap()).ends_with(".atelier/memory"),
        "{project}"
    );
    let notes = project["memories"].as_array().unwrap();
    assert_eq!(notes.len(), 1, "{project}");
    assert_eq!(notes[0]["name"], "build");
    assert_eq!(notes[0]["type"], "reference");
    assert_eq!(notes[0]["description"], "how to build it");
    assert!(Path::new(notes[0]["path"].as_str().unwrap()).ends_with(".atelier/memory/build.md"));
    let global = entry(&memory["scopes"], "scope", "global");
    assert_eq!(global["memories"], json!([]));
    let files = memory["instruction_files"].as_array().unwrap();
    assert_eq!(files.len(), 1, "{memory}");
    assert!(Path::new(files[0]["path"].as_str().unwrap()).ends_with("AGENTS.md"));
    // Asking writes nothing (`/memory` brings the index file up to date).
    assert!(!dir.join(".atelier/memory/MEMORY.md").exists());

    let commands = client.ok("commands/list", json!({}));
    let model = entry(
        &commands["builtin"],
        "usage",
        "/model [provider/model#variant]",
    );
    assert_eq!(model["name"], "model");
    assert_eq!(model["description"], "show or switch the active model");
    // One entry per form of a command; a description is one line.
    entry(&commands["builtin"], "usage", "/mcp remove <name>");
    let new = entry(&commands["builtin"], "name", "new");
    assert!(
        !new["description"].as_str().unwrap().contains('\n'),
        "{new}"
    );
    let custom = commands["custom"].as_array().unwrap();
    assert_eq!(custom.len(), 1, "{commands}");
    assert_eq!(custom[0]["name"], "fix");
    assert_eq!(custom[0]["description"], "fix an issue");
    assert_eq!(custom[0]["argument_hint"], "<issue>");
    assert_eq!(custom[0]["model"], "big");
    assert!(Path::new(custom[0]["path"].as_str().unwrap()).ends_with(".atelier/commands/fix.md"));
    assert_eq!(commands["prompts"], json!([]));
    let skills = commands["skills"].as_array().unwrap();
    assert_eq!(skills.len(), 1, "{commands}");
    assert_eq!(skills[0]["name"], "deploy");
    assert_eq!(skills[0]["description"], "ship a release");
    assert_eq!(skills[0]["global"], false);
    assert!(Path::new(skills[0]["path"].as_str().unwrap()).ends_with(".atelier/skills/deploy"));
    assert_eq!(commands["skipped_skills"].as_array().unwrap().len(), 1);

    // The tests run with the coordinator switched off.
    let peers = client.ok("peers/list", json!({}));
    assert_eq!(
        peers,
        json!({"enabled": false, "name": null, "link": null, "error": null,
               "instances": [], "locks": []})
    );
    assert_eq!(mock.requests(), 0);
}

/// `jobs/list` shows a `bash` job the model left running in the
/// background, and the job is stopped when the session is shut down.
#[cfg(unix)]
#[test]
fn jobs_list_shows_a_background_job() {
    let mock = Mock::scripted(|n, _| match n {
        0 => tool_call(
            "call_1",
            "bash",
            json!({"command": "sleep 30", "background": true}),
        ),
        _ => answer("started"),
    });
    let dir = unique_dir("jobs");
    let mut client = Client::ready(&mock, &dir, &[]);

    let turn = client.start_turn("start it");
    let asked = client.asked("approval/request");
    client.reply(&asked, json!({"decision": "once"}));
    assert_eq!(client.turn_ended(&turn)["status"], "completed");

    let listed = client.ok("jobs/list", json!({}));
    let jobs = listed["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 1, "{listed}");
    let job = &jobs[0];
    assert_eq!(job["id"], "job-1");
    assert_eq!(job["command"], "sleep 30");
    assert_eq!(job["state"], "running");
    assert_eq!(job["status"], "running");
    assert_eq!(job["exit_code"], Value::Null);
    assert!(job["runtime_ms"].is_u64(), "{job}");
    assert_eq!(listed["agents"], json!([]));

    client.ok("shutdown", json!({}));
    assert!(client.exited().success());
}
