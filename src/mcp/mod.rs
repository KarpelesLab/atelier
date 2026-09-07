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
//! `tools/list`, `tools/call`) and returns its tools. [`connect_http`] does the
//! same over MCP's Streamable HTTP transport (see `src/mcp/http.rs` for what's
//! covered). `main` will call these and register the results; do not wire them
//! into `main` yourself.
//!
//! ## For the implementer (owns `src/mcp/`)
//!
//! Implement [`connect_stdio`], [`connect_http`], and the JSON-RPC plumbing. A
//! stdio server is spawned as a child process; requests/responses are framed
//! over its stdin/stdout. An HTTP server is a URL; requests are POSTed and
//! responses may come back as a single JSON object or an SSE stream. Both
//! transports implement the shared `jsonrpc::JsonRpc` trait so the handshake
//! (`handshake_and_list_tools`) and tool-wrapping (`wrap_tools`) logic is
//! written once. Wrap each advertised tool in a type implementing
//! [`Tool`](crate::tools::Tool) whose `call` issues a `tools/call`. Do not
//! edit files outside `src/mcp/`.
//!
//! Both transports also probe the optional MCP *resources* capability after
//! the `tools/list` handshake (`list_resources`, shared): if the server
//! answers `resources/list` with at least one resource, one extra
//! `mcp__<server>__read_resource` tool ([`resource::McpResourceTool`]) is
//! appended, which reads any of them by `uri` via `resources/read`. A server
//! that doesn't support resources (an erroring or empty `resources/list`)
//! simply gets no such tool — the connection still succeeds.

// Contract surface is consumed by the MCP implementation (in progress) and by
// `main` once registration is wired up.
#![allow(dead_code)]

mod conn;
mod http;
mod jsonrpc;
mod resource;
mod tool;

use std::io::BufRead;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::tools::Tool;
use conn::Conn;
use jsonrpc::JsonRpc;
use resource::McpResourceTool;
use tool::McpTool;

// Re-exported for `main` to pick up once HTTP-server registration is wired
// in (see the module doc comment); unused within `src/mcp/` itself for now,
// same as `connect_stdio`/`StdioServer` before this module was consumed.
#[allow(unused_imports)]
pub use http::{HttpServer, connect_http};

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
/// Spawns `server.command server.args...` with piped stdin/stdout, performs
/// the `initialize` / `notifications/initialized` handshake, then calls
/// `tools/list` and wraps each advertised tool as a [`Tool`]. The child
/// process and connection are shared (behind `Arc<Mutex<_>>`) across every
/// returned tool, and calls are serialized through it; the child is kept
/// alive for as long as any of the returned tools are, and is killed once
/// they're all dropped.
pub fn connect_stdio(server: &StdioServer) -> Result<Vec<Box<dyn Tool>>> {
    let mut child = Command::new(&server.command)
        .args(&server.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning MCP server `{}` ({})", server.name, server.command))?;

    let stdin = child.stdin.take().expect("child spawned with piped stdin");
    let stdout = child
        .stdout
        .take()
        .expect("child spawned with piped stdout");

    // Drain stderr on a background thread so a chatty server can't block on a
    // full pipe; we don't have a logging sink to forward it to.
    if let Some(stderr) = child.stderr.take() {
        thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stderr);
            let mut buf = String::new();
            while matches!(reader.read_line(&mut buf), Ok(n) if n > 0) {
                buf.clear();
            }
        });
    }

    let mut conn = Conn::new(child, stdin, stdout);

    let advertised = handshake_and_list_tools(&mut conn, &server.name, "2024-11-05")?;
    let resources = list_resources(&mut conn);

    let conn: Arc<Mutex<dyn JsonRpc>> = Arc::new(Mutex::new(conn));
    let mut tools = wrap_tools(conn.clone(), &server.name, advertised);
    if let Some(resource_tool) = maybe_resource_tool(conn, &server.name, resources) {
        tools.push(resource_tool);
    }
    Ok(tools)
}

/// Run the `initialize` → `notifications/initialized` → `tools/list`
/// handshake over an already-connected [`JsonRpc`] transport (stdio or HTTP)
/// and return the raw `tools` array it advertised.
///
/// Shared by [`connect_stdio`] and [`connect_http`](http::connect_http) so
/// the two transports can't drift on protocol behavior.
pub(crate) fn handshake_and_list_tools(
    conn: &mut dyn JsonRpc,
    server_name: &str,
    protocol_version: &str,
) -> Result<Vec<Value>> {
    conn.request(
        "initialize",
        json!({
            "protocolVersion": protocol_version,
            "capabilities": {},
            "clientInfo": { "name": "atelier", "version": "0.0.0" },
        }),
    )
    .with_context(|| format!("initializing MCP server `{server_name}`"))?;

    conn.notify("notifications/initialized", json!({}))
        .with_context(|| format!("completing handshake with MCP server `{server_name}`"))?;

    let tools_result = conn
        .request("tools/list", json!({}))
        .with_context(|| format!("listing tools from MCP server `{server_name}`"))?;

    Ok(tools_result
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

/// Wrap each raw advertised-tool object (as returned by `tools/list`) as a
/// namespaced [`Tool`] sharing one connection. Shared by [`connect_stdio`]
/// and [`connect_http`](http::connect_http).
pub(crate) fn wrap_tools(
    conn: Arc<Mutex<dyn JsonRpc>>,
    server_name: &str,
    advertised: Vec<Value>,
) -> Vec<Box<dyn Tool>> {
    let mut tools: Vec<Box<dyn Tool>> = Vec::with_capacity(advertised.len());
    for t in advertised {
        let Some(original_name) = t.get("name").and_then(Value::as_str) else {
            continue;
        };
        let description = t
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let parameters = t
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
        let namespaced_name = format!("mcp__{server_name}__{original_name}");

        tools.push(Box::new(McpTool::new(
            conn.clone(),
            namespaced_name,
            original_name.to_string(),
            description,
            parameters,
        )));
    }
    tools
}

/// Try `resources/list` on an already-connected transport and return the raw
/// `resources` array. Resource support is optional per the MCP spec: a
/// server that doesn't implement it (or errors for any other reason) simply
/// yields no resources here, rather than failing the whole connection.
/// Shared by [`connect_stdio`] and [`connect_http`](http::connect_http).
pub(crate) fn list_resources(conn: &mut dyn JsonRpc) -> Vec<Value> {
    conn.request("resources/list", json!({}))
        .ok()
        .and_then(|result| result.get("resources").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

/// If `resources` is non-empty, build the one namespaced
/// `mcp__<server>__read_resource` [`Tool`] that reads any of them by URI.
/// Returns `None` when the server advertised no resources (including when
/// `resources/list` isn't supported at all). Shared by [`connect_stdio`] and
/// [`connect_http`](http::connect_http).
pub(crate) fn maybe_resource_tool(
    conn: Arc<Mutex<dyn JsonRpc>>,
    server_name: &str,
    resources: Vec<Value>,
) -> Option<Box<dyn Tool>> {
    if resources.is_empty() {
        return None;
    }

    let namespaced_name = format!("mcp__{server_name}__read_resource");
    let description = describe_resources(&resources);

    Some(Box::new(McpResourceTool::new(
        conn,
        namespaced_name,
        description,
    )))
}

/// Build the `read_resource` tool's description: an instruction plus a
/// capped listing of the resources discovered via `resources/list`, so the
/// model knows what URIs are actually available to read.
fn describe_resources(resources: &[Value]) -> String {
    const CAP: usize = 30;

    let mut lines: Vec<String> = resources
        .iter()
        .take(CAP)
        .map(|r| {
            let uri = r.get("uri").and_then(Value::as_str).unwrap_or("");
            match r.get("name").and_then(Value::as_str) {
                Some(name) if !name.is_empty() => format!("- {uri} ({name})"),
                _ => format!("- {uri}"),
            }
        })
        .collect();
    if resources.len() > CAP {
        lines.push(format!("- ... and {} more", resources.len() - CAP));
    }

    format!(
        "Read the content of an MCP resource by its `uri`. Available resources ({} total):\n{}",
        resources.len(),
        lines.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{FileState, ToolCtx};
    use std::path::Path;

    /// A tiny POSIX-shell "MCP server" driven purely by `read`/`printf`, used
    /// to exercise `connect_stdio` end-to-end (spawn, handshake, tools/list,
    /// resources/list, tools/call, resources/read) without depending on any
    /// real MCP server being installed. It expects exactly the request
    /// sequence `connect_stdio` + one `tools/call` per tool + one
    /// `resources/read` produce, and replies with fixed, valid JSON-RPC
    /// frames.
    const FAKE_SERVER_SCRIPT: &str = r#"
IFS= read -r _init
printf '%s\n' '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fake","version":"0.0.0"}}}'
IFS= read -r _initialized_notification
IFS= read -r _tools_list
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"echo","description":"Echoes the input","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}},{"name":"boom","description":"Always fails","inputSchema":{}}]}}'
IFS= read -r _resources_list
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"resources":[{"uri":"file:///hello.txt","name":"hello","description":"A greeting","mimeType":"text/plain"}]}}'
IFS= read -r _call
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"hello "},{"type":"text","text":"world"}]}}'
IFS= read -r _call2
printf '%s\n' '{"jsonrpc":"2.0","id":4,"result":{"isError":true,"content":[{"type":"text","text":"kaboom"}]}}'
IFS= read -r _resource_read
printf '%s\n' '{"jsonrpc":"2.0","id":5,"result":{"contents":[{"uri":"file:///hello.txt","mimeType":"text/plain","text":"hello resource"}]}}'
"#;

    /// Same handshake as [`FAKE_SERVER_SCRIPT`], but `resources/list`
    /// returns a JSON-RPC error (as a server that doesn't support resources
    /// might) instead of a result. Used to check that this doesn't fail the
    /// connection and simply yields no `read_resource` tool.
    const FAKE_SERVER_NO_RESOURCES_SCRIPT: &str = r#"
IFS= read -r _init
printf '%s\n' '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fake","version":"0.0.0"}}}'
IFS= read -r _initialized_notification
IFS= read -r _tools_list
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"echo","description":"Echoes the input","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]}}'
IFS= read -r _resources_list
printf '%s\n' '{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"Method not found"}}'
"#;

    fn fake_server() -> StdioServer {
        StdioServer {
            name: "fake".into(),
            command: "sh".into(),
            args: vec!["-c".into(), FAKE_SERVER_SCRIPT.into()],
        }
    }

    #[test]
    fn connects_lists_and_calls_tools() {
        let server = fake_server();
        let tools = connect_stdio(&server).expect("connect_stdio should succeed");

        assert_eq!(tools.len(), 3);

        let echo = tools
            .iter()
            .find(|t| t.name() == "mcp__fake__echo")
            .expect("echo tool should be present, namespaced");
        let spec = echo.spec();
        assert_eq!(spec.name, "mcp__fake__echo");
        assert_eq!(spec.description, "Echoes the input");
        assert_eq!(spec.parameters["type"], "object");

        // No tool under test touches the filesystem, so the root is nominal.
        let root = Path::new("/");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: root,
            fstate: &mut fstate,
        };
        let out = echo
            .call(&mut ctx, json!({"text": "hi"}))
            .expect("tools/call should succeed");
        assert_eq!(out, "hello \nworld");

        let boom = tools
            .iter()
            .find(|t| t.name() == "mcp__fake__boom")
            .expect("boom tool should be present");
        let mut ctx = ToolCtx {
            project_root: root,
            fstate: &mut fstate,
        };
        let err = boom
            .call(&mut ctx, json!({}))
            .expect_err("isError result should surface as Err");
        assert!(err.to_string().contains("kaboom"));

        let read_resource = tools
            .iter()
            .find(|t| t.name() == "mcp__fake__read_resource")
            .expect("read_resource tool should be present when resources/list is non-empty");
        let spec = read_resource.spec();
        assert_eq!(spec.name, "mcp__fake__read_resource");
        // The description should list the discovered resource so the model
        // knows what it can read.
        assert!(spec.description.contains("file:///hello.txt"));
        assert!(spec.description.contains("hello"));
        assert_eq!(spec.parameters["type"], "object");
        assert_eq!(spec.parameters["required"], json!(["uri"]));

        let mut ctx = ToolCtx {
            project_root: root,
            fstate: &mut fstate,
        };
        let out = read_resource
            .call(&mut ctx, json!({"uri": "file:///hello.txt"}))
            .expect("resources/read should succeed");
        assert_eq!(out, "hello resource");
    }

    #[test]
    fn resources_list_error_yields_no_resource_tool() {
        let server = StdioServer {
            name: "fake".into(),
            command: "sh".into(),
            args: vec!["-c".into(), FAKE_SERVER_NO_RESOURCES_SCRIPT.into()],
        };
        let tools = connect_stdio(&server)
            .expect("connect_stdio should succeed even when resources/list errors");

        assert_eq!(tools.len(), 1);
        assert!(
            tools.iter().all(|t| t.name() != "mcp__fake__read_resource"),
            "no read_resource tool should be added when resources/list fails"
        );
    }

    #[test]
    fn missing_command_reports_a_clear_error() {
        let server = StdioServer {
            name: "nope".into(),
            command: "definitely-not-a-real-binary-atelier-mcp-test".into(),
            args: vec![],
        };
        match connect_stdio(&server) {
            Ok(_) => panic!("spawning a missing binary should fail"),
            Err(err) => assert!(err.to_string().contains("spawning MCP server")),
        }
    }
}
