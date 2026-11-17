//! Who owns a project's session: one atelier at a time.
//!
//! A project has one saved session (`.atelier/session.json`), and every run
//! that works on it — the terminal interface, the line REPL, `--print
//! --continue`, `serve`, `acp` — writes it after each turn. Two of them at
//! once would write over each other, so the first one *owns* the session
//! for as long as it lives, and one started later finds out and is given
//! the choice (see [`start`]): join it as a remote, take it over, or leave.
//!
//! # Remote control
//!
//! Joining is being a second front end of the owner's live session, over
//! the owner's socket (see [`hub`] for the owner's side, and
//! `docs/protocol.md`). The newcomer opens no session of its own — it
//! reads no configuration, joins no coordinator, runs no hook — and is
//! either the terminal interface as a client (`tui::remote`) or, for
//! `serve`, a pipe between its own client and the owner ([`relay`]).
//!
//! # The record
//!
//! Ownership is an exclusive advisory lock on `.atelier/session.lock`
//! ([`LOCK_FILE`]), taken when the session starts and held until the
//! process ends — the way the coordinator and the local model's supervisor
//! hold theirs. The kernel lets go of it however the process dies, so a
//! crash leaves nothing to clean up: the next start takes the lock and is
//! the owner. Of two that start together one gets it.
//!
//! The file's content is the [`Record`]: the owner's pid and when that
//! process started, when the session did, what kind of run it is, the
//! terminal it is on, and the socket it listens on. The record is written
//! by whoever holds the lock and is never believed on its own: a newcomer
//! that finds the lock held checks that the pid is still the process that
//! wrote it (its start time), and checks again before every signal.
//!
//! # The socket
//!
//! The owner listens on a Unix socket of its own, `session-<pid>.sock` in
//! the directory the coordinator's is in (see [`crate::coord::runtime_dir`]),
//! protected as `serve --listen`'s: a directory only the user can enter,
//! mode 0600, no TCP. Whoever can open it is the same user on the same
//! machine, and that is the only gate. It answers [`hub`]'s methods: what
//! the session is doing, and the request to yield it.
//!
//! # Taking over
//!
//! See [`takeover`]: the owner is asked to stop, save and exit; if the lock
//! is not free a few seconds later it is signalled (TERM, then KILL), and
//! what it had started is stopped with it.
//!
//! Unix only (`cfg(unixlike)`), like the sockets and the locks it is made
//! of: elsewhere [`start`] always says go, and nothing is recorded.

#[cfg(unixlike)]
mod hub;
#[cfg(unixlike)]
mod proc;
#[cfg(unixlike)]
mod prompt;
#[cfg(unixlike)]
pub(crate) mod remote;
#[cfg(unixlike)]
mod takeover;

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The lock file and record, relative to the project root.
pub const LOCK_FILE: &str = ".atelier/session.lock";

/// Exit status: a session is live in this directory and nothing said what
/// to do about it (a start that cannot ask, without `--attach`,
/// `--take-over` or `--cancel`). Nothing was touched.
pub const EXIT_IN_USE: u8 = 7;
/// Exit status: a session is live in this directory and the answer was to
/// leave it (the prompt's "cancel", or `--cancel`). Nothing was touched.
pub const EXIT_CANCELLED: u8 = 8;

/// How long an owner asked to yield has to stop by itself before it ends
/// regardless (and, a little later, before the newcomer signals it).
pub const YIELD_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// The kind of run that owns a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// The terminal interface.
    Terminal,
    /// The line REPL: input that is not a terminal, or a build without the
    /// interface.
    Repl,
    /// `--print --continue`.
    Print,
    /// `atelier serve`: a program drives it.
    Serve,
    /// `atelier acp`: an editor drives it.
    Acp,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Terminal => "terminal",
            Kind::Repl => "repl",
            Kind::Print => "print",
            Kind::Serve => "serve",
            Kind::Acp => "acp",
        }
    }

    /// Whether another program is driving the session, whose own session a
    /// take-over breaks.
    pub fn driven(self) -> bool {
        matches!(self, Kind::Serve | Kind::Acp)
    }
}

/// What the lock file holds: the live owner, as it described itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub pid: u32,
    /// What tells that process from a later one with the same pid: when it
    /// started, as the system counts it.
    pub started: String,
    /// When the session started, in seconds since the Unix epoch.
    pub since: u64,
    pub kind: Kind,
    /// The terminal it is on, when its input is one.
    #[serde(default)]
    pub tty: Option<String>,
    /// The socket it answers on, when it could open one.
    #[serde(default)]
    pub socket: Option<String>,
    /// The atelier that wrote it.
    #[serde(default)]
    pub version: String,
}

/// A session found live in the project: its owner's record, and what the
/// owner said it is doing when asked (`None` when it did not answer).
#[derive(Debug, Clone)]
pub struct Live {
    pub record: Record,
    pub status: Option<Value>,
}

/// What to do about a live session, as chosen on the command line or at
/// the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Be a second front end of the owner's session (remote control).
    Attach,
    /// Stop the owner and what it runs, and continue its session here.
    TakeOver,
    /// Leave it alone and exit.
    Cancel,
}

impl Choice {
    /// The flag that picks it.
    pub fn flag(self) -> &'static str {
        match self {
            Choice::Attach => "--attach",
            Choice::TakeOver => "--take-over",
            Choice::Cancel => "--cancel",
        }
    }
}

/// How a prompt put to the user ended (see `prompt`).
pub enum Picked {
    /// The option at this index.
    Option(usize),
    /// Ctrl-C, Ctrl-D, the end of input: no answer.
    Dismissed,
    /// What was asked about is no longer there.
    Gone,
}

/// How a start that would own the session may go on.
pub enum Start {
    /// This process owns the session. `took_over` when it was taken from
    /// another: the saved conversation is then resumed, as `--continue`
    /// does.
    Go { took_over: bool },
    /// Be a remote of this live session: no session is opened here.
    Attach(Live),
    /// Leave, with this exit status. What there was to say has been said.
    Exit(u8),
    /// Another atelier owns the session, nothing chose what to do about it
    /// and nobody is here to ask: for the caller to refuse the start (see
    /// [`in_use_message`], [`EXIT_IN_USE`]), or to ask whoever drives it.
    InUse(Live),
}

/// The line an owner leaves behind when its session is taken.
pub fn taken_over_line(by: &str) -> String {
    format!("another atelier ({by}) took this session over; this one has stopped.")
}

#[cfg(unixlike)]
pub use imp::*;

#[cfg(unixlike)]
mod imp {
    use super::*;
    use std::fs::File;
    use std::io::{Read, Seek, Write};
    use std::path::PathBuf;
    use std::sync::{Arc, OnceLock};
    use std::time::{Duration, Instant, SystemTime};

    /// The one-line description of a live session: who, since when, doing what.
    pub fn describe(live: &Live) -> Vec<String> {
        prompt::describe(live)
    }

    /// What a start that cannot ask says when it finds a session live.
    pub fn in_use_message(live: &Live) -> String {
        let mut lines = vec!["a session is already running in this directory:".to_string()];
        lines.extend(describe(live).into_iter().map(|l| format!("  {l}")));
        lines.push(
            "say what to do about it: --attach (join it from here, as a remote control), \
             --take-over (stop it and what it runs, and continue its session here) or --cancel \
             (leave it and exit)."
                .to_string(),
        );
        lines.push(format!("nothing was changed (exit status {EXIT_IN_USE})."));
        lines.join("\n")
    }

    /// Whether the owner of `live` takes remotes: it is a terminal
    /// interface, up, and said so when asked.
    pub fn takes_remotes(live: &Live) -> bool {
        live.status.as_ref().is_some_and(|s| s["remote"] == true)
    }

    /// A connection to the socket of the owner `record` describes.
    pub fn connect(record: &Record) -> Result<std::os::unix::net::UnixStream> {
        hub::connect(record)
    }

    /// `atelier serve` as a remote: carry the lines between this process's
    /// client (stdin, stdout) and the owner's socket, until either end
    /// closes. The client speaks to the owner from its `initialize` on (see
    /// "A session shared with its terminal" in `docs/protocol.md`).
    pub fn relay(live: &Live) -> Result<u8> {
        use std::io::{BufRead, BufReader};
        let stream = connect(&live.record)?;
        let mut to_owner = stream.try_clone()?;
        let closing = stream.try_clone()?;
        std::thread::spawn(move || {
            let mut line = Vec::new();
            loop {
                line.clear();
                match std::io::stdin().lock().read_until(b'\n', &mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                if to_owner.write_all(&line).is_err() {
                    break;
                }
            }
            // The client is gone: so is this remote, and nothing else.
            let _ = closing.shutdown(std::net::Shutdown::Both);
        });
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { break };
            let mut out = std::io::stdout().lock();
            if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
                break;
            }
        }
        Ok(0)
    }

    /// This process's ownership, once it has one.
    static OWNER: OnceLock<Arc<hub::Hub>> = OnceLock::new();

    pub(super) fn owner() -> Option<&'static Arc<hub::Hub>> {
        OWNER.get()
    }

    /// What trying for the lock found.
    pub(super) enum Claimed {
        /// The lock is this process's: it owns the session.
        Owner,
        /// Another process holds it.
        Live(Live),
        /// There is no telling here (a directory that cannot be written, a
        /// file system without locks): go on as atelier always did.
        Unknown,
    }

    fn open_lock(root: &Path) -> Option<File> {
        let path = root.join(LOCK_FILE);
        std::fs::create_dir_all(path.parent()?).ok()?;
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .ok()
    }

    fn read_record(file: &mut File) -> Option<Record> {
        let mut text = String::new();
        file.rewind().ok()?;
        file.read_to_string(&mut text).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// The record in `root`'s lock file, whoever holds the lock.
    pub(super) fn record_of(root: &Path) -> Option<Record> {
        read_record(&mut File::open(root.join(LOCK_FILE)).ok()?)
    }

    /// The record's process is the one that wrote it, and is still there.
    /// (An owner on a system that could not tell it when it started wrote
    /// no start time: there the process being there has to do, next to the
    /// lock it is seen to hold.)
    pub(super) fn genuine(record: &Record) -> bool {
        match (proc::start_token(record.pid), record.started.as_str()) {
            (Some(now), then) if !then.is_empty() => now == then,
            (_, "") => proc::exists(record.pid),
            _ => false,
        }
    }

    /// The socket a record names, if it is where an owner's socket goes:
    /// the only file a record is allowed to have removed.
    fn socket_of(record: &Record) -> Option<PathBuf> {
        let path = PathBuf::from(record.socket.as_deref()?);
        let name = path.file_name()?.to_str()?;
        let ours = name.starts_with("session-") && name.ends_with(".sock");
        (ours && path.parent() == crate::coord::runtime_dir().as_deref()).then_some(path)
    }

    pub(super) fn remove_socket(record: &Record) {
        use std::os::unix::fs::FileTypeExt;
        if let Some(path) = socket_of(record)
            && std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_socket())
        {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Try for the lock on `root`'s session.
    pub(super) fn claim(root: &Path, kind: Kind) -> Result<Claimed> {
        let Some(mut file) = open_lock(root) else {
            return Ok(Claimed::Unknown);
        };
        match file.try_lock() {
            Ok(()) => {
                // Whatever the file says is of a process that is gone.
                if let Some(stale) = read_record(&mut file) {
                    remove_socket(&stale);
                }
                own(file, kind);
                Ok(Claimed::Owner)
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                // The owner writes its record right after taking the lock:
                // one read too early is read again.
                let deadline = Instant::now() + Duration::from_secs(2);
                loop {
                    if let Some(record) = read_record(&mut file).filter(genuine) {
                        let status = hub::ask(&record, "owner/status", Value::Null).ok();
                        return Ok(Claimed::Live(Live { record, status }));
                    }
                    if Instant::now() >= deadline {
                        // Held, and not by a process the record names:
                        // something an atelier that is gone had started,
                        // that kept its files open.
                        let was = read_record(&mut file).map_or_else(String::new, |r| {
                            format!(" (the atelier that took it, pid {}, is gone)", r.pid)
                        });
                        anyhow::bail!(
                            "{} is locked{was}, and whatever holds it does not say who it \
                             is. Nothing was changed. Stop what is left of that atelier, or \
                             remove the file and start again",
                            root.join(LOCK_FILE).display()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(50));
                    // The lock may have been let go meanwhile.
                    if file.try_lock().is_ok() {
                        own(file, kind);
                        return Ok(Claimed::Owner);
                    }
                }
            }
            Err(std::fs::TryLockError::Error(_)) => Ok(Claimed::Unknown),
        }
    }

    /// Take the lock if it is free, and own the session; `false` while
    /// another process holds it.
    fn try_own(root: &Path, kind: Kind) -> bool {
        let Some(file) = open_lock(root) else {
            return false;
        };
        let free = file.try_lock().is_ok();
        if free {
            own(file, kind);
        }
        free
    }

    /// Become the owner: `lock` is held. Open the socket, write the record.
    pub(super) fn own(mut lock: File, kind: Kind) {
        let pid = std::process::id();
        let socket = hub::bind(pid);
        let record = Record {
            pid,
            started: proc::start_token(pid).unwrap_or_default(),
            since: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
            kind,
            tty: proc::terminal(),
            socket: socket.as_ref().map(|(_, path)| path.display().to_string()),
            version: env!("CARGO_PKG_VERSION").to_string(),
        };
        let written = lock
            .set_len(0)
            .and_then(|()| lock.rewind())
            .and_then(|()| lock.write_all(serde_json::to_string(&record).unwrap().as_bytes()))
            .and_then(|()| lock.flush());
        if let Err(e) = written {
            eprintln!("warning: writing {LOCK_FILE}: {e}");
        }
        let hub = hub::Hub::start(record, lock, socket);
        let _ = OWNER.set(hub);
    }

    /// Whether another atelier owns the session in `root` right now. For a
    /// run that does not own it and must not write it.
    pub fn live_elsewhere(root: &Path) -> bool {
        if owner().is_some() {
            return false;
        }
        let Ok(file) = File::open(root.join(LOCK_FILE)) else {
            return false;
        };
        match file.try_lock_shared() {
            Ok(()) => false,
            Err(std::fs::TryLockError::WouldBlock) => true,
            Err(std::fs::TryLockError::Error(_)) => false,
        }
    }

    /// Start a run of `kind` that owns the session in `root`: take the
    /// ownership, or — when another atelier has it — do what `choice` says,
    /// or ask where somebody can be asked (`interactive`), or refuse.
    pub fn start(
        root: &Path,
        kind: Kind,
        choice: Option<Choice>,
        interactive: bool,
    ) -> Result<Start> {
        let mut took_over = false;
        loop {
            let live = match claim(root, kind)? {
                Claimed::Owner | Claimed::Unknown => return Ok(Start::Go { took_over }),
                Claimed::Live(live) => live,
            };
            if took_over {
                // Somebody else was quicker to the session just freed.
                eprintln!(
                    "atelier: the session was taken by another atelier (pid {}) meanwhile.",
                    live.record.pid
                );
            }
            let choice = match choice {
                Some(choice) => choice,
                None if interactive => {
                    // While the question is up, the owner may end: then
                    // there is nothing to ask about.
                    let mut gone = || try_own(root, kind);
                    match prompt::ask(&live, &mut gone)? {
                        prompt::Asked::Choice(choice) => choice,
                        prompt::Asked::Gone => {
                            eprintln!(
                                "the other atelier (pid {}) has ended: starting normally.",
                                live.record.pid
                            );
                            return Ok(Start::Go { took_over: false });
                        }
                    }
                }
                None => return Ok(Start::InUse(live)),
            };
            match choice {
                Choice::Attach => {
                    // Only what can be a front end, of an owner that
                    // takes them.
                    let unable = if !matches!(kind, Kind::Terminal | Kind::Serve) {
                        Some("--attach needs a terminal (or serve, for a program) to attach from")
                    } else if !takes_remotes(&live) {
                        Some("it takes no remote: only a terminal interface does")
                    } else {
                        None
                    };
                    let Some(why) = unable else {
                        // It may have ended since it was described.
                        if try_own(root, kind) {
                            eprintln!(
                                "the other atelier (pid {}) has ended: starting normally.",
                                live.record.pid
                            );
                            return Ok(Start::Go { took_over: false });
                        }
                        return Ok(Start::Attach(live));
                    };
                    eprintln!(
                        "atelier: a session is already running in this directory:\n  {}\n{why}. \
                         Nothing was changed (--take-over takes it, --cancel leaves it).",
                        describe(&live).join("\n  ")
                    );
                    return Ok(Start::Exit(EXIT_IN_USE));
                }
                Choice::Cancel => {
                    if !interactive {
                        eprintln!(
                            "atelier: a session is already running in this directory (pid {}); \
                             left alone.",
                            live.record.pid
                        );
                    }
                    return Ok(Start::Exit(EXIT_CANCELLED));
                }
                Choice::TakeOver => match takeover::run(&live, root)? {
                    takeover::Outcome::Done { lock, notes } => {
                        for note in notes {
                            eprintln!("{note}");
                        }
                        own(lock, kind);
                        return Ok(Start::Go { took_over: true });
                    }
                    takeover::Outcome::Changed => {
                        took_over = true;
                        if !interactive {
                            // A flag was about the session that was there.
                            let now = record_of(root).map_or(0, |r| r.pid);
                            eprintln!(
                                "atelier: the session was taken by another atelier (pid {now}) \
                                 meanwhile; nothing more was done."
                            );
                            return Ok(Start::Exit(EXIT_IN_USE));
                        }
                    }
                },
            }
        }
    }

    /// On the way out: take the socket away. The lock goes with the
    /// process.
    pub fn release() {
        if let Some(hub) = owner() {
            hub.close();
        }
    }

    /// Who took the session over, once somebody has: the owner is to stop,
    /// save and exit.
    pub fn yielding() -> Option<String> {
        owner().and_then(|hub| hub.yielding())
    }

    /// Have `hook` called when the session is asked for (on the socket's
    /// thread): for an interface that has to be woken to notice.
    pub fn on_yield(hook: impl Fn() + Send + Sync + 'static) {
        if let Some(hub) = owner() {
            hub.on_yield(Box::new(hook));
        }
    }

    /// Have `hook` called if the process has to end without the interface
    /// having let go ([`YIELD_GRACE`] after a yield): to give a terminal
    /// back as it was found.
    pub fn on_forced_exit(hook: impl Fn() + Send + Sync + 'static) {
        if let Some(hub) = owner() {
            hub.on_forced_exit(Box::new(hook));
        }
    }

    /// A turn started (`true`) or ended, for what the owner says it is
    /// doing.
    pub fn turn(running: bool) {
        if let Some(hub) = owner() {
            hub.set_turn(running);
        }
    }

    /// How to count the session's background agents, for the same.
    pub fn watch_agents(count: impl Fn() -> usize + Send + Sync + 'static) {
        if let Some(hub) = owner() {
            hub.watch_agents(Box::new(count));
        }
    }

    /// The program driving the session (`serve`'s client), for the same.
    pub fn note_client(name: &str) {
        if let Some(hub) = owner() {
            hub.note_client(name);
        }
    }
}

#[cfg(not(unixlike))]
mod imp {
    use super::*;

    pub fn describe(_: &Live) -> Vec<String> {
        Vec::new()
    }
    pub fn in_use_message(_: &Live) -> String {
        String::new()
    }
    pub fn takes_remotes(_: &Live) -> bool {
        false
    }
    pub fn relay(_: &Live) -> Result<u8> {
        anyhow::bail!("not supported on this platform")
    }

    pub fn live_elsewhere(_: &Path) -> bool {
        false
    }
    pub fn start(_: &Path, _: Kind, _: Option<Choice>, _: bool) -> Result<Start> {
        Ok(Start::Go { took_over: false })
    }
    pub fn release() {}
    pub fn yielding() -> Option<String> {
        None
    }
    pub fn on_yield(_: impl Fn() + Send + Sync + 'static) {}
    pub fn on_forced_exit(_: impl Fn() + Send + Sync + 'static) {}
    pub fn turn(_: bool) {}
    pub fn watch_agents(_: impl Fn() -> usize + Send + Sync + 'static) {}
    pub fn note_client(_: &str) {}
}

#[cfg(not(unixlike))]
pub use imp::*;
