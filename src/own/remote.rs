//! The session shared with remotes, as the interface that owns it sees it:
//! [`hub`](super::hub)'s front side, for this process's ownership. Every
//! function is a no-op (and asks nothing of anybody) in a process that owns
//! no session or has no socket — an interface calls them unconditionally.
//!
//! In the order an interface meets them: [`serve`] once it is up;
//! [`publish`] whenever the session comes to rest changed; [`inbox`]
//! wherever it waits for its own input; [`begin_turn`], [`event`] and
//! [`end_turn`] around what it shows of a turn; [`request`] and [`settle`]
//! around what it asks its user.

// Called by the terminal interface, which not every build has.
#![cfg_attr(not(tui), allow(dead_code, unused_imports))]

use std::sync::Arc;

use serde_json::Value;

use super::hub::Hub;
pub(crate) use super::hub::{Answered, Asked, Inbound, Snapshot, origin};
use crate::rpc::{Failure, Id};
use crate::tools::terminal::{CommandEvent, CommandInput};

fn hub() -> Option<&'static Arc<Hub>> {
    super::imp::owner()
}

/// Take remotes from now on. Whether there is anything to take them on:
/// `false` in a process that does not own a session.
pub fn serve() -> bool {
    hub().is_some_and(|hub| {
        hub.serve_remotes();
        true
    })
}

/// How many remotes are attached.
pub fn attached() -> usize {
    hub().map_or(0, |hub| hub.remotes())
}

/// Leave the session as it is now, for what remotes are answered without
/// it (`session/status`, `session/history`, `initialize`).
pub fn publish(snapshot: impl FnOnce() -> Snapshot) {
    if let Some(hub) = hub() {
        hub.publish(snapshot());
    }
}

/// What remotes asked for since last looked.
pub fn inbox() -> Vec<Inbound> {
    hub().map_or_else(Vec::new, |hub| hub.inbox())
}

pub fn respond(remote: u64, id: &Id, result: Value) {
    if let Some(hub) = hub() {
        hub.respond(remote, id, result);
    }
}

pub fn fail(remote: u64, id: &Id, failure: Failure) {
    if let Some(hub) = hub() {
        hub.fail(remote, id, failure);
    }
}

/// Refuse a request that found the session busy.
pub fn fail_busy(remote: u64, id: &Id) {
    if let Some(hub) = hub() {
        hub.fail_busy(remote, id);
    }
}

/// What a remote asked for is done, and it was not a turn.
pub fn release() {
    if let Some(hub) = hub() {
        hub.release();
    }
}

/// A turn starts: see [`Hub::begin_turn`]. Returns its id among the
/// session's turns as remotes count them (0 without a hub).
pub fn begin_turn(text: &str, by: Option<u64>, answer: Option<(&Id, Value)>) -> u64 {
    hub().map_or(0, |hub| hub.begin_turn(text, by, answer))
}

/// The turn is over; `ended` is `turn/ended`'s params.
pub fn end_turn(ended: impl FnOnce() -> (Value, Snapshot)) {
    if let Some(hub) = hub() {
        let (ended, snapshot) = ended();
        hub.end_turn(ended, snapshot);
    }
}

/// Something the session reports, for every remote.
pub fn event(kind: &str, params: impl FnOnce() -> Value) {
    if let Some(hub) = hub() {
        hub.event(kind, params(), false);
    }
}

/// Put a request to every remote; `None` when there is nobody to put it
/// to and never will be.
pub fn request(method: &str, params: impl FnOnce() -> Value) -> Option<Asked> {
    hub().map(|hub| hub.request(method, params()))
}

/// A request has its answer, from `by` (`None`: the owner's terminal).
pub fn settle(id: u64, by: Option<u64>, how: Value) {
    if let Some(hub) = hub() {
        hub.settle(id, by, how);
    }
}

/// What the running command has to tell.
pub fn command_event(event: &CommandEvent) {
    if let Some(hub) = hub() {
        hub.command_event(event);
    }
}

/// `local` — the owner's own keys for the running command — with what
/// remotes typed.
pub fn command_input(local: Vec<CommandInput>) -> Vec<CommandInput> {
    match hub() {
        Some(hub) => hub.command_input(local),
        None => local,
    }
}
