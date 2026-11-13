//! atelier as an MCP *server*: its tools, for another program's agent.
//!
//! Two things live here.
//!
//! **The protocol** ([`answer`]): what an MCP server says to `initialize`,
//! `ping`, `tools/list` and `tools/call`. It is shared with the relay the
//! Claude backend uses (`claude::host`, `atelier mcp-serve <socket>`), which
//! differs only in which tools it lists and who runs a call.
//!
//! **`atelier mcp`** ([`run_command`]): a server over stdio (one JSON-RPC
//! message per line) that any MCP client — another harness, an editor — can
//! launch in a project directory to get atelier's tools there: the file
//! tools with their read tracking, `bash`, `node`, the web tools, and so on.
//!
//! # A session without a model
//!
//! The server holds an ordinary [`Session`] and never sends it a prompt. A
//! `tools/call` goes through [`Session::call_tool`], which is the path a
//! call from the model takes — `pre_tool` hooks, permission rules and the
//! mode, the tool confined to the project root, `post_tool` hooks — so
//! nothing about what a tool may do is decided here a second time. The
//! session lives as long as the process: what one call read is known to the
//! `edit` that follows. Nothing connects the configured MCP servers, no
//! coordinator is joined, and the `session_start`/`session_end` hooks do not
//! run: this is not a conversation.
//!
//! Left out: `ask` (a question for a user this server has no interface to),
//! `task` (subagents need a model), the coordinator's `peers`/`peer_send`
//! and the MCP client's own tools.
//!
//! # Nobody to ask
//!
//! A terminal session answers "may this run?" at a prompt. Here there is no
//! prompt, so a call the rules and the mode would *ask* about is one of two
//! things:
//!
//! - **refused**, with a result that says so and names the rule that would
//!   let it through (`[permissions] allow = ["bash(cargo test *)"]`) — the
//!   client's model can relay that, and the person can decide;
//! - **put to the client's user**, when the client declared the
//!   `elicitation` capability in `initialize`: the server sends
//!   `elicitation/create` with a yes / always / no choice and honours the
//!   answer ("always" saves the narrow rule, as at the prompt).
//!
//! The mode is `auto-edit` unless `--mode` or `ATELIER_MODE` says otherwise
//! — not the project's `[defaults] mode`, and not `ATELIER_APPROVE`: a
//! server someone else's agent drives does not get `yolo` from a setting
//! meant for a session a person watches. `--allow-all` is the explicit way
//! to ask for it.
//!
//! # One thing at a time
//!
//! Messages are answered in the order they arrive, on one thread. The one
//! exception is elicitation, where the server needs an answer from the
//! client while a `tools/call` is open: [`Conn::elicit`] then reads on,
//! answers a `ping` at once, sets other requests aside to be handled when
//! the call is done, and takes the response that carries its id. There is
//! no cancellation: a call runs to its end (the tools have their own
//! timeouts).

use std::collections::VecDeque;
use std::io::{self, BufRead, Write};

use anyhow::Result;
use serde_json::{Value, json};

use crate::agent::{Approval, Session, Ui};
use crate::config::Config;
use crate::permissions::{DENIED, Mode};
use crate::settings::Settings;
use crate::tools::{self, Tool, ToolOutput};
use crate::trust::{self, Trust};

/// The server's name in `initialize`; a client shows the tools under it.
pub const SERVER_NAME: &str = "atelier";
/// Advertised when the client doesn't name an MCP protocol version.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Answer one MCP JSON-RPC message; `None` for notifications. `tools` gives
/// the entries of `tools/list`; tool calls go through
/// `call(name, arguments) -> (output, ok)`.
pub fn answer(
    msg: &Value,
    tools: impl FnOnce() -> Vec<Value>,
    call: impl FnOnce(&str, Value) -> (ToolOutput, bool),
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
        "tools/list" => json!({ "tools": tools() }),
        "tools/call" => {
            let Some(name) = params["name"].as_str() else {
                return Some(error(id, -32602, "tools/call: missing tool name"));
            };
            let arguments = match &params["arguments"] {
                Value::Null => json!({}),
                a => a.clone(),
            };
            let (out, ok) = call(name, arguments);
            let mut content = vec![json!({ "type": "text", "text": out.text })];
            content.extend(
                out.images
                    .iter()
                    .map(|i| json!({ "type": "image", "data": i.data, "mimeType": i.mime })),
            );
            json!({ "content": content, "isError": !ok })
        }
        other => return Some(error(id, -32601, &format!("method not found: {other}"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// A JSON-RPC error response.
pub fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

const USAGE: &str = "\
usage: atelier mcp [--mode MODE] [--allow-all] [--tools a,b,…] [--no-tools x,y,…]

Serve atelier's tools over MCP on stdin/stdout, for another agent to use in
this directory. Launched by an MCP client:

    {\"command\": \"atelier\", \"args\": [\"mcp\"]}

  --mode MODE      plan | ask-edits | auto-edit (default) | yolo; also ATELIER_MODE
  --allow-all      the same as --mode yolo: run everything without asking.
                   Dangerous: the client's agent gets a shell
  --tools LIST     serve only these tools (comma-separated)
  --no-tools LIST  serve all tools but these

There is nobody to ask: a call that would need approval is refused, saying
which [permissions] rule would allow it — unless the client supports MCP
elicitation, in which case its user is asked. See docs/mcp.md.";

/// The tools of a session `atelier mcp` never serves: `ask` has no user to
/// put its question to.
const NEVER: &[&str] = &[tools::ask::NAME];

/// What the command line asked for.
#[derive(Debug, Default, PartialEq)]
struct Options {
    help: bool,
    mode: Option<Mode>,
    /// `--tools`: the only tools to serve.
    only: Option<Vec<String>>,
    /// `--no-tools`: tools not to serve.
    without: Vec<String>,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut opts = Self::default();
        let list = |text: &str| -> Vec<String> {
            let names = text.split(',').map(str::trim).filter(|n| !n.is_empty());
            names.map(str::to_string).collect()
        };
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let (flag, inline) = match arg.split_once('=') {
                Some((flag, value)) => (flag, Some(value.to_string())),
                None => (arg.as_str(), None),
            };
            let mut value = || {
                inline
                    .clone()
                    .or_else(|| args.next().cloned())
                    .ok_or_else(|| format!("{flag} needs a value"))
            };
            match flag {
                "-h" | "--help" => opts.help = true,
                "--allow-all" => opts.mode = Some(Mode::Yolo),
                "--mode" => opts.mode = Some(Mode::parse(&value()?)?),
                "--tools" => opts.only = Some(list(&value()?)),
                "--no-tools" => opts.without.extend(list(&value()?)),
                other => return Err(format!("unknown argument '{other}'")),
            }
        }
        Ok(opts)
    }

    /// The tools to serve, out of `all`. A name that is not a tool is an
    /// error: a typo in `--no-tools` must not leave the tool served.
    fn select(&self, all: tools::ToolRegistry) -> Result<tools::ToolRegistry, String> {
        let offered = all.filtered(|name| !NEVER.contains(&name));
        let named = self.only.iter().flatten().chain(&self.without);
        if let Some(unknown) = named.clone().find(|name| offered.get(name).is_none()) {
            let names: Vec<String> = offered.all().map(|t| t.name().to_string()).collect();
            return Err(format!(
                "'{unknown}' is not a tool `atelier mcp` serves (it serves: {})",
                names.join(", ")
            ));
        }
        Ok(offered.filtered(|name| {
            self.only
                .as_ref()
                .is_none_or(|only| only.iter().any(|n| n == name))
                && !self.without.iter().any(|n| n == name)
        }))
    }
}

/// `atelier mcp …`: serve the tools on stdin/stdout until the client closes
/// the stream. Returns the exit status.
pub fn run_command(args: &[String]) -> Result<u8> {
    let usage = |msg: String| {
        eprintln!("atelier: {msg}\n\n{USAGE}");
        Ok(crate::headless::EXIT_USAGE)
    };
    let opts = match Options::parse(args) {
        Ok(opts) => opts,
        Err(msg) => return usage(msg),
    };
    if opts.help {
        println!("{USAGE}");
        return Ok(0);
    }
    // `--mode`, else `ATELIER_MODE` (a value naming no mode stops the run,
    // as everywhere), else `auto-edit` whatever the settings say.
    let mode = match opts.mode.map_or_else(Mode::from_env, |m| Ok(Some(m))) {
        Ok(mode) => mode.unwrap_or(Mode::AutoEdit),
        Err(msg) => return usage(msg),
    };
    let tools = match opts.select(tools::builtin_registry()) {
        Ok(tools) => tools,
        Err(msg) => return usage(msg),
    };
    let root = std::env::current_dir()?;
    let settings = Settings::load(&root).unwrap_or_else(|e| {
        eprintln!("warning: {e:#}; using empty settings");
        Settings::default()
    });
    // As for a headless run: an untrusted project's allow rules and hooks
    // are left out unless the environment accepts them, and this says so.
    let trust = Trust::load(&root, &settings, trust::Store::from_env());
    for line in trust.startup(&settings, false) {
        eprintln!("{line}");
    }
    // No model is called; the session wants a configuration all the same.
    let cfg = Config::from_env(false);
    let mut session = Session::new(cfg, root, tools, Vec::new(), settings, trust);
    session.set_mode(mode);

    let served = serve(&mut session, io::stdin().lock(), io::stdout().lock());
    // Background `bash` jobs would outlive the server.
    tools::bash::kill_all();
    served.map(|()| 0)
}

/// Answer the messages on `input` until it ends, writing to `output`.
fn serve(session: &mut Session, input: impl BufRead, output: impl Write) -> Result<()> {
    let mut conn = Conn {
        input,
        output,
        elicits: false,
        asked: 0,
        waiting: VecDeque::new(),
    };
    loop {
        // What arrived while a call waited for its user comes first.
        let msg = match conn.waiting.pop_front() {
            Some(msg) => msg,
            None => match conn.read()? {
                Some(msg) => msg,
                None => return Ok(()),
            },
        };
        if msg["method"] == "initialize" {
            conn.elicits = msg["params"]["capabilities"]["elicitation"].is_object();
        }
        // Taken now: the call below needs the session to itself.
        let listing = match msg["method"].as_str() {
            Some("tools/list") => tool_listing(session.tool_registry()),
            _ => Vec::new(),
        };
        let reply = answer(
            &msg,
            || listing,
            |name, arguments| call(session, &mut conn, name, arguments),
        );
        if let Some(reply) = reply {
            conn.write(&reply)?;
        }
    }
}

/// The `tools/list` entries for `registry`: each tool's spec, and what its
/// [`Tool::mutates`] and name say about it as MCP annotations.
fn tool_listing(registry: &tools::ToolRegistry) -> Vec<Value> {
    registry
        .all()
        .map(|tool| {
            let spec = tool.spec();
            json!({
                "name": spec.name,
                "description": spec.description,
                "inputSchema": spec.parameters,
                "annotations": annotations(tool),
            })
        })
        .collect()
}

/// What a client may assume about `tool`. `readOnlyHint` is the tool's own
/// word on a call with no arguments, which is the cautious reading for the
/// tools whose answer depends on them (`memory` reads or saves: not
/// read-only). `destructiveHint` marks the two that can lose work outright:
/// a shell, and a tool that replaces a file's contents.
fn annotations(tool: &dyn Tool) -> Value {
    let read_only = !tool.mutates(&json!({}));
    let mut hints = json!({ "readOnlyHint": read_only });
    if !read_only && matches!(tool.name(), "bash" | "write") {
        hints["destructiveHint"] = json!(true);
    }
    hints
}

/// Run one `tools/call` against the session.
fn call<R: BufRead, W: Write>(
    session: &mut Session,
    conn: &mut Conn<R, W>,
    name: &str,
    arguments: Value,
) -> (ToolOutput, bool) {
    // Only what is listed: the session answers `ask` by name whether or not
    // it has the tool, and a narrowed server serves what it was narrowed to.
    if session.tool_registry().get(name).is_none() {
        let msg = format!("error: unknown tool '{name}'");
        return (ToolOutput::text(msg), false);
    }
    let mut ui = Asker {
        conn,
        notes: Vec::new(),
        unasked: None,
    };
    let (mut out, ok) = session.call_tool(name, arguments, &mut ui);
    // Refused because nobody could be asked — not because somebody said
    // no: say what would let it run instead of "the user denied".
    if let Some((tool, arguments)) = ui.unasked.take()
        && !ok
        && out.text == DENIED
    {
        out.text = nobody_to_ask(session, &tool, &arguments);
    }
    (out, ok)
}

/// The result of a call that needed an approval nobody could give: why,
/// and the rule that would allow it.
fn nobody_to_ask(session: &Session, tool: &str, arguments: &str) -> String {
    let mut msg = format!(
        "error: {tool} needs approval, and `atelier mcp` has nobody to ask (this MCP client \
         does not support elicitation). The call was not run."
    );
    let why = session.why_asked(tool, arguments);
    if let Some(rule) = why.ask_rule {
        msg.push_str(&format!(
            " The permission rule `{rule}` asks before every such call; it can only run \
             once that rule is removed."
        ));
        return msg;
    }
    let rules = match why.proposed.as_slice() {
        // No narrow rule fits (an open-ended launcher, a command that
        // cannot be followed): the form of one, for the person to fill in.
        [] if tool == "bash" => "\"bash(<command> *)\"".to_string(),
        [] => format!("\"{tool}\""),
        rules => {
            let quoted: Vec<String> = rules.iter().map(|r| json!(r).to_string()).collect();
            quoted.join(", ")
        }
    };
    msg.push_str(&format!(
        " To allow it, the person running this server can add a rule to atelier.toml:\n\n\
         [permissions]\nallow = [{rules}]\n"
    ));
    if !why.project_rules_count {
        msg.push_str(
            "\nThis project's atelier.toml is not trusted, so its allow rules do not count \
             yet: run `atelier trust accept` in the project (or start the server with \
             ATELIER_TRUST_PROJECT=1), or put the rule in the user's own atelier.toml.\n",
        );
    }
    msg
}

/// The connection to the client: lines in, lines out.
struct Conn<R, W> {
    input: R,
    output: W,
    /// The client said, in `initialize`, that it can put a question to its
    /// user (`elicitation`).
    elicits: bool,
    /// How many questions were sent: the next one's id.
    asked: u64,
    /// Requests that arrived while a question was open, in order.
    waiting: VecDeque<Value>,
}

impl<R: BufRead, W: Write> Conn<R, W> {
    /// The next message, or `None` at the end of the input. A line that is
    /// not a JSON-RPC message is answered with an error and skipped.
    fn read(&mut self) -> Result<Option<Value>> {
        loop {
            let mut line = String::new();
            if self.input.read_line(&mut line)? == 0 {
                return Ok(None);
            }
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(&line) {
                Ok(msg) if msg.is_object() => return Ok(Some(msg)),
                // Batches left the protocol in 2025-06-18, the version
                // this server speaks.
                Ok(_) => self.write(&error(
                    Value::Null,
                    -32600,
                    "invalid request: expected one JSON-RPC message (an object) per line",
                ))?,
                Err(e) => self.write(&error(Value::Null, -32700, &format!("parse error: {e}")))?,
            }
        }
    }

    fn write(&mut self, msg: &Value) -> Result<()> {
        writeln!(self.output, "{msg}")?;
        self.output.flush()?;
        Ok(())
    }

    /// Ask the client's user whether `tool` may run, and wait for the
    /// answer. Anything but an accepted "yes" or "always" — a decline, an
    /// error, a client that goes away — is a no.
    fn elicit(&mut self, tool: &str, arguments: &str, notes: &[String]) -> Approval {
        self.asked += 1;
        let id = format!("atelier-approval-{}", self.asked);
        let mut message = format!("atelier: allow the tool `{tool}` to run?\n\n{arguments}");
        for note in notes {
            message.push_str(&format!("\n{note}"));
        }
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "elicitation/create",
            "params": {
                "message": message,
                "requestedSchema": {
                    "type": "object",
                    "properties": {
                        "decision": {
                            "type": "string",
                            "title": "Run it?",
                            "enum": ["yes", "always", "no"],
                            "enumNames": [
                                "Yes, this once",
                                "Always (save a permission rule)",
                                "No",
                            ],
                        },
                    },
                    "required": ["decision"],
                },
            },
        });
        if self.write(&request).is_err() {
            return Approval::Deny;
        }
        loop {
            let Ok(Some(msg)) = self.read() else {
                return Approval::Deny;
            };
            if msg.get("method").is_some() {
                // The client goes on talking while its user thinks. A
                // `ping` is answered now (it may be how the client checks
                // the server is alive); other requests wait for this call
                // to end; notifications ask for nothing.
                if msg["method"] == "ping" {
                    if let Some(pong) = answer(&msg, Vec::new, |_, _| unreachable!("a ping")) {
                        let _ = self.write(&pong);
                    }
                } else if msg.get("id").is_some() {
                    self.waiting.push_back(msg);
                }
                continue;
            }
            if msg["id"] != id.as_str() {
                continue; // an answer to a question that is no longer open
            }
            let result = &msg["result"];
            let decision = result["content"]["decision"].as_str();
            return match (result["action"].as_str(), decision) {
                (Some("accept"), Some("yes")) => Approval::Once,
                (Some("accept"), Some("always")) => Approval::Always,
                _ => Approval::Deny,
            };
        }
    }
}

/// The session's [`Ui`] for one call: the approval prompt goes to the
/// client's user when it has a way to ask them, and is refused otherwise;
/// what a terminal would print above the prompt is kept to go with it.
struct Asker<'a, R, W> {
    conn: &'a mut Conn<R, W>,
    /// What the session said about the call it is about to ask for (the
    /// rule that asks, risks it sees).
    notes: Vec<String>,
    /// The call that was refused for want of anyone to ask: its tool and
    /// arguments.
    unasked: Option<(String, String)>,
}

impl<R: BufRead, W: Write> Ui for Asker<'_, R, W> {
    fn reasoning(&mut self, _text: &str) {}
    fn content(&mut self, _text: &str) {}
    fn tool_start(&mut self, _name: &str, _arguments: &str) {}
    fn tool_end(&mut self, _name: &str, _result: &str, _ok: bool) {}
    fn info(&mut self, text: &str) {
        self.notes.push(text.to_string());
    }
    // A script's inner calls (`node` with `tools: true`): nowhere to show.
    fn activity(&mut self, _text: &str) {}
    fn ask_approval(&mut self, tool: &str, arguments: &str) -> Approval {
        let notes = std::mem::take(&mut self.notes);
        if self.conn.elicits {
            return self.conn.elicit(tool, arguments, &notes);
        }
        self.unasked = Some((tool.to_string(), arguments.to_string()));
        Approval::Deny
    }
    fn notice(&mut self, text: &str) {
        // Hooks' and the rules' troubles: for whoever runs the server.
        eprintln!("{text}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn opts(args: &[&str]) -> Result<Options, String> {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        Options::parse(&args)
    }

    #[test]
    fn flags_parse_in_both_spellings() {
        let o = opts(&[
            "--mode",
            "plan",
            "--tools=read,grep",
            "--no-tools",
            "bash, node",
        ])
        .unwrap();
        assert_eq!(o.mode, Some(Mode::Plan));
        assert_eq!(o.only, Some(vec!["read".to_string(), "grep".to_string()]));
        assert_eq!(o.without, ["bash", "node"]);
        assert_eq!(opts(&["--allow-all"]).unwrap().mode, Some(Mode::Yolo));
        assert!(opts(&["--help"]).unwrap().help);
        assert!(opts(&["--mode"]).unwrap_err().contains("needs a value"));
        assert!(opts(&["--mode", "plna"]).is_err());
        assert!(
            opts(&["--frobnicate"])
                .unwrap_err()
                .contains("unknown argument")
        );
    }

    fn names(registry: &tools::ToolRegistry) -> Vec<String> {
        registry.all().map(|t| t.name().to_string()).collect()
    }

    #[test]
    fn tool_selection_narrows_and_rejects_unknown_names() {
        let all = names(
            &Options::default()
                .select(tools::builtin_registry())
                .unwrap(),
        );
        assert!(all.contains(&"bash".to_string()) && all.contains(&"node".to_string()));
        assert!(!all.contains(&"ask".to_string()), "{all:?}");

        let only = opts(&["--tools", "read,write,bash", "--no-tools", "bash"]).unwrap();
        assert_eq!(
            names(&only.select(tools::builtin_registry()).unwrap()),
            ["read", "write"]
        );
        let typo = opts(&["--no-tools", "bsah"]).unwrap();
        let err = typo.select(tools::builtin_registry()).err().unwrap();
        assert!(err.contains("'bsah' is not a tool"), "{err}");
        // `ask` cannot be asked for either.
        assert!(
            opts(&["--tools", "ask"])
                .unwrap()
                .select(tools::builtin_registry())
                .is_err()
        );
    }

    #[test]
    fn annotations_follow_what_the_tools_say_of_themselves() {
        let registry = tools::builtin_registry();
        let hints = |name: &str| annotations(registry.get(name).unwrap());
        assert_eq!(hints("read"), json!({ "readOnlyHint": true }));
        assert_eq!(hints("grep"), json!({ "readOnlyHint": true }));
        assert_eq!(hints("edit"), json!({ "readOnlyHint": false }));
        assert_eq!(hints("memory"), json!({ "readOnlyHint": false }));
        let destructive = json!({ "readOnlyHint": false, "destructiveHint": true });
        assert_eq!(hints("bash"), destructive);
        assert_eq!(hints("write"), destructive);
    }

    /// A session in a scratch project, serving `input`; returns what the
    /// server wrote, as messages.
    fn exchange(name: &str, mode: Mode, input: &str) -> (Vec<Value>, std::path::PathBuf) {
        let root = trust::testing::dir(&format!("mcp-server-{name}"));
        let mut session = Session::new(
            Config::from_env(false),
            root.clone(),
            Options::default()
                .select(tools::builtin_registry())
                .unwrap(),
            Vec::new(),
            Settings::default(),
            trust::testing::open(&root),
        );
        session.set_mode(mode);
        let mut output = Vec::new();
        serve(&mut session, Cursor::new(input.to_string()), &mut output).unwrap();
        let text = String::from_utf8(output).unwrap();
        let replies = text.lines().map(|l| serde_json::from_str(l).unwrap());
        (replies.collect(), root)
    }

    fn request(id: u64, method: &str, params: Value) -> String {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string() + "\n"
    }

    fn tool_call(id: u64, name: &str, arguments: Value) -> String {
        request(
            id,
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
    }

    /// An unconfined call (`web_fetch`, which no test lets reach the
    /// network) with a client that cannot elicit: refused, with the rule.
    #[test]
    fn without_elicitation_a_call_that_asks_is_refused_with_the_rule() {
        let input = request(1, "initialize", json!({ "capabilities": {} }))
            + &tool_call(2, "web_fetch", json!({ "url": "http://127.0.0.1:9/x" }));
        let (replies, root) = exchange("refuse", Mode::AutoEdit, &input);
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[1]["result"]["isError"], true);
        let text = replies[1]["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("web_fetch needs approval"), "{text}");
        assert!(
            text.contains("allow = [\"web_fetch(domain:127.0.0.1)\"]"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// With elicitation the client's user is asked, mid-call: the request
    /// goes out, a `ping` is answered while it is open, a `tools/list` waits
    /// for the call, and "no" refuses the call as a user's no.
    #[test]
    fn with_elicitation_the_clients_user_decides() {
        let caps = json!({ "capabilities": { "elicitation": {} } });
        let declined = json!({ "jsonrpc": "2.0", "id": "atelier-approval-1",
            "result": { "action": "accept", "content": { "decision": "no" } } });
        let input = request(1, "initialize", caps)
            + &tool_call(2, "web_fetch", json!({ "url": "http://127.0.0.1:9/x" }))
            + &request(3, "ping", json!({}))
            + &request(4, "tools/list", json!({}))
            + "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n"
            + &declined.to_string()
            + "\n";
        let (replies, root) = exchange("elicit", Mode::AutoEdit, &input);
        let ids: Vec<&Value> = replies.iter().map(|r| &r["id"]).collect();
        assert_eq!(
            ids,
            [
                &json!(1),
                &json!("atelier-approval-1"),
                &json!(3),
                &json!(2),
                &json!(4)
            ]
        );
        let ask = &replies[1];
        assert_eq!(ask["method"], "elicitation/create");
        let message = ask["params"]["message"].as_str().unwrap();
        assert!(message.contains("allow the tool `web_fetch`"), "{message}");
        assert!(message.contains("127.0.0.1:9"), "{message}");
        assert_eq!(
            ask["params"]["requestedSchema"]["properties"]["decision"]["enum"],
            json!(["yes", "always", "no"])
        );
        assert_eq!(replies[3]["result"]["isError"], true);
        assert_eq!(replies[3]["result"]["content"][0]["text"], DENIED);
        let _ = std::fs::remove_dir_all(root);
    }

    /// "yes" lets the call run — here a `bash` command, which then does.
    /// A cancelled question, or a client that hangs up, is a no.
    #[test]
    fn an_accepted_elicitation_runs_the_call() {
        let caps = json!({ "capabilities": { "elicitation": {} } });
        let reply = |n: u64, result: Value| {
            json!({ "jsonrpc": "2.0", "id": format!("atelier-approval-{n}"), "result": result })
                .to_string()
                + "\n"
        };
        let bash = |id: u64, file: &str| {
            tool_call(id, "bash", json!({ "command": format!("touch {file}") }))
        };
        let yes = json!({ "action": "accept", "content": { "decision": "yes" } });
        let input = request(1, "initialize", caps)
            + &bash(2, "yes.txt")
            + &reply(1, yes)
            + &bash(3, "cancelled.txt")
            + &reply(2, json!({ "action": "cancel" }))
            + &bash(4, "hung-up.txt");
        let (replies, root) = exchange("accept", Mode::AutoEdit, &input);
        let result = |id: u64| {
            let reply = replies
                .iter()
                .find(|r| r["id"] == id && r.get("result").is_some());
            reply.unwrap_or_else(|| panic!("no reply to {id}: {replies:?}"))["result"].clone()
        };
        assert_eq!(result(2)["isError"], false, "{replies:?}");
        assert!(root.join("yes.txt").exists());
        assert_eq!(result(3)["isError"], true);
        assert_eq!(result(4)["isError"], true);
        assert!(!root.join("cancelled.txt").exists());
        assert!(!root.join("hung-up.txt").exists());
        let _ = std::fs::remove_dir_all(root);
    }
}
