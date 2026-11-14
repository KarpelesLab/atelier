//! The `bash` tool: run a shell command in the project root with a timeout —
//! or in the background, as a job that `bash_output` reads (or waits on) and
//! `bash_kill` stops.
//!
//! Background jobs belong to the atelier process (a registry shared by every
//! call), run in their own process group so stopping one stops what it
//! spawned, and are killed when atelier exits ([`kill_all`]).
//!
//! # A finished job says so
//!
//! The model does not have to poll a job to learn that it ended: each job
//! that exits leaves a [`Notice`] — which job, how it ended, how long it
//! ran, the end of its output — that the agent loop collects with
//! [`notices`] and puts in front of the model as a marked block (see
//! `agent::jobs`). Who has been told is kept with the job ([`Told`]), so the
//! user is shown one line and the model gets one block, whichever of them
//! comes first. No notice is left by a job the model stopped (`bash_kill`)
//! or whose exit it has already seen through `bash_output`.
//!
//! A job's exit is noticed by a thread that watches it ([`watch`]), which
//! is what makes the reported runtime the job's own rather than the time
//! until somebody looked.
//!
//! # The sandbox
//!
//! With the sandbox on ([`crate::sandbox`]), a foreground command runs in a
//! nixvm guest that sees only the project, and so needs no approval; one that
//! asks for the host (`"sandbox": false`) or the background does.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolSpec, spill};
use crate::sandbox;

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// Longest `bash_output` will block waiting.
const MAX_WAIT_MS: u64 = 600_000;
/// The most output, in bytes, a call returns; the rest goes to a file (see
/// [`spill`]).
const MAX_OUTPUT_BYTES: usize = 20_000;
/// How often the watchdog polls the child for exit while waiting for the
/// timeout to elapse.
const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// How often a background job is looked at to see whether it has ended.
const WATCH_INTERVAL: Duration = Duration::from_millis(100);
/// How many lines from the end of a job's output its notice carries, and
/// the most of each line, and of the command, in characters.
const NOTICE_LINES: usize = 10;
const NOTICE_LINE_CHARS: usize = 200;
const NOTICE_COMMAND_CHARS: usize = 80;
/// How long after a job's exit its notice waits for the last of its output
/// (see [`Job::settled`]).
const DRAIN_GRACE: Duration = Duration::from_secs(1);
/// What opens a job's notice as the model sees it.
pub const NOTICE_OPEN: &str = "<background-job ";

pub struct BashTool;

/// Whether the call asks for a background job. `run_in_background` is what
/// Claude models know it as.
fn wants_background(args: &Value) -> bool {
    ["background", "run_in_background"]
        .iter()
        .any(|k| args.get(*k).and_then(Value::as_bool) == Some(true))
}

/// Whether the call asks to run on the host although the sandbox is on.
fn wants_host(args: &Value) -> bool {
    args.get("sandbox").and_then(Value::as_bool) == Some(false)
}

impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    /// A foreground command the sandbox will run cannot leave the project,
    /// like the file tools, so it runs unasked (deny and ask rules still
    /// apply: they are judged before a tool's own default). Everything that
    /// runs on the host asks: with the sandbox off, a background job, a
    /// `"sandbox": false` request — and, so that nothing slips through, a
    /// sandbox that is on but unavailable, which [`call`](Tool::call) refuses.
    fn requires_approval_in(&self, args: &Value, root: &Path) -> bool {
        !(matches!(sandbox::state(root), sandbox::State::Ready(_))
            && !wants_background(args)
            && !wants_host(args))
    }

    fn spec(&self) -> ToolSpec {
        let mut description = "Run a shell command (via `sh -c`) in the project root. Captures \
                combined stdout/stderr and the exit status. Very long output comes back as its start and its end, with all of it saved \
                to a file you can read or grep; \
                the command is killed if it exceeds the timeout. With `background: true` it \
                starts as a job instead and returns its id at once. You are told when the \
                job ends (a <background-job> block), so do not poll it: go on with other \
                work. Read its output, or wait for it to print something, with \
                `bash_output`; stop it with `bash_kill`. Use that for servers, watchers, \
                and long builds."
                .to_string();
        // Only while the sandbox is on, so the description (which is part of
        // every request) is otherwise what it always was.
        let sandboxed =
            std::env::current_dir().is_ok_and(|root| sandbox::state(&root) != sandbox::State::Off);
        if sandboxed {
            description.push_str(sandbox::TOOL_NOTE);
        }
        ToolSpec {
            name: self.name().to_string(),
            description,
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to run."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Maximum time to allow the command to run, in milliseconds (default 120000). Not applied to background jobs."
                    },
                    "background": {
                        "type": "boolean",
                        "description": "Run as a background job and return its id immediately (default false)."
                    },
                    "sandbox": {
                        "type": "boolean",
                        "description": "Only when commands run in a sandbox: false runs this one on the host instead, which the user must approve. Use it after a result says the sandbox was in the way, not before."
                    }
                },
                "required": ["command"]
            }),
        }
    }

    fn call(&self, ctx: &mut ToolCtx, args: Value) -> Result<String> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing required argument 'command'"))?;
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_MS);
        let state = sandbox::state(ctx.project_root);
        let sandboxed = state != sandbox::State::Off;
        // With the sandbox on, the host is reached only through a call that
        // was approved as one (`requires_approval_in`).
        if sandboxed && (wants_background(&args) || wants_host(&args)) && !ctx.approved {
            bail!("running on the host while the sandbox is on needs approval");
        }
        if wants_background(&args) {
            let started = start_job(command, ctx.project_root)?;
            return Ok(if sandboxed {
                format!("{started}\n[background jobs are not sandboxed: this one runs on the host]")
            } else {
                started
            });
        }
        let timeout = Duration::from_millis(timeout_ms);
        match state {
            sandbox::State::Ready(setup) if !wants_host(&args) => {
                let out = sandbox::run(command, ctx.project_root, &setup, timeout)?;
                return Ok(format_sandboxed(ctx.project_root, timeout_ms, out));
            }
            // Asked for and not there: refused, not run on the host instead,
            // so that turning the sandbox on never means less than it says.
            sandbox::State::Unavailable(why) if !wants_host(&args) => bail!(
                "the sandbox is enabled but cannot run commands: {why}. Nothing was run. To \
                 run this on the host instead, call bash again with \"sandbox\": false (the \
                 user will be asked)."
            ),
            _ => {}
        }

        let child = Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(ctx.project_root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning shell command")?;
        let done = capture(child, timeout)?;
        let status = match done.status {
            _ if done.timed_out => timed_out_line(timeout_ms),
            Some(status) => format!("[exit status: {status}]\n"),
            None => "[exit status: unknown]\n".to_string(),
        };
        let mut out = format_result(ctx.project_root, status, done.stdout, done.stderr);
        if sandboxed {
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("[ran on the host, not in the sandbox]");
        }
        Ok(out)
    }
}

/// What a child process did within its time.
pub(crate) struct Captured {
    /// `None` if it was killed at the timeout.
    pub status: Option<std::process::ExitStatus>,
    pub timed_out: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Wait for `child` (spawned with piped stdout and stderr), collecting what
/// it prints, and kill it if it is still running after `timeout`.
pub(crate) fn capture(mut child: Child, timeout: Duration) -> Result<Captured> {
    // Read stdout/stderr on their own threads so a full pipe buffer can't
    // block the child while the watchdog below isn't draining it.
    let mut stdout_pipe = child.stdout.take().expect("piped stdout");
    let mut stderr_pipe = child.stderr.take().expect("piped stderr");
    let stdout_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let stderr_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });

    // Watchdog: poll for exit, killing the child once the timeout elapses.
    let start = Instant::now();
    let (status, timed_out) = loop {
        match child.try_wait().context("waiting for command")? {
            Some(status) => break (Some(status), false),
            None if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break (None, true);
            }
            None => std::thread::sleep(POLL_INTERVAL),
        }
    };

    Ok(Captured {
        status,
        timed_out,
        stdout: stdout_handle.join().unwrap_or_default(),
        stderr: stderr_handle.join().unwrap_or_default(),
    })
}

fn timed_out_line(timeout_ms: u64) -> String {
    format!("[command timed out after {timeout_ms}ms and was killed]\n")
}

/// The result of a command that ran in the sandbox: as for one on the host,
/// plus — when its failure looks like the sandbox's doing — a line saying so
/// and how to ask for the host.
fn format_sandboxed(root: &Path, timeout_ms: u64, out: sandbox::Output) -> String {
    let status = match &out.outcome {
        // As `ExitStatus` prints it.
        sandbox::Outcome::Exited(code) => format!("[exit status: exit status: {code}]\n"),
        sandbox::Outcome::TimedOut => timed_out_line(timeout_ms),
        sandbox::Outcome::Failed(_) => "[not run: the sandbox failed]\n".to_string(),
    };
    let blame = sandbox::diagnose(&out);
    let mut text = format_result(root, status, out.stdout, out.stderr);
    if let Some(why) = blame {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!(
            "[sandbox: {why}. If this command needs the host, call bash again with \
             \"sandbox\": false and say why — the user will be asked to approve it.]"
        ));
    }
    text
}

/// `status` (a line saying how the command ended), then its output.
fn format_result(root: &Path, status: String, stdout: Vec<u8>, stderr: Vec<u8>) -> String {
    let stdout = String::from_utf8_lossy(&stdout).into_owned();
    let stderr = String::from_utf8_lossy(&stderr).into_owned();

    let mut combined = String::new();
    combined.push_str(&stdout);
    if !stderr.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str("[stderr]\n");
        combined.push_str(&stderr);
    }

    let mut out = status;
    // Past the budget, the head and the tail; all of it is in a file.
    out.push_str(&spill::fit(root, combined, MAX_OUTPUT_BYTES));
    out
}

/// A background job.
struct Job {
    command: String,
    child: Child,
    /// Combined stdout/stderr so far, appended by reader threads.
    output: Arc<Mutex<Vec<u8>>>,
    /// How much of `output` the model has already been shown.
    seen: usize,
    started: Instant,
    /// When it was found to have exited (by [`watch`], or by whoever asked
    /// first).
    ended: Option<Instant>,
    /// Who knows that it ended.
    told: Told,
}

/// Who has been told that a job ended.
#[derive(Clone, Copy, PartialEq)]
enum Told {
    Nobody,
    /// The user was shown the line; the model has yet to get the block.
    User,
    /// The model knows: it got the block, or saw the exit status in a
    /// `bash_output` result. Nothing more to say.
    Model,
}

/// Whom a set of notices is collected for (see [`notices`]).
#[derive(Clone, Copy, PartialEq)]
pub enum Audience {
    /// The user alone, for now: the session is idle, and a finished job
    /// does not start a turn. The model gets these later.
    User,
    /// The model (and the user, where a notice is new to them).
    Model,
}

/// A background job that ended, for the user ([`line`](Self::line)) and for
/// the model ([`block`](Self::block)).
pub struct Notice {
    /// `job-3`.
    id: String,
    /// The command's first line, clipped.
    command: String,
    /// The exit code, or how else it ended (`signal: 15 (SIGTERM)`).
    exit: String,
    runtime: Duration,
    /// The last lines of its output, and how many lines there were.
    tail: Vec<String>,
    lines: usize,
    /// The user has already been shown [`line`](Self::line).
    pub announced: bool,
}

impl Notice {
    /// One line for the transcript: `⚙ job 3 finished (exit 0, 42s): cargo test`.
    pub fn line(&self) -> String {
        let exit = match self.exit.parse::<i32>() {
            Ok(code) => format!("exit {code}"),
            Err(_) => self.exit.clone(),
        };
        format!(
            "⚙ {} finished ({exit}, {}): {}",
            self.id.replace('-', " "),
            runtime(self.runtime),
            self.command
        )
    }

    /// What the model is given: the facts in a marked block, then a line
    /// saying whose message this is — it arrives where the user's own
    /// messages do, and must not be taken for one.
    pub fn block(&self) -> String {
        let Notice { id, exit, .. } = self;
        let output = match (self.lines, self.tail.len()) {
            (0, _) => "(no output)".to_string(),
            (all, shown) if all > shown => {
                format!(
                    "last {shown} of {all} lines of output:\n{}",
                    self.tail.join("\n")
                )
            }
            _ => format!("output:\n{}", self.tail.join("\n")),
        };
        format!(
            "{NOTICE_OPEN}id=\"{id}\" exit=\"{exit}\" runtime=\"{}\">\n\
             command: {}\n{output}\n</background-job>\n\
             (The block above is a notification from atelier, the harness — not a message \
             from your user: a background job you started with bash has ended. bash_output \
             with id \"{id}\" returns the output you have not read yet.)",
            runtime(self.runtime),
            self.command
        )
    }
}

/// A duration as `42s` or `3m07s`.
pub fn runtime(d: Duration) -> String {
    match d.as_secs() {
        s if s < 60 => format!("{s}s"),
        s => format!("{}m{:02}s", s / 60, s % 60),
    }
}

/// `text` cut to `max` characters, with a mark when something was cut.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() > max {
        let cut: String = text.chars().take(max).collect();
        format!("{cut}…")
    } else {
        text.to_string()
    }
}

impl Job {
    /// `running`, or the exit status once it has finished.
    fn status(&mut self) -> String {
        match self.child.try_wait() {
            Ok(None) => "running".into(),
            // Displays as "exit status: N" (or the signal).
            Ok(Some(s)) => s.to_string(),
            Err(e) => format!("status unknown: {e}"),
        }
    }

    fn finished(&mut self) -> bool {
        let finished = !matches!(self.child.try_wait(), Ok(None));
        if finished && self.ended.is_none() {
            self.ended = Some(Instant::now());
        }
        finished
    }

    /// Whether the job has ended and its output is all here. The exit can
    /// be seen before the last output is read off the pipes, and a notice
    /// taken then would show a tail that is not the end; the reader threads
    /// drop their handle on `output` when the pipes close. Something the
    /// job left running may hold them open for good, though, so past
    /// [`DRAIN_GRACE`] the notice goes out with what there is.
    fn settled(&mut self) -> bool {
        self.finished()
            && (Arc::strong_count(&self.output) == 1
                || self.ended.is_some_and(|at| at.elapsed() >= DRAIN_GRACE))
    }

    /// The notice of a job that has ended.
    fn notice(&mut self, id: &str) -> Notice {
        let exit = match self.child.try_wait() {
            Ok(Some(status)) => match status.code() {
                Some(code) => code.to_string(),
                // Ended by a signal: the status says which.
                None => status.to_string(),
            },
            _ => "unknown".to_string(),
        };
        let output = self.output.lock().unwrap_or_else(|e| e.into_inner());
        let text = String::from_utf8_lossy(&output);
        let lines: Vec<&str> = text.lines().collect();
        let tail = lines[lines.len().saturating_sub(NOTICE_LINES)..]
            .iter()
            .map(|l| clip(l, NOTICE_LINE_CHARS))
            .collect();
        let whole = self.command.trim();
        let first = whole.lines().next().unwrap_or("");
        let mut command = clip(first, NOTICE_COMMAND_CHARS);
        // A command of several lines is cut after the first, visibly.
        if first.len() < whole.len() && !command.ends_with('…') {
            command.push('…');
        }
        Notice {
            id: id.to_string(),
            command,
            exit,
            runtime: self.ended.unwrap_or_else(Instant::now) - self.started,
            tail,
            lines: lines.len(),
            announced: self.told == Told::User,
        }
    }
}

/// Background jobs by id (`job-1`, `job-2`, …).
#[derive(Default)]
struct Jobs {
    next: u32,
    jobs: BTreeMap<String, Job>,
}

impl Jobs {
    /// The notices `audience` has not had yet, among the jobs `keep`
    /// accepts — marked as had.
    fn notices(&mut self, audience: Audience, keep: impl Fn(&str) -> bool) -> Vec<Notice> {
        let mut notices = Vec::new();
        for (id, job) in self.jobs.iter_mut() {
            let due = match audience {
                Audience::User => job.told == Told::Nobody,
                Audience::Model => job.told != Told::Model,
            };
            if due && keep(id) && job.settled() {
                notices.push(job.notice(id));
                job.told = match audience {
                    Audience::User => Told::User,
                    Audience::Model => Told::Model,
                };
            }
        }
        notices
    }
}

/// The background jobs that ended and that `audience` has not been told
/// about. Each is returned once per audience: asking for the
/// user's leaves them due for the model, with
/// [`announced`](Notice::announced) set; asking for the model's settles
/// them.
pub fn notices(audience: Audience) -> Vec<Notice> {
    jobs().notices(audience, |_| true)
}

/// A background job as a listing shows it (see [`job_list`]).
pub struct JobInfo {
    /// `job-3`.
    pub id: String,
    /// The command, whole.
    pub command: String,
    /// `running`, or how it ended (`exit status: 0`, the signal).
    pub status: String,
    pub running: bool,
    /// The exit code, once it has exited with one (not when a signal ended
    /// it).
    pub exit_code: Option<i32>,
    /// How long it ran, or has been running.
    pub runtime: Duration,
}

/// Every background job of this process, in the order `bash_output` lists
/// them: the running ones, and the ended ones `bash_kill` has not removed.
/// Asking tells nobody that a job ended (see [`notices`]).
pub fn job_list() -> Vec<JobInfo> {
    let mut jobs = jobs();
    let info = |(id, job): (&String, &mut Job)| {
        let running = !job.finished();
        JobInfo {
            id: id.clone(),
            command: job.command.clone(),
            status: job.status(),
            running,
            exit_code: match job.child.try_wait() {
                Ok(Some(status)) => status.code(),
                _ => None,
            },
            runtime: job.ended.unwrap_or_else(Instant::now) - job.started,
        }
    };
    jobs.jobs.iter_mut().map(info).collect()
}

/// Note when the job `id` ends: look at it now and then until it has
/// exited — or is gone from the registry, stopped by `bash_kill`.
fn watch(id: String) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(WATCH_INTERVAL);
            // `finished` is what records the time.
            if jobs().jobs.get_mut(&id).is_none_or(Job::finished) {
                return;
            }
        }
    });
}

fn jobs() -> MutexGuard<'static, Jobs> {
    static JOBS: OnceLock<Mutex<Jobs>> = OnceLock::new();
    JOBS.get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn start_job(command: &str, root: &std::path::Path) -> Result<String> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Its own process group, so bash_kill reaches whatever it spawns.
    #[cfg(unixlike)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd.spawn().context("spawning background command")?;
    let output = Arc::new(Mutex::new(Vec::new()));
    for pipe in [
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let sink = Arc::clone(&output);
        std::thread::spawn(move || {
            let mut pipe = pipe;
            let mut buf = [0u8; 4096];
            while let Ok(n) = pipe.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend_from_slice(&buf[..n]);
            }
        });
    }
    let pid = child.id();
    let mut jobs = jobs();
    jobs.next += 1;
    let id = format!("job-{}", jobs.next);
    jobs.jobs.insert(
        id.clone(),
        Job {
            command: command.to_string(),
            child,
            output,
            seen: 0,
            started: Instant::now(),
            ended: None,
            told: Told::Nobody,
        },
    );
    drop(jobs);
    watch(id.clone());
    Ok(format!(
        "started background job {id} (pid {pid}). You will be told when it ends; read its \
         output or wait on it with bash_output; stop it with bash_kill."
    ))
}

/// Stop a job's whole process group (TERM, then KILL if it lingers).
fn stop(job: &mut Job) {
    #[cfg(unixlike)]
    {
        let pgid = format!("-{}", job.child.id());
        let _ = Command::new("kill").args(["-TERM", "--", &pgid]).status();
        let deadline = Instant::now() + Duration::from_millis(1500);
        while !job.finished() && Instant::now() < deadline {
            std::thread::sleep(POLL_INTERVAL);
        }
        if !job.finished() {
            let _ = Command::new("kill").args(["-KILL", "--", &pgid]).status();
        }
    }
    let _ = job.child.kill();
    let _ = job.child.wait();
}

/// Kill every background job. Called as atelier exits.
pub fn kill_all() {
    for job in jobs().jobs.values_mut() {
        if !job.finished() {
            stop(job);
        }
    }
}

/// `bash_output`: what a background job printed since last asked, optionally
/// waiting for it to finish or to print something matching a pattern.
pub struct BashOutputTool;

impl Tool for BashOutputTool {
    fn name(&self) -> &str {
        "bash_output"
    }

    // Only reads what a job printed.
    fn mutates(&self, _args: &Value) -> bool {
        false
    }

    fn requires_approval(&self, _args: &Value) -> bool {
        // Only reads jobs the user already approved starting.
        false
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: "Get a background job's new output (since the last call) and its \
                status. With wait_ms, block until the job exits, or — with `until` — until \
                its new output matches that regex, or until wait_ms passes; use this to wait \
                for a build to finish or a server to come up. Without an id: with wait_ms or \
                `until`, the most recently started job; otherwise, list the jobs."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "The job id from `bash` (e.g. \"job-1\"). Omit to list jobs (or, when waiting, to use the latest job)."
                    },
                    "wait_ms": {
                        "type": "integer",
                        "description": "How long to wait, at most, in milliseconds (default 0: return at once; max 600000)."
                    },
                    "until": {
                        "type": "string",
                        "description": "With wait_ms: stop waiting as soon as the new output matches this regex."
                    }
                }
            }),
        }
    }

    fn call(&self, ctx: &mut ToolCtx, args: Value) -> Result<String> {
        // Waiting without naming a job means the one just started.
        let waiting = args.get("wait_ms").is_some() || args.get("until").is_some();
        let latest = (waiting && args.get("id").is_none())
            .then(|| {
                jobs()
                    .jobs
                    .keys()
                    .max_by_key(|k| k.trim_start_matches("job-").parse::<u32>().unwrap_or(0))
                    .cloned()
            })
            .flatten();
        let Some(id) = args.get("id").and_then(Value::as_str).or(latest.as_deref()) else {
            let jobs = job_list();
            if jobs.is_empty() {
                return Ok("no background jobs".into());
            }
            let mut out = String::new();
            for job in jobs {
                out.push_str(&format!("{}  [{}]  {}\n", job.id, job.status, job.command));
            }
            return Ok(out);
        };
        let wait = Duration::from_millis(
            args.get("wait_ms")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .min(MAX_WAIT_MS),
        );
        let until = args
            .get("until")
            .and_then(Value::as_str)
            .map(Regex::new)
            .transpose()
            .context("invalid `until` regex")?;

        let deadline = Instant::now() + wait;
        let mut matched = false;
        loop {
            {
                let mut jobs = jobs();
                let Some(job) = jobs.jobs.get_mut(id) else {
                    bail!("no background job {id:?} (bash_output with no id lists them)");
                };
                let fresh = {
                    let out = job.output.lock().unwrap_or_else(|e| e.into_inner());
                    String::from_utf8_lossy(&out[job.seen..]).into_owned()
                };
                if let Some(re) = &until {
                    matched = re.is_match(&fresh);
                }
                if job.finished() || matched || Instant::now() >= deadline {
                    job.seen += fresh.len();
                    // This result carries the exit status: the model needs
                    // no notice of what it is reading here.
                    if job.finished() {
                        job.told = Told::Model;
                    }
                    let mut out = format!(
                        "[{id}: {} after {}s]\n",
                        job.status(),
                        job.started.elapsed().as_secs()
                    );
                    if until.is_some() && !matched && !job.finished() {
                        out.push_str("[`until` pattern not seen yet]\n");
                    }
                    // A long log: its start and its end, the rest in a file.
                    out.push_str(&spill::fit(ctx.project_root, fresh, MAX_OUTPUT_BYTES));
                    return Ok(out);
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// `bash_kill`: stop a background job (and everything it started).
pub struct BashKillTool;

impl Tool for BashKillTool {
    fn name(&self) -> &str {
        "bash_kill"
    }

    // Stops a job this session started; nothing of the user's changes.
    fn mutates(&self, _args: &Value) -> bool {
        false
    }

    fn requires_approval(&self, _args: &Value) -> bool {
        // Only stops jobs the user already approved starting.
        false
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: "Stop a background job started with `bash` (and the processes it \
                spawned)."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The job id (e.g. \"job-1\")." }
                },
                "required": ["id"]
            }),
        }
    }

    fn call(&self, _ctx: &mut ToolCtx, args: Value) -> Result<String> {
        let id = args
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing required argument 'id'"))?;
        let mut jobs = jobs();
        let Some(mut job) = jobs.jobs.remove(id) else {
            bail!("no background job {id:?}");
        };
        drop(jobs);
        if job.finished() {
            return Ok(format!("{id} had already finished ({})", job.status()));
        }
        stop(&mut job);
        Ok(format!("stopped {id}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::FileState;
    use std::path::PathBuf;

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "atelier-test-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn runs_command_and_captures_stdout() {
        let root = tempdir("bash-basic");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        let out = BashTool
            .call(&mut ctx, json!({"command": "echo hi"}))
            .unwrap();
        assert!(out.contains("hi"));
        assert!(out.contains("exit status"));
    }

    #[test]
    fn runs_in_project_root() {
        let root = tempdir("bash-cwd");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        let out = BashTool.call(&mut ctx, json!({"command": "pwd"})).unwrap();
        // Canonicalize both sides since tempdir may involve symlinks (e.g. /tmp -> /private/tmp on macOS).
        let canon_root = std::fs::canonicalize(&root).unwrap();
        assert!(out.contains(canon_root.to_str().unwrap()) || out.contains(root.to_str().unwrap()));
    }

    #[test]
    fn nonzero_exit_reported() {
        let root = tempdir("bash-exit");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        let out = BashTool
            .call(&mut ctx, json!({"command": "exit 3"}))
            .unwrap();
        assert!(out.contains("exit status"));
        assert!(!out.contains("exit status: 0"));
    }

    #[test]
    fn timeout_kills_command() {
        let root = tempdir("bash-timeout");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        let out = BashTool
            .call(&mut ctx, json!({"command": "sleep 5", "timeout_ms": 100}))
            .unwrap();
        assert!(out.contains("timed out"));
    }

    #[test]
    fn background_job_output_wait_and_kill() {
        let root = tempdir("bash-bg");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        let started = BashTool
            .call(
                &mut ctx,
                json!({"command": "echo one; sleep 0.3; echo ready; sleep 30", "run_in_background": true}),
            )
            .unwrap();
        let id = started
            .split_whitespace()
            .find(|w| w.starts_with("job-"))
            .unwrap()
            .to_string();

        // Waits until the pattern shows up, well before the job ends.
        let t0 = Instant::now();
        let out = BashOutputTool
            .call(
                &mut ctx,
                json!({"id": id, "wait_ms": 10000, "until": "ready"}),
            )
            .unwrap();
        assert!(t0.elapsed() < Duration::from_secs(5));
        assert!(out.contains("running"), "{out}");
        assert!(out.contains("one") && out.contains("ready"), "{out}");

        // Only new output is returned next time.
        let out = BashOutputTool.call(&mut ctx, json!({"id": id})).unwrap();
        assert!(!out.contains("ready"), "{out}");

        let listed = BashOutputTool.call(&mut ctx, json!({})).unwrap();
        assert!(listed.contains(&id), "{listed}");

        // Waiting without an id targets the latest job (tests share the job
        // registry, so maybe another test's), not the listing.
        let out = BashOutputTool
            .call(&mut ctx, json!({"wait_ms": 1}))
            .unwrap();
        assert!(out.starts_with("[job-"), "{out}");

        assert!(
            BashKillTool
                .call(&mut ctx, json!({"id": id}))
                .unwrap()
                .contains("stopped")
        );
        assert!(BashOutputTool.call(&mut ctx, json!({"id": id})).is_err());
    }

    #[test]
    fn background_job_runs_to_completion() {
        let root = tempdir("bash-bg-done");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        let started = BashTool
            .call(
                &mut ctx,
                json!({"command": "echo done; exit 3", "background": true}),
            )
            .unwrap();
        let id = started
            .split_whitespace()
            .find(|w| w.starts_with("job-"))
            .unwrap()
            .to_string();
        let out = BashOutputTool
            .call(&mut ctx, json!({"id": id, "wait_ms": 10000}))
            .unwrap();
        assert!(out.contains("exit status: 3"), "{out}");
        assert!(!out.contains("exit status: exit status"), "{out}");
        assert!(out.contains("done"), "{out}");
    }

    /// Start `command` as a job and return its id.
    fn start(ctx: &mut ToolCtx, command: &str) -> String {
        let started = BashTool
            .call(ctx, json!({"command": command, "background": true}))
            .unwrap();
        let id = started.split_whitespace().find(|w| w.starts_with("job-"));
        id.unwrap().to_string()
    }

    /// The notices due to `audience` for the job `id` alone, once it has
    /// ended. (Tests share the job registry: each asks about its own job.)
    fn notices_of(id: &str, audience: Audience) -> Vec<Notice> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let mut jobs = jobs();
            if jobs.jobs.get_mut(id).is_none_or(Job::settled) {
                return jobs.notices(audience, |job| job == id);
            }
            drop(jobs);
            assert!(Instant::now() < deadline, "{id} never ended");
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    #[test]
    fn a_finished_job_leaves_one_notice_for_the_user_and_one_for_the_model() {
        let root = tempdir("bash-notice");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        let id = start(&mut ctx, "seq 1 12; echo oops; exit 3");

        // While it runs there is nothing to say (asked before it can have
        // ended would be a race; a job that never ends is not).
        let running = start(&mut ctx, "sleep 30");
        assert!(
            jobs()
                .notices(Audience::Model, |job| job == running)
                .is_empty()
        );

        // Idle: the user is shown the line, once; the model's is still due.
        let shown = notices_of(&id, Audience::User);
        assert_eq!(shown.len(), 1);
        assert!(!shown[0].announced);
        let n = id.trim_start_matches("job-");
        let line = shown[0].line();
        assert!(
            line.starts_with(&format!("⚙ job {n} finished (exit 3, ")),
            "{line}"
        );
        assert!(line.ends_with("s): seq 1 12; echo oops; exit 3"), "{line}");
        assert!(notices_of(&id, Audience::User).is_empty());

        // The model gets the block, once, marked as already shown.
        let told = notices_of(&id, Audience::Model);
        assert_eq!(told.len(), 1);
        assert!(told[0].announced);
        let block = told[0].block();
        assert!(
            block.starts_with(&format!(
                "<background-job id=\"{id}\" exit=\"3\" runtime=\""
            )),
            "{block}"
        );
        // The command, the end of the output, and where the rest is.
        assert!(
            block.contains(
                "s\">\ncommand: seq 1 12; echo oops; exit 3\n\
                 last 10 of 13 lines of output:\n4\n5\n"
            ),
            "{block}"
        );
        assert!(block.contains("\n12\noops\n</background-job>\n"), "{block}");
        assert!(block.contains("not a message from your user"), "{block}");
        assert!(block.contains(&format!("bash_output with id \"{id}\"")));
        assert!(notices_of(&id, Audience::Model).is_empty());
        assert!(notices_of(&id, Audience::User).is_empty());
        // The notice is not the output: that is still there to read.
        let out = BashOutputTool.call(&mut ctx, json!({"id": id})).unwrap();
        assert!(out.contains("\n1\n2\n3\n"), "{out}");

        // A job the model stops has nothing to announce.
        BashKillTool.call(&mut ctx, json!({"id": running})).unwrap();
        assert!(notices_of(&running, Audience::Model).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_job_whose_exit_the_model_read_leaves_no_notice() {
        let root = tempdir("bash-notice-read");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        // Read after it ended: the result said how it went.
        let id = start(&mut ctx, "echo all there is");
        let out = BashOutputTool
            .call(&mut ctx, json!({"id": id, "wait_ms": 20000}))
            .unwrap();
        assert!(out.contains("exit status: 0"), "{out}");
        assert!(notices_of(&id, Audience::User).is_empty());
        assert!(notices_of(&id, Audience::Model).is_empty());

        // Read while it ran, never after: its end is still news — to the
        // model directly, when the user was not shown it first.
        let id = start(
            &mut ctx,
            "echo early; while [ ! -e go ]; do sleep 0.05; done; echo late",
        );
        let out = BashOutputTool
            .call(
                &mut ctx,
                json!({"id": id, "wait_ms": 20000, "until": "early"}),
            )
            .unwrap();
        assert!(out.contains("running"), "{out}");
        std::fs::write(root.join("go"), "").unwrap();
        let told = notices_of(&id, Audience::Model);
        assert_eq!(told.len(), 1);
        assert!(!told[0].announced);
        assert!(
            told[0]
                .block()
                .contains("output:\nearly\nlate\n</background-job>")
        );
        assert!(notices_of(&id, Audience::User).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn notices_are_short_whatever_the_job_was() {
        assert_eq!(runtime(Duration::from_millis(900)), "0s");
        assert_eq!(runtime(Duration::from_secs(42)), "42s");
        assert_eq!(runtime(Duration::from_secs(187)), "3m07s");
        assert_eq!(clip("abc", 3), "abc");
        assert_eq!(clip("abcd", 3), "abc…");

        let notice = Notice {
            id: "job-7".into(),
            command: "true".into(),
            exit: "signal: 15 (SIGTERM)".into(),
            runtime: Duration::from_secs(61),
            tail: Vec::new(),
            lines: 0,
            announced: false,
        };
        assert_eq!(
            notice.line(),
            "⚙ job 7 finished (signal: 15 (SIGTERM), 1m01s): true"
        );
        assert!(
            notice
                .block()
                .contains("runtime=\"1m01s\">\ncommand: true\n(no output)\n</background-job>")
        );
    }

    /// The file a result with omitted output names, read back.
    fn spilled(root: &std::path::Path, out: &str) -> String {
        let (_, rest) = out.split_once("full output: ").expect("a marker");
        std::fs::read_to_string(root.join(rest.split_once(' ').unwrap().0)).unwrap()
    }

    #[test]
    fn long_output_comes_back_as_head_and_tail_with_the_rest_in_a_file() {
        let root = tempdir("bash-spill");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        let out = BashTool
            .call(&mut ctx, json!({"command": "seq 1 20000; echo oops >&2"}))
            .unwrap();
        assert!(out.len() < MAX_OUTPUT_BYTES, "{} bytes", out.len());
        assert!(
            out.starts_with("[exit status: exit status: 0]\n1\n2\n3\n"),
            "{out}"
        );
        // The end is where a command says how it went.
        assert!(out.ends_with("20000\n[stderr]\noops\n"), "{out}");
        assert!(out.contains(" lines omitted (lines "), "{out}");
        assert!(!out.contains("\n9000\n"));
        // The file holds the output itself, without the status line.
        let full = spilled(&root, &out);
        assert!(full.starts_with("1\n2\n") && full.ends_with("[stderr]\noops\n"));
        assert_eq!(full.lines().count(), 20002);

        // Short output, even of many lines, is returned whole.
        let out = BashTool
            .call(&mut ctx, json!({"command": "seq 1 500"}))
            .unwrap();
        assert!(out.contains("\n250\n") && !out.contains("omitted"), "{out}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn long_background_output_is_saved_the_same_way() {
        let root = tempdir("bash-bg-spill");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        let started = BashTool
            .call(
                &mut ctx,
                json!({"command": "seq 1 20000", "background": true}),
            )
            .unwrap();
        let id = started
            .split_whitespace()
            .find(|w| w.starts_with("job-"))
            .unwrap()
            .to_string();
        let out = BashOutputTool
            .call(&mut ctx, json!({"id": id, "wait_ms": 10000}))
            .unwrap();
        let (status, body) = out.split_once('\n').unwrap();
        assert!(status.contains("exit status"), "{status}");
        assert!(body.starts_with("1\n2\n") && body.ends_with("\n20000\n"));
        assert!(body.contains(" lines omitted (lines "), "{body}");
        assert_eq!(spilled(&root, &out).lines().count(), 20000);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A sandbox that says it is ready, for what is decided before any
    /// guest runs.
    fn ready() -> sandbox::State {
        sandbox::State::Ready(sandbox::Setup {
            rootfs: PathBuf::from("/nonexistent/rootfs"),
            memory_mb: 64,
            interp: false,
            helper: Some(PathBuf::from("/nonexistent/atelier")),
            notes: Vec::new(),
        })
    }

    #[test]
    fn only_a_foreground_command_in_a_ready_sandbox_runs_unasked() {
        let root = Path::new("/project");
        let plain = json!({"command": "make test"});
        let background = json!({"command": "make serve", "run_in_background": true});
        let host = json!({"command": "cargo test", "sandbox": false});
        // No sandbox: as ever.
        for args in [&plain, &background, &host] {
            assert!(BashTool.requires_approval_in(args, root));
        }
        {
            let _reset = sandbox::testing::set(ready());
            assert!(!BashTool.requires_approval_in(&plain, root));
            // `"sandbox": true` is the default spelled out.
            assert!(
                !BashTool.requires_approval_in(&json!({"command": "ls", "sandbox": true}), root)
            );
            assert!(BashTool.requires_approval_in(&background, root));
            assert!(BashTool.requires_approval_in(&host, root));
        }
        // On but unable to run: nothing is waved through.
        let _reset = sandbox::testing::set(sandbox::State::Unavailable("no root".into()));
        for args in [&plain, &background, &host] {
            assert!(BashTool.requires_approval_in(args, root));
        }
    }

    #[test]
    fn deny_and_ask_rules_outrank_the_sandbox() {
        use crate::permissions::{Decision, Policy};
        let _reset = sandbox::testing::set(ready());
        let root = Path::new("/project");
        let project = crate::settings::Permissions {
            allow: Vec::new(),
            ask: vec!["bash(git push *)".to_string()],
            deny: vec!["bash(rm -rf *)".to_string()],
            reviewer: None,
        };
        let policy = Policy::new(&project, &Default::default(), None);
        let denied = json!({"command": "echo hi && rm -rf build"});
        // The tool's own answer is "no need to ask"…
        assert!(!BashTool.requires_approval_in(&denied, root));
        // …which is only consulted when no rule decides.
        assert_eq!(
            policy.decide("bash", &denied, root).decision,
            Decision::Deny
        );
        let asked = json!({"command": "git push origin master"});
        assert_eq!(policy.decide("bash", &asked, root).decision, Decision::Ask);
        let free = json!({"command": "make test"});
        assert_eq!(
            policy.decide("bash", &free, root).decision,
            Decision::Default
        );
    }

    #[test]
    fn an_unavailable_sandbox_refuses_instead_of_using_the_host() {
        let root = tempdir("bash-sandbox-unavailable");
        let _reset = sandbox::testing::set(sandbox::State::Unavailable(
            "no root filesystem at /x".into(),
        ));
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: true,
        };
        let err = BashTool
            .call(&mut ctx, json!({"command": "echo ran > ran.txt"}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no root filesystem at /x"), "{err}");
        assert!(err.contains("\"sandbox\": false"), "{err}");
        assert!(!root.join("ran.txt").exists(), "it ran on the host");

        // The way out it names works, and says where the command ran.
        let out = BashTool
            .call(
                &mut ctx,
                json!({"command": "echo ran > ran.txt; echo ok", "sandbox": false}),
            )
            .unwrap();
        assert!(root.join("ran.txt").exists());
        assert!(
            out.ends_with("ok\n[ran on the host, not in the sandbox]"),
            "{out}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn with_the_sandbox_on_the_host_is_reached_only_by_an_approved_call() {
        let root = tempdir("bash-sandbox-host");
        let _reset = sandbox::testing::set(ready());
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        for args in [
            json!({"command": "echo ran > ran.txt", "sandbox": false}),
            json!({"command": "echo ran > ran.txt", "background": true}),
        ] {
            let err = BashTool.call(&mut ctx, args).unwrap_err().to_string();
            assert!(err.contains("needs approval"), "{err}");
        }
        std::thread::sleep(Duration::from_millis(100));
        assert!(!root.join("ran.txt").exists());

        // Approved, a background job starts — on the host, and says so.
        ctx.approved = true;
        let started = BashTool
            .call(&mut ctx, json!({"command": "true", "background": true}))
            .unwrap();
        assert!(started.contains("started background job"), "{started}");
        assert!(started.contains("runs on the host"), "{started}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_description_mentions_the_sandbox_only_while_it_is_on() {
        let plain = BashTool.spec();
        assert!(!plain.description.contains("sandbox"));
        let _reset = sandbox::testing::set(ready());
        let noted = BashTool.spec();
        assert!(noted.description.starts_with(&plain.description));
        assert!(noted.description.contains("sandbox: false"));
        // The schema is the same either way.
        assert_eq!(noted.parameters, plain.parameters);
    }

    /// Through a real guest: skipped without the feature, a root filesystem
    /// and the built binary (see `sandbox::testing::live_setup`).
    #[test]
    fn a_sandboxed_command_reports_like_one_on_the_host_and_blames_the_sandbox_when_due() {
        let Some(setup) = sandbox::testing::live_setup() else {
            return;
        };
        let root = std::fs::canonicalize(tempdir("bash-sandbox-live")).unwrap();
        let _reset = sandbox::testing::set(sandbox::State::Ready(setup));
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            // As the agent calls it; nothing here needed the user.
            approved: false,
        };
        let out = BashTool
            .call(
                &mut ctx,
                json!({"command": "pwd; uname -n; echo kept > kept.txt; echo warn >&2"}),
            )
            .unwrap();
        assert_eq!(
            out,
            format!(
                "[exit status: exit status: 0]\n{}\nnixvm\n[stderr]\nwarn\n",
                root.display()
            )
        );
        assert_eq!(
            std::fs::read_to_string(root.join("kept.txt")).unwrap(),
            "kept\n"
        );

        // A plain failure is reported plainly.
        let out = BashTool
            .call(&mut ctx, json!({"command": "echo no; exit 2"}))
            .unwrap();
        assert_eq!(out, "[exit status: exit status: 2]\nno\n");

        // A program the image doesn't have: the sandbox's doing, with the
        // way to the host.
        let out = BashTool
            .call(&mut ctx, json!({"command": "no-such-toolchain build"}))
            .unwrap();
        assert!(
            out.starts_with("[exit status: exit status: 127]\n"),
            "{out}"
        );
        assert!(
            out.contains("[sandbox: a program it needs is not in the sandbox's image"),
            "{out}"
        );
        assert!(out.contains("\"sandbox\": false"), "{out}");

        // Long output goes to a file in the project, as on the host.
        let out = BashTool
            .call(&mut ctx, json!({"command": "seq 1 20000"}))
            .unwrap();
        assert!(out.len() < MAX_OUTPUT_BYTES, "{} bytes", out.len());
        assert_eq!(spilled(&root, &out).lines().count(), 20000);

        // The timeout.
        let out = BashTool
            .call(&mut ctx, json!({"command": "sleep 30", "timeout_ms": 500}))
            .unwrap();
        assert!(out.starts_with("[command timed out after 500ms"), "{out}");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
