//! OpenAI-compatible model provider.
//!
//! Streaming `chat/completions` over Server-Sent Events, with three things the
//! rest of the harness relies on:
//!
//! - the model's reasoning ("thinking") separated from the final answer,
//! - **tool advertisement** ([`ToolSpec`]) in the request, and
//! - **streamed tool-call assembly** — `tool_calls` arrive fragmented across
//!   SSE chunks (id/name once, arguments in pieces) and are reassembled here.
//!
//! Transport is [`rsurl`]: `Request::send_reader()` returns a blocking
//! `Read` + `.status()`, which we drive line-by-line as an SSE stream.
//!
//! Not yet: a non-streaming fallback.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::Config;

/// Env var overriding both HTTP timeouts below (milliseconds). Read directly
/// here rather than through `Config` since the provider is the only consumer.
const TIMEOUT_ENV_VAR: &str = "ATELIER_HTTP_TIMEOUT_MS";
/// Env var naming a file to append one JSONL trace record to per
/// [`stream_chat`] call (request + final response). Unset by default (no
/// tracing). Best-effort: a write failure never fails the request. Never
/// includes the `Authorization` header/api key.
const TRACE_ENV_VAR: &str = "ATELIER_TRACE";
/// Default connect timeout for the streaming chat call: generous, since a
/// local/self-hosted model server can be slow to accept a connection under
/// load.
const DEFAULT_CHAT_TIMEOUT_MS: u64 = 60_000;
/// Default connect timeout for `GET /models`: a cheap, quick call, so a
/// shorter default fails fast.
const DEFAULT_LIST_MODELS_TIMEOUT_MS: u64 = 15_000;
/// Appended to `Completion::content` when the response was cut off by the
/// model's output token limit (`finish_reason == "length"`, no tool calls) —
/// see [`should_append_truncation_note`].
const TRUNCATION_NOTE: &str = "\n\n[response truncated: reached the model's output token limit]";

/// Resolve the connect-timeout duration to use, honoring
/// `ATELIER_HTTP_TIMEOUT_MS` (milliseconds) when set to a valid, positive
/// integer, else falling back to `default_ms`.
fn timeout_ms_from_env(default_ms: u64) -> u64 {
    std::env::var(TIMEOUT_ENV_VAR)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .unwrap_or(default_ms)
}

fn connect_timeout(default_ms: u64) -> Duration {
    Duration::from_millis(timeout_ms_from_env(default_ms))
}

/// A chat message. Covers plain text, an assistant turn carrying tool calls,
/// and a tool result (`role = "tool"`). Serializable for session persistence
/// (this is atelier's own on-disk shape, distinct from the OpenAI wire form
/// produced by [`Message::to_wire`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
    /// Present on an assistant turn that requested tools.
    pub tool_calls: Vec<ToolCall>,
    /// Present on a `role = "tool"` result, linking it to its call.
    pub tool_call_id: Option<String>,
    /// Image references (a `data:` URL or an http(s) URL) attached to this
    /// message, passed straight through as `image_url.url` in the wire form.
    /// `#[serde(default)]` so session files written before this field existed
    /// still deserialize.
    #[serde(default)]
    pub images: Vec<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self::text("system", content)
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self::text("user", content)
    }
    #[allow(dead_code)] // plain assistant turns (no tool calls) are used by tests/tui
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::text("assistant", content)
    }
    fn text(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            images: Vec::new(),
        }
    }
    /// An assistant turn that requested one or more tool calls.
    pub fn assistant_tool_calls(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
            tool_calls,
            tool_call_id: None,
            images: Vec::new(),
        }
    }
    /// The result of executing a tool call, fed back to the model.
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
            images: Vec::new(),
        }
    }
    /// A user turn with one or more images attached alongside text (OpenAI
    /// multimodal `content` array). `images` entries are passed straight
    /// through as `image_url.url` — a `data:` URL or an http(s) URL.
    #[allow(dead_code)] // not yet wired into agent/tui callers; exercised by tests
    pub fn user_with_images(content: impl Into<String>, images: Vec<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            images,
        }
    }

    /// Render to the OpenAI wire shape.
    fn to_wire(&self) -> Value {
        if self.role == "tool" {
            return json!({
                "role": "tool",
                "tool_call_id": self.tool_call_id,
                "content": self.content,
            });
        }
        if !self.tool_calls.is_empty() {
            return json!({
                "role": self.role,
                "content": self.content,
                "tool_calls": self.tool_calls.iter().map(ToolCall::to_wire).collect::<Vec<_>>(),
            });
        }
        if !self.images.is_empty() {
            let mut parts = Vec::with_capacity(1 + self.images.len());
            if !self.content.is_empty() {
                parts.push(json!({ "type": "text", "text": self.content }));
            }
            for img in &self.images {
                parts.push(json!({ "type": "image_url", "image_url": { "url": img } }));
            }
            return json!({ "role": self.role, "content": parts });
        }
        json!({ "role": self.role, "content": self.content })
    }
}

/// A tool the model may call, advertised to the endpoint. `parameters` is a
/// JSON Schema object describing the arguments.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl ToolSpec {
    fn to_wire(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            }
        })
    }
}

/// A fully-assembled tool call requested by the model. `arguments` is a raw
/// JSON string (OpenAI convention), parsed by the caller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl ToolCall {
    fn to_wire(&self) -> Value {
        json!({
            "id": self.id,
            "type": "function",
            "function": { "name": self.name, "arguments": self.arguments }
        })
    }
}

/// The outcome of a streamed completion.
#[derive(Debug, Clone, Default)]
pub struct Completion {
    /// Accumulated answer text (reasoning excluded). When the response was
    /// cut off by the model's output token limit (`finish_reason ==
    /// "length"`, no tool calls), a truncation note is appended — see
    /// [`TRUNCATION_NOTE`].
    pub content: String,
    /// Tool calls the model requested, if any.
    pub tool_calls: Vec<ToolCall>,
    /// Token usage reported by the provider, if it honored
    /// `stream_options.include_usage`. Some servers omit this.
    pub usage: Option<Usage>,
    /// The last non-null `finish_reason` seen across streamed chunks (e.g.
    /// `"stop"`, `"tool_calls"`, `"length"`, `"content_filter"`). `None` if
    /// the provider never sent one.
    pub finish_reason: Option<String>,
}

/// Token usage for a completion, as reported on the final SSE chunk when the
/// request set `stream_options.include_usage`. Consumed by the caller.
#[allow(dead_code)]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
}

/// An incremental piece of a streamed response.
#[derive(Debug, Clone)]
pub enum StreamEvent<'a> {
    /// A chunk of the model's private reasoning. Displayed separately and
    /// **never** fed back to the model as assistant output.
    Reasoning(&'a str),
    /// A chunk of the final answer.
    Content(&'a str),
}

/// Stream a chat completion, invoking `on_event` for each text delta as it
/// arrives. Advertises `tools` to the model and reassembles any tool calls.
///
/// Returns the accumulated [`Completion`] once the stream ends.
pub fn stream_chat(
    cfg: &Config,
    messages: &[Message],
    tools: &[ToolSpec],
    mut on_event: impl FnMut(StreamEvent),
) -> Result<Completion> {
    let mut body = json!({
        "model": cfg.model,
        "messages": messages.iter().map(Message::to_wire).collect::<Vec<_>>(),
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools.iter().map(ToolSpec::to_wire).collect());
    }
    let body_bytes = serde_json::to_vec(&body)?;
    if std::env::var_os("ATELIER_DEBUG").is_some() {
        eprintln!("--> {}", String::from_utf8_lossy(&body_bytes));
    }

    let url = cfg.endpoint("chat/completions");
    let reader = send_reader_retrying(&url, &body_bytes, cfg.api_key.as_deref())?;
    let status = reader.status();
    if !(200..300).contains(&status) {
        bail!("provider returned HTTP {status}");
    }

    let mut out = Completion::default();
    // Tool-call fragments, keyed by their streamed `index`.
    let mut calls: BTreeMap<usize, ToolCallAccum> = BTreeMap::new();
    // The last non-null `finish_reason` seen across chunks.
    let mut finish_reason: Option<String> = None;

    let mut lines = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        if lines.read_line(&mut line).context("reading stream")? == 0 {
            break; // EOF
        }
        // SSE: only `data:` lines matter; blank lines separate events.
        let Some(data) = line.trim_end().strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() {
            continue;
        }
        if data == "[DONE]" {
            // Drain any trailing bytes so the connection is left clean rather
            // than half-read (a half-read socket breaks the next request).
            let _ = std::io::copy(&mut lines, &mut std::io::sink());
            break;
        }
        // Be lenient: skip keep-alives / frames we can't parse.
        let Ok(chunk) = serde_json::from_str::<ChatChunk>(data) else {
            continue;
        };
        // The final chunk (when `stream_options.include_usage` is honored)
        // carries `usage` and typically an empty `choices` array, so read it
        // before falling through the empty-choices early-exit below.
        if let Some(usage) = chunk.usage {
            out.usage = Some(usage);
        }
        let Some(choice) = chunk.choices.into_iter().next() else {
            continue;
        };
        let delta = choice.delta;
        if let Some(fr) = choice.finish_reason {
            finish_reason = Some(fr);
        }

        if let Some(r) = delta.reasoning()
            && !r.is_empty()
        {
            on_event(StreamEvent::Reasoning(r));
        }
        if let Some(c) = &delta.content
            && !c.is_empty()
        {
            out.content.push_str(c);
            on_event(StreamEvent::Content(c));
        }
        for tc in delta.tool_calls {
            let slot = calls.entry(tc.index).or_default();
            if let Some(id) = tc.id {
                slot.id = id;
            }
            if let Some(f) = tc.function {
                if let Some(name) = f.name {
                    slot.name.push_str(&name);
                }
                if let Some(args) = f.arguments {
                    slot.arguments.push_str(&args);
                }
            }
        }
    }

    out.tool_calls = calls
        .into_values()
        .map(|a| ToolCall {
            id: a.id,
            name: a.name,
            arguments: a.arguments,
        })
        .collect();
    out.finish_reason = finish_reason;
    if should_append_truncation_note(&out.finish_reason, &out.tool_calls) {
        out.content.push_str(TRUNCATION_NOTE);
    }

    if let Ok(path) = std::env::var(TRACE_ENV_VAR) {
        let tool_names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        let entry = build_trace_entry(&cfg.model, &body, &tool_names, &out);
        trace(&path, &entry);
    }

    Ok(out)
}

/// Whether [`stream_chat`] should append [`TRUNCATION_NOTE`] to the
/// accumulated content: only when the provider's last `finish_reason` was
/// `"length"` (the response was cut off by the output token limit) and no
/// tool calls were requested. A `"length"` finish alongside tool calls
/// usually means the call's *arguments* were truncated mid-stream rather than
/// the answer text, so it's left untouched here — `"stop"`, `"tool_calls"`,
/// `"content_filter"`, and `None` are all left untouched too.
fn should_append_truncation_note(finish_reason: &Option<String>, tool_calls: &[ToolCall]) -> bool {
    finish_reason.as_deref() == Some("length") && tool_calls.is_empty()
}

/// Build one JSONL trace record for a [`stream_chat`] call: the request
/// `messages` (taken from the already-built request body, so this observes
/// rather than reconstructs the wire form), the names of tools advertised,
/// and the resulting [`Completion`] (content, tool calls, usage). Pure and
/// deterministic aside from the timestamp, so it's unit-testable without
/// touching the filesystem — [`trace`] is the only side-effecting part.
///
/// Deliberately omits headers/credentials: the request body never contains
/// the `Authorization` header or api key, so nothing needs to be redacted
/// from it here.
fn build_trace_entry(
    model: &str,
    request: &Value,
    tool_names: &[&str],
    completion: &Completion,
) -> Value {
    let timestamp_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    json!({
        "timestamp_ms": timestamp_ms,
        "model": model,
        "request": {
            "messages": request.get("messages").cloned().unwrap_or(Value::Null),
        },
        "tools": tool_names,
        "response": {
            "content": completion.content,
            "tool_calls": completion.tool_calls.iter().map(|tc| json!({
                "name": tc.name,
                "arguments": tc.arguments,
            })).collect::<Vec<_>>(),
            "usage": completion.usage,
        },
    })
}

/// Append `entry` as one JSON line to the file at `path`, creating it (and
/// appending, not truncating, if it already exists) as needed. Best-effort:
/// any failure — bad path, missing parent dir, permissions, serialization —
/// is silently ignored so tracing can never fail a request.
fn trace(path: &str, entry: &Value) {
    use std::io::Write as _;
    let Ok(mut line) = serde_json::to_string(entry) else {
        return;
    };
    line.push('\n');
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

#[derive(Default)]
struct ToolCallAccum {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Deserialize)]
struct ChatChunk {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Delta,
    /// Set on the chunk carrying the last delta for this choice: `"stop"`,
    /// `"tool_calls"`, `"length"` (hit the output token limit),
    /// `"content_filter"`, etc. `None` on every chunk before that.
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Vec<DeltaToolCall>,
}

impl Delta {
    fn reasoning(&self) -> Option<&str> {
        self.reasoning_content
            .as_deref()
            .or(self.reasoning.as_deref())
    }
}

#[derive(Deserialize)]
struct DeltaToolCall {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<DeltaFunction>,
}

#[derive(Deserialize)]
struct DeltaFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// Maximum number of send attempts (the original attempt plus retries) before
/// a transient transport error is given up on.
const MAX_SEND_ATTEMPTS: u32 = 5;

/// Classifies a transport-layer error message as transient (worth rebuilding
/// the request and retrying) versus a hard failure worth surfacing
/// immediately.
///
/// This only ever sees errors from `Request::send_reader()` itself — i.e.
/// failures to establish/send the request (DNS, connect, TLS handshake, write
/// timeout, a stale pooled connection resurfacing as EAGAIN). An HTTP 4xx/5xx
/// response is *not* an `Err` here: `send_reader()` returns `Ok` with a status
/// to check once headers arrive, so a real HTTP error status never reaches
/// (and is never accidentally retried by) this classifier.
fn is_transient_error(msg: &str) -> bool {
    let msg = msg.to_ascii_lowercase();
    const TRANSIENT_PATTERNS: &[&str] = &[
        "temporarily unavailable", // EAGAIN, spelled out
        "os error 35",             // EAGAIN (macOS/BSD)
        "os error 11",             // EAGAIN (Linux); also matches "os error 111"
        "connection refused",
        "os error 61",  // ECONNREFUSED (macOS/BSD)
        "os error 111", // ECONNREFUSED (Linux)
        "connection reset",
        "os error 54",  // ECONNRESET (macOS/BSD)
        "os error 104", // ECONNRESET (Linux)
        "broken pipe",
        "timed out",
        "timeout",
    ];
    TRANSIENT_PATTERNS.iter().any(|p| msg.contains(p))
}

/// Classifies an HTTP response status as a transient server-side failure
/// worth retrying — 5xx generally (502/503/504 explicitly, but any 500-599
/// counts, matching the sporadic bare 500s observed from the local server) —
/// versus a client error (4xx) or success (2xx/3xx) that must be surfaced to
/// the caller as-is rather than retried.
fn is_retryable_status(status: u16) -> bool {
    (500..600).contains(&status)
}

/// Small capped exponential backoff between retry attempts: 50ms, 100ms,
/// 200ms, 400ms, capped at 800ms so a run of `MAX_SEND_ATTEMPTS` retries never
/// stalls the harness for long.
fn backoff_delay(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(4);
    let ms = 50u64.saturating_mul(1u64 << shift);
    Duration::from_millis(ms.min(800))
}

/// POST `body` and return the streaming body reader, rebuilding the request
/// and retrying a bounded number of times on either:
///
/// - a transient connect/send failure (see [`is_transient_error`]) — e.g. the
///   EAGAIN ("Resource temporarily unavailable") that rsurl can surface on a
///   sequential in-process request (the follow-up call after a tool result),
///   or a connection refused/reset while a local model server is warming up;
///   or
/// - a response that came back with a transient server status (see
///   [`is_retryable_status`]) — e.g. a sporadic bare 500 from a local model
///   server. In this case `send_reader()` itself succeeded (headers arrived),
///   so the reader is dropped and the request is rebuilt and resent rather
///   than being handed to the caller, which would just `bail!` on the status.
///
/// Only once attempts are exhausted is the last reader/error handed back to
/// the caller, which reports the failure (a non-2xx status via `bail!`, or
/// the transport error).
///
/// The connect timeout is configurable via `ATELIER_HTTP_TIMEOUT_MS`
/// (milliseconds), defaulting to [`DEFAULT_CHAT_TIMEOUT_MS`].
fn send_reader_retrying(
    url: &str,
    body: &[u8],
    api_key: Option<&str>,
) -> Result<rsurl::BodyReader> {
    let timeout = connect_timeout(DEFAULT_CHAT_TIMEOUT_MS);
    let mut attempt: u32 = 0;
    loop {
        let mut req = rsurl::Request::new("POST", url)
            .context("building request")?
            .connect_timeout(timeout)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            // Fresh connection per request: reusing a pooled keep-alive
            // connection here can surface a spurious EAGAIN on the next send.
            .header("connection", "close")
            .body(body.to_vec());
        if let Some(key) = api_key {
            req = req.header("authorization", &format!("Bearer {key}"));
        }
        match req.send_reader() {
            Ok(reader) => {
                if is_retryable_status(reader.status()) {
                    attempt += 1;
                    if attempt < MAX_SEND_ATTEMPTS {
                        std::thread::sleep(backoff_delay(attempt));
                        continue;
                    }
                }
                return Ok(reader);
            }
            Err(e) => {
                attempt += 1;
                let transient = is_transient_error(&e.to_string());
                if transient && attempt < MAX_SEND_ATTEMPTS {
                    std::thread::sleep(backoff_delay(attempt));
                    continue;
                }
                return Err(anyhow::anyhow!("sending request: {e}"));
            }
        }
    }
}

/// List model ids advertised by the endpoint (`GET /models`).
///
/// Like [`send_reader_retrying`], a response with a transient server status
/// (see [`is_retryable_status`]) is retried (rebuilding the request, capped
/// at [`MAX_SEND_ATTEMPTS`]) rather than immediately surfaced — this is a
/// cheap, idempotent GET, so retrying it is straightforward.
///
/// The connect timeout is configurable via `ATELIER_HTTP_TIMEOUT_MS`
/// (milliseconds), defaulting to [`DEFAULT_LIST_MODELS_TIMEOUT_MS`].
pub fn list_models(cfg: &Config) -> Result<Vec<String>> {
    let url = cfg.endpoint("models");
    let timeout = connect_timeout(DEFAULT_LIST_MODELS_TIMEOUT_MS);
    let mut attempt: u32 = 0;
    let resp = loop {
        let resp = rsurl::Request::new("GET", &url)
            .context("building request")?
            .connect_timeout(timeout)
            .send()
            .context("sending request")?;
        if is_retryable_status(resp.status) {
            attempt += 1;
            if attempt < MAX_SEND_ATTEMPTS {
                std::thread::sleep(backoff_delay(attempt));
                continue;
            }
        }
        break resp;
    };
    if !(200..300).contains(&resp.status) {
        bail!("provider returned HTTP {}", resp.status);
    }
    let list: ModelList = serde_json::from_slice(&resp.body).context("parsing models")?;
    Ok(list.data.into_iter().map(|m| m.id).collect())
}

#[derive(Deserialize)]
struct ModelList {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A final usage-only chunk (empty `choices`) must still yield a parsed
    /// `Usage`, matching what a real `stream_options.include_usage` response
    /// sends as its last SSE frame.
    #[test]
    fn parses_usage_only_chunk() {
        let data = r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#;
        let chunk: ChatChunk = serde_json::from_str(data).expect("valid chunk");
        assert!(chunk.choices.is_empty());
        let usage = chunk.usage.expect("usage present");
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.total_tokens, 15);
    }

    /// A normal content chunk has no `usage` field at all; it must parse
    /// without one rather than erroring.
    #[test]
    fn chunk_without_usage_parses_as_none() {
        let data = r#"{"choices":[{"delta":{"content":"hi"}}]}"#;
        let chunk: ChatChunk = serde_json::from_str(data).expect("valid chunk");
        assert!(chunk.usage.is_none());
        assert_eq!(chunk.choices.len(), 1);
    }

    /// A chunk carrying `finish_reason: "length"` on its choice (what a
    /// server sends on the final chunk when the response was cut off by the
    /// output token limit) must parse, with the field surfaced rather than
    /// dropped.
    #[test]
    fn parses_finish_reason_length() {
        let data = r#"{"choices":[{"delta":{},"finish_reason":"length"}],"usage":null}"#;
        let chunk: ChatChunk = serde_json::from_str(data).expect("valid chunk");
        assert_eq!(chunk.choices.len(), 1);
        assert_eq!(chunk.choices[0].finish_reason.as_deref(), Some("length"));
    }

    /// A normal mid-stream content chunk has no `finish_reason` at all; it
    /// must parse with the field defaulting to `None` rather than erroring.
    #[test]
    fn chunk_without_finish_reason_parses_as_none() {
        let data = r#"{"choices":[{"delta":{"content":"hi"}}]}"#;
        let chunk: ChatChunk = serde_json::from_str(data).expect("valid chunk");
        assert_eq!(chunk.choices[0].finish_reason, None);
    }

    /// `should_append_truncation_note` is true only for `"length"` with no
    /// tool calls; `"stop"`, `"tool_calls"`, `"length"` alongside tool calls,
    /// and a missing finish reason must all leave content untouched.
    #[test]
    fn truncation_note_only_for_length_without_tool_calls() {
        let no_calls: Vec<ToolCall> = Vec::new();
        let with_call = vec![ToolCall {
            id: "call_1".to_string(),
            name: "read_file".to_string(),
            arguments: "{}".to_string(),
        }];

        assert!(should_append_truncation_note(
            &Some("length".to_string()),
            &no_calls
        ));
        assert!(!should_append_truncation_note(
            &Some("length".to_string()),
            &with_call
        ));
        assert!(!should_append_truncation_note(
            &Some("stop".to_string()),
            &no_calls
        ));
        assert!(!should_append_truncation_note(
            &Some("tool_calls".to_string()),
            &with_call
        ));
        assert!(!should_append_truncation_note(&None, &no_calls));
    }

    /// Transport-level connect/send failures that are worth retrying.
    #[test]
    fn transient_errors_are_recognized() {
        assert!(is_transient_error(
            "Resource temporarily unavailable (os error 35)"
        ));
        assert!(is_transient_error("temporarily unavailable"));
        assert!(is_transient_error("Connection refused (os error 61)"));
        assert!(is_transient_error("connection reset by peer"));
        assert!(is_transient_error("os error 104"));
        assert!(is_transient_error("connect timed out"));
        assert!(is_transient_error("operation timeout"));
        // Case-insensitivity.
        assert!(is_transient_error("CONNECTION REFUSED"));
    }

    /// Real HTTP/application errors are not transport transients and must
    /// never be retried.
    #[test]
    fn non_transient_errors_are_not_retried() {
        assert!(!is_transient_error("provider returned HTTP 404"));
        assert!(!is_transient_error("provider returned HTTP 500"));
        assert!(!is_transient_error("invalid json"));
        assert!(!is_transient_error("parsing models"));
    }

    /// 5xx statuses (the ones we've observed sporadically from a local
    /// server) must be classified as retryable.
    #[test]
    fn retryable_statuses_include_5xx() {
        assert!(is_retryable_status(500));
        assert!(is_retryable_status(502));
        assert!(is_retryable_status(503));
        assert!(is_retryable_status(504));
    }

    /// 4xx (client error), 2xx (success), and 3xx (redirect) statuses must
    /// never be retried.
    #[test]
    fn non_retryable_statuses_are_rejected() {
        assert!(!is_retryable_status(400));
        assert!(!is_retryable_status(401));
        assert!(!is_retryable_status(404));
        assert!(!is_retryable_status(200));
        assert!(!is_retryable_status(301));
    }

    /// Backoff is strictly increasing across the retry window and stays
    /// bounded, so a run of `MAX_SEND_ATTEMPTS` retries can't stall the
    /// harness for long.
    #[test]
    fn backoff_delay_is_bounded_and_increasing() {
        let delays: Vec<_> = (1..MAX_SEND_ATTEMPTS).map(backoff_delay).collect();
        for window in delays.windows(2) {
            assert!(window[0] < window[1], "backoff must increase per attempt");
        }
        for d in &delays {
            assert!(*d <= Duration::from_millis(800), "backoff must stay capped");
        }
    }

    /// A text-only user message must keep serializing `content` as a plain
    /// string (regression: multimodal support must not affect this path).
    #[test]
    fn to_wire_text_only_user_message_has_string_content() {
        let msg = Message::user("hello there");
        let wire = msg.to_wire();
        assert_eq!(wire["role"], "user");
        assert_eq!(wire["content"], json!("hello there"));
        assert!(wire.get("tool_calls").is_none());
    }

    /// A message built with `user_with_images` must serialize `content` as an
    /// array: text part first, then one `image_url` part per image, with the
    /// url passed through exactly.
    #[test]
    fn to_wire_user_with_images_has_array_content() {
        let data_url = "data:image/png;base64,AAAA";
        let msg = Message::user_with_images("hi", vec![data_url.to_string()]);
        let wire = msg.to_wire();
        assert_eq!(wire["role"], "user");
        let content = wire["content"].as_array().expect("array content");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0], json!({ "type": "text", "text": "hi" }));
        assert_eq!(
            content[1],
            json!({ "type": "image_url", "image_url": { "url": data_url } })
        );
    }

    /// When images are present but the text content is empty, the text part
    /// must be omitted entirely rather than emitted as an empty string.
    #[test]
    fn to_wire_images_with_empty_text_omits_text_part() {
        let data_url = "data:image/png;base64,BBBB";
        let msg = Message::user_with_images("", vec![data_url.to_string()]);
        let wire = msg.to_wire();
        let content = wire["content"].as_array().expect("array content");
        assert_eq!(content.len(), 1);
        assert_eq!(
            content[0],
            json!({ "type": "image_url", "image_url": { "url": data_url } })
        );
    }

    /// A `Message` JSON blob written before the `images` field existed (i.e.
    /// missing the key entirely) must still deserialize, defaulting `images`
    /// to empty — this is the on-disk session-file compatibility guarantee.
    #[test]
    fn message_deserializes_without_images_field() {
        let data = r#"{"role":"user","content":"hi","tool_calls":[],"tool_call_id":null}"#;
        let msg: Message = serde_json::from_str(data).expect("deserializes");
        assert!(msg.images.is_empty());
        assert_eq!(msg.content, "hi");
    }

    /// Covers unset/valid/invalid/zero cases for the timeout env var in one
    /// test, since `std::env::set_var` is process-wide and `cargo test` runs
    /// tests concurrently by default — splitting these across tests sharing
    /// `TIMEOUT_ENV_VAR` would race.
    #[test]
    fn timeout_env_var_parsing() {
        // SAFETY: test-only manipulation of a var no other test reads;
        // cleared at the end of this single test so it can't leak state to
        // (or race with) any other test.
        unsafe {
            std::env::remove_var(TIMEOUT_ENV_VAR);
        }
        assert_eq!(timeout_ms_from_env(60_000), 60_000);
        assert_eq!(timeout_ms_from_env(15_000), 15_000);

        unsafe {
            std::env::set_var(TIMEOUT_ENV_VAR, "2500");
        }
        assert_eq!(timeout_ms_from_env(60_000), 2500);

        unsafe {
            std::env::set_var(TIMEOUT_ENV_VAR, "not-a-number");
        }
        assert_eq!(timeout_ms_from_env(60_000), 60_000);

        unsafe {
            std::env::set_var(TIMEOUT_ENV_VAR, "0");
        }
        assert_eq!(timeout_ms_from_env(60_000), 60_000);

        unsafe {
            std::env::remove_var(TIMEOUT_ENV_VAR);
        }
    }

    /// `build_trace_entry` must produce a valid JSON object carrying the
    /// model, the request `messages` (pulled from the already-built request
    /// body), the advertised tool names, and the completion's content, tool
    /// calls (name + arguments), and usage.
    #[test]
    fn build_trace_entry_has_expected_fields() {
        let request = json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
        });
        let completion = Completion {
            content: "hello there".to_string(),
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                name: "read_file".to_string(),
                arguments: r#"{"path":"a.txt"}"#.to_string(),
            }],
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
            }),
            finish_reason: Some("tool_calls".to_string()),
        };
        let tool_names = ["read_file", "write_file"];
        let entry = build_trace_entry("test-model", &request, &tool_names, &completion);

        // Round-trips through a string, confirming it's valid JSON.
        let line = serde_json::to_string(&entry).expect("serializes");
        let reparsed: Value = serde_json::from_str(&line).expect("valid json");

        assert!(reparsed["timestamp_ms"].as_u64().is_some());
        assert_eq!(reparsed["model"], json!("test-model"));
        assert_eq!(
            reparsed["request"]["messages"],
            json!([{"role": "user", "content": "hi"}])
        );
        assert_eq!(reparsed["tools"], json!(["read_file", "write_file"]));
        assert_eq!(reparsed["response"]["content"], json!("hello there"));
        assert_eq!(
            reparsed["response"]["tool_calls"],
            json!([{"name": "read_file", "arguments": r#"{"path":"a.txt"}"#}])
        );
        assert_eq!(
            reparsed["response"]["usage"],
            json!({"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15})
        );

        // Redaction: nothing resembling an api key/auth header ever appears.
        assert!(!line.to_ascii_lowercase().contains("authorization"));
    }

    /// A request with no `messages` key (shouldn't happen in practice, but
    /// `build_trace_entry` must degrade to `null` rather than panicking).
    #[test]
    fn build_trace_entry_handles_missing_messages() {
        let request = json!({ "model": "test-model" });
        let completion = Completion::default();
        let entry = build_trace_entry("test-model", &request, &[], &completion);
        assert_eq!(entry["request"]["messages"], Value::Null);
        assert_eq!(entry["response"]["usage"], Value::Null);
        assert_eq!(entry["response"]["tool_calls"], json!([]));
    }

    /// `trace` is best-effort: writing to an unwritable path (a directory
    /// nested under a nonexistent parent) must not panic.
    #[test]
    fn trace_ignores_unwritable_path() {
        let entry = json!({"ok": true});
        trace("/nonexistent-dir-for-atelier-trace-test/x/y.jsonl", &entry);
        // No panic is the assertion; nothing else to check.
    }

    /// `trace` appends one JSON line per call to a real file, and each line
    /// parses back as the entry that was written.
    #[test]
    fn trace_appends_jsonl_lines_to_file() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "atelier-trace-test-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path_str = path.to_str().expect("utf8 path").to_string();

        trace(&path_str, &json!({"n": 1}));
        trace(&path_str, &json!({"n": 2}));

        let contents = std::fs::read_to_string(&path).expect("file written");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "expected two appended lines, got: {contents:?}"
        );
        assert_eq!(
            serde_json::from_str::<Value>(lines[0]).unwrap(),
            json!({"n": 1})
        );
        assert_eq!(
            serde_json::from_str::<Value>(lines[1]).unwrap(),
            json!({"n": 2})
        );

        let _ = std::fs::remove_file(&path);
    }
}
