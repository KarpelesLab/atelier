//! Git status provider: current branch plus changed/untracked files.

use super::{ContextItem, ContextProvider};
use std::path::Path;
use std::process::Command;

/// Reports `git status --porcelain` (plus the current branch) as a compact
/// [`ContextItem`], or `None` when `root` is not inside a git work tree.
pub struct GitStatusProvider;

const MAX_ENTRIES: usize = 20;

impl ContextProvider for GitStatusProvider {
    fn name(&self) -> &str {
        "git_status"
    }

    fn gather(&self, root: &Path) -> Option<ContextItem> {
        let branch = run_git(root, &["branch", "--show-current"])?;
        let status = run_git(root, &["status", "--porcelain"])?;

        let branch = branch.trim();
        let lines: Vec<&str> = status.lines().filter(|l| !l.is_empty()).collect();

        let mut body = String::new();
        body.push_str(&format!(
            "branch: {}\n",
            if branch.is_empty() {
                "(detached HEAD)"
            } else {
                branch
            }
        ));

        if lines.is_empty() {
            body.push_str("working tree clean\n");
        } else {
            body.push_str(&format!("{} changed file(s):\n", lines.len()));
            for line in lines.iter().take(MAX_ENTRIES) {
                body.push_str(line);
                body.push('\n');
            }
            if lines.len() > MAX_ENTRIES {
                body.push_str(&format!("+{} more\n", lines.len() - MAX_ENTRIES));
            }
        }

        Some(ContextItem::new("git status", body.trim_end(), 200))
    }
}

/// Runs `git -C <root> <args>`, returning stdout on success or `None` if the
/// command fails to spawn, exits non-zero, or `root` isn't a git work tree.
pub(super) fn run_git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_git_dir_returns_none() {
        let dir = std::env::temp_dir().join(format!("atelier-git-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let provider = GitStatusProvider;
        assert!(provider.gather(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
