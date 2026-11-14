//! Code mode: `node` scripts that call the session's tools.
//!
//! A `node` call with `tools: true` gets a `tools` object whose `call` runs
//! another tool and hands the script its result (see [`crate::js::tools`]).
//! The model then reads what the script prints, not every intermediate
//! result: twenty reads whose only use is a count cost one line of context.
//!
//! # A script decides nothing
//!
//! The danger is plain: were `tools.call` to run tools itself, `node` —
//! which runs unasked, being confined to the project — would be a way to
//! run `bash`, or a denied MCP tool, without anyone being asked. So it is
//! not a tool that runs tools, any more than `mcp_call` is (see
//! [`crate::mcp::deferred`]). The script's thread holds a channel and
//! nothing else. Each `tools.call(name, args)` is sent to the turn's thread
//! and served by [`Gate::call`], which treats it as the model calling `name`
//! with `args`, in the order the loop does it:
//!
//! 1. the name must be one a script may call (below) and a tool this
//!    session has;
//! 2. `pre_tool` hooks, matched on and told the real name — they may
//!    refuse, rewrite the arguments, or approve in the user's place;
//! 3. the approval ([`ToolRunner::approve_hooked`]): permission rules, the
//!    mode, the tool's own default, and when those say to ask, the user —
//!    shown the real tool and its arguments after a line saying a script is
//!    asking. In a subagent this is, as for its other calls, the parent's
//!    decision;
//! 4. the tool, through the runner's own `exec`, so with the session's read
//!    tracking and the tool's own caps on what it returns;
//! 5. `post_tool` hooks.
//!
//! A refusal at any step is that call's result (`{ok: false, text}`): the
//! script goes on, and can say what happened.
//!
//! [`ToolRunner::exec`] hands a `node` call with `tools: true` over to
//! [`ToolRunner::run_script`], after the `node` call itself was approved
//! like any other. That one seam covers atelier's own loop, a subagent's,
//! and the tools atelier offers claude. Nothing else runs a script with
//! tools: `NodeTool::call` refuses `tools: true` outright, and calls that
//! run together (`batch`) go by a list of tool names `node` is not on.
//!
//! # What a script may call
//!
//! Every tool of the session, deferred MCP tools included — by their real
//! names: code mode is the other answer to "too many MCP tools to
//! advertise". Not `node` (a script starting scripts multiplies the call
//! cap for nothing), not `ask` and `task` (the loop runs those through the
//! interface, on their own paths), and not `mcp_search`/`mcp_call` (a
//! script has `tools.list()` and the real names).
//!
//! # What is shown
//!
//! Each inner call is a dim line under the `node` call (`⤷ read src/x.rs`),
//! as a subagent's are; its result is the script's business, a failure gets
//! a line. Approval prompts are the usual ones.

use serde_json::Value;

use super::subagent::{clip, summarize};
use super::{Approval, ToolRunner, Ui};
use crate::agents::TASK_TOOL;
use crate::js::tools::{Callable, Host};
use crate::mcp::deferred;
use crate::tools::ToolOutput;
use crate::tools::ask::{self, Answer, Question};

/// The tool scripts run on.
const NODE: &str = "node";

/// Whether the call `name(args)` is a script that wants the `tools` object.
pub(super) fn wants_tools(name: &str, args: &Value) -> bool {
    name == NODE && crate::js::wants_tools(args)
}

/// Whether a script may call the tool named `name` (see the module docs).
fn callable(name: &str) -> bool {
    !matches!(
        name,
        NODE | ask::NAME | TASK_TOOL | deferred::SEARCH | deferred::CALL
    )
}

impl ToolRunner<'_> {
    /// Run a `node` call that has `tools: true` and passed its own
    /// approval: the script on a worker thread, its tool calls served here.
    pub(super) fn run_script(&mut self, args: Value, ui: &mut dyn Ui) -> (ToolOutput, bool) {
        let catalog = self
            .tools
            .all()
            .filter(|tool| callable(tool.name()))
            .map(|tool| {
                let spec = tool.spec();
                Callable {
                    name: spec.name,
                    description: spec.description,
                    schema: spec.parameters,
                }
            })
            .collect();
        let root = self.root;
        let mut gate = Gate { runner: self, ui };
        match crate::js::tools::run(root, &args, catalog, &mut gate) {
            Ok(text) => (ToolOutput::text(text), true),
            Err(e) => (ToolOutput::text(format!("error: {e:#}")), false),
        }
    }
}

/// Serves a script's tool calls on the turn's thread: each one decided and
/// run as a call of that tool by the model would be.
struct Gate<'r, 'a> {
    runner: &'r mut ToolRunner<'a>,
    ui: &'r mut dyn Ui,
}

impl Host for Gate<'_, '_> {
    fn call(&mut self, name: &str, arguments: &str) -> (String, bool) {
        let (out, ok) = self.decide_and_run(name, arguments);
        if !ok {
            let first = out.text.lines().next().unwrap_or("");
            self.ui.activity(&clip(&format!("  ⤷ ✗ {name}: {first}")));
        }
        let mut text = out.text;
        // A script works on text; say so rather than drop pictures silently.
        if !out.images.is_empty() {
            text.push_str(&format!(
                "\n[{} image(s) left out: a script gets a tool's text only]",
                out.images.len()
            ));
        }
        (text, ok)
    }

    fn interrupted(&mut self) -> bool {
        self.ui.interrupted()
    }
}

impl Gate<'_, '_> {
    /// One call of a script, through every check a call by the model gets.
    fn decide_and_run(&mut self, name: &str, arguments: &str) -> (ToolOutput, bool) {
        let refused = |msg: String| (ToolOutput::text(msg), false);
        if !callable(name) {
            return refused(format!(
                "error: '{name}' cannot be called from a script (tools.list() names what can)"
            ));
        }
        if self.runner.tools.get(name).is_none() {
            return refused(format!("error: unknown tool '{name}'"));
        }
        let pre = self
            .runner
            .hooks
            .pre_tool(name, arguments, &mut |m| self.ui.notice(m));
        if let Some(msg) = pre.deny {
            return refused(msg);
        }
        let mut asking = FromScript {
            inner: &mut *self.ui,
        };
        if let Err(msg) = self
            .runner
            .approve_hooked(name, &pre.arguments, pre.allow, &mut asking)
        {
            return refused(msg);
        }
        self.ui
            .activity(&format!("  ⤷ {}", summarize(name, &pre.arguments)));
        // A hook rewrote them, or the script passed something that is not
        // an object: what the tool makes of it is the tool's to say.
        let args = match serde_json::from_str(&pre.arguments) {
            Ok(args) => args,
            Err(e) => return refused(format!("error: invalid arguments JSON: {e}")),
        };
        // `exec_only`: a directory's instruction file is for the model, so
        // it goes with the script's own result (`ToolRunner::exec`), not
        // into text the script may be about to parse.
        let (out, ok) = self.runner.exec_only(name, args, self.ui);
        let out = self
            .runner
            .post_tool(name, &pre.arguments, out, ok, self.ui);
        (out, ok)
    }
}

/// The session's [`Ui`] while it decides a script's call: the same prompt,
/// after a line saying that a script — not the model directly — wants to
/// run the tool.
struct FromScript<'a> {
    inner: &'a mut dyn Ui,
}

impl Ui for FromScript<'_> {
    fn reasoning(&mut self, text: &str) {
        self.inner.reasoning(text);
    }
    fn content(&mut self, text: &str) {
        self.inner.content(text);
    }
    fn tool_start(&mut self, name: &str, arguments: &str) {
        self.inner.tool_start(name, arguments);
    }
    fn tool_end(&mut self, name: &str, result: &str, ok: bool) {
        self.inner.tool_end(name, result, ok);
    }
    fn file_change(&mut self, tool: &str, change: &crate::tools::change::FileChange) {
        self.inner.file_change(tool, change);
    }
    fn info(&mut self, text: &str) {
        self.inner.info(text);
    }
    fn activity(&mut self, text: &str) {
        self.inner.activity(text);
    }
    fn ask_approval(&mut self, tool: &str, arguments: &str) -> Approval {
        self.inner.info("⤷ a node script asks to run:");
        self.inner.ask_approval(tool, arguments)
    }
    fn unattended(&mut self) -> bool {
        self.inner.unattended()
    }
    fn approval_reviewed(&mut self, review: &crate::permissions::reviewer::Record) {
        self.inner.approval_reviewed(review);
    }
    fn ask_user(&mut self, question: &Question) -> Option<Answer> {
        self.inner.ask_user(question)
    }
    fn notice(&mut self, text: &str) {
        self.inner.notice(text);
    }
    fn interrupted(&mut self) -> bool {
        self.inner.interrupted()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use anyhow::Result;
    use serde_json::json;

    use super::*;
    use crate::agent::Session;
    use crate::config::Config;
    use crate::permissions::Mode;
    use crate::settings::Settings;
    use crate::tools::{FileState, Tool, ToolCtx, ToolSpec, builtin_registry};

    /// A stand-in for an unconfined tool (it asks, it changes things) that
    /// records the calls that reached it.
    struct Probe {
        name: &'static str,
        ran: Arc<Mutex<Vec<String>>>,
    }

    impl Tool for Probe {
        fn name(&self) -> &str {
            self.name
        }
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.name.to_string(),
                description: format!("The {} probe.", self.name),
                parameters: json!({ "type": "object", "properties": { "n": {} } }),
            }
        }
        fn call(&self, _ctx: &mut ToolCtx, args: Value) -> Result<String> {
            self.ran.lock().unwrap().push(args.to_string());
            Ok(json!({ "tool": self.name, "got": args }).to_string())
        }
    }

    /// Answers every prompt the same way — yes, once, or no — and records
    /// what it was shown.
    struct Recorder {
        yes: bool,
        asked: Vec<String>,
        lines: Vec<String>,
    }

    impl Recorder {
        fn new(yes: bool) -> Self {
            Self {
                yes,
                asked: Vec::new(),
                lines: Vec::new(),
            }
        }
    }

    impl Ui for Recorder {
        fn reasoning(&mut self, _: &str) {}
        fn content(&mut self, _: &str) {}
        fn tool_start(&mut self, _: &str, _: &str) {}
        fn tool_end(&mut self, _: &str, _: &str, _: bool) {}
        fn info(&mut self, text: &str) {
            self.lines.push(text.to_string());
        }
        fn ask_approval(&mut self, tool: &str, arguments: &str) -> Approval {
            self.asked.push(format!("{tool} {arguments}"));
            if self.yes {
                Approval::Once
            } else {
                Approval::Deny
            }
        }
        fn notice(&mut self, _: &str) {}
    }

    /// A session in a project directory of its own, with the built-in tools
    /// and two probes.
    struct World {
        session: Session,
        root: PathBuf,
        ran: Arc<Mutex<Vec<String>>>,
    }

    impl World {
        fn new(name: &str) -> Self {
            let root = crate::trust::testing::dir(&format!("script-{name}"));
            let ran = Arc::new(Mutex::new(Vec::new()));
            let mut tools = builtin_registry();
            for name in ["probe", "mcp__db__drop"] {
                tools.register(Box::new(Probe {
                    name,
                    ran: Arc::clone(&ran),
                }));
            }
            let session = Session::new(
                Config::from_env(false),
                root.clone(),
                tools,
                Vec::new(),
                Settings::default(),
                crate::trust::testing::open(&root),
            );
            Self { session, root, ran }
        }

        /// Run `code` as a `node` call with `tools: true` under `mode`, the
        /// way the loop does once the `node` call itself is approved.
        fn script(&mut self, mode: Mode, ui: &mut dyn Ui, code: &str) -> String {
            self.session.set_mode(mode);
            let args = json!({ "code": code, "tools": true });
            let (out, ok) = self.session.runner().exec("node", args, ui);
            assert!(ok, "{}", out.text);
            out.text
        }

        fn ran(&self) -> Vec<String> {
            self.ran.lock().unwrap().clone()
        }

        fn path(&self, name: &str) -> PathBuf {
            self.root.join(name)
        }
    }

    impl Drop for World {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn a_script_combines_the_results_of_tools_that_run_unasked() {
        let mut world = World::new("combine");
        std::fs::write(world.path("a.txt"), "one TODO\n").unwrap();
        std::fs::write(world.path("b.txt"), "nothing\n").unwrap();
        let mut ui = Recorder::new(false);
        let out = world.script(
            Mode::AutoEdit,
            &mut ui,
            r#"
                var files = ["a.txt", "b.txt"].map(function (p) {
                    return tools.call("read", { path: p });
                });
                console.log(files.filter(function (f) { return f.text.includes("TODO"); }).length);
            "#,
        );
        assert_eq!(out, "1\n");
        assert!(
            ui.asked.is_empty(),
            "reads in the project are not asked about"
        );
        // Shown as they ran.
        assert!(
            ui.lines.contains(&"  ⤷ read a.txt".to_string()),
            "{:?}",
            ui.lines
        );
    }

    #[test]
    fn a_call_that_asks_is_put_to_the_user_under_the_tools_name() {
        let mut world = World::new("ask");
        let mut ui = Recorder::new(true);
        let out = world.script(
            Mode::AutoEdit,
            &mut ui,
            r#"var r = tools.call("probe", { n: 1 }); console.log(r.ok, r.json.got.n);"#,
        );
        assert_eq!(out, "true 1\n");
        assert_eq!(ui.asked, [r#"probe {"n":1}"#]);
        assert!(
            ui.lines
                .contains(&"⤷ a node script asks to run:".to_string()),
            "{:?}",
            ui.lines
        );
        assert_eq!(world.ran(), [r#"{"n":1}"#]);
    }

    #[test]
    fn a_denied_call_does_not_run_and_the_script_is_told() {
        let mut world = World::new("deny");
        let mut ui = Recorder::new(false);
        let out = world.script(
            Mode::AutoEdit,
            &mut ui,
            r#"var r = tools.call("probe", {}); console.log(r.ok, r.text); console.log("on");"#,
        );
        assert_eq!(out, format!("false {}\non\n", crate::permissions::DENIED));
        assert_eq!(ui.asked.len(), 1);
        assert!(world.ran().is_empty());
    }

    #[test]
    fn rules_apply_to_the_inner_tool() {
        let mut world = World::new("rules");
        world.session.settings.permissions.deny = vec!["mcp__db__*".to_string()];
        world.session.settings.permissions.allow = vec!["probe".to_string()];
        let mut ui = Recorder::new(true);
        let out = world.script(
            Mode::AutoEdit,
            &mut ui,
            r#"
                console.log(tools.call("mcp__db__drop", { n: 1 }).text);
                console.log(tools.call("probe", { n: 2 }).ok);
            "#,
        );
        assert!(
            out.contains("refused by the permission rule `mcp__db__*`"),
            "{out}"
        );
        assert!(out.ends_with("true\n"), "{out}");
        assert!(ui.asked.is_empty(), "a deny never asks, an allow needs not");
        assert_eq!(world.ran(), [r#"{"n":2}"#]);
    }

    /// `node` itself is refused in plan mode (it can write), so this is the
    /// second line of defence: were a script running there, its writes and
    /// its unconfined calls would still be refused one by one.
    #[test]
    fn plan_mode_refuses_inner_calls_that_change_things() {
        let mut world = World::new("plan");
        let mut ui = Recorder::new(true);
        let out = world.script(
            Mode::Plan,
            &mut ui,
            r#"
                console.log(tools.call("write", { path: "x.txt", content: "x" }).text);
                console.log(tools.call("probe", {}).text);
                console.log(tools.call("ls", { path: "." }).ok);
            "#,
        );
        assert!(out.contains("plan mode: write is not available"), "{out}");
        assert!(out.contains("plan mode: probe is not available"), "{out}");
        assert!(out.ends_with("true\n"), "{out}");
        assert!(!world.path("x.txt").exists());
        assert!(world.ran().is_empty());
        assert!(ui.asked.is_empty());
    }

    #[test]
    fn ask_edits_asks_before_an_inner_write() {
        let mut world = World::new("askedits");
        let mut ui = Recorder::new(false);
        let out = world.script(
            Mode::AskEdits,
            &mut ui,
            r#"console.log(tools.call("write", { path: "x.txt", content: "x" }).ok);"#,
        );
        assert_eq!(out, "false\n");
        assert_eq!(ui.asked.len(), 1);
        assert!(ui.asked[0].starts_with("write "), "{:?}", ui.asked);
        assert!(!world.path("x.txt").exists());
    }

    #[test]
    fn the_tools_the_loop_runs_itself_are_not_callable() {
        let mut world = World::new("excluded");
        let mut ui = Recorder::new(true);
        let out = world.script(
            Mode::Yolo,
            &mut ui,
            r#"
                var names = tools.list().map(function (t) { return t.name; });
                console.log(["node", "ask", "task", "mcp_call", "mcp_search"]
                    .filter(function (n) { return names.includes(n); }).length);
                console.log(names.includes("probe"), names.includes("read"));
                console.log(tools.call("node", { code: "1", tools: true }).text);
                console.log(tools.call("ask", { question: "?" }).text);
                console.log(tools.call("mcp_call", { tool: "probe" }).text);
                console.log(tools.call("nope", {}).text);
            "#,
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "0");
        assert_eq!(lines[1], "true true");
        for (line, name) in lines[2..5].iter().zip(["node", "ask", "mcp_call"]) {
            assert!(
                line.contains(&format!("'{name}' cannot be called from a script")),
                "{line}"
            );
        }
        assert_eq!(lines[5], "error: unknown tool 'nope'");
        assert!(world.ran().is_empty());
    }

    /// A deferred (hidden) MCP tool is listed and callable by its real
    /// name, and still asked about.
    #[test]
    fn hidden_tools_are_called_by_their_real_name() {
        let mut world = World::new("hidden");
        world
            .session
            .tools
            .set_hidden(["mcp__db__drop".to_string()].into_iter().collect());
        let mut ui = Recorder::new(true);
        let out = world.script(
            Mode::AutoEdit,
            &mut ui,
            r#"
                console.log(tools.list().some(function (t) { return t.name === "mcp__db__drop"; }));
                console.log(tools.schema("mcp__db__drop").type);
                console.log(tools.call("mcp__db__drop", { n: 3 }).json.tool);
            "#,
        );
        assert_eq!(out, "true\nobject\nmcp__db__drop\n");
        assert_eq!(ui.asked, [r#"mcp__db__drop {"n":3}"#]);
    }

    /// The session's read tracking is the script's too: what it read
    /// through `tools.call` can be edited, and an edit of an unread file is
    /// refused as it would be for the model.
    #[test]
    fn inner_calls_share_the_sessions_read_tracking() {
        let mut world = World::new("fstate");
        std::fs::write(world.path("seen.txt"), "old\n").unwrap();
        std::fs::write(world.path("unseen.txt"), "old\n").unwrap();
        let mut ui = Recorder::new(true);
        let out = world.script(
            Mode::AutoEdit,
            &mut ui,
            r#"
                tools.call("read", { path: "seen.txt" });
                var edit = function (p) {
                    return tools.call("edit", { path: p, old_string: "old", new_string: "new" }).ok;
                };
                console.log(edit("seen.txt"), edit("unseen.txt"));
            "#,
        );
        assert_eq!(out, "true false\n");
        assert_eq!(
            std::fs::read_to_string(world.path("seen.txt")).unwrap(),
            "new\n"
        );
        assert_eq!(
            std::fs::read_to_string(world.path("unseen.txt")).unwrap(),
            "old\n"
        );
    }

    /// The tools atelier offers claude run through the same runner, so a
    /// script claude starts is gated the same way.
    #[test]
    fn the_claude_tool_host_runs_scripts_through_the_gate() {
        let mut world = World::new("claude");
        let mut ui = Recorder::new(false);
        let args = json!({ "code": "console.log(tools.call('probe', {}).ok)", "tools": true });
        let (out, ok) =
            crate::claude::ToolHost::call(&mut world.session.runner(), "node", args, &mut ui);
        assert!(ok, "{}", out.text);
        assert_eq!(out.text, "false\n");
        assert_eq!(ui.asked, ["probe {}"]);
        assert!(world.ran().is_empty());
    }

    /// Without the runner there is no `tools`: the tool alone refuses.
    #[test]
    fn the_node_tool_alone_refuses_tools() {
        let world = World::new("alone");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &world.root,
            fstate: &mut fstate,
            approved: true,
        };
        let args = json!({ "code": "tools.call('probe')", "tools": true });
        let err = crate::js::NodeTool.call(&mut ctx, args).unwrap_err();
        assert!(err.to_string().contains("not available here"), "{err}");
        assert!(world.ran().is_empty());
    }
}
