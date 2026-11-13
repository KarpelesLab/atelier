//! Where there are no Unix sockets there is no coordinator:
//! [`super::start`] never creates a [`Client`], and this only keeps the
//! callers compiling.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, bail};
use serde_json::Value;

use super::Message;

pub struct Options {
    pub dir: PathBuf,
    pub exe: Option<PathBuf>,
    pub name: String,
    pub cwd: String,
    pub model: String,
}

pub struct Client;

const UNSUPPORTED: &str = "the coordinator is not supported on this platform";

impl Client {
    pub fn start(_: Options) -> Arc<Self> {
        Arc::new(Self)
    }
    pub fn name(&self) -> String {
        String::new()
    }
    pub fn link_status(&self) -> String {
        UNSUPPORTED.to_string()
    }
    pub fn list(&self) -> Result<Value> {
        bail!(UNSUPPORTED)
    }
    pub fn send(&self, _: &str, _: &str) -> Result<u64> {
        bail!(UNSUPPORTED)
    }
    pub fn subscribe(&self, _: &str, _: bool) -> Result<()> {
        bail!(UNSUPPORTED)
    }
    pub fn topics(&self) -> Vec<String> {
        Vec::new()
    }
    pub fn update(&self, _: &str, _: &str) {}
    pub fn take_messages(&self) -> Vec<Message> {
        Vec::new()
    }
}
