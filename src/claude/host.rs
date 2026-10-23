//! atelier's own tools, offered to claude over MCP.
//!
//! claude reaches extra tools only through MCP servers it launches itself,
//! but atelier's tools need the live session: the project root they're
//! confined to, the read-tracking that guards edits, the approval prompt and
//! the "always allow" list. So the server claude launches is only a relay:
//! `atelier mcp-serve <socket>` ([`relay`]) connects back to the atelier
//! process that started claude, over a Unix socket in a private directory,
//! and pipes MCP's stdio JSON-RPC through it. The protocol is answered here,
//! in the original process: `initialize` and `tools/list` directly, and
//! `tools/call` by handing the call to the bridge's event loop
//! ([`super::Incoming::Call`]), which runs it with the session's tools and
//! interface — mid-turn or while idle — and sends back the result.
//!
//! The relay authenticates with a random token, passed to it through the
//! environment of the MCP server entry atelier gives claude; connections
//! that don't present it are dropped.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::Incoming;
use crate::provider::ToolSpec;

/// The MCP server name; claude exposes the tools as `mcp__atelier__<tool>`.
pub const SERVER_NAME: &str = "atelier";
/// Environment variable carrying the relay's token.
const TOKEN_ENV: &str = "ATELIER_MCP_TOKEN";
/// Advertised when the client doesn't name an MCP protocol version.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// A `tools/call` waiting for the session to run it.
pub struct ToolRequest {
    pub name: String,
    pub arguments: Value,
    /// `(result_text, ok)`.
    pub reply: Sender<(String, bool)>,
}

/// The socket end of the relay, owned by the bridge. Dropping it removes the
/// socket (its directory included).
pub struct Host {
    dir: PathBuf,
    socket: PathBuf,
    token: String,
}

impl Host {
    /// Listen for relays, serving `specs`; tool calls go to `calls`.
    pub fn start(specs: Vec<ToolSpec>, calls: Sender<Incoming>) -> Result<Self> {
        let token = super::new_uuid();
        let dir =
            std::env::temp_dir().join(format!("atelier-{}-{}", std::process::id(), &token[..8]));
        // Private (0700): the socket is reachable only by this user.
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        let socket = dir.join("mcp.sock");
        let listener = UnixListener::bind(&socket)
            .with_context(|| format!("listening on {}", socket.display()))?;
        let specs = Arc::new(specs);
        let expected = token.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let (specs, calls, token) = (Arc::clone(&specs), calls.clone(), expected.clone());
                std::thread::spawn(move || {
                    let _ = serve(stream, &token, &specs, &calls);
                });
            }
        });
        Ok(Self { dir, socket, token })
    }

    /// The `--mcp-config` entry that makes claude launch the relay.
    pub fn mcp_entry(&self) -> Result<Value> {
        let exe = std::env::current_exe().context("locating the atelier executable")?;
        Ok(json!({
            "type": "stdio",
            "command": exe,
            "args": ["mcp-serve", self.socket],
            "env": { TOKEN_ENV: self.token },
        }))
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Serve one relay connection: check its token, then answer JSON-RPC lines.
fn serve(
    stream: UnixStream,
    token: &str,
    specs: &[ToolSpec],
    calls: &Sender<Incoming>,
) -> Result<()> {
    let mut out = stream.try_clone()?;
    let mut lines = BufReader::new(stream).lines();
    if lines.next().transpose()?.as_deref() != Some(token) {
        return Ok(()); // not our relay
    }
    for line in lines {
        let Ok(msg) = serde_json::from_str::<Value>(&line?) else {
            continue;
        };
        let reply = handle(&msg, specs, |name, arguments| {
            let (tx, rx) = mpsc::channel();
            let req = ToolRequest {
                name: name.to_string(),
                arguments,
                reply: tx,
            };
            if calls.send(Incoming::Call(req)).is_err() {
                return ("error: atelier is shutting down".to_string(), false);
            }
            rx.recv()
                .unwrap_or_else(|_| ("error: atelier dropped the call".to_string(), false))
        });
        if let Some(reply) = reply {
            writeln!(out, "{reply}")?;
            out.flush()?;
        }
    }
    Ok(())
}

/// Answer one MCP JSON-RPC message; `None` for notifications. Tool calls go
/// through `call(name, arguments) -> (text, ok)`.
fn handle(
    msg: &Value,
    specs: &[ToolSpec],
    call: impl FnOnce(&str, Value) -> (String, bool),
) -> Option<Value> {
    let id = msg.get("id")?.clone();
    let params = &msg["params"];
    let result = match msg["method"].as_str().unwrap_or_default() {
        "initialize" => json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or(PROTOCOL_VERSION),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") },
        }),
        "ping" => json!({}),
        "tools/list" => json!({
            "tools": specs.iter().map(|s| json!({
                "name": s.name,
                "description": s.description,
                "inputSchema": s.parameters,
            })).collect::<Vec<_>>(),
        }),
        "tools/call" => {
            let Some(name) = params["name"].as_str() else {
                return Some(error(id, -32602, "tools/call: missing tool name"));
            };
            let arguments = match &params["arguments"] {
                Value::Null => json!({}),
                a => a.clone(),
            };
            let (text, ok) = call(name, arguments);
            json!({
                "content": [{ "type": "text", "text": text }],
                "isError": !ok,
            })
        }
        other => return Some(error(id, -32601, &format!("method not found: {other}"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// `atelier mcp-serve <socket>`: what claude launches. Connects to the
/// atelier process listening on `socket`, authenticates, and pipes stdio
/// through until either side closes.
pub fn relay(socket: &str) -> Result<()> {
    let token = std::env::var(TOKEN_ENV)
        .with_context(|| format!("{TOKEN_ENV} not set (mcp-serve is launched by atelier)"))?;
    let mut sock =
        UnixStream::connect(socket).with_context(|| format!("connecting to {socket}"))?;
    writeln!(sock, "{token}")?;
    let mut up = sock.try_clone()?;
    std::thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut up);
        let _ = up.shutdown(std::net::Shutdown::Write);
    });
    // Stdout is line-buffered, and the protocol is one message per line.
    io::copy(&mut sock, &mut io::stdout().lock())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn specs() -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "read".into(),
            description: "Read a file.".into(),
            parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        }]
    }

    fn no_call(_: &str, _: Value) -> (String, bool) {
        panic!("unexpected tool call")
    }

    #[test]
    fn initialize_echoes_protocol_version() {
        let r = handle(
            &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": {"protocolVersion": "2025-03-26"}}),
            &specs(),
            no_call,
        )
        .unwrap();
        assert_eq!(r["id"], 1);
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(r["result"]["serverInfo"]["name"], "atelier");
        assert!(r["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn lists_tools_with_input_schema() {
        let r = handle(
            &json!({"jsonrpc": "2.0", "id": "a", "method": "tools/list"}),
            &specs(),
            no_call,
        )
        .unwrap();
        let tools = r["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "read");
        assert_eq!(tools[0]["inputSchema"]["type"], "object");
    }

    #[test]
    fn calls_tools_and_reports_errors() {
        let call = json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call",
                          "params": {"name": "read", "arguments": {"path": "a"}}});
        let r = handle(&call, &specs(), |name, args| {
            assert_eq!(name, "read");
            assert_eq!(args, json!({"path": "a"}));
            ("contents".into(), true)
        })
        .unwrap();
        assert_eq!(r["result"]["content"][0]["text"], "contents");
        assert_eq!(r["result"]["isError"], false);

        let r = handle(&call, &specs(), |_, _| ("error: nope".into(), false)).unwrap();
        assert_eq!(r["result"]["isError"], true);

        // Missing arguments become an empty object.
        let bare = json!({"jsonrpc": "2.0", "id": 8, "method": "tools/call",
                          "params": {"name": "read"}});
        handle(&bare, &specs(), |_, args| {
            assert_eq!(args, json!({}));
            (String::new(), true)
        });
    }

    #[test]
    fn notifications_get_no_reply_and_unknown_methods_an_error() {
        let n = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert!(handle(&n, &specs(), no_call).is_none());
        let r = handle(
            &json!({"jsonrpc": "2.0", "id": 2, "method": "resources/list"}),
            &specs(),
            no_call,
        )
        .unwrap();
        assert_eq!(r["error"]["code"], -32601);
    }

    #[test]
    fn socket_round_trip_requires_token() {
        let (tx, rx) = mpsc::channel();
        let host = Host::start(specs(), tx).unwrap();
        // Answer tool calls the way the bridge would.
        std::thread::spawn(move || {
            for inc in rx {
                if let Incoming::Call(req) = inc {
                    let _ = req.reply.send((format!("ran {}", req.name), true));
                }
            }
        });

        // Wrong token: the connection is dropped without an answer.
        let mut bad = UnixStream::connect(&host.socket).unwrap();
        writeln!(bad, "wrong-token").unwrap();
        writeln!(bad, r#"{{"jsonrpc":"2.0","id":1,"method":"ping"}}"#).unwrap();
        let mut s = String::new();
        assert_eq!(BufReader::new(&bad).read_line(&mut s).unwrap(), 0);

        let mut good = UnixStream::connect(&host.socket).unwrap();
        writeln!(good, "{}", host.token).unwrap();
        writeln!(
            good,
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"read","arguments":{{}}}}}}"#
        )
        .unwrap();
        let mut line = String::new();
        BufReader::new(&good).read_line(&mut line).unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["result"]["content"][0]["text"], "ran read");

        let dir = host.dir.clone();
        drop(host);
        assert!(!dir.exists());
    }
}
