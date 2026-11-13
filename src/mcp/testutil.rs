//! Fakes for the MCP tests: a server reached in-process through the
//! [`JsonRpc`] trait, and one reached over real HTTP on a loopback port.
//!
//! Neither needs an interpreter or a real MCP server installed. Both are
//! driven by a closure from method and params to the result, so each test
//! states the server it wants in a few lines.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::jsonrpc::JsonRpc;

/// What a fake server does with a request: `Some(result)`, or `None` for
/// "method not found" (how a server without resources answers
/// `resources/list`).
pub(crate) type Handler = dyn Fn(&str, &Value) -> Option<Value> + Send + Sync;

/// An in-process MCP server: every request goes straight to the handler.
pub(crate) struct FakeRpc {
    handler: Box<Handler>,
    /// The methods requested so far, shared so a test can keep looking
    /// after the connection has been handed to the tools.
    pub(crate) seen: Arc<Mutex<Vec<String>>>,
}

impl FakeRpc {
    pub(crate) fn new(
        handler: impl Fn(&str, &Value) -> Option<Value> + Send + Sync + 'static,
    ) -> Self {
        Self {
            handler: Box::new(handler),
            seen: Arc::default(),
        }
    }
}

impl JsonRpc for FakeRpc {
    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.seen.lock().unwrap().push(method.to_string());
        match (self.handler)(method, &params) {
            Some(result) => Ok(result),
            None => bail!("MCP server returned an error for `{method}`: Method not found"),
        }
    }

    fn notify(&mut self, _method: &str, _params: Value) -> Result<()> {
        Ok(())
    }
}

/// A handler for a server offering `tools` (raw `tools/list` entries) and
/// nothing else, whose every tool call answers with the called tool's name.
pub(crate) fn tools_only(tools: Value) -> impl Fn(&str, &Value) -> Option<Value> + Send + Sync {
    move |method, params| match method {
        "initialize" => Some(json!({ "capabilities": {} })),
        "tools/list" => Some(json!({ "tools": tools })),
        "tools/call" => Some(json!({
            "content": [{ "type": "text", "text": format!("called {}", params["name"]) }],
        })),
        _ => None,
    }
}

/// One HTTP request as a fake server sees it.
pub(crate) struct HttpRequest {
    /// `GET`, `POST`.
    pub(crate) method: String,
    /// Path and query.
    pub(crate) target: String,
    /// Header names lower-cased.
    pub(crate) headers: HashMap<String, String>,
    pub(crate) body: Vec<u8>,
}

/// What a fake server answers.
pub(crate) struct HttpReply {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: String,
}

impl HttpReply {
    pub(crate) fn json(status: u16, body: &Value) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: body.to_string(),
        }
    }
}

/// An HTTP/1.1 server on a loopback port, answering every request through
/// one closure — each on a thread of its own, so a handler that sleeps (a
/// slow server) doesn't hold up the next request. Stops when dropped.
pub(crate) struct FakeHttp {
    pub(crate) addr: SocketAddr,
    stop: Arc<AtomicBool>,
}

impl FakeHttp {
    pub(crate) fn serve(
        handler: impl Fn(&HttpRequest) -> HttpReply + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let handler = Arc::new(handler);
        thread::spawn(move || {
            for stream in listener.incoming() {
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(mut stream) = stream else { continue };
                let handler = handler.clone();
                thread::spawn(move || {
                    if let Some(request) = read_request(&mut stream) {
                        write_reply(&mut stream, &handler(&request));
                    }
                });
            }
        });
        Self { addr, stop }
    }

    /// An MCP server over Streamable HTTP: JSON-RPC requests go to
    /// `handler`, notifications get the `202` the spec asks for.
    pub(crate) fn mcp(
        handler: impl Fn(&str, &Value) -> Option<Value> + Send + Sync + 'static,
    ) -> Self {
        Self::serve(move |request| mcp_reply(request, &handler))
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }
}

impl Drop for FakeHttp {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so it sees the flag.
        let _ = TcpStream::connect(self.addr);
    }
}

/// Answer one POSTed JSON-RPC message the way an MCP server would.
pub(crate) fn mcp_reply(request: &HttpRequest, handler: &Handler) -> HttpReply {
    let msg: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
    let Some(id) = msg.get("id") else {
        return HttpReply {
            status: 202,
            headers: Vec::new(),
            body: String::new(),
        };
    };
    let method = msg["method"].as_str().unwrap_or("");
    let body = match handler(method, &msg["params"]) {
        Some(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        None => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": "Method not found" },
        }),
    };
    HttpReply::json(200, &body)
}

/// Read one request: the head, then exactly `Content-Length` body bytes.
/// `None` for a connection that sent nothing (the wake-up on drop).
fn read_request(stream: &mut TcpStream) -> Option<HttpRequest> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut words = line.split_whitespace();
    let method = words.next()?.to_string();
    let target = words.next()?.to_string();

    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    Some(HttpRequest {
        method,
        target,
        headers,
        body,
    })
}

fn write_reply(stream: &mut TcpStream, reply: &HttpReply) {
    let mut head = format!(
        "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reply.body.len()
    );
    for (k, v) in &reply.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    // The client may have given up (a timeout test): nothing to do then.
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(reply.body.as_bytes());
    let _ = stream.flush();
}
