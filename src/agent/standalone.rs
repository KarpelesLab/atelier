//! Standalone runs: a session with a job of its own.
//!
//! `atelier review` is not a conversation with the user about their project:
//! it is one prompt, built by the harness, answered by a model that speaks
//! from a prompt of its own and may only look. Rather than a second kind of
//! session, it is the ordinary one — same configuration, same loop, same
//! approvals and hooks — narrowed after it was set up:
//!
//! - **its own system prompt**, in place of atelier's (the project's
//!   instruction files still follow it: its rules apply to a reviewer too);
//! - **a subset of the tools**, chosen by the caller;
//! - **standing context only**: what the run is about is in its prompt, and
//!   the snapshot of the moment (git status, layout) would only compete with
//!   it for the model's attention;
//! - **alone**: no link to the other instances (a peer's message is not part
//!   of the job), no background reviewer watching it;
//! - **nothing persisted**: it must not replace the conversation saved for
//!   `--continue`, and it starts from none.
//!
//! Unlike a subagent's session (see `subagent`), nobody stands behind it:
//! its calls are decided by this session's own rules.

use super::Session;
use crate::context;

impl Session {
    /// Narrow this session to a standalone run that speaks from `system`
    /// and keeps only the tools `keep` accepts. See the module docs.
    pub fn standalone(&mut self, system: String, keep: impl Fn(&str) -> bool) {
        self.tools = self.tools.filtered(keep);
        self.context =
            context::standing_providers(context::InstructionFiles::with_user(&self.settings));
        self.review_enabled = false;
        self.coord = None;
        self.history.clear();
        self.summary = None;
        self.standalone = Some(system);
    }
}
