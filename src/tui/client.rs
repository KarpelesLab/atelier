//! The terminal interface as a remote: a second front end of a session
//! another atelier owns (see [`crate::own`], and "A session shared with its
//! terminal" in `docs/protocol.md` for what is said on the wire).
//!
//! It is the interface of [`super`] — the same input line, status strip
//! and scrollback, drawn by the same [`Renderer`] — with the owner's socket
//! where the session would be. Nothing is opened here: no configuration is
//! read, no instance joins the coordinator, no hook runs. What is typed is
//! sent to the owner and served there, in the owner's process, as if typed
//! at its terminal; what the session reports comes back as events and is
//! drawn as a turn of this terminal's own would be.
//!
//! - **A line** is a prompt or a slash command, for the owner to tell
//!   (`command/run` with `start`), or — while a turn runs — a message for
//!   that turn (`turn/steer`). A command typed during a turn waits here
//!   for its end, with whatever is typed behind it, as at the owner's
//!   terminal: lines are served in the order they were typed.
//! - **Ctrl-C** on an empty line drops what is waiting, or else stops the
//!   running turn (`turn/interrupt`).
//! - **An approval or a question** is put to every front end; answered
//!   here it is answered for all, and answered elsewhere it is taken down
//!   here with a line saying where.
//! - **Ctrl-T** hands the keyboard to the running command, as it does at
//!   the owner's terminal (`command/input`).
//! - **Ctrl-D**, `/quit`, `/detach` or closing the terminal leave. The
//!   session is not this terminal's: it and everything in it go on.
//! - **When the owner ends**, this says how and exits. The saved session is
//!   there for `atelier --continue`.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::term::{self, Event, Key};
use super::{
    CommandNews, KEY_POLL, KeyAction, PROMPT, RawGuard, Renderer, SPINNER_TICK, TYPING_POLL,
    approval_key, edit_key, question_key, truncate_tail_head,
};
use crate::agent::Approval;
use crate::own::Live;
use crate::tools::ask::{Answer, Question};
use crate::tools::terminal::CommandInput;

/// How long the owner has to answer the handshake.
const HANDSHAKE: Duration = Duration::from_secs(10);
/// How much of the conversation so far is shown on attaching.
const HISTORY: usize = 200;
/// How long a line the owner was too busy to take waits before it is sent
/// again.
const AGAIN: Duration = Duration::from_millis(200);

/// What a request sent to the owner was, for when its answer comes.
enum Sent {
    /// A line typed while the session was idle (`command/run`).
    Line(String),
    /// A message for the running turn (`turn/steer`).
    Steer(String),
    /// `turn/interrupt`.
    Interrupt,
    /// `session/status`, asked to redraw the strip.
    Status,
}

/// The connection to the owner, and what this remote knows of the session.
struct Link {
    out: UnixStream,
    /// Messages from the owner; `None` once it closed the connection.
    incoming: Receiver<Option<Value>>,
    /// Read while waiting for an answer, not yet looked at.
    backlog: VecDeque<Value>,
    next_id: u64,
    waiting: BTreeMap<u64, Sent>,
    /// This remote, as events name it (`remote-2`).
    me: String,
    owner: String,
    /// A turn is running.
    busy: bool,
    /// A line was sent and the owner has not said what it was yet.
    sending: bool,
    /// A line the owner was too busy to take, to send again once it can,
    /// and since when it waits.
    again: Option<(String, Instant)>,
    /// The owner's request that is up here, to answer by id.
    asked: Option<Value>,
    /// The session, for the strip: model, directory, mode.
    session: Value,
    turn: u64,
}

impl Link {
    fn send(&mut self, message: &Value) {
        // A write that fails is the owner gone: the reader says so.
        let _ = self.out.write_all(format!("{message}\n").as_bytes());
    }

    fn request(&mut self, method: &str, params: Value, sent: Sent) {
        self.next_id += 1;
        self.waiting.insert(self.next_id, sent);
        let id = self.next_id;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
    }

    /// Answer the owner's request that is up.
    fn answer(&mut self, result: Value) {
        if let Some(id) = self.asked.take() {
            self.send(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
        }
    }

    /// Send a request and wait for its answer, keeping what else arrives
    /// for later. For the handshake, before there is a loop.
    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let deadline = Instant::now() + HANDSHAKE;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let message = match self.incoming.recv_timeout(left) {
                Ok(Some(message)) => message,
                Ok(None) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("the session's owner closed the connection")
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    bail!("the session's owner did not answer {method}")
                }
            };
            if message["id"] == id && message.get("method").is_none() {
                if let Some(error) = message.get("error") {
                    bail!("{}", error["message"].as_str().unwrap_or("refused"));
                }
                return Ok(message["result"].clone());
            }
            self.backlog.push_back(message);
        }
    }

    /// The next message, if one is there.
    fn next(&mut self) -> Option<Option<Value>> {
        if let Some(message) = self.backlog.pop_front() {
            return Some(Some(message));
        }
        match self.incoming.try_recv() {
            Ok(message) => Some(message),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(None),
        }
    }

    /// The status strip: whose session this is, then what the owner's own
    /// strip says of it.
    fn status(&self) -> String {
        let s = &self.session;
        let cwd = s["cwd"].as_str().unwrap_or("?");
        let cwd = cwd.rsplit('/').next().unwrap_or(cwd);
        let mut strip = format!(
            " remote of {} · {} · {cwd} · turn {}",
            self.owner,
            s["model"].as_str().unwrap_or("?"),
            self.turn
        );
        match s["mode"].as_str() {
            Some(mode) if mode != crate::permissions::Mode::default().name() => {
                strip.push_str(&format!(" · {mode}"));
            }
            _ => {}
        }
        strip
    }
}

/// How the remote's loop goes on after something happened.
enum Next {
    Stay,
    /// Leave: the line to leave on the terminal, if there is one to say.
    Leave(Option<String>),
}

/// Be a remote of the live session `live` on this terminal, until the user
/// leaves or the owner ends. The exit status: 0, the session's fate being
/// the owner's.
pub fn run(live: &Live) -> Result<u8> {
    let stream = crate::own::connect(&live.record)?;
    let (tx, incoming) = mpsc::channel();
    let reader = stream.try_clone()?;
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            let Ok(line) = line else { break };
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if tx.send(Some(message)).is_err() {
                return;
            }
        }
        let _ = tx.send(None);
    });
    let owner = match &live.record.tty {
        Some(tty) => format!("pid {} on {tty}", live.record.pid),
        None => format!("pid {}", live.record.pid),
    };
    let mut link = Link {
        out: stream,
        incoming,
        backlog: VecDeque::new(),
        next_id: 0,
        waiting: BTreeMap::new(),
        me: String::new(),
        owner: owner.clone(),
        busy: false,
        sending: false,
        again: None,
        asked: None,
        session: Value::Null,
        turn: 0,
    };
    let hello = link
        .call(
            "initialize",
            json!({
                "client": { "name": "atelier", "version": env!("CARGO_PKG_VERSION") },
                "protocol": crate::serve::PROTOCOL,
                "terminal": true,
            }),
        )
        .context("attaching to the running session")?;
    link.me = format!("remote-{}", hello["shared"]["remote"]);
    link.session = hello["session"].clone();
    link.turn = hello["shared"]["turn"].as_u64().unwrap_or(0);
    let history = link.call("session/history", json!({ "limit": HISTORY }))?;

    let guard = RawGuard::enable()?;
    let mut r = Renderer::new();
    r.status = link.status();
    r.push_line(
        format!("atelier — remote control of the session of {owner}"),
        true,
    );
    r.push_line(
        "/help for commands · Ctrl-D leaves this remote, and the session goes on".into(),
        true,
    );
    show_history(&mut r, &history);
    r.refresh()?;

    let left = pump(&mut r, &mut link)?;
    // The terminal as it was found, then the last word.
    r.commit_pending_if_any();
    r.refresh()?;
    drop(guard);
    if let Some(line) = left {
        eprintln!("{line}");
    }
    Ok(0)
}

/// The conversation so far, as the transcript would have it.
fn show_history(r: &mut Renderer, history: &Value) {
    let items = history["items"].as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        return;
    }
    let total = history["total"].as_u64().unwrap_or(0);
    let shown = items.len() as u64;
    r.push_line(
        match history["more"] == true {
            true => format!("── the conversation so far (the last {shown} of {total} items) ──"),
            false => "── the conversation so far ──".to_string(),
        },
        true,
    );
    for item in &items {
        let text = item["text"].as_str().unwrap_or_default();
        match item["type"].as_str() {
            Some("summary") => {
                r.emit_block("(earlier turns, as they were summarized)", true);
                r.emit_block(text, true);
            }
            Some("user") => r.emit_block(&format!("{PROMPT}{text}"), false),
            Some("assistant") => {
                if !text.is_empty() {
                    r.emit_block(text, false);
                }
            }
            Some("tool") => {
                let args = truncate_tail_head(&item["arguments"].to_string(), 200);
                let name = item["name"].as_str().unwrap_or("?");
                r.emit_block(&format!("⚙ {name} {args}"), true);
                if let Some(output) = item["output"].as_str() {
                    let mark = if item["ok"] == false { "✗" } else { "✓" };
                    let preview: String = output.chars().take(200).collect();
                    r.emit_block(&format!("{mark} {preview}"), true);
                }
            }
            _ => {}
        }
    }
    r.push_line("── now ──".to_string(), true);
}

/// The remote's loop: the keyboard and the owner's messages, until one of
/// them ends it. Returns the line to leave on.
fn pump(r: &mut Renderer, link: &mut Link) -> Result<Option<String>> {
    let mut last_paint = Instant::now();
    loop {
        // The keyboard: a command's while it has it, the input line's
        // otherwise.
        let mut dirty = false;
        if r.attached() {
            match term::poll_raw(TYPING_POLL)? {
                Some(term::Raw::Bytes(bytes)) => r.typed(bytes)?,
                Some(term::Raw::Resize) => r.send_size(),
                None => {}
            }
        } else {
            match term::poll(KEY_POLL)? {
                Some(Event::Resize) => dirty = true,
                Some(Event::Key(k)) => {
                    dirty = true;
                    if let Next::Leave(line) = key(r, link, k)? {
                        return Ok(line);
                    }
                }
                None => {}
            }
        }
        // What was typed to the command goes to the owner, which has it.
        let keys: Vec<CommandInput> =
            std::mem::take(&mut *r.keys.lock().unwrap_or_else(|p| p.into_inner()));
        for input in keys {
            let params = match input {
                CommandInput::Attach => json!({ "attached": true }),
                CommandInput::Detach => json!({ "attached": false }),
                CommandInput::Resize { cols, rows } => json!({ "cols": cols, "rows": rows }),
                CommandInput::Bytes(bytes) => {
                    json!({ "data": String::from_utf8_lossy(&bytes) })
                }
            };
            link.send(&json!({ "jsonrpc": "2.0", "method": "command/input", "params": params }));
        }
        // Then what the owner sent meanwhile.
        while let Some(message) = link.next() {
            dirty = true;
            let Some(message) = message else {
                return Ok(Some(format!(
                    "the atelier that owned the session ({}) is gone without a word: killed, or \
                     crashed. `atelier --continue` picks the saved session up here.",
                    link.owner
                )));
            };
            if let Next::Leave(line) = incoming(r, link, &message) {
                return Ok(line);
            }
        }
        // What waited for the session to be free, one line at a time.
        if !link.busy && !link.sending {
            if let Some((line, since)) = link.again.take() {
                match since.elapsed() >= AGAIN {
                    true => run_line(link, &line),
                    false => link.again = Some((line, since)),
                }
            } else if let Some(line) = r.pop_queued() {
                r.push_line(format!("{PROMPT}{line}"), false);
                run_line(link, &line);
                dirty = true;
            }
        }
        r.status = link.status();
        if dirty || (link.busy && last_paint.elapsed() >= SPINNER_TICK) {
            r.refresh()?;
            last_paint = Instant::now();
        }
    }
}

/// A key, with the keyboard this terminal's own.
fn key(r: &mut Renderer, link: &mut Link, k: Key) -> Result<Next> {
    if k == Key::Ctrl('t') && r.command.is_some() {
        r.attach()?;
        return Ok(Next::Stay);
    }
    // Leaving is always possible, a turn or a prompt notwithstanding: what
    // is asked is everybody's to answer, and the session goes on.
    if k == Key::Ctrl('d') && r.editor.is_empty() {
        return Ok(Next::Leave(None));
    }
    if r.approval_pending {
        if let Some(approval) = approval_key(k) {
            r.approval_pending = false;
            let decision = match approval {
                Approval::Once => "once",
                Approval::Always => "always",
                Approval::Deny => "deny",
            };
            link.answer(json!({ "decision": decision }));
        }
        return Ok(Next::Stay);
    }
    if r.question.is_some() {
        if let Some(answer) = question_key(r, k) {
            link.answer(match answer {
                Answer::Labels(labels) => json!({ "labels": labels }),
                Answer::Text(text) => json!({ "text": text }),
                Answer::Declined => json!({ "declined": true }),
            });
        }
        return Ok(Next::Stay);
    }
    // Ctrl-C on an empty line drops what is waiting here, or else stops
    // the turn that is running, whoever started it.
    if k == Key::Ctrl('c') && r.editor.is_empty() {
        let dropped = r.clear_queue();
        if dropped > 0 {
            r.push_line(format!("! dropped {dropped} queued message(s)"), true);
        } else if link.busy {
            link.request("turn/interrupt", json!({}), Sent::Interrupt);
            r.commit_pending_if_any();
            r.push_line("! stopping…".into(), true);
        }
        return Ok(Next::Stay);
    }
    match edit_key(r, k) {
        KeyAction::Exit => Ok(Next::Leave(None)),
        KeyAction::Submit(line) => Ok(submit(r, link, line.trim())),
        KeyAction::Edited => Ok(Next::Stay),
    }
}

/// A line was entered.
fn submit(r: &mut Renderer, link: &mut Link, line: &str) -> Next {
    if line.is_empty() {
        return Next::Stay;
    }
    let command = line
        .split_whitespace()
        .next()
        .filter(|w| w.starts_with('/'));
    // Leaving is this terminal's own business.
    if matches!(command, Some("/quit" | "/exit" | "/detach")) {
        r.push_line(format!("{PROMPT}{line}"), false);
        return Next::Leave(None);
    }
    // A command needs the session, which is the turn's: it waits for the
    // turn's end, and so does what is typed behind it. So does anything
    // typed before the owner has said what the last line was.
    let waiting = link.sending || link.again.is_some() || r.queued_len() > 0;
    if waiting || (link.busy && command.is_some()) {
        r.enqueue(line.to_string());
        return Next::Stay;
    }
    if link.busy {
        r.push_line(format!("{PROMPT}{line}  (queued)"), true);
        link.request(
            "turn/steer",
            json!({ "text": line }),
            Sent::Steer(line.to_string()),
        );
        return Next::Stay;
    }
    r.push_line(format!("{PROMPT}{line}"), false);
    run_line(link, line);
    Next::Stay
}

/// Send a line for the owner to serve as typed.
fn run_line(link: &mut Link, line: &str) {
    link.sending = true;
    link.request(
        "command/run",
        json!({ "line": line, "start": true }),
        Sent::Line(line.to_string()),
    );
}

/// One message from the owner.
fn incoming(r: &mut Renderer, link: &mut Link, message: &Value) -> Next {
    let params = &message["params"];
    match (message["method"].as_str(), message.get("id")) {
        // The owner asks: put up here as at its own terminal.
        (Some("approval/request"), Some(id)) => {
            let args = truncate_tail_head(&params["arguments"].to_string(), 200);
            let tool = params["tool"].as_str().unwrap_or("?");
            r.emit_block(&format!("⚠ allow tool '{tool}'?  {args}"), false);
            r.emit_block("   [y] once   [a] always   [n] deny", false);
            r.approval_pending = true;
            link.asked = Some(id.clone());
        }
        (Some("question/request"), Some(id)) => match Question::from_args(params) {
            Ok(question) => {
                r.ask(question);
                link.asked = Some(id.clone());
            }
            // Not a question this build can put: somebody else's to answer.
            Err(e) => r.emit_block(&format!("! a question could not be shown here: {e}"), true),
        },
        (Some(_), Some(_)) => {}
        (Some(method), None) => return notified(r, link, method, params),
        (None, Some(id)) => answered(r, link, id, message),
        (None, None) => {}
    }
    Next::Stay
}

/// Another front end, as this one names it: by what events call it.
fn whose(from: &str) -> String {
    match from.strip_prefix("remote-") {
        Some(n) => format!("remote {n}"),
        None => "the owner's terminal".to_string(),
    }
}

/// A notification.
fn notified(r: &mut Renderer, link: &mut Link, method: &str, params: &Value) -> Next {
    let text = params["text"].as_str().unwrap_or_default();
    // A command line typed at another front end: what follows is its
    // output.
    let kind = match method {
        "event/idle" => params["kind"].as_str(),
        method => method.strip_prefix("event/"),
    };
    if kind == Some("input") {
        let from = params["from"].as_str().unwrap_or("terminal");
        if from != link.me {
            r.commit_pending_if_any();
            r.push_line(format!("{PROMPT}{text}  ← {}", whose(from)), false);
        }
        return Next::Stay;
    }
    match method {
        "turn/started" => {
            link.busy = true;
            link.turn = params["turn"].as_u64().unwrap_or(link.turn);
            r.working = Some(Instant::now());
            r.commit_pending_if_any();
            // What this terminal sent is on its screen already.
            let from = params["from"].as_str().unwrap_or("terminal");
            if from != link.me {
                r.push_line(format!("{PROMPT}{text}  ← {}", whose(from)), false);
            }
        }
        "turn/ended" => {
            link.busy = false;
            r.working = None;
            r.approval_pending = false;
            r.question = None;
            link.asked = None;
            r.command_ended();
            r.commit_pending_if_any();
            if let Some(error) = params["error"].as_str() {
                r.push_line(format!("error: {error}"), false);
            }
            // A blank line separates turns in the scrollback.
            r.push_line(String::new(), false);
            // The strip may have changed with the turn.
            link.request("session/status", json!({}), Sent::Status);
        }
        "request/cancelled" => {
            let ours = link.asked.as_ref() == Some(&params["id"]);
            if ours {
                link.asked = None;
                r.approval_pending = false;
                r.question = None;
            }
            let by = params["by"].as_str().unwrap_or_default();
            if by != link.me && params["reason"] == "answered" {
                let who = whose(by);
                let what = match (params["decision"].as_str(), &params["answer"]) {
                    (Some("once"), _) => "allowed once".to_string(),
                    (Some("always"), _) => "allowed, and from now on".to_string(),
                    (Some(_), _) => "denied".to_string(),
                    (None, answer) if answer["declined"] == true => "declined".to_string(),
                    (None, answer) => match answer["labels"].as_array() {
                        Some(labels) => {
                            let labels: Vec<&str> =
                                labels.iter().filter_map(Value::as_str).collect();
                            format!("answered: {}", labels.join(", "))
                        }
                        None => format!("answered: {}", answer["text"].as_str().unwrap_or("")),
                    },
                };
                r.commit_pending_if_any();
                r.push_line(format!("  ↳ {what} at {who}"), true);
            }
        }
        // The model or the mode was changed, here or elsewhere.
        "session/updated" => {
            for key in ["model", "mode", "cwd", "backend"] {
                link.session[key] = params[key].clone();
            }
        }
        "session/closed" => {
            let line = match params["reason"].as_str() {
                Some("taken_over") => format!(
                    "another atelier ({}) took the session over: this remote has ended.",
                    params["by"].as_str().unwrap_or("?")
                ),
                _ => format!(
                    "the atelier that owned the session ({}) has quit: this remote has ended. \
                     `atelier --continue` picks the saved session up here.",
                    link.owner
                ),
            };
            return Next::Leave(Some(line));
        }
        "event/idle" => event(r, params["kind"].as_str().unwrap_or(""), params, text),
        _ => {
            if let Some(kind) = method.strip_prefix("event/") {
                event(r, kind, params, text);
            }
        }
    }
    Next::Stay
}

/// An event of the session, drawn as the owner's terminal draws it.
fn event(r: &mut Renderer, kind: &str, params: &Value, text: &str) {
    match kind {
        "text" => r.emit(text, false),
        "reasoning" => r.emit(text, true),
        "tool_start" => {
            let args = truncate_tail_head(&params["arguments"].to_string(), 200);
            let name = params["name"].as_str().unwrap_or("?");
            r.emit_block(&format!("⚙ {name} {args}"), true);
        }
        "tool_end" => {
            let mark = if params["ok"] == false { "✗" } else { "✓" };
            let output = params["output"].as_str().unwrap_or_default();
            let preview: String = output.chars().take(200).collect();
            r.emit_block(&format!("{mark} {preview}"), true);
        }
        "info" => r.emit_block(text, false),
        "activity" => r.emit_block(text, true),
        "notice" => r.emit_block(&format!("! {text}"), false),
        "review" => r.emit_block(&format!("💭 {text}"), true),
        "delivered" => {
            r.commit_pending_if_any();
            r.push_line(format!("{PROMPT}{text}"), false);
        }
        "command_started" => r.command_news(CommandNews::Started {
            interactive: params["interactive"] == true,
        }),
        "command_output" => {
            let data = params["data"].as_str().unwrap_or_default();
            r.command_news(CommandNews::Output(data.as_bytes().to_vec()));
        }
        "input_wanted" => {
            let prompt = params["prompt"].as_str().unwrap_or_default();
            r.command_news(CommandNews::InputWanted(prompt.to_string()));
        }
        "command_ended" => r.command_news(CommandNews::Ended),
        // A file change is the tool call the transcript already shows.
        _ => {}
    }
}

/// The owner's answer to something this remote asked.
fn answered(r: &mut Renderer, link: &mut Link, id: &Value, message: &Value) {
    let Some(sent) = id.as_u64().and_then(|id| link.waiting.remove(&id)) else {
        return;
    };
    let error = message.get("error");
    let code = error.map(|e| e["code"].as_i64().unwrap_or(0));
    let why = error.map(|e| e["message"].as_str().unwrap_or("refused").to_string());
    let result = &message["result"];
    if matches!(sent, Sent::Line(_)) {
        link.sending = false;
    }
    match (sent, code) {
        (Sent::Status, None) => {
            for key in ["model", "mode", "cwd", "backend"] {
                link.session[key] = result[key].clone();
            }
        }
        // The session was busy when the line arrived. With a turn that
        // started meanwhile, a prompt joins it, as one typed during it
        // would have; anything else is sent again once the session is free
        // (the end of that turn, or of the command another front end is
        // running).
        (Sent::Line(line), Some(crate::serve::code::TURN_RUNNING)) => {
            let in_turn = message["error"]["data"]["turn"].is_u64();
            if in_turn && !line.starts_with('/') {
                link.request("turn/steer", json!({ "text": line }), Sent::Steer(line));
            } else {
                link.again = Some((line, Instant::now()));
            }
        }
        (Sent::Line(_), None) => match result["kind"].as_str() {
            Some("config") => r.push_line(
                format!(
                    "! the settings screen is the owner terminal's ({}): /model, /mode and the \
                     others work from here",
                    link.owner
                ),
                true,
            ),
            Some("quit") => {}
            // A command ran: what it changed may show in the strip.
            Some("handled") => link.request("session/status", json!({}), Sent::Status),
            _ => {}
        },
        // The turn ended first: the message is a prompt of its own.
        (Sent::Steer(line), Some(crate::serve::code::NO_TURN)) => {
            link.busy = false;
            run_line(link, &line);
        }
        (Sent::Steer(_) | Sent::Interrupt | Sent::Status, _) if why.is_none() => {}
        (Sent::Interrupt, Some(crate::serve::code::NO_TURN)) => {}
        (_, _) => {
            if let Some(why) = why {
                r.push_line(format!("! {why}"), false);
            }
        }
    }
}
