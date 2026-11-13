//! End-to-end tests of `atelier mcp`: the real binary as an MCP server,
//! driven over its stdin and stdout the way an MCP client drives it.
//!
//! No model, no network (but for the approval reviewer's case, which brings
//! a local stand-in for its model): every test starts the server in a fresh
//! project directory, sends JSON-RPC lines and reads the answers. Every line the
//! server writes to stdout must be a JSON-RPC message — [`Client::recv`]
//! fails the test on anything else, so each test also checks that nothing
//! but protocol goes there.

// The calls these tests make run shell commands (`echo`, `touch`).
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::SystemTime;

use serde_json::{Value, json};

/// A fresh project directory, holding `settings` as its `atelier.toml` when
/// there are any.
fn project(name: &str, settings: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "atelier-mcpserver-{name}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create the project directory");
    // The canonical path: what the server's own `current_dir` reports.
    let dir = dir.canonicalize().expect("canonical project path");
    if !settings.is_empty() {
        std::fs::write(dir.join("atelier.toml"), settings).expect("write atelier.toml");
    }
    dir
}

/// An MCP client talking to one `atelier mcp` process.
struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Client {
    /// Start `atelier mcp <args>` in `cwd` with extra environment `env`.
    fn start(cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Client {
        let mut child = Command::new(env!("CARGO_BIN_EXE_atelier"))
            .arg("mcp")
            .args(args)
            .env_remove("ATELIER_MODE")
            .env_remove("ATELIER_APPROVE")
            .env_remove("ATELIER_TRUST_PROJECT")
            .env_remove("ATELIER_TRUST_HOOKS")
            .env_remove("ATELIER_BACKEND")
            .env_remove("ATELIER_SANDBOX")
            .env("ATELIER_NO_COORDINATOR", "1")
            // User-global files go next to the temp project, never the real home.
            .env("XDG_DATA_HOME", cwd.with_extension("data"))
            .env("XDG_CONFIG_HOME", cwd.with_extension("config"))
            .env("XDG_STATE_HOME", cwd.with_extension("state"))
            .env("HOME", cwd.with_extension("home"))
            .envs(env.iter().copied())
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the atelier binary");
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        Client {
            child,
            stdin,
            stdout,
            next_id: 0,
        }
    }

    /// A server that has been through the handshake.
    fn connect(cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Client {
        let mut client = Client::start(cwd, args, env);
        let hello = client.request(
            "initialize",
            json!({ "protocolVersion": "2025-06-18", "capabilities": {},
                    "clientInfo": { "name": "test", "version": "0" } }),
        );
        assert_eq!(hello["result"]["serverInfo"]["name"], "atelier", "{hello}");
        client.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        client
    }

    fn send_line(&mut self, line: &str) {
        writeln!(self.stdin, "{line}").expect("write to the server");
        self.stdin.flush().expect("flush");
    }

    fn send(&mut self, msg: &Value) {
        self.send_line(&msg.to_string());
    }

    /// The next message from the server.
    fn recv(&mut self) -> Value {
        let mut line = String::new();
        let n = self
            .stdout
            .read_line(&mut line)
            .expect("read from the server");
        assert!(n > 0, "the server closed its output");
        serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("stdout is not JSON-RPC ({e}): {line:?}"))
    }

    /// Send a request and return its response.
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let reply = self.recv();
        assert_eq!(reply["id"], id, "{reply}");
        reply
    }

    /// Call a tool: its text, and whether the call succeeded.
    fn call(&mut self, name: &str, arguments: Value) -> (String, bool) {
        let reply = self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        );
        let result = &reply["result"];
        assert!(result.is_object(), "{reply}");
        let text = result["content"][0]["text"].as_str().expect("text content");
        (text.to_string(), result["isError"] == false)
    }

    /// Close the input, wait for the server to exit, and return what it
    /// wrote to stderr. It must exit cleanly, leaving nothing on stdout.
    fn finish(self) -> String {
        let Client {
            child,
            stdin,
            mut stdout,
            ..
        } = self;
        drop(stdin);
        let mut rest = String::new();
        std::io::Read::read_to_string(&mut stdout, &mut rest).expect("drain stdout");
        assert_eq!(rest, "", "unexpected output after the last answer");
        let out = child.wait_with_output().expect("wait for the server");
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }
}

/// The handshake: the server takes the client's protocol version, says who
/// it is and that it has tools; `ping` is answered, notifications are not,
/// and a method it does not have is an error, not silence.
#[test]
fn handshake_negotiates_the_version_and_names_the_server() {
    let dir = project("handshake", "");
    let mut client = Client::start(&dir, &[], &[]);
    let hello = client.request(
        "initialize",
        json!({ "protocolVersion": "2025-03-26", "capabilities": {} }),
    );
    assert_eq!(hello["jsonrpc"], "2.0");
    assert_eq!(hello["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(hello["result"]["serverInfo"]["name"], "atelier");
    assert_eq!(
        hello["result"]["serverInfo"]["version"],
        env!("CARGO_PKG_VERSION")
    );
    assert!(hello["result"]["capabilities"]["tools"].is_object());

    // No answer to a notification: the next line is the ping's.
    client.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    assert_eq!(client.request("ping", json!({}))["result"], json!({}));
    let unknown = client.request("resources/list", json!({}));
    assert_eq!(unknown["error"]["code"], -32601, "{unknown}");
    client.finish();
}

/// `tools/list`: the tools a session has, minus the ones that need a user
/// or a model, each with its schema and annotations.
#[test]
fn tools_are_listed_with_schemas_and_annotations() {
    let dir = project("list", "");
    let mut client = Client::connect(&dir, &[], &[]);
    let reply = client.request("tools/list", json!({}));
    let tools = reply["result"]["tools"].as_array().expect("tools");
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for expected in [
        "read",
        "write",
        "edit",
        "multiedit",
        "apply_patch",
        "grep",
        "glob",
        "ls",
        "tree",
        "bash",
        "bash_output",
        "bash_kill",
        "web_fetch",
        "web_search",
        "node",
        "notebook_edit",
        "todo",
        "memory",
        "skill",
    ] {
        assert!(names.contains(&expected), "{expected} missing: {names:?}");
    }
    for excluded in [
        "ask",
        "task",
        "peers",
        "peer_send",
        "mcp_search",
        "mcp_call",
    ] {
        assert!(!names.contains(&excluded), "{excluded} served: {names:?}");
    }
    let tool = |name: &str| tools.iter().find(|t| t["name"] == name).unwrap();
    let read = tool("read");
    assert_eq!(read["inputSchema"]["type"], "object");
    assert!(
        read["inputSchema"]["properties"]["path"].is_object(),
        "{read}"
    );
    assert!(!read["description"].as_str().unwrap().is_empty());
    assert_eq!(read["annotations"], json!({ "readOnlyHint": true }));
    assert_eq!(
        tool("edit")["annotations"],
        json!({ "readOnlyHint": false })
    );
    let destructive = json!({ "readOnlyHint": false, "destructiveHint": true });
    assert_eq!(tool("bash")["annotations"], destructive);
    assert_eq!(tool("write")["annotations"], destructive);
    // The relay's hint to claude is not this server's to give.
    assert!(read.get("_meta").is_none(), "{read}");
    client.finish();
}

/// Reading a file of the project; and the project is where it ends — a
/// path outside the root is not read (that needs an approval nobody is here
/// to give) and not written (never, approval or not).
#[test]
fn read_works_inside_the_project_and_not_outside() {
    let dir = project("read", "");
    std::fs::write(dir.join("hello.txt"), "bonjour\n").unwrap();
    let outside = dir.with_extension("outside");
    std::fs::write(&outside, "SECRET\n").unwrap();
    let mut client = Client::connect(&dir, &[], &[]);

    let (text, ok) = client.call("read", json!({ "path": "hello.txt" }));
    assert!(ok, "{text}");
    assert!(text.contains("bonjour"), "{text}");

    for path in [outside.to_str().unwrap(), "../", "/etc/hostname"] {
        let (text, ok) = client.call("read", json!({ "path": path }));
        assert!(!ok, "{path} was read: {text}");
        assert!(!text.contains("SECRET"), "{text}");
    }
    let escape = format!("../{}", outside.file_name().unwrap().to_str().unwrap());
    let (text, ok) = client.call("write", json!({ "path": escape, "content": "x" }));
    assert!(!ok, "{text}");
    assert!(text.contains("escapes the project root"), "{text}");
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "SECRET\n");
    client.finish();
}

/// Read tracking lasts as long as the server: an `edit` of a file that
/// changed on disk since the server last saw it is refused, and goes
/// through once it has been read again.
#[test]
fn a_stale_edit_is_rejected_until_the_file_is_read_again() {
    let dir = project("stale", "");
    let mut client = Client::connect(&dir, &[], &[]);
    let (text, ok) = client.call("write", json!({ "path": "notes.txt", "content": "one\n" }));
    assert!(ok, "{text}");
    assert_eq!(
        std::fs::read_to_string(dir.join("notes.txt")).unwrap(),
        "one\n"
    );

    // Someone else edits the file behind the server's back.
    std::fs::write(dir.join("notes.txt"), "one\nand more\n").unwrap();
    let edit = json!({ "path": "notes.txt", "old_string": "one", "new_string": "two" });
    let (text, ok) = client.call("edit", edit.clone());
    assert!(!ok, "a stale edit went through: {text}");
    assert_eq!(
        std::fs::read_to_string(dir.join("notes.txt")).unwrap(),
        "one\nand more\n"
    );

    let (_, ok) = client.call("read", json!({ "path": "notes.txt" }));
    assert!(ok);
    let (text, ok) = client.call("edit", edit);
    assert!(ok, "{text}");
    assert_eq!(
        std::fs::read_to_string(dir.join("notes.txt")).unwrap(),
        "two\nand more\n"
    );
    client.finish();
}

/// `bash` asks, and nobody is there: without a rule the call is refused
/// with the rule that would allow it; with that rule — in a project whose
/// rules count — it runs. An untrusted project's rule does not count, and
/// the refusal says so.
#[test]
fn bash_is_denied_without_a_rule_and_allowed_with_one() {
    let command = json!({ "command": "echo ran > proof.txt" });
    let listing = json!({ "command": "echo hello from bash" });

    let dir = project("bash-none", "");
    let mut client = Client::connect(&dir, &[], &[]);
    let (text, ok) = client.call("bash", listing.clone());
    assert!(!ok, "{text}");
    assert!(text.contains("bash needs approval"), "{text}");
    assert!(text.contains("nobody to ask"), "{text}");
    assert!(
        text.contains("[permissions]\nallow = [\"bash(echo *)\"]"),
        "{text}"
    );
    client.finish();

    let rules = "[permissions]\nallow = [\"bash(echo *)\"]\n";
    let dir = project("bash-rule", rules);
    let mut client = Client::connect(&dir, &[], &[("ATELIER_TRUST_PROJECT", "1")]);
    let (text, ok) = client.call("bash", listing.clone());
    assert!(ok, "{text}");
    assert!(text.contains("hello from bash"), "{text}");
    // The rule covers what it names, no more: a redirection is not `echo *`.
    let (text, ok) = client.call("bash", command);
    assert!(!ok, "{text}");
    assert!(!dir.join("proof.txt").exists());
    client.finish();

    let dir = project("bash-untrusted", rules);
    let mut client = Client::connect(&dir, &[], &[]);
    let (text, ok) = client.call("bash", listing);
    assert!(!ok, "{text}");
    assert!(text.contains("is not trusted"), "{text}");
    let stderr = client.finish();
    assert!(stderr.contains("not trusted"), "{stderr}");
}

/// A one-thread stand-in for a chat-completions endpoint that answers every
/// request with `reply` as the streamed text; returns its base URL and the
/// count of requests it has served.
fn model_answering(reply: String) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::io::Read;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let served = std::sync::Arc::new(AtomicUsize::new(0));
    let count = std::sync::Arc::clone(&served);
    // Ends with the test process; the server under test is its only client.
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut length = 0;
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|n| n > 0) && line.trim() != "" {
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
                line.clear();
            }
            let mut body = vec![0; length];
            let _ = reader.read_exact(&mut body);
            count.fetch_add(1, Ordering::SeqCst);
            let chunk = json!({"choices": [{"index": 0, "delta": {"content": reply}}]});
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                 Connection: close\r\n\r\ndata: {chunk}\n\ndata: [DONE]\n\n"
            );
        }
    });
    (url, served)
}

/// With the approval reviewer on, a call that would be refused for want of
/// anyone to ask (no elicitation) is judged by its model instead: allowed
/// and run, or refused with the reviewer's reason — and a reply that is no
/// verdict leaves the usual "nobody to ask" refusal in place.
#[test]
fn the_approval_reviewer_answers_where_the_client_cannot_ask() {
    use std::sync::atomic::Ordering;
    let reviewer = "[permissions.reviewer]\nenabled = true\n";
    let command = json!({ "command": "echo ran > proof.txt" });
    let verdict = |decision: &str, reason: &str| {
        json!({"decision": decision, "risk": "low", "reason": reason}).to_string()
    };
    let serve = |name: &str, reply: String| {
        let (url, served) = model_answering(reply);
        let dir = project(name, reviewer);
        let env = [
            ("ATELIER_TRUST_PROJECT", "1"),
            ("ATELIER_BASE_URL", url.as_str()),
            ("ATELIER_MODEL", "judge"),
        ];
        (Client::connect(&dir, &[], &env), dir, served)
    };

    let (mut client, dir, served) = serve("reviewer-allow", verdict("allow", "writes one file"));
    let (text, ok) = client.call("bash", command.clone());
    assert!(ok, "{text}");
    assert_eq!(
        std::fs::read_to_string(dir.join("proof.txt")).unwrap(),
        "ran\n"
    );
    // What the rules decide alone never reaches it.
    let (_, ok) = client.call("write", json!({"path": "a.txt", "content": "x"}));
    assert!(ok);
    assert_eq!(served.load(Ordering::SeqCst), 1);
    let stderr = client.finish();
    assert!(
        stderr.contains("⚖ reviewer allowed bash (low): writes one file"),
        "{stderr}"
    );

    let (mut client, dir, _) = serve("reviewer-deny", verdict("deny", "not asked for"));
    let (text, ok) = client.call("bash", command.clone());
    assert!(!ok, "{text}");
    assert!(
        text.contains("the approval reviewer denied this call"),
        "{text}"
    );
    assert!(text.contains("not asked for"), "{text}");
    assert!(!dir.join("proof.txt").exists());
    client.finish();

    let (mut client, dir, served) = serve("reviewer-garbage", "Looks fine to me.".to_string());
    let (text, ok) = client.call("bash", command);
    assert!(!ok, "{text}");
    assert!(text.contains("nobody to ask"), "{text}");
    assert!(!dir.join("proof.txt").exists());
    assert_eq!(served.load(Ordering::SeqCst), 1);
    client.finish();

    // Untrusted, the project's table switches nothing on: no request.
    let (url, served) = model_answering(verdict("allow", "anything"));
    let dir = project("reviewer-untrusted", reviewer);
    let env = [("ATELIER_BASE_URL", url.as_str())];
    let mut client = Client::connect(&dir, &[], &env);
    let (text, ok) = client.call("bash", json!({ "command": "echo ran > proof.txt" }));
    assert!(!ok, "{text}");
    assert_eq!(served.load(Ordering::SeqCst), 0);
    client.finish();
}

/// `--allow-all` is `yolo`: what would ask runs.
#[test]
fn allow_all_runs_what_would_ask() {
    let dir = project("allow-all", "");
    let mut client = Client::connect(&dir, &["--allow-all"], &[]);
    let (text, ok) = client.call("bash", json!({ "command": "echo unchecked" }));
    assert!(ok, "{text}");
    assert!(text.contains("unchecked"), "{text}");
    client.finish();
}

/// Only `--allow-all`/`--mode` and `ATELIER_MODE` set the mode: neither a
/// session's `ATELIER_APPROVE=all` nor the project's `[defaults] mode`
/// makes a server run everything.
#[test]
fn a_sessions_yolo_settings_do_not_reach_the_server() {
    let dir = project("no-yolo", "[defaults]\nmode = \"yolo\"\n");
    let env = [("ATELIER_APPROVE", "all"), ("ATELIER_TRUST_PROJECT", "1")];
    let mut client = Client::connect(&dir, &[], &env);
    let (text, ok) = client.call("bash", json!({ "command": "echo unchecked" }));
    assert!(!ok, "{text}");
    assert!(text.contains("bash needs approval"), "{text}");
    client.finish();
}

/// A deny rule refuses even under `--allow-all`, and names itself.
#[test]
fn a_deny_rule_holds_under_allow_all() {
    let dir = project("deny", "[permissions]\ndeny = [\"bash(echo *)\"]\n");
    let mut client = Client::connect(&dir, &["--allow-all"], &[]);
    let (text, ok) = client.call("bash", json!({ "command": "echo no" }));
    assert!(!ok, "{text}");
    assert!(
        text.contains("refused by the permission rule `bash(echo *)`"),
        "{text}"
    );
    client.finish();
}

/// `--tools` and `--no-tools` narrow what is listed and what can be called;
/// a name that is no tool is a usage error.
#[test]
fn tools_flags_narrow_the_server() {
    let dir = project("narrow", "");
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    let mut client = Client::connect(
        &dir,
        &["--tools", "read,grep,write", "--no-tools=write"],
        &[],
    );
    let reply = client.request("tools/list", json!({}));
    let tools = reply["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["read", "grep"]);
    let (_, ok) = client.call("read", json!({ "path": "a.txt" }));
    assert!(ok);
    for (name, args) in [
        ("write", json!({ "path": "b.txt", "content": "b" })),
        ("ls", json!({})),
        ("ask", json!({ "question": "?", "options": ["a", "b"] })),
    ] {
        let (text, ok) = client.call(name, args);
        assert!(!ok, "{name}: {text}");
        assert_eq!(text, format!("error: unknown tool '{name}'"));
    }
    assert!(!dir.join("b.txt").exists());
    client.finish();

    let out = Command::new(env!("CARGO_BIN_EXE_atelier"))
        .args(["mcp", "--tools", "read,nope"])
        .env("ATELIER_NO_COORDINATOR", "1")
        .env("HOME", dir.with_extension("home"))
        .current_dir(&dir)
        .stdin(Stdio::null())
        .output()
        .expect("run atelier");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("'nope' is not a tool"), "{stderr}");
}

/// Plan mode — by flag or by `ATELIER_MODE` — refuses the tools that
/// change things and runs the ones that read.
#[test]
fn plan_mode_refuses_write() {
    for (args, env) in [
        (&["--mode", "plan"][..], &[][..]),
        (&[][..], &[("ATELIER_MODE", "plan")][..]),
    ] {
        let dir = project("plan", "");
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        let mut client = Client::connect(&dir, args, env);
        let (text, ok) = client.call("write", json!({ "path": "b.txt", "content": "b" }));
        assert!(!ok, "{text}");
        assert!(text.contains("plan mode: write is not available"), "{text}");
        assert!(!dir.join("b.txt").exists());
        let (text, ok) = client.call("read", json!({ "path": "a.txt" }));
        assert!(ok, "{text}");
        client.finish();
    }

    // A mode that is none stops the server before it serves anything.
    let dir = project("plan-typo", "");
    let out = Command::new(env!("CARGO_BIN_EXE_atelier"))
        .args(["mcp", "--mode", "plna"])
        .env("ATELIER_NO_COORDINATOR", "1")
        .env("HOME", dir.with_extension("home"))
        .current_dir(&dir)
        .stdin(Stdio::null())
        .output()
        .expect("run atelier");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
}

/// A line that is not JSON gets a parse error (id null) and the server goes
/// on; so does JSON that is not a message; a call without a tool name is an
/// invalid-params error.
#[test]
fn malformed_input_is_answered_with_errors_and_survived() {
    let dir = project("malformed", "");
    let mut client = Client::connect(&dir, &[], &[]);
    client.send_line("{this is not json");
    let reply = client.recv();
    assert_eq!(reply["error"]["code"], -32700, "{reply}");
    assert_eq!(reply["id"], Value::Null);

    client.send_line("[1, 2, 3]");
    assert_eq!(client.recv()["error"]["code"], -32600);
    // Blank lines are nothing.
    client.send_line("");

    let reply = client.request("tools/call", json!({ "arguments": {} }));
    assert_eq!(reply["error"]["code"], -32602, "{reply}");
    // Arguments of the wrong shape are the tool's to refuse, as a result.
    let (text, ok) = client.call("read", json!({ "path": 42 }));
    assert!(!ok, "{text}");
    assert_eq!(client.request("ping", json!({}))["result"], json!({}));
    client.finish();
}

/// With a client that can elicit, the question goes to its user over the
/// same connection, mid-call, and the answer decides.
#[test]
fn a_client_with_elicitation_is_asked() {
    let dir = project("elicit", "");
    let mut client = Client::start(&dir, &[], &[]);
    client.request(
        "initialize",
        json!({ "protocolVersion": "2025-06-18", "capabilities": { "elicitation": {} } }),
    );
    for (decision, runs) in [("yes", true), ("no", false)] {
        let file = format!("{decision}.txt");
        client.send(
            &json!({ "jsonrpc": "2.0", "id": 100, "method": "tools/call",
            "params": { "name": "bash", "arguments": { "command": format!("touch {file}") } } }),
        );
        let ask = client.recv();
        assert_eq!(ask["method"], "elicitation/create", "{ask}");
        let message = ask["params"]["message"].as_str().unwrap();
        assert!(
            message.contains("`bash`") && message.contains(&file),
            "{message}"
        );
        client.send(&json!({ "jsonrpc": "2.0", "id": ask["id"],
            "result": { "action": "accept", "content": { "decision": decision } } }));
        let reply = client.recv();
        assert_eq!(reply["id"], 100, "{reply}");
        assert_eq!(reply["result"]["isError"], !runs, "{reply}");
        assert_eq!(dir.join(&file).exists(), runs);
    }
    client.finish();
}

/// `pre_tool` and `post_tool` hooks of a trusted project apply to the
/// calls an MCP client makes; `session_start` does not run.
#[test]
fn tool_hooks_apply_and_session_hooks_do_not() {
    let hooks = r#"
[[hooks]]
event = "session_start"
command = "touch started.txt"

[[hooks]]
event = "pre_tool"
match = "write"
command = "echo 'no writes here' >&2; exit 2"

[[hooks]]
event = "post_tool"
match = "read"
command = 'echo "$ATELIER_TOOL" >> seen.log'
"#;
    let dir = project("hooks", hooks);
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    let mut client = Client::connect(&dir, &[], &[("ATELIER_TRUST_PROJECT", "1")]);
    let (text, ok) = client.call("write", json!({ "path": "b.txt", "content": "b" }));
    assert!(!ok, "{text}");
    assert!(
        text.contains("blocked by a pre_tool hook: no writes here"),
        "{text}"
    );
    let (_, ok) = client.call("read", json!({ "path": "a.txt" }));
    assert!(ok);
    client.finish();
    assert!(!dir.join("b.txt").exists());
    assert_eq!(
        std::fs::read_to_string(dir.join("seen.log")).unwrap(),
        "read\n"
    );
    assert!(!dir.join("started.txt").exists());
}
