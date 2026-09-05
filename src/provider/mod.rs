//! OpenAI-compatible model provider.
//!
//! M0 scope: streaming `chat/completions` over Server-Sent Events, with the
//! model's reasoning ("thinking") separated from the final answer. Tool calls,
//! multimodal content parts, and non-streaming fallback land in later
//! milestones (see roadmap M0/M1).
//!
//! Transport is [`rsurl`]: `Request::send_reader()` returns a blocking
//! `Read` + `.status()`, which we drive line-by-line as an SSE stream.

use std::io::{BufRead, BufReader};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::Config;

/// A single chat message. Text-only for M0; content parts arrive with vision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    #[allow(dead_code)] // used once the system prompt lands (M1)
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
        }
    }
}

/// An incremental piece of a streamed response.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A chunk of the model's private reasoning. Displayed separately and
    /// **never** fed back to the model as assistant output.
    Reasoning(String),
    /// A chunk of the final answer.
    Content(String),
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
    stream: bool,
}

/// Stream a chat completion, invoking `on_event` for each delta as it arrives.
///
/// Returns the fully-accumulated answer text (reasoning excluded) once the
/// stream completes.
pub fn stream_chat(
    cfg: &Config,
    messages: &[Message],
    mut on_event: impl FnMut(StreamEvent),
) -> Result<String> {
    let body = serde_json::to_vec(&ChatRequest {
        model: &cfg.model,
        messages,
        stream: true,
    })?;

    let mut req = rsurl::Request::new("POST", &cfg.endpoint("chat/completions"))
        .context("building request")?
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .body(body);
    if let Some(key) = &cfg.api_key {
        req = req.header("authorization", &format!("Bearer {key}"));
    }

    let reader = req.send_reader().context("sending request")?;
    let status = reader.status();
    if !(200..300).contains(&status) {
        bail!("provider returned HTTP {status}");
    }

    let mut answer = String::new();
    let mut lines = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        let n = lines.read_line(&mut line).context("reading stream")?;
        if n == 0 {
            break; // EOF
        }
        let line = line.trim_end();
        // SSE: we only care about `data:` lines; blank lines separate events.
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() {
            continue;
        }
        if data == "[DONE]" {
            break;
        }

        // Be lenient: servers vary in exact field shape.
        let chunk: ChatChunk = match serde_json::from_str(data) {
            Ok(c) => c,
            Err(_) => continue, // skip keep-alives / unknown frames
        };
        let Some(choice) = chunk.choices.into_iter().next() else {
            continue;
        };
        if let Some(r) = choice.delta.reasoning()
            && !r.is_empty()
        {
            on_event(StreamEvent::Reasoning(r.to_string()));
        }
        if let Some(c) = &choice.delta.content
            && !c.is_empty()
        {
            answer.push_str(c);
            on_event(StreamEvent::Content(c.clone()));
        }
    }

    Ok(answer)
}

#[derive(Deserialize)]
struct ChatChunk {
    #[serde(default)]
    choices: Vec<Choice>,
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
    // Different servers name the thinking channel differently; accept both.
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
}

impl Delta {
    fn reasoning(&self) -> Option<&str> {
        self.reasoning_content
            .as_deref()
            .or(self.reasoning.as_deref())
    }
}

/// List model ids advertised by the endpoint (`GET /models`).
pub fn list_models(cfg: &Config) -> Result<Vec<String>> {
    let resp = rsurl::Request::new("GET", &cfg.endpoint("models"))
        .context("building request")?
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
