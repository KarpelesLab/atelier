//! Advisory locks: instances telling each other what they are working on.
//!
//! Two agents in the same checkout will, sooner or later, edit the same
//! file. A lock is how one says "I am on this" — a path (a file, or a
//! directory and everything under it) with a note — so the other finds out
//! before it starts rather than from a failed edit afterwards.
//!
//! It is **advisory**: nothing is enforced, no tool call is refused because
//! of a lock, and nothing is locked automatically. What a lock does:
//!
//! - `acquire` on a path somebody else holds — the same path, a path inside
//!   a directory they hold, or a directory containing something they hold —
//!   is not granted, and says who holds it, since when, and their note. The
//!   agent decides what to do with that (wait, ask, work elsewhere).
//! - An edit tool about to modify a path under another instance's lock gets
//!   a one-line note appended to its result ([`edit_note`]). The edit still
//!   happens.
//!
//! The table lives in the coordinator's memory ([`super::daemon`]): holder,
//! path, note, since when. A holder's locks go when it disconnects. The
//! coordinator itself can go at any time, so each instance keeps the list of
//! what it holds and announces it again in its `hello` to the next one, as
//! it does its topic subscriptions.
//!
//! Every instance keeps a copy of the whole table ([`super::Client::locks`]),
//! replaced each time the coordinator says it changed (a `locks`
//! notification), so the check an edit makes costs no round trip — and
//! nothing at all while nobody holds a lock.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use super::Client;
use crate::tools;

/// One entry of the lock table, as an instance sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Lock {
    /// The holding instance's name.
    pub holder: String,
    /// Absolute and normalized.
    pub path: PathBuf,
    pub note: String,
    /// Unix seconds when it was acquired.
    pub since: u64,
    /// Held by this instance.
    pub yours: bool,
}

impl Lock {
    /// From the coordinator's entry; `me` is this instance's id.
    pub(super) fn from_value(v: &Value, me: &str) -> Self {
        let text = |key: &str| v[key].as_str().unwrap_or_default().to_string();
        Self {
            holder: text("holder"),
            path: PathBuf::from(text("path")),
            note: text("note"),
            since: v["since"].as_u64().unwrap_or(0),
            yours: v["holder_id"].as_str() == Some(me),
        }
    }

    /// `<path> — <holder>, for 3m: <note>`.
    pub fn describe(&self) -> String {
        let mut line = format!(
            "{} — {}{}, for {}",
            self.path.display(),
            self.holder,
            if self.yours { " (you)" } else { "" },
            age(self.since)
        );
        if !self.note.is_empty() {
            line.push_str(&format!(": {}", self.note));
        }
        line
    }
}

/// The entries of a `locks` table or notification.
pub(super) fn parse(table: &Value, me: &str) -> Vec<Lock> {
    table
        .as_array()
        .map(|a| a.iter().map(|v| Lock::from_value(v, me)).collect())
        .unwrap_or_default()
}

/// A lock this instance holds, as it re-announces it.
pub(super) fn held_value(path: &Path, note: &str, since: u64) -> Value {
    json!({ "path": path.to_string_lossy(), "note": note, "since": since })
}

/// Whether a lock on `a` and work on `b` concern the same files: the same
/// path, or one inside the other.
pub fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// `path` as a lock names it: absolute (against `root` if it is relative)
/// and normalized.
pub fn normalized(root: &Path, path: &str) -> PathBuf {
    tools::normalize(&root.join(path))
}

/// How long ago `since` (Unix seconds) was, roughly: `40s`, `12m`, `3h`.
fn age(since: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    match now.saturating_sub(since) {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s => format!("{}h", s / 3600),
    }
}

/// The tools that modify the files they are pointed at.
const EDIT_TOOLS: &[&str] = &["write", "edit", "multiedit", "notebook_edit", "apply_patch"];

/// What to add to the result of the edit `tool` is about to make with
/// `args`, if it touches a path another instance says it is working on:
/// `note: <peer> says it is working on this path: <note>`, one line per
/// lock. `None` — at the cost of one look at the cached table — in the
/// usual case where nobody else holds anything.
pub fn edit_note(client: &Client, tool: &str, args: &Value, root: &Path) -> Option<String> {
    if !EDIT_TOOLS.contains(&tool) {
        return None;
    }
    let theirs: Vec<Lock> = client.locks().into_iter().filter(|l| !l.yours).collect();
    if theirs.is_empty() {
        return None;
    }
    let targets: Vec<String> = if tool == "apply_patch" {
        tools::apply_patch::target_paths(args)
    } else {
        tools::path_arg(args)
            .ok()
            .map(String::from)
            .into_iter()
            .collect()
    };
    let mut note = String::new();
    for lock in &theirs {
        let hit = targets
            .iter()
            .map(|t| normalized(root, t))
            .find(|t| overlaps(&lock.path, t));
        let Some(target) = hit else { continue };
        // A directory's lock, or one file of a patch: say which path.
        let what = if target == lock.path && targets.len() == 1 {
            "this path".to_string()
        } else {
            lock.path.display().to_string()
        };
        note.push_str(&format!(
            "\nnote: {} says it is working on {what}",
            lock.holder
        ));
        if !lock.note.is_empty() {
            note.push_str(&format!(": {}", lock.note));
        }
    }
    (!note.is_empty()).then_some(note)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lock_covers_its_path_and_what_is_inside() {
        let p = Path::new;
        assert!(overlaps(p("/w/src"), p("/w/src")));
        assert!(overlaps(p("/w/src"), p("/w/src/a/b.rs")));
        assert!(overlaps(p("/w/src/a/b.rs"), p("/w/src")));
        // Whole components: `/w/src` is not inside `/w/s`, nor a sibling.
        assert!(!overlaps(p("/w/src"), p("/w/s")));
        assert!(!overlaps(p("/w/src"), p("/w/srcs/a.rs")));
        assert!(!overlaps(p("/w/src/a.rs"), p("/w/src/b.rs")));
    }

    #[test]
    fn paths_are_made_absolute_and_normal() {
        let root = Path::new("/w/proj");
        assert_eq!(normalized(root, "src/./a/../b.rs"), p("/w/proj/src/b.rs"));
        assert_eq!(normalized(root, "/etc/x/../hosts"), p("/etc/hosts"));
        assert_eq!(normalized(root, "."), p("/w/proj"));
        fn p(s: &str) -> PathBuf {
            PathBuf::from(s)
        }
    }

    #[test]
    fn a_lock_is_described_with_its_holder_and_note() {
        let mut lock = Lock {
            holder: "api".into(),
            path: "/w/src".into(),
            note: "renaming the handlers".into(),
            since: 0,
            yours: false,
        };
        let line = lock.describe();
        assert!(line.starts_with("/w/src — api, for "), "{line}");
        assert!(line.ends_with("h: renaming the handlers"), "{line}");
        lock.yours = true;
        lock.note.clear();
        lock.since = u64::MAX;
        assert_eq!(lock.describe(), "/w/src — api (you), for 0s");
    }
}
