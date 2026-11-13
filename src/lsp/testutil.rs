//! A fake language server for the tests: a thread speaking LSP over pipes,
//! driven by a closure from a request's method and params to what the
//! server does about it. No real server is needed (CI has none), and what a
//! test's server does — answer late, never, with an error, or by exiting —
//! is stated in the test.

use std::io::{PipeWriter, Read, pipe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use super::client::Transport;
use super::frame::{self, Decoder};

/// What the fake server does with a request.
pub(crate) enum Act {
    /// Answer with this result.
    Reply(Value),
    /// Answer with an error carrying this message.
    Error(String),
    /// Say nothing: a server that is stuck.
    Ignore,
    /// Close the connection: a server that crashed.
    Exit,
}

type Handler = dyn Fn(&str, &Value) -> Act + Send + Sync;

struct Inner {
    handler: Box<Handler>,
    /// Handle each request on a thread of its own, so answers can overtake
    /// one another.
    concurrent: AtomicBool,
    /// Every request and notification received, in order, over every
    /// connection.
    seen: Mutex<Vec<(String, Value)>>,
    /// The output of the connection being served, if one is.
    out: Mutex<Option<PipeWriter>>,
    /// The client's answers to [`FakeServer::ask`].
    answers: Mutex<Vec<Value>>,
    answered: Condvar,
    /// How many times the server was started.
    starts: AtomicUsize,
}

impl Inner {
    fn write(&self, message: &Value) {
        if let Some(out) = self.out.lock().unwrap().as_mut() {
            let _ = frame::write(out, message);
        }
    }
}

/// See the module docs. Cloning gives another handle on the same server.
#[derive(Clone)]
pub(crate) struct FakeServer {
    inner: Arc<Inner>,
}

impl FakeServer {
    pub(crate) fn new(handler: impl Fn(&str, &Value) -> Act + Send + Sync + 'static) -> Self {
        Self {
            inner: Arc::new(Inner {
                handler: Box::new(handler),
                concurrent: AtomicBool::new(false),
                seen: Mutex::default(),
                out: Mutex::default(),
                answers: Mutex::default(),
                answered: Condvar::new(),
                starts: AtomicUsize::new(0),
            }),
        }
    }

    /// Handle requests concurrently.
    pub(crate) fn concurrent(self) -> Self {
        self.inner.concurrent.store(true, Ordering::SeqCst);
        self
    }

    /// "Start the server": a new connection to it, served by a new thread.
    pub(crate) fn transport(&self) -> Transport {
        let (mut server_in, client_out) = pipe().unwrap();
        let (client_in, server_out) = pipe().unwrap();
        *self.inner.out.lock().unwrap() = Some(server_out);
        self.inner.starts.fetch_add(1, Ordering::SeqCst);
        let inner = Arc::clone(&self.inner);
        thread::spawn(move || {
            let mut decoder = Decoder::default();
            let mut buf = [0u8; 4096];
            loop {
                let n = match server_in.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                decoder.push(&buf[..n]);
                while let Some(body) = decoder.next().unwrap() {
                    let message: Value = serde_json::from_slice(&body).unwrap();
                    if !serve(&inner, message) {
                        return;
                    }
                }
            }
        });
        Transport {
            writer: Box::new(client_out),
            reader: Box::new(client_in),
            child: None,
        }
    }

    /// How many times [`transport`](Self::transport) was called.
    pub(crate) fn starts(&self) -> usize {
        self.inner.starts.load(Ordering::SeqCst)
    }

    /// The methods received so far, in order.
    pub(crate) fn methods(&self) -> Vec<String> {
        let seen = self.inner.seen.lock().unwrap();
        seen.iter().map(|(method, _)| method.clone()).collect()
    }

    /// The params of the last `method` received.
    pub(crate) fn params_of(&self, method: &str) -> Value {
        let seen = self.inner.seen.lock().unwrap();
        let found = seen.iter().rev().find(|(m, _)| m == method);
        found.map_or(Value::Null, |(_, params)| params.clone())
    }

    /// Send the client a notification.
    pub(crate) fn tell(&self, method: &str, params: Value) {
        self.inner
            .write(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// Send the client a request and wait for its answer (the whole
    /// response message).
    pub(crate) fn ask(&self, method: &str, params: Value) -> Value {
        // A string id, as servers use: it cannot be taken for one of the
        // client's own.
        let id = format!("server-{method}");
        self.inner
            .write(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let mut answers = self.inner.answers.lock().unwrap();
        loop {
            if let Some(i) = answers.iter().position(|a| a["id"] == id.as_str()) {
                return answers.remove(i);
            }
            let (guard, timeout) = self
                .inner
                .answered
                .wait_timeout(answers, Duration::from_secs(10))
                .unwrap();
            assert!(!timeout.timed_out(), "the client never answered {method}");
            answers = guard;
        }
    }
}

/// Deal with one message from the client. `false` ends the connection.
fn serve(inner: &Arc<Inner>, message: Value) -> bool {
    let Some(method) = message["method"].as_str().map(str::to_string) else {
        inner.answers.lock().unwrap().push(message);
        inner.answered.notify_all();
        return true;
    };
    let params = message["params"].clone();
    inner.seen.lock().unwrap().push((method.clone(), params));
    let Some(id) = message.get("id").cloned() else {
        return true;
    };
    let answer = {
        let inner = Arc::clone(inner);
        move || match (inner.handler)(&method, &message["params"]) {
            Act::Reply(result) => {
                inner.write(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
                true
            }
            Act::Error(text) => {
                let error = json!({ "code": -32603, "message": text });
                inner.write(&json!({ "jsonrpc": "2.0", "id": id, "error": error }));
                true
            }
            Act::Ignore => true,
            Act::Exit => {
                // Dropping the output is what the client sees of a crash.
                *inner.out.lock().unwrap() = None;
                false
            }
        }
    };
    match inner.concurrent.load(Ordering::SeqCst) {
        true => {
            thread::spawn(answer);
            true
        }
        false => answer(),
    }
}
