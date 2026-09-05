//! MCP (Model Context Protocol) client.
//!
//! Connects to MCP servers, lists their tools, and exposes each as a
//! [`Tool`](crate::tools::Tool) so discovered tools plug into the same registry
//! as built-ins — namespaced `mcp__<server>__<tool>` — and flow through the same
//! execution path.
//!
//! # Contract (stable — implementers must not change these signatures)
//!
//! [`connect_stdio`] launches a server over stdio (JSON-RPC 2.0: `initialize`,
//! `tools/list`, `tools/call`) and returns its tools. HTTP/SSE transport comes
//! later. `main` will call this and register the results; do not wire it into
//! `main` yourself.
//!
//! ## For the implementer (owns `src/mcp/`)
//!
//! Implement [`connect_stdio`] and the JSON-RPC plumbing. A server is spawned
//! as a child process; frame requests/responses over its stdin/stdout. Wrap
//! each advertised tool in a type implementing [`Tool`](crate::tools::Tool)
//! whose `call` issues a `tools/call`. Do not edit files outside `src/mcp/`.

// Contract surface is consumed by the MCP implementation (in progress) and by
// `main` once registration is wired up.
#![allow(dead_code)]

use anyhow::Result;

use crate::tools::Tool;

/// Configuration for one stdio MCP server.
pub struct StdioServer {
    /// Logical server name, used for the `mcp__<server>__<tool>` prefix.
    pub name: String,
    /// Command to launch.
    pub command: String,
    /// Arguments to the command.
    pub args: Vec<String>,
}

/// Launch an MCP server over stdio and return its tools.
///
/// TODO(mcp agent): implement the JSON-RPC handshake and tool wrappers.
pub fn connect_stdio(_server: &StdioServer) -> Result<Vec<Box<dyn Tool>>> {
    Ok(Vec::new())
}
