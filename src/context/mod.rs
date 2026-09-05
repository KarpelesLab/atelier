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

/// The default set of context providers.
///
/// TODO(context agent): return git status, recent diff, and project layout.
pub fn default_providers() -> Vec<Box<dyn ContextProvider>> {
    Vec::new()
}
