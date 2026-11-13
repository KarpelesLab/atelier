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
//!
//! Another instance's `call` is answered by the handler registered for its
//! method. Handlers run on the reader thread, so they must return quickly;
//! one whose answer takes longer than that — a question for the agent, see
//! [`super::ask`] — registers with [`Client::handle_deferred`] and keeps the
//! [`Responder`] it is handed, to answer whenever the answer exists. The
//! coordinator holds the call open until then, or until its deadline.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::ask::{self, Desk, Outcome};
use super::{ENV_DIR, ERR_GONE, ERR_INCOMPATIBLE, ERR_TIMEOUT, Message, PROTO, socket_path};
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
    /// Counts the connections established. A forwarded `call` is known by
    /// a number of the coordinator's that means nothing to the next one.
    session: u64,
}

/// A request the coordinator (or the instance it forwarded a `call` to)
/// answered with an error.
#[derive(Debug)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RpcError {}

type Waiter = Sender<Result<Value, RpcError>>;
/// Deals with a `call` from another instance.
type Handler = Box<dyn Fn(Incoming) + Send + Sync>;

/// A `call` from another instance, as its handler gets it.
pub struct Incoming {
    /// The calling instance's name, and the directory it works in.
    pub from: String,
    pub cwd: String,
    pub params: Value,
    /// How to answer; may be kept and used after the handler has returned.
    pub reply: Responder,
}

/// The answer to one forwarded `call`, to be given once. Dropped without
/// one, it tells the caller so rather than leave it waiting for its timeout.
pub struct Responder {
    sh: Weak<Shared>,
    id: Value,
    /// The connection the call arrived on (see [`Link::session`]).
    session: u64,
    answered: bool,
}

impl Responder {
    /// Send the answer. Fails when the connection the call arrived on is
    /// gone — the coordinator has already failed the call for the caller.
    pub fn answer(mut self, result: Result<Value, String>) -> Result<()> {
        self.answered = true;
        self.send(result)
    }

    fn send(&self, result: Result<Value, String>) -> Result<()> {
        let Some(sh) = self.sh.upgrade() else {
            bail!("this instance has left the coordinator");
        };
        let mut reply = json!({ "jsonrpc": "2.0", "id": self.id });
        match result {
            Ok(v) => reply["result"] = v,
            Err(e) => reply["error"] = json!({ "code": -32000, "message": e }),
        }
        sh.write_on(Some(self.session), &reply)
    }
}

impl Drop for Responder {
    fn drop(&mut self) {
        if !self.answered {
            let _ = self.send(Err("the call was left unanswered".into()));
        }
    }
}

struct Shared {
    /// Itself, for the responders it hands out.
    me_ref: Weak<Shared>,
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
    /// The questions other instances asked this one (see [`ask`]). Locked
    /// after `inbox` where both are held.
    desk: Mutex<Desk>,
    /// The instances this one is waiting on an answer from.
    asking: Mutex<Vec<String>>,
    /// What goes out as `wake_depth` (see [`Client::set_wake_depth`]).
    wake_depth: AtomicU32,
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
        let sh = Arc::new_cyclic(|me_ref| Shared {
            me_ref: me_ref.clone(),
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
            desk: Mutex::new(Desk::default()),
            asking: Mutex::new(Vec::new()),
            wake_depth: AtomicU32::new(0),
            stop: AtomicBool::new(false),
        });
        let client = Arc::new(Self {
            sh: Arc::clone(&sh),
        });
        client.handle("ping", |_| Ok(json!("pong")));
        let info = Arc::clone(&sh);
        client.handle("info", move |_| Ok(info.describe()));
        let asked = Arc::downgrade(&sh);
        client.handle_deferred(ask::METHOD, move |call| {
            if let Some(sh) = asked.upgrade() {
                sh.asked(call);
            }
        });
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
        self.handle_deferred(method, move |call| {
            let _ = call.reply.answer(f(&call.params));
        });
    }

    /// Like [`handle`](Self::handle), for a method whose answer is not ready
    /// when the handler returns: `f` keeps the call's [`Responder`] and
    /// answers through it later, from any thread.
    pub fn handle_deferred(&self, method: &str, f: impl Fn(Incoming) + Send + Sync + 'static) {
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
        let wake_depth = self.sh.wake_depth.load(Ordering::Relaxed);
        let r = match to.strip_prefix('#') {
            Some(topic) => self.request(
                "publish",
                json!({ "topic": topic, "body": body, "wake_depth": wake_depth }),
                REQUEST_TIMEOUT,
            )?,
            None => self.request(
                "send",
                json!({ "to": to, "body": body, "wake_depth": wake_depth }),
                REQUEST_TIMEOUT,
            )?,
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

    /// Put `question` to the agent of the instance `to` and wait, up to
    /// `timeout`, for what it answers (see [`ask`]). `Err` is for a question
    /// that could not be asked at all.
    pub fn ask(&self, to: &str, question: &str, timeout: Duration) -> Result<Outcome> {
        let timeout = timeout.min(ask::MAX_TIMEOUT);
        let params = json!({
            "question": question,
            "timeout_ms": timeout.as_millis() as u64,
            "wake_depth": self.sh.wake_depth.load(Ordering::Relaxed),
        });
        lock(&self.sh.asking).push(to.to_string());
        let answer = self.call(to, ask::METHOD, params, timeout);
        {
            let mut asking = lock(&self.sh.asking);
            if let Some(i) = asking.iter().position(|n| n == to) {
                asking.swap_remove(i);
            }
        }
        match answer {
            Ok(v) => Ok(Outcome::from_value(&v)),
            Err(e) => match e.downcast_ref::<RpcError>() {
                Some(rpc) if rpc.code == ERR_TIMEOUT => Ok(Outcome::Unanswered(format!(
                    "{to} did not answer within {}s: it may be idle, waiting for its user, or \
                     busy with something long",
                    timeout.as_secs()
                ))),
                Some(rpc) if rpc.code == ERR_GONE => Ok(Outcome::Unanswered(rpc.message.clone())),
                _ => Err(e),
            },
        }
    }

    /// Say how far what this instance sends from now on is from a prompt a
    /// user typed: 0 in a turn the user started, one more than the depth of
    /// the message that woke it in a turn the instance started by itself
    /// (see `agent::idle`). It rides on every `send` and `ask`.
    pub fn set_wake_depth(&self, depth: u32) {
        self.sh.wake_depth.store(depth, Ordering::Relaxed);
    }

    /// Answer the question `id` another instance asked this one; returns
    /// the asker's name.
    pub fn answer_question(&self, id: &str, text: &str) -> Result<String> {
        lock(&self.sh.desk).answer(id, text)
    }

    /// Whether the question `id` still has someone waiting for its answer.
    pub fn question_open(&self, id: &str) -> bool {
        lock(&self.sh.desk).is_open(id)
    }

    /// Close the questions that were handed out (they are no longer in the
    /// inbox, nor among `held`, the ids the caller still keeps for later)
    /// and are still unanswered, telling each asker `why`. Returns
    /// `(id, asker)` of each.
    pub fn close_questions(&self, held: &[&str], why: &str) -> Vec<(String, String)> {
        let inbox = lock(&self.sh.inbox);
        let waiting = |id: &str| {
            held.contains(&id) || inbox.iter().any(|m| m.question.as_deref() == Some(id))
        };
        lock(&self.sh.desk).close(waiting, why)
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
            Ok(Err(e)) => Err(anyhow::Error::new(e)),
            Err(_) => {
                lock(&sh.pending).remove(&id);
                bail!("the coordinator did not answer '{method}' in time")
            }
        }
    }
}

/// A JSON string, or nothing.
fn text(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

/// A `wake_depth` as received; one that is missing is a user's own.
fn depth(v: &Value) -> u32 {
    v.as_u64().map_or(0, |d| d.min(u64::from(u32::MAX)) as u32)
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
        self.write_on(None, msg)
    }

    /// [`write`](Self::write), but only on the connection numbered
    /// `session`, if one is given.
    fn write_on(&self, session: Option<u64>, msg: &Value) -> Result<()> {
        let mut link = lock(&self.link);
        if session.is_some_and(|s| s != link.session) {
            bail!("the coordinator connection it arrived on is gone");
        }
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
                let _ = waiter.send(Err(RpcError {
                    code: -32000,
                    message: "the coordinator connection was lost".into(),
                }));
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
            link.session += 1;
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

    /// Put a message in the inbox, making room if it is full.
    fn deliver(&self, inbox: &mut VecDeque<Message>, m: Message) {
        if inbox.len() >= INBOX_MAX
            && let Some(Message {
                question: Some(id), ..
            }) = inbox.pop_front()
        {
            // A question nobody will see must not keep its asker waiting.
            let me = lock(&self.me).name.clone();
            let why = format!("{me} had too many unread messages; the question was dropped");
            lock(&self.desk).close(|open| open != id, &why);
        }
        inbox.push_back(m);
    }

    /// Another instance's question (see [`ask`]): written down for the
    /// agent, to be answered through the kept responder.
    fn asked(&self, call: Incoming) {
        let Incoming {
            from,
            cwd,
            params,
            reply,
        } = call;
        let reply: ask::Reply = Box::new(move |outcome| reply.answer(Ok(outcome.to_value())));
        // This instance's agent is blocked on `from`'s answer, so it will
        // not get to the question before that call is over: say so now
        // rather than have both wait out their timeouts.
        if lock(&self.asking).contains(&from) {
            let me = lock(&self.me).name.clone();
            let why = format!(
                "{me} is itself waiting for an answer from {from}, and cannot answer until it \
                 has it"
            );
            let _ = reply(Outcome::Unanswered(why));
            return;
        }
        let timeout = params["timeout_ms"]
            .as_u64()
            .map_or(ask::DEFAULT_TIMEOUT, Duration::from_millis)
            .min(ask::MAX_TIMEOUT);
        let mut inbox = lock(&self.inbox);
        let id = lock(&self.desk).admit(&from, timeout, reply);
        self.deliver(
            &mut inbox,
            Message {
                from,
                cwd,
                topic: None,
                broadcast: false,
                body: text(&params["question"]),
                question: Some(id),
                wake_depth: depth(&params["wake_depth"]),
            },
        );
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
                    Some(e) => Err(RpcError {
                        code: e["code"].as_i64().unwrap_or(-32000),
                        message: e["message"].as_str().unwrap_or("error").to_string(),
                    }),
                    None => Ok(msg["result"].clone()),
                };
                let _ = waiter.send(answer);
            }
            (Some("message"), _) => {
                let mut inbox = lock(&self.inbox);
                self.deliver(
                    &mut inbox,
                    Message {
                        from: text(&params["from"]["name"]),
                        cwd: text(&params["from"]["cwd"]),
                        topic: params["topic"].as_str().map(String::from),
                        broadcast: params["broadcast"].as_bool().unwrap_or(false),
                        body: text(&params["body"]),
                        question: None,
                        wake_depth: depth(&params["wake_depth"]),
                    },
                );
            }
            // Another instance's `call`, forwarded for us to answer.
            (Some("call"), Some(id)) => {
                let method = params["method"].as_str().unwrap_or_default();
                let call = Incoming {
                    from: text(&params["from"]["name"]),
                    cwd: text(&params["from"]["cwd"]),
                    params: params["params"].clone(),
                    reply: Responder {
                        sh: self.me_ref.clone(),
                        id: id.clone(),
                        session: lock(&self.link).session,
                        answered: false,
                    },
                };
                match lock(&self.handlers).get(method) {
                    Some(f) => f(call),
                    None => {
                        let why = format!("'{method}' is not something this instance answers");
                        let _ = call.reply.answer(Err(why));
                    }
                }
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
