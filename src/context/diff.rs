//! Recent diff provider: `git diff --stat`, falling back to staged changes.

use super::git::run_git;
use super::{ContextItem, ContextProvider};
use std::path::Path;

const MAX_LINES: usize = 25;

/// Reports a `git diff --stat` summary (unstaged, or staged if nothing is
/// unstaged) as a compact [`ContextItem`], or `None` if there's nothing to
/// show or `root` isn't a git work tree.
pub struct GitDiffProvider;

impl ContextProvider for GitDiffProvider {
    fn name(&self) -> &str {
        "git_diff"
    }

    fn gather(&self, root: &Path) -> Option<ContextItem> {
        let mut stat = run_git(root, &["diff", "--stat"])?;
        let mut label = "unstaged";
        if stat.trim().is_empty() {
            stat = run_git(root, &["diff", "--cached", "--stat"])?;
            label = "staged";
        }
        if stat.trim().is_empty() {
            return None;
        }

        let lines: Vec<&str> = stat.lines().filter(|l| !l.is_empty()).collect();
        let mut body = format!("({label})\n");
        for line in lines.iter().take(MAX_LINES) {
            body.push_str(line);
            body.push('\n');
        }
        if lines.len() > MAX_LINES {
            body.push_str(&format!("+{} more line(s)\n", lines.len() - MAX_LINES));
        }

        Some(ContextItem::new("recent changes", body.trim_end(), 120))
    }
}
