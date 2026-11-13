//! Questions between instances: ask another agent, and wait for its answer.
//!
//! `peer_send` leaves a message and moves on. `peer_ask` is for when the
//! asker needs what the other agent knows before it can continue: the tool
//! call returns the other agent's answer.
//!
//! On the wire it is a `call` of the method [`METHOD`] (see
//! [`super::daemon`]): the coordinator forwards it to the target and holds
//! it until the target answers or the asker's timeout runs out. What makes
//! it different from `ping` is that the answer is not the target *process*'s
//! to give. The handler runs on the connection's reader thread and returns
//! at once: it writes the question down here, on the [`Desk`], puts it in
//! the instance's inbox as a message marked as a question, and keeps the
//! means to reply ([`Reply`], a deferred responder — see
//! [`super::client::Responder`]). The agent then meets the question the way
//! it meets any peer message — at its next step if it is working, with the
//! next prompt if it is idle — and answers it with the `peer_answer` tool,
//! which takes the question off the desk and sends the reply.
//!
//! The asker always gets an [`Outcome`], never a hang or a bare error:
//!
//! - **answered**: the other agent's text;
//! - **unanswered**, with the reason: the turn the question was delivered to
//!   ended without a `peer_answer` (the session closes those questions when
//!   a turn ends); the timeout ran out — typically because the target is
//!   idle and its user did not prompt it; the target disconnected; or the
//!   target is itself waiting for an answer from the asker, which neither
//!   could ever give the other.
//!
//! Each side keeps time on its own: the coordinator fails the call at the
//! deadline, and the desk forgets the question after the same delay, so an
//! agent is not asked to answer something nobody is waiting for.

// Where there is no coordinator (see `stub.rs`) nobody keeps a desk.
#![cfg_attr(not(unixlike), allow(dead_code))]

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::{Value, json};

/// The `call` method a question travels as.
pub const METHOD: &str = "ask";
/// How long an asker waits when it does not say.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);
/// The longest an asker may wait (the coordinator holds calls this long).
pub const MAX_TIMEOUT: Duration = Duration::from_secs(1800);

/// How a question ended, for the one who asked.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The other agent's answer.
    Answered(String),
    /// There is no answer, and why.
    Unanswered(String),
}

impl Outcome {
    /// As the `call`'s result.
    pub(super) fn to_value(&self) -> Value {
        match self {
            Outcome::Answered(text) => json!({ "answered": true, "answer": text }),
            Outcome::Unanswered(why) => json!({ "answered": false, "reason": why }),
        }
    }

    pub(super) fn from_value(v: &Value) -> Self {
        let text = |key: &str| v[key].as_str().unwrap_or_default().to_string();
        if v["answered"] == true {
            Outcome::Answered(text("answer"))
        } else {
            Outcome::Unanswered(text("reason"))
        }
    }
}

/// Sends a question's outcome back to its asker. `Err` when it can no
/// longer be delivered (the connection it arrived on is gone).
pub(super) type Reply = Box<dyn FnOnce(Outcome) -> Result<()> + Send>;

/// A question waiting for this instance's agent.
struct Open {
    /// The asker's name.
    from: String,
    /// When the asker stops waiting.
    deadline: Instant,
    reply: Reply,
}

/// The questions this instance was asked and has not answered.
#[derive(Default)]
pub(super) struct Desk {
    /// Questions admitted so far; numbers the ids.
    asked: u64,
    open: HashMap<String, Open>,
}

impl Desk {
    /// Take a question in; returns the id its answer must name.
    pub(super) fn admit(&mut self, from: &str, timeout: Duration, reply: Reply) -> String {
        self.expire();
        self.asked += 1;
        let id = format!("q{}", self.asked);
        self.open.insert(
            id.clone(),
            Open {
                from: from.to_string(),
                deadline: Instant::now() + timeout,
                reply,
            },
        );
        id
    }

    /// Forget the questions whose asker has stopped waiting. The
    /// coordinator's clock may be a moment behind this one, so the asker is
    /// told all the same: it gets this outcome or the coordinator's
    /// timeout, whichever is first, and they say the same thing.
    fn expire(&mut self) {
        let now = Instant::now();
        let late: Vec<String> = self
            .open
            .iter()
            .filter(|(_, q)| q.deadline <= now)
            .map(|(id, _)| id.clone())
            .collect();
        for id in late {
            if let Some(q) = self.open.remove(&id) {
                let _ = (q.reply)(Outcome::Unanswered(
                    "the question was not answered in time".to_string(),
                ));
            }
        }
    }

    /// Whether `id` can still be answered.
    pub(super) fn is_open(&mut self, id: &str) -> bool {
        self.expire();
        self.open.contains_key(id)
    }

    /// Answer question `id`; returns the asker's name.
    pub(super) fn answer(&mut self, id: &str, text: &str) -> Result<String> {
        self.expire();
        let Some(q) = self.open.remove(id) else {
            bail!("no open question '{id}': it was answered already, or its asker stopped waiting");
        };
        (q.reply)(Outcome::Answered(text.to_string()))?;
        Ok(q.from)
    }

    /// Close every open question but the ones `keep` accepts, telling their
    /// askers `why` there is no answer. Returns `(id, asker)` of each.
    pub(super) fn close(
        &mut self,
        keep: impl Fn(&str) -> bool,
        why: &str,
    ) -> Vec<(String, String)> {
        self.expire();
        let ids: Vec<String> = self.open.keys().filter(|id| !keep(id)).cloned().collect();
        let mut closed = Vec::new();
        for id in ids {
            if let Some(q) = self.open.remove(&id) {
                // An asker that cannot be told has stopped waiting anyway.
                let _ = (q.reply)(Outcome::Unanswered(why.to_string()));
                closed.push((id, q.from));
            }
        }
        closed.sort();
        closed
    }
}
