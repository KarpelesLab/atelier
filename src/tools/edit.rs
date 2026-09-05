//! The `edit` tool: exact-string replacement in a previously-read file.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolSpec};

pub struct EditTool;

impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: "Replace an exact string in a file with another. The file must have \
                been read earlier in this session (and not have changed on disk since). By \
                default the old string must appear exactly once; pass replace_all to replace \
                every occurrence."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file, relative to the project root (or absolute within it)."
                    },
                    "old_string": {
                        "type": "string",
                        "description": "The exact text to find and replace."
                    },
                    "new_string": {
                        "type": "string",
                        "description": "The text to replace it with."
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "Replace all occurrences instead of requiring exactly one (default false)."
                    }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        }
    }

    fn call(&self, ctx: &mut ToolCtx, args: Value) -> Result<String> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing required argument 'path'"))?;
        let old_string = args
            .get("old_string")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing required argument 'old_string'"))?;
        let new_string = args
            .get("new_string")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing required argument 'new_string'"))?;
        let replace_all = args
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let resolved = ctx.resolve(path)?;

        if !ctx.fstate.was_read(&resolved) {
            bail!("{path:?} has not been read in this session yet; read it before editing");
        }

        let current =
            std::fs::read_to_string(&resolved).with_context(|| format!("reading {path:?}"))?;

        if ctx.fstate.is_stale(&resolved, &current) {
            bail!("{path:?} has changed on disk since it was last read; re-read it before editing");
        }

        let count = current.matches(old_string).count();
        if count == 0 {
            bail!(
                "old_string not found in {path:?}. It must match the file's contents exactly \
                (including whitespace and indentation). Re-read the file to confirm the exact \
                text before retrying."
            );
        }
        if count > 1 && !replace_all {
            bail!(
                "old_string appears {count} times in {path:?}; it must be unique, or pass \
                replace_all: true to replace every occurrence."
            );
        }

        let updated = if replace_all {
            current.replace(old_string, new_string)
        } else {
            current.replacen(old_string, new_string, 1)
        };

        std::fs::write(&resolved, &updated).with_context(|| format!("writing {path:?}"))?;
        ctx.fstate.record(&resolved, &updated);

        let n = if replace_all { count } else { 1 };
        Ok(format!("replaced {n} occurrence(s) in {path}"))
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
    fn requires_read_first() {
        let root = tempdir("edit-unread");
        std::fs::write(root.join("f.txt"), "hello world").unwrap();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let err = EditTool
            .call(
                &mut ctx,
                json!({"path": "f.txt", "old_string": "hello", "new_string": "hi"}),
            )
            .unwrap_err();
        assert!(err.to_string().contains("has not been read"));
    }

    #[test]
    fn rejects_stale_read() {
        let root = tempdir("edit-stale");
        let file = root.join("f.txt");
        std::fs::write(&file, "hello world").unwrap();
        let mut fstate = FileState::new();
        fstate.record(&file, "hello world");
        // File changes on disk after it was read.
        std::fs::write(&file, "hello mars").unwrap();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let err = EditTool
            .call(
                &mut ctx,
                json!({"path": "f.txt", "old_string": "hello", "new_string": "hi"}),
            )
            .unwrap_err();
        assert!(err.to_string().contains("changed on disk"));
    }

    #[test]
    fn replaces_unique_occurrence() {
        let root = tempdir("edit-unique");
        let file = root.join("f.txt");
        std::fs::write(&file, "hello world").unwrap();
        let mut fstate = FileState::new();
        fstate.record(&file, "hello world");
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        EditTool
            .call(
                &mut ctx,
                json!({"path": "f.txt", "old_string": "world", "new_string": "mars"}),
            )
            .unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello mars");
    }

    #[test]
    fn requires_uniqueness_without_replace_all() {
        let root = tempdir("edit-dup");
        let file = root.join("f.txt");
        std::fs::write(&file, "aa aa").unwrap();
        let mut fstate = FileState::new();
        fstate.record(&file, "aa aa");
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let err = EditTool
            .call(
                &mut ctx,
                json!({"path": "f.txt", "old_string": "aa", "new_string": "bb"}),
            )
            .unwrap_err();
        assert!(err.to_string().contains("appears 2 times"));
    }

    #[test]
    fn replace_all_replaces_every_occurrence() {
        let root = tempdir("edit-replall");
        let file = root.join("f.txt");
        std::fs::write(&file, "aa aa aa").unwrap();
        let mut fstate = FileState::new();
        fstate.record(&file, "aa aa aa");
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        EditTool
            .call(
                &mut ctx,
                json!({"path": "f.txt", "old_string": "aa", "new_string": "bb", "replace_all": true}),
            )
            .unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "bb bb bb");
    }

    #[test]
    fn not_found_errors() {
        let root = tempdir("edit-notfound");
        let file = root.join("f.txt");
        std::fs::write(&file, "hello world").unwrap();
        let mut fstate = FileState::new();
        fstate.record(&file, "hello world");
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let err = EditTool
            .call(
                &mut ctx,
                json!({"path": "f.txt", "old_string": "nope", "new_string": "x"}),
            )
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }
}
