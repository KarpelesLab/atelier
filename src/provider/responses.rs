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

use anyhow::{Result, bail};

use super::{Completion, Message, StreamEvent, ToolSpec};

/// Stream a Responses-API turn. See the module contract.
#[allow(dead_code)] // wired into the agent loop; body is a stub until implemented
pub fn stream_responses(
    _base_url: &str,
    _access_token: &str,
    _model: &str,
    _messages: &[Message],
    _tools: &[ToolSpec],
    _on_event: impl FnMut(StreamEvent),
) -> Result<Completion> {
    bail!("the Responses API backend is not yet implemented")
}
