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
//! Not here yet (see ROADMAP.md, M18): background agents, worktree
//! isolation, and running a child as another atelier instance through the
//! coordinator.

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

/// A child's line to its parent's thread.
#[derive(Clone)]
pub(super) struct Link {
    /// Which of the calls running together this child is.
    index: usize,
    events: Sender<Event>,
    /// Set when the user stopped the parent's turn.
    stop: Arc<AtomicBool>,
}

impl Link {
    /// Have the parent decide whether this child may make a call — by the
    /// parent's rules, asking the parent's user. `Err` is the tool result
    /// for a refused call.
    pub(super) fn approve(&self, tool: &str, arguments: &str) -> Result<(), String> {
        let (reply, verdict) = mpsc::channel();
        let asked = self.events.send(Event::Approve {
            index: self.index,
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
    /// The tool result for the parent model.
    text: String,
    ok: bool,
    /// The line shown to the user.
    line: String,
    /// Output tokens the child generated, for the session's count.
    tokens_out: u64,
}

/// Where one call of a batch stands.
enum Slot {
    /// Never started: refused by a rule or the user, or malformed. Holds
    /// its tool result.
    Refused(String),
    Running,
    /// The child's tool result, and whether it succeeded.
    Done(String, bool),
}

/// One child ready to run.
struct Job {
    index: usize,
    /// How it is named on screen: the agent's name, numbered when several
    /// run together.
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
        let outcome = match result {
            Ok(()) => {
                let answer = self.session.last_answer();
                let answer = match answer.trim() {
                    "" => "(the subagent ended without a final message)",
                    text => text,
                };
                Outcome {
                    text: format!("{answer}\n\n{footer}"),
                    ok: true,
                    line: if capped {
                        format!("⤶ {label}: done ({steps} — wrapped up at its step cap)")
                    } else {
                        format!("⤶ {label}: done ({steps})")
                    },
                    tokens_out,
                }
            }
            Err(e) => Outcome {
                text: format!("error: the subagent failed: {e:#}\n\n{footer}"),
                ok: false,
                line: format!("⤶ {label}: failed after {steps} — {e:#}"),
                tokens_out,
            },
        };
        let _ = self.link.events.send(Event::Done {
            index: self.index,
            outcome,
        });
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
                Ok(task) => {
                    ui.tool_start(&call.name, &call.arguments);
                    let link = Link {
                        index,
                        events: events.clone(),
                        stop: Arc::clone(&stop),
                    };
                    jobs.push((self.child(&task.agent, link.clone()), task, link));
                    slots.push(Slot::Running);
                }
                Err(msg) => {
                    ui.tool_end(&call.name, &msg, false);
                    slots.push(Slot::Refused(msg));
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
            .map(|(n, (session, task, link))| {
                let label = if several {
                    format!("{}#{}", task.agent.name, n + 1)
                } else {
                    task.agent.name.clone()
                };
                ui.info(&format!("⤷ {label}: {}", task.description));
                labels[link.index] = label.clone();
                Job {
                    index: link.index,
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
                        slots[index] = Slot::Done(outcome.text, outcome.ok);
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
                    // Its `tool_end` was shown when it was refused.
                    Slot::Refused(text) => return text,
                    Slot::Done(text, ok) => (text, ok),
                    // Its thread ended without a word: it panicked.
                    Slot::Running => ("error: the subagent crashed".to_string(), false),
                };
                ui.tool_end(&call.name, &text, ok);
                text
            })
            .collect()
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
        let _ = self.link.events.send(Event::Activity {
            index: self.link.index,
            text,
        });
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
        // Not reached: a child's calls are approved on the parent's thread
        // (`Link::approve`), which is where the user is asked. Should a
        // prompt ever come this way, nobody is here to answer it.
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
    use super::*;

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
