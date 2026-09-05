//! The `bash` tool: run a shell command in the project root with a timeout.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolSpec};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
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
                the command is killed if it exceeds the timeout."
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
                        "description": "Maximum time to allow the command to run, in milliseconds (default 120000)."
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
        };
        let out = BashTool
            .call(&mut ctx, json!({"command": "sleep 5", "timeout_ms": 100}))
            .unwrap();
        assert!(out.contains("timed out"));
    }
}
