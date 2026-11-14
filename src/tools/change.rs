//! What a file tool did to a file: the record behind `event/file_change`.
//!
//! A tool that writes a file's text — `write`, `edit`, `multiedit`,
//! `apply_patch`, `notebook_edit` — notes the change in the session's
//! [`FileState`](super::FileState) once the write has succeeded
//! ([`FileState::changed`](super::FileState::changed)), with the text before
//! and after. The tool runner hands each note to the interface
//! ([`crate::agent::Ui::file_change`]) between the call's start and its end,
//! and an interface that reports to a program turns it into a diff
//! ([`crate::events::file_change`]). So the record describes what happened
//! on disk — nothing is noted for a call that was refused or failed before
//! it wrote — and the diff is computed only where someone reads it.

use std::path::{Path, PathBuf};

/// The largest text kept for a diff, in bytes. A side over this is recorded
/// as [`Side::Large`]: the change is still reported, without its diff.
pub const TEXT_CAP: u64 = 1024 * 1024;

/// One side of a change: the file before, or after.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Side {
    /// There is no file on this side: it was created, or deleted.
    Absent,
    /// The file's text.
    Text(String),
    /// Not text (it could not be read as UTF-8).
    Binary,
    /// Text too long to diff (over [`TEXT_CAP`]).
    Large,
}

impl Side {
    /// A file's text as a side of a change.
    pub fn text(text: &str) -> Side {
        if text.len() as u64 > TEXT_CAP {
            Side::Large
        } else {
            Side::Text(text.to_string())
        }
    }

    /// The file at `path` as it is on disk now: absent, text, or neither.
    ///
    /// The disk, not [`super::files`]: asking an editor for its buffer is a
    /// request to it, and the one interface that has an editor
    /// ([`crate::acp`]) shows an edit its own way, without this record.
    pub fn of(path: &Path) -> Side {
        match std::fs::metadata(path) {
            Ok(meta) if meta.len() > TEXT_CAP => Side::Large,
            Ok(_) => match std::fs::read_to_string(path) {
                Ok(text) => Side::Text(text),
                Err(_) => Side::Binary,
            },
            Err(_) => Side::Absent,
        }
    }
}

/// What became of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Created,
    Modified,
    Deleted,
}

impl Kind {
    /// The name the events use.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Created => "created",
            Kind::Modified => "modified",
            Kind::Deleted => "deleted",
        }
    }
}

/// A file a tool call wrote, created or deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// The file, absolute.
    pub path: PathBuf,
    pub before: Side,
    pub after: Side,
}

impl FileChange {
    pub fn kind(&self) -> Kind {
        match (&self.before, &self.after) {
            (Side::Absent, _) => Kind::Created,
            (_, Side::Absent) => Kind::Deleted,
            _ => Kind::Modified,
        }
    }

    /// Whether the write left the file as it was: nothing to report.
    pub fn is_noop(&self) -> bool {
        match (&self.before, &self.after) {
            (Side::Text(a), Side::Text(b)) => a == b,
            (Side::Absent, Side::Absent) => true,
            _ => false,
        }
    }
}
