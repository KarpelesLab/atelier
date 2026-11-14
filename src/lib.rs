//! atelier — an AI coding harness over OpenAI-compatible APIs, as a library.
//!
//! The `atelier` program is this library and a `main` that calls
//! [`app::main`]. Another program can build on the same pieces to put an
//! interface of its own in front of the agent — see `docs/library.md` and
//! `examples/embed.rs`. In short:
//!
//! - [`app::open_session`] sets up a [`Session`] in the current directory,
//!   exactly as the program does (configuration, tools, trust, the link to
//!   the other instances);
//! - the interface implements [`Ui`], through which a turn reports what the
//!   model says and does, and asks for approvals;
//! - [`Session::send`] runs a turn.
//!
//! Every module is public, since the interfaces atelier ships with
//! ([`tui`], [`headless`], [`serve`], [`acp`]) are written against them and
//! a new one may need the same. What is promised to stay put is the smaller
//! set re-exported here.

pub mod acp;
pub mod agent;
pub mod agents;
pub mod app;
pub mod auth;
pub mod claude;
pub mod cli;
pub mod commands;
pub mod config;
pub mod context;
pub mod coord;
pub mod diff;
pub mod doctor;
pub mod events;
pub mod headless;
pub mod hooks;
pub mod js;
pub mod listen;
pub mod local;
pub mod lsp;
pub mod mcp;
pub mod memory;
pub mod oauth;
pub mod permissions;
pub mod provider;
pub mod review;
pub mod risk;
pub mod rpc;
pub mod sandbox;
pub mod schema;
pub mod serve;
pub mod session;
pub mod settings;
pub mod settings_api;
pub mod shlex;
pub mod skills;
pub mod tools;
pub mod trust;
#[cfg(tui)]
pub mod tui;
pub mod update;
pub mod usage;

pub use agent::{Approval, Session, Ui};
pub use config::Config;
pub use permissions::Mode;
pub use settings::Settings;
pub use tools::ask::{Answer, Question};
pub use tools::change::FileChange;
