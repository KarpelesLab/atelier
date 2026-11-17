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
//! # Part of the output, without a pipe
//!
//! `tail`, `head` and `grep` are parameters of `bash` and `bash_output`,
//! not something to pipe a command into: see [`filter`], which also says
//! why, and how a filtered result is kept from hiding a failure.
//!
//! # A command the user can type to
//!
//! Where somebody is at an interface that can pass keys on, a foreground
//! command runs on a terminal of its own and can be handed the keyboard at
//! any point: see [`terminal`], and [`BashTool::call_on`], which is how the
//! agent runs `bash` when it has such an interface. Everywhere else — and
//! through [`Tool::call`] — a command runs on pipes with no stdin, as it
//! always did, and a result that ends at what looks like a prompt says so.
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

use super::filter::{self, Filter};
use super::terminal::{self, Console};
use super::{Tool, ToolCtx, ToolSpec};
use crate::sandbox;

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// Longest `bash_output` will block waiting.
const MAX_WAIT_MS: u64 = 600_000;
/// The most output, in bytes, a call returns; the rest goes to a file (see
/// [`super::spill`]).
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

/// The tool's name: the agent runs it itself, to give it the interface
/// (see [`BashTool::call_on`]).
pub const NAME: &str = "bash";

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
        NAME
    }

    /// A command the sandbox will run, in the foreground or as a background
    /// job, cannot leave the project, like the file tools, so it runs
    /// unasked (deny and ask rules still apply: they are judged before a
    /// tool's own default). Everything that runs on the host asks: with
    /// the sandbox off, a `"sandbox": false` request — and, so that nothing slips through, a
    /// sandbox that is on but unavailable, which [`call`](Tool::call) refuses.
    fn requires_approval_in(&self, args: &Value, root: &Path) -> bool {
        !(matches!(sandbox::state(root), sandbox::State::Ready(_)) && !wants_host(args))
    }

    fn spec(&self) -> ToolSpec {
        let mut description = "Run a shell command (via `sh -c`) in the project root. Captures \
                combined stdout/stderr and the exit status. Very long output comes back as its start and its end, with all of it saved \
                to a file you can read or grep; \
                the command is killed if it exceeds the timeout. Never pipe a command into \
                `tail`, `head` or `grep` to shorten its output: the pipe replaces the \
                command's exit status with the filter's and throws away the rest of the \
                output, which is how a failed run gets mistaken for a good one. Pass the \
                `tail`, `head` or `grep` parameter instead: the command runs as it is, its \
                own exit status is reported, and all of its output is kept, so you can look \
                at another part of it afterwards (`bash_output` with the id the result \
                names) without running it again. A command that stops to ask the user \
                something (a password, a 2FA code, a confirmation) can be answered by them \
                when they are at a terminal: pass `interactive: true` when you know it will \
                ask (a login, `sudo`, `ssh` to a new host, a release tool that wants a code), \
                and never try to answer such a prompt yourself or pipe an answer in. With \
                `background: true` it \
                starts as a job instead and returns its id at once. You are told when the \
                job ends (a <background-job> block), so do not poll it: go on with other \
                work. Read its output, or wait for it to print something, with \
                `bash_output`; stop it with `bash_kill`. Use that for servers, watchers, \
                and long builds."
                .to_string();
        // Only while the sandbox is on, so the description (which is part of
        // every request) is otherwise what it always was.
        match std::env::current_dir().map(|root| sandbox::state(&root)) {
            Ok(sandbox::State::Ready(setup)) => {
                description.push_str(&sandbox::tool_note(&setup.network));
            }
            Ok(sandbox::State::Unavailable(_)) => description.push_str(&sandbox::tool_note(&[])),
            Ok(sandbox::State::Off) | Err(_) => {}
        }
        let mut parameters = json!({
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
                    "interactive": {
                        "type": "boolean",
                        "description": "The command will need the user to type something (a password, a 2FA code, a login, an editor): they are given its terminal as it starts, with the time to answer. Commands that turn out to ask are noticed anyway; this is for when you know beforehand. Does not change what needs approval."
                    },
                    "sandbox": {
                        "type": "boolean",
                        "description": "Only when commands run in a sandbox: false runs this one on the host instead, which the user must approve. Use it after a result says the sandbox was in the way, not before."
                    }
                },
                "required": ["command"]
        });
        with_filter(&mut parameters);
        ToolSpec {
            name: self.name().to_string(),
            description,
            parameters,
        }
    }

    /// On pipes, with nobody to type to the command: what a caller without
    /// an interface gets (a script, a subagent, a test).
    fn call(&self, ctx: &mut ToolCtx, args: Value) -> Result<String> {
        self.call_on(ctx, args, None)
    }
}

/// Whether the call says the user will have to type to the command.
fn wants_interactive(args: &Value) -> bool {
    args.get("interactive").and_then(Value::as_bool) == Some(true)
}

impl BashTool {
    /// [`call`](Tool::call), with the interface when it is one the user can
    /// type to a command through (`console`): then a foreground command on
    /// the host runs on a terminal of its own ([`terminal`]). What needs
    /// approval is decided before this, and by nothing given here.
    pub fn call_on(
        &self,
        ctx: &mut ToolCtx,
        args: Value,
        console: Option<&mut dyn Console>,
    ) -> Result<String> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing required argument 'command'"))?;
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_MS);
        let filter = Filter::from_args(&args)?;
        let state = sandbox::state(ctx.project_root);
        let sandboxed = state != sandbox::State::Off;
        // With the sandbox on, the host is reached only through a call that
        // was approved as one (`requires_approval_in`).
        if sandboxed && wants_host(&args) && !ctx.approved {
            bail!("running on the host while the sandbox is on needs approval");
        }
        if wants_background(&args) {
            return match state {
                sandbox::State::Ready(setup) if !wants_host(&args) => {
                    let guest = sandbox::background(command, ctx.project_root, &setup)?;
                    start_job(command, guest)
                }
                sandbox::State::Unavailable(why) if !wants_host(&args) => bail!(
                    "the sandbox is enabled but cannot run commands: {why}. Nothing was \
                     started. To run this on the host instead, call bash again with \
                     \"sandbox\": false (the user will be asked to approve it)."
                ),
                _ => {
                    let started = start_job(command, host_shell(command, ctx.project_root))?;
                    Ok(if sandboxed {
                        format!("{started}\n[ran on the host, not in the sandbox]")
                    } else {
                        started
                    })
                }
            };
        }
        let timeout = Duration::from_millis(timeout_ms);
        match state {
            sandbox::State::Ready(setup) if !wants_host(&args) => {
                let out = sandbox::run(command, ctx.project_root, &setup, timeout)?;
                let mut out = format_sandboxed(ctx.project_root, timeout_ms, out, filter.as_ref());
                append(&mut out, filter::pipe_note(command));
                if wants_interactive(&args) {
                    append(&mut out, Some(format!("[{NOT_INTERACTIVE}: {IN_SANDBOX}]")));
                }
                return Ok(out);
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

        // Somebody can type to it: on a terminal, if one can be had.
        let mut no_terminal = NOBODY.to_string();
        if let Some(console) = console {
            match terminal::open() {
                Ok(term) => {
                    let interactive = wants_interactive(&args);
                    let done =
                        term.run(command, ctx.project_root, timeout, interactive, console)?;
                    let mut out = format_attended(ctx.project_root, timeout_ms, done, &filter);
                    append(&mut out, filter::pipe_note(command));
                    if sandboxed {
                        append(
                            &mut out,
                            Some("[ran on the host, not in the sandbox]".into()),
                        );
                    }
                    return Ok(out);
                }
                Err(e) => no_terminal = format!("no terminal could be opened for it ({e})"),
            }
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
            Some(status) => exited_line(status),
            None => "[exit status: unknown]\n".to_string(),
        };
        let failed = !done.status.is_some_and(|s| s.success());
        let asked = failed
            .then(|| prompt_left(&done.stdout, &done.stderr))
            .flatten();
        let output = combine(done.stdout, done.stderr);
        let mut out = format_result(ctx.project_root, status, output, filter.as_ref(), failed);
        append(&mut out, filter::pipe_note(command));
        if let Some(prompt) = asked {
            append(&mut out, Some(unanswered(&prompt, &no_terminal)));
        } else if wants_interactive(&args) {
            append(
                &mut out,
                Some(format!("[{NOT_INTERACTIVE}: {no_terminal}]")),
            );
        }
        if sandboxed {
            append(
                &mut out,
                Some("[ran on the host, not in the sandbox]".into()),
            );
        }
        Ok(out)
    }
}

/// Why a command had nobody to type to it.
const NOBODY: &str = "nobody is at a terminal in this mode";
const IN_SANDBOX: &str = "a command in the sandbox cannot be typed to yet";
/// What opens the note on a call that asked for `interactive` in vain.
const NOT_INTERACTIVE: &str = "interactive was asked for, but the command ran without a terminal";

/// The prompt a command that did not end well seems to have been left at:
/// the unfinished last line of what it printed, on either stream.
fn prompt_left(stdout: &[u8], stderr: &[u8]) -> Option<String> {
    [stderr, stdout]
        .iter()
        .find_map(|out| terminal::prompt_left(&String::from_utf8_lossy(out)))
}

/// The note on a result whose command asked something nobody could answer.
fn unanswered(prompt: &str, why: &str) -> String {
    format!(
        "[the command seems to have stopped to ask for input — its output ends with the \
         prompt {prompt:?} — and could not be answered: {why}. Do not try to answer it \
         yourself: tell the user, who can run it in atelier's terminal interface (where a \
         command can be typed to) or by hand.]"
    )
}

/// The result of a command that ran on a terminal the user could type to
/// (see [`terminal`]): one stream, and a line for each thing the model
/// cannot see for itself — that the user typed, that nobody did.
fn format_attended(
    root: &Path,
    timeout_ms: u64,
    done: terminal::Finished,
    filter: &Option<Filter>,
) -> String {
    let grace = terminal::ANSWER_GRACE.as_secs();
    let status = match (done.end, done.status) {
        (terminal::End::Stopped, _) => "[stopped by the user and killed]\n".to_string(),
        (terminal::End::TimedOut, _) if done.unanswered.is_some() => {
            format!("[command killed: it waited {grace}s for input that nobody gave]\n")
        }
        (terminal::End::TimedOut, _) => timed_out_line(timeout_ms),
        (_, Some(status)) => exited_line(status),
        (_, None) => "[exit status: unknown]\n".to_string(),
    };
    let failed = !done.status.is_some_and(|s| s.success());
    let mut out = format_result(root, status, done.output, filter.as_ref(), failed);
    if done.typed {
        append(
            &mut out,
            Some(
                "[the user typed to this command while it ran. What they typed, and the \
                 terminal's echo of it, is not part of this output]"
                    .into(),
            ),
        );
    }
    if let Some(prompt) = done.unanswered {
        append(
            &mut out,
            Some(format!(
                "[the command was waiting for input at the prompt {prompt:?}. The user was \
                 told and did not answer in time. Do not try to answer it yourself: tell \
                 the user, and run it again when they are there to type.]"
            )),
        );
    }
    out
}

/// Add the output filter's parameters to a tool's schema.
fn with_filter(parameters: &mut Value) {
    let Value::Object(filter) = filter::schema() else {
        return;
    };
    if let Some(properties) = parameters["properties"].as_object_mut() {
        properties.extend(filter);
    }
}

/// Add `note`, if there is one, to a result as a line of its own.
fn append(out: &mut String, note: Option<String>) {
    if let Some(note) = note {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&note);
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

/// The line a result opens with for a command that ended by itself: its
/// exit status, or the signal that ended it. One wording wherever the
/// command ran.
fn exited_line(status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("[exit status: {code}]\n"),
        // `signal: 15 (SIGTERM)`, as `ExitStatus` says it.
        None => format!("[killed by {status}]\n"),
    }
}

fn timed_out_line(timeout_ms: u64) -> String {
    format!("[command timed out after {timeout_ms}ms and was killed]\n")
}

/// The result of a command that ran in the sandbox: as for one on the host,
/// plus — when its failure looks like the sandbox's doing — a line saying so
/// and how to ask for the host.
fn format_sandboxed(
    root: &Path,
    timeout_ms: u64,
    out: sandbox::Output,
    filter: Option<&Filter>,
) -> String {
    let status = match &out.outcome {
        // As for a command on the host (`exited_line`).
        sandbox::Outcome::Exited(code) => format!("[exit status: {code}]\n"),
        sandbox::Outcome::TimedOut => timed_out_line(timeout_ms),
        sandbox::Outcome::Failed(_) => "[not run: the sandbox failed]\n".to_string(),
    };
    let blame = sandbox::diagnose(&out);
    let failed = !matches!(out.outcome, sandbox::Outcome::Exited(0));
    let asked = failed
        .then(|| prompt_left(&out.stdout, &out.stderr))
        .flatten();
    let output = combine(out.stdout, out.stderr);
    let mut text = format_result(root, status, output, filter, failed);
    if let Some(prompt) = asked {
        append(&mut text, Some(unanswered(&prompt, IN_SANDBOX)));
    }
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

/// What a command printed, as one text: its stdout, then — marked — its
/// stderr.
fn combine(stdout: Vec<u8>, stderr: Vec<u8>) -> String {
    let mut combined = String::from_utf8_lossy(&stdout).into_owned();
    if !stderr.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str("[stderr]\n");
        combined.push_str(&String::from_utf8_lossy(&stderr));
    }
    combined
}

/// `status` (a line saying how the command ended), then its output: what
/// `filter` selects of it, or — past the budget — its head and its tail.
/// All of it is in a file whenever some of it is not shown (see
/// [`filter::render`]).
fn format_result(
    root: &Path,
    status: String,
    output: String,
    filter: Option<&Filter>,
    failed: bool,
) -> String {
    let mut out = status;
    out.push_str(&filter::render(
        root,
        output,
        filter,
        failed,
        MAX_OUTPUT_BYTES,
    ));
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

/// `sh -c command` on the host, for a background job.
fn host_shell(command: &str, root: &std::path::Path) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// Start `cmd` — the process that runs `command`, on the host or in a guest
/// — as a background job.
fn start_job(command: &str, mut cmd: Command) -> Result<String> {
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
        let mut parameters = json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The job id from `bash` (e.g. \"job-1\"), or the id of a finished command's kept output (\"out-ab12cd\"). Omit to list jobs (or, when waiting, to use the latest job)."
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
        });
        with_filter(&mut parameters);
        ToolSpec {
            name: self.name().to_string(),
            description: "Get a background job's new output (since the last call) and its \
                status. With wait_ms, block until the job exits, or — with `until` — until \
                its new output matches that regex, or until wait_ms passes; use this to wait \
                for a build to finish or a server to come up. Without an id: with wait_ms or \
                `until`, the most recently started job; otherwise, list the jobs. Also looks \
                again at the kept output of a command that already ran: give the id a bash \
                result named (\"out-…\") with tail, head or grep to see another part of it, \
                instead of running the command again."
                .to_string(),
            parameters,
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
        let filter = Filter::from_args(&args)?;
        // The kept output of a command that has ended: another look at it.
        let named = args.get("id").and_then(Value::as_str);
        if let Some(kept) = named.and_then(filter::kept_id) {
            return look_again(ctx.project_root, kept, filter.as_ref());
        }
        let Some(id) = named.or(latest.as_deref()) else {
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
                // Asked before the output is read: a job that has ended is
                // reported once the last of what it printed is here.
                let settled = job.settled();
                let fresh = {
                    let out = job.output.lock().unwrap_or_else(|e| e.into_inner());
                    String::from_utf8_lossy(&out[job.seen..]).into_owned()
                };
                if let Some(re) = &until {
                    matched = re.is_match(&fresh);
                }
                if settled || matched || Instant::now() >= deadline {
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
                    let failed = match job.child.try_wait() {
                        Ok(Some(status)) => !status.success(),
                        _ => false,
                    };
                    // What the filter selects of it, or a long log's start
                    // and its end; the rest is in a file.
                    out.push_str(&filter::render(
                        ctx.project_root,
                        fresh,
                        filter.as_ref(),
                        failed,
                        MAX_OUTPUT_BYTES,
                    ));
                    return Ok(out);
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Another part of the output kept under `id` (see [`filter`]): what
/// `filter` selects of it, or its start and its end.
fn look_again(root: &Path, id: &str, filter: Option<&Filter>) -> Result<String> {
    let Some(text) = super::spill::kept(root, id) else {
        bail!(
            "no kept output \"out-{id}\": only the most recent ones are kept. Run the \
             command again (with tail, head or grep rather than a pipe)."
        );
    };
    let lines = text.lines().count();
    let mut out = format!("[out-{id}: kept output, {lines} lines]\n");
    out.push_str(&match filter {
        Some(filter) => filter::again(id, text, filter, MAX_OUTPUT_BYTES),
        // Asked for whole: its start and its end, as when it was made.
        None if text.len() > MAX_OUTPUT_BYTES => {
            let place = format!(
                "pass tail, head or grep to see another part, or read {}/{id}.txt with \
                 offset/limit",
                super::spill::DIR
            );
            super::spill::cut(&text, MAX_OUTPUT_BYTES, &place)
        }
        None => text,
    });
    Ok(out)
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
    fn the_status_line_says_once_how_the_command_ended() {
        let root = tempdir("bash-status");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        let mut run = |command: &str| {
            let args = json!({"command": command, "timeout_ms": 300});
            BashTool.call(&mut ctx, args).unwrap()
        };
        assert_eq!(run("echo hi"), "[exit status: 0]\nhi\n");
        assert_eq!(run("exit 3"), "[exit status: 3]\n");
        // A signal is not a status.
        assert_eq!(run("kill -TERM $$"), "[killed by signal: 15 (SIGTERM)]\n");
        assert_eq!(
            run("echo slow; sleep 20"),
            "[command timed out after 300ms and was killed]\nslow\n"
        );
        std::fs::remove_dir_all(&root).unwrap();
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
        assert!(out.starts_with("[exit status: 0]\n1\n2\n3\n"), "{out}");
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

    /// The id (`out-ab12cd`) a result gives for its kept output.
    fn kept_id(out: &str) -> String {
        let (_, rest) = out.split_once("bash_output with id \"").expect("a handle");
        rest.split_once('"').unwrap().0.to_string()
    }

    #[test]
    fn a_filter_shapes_what_is_shown_and_not_how_the_command_ran() {
        let root = tempdir("bash-filter");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        // The command's own status, which `| tail` would have replaced
        // with tail's 0; and all of what it printed, in the file.
        let out = BashTool
            .call(
                &mut ctx,
                json!({"command": "seq 1 300; exit 3", "tail": 25}),
            )
            .unwrap();
        let mut lines = out.lines();
        assert_eq!(lines.next(), Some("[exit status: 3]"));
        let header = lines.next().unwrap();
        assert!(
            header.starts_with(
                "[filtered: 25 of 300 lines shown (last 25) — full output: .atelier/tool-output/"
            ),
            "{header}"
        );
        assert_eq!(lines.next(), Some("276"));
        assert!(out.ends_with("\n299\n300\n"), "{out}");
        assert_eq!(spilled(&root, &out).lines().count(), 300);

        // The first lines, and the lines that match, the same way.
        let out = BashTool
            .call(&mut ctx, json!({"command": "seq 1 300", "head": 2}))
            .unwrap();
        assert!(out.ends_with("]\n1\n2\n"), "{out}");
        let out = BashTool
            .call(
                &mut ctx,
                json!({"command": "seq 1 300", "grep": "^7$", "context": 1}),
            )
            .unwrap();
        assert!(out.starts_with("[exit status: 0]\n[filtered: 3 of 300"));
        assert!(out.ends_with("]\n6\n7\n8\n"), "{out}");

        // Output the filter leaves whole is returned as ever.
        let out = BashTool
            .call(&mut ctx, json!({"command": "echo hi", "tail": 20}))
            .unwrap();
        assert_eq!(out, "[exit status: 0]\nhi\n");
        // A pattern that is not one is the call's mistake, before anything runs.
        let err = BashTool
            .call(&mut ctx, json!({"command": "touch ran", "grep": "("}))
            .unwrap_err();
        assert!(err.to_string().contains("invalid `grep` regex"), "{err}");
        assert!(!root.join("ran").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_filter_cannot_hide_a_failure() {
        let root = tempdir("bash-filter-failed");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        // The error is on stderr at the very end; the pattern only finds
        // progress lines.
        let failing = "seq 1 100 | sed 's/^/step /'; echo 'error: disk full' >&2; exit 2";
        let out = BashTool
            .call(&mut ctx, json!({"command": failing, "grep": "step 5$"}))
            .unwrap();
        assert!(
            out.starts_with("[exit status: 2]\n[filtered: 1 of 102 lines shown"),
            "{out}"
        );
        assert!(out.contains("\nstep 5\n[the command failed, and the filter left out the end"));
        assert!(
            out.ends_with("step 100\n[stderr]\nerror: disk full\n"),
            "{out}"
        );
        // A pattern that matches nothing does not read as "no errors".
        let out = BashTool
            .call(&mut ctx, json!({"command": failing, "grep": "warning"}))
            .unwrap();
        assert!(
            out.contains("[filtered: no line of the 102 lines of output matches \"warning\""),
            "{out}"
        );
        assert!(out.ends_with("error: disk full\n"), "{out}");
        // Nor does it on a command that succeeded: it says there were lines.
        let out = BashTool
            .call(&mut ctx, json!({"command": "seq 1 30", "grep": "error"}))
            .unwrap();
        let (status, rest) = out.split_once('\n').unwrap();
        assert_eq!(status, "[exit status: 0]");
        assert!(
            rest.starts_with("[filtered: no line of the 30 lines"),
            "{rest}"
        );
        assert_eq!(rest.lines().count(), 1);
        // Killed at the timeout: what it had printed, the same.
        let out = BashTool
            .call(
                &mut ctx,
                json!({"command": "seq 1 50; echo stuck; sleep 20", "timeout_ms": 500, "head": 1}),
            )
            .unwrap();
        assert!(out.starts_with("[command timed out after 500ms"), "{out}");
        assert!(out.ends_with("\n50\nstuck\n"), "{out}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn kept_output_is_looked_at_again_without_rerunning() {
        let root = tempdir("bash-filter-again");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        // Runs once: a second run would count twice.
        let command = "echo run >> runs; seq 1 200 | sed 's/^/row /'; echo 'row 77 FAILED'";
        let out = BashTool
            .call(&mut ctx, json!({"command": command, "tail": 2}))
            .unwrap();
        assert!(out.ends_with("row 200\nrow 77 FAILED\n"), "{out}");
        let id = kept_id(&out);
        assert!(id.starts_with("out-") && id.len() == 10, "{id}");

        // Another part of it: a pattern with context…
        let again = BashOutputTool
            .call(
                &mut ctx,
                json!({"id": id, "grep": "FAILED|row 42$", "context": 1}),
            )
            .unwrap();
        assert_eq!(
            again,
            format!(
                "[{id}: kept output, 201 lines]\n\
                 [filtered: 5 of 201 lines shown (grep \"FAILED|row 42$\" with 1 lines of \
                 context) — all of it is still {id}]\n\
                 row 41\nrow 42\nrow 43\n--\nrow 200\nrow 77 FAILED\n"
            )
        );
        // …the first lines, by the path the result gave…
        let path = format!(".atelier/tool-output/{}.txt", &id[4..]);
        let again = BashOutputTool
            .call(&mut ctx, json!({"id": path, "head": 1}))
            .unwrap();
        assert!(again.ends_with("]\nrow 1\n"), "{again}");
        // …and a range of lines, with `read`.
        let range = crate::tools::read::ReadTool
            .call(&mut ctx, json!({"path": path, "offset": 100, "limit": 2}))
            .unwrap();
        assert!(
            range.contains("row 100") && range.contains("row 101"),
            "{range}"
        );
        assert!(!range.contains("row 102"), "{range}");
        // Nothing of that made a new file, or ran the command again.
        let kept = std::fs::read_dir(root.join(".atelier/tool-output")).unwrap();
        assert_eq!(kept.count(), 1);
        assert_eq!(std::fs::read_to_string(root.join("runs")).unwrap(), "run\n");

        // An id that names nothing kept says what to do.
        let err = BashOutputTool
            .call(&mut ctx, json!({"id": "out-000000"}))
            .unwrap_err();
        assert!(err.to_string().contains("no kept output"), "{err}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_background_jobs_output_takes_the_same_filter() {
        let root = tempdir("bash-filter-bg");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        let id = start(&mut ctx, "seq 1 100; echo 'fatal: no' >&2; exit 1");
        let out = BashOutputTool
            .call(&mut ctx, json!({"id": id, "wait_ms": 20000, "head": 2}))
            .unwrap();
        let (status, rest) = out.split_once('\n').unwrap();
        assert!(status.contains("exit status: 1"), "{status}");
        assert!(
            rest.starts_with("[filtered: 2 of 101 lines shown (first 2)"),
            "{rest}"
        );
        // It failed: its end is shown although the filter asked for the start.
        assert!(rest.ends_with("\n100\nfatal: no\n"), "{rest}");
        // And all of it can be looked at again.
        let again = BashOutputTool
            .call(&mut ctx, json!({"id": kept_id(&out), "grep": "^50$"}))
            .unwrap();
        assert!(again.ends_with("]\n50\n"), "{again}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_pipe_into_tail_runs_as_written_and_is_told_about_the_parameter() {
        let root = tempdir("bash-pipe-note");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        // Not rewritten: the status is still the pipeline's, as sh has it.
        let out = BashTool
            .call(
                &mut ctx,
                json!({"command": "(seq 1 50; exit 3) | tail -n 2"}),
            )
            .unwrap();
        assert!(out.starts_with("[exit status: 0]\n49\n50\n"), "{out}");
        // A subshell is past what the parser follows: no advice, no harm.
        assert!(!out.contains("[note:"), "{out}");
        let out = BashTool
            .call(&mut ctx, json!({"command": "seq 1 50 | tail -n 2"}))
            .unwrap();
        let (run, note) = out.split_once("[note: ").expect("a note");
        assert_eq!(run, "[exit status: 0]\n49\n50\n");
        assert!(
            note.contains("The exit status reported is tail's"),
            "{note}"
        );
        assert!(note.contains("pass \"tail\": 2 to bash"), "{note}");
        // A pipe that transforms is the command's business.
        let out = BashTool
            .call(&mut ctx, json!({"command": "seq 1 50 | wc -l"}))
            .unwrap();
        assert_eq!(out, "[exit status: 0]\n50\n");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn both_tools_advertise_the_filter() {
        for spec in [BashTool.spec(), BashOutputTool.spec()] {
            for name in ["tail", "head", "grep", "context", "invert"] {
                assert!(
                    spec.parameters["properties"][name]["description"].is_string(),
                    "{}: {name}",
                    spec.name
                );
            }
        }
        let description = BashTool.spec().description;
        assert!(description.contains("Never pipe a command into `tail`, `head` or `grep`"));
        // What was there is still there.
        assert_eq!(BashTool.spec().parameters["required"], json!(["command"]));
        assert!(BashOutputTool.spec().parameters["properties"]["until"].is_object());
    }

    /// An interface with somebody at it, who does what the test says.
    #[derive(Default)]
    struct Typist {
        /// What to type when the command is seen to want input, and how
        /// long after.
        answer: Option<(Duration, Vec<u8>)>,
        /// Take the keyboard as the command starts, and keep it.
        attach: bool,
        /// Stop the turn this long after the command started.
        stop_after: Option<Duration>,
        // What was seen.
        started: Option<(Instant, bool)>,
        wanted: Vec<String>,
        wanted_at: Option<Instant>,
        screen: Vec<u8>,
        ended: bool,
    }

    impl Console for Typist {
        fn event(&mut self, event: terminal::CommandEvent) {
            match event {
                terminal::CommandEvent::Started { interactive, .. } => {
                    self.started = Some((Instant::now(), interactive));
                }
                terminal::CommandEvent::Output(bytes) => self.screen.extend_from_slice(bytes),
                terminal::CommandEvent::InputWanted { prompt } => {
                    self.wanted.push(prompt.to_string());
                    self.wanted_at = Some(Instant::now());
                }
                terminal::CommandEvent::Ended => self.ended = true,
            }
        }
        fn input(&mut self) -> Vec<terminal::CommandInput> {
            let mut input = Vec::new();
            if std::mem::take(&mut self.attach) {
                input.push(terminal::CommandInput::Attach);
            }
            let due = |(wait, _): &(Duration, Vec<u8>)| {
                self.wanted_at.is_some_and(|at| at.elapsed() >= *wait)
            };
            if let Some((_, keys)) = self.answer.take_if(|a| due(a)) {
                input.push(terminal::CommandInput::Attach);
                // Key by key, as a person types.
                input.extend(keys.iter().map(|k| terminal::CommandInput::Bytes(vec![*k])));
                input.push(terminal::CommandInput::Detach);
            }
            input
        }
        fn interrupted(&mut self) -> bool {
            let started = self.started.map(|(at, _)| at);
            self.stop_after
                .zip(started)
                .is_some_and(|(after, at)| at.elapsed() >= after)
        }
    }

    /// Run `args` as the agent does when the user can type to commands.
    fn attended(root: &Path, args: Value, typist: &mut Typist) -> String {
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: root,
            fstate: &mut fstate,
            approved: true,
        };
        BashTool.call_on(&mut ctx, args, Some(typist)).unwrap()
    }

    #[test]
    fn an_ordinary_command_on_a_terminal_reads_as_it_did_on_pipes() {
        if !terminal::supported() {
            return;
        }
        let root = tempdir("bash-tty-plain");
        let mut typist = Typist::default();
        let command = "echo out; echo err >&2; printf '\\033[1;31mred\\033[0m\\n'; \
                       printf ' 10%%\\r100%%\\n'; test -t 0 && echo \"tty $TERM $PAGER\"; exit 3";
        let out = attended(&root, json!({"command": command}), &mut typist);
        // One stream, in order; no colour codes, no carriage returns, the
        // progress line as it ended; and the command's own status.
        assert_eq!(out, "[exit status: 3]\nout\nerr\nred\n100%\ntty dumb cat\n");
        assert_eq!(typist.started.map(|(_, i)| i), Some(false));
        assert!(typist.ended && typist.wanted.is_empty());
        // The user's screen got it as the terminal did.
        let screen = String::from_utf8_lossy(&typist.screen);
        assert!(screen.contains("\x1b[1;31mred") && screen.contains("out\r\n"));

        // Filters, long output and the pipe note work the same.
        let out = attended(
            &root,
            json!({"command": "seq 1 300 | tail -n 250", "tail": 2}),
            &mut Typist::default(),
        );
        assert!(
            out.starts_with("[exit status: 0]\n[filtered: 2 of 250 lines shown"),
            "{out}"
        );
        assert!(
            out.contains("\n299\n300\n[note: this command pipes"),
            "{out}"
        );
        assert_eq!(spilled(&root, &out).lines().count(), 250);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_command_that_asks_is_answered_by_the_user_and_the_answer_stays_theirs() {
        if !terminal::supported() {
            return;
        }
        let root = tempdir("bash-tty-prompt");
        // As a release tool does it: on the terminal, not on stdin, and
        // only when there is one.
        let command = "echo Signing in; printf 'Enter the 6 digit code: ' > /dev/tty; \
                       read code < /dev/tty; echo \"Verifying $code...\"; \
                       [ \"$code\" = 424242 ] && echo signed in";
        let mut typist = Typist {
            answer: Some((Duration::ZERO, b"4242x\x7f42\r".to_vec())),
            ..Typist::default()
        };
        let out = attended(&root, json!({"command": command}), &mut typist);
        // Noticed without the model having said anything.
        assert_eq!(typist.wanted, ["Enter the 6 digit code:"]);
        // The user saw their code echoed; the model sees neither the echo
        // nor the code the program printed back.
        assert!(String::from_utf8_lossy(&typist.screen).contains("Verifying 424242"));
        assert_eq!(
            out,
            "[exit status: 0]\nSigning in\nEnter the 6 digit code: \n\
             Verifying [typed]...\nsigned in\n\
             [the user typed to this command while it ran. What they typed, and the \
             terminal's echo of it, is not part of this output]"
        );
        assert!(!out.contains("4242"), "{out}");
        // Nor is it in anything the call left behind.
        assert!(!root.join(".atelier").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_typed_secret_is_not_in_the_kept_output_either() {
        if !terminal::supported() {
            return;
        }
        let root = tempdir("bash-tty-kept");
        let command = "printf 'Password: '; read p; seq 1 300; echo \"token for $p\"";
        let mut typist = Typist {
            answer: Some((Duration::ZERO, b"hunter2\r".to_vec())),
            ..Typist::default()
        };
        let out = attended(&root, json!({"command": command, "tail": 1}), &mut typist);
        assert!(out.contains("\ntoken for [typed]\n"), "{out}");
        let kept = spilled(&root, &out);
        assert!(kept.starts_with("Password: \n1\n2\n"), "{kept}");
        assert!(!kept.contains("hunter2") && !out.contains("hunter2"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn waiting_for_an_answer_is_not_on_the_commands_clock() {
        if !terminal::supported() {
            return;
        }
        let root = tempdir("bash-tty-clock");
        // Seen waiting about a second in; its 1.5s would be over before the
        // user, who takes two seconds, has typed.
        let mut typist = Typist {
            answer: Some((Duration::from_secs(2), b"y\r".to_vec())),
            ..Typist::default()
        };
        let command = "printf 'Continue? [y/N] '; read a; echo \"answered $a\"";
        let out = attended(
            &root,
            json!({"command": command, "timeout_ms": 1500}),
            &mut typist,
        );
        assert!(
            out.starts_with("[exit status: 0]\nContinue? [y/N] \nanswered y\n"),
            "{out}"
        );

        // With the keyboard in the user's hands no time counts at all.
        let mut typist = Typist {
            attach: true,
            ..Typist::default()
        };
        let out = attended(
            &root,
            json!({"command": "sleep 1; echo done", "timeout_ms": 200}),
            &mut typist,
        );
        assert_eq!(out, "[exit status: 0]\ndone\n");

        // Left alone, a command that asks nothing has the time it was given.
        let t0 = Instant::now();
        let out = attended(
            &root,
            json!({"command": "echo working; sleep 30", "timeout_ms": 300}),
            &mut Typist::default(),
        );
        assert_eq!(
            out,
            "[command timed out after 300ms and was killed]\nworking\n"
        );
        assert!(t0.elapsed() < Duration::from_secs(10));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn stopping_the_turn_kills_the_command() {
        if !terminal::supported() {
            return;
        }
        let root = tempdir("bash-tty-stop");
        let mut typist = Typist {
            stop_after: Some(Duration::from_millis(300)),
            ..Typist::default()
        };
        let t0 = Instant::now();
        let out = attended(
            &root,
            json!({"command": "echo started; sleep 30; echo never"}),
            &mut typist,
        );
        assert_eq!(out, "[stopped by the user and killed]\nstarted\n");
        assert!(t0.elapsed() < Duration::from_secs(10));
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Where a command can be seen reading its terminal, that is what
    /// counts: a read with no prompt is noticed, a line left unfinished by
    /// a command that is busy is not taken for a question.
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn a_command_is_seen_waiting_by_what_it_does_not_by_what_it_printed() {
        let root = tempdir("bash-tty-seen");
        let mut typist = Typist {
            answer: Some((Duration::ZERO, b"go\r".to_vec())),
            ..Typist::default()
        };
        let out = attended(
            &root,
            json!({"command": "echo ready; read x; echo \"got $x\""}),
            &mut typist,
        );
        assert_eq!(typist.wanted, [""]);
        assert!(out.contains("\nready\n\ngot go\n"), "{out}");

        let mut typist = Typist::default();
        let out = attended(
            &root,
            json!({"command": "printf 'Password: '; sleep 2; echo; echo no question"}),
            &mut typist,
        );
        assert!(typist.wanted.is_empty(), "{:?}", typist.wanted);
        assert!(out.ends_with("Password: \nno question\n"), "{out}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn with_nobody_to_answer_a_command_that_asks_is_reported_as_such() {
        let root = tempdir("bash-unattended");
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: false,
        };
        // No terminal, no stdin: the read fails at once, as it always did.
        let command = "printf 'Enter the 6 digit code: ' >&2; read code || exit 1; echo in";
        let out = BashTool
            .call(&mut ctx, json!({"command": command}))
            .unwrap();
        assert!(
            out.starts_with("[exit status: 1]\n[stderr]\nEnter the 6 digit code: \n"),
            "{out}"
        );
        assert!(
            out.contains(
                "[the command seems to have stopped to ask for input — its output ends with \
                 the prompt \"Enter the 6 digit code:\" — and could not be answered: nobody \
                 is at a terminal in this mode."
            ),
            "{out}"
        );
        // Asked to be interactive where it cannot be: said, not ignored.
        let out = BashTool
            .call(&mut ctx, json!({"command": "echo hi", "interactive": true}))
            .unwrap();
        assert_eq!(
            out,
            "[exit status: 0]\nhi\n[interactive was asked for, but the command \
             ran without a terminal: nobody is at a terminal in this mode]"
        );
        // A command that just fails, or succeeds with an unfinished line,
        // gets no such note.
        for command in ["echo 'error: nope'; exit 1", "printf 'done:'"] {
            let out = BashTool
                .call(&mut ctx, json!({"command": command}))
                .unwrap();
            assert!(!out.contains("ask for input"), "{out}");
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn interactive_is_not_a_way_around_approval() {
        let root = Path::new("/project");
        let plain = json!({"command": "sudo make install"});
        let interactive = json!({"command": "sudo make install", "interactive": true});
        assert!(BashTool.requires_approval_in(&plain, root));
        assert!(BashTool.requires_approval_in(&interactive, root));
        // Rules read the command, whatever else the call says.
        use crate::permissions::{Decision, Policy};
        let project = crate::settings::Permissions {
            allow: vec!["bash(make *)".to_string()],
            ask: Vec::new(),
            deny: vec!["bash(sudo *)".to_string()],
            reviewer: None,
        };
        let policy = Policy::new(&project, &Default::default(), None);
        assert_eq!(
            policy.decide("bash", &interactive, root).decision,
            Decision::Deny
        );
        let unlisted = json!({"command": "fastlane release", "interactive": true});
        assert_eq!(
            policy.decide("bash", &unlisted, root).decision,
            Decision::Default
        );
        // In the sandbox, which cannot be typed to, the flag changes nothing
        // either: the host still takes `"sandbox": false`, and approval.
        let _reset = sandbox::testing::set(ready());
        assert!(!BashTool.requires_approval_in(&interactive, root));
        let host = json!({"command": "sudo x", "interactive": true, "sandbox": false});
        assert!(BashTool.requires_approval_in(&host, root));
    }

    /// A sandbox that says it is ready, for what is decided before any
    /// guest runs.
    fn ready() -> sandbox::State {
        sandbox::State::Ready(sandbox::Setup {
            rootfs: PathBuf::from("/nonexistent/rootfs"),
            memory_mb: 64,
            interp: false,
            network: Vec::new(),
            helper: Some(PathBuf::from("/nonexistent/atelier")),
            notes: Vec::new(),
        })
    }

    #[test]
    fn only_what_a_ready_sandbox_runs_goes_unasked() {
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
            assert!(!BashTool.requires_approval_in(&background, root));
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
        let host = json!({"command": "echo ran > ran.txt", "sandbox": false});
        let err = BashTool.call(&mut ctx, host).unwrap_err().to_string();
        assert!(err.contains("needs approval"), "{err}");
        // A background job goes to the guest (which this made-up sandbox
        // cannot start), never to the host for want of one.
        let job = json!({"command": "echo ran > ran.txt", "background": true});
        assert!(BashTool.call(&mut ctx, job).is_err());
        std::thread::sleep(Duration::from_millis(100));
        assert!(!root.join("ran.txt").exists());

        // Approved for the host, a background job starts there, and says so.
        ctx.approved = true;
        let started = BashTool
            .call(
                &mut ctx,
                json!({"command": "true", "background": true, "sandbox": false}),
            )
            .unwrap();
        assert!(started.contains("started background job"), "{started}");
        assert!(started.contains("ran on the host"), "{started}");
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
                "[exit status: 0]\n{}\nnixvm\n[stderr]\nwarn\n",
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
        assert_eq!(out, "[exit status: 2]\nno\n");

        // A program the image doesn't have: the sandbox's doing, with the
        // way to the host.
        let out = BashTool
            .call(&mut ctx, json!({"command": "no-such-toolchain build"}))
            .unwrap();
        assert!(out.starts_with("[exit status: 127]\n"), "{out}");
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
