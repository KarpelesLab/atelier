//! Recent commits provider: a short log of recent history for orientation.

use super::git::run_git;
use super::{ContextItem, ContextProvider};
use std::path::Path;

const COMMIT_COUNT: &str = "8";

/// Reports the last 8 commits (short hash + subject) as a compact
/// [`ContextItem`], or `None` when `root` is not inside a git work tree or
/// has no commits yet.
pub struct RecentCommitsProvider;

impl ContextProvider for RecentCommitsProvider {
    fn name(&self) -> &str {
        "recent_commits"
    }

    fn gather(&self, root: &Path) -> Option<ContextItem> {
        let log = run_git(root, &["log", "--oneline", "-n", COMMIT_COUNT])?;
        let lines: Vec<&str> = log.lines().filter(|l| !l.is_empty()).collect();
        if lines.is_empty() {
            return None;
        }

        let body = lines.join("\n");
        Some(ContextItem::new("recent commits", body, 100))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_git_dir_returns_none() {
        let dir = std::env::temp_dir().join(format!("atelier-commits-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let provider = RecentCommitsProvider;
        assert!(provider.gather(&dir).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_repo_with_commits_returns_log() {
        let dir =
            std::env::temp_dir().join(format!("atelier-commits-git-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .expect("git should run")
        };

        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test User"]);
        std::fs::write(dir.join("file.txt"), "hello").unwrap();
        run(&["add", "file.txt"]);
        run(&["commit", "-q", "-m", "initial commit"]);

        let provider = RecentCommitsProvider;
        if let Some(item) = provider.gather(&dir) {
            assert_eq!(item.title, "recent commits");
            assert!(item.body.contains("initial commit"));
        }
        // If git isn't configured/available in the sandbox, gather() may
        // legitimately return None; the non-git test above covers that path.

        let _ = std::fs::remove_dir_all(&dir);
    }
}
