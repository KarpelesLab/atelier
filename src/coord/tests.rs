//! Coordinator and clients together, over a real socket in a temp dir. The
//! daemon runs on a thread here; launching it as a process is covered by
//! `tests/coordinator.rs`.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::ask::Outcome;
use super::client::{self, Client, Options};
use super::{Message, daemon, socket_path};

/// A private directory for one test, removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("atl-coord-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Self(dir)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn spawn_daemon(dir: &Path, idle: Duration) -> JoinHandle<()> {
    let cfg = daemon::Config {
        dir: dir.to_path_buf(),
        idle,
        watch_exe: false,
    };
    let handle = std::thread::spawn(move || daemon::run(cfg).expect("daemon"));
    wait_for("the socket", || super::connect(dir).is_ok());
    handle
}

fn client(dir: &Path, name: &str) -> Arc<Client> {
    Client::start(Options {
        dir: dir.to_path_buf(),
        exe: None,
        name: name.into(),
        cwd: format!("/work/{name}"),
        model: "mock".into(),
    })
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The next message `c` receives.
fn next_message(c: &Client) -> Message {
    let mut got = Vec::new();
    wait_for("a message", || {
        got = c.take_messages();
        !got.is_empty()
    });
    assert_eq!(got.len(), 1, "{got:?}");
    got.remove(0)
}

fn names(c: &Client) -> Vec<String> {
    let list = c.list().expect("list");
    list["instances"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn instances_see_and_message_each_other() {
    let dir = Dir::new("msg");
    let daemon = spawn_daemon(&dir.0, Duration::from_secs(60));
    let (a, b, c) = (
        client(&dir.0, "proj"),
        client(&dir.0, "proj"),
        client(&dir.0, "o ther"),
    );
    wait_for("three instances", || names(&a).len() == 3);
    // Listed is not yet "has read its own name": wait for each to be linked.
    wait_for("b and c linked", || b.list().is_ok() && c.list().is_ok());

    // Names are made unique and typeable; each instance knows its own.
    let mut all = names(&a);
    all.sort();
    assert_eq!(all, ["o-ther", "proj", "proj-2"]);
    assert_ne!(a.name(), b.name());
    let list = a.list().unwrap();
    let me: Vec<&Value> = list["instances"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["you"] == true)
        .collect();
    assert_eq!(me.len(), 1);
    assert_eq!(me[0]["name"], a.name().as_str());
    assert_eq!(me[0]["cwd"], "/work/proj");
    assert_eq!(me[0]["status"], "idle");

    // Direct.
    assert_eq!(a.send(&b.name(), "hello b").unwrap(), 1);
    let m = next_message(&b);
    assert_eq!(
        (m.from.as_str(), m.body.as_str()),
        (a.name().as_str(), "hello b")
    );
    assert!(!m.broadcast && m.topic.is_none());
    assert!(
        a.send("nobody", "x")
            .unwrap_err()
            .to_string()
            .contains("no instance named")
    );

    // Broadcast reaches everyone but the sender.
    assert_eq!(c.send("*", "to all").unwrap(), 2);
    assert!(next_message(&a).broadcast);
    assert_eq!(next_message(&b).body, "to all");
    assert!(c.take_messages().is_empty());

    // Topics reach subscribers only, and not the publisher.
    a.subscribe("build", true).unwrap();
    c.subscribe("#build", true).unwrap();
    assert_eq!(c.send("#build", "green").unwrap(), 1);
    let m = next_message(&a);
    assert_eq!(m.topic.as_deref(), Some("build"));
    assert_eq!(m.scope(), " (#build)");
    assert!(b.take_messages().is_empty());
    a.subscribe("build", false).unwrap();
    assert_eq!(c.send("#build", "red").unwrap(), 0);

    // Status changes show up for the others.
    b.update("working", "big-model");
    wait_for("b's status", || {
        let list = a.list().unwrap();
        list["instances"].as_array().unwrap().iter().any(|i| {
            i["name"] == b.name().as_str() && i["status"] == "working" && i["model"] == "big-model"
        })
    });

    // An instance that leaves is gone from the list.
    c.stop();
    wait_for("c to leave", || names(&a).len() == 2);

    client::control(&dir.0, "shutdown").unwrap();
    daemon.join().unwrap();
    assert!(!socket_path(&dir.0).exists());
}

#[test]
fn instances_call_each_other() {
    let dir = Dir::new("call");
    let _daemon = spawn_daemon(&dir.0, Duration::from_secs(60));
    let (a, b) = (client(&dir.0, "a"), client(&dir.0, "b"));
    wait_for("both", || names(&a).len() == 2);
    b.handle("add", |p| match (p["x"].as_i64(), p["y"].as_i64()) {
        (Some(x), Some(y)) => Ok(json!(x + y)),
        _ => Err("x and y, please".into()),
    });
    b.handle("sleep", |_| {
        std::thread::sleep(Duration::from_millis(600));
        Ok(json!(null))
    });
    let t = Duration::from_secs(5);

    assert_eq!(a.call("b", "ping", json!({}), t).unwrap(), "pong");
    assert_eq!(a.call("b", "info", json!({}), t).unwrap()["cwd"], "/work/b");
    assert_eq!(a.call("b", "add", json!({"x": 2, "y": 3}), t).unwrap(), 5);
    let err = |r: anyhow::Result<Value>| r.unwrap_err().to_string();
    assert!(err(a.call("b", "add", json!({}), t)).contains("x and y"));
    assert!(err(a.call("b", "nope", json!({}), t)).contains("not something"));
    assert!(err(a.call("z", "ping", json!({}), t)).contains("no instance named"));
    // The coordinator gives up on a target that takes too long.
    let slow = a.call("b", "sleep", json!({}), Duration::from_millis(100));
    assert!(err(slow).contains("timed out"));

    // A target that goes away mid-call fails the call rather than hanging it.
    let (a2, b2) = (Arc::clone(&a), Arc::clone(&b));
    b.handle("vanish", move |_| {
        b2.stop();
        std::thread::sleep(Duration::from_millis(300));
        Ok(json!(null))
    });
    assert!(err(a2.call("b", "vanish", json!({}), t)).contains("disconnected"));
    client::control(&dir.0, "shutdown").unwrap();
}

#[test]
fn instances_rejoin_a_new_coordinator() {
    let dir = Dir::new("rejoin");
    let first = spawn_daemon(&dir.0, Duration::from_secs(60));
    let (a, b) = (client(&dir.0, "same"), client(&dir.0, "same"));
    wait_for("both", || names(&a).len() == 2);
    // The coordinator lists an instance a moment before the instance itself
    // has read which name it was given.
    wait_for("b to know its name", || b.list().is_ok());
    let (a_name, b_name) = (a.name(), b.name());
    assert_ne!(a_name, b_name);
    b.subscribe("news", true).unwrap();

    // The coordinator goes away: requests fail, saying why.
    client::control(&dir.0, "shutdown").unwrap();
    first.join().unwrap();
    wait_for("a to notice", || a.link_status().contains("not connected"));
    assert!(a.list().unwrap_err().to_string().contains("not connected"));

    // A new one appears: both come back as who they were, subscriptions
    // included, without being told.
    let second = spawn_daemon(&dir.0, Duration::from_secs(60));
    wait_for("both again", || {
        a.list()
            .is_ok_and(|l| l["instances"].as_array().unwrap().len() == 2)
    });
    wait_for("b linked again", || b.list().is_ok());
    assert_eq!((a.name(), b.name()), (a_name, b_name.clone()));
    assert_eq!(a.send("#news", "back").unwrap(), 1);
    assert_eq!(next_message(&b).body, "back");
    assert_eq!(a.send(&b_name, "direct").unwrap(), 1);
    assert_eq!(next_message(&b).body, "direct");

    client::control(&dir.0, "shutdown").unwrap();
    second.join().unwrap();
}

/// The question `c` was just asked.
fn next_question(c: &Client) -> (String, Message) {
    let m = next_message(c);
    (m.question.clone().expect("a question"), m)
}

#[test]
fn a_question_is_answered_later_by_whoever_holds_it() {
    let dir = Dir::new("ask");
    let _daemon = spawn_daemon(&dir.0, Duration::from_secs(60));
    let (a, b) = (client(&dir.0, "a"), client(&dir.0, "b"));
    wait_for("both linked", || a.list().is_ok() && b.list().is_ok());
    let t = Duration::from_secs(30);

    // The asker blocks; the question sits in the target's inbox, and the
    // target's reader thread is free meanwhile (it answers a `ping`).
    let asker = {
        let a = Arc::clone(&a);
        std::thread::spawn(move || a.ask("b", "which port does it listen on?", t))
    };
    let (id, m) = next_question(&b);
    assert_eq!(
        (m.from.as_str(), m.cwd.as_str(), m.body.as_str()),
        ("a", "/work/a", "which port does it listen on?")
    );
    assert_eq!(id, "q1");
    assert!(b.question_open(&id));
    assert_eq!(a.call("b", "ping", json!({}), t).unwrap(), "pong");
    assert!(!asker.is_finished());

    assert_eq!(b.answer_question(&id, "8080").unwrap(), "a");
    assert_eq!(
        asker.join().unwrap().unwrap(),
        Outcome::Answered("8080".into())
    );
    // Once only.
    assert!(!b.question_open(&id));
    let again = b.answer_question(&id, "8081").unwrap_err().to_string();
    assert!(again.contains("no open question 'q1'"), "{again}");

    // Nobody by that name: the question could not be asked at all.
    let nobody = a.ask("z", "hello?", t).unwrap_err().to_string();
    assert!(nobody.contains("no instance named"), "{nobody}");
    client::control(&dir.0, "shutdown").unwrap();
}

#[test]
fn an_unanswered_question_says_why() {
    let dir = Dir::new("noanswer");
    let _daemon = spawn_daemon(&dir.0, Duration::from_secs(60));
    let (a, b) = (client(&dir.0, "a"), client(&dir.0, "b"));
    wait_for("both linked", || a.list().is_ok() && b.list().is_ok());
    let unanswered = |r: anyhow::Result<Outcome>| match r {
        Ok(Outcome::Unanswered(why)) => why,
        other => panic!("expected no answer, got {other:?}"),
    };

    // Nobody answers in time: the coordinator gives up at the deadline, and
    // the target forgets the question.
    let why = unanswered(a.ask("b", "anyone?", Duration::from_millis(100)));
    assert!(why.contains("b did not answer within"), "{why}");
    let (id, _) = next_question(&b);
    wait_for("the question to expire", || !b.question_open(&id));
    assert!(b.answer_question(&id, "too late").is_err());

    // The turn it was handed to ends without an answer. A question that is
    // still in the inbox, or that the session holds for its next prompt,
    // was not handed to that turn and stays open.
    let ask = |q: &'static str| {
        let a = Arc::clone(&a);
        std::thread::spawn(move || a.ask("b", q, Duration::from_secs(30)))
    };
    let (first, second, third) = (ask("one"), ask("two"), ask("three"));
    let mut got = Vec::new();
    wait_for("three questions", || {
        got.extend(b.take_messages());
        got.len() == 3
    });
    let id_of = |body: &str| {
        let m = got.iter().find(|m| m.body == body).expect("asked");
        m.question.clone().unwrap()
    };
    let (one, two, three) = (id_of("one"), id_of("two"), id_of("three"));
    let fourth = ask("four");
    wait_for("the fourth to be admitted", || b.question_open("q5"));
    let closed = b.close_questions(&[two.as_str()], "b ended its turn without answering");
    let mut expected = vec![(one.clone(), "a".to_string()), (three, "a".to_string())];
    expected.sort();
    assert_eq!(closed, expected);
    assert_eq!(
        unanswered(first.join().unwrap()),
        "b ended its turn without answering"
    );
    assert_eq!(
        unanswered(third.join().unwrap()),
        "b ended its turn without answering"
    );
    assert!(b.question_open(&two));
    b.answer_question(&two, "2").unwrap();
    assert_eq!(
        second.join().unwrap().unwrap(),
        Outcome::Answered("2".into())
    );
    let (four, _) = next_question(&b);
    b.answer_question(&four, "4").unwrap();
    assert_eq!(
        fourth.join().unwrap().unwrap(),
        Outcome::Answered("4".into())
    );

    // The target leaves with the question open.
    let last = ask("five");
    next_question(&b);
    b.stop();
    let why = unanswered(last.join().unwrap());
    assert!(why.contains("b disconnected before answering"), "{why}");
    client::control(&dir.0, "shutdown").unwrap();
}

#[test]
fn two_instances_asking_each_other_do_not_wait_each_other_out() {
    let dir = Dir::new("mutual");
    let _daemon = spawn_daemon(&dir.0, Duration::from_secs(60));
    let (a, b) = (client(&dir.0, "a"), client(&dir.0, "b"));
    wait_for("both linked", || a.list().is_ok() && b.list().is_ok());
    let t = Duration::from_secs(30);

    let asker = {
        let a = Arc::clone(&a);
        std::thread::spawn(move || a.ask("b", "what do you need?", t))
    };
    let (id, _) = next_question(&b);
    // b's agent, instead of answering, asks a — whose agent is blocked on b.
    match b.ask("a", "what do *you* need?", t).unwrap() {
        Outcome::Unanswered(why) => {
            assert!(
                why.contains("a is itself waiting for an answer from b"),
                "{why}"
            )
        }
        other => panic!("{other:?}"),
    }
    assert!(a.take_messages().is_empty());
    b.answer_question(&id, "nothing").unwrap();
    assert_eq!(
        asker.join().unwrap().unwrap(),
        Outcome::Answered("nothing".into())
    );
    client::control(&dir.0, "shutdown").unwrap();
}

#[test]
fn a_deferred_handler_answers_from_another_thread_or_not_at_all() {
    let dir = Dir::new("deferred");
    let _daemon = spawn_daemon(&dir.0, Duration::from_secs(60));
    let (a, b) = (client(&dir.0, "a"), client(&dir.0, "b"));
    wait_for("both linked", || a.list().is_ok() && b.list().is_ok());
    let t = Duration::from_secs(30);

    let (tx, rx) = std::sync::mpsc::channel::<client::Incoming>();
    let tx = std::sync::Mutex::new(tx);
    b.handle_deferred("later", move |call| {
        tx.lock().unwrap().send(call).unwrap();
    });
    let caller = {
        let a = Arc::clone(&a);
        std::thread::spawn(move || a.call("b", "later", json!({"n": 20}), t))
    };
    let call = rx.recv_timeout(Duration::from_secs(10)).expect("the call");
    assert_eq!((call.from.as_str(), call.cwd.as_str()), ("a", "/work/a"));
    assert!(!caller.is_finished());
    let n = call.params["n"].as_i64().unwrap();
    call.reply.answer(Ok(json!(n + 1))).unwrap();
    assert_eq!(caller.join().unwrap().unwrap(), 21);

    // A responder that is dropped fails the call instead of leaving the
    // caller to its timeout.
    let caller = {
        let a = Arc::clone(&a);
        std::thread::spawn(move || a.call("b", "later", json!({}), t))
    };
    drop(rx.recv_timeout(Duration::from_secs(10)).expect("the call"));
    let err = caller.join().unwrap().unwrap_err().to_string();
    assert!(err.contains("left unanswered"), "{err}");
    client::control(&dir.0, "shutdown").unwrap();
}

#[test]
fn the_ask_tools_are_gated_like_what_they_do() {
    use crate::tools::{FileState, ToolCtx, ToolRegistry};
    let dir = Dir::new("asktools");
    let _daemon = spawn_daemon(&dir.0, Duration::from_secs(60));
    let (a, b) = (client(&dir.0, "a"), client(&dir.0, "b"));
    wait_for("both linked", || a.list().is_ok() && b.list().is_ok());
    let (mut for_a, mut for_b) = (ToolRegistry::new(), ToolRegistry::new());
    super::tools::register(&mut for_a, &a);
    super::tools::register(&mut for_b, &b);

    // Asking sets another agent to work: approved by the user, and not for
    // plan mode. Answering only completes what was already admitted.
    let (ask, answer) = (
        for_a.get("peer_ask").unwrap(),
        for_b.get("peer_answer").unwrap(),
    );
    assert!(ask.requires_approval(&json!({})) && ask.mutates(&json!({})));
    assert!(!answer.requires_approval(&json!({})) && !answer.mutates(&json!({})));

    let root = dir.0.clone();
    let call = move |tools: &ToolRegistry, name: &str, args: Value| {
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: &root,
            fstate: &mut fstate,
            approved: true,
        };
        tools.get(name).unwrap().call(&mut ctx, args)
    };
    let asker = {
        let call = call.clone();
        std::thread::spawn(move || {
            call(&for_a, "peer_ask", json!({"to": "b", "question": "ready?"}))
        })
    };
    let (id, _) = next_question(&b);
    let sent = call(&for_b, "peer_answer", json!({"id": id, "answer": "yes"})).unwrap();
    assert_eq!(sent, "answer sent to a");
    assert_eq!(asker.join().unwrap().unwrap(), "yes");

    // No answer is a result the asking model reads, not an error.
    let mut for_a = ToolRegistry::new();
    super::tools::register(&mut for_a, &a);
    let asker = std::thread::spawn(move || {
        call(
            &for_a,
            "peer_ask",
            json!({"to": "b", "question": "and now?"}),
        )
    });
    next_question(&b);
    b.close_questions(&[], "b ended its turn without answering");
    assert_eq!(
        asker.join().unwrap().unwrap(),
        "no answer: b ended its turn without answering"
    );
    client::control(&dir.0, "shutdown").unwrap();
}

/// One raw JSON-RPC exchange.
fn raw(dir: &Path, request: Value) -> Value {
    let mut s = super::connect(dir).unwrap();
    writeln!(s, "{request}").unwrap();
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

#[test]
fn steps_down_for_a_newer_atelier_and_refuses_an_older_protocol() {
    let dir = Dir::new("ver");
    let daemon = spawn_daemon(&dir.0, Duration::from_secs(60));
    let hello = |version: &str, proto: u64| {
        json!({"jsonrpc": "2.0", "id": 1, "method": "hello",
               "params": {"id": "x", "name": "x", "version": version, "proto": proto}})
    };

    // Unregistered connections can ask for status, nothing else.
    let r = raw(
        &dir.0,
        json!({"jsonrpc": "2.0", "id": 1, "method": "status"}),
    );
    assert_eq!(r["result"]["instances"], 0);
    let r = raw(
        &dir.0,
        json!({"jsonrpc": "2.0", "id": 1, "method": "send", "params": {"to": "*", "body": "x"}}),
    );
    assert!(r["error"]["message"].as_str().unwrap().contains("hello"));

    // An older protocol is refused; the coordinator stays.
    let r = raw(&dir.0, hello("0.0.1", 0));
    assert_eq!(r["error"]["code"], super::ERR_INCOMPATIBLE);
    assert!(client::control(&dir.0, "status").is_ok());

    // A newer atelier makes it step down.
    let r = raw(&dir.0, hello("999.0.0", super::PROTO));
    assert_eq!(r["error"]["code"], super::ERR_SUPERSEDED);
    daemon.join().unwrap();
    assert!(client::control(&dir.0, "status").is_err());
}

#[test]
fn only_one_coordinator_serves_and_it_leaves_when_idle() {
    let dir = Dir::new("idle");
    let daemon = spawn_daemon(&dir.0, Duration::from_millis(300));
    // A second one sees the first and bows out at once.
    let cfg = daemon::Config {
        dir: dir.0.clone(),
        idle: Duration::from_secs(60),
        watch_exe: false,
    };
    let t0 = Instant::now();
    daemon::run(cfg).unwrap();
    assert!(t0.elapsed() < Duration::from_secs(1));

    // An instance keeps it up past the idle time; without one it ends.
    let a = client(&dir.0, "a");
    wait_for("a", || a.list().is_ok());
    std::thread::sleep(Duration::from_millis(700));
    assert_eq!(names(&a).len(), 1);
    a.stop();
    daemon.join().unwrap();
}

#[test]
fn works_when_the_socket_path_is_too_long_for_an_address() {
    let dir = Dir::new("long");
    let deep = dir.0.join("d".repeat(60)).join("e".repeat(60));
    assert!(socket_path(&deep).as_os_str().len() > 108);
    let daemon = spawn_daemon(&deep, Duration::from_secs(60));
    let (a, b) = (client(&deep, "a"), client(&deep, "b"));
    wait_for("both", || names(&a).len() == 2);
    assert_eq!(a.send("b", "hi").unwrap(), 1);
    assert_eq!(next_message(&b).body, "hi");
    client::control(&deep, "shutdown").unwrap();
    daemon.join().unwrap();
}
