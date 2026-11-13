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
    // Hook approvals (see the hooks cases) are neither read from nor
    // written to the real home.
    cmd.env("XDG_STATE_HOME", cwd.with_extension("state"))
        .env_remove("ATELIER_TRUST_HOOKS")
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
        .current_dir(cwd);
    cmd
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
        .env("HOME", cwd.with_extension("home"))
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
/// and run `--print` there. `trusted` sets `ATELIER_TRUST_HOOKS=1`, the
/// only way a headless run takes a project's hooks.
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

/// Hooks 5 — a headless run never asks: without `ATELIER_TRUST_HOOKS=1` an
/// unapproved project's hooks are skipped, with one notice on stderr.
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
    assert_eq!(
        stderr.matches("project hook(s) skipped").count(),
        1,
        "{stderr}"
    );
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
        &[("ATELIER_DEBUG", "1")],
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
        &[("ATELIER_DEBUG", "1")],
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
        &[("ATELIER_MODEL", "vault/m")],
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
