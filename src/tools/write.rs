//! The `write` tool: create or overwrite a file with new contents.

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolSpec};

pub struct WriteTool;

impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn requires_approval(&self) -> bool {
        // Confined to the project root by `ToolCtx::resolve`; auto-approved.
        false
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: "Write content to a file, creating it (and any parent directories) \
                if needed, or overwriting it if it already exists."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file, relative to the project root (or absolute within it)."
                    },
                    "content": {
                        "type": "string",
                        "description": "The full contents to write to the file."
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }

    fn call(&self, ctx: &mut ToolCtx, args: Value) -> Result<String> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing required argument 'path'"))?;
        let content = args
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing required argument 'content'"))?;

        let resolved = ctx.resolve(path)?;
        if let Some(parent) = resolved.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating parent directories for {path:?}"))?;
        }
        std::fs::write(&resolved, content).with_context(|| format!("writing to {path:?}"))?;

        ctx.fstate.record(&resolved, content);

        Ok(format!("wrote {} bytes to {}", content.len(), path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::FileState;
    use std::path::PathBuf;

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "atelier-test-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writes_new_file() {
        let root = tempdir("write-new");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let out = WriteTool
            .call(&mut ctx, json!({"path": "a/b/f.txt", "content": "hello"}))
            .unwrap();
        assert!(out.contains("5 bytes"));
        assert_eq!(
            std::fs::read_to_string(root.join("a/b/f.txt")).unwrap(),
            "hello"
        );
        assert!(fstate.was_read(&root.join("a/b/f.txt")));
    }

    #[test]
    fn overwrites_existing_file() {
        let root = tempdir("write-overwrite");
        std::fs::write(root.join("f.txt"), "old").unwrap();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        WriteTool
            .call(&mut ctx, json!({"path": "f.txt", "content": "new"}))
            .unwrap();
        assert_eq!(std::fs::read_to_string(root.join("f.txt")).unwrap(), "new");
    }

    #[test]
    fn path_escape_rejected() {
        let root = tempdir("write-escape");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        assert!(
            WriteTool
                .call(&mut ctx, json!({"path": "../evil.txt", "content": "x"}))
                .is_err()
        );
    }
}
