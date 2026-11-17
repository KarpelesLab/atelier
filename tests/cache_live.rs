//! A live measurement of what the cache-stable prompt buys (see
//! `src/agent/prompt.rs` and docs/sessions.md, "What the cache is worth"),
//! through the real `atelier` binary against a real model server. Ignored
//! by default: it needs the endpoint, and runs a few short sessions on it.
//!
//! ```sh
//! cargo test --test cache_live -- --ignored --nocapture
//! ```
//!
//! The endpoint and model are the ones atelier itself would use:
//! `ATELIER_BASE_URL` and `ATELIER_MODEL`, else the built-in development
//! server. `ATELIER_CACHE_SESSIONS` is the number of sessions per arm
//! (default 2). Only `http://` endpoints: the recorder in the middle speaks
//! no TLS.
//!
//! The same scripted session — five prompts that make the model read,
//! search and edit a small project — is run in three arms, in turn:
//!
//! - **stable**: the requests go out as atelier builds them, each one a
//!   prefix of the next;
//! - **system message changes**: a recorder between atelier and the server
//!   writes a different tag at the start of the system message of every
//!   request, the way a prompt that carried per-turn context in its system
//!   message used to change;
//! - **nothing reusable**: the tag goes at the start of the first tool's
//!   description as well. Chat templates differ in which of the two they
//!   render first — the tool list, with the Qwen ones — so only this arm
//!   changes the first tokens of the prompt on every server.
//!
//! Nothing else differs between the arms. For every request the recorder notes the time to the first streamed
//! token (reasoning counts) and the endpoint's own count of prompt tokens
//! and of those it served from its cache
//! (`usage.prompt_tokens_details.cached_tokens`; llama.cpp's
//! `timings.cache_n` where that is what the server sends). In the stable
//! arm it also checks, on the real traffic, what the e2e suite checks
//! against the mock: that each request starts with the bytes of the one
//! before.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// The endpoint and model atelier falls back to (`src/config.rs`).
const DEFAULT_BASE_URL: &str = "http://192.168.0.50:11400/v1";
const DEFAULT_MODEL: &str = "qwen3.8-unc:q4";

/// How long the server may stay silent: a cold prompt takes a while.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(600);

/// The session: each prompt is a turn, and most need a tool or two.
const PROMPTS: [&str; 5] = [
    "Read src/lib.rs and say in one sentence what it does.",
    "Add a function `sub(a: i64, b: i64) -> i64` next to `add` in src/lib.rs.",
    "Use the grep tool to find every `pub fn` under src and list their names.",
    "Write that list to NOTES.md, one name per line.",
    "Read NOTES.md back and say in one word whether it names both functions.",
];

/// What the bytes every request body starts with look like: the system
/// message is the first thing in it (`provider::chat_body`).
const BODY_START: &str = r#"{"messages":[{"content":""#;
/// Likewise the tool list, up to the first tool's description.
const TOOLS_START: &str = r#""tools":[{"function":{"description":""#;

/// What the recorder does to the requests of the conversation.
#[derive(Clone, Copy, PartialEq)]
enum Arm {
    /// Nothing.
    Stable,
    /// A different first word in the system message, every request.
    System,
    /// And in the first tool's description.
    Everything,
}

impl Arm {
    fn title(self) -> &'static str {
        match self {
            Arm::Stable => "stable prefix (atelier as it is)",
            Arm::System => "the system message changes with every request",
            Arm::Everything => "nothing reusable (system message and tool list change)",
        }
    }
}

/// One completion request, as the recorder saw it.
#[derive(Debug, Clone)]
struct Sample {
    /// The body atelier sent (before a control arm's tag went in).
    body: String,
    /// A request of the conversation, which carries the tool list — as
    /// against a compaction's summary request, which has none.
    conversation: bool,
    /// From the request being read in full to the first streamed token.
    first_token: Option<Duration>,
    /// From the same moment to the end of the response.
    total: Duration,
    prompt_tokens: u64,
    cached_tokens: u64,
    completion_tokens: u64,
    status: u16,
}

/// Where the requests really go.
#[derive(Clone)]
struct Upstream {
    /// `host:port`.
    authority: String,
    /// The base URL's path (`/v1`), without a trailing slash.
    path: String,
}

impl Upstream {
    fn parse(base_url: &str) -> Upstream {
        let rest = base_url
            .strip_prefix("http://")
            .unwrap_or_else(|| panic!("{base_url}: only http:// endpoints can be recorded"));
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        let authority = if authority.contains(':') {
            authority.to_string()
        } else {
            format!("{authority}:80")
        };
        Upstream {
            authority,
            path: path.trim_end_matches('/').to_string(),
        }
    }
}

/// The recorder: an HTTP server on loopback that passes each request on to
/// the real endpoint and times the answer.
struct Recorder {
    port: u16,
    samples: Arc<Mutex<Vec<Sample>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Recorder {
    fn start(upstream: Upstream, arm: Arm) -> Recorder {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the recorder");
        let port = listener.local_addr().expect("local addr").port();
        listener.set_nonblocking(true).expect("non-blocking accept");
        let stop = Arc::new(AtomicBool::new(false));
        let samples = Arc::new(Mutex::new(Vec::new()));
        let (stop_thread, samples_thread) = (Arc::clone(&stop), Arc::clone(&samples));
        let handle = thread::spawn(move || {
            let counter = AtomicUsize::new(0);
            while !stop_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let n = counter.fetch_add(1, Ordering::SeqCst);
                        // atelier makes one request at a time here, so one
                        // connection at a time is enough.
                        if let Err(e) = relay(stream, &upstream, arm, n)
                            .map(|sample| samples_thread.lock().unwrap().extend(sample))
                        {
                            eprintln!("recorder: request {n}: {e}");
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Recorder {
            port,
            samples,
            stop,
            handle: Some(handle),
        }
    }

    /// What to set `ATELIER_BASE_URL` to.
    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    fn finish(mut self) -> Vec<Sample> {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        self.samples.lock().unwrap().clone()
    }
}

/// A request or response head: its first line, and the headers lowercased.
struct Head {
    first: String,
    headers: Vec<(String, String)>,
}

impl Head {
    fn read(reader: &mut impl BufRead) -> std::io::Result<Head> {
        let mut first = String::new();
        reader.read_line(&mut first)?;
        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 || line.trim_end().is_empty() {
                break;
            }
            if let Some((name, value)) = line.trim_end().split_once(':') {
                headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
            }
        }
        Ok(Head {
            first: first.trim_end().to_string(),
            headers,
        })
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// What the lines of an event stream say, as they arrive.
#[derive(Default)]
struct Stream {
    /// An unfinished line.
    pending: Vec<u8>,
    first_token: Option<Instant>,
    prompt_tokens: u64,
    cached_tokens: u64,
    completion_tokens: u64,
}

impl Stream {
    fn feed(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
        while let Some(end) = self.pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line);
            if let Some(data) = line.trim().strip_prefix("data:")
                && let Ok(event) = serde_json::from_str::<Value>(data.trim())
            {
                self.event(&event);
            }
        }
    }

    fn event(&mut self, event: &Value) {
        let delta = &event["choices"][0]["delta"];
        let said = ["content", "reasoning", "reasoning_content"]
            .iter()
            .any(|key| delta[key].as_str().is_some_and(|text| !text.is_empty()))
            || delta["tool_calls"]
                .as_array()
                .is_some_and(|c| !c.is_empty());
        if said && self.first_token.is_none() {
            self.first_token = Some(Instant::now());
        }
        let usage = &event["usage"];
        if usage.is_object() {
            self.prompt_tokens = usage["prompt_tokens"].as_u64().unwrap_or(0);
            self.completion_tokens = usage["completion_tokens"].as_u64().unwrap_or(0);
            if let Some(n) = usage["prompt_tokens_details"]["cached_tokens"].as_u64() {
                self.cached_tokens = n;
            }
        }
        // llama.cpp's own account of the same thing.
        let timings = &event["timings"];
        if let Some(n) = timings["cache_n"].as_u64() {
            self.cached_tokens = self.cached_tokens.max(n);
            if self.prompt_tokens == 0 {
                self.prompt_tokens = n + timings["prompt_n"].as_u64().unwrap_or(0);
            }
        }
    }
}

/// Pass one request on and its answer back. A completion request yields a
/// [`Sample`]; anything else (`GET /models`) is only passed on.
fn relay(
    client: TcpStream,
    upstream: &Upstream,
    arm: Arm,
    n: usize,
) -> std::io::Result<Option<Sample>> {
    client.set_nonblocking(false)?;
    client.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut to_client = client.try_clone()?;
    let mut from_client = BufReader::new(client);

    let head = Head::read(&mut from_client)?;
    let length = head
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    from_client.read_exact(&mut body)?;
    let body = String::from_utf8_lossy(&body).into_owned();
    let mut words = head.first.split_whitespace();
    let (method, path) = (words.next().unwrap_or("GET"), words.next().unwrap_or("/"));
    let is_completion = method == "POST" && path.ends_with("/chat/completions");

    let conversation = is_completion && body.contains(TOOLS_START);
    let mut sent = body.clone();
    if conversation && arm != Arm::Stable {
        assert!(body.starts_with(BODY_START), "an unexpected request body");
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let tag = format!("[request {n}-{nonce:08x}] ");
        if arm == Arm::Everything {
            sent = sent.replacen(TOOLS_START, &format!("{TOOLS_START}{tag}"), 1);
        }
        sent.insert_str(BODY_START.len(), &tag);
    }

    let started = Instant::now();
    let server = TcpStream::connect(&upstream.authority)?;
    server.set_read_timeout(Some(UPSTREAM_TIMEOUT))?;
    let mut to_server = server.try_clone()?;
    // atelier's base URL here is the recorder's `/v1`.
    let path = format!(
        "{}{}",
        upstream.path,
        path.strip_prefix("/v1").unwrap_or(path)
    );
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nhost: {}\r\nconnection: close\r\ncontent-length: {}\r\n",
        upstream.authority,
        sent.len()
    );
    for (name, value) in &head.headers {
        if !["host", "connection", "content-length", "accept-encoding"].contains(&name.as_str()) {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    request.push_str("\r\n");
    to_server.write_all(request.as_bytes())?;
    to_server.write_all(sent.as_bytes())?;
    to_server.flush()?;

    let mut from_server = BufReader::new(server);
    let answer = Head::read(&mut from_server)?;
    let status = answer
        .first
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let content_type = answer.get("content-type").unwrap_or("application/json");
    // The answer goes back close-delimited, whatever framing it came in.
    write!(
        to_client,
        "{}\r\ncontent-type: {content_type}\r\nconnection: close\r\n\r\n",
        answer.first
    )?;
    let mut stream = Stream::default();
    let mut pass = |bytes: &[u8]| -> std::io::Result<()> {
        stream.feed(bytes);
        to_client.write_all(bytes)?;
        to_client.flush()
    };
    let chunked = answer
        .get("transfer-encoding")
        .is_some_and(|v| v.eq_ignore_ascii_case("chunked"));
    if chunked {
        loop {
            let mut size = String::new();
            if from_server.read_line(&mut size)? == 0 {
                break;
            }
            let size = size.trim().split(';').next().unwrap_or("");
            let size = usize::from_str_radix(size, 16).unwrap_or(0);
            if size == 0 {
                break;
            }
            let mut chunk = vec![0u8; size + 2];
            from_server.read_exact(&mut chunk)?;
            pass(&chunk[..size])?;
        }
    } else {
        let mut buffer = [0u8; 8192];
        loop {
            let n = from_server.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            pass(&buffer[..n])?;
        }
    }
    let _ = to_client.shutdown(Shutdown::Write);

    Ok(is_completion.then(|| Sample {
        body,
        conversation,
        first_token: stream.first_token.map(|at| at - started),
        total: started.elapsed(),
        prompt_tokens: stream.prompt_tokens,
        cached_tokens: stream.cached_tokens,
        completion_tokens: stream.completion_tokens,
        status,
    }))
}

/// A fresh directory holding a small project for the session to work on,
/// `sums`, with room next to it for what atelier keeps outside a project.
fn scratch(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let scratch = std::env::temp_dir().join(format!("atelier-cache-{tag}-{nanos}"));
    let dir = scratch.join("sums");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"sums\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    // Large enough that the conversation, once the model has read it, is
    // a good part of the prompt: some 4,000 tokens.
    let mut lib = String::from(
        "//! Small integer helpers.\n\n\
         /// The sum of `a` and `b`.\n\
         pub fn add(a: i64, b: i64) -> i64 {\n    a + b\n}\n",
    );
    for n in 2..=60 {
        lib.push_str(&format!(
            "\n/// `value` scaled by {n}, saturating at the bounds of `i64` rather than\n\
             /// wrapping around, which is what callers summing many of these expect.\n\
             pub fn times_{n}(value: i64) -> i64 {{\n    value.saturating_mul({n})\n}}\n"
        ));
    }
    std::fs::write(dir.join("src/lib.rs"), lib).unwrap();
    scratch
}

/// Run the scripted session through a recorder and return what it saw.
fn session(upstream: &Upstream, model: &str, arm: Arm, tag: &str) -> Vec<Sample> {
    let recorder = Recorder::start(upstream.clone(), arm);
    let scratch = scratch(tag);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_atelier"));
    cmd.args([
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
    ])
    .current_dir(scratch.join("sums"))
    .env("ATELIER_BASE_URL", recorder.base_url())
    .env("ATELIER_MODEL", model)
    // Nothing of the user's own setup: no instructions, memory, skills or
    // settings, so the prompt is the default build's and nothing else.
    .env("XDG_STATE_HOME", scratch.join("state"))
    .env("XDG_CONFIG_HOME", scratch.join("config"))
    .env("XDG_DATA_HOME", scratch.join("data"))
    .env("HOME", scratch.join("home"))
    .env("ATELIER_NO_COORDINATOR", "1")
    // The default mode: edits in the project go ahead, a shell command
    // would ask and, with nobody to ask, be refused.
    .env_remove("ATELIER_APPROVE")
    .env_remove("ATELIER_MODE")
    .env_remove("ATELIER_BACKEND")
    .env_remove("ATELIER_CONTEXT_LIMIT")
    // A reviewer's requests would go to the same server between the
    // session's own and take its cache.
    .env_remove("ATELIER_REVIEW")
    .env_remove("ATELIER_TRACE")
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("running atelier");
    {
        let mut stdin = child.stdin.take().expect("stdin");
        for prompt in PROMPTS {
            let line = serde_json::json!({"type": "user", "text": prompt});
            writeln!(stdin, "{line}").expect("writing a prompt");
        }
    }
    let out = child.wait_with_output().expect("waiting for atelier");
    let samples = recorder.finish();
    assert!(
        out.status.success(),
        "atelier failed ({:?})\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&scratch);
    samples
}

/// The `messages` array of a request body, as sent (it is the body's first
/// key, and `model` the second).
fn raw_messages(body: &str) -> &str {
    let start = body.find(r#""messages":["#).expect("a messages array") + r#""messages":"#.len();
    let end = body.rfind(r#","model":"#).expect("model follows messages");
    &body[start..end]
}

/// Every request of the conversation starts with the bytes of the one
/// before, and carries the same tool list. A compaction is the one thing
/// that may rewrite it, and shows as a summary request in between.
fn assert_prefixes(samples: &[Sample]) {
    for (n, pair) in samples.windows(2).enumerate() {
        if !(pair[0].conversation && pair[1].conversation) {
            continue;
        }
        let (before, after) = (raw_messages(&pair[0].body), raw_messages(&pair[1].body));
        assert!(
            after.starts_with(&before[..before.len() - 1]),
            "request {} does not start with the bytes of request {n}:\n{before}\n{after}",
            n + 1
        );
        // What follows the messages: the model, the options, the tool list.
        let rest = |body: &'_ str| body.rfind(r#","model":"#).map(|at| body[at..].to_string());
        assert_eq!(
            rest(&pair[0].body),
            rest(&pair[1].body),
            "the model, options or tool list changed at request {}",
            n + 1
        );
    }
}

fn seconds(d: Duration) -> f64 {
    d.as_secs_f64()
}

fn median(mut values: Vec<f64>) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len() % 2 == 1 {
        values[mid]
    } else {
        (values[mid - 1] + values[mid]) / 2.0
    }
}

/// The requests of an arm that say something about the layout: those of the
/// conversation after each session's first, which meets whatever the server
/// evaluated last.
fn later(sessions: &[Vec<Sample>]) -> Vec<&Sample> {
    sessions
        .iter()
        .flat_map(|s| s.iter().filter(|r| r.conversation).skip(1))
        .collect()
}

/// One arm's sessions, as a table and a summary line.
fn report(arm: Arm, sessions: &[Vec<Sample>]) {
    println!("\n{}", arm.title());
    println!("  session  request  prompt tok  cached tok  first token  whole request");
    for (s, samples) in sessions.iter().enumerate() {
        for (r, sample) in samples.iter().enumerate() {
            let first = sample
                .first_token
                .map_or_else(|| "-".to_string(), |d| format!("{:.2}s", seconds(d)));
            println!(
                "  {:>7}  {:>7}  {:>10}  {:>10}  {:>11}  {:>12.2}s{}",
                s + 1,
                r + 1,
                sample.prompt_tokens,
                sample.cached_tokens,
                first,
                seconds(sample.total),
                if sample.conversation {
                    ""
                } else {
                    "  (a compaction's summary)"
                }
            );
        }
    }
    let later = later(sessions);
    let all: Vec<&Sample> = sessions.iter().flatten().collect();
    let prompt: u64 = later.iter().map(|s| s.prompt_tokens).sum();
    let cached: u64 = later.iter().map(|s| s.cached_tokens).sum();
    let first: Vec<f64> = later
        .iter()
        .filter_map(|s| s.first_token.map(seconds))
        .collect();
    println!(
        "  {} request(s) in {} session(s); after each session's first: \
         {cached} of {prompt} prompt tokens from the cache ({:.1}%), \
         first token after {:.2}s (median) / {:.2}s (mean), {:.2}s in all; \
         {:.0}s spent in requests, {} completion tokens",
        all.len(),
        sessions.len(),
        cached as f64 * 100.0 / prompt.max(1) as f64,
        median(first.clone()),
        first.iter().sum::<f64>() / first.len().max(1) as f64,
        first.iter().sum::<f64>(),
        all.iter().map(|s| seconds(s.total)).sum::<f64>(),
        all.iter().map(|s| s.completion_tokens).sum::<u64>(),
    );
}

#[test]
#[ignore = "needs the model endpoint; runs a few short sessions on it"]
fn the_stable_prefix_is_served_from_the_cache() {
    let base_url = std::env::var("ATELIER_BASE_URL")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    let model = std::env::var("ATELIER_MODEL")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let rounds: usize = std::env::var("ATELIER_CACHE_SESSIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(2);
    let upstream = Upstream::parse(&base_url);
    let reachable = upstream
        .authority
        .parse()
        .ok()
        .and_then(|addr| TcpStream::connect_timeout(&addr, Duration::from_secs(5)).ok());
    assert!(
        reachable.is_some() || TcpStream::connect(&upstream.authority).is_ok(),
        "the endpoint at {base_url} is out of reach: nothing was measured"
    );
    println!("endpoint {base_url}, model {model}, {rounds} session(s) per arm");

    const ARMS: [Arm; 3] = [Arm::Stable, Arm::System, Arm::Everything];
    let mut arms: [Vec<Vec<Sample>>; 3] = Default::default();
    for round in 0..rounds {
        // In turn, so that a server that is busy or warming up for a while
        // weighs on every arm.
        for (arm, sessions) in ARMS.into_iter().zip(&mut arms) {
            sessions.push(session(&upstream, &model, arm, &format!("{round}")));
        }
    }
    for (arm, sessions) in ARMS.into_iter().zip(&arms) {
        report(arm, sessions);
    }
    let [stable, _, control] = &arms;

    for samples in arms.iter().flatten() {
        assert!(samples.len() >= PROMPTS.len(), "a request per prompt");
        assert!(
            samples.iter().all(|s| s.status == 200),
            "a request was refused: {:?}",
            samples.iter().map(|s| s.status).collect::<Vec<_>>()
        );
        // What atelier sent is the same in every arm: a control's tag
        // goes in after the recorder has kept the body.
        assert_prefixes(samples);
    }
    // Where the server says what it took from its cache, the stable arm
    // must be served from it. (A server that reports nothing is only timed.)
    let cached = |arm: &[Vec<Sample>]| -> (u64, u64) {
        later(arm).iter().fold((0, 0), |(c, p), s| {
            (c + s.cached_tokens, p + s.prompt_tokens)
        })
    };
    let (hit, prompt) = cached(stable);
    if hit > 0 {
        assert!(
            hit * 2 > prompt,
            "less than half the stable arm's prompt came from the cache: {hit} of {prompt}"
        );
        let (control_hit, _) = cached(control);
        assert!(
            control_hit < hit,
            "the control was cached as well as the stable arm: {control_hit} vs {hit}"
        );
    }
}
