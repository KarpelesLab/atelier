//! Running shell commands inside [nixvm](https://github.com/KarpelesLab/nixvm)
//! (roadmap M17): a real Linux userland whose system calls are served by
//! nixvm's kernel, in this process, instead of the host's. Experimental and
//! opt-in — the `sandbox` cargo feature, then `[sandbox] enabled = true` or
//! `ATELIER_SANDBOX=1`. `docs/sandbox.md` has the user's side of this, what
//! was measured, and what is missing.
//!
//! # What the guest is given
//!
//! - `/`: the root filesystem the user provides (an extracted Alpine
//!   minirootfs, say), read-only, under a writable layer in memory that is
//!   thrown away when the command ends.
//! - The project directory, read-write, **at the same absolute path** as on
//!   the host, so the paths a command prints are the paths every other tool
//!   uses. The command starts there.
//! - `/tmp`, `/dev`, `/proc`, `/sys` of its own. No network. A small fixed
//!   environment, none of the host's.
//!
//! and nothing else of the host. Inside the project, what decides which
//! programs run on the *host* later stays read-only ([`protected_paths`]):
//! git's hooks and configuration, and `atelier.toml`.
//!
//! # One VM per command, in a process of its own
//!
//! [`run`] does not call nixvm in this process: it starts atelier again as
//! `atelier sandbox-exec …` ([`helper_main`]), which builds the guest, runs
//! the command with its output on its own stdout and stderr, and leaves the
//! outcome in a status file. That costs a few milliseconds and buys three
//! things. nixvm reports guest faults and its time limit on the process's
//! stderr, which would land in the middle of the terminal interface. A
//! command that outlives its timeout can always be killed, whatever state
//! the guest kernel is in. And a crash in the emulator ends that process,
//! not the session.
//!
//! Nothing outlives the command but what it wrote in the project: the next
//! one starts in a fresh guest, so a file left in `/tmp` or a package
//! installed with `apk` is gone. Background jobs are not sandboxed — a
//! server in one guest could not be reached from the next command's.
//!
//! # When the sandbox is in the way
//!
//! The guest has its own userland: the host's compilers and package managers
//! are not in it, and neither is the network. [`diagnose`] recognises the
//! usual shapes of a command failing *because of* that, and the `bash` tool
//! then says so and how to ask for a run on the host (`"sandbox": false`,
//! which the user approves).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::settings::{SandboxSettings, Settings};

/// The hidden subcommand [`run`] starts atelier with.
pub const HELPER: &str = "sandbox-exec";
/// `atelier sandbox-exec --probe` succeeds if this build can run a guest.
const PROBE: &str = "--probe";

/// Guest RAM ceiling when `[sandbox] memory_mb` doesn't say.
const DEFAULT_MEMORY_MB: u64 = 4096;
/// How long past its timeout the helper gets to stop the guest by itself
/// before it is killed.
const GRACE: Duration = Duration::from_secs(3);
/// The helper's exit status when the guest hit its time limit, and when the
/// sandbox itself failed. Only a convention for someone running it by hand:
/// [`run`] reads the status file.
const EXIT_TIMEOUT: u8 = 124;
const EXIT_FAILED: u8 = 125;

/// Whether `bash` commands go through the sandbox.
#[derive(Debug, Clone, PartialEq)]
pub enum State {
    /// Not asked for: commands run on the host, as ever.
    Off,
    /// Asked for, but it can't run here; why. Commands are refused rather
    /// than quietly run on the host.
    Unavailable(String),
    Ready(Setup),
}

/// What a sandboxed run needs to know.
#[derive(Debug, Clone, PartialEq)]
pub struct Setup {
    /// The guest's root filesystem: a directory holding a Linux userland.
    pub rootfs: PathBuf,
    /// Guest RAM ceiling, in MiB.
    pub memory_mb: u64,
    /// Use nixvm's software CPU even where hardware virtualization is there.
    pub interp: bool,
    /// The executable to start as the helper; `None` is this one.
    pub helper: Option<PathBuf>,
    /// Things about the configuration the user should hear once
    /// (`atelier sandbox status`).
    pub notes: Vec<String>,
}

/// The sandbox's state for the project at `project_root`, from
/// `ATELIER_SANDBOX`, the project's `atelier.toml` and the user-global one.
pub fn state(project_root: &Path) -> State {
    #[cfg(test)]
    {
        let _ = project_root;
        testing::current()
    }
    #[cfg(not(test))]
    {
        resolve(
            &|key| std::env::var(key).ok(),
            &project_settings(project_root, crate::trust::Store::from_env()),
            &user_settings(),
            default_rootfs(),
        )
    }
}

/// `[sandbox]` of the project's `atelier.toml`, as it may be acted on: its
/// `enabled` counts only in a project the user has trusted (see
/// [`crate::trust`]). Switching the sandbox on stops `bash` from asking;
/// switching it off undoes a sandbox the user turned on for themselves —
/// neither is for a file that came with the repository to decide.
///
/// The file is judged as it is on disk now, not as the session loaded it:
/// an edit made since (the agent's file tools can write `atelier.toml`)
/// changes the hash, and is not in effect until it has been accepted.
fn project_settings(project_root: &Path, store: crate::trust::Store) -> SandboxSettings {
    let settings = Settings::load(project_root).unwrap_or_default();
    let trust = crate::trust::Trust::load(project_root, &settings, store);
    trust.effective(&settings).sandbox
}

/// `[sandbox]` of the user-global `atelier.toml`.
fn user_settings() -> SandboxSettings {
    Settings::user_dir()
        .and_then(|dir| Settings::load(&dir).ok())
        .map(|s| s.sandbox)
        .unwrap_or_default()
}

/// Where the root filesystem is looked for when nothing names one:
/// `$XDG_DATA_HOME/atelier/sandbox/rootfs`, else under `~/.local/share`.
pub fn default_rootfs() -> Option<PathBuf> {
    let abs = |key: &str| {
        std::env::var_os(key)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    let base = abs("XDG_DATA_HOME").or_else(|| abs("HOME").map(|h| h.join(".local/share")))?;
    Some(base.join("atelier").join("sandbox").join("rootfs"))
}

fn truthy(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" => Some(false),
        _ => None,
    }
}

/// [`state`], with everything it reads passed in.
///
/// `ATELIER_SANDBOX` decides whether it is on, else the project's file, else
/// the user's. The root filesystem is `ATELIER_SANDBOX_ROOT`, else the
/// user's `[sandbox] root`, else `default_root` — never the project's file:
/// the root is what the guest can read, and a repository's `atelier.toml`
/// naming `/` would hand it the host's files.
fn resolve(
    env: &dyn Fn(&str) -> Option<String>,
    project: &SandboxSettings,
    user: &SandboxSettings,
    default_root: Option<PathBuf>,
) -> State {
    let env = |key: &str| env(key).filter(|v| !v.trim().is_empty());
    let enabled = env("ATELIER_SANDBOX")
        .and_then(|v| truthy(&v))
        .or(project.enabled)
        .or(user.enabled)
        .unwrap_or(false);
    if !enabled {
        return State::Off;
    }
    if let Some(why) = unsupported_build() {
        return State::Unavailable(why.to_string());
    }
    let rootfs = match rootfs(&env, user, default_root) {
        Ok(dir) => dir,
        Err(why) => return State::Unavailable(why),
    };
    let mut notes = Vec::new();
    if !project.network.is_empty() || !user.network.is_empty() {
        notes.push(
            "`[sandbox] network` is not implemented yet: the guest has no network".to_string(),
        );
    }
    if project.root.is_some() {
        notes.push(
            "`[sandbox] root` in the project's atelier.toml is ignored: set it in the \
             user-global file or ATELIER_SANDBOX_ROOT"
                .to_string(),
        );
    }
    State::Ready(Setup {
        rootfs,
        memory_mb: project
            .memory_mb
            .or(user.memory_mb)
            .unwrap_or(DEFAULT_MEMORY_MB)
            .clamp(64, 1 << 20),
        interp: env("ATELIER_SANDBOX_INTERP")
            .and_then(|v| truthy(&v))
            .unwrap_or(false),
        helper: None,
        notes,
    })
}

/// The root filesystem to use, or why there is none.
fn rootfs(
    env: &dyn Fn(&str) -> Option<String>,
    user: &SandboxSettings,
    default_root: Option<PathBuf>,
) -> Result<PathBuf, String> {
    let home = |path: &str| match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(rest))
            .unwrap_or_else(|| PathBuf::from(path)),
        None => PathBuf::from(path),
    };
    let dir = env("ATELIER_SANDBOX_ROOT")
        .or_else(|| user.root.clone())
        .map(|p| home(&p))
        .or(default_root)
        .ok_or("no root filesystem: set ATELIER_SANDBOX_ROOT (see docs/sandbox.md)")?;
    // Not `exists()`: in an Alpine root `/bin/sh` is a link to `/bin/busybox`,
    // which would be looked up on the host.
    if !dir.is_dir() || std::fs::symlink_metadata(dir.join("bin/sh")).is_err() {
        return Err(format!(
            "no root filesystem at {} (a directory holding a Linux userland, with \
             bin/sh; docs/sandbox.md says how to make one)",
            dir.display()
        ));
    }
    Ok(dir)
}

/// Why this build has no sandbox, if it hasn't.
fn unsupported_build() -> Option<&'static str> {
    if cfg!(sandbox) {
        None
    } else if cfg!(feature = "sandbox") {
        Some(
            "this build of atelier cannot mount a host directory in the sandbox (nixvm \
             needs a libc for that: the fullrust release builds have none)",
        )
    } else {
        Some("this atelier was built without the `sandbox` feature")
    }
}

/// How a sandboxed command ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The command ran and exited with this status (128 + the signal for one
    /// that was killed, as a shell reports it).
    Exited(i32),
    /// It was still running at the timeout, and was killed.
    TimedOut,
    /// The sandbox could not run it at all; why.
    Failed(String),
}

/// What nixvm noted about a run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Report {
    /// The CPU backend the guest ran on (`kvm`, `interp-x86_64`, …).
    pub backend: Option<String>,
    /// System calls (and `ioctl`-style subcommands) the guest made that
    /// nixvm does not implement, with how often.
    pub unsupported: Vec<String>,
}

/// A sandboxed command's result.
#[derive(Debug, Clone)]
pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub outcome: Outcome,
    pub report: Report,
}

/// Run `sh -c command` in a fresh guest with the project at `project_root`
/// mounted read-write at that same path, as the working directory, for at
/// most `timeout`. See the module documentation for what the guest sees.
///
/// `Err` is for not being able to start the helper process at all; a sandbox
/// that started and failed is [`Outcome::Failed`].
pub fn run(command: &str, project_root: &Path, setup: &Setup, timeout: Duration) -> Result<Output> {
    let helper = match &setup.helper {
        Some(path) => path.clone(),
        None => std::env::current_exe().context("locating the atelier executable")?,
    };
    let status_path = status_path();
    let mut flags = Vec::new();
    if setup.interp {
        flags.push("interp");
    }
    let child = Command::new(helper)
        .arg(HELPER)
        .arg(&status_path)
        .arg(&setup.rootfs)
        .arg(project_root)
        .arg(timeout.as_millis().to_string())
        .arg(setup.memory_mb.to_string())
        .arg(flags.join(","))
        .arg(command)
        // Nothing of the host's environment reaches the helper, so none of
        // nixvm's `NIXVM_*` switches (host networking among them) can be set
        // from outside.
        .env_clear()
        .current_dir(project_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting the sandbox process")?;
    // The guest's own time limit ends it at `timeout`; the helper is only
    // killed if that didn't work.
    let captured = crate::tools::bash::capture(child, timeout + GRACE)?;
    let status = std::fs::read_to_string(&status_path).ok();
    let _ = std::fs::remove_file(&status_path);
    let (outcome, report) = match status.as_deref().and_then(parse_status) {
        Some(parsed) => parsed,
        None if captured.timed_out => (Outcome::TimedOut, Report::default()),
        None => {
            let how = captured
                .status
                .map_or_else(|| "killed".to_string(), |s| s.to_string());
            let said = String::from_utf8_lossy(&captured.stderr);
            let said = said.trim();
            let tail = said
                .char_indices()
                .rev()
                .nth(599)
                .map_or(said, |(at, _)| &said[at..]);
            (
                Outcome::Failed(format!(
                    "the sandbox process ended without a result ({how}){}{tail}",
                    if tail.is_empty() { "" } else { ": " }
                )),
                Report::default(),
            )
        }
    };
    Ok(Output {
        stdout: captured.stdout,
        stderr: captured.stderr,
        outcome,
        report,
    })
}

/// A fresh path for a helper's status file. In the temporary directory, not
/// the project: the guest can write there.
fn status_path() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    std::env::temp_dir().join(format!(
        "atelier-sandbox-{}-{}-{nanos}.status",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// The status file's text for an outcome: one `key value` line each for the
/// backend and the unsupported calls, then the outcome.
fn format_status(outcome: &Outcome, report: &Report) -> String {
    let mut out = String::new();
    if let Some(backend) = &report.backend {
        out.push_str(&format!("backend {backend}\n"));
    }
    for call in &report.unsupported {
        out.push_str(&format!("unsupported {call}\n"));
    }
    out.push_str(&match outcome {
        Outcome::Exited(code) => format!("exit {code}\n"),
        Outcome::TimedOut => "timeout\n".to_string(),
        Outcome::Failed(why) => format!("error {}\n", why.replace('\n', " ")),
    });
    out
}

/// Read a status file back. `None` if it doesn't end in an outcome (the
/// helper died while writing it).
fn parse_status(text: &str) -> Option<(Outcome, Report)> {
    let mut report = Report::default();
    let mut outcome = None;
    for line in text.lines() {
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "backend" => report.backend = Some(value.to_string()),
            "unsupported" => report.unsupported.push(value.to_string()),
            "exit" => outcome = Some(Outcome::Exited(value.trim().parse().ok()?)),
            "timeout" => outcome = Some(Outcome::TimedOut),
            "error" => outcome = Some(Outcome::Failed(value.to_string())),
            _ => {}
        }
    }
    Some((outcome?, report))
}

/// The paths inside the project the guest may read but not change, relative
/// to `project_root`: what decides which programs run on the host later.
///
/// - `.git/hooks` and `.git/config` (hooks, `core.hooksPath`,
///   `core.fsmonitor`, aliases, filters: all run by the user's next `git`).
///   Where `.git` is not a directory — a linked worktree's or a submodule's
///   pointer file, or nothing yet — it is protected whole, since rewriting
///   it chooses the repository, hooks and all.
/// - `atelier.toml`: permission rules, hooks and the MCP servers to launch.
pub fn protected_paths(project_root: &Path) -> Vec<String> {
    let mut paths = if project_root.join(".git").is_dir() {
        vec![".git/hooks".to_string(), ".git/config".to_string()]
    } else {
        vec![".git".to_string()]
    };
    paths.push(crate::settings::FILE_NAME.to_string());
    paths
}

/// If a sandboxed command's failure looks like the sandbox's doing rather
/// than the command's, what to tell the model. `None` for a success, and for
/// a failure with nothing pointing at the sandbox (a failing test is just a
/// failing test).
pub fn diagnose(output: &Output) -> Option<String> {
    let code = match &output.outcome {
        Outcome::Exited(0) => return None,
        Outcome::Exited(code) => *code,
        Outcome::TimedOut => {
            let interpreted = output
                .report
                .backend
                .as_deref()
                .is_some_and(|b| b.starts_with("interp"));
            return interpreted.then(|| {
                "the sandbox ran this on its software CPU (no hardware virtualization \
                 here), which is far slower than the host"
                    .to_string()
            });
        }
        Outcome::Failed(why) => return Some(format!("the sandbox could not run this: {why}")),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let said = |needles: &[&str]| {
        needles
            .iter()
            .any(|n| stderr.contains(n) || stdout.contains(n))
    };
    let mut why = Vec::new();
    // What a shell exits with for a command it cannot find, or cannot run.
    if matches!(code, 126 | 127) {
        why.push(
            "a program it needs is not in the sandbox's image (the host's toolchains \
             are not: the guest has its own userland)",
        );
    }
    if said(&[
        "Network unreachable",
        "Network is unreachable",
        "bad address",
        "Could not resolve host",
        "Couldn't resolve host",
        "Temporary failure in name resolution",
        "Name does not resolve",
        "Name or service not known",
        "failed to lookup address",
        "EAI_AGAIN",
        "ENETUNREACH",
    ]) {
        why.push("the sandbox has no network");
    }
    if said(&["Read-only file system", "EROFS"]) {
        why.push(
            "the sandbox keeps .git/hooks, .git/config and atelier.toml read-only \
             (in a linked worktree, all of .git)",
        );
    }
    // The guest runs as root, which is never refused for want of permission:
    // this is the host refusing nixvm, which works as the user. The common
    // case is a file created read-only and then written through its still
    // open descriptor — how git writes every object (`git add`, `git commit`).
    if said(&["Permission denied", "os error 13"]) {
        why.push(
            "the sandbox could not write a file the way the command expected (it cannot \
             write to a file created read-only, as `git add` and `git commit` do, nor to \
             one the user running atelier may not write)",
        );
    }
    if said(&["not a git repository"]) {
        why.push(
            "git's data for this checkout may be outside the project (a linked worktree, \
             a submodule), where the sandbox cannot see it",
        );
    }
    if !output.report.unsupported.is_empty() {
        why.push("it made system calls the sandbox does not implement");
    } else if said(&[
        "os error 95",
        "os error 38",
        "Function not implemented",
        "Operation not supported",
        "Not supported",
    ]) {
        why.push("it asked the sandbox's kernel for something that is not implemented");
    }
    if matches!(code, 132 | 134 | 135 | 139) {
        why.push("it crashed, which inside the sandbox may be the emulation's fault");
    }
    if why.is_empty() {
        return None;
    }
    let mut text = why.join("; ");
    if !output.report.unsupported.is_empty() {
        text.push_str(&format!(" ({})", output.report.unsupported.join(", ")));
    }
    Some(text)
}

/// `atelier sandbox-exec …`: the process [`run`] starts. Builds the guest,
/// runs the command with its output on this process's stdout and stderr, and
/// writes the outcome to the status file. Returns the exit status.
pub fn helper_main(args: &[String]) -> u8 {
    if let [probe] = args
        && probe == PROBE
    {
        return if cfg!(sandbox) { 0 } else { EXIT_FAILED };
    }
    let [
        status_path,
        rootfs,
        project,
        timeout_ms,
        memory_mb,
        flags,
        command,
    ] = args
    else {
        eprintln!("atelier {HELPER}: not meant to be run by hand (see `atelier sandbox run`)");
        return EXIT_FAILED;
    };
    let spec = Spec {
        rootfs: Path::new(rootfs),
        project: Path::new(project),
        command,
        timeout: timeout_ms.parse().ok().map(Duration::from_millis),
        memory_mb: memory_mb.parse().unwrap_or(DEFAULT_MEMORY_MB),
        interp: flags.split(',').any(|f| f == "interp"),
        net: false,
        rw_root: false,
    };
    let (outcome, report) = execute(&spec);
    // `create_new`: never through a link someone left at that name.
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(status_path)
        .and_then(|mut f| {
            std::io::Write::write_all(&mut f, format_status(&outcome, &report).as_bytes())
        });
    if let Err(e) = written {
        eprintln!("atelier {HELPER}: writing {status_path}: {e}");
        return EXIT_FAILED;
    }
    exit_status(&outcome)
}

fn exit_status(outcome: &Outcome) -> u8 {
    match outcome {
        Outcome::Exited(code) => (*code & 0xff) as u8,
        Outcome::TimedOut => EXIT_TIMEOUT,
        Outcome::Failed(why) => {
            eprintln!("atelier: sandbox: {why}");
            EXIT_FAILED
        }
    }
}

/// One guest run, as the helper and `atelier sandbox run` describe it.
struct Spec<'a> {
    rootfs: &'a Path,
    project: &'a Path,
    command: &'a str,
    timeout: Option<Duration>,
    memory_mb: u64,
    interp: bool,
    /// Bridge guest sockets to the host's network. Only `atelier sandbox
    /// run --net`, which the user types: never a command of the agent's.
    net: bool,
    /// Mount the root filesystem itself read-write, so what the command
    /// installs stays. Only `atelier sandbox run --rw-root`.
    rw_root: bool,
}

/// Run `spec` in this process, the guest's output going straight to this
/// process's stdout and stderr.
fn execute(spec: &Spec) -> (Outcome, Report) {
    #[cfg(sandbox)]
    {
        match guest::exec(
            spec,
            Box::new(std::io::stdout()),
            Box::new(std::io::stderr()),
        ) {
            Ok(done) => done,
            Err(why) => (Outcome::Failed(why), Report::default()),
        }
    }
    #[cfg(not(sandbox))]
    {
        let _ = (
            spec.rootfs,
            spec.project,
            spec.command,
            spec.timeout,
            spec.memory_mb,
            spec.interp,
            spec.net,
            spec.rw_root,
        );
        let why = unsupported_build().unwrap_or("no sandbox in this build");
        (Outcome::Failed(why.to_string()), Report::default())
    }
}

const USAGE: &str = "usage: atelier sandbox status
       atelier sandbox run [--net] [--rw-root] [--interp] [--] <command…>

`run` executes a shell command in the sandbox, in the current directory, as
the `bash` tool would. Two switches are for preparing the root filesystem and
are never used for the agent's commands: --rw-root mounts it read-write, so
what the command installs stays, and --net gives the guest the host's network
(`atelier sandbox run --net --rw-root -- apk add git`).";

/// `atelier sandbox …`: say what the sandbox would do here, or run a command
/// in it by hand. Returns the exit status.
pub fn run_command(args: &[String]) -> Result<u8> {
    let root = std::env::current_dir()?;
    match args.split_first().map(|(a, rest)| (a.as_str(), rest)) {
        Some(("status", [])) => {
            for line in status_lines(&root) {
                println!("{line}");
            }
            Ok(0)
        }
        Some(("run", rest)) => {
            let (mut net, mut rw_root, mut interp) = (false, false, false);
            let mut words = rest;
            loop {
                match words.split_first() {
                    Some((flag, more)) if flag == "--net" => (net, words) = (true, more),
                    Some((flag, more)) if flag == "--rw-root" => (rw_root, words) = (true, more),
                    Some((flag, more)) if flag == "--interp" => (interp, words) = (true, more),
                    Some((flag, more)) if flag == "--" => {
                        words = more;
                        break;
                    }
                    _ => break,
                }
            }
            if words.is_empty() {
                anyhow::bail!(USAGE);
            }
            if let Some(why) = unsupported_build() {
                anyhow::bail!("{why}");
            }
            let user = user_settings();
            let rootfs = rootfs(
                &|key| std::env::var(key).ok().filter(|v| !v.trim().is_empty()),
                &user,
                default_rootfs(),
            )
            .map_err(|why| anyhow::anyhow!(why))?;
            let command = words.join(" ");
            let (outcome, _) = execute(&Spec {
                rootfs: &rootfs,
                project: &root,
                command: &command,
                timeout: None,
                memory_mb: user.memory_mb.unwrap_or(DEFAULT_MEMORY_MB),
                interp,
                net,
                rw_root,
            });
            Ok(exit_status(&outcome))
        }
        _ => anyhow::bail!(USAGE),
    }
}

/// What `atelier sandbox status` prints.
fn status_lines(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let written = Settings::load(root).map(|s| s.sandbox.enabled);
    let store = crate::trust::Store::from_env();
    if matches!(written, Ok(Some(_))) && project_settings(root, store).enabled.is_none() {
        out.push(
            "note: this project's `[sandbox] enabled` is not in effect — the project is not \
             trusted (/trust, or `atelier trust accept`)"
                .to_string(),
        );
    }
    match state(root) {
        State::Off => out.push(
            "sandbox: off (`[sandbox] enabled = true` in atelier.toml, or ATELIER_SANDBOX=1)"
                .to_string(),
        ),
        State::Unavailable(why) => out.push(format!("sandbox: enabled, but unavailable: {why}")),
        State::Ready(setup) => {
            out.push("sandbox: on — foreground `bash` commands run in nixvm".to_string());
            out.push(format!("root filesystem: {}", setup.rootfs.display()));
            out.push(format!("guest memory: {} MiB", setup.memory_mb));
            let kvm = !setup.interp
                && cfg!(all(target_os = "linux", target_arch = "x86_64"))
                && std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open("/dev/kvm")
                    .is_ok();
            out.push(if kvm {
                "cpu: hardware virtualization (KVM) for an x86-64 root".to_string()
            } else {
                "cpu: nixvm's software interpreter — expect commands to be far slower than \
                 on the host"
                    .to_string()
            });
            out.push(format!(
                "read-only in the project: {}",
                protected_paths(root).join(", ")
            ));
            out.push("network: none".to_string());
            out.extend(setup.notes.iter().map(|n| format!("note: {n}")));
        }
    }
    out
}

/// What the `bash` tool's description says about the sandbox while it is on.
pub const TOOL_NOTE: &str = " Commands run in a sandbox: a small Linux guest (its own \
    userland, typically Alpine with busybox) that sees the project directory at its usual \
    path and nothing else of this machine. There is no network; each command starts in a \
    fresh guest, so only files in the project persist between commands (not /tmp, not \
    installed packages); .git/hooks, .git/config and atelier.toml are read-only; programs \
    installed on the host are not available unless the sandbox's image has them too, and \
    `git add`, `git commit` and `cargo` do not work there yet (reading with git does). If a \
    command cannot work there, the result says so: run it again with `sandbox: false` to \
    use the host, which the user is asked to approve. Background jobs always run on the \
    host.";

#[cfg(sandbox)]
mod guest {
    //! Building a nixvm guest and running one command in it. nixvm 0.0.5's
    //! own entry points don't fit — `Sandbox::run` mounts the working
    //! directory at `/work`, starts in `/root`, wires the guest to the
    //! host's terminal and returns only an exit code — so this assembles the
    //! same pieces (mount table, loader, backend, kernel) itself.

    use std::io::{self, Write};
    use std::sync::Arc;

    use nixvm::abi::Arch;
    use nixvm::fs::blockdev::SharedDisks;
    use nixvm::fs::{
        Attrs, CgroupFs, DevFs, DirEntry, MountFs, MountTable, Overlay, Passthrough, ProcFs,
        SetTime, SysFs, TmpFs,
    };
    use nixvm::kernel::Kernel;
    use nixvm::limits::{Limits, Quota};
    use nixvm::loader::{ProcessSpec, interp_path, load_dynamic, load_static};
    use nixvm::vcpu::{Backend, GuestMemory};

    use super::{Outcome, Report, Spec};

    /// Where the guest's address space starts, and how far it extends: what
    /// nixvm 0.0.5 uses itself (its x86-64 interpreter faults with more).
    const GUEST_BASE: u64 = 0x1_0000;
    const GUEST_VSIZE: u64 = 32 << 30;
    const PAGE: u64 = 4096;
    /// What a guest killed for running out of time exits with (128 + KILL).
    const KILLED: i32 = 137;

    pub fn exec(
        spec: &Spec,
        stdout: Box<dyn Write + Send>,
        stderr: Box<dyn Write + Send>,
    ) -> Result<(Outcome, Report), String> {
        let project = spec
            .project
            .to_str()
            .filter(|p| p.starts_with('/') && p.len() > 1)
            .ok_or("the project path must be absolute and UTF-8")?;
        let project = project.trim_end_matches('/').to_string();

        let mut mounts = MountTable::new();
        if spec.rw_root {
            mounts.mount("/", Box::new(Passthrough::new(spec.rootfs)));
        } else {
            let lower = Box::new(Passthrough::read_only(spec.rootfs));
            mounts.mount("/", Box::new(Overlay::new(lower, Box::new(TmpFs::new()))));
        }
        mounts.mount("/tmp", Box::new(TmpFs::new()));
        mounts.mount("/dev", Box::new(DevFs::new()));
        mounts.mount("/proc", Box::new(ProcFs::new(1)));
        mounts.mount("/sys", Box::new(SysFs::new(1)));
        // The project, at its own path. The directories leading to it exist
        // only in the guest (the root's writable layer, or `/tmp`).
        mkdir_all(&mut mounts, &project);
        mounts.mount(
            project.clone(),
            Box::new(Guarded::new(
                Passthrough::new(spec.project),
                &super::protected_paths(spec.project),
            )),
        );
        for dir in ["/root", "/etc"] {
            mkdir_all(&mut mounts, dir);
        }
        if spec.net {
            let _ = mounts.create("/etc/resolv.conf", 0o644);
            let _ = mounts.truncate("/etc/resolv.conf", 0);
            let _ = mounts.write_at("/etc/resolv.conf", 0, resolv_conf().as_bytes());
        }

        let argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            spec.command.to_string(),
        ];
        let shell = mounts
            .follow("/bin/sh")
            .ok_or("the root filesystem has no /bin/sh")?;
        let elf = mounts
            .read_file(&shell)
            .ok_or("cannot read the root filesystem's /bin/sh")?;
        let arch = Arch::from_elf(&elf).ok_or("the root filesystem's /bin/sh is not an ELF")?;

        let ceiling = nixvm::limits::guest_memory_ceiling().max(64 << 20);
        let mem_bytes = (spec.memory_mb << 20).min(ceiling) / PAGE * PAGE;
        let mut mem = GuestMemory::new_split(GUEST_BASE, GUEST_VSIZE.max(mem_bytes), mem_bytes);
        mem.set_guest_arch(arch)
            .map_err(|e| format!("guest memory: {e:?}"))?;
        let process = ProcessSpec {
            argv: argv.clone(),
            envp: environment(&project),
        };
        let image = match interp_path(&elf) {
            Some(linker) => {
                let linker_elf = mounts
                    .read_file(&linker)
                    .ok_or_else(|| format!("the root filesystem has no {linker}"))?;
                load_dynamic(&mut mem, &elf, &linker_elf, &process)
            }
            None => load_static(&mut mem, &elf, &process),
        }
        .map_err(|e| format!("loading /bin/sh: {e}"))?;
        // The heap grows up from the program break and mappings down from
        // the stack; they meet halfway.
        let mid = image.program_break + (image.stack_bottom - image.program_break) / 2;
        let mid = mid - mid % PAGE;

        let backend = backend(arch, spec.interp)?;
        let vcpu = backend
            .new_vcpu(image.entry, image.stack_pointer)
            .map_err(|e| e.to_string())?;

        let mut kernel = Kernel::new(arch, mounts);
        let mut limits = Limits::for_this_host();
        limits.wall_time = spec.timeout;
        kernel.set_limits(limits);
        kernel.set_ncpus(1);
        kernel.set_stdout(stdout);
        kernel.set_stderr(stderr);
        kernel.set_stdin(Box::new(io::empty()));
        kernel.set_cwd(project);
        kernel.set_exe(&shell);
        kernel.set_cmdline(argv);
        kernel.set_heap(image.program_break, mid);
        kernel.set_mmap_area(image.stack_bottom, mid);
        if spec.net {
            kernel.set_egress(Box::new(nixvm::kernel::egress::HostEgress));
        }

        let code = kernel.run(vcpu, mem).map_err(|e| e.to_string())?;
        let _ = io::stdout().flush();
        let mut unsupported: Vec<String> = kernel
            .unsupported()
            .into_iter()
            .map(|(nr, count)| format!("syscall {nr} ×{count}"))
            .collect();
        unsupported.extend(
            kernel
                .unsupported_subcommands()
                .into_iter()
                .map(|((call, sub), count)| format!("{call} {sub:#x} ×{count}")),
        );
        let report = Report {
            backend: Some(backend.name().to_string()),
            unsupported,
        };
        let outcome = if kernel.time_limit_reached() && code == KILLED {
            Outcome::TimedOut
        } else {
            Outcome::Exited(code)
        };
        Ok((outcome, report))
    }

    /// The CPU backend: hardware virtualization where nixvm finds it for
    /// this guest architecture, else its interpreter — or the interpreter
    /// outright when asked.
    fn backend(arch: Arch, interp: bool) -> Result<Box<dyn Backend>, String> {
        if !interp {
            return nixvm::vcpu::select(arch).map_err(|e| e.to_string());
        }
        match arch {
            Arch::X86_64 => nixvm::vcpu::interp_x86::X86Backend::new(arch)
                .map(|b| Box::new(b) as Box<dyn Backend>),
            Arch::Aarch64 => nixvm::vcpu::interp::InterpBackend::new(arch)
                .map(|b| Box::new(b) as Box<dyn Backend>),
        }
        .map_err(|e| e.to_string())
    }

    /// The guest's whole environment. `safe.directory`: the guest runs as
    /// root and the project's files belong to the host's user, which git
    /// otherwise refuses to work with.
    fn environment(project: &str) -> Vec<String> {
        vec![
            "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
            "HOME=/root".to_string(),
            "USER=root".to_string(),
            "TERM=dumb".to_string(),
            "LANG=C.UTF-8".to_string(),
            format!("PWD={project}"),
            "GIT_CONFIG_COUNT=1".to_string(),
            "GIT_CONFIG_KEY_0=safe.directory".to_string(),
            format!("GIT_CONFIG_VALUE_0={project}"),
        ]
    }

    /// A resolver configuration for a guest given the network: the host's
    /// name servers that are not on its loopback (which the guest has its
    /// own), else a public one.
    fn resolv_conf() -> String {
        let host = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
        let servers: Vec<&str> = host
            .lines()
            .filter_map(|l| l.trim().strip_prefix("nameserver"))
            .map(str::trim)
            .filter(|s| !s.starts_with("127.") && *s != "::1" && !s.is_empty())
            .collect();
        if servers.is_empty() {
            return "nameserver 1.1.1.1\n".to_string();
        }
        servers
            .iter()
            .map(|s| format!("nameserver {s}\n"))
            .collect()
    }

    fn mkdir_all(mounts: &mut MountTable, path: &str) {
        let mut at = String::new();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            at.push('/');
            at.push_str(part);
            if mounts.stat(&at).is_none() {
                let _ = mounts.mkdir(&at, 0o755);
            }
        }
    }

    /// How a path in the project relates to the protected ones.
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Place {
        /// A protected path, or under one: nothing about it may change.
        Protected,
        /// A directory a protected path is under: it may not be renamed,
        /// removed or replaced, or the protected path would go with it.
        Above,
        Free,
    }

    /// The project directory, with some paths under it kept read-only.
    ///
    /// A read-only mount over them would not do: nixvm's mounts are found by
    /// path, so renaming a directory above one (`mv .git g`) takes its
    /// contents out from under it. This sits between the mount table and the
    /// host directory instead and refuses, by path, every operation that
    /// would change a protected path or move it. nixvm's kernel resolves
    /// `..` and symbolic links before it calls a filesystem, so the path
    /// seen here is the one acted on (the tests go through links to check).
    #[derive(Debug)]
    pub(super) struct Guarded {
        inner: Passthrough,
        protected: Vec<Vec<String>>,
    }

    impl Guarded {
        pub(super) fn new(inner: Passthrough, protected: &[String]) -> Self {
            Self {
                inner,
                protected: protected
                    .iter()
                    .map(|p| p.split('/').map(str::to_string).collect())
                    .collect(),
            }
        }

        fn place(&self, rel: &str) -> Place {
            let mut parts = Vec::new();
            for part in rel.split('/') {
                match part {
                    "" | "." => {}
                    // Never seen from the mount table; if it ever is, the
                    // path is not what it looks like.
                    ".." => return Place::Protected,
                    part => parts.push(part),
                }
            }
            let mut place = Place::Free;
            for protected in &self.protected {
                let shared = parts
                    .iter()
                    .zip(protected)
                    .take_while(|(a, b)| **a == b.as_str())
                    .count();
                if shared == protected.len() {
                    return Place::Protected;
                }
                if shared == parts.len() {
                    place = Place::Above;
                }
            }
            place
        }

        /// Refuse to change `rel`'s contents or attributes.
        fn writable(&self, rel: &str) -> io::Result<()> {
            match self.place(rel) {
                Place::Protected => Err(read_only()),
                Place::Above | Place::Free => Ok(()),
            }
        }

        /// Refuse to add, remove or replace the name `rel`.
        fn movable(&self, rel: &str) -> io::Result<()> {
            match self.place(rel) {
                Place::Protected | Place::Above => Err(read_only()),
                Place::Free => Ok(()),
            }
        }
    }

    fn read_only() -> io::Error {
        io::Error::from_raw_os_error(30) // EROFS
    }

    impl MountFs for Guarded {
        fn stat(&mut self, rel: &str) -> Option<Attrs> {
            self.inner.stat(rel)
        }
        fn read_at(&mut self, rel: &str, off: u64, buf: &mut [u8]) -> io::Result<usize> {
            self.inner.read_at(rel, off, buf)
        }
        fn readdir(&mut self, rel: &str) -> io::Result<Vec<DirEntry>> {
            self.inner.readdir(rel)
        }
        fn attach_quota(&mut self, quota: &Arc<Quota>) {
            self.inner.attach_quota(quota);
        }
        fn attach_disk_quota(&mut self, quota: &Arc<Quota>) {
            self.inner.attach_disk_quota(quota);
        }
        fn attach_disks(&mut self, disks: &SharedDisks) {
            self.inner.attach_disks(disks);
        }
        fn own_dev(&self) -> Option<u64> {
            self.inner.own_dev()
        }
        fn data_dev(&mut self, rel: &str) -> Option<u64> {
            self.inner.data_dev(rel)
        }
        fn fs_type(&self) -> &'static str {
            self.inner.fs_type()
        }
        fn read_only(&self) -> bool {
            self.inner.read_only()
        }
        fn write_at(&mut self, rel: &str, off: u64, buf: &[u8]) -> io::Result<usize> {
            self.writable(rel)?;
            self.inner.write_at(rel, off, buf)
        }
        fn create(&mut self, rel: &str, mode: u32) -> io::Result<()> {
            self.movable(rel)?;
            self.inner.create(rel, mode)
        }
        fn mkdir(&mut self, rel: &str, mode: u32) -> io::Result<()> {
            self.movable(rel)?;
            self.inner.mkdir(rel, mode)
        }
        fn mknod(&mut self, rel: &str, mode: u32) -> io::Result<()> {
            self.movable(rel)?;
            self.inner.mknod(rel, mode)
        }
        fn unlink(&mut self, rel: &str) -> io::Result<()> {
            self.movable(rel)?;
            self.inner.unlink(rel)
        }
        fn rmdir(&mut self, rel: &str) -> io::Result<()> {
            self.movable(rel)?;
            self.inner.rmdir(rel)
        }
        fn truncate(&mut self, rel: &str, len: u64) -> io::Result<()> {
            self.writable(rel)?;
            self.inner.truncate(rel, len)
        }
        fn set_times(&mut self, rel: &str, atime: SetTime, mtime: SetTime) -> io::Result<()> {
            self.writable(rel)?;
            self.inner.set_times(rel, atime, mtime)
        }
        fn set_owner(&mut self, rel: &str, uid: Option<u32>, gid: Option<u32>) -> io::Result<()> {
            self.writable(rel)?;
            self.inner.set_owner(rel, uid, gid)
        }
        fn set_mode(&mut self, rel: &str, mode: u32) -> io::Result<()> {
            self.writable(rel)?;
            self.inner.set_mode(rel, mode)
        }
        fn symlink(&mut self, target: &str, linkpath: &str) -> io::Result<()> {
            self.movable(linkpath)?;
            self.inner.symlink(target, linkpath)
        }
        fn readlink(&mut self, rel: &str) -> io::Result<String> {
            self.inner.readlink(rel)
        }
        fn rename(&mut self, from: &str, to: &str) -> io::Result<()> {
            self.movable(from)?;
            self.movable(to)?;
            self.inner.rename(from, to)
        }
        fn link(&mut self, old_rel: &str, new_rel: &str) -> io::Result<()> {
            // A second name for a protected file would be a way to write it.
            self.writable(old_rel)?;
            self.movable(new_rel)?;
            self.inner.link(old_rel, new_rel)
        }
        fn getxattr(&mut self, rel: &str, name: &str) -> io::Result<Vec<u8>> {
            self.inner.getxattr(rel, name)
        }
        fn setxattr(&mut self, rel: &str, name: &str, value: &[u8]) -> io::Result<()> {
            self.writable(rel)?;
            self.inner.setxattr(rel, name, value)
        }
        fn listxattr(&mut self, rel: &str) -> io::Result<Vec<String>> {
            self.inner.listxattr(rel)
        }
        fn removexattr(&mut self, rel: &str, name: &str) -> io::Result<()> {
            self.writable(rel)?;
            self.inner.removexattr(rel, name)
        }
        fn as_procfs_mut(&mut self) -> Option<&mut ProcFs> {
            None
        }
        fn as_cgroupfs_mut(&mut self) -> Option<&mut CgroupFs> {
            None
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn guard(protected: &[&str]) -> Guarded {
            let protected: Vec<String> = protected.iter().map(|p| p.to_string()).collect();
            Guarded::new(Passthrough::new("/nonexistent"), &protected)
        }

        #[test]
        fn a_path_is_protected_above_one_or_free() {
            let g = guard(&[".git/hooks", ".git/config", "atelier.toml"]);
            for path in [
                ".git/hooks",
                ".git/hooks/pre-commit",
                "/.git/hooks/a/b",
                ".git/config",
                "atelier.toml",
                "./atelier.toml",
                ".git//hooks/",
                "src/../.git/hooks/x",
            ] {
                assert_eq!(g.place(path), Place::Protected, "{path}");
            }
            for path in [".git", "", "/", ".git/"] {
                assert_eq!(g.place(path), Place::Above, "{path}");
            }
            for path in [
                ".git/hooks2",
                ".git/config.lock",
                ".git/index",
                ".github/workflows/ci.yml",
                "src/atelier.toml",
                "atelier.toml.bak",
                "hooks",
            ] {
                assert_eq!(g.place(path), Place::Free, "{path}");
            }
        }

        #[test]
        fn what_is_refused_where() {
            let g = guard(&[".git/hooks"]);
            // Contents and attributes: only the protected paths themselves.
            assert!(g.writable(".git/hooks/pre-commit").is_err());
            assert!(g.writable(".git").is_ok());
            assert!(g.writable(".git/index").is_ok());
            // Names: also every directory above them.
            assert!(g.movable(".git").is_err());
            assert!(g.movable(".git/hooks").is_err());
            assert!(g.movable(".git/objects").is_ok());
            assert_eq!(g.movable(".git").unwrap_err().raw_os_error(), Some(30));
        }
    }
}

#[cfg(test)]
pub mod testing {
    //! What [`state`](super::state) answers in unit tests: `Off`, whatever the
    //! environment and the machine's files say, unless the test set
    //! something for its own thread.

    use std::cell::RefCell;

    use super::State;

    thread_local! {
        static STATE: RefCell<State> = const { RefCell::new(State::Off) };
    }

    pub fn current() -> State {
        STATE.with(|s| s.borrow().clone())
    }

    /// Set the state for this thread until the guard is dropped.
    pub fn set(state: State) -> Reset {
        STATE.with(|s| *s.borrow_mut() = state);
        Reset
    }

    pub struct Reset;

    impl Drop for Reset {
        fn drop(&mut self) {
            STATE.with(|s| *s.borrow_mut() = State::Off);
        }
    }

    /// A [`Setup`](super::Setup) for tests that run a real guest, or `None`
    /// (after saying why on stderr) when this machine can't: no `sandbox`
    /// feature, no root filesystem (`ATELIER_SANDBOX_TEST_ROOT`, else the
    /// default location), or no built `atelier` next to the test binary to
    /// act as the helper.
    pub fn live_setup() -> Option<super::Setup> {
        if !cfg!(sandbox) {
            eprintln!("skipped: built without the sandbox");
            return None;
        }
        let rootfs = std::env::var_os("ATELIER_SANDBOX_TEST_ROOT")
            .map(std::path::PathBuf::from)
            .or_else(super::default_rootfs)
            .filter(|dir| std::fs::symlink_metadata(dir.join("bin/sh")).is_ok());
        let Some(rootfs) = rootfs else {
            eprintln!("skipped: no sandbox root filesystem (set ATELIER_SANDBOX_TEST_ROOT)");
            return None;
        };
        // The `atelier` binary cargo builds for the integration tests before
        // it runs any test: in the profile directory, some levels above the
        // test binary (how many depends on cargo's layout). It must have the
        // sandbox in it, which a binary left by another build may not.
        let helper = std::env::current_exe().ok().and_then(|exe| {
            exe.ancestors()
                .skip(1)
                .take(6)
                .map(|dir| dir.join("atelier"))
                .find(|p| p.is_file())
        });
        let helper = helper.filter(|path| {
            std::process::Command::new(path)
                .args([super::HELPER, super::PROBE])
                .output()
                .is_ok_and(|out| out.status.success())
        });
        let Some(helper) = helper else {
            eprintln!("skipped: no atelier binary next to the tests to run the guest");
            return None;
        };
        Some(super::Setup {
            rootfs,
            memory_mb: 1024,
            interp: false,
            helper: Some(helper),
            notes: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            vars.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        }
    }

    /// A directory that passes for a root filesystem.
    fn fake_rootfs(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "atelier-sandbox-test-{name}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin/sh"), b"").unwrap();
        dir
    }

    fn on() -> SandboxSettings {
        SandboxSettings {
            enabled: Some(true),
            ..Default::default()
        }
    }

    #[test]
    fn off_unless_asked_for() {
        let none = SandboxSettings::default();
        assert_eq!(resolve(&env(&[]), &none, &none, None), State::Off);
        // The environment wins over the files, both ways.
        assert_eq!(
            resolve(&env(&[("ATELIER_SANDBOX", "0")]), &on(), &on(), None),
            State::Off
        );
        assert_ne!(
            resolve(&env(&[("ATELIER_SANDBOX", "1")]), &none, &none, None),
            State::Off
        );
        // Either file turns it on; the project's can turn the user's off.
        assert_ne!(resolve(&env(&[]), &on(), &none, None), State::Off);
        assert_ne!(resolve(&env(&[]), &none, &on(), None), State::Off);
        let off = SandboxSettings {
            enabled: Some(false),
            ..Default::default()
        };
        assert_eq!(resolve(&env(&[]), &off, &on(), None), State::Off);
    }

    /// The project's switch is read through project trust, from the file
    /// as it is on disk: untrusted it says nothing (the user's file and
    /// the environment decide), accepted it counts, and an edit made after
    /// that — the agent's file tools can write `atelier.toml` — is not in
    /// effect until it has been accepted in its turn.
    #[test]
    fn the_projects_switch_counts_only_as_it_was_trusted() {
        use crate::trust::{Store, Trust, testing::dir};
        let root = dir("sbx-project");
        let state = dir("sbx-state");
        let store = Store {
            file: Some(state.join("trust.json")),
            ..Store::default()
        };
        let file = root.join(crate::settings::FILE_NAME);
        std::fs::write(&file, "[sandbox]\nenabled = false\nmemory_mb = 512\n").unwrap();
        let read = project_settings(&root, store.clone());
        assert_eq!(read.enabled, None);
        assert_eq!(read.memory_mb, Some(512));
        // With the user's sandbox on, the untrusted file does not turn it
        // off (in a build without the sandbox: asked for, not `Off`).
        assert_ne!(resolve(&env(&[]), &read, &on(), None), State::Off);

        let settings = Settings::load(&root).unwrap();
        let trust = Trust::load(&root, &settings, store.clone());
        trust.accept(&settings).unwrap();
        assert_eq!(project_settings(&root, store.clone()).enabled, Some(false));

        std::fs::write(&file, "[sandbox]\nenabled = true\n").unwrap();
        assert_eq!(project_settings(&root, store).enabled, None);
        for dir in [root, state] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[cfg(not(sandbox))]
    #[test]
    fn asked_for_in_a_build_without_it_is_unavailable_not_off() {
        let root = fake_rootfs("nobuild");
        let state = resolve(&env(&[]), &on(), &on(), Some(root));
        assert!(matches!(state, State::Unavailable(why) if why.contains("built without")));
    }

    #[cfg(sandbox)]
    #[test]
    fn the_root_filesystem_comes_from_the_user_never_the_project() {
        let good = fake_rootfs("good");
        let none = SandboxSettings::default();
        // Nothing names one, and there is no default.
        let state = resolve(&env(&[]), &on(), &none, None);
        assert!(matches!(state, State::Unavailable(why) if why.contains("ATELIER_SANDBOX_ROOT")));
        // The default location.
        let State::Ready(setup) = resolve(&env(&[]), &on(), &none, Some(good.clone())) else {
            panic!("expected ready");
        };
        assert_eq!(setup.rootfs, good);
        assert_eq!(setup.memory_mb, DEFAULT_MEMORY_MB);
        // A project naming a root is not listened to, and told so.
        let project = SandboxSettings {
            enabled: Some(true),
            root: Some("/".to_string()),
            network: vec!["crates.io".to_string()],
            memory_mb: Some(512),
        };
        let State::Ready(setup) = resolve(&env(&[]), &project, &none, Some(good.clone())) else {
            panic!("expected ready");
        };
        assert_eq!(setup.rootfs, good);
        assert_eq!(setup.memory_mb, 512);
        assert!(setup.notes.iter().any(|n| n.contains("root")));
        assert!(setup.notes.iter().any(|n| n.contains("network")));
        // The user's file and the environment are.
        let user = SandboxSettings {
            root: Some(good.to_str().unwrap().to_string()),
            ..Default::default()
        };
        assert!(matches!(
            resolve(&env(&[]), &on(), &user, None),
            State::Ready(_)
        ));
        let missing = [("ATELIER_SANDBOX_ROOT", "/nonexistent/rootfs")];
        let state = resolve(&env(&missing), &on(), &user, Some(good));
        assert!(matches!(state, State::Unavailable(why) if why.contains("/nonexistent/rootfs")));
    }

    #[test]
    fn the_status_file_round_trips() {
        let report = Report {
            backend: Some("kvm".to_string()),
            unsupported: vec!["syscall 53 ×2".to_string(), "ioctl 0x5401 ×1".to_string()],
        };
        for outcome in [
            Outcome::Exited(0),
            Outcome::Exited(137),
            Outcome::TimedOut,
            Outcome::Failed("no /bin/sh\nin there".to_string()),
        ] {
            let text = format_status(&outcome, &report);
            let (back, back_report) = parse_status(&text).unwrap();
            match (&outcome, &back) {
                (Outcome::Failed(_), Outcome::Failed(why)) => {
                    assert_eq!(why, "no /bin/sh in there")
                }
                _ => assert_eq!(outcome, back),
            }
            assert_eq!(back_report, report);
        }
        // Cut short: no outcome, no result.
        assert!(parse_status("backend kvm\n").is_none());
        assert!(parse_status("").is_none());
    }

    #[test]
    fn protected_paths_follow_what_dot_git_is() {
        let dir = std::env::temp_dir().join(format!(
            "atelier-sandbox-test-protected-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // No repository yet: `.git` may not appear.
        assert_eq!(protected_paths(&dir), [".git", "atelier.toml"]);
        // A linked worktree's pointer file.
        std::fs::write(dir.join(".git"), "gitdir: /elsewhere\n").unwrap();
        assert_eq!(protected_paths(&dir), [".git", "atelier.toml"]);
        std::fs::remove_file(dir.join(".git")).unwrap();
        std::fs::create_dir(dir.join(".git")).unwrap();
        assert_eq!(
            protected_paths(&dir),
            [".git/hooks", ".git/config", "atelier.toml"]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn output(code: i32, stderr: &str) -> Output {
        Output {
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
            outcome: Outcome::Exited(code),
            report: Report::default(),
        }
    }

    #[test]
    fn a_failure_is_blamed_on_the_sandbox_only_with_a_reason() {
        // Success, and an ordinary failure: nothing to say.
        assert_eq!(diagnose(&output(0, "warning: Read-only file system")), None);
        assert_eq!(
            diagnose(&output(1, "test result: FAILED. 3 passed; 1 failed")),
            None
        );
        assert_eq!(
            diagnose(&output(101, "error[E0308]: mismatched types")),
            None
        );

        let missing = diagnose(&output(127, "/bin/sh: cargo: not found")).unwrap();
        assert!(missing.contains("not in the sandbox's image"), "{missing}");
        let net = diagnose(&output(1, "wget: bad address 'example.com'")).unwrap();
        assert!(net.contains("no network"), "{net}");
        let ro = diagnose(&output(
            1,
            "error: could not lock config file .git/config: Read-only file system",
        ))
        .unwrap();
        assert!(ro.contains("read-only"), "{ro}");
        let spawn = diagnose(&output(
            101,
            "error: could not exec the linker `cc`\n  = note: Not supported (os error 95)",
        ))
        .unwrap();
        assert!(spawn.contains("not implemented"), "{spawn}");
        assert!(diagnose(&output(139, "Segmentation fault")).is_some());
        let git = diagnose(&output(
            128,
            "error: unable to write loose object file: Permission denied",
        ))
        .unwrap();
        assert!(git.contains("`git add`"), "{git}");

        let mut unsupported = output(1, "");
        unsupported.report.unsupported = vec!["syscall 53 ×2".to_string()];
        let text = diagnose(&unsupported).unwrap();
        assert!(text.contains("syscall 53 ×2"), "{text}");

        let failed = Output {
            outcome: Outcome::Failed("the root filesystem has no /bin/sh".to_string()),
            ..output(0, "")
        };
        assert!(diagnose(&failed).unwrap().contains("no /bin/sh"));

        // A timeout is the command's own business, unless it ran interpreted.
        let mut slow = Output {
            outcome: Outcome::TimedOut,
            ..output(0, "")
        };
        assert_eq!(diagnose(&slow), None);
        slow.report.backend = Some("interp-x86_64".to_string());
        assert!(diagnose(&slow).unwrap().contains("software CPU"));
    }

    #[test]
    fn status_says_how_to_turn_it_on() {
        // `state` is `Off` in tests unless a test says otherwise.
        let lines = status_lines(Path::new("/nonexistent"));
        assert!(lines[0].contains("off"), "{lines:?}");
        let _reset = testing::set(State::Unavailable("no root filesystem".to_string()));
        let lines = status_lines(Path::new("/nonexistent"));
        assert!(
            lines[0].contains("unavailable: no root filesystem"),
            "{lines:?}"
        );
    }

    /// The guest itself, through the helper process: needs the feature, a
    /// root filesystem and the built binary, and is skipped without them.
    #[test]
    fn a_command_runs_in_the_guest_at_the_projects_path() {
        let Some(setup) = testing::live_setup() else {
            return;
        };
        let project =
            std::env::temp_dir().join(format!("atelier-sandbox-test-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&project);
        std::fs::create_dir_all(project.join(".git/hooks")).unwrap();
        std::fs::write(project.join(".git/config"), "[core]\n").unwrap();
        std::fs::write(project.join("atelier.toml"), "").unwrap();
        std::fs::write(project.join("seen.txt"), "from the host\n").unwrap();
        let project = std::fs::canonicalize(&project).unwrap();
        let run = |command: &str| {
            let out = run(command, &project, &setup, Duration::from_secs(60)).unwrap();
            (
                out.outcome.clone(),
                String::from_utf8_lossy(&out.stdout).into_owned(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
                out,
            )
        };

        // Where it runs, what it sees, how it ends.
        let (outcome, stdout, stderr, out) =
            run("pwd; cat seen.txt; echo made > made.txt; echo oops >&2; exit 3");
        assert_eq!(outcome, Outcome::Exited(3), "{stderr}");
        assert_eq!(
            stdout,
            format!("{}\nfrom the host\n", project.display()),
            "{stderr}"
        );
        assert!(stderr.contains("oops"));
        assert!(out.report.backend.is_some());
        assert_eq!(
            std::fs::read_to_string(project.join("made.txt")).unwrap(),
            "made\n"
        );

        // Nothing of the host but the project; no environment; no network.
        let home = std::env::var("HOME").unwrap_or_default();
        let (_, stdout, _, _) = run(&format!(
            "ls -d {home} /home/* 2>&1; echo HOME=$HOME; env | grep -c ATELIER; \
             wget -q -T 2 -O- http://1.1.1.1/ 2>&1 | head -c 200"
        ));
        assert!(stdout.contains("HOME=/root"), "{stdout}");
        assert!(stdout.contains("\n0\n"), "{stdout}");
        assert!(
            stdout.contains("unreachable") || stdout.contains("bad address"),
            "{stdout}"
        );
        // The project's own parents exist only as empty directories.
        let parent = project.parent().unwrap();
        let (_, stdout, _, _) = run(&format!("ls -A {}", parent.display()));
        assert_eq!(
            stdout.trim(),
            project.file_name().unwrap().to_str().unwrap(),
            "the guest sees more of the host than the project"
        );
        // A link in the project that points out of it leads, in the guest,
        // to the guest's own tree — not to the host directory it names.
        std::os::unix::fs::symlink(parent, project.join("out")).unwrap();
        let (_, stdout, _, _) = run("ls -A out/");
        assert_eq!(
            stdout.trim(),
            project.file_name().unwrap().to_str().unwrap(),
            "a link out of the project reached the host"
        );
        std::fs::remove_file(project.join("out")).unwrap();

        // The protected paths, by every route.
        let attempts = [
            "echo x > .git/hooks/pre-commit",
            "echo x >> .git/config",
            "echo x >> atelier.toml",
            "rm atelier.toml",
            "mv atelier.toml a.toml",
            "echo x > new.toml && mv new.toml atelier.toml",
            "mv .git g",
            "ln -s .git/hooks h && echo x > h/pre-commit",
            "ln -s .git g2 && echo x > g2/hooks/pre-commit",
            "ln -s atelier.toml t && echo x >> t",
            "ln -s $PWD/.git/config c && echo x >> c",
            "ln -s $PWD/.git/hooks/pre-commit p && echo x > p",
            "ln -s .git/hooks/pre-commit p2 && echo x > p2",
            "ln .git/config hard && echo x >> hard",
            "cd .git && echo x > hooks/pre-commit",
            "echo x > src/../.git/hooks/pre-commit",
            "chmod 777 .git/config && echo x >> .git/config",
            "rm -rf .git/hooks && mkdir .git/hooks && echo x > .git/hooks/pre-commit",
            "cp seen.txt .git/config",
            "dd if=seen.txt of=.git/config conv=notrunc 2>/dev/null",
            "truncate -s 0 atelier.toml",
        ];
        for attempt in attempts {
            let (outcome, _, stderr, _) = run(attempt);
            assert_ne!(
                outcome,
                Outcome::Exited(0),
                "`{attempt}` succeeded: {stderr}"
            );
            assert_eq!(
                std::fs::read_to_string(project.join(".git/config")).unwrap(),
                "[core]\n",
                "after `{attempt}`"
            );
            assert_eq!(
                std::fs::read_to_string(project.join("atelier.toml")).unwrap(),
                "",
                "after `{attempt}`"
            );
            let hooks: Vec<_> = std::fs::read_dir(project.join(".git/hooks"))
                .unwrap_or_else(|e| panic!("after `{attempt}`: {e}"))
                .collect();
            assert!(hooks.is_empty(), "after `{attempt}`: {hooks:?}");
        }
        // The rest of `.git` is ordinary.
        let (outcome, _, stderr, _) = run("echo ref > .git/HEAD && mkdir .git/objects");
        assert_eq!(outcome, Outcome::Exited(0), "{stderr}");

        // Time is up: a sleeper and a spinner both.
        for command in ["sleep 30", "while :; do :; done"] {
            let started = std::time::Instant::now();
            let out = super::run(command, &project, &setup, Duration::from_millis(700)).unwrap();
            assert_eq!(out.outcome, Outcome::TimedOut, "{command}");
            assert!(started.elapsed() < Duration::from_secs(10), "{command}");
        }
        // A command that wedges nixvm 0.0.5's kernel past its own time limit
        // (removing a FIFO it made in a host directory): the helper process
        // is killed, and the session goes on.
        let started = std::time::Instant::now();
        let out = super::run(
            "mkfifo wedge; rm -f wedge",
            &project,
            &setup,
            Duration::from_millis(700),
        )
        .unwrap();
        assert!(
            matches!(out.outcome, Outcome::TimedOut | Outcome::Exited(0)),
            "{:?}",
            out.outcome
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        let _ = std::fs::remove_file(project.join("wedge"));

        // No such root filesystem: a failure of the sandbox, said as one.
        let broken = Setup {
            rootfs: project.join("nowhere"),
            ..setup.clone()
        };
        let out = super::run("true", &project, &broken, Duration::from_secs(5)).unwrap();
        assert!(
            matches!(out.outcome, Outcome::Failed(_)),
            "{:?}",
            out.outcome
        );
        assert!(diagnose(&out).is_some());

        std::fs::remove_dir_all(&project).unwrap();
    }
}
