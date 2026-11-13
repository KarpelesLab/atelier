//! Memory provider: ambient recall of what the agent saved in earlier
//! sessions (see [`crate::memory`]).
//!
//! Only the **index** is injected — one line per note, name and description —
//! never the notes themselves. The index is what lets the model notice that
//! it knows something; the bodies would spend the context budget on notes
//! irrelevant to this turn, so the item ends by telling the model to
//! `memory read` the ones that look relevant.
//!
//! The index is built from the note files on every turn, not from
//! `MEMORY.md`, so a note the user edited or deleted by hand is reflected at
//! once. It gets its own small budget ([`MAX_INDEX_CHARS`]) on top of the
//! overall one applied by the caller: memory ranks just below the project's
//! instructions, and without a cap of its own a full store would crowd out
//! the git status and diagnostics ranked beneath it.

use super::{ContextItem, ContextProvider};
use crate::memory::Store;
use std::path::{Path, PathBuf};

/// Most characters of index lines to inject (about 400 tokens). Past it the
/// item says how many notes were left out and how to see them.
const MAX_INDEX_CHARS: usize = 1600;

/// How to use the index. Kept short: it is sent with every index.
const PREAMBLE: &str = "Notes you saved in earlier sessions, as `name [type] — description`. \
    Only the index is shown: before acting, call the `memory` tool with \
    {\"action\": \"read\", \"name\": ...} for each one that looks relevant to the task. \
    They are recollections and may be out of date.";

/// Reports the index of saved memories, project scope then user-global, or
/// `None` when nothing has been saved.
pub struct MemoryProvider {
    /// The user-global memory directory, fixed at construction so tests can
    /// use a temp directory instead of the real home.
    global: Option<PathBuf>,
}

impl MemoryProvider {
    pub fn new(global: Option<PathBuf>) -> Self {
        Self { global }
    }
}

impl ContextProvider for MemoryProvider {
    fn name(&self) -> &str {
        "memory"
    }

    fn gather(&self, root: &Path) -> Option<ContextItem> {
        let memories = Store::new(root, self.global.clone()).list();
        if memories.is_empty() {
            return None;
        }

        let mut body = String::from(PREAMBLE);
        let mut used = 0;
        let mut shown = 0;
        for (scope, m) in &memories {
            let line = format!(
                "\n- {} [{}, {scope}] — {}",
                m.name,
                m.kind.as_str(),
                m.description
            );
            used += line.chars().count();
            if used > MAX_INDEX_CHARS {
                break;
            }
            body.push_str(&line);
            shown += 1;
        }
        if shown < memories.len() {
            body.push_str(&format!(
                "\n… and {} more; use {{\"action\": \"list\"}} to see them all",
                memories.len() - shown
            ));
        }
        // Just under the project instructions (250), above everything that
        // describes the repository's current state.
        // Standing: the index is what tells the model a memory exists, so it
        // must be in every request, not only when it last changed.
        Some(ContextItem::new("memory", body, 240).standing())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{Kind, Memory, Scope, testutil};

    fn memory(name: &str, kind: Kind, description: &str) -> Memory {
        Memory {
            name: name.to_string(),
            description: description.to_string(),
            kind,
            body: "body that must not be injected".to_string(),
        }
    }

    #[test]
    fn none_when_nothing_is_saved() {
        let (_, root, global) = testutil::store("ctx-empty");
        assert!(MemoryProvider::new(Some(global)).gather(&root).is_none());
        assert!(MemoryProvider::new(None).gather(&root).is_none());
    }

    #[test]
    fn injects_the_index_of_both_scopes_without_bodies() {
        let (store, root, global) = testutil::store("ctx-index");
        store
            .save(
                Scope::Project,
                &memory("lint-first", Kind::Feedback, "run clippy before committing"),
            )
            .unwrap();
        store
            .save(
                Scope::Global,
                &memory("role", Kind::User, "maintains the build"),
            )
            .unwrap();

        let item = MemoryProvider::new(Some(global)).gather(&root).unwrap();
        assert_eq!(item.title, "memory");
        assert_eq!(item.priority, 240);
        assert!(item.body.starts_with(PREAMBLE));
        assert!(item.body.contains("\"action\": \"read\""));
        assert!(item.body.ends_with(
            "\n- lint-first [feedback, project] — run clippy before committing\
             \n- role [user, global] — maintains the build"
        ));
        assert!(!item.body.contains("must not be injected"));
    }

    #[test]
    fn follows_hand_edits_to_the_files() {
        let (store, root, global) = testutil::store("ctx-edits");
        store
            .save(Scope::Project, &memory("a", Kind::Project, "first"))
            .unwrap();
        let provider = MemoryProvider::new(Some(global));
        std::fs::write(
            root.join(".atelier/memory/by-hand.md"),
            "Written by the user.",
        )
        .unwrap();
        std::fs::remove_file(root.join(".atelier/memory/a.md")).unwrap();

        let body = provider.gather(&root).unwrap().body;
        assert!(body.contains("- by-hand [project, project] — Written by the user."));
        assert!(!body.contains("first"));
    }

    #[test]
    fn a_long_index_is_cut_with_a_count() {
        let (store, root, global) = testutil::store("ctx-budget");
        let description = "d".repeat(150);
        for i in 0..30 {
            store
                .save(
                    Scope::Project,
                    &memory(&format!("note-{i:02}"), Kind::Project, &description),
                )
                .unwrap();
        }
        let body = MemoryProvider::new(Some(global))
            .gather(&root)
            .unwrap()
            .body;
        let shown = body.matches("\n- note-").count();
        assert!(shown > 0 && shown < 30, "shown {shown}");
        assert!(body.contains(&format!("… and {} more", 30 - shown)));
        assert!(body.chars().count() < PREAMBLE.chars().count() + MAX_INDEX_CHARS + 100);
    }
}
