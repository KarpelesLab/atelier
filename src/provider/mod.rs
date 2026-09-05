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
//! Not yet: multimodal content parts (vision) and a non-streaming fallback.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::Config;

/// Env var overriding both HTTP timeouts below (milliseconds). Read directly
/// here rather than through `Config` since the provider is the only consumer.
const TIMEOUT_ENV_VAR: &str = "ATELIER_HTTP_TIMEOUT_MS";
/// Default connect timeout for the streaming chat call: generous, since a
/// local/self-hosted model server can be slow to accept a connection under
/// load.
const DEFAULT_CHAT_TIMEOUT_MS: u64 = 60_000;
/// Default connect timeout for `GET /models`: a cheap, quick call, so a
/// shorter default fails fast.
const DEFAULT_LIST_MODELS_TIMEOUT_MS: u64 = 15_000;

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
/// and a tool result (`role = "tool"`).
#[derive(Debug, Clone)]
pub struct Message {
    pub role: String,
    pub content: String,
    /// Present on an assistant turn that requested tools.
    pub tool_calls: Vec<ToolCall>,
    /// Present on a `role = "tool"` result, linking it to its call.
    pub tool_call_id: Option<String>,
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
        }
    }
    /// An assistant turn that requested one or more tool calls.
    pub fn assistant_tool_calls(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
            tool_calls,
            tool_call_id: None,
        }
    }
    /// The result of executing a tool call, fed back to the model.
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
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
#[derive(Debug, Clone)]
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
    /// Accumulated answer text (reasoning excluded).
    pub content: String,
    /// Tool calls the model requested, if any.
    pub tool_calls: Vec<ToolCall>,
    /// Token usage reported by the provider, if it honored
    /// `stream_options.include_usage`. Some servers omit this.
    pub usage: Option<Usage>,
}

/// Token usage for a completion, as reported on the final SSE chunk when the
/// request set `stream_options.include_usage`. Consumed by the caller.
#[allow(dead_code)]
#[derive(Debug, Clone, Default, Deserialize)]
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
    let body = serde_json::to_vec(&body)?;
    if std::env::var_os("ATELIER_DEBUG").is_some() {
        eprintln!("--> {}", String::from_utf8_lossy(&body));
    }

    let url = cfg.endpoint("chat/completions");
    let reader = send_reader_retrying(&url, &body, cfg.api_key.as_deref())?;
    let status = reader.status();
    if !(200..300).contains(&status) {
        bail!("provider returned HTTP {status}");
    }

    let mut out = Completion::default();
    // Tool-call fragments, keyed by their streamed `index`.
    let mut calls: BTreeMap<usize, ToolCallAccum> = BTreeMap::new();

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
    Ok(out)
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

/// Small capped exponential backoff between retry attempts: 50ms, 100ms,
/// 200ms, 400ms, capped at 800ms so a run of `MAX_SEND_ATTEMPTS` retries never
/// stalls the harness for long.
fn backoff_delay(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(4);
    let ms = 50u64.saturating_mul(1u64 << shift);
    Duration::from_millis(ms.min(800))
}

/// POST `body` and return the streaming body reader, rebuilding the request
/// and retrying a bounded number of times on a transient connect/send
/// failure (see [`is_transient_error`]) — e.g. the EAGAIN ("Resource
/// temporarily unavailable") that rsurl can surface on a sequential
/// in-process request (the follow-up call after a tool result), or a
/// connection refused/reset while a local model server is warming up.
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
            Ok(reader) => return Ok(reader),
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
/// The connect timeout is configurable via `ATELIER_HTTP_TIMEOUT_MS`
/// (milliseconds), defaulting to [`DEFAULT_LIST_MODELS_TIMEOUT_MS`].
pub fn list_models(cfg: &Config) -> Result<Vec<String>> {
    let resp = rsurl::Request::new("GET", &cfg.endpoint("models"))
        .context("building request")?
        .connect_timeout(connect_timeout(DEFAULT_LIST_MODELS_TIMEOUT_MS))
        .send()
        .context("sending request")?;
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
}
