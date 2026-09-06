//! On-disk session persistence (roadmap M6).
//!
//! The conversation (a rolling summary of compacted older turns, plus the recent
//! message history) is saved to `.atelier/session.json` under the project root
//! after each turn, so a session can be resumed later with `--continue`. This is
//! a thin store; the [`Session`](crate::agent::Session) owns when to save/load.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::provider::Message;

/// Session file, relative to the project root.
pub const SESSION_FILE: &str = ".atelier/session.json";

/// A restored session: the compaction summary (if any) and the recent history.
#[derive(Default)]
pub struct SessionData {
    pub summary: Option<String>,
    pub messages: Vec<Message>,
}

/// On-disk shape. `messages` and `summary` both default so partial/older files
/// still load.
#[derive(Default, Serialize, Deserialize)]
struct Stored {
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    messages: Vec<Message>,
}

/// Path to the session file for a project root.
pub fn path(root: &Path) -> PathBuf {
    root.join(SESSION_FILE)
}

/// Load a saved session, or an empty one if none exists / is unreadable.
///
/// Accepts both the current object form and the original bare-array form (a
/// plain `[Message, ...]`) for backward compatibility.
pub fn load(root: &Path) -> SessionData {
    let p = path(root);
    let Ok(text) = std::fs::read_to_string(&p) else {
        return SessionData::default();
    };
    if let Ok(stored) = serde_json::from_str::<Stored>(&text) {
        return SessionData {
            summary: stored.summary,
            messages: stored.messages,
        };
    }
    // Fall back to the original format: a bare array of messages.
    if let Ok(messages) = serde_json::from_str::<Vec<Message>>(&text) {
        return SessionData {
            summary: None,
            messages,
        };
    }
    SessionData::default()
}

/// Persist the session to `.atelier/session.json` (creating `.atelier/`).
pub fn save(root: &Path, summary: Option<&str>, messages: &[Message]) -> Result<()> {
    let p = path(root);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let stored = Stored {
        summary: summary.map(str::to_owned),
        messages: messages.to_vec(),
    };
    let text = serde_json::to_string_pretty(&stored).context("serializing session")?;
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
    fn round_trip_with_summary_and_clear() {
        let root = tmpdir();
        assert!(load(&root).messages.is_empty());
        assert!(load(&root).summary.is_none());

        let history = vec![Message::user("hello"), Message::assistant("hi there")];
        save(&root, Some("earlier: we said hi"), &history).unwrap();

        let data = load(&root);
        assert_eq!(data.summary.as_deref(), Some("earlier: we said hi"));
        assert_eq!(data.messages.len(), 2);
        assert_eq!(data.messages[0].content, "hello");

        clear(&root).unwrap();
        assert!(load(&root).messages.is_empty());
        clear(&root).unwrap(); // no-op, not an error

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reads_legacy_bare_array() {
        let root = tmpdir();
        let legacy = serde_json::to_string(&vec![Message::user("legacy")]).unwrap();
        std::fs::create_dir_all(path(&root).parent().unwrap()).unwrap();
        std::fs::write(path(&root), legacy).unwrap();

        let data = load(&root);
        assert!(data.summary.is_none());
        assert_eq!(data.messages.len(), 1);
        assert_eq!(data.messages[0].content, "legacy");

        let _ = std::fs::remove_dir_all(&root);
    }
}
