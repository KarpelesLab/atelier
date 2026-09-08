//! Project instructions provider: surfaces a repo's own agent-facing
//! instructions file, if it has one.

use super::{ContextItem, ContextProvider};
use std::path::Path;

/// Candidate instruction files, in precedence order. The first one that
/// exists at the project root wins.
const CANDIDATES: &[&str] = &[
    "AGENTS.md",
    ".atelier/instructions.md",
    "CLAUDE.md",
    ".cursorrules",
];

/// Maximum number of characters of file content included in the item body.
/// Long instruction files are truncated with a trailing note so the budget
/// stays predictable.
const MAX_CHARS: usize = 3000;

/// Reports the content of the first project-instructions file found at
/// `root` (checked in [`CANDIDATES`] order), or `None` if none exist.
pub struct InstructionsProvider;

impl ContextProvider for InstructionsProvider {
    fn name(&self) -> &str {
        "instructions"
    }

    fn gather(&self, root: &Path) -> Option<ContextItem> {
        for candidate in CANDIDATES {
            let path = root.join(candidate);
            if let Ok(contents) = std::fs::read_to_string(&path) {
                let body = truncate(&contents, MAX_CHARS);
                return Some(ContextItem::new("project instructions", body, 250));
            }
        }
        None
    }
}

/// Truncates `text` to at most `max_chars` characters (on a char boundary),
/// appending a note when truncation happened. Leaves `text` untouched
/// otherwise.
fn truncate(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim_end();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(max_chars).collect();
    format!(
        "{head}\n... (truncated, {} chars total)",
        trimmed.chars().count()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "atelier-instructions-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn returns_agents_md_content() {
        let dir = temp_dir("agents");
        std::fs::write(dir.join("AGENTS.md"), "Use tabs, not spaces.").unwrap();

        let provider = InstructionsProvider;
        let item = provider.gather(&dir).expect("AGENTS.md should be found");
        assert_eq!(item.title, "project instructions");
        assert_eq!(item.body, "Use tabs, not spaces.");
        assert_eq!(item.priority, 250);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn agents_md_takes_precedence_over_claude_md() {
        let dir = temp_dir("precedence");
        std::fs::write(dir.join("AGENTS.md"), "agents content").unwrap();
        std::fs::write(dir.join("CLAUDE.md"), "claude content").unwrap();

        let provider = InstructionsProvider;
        let item = provider.gather(&dir).expect("a file should be found");
        assert_eq!(item.body, "agents content");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn falls_back_to_claude_md_when_no_agents_md() {
        let dir = temp_dir("fallback");
        std::fs::write(dir.join("CLAUDE.md"), "claude content").unwrap();

        let provider = InstructionsProvider;
        let item = provider.gather(&dir).expect("CLAUDE.md should be found");
        assert_eq!(item.body, "claude content");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn none_when_no_instructions_file_present() {
        let dir = temp_dir("none");

        let provider = InstructionsProvider;
        assert!(provider.gather(&dir).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncates_long_content_with_note() {
        let dir = temp_dir("truncate");
        let long_content = "a".repeat(MAX_CHARS + 500);
        std::fs::write(dir.join("AGENTS.md"), &long_content).unwrap();

        let provider = InstructionsProvider;
        let item = provider.gather(&dir).expect("AGENTS.md should be found");
        assert!(item.body.len() < long_content.len());
        assert!(item.body.contains("truncated"));
        assert!(item.body.starts_with(&"a".repeat(100)));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
