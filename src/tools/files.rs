//! Where the text of a file comes from, and where it goes: the disk, unless
//! this process was given someone better to ask.
//!
//! An editor driving atelier ([`crate::acp`]) holds buffers the disk has not
//! seen yet, and wants the agent's edits to appear in them. So the tools
//! that read and write a file's text — `read`, `write`, `edit`, `multiedit` —
//! do it through [`read_to_string`] and [`write`] here, which hand the call
//! to the installed [`FileAccess`] when there is one.
//!
//! Only the bytes change hands. The path is resolved and confined by the
//! tool before it gets here ([`super::ToolCtx::resolve`]), and what the tool
//! read or wrote is recorded in [`super::FileState`] after, exactly as with
//! the disk: an `edit` against a buffer that changed since it was read is
//! refused the same way.
//!
//! The hook is the process's, not a session's, because a session's tools
//! run on several threads (see `agent::batch`) and in its subagents, and all
//! of them are the one editor's. A process that serves an editor serves one
//! (one session per process: see [`crate::acp`]).

use std::io;
use std::path::Path;
use std::sync::OnceLock;

/// Something that holds the files in place of the disk. Called from
/// whichever thread a tool runs on, possibly several at once.
pub trait FileAccess: Send + Sync {
    /// The text of the file at the absolute `path`. `None` to have it read
    /// from the disk instead (this source doesn't serve reads, or not this
    /// one).
    fn read(&self, path: &Path) -> Option<io::Result<String>>;
    /// Replace the text of the file at the absolute `path`, creating the
    /// file if need be. `None` to have it written to the disk instead.
    fn write(&self, path: &Path, text: &str) -> Option<io::Result<()>>;
}

static ACCESS: OnceLock<Box<dyn FileAccess>> = OnceLock::new();

/// Route the file tools through `access` from here on. Once per process:
/// a second call is ignored.
pub fn install(access: Box<dyn FileAccess>) {
    let _ = ACCESS.set(access);
}

/// [`std::fs::read_to_string`], or the installed source's idea of it.
pub fn read_to_string(path: &Path) -> io::Result<String> {
    match ACCESS.get().and_then(|access| access.read(path)) {
        Some(result) => result,
        None => std::fs::read_to_string(path),
    }
}

/// [`std::fs::write`] for text, or the installed source's idea of it.
pub fn write(path: &Path, text: &str) -> io::Result<()> {
    match ACCESS.get().and_then(|access| access.write(path, text)) {
        Some(result) => result,
        None => std::fs::write(path, text),
    }
}
