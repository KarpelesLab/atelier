//! Integration tests for `atelier acp`: the Agent Client Protocol (version
//! 1, see `docs/acp.md`), spoken to the real binary.
//!
//! Each test plays the editor: it spawns `atelier acp` against a scripted
//! mock of an OpenAI-compatible server (the helpers are those of
//! `tests/serve.rs`, in their smallest form), writes requests on the
//! child's stdin and reads messages off its stdout.
//!
//! Nothing here sleeps to let something happen. The client waits *for a
//! message*, and the protocol supplies the points to wait at: a permission
//! request holds a turn still until it is answered, and the mock can hold a
//! response open until the test lets it go. Every wait has a deadline, so a
//! missing message fails the test (with the child's stderr) instead of
//! hanging it.
//!
//! # The schema
//!
//! Every message atelier writes is held to the protocol's published JSON
//! schema, `tests/data/acp-schema-v1.json` — `schema/v1/schema.json` of
//! <https://github.com/agentclientprotocol/agent-client-protocol> (schema
//! release 1.31.0, commit eeb4db6 of 2026-11-13; Apache-2.0), unmodified.
//! The check is done twice, by atelier's own validator (`src/schema.rs`,
//! compiled into this test): the whole message against the schema's "what
//! an Agent sends" side, and — because that side accepts any `params` an
//! extension method might carry — the payload against the one definition
//! it is supposed to be (`session/update` → `SessionNotification`, the
//! answer to `session/prompt` → `PromptResponse`, …). A message of a kind
//! the table below does not know fails the test.

#[path = "../src/schema.rs"]
mod schema;

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};

/// How long any single wait may take before the test fails.
const DEADLINE: Duration = Duration::from_secs(30);

// Error codes: JSON-RPC's, ACP's "resource not found", and atelier's own.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const NOT_FOUND: i64 = -32002;
const SESSION_LIMIT: i64 = 1001;

// -------------------------------------------------------------- the schema

/// The published schema, parsed once.
fn acp_schema() -> &'static Value {
    static SCHEMA: OnceLock<Value> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/acp-schema-v1.json");
        let text = std::fs::read_to_string(&path).expect("read the ACP schema");
        serde_json::from_str(&text).expect("the ACP schema is JSON")
    })
}

/// Hold `value` to the schema's definition `name`.
fn conforms(name: &str, value: &Value) -> Result<(), String> {
    let root = acp_schema();
    assert!(root["$defs"].get(name).is_some(), "no definition {name}");
    // The definitions, with the one wanted as the document's own schema.
    let schema = json!({ "$defs": root["$defs"], "$ref": format!("#/$defs/{name}") });
    schema::validate(&schema, value).map_err(|why| format!("not a {name}: {why}"))
}

/// The definition the payload of a message atelier sent must satisfy.
/// `asked` is the method of the client's request a response answers.
fn check_message(message: &Value, asked: Option<&str>) -> Result<(), String> {
    // The "Agent" side of the schema: what an agent may write at all.
    let root = acp_schema();
    let agent = json!({ "$defs": root["$defs"], "allOf": [root["anyOf"][0]] });
    schema::validate(&agent, message).map_err(|why| format!("not an Agent message: {why}"))?;

    let method = message["method"].as_str();
    match (method, message.get("id"), asked) {
        (Some("session/update"), None, _) => conforms("SessionNotification", &message["params"]),
        (Some(method), Some(_), _) => {
            let name = match method {
                "session/request_permission" => "RequestPermissionRequest",
                "fs/read_text_file" => "ReadTextFileRequest",
                "fs/write_text_file" => "WriteTextFileRequest",
                "elicitation/create" => "CreateElicitationRequest",
                other => return Err(format!("a request the tests do not know: {other}")),
            };
            conforms(name, &message["params"])
        }
        (Some(other), None, _) => Err(format!("a notification the tests do not know: {other}")),
        (None, _, _) if message.get("error").is_some() => conforms("Error", &message["error"]),
        (None, _, Some(asked)) => {
            let name = match asked {
                "initialize" => "InitializeResponse",
                "session/new" => "NewSessionResponse",
                "session/prompt" => "PromptResponse",
                "session/set_mode" => "SetSessionModeResponse",
                other => return Err(format!("a result for {other}, which has none here")),
            };
            conforms(name, &message["result"])
        }
        (None, _, None) => Err("a result for a request that was not made".to_string()),
    }
}

/// The checker itself: it accepts the specification's own examples and
/// refuses messages that are wrong in the ways an agent's could be.
#[test]
fn the_schema_check_tells_right_from_wrong() {
    let update = |update: Value| {
        json!({"jsonrpc": "2.0", "method": "session/update",
               "params": {"sessionId": "s", "update": update}})
    };
    let ok = [
        update(json!({"sessionUpdate": "agent_message_chunk",
                      "content": {"type": "text", "text": "hi"}})),
        update(
            json!({"sessionUpdate": "tool_call", "toolCallId": "call_001",
                      "title": "Reading configuration file", "kind": "read", "status": "pending"}),
        ),
        update(json!({"sessionUpdate": "plan", "entries": [
            {"content": "Check", "priority": "high", "status": "pending"}]})),
    ];
    for message in &ok {
        check_message(message, None).unwrap();
    }
    let wrong = [
        // An update of no known kind; a chunk without content; a status
        // and a kind that do not exist; a plan entry without priority.
        update(json!({"sessionUpdate": "agent_message", "content": {"type": "text", "text": ""}})),
        update(json!({"sessionUpdate": "agent_message_chunk", "text": "hi"})),
        update(
            json!({"sessionUpdate": "tool_call", "toolCallId": "c", "title": "t",
                      "status": "done"}),
        ),
        update(
            json!({"sessionUpdate": "tool_call", "toolCallId": "c", "title": "t",
                      "kind": "write"}),
        ),
        update(
            json!({"sessionUpdate": "plan", "entries": [{"content": "x", "status": "pending"}]}),
        ),
        json!({"jsonrpc": "2.0", "method": "session/update", "params": {"update": {}}}),
    ];
    for message in &wrong {
        assert!(check_message(message, None).is_err(), "{message}");
    }
    let result = |result: Value| json!({"jsonrpc": "2.0", "id": 1, "result": result});
    check_message(
        &result(json!({"stopReason": "end_turn"})),
        Some("session/prompt"),
    )
    .unwrap();
    assert!(
        check_message(
            &result(json!({"stopReason": "done"})),
            Some("session/prompt")
        )
        .is_err()
    );
    assert!(check_message(&result(json!({})), Some("session/new")).is_err());
    let permission = json!({"jsonrpc": "2.0", "id": 5, "method": "session/request_permission",
        "params": {"sessionId": "s", "toolCall": {"toolCallId": "c"}, "options": [
            {"optionId": "allow-once", "name": "Allow once", "kind": "allow_sometimes"}]}});
    assert!(check_message(&permission, None).is_err());
}

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

/// A unique temp directory to use as a project.
fn unique_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("atelier-acp-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    // As the session names it (a temp directory may sit behind a symlink).
    dir.canonicalize().expect("canonical temp dir")
}

/// The client end of an `atelier acp` process.
struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    /// Messages off the child's stdout, parsed; `None` once it has ended.
    incoming: Receiver<Option<Value>>,
    /// Messages read while waiting for another one, not yet asked for.
    backlog: VecDeque<Value>,
    /// Everything received, in order.
    log: Vec<Value>,
    /// The method of each request sent, by id: what its answer must be.
    sent: HashMap<String, String>,
    /// The child's stderr so far, for failure messages.
    stderr: Arc<Mutex<String>>,
    next_id: u64,
    /// The open session's id, once there is one.
    session: String,
}

impl Client {
    /// Spawn `atelier acp <args>` against the mock, with nothing of the
    /// outer environment or the real home leaking in. It is started in a
    /// directory of its own (`<project>.launch`), as an editor starts an
    /// agent wherever it likes: the project is named by `session/new`.
    fn spawn(mock: &Mock, project: &Path, args: &[&str]) -> Client {
        let launch = project.with_extension("launch");
        std::fs::create_dir_all(&launch).expect("create the launch dir");
        let mut child = Command::new(env!("CARGO_BIN_EXE_atelier"))
            .arg("acp")
            .args(args)
            .current_dir(&launch)
            .env("ATELIER_BASE_URL", mock.base_url())
            .env("ATELIER_MODEL", "mock")
            .env("ATELIER_NO_COORDINATOR", "1")
            .env("XDG_STATE_HOME", project.with_extension("state"))
            .env("XDG_CONFIG_HOME", project.join(".no-user-config"))
            .env("XDG_DATA_HOME", project.with_extension("data"))
            .env("HOME", project.with_extension("home"))
            .env_remove("ATELIER_APPROVE")
            .env_remove("ATELIER_TRUST_HOOKS")
            .env_remove("ATELIER_TRUST_PROJECT")
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
            .expect("spawn atelier acp");

        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, incoming) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                // The stdout contract: protocol messages, one per line, and
                // nothing else.
                let message: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|e| panic!("stdout line is not JSON ({e}): {line:?}"));
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
            sent: HashMap::new(),
            stderr,
            next_id: 0,
            session: String::new(),
        }
    }

    /// A process past `initialize` (as a client with `capabilities`) with
    /// its session open in `project`.
    fn open(mock: &Mock, project: &Path, args: &[&str], capabilities: Value) -> Client {
        let mut client = Client::spawn(mock, project, args);
        client.ok(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": capabilities,
                "clientInfo": {"name": "test", "version": "0"},
            }),
        );
        client.new_session(project, json!([]));
        client
    }

    /// `session/new` in `project`; returns its result.
    fn new_session(&mut self, project: &Path, servers: Value) -> Value {
        let created = self.ok(
            "session/new",
            json!({"cwd": project, "mcpServers": servers}),
        );
        self.session = created["sessionId"]
            .as_str()
            .expect("a session id")
            .to_string();
        created
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
        let id = format!("c{}", self.next_id);
        self.sent.insert(id.clone(), method.to_string());
        self.write_line(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string(),
        );
        Value::from(id)
    }

    /// Send a notification.
    fn notify(&mut self, method: &str, params: Value) {
        self.write_line(&json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string());
    }

    /// Send a notification and wait until atelier has taken it in. A
    /// notification has no answer to wait for, but atelier reads its input
    /// in order: once it has answered a request sent behind the
    /// notification (here one for a method it does not have, which its
    /// reader answers on the spot), it has read the notification too.
    fn notify_and_wait(&mut self, method: &str, params: Value) {
        self.notify(method, params);
        self.err("_test/sync", json!({}), METHOD_NOT_FOUND);
    }

    /// Answer a request atelier made.
    fn reply(&mut self, request: &Value, result: Value) {
        let id = &request["id"];
        self.write_line(&json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string());
    }

    /// Take a message off the wire into the log, holding it to the schema.
    fn log(&mut self, message: Value) {
        let asked = message["id"].as_str().and_then(|id| self.sent.get(id));
        let checked = check_message(&message, asked.map(String::as_str));
        self.log.push(message.clone());
        if let Err(why) = checked {
            self.fail(&format!(
                "atelier sent a message the ACP schema refuses: {why}\n{message}"
            ));
        }
    }

    /// The next message off the wire, or failure at the deadline.
    fn recv(&mut self, waiting_for: &str) -> Value {
        match self.incoming.recv_timeout(DEADLINE) {
            Ok(Some(message)) => {
                self.log(message.clone());
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

    /// The next `session/update` of the kind `kind` (its `update` object).
    fn update(&mut self, kind: &str) -> Value {
        let message = self.take(kind, |m| {
            m["method"] == "session/update" && m["params"]["update"]["sessionUpdate"] == kind
        });
        assert_eq!(message["params"]["sessionId"], self.session, "{message}");
        message["params"]["update"].clone()
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

    /// Send a prompt made of `blocks`; returns the request's id.
    fn prompt_blocks(&mut self, blocks: Value) -> Value {
        let session = self.session.clone();
        self.request(
            "session/prompt",
            json!({"sessionId": session, "prompt": blocks}),
        )
    }

    /// Send a text prompt; returns the request's id.
    fn prompt(&mut self, text: &str) -> Value {
        self.prompt_blocks(json!([{"type": "text", "text": text}]))
    }

    /// The stop reason a prompt ended with.
    fn stopped(&mut self, prompt: &Value) -> String {
        let response = self.response(prompt);
        match response["result"]["stopReason"].as_str() {
            Some(reason) => reason.to_string(),
            None => self.fail(&format!("the prompt failed: {response}")),
        }
    }

    /// The kinds of every `session/update` received so far, in order, with
    /// a tool call's status: `tool_call:pending`, `tool_call_update:completed`.
    fn updates(&self) -> Vec<String> {
        let updates = self.log.iter().filter(|m| m["method"] == "session/update");
        updates
            .map(|m| {
                let update = &m["params"]["update"];
                let kind = update["sessionUpdate"].as_str().unwrap_or("?");
                match update["status"].as_str() {
                    Some(status) => format!("{kind}:{status}"),
                    None => kind.to_string(),
                }
            })
            .collect()
    }

    /// Every piece of the agent's answer received so far, joined.
    fn text(&self) -> String {
        let chunk = |m: &&Value| m["params"]["update"]["sessionUpdate"] == "agent_message_chunk";
        self.log
            .iter()
            .filter(chunk)
            .filter_map(|m| m["params"]["update"]["content"]["text"].as_str())
            .collect()
    }

    /// Wait for the process to end on its own (once its stdin is closed)
    /// and return how it did.
    fn exited(mut self) -> ExitStatus {
        loop {
            match self.incoming.recv_timeout(DEADLINE) {
                Ok(Some(message)) => self.log(message),
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

/// The text of a tool call's content entries, joined.
fn content_text(update: &Value) -> String {
    let entries = update["content"].as_array().cloned().unwrap_or_default();
    let text = |entry: &Value| entry["content"]["text"].as_str().map(str::to_string);
    entries
        .iter()
        .filter_map(text)
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------- the tests

/// `initialize` comes first and negotiates the version: 1, whatever the
/// client asked for. The capabilities say what is really there, and no
/// authentication is asked for.
#[test]
fn initialize_negotiates_version_one_and_says_what_is_supported() {
    let mock = Mock::scripted(|_, _| answer("unused"));
    let dir = unique_dir("init");
    let mut client = Client::spawn(&mock, &dir, &[]);

    // Nothing before the handshake, which must be a handshake.
    let refused = client.err(
        "session/new",
        json!({"cwd": dir, "mcpServers": []}),
        INVALID_REQUEST,
    );
    assert!(refused["message"].as_str().unwrap().contains("initialize"));
    client.err(
        "initialize",
        json!({"protocolVersion": "1.0.0"}),
        INVALID_PARAMS,
    );
    client.err("initialize", json!({}), INVALID_PARAMS);

    let hello = client.ok("initialize", json!({"protocolVersion": 1}));
    assert_eq!(hello["protocolVersion"], 1);
    assert_eq!(
        hello["agentCapabilities"],
        json!({
            "loadSession": false,
            "promptCapabilities": {"image": true, "audio": false, "embeddedContext": true},
            "mcpCapabilities": {"http": true, "sse": false},
        })
    );
    assert_eq!(hello["authMethods"], json!([]));
    assert_eq!(hello["agentInfo"]["name"], "atelier");
    assert_eq!(hello["agentInfo"]["version"], env!("CARGO_PKG_VERSION"));

    client.err("initialize", json!({"protocolVersion": 1}), INVALID_REQUEST);
    // What is not implemented is not there: loading, authenticating, the
    // unstable methods.
    client.err("session/load", json!({}), METHOD_NOT_FOUND);
    client.err("authenticate", json!({"methodId": "x"}), METHOD_NOT_FOUND);
    client.err("session/list", json!({}), METHOD_NOT_FOUND);
    assert_eq!(mock.requests(), 0);

    // A client from the future is answered with the version this agent has.
    let mut newer = Client::spawn(&mock, &dir, &[]);
    assert_eq!(
        newer.ok("initialize", json!({"protocolVersion": 7}))["protocolVersion"],
        1
    );
}

/// `session/new` opens the session in the directory the client names — not
/// the one the process was started in — and answers with an id and the
/// permission modes; the slash commands follow. There is one session per
/// process, and other ids are unknown.
#[test]
fn a_session_opens_in_the_directory_the_client_names() {
    let mock = Mock::scripted(|n, _| match n {
        0 => tool_call(
            "call_1",
            "write",
            json!({"path": "made.txt", "content": "here"}),
        ),
        _ => answer("done"),
    });
    let dir = unique_dir("new");
    std::fs::create_dir_all(dir.join(".atelier/commands")).unwrap();
    std::fs::write(
        dir.join(".atelier/commands/fix.md"),
        "---\ndescription: Fix an issue\nargument-hint: <issue>\n---\nFix issue $ARGUMENTS.\n",
    )
    .unwrap();
    let mut client = Client::spawn(&mock, &dir, &[]);
    client.ok("initialize", json!({"protocolVersion": 1}));

    client.err(
        "session/new",
        json!({"cwd": "relative/path", "mcpServers": []}),
        INVALID_PARAMS,
    );
    client.err("session/new", json!({"mcpServers": []}), INVALID_PARAMS);
    let sse =
        json!([{"type": "sse", "name": "old", "url": "http://127.0.0.1:1/sse", "headers": []}]);
    let refused = client.err(
        "session/new",
        json!({"cwd": dir, "mcpServers": sse}),
        INVALID_PARAMS,
    );
    assert!(
        refused["message"].as_str().unwrap().contains("sse"),
        "{refused}"
    );
    // A directory that is not there fails, and leaves the process free to
    // be asked again.
    client.err(
        "session/new",
        json!({"cwd": dir.join("missing"), "mcpServers": []}),
        INVALID_PARAMS,
    );

    let created = client.new_session(&dir, json!([]));
    assert!(client.session.starts_with("sess_"), "{created}");
    let modes = &created["modes"];
    assert_eq!(modes["currentModeId"], "auto-edit");
    let ids: Vec<&str> = modes["availableModes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert_eq!(ids, ["plan", "ask-edits", "auto-edit", "yolo"]);

    let commands = client.update("available_commands_update");
    let commands = commands["availableCommands"].as_array().unwrap();
    let named = |name: &str| commands.iter().find(|c| c["name"] == name);
    assert_eq!(
        named("compact").unwrap()["input"]["hint"],
        "instructions for the summary (optional)"
    );
    assert!(named("context").unwrap().get("input").is_none());
    // The project's own command file is one of them.
    assert_eq!(
        named("fix").unwrap(),
        &json!({"name": "fix", "description": "Fix an issue", "input": {"hint": "<issue>"}})
    );
    // The terminal's commands are not offered.
    assert!(named("quit").is_none() && named("config").is_none());

    let second = client.err(
        "session/new",
        json!({"cwd": dir, "mcpServers": []}),
        SESSION_LIMIT,
    );
    assert!(
        second["message"]
            .as_str()
            .unwrap()
            .contains("one session per process")
    );
    let unknown = client.err(
        "session/prompt",
        json!({"sessionId": "sess_other", "prompt": [{"type": "text", "text": "hi"}]}),
        NOT_FOUND,
    );
    assert_eq!(unknown["data"]["sessionId"], "sess_other");
    client.err(
        "session/set_mode",
        json!({"sessionId": "nope", "modeId": "plan"}),
        NOT_FOUND,
    );

    // Relative paths are the session's directory's.
    let prompt = client.prompt("make it");
    assert_eq!(client.stopped(&prompt), "end_turn");
    assert_eq!(
        std::fs::read_to_string(dir.join("made.txt")).unwrap(),
        "here"
    );
    assert!(!dir.with_extension("launch").join("made.txt").exists());
}

/// A prompt whose turn calls tools: each call is a `tool_call` with a kind,
/// a title and the file it touches, then a `tool_call_update` with what it
/// produced — an edit as a diff; the model's text is message chunks, and
/// the stop reason is the answer. The content blocks of the prompt reach
/// the model as text.
#[test]
fn a_turn_reports_its_tool_calls_and_ends_with_a_stop_reason() {
    let mock = Mock::scripted(|n, _| match n {
        0 => tool_call("call_1", "read", json!({"path": "notes.txt", "offset": 2})),
        1 => tool_call(
            "call_2",
            "edit",
            json!({"path": "notes.txt", "old_string": "beta", "new_string": "BETA"}),
        ),
        2 => sse(&[text_frame("all "), text_frame("done")]),
        _ => answer("unused"),
    });
    let dir = unique_dir("turn");
    std::fs::write(dir.join("notes.txt"), "alpha\nbeta\ngamma\n").unwrap();
    let notes = dir.join("notes.txt").display().to_string();
    let mut client = Client::open(&mock, &dir, &[], json!({}));

    let prompt = client.prompt_blocks(json!([
        {"type": "text", "text": "Shout the second line of "},
        {"type": "resource_link", "uri": format!("file://{notes}"), "name": "notes.txt"},
        {"type": "text", "text": " like "},
        {"type": "resource", "resource": {"uri": "file:///elsewhere/style.md", "text": "LOUD"}},
    ]));
    assert_eq!(client.stopped(&prompt), "end_turn");

    let turn: Vec<String> = client
        .updates()
        .into_iter()
        .filter(|kind| kind != "available_commands_update")
        .collect();
    assert_eq!(
        turn,
        [
            "tool_call:in_progress",
            "tool_call_update:completed",
            "tool_call:in_progress",
            "tool_call_update:completed",
            "agent_message_chunk",
            "agent_message_chunk",
        ]
    );
    assert_eq!(client.text(), "all done");

    let read = client.update("tool_call");
    assert_eq!(read["toolCallId"], "call_1");
    assert_eq!(read["title"], "Read notes.txt");
    assert_eq!(read["name"], "read");
    assert_eq!(read["kind"], "read");
    assert_eq!(read["rawInput"], json!({"path": "notes.txt", "offset": 2}));
    assert_eq!(read["locations"], json!([{"path": notes, "line": 2}]));
    let read_done = client.update("tool_call_update");
    assert_eq!(read_done["toolCallId"], "call_1");
    assert_eq!(content_text(&read_done), "2\tbeta\n3\tgamma\n");

    let edit = client.update("tool_call");
    assert_eq!(edit["toolCallId"], "call_2");
    assert_eq!(edit["kind"], "edit");
    assert_eq!(edit["title"], "Edit notes.txt");
    assert_eq!(edit["locations"], json!([{"path": notes, "line": 2}]));
    let diff = json!({"type": "diff", "path": notes, "oldText": "beta", "newText": "BETA"});
    assert_eq!(edit["content"], json!([diff]));
    let edit_done = client.update("tool_call_update");
    assert_eq!(edit_done["toolCallId"], "call_2");
    // The diff stays with the change that was made, next to what the tool said.
    assert_eq!(edit_done["content"][0], diff);
    assert_eq!(
        content_text(&edit_done),
        "replaced 1 occurrence(s) in notes.txt"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("notes.txt")).unwrap(),
        "alpha\nBETA\ngamma\n"
    );

    // The link is a path the agent can read; the embedded text came along.
    let asked: Value = serde_json::from_str(&mock.body(0)).unwrap();
    let said = asked["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|m| m["role"] == "user")
        .map(|m| m["content"].to_string())
        .unwrap();
    let expected = format!(
        "Shout the second line of [notes.txt]({notes}) like [style.md](/elsewhere/style.md)"
    );
    assert!(said.contains(&expected), "{said}");
    assert!(
        said.contains(r#"<file path=\"/elsewhere/style.md\">\nLOUD\n</file>"#),
        "{said}"
    );
}

/// A call the rules do not let through on their own is put to the client as
/// `session/request_permission`, tied to a `pending` tool call, and the
/// option chosen is carried out: allow-once runs it, reject-once gives the
/// model the refusal. A rule that asks every time takes "always" off the
/// list.
#[test]
fn permission_is_asked_of_the_client_and_obeyed() {
    let mock = Mock::scripted(|n, _| match n {
        0 => tool_call("call_1", "bash", json!({"command": "echo one > a.txt"})),
        2 => tool_call(
            "call_2",
            "write",
            json!({"path": "b.txt", "content": "two"}),
        ),
        _ => answer("done"),
    });
    let dir = unique_dir("permission");
    std::fs::write(
        dir.join("atelier.toml"),
        "[permissions]\nask = [\"write\"]\n",
    )
    .unwrap();
    let mut client = Client::open(&mock, &dir, &[], json!({}));

    // allow-once: the shell command runs.
    let prompt = client.prompt("run it");
    let asked = client.asked("session/request_permission");
    let params = &asked["params"];
    assert_eq!(params["sessionId"], client.session);
    let call = &params["toolCall"];
    assert_eq!(call["status"], "pending");
    assert_eq!(call["kind"], "execute");
    assert_eq!(call["title"], "echo one > a.txt");
    assert_eq!(call["rawInput"]["command"], "echo one > a.txt");
    let kinds: Vec<&str> = params["options"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|o| o["kind"].as_str())
        .collect();
    assert_eq!(kinds, ["allow_once", "allow_always", "reject_once"]);
    // The call was announced before the question about it.
    let pending = client.update("tool_call");
    assert_eq!(pending["toolCallId"], call["toolCallId"]);
    assert_eq!(pending["status"], "pending");
    // The turn waits for the answer.
    assert!(!dir.join("a.txt").exists());
    client.reply(
        &asked,
        json!({"outcome": {"outcome": "selected", "optionId": "allow-once"}}),
    );
    assert_eq!(client.stopped(&prompt), "end_turn");
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap().trim(),
        "one"
    );
    assert!(!mock.body(1).contains("denied permission"));
    assert_eq!(
        client.updates()[1..],
        [
            "tool_call:pending",
            "tool_call_update:in_progress",
            "tool_call_update:completed",
            "agent_message_chunk",
        ]
    );
    let running = client.update("tool_call_update");
    assert_eq!(running["toolCallId"], call["toolCallId"]);

    // reject-once: the write does not happen, and the model is told why.
    let prompt = client.prompt("write it");
    let asked = client.asked("session/request_permission");
    let params = &asked["params"];
    assert_eq!(params["toolCall"]["kind"], "edit");
    let b = dir.join("b.txt").display().to_string();
    let diff = json!({"type": "diff", "path": b, "oldText": null, "newText": "two"});
    assert_eq!(params["toolCall"]["content"][0], diff);
    assert!(content_text(&params["toolCall"]).contains("the rule `write` asks"));
    let ids: Vec<&str> = params["options"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|o| o["optionId"].as_str())
        .collect();
    assert_eq!(ids, ["allow-once", "reject-once"]);
    client.reply(
        &asked,
        json!({"outcome": {"outcome": "selected", "optionId": "reject-once"}}),
    );
    assert_eq!(client.stopped(&prompt), "end_turn");
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
        .rfind(|m| m["params"]["update"]["sessionUpdate"] == "tool_call_update");
    let refused = &refused.unwrap()["params"]["update"];
    assert_eq!(refused["toolCallId"], params["toolCall"]["toolCallId"]);
    assert_eq!(refused["status"], "failed");
    // A change that was not made leaves no diff behind.
    assert_eq!(refused["content"].as_array().unwrap().len(), 1, "{refused}");
}

/// `session/cancel` stops a turn the model is in the middle of — the
/// prompt ends as `cancelled`, not as an error — and one waiting on a
/// permission request, which atelier gives up on without the client's
/// answer. The session goes on, and knows where it was cut.
#[test]
fn cancel_ends_the_running_turn_as_cancelled() {
    // The first response stops after one chunk until the test lets it go.
    let (release, gate) = mpsc::channel::<()>();
    let gate = Mutex::new(gate);
    let mock = Mock::start(Arc::new(move |n, _body, stream| match n {
        0 => {
            let _ = stream.write_all(format!("{SSE_HEAD}{}", text_frame("first ")).as_bytes());
            let _ = stream.flush();
            let _ = gate.lock().unwrap().recv_timeout(DEADLINE);
            let rest = [text_frame("second "), text_frame("third")].concat();
            let _ = stream.write_all(format!("{rest}data: [DONE]\n\n").as_bytes());
        }
        1 => {
            let _ = stream.write_all(&tool_call("call_1", "bash", json!({"command": "echo hi"})));
        }
        _ => {
            let _ = stream.write_all(&answer("after"));
        }
    }));
    let dir = unique_dir("cancel");
    let mut client = Client::open(&mock, &dir, &[], json!({}));

    let prompt = client.prompt("go on");
    assert_eq!(
        client.update("agent_message_chunk")["content"]["text"],
        "first "
    );
    // A prompt sent meanwhile waits its turn, and the cancel covers it too.
    let queued = client.prompt("and then");
    let session = client.session.clone();
    client.notify_and_wait("session/cancel", json!({"sessionId": session}));
    // The stop is seen at the stream's next chunk, as Ctrl-C is.
    release.send(()).unwrap();
    assert_eq!(client.stopped(&prompt), "cancelled");
    assert_eq!(client.stopped(&queued), "cancelled");
    assert!(!client.text().contains("third"), "{}", client.text());
    assert_eq!(mock.requests(), 1);

    // A turn held by a permission request: cancelling answers it.
    let prompt = client.prompt("run something");
    let asked = client.asked("session/request_permission");
    client.notify("session/cancel", json!({"sessionId": session}));
    assert_eq!(client.stopped(&prompt), "cancelled");
    let call = client
        .log
        .iter()
        .rfind(|m| m["params"]["update"]["sessionUpdate"] == "tool_call_update");
    let call = &call.unwrap()["params"]["update"];
    assert_eq!(
        call["toolCallId"],
        asked["params"]["toolCall"]["toolCallId"]
    );
    assert_eq!(call["status"], "failed");
    // The client's own late answer is not an error.
    client.reply(&asked, json!({"outcome": {"outcome": "cancelled"}}));

    // The conversation continues, and a cancel with nothing running is
    // nothing.
    client.notify("session/cancel", json!({"sessionId": session}));
    let prompt = client.prompt("continue");
    assert_eq!(client.stopped(&prompt), "end_turn");
    assert!(client.text().ends_with("after"));
    assert!(mock.body(1).contains("[stopped by the user]"));
}

/// `$/cancel_request` for a prompt is a cancel of that prompt.
#[test]
fn cancel_request_cancels_the_prompt_it_names() {
    let (release, gate) = mpsc::channel::<()>();
    let gate = Mutex::new(gate);
    let mock = Mock::start(Arc::new(move |n, _body, stream| {
        if n > 0 {
            let _ = stream.write_all(&answer("second answer"));
            return;
        }
        let _ = stream.write_all(format!("{SSE_HEAD}{}", text_frame("first ")).as_bytes());
        let _ = stream.flush();
        let _ = gate.lock().unwrap().recv_timeout(DEADLINE);
        let _ = stream.write_all(format!("{}data: [DONE]\n\n", text_frame("rest")).as_bytes());
    }));
    let dir = unique_dir("cancel-request");
    let mut client = Client::open(&mock, &dir, &[], json!({}));

    let first = client.prompt("go on");
    client.update("agent_message_chunk");
    let second = client.prompt("and then");
    client.notify_and_wait("$/cancel_request", json!({"requestId": first}));
    release.send(()).unwrap();
    assert_eq!(client.stopped(&first), "cancelled");
    // The one behind it was not named, and runs.
    assert_eq!(client.stopped(&second), "end_turn");
    assert!(client.text().ends_with("second answer"));
}

/// The `todo` tool's list is the agent's plan: after a call that changes
/// it, the whole list is sent as a `plan`.
#[test]
fn the_todo_list_is_reported_as_the_plan() {
    let mock = Mock::scripted(|n, _| match n {
        0 => tool_call(
            "call_1",
            "todo",
            json!({"action": "add", "items": ["write it", "test it"]}),
        ),
        1 => tool_call("call_2", "todo", json!({"action": "complete", "ids": [1]})),
        _ => answer("planned"),
    });
    let dir = unique_dir("plan");
    let mut client = Client::open(&mock, &dir, &[], json!({}));

    let prompt = client.prompt("plan the work");
    assert_eq!(client.stopped(&prompt), "end_turn");
    let call = client.update("tool_call");
    assert_eq!(call["kind"], "think");
    assert_eq!(
        client.update("plan")["entries"],
        json!([
            {"content": "write it", "priority": "medium", "status": "pending"},
            {"content": "test it", "priority": "medium", "status": "pending"},
        ])
    );
    assert_eq!(
        client.update("plan")["entries"],
        json!([
            {"content": "write it", "priority": "medium", "status": "completed"},
            {"content": "test it", "priority": "medium", "status": "pending"},
        ])
    );
}

/// The permission modes are ACP session modes: `session/set_mode` switches,
/// and in `plan` a write is refused — reported as a tool call that failed,
/// with the reason.
#[test]
fn set_mode_switches_to_plan_and_a_write_is_refused() {
    let mock = Mock::scripted(|n, _| match n {
        0 => tool_call(
            "call_1",
            "write",
            json!({"path": "out.txt", "content": "nope"}),
        ),
        _ => answer("here is the plan"),
    });
    let dir = unique_dir("mode");
    let mut client = Client::open(&mock, &dir, &[], json!({}));
    let session = client.session.clone();

    client.err(
        "session/set_mode",
        json!({"sessionId": session, "modeId": "careless"}),
        INVALID_PARAMS,
    );
    client.err(
        "session/set_mode",
        json!({"sessionId": session}),
        INVALID_PARAMS,
    );
    assert_eq!(
        client.ok(
            "session/set_mode",
            json!({"sessionId": session, "modeId": "plan"})
        ),
        json!({})
    );

    let prompt = client.prompt("change things");
    assert_eq!(client.stopped(&prompt), "end_turn");
    assert!(!dir.join("out.txt").exists());
    let refused = client.update("tool_call");
    assert_eq!(refused["status"], "failed");
    assert_eq!(refused["name"], "write");
    assert_eq!(refused["kind"], "edit");
    assert!(content_text(&refused).contains("plan mode"), "{refused}");
    assert!(mock.body(0).contains("PLAN MODE"));
    assert!(mock.body(1).contains("plan mode: write is not available"));
    assert_eq!(client.text(), "here is the plan");
}

/// A prompt that starts with a slash is a command: a built-in one runs
/// without the model, its output is the answer, and a mode it changed is
/// reported; a command file expands into the prompt of a turn.
#[test]
fn slash_commands_run_and_their_output_is_the_answer() {
    let mock = Mock::scripted(|_, _| answer("fixed"));
    let dir = unique_dir("slash");
    std::fs::create_dir_all(dir.join(".atelier/commands")).unwrap();
    std::fs::write(
        dir.join(".atelier/commands/fix.md"),
        "---\ndescription: Fix an issue\n---\nFix issue $ARGUMENTS now.\n",
    )
    .unwrap();
    let mut client = Client::open(&mock, &dir, &[], json!({}));

    let prompt = client.prompt("/mode ask-edits");
    assert_eq!(client.stopped(&prompt), "end_turn");
    let said = client.update("agent_message_chunk");
    let said = said["content"]["text"].as_str().unwrap();
    assert!(said.starts_with("mode: ask-edits — "), "{said}");
    assert_eq!(
        client.update("current_mode_update")["currentModeId"],
        "ask-edits"
    );

    // Output of several lines is one block, so its columns survive.
    let prompt = client.prompt("/mode");
    assert_eq!(client.stopped(&prompt), "end_turn");
    let listing = client.update("agent_message_chunk");
    let listing = listing["content"]["text"].as_str().unwrap();
    assert!(
        listing.starts_with("```\n") && listing.ends_with("\n```\n\n"),
        "{listing}"
    );
    assert!(listing.contains("\n* ask-edits "), "{listing}");
    assert_eq!(listing.trim_end().lines().count(), 6, "{listing}");

    let prompt = client.prompt("/nonsense");
    assert_eq!(client.stopped(&prompt), "end_turn");
    assert!(client.text().contains("unknown command '/nonsense'"));
    assert_eq!(mock.requests(), 0);

    // A command file is a prompt: it says what it expanded from, then the
    // turn runs.
    let prompt = client.prompt("/fix 42");
    assert_eq!(client.stopped(&prompt), "end_turn");
    assert!(
        client.text().contains("/fix → expanded from"),
        "{}",
        client.text()
    );
    assert!(client.text().ends_with("fixed"));
    assert!(
        mock.body(0).contains("Fix issue 42 now."),
        "{}",
        mock.body(0)
    );
}

/// A client that serves files is where `read` and `edit` get and put them:
/// the agent sees the editor's buffer, not the disk, and its edit goes to
/// the editor. A client that does not is never asked (every other test).
#[test]
fn files_are_read_and_written_through_a_client_that_offers_to() {
    let mock = Mock::scripted(|n, _| match n {
        0 => tool_call("call_1", "read", json!({"path": "a.txt"})),
        1 => tool_call(
            "call_2",
            "edit",
            json!({"path": "a.txt", "old_string": "unsaved", "new_string": "edited"}),
        ),
        2 => tool_call(
            "call_3",
            "write",
            json!({"path": "locked.txt", "content": "x"}),
        ),
        _ => answer("done"),
    });
    let dir = unique_dir("fs");
    std::fs::write(dir.join("a.txt"), "stale disk text\n").unwrap();
    let a = dir.join("a.txt").display().to_string();
    let capabilities = json!({"fs": {"readTextFile": true, "writeTextFile": true}});
    let mut client = Client::open(&mock, &dir, &[], capabilities);

    let prompt = client.prompt("edit the buffer");
    // `read`: the buffer's text is what the model gets.
    let read = client.asked("fs/read_text_file");
    assert_eq!(
        read["params"],
        json!({"sessionId": client.session, "path": a})
    );
    client.reply(&read, json!({"content": "unsaved buffer\n"}));
    // `edit`: it checks the buffer is still what was read, then writes it.
    let check = client.asked("fs/read_text_file");
    assert_eq!(check["params"]["path"], a);
    client.reply(&check, json!({"content": "unsaved buffer\n"}));
    let write = client.asked("fs/write_text_file");
    assert_eq!(
        write["params"],
        json!({"sessionId": client.session, "path": a, "content": "edited buffer\n"})
    );
    client.reply(&write, json!({}));
    // A write the editor refuses fails, and is not made on the disk instead.
    let refused = client.asked("fs/write_text_file");
    let id = &refused["id"];
    let error = json!({"code": -32603, "message": "that file is locked"});
    client.write_line(&json!({"jsonrpc": "2.0", "id": id, "error": error}).to_string());
    assert_eq!(client.stopped(&prompt), "end_turn");

    assert!(
        mock.body(1).contains(r"1\tunsaved buffer"),
        "{}",
        mock.body(1)
    );
    assert!(
        !mock.body(1).contains("stale disk text"),
        "{}",
        mock.body(1)
    );
    assert!(
        mock.body(2).contains("replaced 1 occurrence(s) in a.txt"),
        "{}",
        mock.body(2)
    );
    assert!(
        mock.body(3)
            .contains("the editor refused the write: that file is locked")
    );
    // The disk is the editor's to write.
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "stale disk text\n"
    );
    assert!(!dir.join("locked.txt").exists());
}

/// The `ask` tool's question is a form elicitation for a client that takes
/// forms, and its submission is the tool's result. For one that does not,
/// there is no one to ask and the model is told so.
#[test]
fn a_question_is_a_form_for_a_client_that_takes_forms() {
    let question = json!({
        "question": "Which one?",
        "options": [{"label": "Alpha", "description": "the first"}, {"label": "Beta"}],
    });
    let asking = question.clone();
    let mock = Mock::scripted(move |n, _| match n {
        0 => tool_call("call_1", "ask", asking.clone()),
        _ => answer("ok"),
    });
    let dir = unique_dir("question");
    let mut client = Client::open(&mock, &dir, &[], json!({"elicitation": {"form": {}}}));

    let prompt = client.prompt("choose");
    let asked = client.asked("elicitation/create");
    let params = &asked["params"];
    assert_eq!(params["mode"], "form");
    assert_eq!(params["message"], "Which one?");
    assert_eq!(params["sessionId"], client.session);
    assert_eq!(params["toolCallId"], "call_1");
    assert_eq!(
        params["requestedSchema"]["properties"]["choice"]["oneOf"],
        json!([{"const": "Alpha", "title": "Alpha — the first"}, {"const": "Beta", "title": "Beta"}])
    );
    client.reply(
        &asked,
        json!({"action": "accept", "content": {"choice": "Beta"}}),
    );
    assert_eq!(client.stopped(&prompt), "end_turn");
    assert!(
        mock.body(1)
            .contains(r#"The user chose the option \"Beta\"."#),
        "{}",
        mock.body(1)
    );

    // No form capability: `{}` advertises no mode at all.
    let asking = question.clone();
    let mock = Mock::scripted(move |n, _| match n {
        0 => tool_call("call_1", "ask", asking.clone()),
        _ => answer("ok"),
    });
    let dir = unique_dir("no-question");
    let mut client = Client::open(&mock, &dir, &[], json!({"elicitation": {}}));
    let prompt = client.prompt("choose");
    assert_eq!(client.stopped(&prompt), "end_turn");
    assert!(
        mock.body(1)
            .contains("cannot ask: this is a non-interactive session")
    );
    assert!(
        client
            .log
            .iter()
            .all(|m| m["method"] != "elicitation/create")
    );
}

/// The MCP servers the client names in `session/new` are connected for the
/// session — with the environment it gave — whatever the project's trust,
/// and their prompts are slash commands. A client that shows notices is
/// told how connecting went.
#[cfg(unix)]
#[test]
fn the_clients_mcp_servers_are_connected() {
    // A stdio MCP server in a few lines of shell: the handshake, one tool,
    // one prompt, and one call, answered with a variable of its environment.
    const SERVER: &str = r#"
IFS= read -r _init
printf '%s\n' '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fake","version":"0"}}}'
IFS= read -r _initialized
IFS= read -r _tools
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"greet","description":"Greets","inputSchema":{"type":"object"}}]}}'
IFS= read -r _resources
printf '%s\n' '{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"Method not found"}}'
IFS= read -r _prompts
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"prompts":[{"name":"review","description":"Review the change"}]}}'
IFS= read -r _call
printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"greeting=$GREETING\"}]}}"
IFS= read -r _end
"#;
    let mock = Mock::scripted(|n, _| match n {
        0 => tool_call("call_1", "mcp__my_tools__greet", json!({})),
        _ => answer("greeted"),
    });
    let dir = unique_dir("mcp");
    let mut client = Client::spawn(&mock, &dir, &[]);
    client.ok(
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {"session": {"notices": {}}}}),
    );
    client.new_session(
        &dir,
        json!([{
            "name": "my tools",
            "command": "/bin/sh",
            "args": ["-c", SERVER],
            "env": [{"name": "GREETING", "value": "hello from the editor"}],
        }]),
    );
    let commands = client.update("available_commands_update");
    let commands = commands["availableCommands"].as_array().unwrap();
    let review = commands.iter().find(|c| c["name"] == "mcp:my_tools:review");
    assert_eq!(review.unwrap()["description"], "Review the change");
    let notice = client.update("notice");
    assert_eq!(notice["severity"], "info");
    let title = notice["title"].as_str().unwrap();
    assert!(
        title.starts_with("mcp: connected 'my_tools' (from the client, "),
        "{title}"
    );

    // An MCP tool is one atelier asks about.
    let prompt = client.prompt("greet");
    let asked = client.asked("session/request_permission");
    assert_eq!(asked["params"]["toolCall"]["title"], "greet (my_tools)");
    assert_eq!(asked["params"]["toolCall"]["kind"], "other");
    client.reply(
        &asked,
        json!({"outcome": {"outcome": "selected", "optionId": "allow-once"}}),
    );
    assert_eq!(client.stopped(&prompt), "end_turn");
    assert!(
        mock.body(1).contains("greeting=hello from the editor"),
        "{}",
        mock.body(1)
    );
}

/// `--max-turns` bounds a prompt, which then ends as `max_turn_requests`.
#[test]
fn a_prompt_out_of_requests_stops_as_max_turn_requests() {
    let mock = Mock::scripted(|_, _| tool_call("call_1", "ls", json!({})));
    let dir = unique_dir("max-turns");
    let mut client = Client::open(&mock, &dir, &["--max-turns", "2"], json!({}));
    let prompt = client.prompt("look around forever");
    assert_eq!(client.stopped(&prompt), "max_turn_requests");
    assert_eq!(mock.requests(), 2);
}

/// A line that is not JSON, or not a message, gets an error with a null id
/// and the session carries on; a prompt with content this agent did not
/// offer to take is refused as such; a failure of the model is the
/// prompt's error. The process leaves when its input ends.
#[test]
fn malformed_input_is_answered_and_survived() {
    let mock = Mock::start(Arc::new(|n, _body, stream| {
        let _ = match n {
            0 => stream.write_all(
                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 4\r\nConnection: close\r\n\r\nboom",
            ),
            _ => stream.write_all(&answer("fine")),
        };
    }));
    let dir = unique_dir("malformed");
    let mut client = Client::open(&mock, &dir, &[], json!({}));
    let session = client.session.clone();
    let null_id = |m: &Value| m.get("method").is_none() && m["id"].is_null();

    client.write_line("this is not json");
    let parse = client.take("a parse error", null_id);
    assert_eq!(parse["error"]["code"], PARSE_ERROR);
    client.write_line("[1, 2]");
    let batch = client.take("an invalid-request error", null_id);
    assert_eq!(batch["error"]["code"], INVALID_REQUEST);
    client.write_line(r#"{"jsonrpc": "2.0", "id": 5}"#);
    let neither = client.take("an invalid-request error", |m| m["id"] == 5);
    assert_eq!(neither["error"]["code"], INVALID_REQUEST);
    // Notifications nobody knows are not answered; nor are answers to
    // questions nobody asked.
    client.notify("session/unknown", json!({}));
    client.write_line(r#"{"jsonrpc": "2.0", "id": 999, "result": {}}"#);

    client.err(
        "session/prompt",
        json!({"sessionId": session}),
        INVALID_PARAMS,
    );
    client.err(
        "session/prompt",
        json!({"sessionId": session, "prompt": "hi"}),
        INVALID_PARAMS,
    );
    let audio = json!([{"type": "audio", "mimeType": "audio/wav", "data": "AAAA"}]);
    let refused = client.err(
        "session/prompt",
        json!({"sessionId": session, "prompt": audio}),
        INVALID_PARAMS,
    );
    assert!(
        refused["message"].as_str().unwrap().contains("audio"),
        "{refused}"
    );
    client.err("session/prompt", json!(["positional"]), INVALID_PARAMS);
    assert_eq!(mock.requests(), 0);

    // The model's server failing is the prompt's failure, not the session's.
    let failed = client.prompt("hello");
    let failed = client.response(&failed);
    assert_eq!(failed["error"]["code"], -32603, "{failed}");
    let prompt = client.prompt("hello again");
    assert_eq!(client.stopped(&prompt), "end_turn");
    assert_eq!(client.text(), "fine");

    client.stdin = None;
    assert!(client.exited().success());
}
