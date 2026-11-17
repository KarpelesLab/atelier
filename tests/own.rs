//! Two ateliers in one project directory: who owns the session, and what
//! the one started second can do about it (`src/own.rs`,
//! `docs/sessions.md`). Real processes throughout — the binary against a
//! scripted mock of an OpenAI-compatible server, on pipes where the line
//! REPL or a protocol will do and on pseudo-terminals where the terminal
//! interface is the subject.
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

/// `atelier <args>` in `cwd` against the mock, with nothing of the outer
/// environment or the real home leaking in, and its sockets in a directory
/// of the test's own.
fn atelier(base_url: &str, cwd: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.args(args)
        .current_dir(cwd)
        .env("ATELIER_BASE_URL", base_url)
        .env("ATELIER_MODEL", "mock")
        .env("ATELIER_APPROVE", "all")
        .env("ATELIER_NO_COORDINATOR", "1")
        .env("ATELIER_NO_UPDATE", "1")
        .env("XDG_RUNTIME_DIR", cwd.with_extension("run"))
        .env("XDG_STATE_HOME", cwd.with_extension("state"))
        .env("XDG_CONFIG_HOME", cwd.join(".no-user-config"))
        .env("XDG_DATA_HOME", cwd.with_extension("data"))
        .env("HOME", cwd.with_extension("home"))
        .env("TERM", "xterm")
        .env_remove("ATELIER_COORDINATOR_DIR")
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

/// The owner's record in `dir`'s lock file, if it holds one.
fn record(dir: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(dir.join(".atelier/session.lock")).ok()?).ok()
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
        dir.join(".atelier/session.json").exists()
    });
    let saved = std::fs::read(dir.join(".atelier/session.json")).unwrap();
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
        std::fs::read(dir.join(".atelier/session.json")).unwrap(),
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
        dir.join(".atelier/session.json").exists()
    });
    let saved = std::fs::read(dir.join(".atelier/session.json")).unwrap();

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
        std::fs::read(dir.join(".atelier/session.json")).unwrap(),
        saved
    );
    assert_eq!(record(&dir).unwrap()["pid"], owner.pid());
    // With nobody owning it, a one-shot run saves as it always did.
    assert_eq!(owner.finish(), 0);
    let (code, ..) = run(&mock, &dir, &["--print", "say after"], "");
    assert_eq!(code, 0);
    assert_ne!(
        std::fs::read(dir.join(".atelier/session.json")).unwrap(),
        saved
    );
}

/// An owner killed outright leaves its record behind; the next start sees
/// that nobody holds the lock, and starts as if there were none.
#[test]
fn a_record_left_by_a_killed_owner_is_replaced_silently() {
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
    assert_eq!(record(&dir).unwrap(), old, "the record outlives its writer");

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
        dir.join(".atelier/session.json").exists()
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
    let coord = dir.with_extension("run");
    let joined = |args: &[&str]| {
        let mut cmd = atelier(&mock.base_url(), &dir, args);
        cmd.env("ATELIER_COORDINATOR_DIR", &coord)
            .env_remove("ATELIER_NO_COORDINATOR");
        cmd
    };
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
    coordinator(&coord, "stop");
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
        dir.join(".atelier/session.json").exists()
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
        dir.join(".atelier/session.json").exists()
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
        dir.join(".atelier/session.json").exists()
    });
    let saved = std::fs::read(dir.join(".atelier/session.json")).unwrap();

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
        std::fs::read(dir.join(".atelier/session.json")).unwrap(),
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
        asked.contains("Cancel — leave it alone and exit"),
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
        dir.join(".atelier/session.json").exists()
    });

    let mut next = Term::start(&mock, &dir, &[]);
    next.wait_for("(answer with a number)");
    next.line("1");
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
