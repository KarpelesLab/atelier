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
//! Uses only `std` — no extra dependencies, and nothing outside `tests/`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
        let handle = thread::spawn(move || {
            let counter = AtomicUsize::new(0);
            while !stop_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let n = counter.fetch_add(1, Ordering::SeqCst);
                        // Ignore per-connection errors: a flaky/aborted
                        // connection must never bring the server down.
                        let _ = handle_conn(stream, n, &responder);
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
            stop,
            handle: Some(handle),
        }
    }

    /// Base URL (including the `/v1` suffix the provider appends endpoints to)
    /// for `ATELIER_BASE_URL`.
    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
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
fn handle_conn(stream: TcpStream, n: usize, responder: &Responder) -> std::io::Result<()> {
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

    // Drain the body so the client's write completes before we reply.
    if content_length > 0 {
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body)?;
    }

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

/// Spawn `atelier --print <prompt>` against `base_url` with `cwd`, wait for it
/// to exit (killing it past a generous deadline so a hang can't wedge the
/// suite), and return its captured output.
fn run_print(base_url: &str, cwd: &Path, prompt: &str) -> Output {
    let bin = env!("CARGO_BIN_EXE_atelier");
    let mut child = Command::new(bin)
        .arg("--print")
        .arg(prompt)
        .env("ATELIER_BASE_URL", base_url)
        .env("ATELIER_MODEL", "mock")
        .env("ATELIER_APPROVE", "all")
        // Keep the run hermetic regardless of the outer environment.
        .env_remove("ATELIER_API_KEY")
        .env_remove("ATELIER_TRACE")
        .env_remove("ATELIER_DEBUG")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn atelier binary");

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
