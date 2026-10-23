//! The `notebook_edit` tool: replace, insert, or delete a Jupyter notebook
//! cell.
//!
//! Like `edit`, it requires the notebook to have been read this session (with
//! `read`) and to be unchanged on disk since, so it never clobbers edits it
//! hasn't seen. The file is rewritten as Jupyter does — one-space indented
//! JSON, keys sorted — so diffs stay small.

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use super::{Tool, ToolCtx, ToolSpec, path_arg};

pub struct NotebookEditTool;

impl Tool for NotebookEditTool {
    fn name(&self) -> &str {
        "notebook_edit"
    }

    fn requires_approval(&self, _args: &Value) -> bool {
        // Confined to the project root by `ToolCtx::resolve`; auto-approved.
        false
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: "Edit a Jupyter notebook (.ipynb) cell: replace its source, insert \
                a new cell after it, or delete it. The notebook must have been read with \
                `read` first (its rendering shows each cell's index and id). Replacing a \
                code cell's source clears its now-stale outputs."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the .ipynb file, relative to the project root (or absolute within it)."
                    },
                    "cell": {
                        "type": "string",
                        "description": "The cell to act on: its id, or its 0-based index. For insert, the new cell goes after it; omit to insert at the top."
                    },
                    "new_source": {
                        "type": "string",
                        "description": "The cell's new source (replace, insert)."
                    },
                    "cell_type": {
                        "type": "string",
                        "enum": ["code", "markdown"],
                        "description": "Type of the new cell (insert; default code), or to change it to (replace)."
                    },
                    "edit_mode": {
                        "type": "string",
                        "enum": ["replace", "insert", "delete"],
                        "description": "What to do (default replace)."
                    }
                },
                "required": ["path"]
            }),
        }
    }

    fn call(&self, ctx: &mut ToolCtx, args: Value) -> Result<String> {
        let path = path_arg(&args)?;
        // Claude models know this parameter as `cell_id`.
        let cell = args
            .get("cell")
            .or_else(|| args.get("cell_id"))
            .and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            });
        let mode = args
            .get("edit_mode")
            .and_then(Value::as_str)
            .unwrap_or("replace");
        let new_source = args.get("new_source").and_then(Value::as_str);
        let cell_type = args.get("cell_type").and_then(Value::as_str);
        if let Some(t) = cell_type
            && t != "code"
            && t != "markdown"
        {
            bail!("cell_type must be \"code\" or \"markdown\", not {t:?}");
        }

        let resolved = ctx.resolve(path)?;
        let current = std::fs::read_to_string(&resolved)
            .map_err(|e| anyhow::anyhow!("failed to read {path:?}: {e}"))?;
        if !ctx.fstate.was_read(&resolved) {
            bail!("{path:?} has not been read yet in this session; read it first");
        }
        if ctx.fstate.is_stale(&resolved, &current) {
            bail!("{path:?} changed on disk since it was last read; read it again first");
        }

        let mut nb: Value =
            serde_json::from_str(&current).with_context(|| format!("parsing notebook {path:?}"))?;
        let minor = nb["nbformat_minor"].as_u64().unwrap_or(0);
        let cells = nb["cells"]
            .as_array_mut()
            .context("notebook has no `cells` array")?;
        let index = cell.as_deref().map(|c| find_cell(cells, c)).transpose()?;

        let summary = match mode {
            "replace" => {
                let i = index.context("replace needs `cell`")?;
                let src = new_source.context("replace needs `new_source`")?;
                let c = &mut cells[i];
                if let Some(t) = cell_type {
                    c["cell_type"] = json!(t);
                }
                c["source"] = source_lines(src);
                normalize_cell(c);
                format!("replaced cell {i}")
            }
            "insert" => {
                let src = new_source.context("insert needs `new_source`")?;
                let at = index.map_or(0, |i| i + 1);
                let mut c = json!({
                    "cell_type": cell_type.unwrap_or("code"),
                    "metadata": {},
                    "source": source_lines(src),
                });
                // Cell ids exist from nbformat 4.5 on.
                if minor >= 5 {
                    c["id"] = json!(&crate::claude::new_uuid()[..8]);
                }
                normalize_cell(&mut c);
                cells.insert(at, c);
                format!("inserted a cell at index {at}")
            }
            "delete" => {
                let i = index.context("delete needs `cell`")?;
                cells.remove(i);
                format!("deleted cell {i}")
            }
            other => bail!("edit_mode must be replace, insert, or delete, not {other:?}"),
        };

        let text = to_ipynb(&nb)?;
        std::fs::write(&resolved, &text)
            .map_err(|e| anyhow::anyhow!("failed to write {path:?}: {e}"))?;
        ctx.fstate.record(&resolved, &text);
        Ok(format!("{summary} in {path}"))
    }
}

/// Index of the cell named by `cell`: an id, or else a 0-based index.
fn find_cell(cells: &[Value], cell: &str) -> Result<usize> {
    if let Some(i) = cells.iter().position(|c| c["id"].as_str() == Some(cell)) {
        return Ok(i);
    }
    // Also accept the "cell-N" form some tools print.
    let n = cell.strip_prefix("cell-").unwrap_or(cell);
    match n.parse::<usize>() {
        Ok(i) if i < cells.len() => Ok(i),
        Ok(i) => bail!("cell index {i} out of range ({} cells)", cells.len()),
        Err(_) => bail!("no cell with id {cell:?}"),
    }
}

/// A code cell has outputs and an execution count — reset, since its source
/// changed; a markdown cell has neither.
fn normalize_cell(c: &mut Value) {
    let Some(obj) = c.as_object_mut() else {
        return;
    };
    if obj.get("cell_type").and_then(Value::as_str) == Some("code") {
        obj.insert("outputs".into(), json!([]));
        obj.insert("execution_count".into(), Value::Null);
    } else {
        obj.remove("outputs");
        obj.remove("execution_count");
    }
}

/// Source as Jupyter stores it: lines, each keeping its newline.
fn source_lines(src: &str) -> Value {
    Value::Array(
        src.split_inclusive('\n')
            .map(|l| Value::String(l.to_string()))
            .collect(),
    )
}

/// Serialize as Jupyter does: keys sorted, one-space indent, trailing newline.
fn to_ipynb(nb: &Value) -> Result<String> {
    fn sorted(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                let mut out = Map::new();
                for k in keys {
                    out.insert(k.clone(), sorted(&m[k]));
                }
                Value::Object(out)
            }
            Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
            v => v.clone(),
        }
    }
    let mut buf = Vec::new();
    let fmt = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
    serde::Serialize::serialize(&sorted(nb), &mut ser)?;
    buf.push(b'\n');
    Ok(String::from_utf8(buf)?)
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

    fn notebook() -> String {
        to_ipynb(&json!({
            "cells": [
                {"cell_type": "markdown", "id": "intro", "metadata": {}, "source": ["# Hi\n"]},
                {"cell_type": "code", "id": "calc", "metadata": {}, "execution_count": 3,
                 "outputs": [{"output_type": "stream", "name": "stdout", "text": ["2\n"]}],
                 "source": ["print(1 + 1)"]}
            ],
            "metadata": {}, "nbformat": 4, "nbformat_minor": 5
        }))
        .unwrap()
    }

    /// Write the notebook and mark it read; returns (root, fstate).
    fn setup(name: &str) -> (PathBuf, FileState) {
        let root = tempdir(name);
        let text = notebook();
        std::fs::write(root.join("n.ipynb"), &text).unwrap();
        let mut fstate = FileState::new();
        fstate.record(&root.join("n.ipynb"), &text);
        (root, fstate)
    }

    fn run(root: &PathBuf, fstate: &mut FileState, args: Value) -> Result<Value> {
        let mut ctx = ToolCtx {
            project_root: root,
            fstate,
            approved: false,
        };
        NotebookEditTool.call(&mut ctx, args)?;
        Ok(serde_json::from_str(&std::fs::read_to_string(root.join("n.ipynb")).unwrap()).unwrap())
    }

    #[test]
    fn replace_by_id_clears_outputs() {
        let (root, mut fs) = setup("nb-replace");
        let nb = run(
            &root,
            &mut fs,
            json!({"path": "n.ipynb", "cell": "calc", "new_source": "x = 1\nprint(x)"}),
        )
        .unwrap();
        let c = &nb["cells"][1];
        assert_eq!(c["source"], json!(["x = 1\n", "print(x)"]));
        assert_eq!(c["outputs"], json!([]));
        assert!(c["execution_count"].is_null());
    }

    #[test]
    fn insert_after_index_and_at_top() {
        let (root, mut fs) = setup("nb-insert");
        let nb = run(
            &root,
            &mut fs,
            json!({"path": "n.ipynb", "cell": "0", "edit_mode": "insert",
                   "cell_type": "markdown", "new_source": "middle"}),
        )
        .unwrap();
        assert_eq!(nb["cells"].as_array().unwrap().len(), 3);
        assert_eq!(nb["cells"][1]["source"], json!(["middle"]));
        assert_eq!(nb["cells"][1]["cell_type"], "markdown");
        assert!(nb["cells"][1]["id"].is_string());
        assert!(nb["cells"][1].get("outputs").is_none());

        let nb = run(
            &root,
            &mut fs,
            json!({"path": "n.ipynb", "edit_mode": "insert", "new_source": "import os"}),
        )
        .unwrap();
        assert_eq!(nb["cells"][0]["source"], json!(["import os"]));
        assert_eq!(nb["cells"][0]["outputs"], json!([]));
    }

    #[test]
    fn delete_with_claude_style_args() {
        let (root, mut fs) = setup("nb-delete");
        let nb = run(
            &root,
            &mut fs,
            json!({"notebook_path": "n.ipynb", "cell_id": "intro", "edit_mode": "delete"}),
        )
        .unwrap();
        assert_eq!(nb["cells"].as_array().unwrap().len(), 1);
        assert_eq!(nb["cells"][0]["id"], "calc");
    }

    #[test]
    fn requires_fresh_read() {
        let root = tempdir("nb-unread");
        std::fs::write(root.join("n.ipynb"), notebook()).unwrap();
        let mut fs = FileState::new();
        let args = json!({"path": "n.ipynb", "cell": "0", "new_source": "x"});
        assert!(run(&root, &mut fs, args.clone()).is_err());

        let (root, mut fs) = setup("nb-stale");
        std::fs::write(root.join("n.ipynb"), notebook().replace("Hi", "Changed")).unwrap();
        assert!(run(&root, &mut fs, args).is_err());
    }

    #[test]
    fn bad_cell_reference() {
        let (root, mut fs) = setup("nb-bad");
        let err = run(
            &root,
            &mut fs,
            json!({"path": "n.ipynb", "cell": "9", "new_source": "x"}),
        )
        .unwrap_err();
        assert!(err.to_string().contains("out of range"), "{err}");
        assert!(
            run(
                &root,
                &mut fs,
                json!({"path": "n.ipynb", "cell": "nope", "new_source": "x"})
            )
            .is_err()
        );
    }

    #[test]
    fn writes_jupyter_style_json() {
        let text = notebook();
        assert!(text.starts_with("{\n \"cells\": [\n"));
        assert!(text.ends_with("}\n"));
    }
}
