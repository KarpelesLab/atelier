//! Minimal terminal interface.
//!
//! The intended interface (roadmap M2): a single input line plus a status strip
//! (model · cwd · git branch · token counters). The input row redraws in place;
//! **everything else — assistant text, reasoning, tool activity — prints to the
//! terminal scrollback and is never redrawn.** No panes, no mouse, no
//! alt-screen. Streamed output must interleave above the live input line
//! without corrupting it. `crossterm` (already a dependency) drives raw mode.
//!
//! # Contract (stable — implementers must not change this signature)
//!
//! [`run`] takes ownership of a [`Session`](crate::agent::Session) and drives it
//! to completion, reading user input and rendering through a [`Ui`](crate::agent::Ui)
//! implementation of your own (a `TuiUi`). Handle `/quit`, `/models`, Ctrl-C
//! (cancel the in-flight turn) and Ctrl-D (exit).
//!
//! ## For the implementer (owns `src/tui/`)
//!
//! Replace the delegating body of [`run`] with the real inline UI and add a
//! `TuiUi` implementing [`Ui`](crate::agent::Ui). This module is gated behind
//! the `tui` feature. Do not edit files outside `src/tui/`.

use anyhow::Result;

use crate::agent::{self, Session};

/// Drive a session through the terminal interface.
///
/// Placeholder: delegates to the plain REPL until the inline UI lands.
pub fn run(session: Session) -> Result<()> {
    agent::repl(session)
}
