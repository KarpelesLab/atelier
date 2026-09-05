//! Minimal terminal interface.
//!
//! M2: a single input line + status strip, with streamed output printed to
//! scrollback and never redrawn. Empty until then; gated behind the `tui`
//! feature so headless builds drop it entirely.
