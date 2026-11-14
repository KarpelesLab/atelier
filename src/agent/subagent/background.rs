//! Background subagents: the ones a `task` call starts and does not wait for.
//!
//! A `task` call with `background: true` returns at once with an id
//! (`agent-1`); the child runs on a thread of its own and outlives the turn
//! that started it — not the process. This module is what stays behind in
//! the parent session while it runs: the [`Roster`], which knows every
//! background agent of the session, running or just ended.
//!
//! # Who decides its calls
//!
//! A foreground child's calls are put to the user through the parent's
//! thread, which is blocked on the child anyway. A background child has no
//! such moment: the parent's thread is on another step, or idle at the
//! prompt, and a prompt appearing from nowhere would be answered by whoever
//! happened to be typing. So nobody is asked. The permission rules and the
//! mode decide alone ([`Rules`]): what they let through runs, what they
//! refuse is refused, and what they would *ask* about is **denied**, with a
//! result that tells the child why and to report what it needed. The rules
//! and the mode are the parent's as they were when the agent started.
//!
//! # How it reports
//!
//! When the child ends — an answer, a wrap-up at its step cap, an error, or
//! stopped — its report is kept here until the session looks, and then goes
//! the way a finished `bash` job does (see `agent::jobs`): the user sees one
//! line, the model gets a `<subagent …>` block through the queued-message
//! channel or in front of the next prompt. An agent that ends while the
//! session is idle does not start a turn.
//!
//! # Stopping
//!
//! Each agent has a stop flag its loop polls as its `Ui::interrupted`:
//! `/agents stop <id>` sets one, leaving the session sets them all
//! ([`Background`]'s `Drop`). Stopping is cooperative — the agent ends at
//! its next step, after the tool call it is in. Ctrl-C does not reach here:
//! it stops the turn in the foreground, and background agents are not part
//! of it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::Outcome;
use crate::agent::approvals::Detached;
use crate::agent::jobs::{Audience, Notice};
use crate::permissions::{self, Mode, Policy, Shown};
use crate::settings::Permissions;
use crate::tools::ToolRegistry;
use crate::tools::bash::runtime;

/// What opens a background agent's report as the model sees it.
pub(in crate::agent) const NOTICE_OPEN: &str = "<subagent ";

/// Background agents that may run at once when `[agents] max_background`
/// does not say.
pub(in crate::agent) const DEFAULT_MAX: usize = 4;

/// How long leaving the session waits for the agents it stopped to wind
/// down (finish the step they are in, settle a worktree). One that is deep
/// in a long tool call is left behind; the process is going away.
const EXIT_GRACE: Duration = Duration::from_secs(3);

/// The tool result of a call the rules would put to the user.
const NOBODY_TO_ASK: &str = "\
error: not run. This call needs the user's approval, and you are running in \
the background, where there is no user to ask. Do not retry it. Go on \
without it if you can, and say in your final report what you needed to run \
and why, so that the agent that started you can run it or ask the user.";

/// The session's background agents, and their end when the session goes:
/// dropping this stops them (see the module docs).
#[derive(Default)]
pub(in crate::agent) struct Background {
    roster: Roster,
}

impl Background {
    /// A handle on the agents, for whoever needs one past a borrow of the
    /// session.
    pub(in crate::agent) fn roster(&self) -> Roster {
        self.roster.clone()
    }
}

impl Drop for Background {
    fn drop(&mut self) {
        self.roster.stop_all();
        self.roster.wait_idle(EXIT_GRACE);
    }
}

/// The background agents of a session: shared between the session and the
/// agents' own threads.
#[derive(Clone, Default)]
pub(in crate::agent) struct Roster {
    shared: Arc<Shared>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    /// Signalled whenever an agent ends.
    ended: Condvar,
}

#[derive(Default)]
struct State {
    /// The number of the last agent started.
    next: u32,
    /// By number, so they list in the order they were started. An agent
    /// stays until the model has its report.
    agents: BTreeMap<u32, Entry>,
    /// Output tokens of the agents that ended, not yet added to the
    /// session's count.
    tokens_out: u64,
}

struct Entry {
    agent: String,
    description: String,
    started: Instant,
    /// The last thing it did, as its activity line.
    activity: String,
    stop: Arc<AtomicBool>,
    /// Where it works, when that is a worktree of its own.
    worktree: Option<PathBuf>,
    /// How it ended; `None` while it runs.
    ended: Option<Ended>,
}

struct Ended {
    outcome: Outcome,
    /// The user has been shown its line; the model has yet to get the
    /// block. (Once the model has it, the entry is gone.)
    announced: bool,
}

/// A background agent as a listing shows it (see [`Roster::agents`]).
pub(in crate::agent) struct AgentInfo {
    /// `agent-3`.
    pub(in crate::agent) id: String,
    /// The definition it runs (`explore`).
    pub(in crate::agent) agent: String,
    /// What it was asked to do, in the words of the call that started it.
    pub(in crate::agent) description: String,
    /// Since it was started.
    pub(in crate::agent) runtime: Duration,
    /// The last thing it did, as its activity line.
    pub(in crate::agent) activity: String,
    /// Where it works, when that is a worktree of its own.
    pub(in crate::agent) worktree: Option<PathBuf>,
    /// How it ended (`done`, `wrapped-up`, `failed`, `stopped`); `None`
    /// while it runs.
    pub(in crate::agent) ended: Option<&'static str>,
    /// The model requests it made, once it has ended.
    pub(in crate::agent) steps: Option<u32>,
}

/// `agent-3`.
fn id_of(number: u32) -> String {
    format!("agent-{number}")
}

impl Roster {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.shared.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The next free id, for an agent that is not kept here (a foreground
    /// one that needs a name for its worktree). `taken` says which ids are
    /// in use outside this session.
    pub(super) fn next_id(&self, taken: impl Fn(&str) -> bool) -> String {
        id_of(self.lock().claim(taken))
    }

    /// Enter an agent that is about to start, unless `max` are running
    /// already. `Err` is the tool result of the call that asked. `taken`
    /// says which ids are in use outside this session.
    pub(super) fn start(
        &self,
        max: usize,
        agent: &str,
        description: &str,
        taken: impl Fn(&str) -> bool,
    ) -> Result<Seat, String> {
        let mut state = self.lock();
        let running = state.running();
        if running >= max {
            return Err(format!(
                "error: {running} background agents are running already, which is the limit \
                 ([agents] max_background). Wait for one to report (you will be notified), or \
                 run this task in the foreground."
            ));
        }
        let number = state.claim(taken);
        let stop = Arc::new(AtomicBool::new(false));
        state.agents.insert(
            number,
            Entry {
                agent: agent.to_string(),
                description: description.to_string(),
                started: Instant::now(),
                activity: "starting".to_string(),
                stop: Arc::clone(&stop),
                worktree: None,
                ended: None,
            },
        );
        Ok(Seat {
            number,
            roster: self.clone(),
            stop,
        })
    }

    /// How many are running.
    pub(in crate::agent) fn running(&self) -> usize {
        self.lock().running()
    }

    /// The worktrees running agents work in.
    pub(super) fn worktrees(&self) -> Vec<PathBuf> {
        let state = self.lock();
        let running = state.agents.values().filter(|e| e.ended.is_none());
        running.filter_map(|e| e.worktree.clone()).collect()
    }

    /// The agents that ended and that `audience` has not been told about,
    /// each once per audience (as `bash::notices` does for jobs): asking
    /// for the user's leaves them due for the model, asking for the model's
    /// settles them.
    pub(in crate::agent) fn notices(&self, audience: Audience) -> Vec<Notice> {
        let mut state = self.lock();
        let mut notices = Vec::new();
        state.agents.retain(|number, entry| {
            let Some(ended) = &mut entry.ended else {
                return true;
            };
            if audience == Audience::User && ended.announced {
                return true;
            }
            notices.push(Notice {
                line: ended.outcome.line.clone(),
                block: block(&id_of(*number), &entry.agent, &ended.outcome),
                announced: ended.announced,
            });
            ended.announced = true;
            audience == Audience::User
        });
        notices
    }

    /// The output tokens of agents that ended since this was last asked.
    pub(super) fn take_tokens_out(&self) -> u64 {
        std::mem::take(&mut self.lock().tokens_out)
    }

    /// `/agents`: one line per agent that is running, or has ended and not
    /// yet reported to the model. Empty when there is none.
    pub(super) fn listing(&self) -> Vec<String> {
        let lines = self.agents().into_iter().map(|a| {
            let state = match a.ended {
                None => format!("running {} · {}", runtime(a.runtime), a.activity),
                Some(status) => {
                    format!("{status} — its report goes to the model with your next message")
                }
            };
            let (id, agent, what) = (a.id, a.agent, a.description);
            format!("  {id}  {agent} — {what} · {state}")
        });
        lines.collect()
    }

    /// The agents that are running, or have ended and not yet reported to
    /// the model, in the order they were started.
    pub(in crate::agent) fn agents(&self) -> Vec<AgentInfo> {
        let state = self.lock();
        let info = |(number, e): (&u32, &Entry)| AgentInfo {
            id: id_of(*number),
            agent: e.agent.clone(),
            description: e.description.clone(),
            runtime: e.started.elapsed(),
            activity: e.activity.clone(),
            worktree: e.worktree.clone(),
            ended: e.ended.as_ref().map(|ended| ended.outcome.status.word()),
            steps: e.ended.as_ref().map(|ended| ended.outcome.steps),
        };
        state.agents.iter().map(info).collect()
    }

    /// `/agents stop <id>`: ask a running agent to stop. The line to show.
    pub(super) fn stop(&self, id: &str) -> String {
        let state = self.lock();
        let found = state.agents.iter().find(|(n, _)| id_of(**n) == id);
        match found {
            Some((_, entry)) if entry.ended.is_none() => {
                entry.stop.store(true, Ordering::Relaxed);
                format!(
                    "stopping {id} ({}) — it ends at its next step and reports where it got to",
                    entry.agent
                )
            }
            Some(_) => format!("{id} has already ended"),
            None => format!("no background agent {id} — /agents lists the ones running"),
        }
    }

    /// Ask every running agent to stop.
    fn stop_all(&self) {
        for entry in self.lock().agents.values() {
            entry.stop.store(true, Ordering::Relaxed);
        }
    }

    /// Wait until no agent is running, at most `limit`. Whether that came.
    pub(super) fn wait_idle(&self, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        let mut state = self.lock();
        while state.running() > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            let waited = self.shared.ended.wait_timeout(state, left);
            state = waited.unwrap_or_else(|e| e.into_inner()).0;
        }
        true
    }

    /// Record how agent `number` ended — once: whoever says so first.
    fn end(&self, number: u32, outcome: Outcome) {
        let mut state = self.lock();
        let Some(entry) = state.agents.get_mut(&number).filter(|e| e.ended.is_none()) else {
            return;
        };
        let tokens_out = outcome.tokens_out;
        entry.ended = Some(Ended {
            outcome,
            announced: false,
        });
        state.tokens_out += tokens_out;
        drop(state);
        self.shared.ended.notify_all();
    }
}

impl State {
    fn running(&self) -> usize {
        self.agents.values().filter(|e| e.ended.is_none()).count()
    }

    /// The next number whose id is not `taken`.
    fn claim(&mut self, taken: impl Fn(&str) -> bool) -> u32 {
        loop {
            self.next += 1;
            if !taken(&id_of(self.next)) {
                return self.next;
            }
        }
    }
}

/// A background agent's report as the model is given it: the marked block,
/// then a line saying whose message this is — it arrives where the user's
/// own messages do, and must not be taken for one.
fn block(id: &str, agent: &str, outcome: &Outcome) -> String {
    format!(
        "{NOTICE_OPEN}id=\"{id}\" agent=\"{}\" status=\"{}\" steps=\"{}\">\n{}\n</subagent>\n\
         (The block above is a notification from atelier, the harness — not a message from \
         your user: the background agent {id} you started with task has ended, and this is \
         its report. The user has not seen it.)",
        agent.replace('"', "'"),
        outcome.status.word(),
        outcome.steps,
        outcome.text
    )
}

/// A running background agent's place in the [`Roster`], held by the agent:
/// where it says what it is doing and how it ended. An agent whose thread
/// dies without a word (a panic) is ended by the drop of its seat, so it
/// never stays "running".
pub(super) struct Seat {
    number: u32,
    roster: Roster,
    stop: Arc<AtomicBool>,
}

impl Seat {
    pub(super) fn id(&self) -> String {
        id_of(self.number)
    }

    /// The flag that tells this agent to stop.
    pub(super) fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

    /// Note where the agent works, when that is a worktree of its own.
    pub(super) fn set_worktree(&self, dir: PathBuf) {
        if let Some(entry) = self.roster.lock().agents.get_mut(&self.number) {
            entry.worktree = Some(dir);
        }
    }

    /// Note what the agent is doing, for `/agents`.
    pub(super) fn activity(&self, text: String) {
        if let Some(entry) = self.roster.lock().agents.get_mut(&self.number) {
            entry.activity = text;
        }
    }

    /// The agent is done: keep its report for the session to pick up.
    pub(super) fn finish(&self, outcome: Outcome) {
        self.roster.end(self.number, outcome);
    }

    /// The agent never ran (its thread could not be started): it leaves no
    /// report, the call that asked has the error.
    pub(super) fn withdraw(&self) {
        self.roster.lock().agents.remove(&self.number);
    }
}

impl Drop for Seat {
    fn drop(&mut self) {
        // A no-op after `finish` or `withdraw`.
        let label = format!("{} {}", self.agent(), self.id());
        self.roster.end(self.number, Outcome::crashed(&label));
    }
}

impl Seat {
    /// The agent's name, as entered.
    fn agent(&self) -> String {
        let state = self.roster.lock();
        let entry = state.agents.get(&self.number);
        entry.map(|e| e.agent.clone()).unwrap_or_default()
    }
}

/// What decides a background agent's calls: the parent's permission rules
/// and mode as they were when it started, over the agent's own tools and
/// root. See the module docs for why nothing here asks.
pub(super) struct Rules {
    pub(super) permissions: Permissions,
    pub(super) mode: Mode,
    pub(super) root: PathBuf,
    pub(super) tools: ToolRegistry,
    /// The way to the approval reviewer, which — when the rules have one —
    /// judges the calls that would have asked: copies of the parent's
    /// configuration and context, the agent's own to use from its thread.
    pub(super) reviewer: Mutex<Detached>,
}

impl Rules {
    /// Whether the agent may make this call. `Err` is the tool result for a
    /// refused one. `activity` gets the reviewer's verdict, if it gave one,
    /// as a line of the agent's progress.
    pub(super) fn decide(
        &self,
        tool: &str,
        arguments: &str,
        activity: &mut dyn FnMut(String),
    ) -> Result<(), String> {
        let policy = Policy::load(&self.permissions);
        self.decide_under(&policy, tool, arguments, activity)
    }

    /// [`decide`](Self::decide) under a given policy.
    fn decide_under(
        &self,
        policy: &Policy,
        tool: &str,
        arguments: &str,
        activity: &mut dyn FnMut(String),
    ) -> Result<(), String> {
        let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
        let known = self.tools.get(tool);
        let req = permissions::Request {
            tool,
            arguments,
            needs_approval: known.is_some_and(|t| t.requires_approval_in(&args, &self.root)),
            mutates: known.is_some_and(|t| t.mutates(&args)),
            hook_allowed: false,
            hooks: None,
        };
        let mut show = |shown: Shown| match shown {
            Shown::Verdict(record) => activity(record.line()),
            Shown::Notice(text) => activity(text.to_string()),
        };
        let (root, mode) = (&self.root, self.mode);
        // Held for the review: this agent's calls are decided one at a time.
        let mut reviewer = self.reviewer.lock().unwrap_or_else(|e| e.into_inner());
        reviewer.host(|host| {
            let host = Some(host);
            permissions::approve_unattended(
                policy,
                root,
                mode,
                &req,
                NOBODY_TO_ASK,
                host,
                &mut show,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::Status;
    use super::*;

    fn outcome(status: Status, text: &str) -> Outcome {
        Outcome {
            status,
            steps: 7,
            text: text.to_string(),
            line: format!("⤶ line of {text}"),
            tokens_out: 5,
        }
    }

    const WAIT: Duration = Duration::from_secs(10);

    #[test]
    fn an_agent_is_announced_once_to_the_user_and_once_to_the_model() {
        let roster = Roster::default();
        let seat = roster.start(4, "explore", "find it", |_| false).unwrap();
        assert_eq!(seat.id(), "agent-1");
        assert_eq!(roster.running(), 1);
        // Nothing to say while it runs.
        assert!(roster.notices(Audience::User).is_empty());
        assert!(roster.notices(Audience::Model).is_empty());

        seat.activity("read src/x.rs".to_string());
        let listing = roster.listing().join("\n");
        assert!(
            listing.starts_with("  agent-1  explore — find it · running 0s · read src/x.rs"),
            "{listing}"
        );

        seat.finish(outcome(Status::Done, "REPORT"));
        assert_eq!(roster.running(), 0);
        assert_eq!(roster.take_tokens_out(), 5);
        assert_eq!(roster.take_tokens_out(), 0);
        // Ended, and the model does not know yet: still listed.
        assert!(roster.listing()[0].contains("done — its report goes to the model"));

        // Idle: the user is shown the line, once; the model's block waits.
        let shown = roster.notices(Audience::User);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].line, "⤶ line of REPORT");
        assert!(roster.notices(Audience::User).is_empty());

        let told = roster.notices(Audience::Model);
        assert_eq!(told.len(), 1);
        assert!(told[0].announced, "the user has seen its line");
        assert!(
            told[0].block.starts_with(
                "<subagent id=\"agent-1\" agent=\"explore\" status=\"done\" steps=\"7\">\nREPORT\n</subagent>\n"
            ),
            "{}",
            told[0].block
        );
        assert!(told[0].block.contains("not a message from your user"));
        // Once: after that it is gone.
        assert!(roster.notices(Audience::Model).is_empty());
        assert!(roster.listing().is_empty());

        // Mid-turn, the model's notice comes first and is new to the user.
        let seat = roster.start(4, "general", "fix it", |_| false).unwrap();
        assert_eq!(seat.id(), "agent-2");
        seat.finish(outcome(Status::Failed, "why"));
        let told = roster.notices(Audience::Model);
        assert!(!told[0].announced);
        assert!(told[0].block.contains("status=\"failed\""));
        assert!(roster.notices(Audience::User).is_empty());
    }

    #[test]
    fn the_limit_counts_the_running_ones() {
        let roster = Roster::default();
        let first = roster.start(2, "a", "one", |_| false).unwrap();
        let _second = roster.start(2, "a", "two", |_| false).unwrap();
        let refused = roster.start(2, "a", "three", |_| false).err().unwrap();
        assert!(refused.starts_with("error: 2 background agents are running already"));
        assert!(refused.contains("max_background"), "{refused}");
        // One that ended makes room, reported or not.
        first.finish(outcome(Status::Done, "r"));
        let third = roster.start(2, "a", "three", |_| false).unwrap();
        // The refused call used no id; ids taken elsewhere are skipped.
        assert_eq!(third.id(), "agent-3");
        assert_eq!(roster.next_id(|id| id == "agent-4"), "agent-5");
        let sixth = roster.start(9, "a", "six", |id| id == "agent-6").unwrap();
        assert_eq!(sixth.id(), "agent-7");

        // Where the running ones work, for those in a worktree.
        assert!(roster.worktrees().is_empty());
        third.set_worktree(PathBuf::from("/w/agent-3"));
        assert_eq!(roster.worktrees(), [PathBuf::from("/w/agent-3")]);
        third.finish(outcome(Status::Done, "r"));
        assert!(roster.worktrees().is_empty());
    }

    #[test]
    fn stopping_sets_the_agents_own_flag() {
        let roster = Roster::default();
        let one = roster.start(4, "explore", "one", |_| false).unwrap();
        let two = roster.start(4, "explore", "two", |_| false).unwrap();
        assert!(
            roster
                .stop("agent-9")
                .starts_with("no background agent agent-9")
        );
        assert!(
            roster
                .stop("agent-1")
                .starts_with("stopping agent-1 (explore)")
        );
        assert!(one.stop_flag().load(Ordering::Relaxed));
        assert!(!two.stop_flag().load(Ordering::Relaxed));

        one.finish(outcome(Status::Stopped, "partial"));
        assert_eq!(roster.stop("agent-1"), "agent-1 has already ended");
        assert!(
            roster.notices(Audience::Model)[0]
                .block
                .contains("status=\"stopped\"")
        );
        two.finish(outcome(Status::Done, "r"));
    }

    /// Leaving the session stops what runs and waits for it to end.
    #[test]
    fn dropping_the_session_side_stops_the_agents_and_waits_for_them() {
        let background = Background::default();
        let roster = background.roster();
        let seat = roster.start(4, "general", "long", |_| false).unwrap();
        let stop = seat.stop_flag();
        let agent = std::thread::spawn(move || {
            // The agent's loop: until told to stop.
            let deadline = Instant::now() + WAIT;
            while !seat.stop_flag().load(Ordering::Relaxed) {
                assert!(Instant::now() < deadline, "never told to stop");
                std::thread::yield_now();
            }
            seat.finish(outcome(Status::Stopped, "partial"));
        });
        assert!(!stop.load(Ordering::Relaxed));
        drop(background);
        // The drop returned once the agent had ended, not before.
        assert!(stop.load(Ordering::Relaxed));
        assert_eq!(roster.running(), 0, "the drop did not wait");
        agent.join().unwrap();
    }

    /// An agent whose thread dies without reporting is not left "running".
    #[test]
    fn a_seat_dropped_without_a_report_ends_the_agent_as_failed() {
        let roster = Roster::default();
        let seat = roster.start(4, "general", "doomed", |_| false).unwrap();
        let thread = std::thread::spawn(move || {
            let _seat = seat;
            panic!("the agent's thread dies (expected in this test)");
        });
        assert!(thread.join().is_err());
        assert!(roster.wait_idle(WAIT));
        let told = roster.notices(Audience::Model);
        assert_eq!(told.len(), 1);
        assert!(
            told[0].block.contains("status=\"failed\""),
            "{}",
            told[0].block
        );
        assert!(told[0].block.contains("crashed"), "{}", told[0].block);
        assert_eq!(told[0].line, "⤶ general agent-1: crashed");

        // One that could not be started at all leaves nothing.
        let seat = roster.start(4, "general", "never ran", |_| false).unwrap();
        seat.withdraw();
        drop(seat);
        assert!(roster.notices(Audience::Model).is_empty());
        assert_eq!(roster.running(), 0);
    }

    #[test]
    fn rules_alone_decide_and_what_would_ask_is_denied() {
        let root = std::env::temp_dir().join(format!("atl-bg-rules-{}", std::process::id()));
        // Decided under the rules given and no others (not the real user's).
        let decide = |mode: Mode, allow: &[&str], deny: &[&str], tool: &str, arguments: &str| {
            let rules = Rules {
                permissions: Permissions {
                    allow: allow.iter().map(|r| r.to_string()).collect(),
                    ask: Vec::new(),
                    deny: deny.iter().map(|r| r.to_string()).collect(),
                    reviewer: None,
                },
                mode,
                root: root.clone(),
                tools: crate::tools::builtin_registry(),
                reviewer: Mutex::new(Detached {
                    cfg: crate::config::Config::from_env(false),
                    chatgpt: None,
                    context: Default::default(),
                }),
            };
            let policy = Policy::new(&rules.permissions, &Permissions::default(), None);
            rules.decide_under(&policy, tool, arguments, &mut |_| {})
        };
        let bash = r#"{"command": "cargo test"}"#;
        let write = r#"{"path": "a.txt", "content": "x"}"#;

        // Default mode: confined tools run, the shell would ask — denied,
        // with the reason.
        let auto = Mode::AutoEdit;
        assert!(decide(auto, &[], &[], "read", r#"{"path": "a.txt"}"#).is_ok());
        assert!(decide(auto, &[], &[], "write", write).is_ok());
        let denied = decide(auto, &[], &[], "bash", bash).unwrap_err();
        assert!(denied.contains("running in the background"), "{denied}");
        assert!(denied.contains("final report"), "{denied}");

        // An allow rule, or yolo, lets it through.
        let cargo = ["bash(cargo test*)"];
        assert!(decide(auto, &cargo, &[], "bash", bash).is_ok());
        assert!(decide(auto, &cargo, &[], "bash", r#"{"command": "rm -rf x"}"#).is_err());
        assert!(decide(Mode::Yolo, &[], &[], "bash", bash).is_ok());

        // A deny rule and plan mode refuse in their own words.
        let refused = decide(Mode::Yolo, &[], &["bash"], "bash", bash).unwrap_err();
        assert!(
            refused.contains("refused by the permission rule"),
            "{refused}"
        );
        let planning = decide(Mode::Plan, &[], &[], "write", write).unwrap_err();
        assert!(planning.contains("plan mode"), "{planning}");
        // ask-edits would ask before a write.
        let asks = decide(Mode::AskEdits, &[], &[], "write", write).unwrap_err();
        assert!(asks.contains("running in the background"), "{asks}");
    }
}
