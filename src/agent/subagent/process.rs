//! Subagents as processes: a child that is an atelier instance of its own.
//!
//! A `task` call with `process: true` (or an agent defined so) does not run
//! its child on a thread of the parent: it starts another `atelier` — the
//! same executable, as `atelier subagent …` — which joins the coordinator as
//! an instance (see [`crate::coord`]), named after its parent and its agent
//! (`proj.explore`). So it is listed by `peers` like any other, can be
//! written to and asked by its siblings, and holds advisory locks in its own
//! name.
//!
//! # What changes, and what does not
//!
//! Nothing changes for the session that made the call. The child's thread is
//! still there, in [`Job::run`]; it runs [`Process::run`] where it would have
//! run the conversation, and that passes on what the child says — activity,
//! calls to approve, the report — through the very [`Link`] a child on a
//! thread speaks through. Foreground and background, the lines shown, the
//! step cap, the worktree: all of it is the link's other end, and none of it
//! knows where the conversation runs.
//!
//! The conversation itself is the same too. The parent builds the child's
//! session as it does for a thread and describes it to the process — system
//! prompt, model, mode, step cap, the names of the tools it may have — and
//! the process narrows an ordinary session to that (see [`main`]).
//!
//! # The routing
//!
//! Everything said between the two travels as a `call` of the method
//! [`METHOD`] from the child to its parent, each naming the task (an id only
//! the two know) and an `op`:
//!
//! | `op`      | the child…                              | answered with              |
//! |-----------|------------------------------------------|----------------------------|
//! | `take`    | is up, and asks for its task             | the task ([`Process::told`]) |
//! | `say`     | has a line of progress                   | —                          |
//! | `change`  | wrote a file (foreground only)           | —                          |
//! | `approve` | wants to make a tool call                | `{refusal}`: none, or the tool result of the refused call |
//! | `report`  | is done: status, steps, report, tokens   | —                          |
//!
//! The child asks and the parent answers, never the other way round, so no
//! call is held open for the length of the task: a coordinator that restarts
//! in the middle costs a retry, not the child.
//!
//! # Approvals stay with the parent
//!
//! As for a child on a thread, and for the same reason: the process decides
//! nothing. Every call it wants to make is an `approve`, which the parent
//! puts to its own rules, mode and user (in the foreground) or to the rules
//! alone (in the background) — after checking that the tool is one this
//! agent was given, which the parent does not take the child's word for. The
//! mode and the rules the process itself read from disk decide nothing. A
//! parent that cannot be reached approves nothing, and the child stops.
//!
//! # Life and death
//!
//! The coordinator carries what is said; it does not keep the child alive.
//! The child is the parent's process, held by a pipe on its standard input:
//! closing it is how the parent stops the child (Ctrl-C in the foreground,
//! `/agents stop` and quitting in the background), and it closes by itself
//! if the parent dies — so a child never outlives its parent by more than
//! the step it is in, however the parent went. A child told to stop ends at
//! its next step and reports where it got to; one that has not after
//! [`STOP_GRACE`] ends itself, or is killed.
//!
//! # When it cannot be
//!
//! A child that was never started is run on a thread instead, after a line
//! saying why: the session has no coordinator (`ATELIER_NO_COORDINATOR`, a
//! platform without one) or is not connected to it, the executable could not
//! be started, or the process did not come for its task. Nothing was done
//! yet, and where the conversation runs changes nothing about its result. A
//! process that dies *after* it took the task is not run again — it may have
//! changed files — and its `task` call fails, saying so.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child as Spawned, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::{Child, Job, Link, Outcome, Parent, Status};
use crate::agent::Session;
use crate::agents::{Agent, Isolation, Source, TASK_TOOL};
use crate::context;
use crate::coord::client::{Incoming, Responder};
use crate::coord::{self, ask};
use crate::permissions::{DENIED, Mode};
use crate::tools::change::{FileChange, Side};

/// The subcommand atelier runs itself with to be a subagent (see [`main`]).
pub const HELPER: &str = "subagent";

/// The `call` method a child's words to its parent travel as.
const METHOD: &str = "task";

/// How long a started process gets to come for its task before the task is
/// run on a thread instead.
const START_WAIT: Duration = Duration::from_secs(20);
/// How long a child told to stop gets to report before it ends itself —
/// and, [`KILL_AFTER`] later, is ended.
const STOP_GRACE: Duration = Duration::from_secs(10);
/// What the parent leaves a child that is ending itself: the time to stop
/// the commands it started.
const KILL_AFTER: Duration = Duration::from_secs(2);
/// How long a child that reported gets to exit by itself.
const EXIT_GRACE: Duration = Duration::from_secs(5);
/// How long the parent gets to answer anything but an `approve`.
const ANSWER_WAIT: Duration = Duration::from_secs(30);
/// How long a child keeps trying to reach its parent (a coordinator that
/// restarts is back within a fraction of a second).
const RETRY_FOR: Duration = Duration::from_secs(10);
/// How often the parent's side looks at the process and the stop flag.
const POLL: Duration = Duration::from_millis(50);
/// How much of what a dead process wrote to its standard error is kept.
const STDERR_TAIL: usize = 2048;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// One thing a child said, as its parent's side receives it.
struct Said {
    op: String,
    params: Value,
    reply: Responder,
}

/// The tasks a session handed to processes and has not had the report of:
/// where what their children say is sorted to the thread that waits for it.
#[derive(Clone, Default)]
pub(in crate::agent) struct Handed {
    tasks: Arc<Mutex<HashMap<String, Sender<Said>>>>,
}

impl Handed {
    /// Answer the children that call through `client`. The handler runs on
    /// the connection's reader thread, and only passes the call on.
    pub(in crate::agent) fn serve(&self, client: &coord::Client) {
        let tasks = Arc::clone(&self.tasks);
        client.handle_deferred(METHOD, move |call: Incoming| {
            let Incoming { params, reply, .. } = call;
            let waiting = params["task"]
                .as_str()
                .and_then(|id| lock(&tasks).get(id).cloned());
            let said = Said {
                op: params["op"].as_str().unwrap_or_default().to_string(),
                params,
                reply,
            };
            // Nobody waits for it (any more): the task ended, or never was.
            let unheard = match waiting {
                Some(to) => to.send(said).err().map(|e| e.0),
                None => Some(said),
            };
            if let Some(said) = unheard {
                let _ = said.reply.answer(Err("no such task is waiting".into()));
            }
        });
    }

    /// Wait for what the child of task `id` says, until the entry is dropped.
    fn enter(&self, id: &str) -> (Entry, Receiver<Said>) {
        let (tx, rx) = mpsc::channel();
        lock(&self.tasks).insert(id.to_string(), tx);
        let entry = Entry {
            tasks: Arc::clone(&self.tasks),
            id: id.to_string(),
        };
        (entry, rx)
    }
}

/// A task's place in [`Handed`], for as long as its child is waited for.
struct Entry {
    tasks: Arc<Mutex<HashMap<String, Sender<Said>>>>,
    id: String,
}

impl Drop for Entry {
    fn drop(&mut self) {
        lock(&self.tasks).remove(&self.id);
    }
}

/// What it takes to run a job as a process: the parent's side of it.
pub(super) struct Process {
    /// The parent's link to the other instances; `None` when it has none.
    coord: Option<Arc<coord::Client>>,
    handed: Handed,
    /// The project whose settings the child reads: the parent's, also when
    /// the child works in a worktree.
    project: PathBuf,
    /// Whether the parent does not wait for it.
    background: bool,
    /// Whether the parent's requests go to the model on this machine.
    local: bool,
}

impl Session {
    /// This session's side of a child that runs as a process.
    pub(super) fn process(&self, background: bool) -> Process {
        Process {
            coord: self.coord.clone(),
            handed: self.handed.clone(),
            project: self.root.clone(),
            background,
            local: self.local.is_some(),
        }
    }
}

impl Process {
    /// Run `job` in a process of its own and wait for how it ends, passing
    /// on what it says through the job's link. `Err` is why no process ran
    /// it — nothing was done, and the job can still be run here.
    pub(super) fn run(&self, job: &Job) -> Result<Outcome, String> {
        let Some(coord) = &self.coord else {
            return Err("this atelier runs without the coordinator".to_string());
        };
        if !coord.connected() {
            return Err(coord.link_status());
        }
        let exe = crate::app::helper_exe().map_err(|e| format!("no executable to start: {e}"))?;
        let id = crate::claude::new_uuid();
        let (_entry, said) = self.handed.enter(&id);
        let name = instance_name(&coord.name(), &job.agent.name);
        let mut command = Command::new(&exe);
        command
            .arg(HELPER)
            .args([&coord.id(), &id, &name, &coord.name()])
            .arg(&self.project)
            .env(coord::ENV_DIR, coord.dir())
            .current_dir(&job.session.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        // A group of its own: Ctrl-C at the terminal is the parent's to
        // pass on (or not: a background agent outlives the turn).
        #[cfg(unixlike)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut child = command
            .spawn()
            .map_err(|e| format!("could not start {}: {e}", exe.display()))?;
        let stderr = tail(child.stderr.take());
        let told = self.told(job, coord);
        let label = &job.label;

        // Held open for as long as the child is to go on (see the module
        // docs).
        let mut leash = child.stdin.take();
        let started = Instant::now();
        let mut taken = false;
        let mut stopped: Option<Instant> = None;
        let mut exited = None;
        // The last `approve` answered: one asked again (its answer was lost
        // with a coordinator) is not put to the user twice.
        let mut decided: Option<(u64, Option<String>)> = None;
        loop {
            // Once the process is gone, only what it had already said.
            let next = match exited {
                None => said.recv_timeout(POLL).ok(),
                Some(_) => said.try_recv().ok(),
            };
            match next {
                Some(Said { op, params, reply }) => match op.as_str() {
                    "take" => {
                        taken = true;
                        let _ = reply.answer(Ok(told.clone()));
                    }
                    "say" => {
                        if let Some(text) = params["text"].as_str() {
                            job.link.activity(text.to_string());
                        }
                        let _ = reply.answer(Ok(json!({})));
                    }
                    "change" => {
                        if let (Some(tool), Some(change)) =
                            (params["tool"].as_str(), change_from(&params["change"]))
                        {
                            job.link.file_change(tool, &change);
                        }
                        let _ = reply.answer(Ok(json!({})));
                    }
                    "approve" => {
                        let n = params["n"].as_u64().unwrap_or(0);
                        let refusal = match &decided {
                            Some((last, refusal)) if *last == n => refusal.clone(),
                            _ => {
                                let tool = params["tool"].as_str().unwrap_or_default();
                                let arguments = params["arguments"].as_str().unwrap_or_default();
                                self.approve(job, tool, arguments).err()
                            }
                        };
                        decided = Some((n, refusal.clone()));
                        let _ = reply.answer(Ok(json!({ "refusal": refusal })));
                    }
                    "report" => {
                        let outcome = outcome_from(&params);
                        let _ = reply.answer(Ok(json!({})));
                        drop(leash.take());
                        reap(&mut child, EXIT_GRACE);
                        return Ok(outcome);
                    }
                    other => {
                        let _ = reply.answer(Err(format!("unknown op '{other}'")));
                    }
                },
                None => {
                    if let Some(status) = exited {
                        let why = format!("its process ended before it reported ({status})");
                        let said = lock(&stderr).trim().to_string();
                        if !taken {
                            return Err(format!("{why}: {said}"));
                        }
                        return Ok(Outcome {
                            status: Status::Failed,
                            steps: 0,
                            text: format!(
                                "error: the subagent failed: {why}. It ran as a process of its \
                                 own, which is gone; what it did until then (files it wrote, \
                                 commands it ran) stays done, and nothing of it was \
                                 reported.\n{said}"
                            )
                            .trim_end()
                            .to_string(),
                            line: format!("⤶ {label}: failed — {why}"),
                            tokens_out: 0,
                        });
                    }
                }
            }
            if exited.is_none() {
                exited = child.try_wait().ok().flatten();
            }
            if exited.is_some() {
                continue;
            }
            if !taken && started.elapsed() > START_WAIT {
                reap(&mut child, Duration::ZERO);
                return Err(format!(
                    "its process did not come for the task within {}s",
                    START_WAIT.as_secs()
                ));
            }
            if stopped.is_none() && job.link.stop.load(Ordering::Relaxed) {
                drop(leash.take());
                stopped = Some(Instant::now());
            }
            if stopped.is_some_and(|at| at.elapsed() > STOP_GRACE + KILL_AFTER) {
                reap(&mut child, Duration::ZERO);
                return Ok(Outcome {
                    status: if self.background {
                        Status::Stopped
                    } else {
                        Status::Failed
                    },
                    steps: 0,
                    text: format!(
                        "(The agent was stopped before it finished; this is not a report.) Its \
                         process did not wind down within {}s and was ended.",
                        STOP_GRACE.as_secs()
                    ),
                    line: format!("⤶ {label}: stopped"),
                    tokens_out: 0,
                });
            }
        }
    }

    /// Whether the child may make a call: put to the parent the way a call
    /// of a child on a thread is — once it is known to be a call of a tool
    /// this agent was given, which the process is not taken at its word for.
    fn approve(&self, job: &Job, tool: &str, arguments: &str) -> Result<(), String> {
        if job.session.tools.get(tool).is_none() {
            return Err(format!("error: unknown tool '{tool}'"));
        }
        job.link.approve(tool, arguments)
    }

    /// The task as the process is told it when it comes for it: the prompt,
    /// and the session the parent would have run it in.
    fn told(&self, job: &Job, coord: &coord::Client) -> Value {
        let session = &job.session;
        let system = session.child.as_ref().map(|c| c.system.as_str());
        let tools: Vec<&str> = session.tools.all().map(|t| t.name()).collect();
        json!({
            "agent": job.agent.name,
            "label": job.label,
            "prompt": job.prompt,
            "system": system,
            "model": session.main_model(),
            "mode": session.mode.name(),
            "max_steps": job.agent.max_steps,
            "tools": tools,
            "background": self.background,
            "local": self.local,
            // What the child sends its peers is as far from a prompt a user
            // typed as what its parent sends (see `agent::idle`).
            "wake_depth": coord.wake_depth(),
        })
    }
}

/// The name a child asks the coordinator for: its parent's, a dot, its
/// agent's (`proj.explore`) — cut so that the whole fits what the
/// coordinator keeps of a name, and the agent's part is never what is lost.
fn instance_name(parent: &str, agent: &str) -> String {
    const MAX: usize = 40;
    let agent: String = agent.chars().take(MAX / 2).collect();
    let room = MAX - 1 - agent.chars().count();
    let parent: String = parent.chars().take(room).collect();
    format!("{parent}.{agent}")
}

/// Keep the end of what a process writes to its standard error, for the
/// message about its death.
fn tail(pipe: Option<impl Read + Send + 'static>) -> Arc<Mutex<String>> {
    let kept = Arc::new(Mutex::new(String::new()));
    let Some(mut pipe) = pipe else {
        return kept;
    };
    let sink = Arc::clone(&kept);
    std::thread::spawn(move || {
        let mut bytes: Vec<u8> = Vec::new();
        let mut buf = [0u8; 1024];
        while let Ok(n) = pipe.read(&mut buf) {
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buf[..n]);
            let excess = bytes.len().saturating_sub(STDERR_TAIL);
            bytes.drain(..excess);
            *lock(&sink) = String::from_utf8_lossy(&bytes).into_owned();
        }
    });
    kept
}

/// Wait up to `patience` for `child` to exit, then end it — and what it
/// started: it leads a process group of its own.
fn reap(child: &mut Spawned, patience: Duration) {
    let deadline = Instant::now() + patience;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(POLL);
    }
    // Through `kill`, there being no libc here to call (as `hooks` does).
    #[cfg(unixlike)]
    {
        let group = format!("-{}", child.id());
        let _ = Command::new("kill")
            .args(["-KILL", "--", &group])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// How a child ended, as it says so itself.
fn outcome_from(v: &Value) -> Outcome {
    let text = |key: &str| v[key].as_str().unwrap_or_default().to_string();
    let status = [Status::Done, Status::WrappedUp, Status::Stopped]
        .into_iter()
        .find(|s| v["status"] == s.word())
        .unwrap_or(Status::Failed);
    Outcome {
        status,
        steps: v["steps"].as_u64().unwrap_or(0) as u32,
        text: text("text"),
        line: text("line"),
        tokens_out: v["tokens_out"].as_u64().unwrap_or(0),
    }
}

fn outcome_value(outcome: &Outcome) -> Value {
    json!({
        "status": outcome.status.word(),
        "steps": outcome.steps,
        "text": outcome.text,
        "line": outcome.line,
        "tokens_out": outcome.tokens_out,
    })
}

fn side_value(side: &Side) -> Value {
    match side {
        Side::Absent => json!("absent"),
        Side::Binary => json!("binary"),
        Side::Large => json!("large"),
        Side::Text(text) => json!({ "text": text }),
    }
}

fn side_from(v: &Value) -> Option<Side> {
    match (v.as_str(), v["text"].as_str()) {
        (Some("absent"), _) => Some(Side::Absent),
        (Some("binary"), _) => Some(Side::Binary),
        (Some("large"), _) => Some(Side::Large),
        (_, Some(text)) => Some(Side::Text(text.to_string())),
        _ => None,
    }
}

fn change_value(change: &FileChange) -> Value {
    json!({
        "path": change.path.to_string_lossy(),
        "before": side_value(&change.before),
        "after": side_value(&change.after),
    })
}

fn change_from(v: &Value) -> Option<FileChange> {
    Some(FileChange {
        path: PathBuf::from(v["path"].as_str()?),
        before: side_from(&v["before"])?,
        after: side_from(&v["after"])?,
    })
}

/// A child's line to its parent when the parent is another process: the
/// [`Link`]'s other end in the child's own process (see the module docs for
/// what travels).
pub(super) struct Remote {
    coord: Arc<coord::Client>,
    /// The parent instance's id, and the task as the two know it.
    parent: String,
    task: String,
    /// Whether the parent does not wait for this child.
    background: bool,
    /// Numbers the calls put to the parent.
    asked: AtomicU64,
}

impl Remote {
    /// Say `op` to the parent and wait up to `timeout` for its answer,
    /// trying again for a while when it cannot be reached.
    fn call(&self, op: &str, mut params: Value, timeout: Duration) -> Result<Value> {
        params["task"] = self.task.as_str().into();
        params["op"] = op.into();
        let begun = Instant::now();
        loop {
            let asked = Instant::now();
            let e = match self
                .coord
                .call(&self.parent, METHOD, params.clone(), timeout)
            {
                Ok(v) => return Ok(v),
                Err(e) => e,
            };
            // It was reached and took too long, or has been out of reach
            // for too long.
            if asked.elapsed() >= timeout || begun.elapsed() >= RETRY_FOR {
                return Err(e);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// Something the parent need not have: one try, and no word back.
    fn tell(&self, op: &str, mut params: Value) {
        params["task"] = self.task.as_str().into();
        params["op"] = op.into();
        let _ = self
            .coord
            .call(&self.parent, METHOD, params, Duration::from_secs(5));
    }

    /// Whether the parent lets this child make a call. A parent that cannot
    /// be reached lets nothing through, and `stop` is set: there is nobody
    /// left to work for.
    pub(super) fn approve(
        &self,
        tool: &str,
        arguments: &str,
        stop: &AtomicBool,
    ) -> Result<(), String> {
        let n = self.asked.fetch_add(1, Ordering::Relaxed) + 1;
        let params = json!({ "n": n, "tool": tool, "arguments": arguments });
        let asked = Instant::now();
        match self.call("approve", params, ask::MAX_TIMEOUT) {
            Ok(v) => match v["refusal"].as_str() {
                Some(refusal) => Err(refusal.to_string()),
                None if v.get("refusal").is_some() => Ok(()),
                // Not an answer this side understands: not a yes.
                None => Err(DENIED.to_string()),
            },
            Err(_) if asked.elapsed() >= ask::MAX_TIMEOUT => Err(format!(
                "error: not run. Nobody decided this call within {} minutes.",
                ask::MAX_TIMEOUT.as_secs() / 60
            )),
            Err(e) => {
                stop.store(true, Ordering::Relaxed);
                Err(format!(
                    "error: not run. The agent that started you decides your calls, and it \
                     cannot be reached ({e:#}). Stop here."
                ))
            }
        }
    }

    pub(super) fn activity(&self, text: String) {
        self.tell("say", json!({ "text": text }));
    }

    /// A file a tool call wrote — for a parent that is waiting; nobody
    /// listens for a background agent's.
    pub(super) fn file_change(&self, tool: &str, change: &FileChange) {
        if !self.background {
            self.tell(
                "change",
                json!({ "tool": tool, "change": change_value(change) }),
            );
        }
    }

    pub(super) fn done(&self, outcome: Outcome) {
        let _ = self.call("report", outcome_value(&outcome), ANSWER_WAIT);
    }

    pub(super) fn background(&self) -> bool {
        self.background
    }
}

/// `atelier subagent <parent-id> <task-id> <name> <parent-name> <project>`:
/// be the child of one `task` call, in the current directory. Started by
/// [`Process::run`] and by nothing else — without a parent waiting for
/// exactly this task there is nothing to do.
///
/// The session is an ordinary one, opened as every session is (but reading
/// the settings of `<project>`, which is not the current directory when the
/// child works in a worktree) and joined to the coordinator under `<name>`;
/// it is then narrowed to the child the parent describes.
pub fn main(args: &[String]) -> Result<u8> {
    let [parent, task, name, parent_name, project] = args else {
        bail!(
            "`atelier {HELPER}` is what atelier starts for a `task` call that runs as a \
             process of its own (see docs/agents.md); it is not run by hand"
        );
    };
    let stop = Arc::new(AtomicBool::new(false));
    held_by_parent(Arc::clone(&stop));
    let session = crate::app::open_child(Path::new(project), name, parent_name)?;
    let Some(coord) = session.coord.clone() else {
        bail!("no coordinator to reach the parent through");
    };
    let mut remote = Remote {
        coord,
        parent: parent.clone(),
        task: task.clone(),
        background: false,
        asked: AtomicU64::new(0),
    };
    let told = remote
        .call("take", json!({}), ANSWER_WAIT)
        .context("asking the parent for the task")?;
    remote.background = told["background"] == true;
    remote
        .coord
        .set_wake_depth(told["wake_depth"].as_u64().unwrap_or(0) as u32);
    let link = Link {
        stop,
        to: Parent::Remote(Arc::new(remote)),
    };
    let label = told["label"].as_str().unwrap_or(name.as_str()).to_string();
    match job(session, &told, link.clone()) {
        Ok(job) => job.run(),
        // Taken and not runnable: the parent is waiting, and is told.
        Err(e) => link.done(Outcome {
            status: Status::Failed,
            steps: 0,
            text: format!("error: the subagent failed: {e:#}"),
            line: format!("⤶ {label}: failed — {e:#}"),
            tokens_out: 0,
        }),
    }
    Ok(0)
}

/// Stop when the parent says so or is gone: either way, the pipe it holds
/// to this process's standard input closes. A child that does not get to
/// its end within [`STOP_GRACE`] of that ends here.
fn held_by_parent(stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut sink = [0u8; 64];
        while matches!(stdin.read(&mut sink), Ok(n) if n > 0) {}
        stop.store(true, Ordering::Relaxed);
        std::thread::sleep(STOP_GRACE);
        crate::app::finish(());
        std::process::exit(1);
    });
}

/// `session` — an ordinary one — narrowed to the child the parent told of,
/// ready to run: what [`Session::child`] builds from a parent in the same
/// process.
fn job(mut session: Session, told: &Value, link: Link) -> Result<Job> {
    let text = |key: &str| told[key].as_str().map(str::to_string);
    let need = |key: &str| text(key).with_context(|| format!("the task has no `{key}`"));
    let (name, prompt, system) = (need("agent")?, need("prompt")?, need("system")?);
    let max_steps = told["max_steps"].as_u64().context("no `max_steps`")? as u32;
    let mode = Mode::parse(&need("mode")?).map_err(anyhow::Error::msg)?;
    let allowed: Vec<&str> = told["tools"]
        .as_array()
        .context("the task has no `tools`")?
        .iter()
        .filter_map(Value::as_str)
        .collect();

    if told["local"] == true {
        session.use_local(&mut crate::app::Quiet)?;
    }
    // The parent's MCP tools are connections of its process; the ones this
    // agent may use are connected again here, where the project is trusted.
    if allowed.iter().any(|t| t.starts_with("mcp_")) {
        session.connect_configured_mcp();
    }
    // Exactly the parent's tools for this agent, by name — never more, and
    // never `task`: the depth is one.
    session.tools = session
        .tools
        .filtered(|tool| tool != TASK_TOOL && allowed.contains(&tool));
    let mut providers =
        context::standing_providers(context::InstructionFiles::with_user(&session.settings));
    providers.push(Box::new(context::McpProvider::new(
        session.mcp_catalog.clone(),
    )));
    session.context = providers;
    session.set_model(&need("model")?);
    session.review_enabled = false;
    // What the parent judges the child's calls by, for the child to be told.
    session.mode = mode;
    session.max_steps = Some(max_steps);
    session.child = Some(Child {
        system,
        link: link.clone(),
    });
    Ok(Job {
        label: text("label").unwrap_or_else(|| name.clone()),
        // Only what the run itself reads: the rest is the parent's, and
        // already in the session.
        agent: Agent {
            name,
            description: String::new(),
            model: None,
            tools: None,
            max_steps,
            isolation: Isolation::Shared,
            process: false,
            prompt: String::new(),
            source: Source::Builtin,
        },
        prompt,
        session,
        link,
        worktree: None,
        process: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_is_named_after_its_parent_and_its_agent() {
        assert_eq!(instance_name("proj", "explore"), "proj.explore");
        // A long directory name does not push the agent's out.
        let long = instance_name(&"p".repeat(60), "explore");
        assert_eq!(long, format!("{}.explore", "p".repeat(32)));
        assert_eq!(long.chars().count(), 40);
        let both = instance_name(&"p".repeat(60), &"a".repeat(64));
        assert_eq!(both, format!("{}.{}", "p".repeat(19), "a".repeat(20)));
    }

    #[test]
    fn an_outcome_and_a_change_survive_the_trip() {
        let outcome = Outcome {
            status: Status::WrappedUp,
            steps: 7,
            text: "REPORT\n\n[explore: 7 steps]".to_string(),
            line: "⤶ explore: done (7 steps — wrapped up at its step cap)".to_string(),
            tokens_out: 412,
        };
        let back = outcome_from(&outcome_value(&outcome));
        assert!(back.status == Status::WrappedUp);
        assert_eq!((back.steps, back.tokens_out), (7, 412));
        assert_eq!((back.text, back.line), (outcome.text, outcome.line));
        // A status this side does not know is not a success.
        assert!(outcome_from(&json!({ "status": "splendid" })).status == Status::Failed);

        for (before, after) in [
            (Side::Absent, Side::Text("new\n".into())),
            (Side::Text("old\n".into()), Side::Absent),
            (Side::Binary, Side::Large),
        ] {
            let change = FileChange {
                path: PathBuf::from("/p/src/x.rs"),
                before,
                after,
            };
            assert_eq!(change_from(&change_value(&change)), Some(change));
        }
        assert_eq!(change_from(&json!({ "path": "/p/x" })), None);
    }

    /// A call for a task nobody waits for is answered at once, with an
    /// error; one for a task that is waited for reaches its thread.
    #[cfg(unixlike)]
    #[test]
    fn what_a_child_says_reaches_the_thread_that_waits_for_its_task() {
        use crate::agent::tests::Peers;

        let peers = Peers::start("handed");
        let (parent, child) = (peers.join("parent"), peers.join("child"));
        let handed = Handed::default();
        handed.serve(&parent);

        let say = |task: &str| {
            let params = json!({ "task": task, "op": "say", "text": "grep fn main" });
            child.call(&parent.id(), METHOD, params, Duration::from_secs(5))
        };
        let unknown = say("no-such-task").unwrap_err().to_string();
        assert!(unknown.contains("no such task is waiting"), "{unknown}");

        let (entry, said) = handed.enter("t1");
        let waiter = std::thread::spawn(move || {
            let Said { op, params, reply } = said.recv().unwrap();
            reply.answer(Ok(json!({ "heard": true }))).unwrap();
            (op, params["text"].as_str().unwrap().to_string())
        });
        assert_eq!(say("t1").unwrap(), json!({ "heard": true }));
        assert_eq!(
            waiter.join().unwrap(),
            ("say".to_string(), "grep fn main".to_string())
        );
        // Once the task is over, so is the listening.
        drop(entry);
        assert!(say("t1").is_err());
    }
}
