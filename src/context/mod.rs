//! Context helpers ("feed the agent").
//!
//! Per turn, the agent loop gathers [`ContextItem`]s from a set of
//! [`ContextProvider`]s and injects them so the model reasons about the
//! *current* repo state (git status, recent diff, project layout, build/lint
//! diagnostics) rather than a stale snapshot.
//!
//! # Contract (stable — implementers must not change these signatures)
//!
//! A provider implements [`ContextProvider`]: [`gather`](ContextProvider::gather)
//! inspects the project root and returns an item, or `None` when it has nothing
//! to add. [`default_providers`] returns the default set.
//!
//! ## For the implementer (owns `src/context/`)
//!
//! Implement [`default_providers`] plus one module per provider (e.g.
//! `git`, `diff`, `layout`). Keep each item compact — a token budget is applied
//! by the caller. Prefer read-only inspection (shell out to `git`, walk the
//! tree). Do not edit files outside `src/context/`.

// Contract surface is consumed by the providers implementation (in progress).
#![allow(dead_code)]

mod budget;
mod commits;
mod dedup;
mod diagnostics;
mod diff;
mod environment;
mod git;
mod instructions;
mod layout;
mod todos;

// Not yet called from the agent loop (wiring is done separately); re-exported
// now so that integration is a one-line change once it lands.
#[allow(unused_imports)]
pub use budget::{estimate_tokens, render_budgeted};
#[allow(unused_imports)]
pub use dedup::dedup_items;

use std::path::Path;

/// A single piece of injected context.
pub struct ContextItem {
    /// Short heading, e.g. `"git status"`.
    pub title: String,
    /// The body shown to the model.
    pub body: String,
    /// Higher priority survives truncation when the context budget is tight.
    pub priority: u8,
}

impl ContextItem {
    pub fn new(title: impl Into<String>, body: impl Into<String>, priority: u8) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            priority,
        }
    }
}

/// A source of per-turn context.
pub trait ContextProvider: Send + Sync {
    /// Stable name, for logging/config.
    fn name(&self) -> &str;
    /// Inspect `root` and return an item, or `None` if there's nothing to say.
    fn gather(&self, root: &Path) -> Option<ContextItem>;
}

/// The default set of context providers: project instructions, cargo-check
/// diagnostics, git status, recent diff, recent commits, project layout,
/// open TODOs, and environment facts (in priority order).
pub fn default_providers() -> Vec<Box<dyn ContextProvider>> {
    vec![
        Box::new(instructions::InstructionsProvider),
        Box::new(diagnostics::DiagnosticsProvider),
        Box::new(git::GitStatusProvider),
        Box::new(diff::GitDiffProvider),
        Box::new(commits::RecentCommitsProvider),
        Box::new(todos::TodosProvider),
        Box::new(layout::LayoutProvider),
        Box::new(environment::EnvironmentProvider),
    ]
}
