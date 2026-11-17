//! Two ateliers in one project directory: who owns the session, and what
//! the one started second can do about it (`src/own.rs`,
//! `docs/sessions.md`). Real processes throughout — the binary against a
//! scripted mock of an OpenAI-compatible server, on pipes where the line
//! REPL or a protocol will do and on pseudo-terminals where the terminal
//! interface is the subject.
//!
//! Who owns a session is the coordinator's to say, so every test has a
//! coordinator of its own, in a directory beside its project; it is asked
//! who the owner is as atelier asks it (`claims`), and stopped when the
//! test is over.
//!
//! Nothing sleeps to let something happen: a test waits for a line, a file,
//! a process to be gone, each with a deadline. The process table is read
//! from `/proc`, so this is Linux's.

#![cfg(target_os = "linux")]

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_atelier");

/// How long a test waits for something that should happen at once (the
/// machine may be busy with other work).
const PATIENCE: Duration = Duration::from_secs(60);

const EXIT_IN_USE: i32 = 7;
const EXIT_CANCELLED: i32 = 8;
const SESSION_IN_USE: i64 = -32010;

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------- the mock

/// Answers the Nth chat request (0-based), given its body, by writing the
/// HTTP response to the connection — at once or, to hold a turn open, when
/// the test says.
type Responder = Arc<dyn Fn(usize, &str, &mut TcpStream) + Send + Sync>;

struct Mock {
    port: u16,
    bodies: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Mock {
    fn scripted(script: impl Fn(usize, &str) -> Vec<u8> + Send + Sync + 'static) -> Mock {
        Mock::start(Arc::new(move |n, body, stream| {
            let _ = stream.write_all(&script(n, body));
        }))
    }

    /// Each request on a thread of its own, so that one held open does not
    /// keep the next from being answered.
    fn start(responder: Responder) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let (stop_thread, bodies_thread) = (Arc::clone(&stop), Arc::clone(&bodies));
        let handle = thread::spawn(move || {
            let counter = Arc::new(AtomicUsize::new(0));
            while !stop_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let (responder, bodies) =
                            (Arc::clone(&responder), Arc::clone(&bodies_thread));
                        let counter = Arc::clone(&counter);
                        thread::spawn(move || {
                            let _ = handle_conn(stream, &counter, &responder, &bodies);
                        });
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

    fn requests(&self) -> usize {
        self.bodies.lock().unwrap().len()
    }

    fn body(&self, n: usize) -> String {
        self.bodies.lock().unwrap()[n].clone()
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
    counter: &AtomicUsize,
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
    let n = {
        let mut bodies = bodies.lock().unwrap();
        bodies.push(body.clone());
        counter.fetch_add(1, Ordering::SeqCst)
    };
    responder(n, &body, &mut writer);
    let _ = writer.shutdown(std::net::Shutdown::Write);
    Ok(())
}

const SSE_HEAD: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                        Cache-Control: no-cache\r\nConnection: close\r\n\r\n";

fn sse(frames: &[String]) -> Vec<u8> {
    format!("{SSE_HEAD}{}data: [DONE]\n\n", frames.concat()).into_bytes()
}

fn answer(text: &str) -> Vec<u8> {
    let chunk = json!({"choices": [{"index": 0, "delta": {"content": text}}]});
    sse(&[format!("data: {chunk}\n\n")])
}

fn tool_calls(calls: &[(&str, &str, Value)]) -> Vec<u8> {
    let calls: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(index, (id, name, arguments))| {
            json!({"index": index, "id": id, "type": "function",
                   "function": {"name": name, "arguments": arguments.to_string()}})
        })
        .collect();
    let chunk = json!({"choices": [{"index": 0, "delta": {"tool_calls": calls}}]});
    sse(&[format!("data: {chunk}\n\n")])
}

/// Whether a request already carries a tool result.
fn has_tool_result(body: &str) -> bool {
    let request: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    request["messages"]
        .as_array()
        .is_some_and(|m| m.iter().any(|m| m["role"] == "tool"))
}

/// Whether a request ends on a tool's result: the step after a call,
/// rather than a prompt's first.
fn after_tool(body: &str) -> bool {
    let request: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let last = request["messages"]
        .as_array()
        .and_then(|m| m.last().cloned());
    last.is_some_and(|m| m["role"] == "tool")
}

/// What the user said last in a request.
fn last_user(body: &str) -> String {
    let request: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let messages = request["messages"].as_array().cloned().unwrap_or_default();
    let last = messages.iter().rev().find(|m| m["role"] == "user");
    last.and_then(|m| m["content"].as_str())
        .unwrap_or_default()
        .to_string()
}

/// A mock that answers every prompt with `re: <the prompt's last word>`.
fn echo() -> Mock {
    Mock::scripted(|_, body| {
        let said = last_user(body);
        answer(&format!(
            "re: {}",
            said.split_whitespace().last().unwrap_or("")
        ))
    })
}

// ------------------------------------------------------------ the project

/// A project directory of a test's own, removed — with the directories
/// beside it that stand for the user's — when the test has passed.
struct Project(PathBuf);

impl std::ops::Deref for Project {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        // The test's coordinator goes with it, passed or not.
        stop_coordinator(&self.0);
        if thread::panicking() {
            return;
        }
        let _ = std::fs::remove_dir_all(&self.0);
        for beside in ["run", "state", "data", "home"] {
            let _ = std::fs::remove_dir_all(self.0.with_extension(beside));
        }
    }
}

fn project(name: &str) -> Project {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    // Short: the owner's socket goes in a directory beside it.
    let dir = std::env::temp_dir().join(format!("atl-own-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    Project(dir)
}

/// Where the coordinator of the project in `dir` is, and its owners'
/// sockets: a directory of the test's own.
fn coord_dir(dir: &Path) -> PathBuf {
    dir.with_extension("run")
}

/// One request to the coordinator of the project in `dir`, if one is
/// running: its result.
fn ask_coordinator(dir: &Path, method: &str, params: Value) -> Option<Value> {
    use std::os::unix::net::UnixStream;
    let stream = UnixStream::connect(coord_dir(dir).join("coordinator.sock")).ok()?;
    stream.set_read_timeout(Some(PATIENCE)).ok()?;
    let request = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    (&stream)
        .write_all(format!("{request}\n").as_bytes())
        .ok()?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).ok()?;
    let answer: Value = serde_json::from_str(&line).ok()?;
    answer.get("result").cloned()
}

/// The pid of the coordinator of the project in `dir`, if one is running.
fn coordinator_pid(dir: &Path) -> Option<u32> {
    let status = ask_coordinator(dir, "status", json!({}))?;
    status["pid"].as_u64().map(|pid| pid as u32)
}

/// End the coordinator of the project in `dir`, and see it gone.
fn stop_coordinator(dir: &Path) {
    let Some(pid) = coordinator_pid(dir) else {
        return;
    };
    ask_coordinator(dir, "shutdown", json!({}));
    let deadline = Instant::now() + PATIENCE;
    while alive(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
}

/// `atelier <args>` in `cwd` against the mock, with nothing of the outer
/// environment or the real home leaking in, and its coordinator and
/// sockets in a directory of the test's own.
fn atelier(base_url: &str, cwd: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.args(args)
        .current_dir(cwd)
        .env("ATELIER_BASE_URL", base_url)
        .env("ATELIER_MODEL", "mock")
        .env("ATELIER_APPROVE", "all")
        .env("ATELIER_COORDINATOR_DIR", coord_dir(cwd))
        .env("ATELIER_NO_UPDATE", "1")
        .env("XDG_RUNTIME_DIR", cwd.with_extension("run"))
        .env("XDG_STATE_HOME", cwd.with_extension("state"))
        // What it keeps for the project: next to it, not in the real home.
        .env(
            "ATELIER_STATE_DIR",
            cwd.with_extension("state").join("atelier"),
        )
        .env("XDG_CONFIG_HOME", cwd.join(".no-user-config"))
        .env("XDG_DATA_HOME", cwd.with_extension("data"))
        .env("HOME", cwd.with_extension("home"))
        .env("TERM", "xterm")
        .env_remove("ATELIER_NO_COORDINATOR")
        .env_remove("ATELIER_COORDINATOR_WAKE")
        .env_remove("ATELIER_TRUST_HOOKS")
        .env_remove("ATELIER_TRUST_PROJECT")
        .env_remove("ATELIER_API_KEY")
        .env_remove("ATELIER_TRACE")
        .env_remove("ATELIER_DEBUG")
        .env_remove("ATELIER_BACKEND")
        .env_remove("ATELIER_CONTEXT_LIMIT")
        .env_remove("ATELIER_REVIEW")
        .env_remove("ATELIER_MODE")
        .env_remove("ATELIER_PARALLEL_TOOLS");
    cmd
}

/// The record of the owner of the session in `dir`, as the coordinator has
/// it — if the session has one.
fn record(dir: &Path) -> Option<Value> {
    let real = dir.canonicalize().ok()?;
    let claims = ask_coordinator(dir, "claims", json!({ "dir": real }))?;
    let owner = claims["claims"][0]["owner"].clone();
    owner.is_object().then_some(owner)
}

/// Wait until `pid` owns the session in `dir`, and return its record.
fn wait_owner(dir: &Path, pid: u32) -> Value {
    wait_until(&format!("pid {pid} to own the session"), || {
        record(dir).is_some_and(|r| r["pid"] == pid)
    });
    record(dir).unwrap()
}

/// Whether `pid` is a process that is still running (a zombie is not).
fn alive(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let state = stat
        .rsplit(')')
        .next()
        .and_then(|rest| rest.split_whitespace().next());
    state.is_some_and(|s| s != "Z")
}

/// The pid a command wrote to `file`, once it has.
fn pid_in(file: &Path) -> u32 {
    let mut pid = None;
    wait_until(&format!("{}", file.display()), || {
        pid = std::fs::read_to_string(file)
            .ok()
            .and_then(|t| t.trim().parse().ok());
        pid.is_some()
    });
    pid.unwrap()
}

/// Every `atelier` process started from this test binary that is still
/// running in `dir`.
fn ateliers_in(dir: &Path) -> Vec<u32> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok();
        let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok();
        if exe.as_deref() == Some(Path::new(BIN)) && cwd.as_deref() == Some(dir) && alive(pid) {
            found.push(pid);
        }
    }
    found
}

// ------------------------------------------------- a process on pipes

/// An `atelier` on pipes whose output the test can watch while it runs.
struct Piped {
    child: Child,
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
    /// The threads reading the two, until the pipes close.
    readers: Vec<JoinHandle<()>>,
}

fn collect(pipe: impl Read + Send + 'static) -> (Arc<Mutex<String>>, JoinHandle<()>) {
    let text = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&text);
    let reader = thread::spawn(move || {
        let mut pipe = pipe;
        let mut bytes = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok(n) = pipe.read(&mut buf) {
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buf[..n]);
            *sink.lock().unwrap() = String::from_utf8_lossy(&bytes).into_owned();
        }
    });
    (text, reader)
}

impl Piped {
    fn spawn(mut cmd: Command) -> Piped {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn atelier");
        let (stdout, out_reader) = collect(child.stdout.take().unwrap());
        let (stderr, err_reader) = collect(child.stderr.take().unwrap());
        Piped {
            child,
            stdout,
            stderr,
            readers: vec![out_reader, err_reader],
        }
    }

    /// The line REPL in `dir`, and the session its: waits until it owns it.
    fn repl(mock: &Mock, dir: &Path, args: &[&str]) -> Piped {
        let piped = Piped::spawn(atelier(&mock.base_url(), dir, args));
        wait_owner(dir, piped.pid());
        piped
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn line(&mut self, text: &str) {
        writeln!(self.child.stdin.as_mut().expect("stdin open"), "{text}").expect("write a line");
    }

    fn stdout(&self) -> String {
        self.stdout.lock().unwrap().clone()
    }

    fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    fn wait_for(&self, needle: &str) {
        let deadline = Instant::now() + PATIENCE;
        while !(self.stdout().contains(needle) || self.stderr().contains(needle)) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {needle:?}\nstdout:\n{}\nstderr:\n{}",
                self.stdout(),
                self.stderr()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait for the process to end by itself, and return its exit status
    /// (-1 when a signal ended it). What it printed is all there then.
    fn exit(&mut self) -> i32 {
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                // The pipes close with the process, unless something it
                // started still holds them: that is not waited for long.
                let patience = Instant::now() + Duration::from_secs(5);
                while self.readers.iter().any(|r| !r.is_finished()) && Instant::now() < patience {
                    thread::sleep(Duration::from_millis(5));
                }
                return status.code().unwrap_or(-1);
            }
            assert!(
                Instant::now() < deadline,
                "atelier did not exit\nstdout:\n{}\nstderr:\n{}",
                self.stdout(),
                self.stderr()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// End the input, and wait for the exit.
    fn finish(&mut self) -> i32 {
        drop(self.child.stdin.take());
        self.exit()
    }
}

impl Drop for Piped {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Run `atelier <args>` to its end with `stdin` as its input: its exit
/// status, stdout and stderr.
fn run(mock: &Mock, dir: &Path, args: &[&str], stdin: &str) -> (i32, String, String) {
    let mut piped = Piped::spawn(atelier(&mock.base_url(), dir, args));
    piped
        .child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let code = piped.finish();
    (code, piped.stdout(), piped.stderr())
}

// ------------------------------------------- a process on a terminal

unsafe extern "C" {
    fn posix_openpt(flags: i32) -> i32;
    fn grantpt(fd: i32) -> i32;
    fn unlockpt(fd: i32) -> i32;
    fn ptsname_r(fd: i32, buf: *mut u8, len: usize) -> i32;
    fn ioctl(fd: i32, request: u64, ...) -> i32;
}

/// An `atelier` on a pseudo-terminal: the terminal interface, typed to and
/// read as a person would.
struct Term {
    child: Child,
    master: File,
    screen: Arc<Mutex<Vec<u8>>>,
}

impl Term {
    fn spawn(mut cmd: Command) -> Term {
        const O_RDWR: i32 = 2;
        const O_NOCTTY: i32 = 0o400;
        const TIOCSWINSZ: u64 = 0x5414;
        // SAFETY: plain calls into the C library std links, on a
        // descriptor this function owns.
        let (master, slave) = unsafe {
            let fd = posix_openpt(O_RDWR | O_NOCTTY);
            assert!(fd >= 0, "posix_openpt");
            assert_eq!(grantpt(fd), 0);
            assert_eq!(unlockpt(fd), 0);
            let mut name = [0u8; 128];
            assert_eq!(ptsname_r(fd, name.as_mut_ptr(), name.len()), 0);
            let len = name.iter().position(|&b| b == 0).unwrap();
            let path = std::str::from_utf8(&name[..len]).unwrap().to_string();
            let size: [u16; 4] = [40, 200, 0, 0];
            assert_eq!(ioctl(fd, TIOCSWINSZ, size.as_ptr()), 0);
            (File::from_raw_fd(fd), path)
        };
        let end = || {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&slave)
                .expect("open the terminal")
        };
        let child = cmd
            .stdin(end())
            .stdout(end())
            .stderr(end())
            .spawn()
            .expect("spawn atelier on a terminal");
        let screen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&screen);
        let mut reader = master.try_clone().unwrap();
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        let _ = master.as_raw_fd();
        Term {
            child,
            master,
            screen,
        }
    }

    /// The terminal interface in `dir`.
    fn start(mock: &Mock, dir: &Path, args: &[&str]) -> Term {
        Term::spawn(atelier(&mock.base_url(), dir, args))
    }

    /// The terminal interface in `dir`, up and the session its.
    fn owner(mock: &Mock, dir: &Path, args: &[&str]) -> Term {
        let term = Term::start(mock, dir, args);
        wait_owner(dir, term.pid());
        term.wait_for("/help for commands");
        term
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Everything the terminal was sent, less the escape sequences.
    fn text(&self) -> String {
        let raw = String::from_utf8_lossy(&self.screen.lock().unwrap()).into_owned();
        let mut out = String::new();
        let mut chars = raw.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                out.push(c);
                continue;
            }
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
        }
        out
    }

    fn wait_for(&self, needle: &str) {
        let deadline = Instant::now() + PATIENCE;
        while !self.text().contains(needle) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {needle:?} on the terminal:\n{}",
                self.text()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait until `needle` has been shown `times` times.
    fn wait_times(&self, needle: &str, times: usize) {
        let deadline = Instant::now() + PATIENCE;
        while self.text().matches(needle).count() < times {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {needle:?} ({times} times) on the terminal:\n{}",
                self.text()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn keys(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).expect("type");
    }

    /// Type a line and Enter.
    fn line(&mut self, text: &str) {
        self.keys(text.as_bytes());
        self.keys(b"\r");
    }

    fn exit(&mut self) -> i32 {
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code().unwrap_or(-1);
            }
            assert!(
                Instant::now() < deadline,
                "atelier did not exit; its terminal:\n{}",
                self.text()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// --------------------------------------------------- a serve client

/// The client end of an `atelier serve` on pipes.
struct Client {
    piped: Piped,
    seen: usize,
    next_id: u64,
}

impl Client {
    fn spawn(mock: &Mock, dir: &Path, args: &[&str]) -> Client {
        let mut all = vec!["serve"];
        all.extend_from_slice(args);
        let mut cmd = atelier(&mock.base_url(), dir, &all);
        cmd.env_remove("ATELIER_APPROVE");
        Client {
            piped: Piped::spawn(cmd),
            seen: 0,
            next_id: 0,
        }
    }

    fn send(&mut self, message: Value) {
        self.piped.line(&message.to_string());
    }

    /// The next message for which `want` holds, skipping the others.
    fn next(&mut self, what: &str, want: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let out = self.piped.stdout();
            let lines: Vec<&str> = out.lines().collect();
            // The last line may still be arriving.
            let whole = if out.ends_with('\n') {
                lines.len()
            } else {
                lines.len().saturating_sub(1)
            };
            while self.seen < whole {
                let message: Value = serde_json::from_str(lines[self.seen])
                    .unwrap_or_else(|e| panic!("not JSON ({e}): {}", lines[self.seen]));
                self.seen += 1;
                if want(&message) {
                    return message;
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}\nstdout:\n{out}\nstderr:\n{}",
                self.piped.stderr()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Send a request and wait for its answer (result or error).
    fn call(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        self.next(method, |m| m["id"] == id && m.get("method").is_none())
    }

    fn result(&mut self, method: &str, params: Value) -> Value {
        let answer = self.call(method, params);
        assert!(answer.get("error").is_none(), "{method} failed: {answer}");
        answer["result"].clone()
    }

    fn notification(&mut self, method: &str) -> Value {
        self.next(method, |m| m["method"] == method)["params"].clone()
    }
}

/// Where atelier keeps what it writes for the project at `dir` — outside
/// it: under the state base these tests give it (`ATELIER_STATE_DIR`, next
/// to the project), in a directory named after the project's path.
fn state_dir(dir: &Path) -> PathBuf {
    let real = dir.canonicalize().expect("the project exists");
    let base = dir.with_extension("state").join("atelier");
    let base = base.canonicalize().unwrap_or(base);
    base.join("projects")
        .join(real.to_string_lossy().replace('/', "-"))
}

// ============================================================== the cases

/// A start that cannot ask finds the session owned and leaves: its own exit
/// status, the owner named, nothing touched. `--cancel` is the same answer
/// given beforehand, with a status of its own.
#[test]
fn a_start_that_cannot_ask_is_refused_and_touches_nothing() {
    let mock = echo();
    let dir = project("refused");
    let mut owner = Piped::repl(&mock, &dir, &[]);
    owner.line("say one");
    owner.wait_for("re: one");
    wait_until("the session to be saved", || {
        state_dir(&dir).join("session.json").exists()
    });
    let saved = std::fs::read(state_dir(&dir).join("session.json")).unwrap();
    let before = record(&dir).unwrap();
    assert_eq!(before["kind"], "repl");
    assert_eq!(before["pid"], owner.pid());

    // The REPL, on a pipe.
    let (code, out, err) = run(&mock, &dir, &[], "say two\n");
    assert_eq!(code, EXIT_IN_USE, "{out}\n{err}");
    assert!(
        err.contains("a session is already running in this directory"),
        "{err}"
    );
    assert!(err.contains(&format!("pid {}", owner.pid())), "{err}");
    assert!(
        err.contains("the line REPL") && err.contains("now: idle"),
        "{err}"
    );
    assert!(
        err.contains("--take-over") && err.contains("--cancel"),
        "{err}"
    );
    // `--print --continue`, and `serve` whose client just leaves.
    let (code, _, err) = run(&mock, &dir, &["--continue", "--print", "say two"], "");
    assert_eq!(code, EXIT_IN_USE, "{err}");
    let (code, out, err) = run(&mock, &dir, &["serve"], "");
    assert_eq!(code, EXIT_IN_USE, "{err}");
    assert!(out.is_empty(), "{out}");
    // The answer given beforehand.
    for args in [
        &["--cancel"][..],
        &["--continue", "--print", "--cancel", "x"],
        &["serve", "--cancel"],
    ] {
        let (code, _, err) = run(&mock, &dir, args, "say two\n");
        assert_eq!(code, EXIT_CANCELLED, "{args:?}: {err}");
        assert!(err.contains("left alone"), "{args:?}: {err}");
    }

    // Nothing was asked of the model, written or stopped.
    assert_eq!(mock.requests(), 1);
    assert_eq!(
        std::fs::read(state_dir(&dir).join("session.json")).unwrap(),
        saved
    );
    assert_eq!(record(&dir).unwrap(), before);
    owner.line("say three");
    owner.wait_for("re: three");
    assert_eq!(owner.finish(), 0);
    assert!(ateliers_in(&dir).is_empty());
}

/// Runs with a conversation of their own are never in the way, nor in it:
/// a one-shot `--print`, `debug prompt` and `doctor` run next to a live
/// session as if it were not there — and leave its saved file alone.
#[test]
fn runs_that_do_not_own_the_session_are_never_blocked() {
    let mock = echo();
    let dir = project("unowned");
    let mut owner = Piped::repl(&mock, &dir, &[]);
    owner.line("say one");
    owner.wait_for("re: one");
    wait_until("the session to be saved", || {
        state_dir(&dir).join("session.json").exists()
    });
    let saved = std::fs::read(state_dir(&dir).join("session.json")).unwrap();

    let (code, out, err) = run(&mock, &dir, &["--print", "say aside"], "");
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("re: aside"), "{out}");
    let (code, out, err) = run(&mock, &dir, &["debug", "prompt", "hello"], "");
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("\"messages\""), "{out}");
    let (code, _, err) = run(&mock, &dir, &["doctor", "--offline"], "");
    assert!(!err.contains("already running"), "{code}: {err}");
    // The flags are about a session a run would own.
    let (code, _, err) = run(&mock, &dir, &["--print", "--take-over", "x"], "");
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("--print needs --continue"), "{err}");

    // The saved session is still the owner's own.
    assert_eq!(
        std::fs::read(state_dir(&dir).join("session.json")).unwrap(),
        saved
    );
    assert_eq!(record(&dir).unwrap()["pid"], owner.pid());
    // With nobody owning it, a one-shot run saves as it always did.
    assert_eq!(owner.finish(), 0);
    let (code, ..) = run(&mock, &dir, &["--print", "say after"], "");
    assert_eq!(code, 0);
    assert_ne!(
        std::fs::read(state_dir(&dir).join("session.json")).unwrap(),
        saved
    );
}

/// An owner killed outright leaves nothing behind: its claim ends with its
/// connection, and the next start is the owner without a word.
#[test]
fn an_owner_killed_outright_leaves_nothing_behind() {
    let mock = echo();
    let dir = project("stale");
    let mut owner = Piped::repl(&mock, &dir, &[]);
    let old = record(&dir).unwrap();
    let socket = PathBuf::from(old["socket"].as_str().expect("a socket"));
    assert!(socket.exists());
    Command::new("kill")
        .args(["-9", &owner.pid().to_string()])
        .status()
        .unwrap();
    assert_eq!(owner.exit(), -1);
    wait_until("the session to be nobody's", || record(&dir).is_none());

    let mut next = Piped::repl(&mock, &dir, &[]);
    assert!(!socket.exists(), "the dead owner's socket is taken away");
    next.line("say hello");
    next.wait_for("re: hello");
    assert_eq!(next.finish(), 0);
    let err = next.stderr();
    assert!(
        !err.contains("already running") && !err.contains("took over"),
        "{err}"
    );
}

/// Owning a session writes nothing in the project: an atelier started and
/// quit in a fresh directory, no turn taken, leaves it as it found it.
#[test]
fn starting_and_quitting_leaves_the_directory_as_it_was() {
    let mock = echo();
    let dir = project("clean");
    let mut repl = Piped::repl(&mock, &dir, &[]);
    assert_eq!(repl.finish(), 0, "{}", repl.stderr());
    let mut term = Term::start(&mock, &dir, &[]);
    term.wait_for("/help for commands");
    wait_owner(&dir, term.pid());
    term.line("/quit");
    assert_eq!(term.exit(), 0);
    // A start that is refused, and one that only looks.
    let mut owner = Piped::repl(&mock, &dir, &[]);
    let (code, ..) = run(&mock, &dir, &[], "");
    assert_eq!(code, EXIT_IN_USE);
    let (code, ..) = run(&mock, &dir, &["doctor", "--offline"], "");
    assert_eq!(code, 0);
    assert_eq!(owner.finish(), 0);

    let left: Vec<_> = std::fs::read_dir(&*dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert!(left.is_empty(), "left in the project: {left:?}");
    assert_eq!(mock.requests(), 0);
}

/// The coordinator is killed under an owner. The owner finds the next one
/// — here the one a newcomer starts — and claims its session again before
/// the newcomer is given it: the newcomer is refused as on any other day.
#[test]
fn an_owner_keeps_its_session_across_coordinators() {
    let mock = echo();
    let dir = project("restart");
    let mut owner = Piped::repl(&mock, &dir, &[]);
    for round in 0..3 {
        let daemon = coordinator_pid(&dir).expect("a coordinator");
        Command::new("kill")
            .args(["-9", &daemon.to_string()])
            .status()
            .unwrap();
        wait_until("the coordinator to be gone", || !alive(daemon));

        let (code, _, err) = run(&mock, &dir, &[], "say two\n");
        assert_eq!(code, EXIT_IN_USE, "round {round}: {err}");
        assert!(err.contains(&format!("pid {}", owner.pid())), "{err}");
        assert_ne!(coordinator_pid(&dir), Some(daemon));
        assert_eq!(wait_owner(&dir, owner.pid())["kind"], "repl");
    }
    // And one that is stopped, with nobody arriving: the owner starts the
    // next one itself.
    let daemon = coordinator_pid(&dir).expect("a coordinator");
    stop_coordinator(&dir);
    wait_owner(&dir, owner.pid());
    assert_ne!(coordinator_pid(&dir), Some(daemon));

    owner.line("say one");
    owner.wait_for("re: one");
    assert_eq!(owner.finish(), 0);
    let err = owner.stderr();
    assert!(!err.contains("another atelier"), "{err}");
    assert_eq!(mock.requests(), 1);
}

/// With the coordinator switched off nobody says whose the session is: a
/// second atelier starts as it did before there were owners — no question,
/// no refusal — and no coordinator is started to ask.
#[test]
fn without_a_coordinator_nothing_is_asked() {
    let mock = echo();
    let dir = project("alone");
    let alone = |args: &[&str]| {
        let mut cmd = atelier(&mock.base_url(), &dir, args);
        cmd.env("ATELIER_NO_COORDINATOR", "1");
        cmd
    };
    let mut first = Piped::spawn(alone(&[]));
    first.line("say one");
    first.wait_for("re: one");
    let mut second = Piped::spawn(alone(&["--take-over"]));
    second.line("say two");
    second.wait_for("re: two");
    // A terminal is asked nothing either.
    let mut third = Term::spawn(alone(&[]));
    third.wait_for("/help for commands");
    third.line("/quit");
    assert_eq!(third.exit(), 0);
    assert!(!third.text().contains("already running"));

    assert_eq!(first.finish(), 0);
    assert_eq!(second.finish(), 0);
    for err in [first.stderr(), second.stderr()] {
        assert!(
            !err.contains("already running") && !err.contains("took over"),
            "{err}"
        );
    }
    assert_eq!(coordinator_pid(&dir), None);
    assert!(!coord_dir(&dir).join("coordinator.sock").exists());
}

/// Two owners that could not see each other when they started — there was
/// no coordinator to be had — meet when one comes up: the first to claim
/// the session has it, the other is told, and neither is stopped.
#[test]
fn two_owners_that_meet_later_are_told_and_left_running() {
    let mock = echo();
    let dir = project("meet");
    // Where the coordinator would be is a file: none can start.
    std::fs::write(coord_dir(&dir), "in the way").unwrap();
    let mut both: Vec<Piped> = (0..2)
        .map(|_| Piped::spawn(atelier(&mock.base_url(), &dir, &[])))
        .collect();
    for one in &mut both {
        one.wait_for("the coordinator cannot be reached");
        one.line("say one");
        one.wait_for("re: one");
    }
    assert_eq!(coordinator_pid(&dir), None);

    std::fs::remove_file(coord_dir(&dir)).unwrap();
    wait_until("one of them to own the session", || record(&dir).is_some());
    let owner = record(&dir).unwrap()["pid"].as_u64().unwrap() as u32;
    let other = both.iter().position(|p| p.pid() != owner).unwrap();
    let mut other = both.remove(other);
    let mut owner = both.remove(0);
    // Said where the interface next looks: at its prompt.
    let told = |one: &mut Piped, what: &str| {
        wait_until(what, || {
            let answered = one.stdout().matches("re: more").count();
            one.line("say more");
            wait_until("an answer", || {
                one.stdout().matches("re: more").count() > answered
            });
            one.stderr().contains(what)
        });
    };
    told(
        &mut other,
        &format!("another atelier (pid {}) has a session", owner.pid()),
    );
    owner.line("say more");
    owner.wait_for("re: more");
    assert!(!owner.stderr().contains("has a session open"));
    assert!(alive(owner.pid()) && alive(other.pid()));

    // A newcomer is told of the one the coordinator has; and when that one
    // ends, the session is the other's.
    let (code, _, err) = run(&mock, &dir, &[], "");
    assert_eq!(code, EXIT_IN_USE, "{err}");
    assert!(err.contains(&format!("pid {}", owner.pid())), "{err}");
    assert_eq!(owner.finish(), 0);
    wait_owner(&dir, other.pid());
    told(&mut other, "the only one here again");
    assert_eq!(other.finish(), 0);
}

/// Of two started together in a free directory, one owns the session and
/// the other is refused.
#[test]
fn two_started_together_resolve_to_one_owner() {
    let mock = echo();
    for round in 0..5 {
        let dir = project(&format!("race{round}"));
        let mut both: Vec<Piped> = (0..2)
            .map(|_| Piped::spawn(atelier(&mock.base_url(), &dir, &[])))
            .collect();
        wait_until("one of them to be refused", || {
            both.iter_mut()
                .any(|p| p.child.try_wait().unwrap().is_some())
        });
        let refused = both
            .iter_mut()
            .position(|p| p.child.try_wait().unwrap().is_some())
            .unwrap();
        let mut refused = both.remove(refused);
        let mut owner = both.remove(0);
        assert_eq!(refused.exit(), EXIT_IN_USE, "{}", refused.stderr());
        assert!(refused.stderr().contains(&format!("pid {}", owner.pid())));
        assert_eq!(wait_owner(&dir, owner.pid())["pid"], owner.pid());
        owner.line("say hi");
        owner.wait_for("re: hi");
        assert_eq!(owner.finish(), 0);
    }
}

/// `--take-over`: the owner is asked, stops, and says why on its way out;
/// the newcomer continues the saved conversation.
#[test]
fn take_over_stops_an_idle_owner_and_continues_its_session() {
    let mock = echo();
    let dir = project("takeover");
    let mut owner = Piped::repl(&mock, &dir, &[]);
    owner.line("say one");
    owner.wait_for("re: one");
    wait_until("the session to be saved", || {
        state_dir(&dir).join("session.json").exists()
    });

    let mut next = Piped::spawn(atelier(&mock.base_url(), &dir, &["--take-over"]));
    assert_eq!(owner.exit(), 0, "it left by itself: {}", owner.stderr());
    let said = owner.stderr();
    assert!(
        said.contains(&format!(
            "another atelier (pid {}) took this session over",
            next.pid()
        )),
        "{said}"
    );
    let now = wait_owner(&dir, next.pid());
    assert_eq!(now["kind"], "repl");
    next.wait_for("resumed session (2 message(s))");
    let err = next.stderr();
    assert!(
        err.contains(&format!(
            "took over the session of pid {} (it stopped when asked)",
            owner.pid()
        )),
        "{err}"
    );
    next.line("say two");
    next.wait_for("re: two");
    // The conversation went on from what was saved.
    let body = mock.body(1);
    assert!(
        body.contains("say one") && body.contains("re: one"),
        "{body}"
    );
    assert_eq!(next.finish(), 0);
    assert!(ateliers_in(&dir).is_empty());
}

/// Two newcomers that both take over: one of them ends up the owner, and
/// the other is told or stopped — never two at once.
#[test]
fn two_take_overs_at_once_leave_one_owner() {
    let mock = echo();
    let dir = project("two-takers");
    let mut owner = Piped::repl(&mock, &dir, &[]);
    let mut takers: Vec<Piped> = (0..2)
        .map(|_| Piped::spawn(atelier(&mock.base_url(), &dir, &["--take-over"])))
        .collect();
    assert_eq!(owner.exit(), 0, "{}", owner.stderr());
    // One of the two leaves: refused, or taken over in its turn.
    wait_until("one taker to leave", || {
        takers
            .iter_mut()
            .any(|p| p.child.try_wait().unwrap().is_some())
    });
    let left = takers
        .iter_mut()
        .position(|p| p.child.try_wait().unwrap().is_some())
        .unwrap();
    let mut left = takers.remove(left);
    let mut winner = takers.remove(0);
    let code = left.exit();
    assert!(
        code == EXIT_IN_USE || code == 0,
        "{code}: {}",
        left.stderr()
    );
    wait_owner(&dir, winner.pid());
    winner.line("say hi");
    winner.wait_for("re: hi");
    assert_eq!(ateliers_in(&dir), vec![winner.pid()]);
    assert_eq!(winner.finish(), 0);
}

/// The hard case: the owner is in the middle of a turn, running a command
/// that will not end, with a background job going. It cannot stop in time;
/// the newcomer gets the session anyway, and nothing of the old one's is
/// left in the process table.
#[test]
fn take_over_kills_a_running_command_and_a_background_job() {
    let mock = Mock::scripted(|_, body| {
        if last_user(body).contains("after") {
            return answer("re: after");
        }
        match has_tool_result(body) {
            false => tool_calls(&[(
                "call_bg",
                "bash",
                json!({"command": "echo $$ > bg.pid; exec sleep 600", "background": true}),
            )]),
            true => tool_calls(&[(
                "call_fg",
                "bash",
                json!({"command": "sleep 600 & echo $! > child.pid; echo $$ > fg.pid; wait"}),
            )]),
        }
    });
    let dir = project("busy");
    let mut owner = Piped::repl(&mock, &dir, &[]);
    owner.line("work");
    let (bg, fg, child) = (
        pid_in(&dir.join("bg.pid")),
        pid_in(&dir.join("fg.pid")),
        pid_in(&dir.join("child.pid")),
    );
    assert!(alive(bg) && alive(fg) && alive(child));

    let (code, _, err) = run(&mock, &dir, &[], "");
    assert_eq!(code, EXIT_IN_USE);
    assert!(
        err.contains("now: a turn in progress · 1 background job"),
        "{err}"
    );

    let mut next = Piped::spawn(atelier(&mock.base_url(), &dir, &["--take-over"]));
    wait_owner(&dir, next.pid());
    owner.exit();
    assert!(
        owner.stderr().contains("took this session over"),
        "{}",
        owner.stderr()
    );
    wait_until("the old owner's processes to be gone", || {
        !alive(bg) && !alive(fg) && !alive(child)
    });
    next.wait_for("took over the session of pid");
    next.line("say after");
    next.wait_for("re: after");
    assert_eq!(ateliers_in(&dir), vec![next.pid()]);
    assert_eq!(next.finish(), 0);
}

/// `atelier coordinator <cmd>` for the coordinator in `coord`.
fn coordinator(coord: &Path, cmd: &str) -> String {
    let out = Command::new(BIN)
        .args(["coordinator", cmd])
        .env("ATELIER_COORDINATOR_DIR", coord)
        .output()
        .expect("run atelier coordinator");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A subagent that runs as a process of its own is the owner's too: it and
/// the command it runs are gone after a take-over. The coordinator both
/// were joined to is not the owner's to stop: it stays, forgets the old
/// instance and its subagent, and has the new owner under the same name.
#[test]
fn take_over_stops_a_process_subagent_and_takes_its_place_among_the_instances() {
    let mock = Mock::scripted(|_, body| {
        let request: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        let system = request["messages"][0]["content"]
            .as_str()
            .unwrap_or_default();
        if system.contains("general-purpose agent") {
            return tool_calls(&[(
                "call_sub",
                "bash",
                json!({"command": "echo $$ > sub.pid; sleep 600"}),
            )]);
        }
        if last_user(body).contains("after") {
            return answer("re: after");
        }
        tool_calls(&[(
            "call_task",
            "task",
            json!({"agent": "general", "prompt": "CHILD-TASK", "description": "wait",
                   "process": true}),
        )])
    });
    let dir = project("sub");
    let coord = coord_dir(&dir);
    let joined = |args: &[&str]| atelier(&mock.base_url(), &dir, args);
    let name = dir.file_name().unwrap().to_str().unwrap().to_string();
    let instances = || -> Vec<String> {
        let status = coordinator(&coord, "status");
        let lines = status.lines().skip(1);
        lines
            .filter(|l| l.contains(" — "))
            .map(str::to_string)
            .collect()
    };
    // The coordinator, less how long it has been up.
    let daemon = || -> String {
        let status = coordinator(&coord, "status");
        let first = status.lines().next().unwrap_or_default();
        first.split(", up").next().unwrap_or_default().to_string()
    };

    let mut owner = Piped::spawn(joined(&[]));
    wait_owner(&dir, owner.pid());
    owner.line("work");
    let command = pid_in(&dir.join("sub.pid"));
    let before = ateliers_in(&dir);
    assert_eq!(before.len(), 2, "the owner and its subagent: {before:?}");
    let subagent = *before.iter().find(|p| **p != owner.pid()).unwrap();
    wait_until("both to be instances", || instances().len() == 2);
    let was = daemon();
    assert!(was.contains("(pid "), "{was}");

    let mut next = Piped::spawn(joined(&["--take-over"]));
    wait_owner(&dir, next.pid());
    owner.exit();
    wait_until("the subagent and its command to be gone", || {
        !alive(subagent) && !alive(command)
    });
    next.line("say after");
    next.wait_for("re: after");
    assert_eq!(ateliers_in(&dir), vec![next.pid()]);
    // One instance, under the name the old owner had; the same coordinator.
    wait_until("the new owner to be the one instance", || {
        let now = instances();
        now.len() == 1 && now[0].starts_with(&format!("{name} — "))
    });
    assert_eq!(
        daemon(),
        was,
        "the coordinator was not the old owner's to stop"
    );
    assert_eq!(next.finish(), 0);
}

/// An owner that cannot be asked — stopped, here, so that it answers
/// nothing — is signalled, once it is certain to be the process the record
/// names.
#[test]
fn an_owner_that_does_not_answer_is_signalled() {
    let mock = echo();
    let dir = project("deaf");
    let mut owner = Piped::repl(&mock, &dir, &[]);
    Command::new("kill")
        .args(["-STOP", &owner.pid().to_string()])
        .status()
        .unwrap();
    let mut next = Piped::spawn(atelier(&mock.base_url(), &dir, &["--take-over"]));
    wait_owner(&dir, next.pid());
    assert_eq!(owner.exit(), -1, "ended by a signal");
    next.wait_for("took over the session of pid");
    let err = next.stderr();
    assert!(
        err.contains("was terminated") || err.contains("had to be killed"),
        "{err}"
    );
    assert_eq!(next.finish(), 0);
}

/// `serve`'s client is the one to ask: `initialize` fails with the owner
/// described and the choices named, `session/take_over` makes one, and the
/// session is then the client's, resumed.
#[test]
fn serve_puts_the_choice_to_its_client() {
    let mock = echo();
    let dir = project("serve");
    let mut owner = Piped::repl(&mock, &dir, &[]);
    owner.line("say one");
    owner.wait_for("re: one");
    wait_until("the session to be saved", || {
        state_dir(&dir).join("session.json").exists()
    });

    // Leaving is the other choice.
    let mut client = Client::spawn(&mock, &dir, &[]);
    let refused = client.call("initialize", json!({}));
    assert_eq!(refused["error"]["code"], SESSION_IN_USE, "{refused}");
    let data = &refused["error"]["data"];
    assert_eq!(data["owner"]["pid"], owner.pid());
    assert_eq!(data["owner"]["kind"], "repl");
    assert_eq!(data["owner"]["busy"], false);
    assert_eq!(data["choices"], json!(["take_over", "cancel"]));
    let refused = client.call("turn/start", json!({"text": "hi"}));
    assert_eq!(refused["error"]["code"], SESSION_IN_USE, "{refused}");
    client.result("shutdown", json!({}));
    assert_eq!(client.piped.exit(), EXIT_CANCELLED);
    assert_eq!(record(&dir).unwrap()["pid"], owner.pid());

    let mut client = Client::spawn(&mock, &dir, &[]);
    let refused = client.call("initialize", json!({}));
    assert_eq!(refused["error"]["code"], SESSION_IN_USE, "{refused}");
    let taken = client.result("session/take_over", json!({}));
    assert_eq!(taken["took_over"], true);
    assert_eq!(owner.exit(), 0);
    let hello = client.result(
        "initialize",
        json!({"client": {"name": "ui", "version": "1"}}),
    );
    assert_eq!(hello["protocol"], 1);
    let history = client.result("session/history", json!({}));
    assert_eq!(history["total"], 2, "{history}");
    assert_eq!(record(&dir).unwrap()["kind"], "serve");

    // And a served session is taken from its client in turn: it is told.
    let mut next = Piped::spawn(atelier(&mock.base_url(), &dir, &["--take-over"]));
    let closed = client.notification("session/closed");
    assert_eq!(closed["reason"], "taken_over");
    assert!(
        closed["by"]
            .as_str()
            .unwrap()
            .contains(&next.pid().to_string())
    );
    assert_eq!(client.piped.exit(), 0);
    wait_owner(&dir, next.pid());
    // Who was driving it is said to whoever asks next.
    let (code, _, err) = run(&mock, &dir, &[], "");
    assert_eq!(code, EXIT_IN_USE);
    assert!(err.contains("the line REPL"), "{err}");
    assert_eq!(next.finish(), 0);

    // With the flag there is nothing to ask.
    let mut owner = Piped::repl(&mock, &dir, &[]);
    let mut client = Client::spawn(&mock, &dir, &["--take-over"]);
    client.result(
        "initialize",
        json!({"client": {"name": "ui", "version": "1"}}),
    );
    assert_eq!(owner.exit(), 0);
    let (code, _, err) = run(&mock, &dir, &[], "");
    assert_eq!(code, EXIT_IN_USE);
    assert!(err.contains("atelier serve, driven by ui 1"), "{err}");
    assert!(err.contains("ends that program's session"), "{err}");
    client.result("shutdown", json!({}));
    assert_eq!(client.piped.exit(), 0);
}

/// `--print --continue --take-over`: a script's way to carry a session on,
/// whoever has it.
#[test]
fn print_continue_takes_over_with_the_flag() {
    let mock = echo();
    let dir = project("print");
    let mut owner = Piped::repl(&mock, &dir, &[]);
    owner.line("say one");
    owner.wait_for("re: one");
    wait_until("the session to be saved", || {
        state_dir(&dir).join("session.json").exists()
    });
    let (code, out, err) = run(
        &mock,
        &dir,
        &["--continue", "--print", "--take-over", "say two"],
        "",
    );
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("re: two"), "{out}");
    assert!(err.contains("took over the session of pid"), "{err}");
    assert_eq!(owner.exit(), 0);
    assert!(mock.body(1).contains("re: one"));
    assert!(ateliers_in(&dir).is_empty());
}

/// An editor's session cannot be asked either: `session/new` fails with the
/// owner named, unless the agent was started with `--take-over`.
#[test]
fn acp_refuses_a_directory_in_use_unless_told_to_take_over() {
    let mock = echo();
    let dir = project("acp");
    let mut owner = Piped::repl(&mock, &dir, &[]);
    let new_session = |args: &[&str]| {
        let mut all = vec!["acp"];
        all.extend_from_slice(args);
        let mut client = Client {
            piped: Piped::spawn(atelier(&mock.base_url(), &dir, &all)),
            seen: 0,
            next_id: 0,
        };
        client.result("initialize", json!({"protocolVersion": 1}));
        let answer = client.call("session/new", json!({"cwd": &*dir, "mcpServers": []}));
        (client, answer)
    };
    let (mut client, refused) = new_session(&[]);
    let message = refused["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("a session is already running"),
        "{refused}"
    );
    assert!(
        message.contains(&format!("pid {}", owner.pid())),
        "{refused}"
    );
    assert!(message.contains("atelier acp --take-over"), "{refused}");
    assert_eq!(client.piped.finish(), 0);
    assert_eq!(record(&dir).unwrap()["pid"], owner.pid());

    let (mut client, opened) = new_session(&["--take-over"]);
    assert!(opened["result"]["sessionId"].is_string(), "{opened}");
    assert_eq!(owner.exit(), 0);
    assert_eq!(record(&dir).unwrap()["kind"], "acp");
    assert_eq!(client.piped.finish(), 0);
}

/// One connection to a `serve --listen` socket: lines out, lines in.
struct Connection {
    stream: std::os::unix::net::UnixStream,
    reader: BufReader<std::os::unix::net::UnixStream>,
}

impl Connection {
    fn open(socket: &Path) -> Connection {
        let stream = std::os::unix::net::UnixStream::connect(socket).expect("connect");
        stream.set_read_timeout(Some(PATIENCE)).unwrap();
        let reader = BufReader::new(stream.try_clone().unwrap());
        Connection { stream, reader }
    }

    /// Send a request and return its answer, skipping notifications.
    fn call(&mut self, method: &str, params: Value) -> Value {
        let request = json!({"jsonrpc": "2.0", "id": method, "method": method, "params": params});
        writeln!(self.stream, "{request}").unwrap();
        loop {
            let mut line = String::new();
            assert!(
                self.reader.read_line(&mut line).unwrap() > 0,
                "closed before {method}'s answer"
            );
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["id"] == method {
                return message;
            }
        }
    }
}

/// A listener's connections are conversations of their own, several at
/// once: next to an owner they are not asked anything and leave its saved
/// session alone. Started with `--continue` they are about the saved
/// session, and the second one is told the first has it.
#[test]
fn listen_connections_own_the_session_only_with_continue() {
    let mock = echo();
    let dir = project("listen");
    let mut owner = Piped::repl(&mock, &dir, &[]);
    owner.line("say one");
    owner.wait_for("re: one");
    wait_until("the session to be saved", || {
        state_dir(&dir).join("session.json").exists()
    });
    let saved = std::fs::read(state_dir(&dir).join("session.json")).unwrap();

    let listen = |args: &[&str], name: &str| {
        let socket = dir.with_extension("run").join(name);
        let at = format!("unix:{}", socket.display());
        let mut all = vec!["serve", "--listen", &at];
        all.extend_from_slice(args);
        let listener = Piped::spawn(atelier(&mock.base_url(), &dir, &all));
        listener.wait_for("\"listening\"");
        (listener, socket)
    };
    let (_listener, socket) = listen(&[], "plain.sock");
    let (mut a, mut b) = (Connection::open(&socket), Connection::open(&socket));
    for c in [&mut a, &mut b] {
        let hello = c.call("initialize", json!({}));
        assert_eq!(hello["result"]["protocol"], 1, "{hello}");
    }
    let started = a.call("turn/start", json!({"text": "say aside"}));
    assert_eq!(started["result"]["turn"], 1, "{started}");
    wait_until("the turn to reach the model", || mock.requests() == 2);
    a.call("shutdown", json!({"force": true}));
    b.call("shutdown", json!({}));
    assert_eq!(
        std::fs::read(state_dir(&dir).join("session.json")).unwrap(),
        saved
    );
    assert_eq!(record(&dir).unwrap()["pid"], owner.pid());
    assert_eq!(owner.finish(), 0);

    let (_listener, socket) = listen(&["--continue"], "continue.sock");
    let (mut a, mut b) = (Connection::open(&socket), Connection::open(&socket));
    let hello = a.call("initialize", json!({}));
    assert_eq!(hello["result"]["protocol"], 1, "{hello}");
    let refused = b.call("initialize", json!({}));
    assert_eq!(refused["error"]["code"], SESSION_IN_USE, "{refused}");
    assert_eq!(refused["error"]["data"]["owner"]["kind"], "serve");
    let taken = b.call("session/take_over", json!({}));
    assert_eq!(taken["result"]["took_over"], true, "{taken}");
    let hello = b.call("initialize", json!({}));
    assert_eq!(hello["result"]["protocol"], 1, "{hello}");
    b.call("shutdown", json!({}));
}

// ------------------------------------------------ at a terminal

/// At a terminal the newcomer is told who has the session and asked, as
/// the `ask` tool asks: numbered lines, answered on the input line.
/// "Cancel" leaves everything as it was.
#[test]
fn the_question_is_asked_at_a_terminal_and_cancel_touches_nothing() {
    let mock = echo();
    let dir = project("ask");
    let mut owner = Term::owner(&mock, &dir, &[]);
    let before = record(&dir).unwrap();
    assert_eq!(before["kind"], "terminal");
    assert!(
        before["tty"].as_str().unwrap().starts_with("/dev/pts/"),
        "{before}"
    );

    let mut next = Term::start(&mock, &dir, &[]);
    next.wait_for("(answer with a number)");
    let asked = next.text();
    assert!(
        asked.contains("A session is already running in this directory:"),
        "{asked}"
    );
    assert!(
        asked.contains(&format!(
            "pid {} · the terminal interface on /dev/pts/",
            owner.pid()
        )),
        "{asked}"
    );
    assert!(asked.contains("now: idle"), "{asked}");
    assert!(asked.contains("? What do you want to do?"), "{asked}");
    assert!(asked.contains("Take over — stop that atelier"), "{asked}");
    assert!(
        asked.contains("3. Cancel — leave it alone and exit"),
        "{asked}"
    );
    // An answer that is none of the options is asked again.
    next.line("9");
    next.wait_for("is not one of the");
    next.line("cancel");
    assert_eq!(next.exit(), EXIT_CANCELLED);
    assert_eq!(record(&dir).unwrap(), before);
    assert_eq!(mock.requests(), 0);

    // Ctrl-C at the question is the same answer.
    let mut next = Term::start(&mock, &dir, &[]);
    next.wait_for("(answer with a number)");
    next.keys(b"\x03");
    assert_eq!(next.exit(), EXIT_CANCELLED);

    // The owner never noticed.
    owner.line("say one");
    owner.wait_for("re: one");
    owner.line("/quit");
    assert_eq!(owner.exit(), 0);
}

/// "Take over", answered at the question: the old terminal is left with a
/// line saying so and its shell back, and this one continues the session.
#[test]
fn take_over_from_the_question_leaves_the_old_terminal_a_line() {
    let mock = echo();
    let dir = project("ask-take");
    let mut owner = Term::owner(&mock, &dir, &[]);
    owner.line("say one");
    owner.wait_for("re: one");
    wait_until("the session to be saved", || {
        state_dir(&dir).join("session.json").exists()
    });

    let mut next = Term::start(&mock, &dir, &[]);
    next.wait_for("(answer with a number)");
    next.line("take over");
    assert_eq!(owner.exit(), 0);
    let left = owner.text();
    assert!(
        left.contains(&format!("another atelier (pid {} on /dev/pts/", next.pid())),
        "{left}"
    );
    assert!(left.contains("took this session over"), "{left}");
    next.wait_for("resumed session (2 message(s))");
    next.wait_for("/help for commands");
    wait_owner(&dir, next.pid());
    next.line("say two");
    next.wait_for("re: two");
    assert!(mock.body(1).contains("re: one"));
    next.line("/quit");
    assert_eq!(next.exit(), 0);
}

/// The owner ends while the question is on the screen: there is nothing
/// left to ask about, and the newcomer starts as on any other day.
#[test]
fn the_owner_ending_while_the_question_is_up_starts_normally() {
    let mock = echo();
    let dir = project("gone");
    let mut owner = Piped::repl(&mock, &dir, &[]);
    let mut next = Term::start(&mock, &dir, &[]);
    next.wait_for("(answer with a number)");
    assert_eq!(owner.finish(), 0);
    next.wait_for(&format!(
        "the other atelier (pid {}) has ended: starting normally",
        owner.pid()
    ));
    next.wait_for("/help for commands");
    wait_owner(&dir, next.pid());
    next.line("say hi");
    next.wait_for("re: hi");
    next.line("/quit");
    assert_eq!(next.exit(), 0);
}

/// A terminal interface in the middle of a turn that will not stop — the
/// model has not answered — is not waited for: it gives its terminal back,
/// says why, and ends.
#[test]
fn a_terminal_owner_stuck_in_a_turn_still_gives_its_terminal_back() {
    let release = Arc::new(AtomicBool::new(false));
    let held = Arc::clone(&release);
    let mock = Mock::start(Arc::new(move |_, body, stream| {
        if last_user(body).contains("after") {
            let _ = stream.write_all(&answer("re: after"));
            return;
        }
        // The model that never answers.
        while !held.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(20));
        }
    }));
    let dir = project("stuck");
    let mut owner = Term::owner(&mock, &dir, &[]);
    owner.line("think forever");
    wait_until("the request to reach the model", || mock.requests() == 1);

    let mut next = Piped::spawn(atelier(&mock.base_url(), &dir, &["--take-over"]));
    assert_eq!(owner.exit(), 0);
    let left = owner.text();
    assert!(
        left.contains("stopping: another atelier is taking this session over"),
        "{left}"
    );
    assert!(left.contains("took this session over"), "{left}");
    wait_owner(&dir, next.pid());
    next.line("say after");
    next.wait_for("re: after");
    release.store(true, Ordering::SeqCst);
    assert_eq!(next.finish(), 0);
    assert!(ateliers_in(&dir).is_empty());
}

// ============================================================ remotes

/// A front end attached to an owner's socket: the embedding protocol,
/// spoken by hand.
struct Remote {
    stream: std::os::unix::net::UnixStream,
    /// Every message received so far, in order.
    log: Arc<Mutex<Vec<Value>>>,
    /// Set once the owner closed the connection.
    closed: Arc<AtomicBool>,
    seen: usize,
    next_id: u64,
    /// Its number among the owner's remotes (`initialize`).
    number: u64,
}

impl Remote {
    /// Connect to the socket of the session's owner in `dir`.
    fn connect(dir: &Path) -> Remote {
        let socket = record(dir).expect("an owner")["socket"]
            .as_str()
            .expect("with a socket")
            .to_string();
        let stream = std::os::unix::net::UnixStream::connect(&socket).expect("connect");
        let (log, closed) = (
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(AtomicBool::new(false)),
        );
        let (sink, ended) = (Arc::clone(&log), Arc::clone(&closed));
        let reader = BufReader::new(stream.try_clone().unwrap());
        thread::spawn(move || {
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let message: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|e| panic!("not JSON ({e}): {line}"));
                sink.lock().unwrap().push(message);
            }
            ended.store(true, Ordering::SeqCst);
        });
        Remote {
            stream,
            log,
            closed,
            seen: 0,
            next_id: 100,
            number: 0,
        }
    }

    /// Connect and attach, as a client that shows a command's terminal
    /// (`terminal`) or one that does not.
    fn attach(dir: &Path, terminal: bool) -> Remote {
        let mut remote = Remote::connect(dir);
        let hello = remote.result(
            "initialize",
            json!({"client": {"name": "test", "version": "1"}, "terminal": terminal}),
        );
        assert_eq!(hello["protocol"], 1, "{hello}");
        assert_eq!(hello["capabilities"]["shared"], true, "{hello}");
        remote.number = hello["shared"]["remote"].as_u64().expect("a number");
        remote
    }

    fn send(&mut self, message: Value) {
        writeln!(self.stream, "{message}").expect("write to the owner");
    }

    /// The next message, from where the last look stopped, for which
    /// `want` holds; the others are skipped.
    fn next(&mut self, what: &str, want: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + PATIENCE;
        loop {
            {
                let log = self.log.lock().unwrap();
                while self.seen < log.len() {
                    let message = log[self.seen].clone();
                    self.seen += 1;
                    if want(&message) {
                        return message;
                    }
                }
            }
            assert!(
                !self.closed.load(Ordering::SeqCst) || self.seen < self.log.lock().unwrap().len(),
                "the owner closed the connection before {what}; received:\n{:#?}",
                self.log.lock().unwrap()
            );
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; received:\n{:#?}",
                self.log.lock().unwrap()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        // Answers are looked for from the start: one may have been passed
        // over while waiting for a notification.
        let deadline = Instant::now() + PATIENCE;
        loop {
            let log = self.log.lock().unwrap();
            let answer = log
                .iter()
                .find(|m| m["id"] == id && m.get("method").is_none());
            if let Some(answer) = answer {
                return answer.clone();
            }
            assert!(Instant::now() < deadline, "no answer to {method}: {log:#?}");
            drop(log);
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn result(&mut self, method: &str, params: Value) -> Value {
        let answer = self.call(method, params);
        assert!(answer.get("error").is_none(), "{method} failed: {answer}");
        answer["result"].clone()
    }

    /// The next notification `method`: its params.
    fn notification(&mut self, method: &str) -> Value {
        self.next(method, |m| m["method"] == method && m.get("id").is_none())["params"].clone()
    }

    /// The next request `method` the owner makes: the whole message.
    fn request(&mut self, method: &str) -> Value {
        self.next(method, |m| m["method"] == method && m.get("id").is_some())
    }

    /// Answer a request of the owner's.
    fn answer(&mut self, request: &Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": request["id"], "result": result}));
    }

    /// The text of the turn's answer: `event/text` up to `turn/ended`,
    /// whose params are returned with it.
    fn turn(&mut self) -> (String, Value) {
        let mut text = String::new();
        loop {
            let message = self.next("the turn's end", |m| {
                m["method"] == "event/text" || m["method"] == "turn/ended"
            });
            if message["method"] == "turn/ended" {
                return (text, message["params"].clone());
            }
            text.push_str(message["params"]["text"].as_str().unwrap_or_default());
        }
    }

    /// Wait for the owner to close the connection.
    fn closed(&self) {
        wait_until("the owner to close the connection", || {
            self.closed.load(Ordering::SeqCst)
        });
    }

    /// Whether a notification `method` was ever received.
    fn ever(&self, method: &str) -> bool {
        self.log
            .lock()
            .unwrap()
            .iter()
            .any(|m| m["method"] == method)
    }
}

impl Drop for Remote {
    /// Hang up: the reader's copy of the socket would keep it open.
    fn drop(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

/// Wait until the owner in `dir` says no turn is running: a turn's answer
/// is on the screen a moment before the turn is over.
fn wait_idle(dir: &Path) {
    let mut asking = Remote::connect(dir);
    wait_until("the owner to be idle", || {
        asking.result("owner/status", json!({}))["busy"] == false
    });
}

/// A `bash` call that says it started (`started`) and waits for the test
/// to let it go (`go`).
fn gated_bash(id: &str) -> Vec<u8> {
    tool_calls(&[(
        id,
        "bash",
        json!({"command": "echo > started; while [ ! -e go ]; do sleep 0.02; done; echo released"}),
    )])
}

/// A remote is a second front end of the owner's session: it is shown the
/// conversation so far, then what happens as it happens, whoever typed;
/// what it sends is served as if typed at the owner's terminal, where it is
/// marked as the remote's; and its leaving ends nothing.
#[test]
fn a_remote_sees_the_conversation_and_drives_the_session() {
    let mock = echo();
    let dir = project("remote");
    let mut owner = Term::owner(&mock, &dir, &[]);
    owner.line("say one");
    owner.wait_for("re: one");
    wait_idle(&dir);

    let mut remote = Remote::connect(&dir);
    // Before it has said who it is, it is nobody.
    let early = remote.call("turn/start", json!({"text": "hi"}));
    assert_eq!(early["error"]["code"], -32002, "{early}");
    let hello = remote.result(
        "initialize",
        json!({"client": {"name": "test", "version": "1"}}),
    );
    assert_eq!(hello["session"]["model"], "mock");
    assert_eq!(hello["session"]["cwd"], dir.display().to_string());
    assert!(
        hello["session"]["tools"]
            .as_array()
            .is_some_and(|t| !t.is_empty()),
        "{hello}"
    );
    assert_eq!(hello["shared"]["owner"]["pid"], owner.pid());
    assert_eq!(hello["shared"]["owner"]["kind"], "terminal");
    assert_eq!(hello["shared"]["busy"], false);
    let number = hello["shared"]["remote"].as_u64().unwrap();
    owner.wait_for(&format!("· remote {number} attached (test 1)"));
    let twice = remote.call("initialize", json!({}));
    assert_eq!(twice["error"]["code"], -32005, "{twice}");

    // The conversation so far.
    let history = remote.result("session/history", json!({}));
    assert_eq!(history["total"], 2, "{history}");
    assert_eq!(history["items"][0]["text"], "say one");
    assert_eq!(history["items"][1]["text"], "re: one");

    // A line of the remote's: a turn in the owner's session.
    let ran = remote.result("command/run", json!({"line": "say two", "start": true}));
    assert_eq!(ran["kind"], "prompt");
    assert_eq!(ran["handled"], true);
    assert_eq!(ran["turn"], 2, "the session's second turn: {ran}");
    let started = remote.notification("turn/started");
    assert_eq!(
        started,
        json!({"turn": 2, "text": "say two", "from": format!("remote-{number}")})
    );
    let (text, ended) = remote.turn();
    assert_eq!(text, "re: two");
    assert_eq!(ended["turn"], 2);
    assert_eq!(ended["status"], "completed");
    owner.wait_for(&format!("› say two  ← remote {number}"));
    owner.wait_for("re: two");
    assert!(mock.body(1).contains("re: one"), "one conversation");

    // A line typed at the owner's terminal: the remote sees it too.
    owner.line("say three");
    let started = remote.notification("turn/started");
    assert_eq!(
        started,
        json!({"turn": 3, "text": "say three", "from": "terminal"})
    );
    let (text, ended) = remote.turn();
    assert_eq!(
        (text.as_str(), &ended["status"]),
        ("re: three", &json!("completed"))
    );
    let history = remote.result("session/history", json!({}));
    assert_eq!(history["total"], 6, "{history}");

    // `turn/start`, as `serve` has it; and what a line is, without sending it.
    let started = remote.result("turn/start", json!({"text": "say four"}));
    assert_eq!(started, json!({"turn": 4}));
    assert_eq!(remote.turn().0, "re: four");
    let asked = remote.result("command/run", json!({"line": "say five"}));
    assert_eq!(
        asked,
        json!({"handled": false, "kind": "prompt", "text": "say five", "model": null})
    );
    assert_eq!(mock.requests(), 4);

    // Settings live in the owner's process and are changed there.
    let ran = remote.result("command/run", json!({"line": "/mode plan"}));
    assert_eq!(ran, json!({"handled": true, "kind": "handled"}));
    assert_eq!(
        remote.notification("event/input"),
        json!({"text": "/mode plan", "from": format!("remote-{number}")})
    );
    assert!(
        remote.notification("event/info")["text"]
            .as_str()
            .unwrap()
            .contains("mode: plan")
    );
    // What the session is has changed, and every remote is told.
    assert_eq!(remote.notification("session/updated")["mode"], "plan");
    let status = remote.result("session/status", json!({}));
    assert_eq!(status["mode"], "plan");
    assert_eq!(
        (&status["busy"], &status["remotes"]),
        (&json!(false), &json!(1))
    );
    owner.wait_for("· plan");
    // What belongs to the owner's terminal is said to.
    assert_eq!(
        remote.result("command/run", json!({"line": "/config"}))["kind"],
        "config"
    );
    assert_eq!(
        remote.result("command/run", json!({"line": "/quit"}))["kind"],
        "quit"
    );
    // What needs the session is answered from it.
    let usage = remote.result("usage/get", json!({}));
    assert_eq!(usage["total"]["requests"], 4, "{usage}");
    let settings = remote.result("settings/get", json!({}));
    assert!(
        settings["settings"]
            .as_array()
            .is_some_and(|s| !s.is_empty())
    );
    let unknown = remote.call("no/such", json!({}));
    assert_eq!(unknown["error"]["code"], -32601, "{unknown}");

    // Leaving ends the connection and nothing else.
    remote.result("shutdown", json!({}));
    remote.closed();
    owner.wait_for(&format!("· remote {number} detached"));
    owner.line("/mode auto-edit");
    owner.line("say six");
    owner.wait_for("re: six");
    assert_eq!(record(&dir).unwrap()["pid"], owner.pid());
    owner.line("/quit");
    assert_eq!(owner.exit(), 0);
}

/// An approval is put to the owner's terminal and to every remote, and
/// whoever answers first has answered: the others see it answered, and by
/// whom. A remote that attaches while one is waiting is asked too.
#[test]
fn an_approval_is_put_to_all_and_answered_by_the_first() {
    let mock = Mock::scripted(|_, body| match after_tool(body) {
        false => tool_calls(&[("call_1", "bash", json!({"command": "echo ran > ran.txt"}))]),
        true => answer("done"),
    });
    let dir = project("approve");
    let mut cmd = atelier(&mock.base_url(), &dir, &[]);
    cmd.env_remove("ATELIER_APPROVE");
    let mut owner = Term::spawn(cmd);
    wait_owner(&dir, owner.pid());
    owner.wait_for("/help for commands");
    let mut a = Remote::attach(&dir, false);

    // Answered at a remote.
    owner.line("first");
    owner.wait_for("allow tool 'bash'?");
    let asked = a.request("approval/request");
    assert_eq!(asked["params"]["tool"], "bash");
    assert_eq!(
        asked["params"]["arguments"]["command"],
        "echo ran > ran.txt"
    );
    assert_eq!(asked["params"]["turn"], 1);
    // One that comes while it waits is brought up to date, and asked.
    let mut b = Remote::attach(&dir, false);
    assert_eq!(b.notification("turn/started")["text"], "first");
    let also = b.request("approval/request");
    assert_eq!(also["id"], asked["id"]);
    // An answer that is none leaves the question up.
    a.answer(&asked, json!({"decision": "maybe"}));
    a.answer(&asked, json!({"decision": "once"}));
    let told = b.notification("request/cancelled");
    assert_eq!(
        told,
        json!({"id": asked["id"], "reason": "answered", "by": format!("remote-{}", a.number),
               "decision": "once"})
    );
    owner.wait_for(&format!("↳ allowed once at remote {}", a.number));
    assert_eq!(a.turn().0, "done");
    assert_eq!(
        std::fs::read_to_string(dir.join("ran.txt")).unwrap(),
        "ran\n"
    );
    // The late one saw the rest of the turn as well.
    assert_eq!(b.turn().1["status"], "completed");

    // Answered at the owner's terminal: the remotes are told.
    std::fs::remove_file(dir.join("ran.txt")).unwrap();
    owner.line("second");
    let asked = a.request("approval/request");
    b.request("approval/request");
    // (Typed once the prompt is up, as a person would.)
    owner.wait_times("allow tool 'bash'?", 2);
    owner.keys(b"n");
    for remote in [&mut a, &mut b] {
        let told = remote.notification("request/cancelled");
        assert_eq!(
            told,
            json!({"id": asked["id"], "reason": "answered", "by": "terminal", "decision": "deny"})
        );
    }
    a.turn();
    assert!(!dir.join("ran.txt").exists(), "denied");
    // An answer that comes after is nobody's.
    b.answer(&asked, json!({"decision": "once"}));
    owner.line("/quit");
    assert_eq!(owner.exit(), 0);
    assert!(!dir.join("ran.txt").exists());
}

/// A question of the model's goes the same way.
#[test]
fn a_question_is_answered_at_a_remote_or_at_the_terminal() {
    let mock = Mock::scripted(|_, body| match after_tool(body) {
        false => tool_calls(&[(
            "call_1",
            "ask",
            json!({"question": "Which database?", "options": ["SQLite", "Postgres"]}),
        )]),
        true => answer("noted"),
    });
    let dir = project("question");
    let mut owner = Term::owner(&mock, &dir, &[]);
    let mut a = Remote::attach(&dir, false);
    let mut b = Remote::attach(&dir, false);

    owner.line("first");
    owner.wait_for("? Which database?");
    let asked = a.request("question/request");
    assert_eq!(asked["params"]["question"], "Which database?");
    a.answer(&asked, json!({"labels": ["Postgres"]}));
    let told = b.notification("request/cancelled");
    assert_eq!(told["by"], format!("remote-{}", a.number));
    assert_eq!(told["answer"], json!({"labels": ["Postgres"]}));
    owner.wait_for(&format!("↳ answered at remote {}: Postgres", a.number));
    assert_eq!(a.turn().0, "noted");
    assert!(mock.body(1).contains("Postgres"), "{}", mock.body(1));

    owner.line("second");
    let asked = b.request("question/request");
    owner.wait_times("? Which database?", 2);
    owner.line("1");
    let told = b.notification("request/cancelled");
    assert_eq!(
        told,
        json!({"id": asked["id"], "reason": "answered", "by": "terminal",
               "answer": {"labels": ["SQLite"]}})
    );
    b.turn();
    owner.line("/quit");
    assert_eq!(owner.exit(), 0);
}

/// A remote steers the running turn and stops it, as the keyboard does.
#[test]
fn a_remote_steers_and_interrupts_the_turn() {
    let mock = Mock::scripted(|n, body| {
        if last_user(body).contains("hold") && !after_tool(body) {
            return gated_bash(&format!("call_{n}"));
        }
        answer(&format!(
            "heard: {}",
            last_user(body).split_whitespace().last().unwrap_or("")
        ))
    });
    let dir = project("steer");
    let mut owner = Term::owner(&mock, &dir, &[]);
    let mut remote = Remote::attach(&dir, false);

    // Not without a turn.
    let none = remote.call("turn/steer", json!({"text": "x"}));
    assert_eq!(none["error"]["code"], -32003, "{none}");
    let none = remote.call("turn/interrupt", json!({}));
    assert_eq!(none["error"]["code"], -32003, "{none}");

    owner.line("hold");
    owner.wait_for("⚙ bash");
    wait_until("the command to start", || dir.join("started").exists());
    // The session is the turn's: what needs it waits or is refused.
    let busy = remote.call("command/run", json!({"line": "/help"}));
    assert_eq!(busy["error"]["code"], -32001, "{busy}");
    assert_eq!(busy["error"]["data"]["turn"], 1);
    let busy = remote.call("turn/start", json!({"text": "x"}));
    assert_eq!(busy["error"]["code"], -32001, "{busy}");
    let status = remote.result("session/status", json!({}));
    assert_eq!(
        (&status["busy"], &status["turn"]),
        (&json!(true), &json!(1))
    );
    let steered = remote.result("turn/steer", json!({"text": "and then lemons"}));
    assert_eq!(steered, json!({"turn": 1, "queued": 1}));
    owner.wait_for(&format!(
        "› and then lemons  (queued · remote {})",
        remote.number
    ));
    std::fs::write(dir.join("go"), "").unwrap();
    assert_eq!(
        remote.notification("event/delivered")["text"],
        "and then lemons"
    );
    let (text, ended) = remote.turn();
    assert_eq!(text, "heard: lemons");
    assert_eq!(ended["status"], "completed");

    // Stopped from the remote: the command on the owner's terminal is
    // killed, and the turn ends interrupted.
    std::fs::remove_file(dir.join("go")).unwrap();
    std::fs::remove_file(dir.join("started")).unwrap();
    let started = remote.result("turn/start", json!({"text": "hold"}));
    wait_until("the command to start", || dir.join("started").exists());
    let stopped = remote.result("turn/interrupt", json!({}));
    assert_eq!(stopped["turn"], started["turn"]);
    let (_, ended) = remote.turn();
    assert_eq!(ended["status"], "interrupted", "{ended}");
    owner.wait_for(&format!("! stopping… (asked at remote {})", remote.number));
    owner.line("/quit");
    assert_eq!(owner.exit(), 0);
}

/// A remote that goes away in the middle of the turn it started takes
/// nothing with it: the turn runs to its end, and is saved.
#[test]
fn a_remote_leaving_mid_turn_does_not_stop_the_turn() {
    let mock = Mock::scripted(|n, body| match has_tool_result(body) {
        false => gated_bash(&format!("call_{n}")),
        true => answer("finished without you"),
    });
    let dir = project("leave");
    let mut owner = Term::owner(&mock, &dir, &[]);
    let mut remote = Remote::attach(&dir, false);
    let number = remote.number;
    remote.result("turn/start", json!({"text": "work"}));
    wait_until("the command to start", || dir.join("started").exists());
    drop(remote);
    owner.wait_for(&format!("· remote {number} detached"));
    std::fs::write(dir.join("go"), "").unwrap();
    owner.wait_for("finished without you");
    wait_until("the turn to be saved", || {
        std::fs::read_to_string(state_dir(&dir).join("session.json"))
            .is_ok_and(|s| s.contains("finished without you"))
    });
    // And the next one is brought up to date from what was saved.
    let mut again = Remote::attach(&dir, false);
    // (Attached while the turn was still closing, it is sent the turn as
    // events and the history as of before it: the turn is in the history
    // once it has ended.)
    wait_until("the session to be idle", || {
        again.result("session/status", json!({}))["busy"] == false
    });
    let history = again.result("session/history", json!({}));
    let last = history["items"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["text"], "finished without you");
    owner.line("/quit");
    assert_eq!(owner.exit(), 0);
}

/// However the owner ends, its remotes find out: told when it could tell
/// (a quit, a take-over), cut off when it could not.
#[test]
fn remotes_learn_that_the_owner_is_gone() {
    let mock = echo();
    let dir = project("gone-owner");

    let mut owner = Term::owner(&mock, &dir, &[]);
    let mut remote = Remote::attach(&dir, false);
    owner.line("/quit");
    assert_eq!(
        remote.notification("session/closed"),
        json!({"reason": "ended"})
    );
    remote.closed();
    assert_eq!(owner.exit(), 0);

    let mut owner = Term::owner(&mock, &dir, &[]);
    let mut remote = Remote::attach(&dir, false);
    let mut next = Piped::spawn(atelier(&mock.base_url(), &dir, &["--take-over"]));
    let closed = remote.notification("session/closed");
    assert_eq!(closed["reason"], "taken_over");
    assert!(
        closed["by"]
            .as_str()
            .unwrap()
            .contains(&next.pid().to_string()),
        "{closed}"
    );
    remote.closed();
    assert_eq!(owner.exit(), 0);
    wait_owner(&dir, next.pid());
    // The line REPL that took it takes no remote, and says so.
    let mut refused = Remote::connect(&dir);
    let no = refused.call("initialize", json!({}));
    assert_eq!(no["error"]["code"], -32011, "{no}");
    assert!(
        no["error"]["message"]
            .as_str()
            .unwrap()
            .contains("the line REPL"),
        "{no}"
    );
    assert_eq!(next.finish(), 0);

    let mut owner = Term::owner(&mock, &dir, &[]);
    let remote = Remote::attach(&dir, false);
    Command::new("kill")
        .args(["-9", &owner.pid().to_string()])
        .status()
        .unwrap();
    remote.closed();
    assert!(!remote.ever("session/closed"));
    assert_eq!(owner.exit(), -1);
}

/// A command that stops to ask is typed to from a remote that shows its
/// terminal. What is typed goes to the command and nowhere else: the
/// owner's terminal and the model see the command's output without it.
#[test]
fn a_command_is_typed_to_from_a_remote() {
    let mock = Mock::scripted(|_, body| match has_tool_result(body) {
        false => tool_calls(&[(
            "call_1",
            "bash",
            json!({"command": "printf 'Code: '; read -r code; echo \"got ${#code} characters\"",
                   "interactive": true}),
        )]),
        true => answer("thanks"),
    });
    let dir = project("typed");
    let mut owner = Term::owner(&mock, &dir, &[]);
    // Something on the input line: the keyboard is not handed over by
    // itself, and stays the owner's.
    owner.keys(b"x");
    let mut typist = Remote::attach(&dir, true);
    let mut watcher = Remote::attach(&dir, true);
    let plain = Remote::attach(&dir, false);

    typist.result("turn/start", json!({"text": "ask me"}));
    let started = typist.notification("event/command_started");
    assert_eq!(started["interactive"], true);
    let mut shown = String::new();
    while !shown.contains("Code: ") {
        shown.push_str(
            typist.notification("event/command_output")["data"]
                .as_str()
                .unwrap(),
        );
    }
    typist.result(
        "command/input",
        json!({"attached": true, "cols": 80, "rows": 24}),
    );
    typist.result("command/input", json!({"data": "sesame-4242\r"}));
    typist.notification("event/command_ended");
    let (text, ended) = typist.turn();
    assert_eq!(
        (text.as_str(), &ended["status"]),
        ("thanks", &json!("completed"))
    );
    // Once it is over there is nothing to type to.
    let late = typist.call("command/input", json!({"data": "x"}));
    assert_eq!(late["error"]["code"], -32006, "{late}");

    // The command ran with what was typed.
    let result = mock.body(1);
    assert!(result.contains("got 11 characters"), "{result}");
    // Nobody was sent it but the command: not the model, not the owner's
    // terminal, and the other remote only as the terminal's own echo.
    assert!(!result.contains("sesame-4242"), "{result}");
    owner.wait_for("thanks");
    assert!(!owner.text().contains("sesame-4242"), "{}", owner.text());
    watcher.notification("event/command_ended");
    let all = |remote: &Remote| -> String {
        let log = remote.log.lock().unwrap();
        log.iter()
            .filter(|m| m["method"] != "event/command_output")
            .map(Value::to_string)
            .collect()
    };
    assert!(!all(&typist).contains("sesame-4242"));
    assert!(!all(&watcher).contains("sesame-4242"));
    // A remote that did not ask for the terminal is shown none of it.
    wait_until("the turn to end for all", || plain.ever("turn/ended"));
    assert!(!plain.ever("event/command_started") && !plain.ever("event/command_output"));
    let saved = std::fs::read_to_string(state_dir(&dir).join("session.json")).unwrap();
    assert!(!saved.contains("sesame-4242"), "{saved}");
    owner.keys(b"\x7f");
    owner.line("/quit");
    assert_eq!(owner.exit(), 0);
}

// ================================================== remote control

/// The first choice at the question: this terminal becomes a second front
/// end of the session. It is shown the conversation so far, then what
/// happens; each terminal marks what the other typed; settings are changed
/// in the owner; leaving ends nothing; and the remote is nobody among the
/// coordinator's instances.
#[test]
fn remote_control_makes_a_second_terminal_of_the_same_session() {
    let mock = echo();
    let dir = project("control");
    let coord = coord_dir(&dir);
    let joined = |args: &[&str]| atelier(&mock.base_url(), &dir, args);
    let instances = || -> usize {
        let status = coordinator(&coord, "status");
        status.lines().skip(1).filter(|l| l.contains(" — ")).count()
    };
    let mut owner = Term::spawn(joined(&[]));
    wait_owner(&dir, owner.pid());
    owner.wait_for("/help for commands");
    owner.line("say one");
    owner.wait_for("re: one");
    wait_idle(&dir);
    wait_until("the owner to be an instance", || instances() == 1);

    let mut remote = Term::spawn(joined(&[]));
    remote.wait_for("(answer with a number)");
    let asked = remote.text();
    assert!(
        asked.contains("1. Remote control — this terminal becomes a second front end"),
        "{asked}"
    );
    assert!(
        asked.contains("2. Take over") && asked.contains("3. Cancel"),
        "{asked}"
    );
    remote.line("1");
    remote.wait_for(&format!(
        "remote control of the session of pid {} on /dev/pts/",
        owner.pid()
    ));
    // The conversation so far.
    remote.wait_for("── the conversation so far ──");
    remote.wait_for("› say one");
    remote.wait_for("re: one");
    remote.wait_for("── now ──");
    remote.wait_for(&format!(" remote of pid {}", owner.pid()));
    owner.wait_for("attached (atelier ");

    // Typed at the remote: served by the owner, shown at both.
    remote.line("say two");
    remote.wait_for("re: two");
    owner.wait_for("› say two  ← remote ");
    owner.wait_for("re: two");
    // Typed at the owner: shown at the remote, as the other terminal's.
    owner.line("say three");
    remote.wait_for("› say three  ← the owner's terminal");
    remote.wait_for("re: three");
    assert_eq!(mock.requests(), 3);
    assert!(mock.body(2).contains("re: two"), "one conversation");

    // A setting is changed where it lives, and both strips show it.
    remote.line("/mode plan");
    remote.wait_for("mode: plan");
    remote.wait_for("· plan");
    owner.wait_for("› /mode plan  ← remote ");
    owner.wait_for("· plan");
    // A command typed at the owner's terminal is shown at the remote with
    // its output, as the other terminal's.
    owner.line("/mode ask-edits");
    remote.wait_for("› /mode ask-edits  ← the owner's terminal");
    remote.wait_for("mode: ask-edits");
    remote.wait_for("· ask-edits");
    // What cannot be done from here says so.
    remote.line("/config");
    remote.wait_for("the settings screen is the owner terminal's");

    // One owner, one instance: the remote is nobody among them.
    assert_eq!(instances(), 1);
    assert_eq!(record(&dir).unwrap()["pid"], owner.pid());
    assert_eq!(ateliers_in(&dir).len(), 2);

    // Leaving ends the remote, and nothing else.
    remote.line("/quit");
    assert_eq!(remote.exit(), 0);
    owner.wait_for("detached");
    owner.line("/mode auto-edit");
    owner.line("say four");
    owner.wait_for("re: four");
    // Ctrl-D leaves too.
    let mut remote = Term::spawn(joined(&["--attach"]));
    remote.wait_for("re: four");
    remote.keys(b"\x04");
    assert_eq!(remote.exit(), 0);
    owner.wait_times("detached", 2);
    owner.line("/quit");
    assert_eq!(owner.exit(), 0);
    coordinator(&coord, "stop");
}

/// `--attach`: the same without the question. From the remote terminal a
/// turn is steered and stopped and an approval answered — and one answered
/// at the owner's terminal is taken down at the remote.
#[test]
fn a_remote_terminal_answers_steers_and_stops() {
    let mock = Mock::scripted(|n, body| {
        let said = last_user(body);
        if after_tool(body) {
            return answer("done");
        }
        if said.contains("hold") {
            return gated_bash(&format!("call_{n}"));
        }
        if said.contains("run") {
            return tool_calls(&[("call_1", "bash", json!({"command": "echo ran >> ran.txt"}))]);
        }
        answer(&format!(
            "heard: {}",
            said.split_whitespace().last().unwrap_or("")
        ))
    });
    let dir = project("answers");
    let mut cmd = atelier(&mock.base_url(), &dir, &[]);
    cmd.env_remove("ATELIER_APPROVE");
    let mut owner = Term::spawn(cmd);
    wait_owner(&dir, owner.pid());
    owner.wait_for("/help for commands");
    let mut remote = Term::start(&mock, &dir, &["--attach"]);
    remote.wait_for("remote control of the session");

    // Asked of both; answered at the remote.
    owner.line("run it");
    owner.wait_for("allow tool 'bash'?");
    remote.wait_for("› run it  ← the owner's terminal");
    remote.wait_for("allow tool 'bash'?");
    remote.wait_for("approval needed [y/a/n]");
    remote.keys(b"y");
    owner.wait_for("↳ allowed once at remote ");
    remote.wait_for("done");
    assert_eq!(
        std::fs::read_to_string(dir.join("ran.txt")).unwrap(),
        "ran\n"
    );

    // Asked from the remote; answered at the owner's terminal.
    remote.line("run it again");
    remote.wait_times("allow tool 'bash'?", 2);
    owner.wait_times("allow tool 'bash'?", 2);
    owner.keys(b"n");
    remote.wait_for("↳ denied at the owner's terminal");
    remote.wait_times("done", 2);
    assert_eq!(
        std::fs::read_to_string(dir.join("ran.txt")).unwrap(),
        "ran\n"
    );

    // A line typed at the remote during a turn joins it.
    remote.line("/mode yolo");
    remote.wait_for("· yolo");
    remote.line("hold");
    wait_until("the command to start", || dir.join("started").exists());
    remote.wait_times("echo > started", 1);
    remote.line("and lemons");
    remote.wait_for("› and lemons  (queued)");
    owner.wait_for("› and lemons  (queued · remote ");
    // A command waits for the turn to end, and is served then.
    remote.line("/help");
    remote.wait_for("› /help  (queued)");
    assert!(!remote.text().contains("show this help"));
    std::fs::write(dir.join("go"), "").unwrap();
    remote.wait_for("heard: lemons");
    owner.wait_for("heard: lemons");
    remote.wait_for("show this help");

    // Ctrl-C at the remote stops the turn.
    std::fs::remove_file(dir.join("go")).unwrap();
    std::fs::remove_file(dir.join("started")).unwrap();
    remote.line("hold");
    wait_until("the command to start", || dir.join("started").exists());
    remote.wait_times("echo > started", 2);
    remote.keys(b"\x03");
    remote.wait_for("! stopping…");
    owner.wait_for("! stopping… (asked at remote ");
    // Both are back at their prompts: the next line is a turn of its own.
    owner.line("say after");
    owner.wait_for("heard: after");
    remote.wait_for("heard: after");
    remote.line("/quit");
    assert_eq!(remote.exit(), 0);
    owner.line("/quit");
    assert_eq!(owner.exit(), 0);
}

/// A command that asks for something is typed to from the remote terminal
/// (Ctrl-T), and what is typed there is not shown at the owner's.
#[test]
fn a_remote_terminal_types_to_a_command() {
    let mock = Mock::scripted(|_, body| match after_tool(body) {
        false => tool_calls(&[(
            "call_1",
            "bash",
            json!({"command": "printf 'Code: '; read -r code; echo \"got ${#code} characters\""}),
        )]),
        true => answer("thanks"),
    });
    let dir = project("ctrl-t");
    let mut owner = Term::owner(&mock, &dir, &[]);
    let mut remote = Term::start(&mock, &dir, &["--attach"]);
    remote.wait_for("remote control of the session");
    remote.line("ask me");
    remote.wait_for("⌨ the command is waiting for input: Code:");
    owner.wait_for("⌨ the command is waiting for input: Code:");
    remote.keys(b"\x14");
    remote.wait_for("your keys now go to the command");
    remote.keys(b"sesame-4242\r");
    remote.wait_for("thanks");
    owner.wait_for("thanks");
    assert!(
        mock.body(1).contains("got 11 characters"),
        "{}",
        mock.body(1)
    );
    assert!(!mock.body(1).contains("sesame-4242"));
    assert!(!owner.text().contains("sesame-4242"), "{}", owner.text());
    remote.line("/quit");
    assert_eq!(remote.exit(), 0);
    owner.line("/quit");
    assert_eq!(owner.exit(), 0);
}

/// A remote terminal learns how the owner ended, says so, and exits; and a
/// remote terminal that dies in the middle of a turn takes nothing with it.
#[test]
fn a_remote_terminal_outlives_nothing_and_ends_nothing() {
    let mock = Mock::scripted(|n, body| {
        if after_tool(body) {
            return answer("finished anyway");
        }
        match last_user(body).contains("hold") {
            true => gated_bash(&format!("call_{n}")),
            false => answer("re"),
        }
    });
    let dir = project("ends");

    // The remote dies mid-turn: the turn goes on.
    let mut owner = Term::owner(&mock, &dir, &[]);
    let mut remote = Term::start(&mock, &dir, &["--attach"]);
    remote.wait_for("remote control of the session");
    remote.line("hold");
    wait_until("the command to start", || dir.join("started").exists());
    Command::new("kill")
        .args(["-9", &remote.pid().to_string()])
        .status()
        .unwrap();
    assert_eq!(remote.exit(), -1);
    owner.wait_for("detached");
    std::fs::write(dir.join("go"), "").unwrap();
    owner.wait_for("finished anyway");

    // The owner quits: the remote says so and ends.
    let mut remote = Term::start(&mock, &dir, &["--attach"]);
    remote.wait_for("finished anyway");
    owner.line("/quit");
    assert_eq!(owner.exit(), 0);
    remote.wait_for("has quit: this remote has ended");
    remote.wait_for("atelier --continue");
    assert_eq!(remote.exit(), 0);

    // The owner is taken over: the remote says by whom.
    let mut owner = Term::owner(&mock, &dir, &[]);
    let mut remote = Term::start(&mock, &dir, &["--attach"]);
    remote.wait_for("remote control of the session");
    let mut taker = Piped::spawn(atelier(&mock.base_url(), &dir, &["--take-over"]));
    remote.wait_for(&format!(
        "another atelier (pid {}) took the session over",
        taker.pid()
    ));
    assert_eq!(remote.exit(), 0);
    assert_eq!(owner.exit(), 0);
    wait_owner(&dir, taker.pid());
    assert_eq!(taker.finish(), 0);

    // The owner is killed: the remote notices, and says that too.
    let mut owner = Term::owner(&mock, &dir, &[]);
    let mut remote = Term::start(&mock, &dir, &["--attach"]);
    remote.wait_for("remote control of the session");
    Command::new("kill")
        .args(["-9", &owner.pid().to_string()])
        .status()
        .unwrap();
    remote.wait_for("is gone without a word");
    assert_eq!(remote.exit(), 0);
    assert_eq!(owner.exit(), -1);
    assert!(ateliers_in(&dir).is_empty());
}

/// `--attach` where it cannot be had: an owner that takes no remote is
/// left alone, with the reason; and with no session running there is
/// nothing to attach to, so the run starts as it would without the flag.
#[test]
fn attach_needs_an_owner_that_takes_remotes() {
    let mock = echo();
    let dir = project("no-remote");
    let mut owner = Piped::repl(&mock, &dir, &[]);
    let mut remote = Term::start(&mock, &dir, &["--attach"]);
    assert_eq!(remote.exit(), EXIT_IN_USE);
    let said = remote.text();
    assert!(
        said.contains("it takes no remote: only a terminal interface does"),
        "{said}"
    );
    assert!(said.contains(&format!("pid {}", owner.pid())), "{said}");
    // The question does not offer it either, and says why.
    let mut asking = Term::start(&mock, &dir, &[]);
    asking.wait_for("(answer with a number)");
    let asked = asking.text();
    assert!(!asked.contains("Remote control —"), "{asked}");
    assert!(
        asked.contains("1. Take over") && asked.contains("2. Cancel"),
        "{asked}"
    );
    assert!(
        asked.contains("(remote control is not offered: only a terminal interface takes remotes)"),
        "{asked}"
    );
    asking.line("2");
    assert_eq!(asking.exit(), EXIT_CANCELLED);
    // `serve --attach` is told the same.
    let (code, out, err) = run(&mock, &dir, &["serve", "--attach"], "");
    assert_eq!(code, EXIT_IN_USE, "{err}");
    assert!(
        out.is_empty() && err.contains("it takes no remote"),
        "{err}"
    );
    assert_eq!(owner.finish(), 0);

    let mut alone = Term::start(&mock, &dir, &["--attach"]);
    alone.wait_for("/help for commands");
    wait_owner(&dir, alone.pid());
    alone.line("/quit");
    assert_eq!(alone.exit(), 0);
}

/// A program attaches through `serve`: with `--attach`, or by choosing
/// `session/attach` when its `initialize` was refused. From there it speaks
/// to the owner, and its going away ends nothing.
#[test]
fn serve_attaches_its_client_to_the_running_session() {
    let mock = echo();
    let dir = project("serve-attach");
    let mut owner = Term::owner(&mock, &dir, &[]);
    owner.line("say one");
    owner.wait_for("re: one");
    wait_idle(&dir);

    let mut client = Client::spawn(&mock, &dir, &[]);
    let refused = client.call("initialize", json!({}));
    assert_eq!(refused["error"]["code"], SESSION_IN_USE, "{refused}");
    let data = &refused["error"]["data"];
    assert_eq!(data["choices"], json!(["attach", "take_over", "cancel"]));
    assert_eq!(data["owner"]["remote"], true);
    assert_eq!(
        client.result("session/attach", json!({})),
        json!({"attached": true})
    );
    let hello = client.result(
        "initialize",
        json!({"client": {"name": "ui", "version": "2"}}),
    );
    assert_eq!(hello["capabilities"]["shared"], true, "{hello}");
    assert_eq!(hello["shared"]["owner"]["pid"], owner.pid());
    owner.wait_for("attached (ui 2)");
    let history = client.result("session/history", json!({}));
    assert_eq!(history["total"], 2);
    let started = client.result("turn/start", json!({"text": "say two"}));
    assert_eq!(started["turn"], 2);
    assert_eq!(
        client.notification("turn/started")["from"],
        hello["shared"]["remote"]
            .as_u64()
            .map(|n| format!("remote-{n}"))
            .unwrap()
    );
    assert_eq!(client.notification("event/text")["text"], "re: two");
    assert_eq!(client.notification("turn/ended")["status"], "completed");
    owner.wait_for("re: two");
    // Its shutdown is a detach.
    client.result("shutdown", json!({}));
    assert_eq!(client.piped.exit(), 0);
    owner.wait_for("detached");
    assert_eq!(record(&dir).unwrap()["pid"], owner.pid());

    // With the flag there is nothing to choose; and a client that just
    // goes away is a remote that left.
    let mut client = Client::spawn(&mock, &dir, &["--attach"]);
    let hello = client.result("initialize", json!({}));
    assert_eq!(hello["shared"]["owner"]["kind"], "terminal");
    assert_eq!(client.piped.finish(), 0);
    owner.wait_times("detached", 2);
    // And the owner's end is the relay's.
    let mut client = Client::spawn(&mock, &dir, &["--attach"]);
    client.result("initialize", json!({}));
    owner.line("/quit");
    assert_eq!(client.notification("session/closed")["reason"], "ended");
    assert_eq!(client.piped.exit(), 0);
    assert_eq!(owner.exit(), 0);
}
