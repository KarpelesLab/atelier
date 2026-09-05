//! Diagnostics provider: `cargo check` errors/warnings for Rust projects.
//!
//! Rust-only for now (gated on `root/Cargo.toml` existing). Runs `cargo check
//! --message-format short`, capped by a watchdog-thread timeout so a hung
//! `cargo` process can never block the agent loop indefinitely, and reports a
//! compact summary. Returns `None` on a clean build so a passing project
//! doesn't spend tokens saying so.

use super::{ContextItem, ContextProvider};
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

/// How long we let `cargo check` run before killing it.
const TIMEOUT: Duration = Duration::from_secs(60);
/// Max diagnostic lines included in the rendered item (errors first).
const MAX_LINES: usize = 15;

/// Reports `cargo check` diagnostics (errors/warnings, short format) as a
/// compact [`ContextItem`]. Returns `None` when `root` isn't a Rust project,
/// the build is clean, `cargo` can't be run, or the check times out having
/// produced nothing actionable.
pub struct DiagnosticsProvider;

impl ContextProvider for DiagnosticsProvider {
    fn name(&self) -> &str {
        "diagnostics"
    }

    fn gather(&self, root: &Path) -> Option<ContextItem> {
        if !root.join("Cargo.toml").is_file() {
            return None;
        }

        match run_cargo_check(root, TIMEOUT) {
            CheckOutcome::TimedOut => Some(ContextItem::new(
                "diagnostics",
                format!(
                    "cargo check did not finish within {}s and was killed (timed out)",
                    TIMEOUT.as_secs()
                ),
                180,
            )),
            CheckOutcome::Unavailable => None,
            CheckOutcome::Output(text) => {
                let summary = CheckSummary::parse(&text);
                if summary.errors == 0 && summary.warnings == 0 {
                    return None;
                }
                Some(ContextItem::new("diagnostics", summary.render(), 180))
            }
        }
    }
}

/// Result of attempting to run `cargo check`.
enum CheckOutcome {
    /// Combined stdout+stderr, process exited (any status — `cargo check`
    /// exits non-zero when there are errors, which is expected).
    Output(String),
    /// The watchdog killed the process before it finished.
    TimedOut,
    /// `cargo` is missing, failed to spawn, or something else went wrong
    /// such that we have nothing useful to report.
    Unavailable,
}

/// Runs `cargo check --message-format short` in `root`, capturing combined
/// stdout+stderr, and enforces `timeout` via a watchdog thread that kills the
/// child if it hasn't finished in time (mirroring the harness's own bash
/// tool timeout pattern).
fn run_cargo_check(root: &Path, timeout: Duration) -> CheckOutcome {
    let child: Child = match Command::new("cargo")
        .arg("check")
        .arg("--message-format")
        .arg("short")
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return CheckOutcome::Unavailable,
    };

    let child = Arc::new(Mutex::new(child));

    // Take the pipes out before anything else touches the child, and read
    // them on their own threads so a full pipe buffer can never deadlock
    // against `wait()`.
    let stdout_pipe = child.lock().ok().and_then(|mut c| c.stdout.take());
    let stderr_pipe = child.lock().ok().and_then(|mut c| c.stderr.take());

    let stdout_handle = thread::spawn(move || read_all(stdout_pipe));
    let stderr_handle = thread::spawn(move || read_all(stderr_pipe));

    let timed_out = Arc::new(AtomicBool::new(false));
    // Lets the main thread wake the watchdog early once `wait()` returns,
    // instead of always sleeping the full timeout.
    let done_pair = Arc::new((Mutex::new(false), Condvar::new()));

    let watchdog = {
        let child = Arc::clone(&child);
        let timed_out = Arc::clone(&timed_out);
        let done_pair = Arc::clone(&done_pair);
        thread::spawn(move || {
            let (lock, cvar) = &*done_pair;
            let guard = match lock.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            let (guard, wait_result) = match cvar.wait_timeout_while(guard, timeout, |done| !*done)
            {
                Ok(r) => r,
                Err(_) => return,
            };
            if wait_result.timed_out()
                && !*guard
                && let Ok(mut child) = child.lock()
                && let Ok(None) = child.try_wait()
            {
                timed_out.store(true, Ordering::SeqCst);
                let _ = child.kill();
            }
        })
    };

    let status = match child.lock() {
        Ok(mut guard) => guard.wait(),
        Err(_) => return CheckOutcome::Unavailable,
    };

    {
        let (lock, cvar) = &*done_pair;
        if let Ok(mut done) = lock.lock() {
            *done = true;
            cvar.notify_all();
        }
    }
    let _ = watchdog.join();

    let stdout = stdout_handle.join().unwrap_or_default();
    let stderr = stderr_handle.join().unwrap_or_default();

    if timed_out.load(Ordering::SeqCst) {
        return CheckOutcome::TimedOut;
    }

    match status {
        Ok(_) => {
            let mut combined = stdout;
            if !stderr.is_empty() {
                if !combined.is_empty() {
                    combined.push('\n');
                }
                combined.push_str(&stderr);
            }
            CheckOutcome::Output(combined)
        }
        Err(_) => CheckOutcome::Unavailable,
    }
}

fn read_all(pipe: Option<impl Read>) -> String {
    let mut buf = String::new();
    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_string(&mut buf);
    }
    buf
}

/// Parsed counts and matched lines from a `cargo check --message-format
/// short` run.
#[derive(Debug, Default, PartialEq, Eq)]
struct CheckSummary {
    errors: usize,
    warnings: usize,
    /// Matched error lines, in output order.
    error_lines: Vec<String>,
    /// Matched warning lines, in output order.
    warning_lines: Vec<String>,
}

impl CheckSummary {
    /// Parses `cargo check` output, collecting lines of the short-format
    /// shape `path:line:col: error[..]: msg` / `path:line:col: warning: msg`.
    /// Non-diagnostic lines (notes, summary lines like "aborting due to N
    /// previous errors", blank lines) are ignored so counts reflect actual
    /// diagnostics rather than being inflated by the trailing summary.
    fn parse(output: &str) -> Self {
        let mut summary = CheckSummary::default();
        for line in output.lines() {
            match diagnostic_kind(line) {
                Some(Kind::Error) => summary.error_lines.push(line.to_string()),
                Some(Kind::Warning) => summary.warning_lines.push(line.to_string()),
                None => {}
            }
        }
        summary.errors = summary.error_lines.len();
        summary.warnings = summary.warning_lines.len();
        summary
    }

    /// Renders the one-line summary plus up to [`MAX_LINES`] diagnostic
    /// lines (errors first), with a "+K more" note when truncated.
    fn render(&self) -> String {
        let mut body = format!(
            "{} error(s), {} warning(s) from cargo check\n",
            self.errors, self.warnings
        );

        let total = self.errors + self.warnings;
        let shown: Vec<&String> = self
            .error_lines
            .iter()
            .chain(self.warning_lines.iter())
            .take(MAX_LINES)
            .collect();
        for line in &shown {
            body.push_str(line);
            body.push('\n');
        }
        if total > shown.len() {
            body.push_str(&format!("+{} more\n", total - shown.len()));
        }

        body.trim_end().to_string()
    }
}

enum Kind {
    Error,
    Warning,
}

/// Classifies a single line of `cargo check --message-format short` output.
///
/// Matches the shape `<path>:<line>:<col>: error...` / `...: warning...`,
/// requiring the two colon-separated fields right before the marker to be
/// numeric (the line/col) so we don't match unrelated lines that merely
/// contain the words "error" or "warning" (notes, summary lines, message
/// text).
fn diagnostic_kind(line: &str) -> Option<Kind> {
    let (prefix, rest) = split_after_location(line)?;
    let _ = prefix;
    let rest = rest.trim_start();
    if rest.starts_with("error") {
        Some(Kind::Error)
    } else if rest.starts_with("warning") {
        Some(Kind::Warning)
    } else {
        None
    }
}

/// If `line` starts with `<anything>:<digits>:<digits>: `, returns the part
/// before that prefix and the remainder after it. Otherwise `None`.
fn split_after_location(line: &str) -> Option<(&str, &str)> {
    // Find "col: " preceded by ":line" preceded by a path. Scan from the
    // left for the first `: ` and verify the two prior colon-delimited
    // fields are both non-empty numbers.
    let idx = line.find(": ")?;
    let head = &line[..idx];
    let rest = &line[idx + 2..];

    let mut parts = head.rsplitn(3, ':');
    let col = parts.next()?;
    let row = parts.next()?;
    let path = parts.next()?;

    if path.is_empty() || row.is_empty() || col.is_empty() {
        return None;
    }
    if !row.chars().all(|c| c.is_ascii_digit()) || !col.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }

    Some((head, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_cargo_toml_returns_none() {
        let dir = std::env::temp_dir().join(format!(
            "atelier-diagnostics-nocargo-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let provider = DiagnosticsProvider;
        assert!(provider.gather(&dir).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_errors_and_warnings_only() {
        let output = "\
src/main.rs:10:5: error[E0308]: mismatched types
  = note: expected type `u32`
             found type `&str`
src/lib.rs:3:1: warning: unused import: `std::fmt`
error: aborting due to 1 previous error; 1 warning emitted
";
        let summary = CheckSummary::parse(output);
        assert_eq!(summary.errors, 1);
        assert_eq!(summary.warnings, 1);
        assert_eq!(
            summary.error_lines,
            vec!["src/main.rs:10:5: error[E0308]: mismatched types".to_string()]
        );
        assert_eq!(
            summary.warning_lines,
            vec!["src/lib.rs:3:1: warning: unused import: `std::fmt`".to_string()]
        );
    }

    #[test]
    fn clean_output_has_no_diagnostics() {
        let summary = CheckSummary::parse("    Checking atelier v0.0.0\n    Finished dev\n");
        assert_eq!(summary.errors, 0);
        assert_eq!(summary.warnings, 0);
    }

    #[test]
    fn render_puts_errors_before_warnings_with_summary_line() {
        let output = "\
a.rs:1:1: warning: unused variable
b.rs:2:2: error[E0384]: cannot assign twice
";
        let summary = CheckSummary::parse(output);
        let body = summary.render();
        let summary_line = body.lines().next().unwrap();
        assert_eq!(summary_line, "1 error(s), 1 warning(s) from cargo check");
        let err_pos = body.find("b.rs:2:2").unwrap();
        let warn_pos = body.find("a.rs:1:1").unwrap();
        assert!(err_pos < warn_pos, "errors should be rendered first");
    }

    #[test]
    fn render_truncates_with_more_note() {
        let mut summary = CheckSummary::default();
        for i in 0..20 {
            summary.error_lines.push(format!("f.rs:{i}:1: error: e{i}"));
        }
        summary.errors = summary.error_lines.len();
        let body = summary.render();
        assert!(body.contains("+5 more"));
        assert_eq!(body.lines().count(), 1 + MAX_LINES + 1);
    }

    #[test]
    fn ignores_non_diagnostic_lines() {
        assert!(diagnostic_kind("    = note: this is not a real error").is_none());
        assert!(diagnostic_kind("error: aborting due to previous error").is_none());
        assert!(diagnostic_kind("warning: build finished with warnings").is_none());
        assert!(diagnostic_kind("random text mentioning error and warning").is_none());
    }

    #[test]
    fn matches_short_format_lines() {
        assert!(matches!(
            diagnostic_kind("src/foo.rs:12:34: error[E0433]: failed to resolve"),
            Some(Kind::Error)
        ));
        assert!(matches!(
            diagnostic_kind("src/foo.rs:1:1: warning: unused variable: `x`"),
            Some(Kind::Warning)
        ));
    }
}
