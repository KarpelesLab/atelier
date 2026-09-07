//! The `tree` tool: print an indented directory tree, respecting `.gitignore`.

use anyhow::{Result, bail};
use ignore::WalkBuilder;
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolSpec};

/// Cap on the number of entries printed, so a huge tree doesn't blow up the
/// context window.
const MAX_ENTRIES: usize = 200;

/// Default depth (levels below the root) when `depth` is not given.
const DEFAULT_DEPTH: usize = 3;

pub struct TreeTool;

impl Tool for TreeTool {
    fn name(&self) -> &str {
        "tree"
    }

    fn requires_approval(&self, _args: &Value) -> bool {
        false
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: "Print an indented directory tree under a path (default: the \
                project root), respecting .gitignore. Directories are shown with a \
                trailing '/'. Capped at a maximum depth and a maximum number of entries."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory to root the tree at, relative to the project root (default: project root)."
                    },
                    "depth": {
                        "type": "integer",
                        "description": "Maximum number of levels below the root to descend (default: 3)."
                    }
                },
                "required": []
            }),
        }
    }

    fn call(&self, ctx: &mut ToolCtx, args: Value) -> Result<String> {
        let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let depth = args
            .get("depth")
            .and_then(Value::as_u64)
            .map(|d| d as usize)
            .unwrap_or(DEFAULT_DEPTH);

        let root = ctx.resolve(path)?;
        if !root.exists() {
            bail!("path {path:?} does not exist");
        }
        if !root.is_dir() {
            bail!("path {path:?} is not a directory");
        }

        let mut lines = Vec::new();
        lines.push(format!("{path}/"));

        let mut count = 0usize;
        let mut truncated = false;

        let walker = WalkBuilder::new(&root)
            .hidden(false)
            .max_depth(Some(depth))
            .sort_by_file_name(|a, b| a.cmp(b))
            // Respect a .gitignore file even when the tree isn't rooted in an
            // actual git repository (e.g. under a project subdirectory).
            .require_git(false)
            .build();

        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            // Skip the root itself; it's already printed above.
            if entry.depth() == 0 {
                continue;
            }
            if count >= MAX_ENTRIES {
                truncated = true;
                break;
            }
            let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
            let name = entry.file_name().to_string_lossy();
            let indent = "  ".repeat(entry.depth() - 1);
            if is_dir {
                lines.push(format!("{indent}{name}/"));
            } else {
                lines.push(format!("{indent}{name}"));
            }
            count += 1;
        }

        if truncated {
            lines.push(format!("[truncated at {MAX_ENTRIES} entries]"));
        }

        Ok(lines.join("\n"))
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
    fn builds_indented_tree() {
        let root = tempdir("tree-basic");
        std::fs::create_dir_all(root.join("a_dir")).unwrap();
        std::fs::write(root.join("a_dir/nested.txt"), "").unwrap();
        std::fs::write(root.join("b.txt"), "").unwrap();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let out = TreeTool.call(&mut ctx, json!({})).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines,
            vec!["./", "a_dir/", "  nested.txt", "b.txt"],
            "unexpected tree output: {out}"
        );
    }

    #[test]
    fn respects_depth_limit() {
        let root = tempdir("tree-depth");
        std::fs::create_dir_all(root.join("a/b/c")).unwrap();
        std::fs::write(root.join("a/b/c/deep.txt"), "").unwrap();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let out = TreeTool.call(&mut ctx, json!({"depth": 1})).unwrap();
        assert!(out.contains("a/"));
        assert!(!out.contains("b/"));
        assert!(!out.contains("deep.txt"));
    }

    #[test]
    fn respects_gitignore() {
        let root = tempdir("tree-gitignore");
        std::fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(root.join("ignored.txt"), "").unwrap();
        std::fs::write(root.join("kept.txt"), "").unwrap();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let out = TreeTool.call(&mut ctx, json!({})).unwrap();
        assert!(out.contains("kept.txt"));
        assert!(!out.contains("ignored.txt"));
    }

    #[test]
    fn missing_path_errors() {
        let root = tempdir("tree-missing");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        assert!(TreeTool.call(&mut ctx, json!({"path": "nope"})).is_err());
    }

    #[test]
    fn file_path_errors() {
        let root = tempdir("tree-file");
        std::fs::write(root.join("f.txt"), "x").unwrap();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        assert!(TreeTool.call(&mut ctx, json!({"path": "f.txt"})).is_err());
    }

    #[test]
    fn path_escape_rejected() {
        let root = tempdir("tree-escape");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        assert!(TreeTool.call(&mut ctx, json!({"path": "../"})).is_err());
    }
}
