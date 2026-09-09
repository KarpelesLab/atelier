//! The `todo` tool: a durable task checklist stored in the project.
//!
//! Persists to `<project_root>/.atelier/todos.json` so the agent can track
//! multi-step work across turns (and across context compaction). The file
//! path is a fixed internal location under the project root — not a
//! user-supplied path — so it is joined directly rather than passed through
//! [`super::confine`].

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use super::{Tool, ToolCtx, ToolSpec};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct TodoItem {
    text: String,
    done: bool,
}

pub struct TodoTool;

impl TodoTool {
    fn todos_path(project_root: &Path) -> PathBuf {
        project_root.join(".atelier").join("todos.json")
    }

    /// Load the list, treating a missing or corrupt file as empty.
    fn load(path: &Path) -> Vec<TodoItem> {
        let Ok(raw) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        serde_json::from_str(&raw).unwrap_or_default()
    }

    fn save(path: &Path, items: &[TodoItem]) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let raw = serde_json::to_string_pretty(items)?;
        std::fs::write(path, raw)?;
        Ok(())
    }

    fn render(items: &[TodoItem]) -> String {
        if items.is_empty() {
            return "(todo list is empty)".to_string();
        }
        let mut out = String::new();
        let mut done_count = 0;
        for (i, item) in items.iter().enumerate() {
            if item.done {
                done_count += 1;
            }
            out.push_str(&format!(
                "[{}] {}. {}\n",
                if item.done { "x" } else { " " },
                i + 1,
                item.text
            ));
        }
        out.push_str(&format!(
            "\n{done_count}/{} done, {} pending",
            items.len(),
            items.len() - done_count
        ));
        out
    }
}

impl Tool for TodoTool {
    fn name(&self) -> &str {
        "todo"
    }

    fn requires_approval(&self, _args: &Value) -> bool {
        // Only reads/writes .atelier/todos.json inside the project root.
        false
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: "Maintain a durable, numbered task checklist for the current project, \
                persisted to .atelier/todos.json so it survives across turns and context \
                compaction. Use it to plan and track progress on multi-step work. Actions:\n\
                - {\"action\": \"list\"}: show the current checklist with status and a count summary.\n\
                - {\"action\": \"add\", \"items\": [\"...\"]}: append one or more new pending items.\n\
                - {\"action\": \"complete\", \"ids\": [1, 3]}: mark items done by their 1-based id \
                (as shown in `list`).\n\
                - {\"action\": \"set\", \"items\": [\"...\"]}: replace the entire list with these \
                pending items — use this to re-plan from scratch.\n\
                - {\"action\": \"clear\"}: empty the checklist."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list", "add", "complete", "set", "clear"],
                        "description": "Which operation to perform."
                    },
                    "items": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "For 'add': items to append. For 'set': the full new list of pending items."
                    },
                    "ids": {
                        "type": "array",
                        "items": {"type": "integer"},
                        "description": "For 'complete': 1-based ids of items to mark done."
                    }
                },
                "required": ["action"]
            }),
        }
    }

    fn call(&self, ctx: &mut ToolCtx, args: Value) -> Result<String> {
        let path = Self::todos_path(ctx.project_root);
        let action = args.get("action").and_then(Value::as_str).ok_or_else(|| {
            anyhow::anyhow!(
                "missing required argument 'action' (expected one of: \
                    list, add, complete, set, clear)"
            )
        })?;

        let mut items = Self::load(&path);

        match action {
            "list" => Ok(Self::render(&items)),
            "add" => {
                let new_items = args
                    .get("items")
                    .and_then(Value::as_array)
                    .ok_or_else(|| anyhow::anyhow!("'add' requires an 'items' array of strings"))?;
                for v in new_items {
                    let text = v
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("'items' must be an array of strings"))?;
                    items.push(TodoItem {
                        text: text.to_string(),
                        done: false,
                    });
                }
                Self::save(&path, &items)?;
                Ok(Self::render(&items))
            }
            "complete" => {
                let ids = args.get("ids").and_then(Value::as_array).ok_or_else(|| {
                    anyhow::anyhow!("'complete' requires an 'ids' array of integers")
                })?;
                for v in ids {
                    let id = v.as_u64().ok_or_else(|| {
                        anyhow::anyhow!("'ids' must be an array of positive integers")
                    })? as usize;
                    if id == 0 || id > items.len() {
                        anyhow::bail!("id {id} is out of range (list has {} items)", items.len());
                    }
                    items[id - 1].done = true;
                }
                Self::save(&path, &items)?;
                Ok(Self::render(&items))
            }
            "set" => {
                let new_items = args
                    .get("items")
                    .and_then(Value::as_array)
                    .ok_or_else(|| anyhow::anyhow!("'set' requires an 'items' array of strings"))?;
                let mut replaced = Vec::with_capacity(new_items.len());
                for v in new_items {
                    let text = v
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("'items' must be an array of strings"))?;
                    replaced.push(TodoItem {
                        text: text.to_string(),
                        done: false,
                    });
                }
                Self::save(&path, &replaced)?;
                Ok(Self::render(&replaced))
            }
            "clear" => {
                items.clear();
                Self::save(&path, &items)?;
                Ok(Self::render(&items))
            }
            other => anyhow::bail!(
                "unknown action {other:?}; expected one of: list, add, complete, set, clear"
            ),
        }
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

    fn call(root: &Path, args: Value) -> String {
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: root,
            fstate: &mut fstate,
        };
        TodoTool.call(&mut ctx, args).unwrap()
    }

    #[test]
    fn add_then_list() {
        let root = tempdir("todo-add-list");
        let out = call(
            &root,
            json!({"action": "add", "items": ["first", "second"]}),
        );
        assert!(out.contains("[ ] 1. first"));
        assert!(out.contains("[ ] 2. second"));
        assert!(out.contains("0/2 done"));

        let listed = call(&root, json!({"action": "list"}));
        assert_eq!(listed, out);
    }

    #[test]
    fn complete_marks_done() {
        let root = tempdir("todo-complete");
        call(&root, json!({"action": "add", "items": ["a", "b", "c"]}));
        let out = call(&root, json!({"action": "complete", "ids": [1, 3]}));
        assert!(out.contains("[x] 1. a"));
        assert!(out.contains("[ ] 2. b"));
        assert!(out.contains("[x] 3. c"));
        assert!(out.contains("2/3 done"));
    }

    #[test]
    fn complete_out_of_range_errors() {
        let root = tempdir("todo-complete-oob");
        call(&root, json!({"action": "add", "items": ["a"]}));
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        assert!(
            TodoTool
                .call(&mut ctx, json!({"action": "complete", "ids": [5]}))
                .is_err()
        );
    }

    #[test]
    fn set_replaces_list() {
        let root = tempdir("todo-set");
        call(&root, json!({"action": "add", "items": ["old1", "old2"]}));
        let out = call(&root, json!({"action": "set", "items": ["fresh"]}));
        assert!(out.contains("[ ] 1. fresh"));
        assert!(!out.contains("old1"));
        assert!(out.contains("0/1 done"));
    }

    #[test]
    fn clear_empties_list() {
        let root = tempdir("todo-clear");
        call(&root, json!({"action": "add", "items": ["a"]}));
        let out = call(&root, json!({"action": "clear"}));
        assert_eq!(out, "(todo list is empty)");
    }

    #[test]
    fn persists_across_calls() {
        let root = tempdir("todo-persist");
        call(&root, json!({"action": "add", "items": ["persisted"]}));

        // A brand new ctx/fstate simulates a fresh tool call in a later turn.
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        let out = TodoTool.call(&mut ctx, json!({"action": "list"})).unwrap();
        assert!(out.contains("persisted"));

        assert!(root.join(".atelier").join("todos.json").exists());
    }

    #[test]
    fn corrupt_file_treated_as_empty() {
        let root = tempdir("todo-corrupt");
        std::fs::create_dir_all(root.join(".atelier")).unwrap();
        std::fs::write(root.join(".atelier").join("todos.json"), "not json{{{").unwrap();

        let out = call(&root, json!({"action": "list"}));
        assert_eq!(out, "(todo list is empty)");

        // Should still be usable afterwards (overwrites the corrupt file).
        let out = call(&root, json!({"action": "add", "items": ["recovered"]}));
        assert!(out.contains("recovered"));
    }

    #[test]
    fn missing_action_errors() {
        let root = tempdir("todo-missing-action");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        assert!(TodoTool.call(&mut ctx, json!({})).is_err());
    }

    #[test]
    fn unknown_action_errors() {
        let root = tempdir("todo-unknown-action");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
        };
        assert!(TodoTool.call(&mut ctx, json!({"action": "bogus"})).is_err());
    }
}
