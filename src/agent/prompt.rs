//! What a request is made of, laid out so that each one is a prefix of the
//! next.
//!
//! An inference server caches the prompt it has already evaluated —
//! llama.cpp's KV cache as much as a hosted provider's prompt cache — and
//! reuses it up to the first byte that differs from the new request. So the
//! requests of a session are built to only ever *grow*:
//!
//! ```text
//! system     system prompt + standing context (instructions, memory index)
//!            [+ summary of compacted turns]
//! user       <context> git status, diff, layout, … </context>  first prompt
//! assistant  … tool calls …
//! tool       …
//! assistant  answer
//! user       <context> only what changed since </context>      second prompt
//! …
//! ```
//!
//! - The **system message** ([`system_message`]) holds what is constant for
//!   the session: the system prompt and the *standing* context items. It
//!   changes only when an instruction file or the memory index does, which
//!   legitimately costs the cache. The rolling summary comes last in it, so a
//!   compaction (which rewrites the history anyway) still leaves the prompt
//!   and the instructions cached in front of it.
//! - **Volatile context** — everything else the providers gather — belongs
//!   to the user turn it was gathered for
//!   ([`Message::context`](crate::provider::Message::context)), inside the
//!   same user message: strict chat templates accept neither a second system
//!   message nor two user messages in a row. A block is never rewritten once
//!   sent; the next turn carries only the items that changed
//!   ([`context::dedup_items`](crate::context::dedup_items)) and names the
//!   rest in a line, so the model knows which older blocks still hold.
//!
//! The Claude backend doesn't come through here: claude assembles its own
//! prompt, and atelier only prefixes a context block to the user's message
//! (`Session::gather_context`).
//!
//! # Seeing it
//!
//! `atelier debug prompt` prints the first request a prompt would make
//! ([`Session::first_request`]). It is not a second description of the
//! layout that could drift from the first: the loop's own two steps —
//! [`Session::push_user`], then [`Session::next_request`] — are run, and
//! what they produce is rendered by the function that builds the body the
//! provider sends. Only the sending is left out.

use anyhow::{Result, bail};
use serde_json::Value;

use super::{CONTEXT_TOKEN_BUDGET, MODE_NOTE, SYSTEM_PROMPT, Session, Ui};
use crate::context::{self, ContextItem};
use crate::provider::{self, Message, ToolSpec};

/// The session's one system message: `prompt`, then the standing context
/// (whole — its providers budget it themselves), then the summary of
/// compacted turns if there is one.
pub(super) fn system_message(
    prompt: &str,
    standing: &[ContextItem],
    summary: Option<&str>,
) -> String {
    let mut system = prompt.to_string();
    if !standing.is_empty() {
        system.push_str("\n\nStanding project context:\n");
        for item in standing {
            system.push_str(&format!("\n## {}\n{}\n", item.title, item.body));
        }
    }
    if let Some(summary) = summary {
        system.push_str("\n\nSummary of earlier conversation:\n");
        system.push_str(summary);
    }
    system
}

/// One turn's context block: the changed items, prioritized and cut to the
/// per-turn budget, then a line each for what is unchanged and what no
/// longer applies. `None` when nothing changed — an unchanged turn costs no
/// tokens at all.
fn turn_block(delta: context::Delta) -> Option<String> {
    if delta.changed.is_empty() && delta.cleared.is_empty() {
        return None;
    }
    let mut block = context::render_budgeted(delta.changed, CONTEXT_TOKEN_BUDGET)
        .unwrap_or_default()
        .trim_end()
        .to_string();
    let mut note = |label: &str, titles: &[String]| {
        if !titles.is_empty() {
            if !block.is_empty() {
                block.push('\n');
            }
            block.push_str(&format!("({label}: {})", titles.join(", ")));
        }
    };
    note("unchanged since last shown", &delta.unchanged);
    note("no longer applies", &delta.cleared);
    Some(block)
}

/// Add `more` to `text` as a paragraph of its own.
fn join(text: &mut String, more: &str) {
    if !text.is_empty() && !more.is_empty() {
        text.push_str("\n\n");
    }
    text.push_str(more);
}

impl Session {
    /// Inspect the project for a new turn of atelier's own loop: refresh the
    /// standing context (it goes in the system message) and return the
    /// context block to attach to the turn's user message, if anything
    /// changed since the last one.
    pub(super) fn gather_turn_context(&mut self) -> Option<String> {
        let (standing, mut volatile): (Vec<_>, Vec<_>) = self
            .context
            .iter()
            .filter_map(|p| p.gather(&self.root))
            .partition(|item| item.standing);
        self.standing = standing;
        // Plan mode is stated up front, so the model plans rather than
        // finding out one refused call at a time. It rides with the turn
        // (not the system message): the mode can change mid-session, and
        // the prompt cache should survive that.
        let was_told = self.last_ctx.contains_key(MODE_NOTE);
        if let Some(note) = self.mode.note(was_told) {
            volatile.push(ContextItem::new(MODE_NOTE, note, u8::MAX));
        }
        let mut delta = context::dedup_items(&self.last_ctx, volatile);
        self.last_ctx = std::mem::take(&mut delta.snapshot);
        turn_block(delta)
    }

    /// Open a turn: append the user's message to the conversation, with any
    /// staged images and the context gathered for it. What changed in the
    /// project since the last turn rides with this message, and stays
    /// there (see the module docs).
    ///
    /// A turn that failed before the model answered (the endpoint was
    /// down) left its prompt as the last message; the new prompt joins it
    /// (see [`push_user_text`](Self::push_user_text)).
    pub(super) fn push_user(&mut self, user_input: &str) {
        let context = self.gather_turn_context();
        let images = std::mem::take(&mut self.pending_images);
        if let Some(last) = self.history.last_mut().filter(|m| m.role == "user") {
            join(&mut last.content, user_input);
            last.images.extend(images);
            last.context = match (last.context.take(), context) {
                (Some(before), Some(now)) => Some(format!("{before}\n{now}")),
                (before, now) => before.or(now),
            };
            return;
        }
        let user = if images.is_empty() {
            Message::user(user_input)
        } else {
            Message::user_with_images(user_input, images)
        };
        self.history.push(user.with_context(context));
    }

    /// Append `text` to the conversation as the user's, mid-turn: what was
    /// queued while the turn ran, a subagent's order to wrap up.
    ///
    /// Never as a second user message in a row: strict chat templates
    /// refuse a conversation whose roles do not alternate (the request
    /// fails, and every later one with it, the two messages being there to
    /// stay), and the others show the model two prompts where one was
    /// meant. So after a user message — the turn's prompt, when something
    /// was queued before the first request; a prompt whose turn failed; the
    /// note carrying the images tools returned — `text` becomes a further
    /// paragraph of that message. That message has not been part of a
    /// request that was answered, so no cached prefix is rewritten.
    pub(super) fn push_user_text(&mut self, text: &str) {
        match self.history.last_mut().filter(|m| m.role == "user") {
            Some(last) => join(&mut last.content, text),
            None => self.history.push(Message::user(text)),
        }
    }

    /// The model's next request: a single leading system message (some
    /// servers reject a second), constant for the session, then the
    /// conversation history — so each request is a prefix of the next and
    /// the server's prompt cache carries over — and the tools on offer:
    /// none for a subagent that is `wrapping_up` at its step cap.
    pub(super) fn next_request(&self, wrapping_up: bool) -> (Vec<Message>, Vec<ToolSpec>) {
        let mut messages = Vec::with_capacity(self.history.len() + 1);
        messages.push(Message::system(self.system_message()));
        messages.extend(self.history.iter().cloned());
        let specs = if wrapping_up {
            Vec::new()
        } else {
            self.tools.specs()
        };
        (messages, specs)
    }

    /// The body of the first request `prompt` would make, as the active
    /// backend would send it — without sending it (`atelier debug prompt`).
    ///
    /// What shapes a prompt on its way in is applied as `send` applies it:
    /// the `user_prompt` hooks run (and may block it, or add context, as
    /// may `session_start`'s). Nothing is persisted, and the result holds
    /// no credential: a key or token travels in a header, never in a body.
    pub fn first_request(&mut self, prompt: &str, ui: &mut dyn Ui) -> Result<Value> {
        if self.is_claude() {
            bail!(
                "with the Claude backend, claude assembles the request itself: there is \
                 nothing of atelier's to print"
            );
        }
        let prompt = self.hook_prompt(prompt, ui)?;
        self.push_user(&prompt);
        let (messages, specs) = self.next_request(false);
        Ok(match &self.chatgpt {
            Some(cg) => {
                let (model, options) = self.cfg.model_and_options(&cg.model);
                provider::responses::build_body(model, &options, &messages, &specs)
            }
            None => {
                let target = self.cfg.target(crate::config::Role::Main);
                provider::chat_body(&target, &messages, &specs)
            }
        })
    }

    /// The system message for the next request (see [`system_message`]).
    pub(super) fn system_message(&self) -> String {
        // A subagent's session speaks from its agent's prompt, a standalone
        // run from its own.
        let prompt = match (&self.child, &self.standalone) {
            (Some(child), _) => child.system.as_str(),
            (None, Some(system)) => system.as_str(),
            (None, None) => SYSTEM_PROMPT,
        };
        system_message(prompt, &self.standing, self.summary.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn delta(prev: &[(&str, &str)], now: &[(&str, &str)]) -> context::Delta {
        let prev: HashMap<String, String> = prev
            .iter()
            .map(|(t, b)| (t.to_string(), b.to_string()))
            .collect();
        let items = now
            .iter()
            .map(|(t, b)| ContextItem::new(*t, *b, 50))
            .collect();
        context::dedup_items(&prev, items)
    }

    #[test]
    fn system_message_is_prompt_then_standing_then_summary() {
        let standing = vec![
            ContextItem::new("project instructions", "RULES", 250).standing(),
            ContextItem::new("memory", "INDEX", 240).standing(),
        ];
        let bare = system_message(SYSTEM_PROMPT, &[], None);
        assert_eq!(bare, SYSTEM_PROMPT);

        let full = system_message(SYSTEM_PROMPT, &standing, Some("SUMMARY"));
        let without_summary = system_message(SYSTEM_PROMPT, &standing, None);
        // A compaction changes only the end: what precedes stays cached.
        assert!(full.starts_with(&without_summary));
        assert!(without_summary.starts_with(SYSTEM_PROMPT));
        assert!(without_summary.contains("## project instructions\nRULES\n"));
        assert!(
            full.find("## project instructions").unwrap() < full.find("## memory").unwrap()
                && full.find("## memory").unwrap() < full.find("SUMMARY").unwrap()
        );
    }

    #[test]
    fn first_turn_block_carries_every_item() {
        let block = turn_block(delta(&[], &[("git status", "clean"), ("layout", "src/")])).unwrap();
        assert!(block.contains("## git status\nclean"));
        assert!(block.contains("## layout\nsrc/"));
        assert!(!block.contains("unchanged") && !block.contains("no longer"));
    }

    #[test]
    fn later_block_carries_only_changes_and_names_the_rest() {
        let block = turn_block(delta(
            &[("git status", "clean"), ("layout", "src/"), ("diff", "+x")],
            &[("git status", "M a.rs"), ("layout", "src/")],
        ))
        .unwrap();
        assert!(block.contains("## git status\nM a.rs"));
        assert!(!block.contains("## layout"));
        assert!(block.ends_with("(unchanged since last shown: layout)\n(no longer applies: diff)"));
    }

    #[test]
    fn nothing_changed_means_no_block() {
        let same = [("git status", "clean")];
        assert!(turn_block(delta(&same, &same)).is_none());
    }

    #[test]
    fn a_cleared_item_alone_still_gets_said() {
        let block = turn_block(delta(
            &[("git status", "clean"), ("diagnostics", "error")],
            &[("git status", "clean")],
        ))
        .unwrap();
        assert_eq!(
            block,
            "(unchanged since last shown: git status)\n(no longer applies: diagnostics)"
        );
    }
}
