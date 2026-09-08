//! [`Tool`] wrapper exposing an MCP server's `prompts/get` capability as one
//! tool per server: `mcp__<server>__get_prompt`.
//!
//! Like [`McpResourceTool`](crate::mcp::resource::McpResourceTool), this
//! wraps a whole *capability* (prompts) behind a single tool that takes a
//! `name` (plus optional `arguments`), since prompts aren't individually
//! callable the way tools are — a server can advertise arbitrarily many of
//! them via `prompts/list`.

use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::mcp::jsonrpc::JsonRpc;
use crate::tools::{Tool, ToolCtx, ToolSpec};

/// Fetches and renders one MCP prompt (`prompts/get`) by name, through the
/// shared connection to its server. `conn` is transport-agnostic (stdio or
/// HTTP) — see [`JsonRpc`].
pub(crate) struct McpPromptTool {
    conn: Arc<Mutex<dyn JsonRpc>>,
    /// Namespaced name exposed to the model: `mcp__<server>__get_prompt`.
    namespaced_name: String,
    /// Describes the tool and enumerates the prompts discovered via
    /// `prompts/list` at connect time, so the model knows what names (and
    /// arguments) it can pass.
    description: String,
}

impl McpPromptTool {
    pub(crate) fn new(
        conn: Arc<Mutex<dyn JsonRpc>>,
        namespaced_name: String,
        description: String,
    ) -> Self {
        Self {
            conn,
            namespaced_name,
            description,
        }
    }
}

impl Tool for McpPromptTool {
    fn name(&self) -> &str {
        &self.namespaced_name
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.namespaced_name.clone(),
            description: self.description.clone(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The name of the prompt to fetch, as listed in this tool's description.",
                    },
                    "arguments": {
                        "type": "object",
                        "description": "Named arguments the prompt template expects, as listed in this tool's description.",
                    },
                },
                "required": ["name"],
            }),
        }
    }

    // `requires_approval` is intentionally left at the `Tool` trait default
    // (`true`): this makes a server call, same as any other MCP tool.

    fn call(&self, _ctx: &mut ToolCtx, args: Value) -> Result<String> {
        let Some(name) = args.get("name").and_then(Value::as_str) else {
            bail!("missing required `name` argument");
        };
        let arguments = args.get("arguments").cloned().unwrap_or_else(|| json!({}));

        let mut conn = match self.conn.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let result = conn.request(
            "prompts/get",
            json!({
                "name": name,
                "arguments": arguments,
            }),
        )?;

        let messages = result
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        Ok(render_messages(&messages))
    }
}

/// Render a `prompts/get` `messages` array into a readable transcript: one
/// `role: text` line per message, concatenating the text parts of that
/// message's content (which the spec allows to be either a single content
/// object or an array of them).
fn render_messages(messages: &[Value]) -> String {
    let mut lines = Vec::with_capacity(messages.len());
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");

        let mut text = String::new();
        match message.get("content") {
            Some(Value::Array(parts)) => {
                for part in parts {
                    push_text_part(&mut text, part);
                }
            }
            Some(part @ Value::Object(_)) => push_text_part(&mut text, part),
            _ => {}
        }

        lines.push(format!("{role}: {text}"));
    }
    lines.join("\n")
}

/// Append `part`'s text content (if it is a `{"type":"text","text":...}`
/// content part, or at least carries a `text` field) to `out`.
fn push_text_part(out: &mut String, part: &Value) {
    if let Some(t) = part.get("text").and_then(Value::as_str) {
        out.push_str(t);
    }
}
