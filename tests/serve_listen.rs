//! Integration tests for `atelier serve --listen`: the embedding protocol
//! on a socket (see "Listening" in `docs/protocol.md`), spoken to the real
//! binary by clients that did not launch it.
//!
//! Each test starts a listener against a scripted mock of an
//! OpenAI-compatible server (the helpers of `tests/serve.rs`, in their
//! smallest form), reads the one line it prints on stdout to learn where it
//! listens, and connects: over a Unix socket, over TCP with JSON-RPC lines,
//! or over WebSocket with a client written here by hand — so the server's
//! handshake and frames are checked against a second reading of RFC 6455
//! rather than against themselves.
//!
//! As in `tests/serve.rs`, nothing sleeps to let something happen: a client
//! waits for a message, with a deadline.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

/// How long any single wait may take before the test fails.
const DEADLINE: Duration = Duration::from_secs(30);

// The listener's error codes (`docs/protocol.md`).
const UNAUTHORIZED: i64 = -32020;
const TOO_MANY_SESSIONS: i64 = -32021;

/// A token as a caller would choose one.
const TOKEN: &str = "test-token-0123456789abcdef";

// ---------------------------------------------------------------- the mock

/// A background mock OpenAI-compatible server: one request per connection,
/// each answered by `script` from its number and its body.
struct Mock {
    port: u16,
    bodies: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Mock {
    fn scripted(script: impl Fn(usize, &str) -> Vec<u8> + Send + Sync + 'static) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let port = listener.local_addr().expect("local addr").port();
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
                        let _ = handle_conn(stream, n, &script, &bodies_thread);
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

    fn bodies(&self) -> Vec<String> {
        self.bodies.lock().unwrap().clone()
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

fn handle_conn(
    stream: TcpStream,
    n: usize,
    script: &impl Fn(usize, &str) -> Vec<u8>,
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
    writer.write_all(&script(n, &body))?;
    writer.flush()?;
    let _ = writer.shutdown(Shutdown::Write);
    Ok(())
}

/// A streamed plain answer.
fn answer(text: &str) -> Vec<u8> {
    let chunk = json!({"choices": [{"index": 0, "delta": {"content": text}}]});
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\
         Connection: close\r\n\r\ndata: {chunk}\n\ndata: [DONE]\n\n"
    )
    .into_bytes()
}

// ------------------------------------------------------------ the listener

/// A unique temp directory to use as the served project.
fn unique_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "atelier-listen-{name}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// `atelier serve <args>` in `cwd` against the mock, with nothing of the
/// outer environment or the real home leaking in. `token` is what
/// `ATELIER_SERVE_TOKEN` holds, if anything.
fn atelier(mock: &Mock, cwd: &Path, args: &[&str], token: Option<&str>) -> Command {
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
        .env("XDG_RUNTIME_DIR", cwd.with_extension("run"))
        .env("HOME", cwd.with_extension("home"));
    for unset in [
        "ATELIER_APPROVE",
        "ATELIER_TRUST_HOOKS",
        "ATELIER_API_KEY",
        "ATELIER_TRACE",
        "ATELIER_DEBUG",
        "ATELIER_BACKEND",
        "ATELIER_CONTEXT_LIMIT",
        "ATELIER_REVIEW",
        "ATELIER_MODE",
        "ATELIER_PARALLEL_TOOLS",
        "ATELIER_COORDINATOR_DIR",
        "ATELIER_SERVE_TOKEN",
    ] {
        command.env_remove(unset);
    }
    if let Some(token) = token {
        command.env("ATELIER_SERVE_TOKEN", token);
    }
    command
}

/// A listener that is expected not to start: how it ended.
fn refused(mock: &Mock, cwd: &Path, args: &[&str]) -> Output {
    atelier(mock, cwd, args, Some(TOKEN))
        .stdin(Stdio::null())
        .output()
        .expect("run atelier serve")
}

/// A running `atelier serve --listen`.
struct Listener {
    child: Child,
    /// What it printed on stdout: `{"listening": this}`.
    listening: Value,
    stderr: Arc<Mutex<String>>,
}

impl Listener {
    /// Start listening and wait for the line that says where.
    fn start(mock: &Mock, cwd: &Path, args: &[&str], token: Option<&str>) -> Listener {
        let mut child = atelier(mock, cwd, args, token)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn atelier serve --listen");
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
        // The stdout contract: one JSON line, then nothing.
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut lines = BufReader::new(stdout).lines();
            let _ = tx.send(lines.next().and_then(Result::ok));
            if let Some(Ok(more)) = lines.next() {
                panic!("a second line on the listener's stdout: {more:?}");
            }
        });
        let line = match rx.recv_timeout(DEADLINE) {
            Ok(Some(line)) => line,
            _ => panic!(
                "the listener did not say where it listens\n--- stderr ---\n{}",
                stderr.lock().unwrap()
            ),
        };
        let announced: Value = serde_json::from_str(&line).expect("the stdout line is JSON");
        let listening = announced["listening"].clone();
        assert!(listening.is_object(), "{line}");
        Listener {
            child,
            listening,
            stderr,
        }
    }

    /// The TCP address it listens on.
    fn address(&self) -> String {
        self.listening["address"]
            .as_str()
            .expect("an address")
            .to_string()
    }

    fn tcp(&self) -> TcpStream {
        TcpStream::connect(self.address()).expect("connect to the listener")
    }

    /// A raw-line connection that has presented `token`.
    fn lines(&self, token: &str) -> Peer {
        let mut stream = self.tcp();
        writeln!(stream, "{}", json!({ "token": token })).expect("write the token");
        let mut peer = self.peer_on_lines(
            stream.try_clone().expect("clone"),
            stream.try_clone().expect("clone"),
        );
        peer.socket = Some(stream);
        peer
    }

    /// A client speaking JSON-RPC lines on the two halves of a stream.
    fn peer_on_lines(
        &self,
        reader: impl Read + Send + 'static,
        mut writer: impl Write + Send + 'static,
    ) -> Peer {
        let (tx, incoming) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(reader).lines() {
                let Ok(line) = line else { break };
                let message: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|e| panic!("line is not JSON ({e}): {line:?}"));
                assert_eq!(message["jsonrpc"], "2.0", "not JSON-RPC: {line}");
                if tx.send(Some(message)).is_err() {
                    return;
                }
            }
            let _ = tx.send(None);
        });
        Peer {
            raw: Box::new(move |bytes| {
                let _ = writer.write_all(bytes);
                let _ = writer.flush();
            }),
            websocket: false,
            socket: None,
            incoming,
            backlog: VecDeque::new(),
            log: Vec::new(),
            stderr: Arc::clone(&self.stderr),
            next_id: 0,
        }
    }

    /// A WebSocket connection, its handshake done with these extra request
    /// headers; or the HTTP response that refused it.
    fn websocket(&self, target: &str, headers: &[(&str, &str)]) -> Result<Peer, String> {
        let mut stream = self.tcp();
        let mut request = format!(
            "GET {target} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\n\
             Connection: keep-alive, Upgrade\r\nSec-WebSocket-Key: {WS_KEY}\r\n\
             Sec-WebSocket-Version: 13\r\n",
            self.address()
        );
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes()).expect("write request");
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let head = read_head(&mut reader);
        if !head.starts_with("HTTP/1.1 101 ") {
            let mut body = String::new();
            let _ = reader.read_to_string(&mut body);
            return Err(format!("{head}{body}"));
        }
        let lower = head.to_ascii_lowercase();
        assert!(lower.contains("\r\nupgrade: websocket\r\n"), "{head}");
        assert!(lower.contains("\r\nconnection: upgrade\r\n"), "{head}");
        assert!(
            head.contains(&format!("\r\nSec-WebSocket-Accept: {WS_ACCEPT}\r\n")),
            "{head}"
        );
        // The token's subprotocol is never the one chosen.
        assert!(!lower.contains("token."), "{head}");
        if headers.iter().any(|(_, v)| v.contains("atelier,")) {
            assert!(head.contains("\r\nSec-WebSocket-Protocol: atelier\r\n"));
        }

        let (tx, incoming) = mpsc::channel();
        thread::spawn(move || {
            // A message's fragments, put back together.
            let mut partial = Vec::new();
            while let Some((fin, opcode, payload)) = read_frame(&mut reader) {
                let message = match opcode {
                    0x0 | 0x1 => {
                        partial.extend(payload);
                        if !fin {
                            continue;
                        }
                        let text = String::from_utf8(std::mem::take(&mut partial)).expect("UTF-8");
                        let message: Value = serde_json::from_str(&text)
                            .unwrap_or_else(|e| panic!("frame is not JSON ({e}): {text:?}"));
                        assert_eq!(message["jsonrpc"], "2.0", "not JSON-RPC: {text}");
                        message
                    }
                    0x8 => {
                        let code = u16::from_be_bytes([payload[0], payload[1]]);
                        json!({ "ws": "close", "code": code })
                    }
                    0xA => json!({ "ws": "pong", "payload": String::from_utf8_lossy(&payload) }),
                    other => panic!("unexpected opcode {other:#x} from the server"),
                };
                if tx.send(Some(message)).is_err() {
                    return;
                }
            }
            let _ = tx.send(None);
        });
        Ok(Peer {
            raw: Box::new(move |bytes| {
                let _ = stream.write_all(bytes);
            }),
            websocket: true,
            socket: None,
            incoming,
            backlog: VecDeque::new(),
            log: Vec::new(),
            stderr: Arc::clone(&self.stderr),
            next_id: 0,
        })
    }

    /// A plain exchange: write `request`, read everything that comes back.
    fn exchange(&self, request: &str) -> String {
        let mut stream = self.tcp();
        stream
            .set_read_timeout(Some(DEADLINE))
            .expect("read timeout");
        stream.write_all(request.as_bytes()).expect("write");
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        response
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// -------------------------------------------------- a WebSocket, by hand

/// The key of RFC 6455's own example, and the answer it gives for it.
const WS_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
const WS_ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

/// A client's frame: masked, as the RFC requires of clients.
fn frame(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
    const KEY: [u8; 4] = [0x12, 0x34, 0x56, 0x78];
    let mut out = vec![if fin { 0x80 } else { 0 } | opcode];
    match payload.len() {
        n if n < 126 => out.push(0x80 | n as u8),
        n if n <= 0xFFFF => {
            out.push(0x80 | 126);
            out.extend((n as u16).to_be_bytes());
        }
        n => {
            out.push(0x80 | 127);
            out.extend((n as u64).to_be_bytes());
        }
    }
    out.extend(KEY);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ KEY[i % 4]));
    out
}

/// A server's frame (fin, opcode, payload): never masked, never with
/// reserved bits. `None` once the connection has ended.
fn read_frame(r: &mut impl Read) -> Option<(bool, u8, Vec<u8>)> {
    let mut head = [0u8; 2];
    r.read_exact(&mut head).ok()?;
    assert_eq!(head[0] & 0x70, 0, "reserved bits set");
    assert_eq!(head[1] & 0x80, 0, "a server's frame must not be masked");
    let len = match head[1] {
        126 => {
            let mut b = [0u8; 2];
            r.read_exact(&mut b).ok()?;
            u16::from_be_bytes(b) as usize
        }
        127 => {
            let mut b = [0u8; 8];
            r.read_exact(&mut b).ok()?;
            u64::from_be_bytes(b) as usize
        }
        n => n as usize,
    };
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).ok()?;
    Some((head[0] & 0x80 != 0, head[0] & 0x0F, payload))
}

/// An HTTP response's status line and headers, through the blank line.
fn read_head(reader: &mut impl BufRead) -> String {
    let mut head = String::new();
    loop {
        let before = head.len();
        let n = reader.read_line(&mut head).expect("read the response");
        if n == 0 || head[before..].trim_end().is_empty() {
            return head;
        }
    }
}

// -------------------------------------------------------------- the client

/// Writes bytes on a connection, as they are.
type RawWrite = Box<dyn FnMut(&[u8])>;

/// The client end of one connection, whatever carries it.
struct Peer {
    /// The way out, under the framing.
    raw: RawWrite,
    /// Whether a message goes in a WebSocket frame, or on a line.
    websocket: bool,
    /// The TCP socket under a line connection, to hang up on.
    socket: Option<TcpStream>,
    /// Messages received, parsed; `None` once the connection has ended.
    incoming: Receiver<Option<Value>>,
    backlog: VecDeque<Value>,
    log: Vec<Value>,
    /// The listener's stderr so far, for failure messages.
    stderr: Arc<Mutex<String>>,
    next_id: u64,
}

impl Peer {
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

    /// Leave without a word, as a client that crashed would.
    fn hang_up(self) {
        let socket = self.socket.as_ref().expect("a TCP line connection");
        socket.shutdown(Shutdown::Both).expect("shutdown");
    }

    /// Send one protocol message.
    fn send(&mut self, text: &str) {
        if self.websocket {
            (self.raw)(&frame(true, 0x1, text.as_bytes()));
        } else {
            (self.raw)(format!("{text}\n").as_bytes());
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = json!(format!("c{}", self.next_id));
        self.send(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string(),
        );
        id
    }

    /// The next message, or `None` if the connection ended instead.
    fn next(&mut self, waiting_for: &str) -> Option<Value> {
        match self.incoming.recv_timeout(DEADLINE) {
            Ok(Some(message)) => {
                self.log.push(message.clone());
                Some(message)
            }
            Ok(None) => None,
            Err(_) => self.fail(&format!("timed out waiting for {waiting_for}")),
        }
    }

    /// The first message matching `wanted` not taken yet.
    fn take(&mut self, what: &str, wanted: impl Fn(&Value) -> bool) -> Value {
        if let Some(i) = self.backlog.iter().position(&wanted) {
            return self.backlog.remove(i).unwrap();
        }
        loop {
            let Some(message) = self.next(what) else {
                self.fail(&format!("the connection ended while waiting for {what}"));
            };
            if wanted(&message) {
                return message;
            }
            self.backlog.push_back(message);
        }
    }

    fn response(&mut self, id: &Value) -> Value {
        self.take(&format!("the response to {id}"), |m| {
            m.get("method").is_none() && &m["id"] == id
        })
    }

    fn notification(&mut self, method: &str) -> Value {
        let message = self.take(method, |m| m["method"] == method && m.get("id").is_none());
        message["params"].clone()
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

    /// The handshake; returns the session it describes.
    fn initialize(&mut self) -> Value {
        self.ok(
            "initialize",
            json!({"client": {"name": "test", "version": "0"}}),
        )
    }

    /// Run a whole turn, the session's `n`th; returns what the model said.
    fn turn(&mut self, n: u64, text: &str) -> String {
        let turn = self.ok("turn/start", json!({"text": text}))["turn"].clone();
        assert_eq!(turn, n, "the turn's number in its session");
        let ended = self.notification("turn/ended");
        assert_eq!(ended["turn"], turn, "{ended}");
        assert_eq!(ended["status"], "completed", "{ended}");
        self.log
            .iter()
            .filter(|m| m["method"] == "event/text" && m["params"]["turn"] == turn)
            .filter_map(|m| m["params"]["text"].as_str())
            .collect()
    }

    /// The error (its code) this connection is turned away with, and that
    /// it is then closed.
    fn turned_away(&mut self) -> i64 {
        let message = self.take("a refusal", |m| m.get("error").is_some());
        assert_eq!(message["id"], Value::Null, "{message}");
        self.ended();
        message["error"]["code"].as_i64().expect("a code")
    }

    /// Wait for the connection to end; nothing but a WebSocket's closing
    /// frame may come first.
    fn ended(&mut self) {
        while let Some(message) = self.next("the connection to end") {
            if message["ws"] != "close" {
                self.fail(&format!("expected the end, got {message}"));
            }
        }
    }
}

// ---------------------------------------------------------------- the tests

/// A Unix socket: a file only the user can open, in a directory only the
/// user can enter. A connection is a session — handshake, turn, shutdown —
/// and the connection closes when the session ends. A second listener on
/// the same socket is turned away; a socket nobody answers on is replaced.
#[cfg(unixlike)]
#[test]
fn a_unix_socket_session_runs_a_turn() {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;

    let mock = Mock::scripted(|_, _| answer("hello over the socket"));
    let dir = unique_dir("unix");
    // Short, whatever the temp directory is: a socket's path has to fit in
    // its address.
    let name = dir.file_name().unwrap().to_str().unwrap();
    let tag = &name[name.len() - 12..];
    let socket = PathBuf::from(format!("/tmp/atl-{tag}")).join("serve.sock");
    let arg = format!("unix:{}", socket.display());
    let mut listener = Listener::start(&mock, &dir, &["--listen", &arg], None);
    assert_eq!(listener.listening["transport"], "unix");
    assert_eq!(listener.listening["path"], socket.to_str().unwrap());
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&socket), 0o600);
    assert_eq!(mode(socket.parent().unwrap()), 0o700);

    let stream = UnixStream::connect(&socket).expect("connect");
    let mut peer = listener.peer_on_lines(stream.try_clone().unwrap(), stream);
    let session = peer.initialize()["session"].clone();
    assert_eq!(
        Path::new(session["cwd"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        dir.canonicalize().unwrap(),
        "a session works in the listener's directory"
    );
    assert_eq!(peer.turn(1, "say hello"), "hello over the socket");
    assert_eq!(peer.ok("shutdown", json!({})), json!({}));
    peer.ended();

    // The socket is taken for as long as the listener lives…
    let second = refused(&mock, &dir, &["--listen", &arg]);
    assert!(!second.status.success());
    let why = String::from_utf8_lossy(&second.stderr);
    assert!(why.contains("another listener is serving"), "{why}");
    assert!(second.stdout.is_empty());
    // …and a listener that died leaves a file the next one replaces.
    listener.child.kill().unwrap();
    listener.child.wait().unwrap();
    assert!(socket.exists(), "killed, it had no chance to clean up");
    let listener = Listener::start(&mock, &dir, &["--listen", &arg], None);
    let stream = UnixStream::connect(&socket).expect("connect again");
    let mut peer = listener.peer_on_lines(stream.try_clone().unwrap(), stream);
    peer.initialize();
}

/// The socket's directory is the lock on it: one other users can enter is
/// refused, and so is a path that is something else's.
#[cfg(unixlike)]
#[test]
fn a_unix_socket_in_an_open_directory_is_refused() {
    use std::os::unix::fs::PermissionsExt;

    let mock = Mock::scripted(|_, _| answer("unused"));
    let dir = unique_dir("open-dir");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let socket = dir.join("serve.sock");
    let out = refused(&mock, &dir, &["--listen", socket.to_str().unwrap()]);
    assert!(!out.status.success());
    let why = String::from_utf8_lossy(&out.stderr);
    assert!(why.contains("open to other users"), "{why}");
    assert!(!socket.exists());

    let private = dir.join("private");
    std::fs::create_dir(&private).unwrap();
    std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
    let file = private.join("notes.txt");
    std::fs::write(&file, "mine").unwrap();
    let out = refused(&mock, &dir, &["--listen", file.to_str().unwrap()]);
    let why = String::from_utf8_lossy(&out.stderr);
    assert!(why.contains("is not a socket"), "{why}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "mine");
}

/// Only loopback is listened on, and the listener's flags are `serve`'s
/// alone.
#[test]
fn a_listen_address_that_is_not_loopback_is_refused() {
    let mock = Mock::scripted(|_, _| answer("unused"));
    let dir = unique_dir("open");
    for address in ["0.0.0.0:0", "[::]:0", "192.0.2.1:8080", "example.com:80"] {
        let out = refused(&mock, &dir, &["--listen", address]);
        assert_eq!(out.status.code(), Some(2), "{address}");
        let why = String::from_utf8_lossy(&out.stderr);
        assert!(why.contains("loopback"), "{address}: {why}");
        assert!(out.stdout.is_empty(), "{address}");
    }
    // A token chosen by the caller has to be worth the name.
    let out = atelier(&mock, &dir, &["--listen", "127.0.0.1:0"], Some("short"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let why = String::from_utf8_lossy(&out.stderr);
    assert!(why.contains("ATELIER_SERVE_TOKEN must be"), "{why}");
}

/// Without a token of the caller's, one is made and written where only the
/// user can read it. A connection that does not present it first — a wrong
/// one, or a request in its place — is told so and closed; with it, the
/// connection is a session over JSON-RPC lines.
#[test]
fn tcp_wants_the_token_first_then_speaks_lines() {
    let mock = Mock::scripted(|_, _| answer("hello over tcp"));
    let dir = unique_dir("tcp");
    let listener = Listener::start(&mock, &dir, &["--listen", "127.0.0.1:0"], None);
    assert_eq!(listener.listening["transport"], "tcp");
    let address = listener.address();
    assert!(address.starts_with("127.0.0.1:") && !address.ends_with(":0"));
    assert_eq!(listener.listening["ws"], format!("ws://{address}/"));

    let file = PathBuf::from(listener.listening["token_file"].as_str().expect("a file"));
    let port = address.rsplit(':').next().unwrap();
    assert_eq!(
        file.file_name().unwrap().to_str().unwrap(),
        format!("serve-{port}.token")
    );
    let token = std::fs::read_to_string(&file).unwrap();
    let token = token.trim_end();
    // 256 bits, base64url.
    assert_eq!(token.len(), 43, "{token}");
    #[cfg(unixlike)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    // The token is in the file and nowhere else.
    assert!(!listener.stderr.lock().unwrap().contains(token));
    assert!(!listener.listening.to_string().contains(token));

    for wrong in [
        json!({ "token": "wrong" }).to_string(),
        json!({ "token": format!("{token}x") }).to_string(),
        json!({ "token": &token[..42] }).to_string(),
        json!({ "token": 7 }).to_string(),
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}).to_string(),
        "not json".to_string(),
    ] {
        let mut stream = listener.tcp();
        writeln!(stream, "{wrong}").unwrap();
        let mut peer = listener.peer_on_lines(stream.try_clone().unwrap(), stream);
        assert_eq!(peer.turned_away(), UNAUTHORIZED, "{wrong}");
    }
    assert!(mock.bodies().is_empty());

    let mut peer = listener.lines(token);
    let hello = peer.initialize();
    assert_eq!(hello["protocol"], 1);
    assert_eq!(peer.turn(1, "say hello"), "hello over tcp");
    assert_eq!(peer.ok("shutdown", json!({})), json!({}));
    peer.ended();
}

/// A WebSocket on the same port: the RFC's handshake, the token as a
/// subprotocol or in the query, one message per text frame both ways.
/// Fragments are joined, a ping is answered, a message written over several
/// lines is still one message, and a close is answered with a close.
#[test]
fn a_websocket_session_runs_a_turn() {
    let mock = Mock::scripted(|_, _| answer("hello over websocket"));
    let dir = unique_dir("ws");
    let listener = Listener::start(&mock, &dir, &["--listen", "127.0.0.1:0"], Some(TOKEN));
    assert_eq!(listener.listening["token_file"], Value::Null);

    let protocols = format!("atelier, token.{TOKEN}");
    let mut peer = listener
        .websocket("/", &[("Sec-WebSocket-Protocol", &protocols)])
        .expect("the handshake");
    peer.initialize();
    assert_eq!(peer.turn(1, "say hello"), "hello over websocket");

    // One message in three frames, a ping in the middle of them, and the
    // JSON spread over lines.
    let status =
        "{\n  \"jsonrpc\": \"2.0\",\r\n  \"id\": \"split\",\n  \"method\": \"session/status\"\n}";
    let (a, rest) = status.as_bytes().split_at(10);
    let (b, c) = rest.split_at(20);
    let mut bytes = frame(false, 0x1, a);
    bytes.extend(frame(true, 0x9, b"are you there"));
    bytes.extend(frame(false, 0x0, b));
    bytes.extend(frame(true, 0x0, c));
    (peer.raw)(&bytes);
    let pong = peer.take("the pong", |m| m["ws"] == "pong");
    assert_eq!(pong["payload"], "are you there");
    let status = peer.response(&json!("split"));
    assert_eq!(status["result"]["busy"], false, "{status}");

    // A frame as large as a tool's output: the 64-bit length, both ways.
    let big = "x".repeat(70_000);
    let echoed = peer.ok("command/run", json!({ "line": format!("/nonesuch {big}") }));
    assert!(echoed.is_object());

    assert_eq!(peer.ok("shutdown", json!({})), json!({}));
    let close = peer.take("the closing frame", |m| m["ws"] == "close");
    assert_eq!(close["code"], 1000);
    peer.ended();

    // The token in the query does as well.
    let mut peer = listener
        .websocket(&format!("/?token={TOKEN}"), &[])
        .expect("the handshake, token in the query");
    peer.initialize();
}

/// What a browser may do: a page's WebSocket says where the page is from,
/// and only origins named with `--allow-origin` get in — before the token
/// is even looked at. Without the token nothing gets in, and plain HTTP is
/// answered with an error rather than served.
#[test]
fn origins_tokens_and_plain_http_are_checked() {
    let mock = Mock::scripted(|_, _| answer("unused"));
    let dir = unique_dir("origin");
    let args = [
        "--listen",
        "127.0.0.1:0",
        "--allow-origin",
        "http://localhost:5173",
    ];
    let listener = Listener::start(&mock, &dir, &args, Some(TOKEN));
    let good = format!("/?token={TOKEN}");
    let refusal = |target: &str, headers: &[(&str, &str)]| match listener.websocket(target, headers)
    {
        Ok(_) => panic!("{target} {headers:?} was let in"),
        Err(response) => response,
    };

    // The right token, from the wrong place.
    for origin in [
        "http://evil.example",
        "http://localhost:5174",
        "https://localhost:5173",
        "http://localhost:5173.evil.example",
        "null",
    ] {
        let response = refusal(&good, &[("Origin", origin)]);
        assert!(
            response.starts_with("HTTP/1.1 403 "),
            "{origin}: {response}"
        );
        // Nothing a page could read across origins.
        assert!(!response.to_ascii_lowercase().contains("access-control"));
    }
    // The right place (however it is spelt), or no page at all.
    for headers in [
        &[("Origin", "http://localhost:5173")][..],
        &[("origin", "HTTP://LOCALHOST:5173")][..],
        &[][..],
    ] {
        let mut peer = listener.websocket(&good, headers).expect("allowed");
        peer.initialize();
    }

    // The token: missing, wrong, or right but somewhere it is not read.
    let allowed = ("Origin", "http://localhost:5173");
    for (target, headers) in [
        ("/", &[allowed][..]),
        ("/?token=wrong", &[allowed][..]),
        ("/?token=", &[][..]),
        (
            "/",
            &[("Sec-WebSocket-Protocol", "atelier, token.wrong")][..],
        ),
        (
            "/",
            &[("Authorization", "Bearer test-token-0123456789abcdef")][..],
        ),
    ] {
        let response = refusal(target, headers);
        assert!(
            response.starts_with("HTTP/1.1 401 "),
            "{target}: {response}"
        );
    }

    // Not WebSocket at all: an error, whatever it asks for and with the
    // token or not.
    for request in [
        format!("GET /?token={TOKEN} HTTP/1.1\r\nHost: x\r\n\r\n"),
        format!("POST /?token={TOKEN} HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n"),
        format!(
            "POST / HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: {WS_KEY}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        ),
    ] {
        let response = listener.exchange(&request);
        assert!(
            response.starts_with("HTTP/1.1 426 "),
            "{request}: {response}"
        );
    }
    // An old or missing version, a missing key.
    let response = listener.exchange(&format!(
        "GET /?token={TOKEN} HTTP/1.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: {WS_KEY}\r\nSec-WebSocket-Version: 8\r\n\r\n"
    ));
    assert!(response.starts_with("HTTP/1.1 426 "), "{response}");
    assert!(response.contains("Sec-WebSocket-Version: 13"), "{response}");
    let response = listener.exchange(&format!(
        "GET /?token={TOKEN} HTTP/1.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Version: 13\r\n\r\n"
    ));
    assert!(response.starts_with("HTTP/1.1 400 "), "{response}");

    // None of that started a session, or stopped the listener.
    assert!(mock.bodies().is_empty());
    let mut peer = listener.lines(TOKEN);
    peer.initialize();
}

/// What a WebSocket client must not do ends the connection with the close
/// code that says why: a frame that is not masked, a binary message.
#[test]
fn websocket_violations_close_the_connection() {
    let mock = Mock::scripted(|_, _| answer("unused"));
    let dir = unique_dir("ws-bad");
    let listener = Listener::start(&mock, &dir, &["--listen", "127.0.0.1:0"], Some(TOKEN));
    let target = format!("/?token={TOKEN}");

    let mut peer = listener.websocket(&target, &[]).unwrap();
    (peer.raw)(&[0x81, 0x02, b'{', b'}']);
    assert_eq!(peer.take("a close", |m| m["ws"] == "close")["code"], 1002);
    peer.ended();

    let mut peer = listener.websocket(&target, &[]).unwrap();
    (peer.raw)(&frame(true, 0x2, b"{}"));
    assert_eq!(peer.take("a close", |m| m["ws"] == "close")["code"], 1003);
    peer.ended();

    // A frame announcing more than a message may hold: refused on its
    // header, before any of it is sent.
    let mut peer = listener.websocket(&target, &[]).unwrap();
    let mut huge = vec![0x81, 0x80 | 127];
    huge.extend((1u64 << 32).to_be_bytes());
    huge.extend([0u8; 4]);
    (peer.raw)(&huge);
    assert_eq!(peer.take("a close", |m| m["ws"] == "close")["code"], 1009);
    peer.ended();
}

/// Each connection is a session of its own — its own turns, its own
/// history, its own mode — and `--max-sessions` of them at most: one more
/// is told so and closed, until a session ends and leaves its place.
#[test]
fn connections_are_independent_sessions_up_to_the_limit() {
    let mock = Mock::scripted(|_, body| {
        if body.contains("question from the second") {
            answer("answer to the second")
        } else {
            answer("answer to the first")
        }
    });
    let dir = unique_dir("two");
    let args = [
        "--listen",
        "127.0.0.1:0",
        "--max-sessions",
        "2",
        "--mode",
        "plan",
    ];
    let listener = Listener::start(&mock, &dir, &args, Some(TOKEN));

    let mut first = listener.lines(TOKEN);
    let mut second = listener
        .websocket(&format!("/?token={TOKEN}"), &[])
        .unwrap();
    // The session's flags are the listener's.
    assert_eq!(first.initialize()["session"]["mode"], "plan");
    assert_eq!(second.initialize()["session"]["mode"], "plan");

    assert_eq!(
        first.turn(1, "question from the first"),
        "answer to the first"
    );
    assert_eq!(
        second.turn(1, "question from the second"),
        "answer to the second"
    );
    // Each was its session's first turn, and neither saw the other's.
    let bodies = mock.bodies();
    assert_eq!(bodies.len(), 2);
    assert!(bodies[0].contains("from the first") && !bodies[0].contains("from the second"));
    assert!(bodies[1].contains("from the second") && !bodies[1].contains("from the first"));
    // What one changes, the other keeps.
    first.ok("command/run", json!({"line": "/mode auto-edit"}));
    assert_eq!(first.ok("session/status", json!({}))["mode"], "auto-edit");
    assert_eq!(second.ok("session/status", json!({}))["mode"], "plan");

    // A third is one too many, on either framing.
    assert_eq!(listener.lines(TOKEN).turned_away(), TOO_MANY_SESSIONS);
    let mut third = listener
        .websocket(&format!("/?token={TOKEN}"), &[])
        .unwrap();
    let refusal = third.take("a refusal", |m| m.get("error").is_some());
    assert_eq!(refusal["error"]["code"], TOO_MANY_SESSIONS);
    assert_eq!(third.take("a close", |m| m["ws"] == "close")["code"], 1013);
    third.ended();

    // The first leaves; once its session is gone there is room again.
    first.hang_up();
    let deadline = Instant::now() + DEADLINE;
    let mut again = loop {
        let mut peer = listener.lines(TOKEN);
        let id = peer.request("initialize", json!({}));
        match peer.next("the handshake's result or a refusal") {
            Some(m) if m["id"] == id => break peer,
            _ if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            other => panic!("no room after a session ended: {other:?}"),
        }
    };
    // The second was not disturbed by any of it.
    assert_eq!(
        second.turn(2, "another question from the second"),
        "answer to the second"
    );
    assert_eq!(
        again.turn(1, "a question from the third"),
        "answer to the first"
    );
}
