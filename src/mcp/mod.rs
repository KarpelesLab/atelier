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
//! (`discover`) and tool-wrapping (`assemble`) logic is written once. Wrap each advertised tool in a type implementing
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
//!
//! Both transports likewise probe the optional MCP *prompts* capability
//! (`list_prompts`, shared): if the server answers `prompts/list` with at
//! least one prompt, one extra `mcp__<server>__get_prompt` tool
//! ([`prompt::McpPromptTool`]) is appended, which renders any of them by
//! `name` (plus optional `arguments`) via `prompts/get` into a readable
//! transcript. A server that doesn't support prompts (an erroring or empty
//! `prompts/list`) simply gets no such tool — the connection still succeeds.
//! The prompts also come back in the [`Connection`], each to be a slash
//! command of the user's (see `prompt.rs`).
//!
//! A server's `initialize` answer may also carry `instructions` for whoever
//! uses its tools. They come back with the tools in the [`Connection`], and
//! reach the model through the [`Catalog`] the session records connections
//! in (see `catalog.rs`). So do the *annotations* a server puts on its tools
//! (a title, read-only and destructive hints; see `tool.rs`), which the
//! `/tools` listing shows.
//!
//! # Connecting several servers
//!
//! [`connect_all`] connects a list of servers at once, each on a thread of
//! its own, and returns the outcomes *in the order the servers were given*.
//! Startup then costs as long as the slowest server rather than the sum of
//! them, while the tools are still registered in configuration order — the
//! order they are advertised to the model in, which has to be the same from
//! one run to the next for the provider's prompt cache to be hit.
//!
//! # Deferred tools
//!
//! When the connected servers' tools would cost too much to advertise one by
//! one, they are hidden behind two tools, `mcp_search` and `mcp_call` — see
//! `deferred.rs`, which also explains how such a call is still approved and
//! judged as the tool it runs.
//!
//! # Sign-in
//!
//! An HTTP server may require OAuth. Connecting never starts a sign-in: the
//! transport sends the stored token if there is one, and otherwise fails
//! with [`oauth::LoginRequired`] for the session to report. See `oauth.rs`.
//!
//! # Per-server options
//!
//! [`ServerOptions`] carries what the user may set on any server, whichever
//! the transport: which of its tools to offer (`enabled_tools` /
//! `disabled_tools`, matched against the server's own names) and how long to
//! wait for it. The handshake runs under the *startup* timeout — a server
//! that doesn't answer is reported and skipped rather than holding up the
//! session — and the connection is then switched to the *tool* timeout, past
//! which a call returns an error to the model and the connection stays
//! usable (see `conn.rs` and `http.rs` for how each transport gives up).

// Contract surface is consumed by the MCP implementation (in progress) and by
// `main` once registration is wired up.
#![allow(dead_code)]

mod catalog;
mod conn;
pub mod deferred;
mod http;
mod jsonrpc;
pub mod oauth;
mod prompt;
mod resource;
pub mod server;
#[cfg(test)]
pub(crate) mod testutil;
mod tool;

use std::collections::HashMap;
use std::io::BufRead;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::tools::Tool;
use conn::Conn;
use jsonrpc::JsonRpc;
use prompt::McpPromptTool;
use resource::McpResourceTool;
use tool::McpTool;

// Re-exported for `main` to pick up once HTTP-server registration is wired
// in (see the module doc comment); unused within `src/mcp/` itself for now,
// same as `connect_stdio`/`StdioServer` before this module was consumed.
#[allow(unused_imports)]
pub use http::{HttpServer, connect_http};

pub use catalog::Catalog;
pub use prompt::Prompt;
pub use tool::Annotations;

/// A connected server: what [`connect_stdio`] and [`connect_http`] return.
pub struct Connection {
    /// The server's tools, in the order it listed them, all sharing the
    /// connection (which lives as long as any of them does).
    pub tools: Vec<Box<dyn Tool>>,
    /// The `instructions` of the server's `initialize` answer, as sent.
    pub instructions: Option<String>,
    /// What the server said about each of `tools`, by the tool's registered
    /// (`mcp__<server>__<tool>`) name. The tools atelier adds itself
    /// (`read_resource`, `get_prompt`) have none.
    pub annotations: HashMap<String, Annotations>,
    /// The server's prompts, for the user to run as `/mcp:<server>:<prompt>`.
    /// The tool filters do not apply: they decide what the model is offered,
    /// and these are the user's to type.
    pub prompts: Vec<Prompt>,
}

/// Seconds a handshake request may take unless the server's
/// `startup_timeout_secs` says otherwise.
pub const DEFAULT_STARTUP_TIMEOUT_SECS: u64 = 10;
/// Seconds a tool call may take unless `tool_timeout_secs` says otherwise.
pub const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 120;

/// What can be set on a server of either transport (see the module docs).
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// When non-empty, only tools matching one of these (`*` wildcards) are
    /// kept.
    pub enabled_tools: Vec<String>,
    /// Tools dropped even if `enabled_tools` matches them.
    pub disabled_tools: Vec<String>,
    /// How long each handshake request may take.
    pub startup_timeout: Duration,
    /// How long each later request — a tool call — may take.
    pub tool_timeout: Duration,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            enabled_tools: Vec::new(),
            disabled_tools: Vec::new(),
            startup_timeout: Duration::from_secs(DEFAULT_STARTUP_TIMEOUT_SECS),
            tool_timeout: Duration::from_secs(DEFAULT_TOOL_TIMEOUT_SECS),
        }
    }
}

impl ServerOptions {
    /// Whether the tool the server calls `name` is offered to the model.
    /// The synthetic `read_resource` and `get_prompt` tools go by those
    /// names here, so they can be filtered like any other.
    pub fn allows(&self, name: &str) -> bool {
        let matches =
            |patterns: &[String]| patterns.iter().any(|p| crate::permissions::wild(p, name));
        (self.enabled_tools.is_empty() || matches(&self.enabled_tools))
            && !matches(&self.disabled_tools)
    }
}

/// Configuration for one stdio MCP server.
pub struct StdioServer {
    /// Logical server name, used for the `mcp__<server>__<tool>` prefix.
    pub name: String,
    /// Command to launch.
    pub command: String,
    /// Arguments to the command.
    pub args: Vec<String>,
    /// Variables to add to the server's environment (it inherits atelier's).
    pub env: Vec<(String, String)>,
    /// Tool filters and timeouts.
    pub options: ServerOptions,
}

/// A server to connect, of either transport.
pub enum Server {
    Stdio(StdioServer),
    Http(HttpServer),
}

impl Server {
    pub fn name(&self) -> &str {
        match self {
            Server::Stdio(s) => &s.name,
            Server::Http(s) => &s.name,
        }
    }

    fn connect(&self) -> Result<Connection> {
        match self {
            Server::Stdio(s) => connect_stdio(s),
            Server::Http(s) => connect_http(s),
        }
    }
}

/// Connect `servers` concurrently; the Nth outcome is the Nth server's (see
/// the module docs). One server failing, or being slow, costs the others
/// nothing.
pub fn connect_all(servers: &[Server]) -> Vec<Result<Connection>> {
    in_order(servers, Server::connect)
}

/// `f` applied to every item at once, each on a scoped thread, with the
/// results in the items' order whichever finished first. A panic in `f`
/// resurfaces here, as it would have in a plain loop.
fn in_order<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    // Nothing to gain from a thread for one item (the usual `/mcp add`).
    if let [only] = items {
        return vec![f(only)];
    }
    thread::scope(|scope| {
        let running: Vec<_> = items.iter().map(|item| scope.spawn(|| f(item))).collect();
        running
            .into_iter()
            .map(|handle| match handle.join() {
                Ok(result) => result,
                Err(panic) => std::panic::resume_unwind(panic),
            })
            .collect()
    })
}

/// Launch an MCP server over stdio and return its tools and instructions.
///
/// Spawns `server.command server.args...` with piped stdin/stdout, performs
/// the `initialize` / `notifications/initialized` handshake, then calls
/// `tools/list` and wraps each advertised tool as a [`Tool`]. The child
/// process and connection are shared (behind `Arc<Mutex<_>>`) across every
/// returned tool, and calls are serialized through it; the child is kept
/// alive for as long as any of the returned tools are, and is killed once
/// they're all dropped — or right here, if the handshake fails or times out.
pub fn connect_stdio(server: &StdioServer) -> Result<Connection> {
    let mut child = Command::new(&server.command)
        .args(&server.args)
        .envs(server.env.iter().map(|(name, value)| (name, value)))
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

    let options = &server.options;
    let mut conn = Conn::new(Some(child), stdin, stdout, options.startup_timeout);
    let found = discover(&mut conn, &server.name, "2024-11-05")?;
    conn.set_timeout(options.tool_timeout);
    Ok(assemble(
        Arc::new(Mutex::new(conn)),
        &server.name,
        options,
        found,
    ))
}

/// What a server said it offers, as raw protocol objects.
pub(crate) struct Discovered {
    instructions: Option<String>,
    tools: Vec<Value>,
    resources: Vec<Value>,
    prompts: Vec<Value>,
}

/// Everything asked of a server at connect time, over an already-connected
/// [`JsonRpc`] transport: the handshake and `tools/list`, which must
/// succeed, then the optional resources and prompts.
///
/// Shared by [`connect_stdio`] and [`connect_http`](http::connect_http) so
/// the two transports can't drift on protocol behavior.
pub(crate) fn discover(
    conn: &mut dyn JsonRpc,
    server_name: &str,
    protocol_version: &str,
) -> Result<Discovered> {
    let (instructions, tools) = handshake_and_list_tools(conn, server_name, protocol_version)?;
    Ok(Discovered {
        instructions,
        tools,
        resources: list_resources(conn),
        prompts: list_prompts(conn),
    })
}

/// Turn what [`discover`] found into a [`Connection`]. Its [`Tool`]s all
/// share `conn`: one per advertised tool, then `read_resource` and
/// `get_prompt` where the server has any — each only if `options` allows it.
pub(crate) fn assemble(
    conn: Arc<Mutex<dyn JsonRpc>>,
    server_name: &str,
    options: &ServerOptions,
    found: Discovered,
) -> Connection {
    let mut tools: Vec<Box<dyn Tool>> = Vec::new();
    let mut annotations = HashMap::new();
    for tool in wrap_tools(conn.clone(), server_name, options, found.tools) {
        annotations.insert(tool.name().to_string(), tool.annotations().clone());
        tools.push(Box::new(tool));
    }
    if options.allows("read_resource")
        && let Some(tool) = maybe_resource_tool(conn.clone(), server_name, found.resources)
    {
        tools.push(tool);
    }
    let prompts = Prompt::advertised(&conn, &found.prompts);
    if options.allows("get_prompt")
        && let Some(tool) = maybe_prompt_tool(conn, server_name, found.prompts)
    {
        tools.push(tool);
    }
    Connection {
        tools,
        instructions: found.instructions,
        annotations,
        prompts,
    }
}

/// Run the `initialize` → `notifications/initialized` → `tools/list`
/// handshake and return the server's `instructions`, if it gave any, and the
/// raw `tools` array it advertised.
fn handshake_and_list_tools(
    conn: &mut dyn JsonRpc,
    server_name: &str,
    protocol_version: &str,
) -> Result<(Option<String>, Vec<Value>)> {
    let initialized = conn
        .request(
            "initialize",
            json!({
                "protocolVersion": protocol_version,
                "capabilities": {},
                "clientInfo": { "name": "atelier", "version": "0.0.0" },
            }),
        )
        .with_context(|| format!("initializing MCP server `{server_name}`"))?;
    let instructions = initialized
        .get("instructions")
        .and_then(Value::as_str)
        .map(str::to_string);

    conn.notify("notifications/initialized", json!({}))
        .with_context(|| format!("completing handshake with MCP server `{server_name}`"))?;

    let tools_result = conn
        .request("tools/list", json!({}))
        .with_context(|| format!("listing tools from MCP server `{server_name}`"))?;

    let tools = tools_result
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok((instructions, tools))
}

/// Wrap each raw advertised-tool object (as returned by `tools/list`) that
/// `options` allows as a namespaced [`Tool`] sharing one connection.
fn wrap_tools(
    conn: Arc<Mutex<dyn JsonRpc>>,
    server_name: &str,
    options: &ServerOptions,
    advertised: Vec<Value>,
) -> Vec<McpTool> {
    let mut tools = Vec::with_capacity(advertised.len());
    for t in advertised {
        let Some(original_name) = t.get("name").and_then(Value::as_str) else {
            continue;
        };
        if !options.allows(original_name) {
            continue;
        }
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

        tools.push(McpTool::new(
            conn.clone(),
            namespaced_name,
            original_name.to_string(),
            description,
            parameters,
            Annotations::of(&t),
        ));
    }
    tools
}

/// Try `resources/list` on an already-connected transport and return the raw
/// `resources` array. Resource support is optional per the MCP spec: a
/// server that doesn't implement it (or errors for any other reason) simply
/// yields no resources here, rather than failing the whole connection.
fn list_resources(conn: &mut dyn JsonRpc) -> Vec<Value> {
    conn.request("resources/list", json!({}))
        .ok()
        .and_then(|result| result.get("resources").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

/// If `resources` is non-empty, build the one namespaced
/// `mcp__<server>__read_resource` [`Tool`] that reads any of them by URI.
/// Returns `None` when the server advertised no resources (including when
/// `resources/list` isn't supported at all).
fn maybe_resource_tool(
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

/// Try `prompts/list` on an already-connected transport and return the raw
/// `prompts` array. Prompt support is optional per the MCP spec: a server
/// that doesn't implement it (or errors for any other reason) simply yields
/// no prompts here, rather than failing the whole connection.
fn list_prompts(conn: &mut dyn JsonRpc) -> Vec<Value> {
    conn.request("prompts/list", json!({}))
        .ok()
        .and_then(|result| result.get("prompts").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

/// If `prompts` is non-empty, build the one namespaced
/// `mcp__<server>__get_prompt` [`Tool`] that renders any of them by `name`
/// via `prompts/get`. Returns `None` when the server advertised no prompts
/// (including when `prompts/list` isn't supported at all).
fn maybe_prompt_tool(
    conn: Arc<Mutex<dyn JsonRpc>>,
    server_name: &str,
    prompts: Vec<Value>,
) -> Option<Box<dyn Tool>> {
    if prompts.is_empty() {
        return None;
    }

    let namespaced_name = format!("mcp__{server_name}__get_prompt");
    let description = describe_prompts(&prompts);

    Some(Box::new(McpPromptTool::new(
        conn,
        namespaced_name,
        description,
    )))
}

/// Build the `get_prompt` tool's description: an instruction plus a capped
/// listing of the prompts discovered via `prompts/list` (name, description,
/// and argument names), so the model knows what's actually available and
/// what arguments each prompt expects.
fn describe_prompts(prompts: &[Value]) -> String {
    const CAP: usize = 30;

    let mut lines: Vec<String> = prompts
        .iter()
        .take(CAP)
        .map(|p| {
            let name = p.get("name").and_then(Value::as_str).unwrap_or("");
            let mut line = match p.get("description").and_then(Value::as_str) {
                Some(desc) if !desc.is_empty() => format!("- {name}: {desc}"),
                _ => format!("- {name}"),
            };
            if let Some(args) = p.get("arguments").and_then(Value::as_array)
                && !args.is_empty()
            {
                let arg_strs: Vec<String> = args
                    .iter()
                    .map(|a| {
                        let arg_name = a.get("name").and_then(Value::as_str).unwrap_or("");
                        let required = a.get("required").and_then(Value::as_bool).unwrap_or(false);
                        if required {
                            format!("{arg_name} (required)")
                        } else {
                            arg_name.to_string()
                        }
                    })
                    .collect();
                line.push_str(&format!(" [arguments: {}]", arg_strs.join(", ")));
            }
            line
        })
        .collect();
    if prompts.len() > CAP {
        lines.push(format!("- ... and {} more", prompts.len() - CAP));
    }

    format!(
        "Get an MCP prompt by `name` (optionally with `arguments`). Available prompts ({} total):\n{}",
        prompts.len(),
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
    /// resources/list, prompts/list, tools/call, resources/read,
    /// prompts/get) without depending on any real MCP server being
    /// installed. It expects exactly the request sequence `connect_stdio` +
    /// one `tools/call` per tool + one `resources/read` + one `prompts/get`
    /// produce, and replies with fixed, valid JSON-RPC frames.
    const FAKE_SERVER_SCRIPT: &str = r#"
IFS= read -r _init
printf '%s\n' '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fake","version":"0.0.0"}}}'
IFS= read -r _initialized_notification
IFS= read -r _tools_list
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"echo","description":"Echoes the input","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}},{"name":"boom","description":"Always fails","inputSchema":{}}]}}'
IFS= read -r _resources_list
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"resources":[{"uri":"file:///hello.txt","name":"hello","description":"A greeting","mimeType":"text/plain"}]}}'
IFS= read -r _prompts_list
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"prompts":[{"name":"greet","description":"Greets someone","arguments":[{"name":"who","description":"Who to greet","required":true}]}]}}'
IFS= read -r _call
printf '%s\n' '{"jsonrpc":"2.0","id":4,"result":{"content":[{"type":"text","text":"hello "},{"type":"text","text":"world"}]}}'
IFS= read -r _call2
printf '%s\n' '{"jsonrpc":"2.0","id":5,"result":{"isError":true,"content":[{"type":"text","text":"kaboom"}]}}'
IFS= read -r _resource_read
printf '%s\n' '{"jsonrpc":"2.0","id":6,"result":{"contents":[{"uri":"file:///hello.txt","mimeType":"text/plain","text":"hello resource"}]}}'
IFS= read -r _prompt_get
printf '%s\n' '{"jsonrpc":"2.0","id":7,"result":{"messages":[{"role":"user","content":{"type":"text","text":"Hello, world!"}}]}}'
"#;

    /// Same handshake as [`FAKE_SERVER_SCRIPT`], but `resources/list` and
    /// `prompts/list` both return a JSON-RPC error (as a server that doesn't
    /// support either capability might) instead of a result. Used to check
    /// that this doesn't fail the connection and simply yields no
    /// `read_resource`/`get_prompt` tools.
    const FAKE_SERVER_NO_RESOURCES_SCRIPT: &str = r#"
IFS= read -r _init
printf '%s\n' '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fake","version":"0.0.0"}}}'
IFS= read -r _initialized_notification
IFS= read -r _tools_list
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"echo","description":"Echoes the input","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]}}'
IFS= read -r _resources_list
printf '%s\n' '{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"Method not found"}}'
IFS= read -r _prompts_list
printf '%s\n' '{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"Method not found"}}'
"#;

    fn fake_server() -> StdioServer {
        StdioServer {
            name: "fake".into(),
            command: "sh".into(),
            args: vec!["-c".into(), FAKE_SERVER_SCRIPT.into()],
            env: Vec::new(),
            options: ServerOptions::default(),
        }
    }

    fn names(tools: &[Box<dyn Tool>]) -> Vec<&str> {
        tools.iter().map(|t| t.name()).collect()
    }

    fn filters(enabled: &[&str], disabled: &[&str]) -> ServerOptions {
        ServerOptions {
            enabled_tools: enabled.iter().map(|s| s.to_string()).collect(),
            disabled_tools: disabled.iter().map(|s| s.to_string()).collect(),
            ..ServerOptions::default()
        }
    }

    #[test]
    fn filters_match_the_servers_own_tool_names() {
        let all = ServerOptions::default();
        assert!(all.allows("anything"));

        let only = filters(&["list_*", "get_issue"], &[]);
        assert!(only.allows("list_issues"));
        assert!(only.allows("get_issue"));
        assert!(!only.allows("get_issues"));
        assert!(!only.allows("delete_issue"));

        // A deny wins over an allow, and works alone.
        let both = filters(&["list_*"], &["list_secrets"]);
        assert!(both.allows("list_issues"));
        assert!(!both.allows("list_secrets"));
        let without = filters(&[], &["delete_*"]);
        assert!(without.allows("list_issues"));
        assert!(!without.allows("delete_repo"));
    }

    /// A server with three tools, a resource and a prompt.
    fn github(method: &str, params: &Value) -> Option<Value> {
        match method {
            "resources/list" => Some(json!({ "resources": [{ "uri": "gh://readme" }] })),
            "prompts/list" => Some(json!({ "prompts": [{ "name": "triage" }] })),
            _ => testutil::tools_only(json!([
                { "name": "list_issues" },
                { "name": "get_issue" },
                { "name": "delete_issue" },
            ]))(method, params),
        }
    }

    fn connect_fake(options: &ServerOptions) -> Vec<Box<dyn Tool>> {
        let mut rpc = testutil::FakeRpc::new(github);
        let found = discover(&mut rpc, "gh", "2024-11-05").unwrap();
        assemble(Arc::new(Mutex::new(rpc)), "gh", options, found).tools
    }

    #[test]
    fn results_keep_the_order_given_whichever_finishes_first() {
        // The first item is the slowest: run at once, they finish in the
        // reverse order (one after another, they would finish as listed).
        let delays = [600u64, 300, 0];
        let finished = Mutex::new(Vec::new());
        let results = in_order(&delays, |&ms| {
            thread::sleep(Duration::from_millis(ms));
            finished.lock().unwrap().push(ms);
            ms
        });
        assert_eq!(results, delays);
        assert_eq!(*finished.lock().unwrap(), [0, 300, 600]);
        assert!(in_order(&[] as &[u64], |&ms| ms).is_empty());
    }

    #[test]
    fn servers_connect_concurrently_and_report_in_config_order() {
        use testutil::{FakeHttp, tools_only};
        // `slow` is listed first and answers last; `broken` fails in between.
        let slow = FakeHttp::mcp(|method, params| {
            if method == "initialize" {
                thread::sleep(Duration::from_millis(300));
            }
            tools_only(json!([{ "name": "a" }]))(method, params)
        });
        let fast = FakeHttp::mcp(tools_only(json!([{ "name": "b" }, { "name": "c" }])));
        let http = |name: &str, url: String| {
            Server::Http(HttpServer {
                name: name.into(),
                url,
                headers: Vec::new(),
                options: ServerOptions::default(),
                client_id: None,
                tokens: None,
            })
        };
        let servers = [
            http("slow", slow.url("/mcp")),
            Server::Stdio(StdioServer {
                name: "broken".into(),
                command: "definitely-not-a-real-binary-atelier-mcp-test".into(),
                args: Vec::new(),
                env: Vec::new(),
                options: ServerOptions::default(),
            }),
            http("fast", fast.url("/mcp")),
        ];

        let outcomes = connect_all(&servers);
        let tools: Vec<Vec<&str>> = outcomes
            .iter()
            .map(|o| o.as_ref().map(|c| names(&c.tools)).unwrap_or_default())
            .collect();
        assert_eq!(
            tools,
            [
                vec!["mcp__slow__a"],
                vec![],
                vec!["mcp__fast__b", "mcp__fast__c"],
            ]
        );
        assert!(outcomes[1].is_err());
        assert_eq!(servers[1].name(), "broken");
    }

    #[test]
    fn annotations_are_kept_per_tool() {
        let mut rpc = testutil::FakeRpc::new(testutil::tools_only(json!([
            { "name": "list", "annotations": { "title": "List things", "readOnlyHint": true } },
            { "name": "wipe", "annotations": { "destructiveHint": true, "readOnlyHint": false } },
            { "name": "odd", "annotations": { "readOnlyHint": true, "destructiveHint": true } },
            { "name": "new_style", "title": "Top-level title" },
            { "name": "plain" },
            { "name": "hidden", "annotations": { "readOnlyHint": true } },
        ])));
        let found = discover(&mut rpc, "s", "2024-11-05").unwrap();
        let options = filters(&[], &["hidden"]);
        let connection = assemble(Arc::new(Mutex::new(rpc)), "s", &options, found);
        let of = |name: &str| &connection.annotations[&format!("mcp__s__{name}")];

        assert_eq!(of("list").title.as_deref(), Some("List things"));
        assert_eq!(of("list").marker(), Some("[read-only]"));
        assert_eq!(of("wipe").marker(), Some("[destructive]"));
        assert_eq!(of("wipe").read_only, Some(false));
        // Read-only wins: "destructive" only qualifies a tool that writes.
        assert_eq!(of("odd").marker(), Some("[read-only]"));
        assert_eq!(of("new_style").title.as_deref(), Some("Top-level title"));
        // Unsaid is not "false", and earns no marker.
        assert_eq!(of("plain"), &Annotations::default());
        assert_eq!(of("plain").marker(), None);
        // Only the tools that were kept.
        assert_eq!(connection.annotations.len(), 5);
    }

    #[test]
    fn instructions_come_back_with_the_connection() {
        let connect = |initialize: Value| {
            let mut rpc = testutil::FakeRpc::new(move |method, params| match method {
                "initialize" => Some(initialize.clone()),
                _ => testutil::tools_only(json!([]))(method, params),
            });
            let found = discover(&mut rpc, "s", "2024-11-05").unwrap();
            assemble(
                Arc::new(Mutex::new(rpc)),
                "s",
                &ServerOptions::default(),
                found,
            )
        };
        let with = connect(json!({ "capabilities": {}, "instructions": "Call `plan` first." }));
        assert_eq!(with.instructions.as_deref(), Some("Call `plan` first."));
        assert!(
            connect(json!({ "capabilities": {} }))
                .instructions
                .is_none()
        );
        // Not a string: ignored rather than failing the connection.
        assert!(connect(json!({ "instructions": 7 })).instructions.is_none());
    }

    #[test]
    fn only_the_tools_the_filters_allow_are_returned() {
        assert_eq!(
            names(&connect_fake(&ServerOptions::default())),
            [
                "mcp__gh__list_issues",
                "mcp__gh__get_issue",
                "mcp__gh__delete_issue",
                "mcp__gh__read_resource",
                "mcp__gh__get_prompt",
            ]
        );
        assert_eq!(
            names(&connect_fake(&filters(&[], &["delete_*", "get_prompt"]))),
            [
                "mcp__gh__list_issues",
                "mcp__gh__get_issue",
                "mcp__gh__read_resource",
            ]
        );
        // An allow list leaves out the synthetic tools too unless it names
        // them.
        assert_eq!(
            names(&connect_fake(&filters(&["*_issue", "read_resource"], &[]))),
            [
                "mcp__gh__get_issue",
                "mcp__gh__delete_issue",
                "mcp__gh__read_resource",
            ]
        );
    }

    #[test]
    fn connects_lists_and_calls_tools() {
        let server = fake_server();
        let tools = connect_stdio(&server)
            .expect("connect_stdio should succeed")
            .tools;

        assert_eq!(tools.len(), 4);

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
            approved: false,
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
            approved: false,
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
            approved: false,
        };
        let out = read_resource
            .call(&mut ctx, json!({"uri": "file:///hello.txt"}))
            .expect("resources/read should succeed");
        assert_eq!(out, "hello resource");

        let get_prompt = tools
            .iter()
            .find(|t| t.name() == "mcp__fake__get_prompt")
            .expect("get_prompt tool should be present when prompts/list is non-empty");
        let spec = get_prompt.spec();
        assert_eq!(spec.name, "mcp__fake__get_prompt");
        // The description should list the discovered prompt (and its
        // arguments) so the model knows what it can request.
        assert!(spec.description.contains("greet"));
        assert!(spec.description.contains("Greets someone"));
        assert!(spec.description.contains("who (required)"));
        assert_eq!(spec.parameters["type"], "object");
        assert_eq!(spec.parameters["required"], json!(["name"]));

        let mut ctx = ToolCtx {
            project_root: root,
            fstate: &mut fstate,
            approved: false,
        };
        let out = get_prompt
            .call(
                &mut ctx,
                json!({"name": "greet", "arguments": {"who": "world"}}),
            )
            .expect("prompts/get should succeed");
        assert_eq!(out, "user: Hello, world!");
    }

    #[test]
    fn resources_and_prompts_list_error_yields_no_extra_tools() {
        let server = StdioServer {
            name: "fake".into(),
            command: "sh".into(),
            args: vec!["-c".into(), FAKE_SERVER_NO_RESOURCES_SCRIPT.into()],
            env: Vec::new(),
            options: ServerOptions::default(),
        };
        let tools = connect_stdio(&server)
            .expect("connect_stdio should succeed even when resources/list errors")
            .tools;

        assert_eq!(tools.len(), 1);
        assert!(
            tools.iter().all(|t| t.name() != "mcp__fake__read_resource"),
            "no read_resource tool should be added when resources/list fails"
        );
        assert!(
            tools.iter().all(|t| t.name() != "mcp__fake__get_prompt"),
            "no get_prompt tool should be added when prompts/list fails"
        );
    }

    /// A connection nothing is sent on.
    struct Unused;

    impl JsonRpc for Unused {
        fn request(&mut self, method: &str, _: Value) -> Result<Value> {
            panic!("unexpected request: {method}")
        }
        fn notify(&mut self, method: &str, _: Value) -> Result<()> {
            panic!("unexpected notification: {method}")
        }
    }

    #[test]
    fn only_a_true_read_only_hint_makes_a_tool_non_mutating() {
        let advertised = vec![
            json!({"name": "search", "annotations": {"readOnlyHint": true}}),
            json!({"name": "create", "annotations": {"readOnlyHint": false}}),
            json!({"name": "titled", "annotations": {"title": "x"}}),
            json!({"name": "truthy", "annotations": {"readOnlyHint": "true"}}),
            json!({"name": "plain"}),
        ];
        let tools = wrap_tools(
            Arc::new(Mutex::new(Unused)),
            "srv",
            &ServerOptions::default(),
            advertised,
        );
        let mutates: Vec<(String, bool)> = tools
            .iter()
            .map(|t| (t.name().to_string(), t.mutates(&json!({}))))
            .collect();
        let expected = [
            ("mcp__srv__search", false),
            ("mcp__srv__create", true),
            ("mcp__srv__titled", true),
            ("mcp__srv__truthy", true),
            ("mcp__srv__plain", true),
        ];
        assert_eq!(mutates.len(), expected.len());
        for ((name, got), (want_name, want)) in mutates.iter().zip(expected) {
            assert_eq!((name.as_str(), *got), (want_name, want));
        }
        // The hint is about modifying, not about asking: the call leaves the
        // machine either way.
        assert!(tools.iter().all(|t| t.requires_approval(&json!({}))));
    }

    #[test]
    fn missing_command_reports_a_clear_error() {
        let server = StdioServer {
            name: "nope".into(),
            command: "definitely-not-a-real-binary-atelier-mcp-test".into(),
            args: vec![],
            env: Vec::new(),
            options: ServerOptions::default(),
        };
        match connect_stdio(&server) {
            Ok(_) => panic!("spawning a missing binary should fail"),
            Err(err) => assert!(err.to_string().contains("spawning MCP server")),
        }
    }
}
