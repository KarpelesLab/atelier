//! [`Tool`] wrapper around one tool advertised by an MCP server.

use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::mcp::jsonrpc::JsonRpc;
use crate::tools::{Tool, ToolCtx, ToolSpec};

/// A single MCP-advertised tool, callable through the shared connection to
/// its server. `conn` is transport-agnostic (stdio or HTTP) — see
/// [`JsonRpc`].
pub(crate) struct McpTool {
    conn: Arc<Mutex<dyn JsonRpc>>,
    /// Namespaced name exposed to the model: `mcp__<server>__<tool>`.
    namespaced_name: String,
    /// The tool's own name, as advertised by the server — used on the wire
    /// for `tools/call` (the server doesn't know about our namespacing).
    original_name: String,
    description: String,
    parameters: Value,
}

impl McpTool {
    pub(crate) fn new(
        conn: Arc<Mutex<dyn JsonRpc>>,
        namespaced_name: String,
        original_name: String,
        description: String,
        parameters: Value,
    ) -> Self {
        Self {
            conn,
            namespaced_name,
            original_name,
            description,
            parameters,
        }
    }
}

impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.namespaced_name
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.namespaced_name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }

    fn call(&self, _ctx: &mut ToolCtx, args: Value) -> Result<String> {
        let mut conn = match self.conn.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let result = conn.request(
            "tools/call",
            json!({
                "name": self.original_name,
                "arguments": args,
            }),
        )?;

        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let mut text = String::new();
        if let Some(parts) = result.get("content").and_then(Value::as_array) {
            for part in parts {
                if part.get("type").and_then(Value::as_str) == Some("text")
                    && let Some(t) = part.get("text").and_then(Value::as_str)
                {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(t);
                }
            }
        }

        if is_error {
            bail!(
                "MCP tool `{}` returned an error: {}",
                self.original_name,
                if text.is_empty() {
                    "(no message)"
                } else {
                    text.as_str()
                }
            );
        }

        Ok(text)
    }
}
