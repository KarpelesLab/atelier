//! Project layout provider: a compact, gitignore-aware directory tree.

use super::{ContextItem, ContextProvider};
use ignore::WalkBuilder;
use std::path::Path;

const MAX_DEPTH: usize = 2;
const MAX_ENTRIES: usize = 40;

/// Renders a shallow tree of `root` (respecting `.gitignore`, depth- and
/// count-limited) as a compact [`ContextItem`]. Always returns `Some`, even
/// outside a git repository.
pub struct LayoutProvider;

impl ContextProvider for LayoutProvider {
    fn name(&self) -> &str {
        "layout"
    }

    fn gather(&self, root: &Path) -> Option<ContextItem> {
        let mut entries = Vec::new();
        let mut truncated = false;

        let walker = WalkBuilder::new(root)
            .max_depth(Some(MAX_DEPTH))
            .hidden(true)
            .git_ignore(true)
            .sort_by_file_name(|a, b| a.cmp(b))
            .build();

        for result in walker {
            let entry = match result {
                Ok(e) => e,
                Err(_) => continue,
            };
            // Skip the root itself.
            if entry.depth() == 0 {
                continue;
            }
            if entries.len() >= MAX_ENTRIES {
                truncated = true;
                break;
            }

            let depth = entry.depth();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let name = entry.file_name().to_string_lossy();
            let indent = "  ".repeat(depth.saturating_sub(1));
            if is_dir {
                entries.push(format!("{indent}{name}/"));
            } else {
                entries.push(format!("{indent}{name}"));
            }
        }

        let mut body = if entries.is_empty() {
            "(empty)".to_string()
        } else {
            entries.join("\n")
        };
        if truncated {
            body.push_str("\n… (truncated)");
        }

        Some(ContextItem::new("project layout", body, 60))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_files_and_dirs() {
        let dir = std::env::temp_dir().join(format!("atelier-layout-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "hi").unwrap();
        std::fs::write(dir.join("sub/b.txt"), "hi").unwrap();

        let provider = LayoutProvider;
        let item = provider.gather(&dir).expect("always returns Some");
        assert_eq!(item.title, "project layout");
        assert!(item.body.contains("a.txt"));
        assert!(item.body.contains("sub/"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_dir_returns_placeholder() {
        let dir = std::env::temp_dir().join(format!("atelier-layout-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let provider = LayoutProvider;
        let item = provider.gather(&dir).expect("always returns Some");
        assert_eq!(item.body, "(empty)");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
