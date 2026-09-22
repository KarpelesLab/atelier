//! Full-screen configuration editor, shown on `/config` (alternate screen).
//!
//! # Contract (stable — the implementer owns `src/tui/config.rs`)
//!
//! `run(session) -> Result<()>` takes over the terminal (enter the alternate
//! screen; raw mode is already on via the caller's `RawGuard`), renders an
//! editable list of settings, and returns when the user saves & exits (Esc /
//! `q`) or cancels. Read live values from the `Session` accessors
//! (`config().model`, `auto_approve()`, `context_limit()`, `review_enabled()`,
//! `settings()`), and apply edits with the setters
//! (`set_default_model`/`set_auto_approve`/`set_context_limit`/`set_review_enabled`),
//! then `session.save_settings()`. Restore the main screen on every exit path.
//! Do not edit files outside `src/tui/`.

use anyhow::Result;

use crate::agent::Session;

/// Run the full-screen settings editor. Placeholder until the real UI lands —
/// it currently returns immediately (the caller repaints the main screen).
pub fn run(_session: &mut Session) -> Result<()> {
    Ok(())
}
