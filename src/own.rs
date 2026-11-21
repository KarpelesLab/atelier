//! Who owns a project's session: one atelier at a time.
//!
//! A project has one saved session (`session.json` in its state directory,
//! [`crate::state`]), and every run
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
//! # The claim
//!
//! Who owns the session of a directory is the coordinator's to say (see
//! [`crate::coord::claims`]): a run that would own it *claims* the
//! directory, by its real path, on a connection it then keeps open, and is
//! told that the session is its own or whose it is. The coordinator answers
//! one request at a time, so of two that start together one is the owner.
//! The claim ends with the connection, however the process went, and
//! nothing is written anywhere — not in the project, not beside the
//! coordinator — so a crash leaves nothing to clean up.
//!
//! With the claim goes the [`Record`]: the owner's pid and when that
//! process started, when the session did, what kind of run it is, the
//! terminal it is on, and the socket it listens on. A newcomer told the
//! session is somebody's is handed that record. Before every signal it
//! checks that the pid is still the process the record describes (its start
//! time).
//!
//! Without a coordinator there is nobody to say, and every run starts as
//! if it were alone: when it is switched off (`ATELIER_NO_COORDINATOR`)
//! nothing is asked at all, and when none can be reached or started the
//! run says so, starts, and claims the session once one answers (see
//! `keep`, which also holds the claim across coordinators).
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
//! See [`takeover`]: the newcomer takes the place behind the owner in the
//! coordinator's table, and the owner is asked to stop, save and exit; if
//! it has not a few seconds later it is signalled (TERM, then KILL), and
//! what it had started is stopped with it. The claim passes from the one to
//! the other with nobody in between.
//!
//! Unix only (`cfg(unixlike)`), like the sockets and the coordinator it is
//! made of: elsewhere [`start`] always says go, and nothing is recorded.

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

/// What the coordinator holds of a session's owner: the owner, as it
/// described itself.
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
    /// The atelier it is.
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
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, Instant, SystemTime};

    use crate::coord::claims::{self, Answer, How};

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
        use std::io::{BufRead, BufReader, Write};
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

    /// What the owner has to tell its user, when its interface next looks.
    static NOTICES: Mutex<Vec<String>> = Mutex::new(Vec::new());

    fn notice(line: String) {
        NOTICES.lock().unwrap_or_else(|p| p.into_inner()).push(line);
    }

    /// Something about the ownership of this session that its user should
    /// read (see [`keep`]), once: for the interfaces to show where they
    /// wait.
    pub fn take_notice() -> Option<String> {
        let mut notices = NOTICES.lock().unwrap_or_else(|p| p.into_inner());
        (!notices.is_empty()).then(|| notices.remove(0))
    }

    /// The record's process is the one that wrote it, and is still there.
    /// (An owner on a system that could not tell it when it started wrote
    /// no start time: there the process being there has to do, next to the
    /// claim it is seen to hold.)
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

    /// What claiming the session found.
    pub(super) enum Claimed {
        /// The claim is this process's: it owns the session.
        Owner,
        /// Another process owns it.
        Live(Live),
        /// There is no telling, and why: no coordinator answers.
        Unknown(String),
    }

    /// Whose turn it is when a take-over waits (see [`Seat::next`]).
    pub(super) enum Turn {
        /// The session is this process's.
        Mine,
        /// It is still the owner's, and this process's next.
        Waiting,
        /// It is another atelier's now, or about to be: not what this was
        /// about. In a sentence.
        Changed(String),
    }

    /// A start that would own the session, until it does or gives up: its
    /// connection to the coordinator, what it says of itself, and the
    /// socket it will answer on.
    pub(super) struct Seat {
        link: claims::Link,
        record: Record,
        socket: Bound,
        /// The coordinator said the session is this process's.
        mine: bool,
    }

    /// A socket bound and not answered on yet: taken away again if it
    /// never is (the run was not the owner after all).
    struct Bound(Option<(UnixListener, PathBuf)>);

    impl Drop for Bound {
        fn drop(&mut self) {
            if let Some((_, path)) = self.0.take() {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    impl Seat {
        fn new(link: claims::Link, kind: Kind) -> Seat {
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
            Seat {
                link,
                record,
                socket: Bound(socket),
                mine: false,
            }
        }

        fn value(&self) -> Value {
            serde_json::to_value(&self.record).unwrap_or_default()
        }

        /// Claim the session.
        fn claim(&mut self) -> Result<Claimed> {
            // An owner that has just died is still the owner until the
            // coordinator has seen its connection close: asked again.
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let owner = match self.link.claim(&self.value(), How::Fresh) {
                    Ok(Answer::Mine) => {
                        self.mine = true;
                        return Ok(Claimed::Owner);
                    }
                    Ok(Answer::Held { owner, .. }) => owner,
                    Err(e) => return Ok(Claimed::Unknown(format!("{e:#}"))),
                };
                let record: Record = serde_json::from_value(owner).map_err(|e| {
                    anyhow::anyhow!(
                        "a session is already running in this directory, and what the \
                         coordinator says of its owner cannot be read ({e}). Nothing was changed"
                    )
                })?;
                // A process that cannot be seen from here (another pid
                // namespace) is the owner all the same: it holds the claim.
                if genuine(&record) || Instant::now() >= deadline {
                    let status = hub::ask(&record, "owner/status", Value::Null).ok();
                    return Ok(Claimed::Live(Live { record, status }));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }

        /// Claim the session if it is free now; `false` while it is not.
        fn try_claim(&mut self) -> bool {
            if !self.mine {
                self.mine = matches!(self.link.claim(&self.value(), How::Fresh), Ok(Answer::Mine));
            }
            self.mine
        }

        /// Take the place behind `owner`: the session is this process's
        /// when that one's claim ends, and nobody else's in between.
        pub(super) fn queue(&mut self, owner: &Record) -> Result<Turn> {
            Ok(match self.link.claim(&self.value(), How::Wait)? {
                Answer::Mine => {
                    self.mine = true;
                    Turn::Mine
                }
                Answer::Held {
                    owner: now,
                    waiting,
                    ..
                } if waiting && now["pid"] == owner.pid && now["started"] == owner.started => {
                    Turn::Waiting
                }
                // Another owner than the one this started from, or another
                // atelier already waiting behind it.
                Answer::Held {
                    owner: now, heir, ..
                } => {
                    self.link.let_go();
                    Turn::Changed(match heir["pid"].as_u64() {
                        Some(pid) if now["pid"] == owner.pid => {
                            format!(
                                "another atelier (pid {pid}) is already taking the session over"
                            )
                        }
                        _ => format!(
                            "the session was taken by another atelier (pid {}) meanwhile",
                            now["pid"]
                        ),
                    })
                }
            })
        }

        /// Where a take-over of `owner` stands, waiting up to `within` for
        /// the coordinator's word. A coordinator that was replaced
        /// meanwhile is asked anew.
        pub(super) fn next(&mut self, owner: &Record, within: Duration) -> Result<Turn> {
            if self.mine {
                return Ok(Turn::Mine);
            }
            match self.link.granted(within) {
                Ok(true) => {
                    self.mine = true;
                    Ok(Turn::Mine)
                }
                Ok(false) => Ok(Turn::Waiting),
                Err(_) => self.queue(owner),
            }
        }

        /// Give up the claim, or the place in line.
        pub(super) fn let_go(&mut self) {
            self.mine = false;
            self.link.let_go();
        }

        /// Become the owner: open the socket to whoever asks, and keep the
        /// claim for as long as the process lives.
        fn own(self) {
            let value = self.value();
            let Seat {
                link,
                record,
                mut socket,
                mine,
            } = self;
            let hub = hub::Hub::start(record, socket.0.take());
            let _ = OWNER.set(hub);
            std::thread::spawn(move || keep(link, &value, mine));
        }
    }

    /// How often an owner that found the session claimed by another asks
    /// again.
    const CONTEST: Duration = Duration::from_secs(2);
    /// The pause before looking for a coordinator again: the first, and
    /// the cap it doubles up to.
    const RETRY_MIN: Duration = Duration::from_millis(100);
    const RETRY_MAX: Duration = Duration::from_secs(5);

    /// The owner's side of the claim, for the life of the process: when the
    /// connection that holds it ends — the coordinator was replaced, or
    /// killed — find the next coordinator (starting it, as instances do)
    /// and claim again.
    ///
    /// Two owners of one directory can meet there: they started while no
    /// coordinator could be reached, or one arrived in the moment the
    /// other was between two coordinators. The first to claim has it. The
    /// other is not stopped — nothing is, without somebody asking — but
    /// says so to its user, and keeps asking, so that it is the owner once
    /// the first one ends.
    fn keep(mut link: claims::Link, record: &Value, mut claimed: bool) {
        let mut rival = None;
        let mut delay = RETRY_MIN;
        loop {
            if claimed {
                link.watch();
            }
            match link.claim(record, How::Again) {
                Ok(Answer::Mine) => {
                    claimed = true;
                    delay = RETRY_MIN;
                    if rival.take().is_some() {
                        notice(
                            "the other atelier in this directory has ended: this session is \
                             the only one here again."
                                .to_string(),
                        );
                    }
                }
                Ok(Answer::Held { owner, .. }) => {
                    claimed = false;
                    let pid = owner["pid"].as_u64();
                    if rival != Some(pid) {
                        rival = Some(pid);
                        notice(format!(
                            "another atelier (pid {}) has a session open in this directory \
                             too: the two could not see each other when they started. Both \
                             go on, and both write the saved session — quit one of them.",
                            pid.map_or_else(|| "?".to_string(), |p| p.to_string())
                        ));
                    }
                    std::thread::sleep(CONTEST);
                }
                Err(_) => {
                    claimed = false;
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(RETRY_MAX);
                }
            }
        }
    }

    /// Whether another atelier owns the session in `root` right now. For a
    /// run that does not own it and must not write it.
    pub fn live_elsewhere(root: &Path) -> bool {
        owner().is_none() && claims::owner_of(root).is_some()
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
        // Already this process's: an editor's `session/new` again after
        // one that failed past the claim. A process owns one directory's
        // session, and keeps it for as long as it lives: another directory
        // is another process's (`acp` starts one for it).
        if owner().is_some() {
            let asked = claims::project_key(root);
            if let Some(owned) = CLAIMED.get().filter(|owned| **owned != asked) {
                anyhow::bail!(
                    "this atelier owns the session of {owned}, and a process owns one \
                     directory's session: {asked} is another's"
                );
            }
            return Ok(Start::Go { took_over: false });
        }
        let start = start_owning(root, kind, choice, interactive)?;
        if owner().is_some() {
            let _ = CLAIMED.set(claims::project_key(root));
        }
        Ok(start)
    }

    /// The directory whose session this process owns, by its real path
    /// (see [`start`]); set once, for the life of the process.
    static CLAIMED: OnceLock<String> = OnceLock::new();

    /// The directory whose session this process owns, if it owns one.
    pub fn claimed_dir() -> Option<String> {
        CLAIMED.get().cloned()
    }

    fn start_owning(
        root: &Path,
        kind: Kind,
        choice: Option<Choice>,
        interactive: bool,
    ) -> Result<Start> {
        // Without a coordinator nobody says whose the session is: the run
        // starts as if it were alone.
        let Some(link) = claims::Link::open(root) else {
            return Ok(Start::Go { took_over: false });
        };
        let mut seat = Seat::new(link, kind);
        let mut took_over = false;
        loop {
            let live = match seat.claim()? {
                Claimed::Owner => {
                    seat.own();
                    return Ok(Start::Go { took_over });
                }
                Claimed::Unknown(why) => {
                    eprintln!(
                        "atelier: the coordinator cannot be reached ({why}): whether another \
                         atelier is running in this directory is not known. Starting."
                    );
                    seat.own();
                    return Ok(Start::Go { took_over });
                }
                Claimed::Live(live) => live,
            };
            let choice = match choice {
                Some(choice) => choice,
                None if interactive => {
                    // While the question is up, the owner may end: then
                    // there is nothing to ask about.
                    let mut gone = || seat.try_claim();
                    match prompt::ask(&live, &mut gone)? {
                        prompt::Asked::Choice(choice) => choice,
                        prompt::Asked::Gone => {
                            eprintln!(
                                "the other atelier (pid {}) has ended: starting normally.",
                                live.record.pid
                            );
                            seat.own();
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
                        if seat.try_claim() {
                            eprintln!(
                                "the other atelier (pid {}) has ended: starting normally.",
                                live.record.pid
                            );
                            seat.own();
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
                Choice::TakeOver => match takeover::run(&live, &mut seat)? {
                    takeover::Outcome::Done { notes } => {
                        for note in notes {
                            eprintln!("{note}");
                        }
                        seat.own();
                        return Ok(Start::Go { took_over: true });
                    }
                    takeover::Outcome::Changed(how) => {
                        if !interactive {
                            // A flag was about the session that was there.
                            eprintln!("atelier: {how}; nothing more was done.");
                            return Ok(Start::Exit(EXIT_IN_USE));
                        }
                        // Asked again, about the session as it is now.
                        eprintln!("atelier: {how}.");
                        took_over = true;
                    }
                },
            }
        }
    }

    /// On the way out: take the socket away. The claim goes with the
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
    pub fn claimed_dir() -> Option<String> {
        None
    }
    pub fn take_notice() -> Option<String> {
        None
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
