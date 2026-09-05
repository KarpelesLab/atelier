//! The `read` tool: read a file's contents with `cat -n` style line numbers.

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolSpec};

pub struct ReadTool;

impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: "Read a file from the project, returning its contents with 1-based \
                line numbers. Supports reading a slice via offset/limit for large files."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file, relative to the project root (or absolute within it)."
                    },
                    "offset": {
                        "type": "integer",
                        "description": "1-based line number to start reading from (default 1)."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of lines to return (default: all)."
                    }
                },
                "required": ["path"]
            }),
        }
    }

    fn call(&self, ctx: &mut ToolCtx, args: Value) -> Result<String> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing required argument 'path'"))?;
        let offset = args
            .get("offset")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1) as usize;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|v| v as usize);

        let resolved = ctx.resolve(path)?;
        if resolved.is_dir() {
            bail!("{path:?} is a directory, not a file");
        }
        let contents = std::fs::read_to_string(&resolved)
            .map_err(|e| anyhow::anyhow!("failed to read {path:?}: {e}"))?;

        // Record the FULL contents so Edit can detect staleness correctly.
        ctx.fstate.record(&resolved, &contents);

        let lines: Vec<&str> = contents.lines().collect();
        if lines.is_empty() {
            return Ok(String::new());
        }
        let start = offset.min(lines.len() + 1);
        let end = match limit {
            Some(l) => (start + l - 1).min(lines.len()),
            None => lines.len(),
        };

        let mut out = String::new();
        for (i, line) in lines
            .iter()
            .enumerate()
            .skip(start.saturating_sub(1))
            .take(end.saturating_sub(start.saturating_sub(1)))
        {
            out.push_str(&format!("{}\t{}\n", i + 1, line));
        }
        Ok(out)
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
    fn reads_with_line_numbers() {
        let root = tempdir("read-basic");
        std::fs::write(root.join("f.txt"), "a\nb\nc\n").unwrap();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let out = ReadTool.call(&mut ctx, json!({"path": "f.txt"})).unwrap();
        assert_eq!(out, "1\ta\n2\tb\n3\tc\n");
        assert!(fstate.was_read(&root.join("f.txt")));
    }

    #[test]
    fn offset_and_limit() {
        let root = tempdir("read-offset");
        std::fs::write(root.join("f.txt"), "a\nb\nc\nd\n").unwrap();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let out = ReadTool
            .call(&mut ctx, json!({"path": "f.txt", "offset": 2, "limit": 2}))
            .unwrap();
        assert_eq!(out, "2\tb\n3\tc\n");
    }

    #[test]
    fn missing_file_errors() {
        let root = tempdir("read-missing");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        assert!(
            ReadTool
                .call(&mut ctx, json!({"path": "nope.txt"}))
                .is_err()
        );
    }

    #[test]
    fn directory_errors() {
        let root = tempdir("read-dir");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        assert!(ReadTool.call(&mut ctx, json!({"path": "sub"})).is_err());
    }

    #[test]
    fn path_escape_rejected() {
        let root = tempdir("read-escape");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        assert!(
            ReadTool
                .call(&mut ctx, json!({"path": "../etc/passwd"}))
                .is_err()
        );
    }
}
