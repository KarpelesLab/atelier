//! Transport-agnostic JSON-RPC 2.0 interface shared by the stdio and HTTP MCP
//! transports.
//!
//! [`McpTool`](crate::mcp::tool::McpTool) and the handshake/`tools/list` logic
//! in [`crate::mcp`] are written once against this trait; [`Conn`](crate::mcp::conn::Conn)
//! (stdio, newline-delimited JSON) and [`HttpConn`](crate::mcp::http::HttpConn)
//! (Streamable HTTP) each implement it for their own wire format.

use anyhow::Result;
use serde_json::Value;

/// A live JSON-RPC 2.0 connection to an MCP server, independent of transport.
pub(crate) trait JsonRpc: Send {
    /// Send a request and block until the matching response arrives. Returns
    /// the response's `result` field, or `Err` if the response carried a
    /// JSON-RPC `error`.
    fn request(&mut self, method: &str, params: Value) -> Result<Value>;

    /// Send a notification (no response expected).
    fn notify(&mut self, method: &str, params: Value) -> Result<()>;
}
