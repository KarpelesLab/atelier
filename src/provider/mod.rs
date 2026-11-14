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
//! Where a request goes — endpoint, key, extra headers, the model id and the
//! request options of its variant — is a [`Target`] (see [`endpoint`]); the
//! options are merged into the request *body*, never the messages, so the
//! prompt a server caches doesn't depend on them.
//!
//! Not yet: a non-streaming fallback.

pub mod endpoint;
pub mod responses;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::Config;
pub use endpoint::{LOCAL_PROVIDER, Provider, Target};

/// Env var overriding every HTTP connect timeout (milliseconds), a
/// provider's own `timeout_secs` included. Read directly here rather than
/// through `Config` since the provider is the only consumer.
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
/// The shortest silence a streaming response is given up after, whatever the
/// connect timeout: a provider set to connect quickly may still think long.
const MIN_READ_TIMEOUT: Duration = Duration::from_secs(60);
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

/// The connect timeout for a request to `provider`: the env var, else the
/// provider's `timeout_secs`, else `default_ms`.
fn connect_timeout(provider: &Provider, default_ms: u64) -> Duration {
    Duration::from_millis(timeout_ms_from_env(
        provider.timeout_ms.unwrap_or(default_ms),
    ))
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
    /// Project context gathered for this user turn (git status, diagnostics,
    /// … — see `agent::prompt`), sent in front of `content` inside a
    /// `<context>` block. It is kept apart from what the user typed so the
    /// transcript stays readable, and it is never rewritten once sent: a
    /// server's prompt cache covers a request up to the first byte that
    /// differs from the previous one. Absent from session files written
    /// before it existed, and from every other kind of message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
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
            context: None,
        }
    }
    /// Attaches the context gathered for this user turn (see
    /// [`Message::context`]).
    pub fn with_context(mut self, context: Option<String>) -> Self {
        self.context = context;
        self
    }
    /// The text of the message as the model gets it: the turn's context
    /// block, if any, then the content. Both wire forms (chat completions
    /// and Responses) send user text through this, so the two agree.
    pub fn wire_text(&self) -> Cow<'_, str> {
        match &self.context {
            Some(ctx) => Cow::Owned(format!("<context>\n{ctx}\n</context>\n\n{}", self.content)),
            None => Cow::Borrowed(&self.content),
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
            context: None,
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
            context: None,
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
            context: None,
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
            let text = self.wire_text();
            if !text.is_empty() {
                parts.push(json!({ "type": "text", "text": text }));
            }
            for img in &self.images {
                parts.push(json!({ "type": "image_url", "image_url": { "url": img } }));
            }
            return json!({ "role": self.role, "content": parts });
        }
        json!({ "role": self.role, "content": self.wire_text() })
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
    /// The caller stopped the stream early (see [`stream_chat`]): `content`
    /// is partial and `tool_calls` is empty.
    pub interrupted: bool,
}

/// Token usage for a completion, as reported on the final SSE chunk when the
/// request set `stream_options.include_usage`. Consumed by the caller.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    /// The part of `prompt_tokens` the provider read from its prompt cache
    /// (`prompt_tokens_details.cached_tokens`); 0 when it doesn't say.
    #[serde(skip_serializing_if = "is_zero")]
    pub cached_tokens: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// [`Usage`] as chat completions reports it.
#[derive(Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default)]
    total_tokens: u32,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: u32,
}

impl From<WireUsage> for Usage {
    fn from(wire: WireUsage) -> Self {
        Self {
            prompt_tokens: wire.prompt_tokens,
            completion_tokens: wire.completion_tokens,
            total_tokens: wire.total_tokens,
            cached_tokens: wire.prompt_tokens_details.map_or(0, |d| d.cached_tokens),
        }
    }
}

/// How much of an error response's body is read: enough for any server's
/// error object, bounded in case it sends a page instead.
const ERROR_BODY_LIMIT: u64 = 8 * 1024;

/// How much of the server's explanation is shown in the error message.
const ERROR_DETAIL_CHARS: usize = 300;

/// What servers say when a request doesn't fit the model's context window
/// (matched in lowercase): OpenAI and vLLM "maximum context length" /
/// `context_length_exceeded`, llama.cpp "exceeds the available context
/// size", Anthropic-style "prompt is too long", and the like.
const OVERFLOW_MARKERS: &[&str] = &[
    "context length",
    "context_length",
    "context size",
    "context window",
    "maximum context",
    "too many tokens",
    "prompt is too long",
    "prompt too long",
    "input is too long",
    "request too large",
];

/// A completion request the endpoint answered with a non-2xx status, with
/// the start of what it said. A typed error (rather than a formatted
/// message) so the agent loop can tell a request that was too long for the
/// model — which it can fix by compacting — from any other failure: see
/// [`is_context_overflow`].
#[derive(Debug)]
pub struct HttpError {
    pub status: u16,
    /// The response body, up to [`ERROR_BODY_LIMIT`] bytes.
    pub body: String,
}

impl HttpError {
    /// Takes the status and reads the beginning of the error body.
    pub(crate) fn read(status: u16, body: impl Read) -> Self {
        let mut bytes = Vec::new();
        // Best effort: a body that can't be read leaves the status alone.
        let _ = body.take(ERROR_BODY_LIMIT).read_to_end(&mut bytes);
        Self {
            status,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    /// Blank `key` out of the body, should the server have echoed the
    /// credential it was sent ("invalid key sk-…"): the body ends up in an
    /// error message, and keys are never printed.
    pub(crate) fn without(mut self, key: Option<&str>) -> Self {
        if let Some(key) = key.filter(|k| !k.is_empty()) {
            self.body = self.body.replace(key, "[key]");
        }
        self
    }

    /// The server's own explanation: the `error.message` (OpenAI shape),
    /// `error` or `message` string of a JSON body, else the body's text.
    fn detail(&self) -> String {
        let json: Option<Value> = serde_json::from_str(&self.body).ok();
        let message = json.as_ref().and_then(|v| {
            [&v["error"]["message"], &v["error"], &v["message"]]
                .into_iter()
                .find_map(Value::as_str)
        });
        let text = message.unwrap_or(&self.body);
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.chars().count() > ERROR_DETAIL_CHARS {
            let cut: String = text.chars().take(ERROR_DETAIL_CHARS).collect();
            format!("{cut}…")
        } else {
            text
        }
    }

    /// Whether the request was refused for not fitting the model's context
    /// window: a 413, or a 400 whose body says so.
    pub fn is_context_overflow(&self) -> bool {
        let body = self.body.to_lowercase();
        self.status == 413
            || (self.status == 400 && OVERFLOW_MARKERS.iter().any(|m| body.contains(m)))
    }
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "provider returned HTTP {}", self.status)?;
        let detail = self.detail();
        if detail.is_empty() {
            Ok(())
        } else {
            write!(f, ": {detail}")
        }
    }
}

impl std::error::Error for HttpError {}

/// Whether `err` is the endpoint refusing a request as too long for the
/// model's context window (see [`HttpError::is_context_overflow`]).
pub fn is_context_overflow(err: &anyhow::Error) -> bool {
    err.downcast_ref::<HttpError>()
        .is_some_and(HttpError::is_context_overflow)
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
/// `on_event` returning [`ControlFlow::Break`] stops the stream there (the
/// connection is dropped): the result is marked `interrupted`, keeps the
/// partial text, and carries no tool calls, since those may be incomplete.
///
/// Returns the accumulated [`Completion`] once the stream ends. Every
/// request that gets that far is logged (see [`crate::usage`]).
pub fn stream_chat(
    target: &Target,
    messages: &[Message],
    tools: &[ToolSpec],
    on_event: impl FnMut(StreamEvent) -> ControlFlow<()>,
) -> Result<Completion> {
    let started = Instant::now();
    let completion = chat_stream(target, messages, tools, on_event)?;
    crate::usage::record(
        &target.provider.name,
        &target.model,
        completion.usage.as_ref(),
        started.elapsed(),
    );
    Ok(completion)
}

/// [`stream_chat`] proper: the request and its stream.
fn chat_stream(
    target: &Target,
    messages: &[Message],
    tools: &[ToolSpec],
    mut on_event: impl FnMut(StreamEvent) -> ControlFlow<()>,
) -> Result<Completion> {
    let body = chat_body(target, messages, tools);
    let body_bytes = serde_json::to_vec(&body)?;
    // The body only: the key travels in a header, which is never printed.
    if std::env::var_os("ATELIER_DEBUG").is_some() {
        eprintln!("--> {}", String::from_utf8_lossy(&body_bytes));
    }

    let (reader, key) = post_with_key(&target.provider, "chat/completions", &body_bytes)?;
    let status = reader.status();
    if !(200..300).contains(&status) {
        return Err(HttpError::read(status, reader)
            .without(key.as_deref())
            .into());
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
            out.usage = Some(usage.into());
        }
        let Some(choice) = chunk.choices.into_iter().next() else {
            continue;
        };
        let delta = choice.delta;
        if let Some(fr) = choice.finish_reason {
            finish_reason = Some(fr);
        }

        let mut flow = ControlFlow::Continue(());
        if let Some(r) = delta.reasoning()
            && !r.is_empty()
        {
            flow = on_event(StreamEvent::Reasoning(r));
        }
        if let Some(c) = &delta.content
            && !c.is_empty()
            && flow.is_continue()
        {
            out.content.push_str(c);
            flow = on_event(StreamEvent::Content(c));
        }
        if flow.is_break() {
            out.interrupted = true;
            return Ok(out);
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
        let entry = build_trace_entry(&target.model, &body, &tool_names, &out);
        trace(&path, &entry);
    }

    Ok(out)
}

/// The chat-completions request body: what [`stream_chat`] sends, and what
/// `atelier debug prompt` prints. Pure — and without the API key, which is
/// a header. The variant's options go in first and the fields atelier owns
/// are written over them, so no option can replace the model, the messages
/// or the stream settings.
pub fn chat_body(target: &Target, messages: &[Message], tools: &[ToolSpec]) -> Value {
    let mut body = Value::Object(target.options.clone());
    body["model"] = json!(target.model);
    body["messages"] = messages.iter().map(Message::to_wire).collect();
    body["stream"] = json!(true);
    body["stream_options"] = json!({ "include_usage": true });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools.iter().map(ToolSpec::to_wire).collect());
    }
    body
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
    usage: Option<WireUsage>,
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
/// `headers` are sent in addition to the bearer token; `timeout` is the
/// connect timeout, and the longest silence on the stream (60 s at least).
fn send_reader_retrying(
    url: &str,
    body: &[u8],
    api_key: Option<&str>,
    headers: &[(String, String)],
    timeout: Duration,
) -> Result<rsurl::BodyReader> {
    let mut attempt: u32 = 0;
    loop {
        let mut req = rsurl::Request::new("POST", url)
            .context("building request")?
            .connect_timeout(timeout)
            // Also the longest silence allowed on the open stream: a server
            // that is still reading a long prompt sends nothing meanwhile.
            .read_timeout(Some(timeout.max(MIN_READ_TIMEOUT)))
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            // Fresh connection per request: reusing a pooled keep-alive
            // connection here can surface a spurious EAGAIN on the next send.
            .header("connection", "close")
            .body(body.to_vec());
        for (name, value) in headers {
            req = req.header(name, value);
        }
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

/// POST `body` to `path` on `provider` with its key and headers (see
/// [`send_reader_retrying`]). A 401 from a provider whose key comes from a
/// command means the cached key has expired: the command is run again and
/// the request sent once more. Returns the key that was used along with the
/// response, so the caller can keep it out of an error message.
fn post_with_key(
    provider: &Provider,
    path: &str,
    body: &[u8],
) -> Result<(rsurl::BodyReader, Option<String>)> {
    let url = provider.endpoint(path);
    let timeout = connect_timeout(provider, DEFAULT_CHAT_TIMEOUT_MS);
    let send = || -> Result<_> {
        let key = provider.key()?;
        let reader = send_reader_retrying(&url, body, key.as_deref(), &provider.headers, timeout)?;
        Ok((reader, key))
    };
    let (reader, key) = send()?;
    if reader.status() == 401 && provider.refresh_key() {
        return send();
    }
    Ok((reader, key))
}

/// What `/models` prints: the ids each configured provider advertises. With
/// several providers they are written the way `/model` takes them,
/// `provider/model`, and a provider that can't be reached gets a line saying
/// so in place of its models; with only the endpoint from the environment,
/// the bare ids.
pub fn model_listing(cfg: &Config) -> Vec<String> {
    let mut offered = models_offered(cfg);
    if offered.len() == 1 {
        return match offered.remove(0).1 {
            Ok(models) if models.is_empty() => vec!["(no models returned)".to_string()],
            Ok(models) => models,
            Err(e) => vec![format!("error: {e:#}")],
        };
    }
    let mut out = Vec::new();
    for (provider, models) in offered {
        match models {
            Ok(models) if models.is_empty() => {
                out.push(format!("{}: (no models returned)", provider.name));
            }
            Ok(models) => out.extend(models.iter().map(|m| format!("{}/{m}", provider.name))),
            Err(e) => out.push(format!("{}: error: {e:#}", provider.name)),
        }
    }
    out
}

/// What each configured provider says it offers ([`list_models`]), in
/// configuration order: the ids, or why they could not be had.
pub fn models_offered(cfg: &Config) -> Vec<(Arc<Provider>, Result<Vec<String>>)> {
    let providers = cfg.providers().iter();
    providers
        .map(|provider| (Arc::clone(provider), list_models(provider)))
        .collect()
}

/// List model ids advertised by the endpoint (`GET /models`).
///
/// Like [`send_reader_retrying`], a response with a transient server status
/// (see [`is_retryable_status`]) is retried (rebuilding the request, capped
/// at [`MAX_SEND_ATTEMPTS`]) rather than immediately surfaced — this is a
/// cheap, idempotent GET, so retrying it is straightforward.
///
/// The provider's key and headers are sent, as with a completion. The
/// connect timeout is `ATELIER_HTTP_TIMEOUT_MS` (milliseconds), else the
/// provider's own, else [`DEFAULT_LIST_MODELS_TIMEOUT_MS`].
pub fn list_models(provider: &Provider) -> Result<Vec<String>> {
    let timeout = connect_timeout(provider, DEFAULT_LIST_MODELS_TIMEOUT_MS);
    list_models_within(provider, timeout)
}

/// [`list_models`] with the connect timeout given by the caller, for
/// `atelier doctor`, which is not prepared to wait as long.
pub fn list_models_within(provider: &Provider, timeout: Duration) -> Result<Vec<String>> {
    let url = provider.endpoint("models");
    let key = provider.key()?;
    let mut attempt: u32 = 0;
    let resp = loop {
        let mut req = rsurl::Request::new("GET", &url)
            .context("building request")?
            .connect_timeout(timeout);
        for (name, value) in &provider.headers {
            req = req.header(name, value);
        }
        if let Some(key) = &key {
            req = req.header("authorization", &format!("Bearer {key}"));
        }
        let resp = req.send().context("sending request")?;
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
        let usage: Usage = chunk.usage.expect("usage present").into();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.total_tokens, 15);
        assert_eq!(usage.cached_tokens, 0);
    }

    /// The cached part of the prompt, where the provider reports it.
    #[test]
    fn parses_cached_prompt_tokens() {
        let data = r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":5,"total_tokens":105,"prompt_tokens_details":{"cached_tokens":80,"audio_tokens":0}}}"#;
        let chunk: ChatChunk = serde_json::from_str(data).expect("valid chunk");
        let usage: Usage = chunk.usage.expect("usage present").into();
        assert_eq!(usage.cached_tokens, 80);
        // Details without the count, or a null, are no cached tokens.
        for details in ["{}", "null"] {
            let data = format!(
                r#"{{"choices":[],"usage":{{"prompt_tokens":1,"prompt_tokens_details":{details}}}}}"#
            );
            let chunk: ChatChunk = serde_json::from_str(&data).expect("valid chunk");
            assert_eq!(Usage::from(chunk.usage.unwrap()).cached_tokens, 0);
        }
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
    /// A turn's context goes out in front of what the user typed, in the
    /// same user message (no second system message, no extra turn), and is
    /// left out of the stored form when there is none.
    #[test]
    fn to_wire_puts_the_turn_context_before_the_user_text() {
        let msg = Message::user("fix it").with_context(Some("## git status\nM a.rs".into()));
        let wire = msg.to_wire();
        assert_eq!(wire["role"], "user");
        assert_eq!(
            wire["content"],
            "<context>\n## git status\nM a.rs\n</context>\n\nfix it"
        );

        let with_image = Message::user_with_images("look", vec!["data:x".into()])
            .with_context(Some("ctx".into()));
        assert_eq!(
            with_image.to_wire()["content"][0]["text"],
            "<context>\nctx\n</context>\n\nlook"
        );

        let stored = serde_json::to_value(Message::user("plain")).unwrap();
        assert!(stored.get("context").is_none(), "{stored}");
        let stored = serde_json::to_value(&msg).unwrap();
        assert_eq!(stored["context"], "## git status\nM a.rs");
        assert_eq!(stored["content"], "fix it");
    }

    /// A variant's options are top-level body fields next to the ones
    /// atelier sets, which they can't replace; without options the body is
    /// what it always was.
    #[test]
    fn chat_body_merges_the_variant_options() {
        let provider = std::sync::Arc::new(Provider::from_env("http://x/v1".into(), None));
        let mut target = Target {
            provider,
            model: "m".into(),
            options: serde_json::Map::new(),
        };
        let messages = [Message::system("sys"), Message::user("hi")];
        let plain = chat_body(&target, &messages, &[]);
        assert_eq!(
            plain,
            json!({
                "model": "m",
                "messages": [
                    {"role": "system", "content": "sys"},
                    {"role": "user", "content": "hi"},
                ],
                "stream": true,
                "stream_options": {"include_usage": true},
            })
        );

        for (key, value) in [
            ("reasoning_effort", json!("high")),
            ("temperature", json!(0.2)),
            ("model", json!("hijacked")),
            ("messages", json!([])),
            ("stream", json!(false)),
        ] {
            target.options.insert(key.into(), value);
        }
        let body = chat_body(&target, &messages, &[]);
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["temperature"], 0.2);
        for owned in ["model", "messages", "stream", "stream_options"] {
            assert_eq!(body[owned], plain[owned], "{owned}");
        }
    }

    #[test]
    fn an_echoed_key_is_blanked_out_of_the_error() {
        let err = HttpError::read(401, &br#"{"error":{"message":"bad key sk-abc"}}"#[..]);
        let shown = err.without(Some("sk-abc")).to_string();
        assert_eq!(shown, "provider returned HTTP 401: bad key [key]");
        let untouched = HttpError::read(401, &b"nope"[..]).without(None);
        assert_eq!(untouched.body, "nope");
    }

    #[test]
    fn http_error_shows_what_the_server_said() {
        let openai = HttpError::read(
            400,
            &br#"{"error":{"message":"This model's maximum context length is 8192 tokens.","code":"context_length_exceeded"}}"#[..],
        );
        assert_eq!(
            openai.to_string(),
            "provider returned HTTP 400: This model's maximum context length is 8192 tokens."
        );
        let flat = HttpError::read(500, &br#"{"error":"model not loaded"}"#[..]);
        assert_eq!(
            flat.to_string(),
            "provider returned HTTP 500: model not loaded"
        );
        let text = HttpError::read(502, &b"Bad\n  Gateway"[..]);
        assert_eq!(text.to_string(), "provider returned HTTP 502: Bad Gateway");
        let empty = HttpError::read(404, &b""[..]);
        assert_eq!(empty.to_string(), "provider returned HTTP 404");
        let long = HttpError::read(500, "x".repeat(5_000).as_bytes());
        assert!(long.to_string().chars().count() < 400);
    }

    #[test]
    fn context_overflow_is_recognized_by_status_and_wording() {
        let overflow = |status: u16, body: &str| {
            let err: anyhow::Error = HttpError::read(status, body.as_bytes()).into();
            is_context_overflow(&err)
        };
        assert!(overflow(
            400,
            r#"{"error":{"code":"context_length_exceeded","message":"too big"}}"#
        ));
        assert!(overflow(
            400,
            r#"{"error":{"message":"the request exceeds the available context size, try increasing it"}}"#
        ));
        assert!(overflow(
            400,
            "This model's Maximum Context Length is 4096 tokens"
        ));
        assert!(overflow(413, ""));
        // Any other bad request, or the wording under another status, isn't.
        assert!(!overflow(400, r#"{"error":{"message":"unknown model"}}"#));
        assert!(!overflow(500, "context length exceeded"));
        assert!(!is_context_overflow(&anyhow::anyhow!(
            "context length exceeded"
        )));
    }

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
                cached_tokens: 0,
            }),
            finish_reason: Some("tool_calls".to_string()),
            interrupted: false,
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
