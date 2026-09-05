//! MCP "Streamable HTTP" transport.
//!
//! Each JSON-RPC message is sent as one blocking HTTP POST (via [`rsurl`]) to
//! the server's endpoint URL, with `Content-Type: application/json` and
//! `Accept: application/json, text/event-stream`. The server may reply with:
//!
//! - `Content-Type: application/json` — a single JSON-RPC object.
//! - `Content-Type: text/event-stream` — an SSE stream of `data: <json>`
//!   lines; we read the (non-chunked, already-buffered) response body and
//!   pick out the event whose `id` matches the request, ignoring any
//!   notifications the server interleaves.
//!
//! The `initialize` response may carry an `Mcp-Session-Id` response header;
//! if present, it is echoed back as a request header (`Mcp-Session-Id`) on
//! every subsequent call, as the spec requires.
//!
//! ## What this v1 does *not* cover
//!
//! - Server-initiated requests/notifications delivered on a long-lived GET
//!   SSE stream (the "server can push" half of Streamable HTTP). We only
//!   read the SSE events embedded in the direct response to our own POST.
//! - Resumable streams (`Last-Event-ID` replay).
//! - Batched JSON-RPC requests (an array of messages in one POST).
//! - Cancelling an in-flight request (`notifications/cancelled` is not sent
//!   automatically).
//!
//! These match "the common path" called out for a first version: a client
//! that sends requests and reads back either a plain JSON or a single-shot
//! SSE response, which is what most current MCP servers implement.

use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::thread;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::jsonrpc::JsonRpc;
use crate::tools::Tool;

/// Configuration for one Streamable-HTTP MCP server.
pub struct HttpServer {
    /// Logical server name, used for the `mcp__<server>__<tool>` prefix.
    pub name: String,
    /// The server's MCP endpoint URL.
    pub url: String,
    /// Extra headers to send on every request (e.g. `Authorization`).
    pub headers: Vec<(String, String)>,
}

/// Connect to an MCP server over Streamable HTTP and return its tools.
///
/// Performs the `initialize` / `notifications/initialized` handshake against
/// `server.url`, then calls `tools/list` and wraps each advertised tool as a
/// [`Tool`]. The connection (and any session id the server assigned) is
/// shared across every returned tool; calls are serialized through it.
pub fn connect_http(server: &HttpServer) -> Result<Vec<Box<dyn Tool>>> {
    let mut conn = HttpConn::new(server.url.clone(), server.headers.clone());

    // "2025-03-26" is the protocol revision that introduced Streamable HTTP;
    // servers speaking the older "2024-11-05" revision still accept this
    // value in `initialize` and negotiate down themselves per the spec.
    let advertised = super::handshake_and_list_tools(&mut conn, &server.name, "2025-03-26")?;

    let conn: Arc<Mutex<dyn JsonRpc>> = Arc::new(Mutex::new(conn));
    Ok(super::wrap_tools(conn, &server.name, advertised))
}

/// A JSON-RPC connection to an MCP server over Streamable HTTP: every message
/// is one POST to `url`, carrying `headers` plus (once known) the session id
/// the server assigned during `initialize`.
pub(crate) struct HttpConn {
    url: String,
    headers: Vec<(String, String)>,
    session_id: Option<String>,
    next_id: u64,
}

impl HttpConn {
    pub(crate) fn new(url: String, headers: Vec<(String, String)>) -> Self {
        Self {
            url,
            headers,
            session_id: None,
            next_id: 0,
        }
    }

    /// POST `msg`, capture a session id if the server sets one, and return
    /// the raw response body/content-type for the caller to interpret.
    fn post(&mut self, msg: &Value) -> Result<PostResponse> {
        let body = serde_json::to_vec(msg).context("encoding JSON-RPC message")?;

        let mut req = rsurl::Request::new("POST", &self.url)
            .context("building HTTP request")?
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(body);
        for (name, value) in &self.headers {
            req = req.header(name, value);
        }
        if let Some(session_id) = &self.session_id {
            req = req.header("mcp-session-id", session_id);
        }

        let resp = req.send().context("sending request to MCP HTTP server")?;
        if !(200..300).contains(&resp.status) {
            bail!(
                "MCP HTTP server returned HTTP {}: {}",
                resp.status,
                String::from_utf8_lossy(&resp.body)
            );
        }

        if let Some(session_id) = resp.header("mcp-session-id") {
            self.session_id = Some(session_id.to_string());
        }

        let content_type = resp.header("content-type").unwrap_or("").to_string();
        Ok(PostResponse {
            content_type,
            body: resp.body,
        })
    }
}

struct PostResponse {
    content_type: String,
    body: Vec<u8>,
}

impl JsonRpc for HttpConn {
    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;

        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let resp = self
            .post(&msg)
            .with_context(|| format!("sending `{method}` to MCP HTTP server"))?;

        let want_id = json!(id);
        let v = parse_response(&resp.content_type, &resp.body, &want_id)
            .with_context(|| format!("reading response to `{method}` from MCP HTTP server"))?;

        if let Some(err) = v.get("error") {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            bail!("MCP server returned an error for `{method}`: {msg} ({err})");
        }
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let msg = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        // Notifications get no matching response; a compliant server answers
        // with an empty `202 Accepted` body, which we simply discard. A
        // non-compliant server that echoes something back is fine too — we
        // don't try to parse it.
        self.post(&msg)
            .with_context(|| format!("sending `{method}` notification to MCP HTTP server"))?;
        Ok(())
    }
}

/// Parse a Streamable-HTTP response body into the JSON-RPC message matching
/// `want_id`, handling both a plain `application/json` object and a
/// `text/event-stream` body carrying one or more `data: <json>` events.
fn parse_response(content_type: &str, body: &[u8], want_id: &Value) -> Result<Value> {
    let content_type = content_type.split(';').next().unwrap_or("").trim();
    if content_type.eq_ignore_ascii_case("text/event-stream") {
        let text = String::from_utf8_lossy(body);
        extract_sse_response(&text, want_id)
    } else {
        // Treat anything else as a single JSON object — most servers that
        // don't stream just answer `application/json`, but we don't want to
        // hard-fail on a missing/odd content-type if the body still parses.
        serde_json::from_slice(body).context("parsing JSON-RPC response body")
    }
}

/// Scan an SSE-formatted body for `data: <json>` lines and return the first
/// JSON-RPC message whose `id` equals `want_id`. Lines that aren't `data:`
/// events, and `data:` payloads that aren't valid JSON or are notifications
/// (no `id`) or responses to a different request, are skipped.
fn extract_sse_response(body: &str, want_id: &Value) -> Result<Value> {
    for line in body.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if v.get("id") == Some(want_id) {
            return Ok(v);
        }
    }
    bail!("no JSON-RPC response with id {want_id} found in SSE stream")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_matching_event_from_sse_stream() {
        let body = "event: message\n\
                     data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{}}\n\
                     \n\
                     event: message\n\
                     data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"ok\":true}}\n\
                     \n";
        let want_id = json!(2);
        let v = extract_sse_response(body, &want_id).expect("should find the matching response");
        assert_eq!(v["result"]["ok"], true);
    }

    #[test]
    fn ignores_events_for_other_ids() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\
                     data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"ok\":true}}\n";
        let v = extract_sse_response(body, &json!(2)).expect("id 2 should be found");
        assert_eq!(v["result"]["ok"], true);
    }

    #[test]
    fn missing_response_is_an_error() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n";
        let err = extract_sse_response(body, &json!(0)).expect_err("no matching id present");
        assert!(err.to_string().contains("no JSON-RPC response"));
    }

    #[test]
    fn parse_response_handles_plain_json() {
        let body = br#"{"jsonrpc":"2.0","id":0,"result":{"tools":[]}}"#;
        let v = parse_response("application/json", body, &json!(0))
            .expect("plain JSON body should parse");
        assert_eq!(v["result"]["tools"], serde_json::json!([]));
    }

    #[test]
    fn parse_response_handles_json_with_charset() {
        let body = br#"{"jsonrpc":"2.0","id":5,"result":{}}"#;
        let v = parse_response("application/json; charset=utf-8", body, &json!(5))
            .expect("content-type params should be ignored");
        assert_eq!(v["id"], 5);
    }

    #[test]
    fn parse_response_handles_sse() {
        let body = b"data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"x\":1}}\n\n";
        let v = parse_response("text/event-stream", body, &json!(7))
            .expect("SSE body should be scanned for the matching event");
        assert_eq!(v["result"]["x"], 1);
    }

    /// Read one buffered HTTP/1.1 request off `stream`: the header block plus
    /// exactly `Content-Length` body bytes. Returns the lower-cased header map
    /// and the parsed JSON body (or `Value::Null` for an empty body, as with
    /// the `202` we send back for notifications).
    fn read_http_request(
        stream: &mut std::net::TcpStream,
    ) -> (std::collections::HashMap<String, String>, Value) {
        use std::io::{BufRead, BufReader, Read};

        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        let mut request_line = String::new();
        reader
            .read_line(&mut request_line)
            .expect("read request line");

        let mut headers = std::collections::HashMap::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read header line");
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }

        let content_length: usize = headers
            .get("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).expect("read request body");
        let v = if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body).expect("request body should be JSON")
        };
        (headers, v)
    }

    /// Write a minimal HTTP/1.1 response with the given status, body, and any
    /// extra headers (e.g. `Mcp-Session-Id`).
    fn write_http_response(
        stream: &mut std::net::TcpStream,
        status: u16,
        extra_headers: &[(&str, &str)],
        body: &str,
    ) {
        use std::io::Write;

        let mut resp = format!(
            "HTTP/1.1 {status} OK\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n",
            body.len()
        );
        for (k, v) in extra_headers {
            resp.push_str(&format!("{k}: {v}\r\n"));
        }
        resp.push_str("\r\n");
        resp.push_str(body);
        stream.write_all(resp.as_bytes()).expect("write response");
        stream.flush().expect("flush response");
    }

    /// End-to-end exercise of [`connect_http`] against a hand-rolled HTTP/1.1
    /// server (no real MCP server needed): runs the full `initialize` →
    /// `notifications/initialized` → `tools/list` → `tools/call` sequence over
    /// real TCP sockets, checks the `Mcp-Session-Id` returned on `initialize`
    /// is echoed back on every later request, and that a plain
    /// `application/json` response is parsed correctly end to end.
    #[test]
    fn connect_http_end_to_end_with_session_id() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");

        let server_thread = thread::spawn(move || {
            // initialize
            let (mut stream, _) = listener.accept().expect("accept #1");
            let (headers, req) = read_http_request(&mut stream);
            assert_eq!(req["method"], "initialize");
            assert!(!headers.contains_key("mcp-session-id"));
            let id = req["id"].clone();
            let body = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "serverInfo": {"name": "fake", "version": "0.0.0"},
                },
            })
            .to_string();
            write_http_response(&mut stream, 200, &[("Mcp-Session-Id", "sess-123")], &body);

            // notifications/initialized
            let (mut stream, _) = listener.accept().expect("accept #2");
            let (headers, req) = read_http_request(&mut stream);
            assert_eq!(req["method"], "notifications/initialized");
            assert_eq!(
                headers.get("mcp-session-id").map(String::as_str),
                Some("sess-123")
            );
            write_http_response(&mut stream, 202, &[], "");

            // tools/list
            let (mut stream, _) = listener.accept().expect("accept #3");
            let (headers, req) = read_http_request(&mut stream);
            assert_eq!(req["method"], "tools/list");
            assert_eq!(
                headers.get("mcp-session-id").map(String::as_str),
                Some("sess-123")
            );
            let body = json!({
                "jsonrpc": "2.0",
                "id": req["id"],
                "result": {
                    "tools": [{
                        "name": "echo",
                        "description": "Echoes the input",
                        "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}},
                    }],
                },
            })
            .to_string();
            write_http_response(&mut stream, 200, &[], &body);

            // tools/call
            let (mut stream, _) = listener.accept().expect("accept #4");
            let (headers, req) = read_http_request(&mut stream);
            assert_eq!(req["method"], "tools/call");
            assert_eq!(
                headers.get("mcp-session-id").map(String::as_str),
                Some("sess-123")
            );
            assert_eq!(req["params"]["name"], "echo");
            let body = json!({
                "jsonrpc": "2.0",
                "id": req["id"],
                "result": {"content": [{"type": "text", "text": "hello http"}]},
            })
            .to_string();
            write_http_response(&mut stream, 200, &[], &body);
        });

        let server = HttpServer {
            name: "fake".into(),
            url: format!("http://{addr}/mcp"),
            headers: vec![],
        };
        let tools = connect_http(&server).expect("connect_http should succeed");
        assert_eq!(tools.len(), 1);

        let echo = &tools[0];
        assert_eq!(echo.name(), "mcp__fake__echo");
        assert_eq!(echo.spec().description, "Echoes the input");

        let root = std::path::Path::new("/");
        let mut fstate = crate::tools::FileState::new();
        let mut ctx = crate::tools::ToolCtx {
            project_root: root,
            fstate: &mut fstate,
        };
        let out = echo
            .call(&mut ctx, json!({"text": "hi"}))
            .expect("tools/call should succeed");
        assert_eq!(out, "hello http");

        server_thread
            .join()
            .expect("server thread should not panic");
    }
}
