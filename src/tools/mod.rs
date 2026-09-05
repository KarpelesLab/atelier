//! Built-in tools and the tool registry.
//!
//! # Contract (stable — implementers must not change these signatures)
//!
//! A tool implements [`Tool`]: it advertises a [`ToolSpec`] (name, description,
//! JSON-Schema parameters) and runs via [`Tool::call`], receiving parsed
//! arguments and a [`ToolCtx`]. [`builtin_registry`] assembles the default set.
//!
//! Every filesystem/exec tool is sandboxed to the project root
//! ([`ToolCtx::resolve`]) and records reads through [`FileState`] so `Edit` can
//! reject writes against a stale read (see the roadmap's ergonomics section).
//!
//! ## For the implementer (owns `src/tools/`)
//!
//! Implement [`builtin_registry`] and add one module per tool
//! (`read`, `write`, `edit`, `bash`, `grep`, `glob`, `ls`). Register each in
//! `builtin_registry`. Do not edit files outside `src/tools/`.

// Contract surface is consumed by the tools implementation (in progress) and by
// the agent loop; drop this once every item is wired up.
#![allow(dead_code)]

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde_json::Value;

pub use crate::provider::ToolSpec;

mod bash;
mod edit;
mod glob;
mod grep;
mod ls;
mod read;
mod write;

/// Execution context handed to every tool call.
pub struct ToolCtx<'a> {
    /// Absolute project root. All tool file access stays within it.
    pub project_root: &'a Path,
    /// Read-state tracker, shared across a session.
    pub fstate: &'a mut FileState,
}

impl ToolCtx<'_> {
    /// Resolve a (possibly relative) path against the project root and reject
    /// anything that escapes it. Use this in every tool that touches the fs.
    pub fn resolve(&self, path: &str) -> Result<PathBuf> {
        let p = Path::new(path);
        let joined = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.project_root.join(p)
        };
        let normalized = normalize(&joined);
        if !normalized.starts_with(self.project_root) {
            bail!("path {path:?} escapes the project root");
        }
        Ok(normalized)
    }
}

/// A callable tool.
pub trait Tool: Send + Sync {
    /// Unique tool name (what the model calls).
    fn name(&self) -> &str;
    /// The advertised schema (name, description, JSON-Schema parameters).
    fn spec(&self) -> ToolSpec;
    /// Whether running this tool changes the world (writes files, runs commands)
    /// and therefore needs the user's approval. Defaults to `true` — the safe
    /// default, so unknown/MCP tools require confirmation. Read-only tools
    /// override to `false`.
    fn side_effecting(&self) -> bool {
        true
    }
    /// Execute with parsed JSON `args`. The returned string is fed back to the
    /// model as the tool result; return `Err` for a failure the model should
    /// see (it is rendered as an error result, not a crash).
    fn call(&self, ctx: &mut ToolCtx, args: Value) -> Result<String>;
}

/// The set of tools available to a session.
#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }
    /// Specs for every registered tool, to advertise to the provider.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|t| t.spec()).collect()
    }
    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|t| t.as_ref())
    }
    /// Drop every tool whose name starts with `prefix` (e.g. when an MCP
    /// server is removed, its `mcp__<server>__*` tools). Returns how many.
    pub fn remove_prefix(&mut self, prefix: &str) -> usize {
        let before = self.tools.len();
        self.tools.retain(|t| !t.name().starts_with(prefix));
        before - self.tools.len()
    }
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
    pub fn len(&self) -> usize {
        self.tools.len()
    }
}

/// Assemble the default built-in tools.
pub fn builtin_registry() -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(read::ReadTool));
    reg.register(Box::new(write::WriteTool));
    reg.register(Box::new(edit::EditTool));
    reg.register(Box::new(bash::BashTool));
    reg.register(Box::new(grep::GrepTool));
    reg.register(Box::new(glob::GlobTool));
    reg.register(Box::new(ls::LsTool));
    reg
}

/// Tracks the content the agent has read, so an [`Edit`] can detect that a file
/// changed underneath it since it was last read.
#[derive(Default)]
pub struct FileState {
    /// path -> hash of the content last read/written by the agent.
    seen: HashMap<PathBuf, u64>,
}

impl FileState {
    pub fn new() -> Self {
        Self::default()
    }
    /// Record the content the agent has now observed for `path`.
    pub fn record(&mut self, path: &Path, content: &str) {
        self.seen.insert(path.to_path_buf(), hash(content));
    }
    /// Whether the agent has read `path` at all this session.
    pub fn was_read(&self, path: &Path) -> bool {
        self.seen.contains_key(path)
    }
    /// True if `current` differs from what the agent last observed — i.e. the
    /// file changed on disk since it was read (or was never read).
    pub fn is_stale(&self, path: &Path, current: &str) -> bool {
        match self.seen.get(path) {
            Some(h) => *h != hash(current),
            None => true,
        }
    }
}

fn hash(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Lexically normalize a path (resolve `.`/`..`) without touching the fs.
fn normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}
