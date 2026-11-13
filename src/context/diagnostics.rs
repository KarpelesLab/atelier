//! Diagnostics provider: the project's own lint/typecheck commands, with
//! only what changed reported.
//!
//! Rather than speaking LSP, this runs the checkers a project already has —
//! `cargo check`, `tsc`, `go vet`, `ruff` — and feeds their output back each
//! turn, so the model sees what its last edit did.
//!
//! - **Which checkers.** The `[[diagnostics]]` entries of `atelier.toml`
//!   when there are any; otherwise whatever [`detect`] finds from the
//!   project's marker files, and only when the tool is already installed.
//!   Nothing is ever installed, and every auto-detected command is one that
//!   only reads the sources.
//! - **When.** A checker re-runs only if a file matching its `when` globs
//!   changed since its last run (a fingerprint of paths, sizes and mtimes,
//!   honoring `.gitignore`); otherwise its last result is reused, which the
//!   cross-turn dedup then drops. A checker without `when` runs every turn.
//! - **What is reported.** Each run is compared with the previous one, line
//!   numbers aside (an edit above a diagnostic moves it without changing
//!   it): diagnostics that are new come first, the unchanged remainder is
//!   only counted, and those that went away are counted as fixed. A clean
//!   project says nothing.
//! - **Failures.** A command runs under a watchdog that kills it (and, on
//!   Unix, everything it started) at its timeout, so a hung checker can't
//!   block the agent loop. A checker that can't start is reported once and
//!   dropped for the session; one that times out is reported once and tried
//!   again after the next change — a cold build cache is the usual cause —
//!   then dropped if it times out a second time running.
//! - **Trust.** A checker is a command the project chose — or, detected,
//!   the project's own build (`cargo check` runs build scripts) — so none
//!   runs, and nothing is probed, until the project is trusted (see
//!   [`crate::trust`]).

use super::{ContextItem, ContextProvider};
use crate::settings::DiagnosticCheck;
use crate::trust::Trust;
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use regex::Regex;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

/// How long an auto-detected checker may run before it is killed.
const TIMEOUT: Duration = Duration::from_secs(60);
/// Max diagnostic lines in the rendered item, shared between the checkers.
const MAX_LINES: usize = 15;
/// A checker's share of [`MAX_LINES`] never drops below this, however many
/// checkers there are.
const MIN_LINES: usize = 3;
/// Longest diagnostic line kept; a minified source line quoted in a message
/// shouldn't cost a paragraph.
const MAX_LINE_CHARS: usize = 300;
/// Lines kept from a failed run whose output has no recognizable
/// diagnostics (see [`parse`]).
const MAX_UNPARSED: usize = 40;
/// Consecutive timeouts after which a checker is dropped for the session.
const MAX_TIMEOUTS: u8 = 2;

/// Reports lint/typecheck diagnostics as a compact [`ContextItem`], or
/// `None` when there is no checker for the project or nothing to say.
pub struct DiagnosticsProvider {
    /// `[[diagnostics]]` from `atelier.toml`; empty means auto-detect.
    configured: Vec<DiagnosticCheck>,
    /// What each checker (by name) found last time.
    state: Mutex<HashMap<String, CheckerState>>,
    /// The Python that can `import pyflakes`, probed once: it costs a
    /// process, unlike the other presence checks.
    pyflakes: OnceLock<Option<&'static str>>,
    /// Whether the project's commands may run at all.
    trust: Trust,
}

impl DiagnosticsProvider {
    pub fn new(configured: Vec<DiagnosticCheck>, trust: Trust) -> Self {
        Self {
            trust,
            configured,
            state: Mutex::new(HashMap::new()),
            pyflakes: OnceLock::new(),
        }
    }

    /// The checkers for `root`: the configured ones, else the detected ones.
    fn checkers(&self, root: &Path) -> Vec<Checker> {
        if self.configured.is_empty() {
            return detect(root, &on_path, &|| {
                *self.pyflakes.get_or_init(|| python_with_pyflakes(root))
            });
        }
        self.configured
            .iter()
            .map(|c| Checker {
                name: c.name.clone(),
                command: c.command.clone(),
                when: c.when.clone(),
                timeout: Duration::from_secs(c.timeout_secs.max(1)),
            })
            .collect()
    }
}

impl ContextProvider for DiagnosticsProvider {
    fn name(&self) -> &str {
        "diagnostics"
    }

    fn gather(&self, root: &Path) -> Option<ContextItem> {
        // Asked every turn: `/trust` switches the checkers on mid-session.
        if !self.trust.active() {
            return None;
        }
        let checkers = self.checkers(root);
        if checkers.is_empty() {
            return None;
        }
        // A fixed share each, so a section rendered on one turn is still the
        // right size when it is reused on a later one.
        let budget = (MAX_LINES / checkers.len()).max(MIN_LINES);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let sections: Vec<String> = checkers
            .iter()
            .filter_map(|c| {
                state
                    .entry(c.name.clone())
                    .or_default()
                    .update(c, root, budget)
            })
            .collect();
        if sections.is_empty() {
            return None;
        }
        Some(ContextItem::new("diagnostics", sections.join("\n\n"), 180))
    }
}

/// One command whose output is diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Checker {
    name: String,
    /// Split with [`crate::shlex::split`] and run without a shell.
    command: String,
    /// Globs of the files it depends on; empty means "run every turn".
    when: Vec<String>,
    timeout: Duration,
}

impl Checker {
    fn auto(name: &str, command: &str, when: &[&str]) -> Self {
        Self {
            name: name.to_string(),
            command: command.to_string(),
            when: when.iter().map(|s| s.to_string()).collect(),
            timeout: TIMEOUT,
        }
    }
}

/// Picks checkers from the project's marker files. `has` says whether a
/// program is on `PATH`; `pyflakes` names the Python that can import
/// pyflakes, if any. Both are parameters so tests don't depend on what the
/// machine has installed.
fn detect(
    root: &Path,
    has: &dyn Fn(&str) -> bool,
    pyflakes: &dyn Fn() -> Option<&'static str>,
) -> Vec<Checker> {
    let mut found = Vec::new();
    if root.join("Cargo.toml").is_file() && has("cargo") {
        found.push(Checker::auto(
            "cargo check",
            "cargo check --message-format short",
            &["**/*.rs", "**/Cargo.toml"],
        ));
    }
    // `--no-install` and the local-binary test together mean npx can only
    // run the project's own compiler, never download one.
    if root.join("tsconfig.json").is_file()
        && root.join("node_modules/.bin/tsc").is_file()
        && has("npx")
    {
        found.push(Checker::auto(
            "tsc",
            "npx --no-install tsc --noEmit --pretty false",
            &[
                "**/*.ts",
                "**/*.tsx",
                "**/*.mts",
                "**/*.cts",
                "**/tsconfig*.json",
                "package.json",
            ],
        ));
    }
    if root.join("go.mod").is_file() && has("go") {
        found.push(Checker::auto(
            "go vet",
            "go vet ./...",
            &["**/*.go", "**/go.mod", "**/go.sum"],
        ));
    }
    if root.join("pyproject.toml").is_file() || root.join("setup.py").is_file() {
        let when = &["**/*.py", "pyproject.toml", "ruff.toml"];
        if has("ruff") {
            // `--no-cache`: ruff would otherwise write `.ruff_cache/` into
            // the project.
            found.push(Checker::auto(
                "ruff",
                "ruff check --no-cache --output-format concise",
                when,
            ));
        } else if let Some(python) = pyflakes() {
            // `-B`: no `__pycache__` either.
            found.push(Checker::auto(
                "pyflakes",
                &format!("{python} -B -m pyflakes ."),
                when,
            ));
        }
    }
    found
}

/// The commands detection could pick for `root`, going by its marker files
/// alone: nothing is run and what is installed is not looked at, so the
/// answer is the same on every machine. For the trust summary.
pub fn detectable(root: &Path) -> Vec<String> {
    let found = detect(root, &|_| true, &|| None);
    found.into_iter().map(|c| c.command).collect()
}

/// Whether `program` is an existing file in some `PATH` directory.
pub fn on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        dir.join(program).is_file()
            || (cfg!(windows)
                && ["exe", "cmd", "bat"]
                    .iter()
                    .any(|ext| dir.join(program).with_extension(ext).is_file()))
    })
}

/// The Python on `PATH` that can import pyflakes, if any. Importing it runs
/// none of the project's code.
fn python_with_pyflakes(root: &Path) -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|python| {
        on_path(python)
            && matches!(
                run_command(
                    root,
                    &format!("{python} -B -c 'import pyflakes'"),
                    Duration::from_secs(10)
                ),
                RunOutcome::Finished { success: true, .. }
            )
    })
}

/// What a checker found on its last runs, kept across turns.
#[derive(Default)]
struct CheckerState {
    /// Fingerprint of the `when` files at the last run. `None` before the
    /// first run, and always for a checker with no usable `when`.
    fingerprint: Option<u64>,
    /// Diagnostics of the last run that finished; `None` until one has.
    previous: Option<Vec<Diagnostic>>,
    /// The section rendered from the last run, reused while nothing changes.
    section: Option<String>,
    /// Timeouts in a row.
    timeouts: u8,
    /// Dropped for the session (couldn't start, or kept timing out).
    disabled: bool,
}

impl CheckerState {
    /// This checker's section for the turn: run it if its files changed,
    /// else hand back what the last run rendered.
    fn update(&mut self, checker: &Checker, root: &Path, budget: usize) -> Option<String> {
        if self.disabled {
            return None;
        }
        let fingerprint = fingerprint(root, &checker.when);
        if fingerprint.is_some() && fingerprint == self.fingerprint {
            return self.section.clone();
        }
        self.fingerprint = fingerprint;

        let name = &checker.name;
        match run_command(root, &checker.command, checker.timeout) {
            RunOutcome::Unavailable(why) => {
                self.disabled = true;
                self.section = None;
                Some(format!(
                    "{name}: could not run `{}` ({why}); skipped for this session",
                    checker.command
                ))
            }
            RunOutcome::TimedOut => {
                self.timeouts += 1;
                self.disabled = self.timeouts >= MAX_TIMEOUTS;
                // Said once: the section is not kept, so an unchanged tree
                // doesn't repeat it next turn.
                self.section = None;
                let secs = checker.timeout.as_secs_f32();
                Some(if self.disabled {
                    format!(
                        "{name}: timed out after {secs}s again and was killed; \
                         skipped for this session"
                    )
                } else {
                    format!(
                        "{name}: timed out after {secs}s and was killed; \
                         it will be tried again after the next change"
                    )
                })
            }
            RunOutcome::Finished { output, success } => {
                self.timeouts = 0;
                let current = parse(&output, success);
                self.section = render(name, &current, self.previous.as_deref(), budget);
                self.previous = Some(current);
                self.section.clone()
            }
        }
    }
}

/// A cheap fingerprint of the files matching `when` under `root`: their
/// paths, sizes and modification times, `.gitignore` honored. `None` when
/// `when` is empty or not valid globs — the checker then runs every turn,
/// which is the safe reading of a pattern we can't apply.
fn fingerprint(root: &Path, when: &[String]) -> Option<u64> {
    let matcher = globs(when)?;
    // Per-file hashes are summed, so the walk's order doesn't matter.
    let mut sum: u64 = 0;
    for entry in WalkBuilder::new(root).build().flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
        if !matcher.is_match(rel) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let mut h = std::collections::hash_map::DefaultHasher::new();
        rel.hash(&mut h);
        meta.len().hash(&mut h);
        meta.modified().ok().hash(&mut h);
        sum = sum.wrapping_add(h.finish());
    }
    Some(sum)
}

fn globs(patterns: &[String]) -> Option<GlobSet> {
    if patterns.is_empty() {
        return None;
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern).ok()?);
    }
    builder.build().ok()
}

/// Result of running a checker's command.
enum RunOutcome {
    /// It exited. `output` is stdout then stderr; `success` is its exit
    /// status (checkers exit non-zero when they find errors, as expected).
    Finished { output: String, success: bool },
    /// The watchdog killed it before it finished.
    TimedOut,
    /// It couldn't be started; the reason, for the one-line report.
    Unavailable(String),
}

/// Runs `command` in `root` with no shell and no stdin, capturing its
/// output, and enforces `timeout` via a watchdog thread that kills it if it
/// hasn't finished in time (mirroring the `bash` tool's timeout pattern).
fn run_command(root: &Path, command: &str, timeout: Duration) -> RunOutcome {
    let argv = match crate::shlex::split(command) {
        Ok(argv) if !argv.is_empty() => argv,
        Ok(_) => return RunOutcome::Unavailable("empty command".to_string()),
        Err(e) => return RunOutcome::Unavailable(e.to_string()),
    };
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Its own process group, so the watchdog reaches what it spawned too
    // (cargo's rustc, npx's node) — a survivor would hold the pipes open
    // and keep the readers below waiting.
    #[cfg(unixlike)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let child: Child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return RunOutcome::Unavailable(e.to_string()),
    };

    let child = Arc::new(Mutex::new(child));

    // Take the pipes out before anything else touches the child, and read
    // them on their own threads so a full pipe buffer can never stall the
    // child while we wait for it.
    let stdout_pipe = child.lock().ok().and_then(|mut c| c.stdout.take());
    let stderr_pipe = child.lock().ok().and_then(|mut c| c.stderr.take());

    let stdout_handle = thread::spawn(move || read_all(stdout_pipe));
    let stderr_handle = thread::spawn(move || read_all(stderr_pipe));

    let timed_out = Arc::new(AtomicBool::new(false));
    // Lets the main thread wake the watchdog early once the child is gone,
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
                #[cfg(unixlike)]
                let _ = Command::new("kill")
                    .args(["-KILL", "--", &format!("-{}", child.id())])
                    .stderr(Stdio::null())
                    .status();
                let _ = child.kill();
            }
        })
    };

    // Poll rather than block in `wait()`: that would hold the child's lock
    // for the whole run, and the watchdog could never take it to kill.
    let status = loop {
        let polled = match child.lock() {
            Ok(mut guard) => guard.try_wait(),
            Err(_) => break Err(std::io::Error::other("checker state lost")),
        };
        match polled {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(e) => break Err(e),
        }
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
        return RunOutcome::TimedOut;
    }

    match status {
        Ok(status) => {
            let mut output = stdout;
            if !stderr.is_empty() {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str(&stderr);
            }
            RunOutcome::Finished {
                output,
                success: status.success(),
            }
        }
        Err(e) => RunOutcome::Unavailable(e.to_string()),
    }
}

fn read_all(pipe: Option<impl Read>) -> String {
    let mut bytes = Vec::new();
    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_end(&mut bytes);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// One diagnostic line from a checker.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Diagnostic {
    /// The line as the checker printed it (cut at [`MAX_LINE_CHARS`]).
    line: String,
    /// The line without its line/column, which is what two runs are
    /// compared by: an edit above a diagnostic moves it, and a moved
    /// diagnostic is neither new nor fixed.
    key: String,
    warning: bool,
}

/// Extracts the diagnostics from a checker's output.
///
/// A diagnostic is a line that starts with a source location — see
/// [`locate`] — which is the one thing `cargo check --message-format short`,
/// `tsc --pretty false`, `go vet`, `ruff`, `pyflakes`, gcc and most linters'
/// "unix"/"compact" formats agree on. Everything else (notes, progress,
/// summaries like "aborting due to 2 previous errors") is dropped, so the
/// counts are of actual diagnostics.
///
/// A run that failed without printing any such line still has something to
/// say (a broken manifest, a linter with its own format), so its output is
/// kept as it is, up to [`MAX_UNPARSED`] lines, each counted as an error.
fn parse(output: &str, success: bool) -> Vec<Diagnostic> {
    let located: Vec<Diagnostic> = output.lines().filter_map(locate).collect();
    if !located.is_empty() || success {
        return located;
    }
    output
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .take(MAX_UNPARSED)
        .map(|l| Diagnostic {
            line: clip(l),
            key: l.to_string(),
            warning: false,
        })
        .collect()
}

/// Reads `line` as `<path>:<line>[:<col>]: <message>` or
/// `<path>(<line>[,<col>]): <message>` (tsc). The message decides the kind:
/// one starting with "warning" is a warning; "note", "help" and "info" are
/// commentary on another diagnostic and are skipped; anything else is an
/// error, since most checkers don't label theirs.
fn locate(line: &str) -> Option<Diagnostic> {
    static SHAPE: OnceLock<Regex> = OnceLock::new();
    let shape = SHAPE.get_or_init(|| {
        Regex::new(r"^(.+?)(?::\d+(?::\d+)?|\(\d+(?:,\d+)?\)): (.*)$").expect("valid regex")
    });
    let caps = shape.captures(line.trim_end())?;
    let (path, message) = (&caps[1], caps[2].trim_start());
    let kind = message.to_ascii_lowercase();
    if ["note", "help", "info"].iter().any(|k| kind.starts_with(k)) {
        return None;
    }
    Some(Diagnostic {
        line: clip(line.trim_end()),
        key: format!("{path}: {message}"),
        warning: kind.starts_with("warning"),
    })
}

fn clip(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_CHARS {
        return line.to_string();
    }
    let mut cut: String = line.chars().take(MAX_LINE_CHARS).collect();
    cut.push('…');
    cut
}

/// Renders a checker's section: a count line, then the diagnostics worth
/// showing (errors before warnings, at most `budget` lines).
///
/// With no `previous` run, that is all of them. With one, only those that
/// are new since it; the rest are counted, and so are the ones that went
/// away. `None` when there is nothing to say: no diagnostics now and none
/// fixed.
fn render(
    name: &str,
    current: &[Diagnostic],
    previous: Option<&[Diagnostic]>,
    budget: usize,
) -> Option<String> {
    // What of `current` was already there, matched one for one: the same
    // message twice in a file is two diagnostics.
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for d in previous.unwrap_or_default() {
        *seen.entry(d.key.as_str()).or_default() += 1;
    }
    let mut fresh: Vec<&Diagnostic> = Vec::new();
    for d in current {
        match seen.get_mut(d.key.as_str()) {
            Some(n) if *n > 0 => *n -= 1,
            _ => fresh.push(d),
        }
    }
    let unchanged = current.len() - fresh.len();
    let fixed = previous.map_or(0, |p| p.len() - unchanged);

    if current.is_empty() {
        return (fixed > 0).then(|| format!("{name}: clean — {}", fixed_note(fixed)));
    }

    let warnings = current.iter().filter(|d| d.warning).count();
    let mut body = format!(
        "{name}: {} error(s), {warnings} warning(s)\n",
        current.len() - warnings
    );
    if previous.is_some() && !fresh.is_empty() {
        body.push_str("new since last check:\n");
    }
    // Stable, so each kind keeps the checker's own order.
    fresh.sort_by_key(|d| d.warning);
    for d in fresh.iter().take(budget) {
        body.push_str(&d.line);
        body.push('\n');
    }
    if fresh.len() > budget {
        body.push_str(&format!("+{} more\n", fresh.len() - budget));
    }
    if unchanged > 0 {
        body.push_str(&format!("{unchanged} unchanged since last check\n"));
    }
    if fixed > 0 {
        body.push_str(&fixed_note(fixed));
    }
    Some(body.trim_end().to_string())
}

fn fixed_note(fixed: usize) -> String {
    let noun = if fixed == 1 {
        "diagnostic"
    } else {
        "diagnostics"
    };
    format!("{fixed} {noun} fixed since last check")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("atelier-diagnostics-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A provider in a trusted project.
    fn provider(dir: &Path, configured: Vec<DiagnosticCheck>) -> DiagnosticsProvider {
        DiagnosticsProvider::new(configured, crate::trust::testing::open(dir))
    }

    fn diags(output: &str) -> Vec<Diagnostic> {
        parse(output, false)
    }

    #[test]
    fn no_marker_files_means_no_checkers() {
        let dir = temp_dir("nomarkers");
        assert!(detect(&dir, &|_| true, &|| Some("python3")).is_empty());
        assert!(provider(&dir, Vec::new()).gather(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_checkers_only_when_their_tool_is_present() {
        let dir = temp_dir("detect");
        for f in ["Cargo.toml", "tsconfig.json", "go.mod", "pyproject.toml"] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        let names = |has: &dyn Fn(&str) -> bool, py: Option<&'static str>| -> Vec<String> {
            detect(&dir, has, &|| py)
                .into_iter()
                .map(|c| c.name)
                .collect()
        };

        // Nothing installed: nothing runs.
        assert!(names(&|_| false, None).is_empty());
        // tsc needs the project's own compiler, not just npx.
        assert_eq!(
            names(&|_| true, None),
            vec!["cargo check", "go vet", "ruff"]
        );
        std::fs::create_dir_all(dir.join("node_modules/.bin")).unwrap();
        std::fs::write(dir.join("node_modules/.bin/tsc"), "").unwrap();
        assert_eq!(
            names(&|_| true, None),
            vec!["cargo check", "tsc", "go vet", "ruff"]
        );
        // pyflakes is the fallback for a machine without ruff.
        let pyflakes = detect(&dir, &|p| p == "go", &|| Some("python3"));
        assert_eq!(pyflakes.last().unwrap().command, "python3 -B -m pyflakes .");
        assert_eq!(
            names(&|p| p == "go", None),
            vec!["go vet"],
            "no ruff, no pyflakes: no Python checker"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_cargo_short_format() {
        let output = "\
    Checking atelier v0.1.1 (/src/atelier)
src/main.rs:10:5: error[E0308]: mismatched types
  = note: expected type `u32`
             found type `&str`
src/lib.rs:3:1: warning: unused import: `std::fmt`
error: could not compile `atelier` (bin \"atelier\") due to 1 previous error; 1 warning emitted
";
        let found = diags(output);
        assert_eq!(found.len(), 2);
        assert_eq!(
            found[0].line,
            "src/main.rs:10:5: error[E0308]: mismatched types"
        );
        assert!(!found[0].warning);
        assert!(found[1].warning);
        assert_eq!(
            found[1].key,
            "src/lib.rs: warning: unused import: `std::fmt`"
        );
    }

    #[test]
    fn parses_tsc_go_vet_ruff_and_pyflakes() {
        let tsc = "\
src/app.ts(12,7): error TS2322: Type 'string' is not assignable to type 'number'.
src/util/fmt.tsx(3,1): error TS6133: 'React' is declared but its value is never read.
";
        let found = diags(tsc);
        assert_eq!(found.len(), 2);
        assert_eq!(
            found[0].key,
            "src/app.ts: error TS2322: Type 'string' is not assignable to type 'number'."
        );

        let go = "\
# example.com/app/internal/store
internal/store/db.go:41:2: unreachable code
vet: internal/store/tx.go:17:9: undefined: commit
";
        let found = diags(go);
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|d| !d.warning));

        let ruff = "\
app/main.py:1:8: F401 [*] `os` imported but unused
app/main.py:14:5: E722 Do not use bare `except`
Found 2 errors.
[*] 1 fixable with the `--fix` option.
";
        assert_eq!(diags(ruff).len(), 2);

        // No column.
        let pyflakes = "./app/main.py:1: 'os' imported but unused\n";
        assert_eq!(
            diags(pyflakes)[0].key,
            "./app/main.py: 'os' imported but unused"
        );
    }

    #[test]
    fn ignores_non_diagnostic_lines() {
        for line in [
            "    = note: this is not a real error",
            "error: aborting due to previous error",
            "warning: build finished with warnings",
            "random text mentioning error and warning",
            "src/a.rs:3:1: note: the lint level is defined here",
        ] {
            assert!(locate(line).is_none(), "{line}");
        }
        // A clean run's chatter isn't diagnostics either.
        assert!(parse("    Checking x v0.0.0\n    Finished dev\n", true).is_empty());
    }

    #[test]
    fn failed_run_without_locations_keeps_its_output() {
        let output = "error: failed to parse manifest at `/p/Cargo.toml`\n\nCaused by:\n  bad\n";
        let found = parse(output, false);
        assert_eq!(found.len(), 3);
        assert!(found.iter().all(|d| !d.warning));
        // The same text from a run that succeeded is just noise.
        assert!(parse(output, true).is_empty());
    }

    #[test]
    fn first_render_lists_errors_before_warnings() {
        let current = diags(
            "a.rs:1:1: warning: unused variable\nb.rs:2:2: error[E0384]: cannot assign twice\n",
        );
        let body = render("cargo check", &current, None, MAX_LINES).unwrap();
        assert_eq!(
            body,
            "cargo check: 1 error(s), 1 warning(s)\n\
             b.rs:2:2: error[E0384]: cannot assign twice\n\
             a.rs:1:1: warning: unused variable"
        );
        // Nothing wrong and nothing before: nothing to say.
        assert!(render("cargo check", &[], None, MAX_LINES).is_none());
        assert!(render("cargo check", &[], Some(&[]), MAX_LINES).is_none());
    }

    #[test]
    fn render_truncates_with_more_note() {
        let output: String = (0..20)
            .map(|i| format!("f.rs:{i}:1: error: e{i}\n"))
            .collect();
        let body = render("lint", &diags(&output), None, MAX_LINES).unwrap();
        assert!(body.contains("+5 more"));
        assert_eq!(body.lines().count(), 1 + MAX_LINES + 1);
    }

    #[test]
    fn later_render_reports_only_the_change() {
        let before =
            diags("a.rs:1:1: error: one\na.rs:5:1: error: two\nb.rs:9:9: warning: three\n");
        // "one" is fixed, "two" moved down two lines, "four" is new.
        let after =
            diags("a.rs:7:1: error: two\nb.rs:9:9: warning: three\nc.rs:1:1: error: four\n");
        let body = render("cargo check", &after, Some(&before), MAX_LINES).unwrap();
        assert_eq!(
            body,
            "cargo check: 2 error(s), 1 warning(s)\n\
             new since last check:\n\
             c.rs:1:1: error: four\n\
             2 unchanged since last check\n\
             1 diagnostic fixed since last check"
        );

        // Nothing new: counts only.
        let body = render("cargo check", &before, Some(&before), MAX_LINES).unwrap();
        assert_eq!(
            body,
            "cargo check: 2 error(s), 1 warning(s)\n3 unchanged since last check"
        );

        // All gone: say so once rather than going silent.
        let body = render("cargo check", &[], Some(&before), MAX_LINES).unwrap();
        assert_eq!(
            body,
            "cargo check: clean — 3 diagnostics fixed since last check"
        );
    }

    #[test]
    fn duplicate_messages_are_matched_one_for_one() {
        let before = diags("a.rs:1:1: error: dup\n");
        let after = diags("a.rs:1:1: error: dup\na.rs:9:1: error: dup\n");
        let body = render("lint", &after, Some(&before), MAX_LINES).unwrap();
        assert!(
            body.contains("new since last check:\na.rs:9:1: error: dup"),
            "{body}"
        );
        assert!(body.contains("1 unchanged"), "{body}");
    }

    #[test]
    fn fingerprint_follows_matching_files_only() {
        let dir = temp_dir("fingerprint");
        std::fs::write(dir.join("a.ts"), "let a = 1;").unwrap();
        std::fs::write(dir.join("notes.md"), "x").unwrap();
        let when = vec!["**/*.ts".to_string()];

        let first = fingerprint(&dir, &when);
        assert!(first.is_some());
        std::fs::write(dir.join("notes.md"), "something else").unwrap();
        assert_eq!(fingerprint(&dir, &when), first, "unrelated file");
        std::fs::write(dir.join("a.ts"), "let a = 'changed';").unwrap();
        let second = fingerprint(&dir, &when);
        assert_ne!(second, first, "matching file changed");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/b.ts"), "").unwrap();
        assert_ne!(fingerprint(&dir, &when), second, "matching file added");

        // No patterns, or unusable ones: no fingerprint, so always run.
        assert!(fingerprint(&dir, &[]).is_none());
        assert!(fingerprint(&dir, &["a{".to_string()]).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unixlike)]
    fn lint(command: &str, when: &[&str]) -> DiagnosticCheck {
        DiagnosticCheck {
            name: "lint".to_string(),
            command: command.to_string(),
            when: when.iter().map(|s| s.to_string()).collect(),
            timeout_secs: 30,
        }
    }

    /// In an untrusted project nothing runs — not a configured checker, not
    /// a detected one — until the project is accepted, mid-session.
    #[cfg(unixlike)]
    #[test]
    fn nothing_runs_until_the_project_is_trusted() {
        let dir = temp_dir("untrusted");
        let state = temp_dir("untrusted-state");
        let command = "sh -c 'touch ran; echo a.rs:1:1: error: x; exit 1'";
        let settings = crate::settings::Settings {
            diagnostics: vec![lint(command, &[])],
            ..Default::default()
        };
        let store = crate::trust::Store {
            file: Some(state.join("trust.json")),
            ..Default::default()
        };
        let trust = Trust::load(&dir, &settings, store);
        let provider = DiagnosticsProvider::new(settings.diagnostics.clone(), trust.clone());
        assert!(provider.gather(&dir).is_none());
        assert!(!dir.join("ran").exists(), "the checker ran untrusted");

        trust.accept(&settings).unwrap();
        assert!(provider.gather(&dir).is_some());
        assert!(dir.join("ran").exists());

        // Detected checkers wait the same way.
        std::fs::write(dir.join("Cargo.toml"), "").unwrap();
        assert_eq!(detectable(&dir), ["cargo check --message-format short"]);
        let closed = crate::trust::testing::closed(&dir);
        let detected = DiagnosticsProvider::new(Vec::new(), closed);
        assert!(detected.gather(&dir).is_none());
        assert!(!dir.join("target").exists(), "cargo ran untrusted");
        for d in [dir, state] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[cfg(unixlike)]
    fn configured(command: &str, when: &[&str]) -> DiagnosticsProvider {
        provider(Path::new("."), vec![lint(command, when)])
    }

    /// The whole loop with a stand-in checker: it prints `diag.txt` and
    /// logs each run, so the test can tell a re-run from a reused result.
    #[cfg(unixlike)]
    #[test]
    fn reruns_only_on_change_and_reports_the_delta() {
        let dir = temp_dir("delta");
        let runs =
            || std::fs::read_to_string(dir.join("runs.log")).map_or(0, |s| s.lines().count());
        let provider = configured(
            "sh -c 'echo run >> runs.log; cat diag.txt; exit 1'",
            &["*.txt"],
        );

        std::fs::write(
            dir.join("diag.txt"),
            "a.c:1:1: error: one\na.c:2:1: error: two\n",
        )
        .unwrap();
        let first = provider.gather(&dir).unwrap().body;
        assert_eq!(
            first,
            "lint: 2 error(s), 0 warning(s)\na.c:1:1: error: one\na.c:2:1: error: two"
        );
        assert_eq!(runs(), 1);

        // Nothing changed: same text (dedup drops it), and no second run.
        assert_eq!(provider.gather(&dir).unwrap().body, first);
        assert_eq!(runs(), 1);

        std::fs::write(
            dir.join("diag.txt"),
            "a.c:4:1: error: two\na.c:9:1: error: three!\n",
        )
        .unwrap();
        assert_eq!(
            provider.gather(&dir).unwrap().body,
            "lint: 2 error(s), 0 warning(s)\n\
             new since last check:\n\
             a.c:9:1: error: three!\n\
             1 unchanged since last check\n\
             1 diagnostic fixed since last check"
        );
        assert_eq!(runs(), 2);

        // Fixed: said once, then silence once the tree moves on clean.
        let clean = configured("sh -c 'cat diag.txt'", &["*.txt"]);
        std::fs::write(dir.join("diag.txt"), "a.c:1:1: error: one\n").unwrap();
        assert!(clean.gather(&dir).is_some());
        std::fs::write(dir.join("diag.txt"), "").unwrap();
        assert_eq!(
            clean.gather(&dir).unwrap().body,
            "lint: clean — 1 diagnostic fixed since last check"
        );
        std::fs::write(dir.join("diag.txt"), "\n").unwrap();
        assert!(clean.gather(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_checker_that_cannot_start_is_reported_once() {
        let dir = temp_dir("nostart");
        let check = DiagnosticCheck {
            name: "lint".to_string(),
            command: "atelier-no-such-checker --flag".to_string(),
            when: Vec::new(),
            timeout_secs: 5,
        };
        let provider = provider(&dir, vec![check]);
        let body = provider.gather(&dir).unwrap().body;
        assert!(body.starts_with("lint: could not run `atelier-no-such-checker --flag`"));
        assert_eq!(body.lines().count(), 1);
        assert!(provider.gather(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unixlike)]
    #[test]
    fn a_checker_that_times_out_is_killed_and_reported_once() {
        let dir = temp_dir("timeout");
        std::fs::write(dir.join("a.txt"), "1").unwrap();
        let checker = Checker {
            name: "slow".to_string(),
            // The sleep is a grandchild: killing only `sh` would leave it
            // holding the output pipe for the full 30s.
            command: "sh -c 'sleep 30; echo done'".to_string(),
            when: vec!["*.txt".to_string()],
            timeout: Duration::from_millis(200),
        };
        let mut state = CheckerState::default();
        let started = std::time::Instant::now();

        let line = state.update(&checker, &dir, MAX_LINES).unwrap();
        assert!(
            line.contains("timed out") && line.contains("tried again"),
            "{line}"
        );
        assert_eq!(line.lines().count(), 1);
        // Unchanged tree: not re-run, not repeated.
        assert!(state.update(&checker, &dir, MAX_LINES).is_none());

        // A change earns one retry; a second timeout drops it for good.
        std::fs::write(dir.join("a.txt"), "changed").unwrap();
        let line = state.update(&checker, &dir, MAX_LINES).unwrap();
        assert!(line.contains("skipped for this session"), "{line}");
        std::fs::write(dir.join("a.txt"), "changed again").unwrap();
        assert!(state.update(&checker, &dir, MAX_LINES).is_none());

        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the kill didn't take"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
