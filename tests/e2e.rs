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

/// A background mock OpenAI-compatible server. Accepts connections until
/// dropped; each connection is answered by the `Responder`, which is handed a
/// monotonically increasing request index so one mock can script a multi-step
/// exchange.
struct Mock {
    port: u16,
    /// The body of every request received so far, in order, so a test can
    /// check what atelier actually sent.
    bodies: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Mock {
    fn start(responder: Responder) -> Mock {
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
        let handle = thread::spawn(move || {
            let counter = AtomicUsize::new(0);
            while !stop_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let n = counter.fetch_add(1, Ordering::SeqCst);
                        // Ignore per-connection errors: a flaky/aborted
                        // connection must never bring the server down.
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

    /// Base URL (including the `/v1` suffix the provider appends endpoints to)
    /// for `ATELIER_BASE_URL`.
    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    /// The body of the Nth request (0-based) atelier sent.
    fn body(&self, n: usize) -> String {
        self.bodies.lock().unwrap()[n].clone()
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
    responder: &Responder,
    bodies: &Mutex<Vec<String>>,
) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);

    // Request line + headers, up to the blank line that ends the head.
    let mut content_length = 0usize;
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
    }

    // Drain the body so the client's write completes before we reply, and
    // keep it for tests that assert on what was sent.
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;
    bodies
        .lock()
        .unwrap()
        .push(String::from_utf8_lossy(&body).into_owned());

    let response = responder(n);
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
    let bin = env!("CARGO_BIN_EXE_atelier");
    let mut child = Command::new(bin)
        .args(args)
        .env("ATELIER_BASE_URL", base_url)
        .env("ATELIER_MODEL", "mock")
        .env("ATELIER_APPROVE", "all")
        // No coordinator daemon left behind by the test run.
        .env("ATELIER_NO_COORDINATOR", "1")
        // Keep the run hermetic regardless of the outer environment: no API
        // key, no user config (global instructions, a saved login), and
        // global memory next to the temp project, never the real home.
        .env("XDG_CONFIG_HOME", cwd.join(".no-user-config"))
        .env("XDG_DATA_HOME", cwd.with_extension("data"))
        // Likewise the user's skills under ~/.claude and ~/.agents.
        .env("HOME", cwd.with_extension("home"))
        .env_remove("ATELIER_API_KEY")
        .env_remove("ATELIER_TRACE")
        .env_remove("ATELIER_DEBUG")
        .current_dir(cwd)
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
