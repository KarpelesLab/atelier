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
//!
//! ## Timeouts
//!
//! Every POST is given the connection's current timeout (the server's
//! startup timeout during the handshake, its tool timeout afterwards) from
//! start to end. The exchange runs on a thread of its own and the caller
//! waits on a channel for that long ([`send_within`]): the bound then holds
//! whatever the HTTP client is blocked in — connecting, a response that
//! never starts, one that trickles. The same limits are also set on the
//! request, so an exchange that was given up on ends by itself rather than
//! lingering. Nothing needs resetting afterwards: each message is its own
//! HTTP exchange, so the next call starts clean.

use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::jsonrpc::JsonRpc;
use super::{Connection, ServerOptions};

/// Configuration for one Streamable-HTTP MCP server.
pub struct HttpServer {
    /// Logical server name, used for the `mcp__<server>__<tool>` prefix.
    pub name: String,
    /// The server's MCP endpoint URL.
    pub url: String,
    /// Extra headers to send on every request (e.g. `Authorization`).
    pub headers: Vec<(String, String)>,
    /// Tool filters and timeouts.
    pub options: ServerOptions,
}

/// Connect to an MCP server over Streamable HTTP and return its tools and
/// instructions.
///
/// Performs the `initialize` / `notifications/initialized` handshake against
/// `server.url`, then calls `tools/list` and wraps each advertised tool as a
/// [`Tool`]. The connection (and any session id the server assigned) is
/// shared across every returned tool; calls are serialized through it.
pub fn connect_http(server: &HttpServer) -> Result<Connection> {
    let options = &server.options;
    let mut conn = HttpConn::new(
        server.url.clone(),
        server.headers.clone(),
        options.startup_timeout,
    );

    // "2025-03-26" is the protocol revision that introduced Streamable HTTP;
    // servers speaking the older "2024-11-05" revision still accept this
    // value in `initialize` and negotiate down themselves per the spec.
    let found = super::discover(&mut conn, &server.name, "2025-03-26")?;
    conn.timeout = options.tool_timeout;
    Ok(super::assemble(
        Arc::new(Mutex::new(conn)),
        &server.name,
        options,
        found,
    ))
}

/// A JSON-RPC connection to an MCP server over Streamable HTTP: every message
/// is one POST to `url`, carrying `headers` plus (once known) the session id
/// the server assigned during `initialize`.
pub(crate) struct HttpConn {
    url: String,
    headers: Vec<(String, String)>,
    session_id: Option<String>,
    next_id: u64,
    /// How long one POST may take, start to end.
    timeout: Duration,
}

impl HttpConn {
    pub(crate) fn new(url: String, headers: Vec<(String, String)>, timeout: Duration) -> Self {
        Self {
            url,
            headers,
            session_id: None,
            next_id: 0,
            timeout,
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
            .max_time(self.timeout)
            .read_timeout(Some(self.timeout))
            .body(body);
        for (name, value) in &self.headers {
            req = req.header(name, value);
        }
        if let Some(session_id) = &self.session_id {
            req = req.header("mcp-session-id", session_id);
        }

        let resp = send_within(req, self.timeout)?;
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

/// Send `req`, giving up once `timeout` has passed (see the module docs).
fn send_within(req: rsurl::Request, timeout: Duration) -> Result<rsurl::Response> {
    let (tx, rx) = channel();
    thread::spawn(move || {
        // Nobody is listening any more if the caller gave up.
        let _ = tx.send(req.send());
    });
    match rx.recv_timeout(timeout) {
        Ok(resp) => resp.context("sending request to MCP HTTP server"),
        Err(_) => bail!(
            "no answer from the MCP HTTP server within {}s",
            timeout.as_secs_f32()
        ),
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
    /// `notifications/initialized` → `tools/list` → `resources/list` →
    /// `prompts/list` → `tools/call` → `resources/read` → `prompts/get`
    /// sequence over real TCP sockets, checks the `Mcp-Session-Id` returned
    /// on `initialize` is echoed back on every later request, and that a
    /// plain `application/json` response is parsed correctly end to end —
    /// including the `read_resource` tool built from `resources/list` and
    /// the `get_prompt` tool built from `prompts/list`.
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

            // resources/list
            let (mut stream, _) = listener.accept().expect("accept #4");
            let (headers, req) = read_http_request(&mut stream);
            assert_eq!(req["method"], "resources/list");
            assert_eq!(
                headers.get("mcp-session-id").map(String::as_str),
                Some("sess-123")
            );
            let body = json!({
                "jsonrpc": "2.0",
                "id": req["id"],
                "result": {
                    "resources": [{
                        "uri": "file:///hello.txt",
                        "name": "hello",
                        "mimeType": "text/plain",
                    }],
                },
            })
            .to_string();
            write_http_response(&mut stream, 200, &[], &body);

            // prompts/list
            let (mut stream, _) = listener.accept().expect("accept #5");
            let (headers, req) = read_http_request(&mut stream);
            assert_eq!(req["method"], "prompts/list");
            assert_eq!(
                headers.get("mcp-session-id").map(String::as_str),
                Some("sess-123")
            );
            let body = json!({
                "jsonrpc": "2.0",
                "id": req["id"],
                "result": {
                    "prompts": [{
                        "name": "greet",
                        "description": "Greets someone",
                        "arguments": [{"name": "who", "description": "Who to greet", "required": true}],
                    }],
                },
            })
            .to_string();
            write_http_response(&mut stream, 200, &[], &body);

            // tools/call
            let (mut stream, _) = listener.accept().expect("accept #6");
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

            // resources/read
            let (mut stream, _) = listener.accept().expect("accept #7");
            let (headers, req) = read_http_request(&mut stream);
            assert_eq!(req["method"], "resources/read");
            assert_eq!(
                headers.get("mcp-session-id").map(String::as_str),
                Some("sess-123")
            );
            assert_eq!(req["params"]["uri"], "file:///hello.txt");
            let body = json!({
                "jsonrpc": "2.0",
                "id": req["id"],
                "result": {
                    "contents": [{
                        "uri": "file:///hello.txt",
                        "mimeType": "text/plain",
                        "text": "hello resource",
                    }],
                },
            })
            .to_string();
            write_http_response(&mut stream, 200, &[], &body);

            // prompts/get
            let (mut stream, _) = listener.accept().expect("accept #8");
            let (headers, req) = read_http_request(&mut stream);
            assert_eq!(req["method"], "prompts/get");
            assert_eq!(
                headers.get("mcp-session-id").map(String::as_str),
                Some("sess-123")
            );
            assert_eq!(req["params"]["name"], "greet");
            assert_eq!(req["params"]["arguments"]["who"], "http world");
            let body = json!({
                "jsonrpc": "2.0",
                "id": req["id"],
                "result": {
                    "messages": [{
                        "role": "user",
                        "content": {"type": "text", "text": "Hello, http world!"},
                    }],
                },
            })
            .to_string();
            write_http_response(&mut stream, 200, &[], &body);
        });

        let server = HttpServer {
            name: "fake".into(),
            url: format!("http://{addr}/mcp"),
            headers: vec![],
            options: ServerOptions::default(),
        };
        let tools = connect_http(&server)
            .expect("connect_http should succeed")
            .tools;
        assert_eq!(tools.len(), 3);

        let echo = tools
            .iter()
            .find(|t| t.name() == "mcp__fake__echo")
            .expect("echo tool should be present");
        assert_eq!(echo.spec().description, "Echoes the input");

        let root = std::path::Path::new("/");
        let mut fstate = crate::tools::FileState::new();
        let mut ctx = crate::tools::ToolCtx {
            project_root: root,
            fstate: &mut fstate,
            approved: false,
        };
        let out = echo
            .call(&mut ctx, json!({"text": "hi"}))
            .expect("tools/call should succeed");
        assert_eq!(out, "hello http");

        let read_resource = tools
            .iter()
            .find(|t| t.name() == "mcp__fake__read_resource")
            .expect("read_resource tool should be present when resources/list is non-empty");
        assert!(
            read_resource
                .spec()
                .description
                .contains("file:///hello.txt")
        );
        let mut ctx = crate::tools::ToolCtx {
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
        assert!(get_prompt.spec().description.contains("greet"));
        let mut ctx = crate::tools::ToolCtx {
            project_root: root,
            fstate: &mut fstate,
            approved: false,
        };
        let out = get_prompt
            .call(
                &mut ctx,
                json!({"name": "greet", "arguments": {"who": "http world"}}),
            )
            .expect("prompts/get should succeed");
        assert_eq!(out, "user: Hello, http world!");

        server_thread
            .join()
            .expect("server thread should not panic");
    }

    use crate::mcp::testutil::{FakeHttp, tools_only};

    fn server(fake: &FakeHttp, startup_ms: u64, tool_ms: u64) -> HttpServer {
        HttpServer {
            name: "fake".into(),
            url: fake.url("/mcp"),
            headers: vec![],
            options: ServerOptions {
                startup_timeout: Duration::from_millis(startup_ms),
                tool_timeout: Duration::from_millis(tool_ms),
                ..ServerOptions::default()
            },
        }
    }

    #[test]
    fn a_server_that_does_not_answer_initialize_is_given_up_on() {
        let fake = FakeHttp::mcp(|method, params| {
            if method == "initialize" {
                thread::sleep(Duration::from_secs(2));
            }
            tools_only(json!([]))(method, params)
        });
        let started = std::time::Instant::now();
        let err = match connect_http(&server(&fake, 200, 5_000)) {
            Ok(_) => panic!("the handshake should have timed out"),
            Err(e) => format!("{e:#}"),
        };
        assert!(err.contains("initializing MCP server `fake`"), "{err}");
        assert!(started.elapsed() < Duration::from_millis(1_500), "{err}");
    }

    #[test]
    fn a_tool_call_past_its_timeout_fails_and_the_next_one_works() {
        let fake = FakeHttp::mcp(|method, params| {
            if method == "tools/call" && params["name"] == "slow" {
                thread::sleep(Duration::from_secs(2));
            }
            tools_only(json!([{ "name": "slow" }, { "name": "fast" }]))(method, params)
        });
        let tools = connect_http(&server(&fake, 5_000, 300))
            .expect("connects")
            .tools;
        let call = |name: &str| {
            let mut fstate = crate::tools::FileState::new();
            let mut ctx = crate::tools::ToolCtx {
                project_root: std::path::Path::new("/"),
                fstate: &mut fstate,
                approved: true,
            };
            let tool = tools.iter().find(|t| t.name() == name).unwrap();
            tool.call(&mut ctx, json!({}))
        };
        let started = std::time::Instant::now();
        let err = call("mcp__fake__slow").unwrap_err();
        assert!(started.elapsed() < Duration::from_millis(1_500));
        assert!(format!("{err:#}").contains("no answer from the MCP HTTP server within"));
        assert_eq!(call("mcp__fake__fast").unwrap(), "called \"fast\"");
    }
}
