//! The local model's server: the coordinator as the one that runs oxinfer
//! for every atelier of the user (see `crate::local`).
//!
//! An atelier that uses the model on this machine asks for the server, on a
//! connection of its own:
//!
//! ```text
//! local {bin, update, version, proto, pid, start?, lease?}
//!   → {state: "up", port, key, pid, version, bin, leases}
//!   → {state: "starting"}        ask again in a moment
//!   → {state: "down"}            only with start: false
//! ```
//!
//! With no server running the coordinator starts one from `bin` — `oxinfer
//! serve` on a loopback port of its choosing, with a key of its making in
//! the server's environment and **no model**: each atelier has the server
//! load its own through oxinfer's management API. Starting takes a moment
//! (longer when oxinfer is first brought up to date, `update`), so the
//! request is answered at once with `starting` and asked again.
//!
//! **A lease is its connection's**, as a session claim is: the request
//! makes the connection a user of the server, and it stops being one when
//! it ends, however the process went. The server is stopped once nobody has
//! held a lease for a minute. Nothing is written for a lease.
//!
//! **The server outlives the coordinator.** It is started detached, and
//! what it takes to find it again — port, key, pid and when that process
//! started — is kept in a private file (`oxinfer-server.json`, mode 0600,
//! beside the socket). A coordinator that starts reads it and *adopts* the
//! server if the process is still that one and answers to its key; the
//! holders make their leases again as they reconnect, and the idle minute
//! counts from the adoption, which gives them the time to. A coordinator
//! that ends with no lease held stops the server on its way out.
//!
//! `local-status` tells the same without a lease and without starting
//! anything; `local-stop` ends the server now. Like `status`, none of the
//! three needs a `hello`.
//!
//! The key travels over the coordinator's socket only (mode 0600, in a
//! directory of mode 0700): it reaches the user's own processes.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::claims::Conn;
use super::{ERR_INCOMPATIBLE, ERR_SUPERSEDED, PROTO};
use crate::own::proc;
use crate::update::VERSION;

/// Where a running server is written down, in the coordinator's directory.
pub(super) const STATE_FILE: &str = "oxinfer-server.json";
/// The server's own output, there too.
pub const LOG_FILE: &str = "oxinfer.log";
/// The log is started again once it is past this.
const LOG_MAX: u64 = 1024 * 1024;
/// How long a server without a model gets to start answering.
const START_WAIT: Duration = Duration::from_secs(30);
/// How long a server found recorded gets to answer before it is given up
/// on: the coordinator before this one may have just started it.
const ADOPT_WAIT: Duration = Duration::from_secs(10);
/// How long a server asked to end gets before it is killed.
const END_WAIT: Duration = Duration::from_secs(5);
/// How long a failed start is told to whoever asks, before one of them
/// makes the coordinator try again.
pub(super) const FAILED_KEEP: Duration = Duration::from_secs(3);

/// How long an instance waits for the server: the start, and before it the
/// update of an oxinfer too old to serve.
const ACQUIRE_WAIT: Duration = Duration::from_secs(420);
/// The pause between two askings.
const POLL: Duration = Duration::from_millis(150);
/// The pause before looking for a coordinator again: the first, and the
/// cap it doubles up to.
const RETRY_MIN: Duration = Duration::from_millis(100);
const RETRY_MAX: Duration = Duration::from_secs(5);

/// A running server, as the coordinator knows it and writes it down.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Server {
    pub port: u16,
    pub key: String,
    pub pid: u32,
    /// When the process `pid` started (`proc::start_token`): what tells it
    /// from another process that got the number later.
    pub start: String,
    /// The program it runs, and the version that program said it was.
    pub bin: String,
    pub version: String,
}

impl Server {
    fn to_value(&self) -> Value {
        json!({
            "port": self.port,
            "key": self.key,
            "pid": self.pid,
            "start": self.start,
            "bin": self.bin,
            "version": self.version,
        })
    }

    fn from_value(v: &Value) -> Option<Server> {
        Some(Server {
            port: u16::try_from(v["port"].as_u64()?).ok()?,
            key: v["key"].as_str()?.to_string(),
            pid: u32::try_from(v["pid"].as_u64()?).ok()?,
            start: v["start"].as_str()?.to_string(),
            bin: v["bin"].as_str().unwrap_or_default().to_string(),
            version: v["version"].as_str().unwrap_or_default().to_string(),
        })
    }

    /// Whether the process is still there, and still the one started.
    pub(super) fn running(&self) -> bool {
        proc::start_token(self.pid).as_deref() == Some(self.start.as_str())
    }

    fn answers(&self) -> bool {
        crate::local::answers(self.port, &self.key)
    }
}

/// Where the server is at.
#[derive(Debug, Default)]
pub(super) enum Phase {
    #[default]
    Down,
    /// Being started, or looked for (a thread is at it).
    Starting,
    Up(Server),
    /// The last start failed, with why and when.
    Failed(String, Instant),
}

/// What the coordinator keeps of the local model's server.
#[derive(Debug)]
pub(super) struct Table {
    pub phase: Phase,
    /// The connections that use the server, with what each said of itself.
    pub leases: BTreeMap<u64, Value>,
    /// Since when nobody has held a lease.
    pub unused_since: Instant,
}

impl Default for Table {
    fn default() -> Self {
        Self {
            phase: Phase::Down,
            leases: BTreeMap::new(),
            unused_since: Instant::now(),
        }
    }
}

impl Table {
    /// Whether there is nothing here to stay for: no server, none on its
    /// way, nobody waiting for one.
    pub(super) fn idle(&self) -> bool {
        self.leases.is_empty() && matches!(self.phase, Phase::Down | Phase::Failed(..))
    }

    /// The table for `local` and `local-status`; the key only `with_key`.
    pub(super) fn describe(&self, with_key: bool) -> Value {
        match &self.phase {
            Phase::Up(server) => {
                let mut v = server.to_value();
                v["state"] = "up".into();
                v["leases"] = self.leases.len().into();
                if !with_key {
                    v.as_object_mut().expect("an object").remove("key");
                }
                v
            }
            Phase::Starting => json!({ "state": "starting", "leases": self.leases.len() }),
            Phase::Down | Phase::Failed(..) => json!({ "state": "down" }),
        }
    }
}

/// The server written down in `dir`, if one is.
fn recorded(dir: &Path) -> Option<Server> {
    let text = std::fs::read(dir.join(STATE_FILE)).ok()?;
    Server::from_value(&serde_json::from_slice(&text).ok()?)
}

/// Whether a server is written down in `dir` (running or not).
pub(super) fn is_recorded(dir: &Path) -> bool {
    dir.join(STATE_FILE).exists()
}

/// Written whole, private (it holds the key), then moved into place.
fn record(dir: &Path, server: &Server) -> Result<()> {
    let tmp = dir.join(format!("{STATE_FILE}.{}", std::process::id()));
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(server.to_value().to_string().as_bytes())?;
        std::fs::rename(&tmp, dir.join(STATE_FILE))
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written.context("writing down the local model's server")
}

/// Forget the record of `server` — not another's, should one have been
/// started since.
fn forget(dir: &Path, server: &Server) {
    if recorded(dir).is_none_or(|r| r.pid == server.pid && r.start == server.start) {
        let _ = std::fs::remove_file(dir.join(STATE_FILE));
    }
}

/// The server a coordinator before this one left running, if it is still
/// there: the process recorded, started when the record says, answering to
/// its key. One that is there and does not answer in time is ended; a
/// record of a process that is gone is removed.
pub(super) fn adopt(dir: &Path) -> Option<Server> {
    let Some(server) = recorded(dir) else {
        let _ = std::fs::remove_file(dir.join(STATE_FILE));
        return None;
    };
    let deadline = Instant::now() + ADOPT_WAIT;
    while server.running() {
        if server.answers() {
            return Some(server);
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    end(dir, &server);
    None
}

/// Start a server from `bin` and wait for it to answer. `update` allows
/// bringing the program up to date first (see `crate::local::make_current`).
pub(super) fn start(dir: &Path, bin: &Path, update: bool, log: &dyn Fn(&str)) -> Result<Server> {
    let version = crate::local::make_current(bin, update, &mut |line| log(line))?;

    // A free port: bound, read, released for the server to take.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .context("finding a free port")?
        .port();
    let key = crate::claude::new_uuid();
    let log_path = dir.join(LOG_FILE);
    let big = std::fs::metadata(&log_path).is_ok_and(|m| m.len() > LOG_MAX);
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(!big)
        .write(true)
        .truncate(big)
        .open(&log_path)
        .map_or_else(|_| Stdio::null(), Stdio::from);
    let mut child = std::process::Command::new(bin)
        .args(["serve", "--listen", &format!("127.0.0.1:{port}")])
        // Screenshots are megabytes once in base64.
        .args(["--max-body-mb", "64"])
        // oxinfer is updated between two servers (`make_current`), never
        // under one: its own restart-into-a-new-version forgets the models
        // the sessions had it load.
        .arg("--no-update-check")
        // In the environment, not on the command line for `ps` to show.
        .env("OXINFER_API_KEY", &key)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(out)
        // Its own process group: it is nobody's terminal's, and it stays
        // when this coordinator goes.
        .process_group(0)
        .spawn()
        .with_context(|| format!("starting {}", bin.display()))?;
    let pid = child.id();
    let start = proc::start_token(pid);
    // Reaped whenever it ends, so it never lingers as a zombie of ours —
    // and so that one that ended reads as gone.
    let (ended_tx, ended) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = ended_tx.send(child.wait());
    });
    let Some(start) = start else {
        let status = ended.recv_timeout(Duration::from_secs(2));
        bail!(
            "oxinfer ended at once ({}); see {}",
            status.map_or_else(|_| "?".to_string(), |s| format!("{s:?}")),
            log_path.display()
        );
    };
    let server = Server {
        port,
        key,
        pid,
        start,
        bin: bin.to_string_lossy().into_owned(),
        version,
    };
    // Written down before it answers: should this coordinator be killed
    // now, the next one finds the server and adopts or ends it.
    record(dir, &server)?;

    let deadline = Instant::now() + START_WAIT;
    while !server.answers() {
        if let Ok(status) = ended.try_recv() {
            forget(dir, &server);
            bail!(
                "oxinfer ended at once ({}); see {}",
                status.map_or_else(|e| e.to_string(), |s| s.to_string()),
                log_path.display()
            );
        }
        if Instant::now() >= deadline {
            end(dir, &server);
            bail!(
                "oxinfer did not start answering; see {}",
                log_path.display()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(server)
}

/// End `server` — if the process is still the one that was started — and
/// forget its record.
pub(super) fn end(dir: &Path, server: &Server) {
    forget(dir, server);
    if !server.running() {
        return;
    }
    proc::signal(server.pid, "TERM");
    let deadline = Instant::now() + END_WAIT;
    while server.running() {
        if Instant::now() >= deadline {
            proc::signal(server.pid, "KILL");
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ---- the instance's side ----

/// What an instance asks the server with.
#[derive(Debug, Clone)]
pub struct Ask {
    /// The oxinfer to start, if none is running.
    pub bin: PathBuf,
    /// Whether that program may be brought up to date first: not one the
    /// user named themselves, nor when updates are turned off.
    pub update: bool,
}

/// Where the server is, for the one that holds a lease on it.
#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    pub port: u16,
    pub key: String,
    pub pid: u64,
    /// The version of the oxinfer that runs.
    pub version: String,
}

impl Endpoint {
    fn from_value(v: &Value) -> Option<Endpoint> {
        Some(Endpoint {
            port: u16::try_from(v["port"].as_u64()?).ok()?,
            key: v["key"].as_str()?.to_string(),
            pid: v["pid"].as_u64().unwrap_or(0),
            version: v["version"].as_str().unwrap_or_default().to_string(),
        })
    }
}

/// This process's use of the server: held for as long as this is, and made
/// again with every coordinator that replaces the one it was made with.
pub struct Lease {
    shared: Arc<Held>,
}

struct Held {
    stop: AtomicBool,
    /// The connection the lease is on, to close it from another thread.
    stream: Mutex<Option<UnixStream>>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        let stream = self.shared.stream.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(stream) = stream.as_ref() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// One asking, to the end: a connection that holds the lease, and where
/// the server is — `None` when there is none and none was to be started.
fn take(
    dir: &Path,
    exe: Option<&Path>,
    ask: &Ask,
    start: bool,
    wait: Duration,
    say: &mut dyn FnMut(&str),
) -> Result<(Conn, Option<Endpoint>)> {
    let deadline = Instant::now() + wait;
    let params = json!({
        "bin": ask.bin.to_string_lossy(),
        "update": ask.update,
        "start": start,
        "pid": std::process::id(),
        "version": VERSION,
        "proto": PROTO,
    });
    let mut conn: Option<Conn> = None;
    let mut told = false;
    loop {
        let link = match conn.as_mut() {
            Some(link) => link,
            // A connection that cannot be made is not tried again: that
            // already waited for a coordinator to come up.
            None => conn.insert(Conn::open(dir, exe)?),
        };
        let pause = match link.request("local", params.clone()) {
            Ok(Ok(result)) => match result["state"].as_str() {
                Some("up") => {
                    let endpoint = Endpoint::from_value(&result)
                        .context("the coordinator's answer names no server")?;
                    return Ok((conn.take().expect("in use"), Some(endpoint)));
                }
                Some("down") if !start => return Ok((conn.take().expect("in use"), None)),
                _ => {
                    if !told && start {
                        told = true;
                        say("starting the local model's server…");
                    }
                    POLL
                }
            },
            Ok(Err((ERR_INCOMPATIBLE, message))) => bail!(message),
            // On its way out for a newer one, which the next try starts.
            Ok(Err((ERR_SUPERSEDED, _))) => {
                conn = None;
                RETRY_MIN
            }
            // A coordinator from before it ran the server. It steps down
            // for a newer atelier that says hello, as this one is.
            Ok(Err((code, message))) if code == -32601 || message.contains("say hello") => {
                let hello = json!({ "version": VERSION, "proto": PROTO });
                let _ = link.request("hello", hello);
                conn = None;
                RETRY_MIN
            }
            Ok(Err((_, message))) => bail!(message),
            // The connection ended under the request.
            Err(e) => {
                conn = None;
                if Instant::now() >= deadline {
                    return Err(e);
                }
                RETRY_MIN
            }
        };
        if Instant::now() >= deadline {
            bail!("the local model's server did not come up in time");
        }
        std::thread::sleep(pause);
    }
}

/// The server, started if there is none, and this process's lease on it.
/// `exe` is the atelier to start a coordinator from when none answers;
/// `say` gets a line when the server has to be started.
pub fn acquire(
    dir: &Path,
    exe: Option<&Path>,
    ask: &Ask,
    say: &mut dyn FnMut(&str),
) -> Result<(Lease, Endpoint)> {
    let (conn, endpoint) = take(dir, exe, ask, true, ACQUIRE_WAIT, say)?;
    let endpoint = endpoint.expect("asked to start one");
    let shared = Arc::new(Held {
        stop: AtomicBool::new(false),
        stream: Mutex::new(conn.out.try_clone().ok()),
    });
    let keeper = Arc::clone(&shared);
    let (dir, exe, ask) = (dir.to_path_buf(), exe.map(Path::to_path_buf), ask.clone());
    std::thread::spawn(move || keep(&keeper, conn, &dir, exe.as_deref(), &ask));
    Ok((Lease { shared }, endpoint))
}

/// The holder's side of the lease, for as long as it is held: when the
/// connection it is on ends — the coordinator was replaced, or killed —
/// find the next coordinator (starting it, as instances do) and say again
/// that the server is in use. That coordinator found the server by itself;
/// it is not asked to start another.
fn keep(held: &Held, mut conn: Conn, dir: &Path, exe: Option<&Path>, ask: &Ask) {
    let stopped = || held.stop.load(Ordering::Relaxed);
    loop {
        while !stopped() {
            if conn
                .read(Instant::now() + Duration::from_secs(3600))
                .is_err()
            {
                break;
            }
        }
        let mut delay = RETRY_MIN;
        conn = loop {
            if stopped() {
                return;
            }
            match take(dir, exe, ask, false, Duration::from_secs(15), &mut |_| {}) {
                Ok((conn, _)) => break conn,
                Err(_) => {
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(RETRY_MAX);
                }
            }
        };
        *held.stream.lock().unwrap_or_else(|p| p.into_inner()) = conn.out.try_clone().ok();
        if stopped() {
            return;
        }
    }
}

/// The server as a running coordinator has it (`local-status`: with the
/// key), `None` when no coordinator answers. Never starts either.
pub fn status(dir: &Path) -> Option<Value> {
    super::client::control(dir, "local-status").ok()
}

/// End the server now. Whether there was one to end.
pub fn stop(dir: &Path) -> bool {
    super::client::control(dir, "local-stop").is_ok_and(|r| r["stopped"] == true)
}

/// The server's line of `atelier coordinator status`, if there is one.
pub fn render(status: &Value) -> Option<String> {
    let local = &status["local"];
    match local["state"].as_str()? {
        "up" => Some(format!(
            "local model server: oxinfer {} (pid {}) on 127.0.0.1:{}, used by {}",
            local["version"].as_str().unwrap_or("?"),
            local["pid"],
            local["port"],
            match local["leases"].as_u64().unwrap_or(0) {
                1 => "1 atelier".to_string(),
                n => format!("{n} ateliers"),
            }
        )),
        "starting" => Some("local model server: starting".to_string()),
        _ => None,
    }
}
