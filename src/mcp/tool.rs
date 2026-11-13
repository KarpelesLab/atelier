//! [`Tool`] wrapper around one tool advertised by an MCP server.

use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::mcp::jsonrpc::JsonRpc;
use crate::tools::{Tool, ToolCtx, ToolSpec, spill};

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

    fn call(&self, ctx: &mut ToolCtx, args: Value) -> Result<String> {
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

        // A server can answer with any amount of text; past the budget the
        // model gets the head and the tail, and the rest is in a file.
        let text = spill::fit(ctx.project_root, text, spill::BUDGET);

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::FileState;

    /// A server whose every `tools/call` answers with the same result.
    struct Canned(Value);

    impl JsonRpc for Canned {
        fn request(&mut self, _method: &str, _params: Value) -> Result<Value> {
            Ok(self.0.clone())
        }
        fn notify(&mut self, _method: &str, _params: Value) -> Result<()> {
            Ok(())
        }
    }

    fn call(root: &std::path::Path, result: Value) -> Result<String> {
        let tool = McpTool::new(
            Arc::new(Mutex::new(Canned(result))),
            "mcp__srv__dump".into(),
            "dump".into(),
            String::new(),
            json!({}),
        );
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: root,
            fstate: &mut fstate,
            approved: true,
        };
        tool.call(&mut ctx, json!({}))
    }

    #[test]
    fn a_result_past_the_budget_is_saved_and_cut_to_head_and_tail() {
        let root = std::env::temp_dir().join(format!("atelier-mcp-spill-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let small = call(
            &root,
            json!({"content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]}),
        );
        assert_eq!(small.unwrap(), "a\nb");
        assert!(!root.join(".atelier").exists());

        let big: String = (1..=10000).map(|i| format!("row {i}\n")).collect();
        let out = call(&root, json!({"content": [{"type": "text", "text": big}]})).unwrap();
        assert!(out.len() < spill::BUDGET, "{} bytes", out.len());
        assert!(out.starts_with("row 1\nrow 2\n") && out.ends_with("row 10000\n"));
        let (_, rest) = out.split_once("full output: ").expect("a marker");
        let file = root.join(rest.split_once(' ').unwrap().0);
        assert_eq!(std::fs::read_to_string(file).unwrap(), big);

        // An error result is bounded the same way.
        let err = call(
            &root,
            json!({"isError": true, "content": [{"type": "text", "text": big}]}),
        )
        .unwrap_err()
        .to_string();
        assert!(err.len() < spill::BUDGET + 100 && err.contains("full output: "));
        let _ = std::fs::remove_dir_all(&root);
    }
}
