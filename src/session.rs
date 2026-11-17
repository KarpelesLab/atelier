//! On-disk session persistence (roadmap M6).
//!
//! The conversation (a rolling summary of compacted older turns, plus the recent
//! message history) is saved to `session.json` in the project's state directory
//! ([`crate::state`] — outside the project) after each turn, so a session can be resumed later with `--continue`. This is
//! a thin store; the [`Session`](crate::agent::Session) owns when to save/load.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::provider::Message;

/// Session file, in the project's state directory.
pub const SESSION_FILE: &str = "session.json";

/// Claude backend: the id of the Claude Code session to `--resume`. The
/// conversation itself lives in Claude Code's own store.
pub const CLAUDE_SESSION_FILE: &str = "claude-session";

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

/// Path to the session file for a project root; `None` when there is no
/// state directory to keep it in.
pub fn path(root: &Path) -> Option<PathBuf> {
    Some(crate::state::dir(root)?.join(SESSION_FILE))
}

/// Where the Claude session id of a project root is kept.
fn claude_path(root: &Path) -> Option<PathBuf> {
    Some(crate::state::dir(root)?.join(CLAUDE_SESSION_FILE))
}

/// Why nothing can be saved, when [`path`] is `None`.
const NOWHERE: &str =
    "nowhere to keep the session (none of ATELIER_STATE_DIR, XDG_STATE_HOME and HOME is set)";

/// Load a saved session, or an empty one if none exists / is unreadable.
///
/// Accepts both the current object form and the original bare-array form (a
/// plain `[Message, ...]`) for backward compatibility.
pub fn load(root: &Path) -> SessionData {
    let Some(Ok(text)) = path(root).map(std::fs::read_to_string) else {
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

/// Persist the session to `session.json` in the project's state directory
/// (creating it).
pub fn save(root: &Path, summary: Option<&str>, messages: &[Message]) -> Result<()> {
    let p = path(root).context(NOWHERE)?;
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let stored = Stored {
        summary: summary.map(str::to_owned),
        messages: messages.to_vec(),
    };
    let text = serde_json::to_string_pretty(&stored).context("serializing session")?;
    // Written beside and moved into place, so that a process stopped in the
    // middle (a take-over that could not wait, see `own`) leaves the file
    // of the turn before rather than half of this one.
    let partial = p.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&partial, text)
        .and_then(|()| std::fs::rename(&partial, &p))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&partial);
        })
        .with_context(|| format!("writing {}", p.display()))
}

/// Delete the saved session (both backends), if any.
pub fn clear(root: &Path) -> Result<()> {
    for p in [path(root), claude_path(root)].into_iter().flatten() {
        match std::fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", p.display())),
        }
    }
    Ok(())
}

/// The saved Claude session id, if any.
pub fn load_claude(root: &Path) -> Option<String> {
    let id = std::fs::read_to_string(claude_path(root)?).ok()?;
    let id = id.trim();
    (!id.is_empty()).then(|| id.to_string())
}

/// Remember the Claude session id for `--continue`.
pub fn save_claude(root: &Path, id: &str) -> Result<()> {
    let p = claude_path(root).context(NOWHERE)?;
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&p, id).with_context(|| format!("writing {}", p.display()))
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

        // Kept for the project, not in it.
        assert!(path(&root).unwrap().is_file());
        assert!(!path(&root).unwrap().starts_with(&root));
        save_claude(&root, "abc").unwrap();
        assert_eq!(load_claude(&root).as_deref(), Some("abc"));
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);

        let data = load(&root);
        assert_eq!(data.summary.as_deref(), Some("earlier: we said hi"));
        assert_eq!(data.messages.len(), 2);
        assert_eq!(data.messages[0].content, "hello");

        clear(&root).unwrap();
        assert!(load(&root).messages.is_empty());
        assert_eq!(load_claude(&root), None);
        clear(&root).unwrap(); // no-op, not an error

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reads_legacy_bare_array() {
        let root = tmpdir();
        let legacy = serde_json::to_string(&vec![Message::user("legacy")]).unwrap();
        let file = path(&root).unwrap();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, legacy).unwrap();

        let data = load(&root);
        assert!(data.summary.is_none());
        assert_eq!(data.messages.len(), 1);
        assert_eq!(data.messages[0].content, "legacy");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A file from before turns carried their context (no `context` key on
    /// its messages) still loads, and a turn's context survives a save.
    #[test]
    fn reads_messages_without_context_and_keeps_it_when_present() {
        let root = tmpdir();
        let old = r#"{"summary":"s","messages":[
            {"role":"user","content":"hi","tool_calls":[],"tool_call_id":null,"images":[]},
            {"role":"assistant","content":"yo","tool_calls":[],"tool_call_id":null}]}"#;
        let file = path(&root).unwrap();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, old).unwrap();

        let mut data = load(&root);
        assert_eq!(data.summary.as_deref(), Some("s"));
        assert_eq!(data.messages.len(), 2);
        assert!(data.messages[0].context.is_none());

        data.messages
            .push(Message::user("next").with_context(Some("## git status\nclean".into())));
        save(&root, data.summary.as_deref(), &data.messages).unwrap();
        let again = load(&root);
        assert_eq!(
            again.messages[2].context.as_deref(),
            Some("## git status\nclean")
        );
        assert_eq!(again.messages[2].content, "next");
        assert!(again.messages[0].context.is_none());

        let _ = std::fs::remove_dir_all(&root);
    }
}
