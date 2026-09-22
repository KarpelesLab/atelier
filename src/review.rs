//! The "subconscious" reviewer — a parallel, read-only pass over the exchange.
//!
//! When review mode is on, the agent loop spawns [`review`] on a worker thread
//! after each tool batch, handing it a text excerpt of what just happened (the
//! user's ask, the tool calls, and their results). It calls the model with a
//! reviewer prompt and returns a short note, which the loop surfaces in the main
//! dialog through [`Ui::subconscious`](crate::agent::Ui::subconscious). The
//! reviewer has no tools and no side effects — it can only observe and comment.
//!
//! # Contract (stable — the implementer owns `src/review.rs`)
//!
//! `review(cfg, model, excerpt) -> Option<String>`: run one review, returning
//! the note, or `None` when there's nothing worth saying (or on error). Use
//! `model` (a per-reviewer override) when `Some`, else `cfg.model`. Call
//! `provider::stream_chat` with a reviewer system prompt and NO tools; ignore
//! the stream (pass a no-op `on_event`). Treat a trimmed-empty or bare-"OK"
//! reply as `None`.

use crate::config::Config;

/// Run one background review over `excerpt`. Returns a short note, or `None`
/// when nothing needs saying.
#[allow(dead_code)] // wired into the agent loop; body is a stub until implemented
pub fn review(_cfg: &Config, _model: Option<&str>, _excerpt: &str) -> Option<String> {
    // Stub: no note. Replaced by a real reviewer call.
    None
}
