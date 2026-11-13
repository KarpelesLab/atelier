//! End-to-end integration tests for the real compiled `atelier` binary.
//!
//! These run the actual `atelier --print` executable (located via the
//! Cargo-provided `CARGO_BIN_EXE_atelier` env var) against a tiny in-process
//! mock of an OpenAI-compatible server — a `std::net::TcpListener` bound to
//! `127.0.0.1:0` on a background thread. No network, no real model: every
//! completion is scripted here, so the whole agent loop (SSE streaming, tool
//! advertisement, streamed tool-call assembly, tool execution, follow-up
//! request, final answer) is exercised deterministically and headlessly.
//!
//! The mock speaks just enough HTTP: it reads the request (headers + the
//! `Content-Length` body) on each accepted connection, then writes a
//! `text/event-stream` response of `data: {json}\n\n` frames terminated by
//! `data: [DONE]\n\n`. The provider sends `Connection: close`, so each request
//! is its own connection; a shared request counter lets a single mock script a
//! multi-step exchange (e.g. tool call on the first request, final answer on
//! the second).
//!
//! The mock is `std` only; the headless-mode cases parse atelier's JSON
//! output with `serde_json`, which the crate depends on anyway. No extra
//! dependencies, and nothing outside `tests/`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

/// Builds the full HTTP response bytes for the Nth chat request (0-based).
type Responder = Arc<dyn Fn(usize) -> Vec<u8> + Send + Sync>;

/// Builds the response from the request's body instead of its index: for
/// exchanges whose requests arrive in no fixed order (subagents running at
/// the same time), where only what a request says tells which one it is.
type BodyResponder = Arc<dyn Fn(&str) -> Vec<u8> + Send + Sync>;

/// What the mock runs per request: handed both the index and the body.
type AnyResponder = Arc<dyn Fn(usize, &str) -> Vec<u8> + Send + Sync>;

/// A background mock OpenAI-compatible server. Accepts connections until
/// dropped; each connection is answered by the `Responder`, which is handed a
/// monotonically increasing request index so one mock can script a multi-step
/// exchange.
struct Mock {
    port: u16,
    /// The body of every request received so far, in order, so a test can
    /// check what atelier actually sent.
    bodies: Arc<Mutex<Vec<String>>>,
    /// The request line and headers of each, lowercased.
    heads: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Mock {
    fn start(responder: Responder) -> Mock {
        Mock::start_by_body(Arc::new(move |n, _body| responder(n)))
    }

    /// A mock that answers each request by what it contains.
    fn by_body(responder: BodyResponder) -> Mock {
        Mock::start_by_body(Arc::new(move |_n, body| responder(body)))
    }

    fn start_by_body(responder: AnyResponder) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let port = listener.local_addr().expect("local addr").port();
        // Non-blocking accept so the loop can poll the stop flag and shut down
        // cleanly when the `Mock` is dropped (accepted sockets are put back
        // into blocking mode individually below).
        listener
            .set_nonblocking(true)
            .expect("set listener non-blocking");

        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let bodies_thread = Arc::clone(&bodies);
        let heads = Arc::new(Mutex::new(Vec::new()));
        let heads_thread = Arc::clone(&heads);
        let handle = thread::spawn(move || {
            let counter = AtomicUsize::new(0);
            while !stop_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let n = counter.fetch_add(1, Ordering::SeqCst);
                        // Ignore per-connection errors: a flaky/aborted
                        // connection must never bring the server down.
                        let _ = handle_conn(stream, n, &responder, &bodies_thread, &heads_thread);
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
            heads,
            stop,
            handle: Some(handle),
        }
    }

    /// Base URL (including the `/v1` suffix the provider appends endpoints to)
    /// for `ATELIER_BASE_URL`.
    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    /// The body of the Nth request (0-based) atelier sent.
    fn body(&self, n: usize) -> String {
        self.bodies.lock().unwrap()[n].clone()
    }

    /// The request line and headers (lowercased) of the Nth request.
    fn head(&self, n: usize) -> String {
        self.heads.lock().unwrap()[n].clone()
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

/// Read one HTTP request off `stream` (headers, then the `Content-Length`
/// body) and write the scripted response for request index `n`.
fn handle_conn(
    stream: TcpStream,
    n: usize,
    responder: &AnyResponder,
    bodies: &Mutex<Vec<String>>,
    heads: &Mutex<Vec<String>>,
) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);

    // Request line + headers, up to the blank line that ends the head.
    let mut content_length = 0usize;
    let mut head = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break; // EOF before headers finished
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break; // end of headers
        }
        let lower = trimmed.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
        head.push_str(&lower);
        head.push('\n');
    }
    heads.lock().unwrap().push(head);

    // Drain the body so the client's write completes before we reply, and
    // keep it for tests that assert on what was sent.
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;
    let body = String::from_utf8_lossy(&body).into_owned();
    bodies.lock().unwrap().push(body.clone());

    let response = responder(n, &body);
    writer.write_all(&response)?;
    writer.flush()?;
    // Close-delimited body: the provider reads the SSE stream until EOF.
    let _ = writer.shutdown(Shutdown::Write);
    Ok(())
}

/// Build an SSE `text/event-stream` HTTP response from pre-rendered JSON chunk
/// strings, terminated by the `[DONE]` sentinel.
fn sse_response(chunks: &[&str]) -> Vec<u8> {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str("data: ");
        body.push_str(chunk);
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");

    let response = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/event-stream\r\n\
         Cache-Control: no-cache\r\n\
         Connection: close\r\n\
         \r\n\
         {body}"
    );
    response.into_bytes()
}

/// A unique temp directory to use as the spawned binary's working directory.
fn unique_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("atelier-e2e-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// The `atelier` binary, set up to talk to the mock at `base_url` from `cwd`
/// with nothing of the outer environment or the real home leaking in.
fn atelier(base_url: &str, cwd: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_atelier"));
    // Trust decisions (see the project-trust and hooks cases) are neither
    // read from nor written to the real home, and nothing is trusted that a
    // test did not trust itself.
    cmd.env("XDG_STATE_HOME", cwd.with_extension("state"))
        .env_remove("ATELIER_TRUST_HOOKS")
        .env_remove("ATELIER_TRUST_PROJECT")
        .env("ATELIER_BASE_URL", base_url)
        .env("ATELIER_MODEL", "mock")
        .env("ATELIER_APPROVE", "all")
        // No coordinator daemon left behind by the test run.
        .env("ATELIER_NO_COORDINATOR", "1")
        // No user config (global instructions, a saved login), and global
        // memory next to the temp project.
        .env("XDG_CONFIG_HOME", cwd.join(".no-user-config"))
        .env("XDG_DATA_HOME", cwd.with_extension("data"))
        // Likewise the user's skills under ~/.claude and ~/.agents.
        .env("HOME", cwd.with_extension("home"))
        .env_remove("ATELIER_API_KEY")
        .env_remove("ATELIER_TRACE")
        .env_remove("ATELIER_DEBUG")
        .env_remove("ATELIER_BACKEND")
        .env_remove("ATELIER_CONTEXT_LIMIT")
        .env_remove("ATELIER_REVIEW")
        .env_remove("ATELIER_MODE")
        .env_remove("ATELIER_PARALLEL_TOOLS")
        .current_dir(cwd);
    cmd
}

/// Trust the project in `cwd` as it is now, the way a user does it for
/// scripts: `atelier trust accept`, recorded in the test's own state
/// directory. For the cases whose `atelier.toml` configures something that
/// only a trusted project gets (a provider, an allow rule, an MCP server).
fn trust_project(cwd: &Path) {
    let out = atelier("http://127.0.0.1:9", cwd)
        .args(["trust", "accept"])
        .stdin(Stdio::null())
        .output()
        .expect("run atelier trust accept");
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Spawn `atelier --print <prompt>` against `base_url` with `cwd` and return
/// its captured output.
fn run_print(base_url: &str, cwd: &Path, prompt: &str) -> Output {
    run_atelier(base_url, cwd, &["--print", prompt], None)
}

/// Spawn `atelier <args>` against `base_url` with `cwd`, feeding it `stdin`
/// (closed right after; no stdin at all for `None`), wait for it to exit
/// (killing it past a generous deadline so a hang can't wedge the suite),
/// and return its captured output.
fn run_atelier(base_url: &str, cwd: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    run_atelier_env(base_url, cwd, args, stdin, &[])
}

/// [`run_atelier`] with changes to its environment: each variable is set to
/// the value given, or removed for `None`.
fn run_atelier_setenv(
    base_url: &str,
    cwd: &Path,
    args: &[&str],
    stdin: Option<&str>,
    env: &[(&str, Option<&str>)],
) -> Output {
    let mut cmd = atelier(base_url, cwd);
    cmd.args(args);
    for (name, value) in env {
        match value {
            Some(v) => cmd.env(name, v),
            None => cmd.env_remove(name),
        };
    }
    finish(cmd, stdin)
}

/// [`run_atelier`] with extra environment variables for the child.
fn run_atelier_env(
    base_url: &str,
    cwd: &Path,
    args: &[&str],
    stdin: Option<&str>,
    env: &[(&str, &str)],
) -> Output {
    let mut cmd = atelier(base_url, cwd);
    cmd.args(args).envs(env.iter().copied());
    finish(cmd, stdin)
}

/// Run `cmd` to its end, feeding it `stdin`, and return its captured output.
fn finish(mut cmd: Command, stdin: Option<&str>) -> Output {
    let mut child = cmd
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn atelier binary");
    if let Some(text) = stdin {
        // Small enough to fit the pipe buffer; dropping the handle is EOF.
        let mut pipe = child.stdin.take().expect("child stdin");
        pipe.write_all(text.as_bytes()).expect("write child stdin");
    }

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match child.try_wait().expect("try_wait on child") {
            Some(_) => break,
            None => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
    }

    // Output is tiny (well under the pipe buffer), so collecting it after exit
    // cannot deadlock.
    child.wait_with_output().expect("collect child output")
}

/// Case 1 — plain answer: the mock streams an assistant message across two
/// content deltas and `[DONE]`; stdout must carry the joined answer.
#[test]
fn plain_answer_is_streamed_to_stdout() {
    let responder: Responder = Arc::new(|_n| {
        sse_response(&[
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"hello from "}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"mock"}}]}"#,
        ])
    });
    let mock = Mock::start(responder);
    let dir = unique_dir("plain");

    let out = run_print(&mock.base_url(), &dir, "hi");

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("hello from mock"),
        "expected answer on stdout.\n stdout: {stdout:?}\n stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Case 2 — full tool-call loop. The mock is scripted by request count:
///
/// - request 0 streams a `write` tool call, with its `arguments` JSON split
///   across several SSE frames to exercise the provider's fragmented
///   tool-call reassembly (id/name once, arguments in pieces);
/// - request 1 (sent after atelier executes the tool and feeds back the
///   result) streams the final answer `"done"`.
///
/// Proves the end-to-end loop: the file is created in the cwd with the exact
/// content, and the final answer reaches stdout.
#[test]
fn tool_call_loop_writes_file_and_answers() {
    let responder: Responder = Arc::new(|n| {
        if n == 0 {
            sse_response(&[
                // id + name, empty arguments to start.
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"write","arguments":""}}]}}]}"#,
                // arguments, first fragment.
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":\"out.txt\","}}]}}]}"#,
                // arguments, final fragment.
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"content\":\"written by tool\"}"}}]}}]}"#,
            ])
        } else {
            sse_response(&[r#"{"choices":[{"index":0,"delta":{"content":"done"}}]}"#])
        }
    });
    let mock = Mock::start(responder);
    let dir = unique_dir("tool");

    let out = run_print(&mock.base_url(), &dir, "write the file");

    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);

    let written = dir.join("out.txt");
    let contents = std::fs::read_to_string(&written).unwrap_or_else(|e| {
        panic!("expected {written:?} to exist: {e}\n stdout: {stdout:?}\n stderr: {stderr:?}")
    });
    assert_eq!(contents, "written by tool");
    assert!(
        stdout.contains("done"),
        "expected final answer on stdout.\n stdout: {stdout:?}\n stderr: {stderr:?}"
    );
}

/// Case 3 — a trailing `usage` chunk (empty `choices`, as sent when
/// `stream_options.include_usage` is honored) must not break parsing: the
/// answer still streams through intact.
#[test]
fn usage_chunk_does_not_break_parsing() {
    let responder: Responder = Arc::new(|_n| {
        sse_response(&[
            r#"{"choices":[{"index":0,"delta":{"content":"with usage"}}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
        ])
    });
    let mock = Mock::start(responder);
    let dir = unique_dir("usage");

    let out = run_print(&mock.base_url(), &dir, "hi");

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("with usage"),
        "expected answer on stdout despite usage chunk.\n stdout: {stdout:?}\n stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The system message of a captured chat request body.
fn system_message(body: &str) -> String {
    let request: serde_json::Value = serde_json::from_str(body).expect("request body is JSON");
    request["messages"][0]["content"]
        .as_str()
        .expect("first message has text content")
        .to_string()
}

/// Case 4 — memory across sessions. In a first run the mock model calls
/// `memory save` and the note lands in `.atelier/memory/`, with its index.
/// A second, separate run in the same directory — a new process with no
/// saved conversation — must send the memory index in its system message,
/// which is how a correction given once carries over to the next session.
#[test]
fn memory_saved_in_one_run_is_recalled_in_the_next() {
    let responder: Responder = Arc::new(|n| {
        if n == 0 {
            sse_response(&[
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"memory","arguments":"{\"action\":\"save\",\"name\":\"lint-first\",\"type\":\"feedback\",\"description\":\"run clippy before committing\",\"content\":\"The user wants clippy clean before every commit.\"}"}}]}}]}"#,
            ])
        } else {
            sse_response(&[r#"{"choices":[{"index":0,"delta":{"content":"noted"}}]}"#])
        }
    });
    let mock = Mock::start(responder);
    let dir = unique_dir("memory");

    let out = run_print(
        &mock.base_url(),
        &dir,
        "always run clippy before committing",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let note = dir.join(".atelier/memory/lint-first.md");
    let saved = std::fs::read_to_string(&note).unwrap_or_else(|e| {
        panic!("expected {note:?} to exist: {e}\n stdout: {stdout:?}\n stderr: {stderr:?}")
    });
    assert!(saved.contains("type: feedback"), "{saved}");
    assert!(
        saved.contains("clippy clean before every commit"),
        "{saved}"
    );
    let index = std::fs::read_to_string(dir.join(".atelier/memory/MEMORY.md")).unwrap();
    assert!(
        index.contains("- [lint-first](lint-first.md) — run clippy before committing"),
        "{index}"
    );

    let first_run = mock.bodies.lock().unwrap().clone();
    assert_eq!(first_run.len(), 2, "tool call, then the final answer");
    assert!(
        !system_message(&first_run[0]).contains("lint-first"),
        "nothing is remembered before the save"
    );
    assert!(
        first_run[0].contains(r#""name":"memory""#),
        "the memory tool is advertised"
    );
    assert!(
        first_run[1].contains("saved memory 'lint-first' (project)"),
        "the tool result is fed back: {}",
        first_run[1]
    );

    run_print(&mock.base_url(), &dir, "commit this");
    let bodies = mock.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 3);
    let system = system_message(&bodies[2]);
    assert!(
        system.contains("## memory") && system.contains(r#""action": "read""#),
        "the index and how to use it are injected: {system}"
    );
    assert!(
        system.contains("- lint-first [feedback, project] — run clippy before committing"),
        "{system}"
    );
    assert!(
        !system.contains("clippy clean before every commit"),
        "only the index is ambient, not the note: {system}"
    );
}

/// Case 5 — instruction files reach the model. The project root's `AGENTS.md`
/// is in the system message from the first request; a subdirectory's own
/// `AGENTS.md` arrives only with the result of the first tool call under it.
#[test]
fn instruction_files_are_sent_root_first_and_nested_on_demand() {
    let responder: Responder = Arc::new(|n| {
        if n == 0 {
            sse_response(&[
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"path\":\"web/page.txt\"}"}}]}}]}"#,
            ])
        } else {
            sse_response(&[r#"{"choices":[{"index":0,"delta":{"content":"done"}}]}"#])
        }
    });
    let mock = Mock::start(responder);
    let dir = unique_dir("instructions");
    std::fs::create_dir_all(dir.join("web")).unwrap();
    std::fs::write(dir.join("AGENTS.md"), "ROOT-RULE-7f3a").unwrap();
    std::fs::write(dir.join("web/AGENTS.md"), "WEB-RULE-91cc").unwrap();
    std::fs::write(dir.join("web/page.txt"), "page content").unwrap();

    let out = run_print(&mock.base_url(), &dir, "read the page");

    let bodies = mock.bodies.lock().unwrap().clone();
    assert!(
        bodies.len() >= 2,
        "expected two requests, got {}.\n stderr: {:?}",
        bodies.len(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(bodies[0].contains("ROOT-RULE-7f3a"));
    assert!(
        !bodies[0].contains("WEB-RULE-91cc"),
        "not before it's needed"
    );
    let last = bodies.last().unwrap();
    assert!(last.contains("ROOT-RULE-7f3a"));
    assert!(last.contains("page content") && last.contains("WEB-RULE-91cc"));
}

/// `[instructions] files` adds instruction files: after the `AGENTS.md` found
/// by name, in the order written with a glob's matches sorted, each under its
/// path — the user-global settings' files before the project's. What the
/// project's settings name outside the repository is not read.
#[test]
fn instruction_files_named_in_the_settings_reach_the_system_message_in_order() {
    let mock = Mock::start(always("ok"));
    let dir = unique_dir("instruction-files");
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::create_dir_all(dir.join("team")).unwrap();
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    std::fs::write(dir.join("AGENTS.md"), "ROOT-RULE-11aa").unwrap();
    std::fs::write(dir.join("team/b.md"), "TEAM-B-22bb").unwrap();
    std::fs::write(dir.join("team/a.md"), "TEAM-A-33cc").unwrap();
    std::fs::write(dir.join("docs/conventions.md"), "CONVENTIONS-44dd").unwrap();
    let outside = dir.with_extension("outside.md");
    std::fs::write(&outside, "OUTSIDE-55ee").unwrap();
    std::fs::write(
        dir.join("atelier.toml"),
        format!(
            "[instructions]\nfiles = [\"team/*.md\", \"docs/conventions.md\", \"{}\"]\n",
            outside.display()
        ),
    )
    .unwrap();
    // The user's own settings may name a file anywhere.
    let config = dir.join(".no-user-config/atelier");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("atelier.toml"),
        format!("[instructions]\nfiles = [\"{}\"]\n", outside.display()),
    )
    .unwrap();

    let out = run_print(&mock.base_url(), &dir, "hello");
    let bodies = mock.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 1, "{}", String::from_utf8_lossy(&out.stderr));
    let system = system_message(&bodies[0]);
    let order = [
        "OUTSIDE-55ee",
        "ROOT-RULE-11aa",
        "TEAM-A-33cc",
        "TEAM-B-22bb",
        "CONVENTIONS-44dd",
        "[skipped: outside the project]",
    ];
    let at: Vec<usize> = order
        .iter()
        .map(|needle| {
            system
                .find(needle)
                .unwrap_or_else(|| panic!("{needle} in {system}"))
        })
        .collect();
    assert!(at.is_sorted(), "{system}");
    assert_eq!(system.matches("OUTSIDE-55ee").count(), 1, "{system}");
    assert!(
        system.contains(&format!(
            "### {}\nTEAM-A-33cc",
            dir.join("team/a.md").display()
        )),
        "{system}"
    );
}

/// stdout as JSON lines: every non-empty line must parse, which is the
/// `stream-json` contract (nothing else on stdout).
fn json_lines(out: &Output) -> Vec<serde_json::Value> {
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            serde_json::from_str(l).unwrap_or_else(|e| {
                panic!(
                    "stdout line is not JSON ({e}): {l:?}\n stderr: {:?}",
                    String::from_utf8_lossy(&out.stderr)
                )
            })
        })
        .collect()
}

/// The events of `kind` among `events`.
fn of_type<'a>(events: &'a [serde_json::Value], kind: &str) -> Vec<&'a serde_json::Value> {
    events.iter().filter(|e| e["type"] == kind).collect()
}

/// A responder for the scripted `write` tool call followed by the answer
/// `"done"` (with a usage chunk), as in case 2.
fn write_then_done() -> Responder {
    Arc::new(|n| {
        if n == 0 {
            sse_response(&[
                r#"{"choices":[{"index":0,"delta":{"content":"on it"}}]}"#,
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"write","arguments":"{\"path\":\"out.txt\",\"content\":\"written by tool\"}"}}]}}]}"#,
            ])
        } else {
            sse_response(&[
                r#"{"choices":[{"index":0,"delta":{"content":"do"}}]}"#,
                r#"{"choices":[{"index":0,"delta":{"content":"ne"}}]}"#,
                r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
            ])
        }
    })
}

/// A responder that always answers with `text` as one content delta.
fn always(text: &'static str) -> Responder {
    Arc::new(move |_n| {
        let chunk = serde_json::json!({"choices":[{"index":0,"delta":{"content":text}}]});
        sse_response(&[&chunk.to_string()])
    })
}

/// A schema file in `dir` requiring `{"name": string, "count": integer}`.
fn write_schema(dir: &Path) -> String {
    let path = dir.join("schema.json");
    std::fs::write(
        &path,
        r#"{"type":"object","properties":{"name":{"type":"string"},"count":{"type":"integer"}},"required":["name","count"],"additionalProperties":false}"#,
    )
    .expect("write schema");
    path.to_string_lossy().into_owned()
}

/// Case 4 — `--output-format stream-json` over the tool-call loop: stdout is
/// JSON lines only, opening with `init`, carrying the text deltas and the
/// tool call's start/end, and closing with a `result` that holds the final
/// answer (the text after the last tool call), step count and usage.
#[test]
fn stream_json_emits_events_for_a_tool_loop() {
    let mock = Mock::start(write_then_done());
    let dir = unique_dir("stream");

    let out = run_atelier(
        &mock.base_url(),
        &dir,
        &["-p", "--output-format", "stream-json", "write the file"],
        None,
    );
    assert_eq!(out.status.code(), Some(0));
    let events = json_lines(&out);

    let init = &events[0];
    assert_eq!(init["type"], "system");
    assert_eq!(init["subtype"], "init");
    assert_eq!(init["model"], "mock");
    assert!(init["version"].is_string() && init["cwd"].is_string());
    assert!(
        init["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t == "write"),
        "init lists the tools: {init}"
    );

    let texts: Vec<&str> = of_type(&events, "text")
        .iter()
        .map(|e| e["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts, ["on it", "do", "ne"]);

    let starts = of_type(&events, "tool_start");
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0]["name"], "write");
    // Arguments arrive parsed, not as the model's JSON string.
    assert_eq!(starts[0]["arguments"]["path"], "out.txt");
    let ends = of_type(&events, "tool_end");
    assert_eq!(ends.len(), 1);
    assert_eq!(ends[0]["name"], "write");
    assert_eq!(ends[0]["ok"], true);
    assert!(ends[0]["output"].is_string());

    let result = events.last().unwrap();
    assert_eq!(result["type"], "result");
    assert_eq!(result["subtype"], "success");
    assert_eq!(result["is_error"], false);
    assert_eq!(result["result"], "done");
    assert_eq!(result["num_steps"], 2);
    assert_eq!(result["usage"]["context_tokens"], 15);
    assert_eq!(result["usage"]["output_tokens"], 5);
    assert!(result["duration_ms"].is_u64());
    assert_eq!(of_type(&events, "result").len(), 1);

    assert_eq!(
        std::fs::read_to_string(dir.join("out.txt")).unwrap(),
        "written by tool"
    );
}

/// Case 5 — `--output-format json`: stdout is exactly one JSON object, the
/// result, with the whole answer in it.
#[test]
fn json_mode_prints_only_the_result() {
    let mock = Mock::start(write_then_done());
    let dir = unique_dir("json");

    let out = run_atelier(
        &mock.base_url(),
        &dir,
        &["--print", "--output-format=json", "write the file"],
        None,
    );
    assert_eq!(out.status.code(), Some(0));
    let events = json_lines(&out);
    assert_eq!(events.len(), 1, "one object on stdout: {events:?}");
    assert_eq!(events[0]["type"], "result");
    assert_eq!(events[0]["subtype"], "success");
    assert_eq!(events[0]["result"], "done");
    assert_eq!(events[0]["num_steps"], 2);
    // Tool activity still shows on stderr for whoever watches the run.
    assert!(String::from_utf8_lossy(&out.stderr).contains("write"));
}

/// Case 6 — `--max-turns`: a model that never stops calling tools is cut off
/// after N requests, with exit status 3 and a `max_turns` result.
#[test]
fn max_turns_stops_the_loop_with_exit_3() {
    let responder: Responder = Arc::new(|_n| {
        sse_response(&[
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"ls","arguments":"{}"}}]}}]}"#,
        ])
    });
    let mock = Mock::start(responder);
    let dir = unique_dir("maxturns");

    let out = run_atelier(
        &mock.base_url(),
        &dir,
        &[
            "-p",
            "--max-turns",
            "2",
            "--output-format",
            "json",
            "loop forever",
        ],
        None,
    );
    assert_eq!(out.status.code(), Some(3));
    let events = json_lines(&out);
    assert_eq!(events[0]["subtype"], "max_turns");
    assert_eq!(events[0]["is_error"], true);
    assert_eq!(events[0]["num_steps"], 2);
    assert_eq!(mock.requests(), 2, "no request past the limit");

    // The default text mode reports the same through the status and stderr.
    let out = run_atelier(
        &mock.base_url(),
        &dir,
        &["-p", "--max-turns=1", "loop forever"],
        None,
    );
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--max-turns"));
}

/// Case 7 — `--output-schema`, satisfied: the schema rides along with the
/// prompt, a fenced answer is accepted, and the parsed value is what comes
/// out (`structured` in JSON modes, alone on stdout in text mode).
#[test]
fn output_schema_success() {
    let mock = Mock::start(always(
        "```json\n{\"name\": \"atelier\", \"count\": 3}\n```",
    ));
    let dir = unique_dir("schema-ok");
    let schema = write_schema(&dir);

    let out = run_atelier(
        &mock.base_url(),
        &dir,
        &[
            "-p",
            "--output-schema",
            &schema,
            "--output-format",
            "json",
            "describe",
        ],
        None,
    );
    assert_eq!(out.status.code(), Some(0));
    let events = json_lines(&out);
    assert_eq!(events[0]["subtype"], "success");
    assert_eq!(
        events[0]["structured"],
        serde_json::json!({"name": "atelier", "count": 3})
    );
    assert_eq!(mock.requests(), 1);
    assert!(
        mock.body(0).contains("additionalProperties"),
        "the schema is part of the prompt"
    );

    let out = run_atelier(
        &mock.base_url(),
        &dir,
        &["-p", "--output-schema", &schema, "describe"],
        None,
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "{\"count\":3,\"name\":\"atelier\"}\n"
    );
}

/// Case 8 — `--output-schema`, violated: the model is told what is wrong
/// once. A corrected answer succeeds; a second bad one exits 4 with a
/// `schema_error` result naming the violation.
#[test]
fn output_schema_retries_once_then_fails() {
    let responder: Responder = Arc::new(|n| {
        let text = if n == 0 {
            r#"{"name": "atelier"}"#
        } else {
            r#"{"name": "atelier", "count": 1}"#
        };
        let chunk = serde_json::json!({"choices":[{"index":0,"delta":{"content":text}}]});
        sse_response(&[&chunk.to_string()])
    });
    let mock = Mock::start(responder);
    let dir = unique_dir("schema-retry");
    let schema = write_schema(&dir);
    let args = [
        "-p",
        "--output-schema",
        &schema,
        "--output-format",
        "stream-json",
        "describe",
    ];

    let out = run_atelier(&mock.base_url(), &dir, &args, None);
    assert_eq!(out.status.code(), Some(0));
    let events = json_lines(&out);
    let result = events.last().unwrap();
    assert_eq!(result["subtype"], "success");
    assert_eq!(result["structured"]["count"], 1);
    assert_eq!(result["num_steps"], 2);
    assert_eq!(
        of_type(&events, "notice").len(),
        1,
        "the retry is announced"
    );
    assert!(
        mock.body(1).contains("missing required property"),
        "the retry prompt carries the validation error"
    );

    let mock = Mock::start(always(r#"{"name": "atelier", "count": "many"}"#));
    let dir = unique_dir("schema-bad");
    let out = run_atelier(&mock.base_url(), &dir, &args, None);
    assert_eq!(out.status.code(), Some(4));
    let events = json_lines(&out);
    let result = events.last().unwrap();
    assert_eq!(result["subtype"], "schema_error");
    assert_eq!(result["is_error"], true);
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("$.count: expected integer, got string")
    );
    assert!(result.get("structured").is_none());
    assert_eq!(mock.requests(), 2, "one retry, no more");
}

/// Case 9 — `--input-format stream-json`: each stdin line is a turn in the
/// same session (the second request carries the first exchange), each
/// answered by its own result; EOF ends the run.
#[test]
fn stream_json_input_runs_several_turns() {
    let responder: Responder = Arc::new(|n| {
        let text = if n == 0 { "first" } else { "second" };
        let chunk = serde_json::json!({"choices":[{"index":0,"delta":{"content":text}}]});
        sse_response(&[&chunk.to_string()])
    });
    let mock = Mock::start(responder);
    let dir = unique_dir("multiturn");

    let out = run_atelier(
        &mock.base_url(),
        &dir,
        &[
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
        ],
        Some(
            "{\"type\":\"user\",\"text\":\"alpha question\"}\n\n{\"type\":\"user\",\"text\":\"beta question\"}\n",
        ),
    );
    assert_eq!(out.status.code(), Some(0));
    let events = json_lines(&out);
    assert_eq!(of_type(&events, "system").len(), 1);
    let results: Vec<&str> = of_type(&events, "result")
        .iter()
        .map(|e| e["result"].as_str().unwrap())
        .collect();
    assert_eq!(results, ["first", "second"]);
    assert_eq!(mock.requests(), 2);
    let second = mock.body(1);
    assert!(second.contains("alpha question") && second.contains("beta question"));
    assert!(second.contains("first"), "history carries the first answer");

    // A line that isn't a user message ends the run as a usage error.
    let out = run_atelier(
        &mock.base_url(),
        &dir,
        &[
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
        ],
        Some("not json\n"),
    );
    assert_eq!(out.status.code(), Some(2));
    let events = json_lines(&out);
    assert_eq!(events.last().unwrap()["subtype"], "input_error");
}

/// Case 10 — the command line: unknown options and bad values are usage
/// errors (status 2, nothing sent), a provider failure is status 1, and
/// `--` lets a prompt start with dashes.
#[test]
fn usage_errors_and_dashed_prompts() {
    let mock = Mock::start(always("ok"));
    let dir = unique_dir("usage");

    for args in [
        &["-p", "--frobnicate", "hi"][..],
        &["-p", "--output-format", "yaml", "hi"],
        &["-p", "--max-turns", "none", "hi"],
        &["--max-turns", "3"],
        &["-p", "--output-schema", "no-such-schema.json", "hi"],
    ] {
        let out = run_atelier(&mock.base_url(), &dir, args, None);
        assert_eq!(out.status.code(), Some(2), "args: {args:?}");
        assert!(out.stdout.is_empty(), "args: {args:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("usage:"));
    }
    assert_eq!(mock.requests(), 0);

    let out = run_atelier(
        &mock.base_url(),
        &dir,
        &["-p", "--output-format", "json", "--", "--frobnicate", "it"],
        None,
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(json_lines(&out)[0]["result"], "ok");
    assert!(mock.body(0).contains("--frobnicate it"));

    // Nothing listens here: the request fails, which is a runtime error.
    let out = run_atelier(
        "http://127.0.0.1:1/v1",
        &dir,
        &["-p", "--output-format", "json", "hi"],
        None,
    );
    assert_eq!(out.status.code(), Some(1));
    let result = &json_lines(&out)[0];
    assert_eq!(result["subtype"], "error");
    assert!(result["error"].is_string());
}

/// Run the line REPL (what atelier is when stdin is a pipe) with `input` as
/// everything typed, and return its output once it exits at end of input.
/// Tools are *not* auto-approved, so approval prompts read their answer from
/// `input` too.
#[cfg(unix)]
fn run_repl(base_url: &str, cwd: &Path, input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_atelier"))
        .env("ATELIER_BASE_URL", base_url)
        .env("ATELIER_MODEL", "mock")
        .env("ATELIER_NO_COORDINATOR", "1")
        // User-global files go next to the temp project, never the real home.
        .env("XDG_DATA_HOME", cwd.with_extension("data"))
        .env("XDG_CONFIG_HOME", cwd.with_extension("config"))
        .env("XDG_STATE_HOME", cwd.with_extension("state"))
        .env("HOME", cwd.with_extension("home"))
        .env_remove("ATELIER_TRUST_HOOKS")
        .env_remove("ATELIER_TRUST_PROJECT")
        .env_remove("ATELIER_APPROVE")
        .env_remove("ATELIER_BACKEND")
        .env_remove("ATELIER_API_KEY")
        .env_remove("ATELIER_TRACE")
        .env_remove("ATELIER_DEBUG")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn atelier binary");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(input.as_bytes())
        .expect("write the input");
    child.wait_with_output().expect("collect child output")
}

/// Case 5 — a file-defined slash command. `/greet` is a template in the
/// project; typing it sends the expanded template as the prompt: arguments
/// substituted, and the `` !`command` `` replaced by its output once the user
/// approves it (the same prompt as the `bash` tool). Built-ins can't be
/// shadowed, `/help` lists the command, and a refusal sends nothing.
#[cfg(unix)]
#[test]
fn custom_command_expands_into_the_prompt() {
    let responder: Responder =
        Arc::new(|_| sse_response(&[r#"{"choices":[{"index":0,"delta":{"content":"hello"}}]}"#]));
    let mock = Mock::start(responder);
    let dir = unique_dir("commands");
    let commands = dir.join(".atelier/commands");
    std::fs::create_dir_all(&commands).unwrap();
    std::fs::write(
        commands.join("greet.md"),
        "---\ndescription: say hello\nargument-hint: <who>\n---\n\
         Greet $1 from !`cat who.txt`. Raw: $ARGUMENTS\n",
    )
    .unwrap();
    std::fs::write(commands.join("help.md"), "shadowing a built-in").unwrap();
    std::fs::write(dir.join("who.txt"), "the-file\n").unwrap();
    let global = dir.with_extension("config").join("atelier/commands");
    std::fs::create_dir_all(&global).unwrap();
    std::fs::write(global.join("standup.md"), "What changed yesterday?").unwrap();

    // Refused: nothing reaches the model.
    let out = run_repl(&mock.base_url(), &dir, "/greet you\nn\n");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("allow tool 'bash'"), "{stdout}");
    assert!(
        stdout.contains("/greet: not sent — `cat who.txt` was not approved"),
        "{stdout}"
    );
    assert!(mock.bodies.lock().unwrap().is_empty());

    // Approved: the expanded template is the prompt.
    let out = run_repl(&mock.base_url(), &dir, "/greet it's me\ny\n/help\n/nope\n");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let bodies = mock.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 1, "stdout: {stdout}\nstderr: {stderr}");
    let request: serde_json::Value = serde_json::from_str(&bodies[0]).unwrap();
    let prompt = request["messages"]
        .as_array()
        .and_then(|m| m.last())
        .map(|m| m["content"].to_string())
        .unwrap_or_default();
    assert!(
        prompt.contains("Greet it's from the-file. Raw: it's me"),
        "{prompt}"
    );
    assert!(!prompt.contains("/greet"), "{prompt}");
    assert!(stdout.contains("/greet → expanded from"), "{stdout}");
    assert!(stdout.contains("hello"), "{stdout}");

    // `/help` is still the built-in, and lists both custom commands after it.
    assert!(!bodies[0].contains("shadowing a built-in"));
    let builtins = stdout.find("/quit ").expect("built-in help");
    let custom = stdout.find("custom commands:").expect("custom section");
    assert!(builtins < custom, "{stdout}");
    assert!(stdout.contains("/greet <who>"), "{stdout}");
    assert!(stdout.contains("say hello"), "{stdout}");
    assert!(stdout.contains("/standup"), "{stdout}");
    assert!(!stdout[custom..].contains("\n  /help"), "{stdout}");
    assert!(stdout.contains("unknown command '/nope'"), "{stdout}");
}

/// `model:` in a command file's front matter is the model for the turn that
/// command starts, variant included — and only that turn: the next prompt is
/// on the session's model again, also after a turn that failed.
#[cfg(unix)]
#[test]
fn a_commands_model_applies_to_its_turn_only() {
    let mock = Mock::start(Arc::new(|n| match n {
        // The command's first turn fails at the provider.
        0 => error_response(400, r#"{"error":{"message":"no such model"}}"#),
        _ => answer_response("done"),
    }));
    let dir = unique_dir("command-model");
    let commands = dir.join(".atelier/commands");
    std::fs::create_dir_all(&commands).unwrap();
    std::fs::write(
        commands.join("deep.md"),
        "---\ndescription: think hard\nmodel: big-model#high\n---\nThink hard about $ARGUMENTS\n",
    )
    .unwrap();

    let out = run_repl(
        &mock.base_url(),
        &dir,
        "/deep the parser\nplain one\n/deep the lexer\nplain two\n/model\n",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(mock.requests(), 4, "{stdout}\n{stderr}");
    let sent: Vec<serde_json::Value> = (0..4).map(|n| request_json(&mock, n)).collect();
    let models: Vec<&str> = sent.iter().map(|r| r["model"].as_str().unwrap()).collect();
    assert_eq!(models, ["big-model", "mock", "big-model", "mock"]);
    let efforts: Vec<Option<&str>> = sent
        .iter()
        .map(|r| r["reasoning_effort"].as_str())
        .collect();
    assert_eq!(efforts, [Some("high"), None, Some("high"), None]);
    assert!(
        messages_of(&sent[2], "user")
            .last()
            .unwrap()
            .contains("Think hard about the lexer")
    );

    assert!(stderr.contains("no such model"), "{stderr}");
    assert_eq!(
        stdout
            .matches("model: big-model#high for this turn")
            .count(),
        2,
        "{stdout}"
    );
    // Afterwards the session is where it was.
    assert!(stdout.contains("model: mock\n"), "{stdout}");
}

/// `/init` is a prompt, not a generator: it sends the brief for writing
/// `AGENTS.md` — with what was typed after it as guidance — as an ordinary
/// turn, and is a built-in a command file cannot replace.
#[cfg(unix)]
#[test]
fn init_sends_the_brief_for_writing_agents_md() {
    let mock = Mock::start(always("on it"));
    let dir = unique_dir("init");
    let commands = dir.join(".atelier/commands");
    std::fs::create_dir_all(&commands).unwrap();
    std::fs::write(commands.join("init.md"), "shadowing /init").unwrap();

    let out = run_repl(
        &mock.base_url(),
        &dir,
        "/init the web/ app matters most\n/help\n",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let bodies = mock.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 1, "{stdout}");
    let prompt = messages_of(&request(&bodies[0]), "user").pop().unwrap();
    for needle in [
        "Write an `AGENTS.md` for this repository",
        "how to run a single test",
        "Keep it under about 60 lines",
        "improve it rather than overwrite it",
        "leave `CLAUDE.md` as it is",
        "Further guidance from the user:\nthe web/ app matters most",
    ] {
        assert!(prompt.contains(needle), "{needle:?} in:\n{prompt}");
    }
    assert!(!prompt.contains("shadowing /init"), "{prompt}");
    assert!(
        stdout.contains("/init → a brief to write AGENTS.md"),
        "{stdout}"
    );
    assert!(stdout.contains("  /init [guidance] "), "{stdout}");
}

/// Case 6 — skills. A project skill and a user-global one are advertised in
/// the system message by name and description only. The mock model then
/// loads the global skill and one of its files with the `skill` tool: both
/// come back without an approval prompt (nothing is auto-approved in this
/// run, and a prompt would be refused at end of input), although the files
/// are outside the project.
#[cfg(unix)]
#[test]
fn skills_are_advertised_and_loaded_on_demand() {
    let responder: Responder = Arc::new(|n| match n {
        0 => sse_response(&[
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"skill","arguments":"{\"name\":\"deploy\"}"}}]}}]}"#,
        ]),
        1 => sse_response(&[
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_2","type":"function","function":{"name":"skill","arguments":"{\"name\":\"deploy\",\"file\":\"hosts.md\"}"}}]}}]}"#,
        ]),
        _ => sse_response(&[r#"{"choices":[{"index":0,"delta":{"content":"deployed"}}]}"#]),
    });
    let mock = Mock::start(responder);
    let dir = unique_dir("skills");

    let project = dir.join(".claude/skills/changelog");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("SKILL.md"),
        "---\nname: changelog\ndescription: write the changelog entry\n---\nPROJECT-BODY\n",
    )
    .unwrap();
    let global = dir.with_extension("home").join(".agents/skills/deploy");
    std::fs::create_dir_all(&global).unwrap();
    std::fs::write(
        global.join("SKILL.md"),
        "---\nname: deploy\ndescription: ship to production\n---\nGLOBAL-BODY: see hosts.md\n",
    )
    .unwrap();
    std::fs::write(global.join("hosts.md"), "HOSTS-CONTENT\n").unwrap();

    let out = run_repl(&mock.base_url(), &dir, "/skills\nship it\n");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stdout.contains("2 skill(s):"), "{stdout}");
    assert!(
        stdout.contains("changelog — write the changelog entry\n      .claude/skills/changelog"),
        "{stdout}"
    );
    assert!(stdout.contains("deployed"), "{stdout}\n{stderr}");
    assert!(!stdout.contains("allow tool"), "{stdout}");

    let bodies = mock.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 3, "stdout: {stdout}\nstderr: {stderr}");
    let system = system_message(&bodies[0]);
    assert!(
        system.contains("## skills") && system.contains("`skill` tool"),
        "{system}"
    );
    assert!(
        system.contains("- changelog — write the changelog entry\n- deploy — ship to production"),
        "{system}"
    );
    assert!(
        !bodies[0].contains("PROJECT-BODY") && !bodies[0].contains("GLOBAL-BODY"),
        "only names and descriptions are ambient: {}",
        bodies[0]
    );
    assert!(
        bodies[0].contains(r#""name":"skill""#),
        "the skill tool is advertised"
    );
    assert!(
        bodies[1].contains("GLOBAL-BODY: see hosts.md") && bodies[1].contains("  hosts.md"),
        "the instructions and file listing are the tool result: {}",
        bodies[1]
    );
    assert!(
        !bodies[1].contains("HOSTS-CONTENT") && bodies[2].contains("HOSTS-CONTENT"),
        "the file arrives when asked for: {}",
        bodies[2]
    );
}

/// Write `hooks` (TOML `[[hooks]]` tables) as the project's `atelier.toml`
/// and run `--print` there. `trusted` sets `ATELIER_TRUST_HOOKS=1`, which
/// accepts a project's hooks (and nothing else of it) for one run.
fn run_with_hooks(mock: &Mock, dir: &Path, hooks: &str, trusted: bool) -> Output {
    std::fs::write(dir.join("atelier.toml"), hooks).expect("write atelier.toml");
    let env: &[(&str, &str)] = if trusted {
        &[("ATELIER_TRUST_HOOKS", "1")]
    } else {
        &[]
    };
    run_atelier_env(&mock.base_url(), dir, &["--print", "go"], None, env)
}

/// Hooks 1 — a `pre_tool` hook that exits 2 stops the call: the tool does
/// not run, and the model gets a tool error carrying the hook's reason.
#[test]
fn pre_tool_hook_denies_a_call_with_its_reason() {
    let mock = Mock::start(write_then_done());
    let dir = unique_dir("hook-deny");
    let hooks = r#"
[[hooks]]
event = "pre_tool"
match = "write|edit"
command = "echo 'no writes here' >&2; exit 2"
"#;
    let out = run_with_hooks(&mock, &dir, hooks, true);

    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(!dir.join("out.txt").exists(), "the denied write ran");
    assert_eq!(mock.requests(), 2);
    let followup = mock.body(1);
    assert!(
        followup.contains("error: blocked by a pre_tool hook: no writes here"),
        "{followup}"
    );
}

/// Hooks 2 — a `post_tool` hook's `context` is appended to the tool result
/// the model sees; the call itself ran. The hook also proves the event's
/// environment: it reports the file the call named.
#[test]
fn post_tool_hook_context_is_appended_to_the_result() {
    let mock = Mock::start(write_then_done());
    let dir = unique_dir("hook-post");
    let hooks = r#"
[[hooks]]
event = "post_tool"
match = "write"
command = '''printf '{"context": "formatted %s"}' "$(basename "$ATELIER_FILE")"'''
timeout_secs = 10
"#;
    let out = run_with_hooks(&mock, &dir, hooks, true);

    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        std::fs::read_to_string(dir.join("out.txt")).unwrap(),
        "written by tool"
    );
    let followup = mock.body(1);
    assert!(followup.contains("<hook-context>"), "{followup}");
    assert!(followup.contains("formatted out.txt"), "{followup}");
}

/// Hooks 3 — a `turn_end` hook that answers `continue` sends the agent back
/// to work: another model request, with the reason as a user message. (The
/// hook relents the second time.) A `session_start` hook's output rides
/// along with the first prompt.
#[test]
fn turn_end_hook_continue_produces_another_request() {
    let mock = Mock::start(always("done"));
    let dir = unique_dir("hook-continue");
    let hooks = r#"
[[hooks]]
event = "session_start"
command = "echo 'the branch is main'"

[[hooks]]
event = "turn_end"
command = '''
if [ ! -e .asked ]; then
  touch .asked
  echo '{"decision": "continue", "reason": "tests are failing"}'
fi
'''
"#;
    let out = run_with_hooks(&mock, &dir, hooks, true);

    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(mock.requests(), 2);
    assert!(mock.body(0).contains("the branch is main"));
    assert!(!mock.body(0).contains("tests are failing"));
    let second = mock.body(1);
    assert!(
        second.contains("[turn_end hook] tests are failing"),
        "{second}"
    );
}

/// Hooks 4 — a `turn_end` hook that is never satisfied is cut off: three
/// continues for one prompt, then the run ends normally with a notice.
#[test]
fn turn_end_hook_continues_are_capped() {
    let mock = Mock::start(always("done"));
    let dir = unique_dir("hook-cap");
    let hooks = r#"
[[hooks]]
event = "turn_end"
command = "echo 'never good enough' >&2; exit 2"
"#;
    let out = run_with_hooks(&mock, &dir, hooks, true);

    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(mock.requests(), 4);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("after 3 rounds"), "{stderr}");
}

/// Hooks 5 — a headless run never asks: in a project that is not trusted
/// the hooks are skipped, with one notice on stderr naming both ways to
/// accept them for a run.
#[test]
fn unapproved_project_hooks_are_skipped_headless() {
    let mock = Mock::start(write_then_done());
    let dir = unique_dir("hook-untrusted");
    let hooks = r#"
[[hooks]]
event = "pre_tool"
command = "exit 2"
"#;
    let out = run_with_hooks(&mock, &dir, hooks, false);

    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(dir.join("out.txt").exists(), "the hook ran unapproved");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(stderr.matches("project trust:").count(), 1, "{stderr}");
    assert!(stderr.contains("skipped 1 hook"), "{stderr}");
    assert!(stderr.contains("ATELIER_TRUST_PROJECT=1"), "{stderr}");
    assert!(stderr.contains("ATELIER_TRUST_HOOKS=1"), "{stderr}");
}

/// A responder whose first answer is an `ask` tool call (two options) and
/// whose later ones are `"ok"`.
fn ask_then_done() -> Responder {
    Arc::new(|n| {
        if n == 0 {
            sse_response(&[
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"ask","arguments":"{\"question\":\"Which one?\",\"options\":[{\"label\":\"Alpha\",\"description\":\"the first\"},{\"label\":\"Beta\"}]}"}}]}}]}"#,
            ])
        } else {
            sse_response(&[r#"{"choices":[{"index":0,"delta":{"content":"ok"}}]}"#])
        }
    })
}

/// Case 11 — the `ask` tool with `stream-json` both ways: the question is an
/// event, the parent's `answer` line on stdin becomes the tool result, and a
/// prompt sent ahead of the answer still runs afterwards.
#[test]
fn ask_is_answered_over_stream_json() {
    let mock = Mock::start(ask_then_done());
    let dir = unique_dir("ask-stream");

    let input = concat!(
        r#"{"type":"user","text":"choose"}"#,
        "\n",
        r#"{"type":"user","text":"and then"}"#,
        "\n",
        r#"{"type":"answer","id":1,"labels":["Gamma"]}"#,
        "\n",
        r#"{"type":"answer","id":1,"labels":["Beta"]}"#,
        "\n",
    );
    let out = run_atelier(
        &mock.base_url(),
        &dir,
        &[
            "-p",
            "--output-format",
            "stream-json",
            "--input-format",
            "stream-json",
        ],
        Some(input),
    );
    assert_eq!(out.status.code(), Some(0));
    let events = json_lines(&out);
    let questions = of_type(&events, "question");
    assert_eq!(questions.len(), 1, "{events:?}");
    assert_eq!(questions[0]["id"], 1);
    assert_eq!(questions[0]["question"], "Which one?");
    assert_eq!(questions[0]["options"][0]["description"], "the first");
    assert_eq!(questions[0]["options"][1]["label"], "Beta");
    assert_eq!(questions[0]["multi_select"], false);
    // The answer naming no option was refused, and the wait went on.
    assert!(
        of_type(&events, "notice")
            .iter()
            .any(|n| n["text"].as_str().unwrap().contains("Gamma")),
        "{events:?}"
    );
    assert!(
        mock.body(1)
            .contains(r#"The user chose the option \"Beta\"."#),
        "{}",
        mock.body(1)
    );
    // The early prompt ran as the second one.
    assert_eq!(of_type(&events, "result").len(), 2);
    assert!(mock.body(2).contains("and then"));
}

/// Case 12 — the `ask` tool with no one to ask (one-shot `--print`): the
/// model is told so instead of the run blocking on stdin.
#[test]
fn ask_without_an_interface_tells_the_model_to_decide() {
    let mock = Mock::start(ask_then_done());
    let dir = unique_dir("ask-print");

    let out = run_print(&mock.base_url(), &dir, "choose");
    assert_eq!(out.status.code(), Some(0));
    assert!(
        mock.body(1)
            .contains("cannot ask: this is a non-interactive session"),
        "{}",
        mock.body(1)
    );
}

/// Case 13 — the `ask` tool in the line REPL: the options are printed, an
/// answer that can't be taken gets the hint again, and the next line of
/// stdin is the answer.
#[test]
fn ask_is_answered_on_the_repl_input() {
    let mock = Mock::start(ask_then_done());
    let dir = unique_dir("ask-repl");

    let out = run_atelier(&mock.base_url(), &dir, &[], Some("choose\n1 2\n2\n"));
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("? Which one?"), "{stdout}");
    assert!(stdout.contains("  1. Alpha — the first"), "{stdout}");
    assert!(stdout.contains("  2. Beta"), "{stdout}");
    assert!(stdout.contains("choose one option"), "{stdout}");
    assert_eq!(stdout.matches("or type your own answer").count(), 2);
    assert!(
        mock.body(1)
            .contains(r#"The user chose the option \"Beta\"."#),
        "{}",
        mock.body(1)
    );
}

/// An interactive `atelier` (the line REPL, since stdin is a pipe) whose
/// prompts the test types one at a time, waiting on the mock in between.
struct Repl {
    child: std::process::Child,
}

impl Repl {
    fn start(base_url: &str, cwd: &Path, env: &[(&str, &str)]) -> Repl {
        let child = atelier(base_url, cwd)
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn atelier binary");
        Repl { child }
    }

    /// Type one line at the prompt.
    fn line(&mut self, text: &str) {
        writeln!(self.child.stdin.as_mut().expect("stdin open"), "{text}").expect("write a line");
    }

    /// End the input and collect everything printed (stdout, stderr). The
    /// output is small, so reading it only now cannot fill a pipe.
    fn finish(mut self) -> (String, String) {
        drop(self.child.stdin.take());
        let out = self.child.wait_with_output().expect("collect child output");
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
}

/// Block until the mock has received `n` requests, and return their bodies.
fn wait_requests(mock: &Mock, n: usize) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let bodies = mock.bodies.lock().unwrap().clone();
        if bodies.len() >= n {
            return bodies;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for request {n}; got {}",
            bodies.len()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// The `messages` array of a chat request body, exactly as it was sent.
/// (`messages` is the body's first key and `model` its second.)
fn raw_messages(body: &str) -> &str {
    let start = body.find(r#""messages":["#).expect("a messages array") + r#""messages":"#.len();
    let end = body.rfind(r#","model":"#).expect("model follows messages");
    &body[start..end]
}

/// The parsed `messages` of a chat request body.
fn messages(body: &str) -> Vec<serde_json::Value> {
    let request: serde_json::Value = serde_json::from_str(body).expect("request body is JSON");
    request["messages"].as_array().expect("messages").clone()
}

/// A streamed call of `tool` with the given JSON arguments.
fn tool_call_response(id: &str, tool: &str, arguments: &str) -> Vec<u8> {
    let chunk = serde_json::json!({"choices": [{"index": 0, "delta": {"tool_calls": [{
        "index": 0, "id": id, "type": "function",
        "function": {"name": tool, "arguments": arguments},
    }]}}]});
    sse_response(&[&chunk.to_string()])
}

/// A streamed plain answer.
fn answer_response(text: &str) -> Vec<u8> {
    let chunk = serde_json::json!({"choices": [{"index": 0, "delta": {"content": text}}]});
    sse_response(&[&chunk.to_string()])
}

/// Case 6 — the prompt cache survives. Over three turns (two of them with a
/// tool step) in a working tree that changes in between — a file the model
/// writes, a file the user adds — every request must be the previous one plus
/// new messages at the end: the system message and the tool list
/// byte-identical throughout, and no earlier message rewritten. That is the
/// condition for a server's prompt cache (llama.cpp's KV cache, a hosted
/// provider's) to be reused from one request to the next; the fresh project
/// context therefore travels with the user turn it was gathered for.
#[test]
fn each_request_is_a_prefix_of_the_next() {
    let responder: Responder = Arc::new(|n| match n {
        0 => tool_call_response("call_1", "write", r#"{"path":"a.txt","content":"alpha"}"#),
        1 => answer_response("one"),
        2 => tool_call_response("call_2", "read", r#"{"path":"a.txt"}"#),
        3 => answer_response("two"),
        _ => answer_response("three"),
    });
    let mock = Mock::start(responder);
    let dir = unique_dir("prefix");
    std::fs::write(dir.join("AGENTS.md"), "STANDING-RULE-5c1e").unwrap();
    // A repository makes `git status` one of the things that change; without
    // git on the machine the layout still does.
    let _ = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&dir)
        .output();

    let mut repl = Repl::start(&mock.base_url(), &dir, &[]);
    repl.line("first prompt");
    wait_requests(&mock, 2);
    // Between turns the tree changes under atelier (as does `a.txt`, written
    // by the model in the first turn).
    std::fs::write(dir.join("made-by-test.txt"), "beta").unwrap();
    repl.line("second prompt");
    wait_requests(&mock, 4);
    // Nothing changes before the third turn.
    repl.line("third prompt");
    let bodies = wait_requests(&mock, 5);
    let (stdout, stderr) = repl.finish();
    assert_eq!(bodies.len(), 5, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stdout.contains("three"),
        "stdout: {stdout}\nstderr: {stderr}"
    );

    let system = system_message(&bodies[0]);
    assert!(system.contains("STANDING-RULE-5c1e"), "{system}");
    assert!(
        !system.contains("## layout") && !system.contains("## environment"),
        "only standing context is in the system message: {system}"
    );
    let tools = |body: &str| -> serde_json::Value {
        serde_json::from_str::<serde_json::Value>(body).unwrap()["tools"].clone()
    };

    for (n, pair) in bodies.windows(2).enumerate() {
        let (before, after) = (messages(&pair[0]), messages(&pair[1]));
        assert!(
            before.len() < after.len(),
            "request {} adds messages",
            n + 1
        );
        for (i, message) in before.iter().enumerate() {
            assert_eq!(
                message,
                &after[i],
                "message {i} of request {n} was rewritten in request {}",
                n + 1
            );
        }
        assert_eq!(
            system,
            system_message(&pair[1]),
            "system message, request {}",
            n + 1
        );
        assert_eq!(
            tools(&pair[0]),
            tools(&pair[1]),
            "tool list, request {}",
            n + 1
        );
        // And on the wire, byte for byte: all of request n's array but its
        // closing bracket opens request n+1's.
        let (raw_before, raw_after) = (raw_messages(&pair[0]), raw_messages(&pair[1]));
        assert!(
            raw_after.starts_with(&raw_before[..raw_before.len() - 1]),
            "request {} does not start with the bytes of request {n}:\n{raw_before}\n{raw_after}",
            n + 1
        );
    }

    // The context is in the user turn it was gathered for: everything with
    // the first prompt, only what changed with the second, nothing with the
    // third.
    let last = messages(&bodies[4]);
    let user_turn = |prompt: &str| -> String {
        last.iter()
            .filter_map(|m| m["content"].as_str())
            .find(|c| c.ends_with(prompt))
            .unwrap_or_else(|| panic!("no user message for {prompt:?}"))
            .to_string()
    };
    let first = user_turn("first prompt");
    assert!(first.starts_with("<context>\n"), "{first}");
    assert!(first.contains("## environment"), "{first}");
    assert!(first.contains("</context>\n\nfirst prompt"), "{first}");
    let second = user_turn("second prompt");
    assert!(second.starts_with("<context>\n"), "{second}");
    assert!(
        second.contains("made-by-test.txt") && second.contains("a.txt"),
        "the changed tree is described: {second}"
    );
    assert!(
        !second.contains("## environment") && second.contains("(unchanged since last shown: "),
        "unchanged items are named, not repeated: {second}"
    );
    assert_eq!(user_turn("third prompt"), "third prompt");
}

/// An HTTP error response with a JSON body, as an endpoint refusing a request
/// sends it.
fn error_response(status: u16, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} Error\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    )
    .into_bytes()
}

/// A streamed plain answer followed by a usage chunk reporting `total` tokens.
fn answer_with_usage(text: &str, total: u32) -> Vec<u8> {
    let answer = serde_json::json!({"choices": [{"index": 0, "delta": {"content": text}}]});
    let usage = serde_json::json!({"choices": [], "usage": {
        "prompt_tokens": total, "completion_tokens": 0, "total_tokens": total,
    }});
    sse_response(&[&answer.to_string(), &usage.to_string()])
}

/// Case 7 — a request the endpoint refuses as too long is compacted and
/// retried instead of failing the turn. The model reads a large file; the
/// follow-up request is answered with a context-length error; atelier prunes
/// the file's content down to a stub (the turn in progress can't be
/// summarized, and nothing came before it) and sends the request again.
#[test]
fn a_request_rejected_as_too_long_is_compacted_and_retried() {
    let responder: Responder = Arc::new(|n| match n {
        0 => tool_call_response("call_1", "read", r#"{"path":"big.txt"}"#),
        1 => error_response(
            400,
            r#"{"error":{"message":"This model's maximum context length is 4096 tokens.","code":"context_length_exceeded"}}"#,
        ),
        _ => answer_response("done"),
    });
    let mock = Mock::start(responder);
    let dir = unique_dir("overflow");
    // Larger than the recent tail kept verbatim (a share of the default
    // threshold), yet small enough that the tool result `--print` echoes on
    // stderr fits the pipe `run_print` only reads at exit.
    let big: String = (1..=400)
        .map(|i| format!("line {i} of the big file, padded out a little\n"))
        .collect();
    std::fs::write(dir.join("big.txt"), big).unwrap();

    let out = run_print(&mock.base_url(), &dir, "read the big file");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let bodies = mock.bodies.lock().unwrap().clone();
    assert_eq!(
        bodies.len(),
        3,
        "tool call, refused, retried.\n stderr: {stderr}"
    );
    assert!(
        stdout.contains("done"),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("maximum context length is 4096 tokens")
            && stderr.contains("compacting and retrying"),
        "the refusal is shown: {stderr}"
    );
    assert!(
        stderr.contains("compacted: pruned 1 old tool result(s) — ~"),
        "{stderr}"
    );

    assert!(bodies[1].contains("line 200 of the big file"));
    assert!(
        !bodies[2].contains("line 200 of the big file"),
        "the file's content is gone from the retried request"
    );
    let (refused, retried) = (messages(&bodies[1]), messages(&bodies[2]));
    assert_eq!(refused.len(), retried.len(), "no message was dropped");
    let result = retried.last().unwrap();
    assert_eq!(result["role"], "tool");
    assert_eq!(
        result["tool_call_id"], "call_1",
        "the call keeps its result"
    );
    let stub = result["content"].as_str().unwrap();
    assert!(
        stub.starts_with("[output of read big.txt — ") && stub.ends_with(" lines, pruned]"),
        "{stub}"
    );
}

/// Any other refusal still fails the turn, with what the server said.
#[test]
fn another_http_error_fails_the_turn_with_the_servers_message() {
    let responder: Responder =
        Arc::new(|_| error_response(400, r#"{"error":{"message":"unknown model 'mock'"}}"#));
    let mock = Mock::start(responder);
    let dir = unique_dir("http-error");

    let out = run_print(&mock.base_url(), &dir, "hi");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert_eq!(mock.bodies.lock().unwrap().len(), 1, "not retried");
    assert!(
        stderr.contains("provider returned HTTP 400: unknown model 'mock'"),
        "{stderr}"
    );
}

/// Case 8 — `/compact [instructions]`. The older turn is replaced by a
/// structured hand-off summary: the summary request asks for the fixed
/// sections and carries the user's instructions; afterwards the summary
/// closes the system message, the summarized turn is gone from the request,
/// the kept turn is intact, and the next turn gets its context afresh. The
/// summary is saved, so `--continue` picks it up.
#[test]
fn compact_command_replaces_older_turns_with_a_hand_off_summary() {
    let responder: Responder = Arc::new(|n| match n {
        0 => answer_response("the port is 8123"),
        1 => answer_response("noted"),
        2 => answer_response("## Goal\nHANDOFF-NOTES-77"),
        _ => answer_response("fine"),
    });
    let mock = Mock::start(responder);
    let dir = unique_dir("compact");

    let mut repl = Repl::start(&mock.base_url(), &dir, &[]);
    repl.line("which port does it use?");
    wait_requests(&mock, 1);
    repl.line("second question");
    wait_requests(&mock, 2);
    repl.line("/compact keep the port number");
    wait_requests(&mock, 3);
    repl.line("third question");
    let bodies = wait_requests(&mock, 4);
    let (stdout, stderr) = repl.finish();

    // The summary request: no tools, the hand-off sections, the transcript
    // of the older turn only, the user's instructions.
    let request: serde_json::Value = serde_json::from_str(&bodies[2]).unwrap();
    assert!(request.get("tools").is_none(), "no tools to summarize with");
    let summary_request = messages(&bodies[2]);
    assert_eq!(summary_request.len(), 2);
    let asked = summary_request[0]["content"].as_str().unwrap();
    for section in [
        "## Goal",
        "## Decisions",
        "## Current state",
        "## Files touched",
        "## Tried and failed",
        "## Next steps",
        "## From the user",
    ] {
        assert!(asked.contains(section), "{section} in: {asked}");
    }
    let transcript = summary_request[1]["content"].as_str().unwrap();
    assert!(
        transcript.contains("user: which port does it use?")
            && transcript.contains("assistant: the port is 8123"),
        "{transcript}"
    );
    assert!(
        !transcript.contains("second question"),
        "the last turn stays verbatim: {transcript}"
    );
    assert!(transcript.ends_with("keep the port number"), "{transcript}");

    assert!(
        stdout.contains("compacted: summarized 2 earlier message(s) — ~")
            && stdout.contains("tokens (estimated)"),
        "stdout: {stdout}\nstderr: {stderr}"
    );

    // The next request: summary at the end of the system message, the
    // summarized turn gone, the kept one intact, context sent again.
    let system = system_message(&bodies[3]);
    assert!(
        system.ends_with("Summary of earlier conversation:\n## Goal\nHANDOFF-NOTES-77"),
        "{system}"
    );
    assert!(system.starts_with(&system_message(&bodies[1])));
    let after = messages(&bodies[3]);
    let texts: Vec<&str> = after.iter().filter_map(|m| m["content"].as_str()).collect();
    assert_eq!(after.len(), 4, "system, kept turn, new prompt: {texts:?}");
    assert!(!bodies[3].contains("which port does it use?"));
    assert!(texts[1].ends_with("second question"));
    assert_eq!(texts[2], "noted");
    assert!(
        texts[3].starts_with("<context>\n") && texts[3].contains("## environment"),
        "context is sent afresh after a summary: {}",
        texts[3]
    );

    // Saved with the session, and back with `--continue`.
    let saved = std::fs::read_to_string(dir.join(".atelier/session.json")).unwrap();
    assert!(saved.contains("HANDOFF-NOTES-77"), "{saved}");
    let out = atelier(&mock.base_url(), &dir)
        .args(["--continue", "--print", "and again"])
        .stdin(Stdio::null())
        .output()
        .expect("run atelier --continue");
    assert!(out.status.success());
    let bodies = mock.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 5);
    assert!(system_message(&bodies[4]).contains("HANDOFF-NOTES-77"));
    assert!(bodies[4].contains("third question"));
}

/// Case 9 — automatic compaction. Once the endpoint reports a request past
/// the threshold, the turns before the last one are summarized after the
/// turn — but not while there is only one turn, which can't be split.
#[test]
fn passing_the_threshold_summarizes_older_turns_after_the_turn() {
    let responder: Responder = Arc::new(|n| match n {
        0 => answer_with_usage("one", 9_000),
        1 => answer_with_usage("two", 9_000),
        2 => answer_response("AUTO-NOTES-31"),
        _ => answer_response("three"),
    });
    let mock = Mock::start(responder);
    let dir = unique_dir("auto-compact");

    let mut repl = Repl::start(&mock.base_url(), &dir, &[("ATELIER_CONTEXT_LIMIT", "50")]);
    repl.line("first prompt");
    wait_requests(&mock, 1);
    repl.line("second prompt");
    // The second turn's answer is followed by the summary request.
    wait_requests(&mock, 3);
    repl.line("third prompt");
    let bodies = wait_requests(&mock, 4);
    let (stdout, stderr) = repl.finish();
    assert_eq!(bodies.len(), 4, "stdout: {stdout}\nstderr: {stderr}");

    // After one turn there was nothing to summarize: request 1 is turn two.
    assert!(bodies[1].contains("first prompt") && bodies[1].contains("second prompt"));
    let transcript = messages(&bodies[2])[1]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        transcript.contains("user: first prompt") && !transcript.contains("second prompt"),
        "{transcript}"
    );
    assert!(
        stdout.contains("compacted: summarized 2 earlier message(s)"),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(system_message(&bodies[3]).ends_with("AUTO-NOTES-31"));
    assert!(!bodies[3].contains("first prompt"));
    assert!(bodies[3].contains("second prompt") && bodies[3].contains("third prompt"));
}

/// Case 10 — `/context` accounts for the window: before any turn only the
/// fixed parts, afterwards also the standing instructions, the turn's
/// context block by item, the conversation by role, and the endpoint's own
/// count for the last request.
#[test]
fn context_command_shows_where_the_window_goes() {
    let responder: Responder = Arc::new(|n| match n {
        0 => tool_call_response("call_1", "read", r#"{"path":"notes.txt"}"#),
        _ => answer_with_usage("read it", 4_321),
    });
    let mock = Mock::start(responder);
    let dir = unique_dir("context");
    std::fs::write(dir.join("AGENTS.md"), "STANDING-RULE-2b7d").unwrap();
    std::fs::write(dir.join("notes.txt"), "some notes\n").unwrap();

    let mut repl = Repl::start(&mock.base_url(), &dir, &[]);
    repl.line("/context");
    repl.line("read the notes");
    wait_requests(&mock, 2);
    repl.line("/context");
    let (stdout, stderr) = repl.finish();

    let reports: Vec<&str> = stdout.split("context window — ").skip(1).collect();
    assert_eq!(reports.len(), 2, "stdout: {stdout}\nstderr: {stderr}");
    let (before, after) = (reports[0], reports[1]);

    assert!(
        before.contains("share of the compaction threshold (8,000)"),
        "{before}"
    );
    assert!(before.contains("  system prompt "), "{before}");
    assert!(before.contains("  tool list ("), "{before}");
    assert!(before.contains("  history: user (0)"), "{before}");
    assert!(
        before.contains("last request: no usage reported"),
        "{before}"
    );
    assert!(
        before.contains("(project context is gathered when you send a message)"),
        "{before}"
    );
    assert!(!before.contains("standing: "), "{before}");

    for line in [
        "  standing: project instructions ",
        "  turn context (1 block(s)) ",
        "    environment ",
        "  history: user (1) ",
        "  history: assistant (2) ",
        "  history: tool results (1) ",
        "  total ",
        "last request: 4,321 tokens as counted by the endpoint",
    ] {
        assert!(after.contains(line), "{line:?} missing from:\n{after}");
    }
    assert!(!after.contains("gathered when you send"), "{after}");
    // Every row carries an estimate and its share of the threshold.
    let total = after
        .lines()
        .find(|l| l.starts_with("  total "))
        .expect("a total row");
    assert!(total.trim_end().ends_with('%'), "{total}");
}

/// An SSE response carrying one tool call.
fn json_tool_call(id: &str, name: &str, arguments: serde_json::Value) -> Vec<u8> {
    tool_calls_response(&[(id, name, arguments)])
}

/// An SSE response carrying several tool calls in one assistant message.
fn tool_calls_response(calls: &[(&str, &str, serde_json::Value)]) -> Vec<u8> {
    let calls: Vec<serde_json::Value> = calls
        .iter()
        .enumerate()
        .map(|(index, (id, name, arguments))| {
            serde_json::json!({
                "index": index,
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": arguments.to_string() },
            })
        })
        .collect();
    let chunk = serde_json::json!({"choices":[{"index":0,"delta":{"tool_calls":calls}}]});
    sse_response(&[&chunk.to_string()])
}

/// An SSE response carrying `text` as the answer.
fn text_response(text: &str) -> Vec<u8> {
    let chunk = serde_json::json!({"choices":[{"index":0,"delta":{"content":text}}]});
    sse_response(&[&chunk.to_string()])
}

/// A captured request, parsed.
fn request(body: &str) -> serde_json::Value {
    serde_json::from_str(body).expect("request body is JSON")
}

/// The text of every message with `role` in a request.
fn messages_of(request: &serde_json::Value, role: &str) -> Vec<String> {
    request["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|m| m["role"] == role)
        .map(|m| m["content"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// The names of the tools a request offers, sorted (empty when it offers
/// none).
fn tool_names(request: &serde_json::Value) -> Vec<String> {
    let mut names: Vec<String> = request["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .map(|t| t["function"]["name"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// The requests the mock received whose system message contains `marker`.
fn requests_with_system(mock: &Mock, marker: &str) -> Vec<serde_json::Value> {
    (0..mock.requests())
        .map(|n| mock.body(n))
        .filter(|b| system_message(b).contains(marker))
        .map(|b| request(&b))
        .collect()
}

/// What opens the built-in `explore` agent's system prompt.
const EXPLORE_MARKER: &str = "read-only exploration agent";
/// What opens atelier's own system prompt (the parent's).
const PARENT_MARKER: &str = "You are atelier";

/// Subagents — the `task` tool end to end. The parent model calls `task`
/// with the built-in `explore` agent; the child is a separate conversation
/// (the agent's system prompt, only read-only tools, nothing of the parent's
/// conversation) that reads a file and answers; that answer is the parent's
/// tool result, and the parent then answers the user. The transcript shows
/// the child as a start line, its tool call, and an end line.
#[test]
fn task_runs_a_subagent_and_returns_its_report() {
    let mock = Mock::by_body(Arc::new(|body| {
        let child = system_message(body).contains(EXPLORE_MARKER);
        let answered = !messages_of(&request(body), "tool").is_empty();
        match (child, answered) {
            (false, false) => json_tool_call(
                "call_task",
                "task",
                serde_json::json!({
                    "agent": "explore",
                    "prompt": "CHILD-TASK: say what notes.txt holds",
                    "description": "inspect the notes",
                }),
            ),
            (true, false) => json_tool_call(
                "call_read",
                "read",
                serde_json::json!({"path": "notes.txt"}),
            ),
            (true, true) => text_response("CHILD-REPORT: notes.txt:1 holds the marker"),
            (false, true) => text_response("parent done"),
        }
    }));
    let dir = unique_dir("task");
    std::fs::write(dir.join("notes.txt"), "MARKER-IN-FILE\n").unwrap();
    std::fs::write(dir.join("AGENTS.md"), "PROJECT-RULES apply to everyone").unwrap();

    let out = run_atelier(
        &mock.base_url(),
        &dir,
        &[
            "-p",
            "--output-format",
            "stream-json",
            "PARENT-PROMPT: look into the notes",
        ],
        None,
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert_eq!(mock.requests(), 4, "stderr: {stderr}");

    // The parent is offered `task`, described with the agents it can run.
    let parent = requests_with_system(&mock, PARENT_MARKER);
    assert_eq!(parent.len(), 2);
    let task_spec = parent[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["function"]["name"] == "task")
        .expect("the parent has the task tool");
    let description = task_spec["function"]["description"].as_str().unwrap();
    assert!(description.contains("- explore: ") && description.contains("- general: "));

    // The child: its own system prompt (with the project's standing
    // instructions), the read-only tools and no `task`, and a conversation
    // that starts at the prompt it was given.
    let child = requests_with_system(&mock, EXPLORE_MARKER);
    assert_eq!(child.len(), 2);
    let system = child[0]["messages"][0]["content"].as_str().unwrap();
    assert!(!system.contains(PARENT_MARKER), "{system}");
    assert!(system.contains("PROJECT-RULES"), "{system}");
    assert_eq!(
        tool_names(&child[0]),
        [
            "glob",
            "grep",
            "ls",
            "read",
            "tree",
            "web_fetch",
            "web_search"
        ]
    );
    assert_eq!(
        messages_of(&child[0], "user"),
        ["CHILD-TASK: say what notes.txt holds"]
    );
    assert!(!child[0].to_string().contains("PARENT-PROMPT"));
    // It read the file.
    let read = messages_of(&child[1], "tool");
    assert!(read[0].contains("MARKER-IN-FILE"), "{read:?}");

    // Its answer is the parent's tool result, with the footer; the child's
    // own reading is not.
    let results = messages_of(&parent[1], "tool");
    assert_eq!(results.len(), 1);
    assert!(
        results[0].starts_with("CHILD-REPORT: notes.txt:1 holds the marker\n\n[explore: 2 steps"),
        "{results:?}"
    );
    assert!(!parent[1].to_string().contains("MARKER-IN-FILE"));

    // What the user sees: one tool call (the task), the child's lines as
    // info events, and the parent's answer — never the child's text.
    let events = json_lines(&out);
    let starts = of_type(&events, "tool_start");
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0]["name"], "task");
    let ends = of_type(&events, "tool_end");
    assert_eq!((ends.len(), &ends[0]["ok"]), (1, &serde_json::json!(true)));
    let info: Vec<&str> = of_type(&events, "info")
        .iter()
        .map(|e| e["text"].as_str().unwrap())
        .collect();
    assert_eq!(
        info,
        [
            "⤷ explore: inspect the notes",
            "  ⤷ read notes.txt",
            "⤶ explore: done (2 steps)"
        ]
    );
    let texts: Vec<&str> = of_type(&events, "text")
        .iter()
        .map(|e| e["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts, ["parent done"]);
    let result = events.last().unwrap();
    assert_eq!(result["result"], "parent done");
    // The parent's own steps; the child's are in its footer.
    assert_eq!(result["num_steps"], 2);
}

/// Subagents — agents as files, and the step cap. A project file named
/// `explore` replaces the built-in agent (its prompt, its tools, its cap).
/// With `max_steps: 1` the child gets one request with tools; the next is
/// sent without any and ends with the instruction to report, and what the
/// model answers to that is the result — a report, not an error.
#[test]
fn a_project_agent_overrides_the_builtin_and_wraps_up_at_its_step_cap() {
    let mock = Mock::by_body(Arc::new(|body| {
        let req = request(body);
        let child = system_message(body).contains("CUSTOM-EXPLORE-PROMPT");
        let answered = !messages_of(&req, "tool").is_empty();
        match (child, answered) {
            (false, false) => json_tool_call(
                "call_task",
                "task",
                serde_json::json!({"agent": "explore", "prompt": "count the files"}),
            ),
            // The child keeps asking for tools as long as it is offered any.
            (true, _) if !tool_names(&req).is_empty() => {
                json_tool_call("call_read", "read", serde_json::json!({"path": "a.txt"}))
            }
            (true, _) => text_response("WRAP-UP: read a.txt; b.txt is left"),
            (false, true) => text_response("parent done"),
        }
    }));
    let dir = unique_dir("task-cap");
    std::fs::write(dir.join("a.txt"), "file a\n").unwrap();
    let agents = dir.join(".atelier").join("agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("explore.md"),
        "---\ndescription: The project's own explorer\ntools:\n  - read\n  - task\nmax_steps: 1\n---\n\nCUSTOM-EXPLORE-PROMPT: look, don't touch.\n",
    )
    .unwrap();

    let out = run_print(&mock.base_url(), &dir, "how many files?");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("parent done"),
        "stderr: {stderr}"
    );

    // The parent is told about the file's agent, not the built-in.
    let parent = requests_with_system(&mock, PARENT_MARKER);
    assert!(
        parent[0]
            .to_string()
            .contains("- explore: The project's own explorer")
    );
    assert_eq!(requests_with_system(&mock, EXPLORE_MARKER).len(), 0);

    // The child: the file's prompt and tools — without `task`, though the
    // file lists it.
    let child = requests_with_system(&mock, "CUSTOM-EXPLORE-PROMPT");
    assert_eq!(child.len(), 2, "stderr: {stderr}");
    assert_eq!(tool_names(&child[0]), ["read"]);

    // The request past the cap: no tools, and the last word is the
    // instruction to stop and report.
    assert!(child[1].get("tools").is_none(), "{}", child[1]);
    let users = messages_of(&child[1], "user");
    assert_eq!(users[0], "count the files");
    assert!(users.last().unwrap().contains("step limit"), "{users:?}");
    assert!(users.last().unwrap().contains("final report"));

    let results = messages_of(&parent[1], "tool");
    assert!(
        results[0].starts_with("WRAP-UP: read a.txt; b.txt is left"),
        "{results:?}"
    );
    assert!(results[0].contains("stopped at its cap of 1 steps"));
    assert!(stderr.contains("wrapped up at its step cap"), "{stderr}");
}

/// Subagents — several `task` calls in one step. Both children run (at the
/// same time: the mock answers by what a request says, not by its order),
/// each result goes to its own call, in call order; and a child that asks
/// for `task` — which it is never offered — cannot start a subagent of its
/// own.
#[test]
fn task_calls_in_one_step_run_together_and_children_cannot_nest() {
    let mock = Mock::by_body(Arc::new(|body| {
        let req = request(body);
        let child = system_message(body).contains("general-purpose agent");
        let users = messages_of(&req, "user").join("\n");
        let answered = !messages_of(&req, "tool").is_empty();
        match (child, answered) {
            (false, false) => tool_calls_response(&[
                (
                    "call_a",
                    "task",
                    serde_json::json!({"agent": "general", "prompt": "TASK-ALPHA"}),
                ),
                (
                    "call_b",
                    "task",
                    serde_json::json!({"agent": "general", "prompt": "TASK-BETA"}),
                ),
            ]),
            // Alpha tries to delegate in turn.
            (true, false) if users.contains("TASK-ALPHA") => json_tool_call(
                "call_nested",
                "task",
                serde_json::json!({"agent": "general", "prompt": "TASK-NESTED"}),
            ),
            (true, true) if users.contains("TASK-ALPHA") => text_response("report-alpha"),
            (true, _) => text_response("report-beta"),
            (false, true) => text_response("parent done"),
        }
    }));
    let dir = unique_dir("task-parallel");

    let out = run_atelier(
        &mock.base_url(),
        &dir,
        &["-p", "--output-format", "stream-json", "do both"],
        None,
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");

    // `general` has every tool the parent has, except `task`.
    let parent = requests_with_system(&mock, PARENT_MARKER);
    let child = requests_with_system(&mock, "general-purpose agent");
    let mut expected = tool_names(&parent[0]);
    assert!(expected.contains(&"task".to_string()) && expected.contains(&"bash".to_string()));
    expected.retain(|t| t != "task");
    for request in &child {
        assert_eq!(tool_names(request), expected);
    }
    // Nothing ever ran the nested task: the call failed as an unknown tool.
    assert!((0..mock.requests()).all(|n| {
        !messages_of(&request(&mock.body(n)), "user").contains(&"TASK-NESTED".to_string())
    }));
    let alpha_second = child
        .iter()
        .find(|r| !messages_of(r, "tool").is_empty())
        .expect("alpha went on after its refused call");
    assert!(messages_of(alpha_second, "tool")[0].contains("unknown tool 'task'"));

    // Results in call order, each under its own call id.
    let results: Vec<(&str, &str)> = parent[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| {
            (
                m["tool_call_id"].as_str().unwrap(),
                m["content"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].0, "call_a");
    assert!(
        results[0]
            .1
            .starts_with("report-alpha\n\n[general#1: 2 steps")
    );
    assert_eq!(results[1].0, "call_b");
    assert!(
        results[1]
            .1
            .starts_with("report-beta\n\n[general#2: 1 step")
    );

    // Each child has its lines, named apart.
    let events = json_lines(&out);
    let info: Vec<&str> = of_type(&events, "info")
        .iter()
        .map(|e| e["text"].as_str().unwrap())
        .collect();
    for line in [
        "⤷ general#1: TASK-ALPHA",
        "⤷ general#2: TASK-BETA",
        "  ⤷ general#1 · task",
        "⤶ general#1: done (2 steps)",
        "⤶ general#2: done (1 step)",
    ] {
        assert!(info.contains(&line), "missing {line:?} in {info:?}");
    }
    assert_eq!(of_type(&events, "tool_start").len(), 2);
    assert_eq!(of_type(&events, "tool_end").len(), 2);
    assert_eq!(events.last().unwrap()["result"], "parent done");
}

/// Subagents — approvals. Without auto-approve, a child's unconfined call
/// (`bash`) is put to the parent's user like the parent's own, after a line
/// naming the agent that asks; a refusal is the child's tool result. And
/// `/agents` lists what can be run.
#[cfg(unix)]
#[test]
fn a_subagents_call_asks_the_user_and_names_the_agent() {
    let mock = Mock::by_body(Arc::new(|body| {
        let child = system_message(body).contains("general-purpose agent");
        let results = messages_of(&request(body), "tool").len();
        let bash = |id: &str, command: &str| {
            json_tool_call(id, "bash", serde_json::json!({"command": command}))
        };
        match (child, results) {
            (false, 0) => json_tool_call(
                "call_task",
                "task",
                serde_json::json!({"agent": "general", "prompt": "make the files"}),
            ),
            (true, 0) => bash("call_1", "echo one > first.txt"),
            (true, 1) => bash("call_2", "echo two > second.txt"),
            (true, _) => text_response("child report"),
            (false, _) => text_response("parent done"),
        }
    }));
    let dir = unique_dir("task-approval");

    // The prompt, then the answers to the two approval prompts.
    let out = run_repl(&mock.base_url(), &dir, "go\ny\nn\n/agents\n");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert_eq!(
        stdout
            .matches("⤷ the subagent general asks to run:")
            .count(),
        2,
        "stdout: {stdout}\n stderr: {stderr}"
    );
    assert_eq!(stdout.matches("allow tool 'bash'").count(), 2, "{stdout}");
    // Approved once, refused once.
    assert_eq!(
        std::fs::read_to_string(dir.join("first.txt")).unwrap(),
        "one\n"
    );
    assert!(!dir.join("second.txt").exists());
    let child = requests_with_system(&mock, "general-purpose agent");
    assert_eq!(child.len(), 3, "stdout: {stdout}\n stderr: {stderr}");
    let results = messages_of(&child[2], "tool");
    assert!(results[1].contains("denied permission"), "{results:?}");
    assert!(stdout.contains("parent done"));

    // `/agents`.
    assert!(stdout.contains("2 agent(s):"), "{stdout}");
    assert!(stdout.contains("  explore — Read-only search"));
    assert!(stdout.contains("built-in · tools: read, grep, glob, ls, tree, web_fetch, web_search"));
}

/// A responder for a `write` of `out.txt`, then a `read` of `seed.txt`, then
/// the answer `"done"` — one tool that modifies and one that does not.
fn write_read_done() -> Responder {
    Arc::new(|n| match n {
        0 => sse_response(&[
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"write","arguments":"{\"path\":\"out.txt\",\"content\":\"written by tool\"}"}}]}}]}"#,
        ]),
        1 => sse_response(&[
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_2","type":"function","function":{"name":"read","arguments":"{\"path\":\"seed.txt\"}"}}]}}]}"#,
        ]),
        _ => sse_response(&[r#"{"choices":[{"index":0,"delta":{"content":"done"}}]}"#]),
    })
}

/// Plan mode end to end, however it is selected: the `write` is refused with
/// the message that tells the model what to do instead and no file appears,
/// the `read` after it works, and the model was told the mode up front.
#[test]
fn plan_mode_refuses_write_and_allows_read() {
    type Select = (
        &'static [&'static str],
        &'static [(&'static str, Option<&'static str>)],
        Option<&'static str>,
    );
    let selections: [Select; 3] = [
        // The flag wins over ATELIER_APPROVE=all, which the harness sets.
        (&["--mode", "plan"], &[], None),
        (&[], &[("ATELIER_MODE", Some("plan"))], None),
        (
            &[],
            &[("ATELIER_APPROVE", None)],
            Some("[defaults]\nmode = \"plan\"\n"),
        ),
    ];
    for (i, (flags, env, toml)) in selections.into_iter().enumerate() {
        let mock = Mock::start(write_read_done());
        let dir = unique_dir(&format!("plan-{i}"));
        std::fs::write(dir.join("seed.txt"), "seed contents").unwrap();
        if let Some(toml) = toml {
            std::fs::write(dir.join("atelier.toml"), toml).unwrap();
        }
        let mut args = vec!["--print"];
        args.extend(flags);
        args.push("change things");

        let out = run_atelier_setenv(&mock.base_url(), &dir, &args, None, env);

        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "case {i}: {stderr}");
        assert!(!dir.join("out.txt").exists(), "case {i}: the write ran");
        assert_eq!(mock.requests(), 3, "case {i}: {stderr}");
        assert!(
            mock.body(0).contains("PLAN MODE"),
            "case {i}: the mode was not stated to the model"
        );
        let after_write = mock.body(1);
        assert!(
            after_write.contains("plan mode: write is not available")
                && after_write.contains("present a plan"),
            "case {i}: {after_write}"
        );
        assert!(
            mock.body(2).contains("seed contents"),
            "case {i}: the read did not run"
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains("done"));
    }
}

/// `ask-edits` with nobody to ask (`--print`): the `write` is denied and no
/// file appears; the `read` still runs.
#[test]
fn ask_edits_headless_denies_write() {
    let mock = Mock::start(write_read_done());
    let dir = unique_dir("ask-edits");
    std::fs::write(dir.join("seed.txt"), "seed contents").unwrap();

    let args = ["--print", "--mode=ask-edits", "write the file"];
    let out = run_atelier(&mock.base_url(), &dir, &args, None);

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(!dir.join("out.txt").exists(), "the write ran");
    assert!(
        mock.body(1).contains("the user denied permission"),
        "{}",
        mock.body(1)
    );
    assert!(!mock.body(0).contains("PLAN MODE"));
    assert!(mock.body(2).contains("seed contents"));
}

/// With no mode chosen and no `ATELIER_APPROVE`, nothing changed: a `write`
/// inside the project runs without anyone being asked.
#[test]
fn default_mode_still_writes_without_asking() {
    for env in [
        &[("ATELIER_APPROVE", None)][..],
        &[("ATELIER_MODE", Some("auto-edit"))],
    ] {
        let mock = Mock::start(write_read_done());
        let dir = unique_dir("auto-edit");
        std::fs::write(dir.join("seed.txt"), "seed contents").unwrap();

        let out = run_atelier_setenv(&mock.base_url(), &dir, &["--print", "go"], None, env);

        assert!(out.status.success());
        assert_eq!(
            std::fs::read_to_string(dir.join("out.txt")).unwrap(),
            "written by tool"
        );
        assert!(!mock.body(0).contains("PLAN MODE"));
    }
}

/// A mode that does not exist stops the run (exit 2) before any request: a
/// misspelt `plan` must not become a mode that edits.
#[test]
fn unknown_mode_is_a_usage_error() {
    let mock = Mock::start(write_then_done());
    let dir = unique_dir("bad-mode");
    let by_flag = run_atelier(
        &mock.base_url(),
        &dir,
        &["--print", "--mode", "pln", "go"],
        None,
    );
    let by_env = run_atelier_setenv(
        &mock.base_url(),
        &dir,
        &["--print", "go"],
        None,
        &[("ATELIER_MODE", Some("pln"))],
    );
    for out in [by_flag, by_env] {
        assert_eq!(out.status.code(), Some(2));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("unknown mode 'pln'"), "{stderr}");
    }
    assert_eq!(mock.requests(), 0);
    assert!(!dir.join("out.txt").exists());
}

/// `atelier permissions check --mode`: the mode's part in a decision.
#[test]
fn permissions_check_takes_a_mode() {
    let dir = unique_dir("check-mode");
    std::fs::write(
        dir.join("atelier.toml"),
        "[permissions]\nallow = [\"bash(git log *)\"]\n",
    )
    .unwrap();
    // The allow rule is the project's: it counts once the project is
    // trusted.
    trust_project(&dir);
    let check = |args: &[&str]| {
        let mut full = vec!["permissions", "check"];
        full.extend(args);
        let out = run_atelier_setenv(
            "http://127.0.0.1:9",
            &dir,
            &full,
            None,
            &[("ATELIER_APPROVE", None)],
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let write = ["--tool", "write", "--args", r#"{"path": "a.txt"}"#];

    let out = check(&[&["--mode", "plan"][..], &write].concat());
    assert!(out.starts_with("decision: deny"), "{out}");
    assert!(out.contains("mode:     plan"), "{out}");
    let out = check(&[&["--mode=ask-edits"][..], &write].concat());
    assert!(out.starts_with("decision: ask"), "{out}");
    let out = check(&write);
    assert!(out.starts_with("decision: allow"), "{out}");

    let out = check(&["--mode", "plan", "--", "git", "log", "-3"]);
    assert!(out.starts_with("decision: allow"), "{out}");
    let out = check(&["--mode", "plan", "--", "git", "push"]);
    assert!(out.starts_with("decision: deny"), "{out}");
    let out = check(&["--mode", "yolo", "--", "git", "push"]);
    assert!(out.starts_with("decision: allow"), "{out}");
}

/// A `200 OK` JSON response (what `GET /models` gets).
fn json_response(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    )
    .into_bytes()
}

/// The parsed body of the Nth request a mock received.
fn request_json(mock: &Mock, n: usize) -> serde_json::Value {
    serde_json::from_str(&mock.body(n)).expect("request body is JSON")
}

/// Case 14 — named providers. With a second endpoint configured in
/// `atelier.toml`, a model addressed as `second/model-b` is requested from
/// that endpoint — bare model id, the provider's key and extra header — and
/// the endpoint from the environment sees nothing.
#[test]
fn a_model_on_a_named_provider_goes_to_that_endpoint() {
    let first = Mock::start(always("from the first"));
    let second = Mock::start(always("from the second"));
    let dir = unique_dir("providers");
    std::fs::write(
        dir.join("atelier.toml"),
        format!(
            "[[provider]]\nname = \"second\"\nbase_url = \"{}\"\n\
             api_key_env = \"SECOND_KEY\"\nheaders = {{ \"X-Org\" = \"abc\" }}\n",
            second.base_url()
        ),
    )
    .unwrap();
    trust_project(&dir);

    let out = run_atelier_env(
        &first.base_url(),
        &dir,
        &["--print", "hi"],
        None,
        &[
            ("ATELIER_MODEL", "second/model-b"),
            ("SECOND_KEY", "k-second-123"),
            // The environment's own key belongs to the environment's endpoint.
            ("ATELIER_API_KEY", "k-env-456"),
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stdout.contains("from the second"), "{stdout}\n{stderr}");
    assert_eq!(first.requests(), 0, "the env endpoint is not asked");
    assert_eq!(second.requests(), 1);
    assert_eq!(request_json(&second, 0)["model"], "model-b");
    let head = second.head(0);
    assert!(head.starts_with("post /v1/chat/completions "), "{head}");
    assert!(
        head.contains("authorization: bearer k-second-123"),
        "{head}"
    );
    assert!(head.contains("x-org: abc"), "{head}");
    assert!(!head.contains("k-env-456"), "{head}");

    // And a bare name still means the endpoint from the environment, with
    // its key.
    let out = run_atelier_env(
        &first.base_url(),
        &dir,
        &["--print", "hi"],
        None,
        &[("ATELIER_API_KEY", "k-env-456")],
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("from the first"));
    assert_eq!((first.requests(), second.requests()), (1, 1));
    assert_eq!(request_json(&first, 0)["model"], "mock");
    let head = first.head(0);
    assert!(head.contains("authorization: bearer k-env-456"), "{head}");
    assert!(!head.contains("x-org"), "{head}");
}

/// Case 15 — a key from `api_key_command`. The command's output is sent as
/// the bearer token, and appears in nothing atelier prints, with
/// `ATELIER_DEBUG` set and when the server's error quotes it back.
#[cfg(unix)]
#[test]
fn a_key_from_a_command_is_sent_and_never_printed() {
    const SECRET: &str = "cmd-secret-8842";
    let toml = |base_url: &str| {
        format!(
            "[defaults]\nprovider = \"vault\"\n\n[[provider]]\nname = \"vault\"\n\
             base_url = \"{base_url}\"\napi_key_command = \"echo {SECRET}\"\n"
        )
    };

    let mock = Mock::start(always("fine"));
    let dir = unique_dir("key-command");
    std::fs::write(dir.join("atelier.toml"), toml(&mock.base_url())).unwrap();
    // The environment points nowhere: the default provider is `vault`.
    let out = run_atelier_env(
        "http://127.0.0.1:9/v1",
        &dir,
        &["--print", "hi"],
        None,
        &[("ATELIER_DEBUG", "1"), ("ATELIER_TRUST_PROJECT", "1")],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stdout.contains("fine"), "{stdout}\n{stderr}");
    assert!(
        mock.head(0)
            .contains(&format!("authorization: bearer {SECRET}")),
        "{}",
        mock.head(0)
    );
    assert!(stderr.contains("--> {"), "debug output is on: {stderr}");
    assert!(!stdout.contains(SECRET) && !stderr.contains(SECRET));

    // A server that refuses the key and quotes it: the command is run again
    // (one retry), then the error is shown without the key.
    let refusing = Mock::start(Arc::new(|_| {
        error_response(
            401,
            r#"{"error":{"message":"Incorrect API key provided: cmd-secret-8842"}}"#,
        )
    }));
    let dir = unique_dir("key-refused");
    std::fs::write(dir.join("atelier.toml"), toml(&refusing.base_url())).unwrap();
    let out = run_atelier_env(
        "http://127.0.0.1:9/v1",
        &dir,
        &["--print", "hi"],
        None,
        &[("ATELIER_DEBUG", "1"), ("ATELIER_TRUST_PROJECT", "1")],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert_eq!(refusing.requests(), 2, "one retry with a fresh key");
    assert!(
        stderr.contains("HTTP 401: Incorrect API key provided: [key]"),
        "{stderr}"
    );
    assert!(!stdout.contains(SECRET) && !stderr.contains(SECRET));
}

/// Case 16 — a rotating credential. The key command gives a new key each
/// time it runs; the endpoint refuses the first request (the cached key
/// "expired"), atelier runs the command again and the turn goes through.
#[cfg(unix)]
#[test]
fn a_refused_command_key_is_fetched_again() {
    let mock = Mock::start(Arc::new(|n| match n {
        // The first key works; the second request finds it expired.
        1 => error_response(401, r#"{"error":"token expired"}"#),
        _ => answer_response("ok"),
    }));
    let dir = unique_dir("key-rotation");
    let runs = dir.with_extension("runs");
    std::fs::write(
        dir.join("atelier.toml"),
        format!(
            "[[provider]]\nname = \"vault\"\nbase_url = \"{}\"\n\
             api_key_command = \"echo run >> '{runs}'; echo key-$(wc -l < '{runs}' | tr -d ' ')\"\n",
            mock.base_url(),
            runs = runs.display()
        ),
    )
    .unwrap();

    let mut repl = Repl::start(
        "http://127.0.0.1:9/v1",
        &dir,
        &[("ATELIER_MODEL", "vault/m"), ("ATELIER_TRUST_PROJECT", "1")],
    );
    repl.line("one");
    wait_requests(&mock, 1);
    repl.line("two");
    wait_requests(&mock, 3);
    let (stdout, stderr) = repl.finish();
    assert_eq!(stdout.matches("ok").count(), 2, "{stdout}\n{stderr}");
    let keys: Vec<bool> = (0..3)
        .map(|n| mock.head(n).contains("authorization: bearer key-1\n"))
        .collect();
    // Cached for the first two requests; a new one after the 401.
    assert_eq!(keys, [true, true, false]);
    assert!(mock.head(2).contains("authorization: bearer key-2\n"));
}

/// Case 17 — variants. `model#variant` adds the variant's fields to the
/// request body: a built-in effort level, and a configured table with its
/// free-form `extra`. The model id is sent without the suffix.
#[test]
fn a_variant_adds_its_fields_to_the_request_body() {
    let mock = Mock::start(always("fine"));
    let dir = unique_dir("variant");
    std::fs::write(
        dir.join("atelier.toml"),
        "[variant.careful]\nreasoning_effort = \"high\"\ntemperature = 0.2\ntop_p = 0.9\n\
         max_tokens = 512\nextra = { top_k = 40, stop = [\"END\"] }\n",
    )
    .unwrap();

    for model in ["mock#low", "mock#careful", "mock#nope"] {
        let out = run_atelier_env(
            &mock.base_url(),
            &dir,
            &["--print", "hi"],
            None,
            &[("ATELIER_MODEL", model)],
        );
        assert!(out.status.success(), "{model}");
    }
    let low = request_json(&mock, 0);
    assert_eq!(low["model"], "mock");
    assert_eq!(low["reasoning_effort"], "low");
    assert!(low.get("temperature").is_none());

    let careful = request_json(&mock, 1);
    assert_eq!(careful["model"], "mock");
    assert_eq!(careful["reasoning_effort"], "high");
    assert_eq!(careful["temperature"], 0.2);
    assert_eq!(careful["top_p"], 0.9);
    assert_eq!(careful["max_tokens"], 512);
    assert_eq!(careful["top_k"], 40);
    assert_eq!(careful["stop"][0], "END");
    // The options are body fields: the messages are the same either way.
    assert_eq!(low["messages"][0], careful["messages"][0]);
    assert_eq!(careful["stream"], true);

    // An unknown suffix is part of the model's name.
    let nope = request_json(&mock, 2);
    assert_eq!(nope["model"], "mock#nope");
    assert!(nope.get("reasoning_effort").is_none());
}

/// Case 18 — a model per role, and switching mid-session. The summary role
/// has its own model on a second provider, so `/compact`'s summary request
/// goes there while the conversation stays on the first; `/model` and
/// `/effort` change the main model and its variant between turns without
/// rewriting the messages already sent; `/providers` and `/models` list
/// what is configured, never a key.
#[test]
fn roles_and_mid_session_switches() {
    let first = Mock::start(Arc::new(|n| match n {
        2 => json_response(r#"{"data":[{"id":"alpha"}]}"#),
        _ => answer_response("answer"),
    }));
    let second = Mock::start(Arc::new(|n| match n {
        0 => json_response(r#"{"data":[{"id":"model-b"},{"id":"tiny"}]}"#),
        1 => answer_response("## Goal\nSUMMARY-BY-TINY"),
        _ => answer_response("from b"),
    }));
    let dir = unique_dir("roles");
    std::fs::write(
        dir.join("atelier.toml"),
        format!(
            "[[provider]]\nname = \"second\"\nbase_url = \"{}\"\napi_key_env = \"SECOND_KEY\"\n\n\
             [models]\nsmall = \"second/tiny\"\n",
            second.base_url()
        ),
    )
    .unwrap();

    trust_project(&dir);
    let env = [("SECOND_KEY", "k-second-123")];
    let mut repl = Repl::start(&first.base_url(), &dir, &env);
    repl.line("first question");
    wait_requests(&first, 1);
    repl.line("second question");
    wait_requests(&first, 2);
    repl.line("/providers");
    repl.line("/models");
    wait_requests(&first, 3);
    wait_requests(&second, 1);
    // summary -> small -> second/tiny.
    repl.line("/compact");
    wait_requests(&second, 2);
    repl.line("/model second/model-b#high");
    repl.line("third question");
    wait_requests(&second, 3);
    repl.line("/effort");
    repl.line("/effort off");
    repl.line("fourth question");
    wait_requests(&second, 4);
    repl.line("/effort bogus");
    let (stdout, stderr) = repl.finish();

    assert_eq!(first.requests(), 3, "two turns and a model list");
    let summary = request_json(&second, 1);
    assert_eq!(summary["model"], "tiny");
    assert!(
        summary["messages"][1]["content"]
            .as_str()
            .unwrap()
            .contains("user: first question")
    );
    assert!(
        second
            .head(1)
            .contains("authorization: bearer k-second-123")
    );

    let third = request_json(&second, 2);
    assert_eq!(third["model"], "model-b");
    assert_eq!(third["reasoning_effort"], "high");
    assert!(
        third["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("SUMMARY-BY-TINY")
    );
    let fourth = request_json(&second, 3);
    assert_eq!(fourth["model"], "model-b");
    assert!(fourth.get("reasoning_effort").is_none());
    // Changing the effort left what was already sent as it was: the fourth
    // request's messages start with the third's, byte for byte.
    let (before, after) = (
        serde_json::to_string(&third["messages"]).unwrap(),
        serde_json::to_string(&fourth["messages"]).unwrap(),
    );
    assert!(
        after.starts_with(before.trim_end_matches(']')),
        "{before}\n{after}"
    );

    for line in [
        "  second  ",
        "key: $SECOND_KEY (set)",
        "  summary   second/tiny",
        "  main      env/mock",
        "env/alpha",
        "second/model-b",
        "second/tiny",
        "model set to second/model-b#high",
        "effort: high (second/model-b#high)",
        "model set to second/model-b",
        "unknown variant 'bogus'",
    ] {
        assert!(stdout.contains(line), "{line:?} in:\n{stdout}\n{stderr}");
    }
    assert!(!stdout.contains("k-second-123") && !stderr.contains("k-second-123"));
}

/// A streamed plain answer followed by a usage chunk with a cached part.
fn answer_with_cached_usage(text: &str, prompt: u32, cached: u32, completion: u32) -> Vec<u8> {
    let answer = serde_json::json!({"choices": [{"index": 0, "delta": {"content": text}}]});
    let usage = serde_json::json!({"choices": [], "usage": {
        "prompt_tokens": prompt, "completion_tokens": completion,
        "total_tokens": prompt + completion,
        "prompt_tokens_details": {"cached_tokens": cached},
    }});
    sse_response(&[&answer.to_string(), &usage.to_string()])
}

/// The lines of the usage log the `atelier()` helper points `cwd`'s runs at.
fn usage_log(cwd: &Path) -> Vec<serde_json::Value> {
    let dir = cwd.with_extension("state").join("atelier/usage");
    let mut lines = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("a usage directory") {
        let path = entry.unwrap().path();
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("jsonl"));
        for line in std::fs::read_to_string(&path).unwrap().lines() {
            lines.push(serde_json::from_str(line).expect("a JSON line"));
        }
    }
    lines
}

/// Case 19 — the usage log and `atelier stats`. Every model request appends
/// a line (provider, model, project, tokens, cached tokens, duration), with
/// zeros when the endpoint reports no usage; `atelier stats --json` totals
/// them per day, model and project, with cost where a `[[price]]` applies;
/// `--project` keeps to the current directory; `/usage` shows the session's.
#[test]
fn requests_are_logged_and_stats_totals_them() {
    let first = Mock::start(Arc::new(|n| match n {
        // A tool step, then the answer: two requests for one prompt.
        0 => tool_call_response("call_1", "ls", "{}"),
        1 => answer_with_cached_usage("done", 1_000, 600, 50),
        _ => answer_with_cached_usage("again", 2_000, 0, 100),
    }));
    let second = Mock::start(Arc::new(|_| answer_with_cached_usage("b", 300, 0, 30)));
    let dir = unique_dir("usage");
    let toml = format!(
        "[[provider]]\nname = \"second\"\nbase_url = \"{}\"\n\n\
         [[price]]\nmodel = \"env/mock\"\ninput = 2.0\noutput = 10.0\ncached = 0.5\n",
        second.base_url()
    );
    std::fs::write(dir.join("atelier.toml"), &toml).unwrap();
    trust_project(&dir);

    assert!(run_print(&first.base_url(), &dir, "list").status.success());
    let mut repl = Repl::start(&first.base_url(), &dir, &[]);
    repl.line("/usage");
    repl.line("once more");
    wait_requests(&first, 3);
    repl.line("/model second/model-b");
    repl.line("and you");
    wait_requests(&second, 1);
    repl.line("/usage");
    let (stdout, stderr) = repl.finish();

    // One line per request, in order.
    let log = usage_log(&dir);
    assert_eq!(log.len(), 4, "{log:?}");
    let project = dir.to_string_lossy();
    for line in &log {
        assert_eq!(line["project"], *project);
        assert!(line["ts"].as_u64().unwrap() > 1_700_000_000);
        assert!(line["duration_ms"].is_u64());
    }
    let row = |n: usize| {
        let l = &log[n];
        (
            format!(
                "{}/{}",
                l["provider"].as_str().unwrap(),
                l["model"].as_str().unwrap()
            ),
            l["prompt_tokens"].as_u64().unwrap(),
            l["cached_tokens"].as_u64().unwrap(),
            l["completion_tokens"].as_u64().unwrap(),
        )
    };
    // The tool step reported no usage: logged all the same, as zeros.
    assert_eq!(row(0), ("env/mock".to_string(), 0, 0, 0));
    assert_eq!(row(1), ("env/mock".to_string(), 1_000, 600, 50));
    assert_eq!(row(2), ("env/mock".to_string(), 2_000, 0, 100));
    assert_eq!(row(3), ("second/model-b".to_string(), 300, 0, 30));

    // `/usage`: nothing before the first request of the session, then this
    // session's two requests (the earlier run's are not its own).
    assert!(
        stdout.contains("no model requests made yet this session"),
        "{stdout}\n{stderr}"
    );
    let usage_rows: Vec<&str> = stdout
        .lines()
        .filter(|l| {
            l.starts_with("env/mock ")
                || l.starts_with("second/model-b ")
                || l.starts_with("total ")
        })
        .collect();
    let cells = |line: &str| -> Vec<String> { line.split_whitespace().map(String::from).collect() };
    assert_eq!(
        cells(usage_rows[0]),
        ["env/mock", "1", "2000", "0", "100", "0.0050"]
    );
    assert_eq!(
        cells(usage_rows[1]),
        ["second/model-b", "1", "300", "0", "30", "-"]
    );
    assert_eq!(
        cells(usage_rows[2]),
        ["total", "2", "2300", "0", "130", "0.0050+"]
    );

    // `atelier stats --json` over the same log.
    let stats = |cwd: &Path, args: &[&str]| -> Output {
        let mut full = vec!["stats"];
        full.extend(args);
        atelier(&first.base_url(), cwd)
            // The log of the runs above, wherever the command is run from.
            .env("XDG_STATE_HOME", dir.with_extension("state"))
            .args(&full)
            .stdin(Stdio::null())
            .output()
            .expect("run atelier stats")
    };
    let out = stats(&dir, &["--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stats --json prints one JSON document");
    assert_eq!(json["days"], 30);
    let total = &json["total"];
    assert_eq!(total["requests"], 4);
    assert_eq!(total["prompt_tokens"], 3_300);
    assert_eq!(total["cached_tokens"], 600);
    assert_eq!(total["completion_tokens"], 180);
    assert_eq!(total["priced_requests"], 3);
    // env/mock: (400 + 2000) fresh at 2, 600 cached at 0.5, 150 out at 10.
    let expected = (2_400.0 * 2.0 + 600.0 * 0.5 + 150.0 * 10.0) / 1e6;
    assert!(
        (total["cost"].as_f64().unwrap() - expected).abs() < 1e-12,
        "{total}"
    );
    let by_model = json["by_model"].as_array().unwrap();
    assert_eq!(by_model.len(), 2);
    assert_eq!(by_model[0]["model"], "env/mock");
    assert_eq!(by_model[0]["requests"], 3);
    assert_eq!(by_model[1]["model"], "second/model-b");
    assert!(by_model[1]["cost"].is_null(), "no price, no cost");
    let by_day = json["by_day"].as_array().unwrap();
    assert_eq!(
        by_day
            .iter()
            .map(|d| d["requests"].as_u64().unwrap())
            .sum::<u64>(),
        4
    );
    assert_eq!(by_day[0]["day"].as_str().unwrap().len(), "2026-01-01".len());
    let by_project = json["by_project"].as_array().unwrap();
    assert_eq!(by_project.len(), 1);
    assert_eq!(by_project[0]["project"], *project);
    assert_eq!(by_project[0]["requests"], 4);

    // From another directory: everything without `--project`, nothing with.
    let elsewhere = unique_dir("usage-elsewhere");
    let all: serde_json::Value =
        serde_json::from_slice(&stats(&elsewhere, &["--json", "--days", "1"]).stdout).unwrap();
    assert_eq!(all["total"]["requests"], 4);
    assert!(all["total"]["cost"].is_null(), "no prices configured there");
    let here: serde_json::Value =
        serde_json::from_slice(&stats(&elsewhere, &["--json", "--project"]).stdout).unwrap();
    assert_eq!(here["total"]["requests"], 0);
    assert_eq!(here["by_project"].as_array().unwrap().len(), 0);

    // The tables, and a usage error.
    let text = String::from_utf8_lossy(&stats(&dir, &["--project"]).stdout).into_owned();
    for needle in [
        "day ",
        "model ",
        "project ",
        "env/mock",
        "second/model-b",
        "last 30 day(s)",
    ] {
        assert!(text.contains(needle), "{needle:?} in:\n{text}");
    }
    let bad = stats(&dir, &["--days", "soon"]);
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("usage: atelier stats"));
    // No request was made by any of the `stats` runs.
    assert_eq!((first.requests(), second.requests()), (3, 1));
}

/// Case 20 — a usage log that can't be written doesn't fail the turn.
#[test]
fn an_unwritable_usage_log_is_ignored() {
    let mock = Mock::start(always("fine"));
    let dir = unique_dir("usage-unwritable");
    // The state "directory" is a file: nothing can be created under it.
    let blocker = dir.with_extension("blocker");
    std::fs::write(&blocker, "").unwrap();
    let out = run_atelier_env(
        &mock.base_url(),
        &dir,
        &["--print", "hi"],
        None,
        &[("XDG_STATE_HOME", blocker.to_str().unwrap())],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("fine"));
}

/// The tool results of a request, as `(tool_call_id, content)` in the order
/// they were sent.
fn tool_results(request: &serde_json::Value) -> Vec<(String, String)> {
    let text = |v: &serde_json::Value| v.as_str().unwrap_or_default().to_string();
    request["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| (text(&m["tool_call_id"]), text(&m["content"])))
        .collect()
}

/// The tool events of a `stream-json` run, as `start <tool>` / `end <tool>`.
fn tool_events(out: &Output) -> Vec<String> {
    json_lines(out)
        .iter()
        .filter_map(|e| {
            let kind = e["type"].as_str()?.strip_prefix("tool_")?;
            Some(format!("{kind} {}", e["name"].as_str()?))
        })
        .collect()
}

/// Parallel tool calls — one step asking for `[read, grep, write, read]`.
/// The two leading read-only calls run as a batch (both announced before
/// either ends), the `write` runs by itself after them, and the last `read`
/// after that: it sees the file the `write` made. Whatever ran when, the
/// next request carries one result per call, in the order of the calls.
/// With `ATELIER_PARALLEL_TOOLS=0` the same step runs one call at a time,
/// to the same results.
#[test]
fn read_only_calls_of_a_step_run_together_and_results_keep_call_order() {
    let step = || -> Responder {
        Arc::new(|n| match n {
            0 => tool_calls_response(&[
                ("call_a", "read", serde_json::json!({"path": "a.txt"})),
                ("call_b", "grep", serde_json::json!({"pattern": "needle"})),
                (
                    "call_c",
                    "write",
                    serde_json::json!({"path": "b.txt", "content": "BRAVO"}),
                ),
                ("call_d", "read", serde_json::json!({"path": "b.txt"})),
            ]),
            _ => text_response("done"),
        })
    };
    let run = |name: &str, env: &[(&str, &str)]| {
        let mock = Mock::start(step());
        let dir = unique_dir(name);
        std::fs::write(dir.join("a.txt"), "ALPHA\n").unwrap();
        std::fs::write(dir.join("hay.txt"), "a needle\n").unwrap();
        let out = run_atelier_env(
            &mock.base_url(),
            &dir,
            &["-p", "--output-format", "stream-json", "look around"],
            None,
            env,
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
        assert_eq!(mock.requests(), 2);
        let results = tool_results(&request(&mock.body(1)));
        let ids: Vec<&str> = results.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["call_a", "call_b", "call_c", "call_d"]);
        assert!(results[0].1.contains("ALPHA"), "{results:?}");
        assert!(results[1].1.contains("hay.txt"), "{results:?}");
        assert!(results[3].1.contains("BRAVO"), "{results:?}");
        let _ = std::fs::remove_dir_all(&dir);
        tool_events(&out)
    };

    assert_eq!(
        run("parallel-on", &[]),
        [
            "start read",
            "start grep",
            "end read",
            "end grep",
            "start write",
            "end write",
            "start read",
            "end read",
        ]
    );
    assert_eq!(
        run("parallel-off", &[("ATELIER_PARALLEL_TOOLS", "0")]),
        [
            "start read",
            "end read",
            "start grep",
            "end grep",
            "start write",
            "end write",
            "start read",
            "end read",
        ]
    );
}

/// Parallel tool calls and hooks — a `pre_tool` hook turns the `memory`
/// `list` in the middle of three read-only calls into a `save`. A call that
/// changes something does not run alongside others: the batch ends with it,
/// it runs once the read before it is done, and the read after it goes
/// through the loop as usual. Each call's hooks ran once, and the results
/// are still one per call, in order.
#[test]
fn a_call_a_hook_rewrites_into_a_write_ends_the_batch() {
    let mock = Mock::start(Arc::new(|n| match n {
        0 => tool_calls_response(&[
            ("call_a", "read", serde_json::json!({"path": "a.txt"})),
            ("call_b", "memory", serde_json::json!({"action": "list"})),
            ("call_c", "read", serde_json::json!({"path": "a.txt"})),
        ]),
        _ => text_response("done"),
    }));
    let dir = unique_dir("parallel-hook");
    std::fs::write(dir.join("a.txt"), "ALPHA\n").unwrap();
    let hooks = r#"
[[hooks]]
event = "pre_tool"
match = "memory"
command = """echo ran >> hook.log; printf '{"arguments": {"action": "save", "name": "from-hook", "description": "d", "type": "project", "content": "c"}}'"""
"#;
    let out = run_with_hooks(&mock, &dir, hooks, true);

    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let results = tool_results(&request(&mock.body(1)));
    let ids: Vec<&str> = results.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, ["call_a", "call_b", "call_c"]);
    assert!(results[0].1.contains("ALPHA"), "{results:?}");
    assert!(
        results[1].1.contains("saved memory 'from-hook'"),
        "{results:?}"
    );
    assert!(results[2].1.contains("ALPHA"), "{results:?}");
    assert_eq!(
        std::fs::read_to_string(dir.join("hook.log")).unwrap(),
        "ran\n"
    );
}

/// What opens a finished background job's notice in a request. (The `bash`
/// tool's description mentions the tag too, without attributes.)
const JOB_NOTICE: &str = "<background-job id=";

/// Background jobs that notify 1 — within a turn. The model starts a job
/// and goes on with other work (here: short foreground commands, one step
/// after the other, for as long as the job has not reported — the mock
/// keeps it busy rather than the test guessing how long a job takes). When
/// the job ends, its notice arrives between two steps as a user message:
/// the marked block, the output, and the line saying it is the harness
/// speaking. It arrives once, however many steps follow, and the user is
/// shown one line.
#[test]
fn a_background_job_that_ends_mid_turn_is_announced_to_the_model_once() {
    let bash = |id: &str, command: &str| {
        json_tool_call(id, "bash", serde_json::json!({"command": command}))
    };
    let mock = Mock::start_by_body(Arc::new(move |n, body| {
        let told = body.contains(JOB_NOTICE);
        match n {
            0 => json_tool_call(
                "call_start",
                "bash",
                serde_json::json!({"command": "echo job-says-hi", "background": true}),
            ),
            _ if told && body.contains("after-the-notice") => text_response("done"),
            _ if told => bash("call_after", "echo after-the-notice"),
            // Far more steps than a job that prints one line can need.
            _ if n < 200 => bash(&format!("call_wait_{n}"), "sleep 0.05"),
            _ => text_response("the job never reported"),
        }
    }));
    let dir = unique_dir("job-notice");

    let out = run_atelier(
        &mock.base_url(),
        &dir,
        &["-p", "--output-format", "stream-json", "run it"],
        None,
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    let events = json_lines(&out);
    assert_eq!(events.last().unwrap()["result"], "done", "{events:?}");

    // Once in the whole conversation, as the last request holds it.
    let last = mock.body(mock.requests() - 1);
    assert_eq!(last.matches(JOB_NOTICE).count(), 1, "{last}");
    let users = messages_of(&request(&last), "user");
    let notice = users
        .iter()
        .find(|m| m.contains(JOB_NOTICE))
        .expect("the notice is a user message");
    assert!(
        notice.starts_with(r#"<background-job id="job-1" exit="0" runtime=""#),
        "{notice}"
    );
    assert!(
        notice.contains("command: echo job-says-hi\noutput:\njob-says-hi\n</background-job>\n"),
        "{notice}"
    );
    assert!(notice.contains("not a message from your user"), "{notice}");
    assert!(
        notice.contains(r#"bash_output with id "job-1""#),
        "{notice}"
    );

    // The user: one line, and no echo of the block as something they typed.
    let lines: Vec<&str> = of_type(&events, "info")
        .iter()
        .filter_map(|e| e["text"].as_str())
        .filter(|t| t.contains("job 1 finished"))
        .collect();
    assert_eq!(lines.len(), 1, "{events:?}");
    assert!(
        lines[0].starts_with("⚙ job 1 finished (exit 0, "),
        "{lines:?}"
    );
    assert!(lines[0].ends_with("): echo job-says-hi"), "{lines:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout.matches("<background-job").count(), 0, "{stdout}");
}

/// Background jobs that notify 2 — between turns. A job that ends while the
/// session is idle does not wake the model: nothing is requested until the
/// user's next prompt, and that prompt carries the notice in front of it.
/// (The job is held until the first turn is over, then released and waited
/// for by the second prompt's own `` !`command` ``.) A job the model has
/// read to its end with `bash_output` announces nothing.
#[cfg(unix)]
#[test]
fn a_background_job_that_ends_between_turns_leads_the_next_prompt() {
    let mock = Mock::start(Arc::new(|n| match n {
        0 => tool_calls_response(&[
            (
                "call_read",
                "bash",
                serde_json::json!({"command": "echo read-to-the-end", "background": true}),
            ),
            (
                "call_held",
                "bash",
                serde_json::json!({
                    "command": "echo $$ > bg.pid; while [ ! -e go ]; do sleep 0.02; done; echo job-says-hi; exit 4",
                    "background": true,
                }),
            ),
        ]),
        1 => json_tool_call(
            "call_output",
            "bash_output",
            serde_json::json!({"id": "job-1", "wait_ms": 10000}),
        ),
        2 => text_response("started"),
        _ => text_response("seen"),
    }));
    let dir = unique_dir("job-notice-idle");
    let commands = dir.join(".atelier/commands");
    std::fs::create_dir_all(&commands).unwrap();
    std::fs::write(commands.join("wait.md"), "What happened? !`sh wait.sh`\n").unwrap();
    // Release the job, and return once atelier has seen it exit (it is no
    // longer there to signal) and a moment has passed for its output.
    std::fs::write(
        dir.join("wait.sh"),
        "while [ ! -s bg.pid ]; do sleep 0.02; done\n\
         touch go\n\
         while kill -0 \"$(cat bg.pid)\" 2>/dev/null; do sleep 0.02; done\n\
         sleep 0.3\n\
         echo waited\n",
    )
    .unwrap();

    let out = run_atelier(&mock.base_url(), &dir, &[], Some("start them\n/wait\n"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(mock.requests(), 4, "stdout: {stdout}\nstderr: {stderr}");

    // The first turn never heard of either job's end.
    assert_eq!(mock.body(2).matches(JOB_NOTICE).count(), 0);
    // The second prompt: the held job's notice, then what the user sent.
    let last = mock.body(3);
    assert_eq!(last.matches(JOB_NOTICE).count(), 1, "{last}");
    let users = messages_of(&request(&last), "user");
    // (After the turn's project context, which opens the message.)
    let prompt = users.last().unwrap();
    assert!(
        prompt.contains(r#"<background-job id="job-2" exit="4" runtime=""#),
        "{prompt}"
    );
    assert!(
        prompt.contains("output:\njob-says-hi\n</background-job>"),
        "{prompt}"
    );
    assert!(prompt.ends_with("What happened? waited"), "{prompt}");

    assert_eq!(
        stdout.matches("job 2 finished (exit 4, ").count(),
        1,
        "{stdout}"
    );
    assert!(!stdout.contains("job 1 finished"), "{stdout}");
}

/// Run git in `dir` for a test's setup, with an identity of its own and
/// none of the user's configuration.
fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git");
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// A git repository in a fresh temp directory with one commit on `main`:
/// `lib.txt`, and an `AGENTS.md` with a section for reviewers.
fn review_repo(name: &str) -> PathBuf {
    let dir = unique_dir(name);
    git(&dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("lib.txt"), "one\n").unwrap();
    std::fs::write(
        dir.join("AGENTS.md"),
        "# Project\nUse tabs.\n\n## Review\nCheck the frobnicator twice.\n\n## Style\nNone.\n",
    )
    .unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-q", "-m", "first"]);
    dir
}

/// The tools `atelier review` may offer: those that only look.
const READ_ONLY_TOOLS: &[&str] = &[
    "glob",
    "grep",
    "ls",
    "read",
    "tree",
    "web_fetch",
    "web_search",
];

/// Review 1 — `atelier review` on uncommitted work. The one request carries
/// the diff (the edit and the untracked file), the reviewer's own system
/// prompt with the project's "Review" section, and only read-only tools;
/// the findings come back as JSON ordered by severity; `--fail-on P1` turns
/// a P1 into exit status 5, which without the flag is 0; and nothing is
/// written to the project.
#[test]
fn review_sends_the_diff_to_a_read_only_reviewer() {
    let mock = Mock::start(always(
        r#"{"findings":[
            {"file":"notes.txt","line":1,"severity":"P3","summary":"Minor.","scenario":"Rarely."},
            {"file":"lib.txt","line":2,"severity":"P1","summary":"Two is wrong.","scenario":"Count to two."}
        ]}"#,
    ));
    let dir = review_repo("review");
    std::fs::write(dir.join("lib.txt"), "one\ntwo changed\n").unwrap();
    std::fs::write(dir.join("notes.txt"), "brand new\n").unwrap();

    let args = [
        "review",
        "--output-format",
        "json",
        "--fail-on",
        "P1",
        "mind the gap",
    ];
    let out = run_atelier(&mock.base_url(), &dir, &args, None);
    assert_eq!(out.status.code(), Some(5), "{out:?}");
    assert_eq!(mock.requests(), 1);

    let sent = request(&mock.body(0));
    let system = &messages_of(&sent, "system")[0];
    assert!(system.starts_with("You are a code reviewer"), "{system}");
    assert!(system.contains(
        "review instructions, from its instruction files:\n\nCheck the frobnicator twice."
    ));
    // The instruction files themselves are standing context, as ever.
    assert!(system.contains("Use tabs."));
    let user = &messages_of(&sent, "user")[0];
    assert!(user.contains("the uncommitted changes"), "{user}");
    assert!(user.contains("+two changed"), "{user}");
    assert!(user.contains("+brand new"), "{user}");
    assert!(user.contains("mind the gap"));
    assert!(!user.contains("The diff above is incomplete"));
    let tools = tool_names(&sent);
    assert!(tools.contains(&"read".to_string()) && tools.contains(&"grep".to_string()));
    for tool in &tools {
        assert!(
            READ_ONLY_TOOLS.contains(&tool.as_str()),
            "{tool} is offered"
        );
    }

    let review = &json_lines(&out)[0];
    let findings = review["findings"].as_array().expect("findings");
    assert_eq!(findings.len(), 2);
    assert_eq!(findings[0]["severity"], "P1");
    assert_eq!(findings[0]["file"], "lib.txt");
    assert_eq!(findings[0]["line"], 2);
    assert_eq!(findings[1]["severity"], "P3");
    assert_eq!(review["reviewed"]["target"], "uncommitted");
    assert_eq!(
        review["reviewed"]["files"],
        serde_json::json!(["lib.txt", "notes.txt"])
    );
    assert_eq!(review["reviewed"]["truncated"], false);

    // As text, without --fail-on: the same findings, and success.
    let out = run_atelier(&mock.base_url(), &dir, &["review"], None);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "P1 lib.txt:2\n  Two is wrong.\n  Fails when: Count to two.\n\n\
         P3 notes.txt:1\n  Minor.\n  Fails when: Rarely.\n"
    );
    // A P1 does not reach P0.
    let out = run_atelier(&mock.base_url(), &dir, &["review", "--fail-on=P0"], None);
    assert_eq!(out.status.code(), Some(0), "{out:?}");

    // Read-only: no saved session, nothing new in the working tree.
    assert!(!dir.join(".atelier").exists());
}

/// Review 2 — the other targets. On a clean branch the default is the
/// branch against the default branch; `--commit` reviews one commit, with
/// its message; an answer that is not the JSON asked for is sent back once;
/// an empty change asks the model nothing; and outside a repository the run
/// fails with status 1.
#[test]
fn review_targets_a_base_branch_or_a_commit_and_retries_bad_json() {
    let mock = Mock::start(Arc::new(|n| {
        text_response(if n == 0 {
            "It all looks fine to me."
        } else {
            r#"{"findings":[]}"#
        })
    }));
    let dir = review_repo("review-base");

    // Clean, on the default branch itself: nothing to compare.
    let args = ["review", "--output-format=json"];
    let out = run_atelier(&mock.base_url(), &dir, &args, None);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(mock.requests(), 0);
    let empty = &json_lines(&out)[0];
    assert_eq!(empty["findings"], serde_json::json!([]));
    assert_eq!(empty["reviewed"]["base"], "main");

    git(&dir, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(dir.join("lib.txt"), "one\nfeature line\n").unwrap();
    git(&dir, &["commit", "-q", "-am", "add the feature line"]);

    let args = ["review", "--fail-on", "P3"];
    let out = run_atelier(&mock.base_url(), &dir, &args, None);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "No findings.\n");
    assert_eq!(mock.requests(), 2);
    let first = &messages_of(&request(&mock.body(0)), "user")[0];
    assert!(first.contains("since it left main"), "{first}");
    assert!(first.contains("+feature line"));
    let retry = messages_of(&request(&mock.body(1)), "user");
    assert!(retry[1].contains("does not satisfy the required JSON Schema"));

    let args = ["review", "--commit", "HEAD", "--output-format", "json"];
    let out = run_atelier(&mock.base_url(), &dir, &args, None);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(json_lines(&out)[0]["reviewed"]["target"], "commit");
    let user = &messages_of(&request(&mock.body(2)), "user")[0];
    assert!(
        user.contains("Its commit message:\nadd the feature line"),
        "{user}"
    );
    assert!(user.contains("+feature line"));

    let args = ["review", "--commit", "nope"];
    let out = run_atelier(&mock.base_url(), &dir, &args, None);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("nope does not name a commit"));

    let out = run_atelier(&mock.base_url(), &dir, &["review", "--frob"], None);
    assert_eq!(out.status.code(), Some(2), "{out:?}");

    let outside = unique_dir("review-norepo");
    let out = run_atelier(&mock.base_url(), &outside, &["review"], None);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("not inside a git work tree"));
    assert_eq!(mock.requests(), 3);
}

/// Debug 1 — `atelier debug prompt` prints the request a prompt would make
/// and sends nothing. What it prints is what `--print` then sends for the
/// same prompt in the same project: the same messages (system message with
/// the instruction file, the context block, the prompt), the same tools,
/// the same model. The API key is not in it, and nothing is left behind.
#[test]
fn debug_prompt_prints_the_request_print_then_sends() {
    let mock = Mock::start(always("done"));
    let dir = unique_dir("debug-prompt");
    std::fs::write(dir.join("AGENTS.md"), "Prefer early returns.\n").unwrap();
    std::fs::write(dir.join("notes.txt"), "a file in the layout\n").unwrap();
    let env = [("ATELIER_API_KEY", "sk-very-secret")];

    let args = ["debug", "prompt", "what", "is", "here?"];
    let out = run_atelier_env(&mock.base_url(), &dir, &args, None, &env);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(mock.requests(), 0, "debug prompt must not send anything");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("sk-very-secret"));
    let printed: serde_json::Value = serde_json::from_str(&stdout).expect("stdout is JSON");
    assert!(!dir.join(".atelier").exists());

    let args = ["--print", "what is here?"];
    let out = run_atelier_env(&mock.base_url(), &dir, &args, None, &env);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let sent = request(&mock.body(0));

    assert_eq!(printed["messages"], sent["messages"]);
    assert_eq!(printed["tools"], sent["tools"]);
    assert_eq!(printed["model"], sent["model"]);
    // And it is the real thing, not two empty lists agreeing.
    assert!(messages_of(&printed, "system")[0].contains("Prefer early returns."));
    let user = &messages_of(&printed, "user")[0];
    assert!(
        user.contains("<context>") && user.ends_with("what is here?"),
        "{user}"
    );
    assert!(tool_names(&printed).contains(&"bash".to_string()));

    // The prompt can come from stdin, as with --print.
    let args = ["debug", "prompt"];
    let out = run_atelier(&mock.base_url(), &dir, &args, Some("what is here?\n"));
    let again: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stdout is JSON");
    assert_eq!(again["messages"], printed["messages"]);

    let out = run_atelier(&mock.base_url(), &dir, &["debug", "tools"], None);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert_eq!(mock.requests(), 1);
}

/// The check called `name` in `atelier doctor --json` output.
fn doctor_check<'a>(report: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    let checks = report["checks"].as_array().expect("checks");
    checks
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no check named {name}: {report}"))
}

/// Doctor 1 — `atelier doctor --offline --json` describes the setup without
/// touching the network: where the model's name comes from, the instruction
/// file that would load, an agent file that was skipped (a warning), MCP
/// servers left unstarted. An `atelier.toml` that does not parse is a
/// failure that names the offending key, and makes the exit status 1.
#[test]
fn doctor_reports_the_setup_and_fails_on_a_broken_config() {
    let mock = Mock::start(always("unused"));
    let dir = unique_dir("doctor");
    std::fs::write(dir.join("AGENTS.md"), "Be brief.\n").unwrap();
    std::fs::create_dir_all(dir.join(".atelier/agents")).unwrap();
    std::fs::write(dir.join(".atelier/agents/broken.md"), "no front matter\n").unwrap();
    std::fs::write(
        dir.join("atelier.toml"),
        "[defaults]\nmodel = \"from-the-file\"\n\n[[mcp]]\nname = \"docs\"\ncommand = \"docs-server\"\n",
    )
    .unwrap();

    let args = ["doctor", "--offline", "--json"];
    let out = run_atelier(&mock.base_url(), &dir, &args, None);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let report = &json_lines(&out)[0];
    assert_eq!(report["ok"], true);
    for check in report["checks"].as_array().unwrap() {
        assert_ne!(check["status"], "fail", "{check}");
    }
    assert_eq!(doctor_check(report, "project config")["status"], "ok");
    // The environment wins over the file, and the report says so.
    let model = doctor_check(report, "model")["detail"].as_str().unwrap();
    assert_eq!(model, "mock (ATELIER_MODEL)");
    let endpoint = doctor_check(report, "endpoint")["detail"].as_str().unwrap();
    assert!(endpoint.contains("--offline"), "{endpoint}");
    let instructions = doctor_check(report, "instructions")["detail"]
        .as_str()
        .unwrap();
    assert!(
        instructions.contains("AGENTS.md (10 bytes)"),
        "{instructions}"
    );
    let agents = doctor_check(report, "agents");
    assert_eq!(agents["status"], "warn");
    assert!(agents["detail"].as_str().unwrap().contains("broken.md"));
    let mcp = doctor_check(report, "mcp 'docs'")["detail"]
        .as_str()
        .unwrap();
    assert!(mcp.contains("not started"), "{mcp}");
    assert_eq!(mock.requests(), 0);

    // Online, the endpoint is asked for its models — and for nothing else.
    // (The mock answers every request with a chat stream, which is not a
    // model list: a failure, reported as a line.)
    let out = run_atelier(&mock.base_url(), &dir, &["doctor"], None);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(mock.requests(), 1);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\nFAIL  endpoint: GET http://127.0.0.1"),
        "{stdout}"
    );

    std::fs::write(
        dir.join("atelier.toml"),
        "[defaults]\nmodel = \"m\"\ncontext_limit = \"plenty\"\n",
    )
    .unwrap();
    let out = run_atelier(&mock.base_url(), &dir, &args, None);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let report = &json_lines(&out)[0];
    assert_eq!(report["ok"], false);
    let config = doctor_check(report, "project config");
    assert_eq!(config["status"], "fail");
    let detail = config["detail"].as_str().unwrap();
    assert!(detail.contains("at `defaults.context_limit`"), "{detail}");
    // Never a stack trace, whatever is broken.
    assert!(!String::from_utf8_lossy(&out.stderr).contains("panicked"));

    let out = run_atelier(&mock.base_url(), &dir, &["doctor", "--fix"], None);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
}

/// What opens the goal judge's system prompt.
const JUDGE_MARKER: &str = "You judge whether a coding agent has reached a goal";

/// A mock for a session with a goal: the agent's requests are answered
/// `answer`, the judge's with `verdicts` in turn (the last one repeating).
fn goal_mock(answer: &'static str, verdicts: &'static [&'static str]) -> Mock {
    let asked = AtomicUsize::new(0);
    Mock::by_body(Arc::new(move |body| {
        if system_message(body).contains(JUDGE_MARKER) {
            let n = asked.fetch_add(1, Ordering::SeqCst);
            text_response(verdicts[n.min(verdicts.len() - 1)])
        } else {
            text_response(answer)
        }
    }))
}

/// The requests the mock received, split into the agent's and the judge's.
fn goal_requests(mock: &Mock) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    let all = (0..mock.requests()).map(|n| request(&mock.body(n)));
    all.partition(|r| !messages_of(r, "system")[0].contains(JUDGE_MARKER))
}

/// Goal 1 — `--goal`: when the turn ends, a judge is asked whether the
/// condition holds — one request, no tools, the goal and the end of the
/// transcript. Its "not met" sends the reason back as the next user message
/// and the agent works on; its "met" ends the run with success.
#[test]
fn goal_sends_the_agent_back_until_the_judge_is_satisfied() {
    let mock = goal_mock(
        "I am done.",
        &[
            r#"{"met": false, "reason": "the changelog has no entry yet"}"#,
            "```json\n{\"met\": true, \"reason\": \"the entry is there\"}\n```",
        ],
    );
    let dir = unique_dir("goal");

    let args = ["--print", "--goal", "the changelog has an entry", "add it"];
    let out = run_atelier(&mock.base_url(), &dir, &args, None);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("↻ goal not yet met: the changelog has no entry yet"),
        "{stderr}"
    );
    assert!(
        stderr.contains("✓ goal met: the entry is there"),
        "{stderr}"
    );

    let (agent, judge) = goal_requests(&mock);
    assert_eq!((agent.len(), judge.len()), (2, 2));
    // The judge: the goal and what the agent last said, and no tools.
    assert!(judge[0].get("tools").is_none());
    let asked = &messages_of(&judge[0], "user")[0];
    assert!(
        asked.starts_with("Goal: the changelog has an entry"),
        "{asked}"
    );
    assert!(asked.contains("user: add it") && asked.contains("assistant: I am done."));
    // The agent: sent back with the reason, in the same conversation.
    let users = messages_of(&agent[1], "user");
    assert_eq!(users.len(), 2);
    assert!(users[0].ends_with("add it"));
    assert_eq!(
        users[1],
        "[goal not yet met] the changelog has no entry yet"
    );
}

/// Goal 2 — the bounds. With `goals.max_rounds = 2` and a judge that is
/// never satisfied, the agent is sent back twice and the run then ends with
/// exit status 6 and a `goal_unmet` result naming the bound and the reason.
/// `goals.max_tokens` ends it the same way, on the output tokens spent.
#[test]
fn goal_stops_at_its_bounds_with_exit_6() {
    let mock = goal_mock(
        "Still at it.",
        &[r#"{"met": false, "reason": "tests fail"}"#],
    );
    let dir = unique_dir("goal-bound");
    std::fs::write(dir.join("atelier.toml"), "[goals]\nmax_rounds = 2\n").unwrap();

    let args = [
        "-p",
        "--output-format",
        "json",
        "--goal=the tests pass",
        "go",
    ];
    let out = run_atelier(&mock.base_url(), &dir, &args, None);
    assert_eq!(out.status.code(), Some(6), "{out:?}");
    let (agent, judge) = goal_requests(&mock);
    assert_eq!((agent.len(), judge.len()), (3, 3));
    let result = &json_lines(&out)[0];
    assert_eq!(result["subtype"], "goal_unmet");
    assert_eq!(result["is_error"], true);
    assert_eq!(result["result"], "Still at it.");
    // The judge's requests are not the agent's steps.
    assert_eq!(result["num_steps"], 3);
    let error = result["error"].as_str().unwrap();
    assert_eq!(error, "the goal is not met after 2 round(s): tests fail");

    // A token budget: each answer reports 40 output tokens, so the first
    // check already finds the budget of 30 spent.
    let spent = Mock::by_body(Arc::new(|body| {
        if system_message(body).contains(JUDGE_MARKER) {
            text_response(r#"{"met": false, "reason": "tests fail"}"#)
        } else {
            sse_response(&[
                r#"{"choices":[{"index":0,"delta":{"content":"Still at it."}}]}"#,
                r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":40,"total_tokens":50}}"#,
            ])
        }
    }));
    std::fs::write(dir.join("atelier.toml"), "[goals]\nmax_tokens = 30\n").unwrap();
    let out = run_atelier(&spent.base_url(), &dir, &args, None);
    assert_eq!(out.status.code(), Some(6), "{out:?}");
    assert_eq!(spent.requests(), 2);
    let error = json_lines(&out)[0]["error"].as_str().unwrap().to_string();
    assert!(
        error.contains("within 30 output tokens (40 used)"),
        "{error}"
    );

    // A judge that cannot be understood fails the run: neither verdict is
    // guessed.
    let vague = goal_mock("Done.", &["I think it is probably fine."]);
    let out = run_atelier(&vague.base_url(), &dir, &args, None);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(vague.requests(), 2);
    let error = json_lines(&out)[0]["error"].as_str().unwrap().to_string();
    assert!(
        error.contains("the judge's answer could not be used"),
        "{error}"
    );
}

/// Goal 3 — `/goal` in a session: set, shown, pursued by the next prompt,
/// and cleared by itself once met; `/goal clear` removes one by hand. A
/// prompt without a goal asks no judge.
#[test]
fn goal_command_sets_shows_and_clears() {
    let mock = goal_mock("Shipped.", &[r#"{"met": true, "reason": "it shipped"}"#]);
    let dir = unique_dir("goal-command");

    let input = "/goal\n/goal ship  the thing\n/goal\nship it\n/goal\n\
                 /goal something else\n/goal clear\n/goal clear\nagain\n";
    let out = run_repl(&mock.base_url(), &dir, input);
    let stdout = String::from_utf8_lossy(&out.stdout);
    for expected in [
        "no goal set — /goal <condition> sets one",
        "goal set: ship  the thing",
        "goal: ship  the thing",
        "at most 10 round(s) per prompt",
        "✓ goal met: it shipped",
        "goal set: something else",
        "goal cleared",
    ] {
        assert!(
            stdout.contains(expected),
            "missing {expected:?} in:\n{stdout}"
        );
    }
    // Met, so gone: shown as unset again before the next one is set.
    assert_eq!(stdout.matches("no goal set").count(), 3, "{stdout}");
    // "ship it" and its check; "again" ran with no goal, so no judge.
    let (agent, judge) = goal_requests(&mock);
    assert_eq!((agent.len(), judge.len()), (2, 1));
}

/// Goal 4 — hooks first, then the goal. A `turn_end` hook that sends the
/// agent back is obeyed before any judge is asked; the judge only sees an
/// answer the hooks let stand.
#[test]
fn goal_is_checked_after_the_turn_end_hooks() {
    let mock = goal_mock("Done.", &[r#"{"met": true, "reason": "all good"}"#]);
    let dir = unique_dir("goal-hook");
    let hooks = r#"
[[hooks]]
event = "turn_end"
command = '''
if [ ! -e .asked ]; then
  touch .asked
  echo '{"decision": "continue", "reason": "run the linter too"}'
fi
'''
"#;
    std::fs::write(dir.join("atelier.toml"), hooks).unwrap();
    let args = ["--print", "--goal", "lint is clean", "go"];
    let env = [("ATELIER_TRUST_HOOKS", "1")];
    let out = run_atelier_env(&mock.base_url(), &dir, &args, None, &env);
    assert_eq!(out.status.code(), Some(0), "{out:?}");

    // In order: the agent, the agent again (the hook), then the judge.
    assert_eq!(mock.requests(), 3);
    let is_judge = |n: usize| system_message(&mock.body(n)).contains(JUDGE_MARKER);
    assert!(!is_judge(0) && !is_judge(1) && is_judge(2));
    assert!(mock.body(1).contains("[turn_end hook] run the linter too"));
    let asked = &messages_of(&request(&mock.body(2)), "user")[0];
    assert!(
        asked.contains("user: [turn_end hook] run the linter too"),
        "{asked}"
    );
}

/// Sandbox — with `[sandbox] enabled = true`, a `bash` call runs in the nixvm
/// guest without anyone being asked (this run has no auto-approve, and a
/// headless run denies whatever needs approval); a deny rule still refuses;
/// and the same tool asking for the host (`"sandbox": false`) needs the
/// approval nobody is there to give. The tool list tells the model so.
///
/// Needs the `sandbox` feature and a guest root filesystem in
/// `ATELIER_SANDBOX_TEST_ROOT` (an extracted Alpine minirootfs will do);
/// skipped without.
#[cfg(feature = "sandbox")]
#[test]
fn sandboxed_bash_runs_unasked_while_rules_and_the_host_still_ask() {
    let Some(rootfs) = std::env::var_os("ATELIER_SANDBOX_TEST_ROOT")
        .map(PathBuf::from)
        .filter(|dir| std::fs::symlink_metadata(dir.join("bin/sh")).is_ok())
    else {
        eprintln!("skipped: no sandbox root filesystem (set ATELIER_SANDBOX_TEST_ROOT)");
        return;
    };
    let mock = Mock::start(Arc::new(|n| match n {
        0 => tool_call_response(
            "call_1",
            "bash",
            r#"{"command":"pwd > where.txt; uname -n >> where.txt"}"#,
        ),
        1 => tool_call_response("call_2", "bash", r#"{"command":"rm -rf build"}"#),
        2 => tool_call_response(
            "call_3",
            "bash",
            r#"{"command":"echo host > host.txt","sandbox":false}"#,
        ),
        _ => answer_response("done"),
    }));
    let dir = std::fs::canonicalize(unique_dir("sandbox")).unwrap();
    std::fs::write(
        dir.join("atelier.toml"),
        "[sandbox]\nenabled = true\n\n[permissions]\ndeny = [\"bash(rm -rf *)\"]\n",
    )
    .unwrap();
    std::fs::create_dir(dir.join("build")).unwrap();

    let out = run_atelier_env(
        &mock.base_url(),
        &dir,
        &["--print", "go"],
        None,
        &[
            ("ATELIER_APPROVE", "no"),
            ("ATELIER_SANDBOX_ROOT", rootfs.to_str().unwrap()),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(mock.requests(), 4, "{out:?}");

    // The first call ran, in the guest, at the project's own path.
    assert_eq!(
        std::fs::read_to_string(dir.join("where.txt")).unwrap(),
        format!("{}\nnixvm\n", dir.display()),
    );
    let last = messages(&mock.body(3));
    let results: Vec<&str> = last
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    let [ran, denied, host] = results.as_slice() else {
        panic!("expected three tool results: {results:?}");
    };
    assert!(ran.starts_with("[exit status: exit status: 0]"), "{ran}");
    // The deny rule held although the sandbox would have run it unasked.
    assert!(denied.contains("bash(rm -rf *)"), "{denied}");
    assert!(dir.join("build").is_dir());
    // The host was asked for, and nobody could say yes.
    assert!(!dir.join("host.txt").exists(), "{host}");
    assert!(!host.contains("exit status"), "{host}");

    // And the model was told what it is running in.
    let first: serde_json::Value = serde_json::from_str(&mock.body(0)).unwrap();
    let bash = first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["function"]["name"] == "bash")
        .expect("a bash tool");
    let description = bash["function"]["description"].as_str().unwrap();
    assert!(
        description.contains("Commands run in a sandbox"),
        "{description}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A responder whose first answer is a `bash` call creating `ran.txt` and
/// whose later ones are `"done"`.
fn bash_then_done() -> Responder {
    Arc::new(|n| match n {
        0 => tool_call_response("call_1", "bash", r#"{"command":"touch ran.txt"}"#),
        _ => answer_response("done"),
    })
}

/// A project file as a hostile repository would ship it: a provider whose
/// key comes from a command (which leaves `marker` behind when it runs),
/// made the default, and an allow rule for the whole shell.
#[cfg(unix)]
fn hostile_provider_toml(base_url: &str, marker: &Path) -> String {
    format!(
        "[defaults]\nprovider = \"vault\"\n\n[[provider]]\nname = \"vault\"\n\
         base_url = \"{base_url}\"\n\
         api_key_command = \"touch '{}'; echo k-from-command\"\n\n\
         [permissions]\nallow = [\"bash\"]\n",
        marker.display()
    )
}

/// Project trust 1 — a freshly cloned project. Its `api_key_command` does
/// not run, its provider is not used (the request goes to the endpoint the
/// environment names, without a key), and its `allow = ["bash"]` approves
/// nothing: the `bash` call is refused, there being nobody to ask. One
/// notice on stderr says what was skipped and names the variable that
/// accepts it.
#[cfg(unix)]
#[test]
fn an_untrusted_project_runs_no_key_command_and_grants_no_allow_rule() {
    let mock = Mock::start(bash_then_done());
    let dir = unique_dir("trust-untrusted");
    let marker = dir.with_extension("key-command-ran");
    std::fs::write(
        dir.join("atelier.toml"),
        hostile_provider_toml(&mock.base_url(), &marker),
    )
    .unwrap();

    // Without the harness's ATELIER_APPROVE=all: the rules decide.
    let out = run_atelier_setenv(
        &mock.base_url(),
        &dir,
        &["--print", "go"],
        None,
        &[("ATELIER_APPROVE", None)],
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(!marker.exists(), "the key command ran untrusted");
    assert!(!dir.join("ran.txt").exists(), "the allow rule was honored");
    assert_eq!(mock.requests(), 2, "{stderr}");
    assert!(!mock.head(0).contains("authorization:"), "{}", mock.head(0));
    assert!(
        mock.body(1).contains("the user denied permission"),
        "{}",
        mock.body(1)
    );
    assert_eq!(stderr.matches("project trust:").count(), 1, "{stderr}");
    assert!(
        stderr.contains("skipped 1 provider, 1 allow rule: this project is not trusted"),
        "{stderr}"
    );
    assert!(stderr.contains("ATELIER_TRUST_PROJECT=1"), "{stderr}");
    // Nothing was recorded by running there.
    let status = atelier(&mock.base_url(), &dir)
        .args(["trust", "status"])
        .output()
        .expect("run atelier trust status");
    assert_eq!(status.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(stdout.trim_end().ends_with("untrusted"), "{stdout}");
    assert!(stdout.contains("key from the command: touch"), "{stdout}");
}

/// Project trust 2 — the same project with `ATELIER_TRUST_PROJECT=1`: the
/// key command runs and its key is sent, the allow rule lets the `bash`
/// call through, and there is no notice. Nothing is recorded: the next run
/// without the variable is untrusted again.
#[cfg(unix)]
#[test]
fn the_environment_accepts_a_project_for_one_run() {
    let mock = Mock::start(bash_then_done());
    let dir = unique_dir("trust-env");
    let marker = dir.with_extension("key-command-ran");
    std::fs::write(
        dir.join("atelier.toml"),
        hostile_provider_toml(&mock.base_url(), &marker),
    )
    .unwrap();

    let out = run_atelier_setenv(
        // The environment's endpoint is nowhere: the provider is in use.
        "http://127.0.0.1:9/v1",
        &dir,
        &["--print", "go"],
        None,
        &[
            ("ATELIER_APPROVE", None),
            ("ATELIER_TRUST_PROJECT", Some("1")),
        ],
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(marker.exists(), "the key command did not run");
    assert!(dir.join("ran.txt").exists(), "the allow rule did not count");
    assert!(
        mock.head(0)
            .contains("authorization: bearer k-from-command"),
        "{}",
        mock.head(0)
    );
    assert!(!stderr.contains("project trust:"), "{stderr}");

    let status = atelier(&mock.base_url(), &dir)
        .args(["trust", "status"])
        .output()
        .expect("run atelier trust status");
    assert_eq!(status.status.code(), Some(1), "nothing was recorded");
}

/// Project trust 3 — rules that restrict need no trust: a `deny` rule in an
/// untrusted project's file still refuses the call (here even under the
/// harness's `ATELIER_APPROVE=all`), while the `allow` rule next to it is
/// what makes the project untrusted.
#[test]
fn a_deny_rule_of_an_untrusted_project_still_applies() {
    let mock = Mock::start(write_then_done());
    let dir = unique_dir("trust-deny");
    std::fs::write(
        dir.join("atelier.toml"),
        "[permissions]\nallow = [\"bash\"]\ndeny = [\"write(out.txt)\"]\n",
    )
    .unwrap();

    let out = run_print(&mock.base_url(), &dir, "go");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(!dir.join("out.txt").exists(), "the denied write ran");
    assert!(
        mock.body(1)
            .contains("refused by the permission rule `write(out.txt)`"),
        "{}",
        mock.body(1)
    );
    assert_eq!(stderr.matches("project trust:").count(), 1, "{stderr}");
    assert!(stderr.contains("skipped 1 allow rule"), "{stderr}");
}

/// A project file with a checker and an MCP server that each leave a file
/// behind when they run.
#[cfg(unix)]
fn hostile_commands_toml(checker: &Path, server: &Path) -> String {
    format!(
        "[[diagnostics]]\nname = \"lint\"\ncommand = \"touch '{}'\"\n\n\
         [[mcp]]\nname = \"helper\"\ncommand = \"sh\"\nargs = [\"-c\", \"touch '{}'\"]\n",
        checker.display(),
        server.display()
    )
}

/// Project trust 4 — an interactive session in an untrusted project: the
/// `[[diagnostics]]` command and the `[[mcp]]` command do not run, the
/// notice lists both and says how to accept, and the conversation works.
/// Once the project is trusted (`atelier trust accept`), the same session
/// start runs both — so it was trust that held them back.
#[cfg(unix)]
#[test]
fn an_untrusted_project_starts_no_server_and_runs_no_checker() {
    let mock = Mock::start(always("fine"));
    let dir = unique_dir("trust-commands");
    let (checker, server) = (dir.with_extension("checker"), dir.with_extension("server"));
    std::fs::write(
        dir.join("atelier.toml"),
        hostile_commands_toml(&checker, &server),
    )
    .unwrap();

    let mut repl = Repl::start(&mock.base_url(), &dir, &[]);
    repl.line("hello");
    wait_requests(&mock, 1);
    let (stdout, stderr) = repl.finish();
    assert!(stdout.contains("fine"), "{stdout}\n{stderr}");
    assert!(!checker.exists(), "the checker ran untrusted");
    assert!(!server.exists(), "the MCP server was started untrusted");
    assert!(stderr.contains("NOT in effect yet"), "{stderr}");
    assert!(stderr.contains("a new safety check"), "{stderr}");
    assert!(
        stderr.contains("MCP server 'helper', started with each session: sh -c 'touch "),
        "{stderr}"
    );
    assert!(
        stderr.contains("checker 'lint', run each turn: touch "),
        "{stderr}"
    );
    assert!(stderr.contains("/trust to accept"), "{stderr}");

    trust_project(&dir);
    let mut repl = Repl::start(&mock.base_url(), &dir, &[]);
    repl.line("hello");
    wait_requests(&mock, 2);
    let (_, stderr) = repl.finish();
    assert!(
        checker.exists(),
        "the checker did not run trusted: {stderr}"
    );
    assert!(
        server.exists(),
        "the server was not started trusted: {stderr}"
    );
    assert!(!stderr.contains("project trust:"), "{stderr}");
}

/// Project trust 5 — `/trust` in the session: what the project asks for is
/// shown, takes effect at once (the server is started, the checker runs
/// with the next prompt), and is on record for the next session; changing
/// a gated setting afterwards asks again, and `/trust revoke` forgets.
#[cfg(unix)]
#[test]
fn trust_command_takes_effect_in_the_session_and_is_remembered() {
    let mock = Mock::start(always("fine"));
    let dir = unique_dir("trust-command");
    let (checker, server) = (dir.with_extension("checker"), dir.with_extension("server"));
    let toml = hostile_commands_toml(&checker, &server);
    std::fs::write(dir.join("atelier.toml"), &toml).unwrap();
    let status = |dir: &Path| {
        let out = atelier(&mock.base_url(), dir)
            .args(["trust", "status"])
            .output()
            .expect("run atelier trust status");
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        (out.status.code(), stdout)
    };
    assert_eq!(status(&dir).0, Some(1));

    let mut repl = Repl::start(&mock.base_url(), &dir, &[]);
    repl.line("/trust");
    repl.line("hello");
    wait_requests(&mock, 1);
    let (stdout, stderr) = repl.finish();
    assert!(
        stdout.contains("trusted: in effect from now on"),
        "{stdout}\n{stderr}"
    );
    assert!(server.exists(), "/trust did not start the server: {stdout}");
    assert!(
        checker.exists(),
        "/trust did not enable the checker: {stdout}"
    );
    let (code, stdout) = status(&dir);
    assert_eq!(code, Some(0), "{stdout}");
    assert!(stdout.trim_end().ends_with("trusted"), "{stdout}");

    // An unrelated setting changes nothing; a gated one asks again.
    std::fs::write(
        dir.join("atelier.toml"),
        format!("{toml}\n[review]\nenabled = false\n"),
    )
    .unwrap();
    assert_eq!(status(&dir).0, Some(0));
    std::fs::write(
        dir.join("atelier.toml"),
        toml.replace("name = \"lint\"", "name = \"lint2\""),
    )
    .unwrap();
    let (code, stdout) = status(&dir);
    assert_eq!(code, Some(1), "{stdout}");
    assert!(stdout.trim_end().ends_with("changed"), "{stdout}");

    // Accept that, then revoke from a session.
    trust_project(&dir);
    assert_eq!(status(&dir).0, Some(0));
    let mut repl = Repl::start(&mock.base_url(), &dir, &[]);
    repl.line("/trust revoke");
    repl.line("/trust show");
    let (stdout, stderr) = repl.finish();
    assert!(stdout.contains("revoked:"), "{stdout}\n{stderr}");
    assert!(stdout.contains("is NOT trusted"), "{stdout}\n{stderr}");
    assert_eq!(status(&dir).0, Some(1));
}

/// Project trust 6 — a project with nothing gated is never mentioned, and
/// one the user configures through atelier (`/mcp add`) is trusted by that
/// action: the next session starts the server without asking.
#[cfg(unix)]
#[test]
fn a_plain_project_needs_no_trust_and_mcp_add_trusts_what_it_adds() {
    let mock = Mock::start(always("fine"));
    let dir = unique_dir("trust-plain");
    std::fs::write(dir.join("atelier.toml"), "[review]\nenabled = false\n").unwrap();
    let server = dir.with_extension("server");

    let mut repl = Repl::start(&mock.base_url(), &dir, &[]);
    // The server only leaves its mark and exits, so connecting fails — and
    // a server that fails to connect is not saved. `/review` rewrites the
    // file too, which must not make a plain project need trust.
    repl.line("/review on");
    repl.line("hello");
    wait_requests(&mock, 1);
    let (stdout, stderr) = repl.finish();
    assert!(!stderr.contains("project trust"), "{stderr}");
    assert!(!stdout.contains("project trust"), "{stdout}");
    let status = |dir: &Path| {
        let out = atelier(&mock.base_url(), dir)
            .args(["trust", "status"])
            .output()
            .expect("run atelier trust status");
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };
    let (code, text) = status(&dir);
    assert_eq!(code, Some(0), "{text}");
    assert!(text.trim_end().ends_with("not-needed"), "{text}");

    // A minimal MCP server: it leaves its mark, then answers `initialize`
    // and gives every other request an empty result.
    let script = dir.with_extension("mcp.sh");
    let source = r#"touch 'MARKER'
while read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  [ -n "$id" ] || continue
  case "$line" in
    *'"method":"initialize"'*)
      result='{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"t","version":"0"}}';;
    *) result='{"tools":[],"resources":[],"prompts":[]}';;
  esac
  printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$result"
done
"#;
    std::fs::write(&script, source.replace("MARKER", &server.to_string_lossy())).unwrap();
    let mut repl = Repl::start(&mock.base_url(), &dir, &[]);
    repl.line(&format!("/mcp add helper sh {}", script.display()));
    let (stdout, stderr) = repl.finish();
    assert!(
        stdout.contains("added MCP server 'helper'"),
        "{stdout}\n{stderr}"
    );
    assert!(!stdout.contains("not trusted"), "{stdout}");
    let (code, text) = status(&dir);
    assert_eq!(code, Some(0), "{text}");
    assert!(text.trim_end().ends_with("trusted"), "{text}");

    // The next session starts it, with no notice.
    std::fs::remove_file(&server).unwrap();
    let mut repl = Repl::start(&mock.base_url(), &dir, &[]);
    repl.line("hello");
    wait_requests(&mock, 2);
    let (_, stderr) = repl.finish();
    assert!(server.exists(), "{stderr}");
    assert!(!stderr.contains("project trust"), "{stderr}");
}

/// What opens a background agent's report in a request. (The `task` tool's
/// description mentions the tag too, without attributes.)
const AGENT_NOTICE: &str = "<subagent id=";

/// How long a test waits for something that should happen at once.
const PATIENCE: Duration = Duration::from_secs(20);

/// Block until `done` holds, failing the test if it does not within
/// [`PATIENCE`].
fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

/// An interactive `atelier` (the line REPL) whose output the test can watch
/// while it runs: what it printed so far is kept as it comes, so a test can
/// wait for a line before typing the next one.
struct LiveRepl {
    child: std::process::Child,
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
    readers: Vec<JoinHandle<()>>,
}

impl LiveRepl {
    fn start(base_url: &str, cwd: &Path) -> LiveRepl {
        let mut child = atelier(base_url, cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn atelier binary");
        let collect = |pipe: Box<dyn Read + Send>| {
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
                    // Decoded as a whole each time: a read may end inside
                    // a character.
                    bytes.extend_from_slice(&buf[..n]);
                    *sink.lock().unwrap() = String::from_utf8_lossy(&bytes).into_owned();
                }
            });
            (text, reader)
        };
        let (stdout, out_reader) = collect(Box::new(child.stdout.take().expect("stdout")));
        let (stderr, err_reader) = collect(Box::new(child.stderr.take().expect("stderr")));
        LiveRepl {
            child,
            stdout,
            stderr,
            readers: vec![out_reader, err_reader],
        }
    }

    /// Type one line at the prompt.
    fn line(&mut self, text: &str) {
        writeln!(self.child.stdin.as_mut().expect("stdin open"), "{text}").expect("write a line");
    }

    fn stdout(&self) -> String {
        self.stdout.lock().unwrap().clone()
    }

    /// Wait until `needle` has been printed.
    fn wait_for(&self, needle: &str) {
        wait_until(needle, || self.stdout().contains(needle));
    }

    /// Type `line` again and again until `needle` has been printed: for
    /// what the REPL only shows when it comes back to its prompt.
    fn type_until(&mut self, line: &str, needle: &str) {
        let deadline = Instant::now() + PATIENCE;
        while !self.stdout().contains(needle) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {needle:?}; stdout:\n{}\nstderr:\n{}",
                self.stdout(),
                self.stderr.lock().unwrap()
            );
            self.line(line);
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// End the input and collect everything printed (stdout, stderr).
    fn finish(mut self) -> (String, String) {
        drop(self.child.stdin.take());
        let deadline = Instant::now() + PATIENCE;
        while self.child.try_wait().expect("try_wait").is_none() {
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("atelier did not exit; stdout:\n{}", self.stdout());
            }
            thread::sleep(Duration::from_millis(20));
        }
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
        (self.stdout(), self.stderr.lock().unwrap().clone())
    }
}

/// What opens the built-in `general` agent's system prompt.
const GENERAL_MARKER: &str = "general-purpose agent";

/// A `bash` call that says it started (`started`) and then waits for the
/// test to let it go on (`go`): how a test holds a background agent in
/// place without guessing at times.
#[cfg(unix)]
fn gated_bash(id: &str) -> Vec<u8> {
    json_tool_call(
        id,
        "bash",
        serde_json::json!({
            "command": "echo > started; while [ ! -e go ]; do sleep 0.02; done; echo released"
        }),
    )
}

/// One more step of other work for a parent that waits on a background
/// agent's report — a `read` of `busy.txt`, paced — for as long as the
/// agent can possibly need; past that, an answer that fails the test.
fn busy_step(since: Instant, n: usize) -> Vec<u8> {
    if since.elapsed() > PATIENCE {
        return text_response("the agent never reported");
    }
    thread::sleep(Duration::from_millis(10));
    json_tool_call(
        &format!("call_busy_{n}"),
        "read",
        serde_json::json!({"path": "busy.txt"}),
    )
}

/// Background agents 1 — within a turn. `task` with `background: true`
/// returns at once with the agent's id, and the parent goes on with other
/// work (here: reads, one step after the other, for as long as the agent
/// has not reported). When the agent ends, its report arrives between two
/// steps as a user message: the marked block and the line saying it is the
/// harness speaking. Once, however many steps follow, and the user is shown
/// one line for its start and one for its end.
#[test]
fn a_background_agent_reports_later_in_the_same_turn_exactly_once() {
    let started = Instant::now();
    let mock = Mock::by_body(Arc::new(move |body| {
        let req = request(body);
        let results = messages_of(&req, "tool").len();
        if system_message(body).contains(EXPLORE_MARKER) {
            return match results {
                0 => json_tool_call(
                    "call_read",
                    "read",
                    serde_json::json!({"path": "notes.txt"}),
                ),
                _ => text_response("CHILD-REPORT: notes.txt:1 holds the marker"),
            };
        }
        let told = body.contains(AGENT_NOTICE);
        match results {
            0 => json_tool_call(
                "call_task",
                "task",
                serde_json::json!({
                    "agent": "explore",
                    "prompt": "CHILD-TASK: say what notes.txt holds",
                    "description": "inspect the notes",
                    "background": true,
                }),
            ),
            _ if told && body.contains("AFTER-THE-NOTICE") => text_response("parent done"),
            _ if told => json_tool_call(
                "call_after",
                "read",
                serde_json::json!({"path": "after.txt"}),
            ),
            n => busy_step(started, n),
        }
    }));
    let dir = unique_dir("bg-agent");
    std::fs::write(dir.join("notes.txt"), "MARKER-IN-FILE\n").unwrap();
    std::fs::write(dir.join("busy.txt"), "busy\n").unwrap();
    std::fs::write(dir.join("after.txt"), "AFTER-THE-NOTICE\n").unwrap();

    let out = run_atelier(
        &mock.base_url(),
        &dir,
        &[
            "-p",
            "--output-format",
            "stream-json",
            "look into the notes",
        ],
        None,
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    let events = json_lines(&out);
    assert_eq!(
        events.last().unwrap()["result"],
        "parent done",
        "{events:?}"
    );

    // The call returned before the agent had reported: its result is the
    // id, in the request that follows the call.
    let parent = requests_with_system(&mock, PARENT_MARKER);
    let results = messages_of(&parent[1], "tool");
    assert_eq!(results.len(), 1);
    assert!(
        results[0].starts_with("started background agent agent-1 (explore)."),
        "{results:?}"
    );
    assert!(results[0].contains("do not poll"), "{results:?}");

    // The agent ran as any `explore` does.
    let child = requests_with_system(&mock, EXPLORE_MARKER);
    assert_eq!(child.len(), 2);
    assert!(messages_of(&child[1], "tool")[0].contains("MARKER-IN-FILE"));

    // Its report: once in the whole conversation, as the last request holds
    // it, in a user message.
    let last = mock.body(mock.requests() - 1);
    assert_eq!(last.matches(AGENT_NOTICE).count(), 1, "{last}");
    let users = messages_of(&request(&last), "user");
    let notice = users
        .iter()
        .find(|m| m.contains(AGENT_NOTICE))
        .expect("the report is a user message");
    assert!(
        notice.starts_with(
            "<subagent id=\"agent-1\" agent=\"explore\" status=\"done\" steps=\"2\">\n\
             CHILD-REPORT: notes.txt:1 holds the marker\n\n[explore agent-1: 2 steps"
        ),
        "{notice}"
    );
    assert!(notice.contains("]\n</subagent>\n"), "{notice}");
    assert!(notice.contains("not a message from your user"), "{notice}");

    // The user: a line when it starts, a line when it ends, and no echo of
    // the block as something they typed.
    let info: Vec<&str> = of_type(&events, "info")
        .iter()
        .filter_map(|e| e["text"].as_str())
        .filter(|t| t.contains("agent-1"))
        .collect();
    assert_eq!(
        info,
        [
            "⤷ explore agent-1: inspect the notes (in the background)",
            "⤶ explore agent-1: done (2 steps)"
        ]
    );
    assert_eq!(of_type(&events, "tool_end")[0]["ok"], true);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout.matches("<subagent id").count(), 0, "{stdout}");
}

/// A mock for one background `general` agent held at a gate (see
/// [`gated_bash`]): the parent starts it and answers `started`; a prompt
/// containing `second prompt` is answered `seen`. The agent reports
/// `CHILD-REPORT: released` once let go.
#[cfg(unix)]
fn gated_agent_mock() -> Mock {
    Mock::by_body(Arc::new(|body| {
        let req = request(body);
        let results = messages_of(&req, "tool").len();
        if system_message(body).contains(GENERAL_MARKER) {
            return match results {
                0 => gated_bash("call_gate"),
                _ => text_response("CHILD-REPORT: released"),
            };
        }
        let users = messages_of(&req, "user");
        match results {
            _ if users.last().unwrap().contains("second prompt") => text_response("seen"),
            0 => json_tool_call(
                "call_task",
                "task",
                serde_json::json!({
                    "agent": "general",
                    "prompt": "CHILD-TASK",
                    "run_in_background": true,
                }),
            ),
            _ => text_response("started"),
        }
    }))
}

/// Wait for the gated agent's `bash` to be running.
#[cfg(unix)]
fn wait_for_gate(dir: &Path) {
    let started = dir.join("started");
    wait_until("the agent's bash call", || started.exists());
}

/// Background agents 2 — between turns. An agent that ends while the
/// session is idle does not wake the model: the user is shown one line when
/// the session next looks, nothing is requested until their next prompt,
/// and that prompt carries the report in front of it. Meanwhile `/agents`
/// lists the agent with what it is doing.
#[cfg(unix)]
#[test]
fn a_background_agent_that_ends_between_turns_leads_the_next_prompt() {
    let mock = gated_agent_mock();
    let dir = unique_dir("bg-agent-idle");
    let mut repl = LiveRepl::start(&mock.base_url(), &dir);

    repl.line("first prompt");
    // The turn ends while the agent is still at its gate.
    repl.wait_for("started");
    wait_for_gate(&dir);
    repl.line("/agents");
    repl.wait_for("in the background (/agents stop <id> stops one):");
    repl.wait_for("  agent-1  general — CHILD-TASK · running ");
    repl.wait_for("s · bash echo > started; while");

    // Let it finish; the REPL shows its line when it is next at the prompt.
    std::fs::write(dir.join("go"), "").unwrap();
    repl.type_until("/agents", "⤶ general agent-1: done (2 steps)");
    // Nobody was asked anything on its account: the parent's two requests
    // and the agent's two are all there is.
    assert_eq!(mock.requests(), 4);
    assert!((0..4).all(|n| !mock.body(n).contains(AGENT_NOTICE)));

    repl.line("second prompt");
    let (stdout, stderr) = repl.finish();
    assert_eq!(mock.requests(), 5, "stdout: {stdout}\nstderr: {stderr}");
    let last = mock.body(4);
    assert_eq!(last.matches(AGENT_NOTICE).count(), 1, "{last}");
    let users = messages_of(&request(&last), "user");
    // (After the turn's project context, which opens the message.)
    let prompt = users.last().unwrap();
    assert!(
        prompt.contains(
            "<subagent id=\"agent-1\" agent=\"general\" status=\"done\" steps=\"2\">\n\
             CHILD-REPORT: released\n\n[general agent-1: 2 steps"
        ),
        "{prompt}"
    );
    assert!(prompt.ends_with("second prompt"), "{prompt}");
    assert_eq!(
        stdout.matches("⤶ general agent-1: done").count(),
        1,
        "{stdout}"
    );
    assert!(stdout.contains("seen"), "{stdout}");
}

/// Background agents 3 — `/agents stop`. Stopping is cooperative: the agent
/// ends after the call it is in, without another request, and what comes
/// back is marked as stopped rather than as a report.
#[cfg(unix)]
#[test]
fn agents_stop_ends_a_background_agent_at_its_next_step() {
    let mock = gated_agent_mock();
    let dir = unique_dir("bg-agent-stop");
    let mut repl = LiveRepl::start(&mock.base_url(), &dir);

    repl.line("first prompt");
    repl.wait_for("started");
    wait_for_gate(&dir);
    repl.line("/agents stop agent-7");
    repl.wait_for("no background agent agent-7");
    repl.line("/agents stop agent-1");
    repl.wait_for("stopping agent-1 (general)");
    // Still in its call: it has not ended yet.
    assert!(!repl.stdout().contains("⤶ general agent-1"));

    std::fs::write(dir.join("go"), "").unwrap();
    repl.type_until("/agents", "⤶ general agent-1: stopped (1 step)");
    // It never sent the result of that call.
    assert_eq!(requests_with_system(&mock, GENERAL_MARKER).len(), 1);
    repl.line("/agents stop agent-1");
    repl.wait_for("agent-1 has already ended");

    repl.line("second prompt");
    let (stdout, stderr) = repl.finish();
    let last = mock.body(mock.requests() - 1);
    let users = messages_of(&request(&last), "user");
    let prompt = users.last().unwrap();
    assert!(
        prompt
            .contains("<subagent id=\"agent-1\" agent=\"general\" status=\"stopped\" steps=\"1\">"),
        "{prompt}\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(prompt.contains("stopped before it finished"), "{prompt}");
    assert!(!prompt.contains("CHILD-REPORT"), "{prompt}");
}

/// Background agents 4 — leaving. With an agent running, the first `/quit`
/// only warns; the second goes ahead, and the agent is stopped with the
/// session.
#[cfg(unix)]
#[test]
fn quitting_with_a_background_agent_running_warns_first() {
    let mock = gated_agent_mock();
    let dir = unique_dir("bg-agent-quit");
    let mut repl = LiveRepl::start(&mock.base_url(), &dir);

    repl.line("first prompt");
    repl.wait_for("started");
    wait_for_gate(&dir);
    repl.line("/quit");
    repl.wait_for("1 background agent(s) are running; quitting would stop them.");
    repl.wait_for("quit again to quit anyway");
    repl.line("/quit");
    // More input, which a session that quit never reads.
    repl.line("second prompt");
    // The agent is let out of its call, so that atelier finds it stopped
    // on the way out instead of giving up on it.
    std::fs::write(dir.join("go"), "").unwrap();
    let (stdout, _stderr) = repl.finish();
    assert_eq!(
        stdout.matches("quitting would stop them").count(),
        1,
        "{stdout}"
    );
    assert!(!stdout.contains("seen"), "{stdout}");
}

/// Background agents 5 — the limit. With `[agents] max_background = 1`, the
/// second of two background calls in one step is refused with an error
/// result; the first runs.
#[cfg(unix)]
#[test]
fn background_agents_past_the_limit_are_refused() {
    let dir = unique_dir("bg-agent-limit");
    let go = dir.join("go");
    let mock = Mock::by_body(Arc::new(move |body| {
        let req = request(body);
        let results = messages_of(&req, "tool").len();
        if system_message(body).contains(GENERAL_MARKER) {
            return match results {
                0 => gated_bash("call_gate"),
                _ => text_response("CHILD-REPORT"),
            };
        }
        let task = |prompt: &str| serde_json::json!({"agent": "general", "prompt": prompt, "background": true});
        match results {
            0 => tool_calls_response(&[
                ("call_a", "task", task("TASK-ALPHA")),
                ("call_b", "task", task("TASK-BETA")),
            ]),
            _ => {
                // Both calls have their results: the first agent may go.
                std::fs::write(&go, "").unwrap();
                text_response("parent done")
            }
        }
    }));
    std::fs::write(dir.join("atelier.toml"), "[agents]\nmax_background = 1\n").unwrap();

    let out = run_atelier(
        &mock.base_url(),
        &dir,
        &["-p", "--output-format", "stream-json", "do both"],
        None,
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    let parent = requests_with_system(&mock, PARENT_MARKER);
    let results = tool_results(&parent[1]);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].0, "call_a");
    assert!(
        results[0].1.starts_with("started background agent agent-1"),
        "{results:?}"
    );
    assert_eq!(results[1].0, "call_b");
    assert!(
        results[1]
            .1
            .starts_with("error: 1 background agents are running already, which is the limit"),
        "{results:?}"
    );
    let events = json_lines(&out);
    let ok: Vec<&serde_json::Value> = of_type(&events, "tool_end")
        .iter()
        .map(|e| &e["ok"])
        .collect();
    assert_eq!(ok, [&serde_json::json!(true), &serde_json::json!(false)]);
    // Only the first ever ran.
    let child = requests_with_system(&mock, GENERAL_MARKER);
    assert!(
        child
            .iter()
            .all(|r| messages_of(r, "user") == ["TASK-ALPHA"])
    );
}

/// Background agents 6 — approvals. A background agent has no user to ask:
/// without auto-approve, its `bash` call — which would prompt in the
/// foreground — is refused with a result that says why and what to do, the
/// command does not run, and nobody is prompted. What the rules let through
/// without asking (a write inside the project) still runs.
#[test]
fn a_background_agents_call_that_would_ask_is_denied() {
    let started = Instant::now();
    let mock = Mock::by_body(Arc::new(move |body| {
        let req = request(body);
        let results = messages_of(&req, "tool").len();
        if system_message(body).contains(GENERAL_MARKER) {
            return match results {
                0 => json_tool_call(
                    "call_bash",
                    "bash",
                    serde_json::json!({"command": "echo made > made.txt"}),
                ),
                1 => json_tool_call(
                    "call_write",
                    "write",
                    serde_json::json!({"path": "written.txt", "content": "by the agent"}),
                ),
                _ => text_response("CHILD-REPORT: I needed bash to make made.txt"),
            };
        }
        match results {
            0 => json_tool_call(
                "call_task",
                "task",
                serde_json::json!({
                    "agent": "general",
                    "prompt": "make the files",
                    "background": true,
                }),
            ),
            _ if body.contains(AGENT_NOTICE) => text_response("parent done"),
            n => busy_step(started, n),
        }
    }));
    let dir = unique_dir("bg-agent-approval");
    std::fs::write(dir.join("busy.txt"), "busy\n").unwrap();

    let out = run_atelier_setenv(
        &mock.base_url(),
        &dir,
        &["-p", "--output-format", "stream-json", "go"],
        None,
        &[("ATELIER_APPROVE", None)],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    let events = json_lines(&out);
    assert_eq!(
        events.last().unwrap()["result"],
        "parent done",
        "{events:?}"
    );

    let child = requests_with_system(&mock, GENERAL_MARKER);
    assert_eq!(child.len(), 3);
    let results = messages_of(&child[2], "tool");
    assert!(
        results[0].contains("you are running in the background, where there is no user to ask"),
        "{results:?}"
    );
    assert!(
        results[0].contains("say in your final report"),
        "{results:?}"
    );
    assert!(!dir.join("made.txt").exists());
    assert_eq!(
        std::fs::read_to_string(dir.join("written.txt")).unwrap(),
        "by the agent"
    );
    // The report reached the parent, and no prompt was put to anyone.
    let last = mock.body(mock.requests() - 1);
    assert!(last.contains("CHILD-REPORT: I needed bash"), "{last}");
    assert!(of_type(&events, "approval").is_empty(), "{events:?}");
}
