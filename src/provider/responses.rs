//! OpenAI **Responses API** backend — used with a Sign-in-with-ChatGPT bearer
//! token (the ChatGPT-plan access token is valid for `/v1/responses`, not
//! `/v1/chat/completions`). Mirrors [`super::stream_chat`] but speaks the
//! Responses protocol, mapping its typed SSE events back onto the shared
//! [`Completion`]/[`StreamEvent`]/[`ToolCall`] types so the agent loop is
//! backend-agnostic.
//!
//! # Contract (stable — the implementer owns this file)
//!
//! `stream_responses(base_url, access_token, model, messages, tools, on_event)`
//! POSTs `base_url`/responses (e.g. `https://api.openai.com/v1/responses`) with
//! `Authorization: Bearer <access_token>` and `stream:true`, and returns the
//! accumulated [`Completion`].
//!
//! ## Request mapping (our `Message` → Responses `input`/`instructions`/`tools`)
//! - Build JSON: `{ model, instructions, input: [...], tools: [...], stream:true }`.
//! - The single leading `system` message → top-level `instructions` (string).
//! - `user` → `{ "role":"user", "content": <text> }` (for images, content is an
//!   array of parts incl. `{ "type":"input_image", "image_url": <data-url> }`
//!   alongside `{ "type":"input_text", "text": <text> }`).
//! - `assistant` text → `{ "role":"assistant", "content": <text> }`.
//! - an assistant turn's tool calls → one input item each:
//!   `{ "type":"function_call", "call_id": <id>, "name": <name>, "arguments": <json string> }`.
//! - a tool result (`role="tool"`, `tool_call_id`) →
//!   `{ "type":"function_call_output", "call_id": <id>, "output": <result string> }`.
//! - tools: the Responses **function tool is flat** —
//!   `{ "type":"function", "name", "description", "parameters": <schema> }`
//!   (NOT nested under a `function` key like chat/completions).
//!
//! ## Streaming events (SSE `data: {json}` frames; dispatch on the `type` field)
//! - `response.output_text.delta` → `StreamEvent::Content(ev.delta)`.
//! - reasoning deltas (`response.reasoning_summary_text.delta`, if present) →
//!   `StreamEvent::Reasoning(ev.delta)`.
//! - `response.output_item.added` with `item.type=="function_call"` starts a
//!   tool call (record `call_id`/`name`); `response.function_call_arguments.delta`
//!   (`{ item_id, delta }`) appends argument fragments; the call completes at
//!   `response.function_call_arguments.done` / `response.output_item.done`.
//! - `response.completed` carries `response.usage` with `input_tokens`/
//!   `output_tokens`/`total_tokens` → fill `Completion.usage`; `finish_reason`
//!   isn't present as such — leave `None` (or set from an incomplete/`max_output_tokens`
//!   signal if the API reports one).
//! - ignore unrecognized event types; a `response.failed`/`error` event → `Err`.
//!
//! Use the same connection hygiene as `stream_chat` (fresh connection per
//! request / drain on `[DONE]` if sent). `rsurl`'s `send_reader()` gives the
//! blocking `Read` + status to drive the SSE line-by-line.

use std::io::{BufRead, BufReader};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Completion, Message, StreamEvent, ToolCall, ToolSpec, Usage};

/// Stream a Responses-API turn. See the module contract.
///
/// Mirrors [`super::stream_chat`] but speaks the Responses protocol: it builds
/// the `{ model, instructions, input, tools, stream }` body (see
/// [`build_body`]), POSTs it through [`super::send_reader_retrying`] (which
/// sets `Authorization: Bearer`, `content-type: application/json`,
/// `accept: text/event-stream` and `connection: close`, with the same
/// connect-timeout / retry hygiene), then drives the typed SSE event stream
/// line-by-line, folding each event into a [`ResponsesAccum`] that assembles
/// the final [`Completion`].
#[allow(dead_code)] // wired into the agent loop; call site lands with sign-in-with-ChatGPT
pub fn stream_responses(
    base_url: &str,
    access_token: &str,
    model: &str,
    messages: &[Message],
    tools: &[ToolSpec],
    mut on_event: impl FnMut(StreamEvent),
) -> Result<Completion> {
    let body = build_body(model, messages, tools);
    let body_bytes = serde_json::to_vec(&body)?;
    if std::env::var_os("ATELIER_DEBUG").is_some() {
        eprintln!("--> {}", String::from_utf8_lossy(&body_bytes));
    }

    let url = format!("{}/responses", base_url.trim_end_matches('/'));
    // Reuse the shared transport helper (fresh connection per request,
    // `connection: close`, bounded retries). The access token rides as the
    // bearer credential, exactly like `stream_chat`'s api key.
    let reader = super::send_reader_retrying(&url, &body_bytes, Some(access_token))?;
    let status = reader.status();
    if !(200..300).contains(&status) {
        bail!("provider returned HTTP {status}");
    }

    let mut accum = ResponsesAccum::default();
    let mut lines = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        if lines.read_line(&mut line).context("reading stream")? == 0 {
            break; // EOF
        }
        // SSE: Responses emits an `event:` line plus a `data:` line whose JSON
        // *also* carries the `type`, so dispatching on the data payload alone
        // is sufficient. Only `data:` lines matter; blanks separate events.
        let Some(data) = line.trim_end().strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() {
            continue;
        }
        if data == "[DONE]" {
            // Drain trailing bytes so the socket is left clean for reuse.
            let _ = std::io::copy(&mut lines, &mut std::io::sink());
            break;
        }
        // Be lenient: skip keep-alives / frames we can't parse.
        let Ok(ev) = serde_json::from_str::<RespEvent>(data) else {
            continue;
        };
        accum.handle_event(&ev, &mut on_event)?;
    }

    Ok(accum.finish())
}

/// Build the Responses request body from our shared message/tool types. Pure
/// and deterministic, so it's unit-testable without touching the network.
///
/// See the module contract for the full mapping. In short: a single leading
/// `system` message becomes top-level `instructions`; `user`/`assistant` turns
/// become `{ role, content }` items (user images expand to an `input_text` +
/// `input_image` part array); an assistant turn's tool calls become
/// `function_call` items; a `tool` result becomes a `function_call_output`
/// item; and tools are advertised as *flat* function tools.
fn build_body(model: &str, messages: &[Message], tools: &[ToolSpec]) -> Value {
    let mut instructions: Option<String> = None;
    let mut input: Vec<Value> = Vec::new();

    for (i, msg) in messages.iter().enumerate() {
        match msg.role.as_str() {
            // The single leading system message is lifted to `instructions`.
            "system" if i == 0 => {
                instructions = Some(msg.content.clone());
            }
            // Any further system message (unusual) stays an input item.
            "system" => {
                input.push(json!({ "role": "system", "content": msg.content }));
            }
            "user" => {
                if msg.images.is_empty() {
                    input.push(json!({ "role": "user", "content": msg.content }));
                } else {
                    let mut parts = Vec::with_capacity(1 + msg.images.len());
                    parts.push(json!({ "type": "input_text", "text": msg.content }));
                    for img in &msg.images {
                        parts.push(json!({ "type": "input_image", "image_url": img }));
                    }
                    input.push(json!({ "role": "user", "content": parts }));
                }
            }
            "assistant" => {
                if msg.tool_calls.is_empty() {
                    input.push(json!({ "role": "assistant", "content": msg.content }));
                } else {
                    // Keep any assistant prose as its own item before the calls.
                    if !msg.content.is_empty() {
                        input.push(json!({ "role": "assistant", "content": msg.content }));
                    }
                    for tc in &msg.tool_calls {
                        input.push(json!({
                            "type": "function_call",
                            "call_id": tc.id,
                            "name": tc.name,
                            "arguments": tc.arguments,
                        }));
                    }
                }
            }
            "tool" => {
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": msg.tool_call_id.clone().unwrap_or_default(),
                    "output": msg.content,
                }));
            }
            // Unknown role: pass through as a plain item rather than drop it.
            other => {
                input.push(json!({ "role": other, "content": msg.content }));
            }
        }
    }

    let mut body = json!({
        "model": model,
        "input": input,
        "stream": true,
    });
    if let Some(instr) = instructions {
        body["instructions"] = Value::String(instr);
    }
    if !tools.is_empty() {
        // Flat function tool: `type`/`name`/`description`/`parameters` at the
        // top level (NOT nested under a `function` key like chat/completions).
        body["tools"] = Value::Array(
            tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    })
                })
                .collect(),
        );
    }
    body
}

/// A tool call under assembly, keyed by its streamed output-item `id`.
/// `call_id` is the stable identifier echoed back to the API (and becomes
/// [`ToolCall::id`]); `item_id` is only used to route argument deltas.
#[derive(Default)]
struct RespToolCall {
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
}

/// Running state folded from the Responses SSE event stream into a
/// [`Completion`].
#[derive(Default)]
struct ResponsesAccum {
    content: String,
    usage: Option<Usage>,
    /// Tool calls in stable insertion order (i.e. output-item order).
    calls: Vec<RespToolCall>,
}

impl ResponsesAccum {
    /// Find (or create) the accumulator slot for a given output-item id,
    /// preserving first-seen order.
    fn slot(&mut self, item_id: &str) -> &mut RespToolCall {
        if let Some(pos) = self.calls.iter().position(|c| c.item_id == item_id) {
            &mut self.calls[pos]
        } else {
            self.calls.push(RespToolCall {
                item_id: item_id.to_string(),
                ..Default::default()
            });
            self.calls
                .last_mut()
                .expect("just pushed an element, so last_mut is Some")
        }
    }

    /// Fold one parsed SSE event into the running state, emitting
    /// `Content`/`Reasoning` events as text arrives. Returns `Err` for a
    /// `response.failed` / top-level `error` event.
    fn handle_event<F: FnMut(StreamEvent)>(
        &mut self,
        ev: &RespEvent,
        on_event: &mut F,
    ) -> Result<()> {
        match ev.r#type.as_str() {
            "response.output_text.delta" => {
                if !ev.delta.is_empty() {
                    self.content.push_str(&ev.delta);
                    on_event(StreamEvent::Content(&ev.delta));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if !ev.delta.is_empty() {
                    on_event(StreamEvent::Reasoning(&ev.delta));
                }
            }
            "response.output_item.added" | "response.output_item.done" => {
                if let Some(item) = &ev.item
                    && item.r#type == "function_call"
                {
                    let slot = self.slot(&item.id);
                    if !item.call_id.is_empty() {
                        slot.call_id = item.call_id.clone();
                    }
                    if !item.name.is_empty() {
                        slot.name = item.name.clone();
                    }
                    // `done` may carry the complete arguments; only adopt them
                    // if streamed deltas didn't already build the string.
                    if let Some(args) = &item.arguments
                        && slot.arguments.is_empty()
                        && !args.is_empty()
                    {
                        slot.arguments = args.clone();
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                if !ev.item_id.is_empty() && !ev.delta.is_empty() {
                    self.slot(&ev.item_id).arguments.push_str(&ev.delta);
                }
            }
            "response.function_call_arguments.done" => {
                if !ev.item_id.is_empty()
                    && let Some(args) = &ev.arguments
                {
                    let slot = self.slot(&ev.item_id);
                    if slot.arguments.is_empty() && !args.is_empty() {
                        slot.arguments = args.clone();
                    }
                }
            }
            "response.completed" => {
                if let Some(u) = ev.response.as_ref().and_then(|r| r.usage.as_ref()) {
                    self.usage = Some(Usage {
                        prompt_tokens: u.input_tokens,
                        completion_tokens: u.output_tokens,
                        total_tokens: u.total_tokens,
                    });
                }
            }
            "response.failed" => {
                let msg = ev
                    .response
                    .as_ref()
                    .and_then(|r| r.error.as_ref())
                    .map(|e| e.message.clone())
                    .or_else(|| ev.error.as_ref().map(|e| e.message.clone()))
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| "response failed".to_string());
                bail!("Responses API error: {msg}");
            }
            "error" => {
                let msg = ev
                    .error
                    .as_ref()
                    .map(|e| e.message.clone())
                    .or_else(|| ev.message.clone())
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| "stream error".to_string());
                bail!("Responses API error: {msg}");
            }
            // Ignore unrecognized / lifecycle event types.
            _ => {}
        }
        Ok(())
    }

    /// Collapse the running state into the final [`Completion`]. `finish_reason`
    /// isn't part of the Responses protocol as such, so it's left `None`.
    fn finish(self) -> Completion {
        let tool_calls = self
            .calls
            .into_iter()
            .map(|c| ToolCall {
                id: c.call_id,
                name: c.name,
                arguments: c.arguments,
            })
            .collect();
        Completion {
            content: self.content,
            tool_calls,
            usage: self.usage,
            finish_reason: None,
        }
    }
}

/// A Responses SSE event frame. Deliberately lenient: every field is
/// `#[serde(default)]` and the union of what the handled event types carry, so
/// a frame that only populates a subset still parses.
#[derive(Deserialize, Default)]
struct RespEvent {
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    delta: String,
    #[serde(default)]
    item_id: String,
    #[serde(default)]
    item: Option<RespItem>,
    #[serde(default)]
    arguments: Option<String>,
    #[serde(default)]
    response: Option<RespResponse>,
    #[serde(default)]
    error: Option<RespError>,
    /// Top-level message on an `error` event frame.
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize, Default)]
struct RespItem {
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    call_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize, Default)]
struct RespResponse {
    #[serde(default)]
    usage: Option<RespUsage>,
    #[serde(default)]
    error: Option<RespError>,
}

#[derive(Deserialize, Default)]
struct RespUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[serde(default)]
    total_tokens: u32,
}

#[derive(Deserialize, Default)]
struct RespError {
    #[serde(default)]
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_call(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: args.to_string(),
        }
    }

    /// The request builder maps a system+user+assistant-tool-call+tool-result
    /// conversation and one tool to the expected Responses body: a top-level
    /// `instructions` string, the correctly-shaped `input` items (including
    /// `function_call` / `function_call_output`), and a *flat* function tool.
    #[test]
    fn build_body_maps_messages_and_flat_tool() {
        let messages = vec![
            Message::system("be helpful"),
            Message::user("what's the weather in SF?"),
            Message::assistant_tool_calls(
                "let me check",
                vec![tool_call("call_1", "get_weather", r#"{"city":"SF"}"#)],
            ),
            Message::tool_result("call_1", "sunny, 21C"),
        ];
        let tools = vec![ToolSpec {
            name: "get_weather".to_string(),
            description: "Look up the weather".to_string(),
            parameters: json!({
                "type": "object",
                "properties": { "city": { "type": "string" } },
                "required": ["city"],
            }),
        }];

        let body = build_body("gpt-x", &messages, &tools);

        assert_eq!(body["model"], json!("gpt-x"));
        assert_eq!(body["stream"], json!(true));
        // Leading system -> instructions, and NOT also an input item.
        assert_eq!(body["instructions"], json!("be helpful"));

        let input = body["input"].as_array().expect("input array");
        // user, assistant prose, function_call, function_call_output
        assert_eq!(input.len(), 4);
        assert_eq!(
            input[0],
            json!({ "role": "user", "content": "what's the weather in SF?" })
        );
        assert_eq!(
            input[1],
            json!({ "role": "assistant", "content": "let me check" })
        );
        assert_eq!(
            input[2],
            json!({
                "type": "function_call",
                "call_id": "call_1",
                "name": "get_weather",
                "arguments": r#"{"city":"SF"}"#,
            })
        );
        assert_eq!(
            input[3],
            json!({
                "type": "function_call_output",
                "call_id": "call_1",
                "output": "sunny, 21C",
            })
        );

        // Flat function tool (no nested `function` key).
        let tools_json = body["tools"].as_array().expect("tools array");
        assert_eq!(tools_json.len(), 1);
        assert_eq!(tools_json[0]["type"], json!("function"));
        assert_eq!(tools_json[0]["name"], json!("get_weather"));
        assert_eq!(tools_json[0]["description"], json!("Look up the weather"));
        assert_eq!(tools_json[0]["parameters"]["type"], json!("object"));
        assert!(
            tools_json[0].get("function").is_none(),
            "tool must be flat, not nested under `function`"
        );
    }

    /// A user turn carrying images expands `content` into an `input_text` +
    /// `input_image` part array.
    #[test]
    fn build_body_user_images_become_part_array() {
        let messages = vec![Message::user_with_images(
            "look",
            vec!["data:image/png;base64,AAAA".to_string()],
        )];
        let body = build_body("gpt-x", &messages, &[]);
        // No tools -> no `tools` key.
        assert!(body.get("tools").is_none());
        let content = body["input"][0]["content"]
            .as_array()
            .expect("array content");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0], json!({ "type": "input_text", "text": "look" }));
        assert_eq!(
            content[1],
            json!({ "type": "input_image", "image_url": "data:image/png;base64,AAAA" })
        );
    }

    /// Feed a representative event sequence through the per-event handler and
    /// assert: content accumulates across text deltas, reasoning is surfaced
    /// (not folded into content), the tool call is assembled from
    /// added/name/call_id + concatenated argument deltas, and usage is parsed
    /// from `response.completed`.
    #[test]
    fn handle_events_accumulate_content_tools_and_usage() {
        let lines = [
            r#"{"type":"response.reasoning_summary_text.delta","delta":"thinking..."}"#,
            r#"{"type":"response.output_text.delta","delta":"Hello"}"#,
            r#"{"type":"response.output_text.delta","delta":", world"}"#,
            r#"{"type":"response.output_item.added","item":{"id":"fc_1","type":"function_call","call_id":"call_42","name":"get_weather"}}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"city\":"}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"\"SF\"}"}"#,
            r#"{"type":"response.function_call_arguments.done","item_id":"fc_1","arguments":"{\"city\":\"SF\"}"}"#,
            r#"{"type":"response.output_item.done","item":{"id":"fc_1","type":"function_call","call_id":"call_42","name":"get_weather","arguments":"{\"city\":\"SF\"}"}}"#,
            r#"{"type":"response.some_future_event","delta":"ignored"}"#,
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":12,"output_tokens":7,"total_tokens":19}}}"#,
        ];

        let mut accum = ResponsesAccum::default();
        let mut reasoning = String::new();
        let mut content = String::new();
        for raw in lines {
            let ev: RespEvent = serde_json::from_str(raw).expect("valid event");
            accum
                .handle_event(&ev, &mut |e| match e {
                    StreamEvent::Reasoning(r) => reasoning.push_str(r),
                    StreamEvent::Content(c) => content.push_str(c),
                })
                .expect("no error event");
        }

        // Emitted events mirror the accumulated content.
        assert_eq!(reasoning, "thinking...");
        assert_eq!(content, "Hello, world");
        assert_eq!(accum.content, "Hello, world");

        let completion = accum.finish();
        assert_eq!(completion.content, "Hello, world");
        assert_eq!(completion.tool_calls.len(), 1);
        let tc = &completion.tool_calls[0];
        // `id` is the API `call_id`, not the streamed item id.
        assert_eq!(tc.id, "call_42");
        assert_eq!(tc.name, "get_weather");
        assert_eq!(tc.arguments, r#"{"city":"SF"}"#);

        let usage = completion.usage.expect("usage parsed");
        assert_eq!(usage.prompt_tokens, 12);
        assert_eq!(usage.completion_tokens, 7);
        assert_eq!(usage.total_tokens, 19);
        assert_eq!(completion.finish_reason, None);
    }

    /// Two concurrent tool calls keep a stable order (first-seen output item
    /// first) even when their argument deltas interleave.
    #[test]
    fn handle_events_assemble_multiple_tool_calls_in_order() {
        let lines = [
            r#"{"type":"response.output_item.added","item":{"id":"fc_a","type":"function_call","call_id":"call_a","name":"first"}}"#,
            r#"{"type":"response.output_item.added","item":{"id":"fc_b","type":"function_call","call_id":"call_b","name":"second"}}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_b","delta":"{\"b\":1}"}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_a","delta":"{\"a\":1}"}"#,
        ];
        let mut accum = ResponsesAccum::default();
        for raw in lines {
            let ev: RespEvent = serde_json::from_str(raw).expect("valid event");
            accum.handle_event(&ev, &mut |_| {}).expect("ok");
        }
        let completion = accum.finish();
        assert_eq!(completion.tool_calls.len(), 2);
        assert_eq!(completion.tool_calls[0].id, "call_a");
        assert_eq!(completion.tool_calls[0].name, "first");
        assert_eq!(completion.tool_calls[0].arguments, r#"{"a":1}"#);
        assert_eq!(completion.tool_calls[1].id, "call_b");
        assert_eq!(completion.tool_calls[1].arguments, r#"{"b":1}"#);
    }

    /// A `response.failed` frame surfaces the nested error message as an `Err`.
    #[test]
    fn handle_response_failed_is_err() {
        let raw = r#"{"type":"response.failed","response":{"error":{"message":"model exploded"}}}"#;
        let ev: RespEvent = serde_json::from_str(raw).expect("valid event");
        let mut accum = ResponsesAccum::default();
        let err = accum
            .handle_event(&ev, &mut |_| {})
            .expect_err("failed event must error");
        assert!(err.to_string().contains("model exploded"), "{err}");
    }

    /// A top-level `error` frame surfaces its `message` as an `Err`.
    #[test]
    fn handle_top_level_error_is_err() {
        let raw = r#"{"type":"error","message":"bad token","code":"invalid_api_key"}"#;
        let ev: RespEvent = serde_json::from_str(raw).expect("valid event");
        let mut accum = ResponsesAccum::default();
        let err = accum
            .handle_event(&ev, &mut |_| {})
            .expect_err("error event must error");
        assert!(err.to_string().contains("bad token"), "{err}");
    }

    /// Lenient parsing: a frame missing most fields still deserializes and is
    /// handled as a no-op (nothing accumulated, no error).
    #[test]
    fn handle_sparse_frame_is_noop() {
        let raw = r#"{"type":"response.in_progress"}"#;
        let ev: RespEvent = serde_json::from_str(raw).expect("sparse frame parses");
        let mut accum = ResponsesAccum::default();
        accum.handle_event(&ev, &mut |_| {}).expect("ok");
        let completion = accum.finish();
        assert!(completion.content.is_empty());
        assert!(completion.tool_calls.is_empty());
        assert!(completion.usage.is_none());
    }
}
