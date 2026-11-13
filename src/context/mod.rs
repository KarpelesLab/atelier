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
mod mcp;
mod memory;
mod skills;
mod todos;

// Not yet called from the agent loop (wiring is done separately); re-exported
// now so that integration is a one-line change once it lands.
#[allow(unused_imports)]
pub use budget::{estimate_tokens, render_budgeted};
#[allow(unused_imports)]
pub use dedup::{Delta, dedup_items};
pub use diagnostics::detectable as detectable_checkers;
pub use instructions::{
    instruction_files, nested_instructions, pick as instruction_file, review_section,
};
pub use mcp::McpProvider;

use crate::settings::DiagnosticCheck;
use crate::trust::Trust;
use std::path::Path;

/// A single piece of injected context.
pub struct ContextItem {
    /// Short heading, e.g. `"git status"`.
    pub title: String,
    /// The body shown to the model.
    pub body: String,
    /// Higher priority survives truncation when the context budget is tight.
    pub priority: u8,
    /// Standing context (the instruction files, the memory index) rather
    /// than a snapshot of the moment: constant for most of a session, so
    /// atelier's own loop keeps it in the system message instead of a turn's
    /// context block (see `agent::prompt`), and it is rendered whole — its
    /// provider keeps it within a budget of its own.
    pub standing: bool,
}

impl ContextItem {
    pub fn new(title: impl Into<String>, body: impl Into<String>, priority: u8) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            priority,
            standing: false,
        }
    }

    /// Marks the item as standing context (see [`ContextItem::standing`]).
    pub fn standing(mut self) -> Self {
        self.standing = true;
        self
    }
}

/// A source of per-turn context.
pub trait ContextProvider: Send + Sync {
    /// Stable name, for logging/config.
    fn name(&self) -> &str;
    /// Inspect `root` and return an item, or `None` if there's nothing to say.
    fn gather(&self, root: &Path) -> Option<ContextItem>;
}

/// The providers of *standing* context only — the project's instruction
/// files. What a subagent gets: the rules of the project apply to it too,
/// but the snapshot of the moment (git status, diagnostics, layout) is its
/// parent's to pass on in the task, and gathering it per child is not free.
pub fn standing_providers() -> Vec<Box<dyn ContextProvider>> {
    vec![Box::new(instructions::InstructionsProvider::new())]
}

/// The default set of context providers: project instructions, the memory
/// index, the available skills, lint/typecheck diagnostics, git status,
/// recent diff, recent commits, project layout, open TODOs, and environment
/// facts (in priority order).
///
/// `checkers` are the `[[diagnostics]]` entries of `atelier.toml`; when
/// empty, the diagnostics provider picks checkers from the project's files.
/// Either way they are commands of the project's choosing, which run only
/// while `trust` says the project is trusted.
pub fn default_providers(
    checkers: Vec<DiagnosticCheck>,
    trust: &Trust,
) -> Vec<Box<dyn ContextProvider>> {
    vec![
        Box::new(instructions::InstructionsProvider::new()),
        Box::new(memory::MemoryProvider::new(crate::memory::global_dir())),
        Box::new(skills::SkillsProvider::new(crate::skills::global_dirs())),
        Box::new(diagnostics::DiagnosticsProvider::new(
            checkers,
            trust.clone(),
        )),
        Box::new(git::GitStatusProvider),
        Box::new(diff::GitDiffProvider),
        Box::new(commits::RecentCommitsProvider),
        Box::new(todos::TodosProvider),
        Box::new(layout::LayoutProvider),
        Box::new(environment::EnvironmentProvider),
    ]
}
