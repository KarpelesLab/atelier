//! The `multiedit` tool: apply a sequence of exact-string edits to one
//! previously-read file, atomically.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolSpec};

pub struct MultiEditTool;

/// One edit in the sequence: same semantics as the `edit` tool's arguments.
struct EditOp<'a> {
    old_string: &'a str,
    new_string: &'a str,
    replace_all: bool,
}

/// Apply a single edit to `content`, enforcing the same rules as the `edit`
/// tool (old_string present; unique unless `replace_all`). Returns the
/// updated content and how many occurrences were replaced.
fn apply_edit(content: &str, op: &EditOp) -> Result<(String, usize)> {
    let count = content.matches(op.old_string).count();
    if count == 0 {
        bail!(
            "old_string not found. It must match the file's current contents exactly \
            (including whitespace and indentation) — remember earlier edits in this call \
            already changed the text."
        );
    }
    if count > 1 && !op.replace_all {
        bail!(
            "old_string appears {count} times; it must be unique, or pass replace_all: true \
            to replace every occurrence."
        );
    }

    let updated = if op.replace_all {
        content.replace(op.old_string, op.new_string)
    } else {
        content.replacen(op.old_string, op.new_string, 1)
    };
    let n = if op.replace_all { count } else { 1 };
    Ok((updated, n))
}

impl Tool for MultiEditTool {
    fn name(&self) -> &str {
        "multiedit"
    }

    fn requires_approval(&self, _args: &serde_json::Value) -> bool {
        // Confined to the project root by `ToolCtx::resolve`; auto-approved.
        false
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: "Apply several exact-string edits to a single file in one call, \
                instead of calling `edit` repeatedly — fewer round-trips when a file needs \
                multiple changes. The file must have been read earlier in this session (and \
                not have changed on disk since). Edits are applied in order, each seeing the \
                result of the ones before it. The whole call is atomic: if any edit fails \
                (its old_string is missing, or not unique and replace_all wasn't set), no \
                edits are applied and the file is left untouched. By default each edit's \
                old_string must appear exactly once at the time it's applied; pass \
                replace_all on an edit to replace every occurrence of its old_string instead."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file, relative to the project root (or absolute within it)."
                    },
                    "edits": {
                        "type": "array",
                        "description": "The edits to apply, in order.",
                        "items": {
                            "type": "object",
                            "properties": {
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
                            "required": ["old_string", "new_string"]
                        },
                        "minItems": 1
                    }
                },
                "required": ["path", "edits"]
            }),
        }
    }

    fn call(&self, ctx: &mut ToolCtx, args: Value) -> Result<String> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing required argument 'path'"))?;
        let edits = args
            .get("edits")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("missing required argument 'edits'"))?;

        if edits.is_empty() {
            bail!("'edits' must contain at least one edit");
        }

        // Parse all edits up front, so a malformed edit is reported before we
        // touch the file at all.
        let mut ops = Vec::with_capacity(edits.len());
        for (i, edit) in edits.iter().enumerate() {
            let old_string = edit
                .get("old_string")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    anyhow::anyhow!("edit {} is missing required field 'old_string'", i + 1)
                })?;
            let new_string = edit
                .get("new_string")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    anyhow::anyhow!("edit {} is missing required field 'new_string'", i + 1)
                })?;
            let replace_all = edit
                .get("replace_all")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            ops.push(EditOp {
                old_string,
                new_string,
                replace_all,
            });
        }

        let resolved = ctx.resolve(path)?;

        if !ctx.fstate.was_read(&resolved) {
            bail!("{path:?} has not been read in this session yet; read it before editing");
        }

        let original =
            std::fs::read_to_string(&resolved).with_context(|| format!("reading {path:?}"))?;

        if ctx.fstate.is_stale(&resolved, &original) {
            bail!("{path:?} has changed on disk since it was last read; re-read it before editing");
        }

        // Apply edits sequentially to an in-memory copy; abort without writing
        // if any of them fails.
        let mut current = original;
        for (i, op) in ops.iter().enumerate() {
            match apply_edit(&current, op) {
                Ok((updated, _)) => current = updated,
                Err(e) => {
                    bail!("edit {} of {} failed for {path:?}: {e}", i + 1, ops.len());
                }
            }
        }

        std::fs::write(&resolved, &current).with_context(|| format!("writing {path:?}"))?;
        ctx.fstate.record(&resolved, &current);

        Ok(format!("applied {} edit(s) to {path}", ops.len()))
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
        let root = tempdir("multiedit-unread");
        std::fs::write(root.join("f.txt"), "hello world").unwrap();
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let err = MultiEditTool
            .call(
                &mut ctx,
                json!({"path": "f.txt", "edits": [{"old_string": "hello", "new_string": "hi"}]}),
            )
            .unwrap_err();
        assert!(err.to_string().contains("has not been read"));
    }

    #[test]
    fn rejects_stale_read() {
        let root = tempdir("multiedit-stale");
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
        let err = MultiEditTool
            .call(
                &mut ctx,
                json!({"path": "f.txt", "edits": [{"old_string": "hello", "new_string": "hi"}]}),
            )
            .unwrap_err();
        assert!(err.to_string().contains("changed on disk"));
    }

    #[test]
    fn empty_edits_errors() {
        let root = tempdir("multiedit-empty");
        let file = root.join("f.txt");
        std::fs::write(&file, "hello world").unwrap();
        let mut fstate = FileState::new();
        fstate.record(&file, "hello world");
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let err = MultiEditTool
            .call(&mut ctx, json!({"path": "f.txt", "edits": []}))
            .unwrap_err();
        assert!(err.to_string().contains("at least one edit"));
        // File untouched.
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello world");
    }

    #[test]
    fn applies_multiple_edits_in_order() {
        let root = tempdir("multiedit-order");
        let file = root.join("f.txt");
        std::fs::write(&file, "hello world").unwrap();
        let mut fstate = FileState::new();
        fstate.record(&file, "hello world");
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let result = MultiEditTool
            .call(
                &mut ctx,
                json!({"path": "f.txt", "edits": [
                    {"old_string": "hello", "new_string": "hi"},
                    {"old_string": "world", "new_string": "mars"}
                ]}),
            )
            .unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hi mars");
        assert!(result.contains("applied 2 edit(s)"));
    }

    #[test]
    fn sequential_edit_matches_prior_result() {
        // Edit 2's old_string only exists after edit 1 has run.
        let root = tempdir("multiedit-sequential");
        let file = root.join("f.txt");
        std::fs::write(&file, "foo bar").unwrap();
        let mut fstate = FileState::new();
        fstate.record(&file, "foo bar");
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        MultiEditTool
            .call(
                &mut ctx,
                json!({"path": "f.txt", "edits": [
                    {"old_string": "foo", "new_string": "foobaz"},
                    {"old_string": "foobaz bar", "new_string": "done"}
                ]}),
            )
            .unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "done");
    }

    #[test]
    fn atomic_failure_leaves_file_unchanged() {
        let root = tempdir("multiedit-atomic");
        let file = root.join("f.txt");
        std::fs::write(&file, "hello world").unwrap();
        let mut fstate = FileState::new();
        fstate.record(&file, "hello world");
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let err = MultiEditTool
            .call(
                &mut ctx,
                json!({"path": "f.txt", "edits": [
                    {"old_string": "hello", "new_string": "hi"},
                    {"old_string": "nonexistent", "new_string": "x"},
                    {"old_string": "world", "new_string": "mars"}
                ]}),
            )
            .unwrap_err();
        assert!(err.to_string().contains("edit 2 of 3 failed"));
        // File must be completely unchanged (edit 1 was not persisted either).
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello world");
    }

    #[test]
    fn replace_all_within_one_edit() {
        let root = tempdir("multiedit-replall");
        let file = root.join("f.txt");
        std::fs::write(&file, "aa aa aa").unwrap();
        let mut fstate = FileState::new();
        fstate.record(&file, "aa aa aa");
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        MultiEditTool
            .call(
                &mut ctx,
                json!({"path": "f.txt", "edits": [
                    {"old_string": "aa", "new_string": "bb", "replace_all": true}
                ]}),
            )
            .unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "bb bb bb");
    }

    #[test]
    fn requires_uniqueness_without_replace_all() {
        let root = tempdir("multiedit-dup");
        let file = root.join("f.txt");
        std::fs::write(&file, "aa aa").unwrap();
        let mut fstate = FileState::new();
        fstate.record(&file, "aa aa");
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let err = MultiEditTool
            .call(
                &mut ctx,
                json!({"path": "f.txt", "edits": [{"old_string": "aa", "new_string": "bb"}]}),
            )
            .unwrap_err();
        assert!(err.to_string().contains("appears 2 times"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "aa aa");
    }
}
