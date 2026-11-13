//! [`Tool`] wrapper around one tool advertised by an MCP server.
//!
//! Besides its name, description and schema, a server may *annotate* a tool
//! (`annotations` in `tools/list`): a human-readable title, and hints about
//! what calling it does. [`Annotations`] keeps the ones atelier has a use
//! for. They are the server's own claims about itself — good for telling
//! the user what a tool is, not a reason to trust it: nothing here lets a
//! tool skip approval because its server says it only reads.

use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::mcp::jsonrpc::JsonRpc;
use crate::tools::{Tool, ToolCtx, ToolSpec, spill};

/// What a server says about one of its tools beyond the schema. A hint the
/// server left out is `None`, which is not the same as it saying `false`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Annotations {
    /// A display name for people.
    pub title: Option<String>,
    /// `readOnlyHint`: the tool does not modify its environment.
    pub read_only: Option<bool>,
    /// `destructiveHint`: the tool may delete or overwrite, rather than
    /// only add. Meaningless on a read-only tool.
    pub destructive: Option<bool>,
}

impl Annotations {
    /// Read them off a raw `tools/list` entry. The title is taken from
    /// `annotations.title`, or from the tool's own `title` where the server
    /// follows the later revision of the protocol that moved it there.
    pub(crate) fn of(tool: &Value) -> Self {
        let annotations = &tool["annotations"];
        let title = annotations["title"].as_str().or(tool["title"].as_str());
        Self {
            title: title.map(str::to_string),
            read_only: annotations["readOnlyHint"].as_bool(),
            destructive: annotations["destructiveHint"].as_bool(),
        }
    }

    /// `[read-only]` or `[destructive]` for a listing, when the server
    /// said so. A tool said to be both is read-only: the destructive hint
    /// only qualifies tools that write.
    pub fn marker(&self) -> Option<&'static str> {
        match (self.read_only, self.destructive) {
            (Some(true), _) => Some("[read-only]"),
            (_, Some(true)) => Some("[destructive]"),
            _ => None,
        }
    }
}

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
    annotations: Annotations,
}

impl McpTool {
    pub(crate) fn new(
        conn: Arc<Mutex<dyn JsonRpc>>,
        namespaced_name: String,
        original_name: String,
        description: String,
        parameters: Value,
        annotations: Annotations,
    ) -> Self {
        Self {
            conn,
            namespaced_name,
            original_name,
            description,
            parameters,
            annotations,
        }
    }

    /// What the server said about this tool (see [`Annotations`]).
    pub(crate) fn annotations(&self) -> &Annotations {
        &self.annotations
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

    // The server's word is taken for it: the user chose to connect the
    // server, and the call still asks before it runs. Without the hint a
    // tool counts as modifying.
    fn mutates(&self, _args: &Value) -> bool {
        self.annotations.read_only != Some(true)
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
            Annotations::default(),
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
