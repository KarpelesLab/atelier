//! The terminal interface as the host of a shared session: what it does
//! for the remotes attached to it (see [`crate::own::remote`] for the
//! other side of each call, and `docs/sessions.md` for what a remote is).
//!
//! The interface stays what it was — one thread at the keyboard and the
//! screen, the session lent to a worker for the length of a turn — and
//! looks in one more place wherever it looks for a key: the remotes'
//! [inbox](remote::inbox). A line a remote typed is served as a line typed
//! here is ([`at_idle`] hands it to the main loop), a message for the
//! running turn joins the queue the keyboard fills ([`in_turn`]), and what
//! needs the session without being a line — a query, a setting — is
//! answered on the spot. Everything the interface draws it also reports
//! (`remote::event`, next to each draw), and what it asks its user it asks
//! everybody ([`approval_from`], [`answer_from`]).

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;

use serde_json::{Value, json};

use super::{PROMPT, Renderer, TuiUi, build_status};
use crate::agent::{Approval, Session};
use crate::events;
use crate::own::remote::{self, Answered, Asked, Inbound, Snapshot};
use crate::rpc::{self, Failure, Id};
use crate::tools::ask::{Answer, Question};

/// A line a remote sent, to serve as one typed here — and the request to
/// answer once it is known what it was.
pub(super) struct FromRemote {
    pub(super) remote: u64,
    pub(super) id: Id,
    pub(super) how: How,
}

pub(super) enum How {
    /// `turn/start`: a prompt, whatever it looks like, with these images.
    Prompt(Vec<String>),
    /// `command/run`: a line that may be a command. `start`: a prompt is
    /// sent rather than handed back.
    Line { start: bool },
}

impl FromRemote {
    pub(super) fn respond(&self, result: Value) {
        remote::respond(self.remote, &self.id, result);
    }
}

/// The session as it is now, for what remotes are answered without it.
pub(super) fn snapshot(session: &Session) -> Snapshot {
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut describe = crate::serve::describe(session, &cwd);
    describe["tools"] = Value::from(session.tool_names());
    Snapshot {
        describe,
        status: crate::serve::status(session, &cwd),
        history: session.transcript(),
        stored: !session.is_claude(),
        claude_session: session.claude_session().map(str::to_string),
    }
}

pub(super) fn publish(session: &Session) {
    remote::publish(|| snapshot(session));
}

/// One line of the transcript for a remote that came or went.
fn came_or_went(r: &mut Renderer, inbound: &Inbound) -> bool {
    match inbound {
        Inbound::Joined { remote, name } => {
            r.push_line(format!("· remote {remote} attached ({name})"), true);
        }
        Inbound::Left { remote } => r.push_line(format!("· remote {remote} detached"), true),
        _ => return false,
    }
    true
}

/// What remotes asked for while the session is idle. Returns the first
/// line among it that is to be served as typed; what follows it waits in
/// `r.inbox` for the next look.
pub(super) fn at_idle(
    r: &mut Renderer,
    session: &mut Session,
    turn: u32,
) -> Option<(String, FromRemote)> {
    r.inbox.extend(remote::inbox());
    while let Some(inbound) = r.inbox.pop_front() {
        if came_or_went(r, &inbound) {
            let _ = r.refresh();
            continue;
        }
        match inbound {
            Inbound::Prompt {
                remote,
                id,
                text,
                images,
            } => {
                let how = How::Prompt(images);
                return Some((text, FromRemote { remote, id, how }));
            }
            Inbound::Line {
                remote,
                id,
                line,
                start,
            } => {
                let how = How::Line { start };
                return Some((line, FromRemote { remote, id, how }));
            }
            // The turn it was for ended first: it is a prompt of its own,
            // as a line still queued here at the end of a turn is.
            Inbound::Steer { remote, text } => {
                r.enqueue_from(text, remote);
                let _ = r.refresh();
            }
            Inbound::Interrupt { .. } => {}
            // Free again before the answer goes, so that whoever reads it
            // can ask for the next thing at once.
            Inbound::Query { remote, id, query } => {
                let result = query.answer(session);
                remote::release();
                remote::respond(remote, &id, result);
            }
            Inbound::Settings {
                remote,
                id,
                set,
                params,
            } => {
                let root = std::env::current_dir().unwrap_or_default();
                let result = if set {
                    crate::settings_api::set(session, &root, &params)
                } else {
                    Ok(crate::settings_api::get(session, &root))
                };
                if set {
                    // A setting changes what the strip shows, and what the
                    // remotes are told the session is.
                    r.status = build_status(session, turn);
                    publish(session);
                    let _ = r.refresh();
                }
                remote::release();
                match result {
                    Ok(result) => remote::respond(remote, &id, result),
                    Err(failure) => remote::fail(remote, &id, failure),
                }
            }
            Inbound::Joined { .. } | Inbound::Left { .. } => {}
        }
    }
    None
}

/// What remotes asked for while a turn runs: messages for it join the
/// queue, an interrupt is Ctrl-C, and what needs the session is refused —
/// the session is the turn's.
pub(super) fn in_turn(
    r: &mut Renderer,
    answers: &Sender<Approval>,
    replies: &Sender<Answer>,
) -> bool {
    let inbound: VecDeque<Inbound> = remote::inbox().into();
    let any = !inbound.is_empty();
    for inbound in inbound {
        if came_or_went(r, &inbound) {
            continue;
        }
        match inbound {
            Inbound::Steer { remote, text } => r.enqueue_from(text, remote),
            Inbound::Interrupt { remote } => {
                // What the turn waits on is answered as Ctrl-C answers it.
                if std::mem::take(&mut r.approval_pending) {
                    let _ = answers.send(Approval::Deny);
                }
                if r.question.take().is_some() {
                    let _ = replies.send(Answer::Declined);
                }
                if !r.stop.swap(true, Ordering::Relaxed) {
                    r.commit_pending_if_any();
                    r.push_line(format!("! stopping… (asked at remote {remote})"), true);
                }
            }
            Inbound::Prompt { remote, id, .. }
            | Inbound::Line { remote, id, .. }
            | Inbound::Query { remote, id, .. }
            | Inbound::Settings { remote, id, .. } => remote::fail_busy(remote, &id),
            Inbound::Joined { .. } | Inbound::Left { .. } => {}
        }
    }
    any
}

/// The line a remote typed, as the owner's transcript shows it.
pub(super) fn echo(r: &mut Renderer, line: &str, remote: u64) {
    r.push_line(format!("{PROMPT}{line}  ← remote {remote}"), false);
}

/// Refuse a remote's line that could not be served.
pub(super) fn refuse(from: &FromRemote, why: String) {
    let failure = Failure::new(rpc::code::INTERNAL_ERROR, why);
    remote::release();
    remote::fail(from.remote, &from.id, failure);
}

pub(super) fn decision(approval: &Approval) -> &'static str {
    match approval {
        Approval::Once => "once",
        Approval::Always => "always",
        Approval::Deny => "deny",
    }
}

/// How an approval went, in words for the transcript.
pub(super) fn verdict(approval: &Approval) -> &'static str {
    match approval {
        Approval::Once => "allowed once",
        Approval::Always => "allowed, and from now on",
        Approval::Deny => "denied",
    }
}

/// The `approval/request` for a call, as `serve` sends it.
pub(super) fn approval_params(tool: &str, arguments: &str) -> Value {
    json!({
        "tool": tool,
        "arguments": events::arguments(arguments),
        "risk": crate::risk::signals(tool, arguments),
    })
}

/// A remote's answer to an approval put to all, if one has come: the first
/// that is a decision. An answer that is none is no answer; the prompt
/// stays up for everybody.
pub(super) fn approval_from(asked: &Asked) -> Option<(u64, Approval)> {
    while let Ok((remote, answer)) = asked.answers.try_recv() {
        let Answered::Result(answer) = answer else {
            continue;
        };
        let approval = match answer["decision"].as_str() {
            Some("once") => Approval::Once,
            Some("always") => Approval::Always,
            Some("deny") => Approval::Deny,
            _ => continue,
        };
        return Some((remote, approval));
    }
    None
}

/// The same for a question.
pub(super) fn answer_from(asked: &Asked, question: &Question) -> Option<(u64, Answer)> {
    while let Ok((remote, answer)) = asked.answers.try_recv() {
        let Answered::Result(answer) = answer else {
            continue;
        };
        if answer["declined"] == true {
            return Some((remote, Answer::Declined));
        }
        if let Ok(answer) = events::answer(&answer, question) {
            return Some((remote, answer));
        }
    }
    None
}

/// An answer as `request/cancelled` carries it to the front ends that did
/// not give it, and in words for the transcript.
pub(super) fn answer_given(answer: &Answer) -> (Value, String) {
    match answer {
        Answer::Labels(labels) => (json!({ "labels": labels }), labels.join(", ")),
        Answer::Text(text) => (json!({ "text": text }), text.clone()),
        Answer::Declined => (json!({ "declined": true }), "declined".to_string()),
    }
}

/// [`TuiUi`]'s approval prompt between turns, where the keyboard is read
/// here: the keys, and the remotes' answers, whichever comes first.
pub(super) fn ask_approval(ui: &mut TuiUi<'_>, tool: &str, arguments: &str) -> Approval {
    use super::term::{self, Event};
    let asked = remote::request("approval/request", || approval_params(tool, arguments));
    let settle = |by: Option<u64>, approval: &Approval| {
        if let Some(asked) = &asked {
            remote::settle(asked.id, by, json!({ "decision": decision(approval) }));
        }
    };
    loop {
        if let Some((remote, approval)) = asked.as_ref().and_then(approval_from) {
            settle(Some(remote), &approval);
            let line = format!("  ↳ {} at remote {remote}", verdict(&approval));
            ui.r.push_line(line, true);
            let _ = ui.r.refresh();
            return approval;
        }
        match term::poll(super::KEY_POLL) {
            Ok(Some(Event::Key(k))) => {
                if let Some(approval) = super::approval_key(k) {
                    settle(None, &approval);
                    return approval;
                }
            }
            Ok(_) => {}
            Err(_) => {
                settle(None, &Approval::Deny);
                return Approval::Deny;
            }
        }
    }
}

/// The same for a question asked between turns.
pub(super) fn ask_user(ui: &mut TuiUi<'_>, question: &Question) -> Option<Answer> {
    use super::term::{self, Event};
    let asked = remote::request("question/request", || events::question(question));
    let settle = |by: Option<u64>, answer: &Answer| {
        if let Some(asked) = &asked {
            remote::settle(asked.id, by, json!({ "answer": answer_given(answer).0 }));
        }
    };
    ui.r.ask(question.clone());
    loop {
        let _ = ui.r.refresh();
        let from_remote = asked.as_ref().and_then(|a| answer_from(a, question));
        if let Some((remote, answer)) = from_remote {
            settle(Some(remote), &answer);
            ui.r.question = None;
            let line = format!(
                "  ↳ answered at remote {remote}: {}",
                answer_given(&answer).1
            );
            ui.r.push_line(line, true);
            let _ = ui.r.refresh();
            return Some(answer);
        }
        match term::poll(super::KEY_POLL) {
            Ok(Some(Event::Key(k))) => {
                if let Some(answer) = super::question_key(ui.r, k) {
                    settle(None, &answer);
                    let _ = ui.r.refresh();
                    return Some(answer);
                }
            }
            Ok(_) => {}
            Err(_) => {
                ui.r.question = None;
                settle(None, &Answer::Declined);
                return Some(Answer::Declined);
            }
        }
    }
}
