//! Goals: keep working until a stated condition holds.
//!
//! A model stops when *it* thinks it is done. `/goal <condition>` (or
//! `--goal` with `--print`) gives the session a condition of the user's —
//! "the test suite passes", "every TODO in src/ is resolved" — and from then
//! on the end of a turn is not the end of the work: a judge is asked whether
//! the condition holds, and if it does not, its reason goes back to the
//! agent as the next message (`[goal not yet met] …`) and the loop goes on.
//!
//! # The judge
//!
//! One model request with no tools: the goal, and the final part of the
//! transcript ([`TRANSCRIPT_BYTES`] of it — what was just done and said is
//! what decides, and the request must stay small). It answers strict JSON,
//! `{"met": bool, "reason": "…"}`. It is a second opinion on the agent's
//! claim that it is done, not an oracle: it sees the transcript, not the
//! project, so a goal should be one the agent's own work makes visible
//! ("run the tests and they pass"). An answer that is not the JSON asked
//! for fails the prompt rather than counting as either verdict — guessing
//! "met" would end the work unverified, guessing "not met" would spend the
//! budget on a judge that cannot be read.
//!
//! With the Claude backend there is no endpoint of atelier's to ask: the
//! question is put to claude itself, as a turn of the conversation whose
//! output is not shown. It is then the agent judging its own work — weaker,
//! but claude can check the condition with its tools, which the other
//! judge cannot.
//!
//! # Bounds
//!
//! A condition that cannot be met must not run forever. For each prompt the
//! user sends, the agent is sent back to work at most `goals.max_rounds`
//! times (default [`DEFAULT_MAX_ROUNDS`]) and, if `goals.max_tokens` is set,
//! only while the prompt has generated fewer output tokens than that (the
//! judge's included). Past a bound the turn ends with a notice and the goal
//! stays set: the next prompt pursues it again with a fresh budget, so the
//! user decides whether to keep paying. Ctrl-C stops the turn as always —
//! the loop in `run_turn` returns before the judge is asked — which pauses
//! the goal in the same way. `--max-turns` still counts every request of
//! the agent's across the rounds; the judge's own are not steps.
//!
//! # Where it sits
//!
//! In `Session::run_turn`, after the `turn_end` hooks: a hook that sends the
//! agent back (tests failing, say) is the cheaper and more exact check, so
//! it goes first, and the judge is only asked about an answer the hooks let
//! stand. A subagent's session never has a goal — its task is its parent's
//! to judge — and neither does a standalone run.

use std::ops::ControlFlow;

use anyhow::{Context, Result, anyhow};
use serde_json::Value;

use super::{Approval, Session, Ui};
use crate::provider::Message;

/// How many times a prompt may be sent back to work when `[goals]` does not
/// say.
pub const DEFAULT_MAX_ROUNDS: u32 = 10;

/// The most of the transcript the judge is given, in bytes, taken from its
/// end.
const TRANSCRIPT_BYTES: usize = 12 * 1024;

/// The most of one message's text in that transcript: a tool result can be
/// a whole file, and its last lines (the test summary, the error) are what
/// a verdict turns on.
const MESSAGE_BYTES: usize = 3 * 1024;

const JUDGE_PROMPT: &str = "\
You judge whether a coding agent has reached a goal its user set. You are \
given the goal and the final part of the agent's transcript: what it did, \
what its tools returned, and what it said last.

Decide from the evidence in the transcript, not from the agent's say-so: a \
claim that the tests pass counts only if the transcript shows them passing. \
If the evidence is missing or ambiguous, the goal is not met, and your \
reason says what is still needed to show it.

Answer with one JSON object and nothing else: {\"met\": true or false, \
\"reason\": \"one or two sentences\"}. When the goal is not met, the reason \
is sent to the agent as its next instruction, so say what remains to be \
done.";

/// What the Claude backend's agent is asked, in its own conversation.
const SELF_CHECK: &str = "\
[goal check] Before this turn can end: the user set a goal for this session, \
below. Decide whether it holds right now, from what you have verified — \
check it with your tools if you have not. Do no other work in this message. \
Answer with one JSON object and nothing else: {\"met\": true or false, \
\"reason\": \"one or two sentences; when not met, what remains to be done\"}.";

/// A session's goal.
pub(super) struct Goal {
    condition: String,
    /// Why the last prompt stopped pursuing it, when a bound was reached
    /// with the condition still unmet.
    gave_up: Option<String>,
}

/// One prompt's pursuit of the goal: what the bounds are measured from.
pub(super) struct Pursuit {
    /// Times the agent was sent back to work.
    rounds: u32,
    /// The session's output-token count when the prompt started.
    tokens_before: u64,
}

/// The judge's answer.
#[derive(Debug, PartialEq)]
struct Verdict {
    met: bool,
    reason: String,
}

impl Session {
    /// Set the session's goal (`None` removes it).
    pub fn set_goal(&mut self, condition: Option<String>) {
        self.goal = condition.map(|condition| Goal {
            condition,
            gave_up: None,
        });
    }

    /// Why the last prompt ended with the goal unmet, if it stopped at a
    /// bound: what a headless run reports instead of success.
    pub fn goal_gave_up(&self) -> Option<&str> {
        self.goal.as_ref()?.gave_up.as_deref()
    }

    /// The most a prompt is sent back to work, and its output-token budget.
    fn goal_bounds(&self) -> (u32, Option<u64>) {
        let goals = &self.settings.goals;
        (
            goals.max_rounds.unwrap_or(DEFAULT_MAX_ROUNDS),
            goals.max_tokens,
        )
    }

    /// `/goal` (show), `/goal clear`, `/goal <condition>` (set).
    pub(super) fn goal_command(&mut self, argument: &str, ui: &mut dyn Ui) {
        let (rounds, tokens) = self.goal_bounds();
        let bounds = match tokens {
            Some(t) => format!("at most {rounds} round(s) and {t} output tokens per prompt"),
            None => format!("at most {rounds} round(s) per prompt"),
        };
        match argument {
            "" => match &self.goal {
                Some(goal) => {
                    ui.info(&format!("goal: {}", goal.condition));
                    ui.info(&format!(
                        "checked each time a turn ends ({bounds}); /goal clear removes it"
                    ));
                }
                None => ui.info("no goal set — /goal <condition> sets one"),
            },
            "clear" => match self.goal.take() {
                Some(_) => ui.info("goal cleared"),
                None => ui.info("no goal set"),
            },
            condition => {
                self.set_goal(Some(condition.to_string()));
                ui.info(&format!("goal set: {condition}"));
                ui.info(&format!(
                    "from your next prompt on, the agent is sent back to work until it holds \
                     ({bounds})"
                ));
            }
        }
    }

    /// Start a prompt's pursuit of the goal, with a fresh budget.
    pub(super) fn begin_pursuit(&mut self) -> Pursuit {
        if let Some(goal) = &mut self.goal {
            goal.gave_up = None;
        }
        Pursuit {
            rounds: 0,
            tokens_before: self.usage_out,
        }
    }

    /// The turn has ended with `answer`, and the hooks let it stand: if a
    /// goal is set, ask the judge. `Some` is the message that sends the
    /// agent back to work; `None` ends the prompt — no goal, the goal is
    /// met (and cleared), a bound was reached, or the user stopped it.
    pub(super) fn pursue_goal(
        &mut self,
        pursuit: &mut Pursuit,
        answer: &str,
        ui: &mut dyn Ui,
    ) -> Result<Option<String>> {
        let Some(condition) = self.goal.as_ref().map(|g| g.condition.clone()) else {
            return Ok(None);
        };
        let Some(verdict) = self.judge(&condition, answer, ui)? else {
            return Ok(None);
        };
        if verdict.met {
            ui.info(&format!("✓ goal met: {}", verdict.reason));
            self.goal = None;
            return Ok(None);
        }

        let (max_rounds, max_tokens) = self.goal_bounds();
        let spent = self.usage_out.saturating_sub(pursuit.tokens_before);
        let bound = if pursuit.rounds >= max_rounds {
            Some(format!("after {max_rounds} round(s)"))
        } else {
            max_tokens
                .filter(|max| spent >= *max)
                .map(|max| format!("within {max} output tokens ({spent} used)"))
        };
        if let Some(bound) = bound {
            let why = format!("the goal is not met {bound}: {}", verdict.reason);
            ui.notice(&format!(
                "{why} — stopping here; the goal stays set (/goal clear removes it)"
            ));
            if let Some(goal) = &mut self.goal {
                goal.gave_up = Some(why);
            }
            return Ok(None);
        }
        pursuit.rounds += 1;
        ui.info(&format!("↻ goal not yet met: {}", verdict.reason));
        Ok(Some(format!("[goal not yet met] {}", verdict.reason)))
    }

    /// Ask whether `condition` holds. `None` when the user stopped the
    /// request; an error when the judge cannot be reached or understood.
    fn judge(&mut self, condition: &str, answer: &str, ui: &mut dyn Ui) -> Result<Option<Verdict>> {
        let reply = if self.is_claude() {
            // claude's transcript is its own; `answer` is all atelier has
            // of it, and claude does not need to be told what it just said.
            let question = format!("{SELF_CHECK}\n\nGoal: {condition}");
            self.run_once(&question, &mut Unseen(ui))
                .context("checking the goal")?;
            if ui.interrupted() {
                return Ok(None);
            }
            self.last_answer()
        } else {
            let question = format!(
                "Goal: {condition}\n\nThe final part of the transcript:\n\n{}\n\n\
                 Is the goal met?",
                transcript_tail(&self.history, answer)
            );
            let messages = [Message::system(JUDGE_PROMPT), Message::user(question)];
            let completion = self
                // A yes/no reading of a transcript: the small model's job,
                // where one is configured (it falls back to the main one).
                .request_as(crate::config::Role::Small, &messages, &[], |_| {
                    if ui.interrupted() {
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    }
                })
                .context("checking the goal")?;
            // Counted with the session's output, and so against the goal's
            // own token budget — but not as the conversation's size.
            if let Some(usage) = &completion.usage {
                self.usage_out += u64::from(usage.completion_tokens);
            }
            if completion.interrupted {
                return Ok(None);
            }
            completion.content
        };
        parse_verdict(&reply)
            .map(Some)
            .context("checking the goal: the judge's answer could not be used")
    }
}

/// The judge's verdict in `reply`: the JSON object it was asked for, with
/// or without a code fence or a sentence around it.
fn parse_verdict(reply: &str) -> Result<Verdict> {
    let object = reply
        .find('{')
        .zip(reply.rfind('}'))
        .and_then(|(start, end)| reply.get(start..=end))
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .ok_or_else(|| anyhow!("not a JSON object: {}", excerpt(reply)))?;
    let met = object["met"]
        .as_bool()
        .ok_or_else(|| anyhow!("no boolean \"met\" in {}", excerpt(reply)))?;
    let reason = match object["reason"].as_str().map(str::trim) {
        Some(reason) if !reason.is_empty() => reason.to_string(),
        // A verdict is usable without one; the agent is still told
        // something it can act on.
        _ if met => "the condition holds".to_string(),
        _ => "the condition does not hold yet; keep working toward the goal".to_string(),
    };
    Ok(Verdict { met, reason })
}

/// The start of a reply that could not be used, for the error message.
fn excerpt(reply: &str) -> String {
    let line: String = reply.trim().chars().take(120).collect();
    if line.is_empty() {
        "(an empty answer)".to_string()
    } else {
        format!("{line:?}")
    }
}

/// The end of the conversation as text for the judge: the most recent
/// messages that fit [`TRANSCRIPT_BYTES`], oldest first, each cut to its
/// last [`MESSAGE_BYTES`]. `answer` stands in when there is no history to
/// show.
fn transcript_tail(history: &[Message], answer: &str) -> String {
    let mut kept: Vec<String> = Vec::new();
    let mut size = 0;
    for message in history.iter().rev() {
        let mut text = format!(
            "{}: {}",
            message.role,
            tail(&message.content, MESSAGE_BYTES)
        );
        for call in &message.tool_calls {
            text.push_str(&format!(
                "\n[tool call] {}({})",
                call.name,
                tail(&call.arguments, MESSAGE_BYTES)
            ));
        }
        // The newest message is always shown, whatever its size.
        if !kept.is_empty() && size + text.len() > TRANSCRIPT_BYTES {
            kept.push("[earlier messages are not shown]".to_string());
            break;
        }
        size += text.len();
        kept.push(text);
    }
    if kept.is_empty() {
        return format!("assistant: {}", tail(answer, MESSAGE_BYTES));
    }
    kept.reverse();
    kept.join("\n\n")
}

/// The last `max` bytes of `text` (on a character boundary), marked when
/// the start was dropped.
fn tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("[…] {}", &text[start..])
}

/// The [`Ui`] of the Claude backend's goal check: a turn of the
/// conversation the user did not ask for and need not read. Its text and
/// tool activity are not shown; what must reach the user still does — an
/// approval prompt, a notice — and a stop request is still heard.
struct Unseen<'a>(&'a mut dyn Ui);

impl Ui for Unseen<'_> {
    fn reasoning(&mut self, _text: &str) {}
    fn content(&mut self, _text: &str) {}
    fn tool_start(&mut self, _name: &str, _arguments: &str) {}
    fn tool_end(&mut self, _name: &str, _result: &str, _ok: bool) {}
    fn info(&mut self, _text: &str) {}
    fn ask_approval(&mut self, tool: &str, arguments: &str) -> Approval {
        self.0.ask_approval(tool, arguments)
    }
    fn unattended(&mut self) -> bool {
        self.0.unattended()
    }
    fn approval_reviewed(&mut self, review: &crate::permissions::reviewer::Record) {
        self.0.approval_reviewed(review);
    }
    fn notice(&mut self, text: &str) {
        self.0.notice(text);
    }
    fn interrupted(&mut self) -> bool {
        self.0.interrupted()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ToolCall;

    #[test]
    fn verdicts_are_read_through_fences_and_prose() {
        assert_eq!(
            parse_verdict(r#"{"met": true, "reason": " all 12 tests pass "}"#).unwrap(),
            Verdict {
                met: true,
                reason: "all 12 tests pass".into()
            }
        );
        let fenced = "Here:\n```json\n{\"met\": false, \"reason\": \"2 tests fail\"}\n```";
        assert_eq!(
            parse_verdict(fenced).unwrap(),
            Verdict {
                met: false,
                reason: "2 tests fail".into()
            }
        );
        // No reason: still a verdict, with something the agent can act on.
        assert!(parse_verdict(r#"{"met": true}"#).unwrap().met);
        let bare = parse_verdict(r#"{"met": false, "reason": ""}"#).unwrap();
        assert!(!bare.met && bare.reason.contains("keep working"));
    }

    #[test]
    fn unusable_verdicts_are_errors_not_guesses() {
        let err = |reply: &str| parse_verdict(reply).unwrap_err().to_string();
        assert!(err("Yes, the goal is met.").contains("not a JSON object"));
        assert!(err("").contains("an empty answer"));
        assert!(err(r#"{"met": "yes"}"#).contains("no boolean \"met\""));
        assert!(err(r#"{"reason": "done"}"#).contains("no boolean \"met\""));
    }

    #[test]
    fn the_judge_sees_the_end_of_the_transcript() {
        assert_eq!(transcript_tail(&[], "all done"), "assistant: all done");

        let call = ToolCall {
            id: "1".into(),
            name: "bash".into(),
            arguments: r#"{"command":"cargo test"}"#.into(),
        };
        let long = "x".repeat(MESSAGE_BYTES * 2);
        let history = [
            Message::user("the very first prompt"),
            Message::assistant(long.clone()),
            Message::user(long.clone()),
            Message::assistant(long.clone()),
            Message::user(long),
            Message::assistant_tool_calls("running them", vec![call]),
            Message::tool_result("1", format!("{}test result: ok", "y".repeat(5000))),
            Message::assistant("they pass"),
        ];
        let text = transcript_tail(&history, "they pass");
        // Two of the long messages fit beside the tool result; not three.
        assert!(text.starts_with("[earlier messages are not shown]\n\nassistant: […] xxx"));
        assert_eq!(text.matches("[…] xxx").count(), 2);
        assert!(!text.contains("the very first prompt"));
        assert!(
            text.contains(
                "assistant: running them\n[tool call] bash({\"command\":\"cargo test\"})"
            )
        );
        // A long result keeps its end, where the outcome is.
        assert!(text.contains("tool: […] yyy"));
        assert!(text.ends_with("test result: ok\n\nassistant: they pass"));

        // The newest message is shown even when it alone is over the cap.
        let one = [Message::user("z".repeat(TRANSCRIPT_BYTES * 2))];
        assert!(transcript_tail(&one, "").starts_with("user: […] zzz"));
        assert_eq!(tail("aé", 1), "[…] ");
    }
}
