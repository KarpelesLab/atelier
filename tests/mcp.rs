//! End-to-end tests of the MCP client in the real `atelier` binary.
//!
//! Like `tests/e2e.rs`, nothing here leaves the machine: the model API and
//! the MCP servers are both small HTTP servers on loopback ports, built on
//! one [`Server`] that answers every request through a closure. atelier runs
//! as the line REPL (stdin is a pipe), which is the interface that connects
//! the configured servers and reads approval answers from its input.
//!
//! The harness is this file's own rather than shared with `e2e.rs`: an
//! integration test file is its own crate, and the two need different mocks
//! (that one scripts a model, this one mostly scripts MCP servers).

#![cfg(unix)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::SystemTime;

use serde_json::{Value, json};

/// One HTTP request as a fake server sees it.
struct Request {
    body: String,
}

/// What a fake server answers: status, extra headers, body.
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn json(body: &Value) -> Reply {
        Reply {
            status: 200,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: body.to_string(),
        }
    }
}

/// An HTTP/1.1 server on a loopback port. Every request is answered by the
/// closure and recorded; it stops when dropped.
struct Server {
    port: u16,
    requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
}

impl Server {
    fn start(handler: impl Fn(&Request, usize) -> Reply + Send + Sync + 'static) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        let port = listener.local_addr().expect("local addr").port();
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (stopped, seen) = (stop.clone(), requests.clone());
        thread::spawn(move || {
            for stream in listener.incoming() {
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(mut stream) = stream else { continue };
                let Some(request) = read_request(&mut stream) else {
                    continue;
                };
                // Handled in arrival order on this one thread: the index a
                // handler is given is the request's place in the exchange.
                let n = seen.lock().unwrap().len();
                let reply = handler(&request, n);
                seen.lock().unwrap().push(request);
                let mut head = format!(
                    "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                    reply.status,
                    reply.body.len()
                );
                for (k, v) in &reply.headers {
                    head.push_str(&format!("{k}: {v}\r\n"));
                }
                let _ = stream.write_all(format!("{head}\r\n{}", reply.body).as_bytes());
            }
        });
        Server {
            port,
            requests,
            stop,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// The body of every request received so far.
    fn bodies(&self) -> Vec<String> {
        let requests = self.requests.lock().unwrap();
        requests.iter().map(|r| r.body.clone()).collect()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so it sees the flag.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
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
    let length: usize = headers.get("content-length")?.parse().ok()?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    Some(Request {
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

/// A model API whose Nth chat request (0-based) is answered with the Nth of
/// `steps`, the last one repeating. Each step is one streamed delta.
fn model(steps: Vec<Value>) -> Server {
    Server::start(move |_, n| {
        let delta = &steps[n.min(steps.len() - 1)];
        let chunk = json!({ "choices": [{ "index": 0, "delta": delta }] });
        Reply {
            status: 200,
            headers: vec![("Content-Type".into(), "text/event-stream".into())],
            body: format!("data: {chunk}\n\ndata: [DONE]\n\n"),
        }
    })
}

/// A model step that says `text` and ends the turn.
fn say(text: &str) -> Value {
    json!({ "content": text })
}

/// An MCP server over Streamable HTTP with the given `initialize` result
/// and `tools/list` entries. A tool call answers `ran <name> <arguments>`.
fn mcp_server(initialize: Value, tools: Value) -> Server {
    Server::start(move |request, _| {
        let msg: Value = serde_json::from_str(&request.body).unwrap_or(Value::Null);
        let Some(id) = msg.get("id") else {
            return Reply {
                status: 202,
                headers: Vec::new(),
                body: String::new(),
            };
        };
        let result = match msg["method"].as_str().unwrap_or("") {
            "initialize" => initialize.clone(),
            "tools/list" => json!({ "tools": tools }),
            "tools/call" => {
                let text = format!(
                    "ran {} {}",
                    msg["params"]["name"].as_str().unwrap_or("?"),
                    msg["params"]["arguments"]
                );
                json!({ "content": [{ "type": "text", "text": text }] })
            }
            _ => {
                let error = json!({ "code": -32601, "message": "Method not found" });
                return Reply::json(&json!({ "jsonrpc": "2.0", "id": id, "error": error }));
            }
        };
        Reply::json(&json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    })
}

/// A fresh project directory holding `settings` as its `atelier.toml`.
fn project(name: &str, settings: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("atelier-mcp-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the project directory");
    std::fs::write(dir.join("atelier.toml"), settings).expect("write atelier.toml");
    dir
}

/// Run the line REPL in `cwd` with `input` as everything typed (approval
/// answers included: nothing is auto-approved), until end of input.
fn run_repl(model: &Server, cwd: &Path, input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_atelier"))
        .env("ATELIER_BASE_URL", model.url("/v1"))
        .env("ATELIER_MODEL", "mock")
        .env("ATELIER_NO_COORDINATOR", "1")
        // User-global files go next to the temp project, never the real home.
        .env("XDG_DATA_HOME", cwd.with_extension("data"))
        .env("XDG_CONFIG_HOME", cwd.with_extension("config"))
        .env("XDG_STATE_HOME", cwd.with_extension("state"))
        .env("HOME", cwd.with_extension("home"))
        .env_remove("ATELIER_APPROVE")
        .env_remove("ATELIER_BACKEND")
        .env_remove("ATELIER_API_KEY")
        .env_remove("ATELIER_TRACE")
        .env_remove("ATELIER_DEBUG")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the atelier binary");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(input.as_bytes())
        .expect("write the input");
    child.wait_with_output().expect("collect the output")
}

/// The system message and the advertised tool names of a chat request.
fn system_and_tools(body: &str) -> (String, Vec<String>) {
    let request: Value = serde_json::from_str(body).expect("the request body is JSON");
    let system = request["messages"][0]["content"]
        .as_str()
        .expect("a leading system message")
        .to_string();
    let tools = request["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| t["function"]["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    (system, tools)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A server's `instructions` stand in the system message of every request
/// under its name, and its tool filters decide which tools are advertised.
#[test]
fn server_instructions_and_tool_filters_reach_the_model() {
    let tracker = mcp_server(
        json!({ "capabilities": {}, "instructions": "Search before creating an issue." }),
        json!([
            { "name": "list_issues", "description": "List issues" },
            { "name": "delete_issue", "description": "Delete an issue" },
        ]),
    );
    let model = model(vec![say("first"), say("second")]);
    let dir = project(
        "instructions",
        &format!(
            "[[mcp_http]]\nname = \"tracker\"\nurl = \"{}\"\ndisabled_tools = [\"delete_*\"]\n\n\
             [[mcp_http]]\nname = \"off\"\nurl = \"http://127.0.0.1:9/never\"\nenabled = false\n",
            tracker.url("/mcp")
        ),
    );

    let out = run_repl(&model, &dir, "hello\nagain\n");
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("mcp: connected 'tracker' (http, 1 tool(s))"),
        "{stderr}"
    );
    assert!(stderr.contains("mcp: 'off' (http) is disabled"), "{stderr}");

    let bodies = model.bodies();
    assert_eq!(bodies.len(), 2, "{stderr}");
    // Standing context: on the second turn too, not only when it was new.
    for body in &bodies {
        let (system, tools) = system_and_tools(body);
        assert!(
            system.contains("### tracker\nSearch before creating an issue."),
            "{system}"
        );
        assert!(tools.contains(&"mcp__tracker__list_issues".to_string()));
        assert!(
            !tools.iter().any(|t| t.contains("delete_issue")),
            "{tools:?}"
        );
    }
}
