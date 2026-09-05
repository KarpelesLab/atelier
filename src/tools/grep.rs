//! The `grep` tool: regex search over project files, respecting .gitignore.

use anyhow::{Context, Result};
use globset::{Glob, GlobMatcher};
use ignore::WalkBuilder;
use regex::Regex;
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolSpec};

const MAX_MATCHES: usize = 100;

pub struct GrepTool;

impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn side_effecting(&self) -> bool {
        false
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: "Search file contents for a regular expression, respecting \
                .gitignore. Returns matches as path:line:text, capped at 100 results."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regular expression to search for."
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory to search under, relative to the project root (default: project root)."
                    },
                    "glob": {
                        "type": "string",
                        "description": "Optional glob to filter which filenames are searched (e.g. '*.rs')."
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
        let glob = args.get("glob").and_then(Value::as_str);

        let re = Regex::new(pattern).with_context(|| format!("invalid regex {pattern:?}"))?;
        let matcher: Option<GlobMatcher> = match glob {
            Some(g) => Some(
                Glob::new(g)
                    .with_context(|| format!("invalid glob {g:?}"))?
                    .compile_matcher(),
            ),
            None => None,
        };

        let search_root = ctx.resolve(path)?;
        if !search_root.exists() {
            anyhow::bail!("path {path:?} does not exist");
        }

        let mut matches = Vec::new();
        let mut truncated = false;

        'walk: for entry in WalkBuilder::new(&search_root).hidden(false).build() {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let file_path = entry.path();
            if let Some(m) = &matcher
                && let Some(name) = file_path.file_name()
                && !m.is_match(name)
            {
                continue;
            }
            let Ok(contents) = std::fs::read_to_string(file_path) else {
                continue; // skip binary/unreadable files
            };
            let rel = file_path
                .strip_prefix(ctx.project_root)
                .unwrap_or(file_path)
                .display();
            for (i, line) in contents.lines().enumerate() {
                if re.is_match(line) {
                    matches.push(format!("{rel}:{}:{line}", i + 1));
                    if matches.len() >= MAX_MATCHES {
                        truncated = true;
                        break 'walk;
                    }
                }
            }
        }

        if matches.is_empty() {
            return Ok("no matches found".to_string());
        }
        let mut out = matches.join("\n");
        if truncated {
            out.push_str(&format!("\n[truncated at {MAX_MATCHES} matches]"));
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
    fn finds_matches() {
        let root = tempdir("grep-basic");
        std::fs::write(root.join("a.txt"), "hello\nworld\nhello again\n").unwrap();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let out = GrepTool
            .call(&mut ctx, json!({"pattern": "hello"}))
            .unwrap();
        assert!(out.contains("a.txt:1:hello"));
        assert!(out.contains("a.txt:3:hello again"));
        assert!(!out.contains("world"));
    }

    #[test]
    fn filters_by_glob() {
        let root = tempdir("grep-glob");
        std::fs::write(root.join("a.txt"), "needle\n").unwrap();
        std::fs::write(root.join("b.rs"), "needle\n").unwrap();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let out = GrepTool
            .call(&mut ctx, json!({"pattern": "needle", "glob": "*.rs"}))
            .unwrap();
        assert!(out.contains("b.rs"));
        assert!(!out.contains("a.txt"));
    }

    #[test]
    fn no_matches() {
        let root = tempdir("grep-none");
        std::fs::write(root.join("a.txt"), "hello\n").unwrap();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let out = GrepTool.call(&mut ctx, json!({"pattern": "zzz"})).unwrap();
        assert_eq!(out, "no matches found");
    }
}
