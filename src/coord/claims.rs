//! Session claims: the coordinator as the one that says who owns the
//! session of a project directory (see `crate::own`).
//!
//! A run that works on a project's saved session *claims* the directory:
//!
//! ```text
//! claim {dir, record, version, proto, wait?, again?}
//!   → {claimed: true}
//!   → {claimed: false, owner: {…}, heir: {…}|null, waiting: bool}
//!   → {claimed: false, settling_ms: n}
//! ```
//!
//! `dir` is the project's real path and `record` what the claimant says of
//! itself (pid, start time, kind, terminal, socket, version — the
//! coordinator keeps it as given and hands it to whoever finds the
//! directory claimed). The coordinator handles one request at a time, so
//! of two that claim a directory together one is its owner and the other
//! is told who is.
//!
//! **A claim is its connection's.** It is made on a connection of its own —
//! not the instance's, which says `hello`, has a name and receives
//! messages: this one does none of that, and is opened before the session
//! is — and it ends when that connection does, however the process went.
//! Nothing is written anywhere, so nothing is ever left behind.
//!
//! **Taking over.** With `wait`, a claim on a directory somebody owns takes
//! the one place behind the owner (`waiting: true`): when the owner's
//! connection ends, the claim passes to that connection under the lock the
//! end was noticed under, and it is told (`claimed {dir}`, a notification).
//! Nobody else can get the session in between. `unclaim {dir?}` — or
//! closing the connection — gives up a claim or the place behind one.
//!
//! **A coordinator that replaces another** knows nothing of the claims the
//! other held; their owners make them again (`again: true`) as soon as
//! they find it. Until they have had the time to — a second from its
//! start, when it finds the socket of a coordinator that did not end idle —
//! a directory nobody holds is not given to a newcomer (`settling_ms`).
//!
//! `claims {dir?}` lists the claims (`{claims: [{dir, owner, heir}]}`), for
//! a run that owns nothing and only wants to know. Like `status`, `claim`
//! and `claims` need no `hello`.
//!
//! This module is the claimant's side: [`Link`], a connection that claims
//! one directory. The table itself is in [`super::daemon`].

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::{ERR_INCOMPATIBLE, ERR_SUPERSEDED, PROTO};
use crate::update::VERSION;

/// How long the coordinator gets to answer.
const ANSWER: Duration = Duration::from_secs(10);
/// How long a coordinator this process started gets to open its socket.
const SPAWN_WAIT: Duration = Duration::from_secs(5);
/// How long a claim is tried for, across coordinators that end under it.
const CLAIM_WAIT: Duration = Duration::from_secs(15);
/// The pause before trying again.
const RETRY: Duration = Duration::from_millis(50);

/// The way a claim is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    /// By a run that is starting.
    Fresh,
    /// By one taking the session over: if it is somebody's, be next.
    Wait,
    /// By an owner, to a coordinator that does not know it yet.
    Again,
}

/// What the coordinator said to a claim.
#[derive(Debug)]
pub enum Answer {
    /// The session is this connection's.
    Mine,
    /// It is somebody else's: the owner's record, the record of the one
    /// that gets it next if there is one, and whether that is this
    /// connection.
    Held {
        owner: Value,
        heir: Value,
        waiting: bool,
    },
}

/// One connection to a coordinator.
struct Conn {
    out: UnixStream,
    reader: BufReader<UnixStream>,
    /// A line that has not all arrived yet.
    partial: Vec<u8>,
    requests: u64,
    /// The claim waited for was handed over (`claimed`).
    granted: bool,
}

impl Conn {
    /// The next message, if one arrives before `deadline`. An error is a
    /// connection that is over.
    fn read(&mut self, deadline: Instant) -> Result<Option<Value>> {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            self.out
                .set_read_timeout(Some(left.max(Duration::from_millis(1))))?;
            match self.reader.read_until(b'\n', &mut self.partial) {
                Ok(_) if self.partial.ends_with(b"\n") => {
                    let line = std::mem::take(&mut self.partial);
                    if let Ok(message) = serde_json::from_slice::<Value>(&line) {
                        return Ok(Some(message));
                    }
                }
                Ok(_) => bail!("the coordinator closed the connection"),
                Err(e) => match e.kind() {
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                        return Ok(None);
                    }
                    std::io::ErrorKind::Interrupted => {}
                    _ => return Err(e).context("reading from the coordinator"),
                },
            }
        }
    }

    /// Take note of what the coordinator says unasked.
    fn noted(&mut self, message: &Value) {
        if message["method"] == "claimed" {
            self.granted = true;
        }
    }

    /// One request and its answer: the result, or the error's code and
    /// message.
    fn request(&mut self, method: &str, params: Value) -> Result<Result<Value, (i64, String)>> {
        self.requests += 1;
        let id = self.requests;
        let line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.out
            .write_all(format!("{line}\n").as_bytes())
            .context("writing to the coordinator")?;
        let deadline = Instant::now() + ANSWER;
        loop {
            let Some(message) = self.read(deadline)? else {
                bail!("the coordinator did not answer '{method}' in time");
            };
            if message["id"].as_u64() != Some(id) {
                self.noted(&message);
                continue;
            }
            return Ok(match message.get("error") {
                Some(e) => Err((
                    e["code"].as_i64().unwrap_or(-32000),
                    e["message"].as_str().unwrap_or("error").to_string(),
                )),
                None => Ok(message["result"].clone()),
            });
        }
    }
}

/// A connection to the coordinator that claims the session of one project
/// directory, opened again whenever it is needed and found closed.
pub struct Link {
    /// The coordinator's directory.
    dir: PathBuf,
    /// The atelier to start a coordinator from, when none answers.
    exe: Option<PathBuf>,
    /// The project directory claimed, as the coordinator knows it.
    project: String,
    conn: Option<Conn>,
}

/// The name a project directory is claimed under: its real path.
pub fn project_key(root: &Path) -> String {
    let real = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    real.to_string_lossy().into_owned()
}

impl Link {
    /// A link for the project in `root`, to the coordinator this process
    /// would join. `None` when there is none to speak of: it is switched
    /// off, or has nowhere to be.
    pub fn open(root: &Path) -> Option<Link> {
        if super::disabled() {
            return None;
        }
        let dir = super::runtime_dir()?;
        Some(Link::at(dir, crate::app::helper_exe().ok(), root))
    }

    /// A link to the coordinator in `dir`.
    pub fn at(dir: PathBuf, exe: Option<PathBuf>, root: &Path) -> Link {
        Link {
            dir,
            exe,
            project: project_key(root),
            conn: None,
        }
    }

    /// The connection, made if there is none — starting a coordinator if
    /// none answers.
    fn conn(&mut self) -> Result<&mut Conn> {
        if self.conn.is_none() {
            let stream = match super::connect(&self.dir) {
                Ok(stream) => stream,
                Err(e) => {
                    let Some(exe) = &self.exe else {
                        return Err(e.context("no coordinator is running"));
                    };
                    super::client::spawn_daemon(exe, &self.dir)?;
                    let deadline = Instant::now() + SPAWN_WAIT;
                    loop {
                        match super::connect(&self.dir) {
                            Ok(stream) => break stream,
                            Err(e) if Instant::now() >= deadline => {
                                return Err(e.context(format!(
                                    "the coordinator did not come up (see {})",
                                    self.dir.join("coordinator.log").display()
                                )));
                            }
                            Err(_) => std::thread::sleep(Duration::from_millis(25)),
                        }
                    }
                }
            };
            stream.set_write_timeout(Some(ANSWER))?;
            self.conn = Some(Conn {
                reader: BufReader::new(stream.try_clone()?),
                out: stream,
                partial: Vec::new(),
                requests: 0,
                granted: false,
            });
        }
        Ok(self.conn.as_mut().expect("just made"))
    }

    /// Claim the project's session, describing this process as `record`.
    ///
    /// A coordinator that ends under the claim (it was superseded, it was
    /// killed) is replaced and asked again, and one that is settling is
    /// waited for. An error means there is no telling: no coordinator could
    /// be reached or started, or the one running cannot serve this atelier.
    pub fn claim(&mut self, record: &Value, how: How) -> Result<Answer> {
        let deadline = Instant::now() + CLAIM_WAIT;
        loop {
            let params = json!({
                "dir": self.project,
                "record": record,
                "version": VERSION,
                "proto": PROTO,
                "wait": how == How::Wait,
                "again": how == How::Again,
            });
            // A connection that cannot be made is not tried again: that
            // already waited for a coordinator to come up.
            let answer = self.conn()?.request("claim", params);
            let pause = match answer {
                Ok(Ok(result)) if result["claimed"] == true => return Ok(Answer::Mine),
                Ok(Ok(result)) => match result["settling_ms"].as_u64() {
                    Some(ms) => Duration::from_millis(ms).min(RETRY * 2),
                    None => {
                        return Ok(Answer::Held {
                            owner: result["owner"].clone(),
                            heir: result["heir"].clone(),
                            waiting: result["waiting"] == true,
                        });
                    }
                },
                Ok(Err((ERR_INCOMPATIBLE, message))) => bail!(message),
                // On its way out for a newer one, which the next try starts.
                Ok(Err((ERR_SUPERSEDED, _))) => {
                    self.conn = None;
                    RETRY
                }
                // A coordinator from before there were claims. It steps
                // down for a newer atelier that says hello, as this one is.
                Ok(Err((code, message))) if code == -32601 || message.contains("say hello") => {
                    let hello = json!({ "version": VERSION, "proto": PROTO });
                    let _ = self.conn()?.request("hello", hello);
                    self.conn = None;
                    RETRY
                }
                Ok(Err((_, message))) => bail!(message),
                // The connection ended under the request.
                Err(e) => {
                    self.conn = None;
                    if Instant::now() >= deadline {
                        return Err(e);
                    }
                    RETRY
                }
            };
            if Instant::now() >= deadline {
                bail!("the coordinator did not settle the claim in time");
            }
            std::thread::sleep(pause);
        }
    }

    /// Whether the claim waited for ([`How::Wait`]) has been handed to this
    /// connection, waiting up to `within` for the word. An error when the
    /// connection — and the place in line with it — is gone.
    pub fn granted(&mut self, within: Duration) -> Result<bool> {
        let Some(conn) = self.conn.as_mut() else {
            bail!("not connected to the coordinator");
        };
        let deadline = Instant::now() + within;
        while !conn.granted {
            match conn.read(deadline) {
                Ok(Some(message)) => conn.noted(&message),
                Ok(None) => break,
                Err(e) => {
                    self.conn = None;
                    return Err(e);
                }
            }
        }
        Ok(conn.granted)
    }

    /// Wait for the connection to end, as long as that takes: for an owner,
    /// whose claim ends with it.
    pub fn watch(&mut self) {
        while let Some(conn) = self.conn.as_mut() {
            if conn
                .read(Instant::now() + Duration::from_secs(3600))
                .is_err()
            {
                self.conn = None;
            }
        }
    }

    /// Give up what this connection holds or waits for.
    pub fn let_go(&mut self) {
        self.conn = None;
    }
}

/// The owner of the session in `root` as a running coordinator has it:
/// its record. `None` when nobody owns it, and when there is no telling —
/// no coordinator is started to ask.
pub fn owner_of(root: &Path) -> Option<Value> {
    if super::disabled() {
        return None;
    }
    owner_in(&super::runtime_dir()?, root)
}

/// [`owner_of`], asked of the coordinator in `dir`.
pub fn owner_in(dir: &Path, root: &Path) -> Option<Value> {
    let params = json!({ "dir": project_key(root) });
    let answer = super::client::control_with(dir, "claims", params).ok()?;
    let owner = answer["claims"].as_array()?.first()?["owner"].clone();
    owner.is_object().then_some(owner)
}

/// The claims as `atelier coordinator status` prints them.
pub fn render(claims: &Value) -> Vec<String> {
    let lines = claims["claims"].as_array().into_iter().flatten().map(|c| {
        let owner = &c["owner"];
        let mut line = format!(
            "session in {}: pid {} ({})",
            c["dir"].as_str().unwrap_or("?"),
            owner["pid"],
            owner["kind"].as_str().unwrap_or("?"),
        );
        if c["heir"].is_object() {
            line.push_str(&format!(", being taken over by pid {}", c["heir"]["pid"]));
        }
        line
    });
    lines.collect()
}
