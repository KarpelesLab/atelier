//! On-disk session persistence (roadmap M6).
//!
//! The conversation history is saved to `.atelier/session.json` under the
//! project root after each turn, so a session can be resumed later with
//! `--continue`. This is a thin store; the [`Session`](crate::agent::Session)
//! owns when to save/load.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::provider::Message;

/// Session file, relative to the project root.
pub const SESSION_FILE: &str = ".atelier/session.json";

/// Path to the session file for a project root.
pub fn path(root: &Path) -> PathBuf {
    root.join(SESSION_FILE)
}

/// Load a saved conversation, or an empty history if none exists / is unreadable.
pub fn load(root: &Path) -> Vec<Message> {
    let p = path(root);
    let Ok(text) = std::fs::read_to_string(&p) else {
        return Vec::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// Persist the conversation to `.atelier/session.json` (creating `.atelier/`).
pub fn save(root: &Path, history: &[Message]) -> Result<()> {
    let p = path(root);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(history).context("serializing session")?;
    std::fs::write(&p, text).with_context(|| format!("writing {}", p.display()))
}

/// Delete the saved session, if any.
pub fn clear(root: &Path) -> Result<()> {
    let p = path(root);
    match std::fs::remove_file(&p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", p.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;

    fn tmpdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "atelier-session-{}-{}",
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
    fn round_trip_and_clear() {
        let root = tmpdir();
        assert!(load(&root).is_empty());

        let history = vec![Message::user("hello"), Message::assistant("hi there")];
        save(&root, &history).unwrap();

        let loaded = load(&root);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].role, "user");
        assert_eq!(loaded[0].content, "hello");
        assert_eq!(loaded[1].content, "hi there");

        clear(&root).unwrap();
        assert!(load(&root).is_empty());
        // Clearing again is a no-op, not an error.
        clear(&root).unwrap();

        let _ = std::fs::remove_dir_all(&root);
    }
}
