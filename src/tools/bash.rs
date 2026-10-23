//! The `bash` tool: run a shell command in the project root with a timeout —
//! or in the background, as a job that `bash_output` reads (or waits on) and
//! `bash_kill` stops.
//!
//! Background jobs belong to the atelier process (a registry shared by every
//! call), run in their own process group so stopping one stops what it
//! spawned, and are killed when atelier exits ([`kill_all`]).

use std::collections::BTreeMap;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolSpec};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// Longest `bash_output` will block waiting.
const MAX_WAIT_MS: u64 = 600_000;
const MAX_OUTPUT_BYTES: usize = 20_000;
const MAX_OUTPUT_LINES: usize = 200;
/// How often the watchdog polls the child for exit while waiting for the
/// timeout to elapse.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

pub struct BashTool;

impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: "Run a shell command (via `sh -c`) in the project root. Captures \
                combined stdout/stderr and the exit status. Output is truncated if very large; \
                the command is killed if it exceeds the timeout. With `background: true` it \
                starts as a job instead and returns its id at once: read its output, or wait \
                for it to finish or print something, with `bash_output`; stop it with \
                `bash_kill`. Use that for servers, watchers, and long builds."
                .to_string(),
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
        // `run_in_background` is what Claude models know it as.
        let background = ["background", "run_in_background"]
            .iter()
            .any(|k| args.get(*k).and_then(Value::as_bool) == Some(true));
        if background {
            return start_job(command, ctx.project_root);
        }

        let mut child = Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(ctx.project_root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning shell command")?;

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
        let timeout = Duration::from_millis(timeout_ms);
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

        let stdout_buf = stdout_handle.join().unwrap_or_default();
        let stderr_buf = stderr_handle.join().unwrap_or_default();

        Ok(format_result(
            status, timed_out, timeout_ms, stdout_buf, stderr_buf,
        ))
    }
}

fn format_result(
    status: Option<std::process::ExitStatus>,
    timed_out: bool,
    timeout_ms: u64,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
) -> String {
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

    let (body, truncated) = truncate_output(combined);

    let mut out = String::new();
    if timed_out {
        out.push_str(&format!(
            "[command timed out after {timeout_ms}ms and was killed]\n"
        ));
    } else if let Some(status) = status {
        out.push_str(&format!("[exit status: {status}]\n"));
    } else {
        out.push_str("[exit status: unknown]\n");
    }
    out.push_str(&body);
    if truncated {
        out.push_str("\n[output truncated]");
    }
    out
}

/// Cap output to `MAX_OUTPUT_LINES` lines and `MAX_OUTPUT_BYTES` bytes,
/// reporting whether anything was cut.
fn truncate_output(s: String) -> (String, bool) {
    let mut truncated = false;
    let mut s = if s.lines().count() > MAX_OUTPUT_LINES {
        truncated = true;
        s.lines()
            .take(MAX_OUTPUT_LINES)
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        s
    };
    if s.len() > MAX_OUTPUT_BYTES {
        truncated = true;
        let mut end = MAX_OUTPUT_BYTES;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    (s, truncated)
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
        !matches!(self.child.try_wait(), Ok(None))
    }
}

/// Background jobs by id (`job-1`, `job-2`, …).
#[derive(Default)]
struct Jobs {
    next: u32,
    jobs: BTreeMap<String, Job>,
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
    #[cfg(unix)]
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
        },
    );
    Ok(format!(
        "started background job {id} (pid {pid}). Read its output or wait on it with \
         bash_output; stop it with bash_kill."
    ))
}

/// Stop a job's whole process group (TERM, then KILL if it lingers).
fn stop(job: &mut Job) {
    #[cfg(unix)]
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

    fn call(&self, _ctx: &mut ToolCtx, args: Value) -> Result<String> {
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
            let mut jobs = jobs();
            if jobs.jobs.is_empty() {
                return Ok("no background jobs".into());
            }
            let mut out = String::new();
            for (id, job) in jobs.jobs.iter_mut() {
                out.push_str(&format!("{id}  [{}]  {}\n", job.status(), job.command));
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
                    // A long log: show its end, which is what matters.
                    let (body, cut) = tail(fresh);
                    let mut out = format!(
                        "[{id}: {} after {}s]\n",
                        job.status(),
                        job.started.elapsed().as_secs()
                    );
                    if until.is_some() && !matched && !job.finished() {
                        out.push_str("[`until` pattern not seen yet]\n");
                    }
                    if cut {
                        out.push_str("[earlier output omitted]\n");
                    }
                    out.push_str(&body);
                    return Ok(out);
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// The last `MAX_OUTPUT_BYTES` of `s`, and whether anything was cut.
fn tail(s: String) -> (String, bool) {
    if s.len() <= MAX_OUTPUT_BYTES {
        return (s, false);
    }
    let mut start = s.len() - MAX_OUTPUT_BYTES;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    (s[start..].to_string(), true)
}

/// `bash_kill`: stop a background job (and everything it started).
pub struct BashKillTool;

impl Tool for BashKillTool {
    fn name(&self) -> &str {
        "bash_kill"
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

    #[test]
    fn tail_keeps_the_end() {
        let s = "x".repeat(MAX_OUTPUT_BYTES) + "END";
        let (t, cut) = tail(s);
        assert!(cut);
        assert!(t.ends_with("END"));
        assert_eq!(t.len(), MAX_OUTPUT_BYTES);
    }
}
