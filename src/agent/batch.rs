//! Read-only tool calls of one model step, run at the same time.
//!
//! A model that wants to look at five files asks for five `read`s in one
//! step. Nothing about them depends on the order they run in, so the loop
//! hands the calls that follow one another and only *look* to
//! [`Session::run_batch`], which runs them on scoped threads and gives back
//! their results in call order — the same seam `task` calls use (see
//! `subagent`).
//!
//! # What runs together
//!
//! A maximal run of consecutive calls, each of which is one of the built-in
//! tools in [`PARALLEL`] *and* says of these arguments that it changes
//! nothing ([`Tool::mutates`]: `memory` reads but also saves). Anything else
//! — a tool that writes, `ask` and `task` (run by the loop, through the
//! interface), MCP tools (one connection, a server that may not expect
//! overlapping requests) — ends the run and goes through the loop on its
//! own, as before. So `[read a, grep x, write b, read c]` is `read a` and
//! `grep x` together, then `write b`, then `read c`. A run of one is not a
//! batch.
//!
//! # Order of events
//!
//! 1. **Decide, in call order, on the loop's thread.** Each call gets its
//!    `pre_tool` hooks and its approval (permission rules, the mode, maybe a
//!    prompt to the user) before anything runs. A refused call has its
//!    refusal as its result; the others still run.
//! 2. **Run** the approved calls on up to [`MAX_THREADS`] threads.
//! 3. **Finish, in call order, on the loop's thread**: merge what the call
//!    read into the session's [`FileState`], run `post_tool` hooks, show the
//!    result, hand the text back.
//!
//! The history therefore gets exactly one result per call, in the order of
//! the calls, whatever order they finished in.
//!
//! A `pre_tool` hook may rewrite a call's arguments into ones that change
//! something (`memory` `list` into `save`). Its hooks have run by then and
//! must not run twice, so the batch ends with that call: it runs alone, on
//! the loop's thread, once the calls before it are finished.
//!
//! # Read tracking without a lock
//!
//! Tools record what they read in the session's [`FileState`], which is
//! `&mut`: sharing it would mean a lock held across each call, that is, no
//! concurrency. The tools that run here only ever *record* (the checks
//! against it are `edit`'s and its kin, which never run here), so each call
//! gets an empty tracker of its own and step 3 merges them in call order
//! ([`FileState::merge`]). That is also where a directory's own instruction
//! file is appended to a result: after the merge of the first call, in call
//! order, that entered it — once, even when two calls entered it together.
//!
//! # What is shown
//!
//! Only the loop's thread touches the [`Ui`] (it is not `Send`). A call's
//! `tool_start` is shown when it is approved — all of them up front, in call
//! order — and each `tool_end` in step 3, in call order: whole lines, never
//! interleaved, and nothing a call printed can land inside another's.
//!
//! # Stopping
//!
//! The loop's thread polls [`Ui::interrupted`] while the calls run. Once it
//! is set, calls that have not started are not started — their result is
//! [`NOT_RUN`] — and the ones running finish (a tool call cannot be
//! cancelled from outside). With no more calls than threads they all start
//! at once, so this matters for long batches.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use serde_json::Value;

use super::{Session, Ui};
use crate::provider::ToolCall;
use crate::settings::Defaults;
use crate::tools::{FileState, Tool, ToolCtx, ToolOutput};

/// The tools whose calls may run at the same time, when the call changes
/// nothing. A list of names rather than "whatever does not mutate": that
/// also holds for `bash_kill`, `todo` and the coordinator's tools, whose
/// calls are not independent of their neighbours.
const PARALLEL: &[&str] = &[
    "read",
    "grep",
    "glob",
    "ls",
    "tree",
    "symbols",
    "web_fetch",
    "web_search",
    "skill",
    "memory",
    "bash_output",
];

/// The most calls running at once; the rest of a longer batch wait their
/// turn.
const MAX_THREADS: usize = 8;

/// How often the loop's thread looks for a stop request while calls run.
const POLL: Duration = Duration::from_millis(100);

/// The result of a call that was never started because the user stopped the
/// turn first.
pub(super) const NOT_RUN: &str = "error: not run — the user stopped the turn.";

/// Turns the whole thing off: `ATELIER_PARALLEL_TOOLS=0`.
const ENV: &str = "ATELIER_PARALLEL_TOOLS";

/// Whether read-only calls run together: `ATELIER_PARALLEL_TOOLS` when set
/// (`0`, `off`, `no` or `false` for no), else `[defaults] parallel_tools`,
/// else yes.
fn enabled(env: Option<&str>, defaults: &Defaults) -> bool {
    match env {
        Some(v) => !matches!(v, "0" | "off" | "no" | "false"),
        None => defaults.parallel_tools.unwrap_or(true),
    }
}

/// What was decided about one call of a batch.
enum Plan {
    /// Refused by a hook, a rule or the user: its tool result. Already shown.
    Refused(String),
    /// Approved, to run with the others, with these arguments.
    Run(String),
    /// Approved, but a hook rewrote it into a call that changes something:
    /// runs by itself after the others. Always the last of its batch.
    Alone(String),
    /// The user stopped the turn before it was this call's turn.
    Stopped,
}

/// What a call that ran with the others produced: its output, whether it
/// succeeded, and what it read. `Err` when the tool panicked.
type Outcome = std::thread::Result<(ToolOutput, bool, FileState)>;

/// Run one approved call, on a worker thread: [`super::ToolRunner`]'s
/// `exec_json`, with a tracker of its own in place of the session's.
fn run(tool: &dyn Tool, root: &Path, arguments: &str) -> (ToolOutput, bool, FileState) {
    let mut reads = FileState::new();
    let args = if arguments.trim().is_empty() {
        Ok(Value::Null)
    } else {
        serde_json::from_str(arguments)
    };
    let result = match args {
        Ok(args) => {
            let mut ctx = ToolCtx {
                project_root: root,
                fstate: &mut reads,
                approved: true,
            };
            tool.call_output(&mut ctx, args)
                .map_err(|e| format!("error: {e:#}"))
        }
        Err(e) => Err(format!("error: invalid arguments JSON: {e}")),
    };
    match result {
        Ok(out) => (out, true, reads),
        Err(msg) => (ToolOutput::text(msg), false, reads),
    }
}

impl Session {
    /// Whether a call of `name` with these arguments may run at the same
    /// time as its neighbours.
    fn runs_together(&self, name: &str, arguments: &str) -> bool {
        let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
        PARALLEL.contains(&name) && self.tools.get(name).is_some_and(|t| !t.mutates(&args))
    }

    /// Run the read-only calls that open `calls` — all of them up to the
    /// first call that may not run with others — at the same time, and
    /// return their tool results in call order. Empty when fewer than two
    /// calls qualify (or this is switched off): the loop then runs the
    /// first call itself. Images the calls returned are added to `images`.
    pub(super) fn run_batch(
        &mut self,
        calls: &[ToolCall],
        images: &mut Vec<String>,
        ui: &mut dyn Ui,
    ) -> VecDeque<String> {
        let env = std::env::var(ENV).ok();
        if !enabled(env.as_deref(), &self.settings.defaults) {
            return VecDeque::new();
        }
        let batch = calls
            .iter()
            .take_while(|c| self.runs_together(&c.name, &c.arguments))
            .count();
        if batch < 2 {
            return VecDeque::new();
        }
        let calls = &calls[..batch];
        let stop = AtomicBool::new(false);

        // Step 1: hooks and approval, in call order.
        let mut plans: Vec<Plan> = Vec::with_capacity(batch);
        for call in calls {
            if ui.interrupted() {
                stop.store(true, Ordering::Relaxed);
                plans.push(Plan::Stopped);
                continue;
            }
            let pre = self
                .hooks
                .pre_tool(&call.name, &call.arguments, &mut |m| ui.notice(m));
            let checked = match pre.deny {
                Some(msg) => Err(msg),
                None => self
                    .runner()
                    .approve_hooked(&call.name, &pre.arguments, pre.allow, ui),
            };
            if let Err(msg) = checked {
                ui.tool_end(&call.name, &msg, false);
                plans.push(Plan::Refused(msg));
            } else if self.runs_together(&call.name, &pre.arguments) {
                ui.tool_start(&call.name, &pre.arguments);
                plans.push(Plan::Run(pre.arguments));
            } else {
                plans.push(Plan::Alone(pre.arguments));
                break;
            }
        }

        // Step 2: the approved calls, at the same time.
        let (tools, root) = (&self.tools, self.root.as_path());
        let jobs: Vec<(usize, &dyn Tool, &str)> = plans
            .iter()
            .enumerate()
            .filter_map(|(index, plan)| match plan {
                Plan::Run(arguments) => Some((index, tools.get(&calls[index].name)?, &**arguments)),
                _ => None,
            })
            .collect();
        let mut outcomes: Vec<Option<Outcome>> = plans.iter().map(|_| None).collect();
        let next = AtomicUsize::new(0);
        let (done, inbox) = mpsc::channel();
        std::thread::scope(|scope| {
            for _ in 0..jobs.len().min(MAX_THREADS) {
                let done = done.clone();
                let (jobs, next, stop) = (&jobs, &next, &stop);
                scope.spawn(move || {
                    while let Some(&(index, tool, arguments)) =
                        jobs.get(next.fetch_add(1, Ordering::Relaxed))
                    {
                        // Stopped: this call and the ones after it stay
                        // unstarted.
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        // A tool that panics costs its own result, not the
                        // calls queued behind it on this thread.
                        let outcome = catch_unwind(AssertUnwindSafe(|| run(tool, root, arguments)));
                        let _ = done.send((index, outcome));
                    }
                });
            }
            // The workers hold the only senders, so the channel closes when
            // the last of them is done.
            drop(done);
            loop {
                match inbox.recv_timeout(POLL) {
                    Ok((index, outcome)) => outcomes[index] = Some(outcome),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
                if ui.interrupted() {
                    stop.store(true, Ordering::Relaxed);
                }
            }
        });

        // Step 3: results, in call order.
        let mut results = VecDeque::with_capacity(plans.len());
        for ((call, plan), outcome) in calls.iter().zip(plans).zip(outcomes) {
            let name = &call.name;
            let (arguments, out, ok) = match (plan, outcome) {
                // Shown when it was refused.
                (Plan::Refused(msg), _) => {
                    results.push_back(msg);
                    continue;
                }
                // Never announced, like a call the loop skips itself.
                (Plan::Stopped, _) => {
                    results.push_back(NOT_RUN.to_string());
                    continue;
                }
                (Plan::Alone(arguments), _) => {
                    if stop.load(Ordering::Relaxed) || ui.interrupted() {
                        results.push_back(NOT_RUN.to_string());
                        continue;
                    }
                    ui.tool_start(name, &arguments);
                    let (out, ok) = self.runner().exec_json(name, &arguments, ui);
                    (arguments, out, ok)
                }
                (Plan::Run(arguments), Some(Ok((mut out, ok, reads)))) => {
                    self.fstate.merge(reads);
                    // As in `ToolRunner::exec`: a subdirectory's own
                    // instruction file comes with the first result from
                    // under it.
                    if ok {
                        let entered = self.fstate.take_new_dirs();
                        if let Some(note) =
                            crate::context::nested_instructions(&self.root, &entered)
                        {
                            out.text.push_str(&note);
                        }
                    }
                    (arguments, out, ok)
                }
                // Announced, so it gets its end; no hooks for a call that
                // did not happen.
                (Plan::Run(_), Some(Err(_))) => {
                    let msg = format!("error: the {name} tool crashed");
                    ui.tool_end(name, &msg, false);
                    results.push_back(msg);
                    continue;
                }
                (Plan::Run(_), None) => {
                    ui.tool_end(name, NOT_RUN, false);
                    results.push_back(NOT_RUN.to_string());
                    continue;
                }
            };
            let out = self.runner().post_tool(name, &arguments, out, ok, ui);
            ui.tool_end(name, &out.text, ok);
            images.extend(
                out.images
                    .iter()
                    .map(|i| format!("data:{};base64,{}", i.mime, i.data)),
            );
            results.push_back(out.text);
        }
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Instant;

    use anyhow::{Result, bail};
    use serde_json::json;

    use crate::agent::Approval;
    use crate::config::Config;
    use crate::permissions::{DENIED, Mode};
    use crate::settings::Settings;
    use crate::tools::{ToolRegistry, ToolSpec, builtin_registry};

    /// How long a test tool waits for the calls it expects alongside it
    /// before giving up — failing the test instead of hanging it.
    const PATIENCE: Duration = Duration::from_secs(20);

    /// A counter several threads wait on.
    #[derive(Default)]
    struct Gate {
        count: Mutex<usize>,
        changed: Condvar,
    }

    impl Gate {
        fn add(&self) {
            *self.count.lock().unwrap() += 1;
            self.changed.notify_all();
        }
        fn get(&self) -> usize {
            *self.count.lock().unwrap()
        }
        /// Block until the count reaches `n`; `false` if it never did.
        fn wait_for(&self, n: usize) -> bool {
            let deadline = Instant::now() + PATIENCE;
            let mut count = self.count.lock().unwrap();
            while *count < n {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return false;
                }
                count = self.changed.wait_timeout(count, left).unwrap().0;
            }
            true
        }
    }

    /// A stand-in for `web_fetch` (read-only, asks for approval) that
    /// answers only once `together` calls of it are in flight at the same
    /// moment — which never happens if they run one after the other.
    struct Meet {
        together: usize,
        inside: Arc<Gate>,
    }

    impl Tool for Meet {
        fn name(&self) -> &str {
            "web_fetch"
        }
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.name().to_string(),
                description: String::new(),
                parameters: json!({"type": "object"}),
            }
        }
        fn mutates(&self, _args: &Value) -> bool {
            false
        }
        fn call(&self, _ctx: &mut ToolCtx, args: Value) -> Result<String> {
            self.inside.add();
            if !self.inside.wait_for(self.together) {
                bail!("alone: the other calls never started");
            }
            Ok(format!("fetched {}", args["url"].as_str().unwrap_or("?")))
        }
    }

    /// A stand-in for `glob` that holds every call until the gate opens.
    struct Hold {
        started: Arc<Gate>,
        open: Arc<Gate>,
    }

    impl Tool for Hold {
        fn name(&self) -> &str {
            "glob"
        }
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.name().to_string(),
                description: String::new(),
                parameters: json!({"type": "object"}),
            }
        }
        fn mutates(&self, _args: &Value) -> bool {
            false
        }
        fn call(&self, _ctx: &mut ToolCtx, args: Value) -> Result<String> {
            self.started.add();
            if !self.open.wait_for(1) {
                bail!("never released");
            }
            Ok(format!("held {}", args["n"]))
        }
    }

    /// A [`Ui`] that records tool events and answers approval prompts from
    /// a script (`true` = once; past its end, yes).
    #[derive(Default)]
    struct Rec {
        events: Vec<String>,
        answers: VecDeque<bool>,
        /// Ask to stop once this many `Hold` calls are in flight; the poll
        /// after that opens the gate.
        stop_at: Option<(usize, Arc<Gate>, Arc<Gate>)>,
        stopping: bool,
    }

    impl Ui for Rec {
        fn reasoning(&mut self, _: &str) {}
        fn content(&mut self, _: &str) {}
        fn tool_start(&mut self, name: &str, arguments: &str) {
            self.events.push(format!("start {name} {arguments}"));
        }
        fn tool_end(&mut self, name: &str, result: &str, ok: bool) {
            let first = result.lines().next().unwrap_or("");
            self.events.push(format!("end {name} {ok} {first}"));
        }
        fn info(&mut self, _: &str) {}
        fn ask_approval(&mut self, tool: &str, _: &str) -> Approval {
            self.events.push(format!("ask {tool}"));
            match self.answers.pop_front() {
                Some(false) => Approval::Deny,
                _ => Approval::Once,
            }
        }
        fn notice(&mut self, _: &str) {}
        fn interrupted(&mut self) -> bool {
            let Some((at, started, open)) = &self.stop_at else {
                return false;
            };
            if self.stopping {
                // The loop has seen the stop by now (it asked before):
                // let the running calls finish.
                open.add();
            } else if started.get() >= *at {
                self.stopping = true;
            }
            self.stopping
        }
    }

    fn session(name: &str, tools: ToolRegistry) -> (Session, PathBuf) {
        let root = std::env::temp_dir().join(format!("atl-batch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut session = Session::new(
            Config::from_env(false),
            root.clone(),
            tools,
            Vec::new(),
            Settings::default(),
            crate::trust::testing::open(&root),
        );
        session.set_mode(Mode::AutoEdit);
        (session, root)
    }

    fn call(n: usize, name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: format!("call_{n}"),
            name: name.to_string(),
            arguments: arguments.to_string(),
        }
    }

    /// [`Session::run_batch`], results as a list.
    fn run_batch(session: &mut Session, calls: &[ToolCall], ui: &mut Rec) -> Vec<String> {
        // The variable wins over the setting, and these tests are about
        // batches that run.
        assert!(
            std::env::var(ENV).is_err(),
            "unset {ENV} to run these tests"
        );
        let mut images = Vec::new();
        session.run_batch(calls, &mut images, ui).into()
    }

    #[test]
    fn the_setting_and_the_variable_switch_it_off() {
        let mut defaults = Defaults::default();
        assert!(enabled(None, &defaults));
        assert!(!enabled(Some("0"), &defaults));
        assert!(!enabled(Some("off"), &defaults));
        defaults.parallel_tools = Some(false);
        assert!(!enabled(None, &defaults));
        // The variable wins.
        assert!(enabled(Some("1"), &defaults));

        let (mut session, root) = session("off", builtin_registry());
        std::fs::write(root.join("a.txt"), "A").unwrap();
        let calls = [
            call(0, "read", json!({"path": "a.txt"})),
            call(1, "read", json!({"path": "a.txt"})),
        ];
        session.settings.defaults.parallel_tools = Some(false);
        let mut ui = Rec::default();
        assert!(run_batch(&mut session, &calls, &mut ui).is_empty());
        assert!(ui.events.is_empty(), "{:?}", ui.events);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_batch_is_the_leading_read_only_calls_and_results_keep_call_order() {
        let (mut session, root) = session("order", builtin_registry());
        std::fs::write(root.join("a.txt"), "ALPHA\n").unwrap();
        std::fs::write(root.join("c.txt"), "needle here\n").unwrap();
        let calls = [
            call(0, "read", json!({"path": "a.txt"})),
            call(1, "grep", json!({"pattern": "needle"})),
            call(2, "write", json!({"path": "b.txt", "content": "B"})),
            call(3, "read", json!({"path": "c.txt"})),
        ];
        let mut ui = Rec::default();
        let results = run_batch(&mut session, &calls, &mut ui);
        assert_eq!(results.len(), 2, "{results:?}");
        assert!(results[0].contains("ALPHA"), "{}", results[0]);
        assert!(results[1].contains("c.txt"), "{}", results[1]);
        assert_eq!(
            ui.events
                .iter()
                .map(|e| e.split(' ').take(2).collect::<Vec<_>>().join(" "))
                .collect::<Vec<_>>(),
            ["start read", "start grep", "end read", "end grep"]
        );
        // What ran on a thread is in the session's read tracking: the file
        // can be edited without reading it again.
        assert!(session.fstate.was_read(&root.join("a.txt")));
        assert!(!root.join("b.txt").exists());

        // `write` is never part of a batch, and one call is not a batch.
        assert!(run_batch(&mut session, &calls[2..], &mut ui).is_empty());
        assert!(run_batch(&mut session, &calls[3..], &mut ui).is_empty());
        // Nor is a `memory` call that saves, or a tool outside the list
        // that happens to change nothing.
        let saving = [
            call(0, "memory", json!({"action": "save"})),
            call(1, "memory", json!({"action": "list"})),
        ];
        assert!(run_batch(&mut session, &saving, &mut ui).is_empty());
        assert!(session.runs_together("memory", r#"{"action": "list"}"#));
        assert!(!session.runs_together("bash_kill", r#"{"id": "job-1"}"#));
        assert!(!session.runs_together("todo", "{}"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn calls_of_a_batch_are_in_flight_at_the_same_time() {
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(Meet {
            together: 3,
            inside: Arc::default(),
        }));
        let (mut session, root) = session("together", tools);
        let calls = [
            call(0, "web_fetch", json!({"url": "one"})),
            call(1, "web_fetch", json!({"url": "two"})),
            call(2, "web_fetch", json!({"url": "three"})),
        ];
        let mut ui = Rec::default();
        // Each call answers only when all three are inside the tool.
        let results = run_batch(&mut session, &calls, &mut ui);
        assert_eq!(results, ["fetched one", "fetched two", "fetched three"]);
        // Asked about one at a time, in order, before any of them ran.
        assert_eq!(
            ui.events,
            [
                "ask web_fetch",
                r#"start web_fetch {"url":"one"}"#,
                "ask web_fetch",
                r#"start web_fetch {"url":"two"}"#,
                "ask web_fetch",
                r#"start web_fetch {"url":"three"}"#,
                "end web_fetch true fetched one",
                "end web_fetch true fetched two",
                "end web_fetch true fetched three",
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_denied_call_has_its_denial_and_the_others_still_run_together() {
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(Meet {
            together: 2,
            inside: Arc::default(),
        }));
        let (mut session, root) = session("denied", tools);
        let calls = [
            call(0, "web_fetch", json!({"url": "one"})),
            call(1, "web_fetch", json!({"url": "two"})),
            call(2, "web_fetch", json!({"url": "three"})),
        ];
        let mut ui = Rec {
            answers: [true, false, true].into(),
            ..Rec::default()
        };
        let results = run_batch(&mut session, &calls, &mut ui);
        assert_eq!(results, ["fetched one", DENIED, "fetched three"]);
        assert_eq!(
            ui.events,
            [
                "ask web_fetch",
                r#"start web_fetch {"url":"one"}"#,
                "ask web_fetch",
                &format!("end web_fetch false {DENIED}"),
                "ask web_fetch",
                r#"start web_fetch {"url":"three"}"#,
                "end web_fetch true fetched one",
                "end web_fetch true fetched three",
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn nested_instructions_come_once_when_two_calls_enter_a_directory_together() {
        let (mut session, root) = session("nested", builtin_registry());
        std::fs::create_dir_all(root.join("web")).unwrap();
        std::fs::write(root.join("web/AGENTS.md"), "WEB rules").unwrap();
        std::fs::write(root.join("web/a.txt"), "file a").unwrap();
        std::fs::write(root.join("web/b.txt"), "file b").unwrap();
        let calls = [
            call(0, "read", json!({"path": "web/a.txt"})),
            call(1, "read", json!({"path": "web/b.txt"})),
            call(2, "read", json!({"path": "web/missing.txt"})),
        ];
        let mut ui = Rec::default();
        let results = run_batch(&mut session, &calls, &mut ui);
        assert_eq!(results.len(), 3);
        assert!(results[0].contains("file a") && results[1].contains("file b"));
        // With the first call in call order, whichever read first.
        assert!(results[0].contains("<nested-instructions>") && results[0].contains("WEB rules"));
        assert!(!results[1].contains("WEB rules"), "{}", results[1]);
        assert!(results[2].starts_with("error:"), "{}", results[2]);
        assert!(ui.events.last().unwrap().starts_with("end read false"));

        // And not again for a later call, batched or not.
        let results = run_batch(&mut session, &calls[..2], &mut ui);
        assert!(!results.concat().contains("WEB rules"));
        let (out, _) =
            session
                .runner()
                .exec_json("read", r#"{"path": "web/a.txt"}"#, &mut Rec::default());
        assert!(!out.text.contains("WEB rules"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_stop_skips_the_calls_not_yet_started_and_lets_the_running_ones_finish() {
        let (started, open) = (Arc::<Gate>::default(), Arc::<Gate>::default());
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(Hold {
            started: Arc::clone(&started),
            open: Arc::clone(&open),
        }));
        let (mut session, root) = session("stop", tools);
        let calls: Vec<ToolCall> = (0..MAX_THREADS + 2)
            .map(|n| call(n, "glob", json!({"n": n})))
            .collect();
        // The stop comes once every thread is inside a call.
        let mut ui = Rec {
            stop_at: Some((MAX_THREADS, Arc::clone(&started), Arc::clone(&open))),
            ..Rec::default()
        };
        let results = run_batch(&mut session, &calls, &mut ui);
        assert_eq!(results.len(), MAX_THREADS + 2);
        for (n, result) in results.iter().enumerate() {
            if n < MAX_THREADS {
                assert_eq!(result, &format!("held {n}"));
            } else {
                assert_eq!(result, NOT_RUN);
            }
        }
        assert_eq!(started.get(), MAX_THREADS);
        // Every call that was announced has its end.
        let ends = ui.events.iter().filter(|e| e.starts_with("end ")).count();
        assert_eq!(ends, MAX_THREADS + 2);

        // Stopped before the batch: nothing is asked, announced or run.
        let mut ui = Rec {
            stop_at: Some((0, Arc::clone(&started), Arc::clone(&open))),
            ..Rec::default()
        };
        let results = run_batch(&mut session, &calls[..2], &mut ui);
        assert_eq!(results, [NOT_RUN, NOT_RUN]);
        assert!(ui.events.is_empty(), "{:?}", ui.events);
        assert_eq!(started.get(), MAX_THREADS);
        let _ = std::fs::remove_dir_all(&root);
    }
}
