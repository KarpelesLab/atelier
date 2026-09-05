//! The `ls` tool: list directory entries.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolSpec};

pub struct LsTool;

impl Tool for LsTool {
    fn name(&self) -> &str {
        "ls"
    }

    fn requires_approval(&self, _args: &serde_json::Value) -> bool {
        false
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: "List the entries of a directory (default: the project root). \
                Directories are shown with a trailing '/'."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory to list, relative to the project root (default: project root)."
                    }
                },
                "required": []
            }),
        }
    }

    fn call(&self, ctx: &mut ToolCtx, args: Value) -> Result<String> {
        let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let resolved = ctx.resolve(path)?;
        if !resolved.exists() {
            bail!("path {path:?} does not exist");
        }
        if !resolved.is_dir() {
            bail!("path {path:?} is not a directory");
        }

        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&resolved).with_context(|| format!("listing {path:?}"))? {
            let entry = entry.with_context(|| format!("reading entry in {path:?}"))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            entries.push(if is_dir { format!("{name}/") } else { name });
        }
        entries.sort();

        if entries.is_empty() {
            return Ok("(empty directory)".to_string());
        }
        Ok(entries.join("\n"))
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
    fn lists_entries_with_dir_marker() {
        let root = tempdir("ls-basic");
        std::fs::write(root.join("b.txt"), "").unwrap();
        std::fs::create_dir_all(root.join("a_dir")).unwrap();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let out = LsTool.call(&mut ctx, json!({})).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines, vec!["a_dir/", "b.txt"]);
    }

    #[test]
    fn empty_dir() {
        let root = tempdir("ls-empty");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let out = LsTool.call(&mut ctx, json!({})).unwrap();
        assert_eq!(out, "(empty directory)");
    }

    #[test]
    fn missing_path_errors() {
        let root = tempdir("ls-missing");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        assert!(LsTool.call(&mut ctx, json!({"path": "nope"})).is_err());
    }

    #[test]
    fn file_path_errors() {
        let root = tempdir("ls-file");
        std::fs::write(root.join("f.txt"), "x").unwrap();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        assert!(LsTool.call(&mut ctx, json!({"path": "f.txt"})).is_err());
    }

    #[test]
    fn path_escape_rejected() {
        let root = tempdir("ls-escape");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        assert!(LsTool.call(&mut ctx, json!({"path": "../"})).is_err());
    }
}
