//! An instance's connection to the coordinator.
//!
//! [`Client::start`] returns immediately; a background thread connects,
//! starting a coordinator when none answers, says hello, and then reads
//! what the coordinator sends. Whenever the connection ends — for any
//! reason — the thread goes back to connecting, for as long as the process
//! lives, and presents the same identity (id, name, topic subscriptions)
//! to whichever coordinator it finds next. Requests made while
//! disconnected wait briefly for the link to come back, then fail with an
//! error saying why it is down.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use super::{ENV_DIR, ERR_INCOMPATIBLE, Message, PROTO, socket_path};
use crate::update::VERSION;

/// How long a request waits for the link before giving up.
const CONNECT_WAIT: Duration = Duration::from_secs(3);
/// How long the coordinator gets to answer a request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a just-started coordinator gets to open its socket.
const SPAWN_WAIT: Duration = Duration::from_secs(3);
/// Delay before reconnecting: the first, and the cap it doubles up to.
const RETRY_MIN: Duration = Duration::from_millis(100);
const RETRY_MAX: Duration = Duration::from_secs(5);
/// Retry period when the running coordinator can't serve this build.
const RETRY_INCOMPATIBLE: Duration = Duration::from_secs(60);
/// Messages kept while nobody reads them.
const INBOX_MAX: usize = 200;
/// The coordinator's log is restarted once it grows past this.
const LOG_MAX: u64 = 256 * 1024;

pub struct Options {
    /// Where the coordinator's socket lives.
    pub dir: PathBuf,
    /// The atelier executable to start a coordinator from; `None` to only
    /// ever connect to an existing one.
    pub exe: Option<PathBuf>,
    /// The name to ask for (the coordinator makes it unique).
    pub name: String,
    pub cwd: String,
    pub model: String,
}

/// What this instance tells the coordinator about itself.
struct Me {
    /// The name asked for; replaced by the one the coordinator assigned, so
    /// a reconnect asks for the same one again.
    name: String,
    cwd: String,
    model: String,
    status: String,
}

#[derive(Default)]
struct Link {
    /// The write half while connected.
    out: Option<UnixStream>,
    /// Why there is no connection, when there isn't.
    down: String,
    /// The coordinator's version, while connected.
    coordinator: String,
}

type Waiter = Sender<Result<Value, String>>;
/// Answers a `call` from another instance: `(method, params)`.
type Handler = Box<dyn Fn(&Value) -> Result<Value, String> + Send + Sync>;

struct Shared {
    dir: PathBuf,
    exe: Option<PathBuf>,
    /// Stable for the life of the process, across coordinators.
    id: String,
    me: Mutex<Me>,
    link: Mutex<Link>,
    linked: Condvar,
    pending: Mutex<HashMap<u64, Waiter>>,
    next_id: AtomicU64,
    inbox: Mutex<VecDeque<Message>>,
    topics: Mutex<BTreeSet<String>>,
    handlers: Mutex<HashMap<String, Handler>>,
    stop: AtomicBool,
}

pub struct Client {
    sh: Arc<Shared>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Client {
    pub fn start(opts: Options) -> Arc<Self> {
        let sh = Arc::new(Shared {
            dir: opts.dir,
            exe: opts.exe,
            id: crate::claude::new_uuid(),
            me: Mutex::new(Me {
                name: opts.name,
                cwd: opts.cwd,
                model: opts.model,
                status: "idle".into(),
            }),
            link: Mutex::new(Link {
                down: "connecting".into(),
                ..Link::default()
            }),
            linked: Condvar::new(),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            inbox: Mutex::new(VecDeque::new()),
            topics: Mutex::new(BTreeSet::new()),
            handlers: Mutex::new(HashMap::new()),
            stop: AtomicBool::new(false),
        });
        let client = Arc::new(Self {
            sh: Arc::clone(&sh),
        });
        client.handle("ping", |_| Ok(json!("pong")));
        let info = Arc::clone(&sh);
        client.handle("info", move |_| Ok(info.describe()));
        std::thread::spawn(move || sh.run());
        client
    }

    /// Answer `call`s of `method` from other instances. Runs on the
    /// connection's reader thread, so it must not block for long.
    pub fn handle(
        &self,
        method: &str,
        f: impl Fn(&Value) -> Result<Value, String> + Send + Sync + 'static,
    ) {
        lock(&self.sh.handlers).insert(method.to_string(), Box::new(f));
    }

    /// This instance's name among the connected instances.
    pub fn name(&self) -> String {
        lock(&self.sh.me).name.clone()
    }

    /// One line on the state of the link, for `/peers`.
    pub fn link_status(&self) -> String {
        let link = lock(&self.sh.link);
        match link.out {
            Some(_) => format!(
                "connected to coordinator {} as '{}'",
                link.coordinator,
                self.name()
            ),
            None => format!("not connected to the coordinator ({})", link.down),
        }
    }

    /// The connected instances, as the coordinator describes them.
    pub fn list(&self) -> Result<Value> {
        self.request("list", json!({}), REQUEST_TIMEOUT)
    }

    /// Send `body` to `to`: an instance's name, `*` for every other
    /// instance, or `#topic` for the topic's subscribers. Returns how many
    /// instances it reached.
    pub fn send(&self, to: &str, body: &str) -> Result<u64> {
        let r = match to.strip_prefix('#') {
            Some(topic) => self.request(
                "publish",
                json!({ "topic": topic, "body": body }),
                REQUEST_TIMEOUT,
            )?,
            None => self.request("send", json!({ "to": to, "body": body }), REQUEST_TIMEOUT)?,
        };
        Ok(r["delivered"].as_u64().unwrap_or(0))
    }

    /// Start (or stop) receiving what is published to `topic`. Remembered,
    /// and renewed with every coordinator this instance connects to.
    pub fn subscribe(&self, topic: &str, on: bool) -> Result<()> {
        let topic = topic.trim_start_matches('#');
        if topic.is_empty() {
            bail!("empty topic");
        }
        {
            let mut topics = lock(&self.sh.topics);
            if on {
                topics.insert(topic.to_string());
            } else {
                topics.remove(topic);
            }
        }
        let method = if on { "subscribe" } else { "unsubscribe" };
        self.request(method, json!({ "topic": topic }), REQUEST_TIMEOUT)?;
        Ok(())
    }

    pub fn topics(&self) -> Vec<String> {
        lock(&self.sh.topics).iter().cloned().collect()
    }

    /// Ask the instance `to` to run `method` and wait for its answer.
    #[allow(dead_code)] // the instance-to-instance RPC primitive; no caller in atelier yet
    pub fn call(&self, to: &str, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let p = json!({
            "to": to,
            "method": method,
            "params": params,
            "timeout_ms": timeout.as_millis() as u64,
        });
        // The coordinator enforces the deadline; allow it time to say so.
        self.request("call", p, timeout + Duration::from_secs(2))
    }

    /// Tell the other instances what this one is doing (`idle`/`working`)
    /// and with which model. Best effort; also resent on reconnect.
    pub fn update(&self, status: &str, model: &str) {
        {
            let mut me = lock(&self.sh.me);
            if me.status == status && me.model == model {
                return;
            }
            me.status = status.to_string();
            me.model = model.to_string();
        }
        let line = json!({
            "jsonrpc": "2.0",
            "method": "update",
            "params": { "status": status, "model": model },
        });
        let _ = self.sh.write(&line);
    }

    /// The messages received since the last call, oldest first.
    pub fn take_messages(&self) -> Vec<Message> {
        lock(&self.sh.inbox).drain(..).collect()
    }

    /// Disconnect and stop reconnecting.
    #[allow(dead_code)] // instances stay connected until the process ends
    pub fn stop(&self) {
        self.sh.stop.store(true, Ordering::Relaxed);
        if let Some(out) = lock(&self.sh.link).out.as_ref() {
            let _ = out.shutdown(std::net::Shutdown::Both);
        }
    }

    fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let sh = &self.sh;
        sh.wait_linked(CONNECT_WAIT)?;
        let id = sh.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        lock(&sh.pending).insert(id, tx);
        let line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(e) = sh.write(&line) {
            lock(&sh.pending).remove(&id);
            return Err(e);
        }
        match rx.recv_timeout(timeout) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(anyhow!(e)),
            Err(_) => {
                lock(&sh.pending).remove(&id);
                bail!("the coordinator did not answer '{method}' in time")
            }
        }
    }
}

/// How a connection attempt ended.
enum Ended {
    /// It was established, then lost.
    Lost,
    /// The coordinator turned us away for good (until it changes).
    Refused(String),
}

impl Shared {
    fn describe(&self) -> Value {
        let me = lock(&self.me);
        json!({
            "id": self.id,
            "name": me.name,
            "pid": std::process::id(),
            "cwd": me.cwd,
            "model": me.model,
            "status": me.status,
            "version": VERSION,
        })
    }

    fn write(&self, msg: &Value) -> Result<()> {
        let mut link = lock(&self.link);
        let Some(out) = link.out.as_mut() else {
            bail!("not connected to the coordinator ({})", link.down);
        };
        out.write_all(format!("{msg}\n").as_bytes())
            .context("writing to the coordinator")
    }

    fn wait_linked(&self, wait: Duration) -> Result<()> {
        let link = lock(&self.link);
        let (link, _) = self
            .linked
            .wait_timeout_while(link, wait, |l| l.out.is_none())
            .unwrap_or_else(|p| p.into_inner());
        if link.out.is_none() {
            bail!("not connected to the coordinator ({})", link.down);
        }
        Ok(())
    }

    fn set_down(&self, why: &str) {
        let mut link = lock(&self.link);
        link.out = None;
        link.down = why.to_string();
    }

    /// Connect, serve, reconnect — until stopped.
    fn run(&self) {
        // Spread out instances that all lost the same coordinator at once.
        let jitter = Duration::from_millis(u64::from(std::process::id() % 100));
        let mut delay = RETRY_MIN;
        while !self.stop.load(Ordering::Relaxed) {
            delay = match self.session() {
                Ok(Ended::Lost) => {
                    self.set_down("connection lost; reconnecting");
                    RETRY_MIN
                }
                Ok(Ended::Refused(why)) => {
                    self.set_down(&why);
                    RETRY_INCOMPATIBLE
                }
                Err(e) => {
                    self.set_down(&format!("{e:#}"));
                    (delay * 2).min(RETRY_MAX)
                }
            };
            // Whoever was waiting on the old connection won't get an answer.
            for (_, waiter) in lock(&self.pending).drain() {
                let _ = waiter.send(Err("the coordinator connection was lost".into()));
            }
            let until = Instant::now() + delay + jitter;
            while Instant::now() < until && !self.stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    /// One connection: establish it, then read until it ends.
    fn session(&self) -> Result<Ended> {
        let stream = self.connect()?;
        let mut reader = BufReader::new(stream.try_clone()?);

        // Hello, answered before anything else is sent either way.
        let mut hello = self.describe();
        hello["proto"] = PROTO.into();
        hello["topics"] = json!(*lock(&self.topics));
        let mut out = stream.try_clone()?;
        let line = json!({ "jsonrpc": "2.0", "id": 0, "method": "hello", "params": hello });
        out.write_all(format!("{line}\n").as_bytes())?;
        stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
        let mut answer = String::new();
        if reader.read_line(&mut answer)? == 0 {
            bail!("the coordinator closed the connection");
        }
        stream.set_read_timeout(None)?;
        let answer: Value = serde_json::from_str(&answer).context("coordinator's hello answer")?;
        if let Some(err) = answer.get("error") {
            let msg = err["message"].as_str().unwrap_or("refused").to_string();
            if err["code"].as_i64() == Some(ERR_INCOMPATIBLE) {
                return Ok(Ended::Refused(msg));
            }
            // Superseded: it is on its way out; the retry starts a new one.
            bail!(msg);
        }
        if let Some(name) = answer["result"]["name"].as_str() {
            lock(&self.me).name = name.to_string();
        }
        {
            let mut link = lock(&self.link);
            link.coordinator = answer["result"]["coordinator"]["version"]
                .as_str()
                .unwrap_or("?")
                .to_string();
            link.out = Some(out);
            link.down.clear();
        }
        self.linked.notify_all();

        for line in reader.lines() {
            let Ok(line) = line else { break };
            if let Ok(msg) = serde_json::from_str::<Value>(&line) {
                self.incoming(&msg);
            }
        }
        Ok(Ended::Lost)
    }

    fn incoming(&self, msg: &Value) {
        let params = &msg["params"];
        match (msg["method"].as_str(), msg.get("id")) {
            // An answer to one of our requests.
            (None, Some(id)) => {
                let Some(waiter) = id.as_u64().and_then(|k| lock(&self.pending).remove(&k)) else {
                    return;
                };
                let answer = match msg.get("error") {
                    Some(e) => Err(e["message"].as_str().unwrap_or("error").to_string()),
                    None => Ok(msg["result"].clone()),
                };
                let _ = waiter.send(answer);
            }
            (Some("message"), _) => {
                let text = |v: &Value| v.as_str().unwrap_or_default().to_string();
                let mut inbox = lock(&self.inbox);
                if inbox.len() >= INBOX_MAX {
                    inbox.pop_front();
                }
                inbox.push_back(Message {
                    from: text(&params["from"]["name"]),
                    cwd: text(&params["from"]["cwd"]),
                    topic: params["topic"].as_str().map(String::from),
                    broadcast: params["broadcast"].as_bool().unwrap_or(false),
                    body: text(&params["body"]),
                });
            }
            // Another instance's `call`, forwarded for us to answer.
            (Some("call"), Some(id)) => {
                let method = params["method"].as_str().unwrap_or_default();
                let result = match lock(&self.handlers).get(method) {
                    Some(f) => f(&params["params"]),
                    None => Err(format!("'{method}' is not something this instance answers")),
                };
                let mut reply = json!({ "jsonrpc": "2.0", "id": id });
                match result {
                    Ok(v) => reply["result"] = v,
                    Err(e) => reply["error"] = json!({ "code": -32000, "message": e }),
                }
                let _ = self.write(&reply);
            }
            // `shutdown` and anything newer: the closed connection says it all.
            _ => {}
        }
    }

    /// A connection to the coordinator, starting one if none answers.
    fn connect(&self) -> Result<UnixStream> {
        let first = match super::connect(&self.dir) {
            Ok(s) => return Ok(s),
            Err(e) => e,
        };
        let Some(exe) = &self.exe else {
            return Err(first.context(format!(
                "no coordinator at {}",
                socket_path(&self.dir).display()
            )));
        };
        spawn_daemon(exe, &self.dir)?;
        let deadline = Instant::now() + SPAWN_WAIT;
        loop {
            match super::connect(&self.dir) {
                Ok(s) => return Ok(s),
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
}

/// Launch `atelier coordinator`, detached: in its own process group (the
/// terminal's signals don't reach it) with its output in a log file. If
/// another instance won the race, this one notices and exits by itself.
fn spawn_daemon(exe: &Path, dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let log_path = dir.join("coordinator.log");
    let big = std::fs::metadata(&log_path).is_ok_and(|m| m.len() > LOG_MAX);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(!big)
        .write(true)
        .truncate(big)
        .open(&log_path)
        .map_or_else(|_| Stdio::null(), Stdio::from);
    let mut child = Command::new(exe)
        .arg("coordinator")
        .env(ENV_DIR, dir)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .process_group(0)
        .spawn()
        .with_context(|| format!("starting the coordinator ({})", exe.display()))?;
    // Reap it whenever it ends, so it never lingers as a zombie of ours.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// One request on a throwaway connection that never registers as an
/// instance — for `atelier coordinator status|stop`. Never starts a
/// coordinator.
pub fn control(dir: &Path, method: &str) -> Result<Value> {
    let stream = super::connect(dir).context("connecting to the coordinator")?;
    stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    let mut out = stream.try_clone()?;
    let line = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": {} });
    out.write_all(format!("{line}\n").as_bytes())?;
    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer)?;
    let answer: Value = serde_json::from_str(&answer).context("the coordinator's answer")?;
    if let Some(e) = answer.get("error") {
        bail!("{}", e["message"].as_str().unwrap_or("error"));
    }
    Ok(answer["result"].clone())
}
