//! Subagents: running `task` calls.
//!
//! A subagent is a second [`Session`] — a fresh conversation on the same
//! loop — built from its parent for one prompt and dropped when it has
//! answered. What it gets, and from where:
//!
//! - **its own**: history (it sees nothing of the parent's conversation),
//!   [`FileState`](crate::tools::FileState), step count, and a system prompt
//!   that is the agent's (see [`crate::agents`]) instead of atelier's;
//! - **a subset of the parent's tools**: the same tool objects, filtered to
//!   what the agent's definition allows — never `task`, so the depth is one;
//! - **copies** of the parent's configuration (endpoint, model unless the
//!   agent names one, a ChatGPT sign-in) and project root;
//! - **standing context only**: the project's instruction files, not the
//!   per-turn snapshot (git status, diagnostics) the parent's prompt carries;
//! - **nothing persisted**: it is never saved as the project's session, and
//!   the reviewer does not watch it.
//!
//! # Approvals stay with the parent
//!
//! A child must not be able to do what its parent could not. Rather than
//! give it a copy of the permission rules (which "always" answers would then
//! update in the copy), a child decides nothing: every call it wants to make
//! is sent to the parent's thread ([`Link::approve`]), which applies the
//! parent's rules and settings and, when they say to ask, prompts the user
//! through the one real [`Ui`] — after a line naming the agent that asks.
//!
//! # Threads
//!
//! The `task` calls that follow one another in a model step run at the same
//! time, each child on its own scoped thread. The parent's thread — the only
//! one that touches the `Ui` — waits on a channel for what the children have
//! to say ([`Event`]): activity to print, a call to approve, a result. That
//! serializes the prompts of concurrent children without a lock, and keeps
//! their output to whole lines. Stopping the turn (Ctrl-C) is passed down
//! through a flag the children poll as their own `interrupted`.
//!
//! # What is shown
//!
//! One line when a child starts (`⤷ explore: <description>`), its tool calls
//! as dim one-liners while it runs, one line when it ends. Never its text or
//! reasoning: its answer is the tool result, for the parent model to relay.
//!
//! # In the background
//!
//! A `task` call with `background: true` does not wait: the child gets a
//! thread that outlives the turn, an id, and a place in the session's
//! [`Roster`]; the call's result is the id. Everything a child's thread
//! holds is its own — a [`Session`] built from clones, a [`Link`] — so the
//! only thing that changes is who is at the other end of the link: not the
//! parent's thread waiting on a channel, but the roster, with the permission
//! rules deciding its calls where a foreground child's are put to the user.
//! See the `background` module for that and for how its report comes back.
//!
//! Not here yet (see ROADMAP.md, M18): worktree isolation, and running a
//! child as another atelier instance through the coordinator.

mod background;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

use serde_json::Value;

use super::{Approval, Session, Ui};
use crate::agents::{self, Agent, TASK_TOOL};
use crate::context;
use crate::permissions::DENIED;
use crate::provider::ToolCall;
use crate::tools::task::TaskCall;

pub(super) use background::{Background, NOTICE_OPEN, Roster};
use background::{Rules, Seat};

/// Added to every agent's system prompt: what being a subagent means.
const SUBAGENT_NOTE: &str = "\
You are running as a subagent of atelier, an AI coding assistant, inside the \
project directory. Another agent gave you the task below and sees nothing of \
your work except your final message — so when you are done, stop calling \
tools and answer with a complete, self-contained report.";

/// How often the parent looks for a stop request while children run.
const POLL: Duration = Duration::from_millis(100);

/// Longest tool-call summary shown for a child's activity.
const MAX_ACTIVITY_CHARS: usize = 100;

/// What makes a [`Session`] a subagent's.
pub(super) struct Child {
    /// The system prompt, in place of atelier's own.
    pub(super) system: String,
    pub(super) link: Link,
}

/// A child's line to the session that started it.
#[derive(Clone)]
pub(super) struct Link {
    /// Set when the child is to stop: the user stopped the parent's turn,
    /// or — in the background — this agent.
    stop: Arc<AtomicBool>,
    to: Parent,
}

/// Who is at the other end of a [`Link`].
#[derive(Clone)]
enum Parent {
    /// The parent's thread, waiting in [`Session::run_tasks`] for what this
    /// child has to say.
    Waiting {
        /// Which of the calls running together this child is.
        index: usize,
        events: Sender<Event>,
    },
    /// Nobody is waiting: the child runs in the background. What it does
    /// and how it ends go to its seat in the roster; its calls are decided
    /// by the rules alone.
    Detached { seat: Arc<Seat>, rules: Arc<Rules> },
}

impl Link {
    /// Whether this child may make a call. In the foreground the parent
    /// decides — by its rules, asking its user; in the background the rules
    /// decide alone (see `background`). `Err` is the tool result for a
    /// refused call.
    pub(super) fn approve(&self, tool: &str, arguments: &str) -> Result<(), String> {
        let (index, events) = match &self.to {
            Parent::Waiting { index, events } => (*index, events),
            Parent::Detached { rules, .. } => return rules.decide(tool, arguments),
        };
        let (reply, verdict) = mpsc::channel();
        let asked = events.send(Event::Approve {
            index,
            tool: tool.to_string(),
            arguments: arguments.to_string(),
            reply,
        });
        // A parent that is gone cannot approve anything.
        match (asked, verdict.recv()) {
            (Ok(()), Ok(verdict)) => verdict,
            _ => Err(DENIED.to_string()),
        }
    }

    /// A line of the child's progress.
    fn activity(&self, text: String) {
        match &self.to {
            Parent::Waiting { index, events } => {
                let _ = events.send(Event::Activity {
                    index: *index,
                    text,
                });
            }
            Parent::Detached { seat, .. } => seat.activity(text),
        }
    }

    /// The child is finished. Always the last thing it says.
    fn done(&self, outcome: Outcome) {
        match &self.to {
            Parent::Waiting { index, events } => {
                let _ = events.send(Event::Done {
                    index: *index,
                    outcome,
                });
            }
            Parent::Detached { seat, .. } => seat.finish(outcome),
        }
    }

    /// Whether the child was told to stop and runs in the background —
    /// where that is an end of its own, with a report saying so. (A
    /// foreground child stopped with its parent's turn just ends: the turn
    /// it would report to is over.)
    fn stopped_in_background(&self) -> bool {
        matches!(self.to, Parent::Detached { .. }) && self.stop.load(Ordering::Relaxed)
    }
}

/// What a child tells its parent's thread.
enum Event {
    /// A line of progress to show.
    Activity { index: usize, text: String },
    /// A call the child wants to make; the answer goes back through `reply`.
    Approve {
        index: usize,
        tool: String,
        arguments: String,
        reply: Sender<Result<(), String>>,
    },
    /// The child is finished. Always the last event it sends.
    Done { index: usize, outcome: Outcome },
}

/// How a child ended.
struct Outcome {
    status: Status,
    /// Model requests it made.
    steps: u32,
    /// Its report: the tool result for the parent model, or — from the
    /// background — the body of its notice.
    text: String,
    /// The line shown to the user.
    line: String,
    /// Output tokens the child generated, for the session's count.
    tokens_out: u64,
}

impl Outcome {
    /// The end of a child whose thread died without a word.
    fn crashed(label: &str) -> Self {
        Self {
            status: Status::Failed,
            steps: 0,
            text: CRASHED.to_string(),
            line: format!("⤶ {label}: crashed"),
            tokens_out: 0,
        }
    }
}

/// The report of a child that panicked.
const CRASHED: &str = "error: the subagent crashed";

/// The ways a child ends.
#[derive(Clone, Copy, PartialEq)]
enum Status {
    /// It answered.
    Done,
    /// It used up its steps, and its answer is the wrap-up it was asked for.
    WrappedUp,
    /// Its conversation failed (the endpoint errored), or its thread died.
    Failed,
    /// It was stopped, in the background, before it answered.
    Stopped,
}

impl Status {
    /// As a background agent's notice states it.
    fn word(self) -> &'static str {
        match self {
            Status::Done => "done",
            Status::WrappedUp => "wrapped-up",
            Status::Failed => "failed",
            Status::Stopped => "stopped",
        }
    }
}

/// Where one call of a batch stands.
enum Slot {
    /// Has its result already, and its `tool_end` was shown: never started
    /// (refused by a rule or the user, malformed, over a limit), or started
    /// in the background, where the result is the agent's id. Holds the
    /// result.
    Settled(String),
    Running,
    /// The child's tool result, and whether it succeeded.
    Done(String, bool),
}

/// One child ready to run.
struct Job {
    /// How it is named on screen: the agent's name — numbered when several
    /// run together, followed by its id in the background.
    label: String,
    agent: Agent,
    prompt: String,
    session: Session,
    link: Link,
}

impl Job {
    /// Run the child to its answer and report it. On the child's thread.
    fn run(mut self) {
        let mut ui = ChildUi {
            link: self.link.clone(),
        };
        let result = self.session.send(&self.prompt, &mut ui);
        let count = self.session.steps();
        let steps = match count {
            1 => "1 step".to_string(),
            n => format!("{n} steps"),
        };
        let (_, tokens_out) = self.session.usage();
        // One request past the cap is the wrap-up (see `Session::run_turn`).
        let capped = count > self.agent.max_steps;
        let label = &self.label;
        let mut footer = format!("[{label}: {steps}");
        if tokens_out > 0 {
            footer.push_str(&format!(" · {tokens_out} output tokens"));
        }
        if capped {
            footer.push_str(&format!(
                " · stopped at its cap of {} steps; this is its wrap-up report",
                self.agent.max_steps
            ));
        }
        footer.push(']');
        let last = self.session.last_answer();
        let outcome = match result {
            // Told to stop, it ended without the report it was asked for.
            Ok(()) if self.link.stopped_in_background() => {
                let said = match last.trim() {
                    "" => "It had said nothing yet.".to_string(),
                    text => format!("The last thing it said:\n\n{text}"),
                };
                Outcome {
                    status: Status::Stopped,
                    steps: count,
                    text: format!(
                        "(The agent was stopped before it finished; this is not a report.) \
                         {said}\n\n{footer}"
                    ),
                    line: format!("⤶ {label}: stopped ({steps})"),
                    tokens_out,
                }
            }
            Ok(()) => {
                let answer = match last.trim() {
                    "" => "(the subagent ended without a final message)",
                    text => text,
                };
                Outcome {
                    status: if capped {
                        Status::WrappedUp
                    } else {
                        Status::Done
                    },
                    steps: count,
                    text: format!("{answer}\n\n{footer}"),
                    line: if capped {
                        format!("⤶ {label}: done ({steps} — wrapped up at its step cap)")
                    } else {
                        format!("⤶ {label}: done ({steps})")
                    },
                    tokens_out,
                }
            }
            Err(e) => Outcome {
                status: Status::Failed,
                steps: count,
                text: format!("error: the subagent failed: {e:#}\n\n{footer}"),
                line: format!("⤶ {label}: failed after {steps} — {e:#}"),
                tokens_out,
            },
        };
        self.link.done(outcome);
    }
}

impl Session {
    /// Whether `call` is a `task` call this session runs. A subagent's
    /// session has no `task` tool, so a child asking for one anyway gets
    /// "unknown tool" like any other name — it cannot start a grandchild.
    pub(super) fn runs_task(&self, call: &ToolCall) -> bool {
        call.name == TASK_TOOL && self.tools.get(TASK_TOOL).is_some()
    }

    /// Run the `task` calls that open `calls` — all of them up to the first
    /// call of another tool — at the same time, and return their tool
    /// results in call order.
    pub(super) fn run_tasks(&mut self, calls: &[ToolCall], ui: &mut dyn Ui) -> VecDeque<String> {
        let batch = calls.iter().take_while(|c| self.runs_task(c)).count();
        let found = agents::discover(&self.root, agents::global_dir().as_deref());
        let (events, inbox) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));

        // In call order: approve each (a permission rule may refuse `task`
        // or ask first) and build its child. A refused or malformed call
        // gets its result here and never starts.
        let mut slots: Vec<Slot> = Vec::with_capacity(batch);
        let mut jobs = Vec::new();
        for (index, call) in calls[..batch].iter().enumerate() {
            let checked = self
                .runner()
                .approve(&call.name, &call.arguments, ui)
                .and_then(|()| TaskCall::parse(&call.arguments, &found));
            match checked {
                // Started and left: the call's result is the agent's id.
                Ok(task) if task.background => {
                    ui.tool_start(&call.name, &call.arguments);
                    let started = self.start_background(task, ui);
                    let ok = started.is_ok();
                    let text = started.unwrap_or_else(|refusal| refusal);
                    ui.tool_end(&call.name, &text, ok);
                    slots.push(Slot::Settled(text));
                }
                Ok(task) => {
                    ui.tool_start(&call.name, &call.arguments);
                    let link = Link {
                        stop: Arc::clone(&stop),
                        to: Parent::Waiting {
                            index,
                            events: events.clone(),
                        },
                    };
                    jobs.push((index, self.child(&task.agent, link.clone()), task, link));
                    slots.push(Slot::Running);
                }
                Err(msg) => {
                    ui.tool_end(&call.name, &msg, false);
                    slots.push(Slot::Settled(msg));
                }
            }
        }
        // The children hold the only senders, so the channel closes when
        // the last of them is done.
        drop(events);

        let several = jobs.len() > 1;
        let mut labels = vec![String::new(); batch];
        let jobs: Vec<Job> = jobs
            .into_iter()
            .enumerate()
            .map(|(n, (index, session, task, link))| {
                let label = if several {
                    format!("{}#{}", task.agent.name, n + 1)
                } else {
                    task.agent.name.clone()
                };
                ui.info(&format!("⤷ {label}: {}", task.description));
                labels[index] = label.clone();
                Job {
                    label,
                    agent: task.agent,
                    prompt: task.prompt,
                    session,
                    link,
                }
            })
            .collect();

        std::thread::scope(|scope| {
            let threads: Vec<_> = jobs
                .into_iter()
                .map(|job| scope.spawn(move || job.run()))
                .collect();
            loop {
                match inbox.recv_timeout(POLL) {
                    Ok(Event::Activity { index, text }) => {
                        // Alone, a child needs no name on every line.
                        if several {
                            ui.activity(&format!("  ⤷ {} · {text}", labels[index]));
                        } else {
                            ui.activity(&format!("  ⤷ {text}"));
                        }
                    }
                    Ok(Event::Approve {
                        index,
                        tool,
                        arguments,
                        reply,
                    }) => {
                        let mut asking = AskingFor {
                            inner: &mut *ui,
                            label: &labels[index],
                        };
                        let verdict = self.runner().approve(&tool, &arguments, &mut asking);
                        let _ = reply.send(verdict);
                    }
                    Ok(Event::Done { index, outcome }) => {
                        ui.info(&outcome.line);
                        self.usage_out += outcome.tokens_out;
                        let ok = outcome.status != Status::Failed;
                        slots[index] = Slot::Done(outcome.text, ok);
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
                if ui.interrupted() {
                    stop.store(true, Ordering::Relaxed);
                }
            }
            // Joined by hand so a child that panicked costs its own result
            // (below), not the whole turn.
            for thread in threads {
                let _ = thread.join();
            }
        });

        slots
            .into_iter()
            .zip(&calls[..batch])
            .map(|(slot, call)| {
                let (text, ok) = match slot {
                    // Its `tool_end` was shown when it got its result.
                    Slot::Settled(text) => return text,
                    Slot::Done(text, ok) => (text, ok),
                    // Its thread ended without a word: it panicked.
                    Slot::Running => (CRASHED.to_string(), false),
                };
                ui.tool_end(&call.name, &text, ok);
                text
            })
            .collect()
    }

    /// Start `task` in the background: a child on a thread of its own, which
    /// this turn does not wait for. `Ok` is the call's result — the agent's
    /// id and what to expect — and `Err` the reason it was not started.
    fn start_background(&mut self, task: TaskCall, ui: &mut dyn Ui) -> Result<String, String> {
        let max = self.settings.agents.max_background;
        let roster = self.background.roster();
        let seat = roster.start(
            max.unwrap_or(background::DEFAULT_MAX),
            &task.agent.name,
            &task.description,
        )?;
        let seat = Arc::new(seat);
        let (id, name) = (seat.id(), task.agent.name.clone());
        // Nobody will be there to ask: its calls are decided by the rules
        // and the mode as they are now (see `background`).
        let rules = Rules {
            // Only the rules in effect: an untrusted project's own allow
            // rules don't count (see `trust`), least of all for an agent
            // that runs with nobody to ask.
            permissions: self.trust.permissions(&self.settings.permissions),
            mode: self.mode,
            root: self.root.clone(),
            tools: self.tools.filtered(|tool| task.agent.allows(tool)),
        };
        let link = Link {
            stop: seat.stop_flag(),
            to: Parent::Detached {
                seat: Arc::clone(&seat),
                rules: Arc::new(rules),
            },
        };
        let job = Job {
            label: format!("{name} {id}"),
            session: self.child(&task.agent, link.clone()),
            agent: task.agent,
            prompt: task.prompt,
            link,
        };
        let thread = std::thread::Builder::new().name(id.clone());
        if let Err(e) = thread.spawn(move || job.run()) {
            seat.withdraw();
            return Err(format!("error: could not start the agent's thread: {e}"));
        }
        ui.info(&format!(
            "⤷ {name} {id}: {} (in the background)",
            task.description
        ));
        Ok(format!(
            "started background agent {id} ({name}). It works on its own from here: you will \
             be notified when it ends, and its report arrives then as a <subagent> block. Do \
             not wait for it and do not poll — go on with other work, or end your turn if \
             there is nothing else to do."
        ))
    }

    /// `/agents`: the agents that can be run and the ones running in the
    /// background; `/agents stop <id>` stops one of those.
    pub(super) fn agents_command(&mut self, args: &[&str], ui: &mut dyn Ui) {
        let roster = self.background.roster();
        match args {
            [] => {
                if self.is_claude() {
                    ui.info(
                        "Claude backend: claude runs subagents of its own; these are not offered to it.",
                    );
                }
                let global = agents::global_dir();
                for line in agents::command(&self.root, global.as_deref()) {
                    ui.info(&line);
                }
                let running = roster.listing();
                if !running.is_empty() {
                    ui.info("in the background (/agents stop <id> stops one):");
                }
                for line in running {
                    ui.info(&line);
                }
            }
            ["stop", id] => ui.info(&roster.stop(id)),
            _ => ui.info("usage: /agents [stop <id>]"),
        }
    }

    /// How many background agents are running.
    pub(super) fn background_agents(&self) -> usize {
        self.background.roster().running()
    }

    /// The session's background agents, for a turn (or an idle look) that
    /// is about to collect their reports. Whoever asks is the session
    /// looking, so the output tokens of the agents that ended join its
    /// count here.
    pub(super) fn roster(&mut self) -> Roster {
        let roster = self.background.roster();
        self.usage_out += roster.take_tokens_out();
        roster
    }

    /// A session for one run of `agent`, reporting through `link`. See the
    /// module docs for what it shares with this one.
    fn child(&self, agent: &Agent, link: Link) -> Session {
        let tools = self.tools.filtered(|name| agent.allows(name));
        let mut child = Session::new(
            self.cfg.clone(),
            self.root.clone(),
            tools,
            context::standing_providers(context::InstructionFiles::with_user(&self.settings)),
            self.settings.clone(),
            // The same decision as its parent's, not another reading of it.
            self.trust.clone(),
        );
        child.chatgpt = self.chatgpt.clone();
        // The agent's own `model:` wins; else `[models] subagent`, if set;
        // else the parent's model, which the cloned config already names.
        let role = self.cfg.own_model(crate::config::Role::Subagent);
        if let Some(model) = agent.model.as_deref().or(role) {
            child.set_model(model);
        }
        child.review_enabled = false;
        child.max_steps = Some(agent.max_steps);
        child.child = Some(Child {
            system: format!("{}\n\n{SUBAGENT_NOTE}", agent.prompt),
            link,
        });
        child
    }
}

/// The [`Ui`] a child's loop reports to: its tool calls become activity
/// lines on the parent's thread; its text and reasoning go nowhere.
struct ChildUi {
    link: Link,
}

impl ChildUi {
    fn report(&self, text: String) {
        self.link.activity(text);
    }
}

impl Ui for ChildUi {
    fn reasoning(&mut self, _text: &str) {}
    fn content(&mut self, _text: &str) {}
    fn tool_start(&mut self, name: &str, arguments: &str) {
        self.report(summarize(name, arguments));
    }
    fn tool_end(&mut self, name: &str, result: &str, ok: bool) {
        // A call's result is the child's business; that one was refused or
        // failed is worth a line.
        if !ok {
            let first = result.lines().next().unwrap_or("");
            self.report(clip(&format!("✗ {name}: {first}")));
        }
    }
    fn info(&mut self, text: &str) {
        self.report(text.to_string());
    }
    fn ask_approval(&mut self, _tool: &str, _arguments: &str) -> Approval {
        // Not reached: a child's calls are decided through its link
        // (`Link::approve`) — on the parent's thread, which is where the
        // user is asked, or by the rules alone. Should a prompt ever come
        // this way, nobody is here to answer it.
        Approval::Deny
    }
    fn notice(&mut self, text: &str) {
        self.report(format!("! {text}"));
    }
    fn interrupted(&mut self) -> bool {
        self.link.stop.load(Ordering::Relaxed)
    }
}

/// The parent's [`Ui`] while it decides a child's call: the same prompt,
/// after a line saying which agent wants to run the tool.
struct AskingFor<'a> {
    inner: &'a mut dyn Ui,
    label: &'a str,
}

impl Ui for AskingFor<'_> {
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
    fn info(&mut self, text: &str) {
        self.inner.info(text);
    }
    fn ask_approval(&mut self, tool: &str, arguments: &str) -> Approval {
        self.inner
            .info(&format!("⤷ the subagent {} asks to run:", self.label));
        self.inner.ask_approval(tool, arguments)
    }
    fn notice(&mut self, text: &str) {
        self.inner.notice(text);
    }
}

/// A tool call as one short line: the tool and the argument that says most
/// about it (`read src/x.rs`).
fn summarize(name: &str, arguments: &str) -> String {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let subject = [
        "command",
        "pattern",
        "query",
        "url",
        "path",
        "file_path",
        "name",
    ]
    .iter()
    .find_map(|k| args.get(*k).and_then(Value::as_str));
    match subject {
        Some(subject) => clip(&format!("{name} {subject}")),
        None => name.to_string(),
    }
}

/// `text` as a single line of at most [`MAX_ACTIVITY_CHARS`].
fn clip(text: &str) -> String {
    let line = text.lines().next().unwrap_or("");
    if line.chars().count() > MAX_ACTIVITY_CHARS || line.len() < text.trim_end().len() {
        let cut: String = line.chars().take(MAX_ACTIVITY_CHARS).collect();
        format!("{cut}…")
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::agent::{Dispatch, dispatch};
    use crate::config::Config;
    use crate::settings::Settings;
    use crate::tools::{FileState, ToolCtx, builtin_registry};

    /// A [`Ui`] that keeps the lines it is shown.
    #[derive(Default)]
    struct Lines(Vec<String>);

    impl Ui for Lines {
        fn reasoning(&mut self, _: &str) {}
        fn content(&mut self, _: &str) {}
        fn tool_start(&mut self, _: &str, _: &str) {}
        fn tool_end(&mut self, _: &str, _: &str, _: bool) {}
        fn info(&mut self, text: &str) {
            self.0.push(text.to_string());
        }
        fn ask_approval(&mut self, _: &str, _: &str) -> Approval {
            Approval::Deny
        }
        fn notice(&mut self, _: &str) {}
    }

    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("atl-subagent-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn session(root: &std::path::Path) -> Session {
        Session::new(
            Config::from_env(false),
            root.to_path_buf(),
            builtin_registry(),
            Vec::new(),
            Settings::default(),
            crate::trust::testing::open(root),
        )
    }

    fn done(text: &str) -> Outcome {
        Outcome {
            status: Status::Done,
            steps: 3,
            text: text.to_string(),
            line: "⤶ explore agent-1: done (3 steps)".to_string(),
            tokens_out: 40,
        }
    }

    /// The session's side of a background agent, from its start to its
    /// report: listed and stoppable while it runs, a reason to confirm
    /// quitting, then one line for the user at the idle look and the block
    /// in front of the next prompt — and its tokens in the session's count.
    #[test]
    fn a_session_lists_stops_and_collects_its_background_agents() {
        let root = scratch("lifecycle");
        let mut session = session(&root);
        let mut ui = Lines::default();
        assert_eq!(session.background_agents(), 0);
        assert!(session.confirm_quit(&mut ui));

        let roster = session.background.roster();
        let seat = roster.start(4, "explore", "find the parser").unwrap();
        assert_eq!(session.background_agents(), 1);

        // Quitting: a warning first, then it goes ahead.
        assert!(!session.confirm_quit(&mut ui));
        assert_eq!(
            ui.0,
            [
                "1 background agent(s) are running; quitting would stop them.",
                "quit again to quit anyway"
            ]
        );
        assert!(session.confirm_quit(&mut ui));

        // `/agents` lists it after the agents that can be run.
        ui.0.clear();
        assert!(matches!(
            dispatch(&mut session, "/agents", &mut ui),
            Dispatch::Handled
        ));
        let at = ui.0.iter().position(|l| l.starts_with("in the background"));
        let at = at.expect("a section for the running agents");
        assert!(
            ui.0[at + 1].starts_with("  agent-1  explore — find the parser · running "),
            "{:?}",
            ui.0
        );

        // `/agents stop` reaches that agent's flag; anything else is usage.
        ui.0.clear();
        dispatch(&mut session, "/agents stop agent-1", &mut ui);
        dispatch(&mut session, "/agents halt", &mut ui);
        assert!(
            ui.0[0].starts_with("stopping agent-1 (explore)"),
            "{:?}",
            ui.0
        );
        assert_eq!(ui.0[1], "usage: /agents [stop <id>]");
        assert!(seat.stop_flag().load(Ordering::Relaxed));

        // It ends while the session is idle: the user is shown its line at
        // the next look, once, and nothing is left to confirm at a quit.
        seat.finish(done("REPORT"));
        ui.0.clear();
        let (_, before) = session.usage();
        assert!(session.poll_peers(&mut ui));
        assert_eq!(ui.0, ["⤶ explore agent-1: done (3 steps)"]);
        assert_eq!(session.usage().1, before + 40);
        assert!(!session.poll_peers(&mut ui));
        assert!(session.confirm_quit(&mut ui));

        // The model has not had the block: it leads the next prompt.
        ui.0.clear();
        let agents = session.roster();
        let prompt = crate::agent::jobs::lead(&agents, "what now?", &mut ui);
        assert!(
            prompt.starts_with("<subagent id=\"agent-1\" agent=\"explore\" status=\"done\""),
            "{prompt}"
        );
        assert!(prompt.ends_with("\n\nwhat now?"), "{prompt}");
        // The user saw the line already; and the block goes out once.
        assert!(ui.0.is_empty(), "{:?}", ui.0);
        assert_eq!(crate::agent::jobs::lead(&agents, "again", &mut ui), "again");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Dropping the session stops the agents it started.
    #[test]
    fn a_dropped_session_stops_its_background_agents() {
        let root = scratch("drop");
        let session = session(&root);
        let seat = session
            .background
            .roster()
            .start(4, "general", "long work")
            .unwrap();
        let stop = seat.stop_flag();
        // An agent that ends as soon as it is told to.
        let agent = std::thread::spawn(move || {
            while !seat.stop_flag().load(Ordering::Relaxed) {
                std::thread::yield_now();
            }
            seat.finish(done("partial"));
        });
        drop(session);
        assert!(stop.load(Ordering::Relaxed));
        agent.join().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A child has its own read tracking, so two conversations editing one
    /// file collide where the docs say they do: the parent's edit of a file
    /// a child changed after the parent read it is rejected as stale, not
    /// applied over the child's change.
    #[test]
    fn a_parents_edit_of_a_file_a_child_changed_since_is_rejected_as_stale() {
        let root = scratch("stale");
        std::fs::write(root.join("f.txt"), "one\n").unwrap();
        let tools = builtin_registry();
        let (mut parent, mut child) = (FileState::new(), FileState::new());
        let call = |fstate: &mut FileState, tool: &str, args: Value| {
            let mut ctx = ToolCtx {
                project_root: &root,
                fstate,
                approved: true,
            };
            tools.get(tool).unwrap().call(&mut ctx, args)
        };
        let read = serde_json::json!({"path": "f.txt"});
        let edit = |old: &str, new: &str| serde_json::json!({"path": "f.txt", "old_string": old, "new_string": new});

        call(&mut parent, "read", read.clone()).unwrap();
        // The child must read before it edits, whatever the parent read.
        let unread = call(&mut child, "edit", edit("one", "two")).unwrap_err();
        assert!(unread.to_string().contains("has not been read"), "{unread}");
        call(&mut child, "read", read.clone()).unwrap();
        call(&mut child, "edit", edit("one", "two")).unwrap();

        let stale = call(&mut parent, "edit", edit("one", "three")).unwrap_err();
        assert!(
            stale
                .to_string()
                .contains("has changed on disk since it was last read; re-read it before editing"),
            "{stale}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).unwrap(),
            "two\n"
        );

        // Re-read, the parent edits what is there now.
        call(&mut parent, "read", read).unwrap();
        call(&mut parent, "edit", edit("two", "three")).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).unwrap(),
            "three\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tool_calls_are_summarized_in_a_line() {
        assert_eq!(summarize("read", r#"{"path":"src/x.rs"}"#), "read src/x.rs");
        assert_eq!(
            summarize("grep", r#"{"pattern":"fn main","path":"src"}"#),
            "grep fn main"
        );
        assert_eq!(
            summarize("bash", r#"{"command":"make\ntest"}"#),
            "bash make…"
        );
        assert_eq!(summarize("tree", "{}"), "tree");
        assert_eq!(summarize("tree", "not json"), "tree");
        let long = format!(r#"{{"path":"{}"}}"#, "a".repeat(300));
        assert_eq!(
            summarize("read", &long).chars().count(),
            MAX_ACTIVITY_CHARS + 1
        );
    }
}
