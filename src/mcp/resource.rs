//! [`Tool`] wrapper exposing an MCP server's `resources/read` capability as
//! one tool per server: `mcp__<server>__read_resource`.
//!
//! Unlike [`McpTool`](crate::mcp::tool::McpTool), which wraps one advertised
//! tool 1:1, this wraps a whole *capability* (resources) behind a single
//! tool that takes a `uri` argument, since resources aren't individually
//! callable the way tools are — a server can advertise arbitrarily many of
//! them via `resources/list`.

use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::mcp::jsonrpc::JsonRpc;
use crate::tools::{Tool, ToolCtx, ToolSpec};

/// Reads one MCP resource (`resources/read`) by URI, through the shared
/// connection to its server. `conn` is transport-agnostic (stdio or HTTP) —
/// see [`JsonRpc`].
pub(crate) struct McpResourceTool {
    conn: Arc<Mutex<dyn JsonRpc>>,
    /// Namespaced name exposed to the model: `mcp__<server>__read_resource`.
    namespaced_name: String,
    /// Describes the tool and enumerates the resources discovered via
    /// `resources/list` at connect time, so the model knows what URIs it can
    /// pass.
    description: String,
}

impl McpResourceTool {
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

impl Tool for McpResourceTool {
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
                    "uri": {
                        "type": "string",
                        "description": "The URI of the resource to read, as listed in this tool's description.",
                    }
                },
                "required": ["uri"],
            }),
        }
    }

    // `requires_approval` is intentionally left at the `Tool` trait default
    // (`true`): this makes a server call, same as any other MCP tool.

    fn call(&self, _ctx: &mut ToolCtx, args: Value) -> Result<String> {
        let Some(uri) = args.get("uri").and_then(Value::as_str) else {
            bail!("missing required `uri` argument");
        };

        let mut conn = match self.conn.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let result = conn.request("resources/read", json!({ "uri": uri }))?;

        let mut text = String::new();
        if let Some(parts) = result.get("contents").and_then(Value::as_array) {
            for part in parts {
                if let Some(t) = part.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(t);
                }
            }
        }

        Ok(text)
    }
}
