//! Code mode: the `tools` host object, and the bridge that serves it.
//!
//! With `tools: true` a script can call the session's other tools and work
//! on their results itself, so that only what it prints reaches the model:
//!
//! ```js
//! const issues = tools.call("mcp__tracker__search", { query: "label:bug" });
//! const files = issues.json.items.map(i => tools.call("read", { path: i.file }).text);
//! console.log(files.filter(f => f.includes("TODO")).length);
//! ```
//!
//! # Two threads, one of which may decide
//!
//! A script runs on a worker thread (see [`super::NodeTool`]). Whether a tool
//! call may run is decided on the turn's thread: that is where the session's
//! permission rules, its hooks and the interface that asks the user live, and
//! the interface is not `Send`. So the script side holds no tool and decides
//! nothing. `tools.call` sends a [`Request`] — a name and the JSON of the
//! arguments — to the turn's thread and blocks until the answer comes back;
//! [`run`], on the turn's thread, hands each request to a [`Host`] and
//! returns what it said. The host (the agent's tool runner, see
//! `agent::script`) puts the call through everything a call made by the
//! model goes through. This is the arrangement subagents use for their
//! approvals (`agent::subagent`), for the same reason.
//!
//! `tools.list()` and `tools.schema(name)` need no round trip: the catalog
//! of callable tools is handed to the script when it starts. It is a
//! description only — a name the catalog does not hold is refused by the
//! host like any other call it would not run.
//!
//! # The boundary is strings
//!
//! As with the network host, nothing but strings crosses between JS and
//! Rust: arguments go out as `JSON.stringify` wrote them and each answer
//! comes back as JSON text for `JSON.parse`. kataan's garbage collector
//! moves objects, so building a result object field by field while calling
//! back into the interpreter is the thing to avoid.
//!
//! # The clock
//!
//! `timeout_ms` bounds the script's own running time. The turn's thread
//! measures it as the time it spends *waiting for the script* — to make its
//! next call or to finish. While a request is being served (the user reading
//! an approval prompt, the tool running) the script is blocked and its clock
//! is stopped; a tool that takes long is the tool's business, with its own
//! timeout.

use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use kataan::interrupt::Interrupt;
use kataan::{Ctx, Interp, NanBox};
use serde_json::{Value, json};

use super::{INTERRUPT_GRACE_MS, MAX_OUTPUT_BYTES, Params, WORKER_STACK_BYTES, runtime};
use crate::tools::spill;

/// The most tool calls one script may make. A loop over a few dozen items is
/// what code mode is for; a script calling tools without end is a mistake
/// the timeout would not catch (waiting on a tool is not on its clock).
pub const MAX_CALLS: usize = 50;

/// How often the turn's thread looks for a stop request while the script
/// computes.
const POLL: Duration = Duration::from_millis(100);

/// A tool a script may call, as `tools.list()` and `tools.schema()` show it.
pub struct Callable {
    pub name: String,
    pub description: String,
    /// The JSON Schema of its arguments.
    pub schema: Value,
}

/// What runs a script's tool calls: the session, on the turn's thread.
pub trait Host {
    /// Run `name` with `arguments` (JSON text) if a call of it by the model
    /// would run, and return its result and whether it succeeded. A call
    /// that is refused is a failed call whose result says why.
    fn call(&mut self, name: &str, arguments: &str) -> (String, bool);
    /// Whether the user asked to stop the turn.
    fn interrupted(&mut self) -> bool;
}

/// One `tools.call`, on its way to the turn's thread.
struct Request {
    name: String,
    arguments: String,
    /// Where the result goes: its text and whether the call succeeded.
    reply: Sender<(String, bool)>,
}

/// What the script's thread tells the turn's thread.
enum Event {
    Call(Request),
    /// The script ended; its output. Always the last event.
    Done(String),
}

/// The script's end of the bridge: what [`install`] builds `tools` from.
pub struct Port {
    catalog: Vec<Callable>,
    events: Sender<Event>,
}

/// JS run after the main bootstrap when the call has `tools: true`: the
/// `tools` object over the three `__atelier_tools_*` globals.
pub const BOOTSTRAP: &str = r#"
tools = {
  list: function () { return JSON.parse(__atelier_tools_list()); },
  schema: function (name) { return JSON.parse(__atelier_tools_schema(String(name))); },
  call: function (name, args) {
    if (args === undefined || args === null) args = {};
    return JSON.parse(__atelier_tools_call(String(name), JSON.stringify(args)));
  },
};
"#;

/// A call's result as the JSON text `tools.call` parses: `{ok, text}`, and
/// `json` when the text is itself a JSON object or array — what MCP tools
/// commonly return. Other text is left as text: a file that happens to read
/// `42` is not a number.
fn answer_json(text: &str, ok: bool) -> String {
    let mut answer = json!({ "ok": ok, "text": text });
    if text.trim_start().starts_with(['{', '['])
        && let Ok(parsed) = serde_json::from_str::<Value>(text)
    {
        answer["json"] = parsed;
    }
    answer.to_string()
}

/// Read argument `i` as a string.
fn arg_str(cx: &mut Ctx, args: &[NanBox], i: usize) -> Result<String, NanBox> {
    cx.to_string(args.get(i).copied().unwrap_or_else(|| cx.undefined()))
}

/// Register the `__atelier_tools_*` globals [`BOOTSTRAP`] assembles into
/// `tools`.
pub fn install(interp: &mut Interp, port: Port) {
    let Port { catalog, events } = port;
    let listing = Value::Array(
        catalog
            .iter()
            .map(|t| json!({ "name": t.name, "description": t.description }))
            .collect(),
    )
    .to_string();
    interp.register_global_fn("__atelier_tools_list", 0, move |cx, _this, _args| {
        Ok(cx.string(&listing))
    });

    interp.register_global_fn("__atelier_tools_schema", 1, move |cx, _this, args| {
        let name = arg_str(cx, args, 0)?;
        match catalog.iter().find(|t| t.name == name) {
            Some(tool) => Ok(cx.string(&tool.schema.to_string())),
            None => Err(cx.error(&format!(
                "tools.schema: no tool named '{name}' can be called from a script \
                 (tools.list() names those that can)"
            ))),
        }
    });

    interp.register_global_fn("__atelier_tools_call", 2, move |cx, _this, args| {
        let name = arg_str(cx, args, 0)?;
        let arguments = arg_str(cx, args, 1)?;
        let (reply, answer) = mpsc::channel();
        let asked = events.send(Event::Call(Request {
            name,
            arguments,
            reply,
        }));
        // Nobody answers once the turn's thread has given the script up
        // (its time ran out, or the user stopped the turn).
        let (text, ok) = match (asked, answer.recv()) {
            (Ok(()), Ok(answer)) => answer,
            _ => return Err(cx.error("tools.call: the script was stopped")),
        };
        Ok(cx.string(&answer_json(&text, ok)))
    });
}

/// Run a `node` call that has `tools: true`, on the turn's thread: start the
/// script on a worker and serve its tool calls through `host` until it ends.
/// `catalog` is what the script is told it can call. The result is the
/// script's output, as for any `node` call.
pub fn run(
    project_root: &Path,
    args: &Value,
    catalog: Vec<Callable>,
    host: &mut dyn Host,
) -> Result<String> {
    let Params {
        code,
        network,
        timeout_ms,
    } = Params::parse(args)?;
    let root = project_root.to_path_buf();
    let interrupt = Interrupt::new();
    let worker_interrupt = interrupt.clone();
    let (events, inbox) = mpsc::channel();
    let port = Port {
        catalog,
        events: events.clone(),
    };
    let worker = thread::Builder::new()
        .name("node-script".into())
        .stack_size(WORKER_STACK_BYTES)
        .spawn(move || {
            let out = runtime::execute(code, root, network, Some(port), worker_interrupt);
            // Nobody listening means the script was given up on.
            let _ = events.send(Event::Done(out));
        })
        .map_err(|e| anyhow!("failed to spawn node worker thread: {e}"))?;

    // The script's remaining time: only waiting for it uses any up.
    let mut left = Duration::from_millis(timeout_ms);
    let mut calls = 0;
    let stopped = loop {
        let waiting = Instant::now();
        let event = inbox.recv_timeout(left.min(POLL));
        left = left.saturating_sub(waiting.elapsed());
        match event {
            Ok(Event::Done(output)) => {
                let _ = worker.join();
                return Ok(spill::fit(project_root, output, MAX_OUTPUT_BYTES));
            }
            Ok(Event::Call(request)) => {
                if host.interrupted() {
                    break "node: the script was stopped by the user".to_string();
                }
                let answer = if calls == MAX_CALLS {
                    let msg = format!(
                        "error: this script has made its {MAX_CALLS} tool calls, the most one \
                         script may make; this one was not run. Do the rest in another script."
                    );
                    (msg, false)
                } else {
                    calls += 1;
                    host.call(&request.name, &request.arguments)
                };
                let _ = request.reply.send(answer);
            }
            Err(RecvTimeoutError::Timeout) => {
                if host.interrupted() {
                    break "node: the script was stopped by the user".to_string();
                }
                if left.is_zero() {
                    break format!(
                        "node: script exceeded the {timeout_ms}ms time limit and was aborted"
                    );
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                let _ = worker.join();
                bail!("node: script worker terminated unexpectedly");
            }
        }
    };
    interrupt.trip();
    reap(worker, &inbox);
    Ok(stopped)
}

/// Wait a moment for an interrupted script to unwind, and reap its thread
/// if it does. A call it makes in the meantime is dropped unanswered, which
/// `tools.call` turns into a thrown error. A script that neither ends nor
/// reaches a loop back-edge in that time is abandoned, as without `tools`.
fn reap(worker: thread::JoinHandle<()>, inbox: &Receiver<Event>) {
    let deadline = Instant::now() + Duration::from_millis(INTERRUPT_GRACE_MS);
    loop {
        match inbox.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Event::Call(_)) => {}
            Ok(Event::Done(_)) | Err(RecvTimeoutError::Disconnected) => {
                let _ = worker.join();
                return;
            }
            Err(RecvTimeoutError::Timeout) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host with two stand-in tools: `echo` answers its arguments, `slow`
    /// takes a while, and `no` is refused. Records what it was asked.
    #[derive(Default)]
    struct Fake {
        seen: Vec<(String, String)>,
        stop: bool,
    }

    impl Host for Fake {
        fn call(&mut self, name: &str, arguments: &str) -> (String, bool) {
            self.seen.push((name.to_string(), arguments.to_string()));
            match name {
                "echo" => (arguments.to_string(), true),
                "plain" => ("just text".to_string(), true),
                "slow" => {
                    thread::sleep(Duration::from_millis(600));
                    ("slept".to_string(), true)
                }
                _ => ("error: refused".to_string(), false),
            }
        }
        fn interrupted(&mut self) -> bool {
            self.stop
        }
    }

    fn catalog() -> Vec<Callable> {
        vec![Callable {
            name: "echo".into(),
            description: "Says it back.".into(),
            schema: json!({ "type": "object", "properties": { "n": { "type": "integer" } } }),
        }]
    }

    fn script(host: &mut Fake, code: &str, timeout_ms: u64) -> String {
        let root = std::env::temp_dir();
        let args = json!({ "code": code, "tools": true, "timeout_ms": timeout_ms });
        run(&root, &args, catalog(), host).unwrap()
    }

    #[test]
    fn a_script_calls_tools_and_computes_on_the_results() {
        let mut host = Fake::default();
        let out = script(
            &mut host,
            r#"
                var a = tools.call("echo", { n: 2 });
                var b = tools.call("echo", { n: 40 });
                console.log(a.ok, a.text, a.json.n + b.json.n);
            "#,
            5_000,
        );
        assert_eq!(out, "true {\"n\":2} 42\n");
        assert_eq!(
            host.seen,
            [
                ("echo".to_string(), r#"{"n":2}"#.to_string()),
                ("echo".to_string(), r#"{"n":40}"#.to_string()),
            ]
        );
    }

    /// The example of the docs, in the syntax a model is likely to write.
    #[test]
    fn modern_syntax_works_with_tool_results() {
        let mut host = Fake::default();
        let out = script(
            &mut host,
            r#"
                const found = tools.call("echo", { items: [{ n: 1 }, { n: 5 }] });
                const big = found.json.items.map(i => tools.call("echo", i).json.n);
                console.log(big.filter(n => n > 2).length);
            "#,
            5_000,
        );
        assert_eq!(out, "1\n");
        assert_eq!(host.seen.len(), 3);
    }

    #[test]
    fn a_refused_call_is_a_result_not_an_exception() {
        let mut host = Fake::default();
        let out = script(
            &mut host,
            r#"
                var r = tools.call("no");
                console.log(r.ok, r.text, typeof r.json);
                console.log("still running");
            "#,
            5_000,
        );
        assert_eq!(out, "false error: refused undefined\nstill running\n");
        // Missing arguments are an empty object.
        assert_eq!(host.seen, [("no".to_string(), "{}".to_string())]);
    }

    #[test]
    fn json_is_only_for_objects_and_arrays() {
        let mut host = Fake::default();
        let out = script(
            &mut host,
            r#"
                console.log(typeof tools.call("plain").json);
                console.log(typeof tools.call("echo", [1, 2]).json);
            "#,
            5_000,
        );
        assert_eq!(out, "undefined\nobject\n");
    }

    #[test]
    fn list_and_schema_come_from_the_catalog() {
        let mut host = Fake::default();
        let out = script(
            &mut host,
            r#"
                var l = tools.list();
                console.log(l.length, l[0].name, l[0].description);
                console.log(tools.schema("echo").properties.n.type);
                try { tools.schema("bash"); } catch (e) { console.log("caught:", e.message); }
            "#,
            5_000,
        );
        assert!(
            out.starts_with("1 echo Says it back.\ninteger\ncaught: "),
            "{out}"
        );
        assert!(out.contains("no tool named 'bash'"), "{out}");
        assert!(host.seen.is_empty(), "neither is a tool call");
    }

    #[test]
    fn calls_past_the_cap_are_not_run() {
        let mut host = Fake::default();
        let out = script(
            &mut host,
            r#"
                var ok = 0, last;
                for (var i = 0; i < 60; i++) {
                    last = tools.call("echo", { n: i });
                    if (last.ok) ok++;
                }
                console.log(ok, last.text);
            "#,
            5_000,
        );
        assert!(
            out.starts_with("50 error: this script has made its 50 tool calls"),
            "{out}"
        );
        assert_eq!(host.seen.len(), MAX_CALLS);
    }

    /// Time spent inside a tool is not the script's: a call longer than the
    /// whole budget does not time the script out.
    #[test]
    fn waiting_on_a_tool_is_not_on_the_scripts_clock() {
        let mut host = Fake::default();
        let out = script(
            &mut host,
            r#"console.log(tools.call("slow").text, tools.call("slow").text);"#,
            300,
        );
        assert_eq!(out, "slept slept\n");
    }

    /// The script's own running time still is.
    #[test]
    fn a_script_that_spins_after_a_call_times_out() {
        let mut host = Fake::default();
        let started = Instant::now();
        let out = script(&mut host, r#"tools.call("echo"); while (true) {}"#, 300);
        assert!(out.contains("exceeded the 300ms time limit"), "{out}");
        assert!(started.elapsed() < Duration::from_millis(300 + INTERRUPT_GRACE_MS + 500));
        assert_eq!(host.seen.len(), 1);
    }

    #[test]
    fn stopping_the_turn_stops_the_script() {
        let mut host = Fake {
            stop: true,
            ..Fake::default()
        };
        let out = script(&mut host, r#"while (true) { tools.call("echo"); }"#, 60_000);
        assert_eq!(out, "node: the script was stopped by the user");
        assert!(
            host.seen.is_empty(),
            "no call runs once the turn is stopped"
        );
    }
}
