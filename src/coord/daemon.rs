//! The coordinator daemon (`atelier coordinator`).
//!
//! One thread accepts connections; each connection gets a reader thread
//! (which also handles its requests) and a writer thread fed by a channel,
//! so routing never blocks on a slow peer. All routing state sits behind a
//! single mutex and is lost when the daemon ends — instances bring it back
//! when they reconnect (identity, topic subscriptions, the advisory locks
//! they hold — see [`super::locks`]).
//!
//! The daemon ends on its own when:
//! - an instance newer than it says hello (`superseded`): the instance then
//!   starts its own, newer coordinator;
//! - its executable changes on disk (a self-update or a rebuild), so the
//!   next one runs the new code;
//! - no instance has been connected for [`Config::idle`];
//! - it is asked to (`shutdown`, i.e. `atelier coordinator stop`).
//!
//! Exclusivity comes from an advisory lock on `coordinator.lock`, held for
//! the daemon's lifetime and released by the kernel however it dies, so a
//! stale socket file can be replaced without racing another daemon.

use std::collections::{BTreeSet, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use super::{ERR_GONE, ERR_INCOMPATIBLE, ERR_SUPERSEDED, ERR_TIMEOUT, PROTO, socket_path};
use crate::update::{VERSION, parse_version};

/// How long the daemon outlives its last instance.
const IDLE: Duration = Duration::from_secs(60);
/// How long a starting daemon waits for a dying one to release the lock.
const LOCK_WAIT: Duration = Duration::from_secs(2);
/// Housekeeping period: call deadlines, idleness.
const TICK: Duration = Duration::from_millis(250);
/// How often the executable is checked for replacement.
const EXE_CHECK: Duration = Duration::from_secs(2);
/// A peer that doesn't take a line within this is dropped.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// `call`: default and longest time the target gets to answer. The longest
/// is what a question to another instance's agent may wait (see
/// [`super::ask`]): an answer that takes a model's turn, not a function's.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const CALL_TIMEOUT_MAX: Duration = super::ask::MAX_TIMEOUT;

pub struct Config {
    pub dir: PathBuf,
    pub idle: Duration,
    /// End when the daemon's executable is replaced on disk.
    pub watch_exe: bool,
}

impl Config {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            idle: IDLE,
            watch_exe: true,
        }
    }
}

/// What goes to a connection's writer thread.
enum Out {
    Line(String),
    /// Close the connection (after everything queued before it).
    Close,
}

/// A registered instance.
struct Peer {
    id: String,
    name: String,
    /// What it told us about itself (`pid`, `cwd`, `model`, `status`,
    /// `version`).
    info: Map<String, Value>,
    topics: BTreeSet<String>,
    /// Unix seconds when it (last) said hello.
    since: u64,
}

impl Peer {
    /// How the peer appears in `list`, and as the `from` of what it sends.
    fn describe(&self) -> Value {
        let mut v = self.info.clone();
        v.insert("id".into(), self.id.as_str().into());
        v.insert("name".into(), self.name.as_str().into());
        v.insert("since".into(), self.since.into());
        v.insert("topics".into(), json!(self.topics));
        Value::Object(v)
    }

    fn from(&self) -> Value {
        json!({ "id": self.id, "name": self.name, "cwd": self.info.get("cwd") })
    }
}

/// An advisory lock (see [`super::locks`]).
struct Held {
    /// The holder's connection.
    conn: u64,
    path: PathBuf,
    note: String,
    /// Unix seconds when it was first acquired.
    since: u64,
}

/// A `call` forwarded to its target, awaiting the answer.
struct Call {
    caller: u64,
    caller_id: Value,
    target: u64,
    deadline: Instant,
}

#[derive(Default)]
struct State {
    /// Every open connection's writer, by connection number.
    conns: HashMap<u64, Sender<Out>>,
    /// The connections that said hello.
    peers: HashMap<u64, Peer>,
    calls: HashMap<u64, Call>,
    next_call: u64,
    /// The advisory locks, oldest first.
    locks: Vec<Held>,
    /// When the last instance left (or the daemon started).
    last_active: Option<Instant>,
    /// Why the daemon is ending, once it is.
    stopping: Option<String>,
}

impl State {
    fn send(&self, conn: u64, line: String) -> bool {
        self.conns
            .get(&conn)
            .is_some_and(|tx| tx.send(Out::Line(line)).is_ok())
    }

    /// Forget a connection: close it, unregister its instance, and settle
    /// the calls it was part of.
    fn drop_conn(&mut self, conn: u64) {
        if let Some(tx) = self.conns.remove(&conn) {
            let _ = tx.send(Out::Close);
        }
        // Its locks go with it — while its name is still known, for the
        // entries of the table the others are sent.
        let held = self.locks.len();
        self.locks.retain(|l| l.conn != conn);
        if self.locks.len() != held {
            self.announce_locks(Some(conn));
        }
        if let Some(peer) = self.peers.remove(&conn) {
            self.last_active = Some(Instant::now());
            let gone: Vec<u64> = self
                .calls
                .iter()
                .filter(|(_, c)| c.target == conn)
                .map(|(k, _)| *k)
                .collect();
            for k in gone {
                if let Some(c) = self.calls.remove(&k) {
                    let msg = format!("{} disconnected before answering", peer.name);
                    self.send(c.caller, error(&c.caller_id, ERR_GONE, &msg));
                }
            }
        }
        self.calls.retain(|_, c| c.caller != conn);
    }

    /// The connection of the instance called `to` (a name or an id).
    fn resolve(&self, to: &str) -> Option<u64> {
        self.peers
            .iter()
            .find(|(_, p)| p.name == to || p.id == to)
            .map(|(c, _)| *c)
    }

    /// One lock as instances are told of it.
    fn lock_entry(&self, l: &Held) -> Value {
        let holder = self.peers.get(&l.conn);
        json!({
            "holder": holder.map(|p| p.name.as_str()),
            "holder_id": holder.map(|p| p.id.as_str()),
            "path": l.path.to_string_lossy(),
            "note": l.note,
            "since": l.since,
        })
    }

    /// The whole lock table.
    fn lock_table(&self) -> Value {
        self.locks.iter().map(|l| self.lock_entry(l)).collect()
    }

    /// Tell every instance (but `except`) that the lock table changed: each
    /// keeps a copy, so that checking it costs no request.
    fn announce_locks(&self, except: Option<u64>) {
        let line = notification("locks", json!({ "locks": self.lock_table() }));
        for conn in self.peers.keys().filter(|c| Some(**c) != except) {
            self.send(*conn, line.clone());
        }
    }

    /// `wanted`, or `wanted-2`, `wanted-3`, … if other instances hold it.
    fn unique_name(&self, wanted: &str, conn: u64) -> String {
        let taken = |n: &str| self.peers.iter().any(|(c, p)| *c != conn && p.name == n);
        if !taken(wanted) {
            return wanted.to_string();
        }
        (2..)
            .map(|i| format!("{wanted}-{i}"))
            .find(|n| !taken(n))
            .expect("some suffix is free")
    }
}

/// What a request gets back.
enum Reply {
    Now(Result<Value, (i64, String)>),
    /// Answer, then end the daemon for the given reason.
    ThenStop(Result<Value, (i64, String)>, String),
    /// Answered later (a forwarded `call`).
    Later,
}

fn fail(msg: impl Into<String>) -> Reply {
    Reply::Now(Err((-32000, msg.into())))
}

struct Daemon {
    state: Mutex<State>,
    dir: PathBuf,
    started: Instant,
}

/// Be the coordinator for `cfg.dir` until it ends. Returns `Ok` at once if
/// another coordinator already serves the directory.
pub fn run(cfg: Config) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&cfg.dir)
        .with_context(|| format!("creating {}", cfg.dir.display()))?;
    let socket = socket_path(&cfg.dir);

    // The lock makes us the only coordinator; it's released when `lock`
    // drops, or by the kernel if we die.
    let lock_path = cfg.dir.join("coordinator.lock");
    let lock = std::fs::File::create(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    let waiting = Instant::now();
    while lock.try_lock().is_err() {
        if super::connect(&cfg.dir).is_ok() {
            return Ok(()); // someone else is serving
        }
        // Held but not serving: a coordinator on its way out (or in).
        if waiting.elapsed() > LOCK_WAIT {
            bail!("another coordinator holds {}", lock_path.display());
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let _ = std::fs::remove_file(&socket); // stale, since we hold the lock
    let listener = {
        let (path, _handle) = super::reachable_socket(&cfg.dir)?;
        UnixListener::bind(path).with_context(|| format!("binding {}", socket.display()))?
    };
    let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600));
    log(&format!(
        "coordinator {VERSION} serving {}",
        socket.display()
    ));

    let daemon = Arc::new(Daemon {
        state: Mutex::new(State {
            last_active: Some(Instant::now()),
            ..State::default()
        }),
        dir: cfg.dir.clone(),
        started: Instant::now(),
    });

    let ticker = Arc::clone(&daemon);
    let (idle, watch) = (cfg.idle, cfg.watch_exe.then(ExeWatch::new).flatten());
    std::thread::spawn(move || ticker.housekeeping(idle, watch));

    let mut next_conn = 0u64;
    for stream in listener.incoming() {
        if daemon.lock().stopping.is_some() {
            break;
        }
        let Ok(stream) = stream else { continue };
        next_conn += 1;
        let (d, conn) = (Arc::clone(&daemon), next_conn);
        std::thread::spawn(move || d.serve(conn, stream));
    }

    // Tell everyone, close every connection, and give the writers a moment
    // to get that out before the process ends.
    let _ = std::fs::remove_file(&socket);
    {
        let mut st = daemon.lock();
        let reason = st.stopping.clone().unwrap_or_default();
        log(&format!("coordinator stopping: {reason}"));
        let bye = notification("shutdown", json!({ "reason": reason }));
        let conns: Vec<u64> = st.conns.keys().copied().collect();
        for conn in conns {
            st.send(conn, bye.clone());
            st.drop_conn(conn);
        }
    }
    std::thread::sleep(Duration::from_millis(100));
    drop(lock);
    Ok(())
}

impl Daemon {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Begin ending the daemon (idempotent).
    fn stop(&self, reason: &str) {
        {
            let mut st = self.lock();
            if st.stopping.is_some() {
                return;
            }
            st.stopping = Some(reason.to_string());
        }
        // The accept loop only looks at the flag when a connection arrives.
        let _ = super::connect(&self.dir);
    }

    fn housekeeping(&self, idle: Duration, mut watch: Option<ExeWatch>) {
        let mut last_exe_check = Instant::now();
        loop {
            std::thread::sleep(TICK);
            let now = Instant::now();
            let mut st = self.lock();
            if st.stopping.is_some() {
                return;
            }
            let late: Vec<u64> = st
                .calls
                .iter()
                .filter(|(_, c)| c.deadline <= now)
                .map(|(k, _)| *k)
                .collect();
            for k in late {
                if let Some(c) = st.calls.remove(&k) {
                    st.send(c.caller, error(&c.caller_id, ERR_TIMEOUT, "call timed out"));
                }
            }
            let idle_out = st.peers.is_empty()
                && st
                    .last_active
                    .is_some_and(|t| now.duration_since(t) >= idle);
            drop(st);
            if idle_out {
                return self.stop("idle");
            }
            if last_exe_check.elapsed() >= EXE_CHECK {
                last_exe_check = now;
                if watch.as_mut().is_some_and(|w| w.changed()) {
                    return self.stop("executable replaced");
                }
            }
        }
    }

    /// One connection, start to finish.
    fn serve(&self, conn: u64, stream: UnixStream) {
        let (tx, rx) = mpsc::channel::<Out>();
        let Ok(mut out) = stream.try_clone() else {
            return;
        };
        let _ = out.set_write_timeout(Some(WRITE_TIMEOUT));
        std::thread::spawn(move || {
            for item in rx {
                let Out::Line(line) = item else { break };
                if out
                    .write_all(line.as_bytes())
                    .and_then(|()| out.write_all(b"\n"))
                    .is_err()
                {
                    break;
                }
            }
            // Also wakes the reader below.
            let _ = out.shutdown(std::net::Shutdown::Both);
        });
        self.lock().conns.insert(conn, tx);

        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { break };
            if let Ok(msg) = serde_json::from_str::<Value>(&line) {
                self.incoming(conn, &msg);
            }
        }
        self.lock().drop_conn(conn);
    }

    fn incoming(&self, conn: u64, msg: &Value) {
        let id = msg.get("id").filter(|v| !v.is_null());
        let Some(method) = msg["method"].as_str() else {
            // No method: an answer to a call we forwarded to this peer.
            if let Some(k) = id.and_then(Value::as_u64) {
                let mut st = self.lock();
                if st.calls.get(&k).is_some_and(|c| c.target == conn)
                    && let Some(c) = st.calls.remove(&k)
                {
                    let mut reply = json!({ "jsonrpc": "2.0", "id": c.caller_id });
                    match msg.get("error") {
                        Some(e) => reply["error"] = e.clone(),
                        None => reply["result"] = msg["result"].clone(),
                    }
                    st.send(c.caller, reply.to_string());
                }
            }
            return;
        };
        // The answer is queued under the lock the request ran under. Were it
        // released in between, another connection's `send` could put a
        // `message` for an instance that has just registered in front of
        // its `hello` answer — and the instance, which takes the first line
        // it reads for that answer, would lose the message.
        let mut st = self.lock();
        let reply = self.request(&mut st, conn, id, method, &msg["params"]);
        let (result, stop) = match reply {
            Reply::Later => return,
            Reply::Now(r) => (r, None),
            Reply::ThenStop(r, why) => (r, Some(why)),
        };
        if let Some(id) = id {
            let line = match result {
                Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }).to_string(),
                Err((code, m)) => error(id, code, &m),
            };
            st.send(conn, line);
        }
        drop(st);
        if let Some(why) = stop {
            self.stop(&why);
        }
    }

    fn request(
        &self,
        st: &mut State,
        conn: u64,
        id: Option<&Value>,
        method: &str,
        p: &Value,
    ) -> Reply {
        // Open to any connection — `atelier coordinator status|stop` never
        // registers as an instance.
        match method {
            "status" => {
                return Reply::Now(Ok(json!({
                    "version": VERSION,
                    "proto": PROTO,
                    "pid": std::process::id(),
                    "uptime_secs": self.started.elapsed().as_secs(),
                    "instances": st.peers.len(),
                })));
            }
            "list" => {
                let mut all: Vec<(&u64, &Peer)> = st.peers.iter().collect();
                all.sort_by_key(|(c, _)| **c);
                let instances: Vec<Value> = all
                    .into_iter()
                    .map(|(c, p)| {
                        let mut v = p.describe();
                        v["you"] = (*c == conn).into();
                        v
                    })
                    .collect();
                return Reply::Now(Ok(json!({ "instances": instances })));
            }
            "shutdown" => return Reply::ThenStop(Ok(json!({})), "requested".into()),
            "hello" => return hello(st, conn, p),
            _ => {}
        }

        if !st.peers.contains_key(&conn) {
            return fail(format!("{method}: say hello first"));
        }
        let text = |key: &str| p[key].as_str().filter(|s| !s.is_empty());
        match method {
            "update" => {
                let me = st.peers.get_mut(&conn).expect("checked above");
                for key in ["status", "model", "cwd"] {
                    if let Some(v) = p.get(key) {
                        me.info.insert(key.into(), v.clone());
                    }
                }
                Reply::Now(Ok(json!({})))
            }
            "send" => {
                let (Some(to), Some(body)) = (text("to"), p["body"].as_str()) else {
                    return fail("send: 'to' and 'body' are required");
                };
                let from = st.peers[&conn].from();
                let (targets, broadcast) = if to == "*" {
                    (
                        st.peers.keys().copied().filter(|c| *c != conn).collect(),
                        true,
                    )
                } else {
                    match st.resolve(to) {
                        Some(c) => (vec![c], false),
                        None => return fail(format!("no instance named '{to}'")),
                    }
                };
                let line = notification(
                    "message",
                    json!({
                        "from": from,
                        "body": body,
                        "broadcast": broadcast,
                        "wake_depth": p["wake_depth"],
                        "at": now(),
                    }),
                );
                let delivered = targets
                    .into_iter()
                    .filter(|c| st.send(*c, line.clone()))
                    .count();
                Reply::Now(Ok(json!({ "delivered": delivered })))
            }
            "subscribe" | "unsubscribe" => {
                let Some(topic) = text("topic") else {
                    return fail(format!("{method}: 'topic' is required"));
                };
                let me = st.peers.get_mut(&conn).expect("checked above");
                if method == "subscribe" {
                    me.topics.insert(topic.to_string());
                } else {
                    me.topics.remove(topic);
                }
                Reply::Now(Ok(json!({ "topics": me.topics })))
            }
            "publish" => {
                let (Some(topic), Some(body)) = (text("topic"), p["body"].as_str()) else {
                    return fail("publish: 'topic' and 'body' are required");
                };
                let line = notification(
                    "message",
                    json!({
                        "from": st.peers[&conn].from(),
                        "topic": topic,
                        "body": body,
                        "wake_depth": p["wake_depth"],
                        "at": now(),
                    }),
                );
                let targets: Vec<u64> = st
                    .peers
                    .iter()
                    .filter(|(c, peer)| **c != conn && peer.topics.contains(topic))
                    .map(|(c, _)| *c)
                    .collect();
                let delivered = targets
                    .into_iter()
                    .filter(|c| st.send(*c, line.clone()))
                    .count();
                Reply::Now(Ok(json!({ "delivered": delivered })))
            }
            "lock" => {
                let Some(path) = text("path").map(PathBuf::from).filter(|p| p.is_absolute()) else {
                    return fail("lock: 'path' must be an absolute path");
                };
                // Advisory: somebody else's lock on the path, around it or
                // inside it is reported, and that is all that happens.
                let theirs: Vec<Value> = st
                    .locks
                    .iter()
                    .filter(|l| l.conn != conn && super::locks::overlaps(&l.path, &path))
                    .map(|l| st.lock_entry(l))
                    .collect();
                if !theirs.is_empty() {
                    return Reply::Now(Ok(json!({ "acquired": false, "held_by": theirs })));
                }
                let note = p["note"].as_str().unwrap_or_default().to_string();
                let since = match st
                    .locks
                    .iter_mut()
                    .find(|l| l.conn == conn && l.path == path)
                {
                    // Acquired again: a new note for the same lock.
                    Some(mine) => {
                        mine.note = note;
                        mine.since
                    }
                    None => {
                        let since = now();
                        st.locks.push(Held {
                            conn,
                            path,
                            note,
                            since,
                        });
                        since
                    }
                };
                st.announce_locks(None);
                Reply::Now(Ok(json!({ "acquired": true, "since": since })))
            }
            "unlock" => {
                // One path, or with none, everything the caller holds.
                let path = text("path").map(PathBuf::from);
                let held = st.locks.len();
                st.locks
                    .retain(|l| l.conn != conn || path.as_ref().is_some_and(|p| *p != l.path));
                let released = held - st.locks.len();
                if released > 0 {
                    st.announce_locks(None);
                }
                Reply::Now(Ok(json!({ "released": released })))
            }
            "locks" => Reply::Now(Ok(json!({ "locks": st.lock_table() }))),
            "call" => {
                let (Some(to), Some(inner)) = (text("to"), text("method")) else {
                    return fail("call: 'to' and 'method' are required");
                };
                let Some(target) = st.resolve(to) else {
                    return fail(format!("no instance named '{to}'"));
                };
                // Without an id nobody waits for the answer; forward anyway.
                let timeout = p["timeout_ms"]
                    .as_u64()
                    .map_or(CALL_TIMEOUT, Duration::from_millis)
                    .min(CALL_TIMEOUT_MAX);
                st.next_call += 1;
                let k = st.next_call;
                let line = json!({
                    "jsonrpc": "2.0",
                    "id": k,
                    "method": "call",
                    "params": {
                        "from": st.peers[&conn].from(),
                        "method": inner,
                        "params": p["params"],
                    },
                })
                .to_string();
                if !st.send(target, line) {
                    return fail(format!("'{to}' is not reachable"));
                }
                if let Some(id) = id {
                    st.calls.insert(
                        k,
                        Call {
                            caller: conn,
                            caller_id: id.clone(),
                            target,
                            deadline: Instant::now() + timeout,
                        },
                    );
                }
                Reply::Later
            }
            other => Reply::Now(Err((-32601, format!("method not found: {other}")))),
        }
    }
}

/// Register (or re-register) the connection as an instance.
fn hello(st: &mut State, conn: u64, p: &Value) -> Reply {
    let version = p["version"].as_str().unwrap_or_default();
    let proto = p["proto"].as_u64().unwrap_or(0);
    let newer = matches!(
        (parse_version(version), parse_version(VERSION)),
        (Some(theirs), Some(ours)) if theirs > ours
    );
    if newer || proto > PROTO {
        let why = format!("superseded by atelier {version}");
        let msg = format!("coordinator {VERSION} is older than atelier {version}; restarting");
        return Reply::ThenStop(Err((ERR_SUPERSEDED, msg)), why);
    }
    if proto != PROTO {
        let msg = format!(
            "atelier {version} speaks coordinator protocol {proto}, but coordinator {VERSION} \
             is running (protocol {PROTO}); update this atelier"
        );
        return Reply::Now(Err((ERR_INCOMPATIBLE, msg)));
    }
    let Some(id) = p["id"].as_str().filter(|s| !s.is_empty()) else {
        return fail("hello: 'id' is required");
    };
    // The same instance on an older connection we haven't seen close yet.
    let stale: Vec<u64> = st
        .peers
        .iter()
        .filter(|(c, peer)| **c != conn && peer.id == id)
        .map(|(c, _)| *c)
        .collect();
    for c in stale {
        st.drop_conn(c);
    }
    let name = st.unique_name(&sanitize(p["name"].as_str().unwrap_or_default()), conn);
    let mut info = Map::new();
    for key in ["pid", "cwd", "model", "status", "version"] {
        if let Some(v) = p.get(key) {
            info.insert(key.into(), v.clone());
        }
    }
    let topics = p["topics"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    st.peers.insert(
        conn,
        Peer {
            id: id.to_string(),
            name: name.clone(),
            info,
            topics,
            since: now(),
        },
    );
    // The locks it held under the coordinator before this one. They were
    // granted then, so they are taken as they are: should another instance
    // have got in first with an overlapping one, both stand — the table is
    // advice, and each of the two can see the other.
    let before = st.locks.len();
    for l in p["locks"].as_array().into_iter().flatten() {
        let Some(path) = l["path"].as_str().map(PathBuf::from) else {
            continue;
        };
        if path.is_absolute() && !st.locks.iter().any(|h| h.conn == conn && h.path == path) {
            st.locks.push(Held {
                conn,
                path,
                note: l["note"].as_str().unwrap_or_default().to_string(),
                since: l["since"].as_u64().unwrap_or_else(now),
            });
        }
    }
    if st.locks.len() != before {
        st.announce_locks(Some(conn));
    }
    Reply::Now(Ok(json!({
        "id": id,
        "name": name,
        "coordinator": { "version": VERSION, "pid": std::process::id(), "proto": PROTO },
        "locks": st.lock_table(),
    })))
}

/// A name instances can type: letters, digits, `-`, `_`, `.`; never empty,
/// and never something `send` would read as an address form (`*`, `#topic`).
fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    if s.is_empty() { "atelier".into() } else { s }
}

fn notification(method: &str, params: Value) -> String {
    json!({ "jsonrpc": "2.0", "method": method, "params": params }).to_string()
}

fn error(id: &Value, code: i64, message: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// To stderr, which whoever launched the daemon points at its log file.
fn log(msg: &str) {
    eprintln!("[{}] {msg}", now());
}

/// Notices the daemon's executable being replaced on disk.
struct ExeWatch {
    path: PathBuf,
    stamp: (u64, u64, i64),
}

impl ExeWatch {
    fn new() -> Option<Self> {
        let path = crate::app::helper_exe().ok()?;
        let stamp = Self::stamp(&path)?;
        Some(Self { path, stamp })
    }

    fn stamp(path: &Path) -> Option<(u64, u64, i64)> {
        let m = std::fs::metadata(path).ok()?;
        Some((m.ino(), m.len(), m.mtime()))
    }

    /// A missing file is a replacement in progress, not yet a change.
    fn changed(&mut self) -> bool {
        Self::stamp(&self.path).is_some_and(|s| s != self.stamp)
    }
}
