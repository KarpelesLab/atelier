//! Compaction: making a conversation that outgrew the context window fit
//! again, losing as little as possible.
//!
//! It runs when the last request's real token usage passed the threshold
//! (after a turn), when the user asks (`/compact [instructions]`), and when
//! the endpoint refuses a request as too long (once per turn, then the
//! request is retried). It works in two stages, the second only if the first
//! isn't enough:
//!
//! 1. **Prune tool output.** The bulk of a coding session is file contents
//!    and command output that the model has already acted on. The bodies of
//!    tool results older than the recent tail are replaced with a line saying
//!    what they were (`[output of read src/x.rs — 412 lines, pruned]`). The
//!    calls, their arguments and the pairing with their results stay, so the
//!    model still knows what it did and can run a call again.
//! 2. **Summarize.** Everything before the recent tail is replaced with a
//!    structured hand-off summary (goal, decisions, state, files, failures,
//!    next steps, the user's own words), which joins the system message.
//!
//! The **recent tail** is kept verbatim in both stages. It is measured in
//! estimated tokens — a share of the threshold — rather than in messages,
//! since one message may be a line or a whole file; and the summary boundary
//! only ever falls on a user message, so no tool result is separated from
//! its call.
//!
//! Either stage rewrites history and so costs the inference server's prompt
//! cache once (see [`prompt`](super::prompt)); that is why pruning happens in
//! one batch when the threshold is passed, not a little on every turn.
//!
//! Token counts here are estimates ([`estimate_tokens`]). Where the endpoint
//! reported real usage for the last request, the estimate of the same
//! conversation calibrates them.

use std::collections::HashMap;
use std::ops::ControlFlow;

use anyhow::Result;
use serde_json::Value;

use super::{Session, Ui};
use crate::config::Role;
use crate::context::estimate_tokens;
use crate::provider::{Message, ToolCall, ToolSpec};

/// The share of the threshold kept verbatim as the recent tail.
const KEEP_TAIL_PERCENT: usize = 40;

/// What a compacted request should come down to at most, as a share of the
/// threshold: the tail shrinks when the fixed part of the request (system
/// message, tool list) is large, so that a compaction leaves room for a few
/// turns before the next one instead of recurring every turn.
const TARGET_PERCENT: usize = 70;

/// What an attached image is taken to cost. Its data URL says nothing about
/// it: servers bill an image by its size in tiles, not by its bytes.
const IMAGE_TOKENS: usize = 1_000;

/// Per-message cost of the role and framing around the text.
const MESSAGE_OVERHEAD_TOKENS: usize = 4;

/// How much of one tool result, and of one call's arguments, the transcript
/// given to the summarizer keeps: it needs the gist, and a transcript quoted
/// in full would not fit the window that just overflowed.
const EXCERPT_RESULT_CHARS: usize = 2_000;
const EXCERPT_ARGS_CHARS: usize = 500;

/// How much of a call's telling argument a pruned result's stub quotes.
const STUB_ARG_CHARS: usize = 80;

/// The text of the user message that carries images returned by tools (chat
/// completions tool results are text only).
pub(super) const TOOL_IMAGES_NOTE: &str = "[images returned by the tool calls above]";

/// What asked for the compaction; decides how far it goes.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Trigger {
    /// The last request passed the threshold: summarize only if pruning
    /// doesn't bring the conversation back under it.
    Auto,
    /// `/compact`: the user wants a summary now, however small the history.
    Manual,
    /// The endpoint refused the request as too long, mid-turn: the real
    /// window is smaller than estimated, so keep half the usual tail and do
    /// both stages — but never summarize the turn in progress.
    Overflow,
}

/// The estimated cost of one message in a request.
pub(super) fn message_tokens(m: &Message) -> usize {
    let calls: usize = m
        .tool_calls
        .iter()
        .map(|c| estimate_tokens(&c.name) + estimate_tokens(&c.arguments))
        .sum();
    MESSAGE_OVERHEAD_TOKENS
        + estimate_tokens(&m.wire_text())
        + calls
        + m.images.len() * IMAGE_TOKENS
}

/// The estimated cost of a run of messages.
pub(super) fn history_tokens(history: &[Message]) -> usize {
    history.iter().map(message_tokens).sum()
}

/// The estimated cost of the advertised tool list.
pub(super) fn specs_tokens(specs: &[ToolSpec]) -> usize {
    specs
        .iter()
        .map(|s| {
            estimate_tokens(&s.name)
                + estimate_tokens(&s.description)
                + estimate_tokens(&s.parameters.to_string())
        })
        .sum()
}

/// How many tokens of recent conversation to keep verbatim, given the
/// threshold and the fixed part of the request (see [`TARGET_PERCENT`]).
fn keep_tail(limit: usize, fixed: usize) -> usize {
    (limit * KEEP_TAIL_PERCENT / 100).min((limit * TARGET_PERCENT / 100).saturating_sub(fixed))
}

/// Where the recent tail starts: the smallest index whose suffix costs at
/// most `keep` tokens (`history.len()` if even the last message is larger).
fn tail_start(history: &[Message], keep: usize) -> usize {
    let mut used = 0;
    for (i, m) in history.iter().enumerate().rev() {
        used += message_tokens(m);
        if used > keep {
            return i + 1;
        }
    }
    0
}

/// Shortens `text` to `max` characters, saying how much was left out.
fn clip(text: &str, max: usize) -> String {
    let total = text.chars().count();
    if total <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}… [{} more characters]", total - max)
}

/// What a call was, in a few words: the tool and its most telling argument
/// (`read src/x.rs`, `bash cargo test`).
fn call_label(call: &ToolCall) -> String {
    const TELLING: &[&str] = &[
        "command",
        "pattern",
        "query",
        "url",
        "path",
        "file_path",
        "name",
    ];
    let args: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
    let arg = TELLING.iter().find_map(|k| args[*k].as_str());
    match arg {
        Some(arg) => {
            let line = arg.split_whitespace().collect::<Vec<_>>().join(" ");
            let short: String = line.chars().take(STUB_ARG_CHARS).collect();
            let ellipsis = if short.len() < line.len() { "…" } else { "" };
            format!("{} {short}{ellipsis}", call.name)
        }
        None => call.name.clone(),
    }
}

/// Whether a tool result is already the stub [`prune_tool_output`] leaves.
fn is_stub(content: &str) -> bool {
    content.starts_with("[output of ") && content.ends_with(", pruned]")
}

/// Stage 1: replace the bodies of tool results older than the recent tail
/// (`keep` tokens) with a one-line stub, and drop the images tools returned
/// there. Every message stays in place, so each call keeps its result.
/// Returns how many results were pruned.
pub(super) fn prune_tool_output(history: &mut [Message], keep: usize) -> usize {
    let old = tail_start(history, keep);
    let labels: HashMap<String, String> = history[..old]
        .iter()
        .flat_map(|m| &m.tool_calls)
        .map(|c| (c.id.clone(), call_label(c)))
        .collect();

    let mut pruned = 0;
    for m in &mut history[..old] {
        if m.role == "tool" && !is_stub(&m.content) {
            let label = m
                .tool_call_id
                .as_ref()
                .and_then(|id| labels.get(id))
                .map_or("a tool call", String::as_str);
            let stub = format!(
                "[output of {label} — {} lines, pruned]",
                m.content.lines().count()
            );
            // A result no longer than its stub is kept: nothing to gain.
            if stub.len() < m.content.len() {
                m.content = stub;
                pruned += 1;
            }
        } else if m.role == "user" && m.content == TOOL_IMAGES_NOTE && !m.images.is_empty() {
            m.content = format!(
                "[{} image(s) returned by the tool calls above, pruned]",
                m.images.len()
            );
            m.images.clear();
            pruned += 1;
        }
    }
    pruned
}

/// Stage 2's boundary: summarize `history[..i]`, keep `history[i..]`.
///
/// `i` is always a user message, so the kept part opens a turn and no tool
/// result is cut from its call: the first one inside the recent tail (`keep`
/// tokens), or, when the tail holds none (one long turn), the last one before
/// it — more is then kept than budgeted, which is the safe side. `None` when
/// that leaves nothing to summarize.
///
/// When everything fits in the tail, there is normally nothing to do; a
/// [`Manual`](Trigger::Manual) compaction still summarizes all but the last
/// turn (or all of a single-turn conversation), and an
/// [`Overflow`](Trigger::Overflow) one all but the turn in progress.
pub(super) fn split_point(history: &[Message], keep: usize, trigger: Trigger) -> Option<usize> {
    let n = history.len();
    let is_user = |i: &usize| history[*i].role == "user";
    let target = tail_start(history, keep);
    let by_budget = if target == 0 {
        None
    } else {
        (target..n)
            .find(is_user)
            .or_else(|| (0..target).rev().find(is_user))
    };
    let forced = || match trigger {
        Trigger::Auto => None,
        Trigger::Overflow => (0..n).rev().find(is_user),
        Trigger::Manual => (0..n).rev().find(is_user).filter(|&i| i > 0).or(Some(n)),
    };
    by_budget
        .filter(|&i| i > 0)
        .or_else(forced)
        .filter(|&i| i > 0)
}

/// What the summarizer is asked to be and to produce. Fixed sections, so
/// that what a session needs to continue is asked for by name rather than
/// left to what a model thinks a summary is.
const SUMMARY_SYSTEM: &str = "\
You are handing a software engineering session over to a colleague who has \
not seen it and will continue it from your notes alone. You are given the \
transcript of the session so far. Write the hand-off notes, with exactly \
these sections in this order, as Markdown headings; write \"none\" under a \
section that has nothing.

## Goal
What the user wants, in their terms, including constraints they set.

## Decisions
Each decision that was made, and why.

## Current state
What is done, what is half done, what is broken right now.

## Files touched
Each file read, created or changed that still matters, with what was done \
to it.

## Tried and failed
What was attempted and did not work, and why, so it is not tried again.

## Next steps
What remains to be done, in order.

## From the user
Anything the user said that must not be forgotten — preferences, \
corrections, instructions — verbatim where short.

Be specific: exact paths, names, commands, error messages and values. If \
the transcript opens with earlier hand-off notes, carry forward what still \
matters from them. No preamble and no closing remarks.";

/// One message of the transcript given to the summarizer.
fn render_message(m: &Message) -> String {
    let mut s = String::new();
    match m.role.as_str() {
        "tool" => s.push_str(&format!(
            "tool result: {}\n",
            clip(&m.content, EXCERPT_RESULT_CHARS)
        )),
        role if !m.content.is_empty() => s.push_str(&format!("{role}: {}\n", m.content)),
        _ => {}
    }
    if !m.images.is_empty() {
        s.push_str(&format!("[{} image(s) attached]\n", m.images.len()));
    }
    for c in &m.tool_calls {
        s.push_str(&format!(
            "assistant called {}({})\n",
            c.name,
            clip(&c.arguments, EXCERPT_ARGS_CHARS)
        ));
    }
    s
}

/// The transcript to summarize: the earlier summary, if any, then `msgs`.
/// It is itself held to `max_tokens` — the request that summarizes must fit
/// the window too — by leaving out the oldest messages and saying so.
fn render_excerpt(prev_summary: Option<&str>, msgs: &[Message], max_tokens: usize) -> String {
    let rendered: Vec<String> = msgs.iter().map(render_message).collect();
    let mut budget = max_tokens;
    let mut first = rendered.len();
    while first > 0 && estimate_tokens(&rendered[first - 1]) <= budget {
        first -= 1;
        budget -= estimate_tokens(&rendered[first]);
    }

    let mut s = String::new();
    if let Some(p) = prev_summary {
        s.push_str("Earlier hand-off notes (the session before this transcript):\n");
        s.push_str(p);
        s.push_str("\n\nTranscript since:\n");
    }
    if first > 0 {
        s.push_str(&format!("[{first} older message(s) left out for length]\n"));
    }
    for r in &rendered[first..] {
        s.push_str(r);
    }
    s
}

/// The request that turns `msgs` (and the summary before them) into the new
/// summary, with the user's own instructions for it, if any.
fn summary_request(
    prev_summary: Option<&str>,
    msgs: &[Message],
    instructions: Option<&str>,
    max_tokens: usize,
) -> Vec<Message> {
    let mut ask = render_excerpt(prev_summary, msgs, max_tokens);
    ask.push_str("\nWrite the hand-off notes for the session above.");
    if let Some(extra) = instructions.map(str::trim).filter(|s| !s.is_empty()) {
        ask.push_str(&format!(
            "\n\nThe user asks this of the notes in particular: {extra}"
        ));
    }
    vec![Message::system(SUMMARY_SYSTEM), Message::user(ask)]
}

/// `n` with thousands separators, for token counts.
pub(super) fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

impl Session {
    /// The estimated cost of what every request carries besides the
    /// conversation: the system message and the tool list.
    pub(super) fn fixed_tokens(&self) -> usize {
        estimate_tokens(&self.system_message()) + specs_tokens(&self.tools.specs())
    }

    /// After a turn: compact if the last request passed the threshold.
    pub(super) fn maybe_compact(&mut self, ui: &mut dyn Ui) {
        if self.usage_ctx > self.compact_threshold {
            self.compact(Trigger::Auto, None, ui);
        }
    }

    /// `/compact [instructions]`: compact now, and save the result.
    pub fn compact_now(&mut self, instructions: Option<&str>, ui: &mut dyn Ui) {
        if self.is_claude() {
            ui.info("Claude backend: claude compacts its own conversation; nothing to do here");
            return;
        }
        if self.compact(Trigger::Manual, instructions, ui) {
            self.persist();
        } else {
            ui.info("nothing to compact");
        }
    }

    /// Compact the conversation (see the module docs), reporting what was
    /// done and the estimated size before and after. Returns whether the
    /// history changed.
    pub(super) fn compact(
        &mut self,
        trigger: Trigger,
        instructions: Option<&str>,
        ui: &mut dyn Ui,
    ) -> bool {
        let limit = self.compact_threshold as usize;
        let fixed = self.fixed_tokens();
        let before = fixed + history_tokens(&self.history);
        let mut keep = keep_tail(limit, fixed);
        if trigger == Trigger::Overflow {
            keep /= 2;
        }

        // Hooks: `pre_compact` and `post_compact` watch (see docs/hooks.md).
        let fields = serde_json::json!({ "messages": self.history.len() });
        self.hooks
            .observe(crate::hooks::Event::PreCompact, fields, &mut |m| {
                ui.notice(m)
            });

        let pruned = prune_tool_output(&mut self.history, keep);
        // The estimator is rough; the endpoint's count for the conversation
        // as it was says by how much, so scale what is left by the same.
        let after_prune = fixed + history_tokens(&self.history);
        let projected = match self.usage_ctx as usize {
            0 => after_prune,
            real => after_prune * real / before.max(1),
        };

        let mut summarized = 0;
        if (trigger != Trigger::Auto || projected > limit)
            && let Some(split) = split_point(&self.history, keep, trigger)
        {
            let request = summary_request(
                self.summary.as_deref(),
                &self.history[..split],
                instructions,
                limit,
            );
            match self.summarize(&request) {
                Ok(summary) => {
                    self.summary = Some(summary);
                    self.history.drain(..split);
                    summarized = split;
                    // The context blocks of the summarized turns went with
                    // them: the next turn sends every item afresh.
                    self.last_ctx.clear();
                }
                // What pruning achieved stands; the rest is tried again the
                // next time the threshold is passed.
                Err(e) => ui.notice(&format!("compaction: summary failed: {e:#}")),
            }
        }
        if pruned == 0 && summarized == 0 {
            return false;
        }

        let fields = serde_json::json!({ "messages": self.history.len() });
        self.hooks
            .observe(crate::hooks::Event::PostCompact, fields, &mut |m| {
                ui.notice(m)
            });
        // Nested instruction files arrived in tool results that are now
        // pruned or summarized away: deliver them again.
        self.fstate.forget_dirs();
        // The old count no longer describes the conversation; the next
        // request's real usage decides whether to compact again.
        self.usage_ctx = 0;
        let mut done = Vec::new();
        if pruned > 0 {
            done.push(format!("pruned {pruned} old tool result(s)"));
        }
        if summarized > 0 {
            done.push(format!("summarized {summarized} earlier message(s)"));
        }
        let after = self.fixed_tokens() + history_tokens(&self.history);
        ui.info(&format!(
            "compacted: {} — ~{} → ~{} tokens (estimated)",
            done.join(", "),
            thousands(before),
            thousands(after)
        ));
        true
    }

    /// Run a summary request (no tools) on the model for summaries
    /// (`[models] summary`, else `small`, else the main one) and return the
    /// summary.
    fn summarize(&mut self, request: &[Message]) -> Result<String> {
        let completion =
            self.request_as(Role::Summary, request, &[], |_| ControlFlow::Continue(()))?;
        let summary = completion.content.trim().to_string();
        if summary.is_empty() {
            anyhow::bail!("model returned an empty summary");
        }
        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    /// One turn: the prompt, a `read` of `path` returning `lines` lines, the
    /// answer.
    fn turn(n: usize, path: &str, lines: usize) -> Vec<Message> {
        let id = format!("call_{n}");
        vec![
            Message::user(format!("prompt {n}")),
            Message::assistant_tool_calls(
                "",
                vec![call(&id, "read", &format!(r#"{{"path":"{path}"}}"#))],
            ),
            Message::tool_result(id, "a line of file content\n".repeat(lines)),
            Message::assistant(format!("answer {n}")),
        ]
    }

    fn roles(history: &[Message]) -> Vec<&str> {
        history.iter().map(|m| m.role.as_str()).collect()
    }

    #[test]
    fn old_tool_output_becomes_a_stub_and_the_tail_stays_verbatim() {
        let mut history = [turn(1, "src/a.rs", 400), turn(2, "src/b.rs", 400)].concat();
        let before = history.clone();
        // Room for the last turn's answer, not for its file.
        let keep = message_tokens(&history[7]) + 10;

        assert_eq!(prune_tool_output(&mut history, keep), 2);

        assert_eq!(
            history[2].content,
            "[output of read src/a.rs — 400 lines, pruned]"
        );
        assert_eq!(
            history[6].content,
            "[output of read src/b.rs — 400 lines, pruned]"
        );
        // Calls, results and their pairing are all still there.
        assert_eq!(roles(&history), roles(&before));
        assert_eq!(history[2].tool_call_id, before[2].tool_call_id);
        assert_eq!(history[1].tool_calls[0].arguments, r#"{"path":"src/a.rs"}"#);
        assert_eq!(history[7].content, "answer 2");
        // Nothing left to prune a second time.
        assert_eq!(prune_tool_output(&mut history, keep), 0);
    }

    #[test]
    fn output_inside_the_tail_is_not_pruned() {
        let mut history = [turn(1, "src/a.rs", 400), turn(2, "src/b.rs", 400)].concat();
        let keep = history_tokens(&history[4..]) + 10;

        assert_eq!(prune_tool_output(&mut history, keep), 1);
        assert!(is_stub(&history[2].content));
        assert_eq!(history[6].content.lines().count(), 400);

        let mut all_recent = turn(1, "src/a.rs", 400);
        assert_eq!(prune_tool_output(&mut all_recent, 1_000_000), 0);
    }

    #[test]
    fn short_results_and_unmatched_ones_are_handled() {
        let mut history = vec![
            Message::user("go"),
            Message::assistant_tool_calls("", vec![call("c1", "bash", r#"{"command":"true"}"#)]),
            Message::tool_result("c1", "ok"),
            Message::tool_result("gone", "x\n".repeat(100)),
            Message::user_with_images(TOOL_IMAGES_NOTE, vec!["data:a".into(), "data:b".into()]),
            Message::assistant("done"),
        ];
        assert_eq!(prune_tool_output(&mut history, 0), 2);
        assert_eq!(history[2].content, "ok", "shorter than its stub: kept");
        assert_eq!(
            history[3].content,
            "[output of a tool call — 100 lines, pruned]"
        );
        assert!(history[4].images.is_empty());
        assert_eq!(
            history[4].content,
            "[2 image(s) returned by the tool calls above, pruned]"
        );
    }

    #[test]
    fn call_labels_name_the_telling_argument() {
        assert_eq!(
            call_label(&call("1", "bash", r#"{"command":"cargo test\n  --all"}"#)),
            "bash cargo test --all"
        );
        assert_eq!(
            call_label(&call("1", "grep", r#"{"pattern":"fn main","path":"src"}"#)),
            "grep fn main"
        );
        assert_eq!(call_label(&call("1", "peers", "{}")), "peers");
        assert_eq!(call_label(&call("1", "odd", "not json")), "odd");
        let long = call_label(&call(
            "1",
            "bash",
            &format!(r#"{{"command":"{}"}}"#, "x".repeat(200)),
        ));
        assert_eq!(long.chars().count(), "bash ".len() + STUB_ARG_CHARS + 1);
    }

    #[test]
    fn the_split_keeps_a_tail_by_tokens_and_falls_on_a_user_message() {
        let history = [
            turn(1, "a.rs", 50),
            turn(2, "b.rs", 50),
            turn(3, "c.rs", 50),
        ]
        .concat();
        // Room for the last turn and a little of the one before: the tail
        // starts inside turn 2, so the boundary is turn 3's prompt.
        let keep = history_tokens(&history[8..]) + message_tokens(&history[7]) + 1;
        let i = split_point(&history, keep, Trigger::Auto).expect("a split");
        assert_eq!(i, 8);
        assert_eq!(history[i].content, "prompt 3");

        // Room for exactly two turns: the boundary is turn 2's prompt.
        let keep = history_tokens(&history[4..]);
        assert_eq!(split_point(&history, keep, Trigger::Auto), Some(4));
    }

    #[test]
    fn a_long_last_turn_is_kept_whole_rather_than_cut() {
        let history = [turn(1, "a.rs", 50), turn(2, "b.rs", 5_000)].concat();
        // The tail budget ends inside turn 2, after its prompt: no user
        // message in the tail, so the boundary is that prompt.
        let keep = message_tokens(&history[7]) + 1;
        assert_eq!(split_point(&history, keep, Trigger::Auto), Some(4));
        // A single turn can't be split at all — pruning is what helps there.
        let single = turn(1, "a.rs", 5_000);
        assert_eq!(split_point(&single, 10, Trigger::Auto), None);
        assert_eq!(split_point(&single, 10, Trigger::Overflow), None);
    }

    #[test]
    fn a_conversation_that_fits_the_tail_is_only_summarized_when_asked() {
        let history = [turn(1, "a.rs", 5), turn(2, "b.rs", 5)].concat();
        let keep = 1_000_000;
        assert_eq!(split_point(&history, keep, Trigger::Auto), None);
        // On request: everything but the last turn …
        assert_eq!(split_point(&history, keep, Trigger::Manual), Some(4));
        // … or all of a one-turn conversation.
        assert_eq!(split_point(&history[..4], keep, Trigger::Manual), Some(4));
        assert_eq!(split_point(&[], keep, Trigger::Manual), None);
        // Mid-turn, the turn in progress is never summarized.
        assert_eq!(split_point(&history, keep, Trigger::Overflow), Some(4));
        assert_eq!(split_point(&history[..3], keep, Trigger::Overflow), None);
    }

    #[test]
    fn the_tail_shrinks_when_the_fixed_part_is_large() {
        assert_eq!(keep_tail(10_000, 1_000), 4_000);
        assert_eq!(keep_tail(10_000, 5_000), 2_000);
        assert_eq!(keep_tail(10_000, 9_000), 0);
    }

    #[test]
    fn the_summary_request_asks_for_the_hand_off_sections() {
        let history = turn(1, "src/a.rs", 3);
        let request = summary_request(
            Some("EARLIER NOTES"),
            &history,
            Some("  keep the port  "),
            10_000,
        );
        assert_eq!(roles(&request), ["system", "user"]);
        for section in [
            "## Goal",
            "## Decisions",
            "## Current state",
            "## Files touched",
            "## Tried and failed",
            "## Next steps",
            "## From the user",
        ] {
            assert!(request[0].content.contains(section), "{section}");
        }
        let ask = &request[1].content;
        assert!(ask.starts_with("Earlier hand-off notes"));
        assert!(ask.contains("EARLIER NOTES"));
        assert!(ask.contains("user: prompt 1"));
        assert!(ask.contains(r#"assistant called read({"path":"src/a.rs"})"#));
        assert!(ask.contains("tool result: a line of file content"));
        assert!(ask.contains("assistant: answer 1"));
        assert!(ask.ends_with("The user asks this of the notes in particular: keep the port"));

        let plain = summary_request(None, &history, None, 10_000);
        assert!(plain[1].content.starts_with("user: prompt 1"));
        assert!(plain[1].content.ends_with("for the session above."));
    }

    #[test]
    fn the_transcript_is_clipped_to_fit_the_window() {
        let mut history = turn(1, "a.rs", 2_000);
        let excerpt = render_excerpt(None, &history, 1_000_000);
        assert!(excerpt.contains("more characters]"), "long results are cut");
        assert!(excerpt.len() < 3_000 + EXCERPT_RESULT_CHARS);

        history.extend(turn(2, "b.rs", 1));
        let budget = estimate_tokens(&render_message(&history[7])) + 1;
        let tight = render_excerpt(Some("NOTES"), &history, budget);
        assert!(tight.contains("NOTES"));
        assert!(tight.contains("[7 older message(s) left out for length]"));
        assert!(tight.contains("answer 2") && !tight.contains("prompt 1"));
    }

    /// Records what a compaction reports.
    #[derive(Default)]
    struct Rec {
        info: Vec<String>,
        notices: Vec<String>,
    }
    impl Ui for Rec {
        fn reasoning(&mut self, _: &str) {}
        fn content(&mut self, _: &str) {}
        fn tool_start(&mut self, _: &str, _: &str) {}
        fn tool_end(&mut self, _: &str, _: &str, _: bool) {}
        fn info(&mut self, text: &str) {
            self.info.push(text.to_string());
        }
        fn ask_approval(&mut self, _: &str, _: &str) -> super::super::Approval {
            super::super::Approval::Deny
        }
        fn notice(&mut self, text: &str) {
            self.notices.push(text.to_string());
        }
    }

    /// A session with no tools and no context providers, whose endpoint
    /// nothing listens on: a summary request would fail, and say so.
    fn session(threshold: u32) -> Session {
        let cfg = crate::config::Config::single(
            crate::provider::Provider::from_env("http://127.0.0.1:9/v1".into(), None),
            "m".into(),
        );
        let mut session = Session::new(
            cfg,
            std::env::temp_dir().join("atl-compact-unused"),
            crate::tools::ToolRegistry::new(),
            Vec::new(),
            crate::settings::Settings::default(),
        );
        session.compact_threshold = threshold;
        session
    }

    /// Stage 1 alone, when it is enough: the old file contents go, nothing
    /// is summarized (no request is made), and the sizes are reported.
    #[test]
    fn pruning_that_suffices_is_not_followed_by_a_summary() {
        let mut s = session(10_000);
        s.history = [turn(1, "src/big.rs", 2_000), turn(2, "src/b.rs", 5)].concat();
        s.usage_ctx = 12_000;
        s.last_ctx.insert("git status".into(), "clean".into());
        let mut ui = Rec::default();

        s.maybe_compact(&mut ui);

        assert!(ui.notices.is_empty(), "{:?}", ui.notices);
        assert_eq!(ui.info.len(), 1, "{:?}", ui.info);
        assert!(
            ui.info[0].starts_with("compacted: pruned 1 old tool result(s) — ~"),
            "{}",
            ui.info[0]
        );
        assert!(ui.info[0].ends_with("tokens (estimated)"));
        assert!(s.summary.is_none());
        assert_eq!(s.history.len(), 8);
        assert_eq!(
            s.history[2].content,
            "[output of read src/big.rs — 2000 lines, pruned]"
        );
        assert_eq!(s.usage_ctx, 0, "the next request's usage decides anew");
        assert!(
            !s.last_ctx.is_empty(),
            "no turn was dropped, so its context still stands"
        );

        // Under the threshold, nothing happens at all.
        s.usage_ctx = 9_000;
        s.maybe_compact(&mut ui);
        assert_eq!(ui.info.len(), 1);
    }

    /// With nothing to prune and nothing old enough to summarize, an
    /// automatic compaction leaves the conversation alone, silently.
    #[test]
    fn a_small_conversation_is_left_alone() {
        let mut s = session(10_000);
        s.history = turn(1, "a.rs", 5);
        s.usage_ctx = 12_000;
        let mut ui = Rec::default();
        assert!(!s.compact(Trigger::Auto, None, &mut ui));
        assert!(ui.info.is_empty() && ui.notices.is_empty());
        assert_eq!(s.history.len(), 4);
    }

    #[test]
    fn thousands_are_separated() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }
}
