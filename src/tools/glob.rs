//! The `glob` tool: list files matching a glob pattern, respecting .gitignore.

use anyhow::{Context, Result};
use globset::Glob;
use ignore::WalkBuilder;
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolSpec};

const MAX_RESULTS: usize = 200;

pub struct GlobTool;

impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn requires_approval(&self, _args: &serde_json::Value) -> bool {
        false
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: "List files matching a glob pattern (e.g. 'src/**/*.rs'), respecting \
                .gitignore. Returns sorted relative paths, capped at 200 results."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern to match, relative to the project root (e.g. '**/*.rs')."
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory to search under, relative to the project root (default: project root)."
                    }
                },
                "required": ["pattern"]
            }),
        }
    }

    fn call(&self, ctx: &mut ToolCtx, args: Value) -> Result<String> {
        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing required argument 'pattern'"))?;
        let path = args.get("path").and_then(Value::as_str).unwrap_or(".");

        let matcher = Glob::new(pattern)
            .with_context(|| format!("invalid glob {pattern:?}"))?
            .compile_matcher();

        let search_root = ctx.resolve(path)?;
        if !search_root.exists() {
            anyhow::bail!("path {path:?} does not exist");
        }

        let mut results = Vec::new();
        let mut truncated = false;

        for entry in WalkBuilder::new(&search_root).hidden(false).build() {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let file_path = entry.path();
            let rel = file_path
                .strip_prefix(ctx.project_root)
                .unwrap_or(file_path);
            if matcher.is_match(rel) || matcher.is_match(file_path.file_name().unwrap_or_default())
            {
                results.push(rel.display().to_string());
                if results.len() >= MAX_RESULTS {
                    truncated = true;
                    break;
                }
            }
        }

        results.sort();

        if results.is_empty() {
            return Ok("no files matched".to_string());
        }
        let mut out = results.join("\n");
        if truncated {
            out.push_str(&format!("\n[truncated at {MAX_RESULTS} results]"));
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
    fn matches_by_extension() {
        let root = tempdir("glob-ext");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "").unwrap();
        std::fs::write(root.join("b.txt"), "").unwrap();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let out = GlobTool
            .call(&mut ctx, json!({"pattern": "**/*.rs"}))
            .unwrap();
        assert!(out.contains("src/a.rs"));
        assert!(!out.contains("b.txt"));
    }

    #[test]
    fn no_matches() {
        let root = tempdir("glob-none");
        std::fs::write(root.join("b.txt"), "").unwrap();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let out = GlobTool.call(&mut ctx, json!({"pattern": "*.rs"})).unwrap();
        assert_eq!(out, "no files matched");
    }

    #[test]
    fn results_sorted() {
        let root = tempdir("glob-sorted");
        std::fs::write(root.join("z.rs"), "").unwrap();
        std::fs::write(root.join("a.rs"), "").unwrap();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let out = GlobTool.call(&mut ctx, json!({"pattern": "*.rs"})).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines, vec!["a.rs", "z.rs"]);
    }
}
