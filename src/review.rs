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
use crate::provider::{self, Message};

/// System prompt establishing the reviewer's persona and reply contract.
const REVIEWER_SYSTEM_PROMPT: &str = "You are the coding agent's subconscious — a terse background reviewer watching its work. Given the recent exchange and the edits/commands it just made, reply with ONE short sentence (max ~25 words) flagging any real mistake, bug, risk, or a clearly better approach. Do not restate what was done, do not praise. If nothing needs saying, reply with exactly: OK";

/// Run one background review over `excerpt`. Returns a short note, or `None`
/// when nothing needs saying.
#[allow(dead_code)] // wired into the agent loop; body is a stub until implemented
pub fn review(cfg: &Config, model: Option<&str>, excerpt: &str) -> Option<String> {
    let mut cfg_for_review = cfg.clone();
    if let Some(model) = model {
        cfg_for_review.model = model.to_string();
    }

    let messages = [
        Message::system(REVIEWER_SYSTEM_PROMPT),
        Message::user(excerpt),
    ];

    let completion = provider::stream_chat(&cfg_for_review, &messages, &[], |_ev| {}).ok()?;
    note_from_reply(&completion.content)
}

/// Decide whether a reviewer reply is worth surfacing. Returns `None` when the
/// trimmed reply is empty or is just an affirmation ("OK"/"ok."/"nothing",
/// case-insensitive, ignoring trailing punctuation), else the trimmed note.
fn note_from_reply(reply: &str) -> Option<String> {
    let trimmed = reply.trim();
    if trimmed.is_empty() {
        return None;
    }
    let bare = trimmed.trim_end_matches(['.', '!']).trim();
    if bare.eq_ignore_ascii_case("ok") || bare.eq_ignore_ascii_case("nothing") {
        return None;
    }
    Some(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affirmations_yield_no_note() {
        assert_eq!(note_from_reply("OK"), None);
        assert_eq!(note_from_reply("ok."), None);
        assert_eq!(note_from_reply("  OK  "), None);
        assert_eq!(note_from_reply(""), None);
        assert_eq!(note_from_reply("   "), None);
        assert_eq!(note_from_reply("nothing"), None);
        assert_eq!(note_from_reply("Nothing."), None);
        assert_eq!(note_from_reply("OK!"), None);
    }

    #[test]
    fn real_note_is_kept() {
        assert_eq!(
            note_from_reply("Careful: that rm targets the parent dir."),
            Some("Careful: that rm targets the parent dir.".to_string())
        );
    }

    #[test]
    fn multi_word_note_is_kept() {
        let reply = "  The new function ignores the Err case from parse_config, which will panic downstream.  ";
        assert_eq!(
            note_from_reply(reply),
            Some(
                "The new function ignores the Err case from parse_config, which will panic downstream."
                    .to_string()
            )
        );
    }
}
