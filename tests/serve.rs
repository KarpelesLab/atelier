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
        let mut child = Command::new(env!("CARGO_BIN_EXE_atelier"))
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
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn atelier serve");

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
        let mut client = Client::spawn(mock, cwd, args);
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
