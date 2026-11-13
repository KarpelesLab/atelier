//! Tool calls made to a session from outside a turn.
//!
//! `atelier mcp` (see [`crate::mcp::server`]) serves a session's tools to
//! another program's agent: there is no model here and no conversation,
//! only calls. They take the path the tools atelier offers claude take —
//! the session's [`ToolRunner`](super::ToolRunner) as a
//! [`ToolHost`](crate::claude::ToolHost) — so a call is decided and run
//! exactly as one of the model's would be.

use serde_json::Value;

use super::{Session, Ui};
use crate::permissions::{Decision, Policy};
use crate::tools::{ToolOutput, ToolRegistry};

/// Why a call would be put to the user, for a caller that has nobody to
/// put it to and must say what to do instead.
pub struct WhyAsked {
    /// The `ask` rule that demands the prompt, if one does: no allow rule
    /// gets past it.
    pub ask_rule: Option<String>,
    /// The allow rules "always" would have saved at the prompt — the
    /// narrowest that let this call through. Empty when none fits.
    pub proposed: Vec<String>,
    /// Whether an allow rule in the project's `atelier.toml` would count
    /// (the project is trusted).
    pub project_rules_count: bool,
}

impl Session {
    /// The session's tools.
    pub fn tool_registry(&self) -> &ToolRegistry {
        &self.tools
    }

    /// Run one tool call as a call by the model is run: `pre_tool` hooks,
    /// the permission rules and the mode (asking through `ui` when they say
    /// to), the tool, `post_tool` hooks. Returns its output and whether it
    /// succeeded; a refusal is a failed call whose text says why.
    pub fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
        ui: &mut dyn Ui,
    ) -> (ToolOutput, bool) {
        crate::claude::ToolHost::call(&mut self.runner(), name, arguments, ui)
    }

    /// What the permission rules say about why `tool(arguments)` asks.
    pub fn why_asked(&self, tool: &str, arguments: &str) -> WhyAsked {
        let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
        let policy = Policy::load(&self.trust.permissions(&self.settings.permissions));
        let verdict = policy.decide(tool, &args, &self.root);
        let ask_rule = match verdict.decision {
            Decision::Ask => verdict.by.map(|by| by.rule),
            _ => None,
        };
        WhyAsked {
            ask_rule,
            proposed: policy.propose(tool, &args, &self.root),
            project_rules_count: self.trust.active(),
        }
    }
}
