//! Background jobs that announce their end.
//!
//! A `bash` call with `background: true` returns at once, and the job's end
//! used to be something the model had to ask about (`bash_output`). Now the
//! job tells: when it exits, the `bash` module leaves a notice (see
//! [`crate::tools::bash::Notice`]), and this module carries it to the two
//! who want to know.
//!
//! - **The user** sees one line (`⚙ job 3 finished (exit 0, 42s): cargo
//!   test`), as soon as the session looks: between the steps of a running
//!   turn, or at the idle prompt ([`announce`], on the poll that also shows
//!   other instances' messages).
//! - **The model** gets a `<background-job …>` block, followed by a line
//!   saying it is the harness speaking, through the channel that already
//!   carries what arrives mid-turn — [`Ui::take_queued`], which the loop
//!   drains between steps ([`JobsUi`] adds the notices to it, the way
//!   `PeerUi` adds other instances' messages). A job that ends while the
//!   session is idle does **not** start a turn: its notice leads the user's
//!   next prompt ([`lead`]).
//!
//! Each job is announced once to each of them; the `bash` module keeps
//! count. Under the Claude backend the same wrapper is in place, so a
//! notice reaches claude the way a message typed mid-turn does (handed over
//! while a tool runs, delivered by claude at the turn's next step), or with
//! the next prompt.
//!
//! Jobs belong to the process, not to a conversation: a job a subagent
//! started is announced to the main conversation, which is the one still
//! there when it ends.

use super::{Approval, Ui};
use crate::tools::ask::{Answer, Question};
use crate::tools::bash::{self, Audience, NOTICE_OPEN};

/// Show the user the jobs that ended since the last look, without telling
/// the model yet. Returns whether there were any.
pub(super) fn announce(ui: &mut dyn Ui) -> bool {
    let notices = bash::notices(Audience::User);
    for notice in &notices {
        ui.info(&notice.line());
    }
    !notices.is_empty()
}

/// The model's notices as queued messages, showing the user the ones they
/// have not seen.
fn blocks(ui: &mut dyn Ui) -> Vec<String> {
    let notices = bash::notices(Audience::Model);
    for notice in notices.iter().filter(|n| !n.announced) {
        ui.info(&notice.line());
    }
    notices.iter().map(bash::Notice::block).collect()
}

/// `prompt` with the notices of the jobs that ended since the last turn in
/// front of it.
pub(super) fn lead(prompt: &str, ui: &mut dyn Ui) -> String {
    let mut input = String::new();
    for block in blocks(ui) {
        input.push_str(&block);
        input.push_str("\n\n");
    }
    input.push_str(prompt);
    input
}

/// Wraps the turn's [`Ui`] so the notices of jobs that end while it runs
/// join the messages the user queued — both backends then deliver them to
/// the model at its next step.
pub(super) struct JobsUi<'a> {
    pub(super) inner: &'a mut dyn Ui,
}

impl Ui for JobsUi<'_> {
    fn reasoning(&mut self, text: &str) {
        self.inner.reasoning(text);
    }
    fn content(&mut self, text: &str) {
        self.inner.content(text);
    }
    fn tool_start(&mut self, name: &str, arguments: &str) {
        self.inner.tool_start(name, arguments);
    }
    fn tool_end(&mut self, name: &str, result: &str, ok: bool) {
        self.inner.tool_end(name, result, ok);
    }
    fn turn_end(&mut self) {
        self.inner.turn_end();
    }
    fn info(&mut self, text: &str) {
        self.inner.info(text);
    }
    fn activity(&mut self, text: &str) {
        self.inner.activity(text);
    }
    fn ask_approval(&mut self, tool: &str, arguments: &str) -> Approval {
        self.inner.ask_approval(tool, arguments)
    }
    fn ask_user(&mut self, question: &Question) -> Option<Answer> {
        self.inner.ask_user(question)
    }
    fn notice(&mut self, text: &str) {
        self.inner.notice(text);
    }
    fn subconscious(&mut self, text: &str) {
        self.inner.subconscious(text);
    }
    fn take_queued(&mut self) -> Vec<String> {
        let mut queued = self.inner.take_queued();
        queued.extend(blocks(&mut *self.inner));
        queued
    }
    fn queued_delivered(&mut self, text: &str) {
        // A job's end was shown as its line; only what the user typed is
        // echoed on delivery.
        if !text.starts_with(NOTICE_OPEN) {
            self.inner.queued_delivered(text);
        }
    }
    fn interrupted(&mut self) -> bool {
        self.inner.interrupted()
    }
}
